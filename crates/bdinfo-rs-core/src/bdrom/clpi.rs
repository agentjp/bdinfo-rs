//! Clip-information (`*.clpi`) parser.
//!
//! A `*.clpi` file describes one `*.m2ts` clip: its `ProgramInfo` block lists the
//! elementary streams (PID + `stream_coding_type` + per-type coding info) per
//! program sequence. The parser locates that block via the header's
//! `ProgramInfo_start_address` (offset 12), then walks every program sequence's
//! stream entries into [`crate::stream::TsStream`]s. Pressed BD-ROM clips carry
//! exactly one program sequence; recorder-authored clips can carry several, and
//! their streams merge into the one PID-keyed map (a PID redeclared by a later
//! sequence overwrites the earlier entry — the report models a clip as one
//! stream set per PID).
//!
//! Every file byte is read through the bounds-checked [`super`] helpers, so a
//! short or truncated file surfaces as [`BdError`] rather than a panic (disc
//! bytes are never indexed raw; malformed input → `Result`). Disc offsets
//! advance with `saturating_add` (an out-of-range offset then fails the next
//! bounds-checked read as [`BdError::UnexpectedEof`]); the small coding-info
//! nibble math uses fixed-width `wrapping_*` arithmetic.

use std::collections::BTreeMap;

use super::{ascii, byte, u16_be, u32_off};
use crate::error::BdError;
use crate::primitives::Pid;
use crate::stream::{
    TsAspectRatio, TsAudioStream, TsChannelLayout, TsFrameRate, TsGraphicsStream, TsSampleRate,
    TsStream, TsStreamType, TsTextStream, TsVideoFormat, TsVideoStream,
};

/// One playlist clip.
///
/// A clip is created by the playlist parser ([`super::mpls`]) for each
/// `PlayItem` and angle; it records the clip's name and its timing within the
/// playlist. The demux-derived fields (`file_size`, `packet_count`,
/// `packet_seconds`, …) are left at their defaults by the parse and filled in
/// by the disc-level packet scan.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TsStreamClip {
    /// Angle index for multi-angle titles; `0` is the main angle.
    pub angle_index: i32,
    /// Clip name, e.g. `"00000.M2TS"`.
    pub name: String,
    /// Clip start time within its `*.m2ts`, seconds.
    pub time_in: f64,
    /// Clip end time within its `*.m2ts`, seconds.
    pub time_out: f64,
    /// Start time relative to the whole playlist, seconds.
    pub relative_time_in: f64,
    /// End time relative to the whole playlist, seconds.
    pub relative_time_out: f64,
    /// Clip duration, seconds (`time_out - time_in`).
    pub length: f64,
    /// Clip duration as a fraction of the playlist.
    pub relative_length: f64,
    /// Size of the `*.m2ts` in bytes; filled by the packet scan.
    pub file_size: u64,
    /// Size of the interleaved `*.ssif` in bytes; filled by the packet scan.
    pub interleaved_file_size: u64,
    /// Total payload bytes seen by the demuxer.
    pub payload_bytes: u64,
    /// Transport packets seen by the demuxer.
    pub packet_count: u64,
    /// Demuxed duration in seconds.
    pub packet_seconds: f64,
    /// Chapter times within this clip, seconds.
    pub chapters: Vec<f64>,
}

/// A parsed clip-information file.
#[derive(Debug, Clone, PartialEq)]
pub struct TsStreamClipFile {
    /// The 8-byte type magic, e.g. `"HDMV0300"`.
    pub file_type: String,
    /// Whether the file scanned successfully; always `true` here,
    /// since a failed scan returns [`BdError`] instead.
    pub is_valid: bool,
    /// The upper-cased file name, e.g. `"00000.CLPI"`.
    pub name: String,
    /// The clip's elementary streams, keyed by PID.
    pub streams: BTreeMap<u16, TsStream>,
}

impl TsStreamClipFile {
    /// Parses a `*.clpi` file's `data` (named `name`) into the model.
    ///
    /// # Errors
    /// Returns [`BdError::UnknownFileType`] if the 8-byte magic is not
    /// `HDMV0100`/`HDMV0200`/`HDMV0240`/`HDMV0300`, or [`BdError::UnexpectedEof`] if any
    /// field runs past the end of `data` (a truncated or malformed file).
    pub fn scan(name: &str, data: &[u8]) -> Result<Self, BdError> {
        let file_type = ascii(data, 0, 8)?;
        if !matches!(file_type.as_str(), "HDMV0100" | "HDMV0200" | "HDMV0240" | "HDMV0300") {
            return Err(BdError::UnknownFileType(file_type));
        }

        // ProgramInfo_start_address (offset 12) → its 4-byte length → the block.
        let clip_index = u32_off(data, 12)?;
        let clip_length = u32_off(data, clip_index)?;
        let clip_start = clip_index.saturating_add(4);
        let clip_end = clip_start.saturating_add(clip_length);
        let clip_data = data.get(clip_start..clip_end).ok_or(BdError::UnexpectedEof)?;

        // ProgramInfo: a reserved byte (block offset 0), then the
        // program-sequence count; each program carries an 8-byte header
        // (`spn_program_sequence_start` + `program_map_pid` + the stream
        // count + `num_groups`) followed by its stream entries. Streams from
        // every program sequence merge into the one PID-keyed map; a PID
        // redeclared by a later sequence overwrites the earlier entry.
        let num_prog = byte(clip_data, 1)?;
        let mut streams = BTreeMap::new();
        let mut prog_offset: usize = 2;
        for _ in 0..num_prog {
            let stream_count = byte(clip_data, prog_offset.saturating_add(6))?;
            let mut stream_offset = prog_offset.saturating_add(8);
            for _ in 0..stream_count {
                let pid = u16_be(clip_data, stream_offset)?;
                // Advance past the 2-byte PID — `info` is now the coding-info
                // length byte; the stream_coding_type is the byte after it. The
                // length is read first (before the type) to keep both reads
                // fallible — reading the type first would make the length read
                // provably in-bounds and so an unreachable error path.
                let info = stream_offset.saturating_add(2);
                let len = byte(clip_data, info)?;
                let type_byte = byte(clip_data, info.saturating_add(1))?;
                let stream_type = TsStreamType::from_u8(type_byte);

                if let Some(mut stream) = build_clip_stream(clip_data, info, stream_type)? {
                    stream.base_mut().pid = Pid::new(pid);
                    stream.base_mut().stream_type = stream_type;
                    streams.insert(pid, stream);
                }

                // The next entry starts after the length byte plus its `len`
                // coding-info bytes (`len` ≤ 255, so the `+ 1` cannot overflow
                // the fixed-width wrapping_add).
                stream_offset = info.saturating_add(usize::from(len).wrapping_add(1));
            }
            // The next program's header starts where this program's entries end.
            prog_offset = stream_offset;
        }

        Ok(Self { file_type, is_valid: true, name: name.to_ascii_uppercase(), streams })
    }
}

/// Builds the [`TsStream`] for one clip-info entry whose coding-info length byte
/// is at `info` (just past the PID), or `None` for stream types with no decoded
/// representation (MVC dependent video and any unknown code).
fn build_clip_stream(
    clip_data: &[u8],
    info: usize,
    stream_type: TsStreamType,
) -> Result<Option<TsStream>, BdError> {
    Ok(match stream_type {
        // MVC is a recognised coding type that carries no decoded fields, so it
        // stays a distinct arm (the place dependent-view handling would slot in)
        // rather than being folded into the Unknown default.
        #[expect(
            clippy::match_same_arms,
            reason = "MVC is a recognised coding type, deliberately not the Unknown default"
        )]
        TsStreamType::MvcVideo => None,
        TsStreamType::HevcVideo
        | TsStreamType::AvcVideo
        | TsStreamType::Mpeg1Video
        | TsStreamType::Mpeg2Video
        | TsStreamType::Vc1Video => {
            let format = byte(clip_data, info.saturating_add(2))?;
            let aspect = byte(clip_data, info.saturating_add(3))?;
            // The setters (deriving height/scan + the frame-rate ratio) run first,
            // so the trailing `aspect_ratio` field write isn't a default-reassign.
            let mut stream = TsVideoStream::default();
            stream.set_video_format(TsVideoFormat::from_u8(format.wrapping_shr(4)));
            stream.set_frame_rate(TsFrameRate::from_u8(format & 0x0F));
            stream.aspect_ratio = TsAspectRatio::from_u8(aspect.wrapping_shr(4));
            Some(TsStream::Video(stream))
        }
        TsStreamType::Ac3Audio
        | TsStreamType::Ac3PlusAudio
        | TsStreamType::Ac3PlusSecondaryAudio
        | TsStreamType::Ac3TrueHdAudio
        | TsStreamType::DtsAudio
        | TsStreamType::DtsHdAudio
        | TsStreamType::DtsHdMasterAudio
        | TsStreamType::DtsHdSecondaryAudio
        | TsStreamType::LpcmAudio
        | TsStreamType::Mpeg1Audio
        | TsStreamType::Mpeg2Audio
        | TsStreamType::Mpeg2AacAudio
        | TsStreamType::Mpeg4AacAudio => {
            let format = byte(clip_data, info.saturating_add(2))?;
            let language = ascii(clip_data, info.saturating_add(3), 3)?;
            let mut stream = TsAudioStream {
                channel_layout: TsChannelLayout::from_u8(format.wrapping_shr(4)),
                sample_rate: TsAudioStream::convert_sample_rate(TsSampleRate::from_u8(
                    format & 0x0F,
                )),
                ..TsAudioStream::default()
            };
            stream.base.set_language_code(&language);
            Some(TsStream::Audio(stream))
        }
        TsStreamType::InteractiveGraphics | TsStreamType::PresentationGraphics => {
            let language = ascii(clip_data, info.saturating_add(2), 3)?;
            let mut stream = TsGraphicsStream::default();
            stream.base.set_language_code(&language);
            Some(TsStream::Graphics(stream))
        }
        TsStreamType::Subtitle => {
            let language = ascii(clip_data, info.saturating_add(3), 3)?;
            let mut stream = TsTextStream::default();
            stream.base.set_language_code(&language);
            Some(TsStream::Text(stream))
        }
        TsStreamType::Unknown => None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::{any, proptest};

    use super::{TsStreamClip, TsStreamClipFile};
    use crate::primitives::Pid;
    use crate::stream::{
        TsAspectRatio, TsAudioStream, TsChannelLayout, TsFrameRate, TsGraphicsStream, TsStream,
        TsStreamType, TsTextStream, TsVideoFormat, TsVideoStream,
    };

    /// One per-program 8-byte header: `spn_program_sequence_start` +
    /// `program_map_pid` + the stream count + `num_groups`. The fields around
    /// the count are non-zero filler, so an offset slip lands on visibly
    /// different bytes.
    fn program_header(stream_count: usize) -> Vec<u8> {
        let mut header = 0x0102_0304_u32.to_be_bytes().to_vec(); // spn start
        header.extend_from_slice(&0x0100_u16.to_be_bytes()); // program_map_pid
        header.push(u8::try_from(stream_count).unwrap());
        header.push(1); // num_groups
        header
    }

    /// One stream entry: `[PID:2][len=5][coding_type][payload:4]`, where
    /// `payload` is the four bytes after the coding-type byte.
    fn entry_bytes(pid: u16, coding_type: u8, payload: [u8; 4]) -> Vec<u8> {
        let mut entry = pid.to_be_bytes().to_vec();
        entry.push(5); // coding-info length (coding_type + 4 payload bytes)
        entry.push(coding_type);
        entry.extend_from_slice(&payload);
        entry
    }

    /// Builds a minimal valid `*.clpi`: header → `ProgramInfo` at offset 16 →
    /// one program sequence carrying `entries`.
    fn build_clpi(magic: [u8; 8], entries: &[(u16, u8, [u8; 4])]) -> Vec<u8> {
        let mut clip_data = vec![0_u8]; // index 0: reserved byte
        clip_data.push(1); // index 1: num_prog
        clip_data.extend(program_header(entries.len())); // 2..10; entries at 10
        for &(pid, coding_type, payload) in entries {
            clip_data.extend(entry_bytes(pid, coding_type, payload));
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(&magic); // 0..8
        buf.extend_from_slice(&[0_u8; 4]); // 8..12 (SequenceInfo addr, unused)
        buf.extend_from_slice(&16_u32.to_be_bytes()); // 12..16 ProgramInfo addr
        buf.extend_from_slice(&u32::try_from(clip_data.len()).unwrap().to_be_bytes()); // 16..20 length
        buf.extend_from_slice(&clip_data); // 20..
        buf
    }

    #[test]
    fn scan_parses_every_stream_kind() {
        // video, audio, graphics, subtitle => streams; MVC + unknown => skipped.
        let entries = [
            (0x1011_u16, 0x1B_u8, [0x62, 0x30, 0x00, 0x00]), // AVC: 1080p/24, 16:9
            (0x1100, 0x86, [0x61, b'd', b'e', b'u']),        // DTS-HD MA: multi/48k deu
            (0x1200, 0x90, [b'e', b'n', b'g', 0x00]),        // PG: eng
            (0x1A00, 0x92, [0x00, b'f', b'r', b'a']),        // Subtitle: fra
            (0x1B00, 0x20, [0x00, 0x00, 0x00, 0x00]),        // MVC: no stream emitted
            (0x1C00, 0x99, [0x00, 0x00, 0x00, 0x00]),        // unknown type => skipped
        ];
        let buf = build_clpi(*b"HDMV0300", &entries);
        let file = TsStreamClipFile::scan("00000.clpi", &buf).unwrap();

        assert_eq!(file.file_type, "HDMV0300");
        assert!(file.is_valid);
        assert_eq!(file.name, "00000.CLPI"); // upper-cased

        // Build the exact streams expected from the bytes above and compare the
        // whole map — MVC + the unknown type produce nothing, so neither appears.
        let mut expected: BTreeMap<u16, TsStream> = BTreeMap::new();

        let mut video = TsVideoStream::default();
        video.set_video_format(TsVideoFormat::Videoformat1080p); // 0x62 >> 4 == 6
        video.set_frame_rate(TsFrameRate::Framerate24); // 0x62 & 0x0F == 2
        video.aspect_ratio = TsAspectRatio::Aspect16_9; // 0x30 >> 4 == 3
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::AvcVideo;
        expected.insert(0x1011, TsStream::Video(video));

        let mut audio = TsAudioStream {
            channel_layout: TsChannelLayout::ChannellayoutMulti, // 0x61 >> 4 == 6
            sample_rate: 48_000,                                 // 0x61 & 0x0F == 1 -> 48k
            ..TsAudioStream::default()
        };
        audio.base.set_language_code("deu");
        audio.base.pid = Pid::new(0x1100);
        audio.base.stream_type = TsStreamType::DtsHdMasterAudio;
        expected.insert(0x1100, TsStream::Audio(audio));

        let mut graphics = TsGraphicsStream::default();
        graphics.base.set_language_code("eng");
        graphics.base.pid = Pid::new(0x1200);
        graphics.base.stream_type = TsStreamType::PresentationGraphics;
        expected.insert(0x1200, TsStream::Graphics(graphics));

        let mut text = TsTextStream::default();
        text.base.set_language_code("fra");
        text.base.pid = Pid::new(0x1A00);
        text.base.stream_type = TsStreamType::Subtitle;
        expected.insert(0x1A00, TsStream::Text(text));

        assert_eq!(file.streams, expected);
    }

    #[test]
    fn scan_accepts_all_four_magics() {
        for magic in [*b"HDMV0100", *b"HDMV0200", *b"HDMV0240", *b"HDMV0300"] {
            let buf = build_clpi(magic, &[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
            let file = TsStreamClipFile::scan("x.clpi", &buf).unwrap();
            assert_eq!(file.streams.len(), 1);
        }
    }

    #[test]
    fn scan_walks_every_program_sequence() {
        // Two program sequences, each declaring one stream. A single-program
        // parse would stop after the first and miss the second PID.
        let mut clip_data = vec![0_u8, 2]; // reserved + num_prog = 2
        clip_data.extend(program_header(1));
        clip_data.extend(entry_bytes(0x1011, 0x1B, [0x62, 0x30, 0, 0]));
        clip_data.extend(program_header(1));
        clip_data.extend(entry_bytes(0x1100, 0x86, [0x61, b'd', b'e', b'u']));
        let file = TsStreamClipFile::scan("p.clpi", &wrap_clip_data(&clip_data)).unwrap();
        let pids: Vec<u16> = file.streams.keys().copied().collect();
        assert_eq!(pids, vec![0x1011, 0x1100]);
        assert_eq!(
            file.streams.get(&0x1100).map(TsStream::stream_type),
            Some(TsStreamType::DtsHdMasterAudio)
        );
    }

    #[test]
    fn a_pid_redeclared_by_a_later_program_overwrites_the_earlier_entry() {
        // The same PID in two program sequences with different coding: the
        // later sequence's declaration wins (the documented merge rule).
        let mut clip_data = vec![0_u8, 2];
        clip_data.extend(program_header(1));
        clip_data.extend(entry_bytes(0x1011, 0x1B, [0x62, 0x30, 0, 0])); // AVC
        clip_data.extend(program_header(1));
        clip_data.extend(entry_bytes(0x1011, 0x24, [0x62, 0x30, 0, 0])); // HEVC
        let file = TsStreamClipFile::scan("p.clpi", &wrap_clip_data(&clip_data)).unwrap();
        assert_eq!(file.streams.len(), 1);
        assert_eq!(
            file.streams.get(&0x1011).map(TsStream::stream_type),
            Some(TsStreamType::HevcVideo)
        );
    }

    #[test]
    fn a_clip_with_zero_program_sequences_has_no_streams() {
        // num_prog == 0: nothing past the count is read — in particular the
        // byte at the old fixed stream-count offset (8) is junk here, not a
        // count, and the entry-shaped bytes after it must not parse.
        let mut clip_data = vec![0_u8, 0]; // reserved + num_prog = 0
        clip_data.extend_from_slice(&[0_u8; 6]);
        clip_data.push(1); // junk where the single-program layout put the count
        clip_data.push(0);
        clip_data.extend(entry_bytes(0x1011, 0x1B, [0x62, 0x30, 0, 0]));
        let file = TsStreamClipFile::scan("p.clpi", &wrap_clip_data(&clip_data)).unwrap();
        assert!(file.streams.is_empty());
    }

    #[test]
    fn scan_rejects_unknown_file_type() {
        let buf = build_clpi(*b"XXXX0100", &[]);
        // `BdError` no longer derives `PartialEq` (its `Io` wraps `io::Error`), so the
        // error assertions check the failure's `Display` — region-clean and pinning
        // the message text the codec/report layers surface to the CLI.
        assert_eq!(
            TsStreamClipFile::scan("bad.clpi", &buf).unwrap_err().to_string(),
            "unknown file type: XXXX0100"
        );
    }

    #[test]
    fn scan_rejects_truncated_inputs() {
        let eof = "unexpected end of input";
        // empty => magic read fails.
        assert_eq!(TsStreamClipFile::scan("e", &[]).unwrap_err().to_string(), eof);
        // valid magic but no ProgramInfo offset at 12.
        assert_eq!(TsStreamClipFile::scan("e", b"HDMV0100").unwrap_err().to_string(), eof);
        // ProgramInfo offset present but the block length read runs off the end.
        let mut buf = b"HDMV0100".to_vec();
        buf.extend_from_slice(&[0_u8; 4]); // 8..12
        buf.extend_from_slice(&100_u32.to_be_bytes()); // clip_index = 100 (out of range)
        assert_eq!(TsStreamClipFile::scan("e", &buf).unwrap_err().to_string(), eof);
        // clip block truncated: index/length point past the end of the file.
        let mut buf = b"HDMV0100".to_vec();
        buf.extend_from_slice(&[0_u8; 4]);
        buf.extend_from_slice(&16_u32.to_be_bytes()); // clip_index = 16
        buf.extend_from_slice(&100_u32.to_be_bytes()); // clip_length = 100, no data
        assert_eq!(TsStreamClipFile::scan("e", &buf).unwrap_err().to_string(), eof);
        // num_prog byte missing (clip block of one reserved byte only).
        let mut buf = b"HDMV0100".to_vec();
        buf.extend_from_slice(&[0_u8; 4]);
        buf.extend_from_slice(&16_u32.to_be_bytes());
        buf.extend_from_slice(&1_u32.to_be_bytes()); // clip_length = 1
        buf.push(0); // clip_data of 1 byte (no index 1)
        assert_eq!(TsStreamClipFile::scan("e", &buf).unwrap_err().to_string(), eof);
        // stream-count byte missing (program header cut before its count).
        let mut buf = b"HDMV0100".to_vec();
        buf.extend_from_slice(&[0_u8; 4]);
        buf.extend_from_slice(&16_u32.to_be_bytes());
        buf.extend_from_slice(&5_u32.to_be_bytes()); // clip_length = 5
        buf.extend_from_slice(&[0, 1, 0, 0, 0]); // num_prog = 1, header truncated
        assert_eq!(TsStreamClipFile::scan("e", &buf).unwrap_err().to_string(), eof);
        // stream count says 1 but the entry is missing.
        let mut buf = b"HDMV0100".to_vec();
        buf.extend_from_slice(&[0_u8; 4]);
        buf.extend_from_slice(&16_u32.to_be_bytes());
        buf.extend_from_slice(&10_u32.to_be_bytes()); // clip_length = 10 (header only)
        let mut header = vec![0_u8; 10];
        header.splice(1..2, [1_u8]); // num_prog = 1
        header.splice(8..9, [1_u8]); // stream count = 1, but no entry follows
        buf.extend_from_slice(&header);
        assert_eq!(TsStreamClipFile::scan("e", &buf).unwrap_err().to_string(), eof);
    }

    /// Wraps raw `clip_data` bytes in a valid CLPI envelope (magic + offsets),
    /// `ProgramInfo` at offset 16.
    fn wrap_clip_data(clip_data: &[u8]) -> Vec<u8> {
        let clip_index: u32 = 16;
        let mut buf = Vec::new();
        buf.extend_from_slice(b"HDMV0300");
        buf.extend_from_slice(&[0_u8; 4]);
        buf.extend_from_slice(&clip_index.to_be_bytes());
        buf.extend_from_slice(&u32::try_from(clip_data.len()).unwrap().to_be_bytes());
        buf.extend_from_slice(clip_data);
        buf
    }

    /// One-entry `clip_data` (one program, stream count 1) truncated to
    /// `clip_data_len`, so a read at one specific field offset runs off the end.
    fn one_entry_truncated(coding_type: u8, clip_data_len: usize) -> Vec<u8> {
        let mut full = vec![0_u8; 10];
        full.splice(1..2, [1_u8]); // num_prog = 1
        full.splice(8..9, [1_u8]); // stream count = 1
        full.extend_from_slice(&0x1011_u16.to_be_bytes()); // PID
        full.push(5); // coding-info length
        full.push(coding_type);
        full.extend_from_slice(&[0_u8; 8]); // coding-info payload
        full.truncate(clip_data_len);
        wrap_clip_data(&full)
    }

    #[test]
    fn scan_rejects_truncation_within_each_stream_entry() {
        // Each (coding_type, clip_data_len) truncates just before a distinct
        // bounds-checked read, exercising every per-field EOF path.
        let cases = [
            (0x1B_u8, 10_usize), // PID read (u16 at 10)
            (0x1B, 12),          // coding-info length byte (at 12)
            (0x1B, 13),          // stream_coding_type byte (at 13)
            (0x1B, 14),          // video: format/frame-rate byte
            (0x1B, 15),          // video: aspect-ratio byte
            (0x81, 14),          // audio: channel/sample byte
            (0x81, 15),          // audio: 3-byte language
            (0x90, 14),          // graphics: 3-byte language
            (0x92, 14),          // subtitle: 3-byte language
        ];
        for (coding_type, clip_data_len) in cases {
            let buf = one_entry_truncated(coding_type, clip_data_len);
            assert_eq!(
                TsStreamClipFile::scan("t.clpi", &buf).unwrap_err().to_string(),
                "unexpected end of input",
                "type {coding_type:#04X} len {clip_data_len}"
            );
        }
    }

    #[test]
    fn ts_stream_clip_model_defaults_and_clone() {
        let clip = TsStreamClip {
            name: "00000.M2TS".to_owned(),
            time_in: 600.0,
            time_out: 608.125,
            length: 8.125,
            chapters: vec![600.0],
            ..TsStreamClip::default()
        };
        assert_eq!(clip.name, "00000.M2TS");
        assert_eq!(clip.angle_index, 0);
        assert_eq!(clip.length.to_bits(), 8.125_f64.to_bits());
        assert_eq!(clip.chapters, vec![600.0]);
        assert_eq!(clip.clone(), clip);
        // Pure-default clip (all zero / empty).
        let d = TsStreamClip::default();
        assert_eq!(d.name, "");
        assert_eq!(d.file_size, 0);
        assert!(d.chapters.is_empty());
        assert!(format!("{d:?}").starts_with("TsStreamClip"));
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_input(data in any::<Vec<u8>>()) {
            // Whatever the bytes, scan returns Ok or Err — never panics.
            drop(TsStreamClipFile::scan("fuzz.clpi", &data));
        }

        #[test]
        fn scan_round_trips_built_clips(
            pid in any::<u16>(),
            format in any::<u8>(),
            aspect in any::<u8>(),
        ) {
            let buf = build_clpi(*b"HDMV0300", &[(pid, 0x1B, [format, aspect, 0, 0])]);
            let file = TsStreamClipFile::scan("p.clpi", &buf).unwrap();
            let stream = file.streams.get(&pid).unwrap();
            assert_eq!(stream.pid(), Pid::new(pid));
            assert_eq!(stream.stream_type(), TsStreamType::AvcVideo);
        }
    }
}
