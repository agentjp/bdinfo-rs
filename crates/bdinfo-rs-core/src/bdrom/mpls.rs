//! Playlist (`*.mpls`) parser.
//!
//! A `*.mpls` playlist sequences clips (`PlayItem`s) into a presentation: each
//! item names a `*.m2ts`/`*.clpi` pair, its in/out times, optional camera angles,
//! and a Stream-Number table of the elementary streams to present. A trailing
//! `PlayListMark` table lists the chapter marks. [`TsPlaylistFile::scan`] walks
//! all of that into the model: the [`TsStreamClip`] list, the playlist streams,
//! the chapter times, the angle count, and the `MVC_Base_view_R` flag.
//!
//! This is the *parse* half of playlist handling. The cross-clip resolution
//! that needs the M2TS demux — picking a reference clip and cloning its streams
//! into [`streams`](TsPlaylistFile::streams) — happens in the disc-level scan
//! ([`super::disc`]); the standalone parse records each clip's name and leaves
//! the resolved maps empty.
//!
//! Parsing notes:
//! * Presentation times mask off bit 31 unconditionally (`t & 0x7FFF_FFFF`) — the high bit is not
//!   part of the 45 kHz timestamp — keeping the read in panic-free unsigned arithmetic
//!   ([`read_time`]).
//! * Each item ends at `item_start + item_length + 2` regardless of how far the entry walk
//!   advanced, so the loop jumps there in closed form.
//! * Offsets advance with `saturating_add`; an out-of-range offset then fails the next
//!   bounds-checked read as [`BdError::UnexpectedEof`] (disc bytes are never indexed raw). Time
//!   math is plain `f64` arithmetic.

use std::collections::BTreeMap;

use super::clpi::TsStreamClip;
use super::{ascii, byte, u16_be, u32_be};
use crate::error::BdError;
use crate::primitives::Pid;
use crate::stream::{
    TsAudioStream, TsChannelLayout, TsFrameRate, TsGraphicsStream, TsSampleRate, TsStream,
    TsStreamType, TsTextStream, TsVideoFormat, TsVideoStream,
};

/// A parsed playlist file.
///
/// The parse fills the clip list, the declared streams, and the chapter times.
/// [`streams`](Self::streams) and [`angle_streams`](Self::angle_streams) are
/// the resolved maps the demux writes per-stream bitrates into; the parser
/// leaves them empty and the disc-level scan populates them (an unresolved
/// playlist makes the demux's per-stream writes a no-op).
#[derive(Debug, Clone, PartialEq)]
pub struct TsPlaylistFile {
    /// The 8-byte type magic, e.g. `"MPLS0300"`.
    pub file_type: String,
    /// The upper-cased file name, e.g. `"00000.MPLS"`.
    pub name: String,
    /// The `MVC_Base_view_R_flag` from the playlist's misc flags.
    pub mvc_base_view_r: bool,
    /// Chapter times relative to the whole playlist, seconds.
    pub chapters: Vec<f64>,
    /// The Stream-Number-table streams declared by the play items, keyed by PID.
    pub playlist_streams: BTreeMap<u16, TsStream>,
    /// The presented streams keyed by PID, filled with bitrates by the demux.
    /// Empty until the disc scan resolves the reference clip; empty makes the
    /// demux's per-stream writes no-ops.
    pub streams: BTreeMap<u16, TsStream>,
    /// Per-angle copies of the video streams keyed by PID; one map per extra
    /// camera angle. Empty until the disc scan clones the angle streams.
    pub angle_streams: Vec<BTreeMap<u16, TsStream>>,
    /// The clips the playlist sequences, in order, including angle clips.
    pub stream_clips: Vec<TsStreamClip>,
    /// The number of extra camera angles.
    pub angle_count: i32,
}

impl TsPlaylistFile {
    /// Parses a `*.mpls` file's `data` (named `name`) into the model.
    ///
    /// # Errors
    /// Returns [`BdError::UnknownFileType`] if the 8-byte magic is not
    /// `MPLS0100`/`MPLS0200`/`MPLS0240`/`MPLS0300`, or [`BdError::UnexpectedEof`] if any
    /// field runs past the end of `data` (a truncated or malformed file).
    #[expect(
        clippy::too_many_lines,
        reason = "the scan is one sequential walk of the playlist layout; splitting it would scatter the offset bookkeeping"
    )]
    pub fn scan(name: &str, data: &[u8]) -> Result<Self, BdError> {
        let file_type = ascii(data, 0, 8)?;
        if !matches!(file_type.as_str(), "MPLS0100" | "MPLS0200" | "MPLS0240" | "MPLS0300") {
            return Err(BdError::UnknownFileType(file_type));
        }

        let playlist_offset = usize::try_from(u32_be(data, 8)?).unwrap_or(usize::MAX);
        let chapters_offset = usize::try_from(u32_be(data, 12)?).unwrap_or(usize::MAX);
        // The extensions offset (at 16) plays no part in the model — skipped.

        // misc flags at 0x38; MVC_Base_view_R_flag is bit 4.
        let misc_flags = byte(data, 0x38)?;
        let mvc_base_view_r = (misc_flags & 0x10) != 0;

        // PlayList: length (4) + reserved (2) + item count (2) + sub-item count (2).
        let item_count = u16_be(data, playlist_offset.saturating_add(6))?;
        let mut pos = playlist_offset.saturating_add(10);

        let mut stream_clips: Vec<TsStreamClip> = Vec::new();
        let mut chapter_clips: Vec<usize> = Vec::new();
        let mut playlist_streams: BTreeMap<u16, TsStream> = BTreeMap::new();
        let mut angle_count: i32 = 0;

        for _ in 0..item_count {
            let item_start = pos;
            let item_length = usize::from(u16_be(data, item_start)?);
            let item_name = ascii(data, item_start.saturating_add(2), 5)?;
            // The 4-byte codec id at +7 selects the clip's stream-file
            // extension: `FMTS` clips live in `*.FMTS`, everything else in
            // `*.M2TS` (libbluray navigation.c `_fill_clip`).
            let codec_id = ascii(data, item_start.saturating_add(7), 4)?;
            let flags = byte(data, item_start.saturating_add(12))?;
            let is_multiangle = (flags.wrapping_shr(4) & 0x01) != 0;
            let time_in = read_time(data, item_start.saturating_add(14))?;
            let time_out = read_time(data, item_start.saturating_add(18))?;

            // Build the main clip; relative_time_in is the playlist length so far.
            let relative_time_in = total_length(&stream_clips);
            let length = time_out - time_in;
            let relative_time_out = relative_time_in + length;
            let relative_length = length / relative_time_in;
            let clip = TsStreamClip {
                name: format!("{item_name}{}", clip_extension(&codec_id)),
                time_in,
                time_out,
                length,
                relative_time_in,
                relative_time_out,
                relative_length,
                ..TsStreamClip::default()
            };
            let main_index = stream_clips.len();
            stream_clips.push(clip);
            chapter_clips.push(main_index);

            // The fixed item header runs to item_start + 34 (after the 12-byte
            // skip past the out-time); the angle table, if any, follows.
            let mut pos_item = item_start.saturating_add(34);
            if is_multiangle {
                let angles = byte(data, pos_item)?;
                pos_item = pos_item.saturating_add(2);
                for angle in 0..angles.saturating_sub(1) {
                    // angle name (5) + type (4) + 1 reserved byte. The angle clip is
                    // named after its own `*.m2ts`, so the disc scan can resolve the
                    // angle's stream/clip files and fold its size into `file_size`.
                    let angle_name = ascii(data, pos_item, 5)?;
                    let angle_codec = ascii(data, pos_item.saturating_add(5), 4)?;
                    pos_item = pos_item.saturating_add(10);
                    stream_clips.push(TsStreamClip {
                        angle_index: i32::from(angle.wrapping_add(1)),
                        name: format!("{angle_name}{}", clip_extension(&angle_codec)),
                        time_in,
                        time_out,
                        relative_time_in,
                        relative_time_out,
                        length,
                        ..TsStreamClip::default()
                    });
                }
                // The playlist's angle count is the running maximum of the
                // per-item extra-angle counts.
                let extra_angles = i32::from(angles.saturating_sub(1));
                angle_count = angle_count.max(extra_angles);
            }

            // Stream-Number table: stream-info length (2) + reserved (2) + 7 count
            // bytes + 5 reserved, then the per-stream entries.
            // The length field is read (so truncation errors) but unused.
            u16_be(data, pos_item)?;
            let count_video = byte(data, pos_item.saturating_add(4))?;
            let count_audio = byte(data, pos_item.saturating_add(5))?;
            let count_presentation = byte(data, pos_item.saturating_add(6))?;
            let count_interactive = byte(data, pos_item.saturating_add(7))?;
            let count_secondary_audio = byte(data, pos_item.saturating_add(8))?;
            let count_secondary_video = byte(data, pos_item.saturating_add(9))?;
            let count_pip = byte(data, pos_item.saturating_add(10))?;
            pos = pos_item.saturating_add(16);

            for _ in 0..count_video {
                pos = add_playlist_stream(data, pos, &mut playlist_streams, relative_length)?;
            }
            for _ in 0..count_audio {
                pos = add_playlist_stream(data, pos, &mut playlist_streams, relative_length)?;
            }
            for _ in 0..count_presentation {
                pos = add_playlist_stream(data, pos, &mut playlist_streams, relative_length)?;
            }
            // PiP-PG entries sit in-line in the PG section, before the IG
            // entries; they are consumed to keep the walk aligned but not
            // modelled.
            for _ in 0..count_pip {
                let (_, next) = create_playlist_stream(data, pos)?;
                pos = next;
            }
            for _ in 0..count_interactive {
                pos = add_playlist_stream(data, pos, &mut playlist_streams, relative_length)?;
            }
            for _ in 0..count_secondary_audio {
                pos = add_playlist_stream(data, pos, &mut playlist_streams, relative_length)?;
                // One comb-info block: the primary-audio refs.
                pos = skip_comb_info(data, pos)?;
            }
            for _ in 0..count_secondary_video {
                pos = add_playlist_stream(data, pos, &mut playlist_streams, relative_length)?;
                // Two comb-info blocks: the secondary-audio refs, then the
                // PiP-PG refs.
                pos = skip_comb_info(data, pos)?;
                pos = skip_comb_info(data, pos)?;
            }
            // The next item starts at item_start + item_length + 2, however far
            // the entry walk advanced.
            pos = item_start.saturating_add(item_length).saturating_add(2);
        }

        // PlayListMark: 4-byte length, then a u16 count, then 14-byte entries.
        let mut chapters: Vec<f64> = Vec::new();
        let mut chap_pos = chapters_offset.saturating_add(4);
        let chapter_count = u16_be(data, chap_pos)?;
        chap_pos = chap_pos.saturating_add(2);
        let final_total = total_length(&stream_clips);
        for _ in 0..chapter_count {
            let chapter_type = byte(data, chap_pos.saturating_add(1))?;
            if chapter_type == 1 {
                let stream_file_index = usize::from(u16_be(data, chap_pos.saturating_add(2))?);
                let chapter_time = f64::from(u32_be(data, chap_pos.saturating_add(4))?);
                let chapter_seconds = chapter_time / 45_000.0;
                // `chapter_clips` maps the file index to a clip slot; an
                // out-of-range index (a corrupt mark) folds to `None` and the
                // mark is skipped rather than failing the whole playlist.
                if let Some(clip) =
                    chapter_clips.get(stream_file_index).and_then(|&i| stream_clips.get_mut(i))
                {
                    let relative_seconds = chapter_seconds - clip.time_in + clip.relative_time_in;
                    if final_total - relative_seconds > 1.0 {
                        clip.chapters.push(chapter_seconds);
                        chapters.push(relative_seconds);
                    }
                }
            }
            // Other chapter-mark types are ignored.
            chap_pos = chap_pos.saturating_add(14);
        }

        Ok(Self {
            file_type,
            name: name.to_ascii_uppercase(),
            mvc_base_view_r,
            chapters,
            playlist_streams,
            // `streams`/`angle_streams` are filled by the disc-level scan, not the
            // parse — empty here, which makes the demux's per-stream bitrate writes
            // a no-op until a reference clip resolves.
            streams: BTreeMap::new(),
            angle_streams: Vec::new(),
            stream_clips,
            angle_count,
        })
    }
}

/// Sum of the main clips' (`angle_index == 0`) lengths — the playlist length
/// accumulated over the clips collected so far.
///
/// Written as an explicit `length = 0.0; for …` accumulation rather than
/// `Iterator::sum`, whose `f64` identity is `-0.0` — that would make the first
/// clip's `length / total` evaluate to `-inf` instead of the defined `+inf`
/// (`40.0 / +0.0`), flipping the sign on the empty-prefix case.
fn total_length(clips: &[TsStreamClip]) -> f64 {
    let mut length = 0.0;
    for clip in clips {
        if clip.angle_index == 0 {
            length += clip.length;
        }
    }
    length
}

/// The stream-file extension a play-item clip's 4-byte codec id selects:
/// `.FMTS` for an `FMTS` codec id, `.M2TS` for everything else (libbluray
/// navigation.c `_fill_clip`). The two share the same 192-byte BDAV transport
/// layout, so only the file the clip resolves to differs.
fn clip_extension(codec_id: &str) -> &'static str {
    if codec_id == "FMTS" { ".FMTS" } else { ".M2TS" }
}

/// Reads a 4-byte presentation time at `off` and converts it to seconds.
///
/// The raw value is masked to its low 31 bits (`t & 0x7FFF_FFFF`) — bit 31 is
/// not part of the 45 kHz timestamp — then divided by 45 000. The mask is a
/// no-op when the bit is clear, so it is applied unconditionally, keeping the
/// read unsigned and panic-free.
fn read_time(data: &[u8], off: usize) -> Result<f64, BdError> {
    let raw = u32_be(data, off)? & 0x7FFF_FFFF;
    Ok(f64::from(raw) / 45_000.0)
}

/// Skips one secondary-stream comb-info block at `pos`: a ref-count byte, a
/// reserved byte, `count` 1-byte refs, and a pad byte when the count is odd.
/// Returns the position after the block.
fn skip_comb_info(data: &[u8], pos: usize) -> Result<usize, BdError> {
    let count = byte(data, pos)?;
    let padded = usize::from(count).saturating_add(usize::from(count & 1));
    Ok(pos.saturating_add(2).saturating_add(padded))
}

/// Parses one Stream-Number-table entry at `pos` and, if it yields a stream,
/// records it in `playlist_streams`: an unseen PID is always inserted, and a
/// duplicate only overwrites when the clip's `relative_length` exceeds `0.01`.
/// Returns the position after the entry.
fn add_playlist_stream(
    data: &[u8],
    pos: usize,
    playlist_streams: &mut BTreeMap<u16, TsStream>,
    relative_length: f64,
) -> Result<usize, BdError> {
    let (stream, new_pos) = create_playlist_stream(data, pos)?;
    if let Some(stream) = stream {
        // The map is keyed by the raw wire PID; read the stream's typed PID back out.
        let pid = stream.base().pid.get();
        if !playlist_streams.contains_key(&pid) || relative_length > 0.01 {
            playlist_streams.insert(pid, stream);
        }
    }
    Ok(new_pos)
}

/// Reads one stream entry's variable header (its `header_type` selects where
/// the PID lives) and the stream-coding info, returning the built [`TsStream`]
/// (or `None` for an unhandled type) and the position immediately after the
/// entry.
fn create_playlist_stream(data: &[u8], pos: usize) -> Result<(Option<TsStream>, usize), BdError> {
    let header_length = byte(data, pos)?;
    let header_pos = pos.saturating_add(1);
    let header_type = byte(data, header_pos)?;
    let pid = match header_type {
        1 => u16_be(data, header_pos.saturating_add(1))?,
        2 => u16_be(data, header_pos.saturating_add(3))?,
        // Type 4 (DV enhancement layer) carries subpath_id + PID, the same
        // shape as type 3.
        3 | 4 => u16_be(data, header_pos.saturating_add(2))?,
        _ => 0,
    };

    // The stream block follows the variable header: its length byte sits at
    // header_pos + header_length, and the coding bytes follow it.
    let length_pos = header_pos.saturating_add(usize::from(header_length));
    let stream_length = byte(data, length_pos)?;
    let stream_pos = length_pos.saturating_add(1);
    let type_byte = byte(data, stream_pos)?;
    let stream_type = TsStreamType::from_u8(type_byte);
    let coding_pos = stream_pos.saturating_add(1);
    let stream = build_playlist_stream(data, coding_pos, stream_type)?;
    let new_pos = stream_pos.saturating_add(usize::from(stream_length));

    let stream = stream.map(|mut stream| {
        stream.base_mut().pid = Pid::new(pid);
        stream.base_mut().stream_type = stream_type;
        stream
    });
    Ok((stream, new_pos))
}

/// Builds the [`TsStream`] for a playlist stream entry whose coding info starts at
/// `coding`, or `None` for stream types with no decoded representation.
fn build_playlist_stream(
    data: &[u8],
    coding: usize,
    stream_type: TsStreamType,
) -> Result<Option<TsStream>, BdError> {
    Ok(match stream_type {
        // MVC is a recognised coding type that yields no stream, kept as a
        // distinct arm rather than folded into the Unknown default.
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
            // MPLS video attributes carry only format + rate; the next byte is
            // dynamic_range_type/color_space on HEVC, not an aspect field
            // (aspect lives in the CLPI stream-coding info), so it is not read.
            let format = byte(data, coding)?;
            let mut stream = TsVideoStream::default();
            stream.set_video_format(TsVideoFormat::from_u8(format.wrapping_shr(4)));
            stream.set_frame_rate(TsFrameRate::from_u8(format & 0x0F));
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
            let format = byte(data, coding)?;
            let language = ascii(data, coding.saturating_add(1), 3)?;
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
            let language = ascii(data, coding, 3)?;
            let mut stream = TsGraphicsStream::default();
            stream.base.set_language_code(&language);
            Some(TsStream::Graphics(stream))
        }
        TsStreamType::Subtitle => {
            // A 1-byte code field (unused) precedes the language, so the
            // language sits one byte past `coding`.
            let language = ascii(data, coding.saturating_add(1), 3)?;
            let mut stream = TsTextStream::default();
            stream.base.set_language_code(&language);
            Some(TsStream::Text(stream))
        }
        TsStreamType::Unknown => None,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, proptest};

    use super::TsPlaylistFile;
    use crate::primitives::Pid;
    use crate::stream::{
        TsAspectRatio, TsAudioStream, TsChannelLayout, TsFrameRate, TsStream, TsStreamType,
        TsVideoFormat, TsVideoStream,
    };

    /// One Stream-Number-table entry: a 9-byte header block (PID placed per
    /// `header_type`) + the stream length (5) + the coding type + 4 coding bytes.
    fn pl_stream(header_type: u8, pid: u16, stream_type: u8, coding: [u8; 4]) -> Vec<u8> {
        let [ph, pl] = pid.to_be_bytes();
        let mut header = vec![header_type];
        match header_type {
            1 => header.extend_from_slice(&[ph, pl]),
            2 => header.extend_from_slice(&[0, 0, ph, pl]),
            // Type 4 (DV enhancement layer) shares type 3's subpath_id + PID shape.
            3 | 4 => header.extend_from_slice(&[0, ph, pl]),
            _ => {}
        }
        header.resize(9, 0);
        let mut entry = vec![9]; // header length
        entry.extend_from_slice(&header);
        entry.push(5); // stream length
        entry.push(stream_type);
        entry.extend_from_slice(&coding);
        entry
    }

    /// One secondary-stream comb-info block: the ref count, a reserved byte,
    /// the 1-byte refs, and a pad byte when the count is odd.
    fn comb_info(refs: &[u8]) -> Vec<u8> {
        let mut block = vec![u8::try_from(refs.len()).unwrap(), 0];
        block.extend_from_slice(refs);
        if refs.len() % 2 == 1 {
            block.push(0);
        }
        block
    }

    /// One `PlayItem`. `angles` is `None` for a single-angle item or `Some(names)`
    /// for a multi-angle one (`names.len()` extra angles). `counts` are the seven
    /// SN-table stream counts; `stream_bytes` is the concatenated entries.
    fn build_item(
        name: &str,
        in_time: u32,
        out_time: u32,
        angles: Option<&[&str]>,
        counts: [u8; 7],
        stream_bytes: &[u8],
    ) -> Vec<u8> {
        build_item_codec(name, *b"M2TS", in_time, out_time, angles, counts, stream_bytes)
    }

    /// [`build_item`] with the 4-byte codec id chosen by the caller — `*b"M2TS"`
    /// for an ordinary clip, `*b"FMTS"` for one whose stream file is `*.FMTS`.
    /// The codec id is written for both the item and each angle entry.
    fn build_item_codec(
        name: &str,
        codec: [u8; 4],
        in_time: u32,
        out_time: u32,
        angles: Option<&[&str]>,
        counts: [u8; 7],
        stream_bytes: &[u8],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        let mut name_bytes = name.as_bytes().to_vec();
        name_bytes.resize(5, 0);
        body.extend_from_slice(&name_bytes); // +2..7 item name
        body.extend_from_slice(&codec); // +7..11 item type
        body.push(0); // +11
        body.push(if angles.is_some() { 0x10 } else { 0 }); // +12 multiangle flag
        body.push(0); // +13
        body.extend_from_slice(&in_time.to_be_bytes()); // +14..18
        body.extend_from_slice(&out_time.to_be_bytes()); // +18..22
        body.extend_from_slice(&[0_u8; 12]); // +22..34
        if let Some(angle_names) = angles {
            body.push(u8::try_from(angle_names.len().wrapping_add(1)).unwrap()); // angles byte
            body.push(0); // reserved (pos += 2)
            for angle_name in angle_names {
                let mut ab = angle_name.as_bytes().to_vec();
                ab.resize(5, 0);
                body.extend_from_slice(&ab); // angle name (5)
                body.extend_from_slice(&codec); // angle type (4)
                body.push(0); // reserved (1)
            }
        }
        body.extend_from_slice(&[0_u8; 2]); // stream-info length (unused)
        body.extend_from_slice(&[0_u8; 2]); // reserved
        body.extend_from_slice(&counts); // 7 count bytes
        body.extend_from_slice(&[0_u8; 5]); // reserved
        body.extend_from_slice(stream_bytes);
        let mut item = u16::try_from(body.len()).unwrap().to_be_bytes().to_vec();
        item.extend_from_slice(&body);
        item
    }

    /// A 14-byte `PlayListMark` entry: `chapter_type` at +1, `file_index` at +2,
    /// `chapter_time` at +4.
    fn chapter_entry(chapter_type: u8, file_index: u16, chapter_time: u32) -> Vec<u8> {
        let mut entry = vec![0, chapter_type];
        entry.extend_from_slice(&file_index.to_be_bytes());
        entry.extend_from_slice(&chapter_time.to_be_bytes());
        entry.resize(14, 0);
        entry
    }

    /// Wraps items + chapter entries in a valid MPLS envelope (`PlayList` at
    /// `0x3C`, then `PlayListMark`).
    fn build_mpls(misc_flags: u8, items: &[Vec<u8>], chapters: &[Vec<u8>]) -> Vec<u8> {
        let playlist_offset: usize = 0x3C;
        let mut playlist = Vec::new();
        playlist.extend_from_slice(&[0_u8; 4]); // PlayList length
        playlist.extend_from_slice(&[0_u8; 2]); // reserved
        playlist.extend_from_slice(&u16::try_from(items.len()).unwrap().to_be_bytes());
        playlist.extend_from_slice(&[0_u8; 2]); // sub-item count
        for item in items {
            playlist.extend_from_slice(item);
        }
        let chapters_offset = playlist_offset.wrapping_add(playlist.len());
        let mut mark = Vec::new();
        mark.extend_from_slice(&[0_u8; 4]); // PlayListMark length
        mark.extend_from_slice(&u16::try_from(chapters.len()).unwrap().to_be_bytes());
        for chapter in chapters {
            mark.extend_from_slice(chapter);
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(b"MPLS0300");
        buf.extend_from_slice(&u32::try_from(playlist_offset).unwrap().to_be_bytes());
        buf.extend_from_slice(&u32::try_from(chapters_offset).unwrap().to_be_bytes());
        buf.extend_from_slice(&[0_u8; 4]); // extensions offset (unused)
        buf.resize(0x38, 0);
        buf.push(misc_flags); // misc flags at 0x38
        buf.resize(playlist_offset, 0);
        buf.extend_from_slice(&playlist);
        buf.extend_from_slice(&mark);
        buf
    }

    /// The comprehensive two-item playlist exercised by several tests.
    fn comprehensive_mpls() -> Vec<u8> {
        // Item 0: video + audio + (PG, subtitle) + IG + secondary audio + MVC
        // secondary video — covering header types 1/2/3/4 and every stream kind.
        let mut streams0 = Vec::new();
        streams0.extend(pl_stream(1, 0x1011, 0x1B, [0x62, 0x30, 0, 0])); // AVC 1080p/24 16:9
        streams0.extend(pl_stream(1, 0x1100, 0x86, [0x61, b'd', b'e', b'u'])); // DTS-HD MA deu
        streams0.extend(pl_stream(3, 0x1200, 0x90, [b'e', b'n', b'g', 0])); // PG eng (header 3)
        streams0.extend(pl_stream(1, 0x1A00, 0x92, [0, b'j', b'p', b'n'])); // subtitle jpn
        streams0.extend(pl_stream(2, 0x1300, 0x91, [b'd', b'e', b'u', 0])); // IG deu (header 2)
        streams0.extend(pl_stream(4, 0x1101, 0x81, [0x31, b'f', b'r', b'a'])); // AC3 fra (header 4)
        streams0.extend(comb_info(&[])); // secondary-audio comb block (empty)
        streams0.extend(pl_stream(1, 0x1B01, 0x20, [0, 0, 0, 0])); // MVC -> no stream
        streams0.extend(comb_info(&[])); // secondary-video: secondary-audio refs
        streams0.extend(comb_info(&[])); // secondary-video: PiP-PG refs
        let item0 =
            build_item("00000", 2_700_000, 4_500_000, None, [1, 1, 2, 1, 1, 1, 0], &streams0);

        // Item 1: a second clip reusing PID 0x1011 with DIFFERENT coding, so the
        // relative_length > 0.01 overwrite is observable.
        let streams1 = pl_stream(1, 0x1011, 0x1B, [0x24, 0x20, 0, 0]); // 576i/29.97 4:3
        let item1 =
            build_item("00017", 2_700_000, 3_600_000, None, [1, 0, 0, 0, 0, 0, 0], &streams1);

        let chapters = [
            chapter_entry(1, 0, 2_700_000), // clip 0 @ 60s -> rel 0.0 (added)
            chapter_entry(1, 1, 2_700_000), // clip 1 @ 60s -> rel 40.0 (added)
            chapter_entry(2, 0, 0),         // non-type-1 -> ignored
            chapter_entry(1, 1, 3_555_000), /* clip 1 @ 79s -> rel 59.0; 60-59==1.0 not > 1.0
                                             * (skipped) */
            chapter_entry(1, 99, 2_700_000), // out-of-range clip index -> skipped
        ];
        build_mpls(0x10, &[item0, item1], &chapters)
    }

    #[test]
    fn scan_parses_a_full_playlist() {
        let buf = comprehensive_mpls();
        let file = TsPlaylistFile::scan("00000.mpls", &buf).unwrap();

        assert_eq!(file.file_type, "MPLS0300");
        assert_eq!(file.name, "00000.MPLS");
        assert!(file.mvc_base_view_r); // misc flags 0x10
        assert_eq!(file.angle_count, 0);

        // Two main clips (no angles).
        assert_eq!(file.stream_clips.len(), 2);
        let clip0 = file.stream_clips.first().unwrap();
        assert_eq!(clip0.name, "00000.M2TS");
        assert_eq!(clip0.time_in.to_bits(), 60.0_f64.to_bits());
        assert_eq!(clip0.time_out.to_bits(), 100.0_f64.to_bits());
        assert_eq!(clip0.length.to_bits(), 40.0_f64.to_bits());
        assert_eq!(clip0.relative_time_in.to_bits(), 0.0_f64.to_bits());
        assert_eq!(clip0.relative_time_out.to_bits(), 40.0_f64.to_bits()); // rel_in + length
        assert!(clip0.relative_length.is_infinite()); // length / 0 for the first clip
        assert_eq!(clip0.chapters, vec![60.0]); // one chapter landed in this clip
        let clip1 = file.stream_clips.get(1).unwrap();
        assert_eq!(clip1.name, "00017.M2TS");
        assert_eq!(clip1.relative_time_in.to_bits(), 40.0_f64.to_bits());
        assert_eq!(clip1.relative_time_out.to_bits(), 60.0_f64.to_bits());
        assert_eq!(clip1.relative_length.to_bits(), 0.5_f64.to_bits()); // 20 / 40

        // Playlist streams keyed by PID (MVC produced nothing).
        let pids: Vec<u16> = file.playlist_streams.keys().copied().collect();
        assert_eq!(pids, vec![0x1011, 0x1100, 0x1101, 0x1200, 0x1300, 0x1A00]);
        let kind = |pid: u16| file.playlist_streams.get(&pid).map(TsStream::stream_type);
        assert_eq!(kind(0x1011), Some(TsStreamType::AvcVideo));
        assert_eq!(kind(0x1100), Some(TsStreamType::DtsHdMasterAudio));
        assert_eq!(kind(0x1101), Some(TsStreamType::Ac3Audio));
        assert_eq!(kind(0x1200), Some(TsStreamType::PresentationGraphics));
        assert_eq!(kind(0x1300), Some(TsStreamType::InteractiveGraphics));
        assert_eq!(kind(0x1A00), Some(TsStreamType::Subtitle));

        // The audio stream's details came through.
        let mut audio = TsAudioStream {
            channel_layout: TsChannelLayout::ChannellayoutMulti,
            sample_rate: 48_000,
            ..TsAudioStream::default()
        };
        audio.base.set_language_code("deu");
        audio.base.pid = Pid::new(0x1100);
        audio.base.stream_type = TsStreamType::DtsHdMasterAudio;
        assert_eq!(file.playlist_streams.get(&0x1100), Some(&TsStream::Audio(audio)));

        // PID 0x1011 was overwritten by item 1's clip (relative_length 0.5 > 0.01):
        // it now carries item 1's coding (576i / 29.97), not item 0's.
        let mut video = TsVideoStream::default();
        video.set_video_format(TsVideoFormat::Videoformat576i);
        video.set_frame_rate(TsFrameRate::Framerate29_97);
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::AvcVideo;
        assert_eq!(file.playlist_streams.get(&0x1011), Some(&TsStream::Video(video)));

        // Two chapters cleared the 1.0s gate; the type-2, gate-failing, and
        // out-of-range entries did not.
        assert_eq!(file.chapters, vec![0.0, 40.0]);
    }

    /// A two-item playlist where item 0 has 2 extra angles and item 1 has 1.
    fn multiangle_mpls() -> Vec<u8> {
        let v = pl_stream(1, 0x1011, 0x1B, [0x62, 0x30, 0, 0]);
        let item0 = build_item(
            "00000",
            2_700_000,
            4_500_000,
            Some(&["00010", "00021"]),
            [1, 0, 0, 0, 0, 0, 0],
            &v,
        );
        let item1 =
            build_item("00017", 2_700_000, 3_600_000, Some(&["00030"]), [1, 0, 0, 0, 0, 0, 0], &v);
        build_mpls(0, &[item0, item1], &[])
    }

    #[test]
    fn scan_handles_multiangle_items() {
        // Item 0: 2 extra angles; item 1: 1 extra angle (so the second does NOT
        // raise angle_count — exercising both sides of that comparison).
        let file = TsPlaylistFile::scan("p.mpls", &multiangle_mpls()).unwrap();

        assert_eq!(file.angle_count, 2);
        // 2 main clips + (2 + 1) angle clips: [main0, a0, a0, main1, a1].
        assert_eq!(file.stream_clips.len(), 5);
        assert_eq!(file.stream_clips.get(1).unwrap().angle_index, 1);
        // Each angle clip is named after its own `*.m2ts` (so the disc scan folds the
        // angle's size into file_size); item 0's first extra angle is `00010`.
        assert_eq!(file.stream_clips.get(1).unwrap().name, "00010.M2TS");
        // Item 1's angle clip copies the main clip's timing; every field is
        // non-zero, so a deleted field (defaulting to 0) is caught.
        let angle = file.stream_clips.get(4).unwrap();
        assert_eq!(angle.angle_index, 1);
        assert_eq!(angle.name, "00030.M2TS");
        assert_eq!(angle.time_in.to_bits(), 60.0_f64.to_bits());
        assert_eq!(angle.time_out.to_bits(), 80.0_f64.to_bits());
        assert_eq!(angle.length.to_bits(), 20.0_f64.to_bits());
        assert_eq!(angle.relative_time_in.to_bits(), 40.0_f64.to_bits());
        assert_eq!(angle.relative_time_out.to_bits(), 60.0_f64.to_bits());
        assert!(!file.mvc_base_view_r); // misc flags 0
    }

    #[test]
    fn scan_resolves_fmts_clips_to_fmts_stream_files() {
        // A play item (and its angle) whose codec id is `FMTS` names its clip
        // `*.FMTS`, not `*.M2TS`, so the disc scan resolves the right stream
        // file; an ordinary item keeps `*.M2TS`. One multiangle FMTS item plus
        // one ordinary item covers both the main- and angle-clip naming sites
        // and both arms of `clip_extension`.
        let fmts = build_item_codec(
            "00000",
            *b"FMTS",
            0,
            4_500_000,
            Some(&["00009"]),
            [0, 0, 0, 0, 0, 0, 0],
            &[],
        );
        let m2ts = build_item("00100", 0, 4_500_000, None, [0, 0, 0, 0, 0, 0, 0], &[]);
        let file = TsPlaylistFile::scan("00000.mpls", &build_mpls(0, &[fmts, m2ts], &[])).unwrap();
        assert_eq!(file.stream_clips.first().unwrap().name, "00000.FMTS");
        assert_eq!(file.stream_clips.get(1).unwrap().name, "00009.FMTS"); // the angle clip
        assert_eq!(file.stream_clips.get(2).unwrap().name, "00100.M2TS"); // ordinary item
    }

    #[test]
    fn duplicate_pid_at_the_relative_length_threshold_is_not_overwritten() {
        // Item 0 is a long clip (PID 0x1011, 1080p); item 1 reuses PID 0x1011 with
        // different coding but its relative_length is exactly 0.01 (1s / 100s). The
        // rule is `> 0.01`, so it does NOT overwrite — item 0's stream is kept.
        // (This pins the `>` boundary: a `>=` would wrongly overwrite.)
        let v0 = pl_stream(1, 0x1011, 0x1B, [0x62, 0x30, 0, 0]); // 1080p / 24 / 16:9
        let item0 = build_item("00000", 0, 4_500_000, None, [1, 0, 0, 0, 0, 0, 0], &v0); // 100s
        let v1 = pl_stream(1, 0x1011, 0x1B, [0x24, 0x20, 0, 0]); // 576i / 29.97 / 4:3
        let item1 = build_item("00017", 0, 45_000, None, [1, 0, 0, 0, 0, 0, 0], &v1); // 1s
        let buf = build_mpls(0, &[item0, item1], &[]);
        let file = TsPlaylistFile::scan("p.mpls", &buf).unwrap();

        // relative_length of item 1 is 1.0 / 100.0 == 0.01 (not > 0.01) → kept.
        let mut video = TsVideoStream::default();
        video.set_video_format(TsVideoFormat::Videoformat1080p);
        video.set_frame_rate(TsFrameRate::Framerate24);
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::AvcVideo;
        assert_eq!(file.playlist_streams.get(&0x1011), Some(&TsStream::Video(video)));
    }

    #[test]
    fn header_types_select_the_pid_offset() {
        // Five video streams, header types 1/2/3/4/other — the last forces PID 0.
        let mut streams = Vec::new();
        for (header_type, pid) in
            [(1_u8, 0x1001_u16), (2, 0x1002), (3, 0x1003), (4, 0x1004), (7, 0x1005)]
        {
            streams.extend(pl_stream(header_type, pid, 0x1B, [0x62, 0x30, 0, 0]));
        }
        let item = build_item("00000", 2_700_000, 4_500_000, None, [5, 0, 0, 0, 0, 0, 0], &streams);
        let buf = build_mpls(0, &[item], &[]);
        let file = TsPlaylistFile::scan("p.mpls", &buf).unwrap();
        let pids: Vec<u16> = file.playlist_streams.keys().copied().collect();
        // header type 7 -> default -> PID 0; the others carry their declared PID.
        assert_eq!(pids, vec![0x0000, 0x1001, 0x1002, 0x1003, 0x1004]);
    }

    /// A one-item playlist with 1 PG + 1 PiP-PG + 1 IG entry (the PiP-PG
    /// entry sits in-line in the PG section, before the IG entries).
    fn pip_pg_mpls() -> Vec<u8> {
        let mut streams = Vec::new();
        streams.extend(pl_stream(1, 0x1200, 0x90, [b'e', b'n', b'g', 0])); // PG
        streams.extend(pl_stream(1, 0x1210, 0x90, [b'f', b'r', b'a', 0])); // PiP-PG
        streams.extend(pl_stream(1, 0x1300, 0x91, [b'd', b'e', b'u', 0])); // IG
        let item = build_item("00000", 2_700_000, 4_500_000, None, [0, 0, 1, 1, 0, 0, 1], &streams);
        build_mpls(0, &[item], &[])
    }

    #[test]
    fn pip_pg_entries_are_consumed_inside_the_pg_section() {
        // The PiP-PG entry must be consumed (else the IG loop parses the PiP
        // entry instead) but is not modelled, so the PiP PID never appears in
        // the playlist streams.
        let file = TsPlaylistFile::scan("p.mpls", &pip_pg_mpls()).unwrap();
        let pids: Vec<u16> = file.playlist_streams.keys().copied().collect();
        assert_eq!(pids, vec![0x1200, 0x1300]);
        let kind = |pid: u16| file.playlist_streams.get(&pid).map(TsStream::stream_type);
        assert_eq!(kind(0x1200), Some(TsStreamType::PresentationGraphics));
        assert_eq!(kind(0x1300), Some(TsStreamType::InteractiveGraphics));
    }

    #[test]
    fn secondary_entries_skip_their_variable_comb_info_lists() {
        // Two secondary-audio entries (the first with one ref → a padded
        // 4-byte comb block) and two secondary-video entries (the first with
        // one even and one odd ref list → 4 + 4 bytes). A fixed stride would
        // misalign every entry after the first of each section.
        let mut streams = Vec::new();
        streams.extend(pl_stream(1, 0x1A10, 0xA1, [0x31, b'e', b'n', b'g']));
        streams.extend(comb_info(&[1])); // one primary-audio ref, odd → padded
        streams.extend(pl_stream(1, 0x1A11, 0xA2, [0x31, b'f', b'r', b'a']));
        streams.extend(comb_info(&[])); // empty → 2 bytes
        streams.extend(pl_stream(1, 0x1B10, 0x1B, [0x62, 0x30, 0, 0]));
        streams.extend(comb_info(&[1, 2])); // even ref list → no pad
        streams.extend(comb_info(&[3])); // odd ref list → padded
        streams.extend(pl_stream(1, 0x1B11, 0x1B, [0x62, 0x30, 0, 0]));
        streams.extend(comb_info(&[]));
        streams.extend(comb_info(&[]));
        let item = build_item("00000", 2_700_000, 4_500_000, None, [0, 0, 0, 0, 2, 2, 0], &streams);
        let buf = build_mpls(0, &[item], &[]);
        let file = TsPlaylistFile::scan("p.mpls", &buf).unwrap();
        let pids: Vec<u16> = file.playlist_streams.keys().copied().collect();
        assert_eq!(pids, vec![0x1A10, 0x1A11, 0x1B10, 0x1B11]);
        let kind = |pid: u16| file.playlist_streams.get(&pid).map(TsStream::stream_type);
        assert_eq!(kind(0x1A10), Some(TsStreamType::Ac3PlusSecondaryAudio));
        assert_eq!(kind(0x1A11), Some(TsStreamType::DtsHdSecondaryAudio));
        assert_eq!(kind(0x1B10), Some(TsStreamType::AvcVideo));
        assert_eq!(kind(0x1B11), Some(TsStreamType::AvcVideo));
    }

    #[test]
    fn unknown_and_mvc_stream_types_yield_no_stream() {
        // An unknown coding type (0x99) and MVC (0x20) both produce no stream, so
        // the item's only stream count adds nothing to the playlist streams.
        let mut streams = pl_stream(1, 0x1500, 0x99, [0, 0, 0, 0]); // unknown
        streams.extend(pl_stream(1, 0x1600, 0x20, [0, 0, 0, 0])); // MVC
        let item = build_item("00000", 2_700_000, 4_500_000, None, [2, 0, 0, 0, 0, 0, 0], &streams);
        let buf = build_mpls(0, &[item], &[]);
        let file = TsPlaylistFile::scan("p.mpls", &buf).unwrap();
        assert!(file.playlist_streams.is_empty());
        assert_eq!(file.stream_clips.len(), 1);
    }

    #[test]
    fn mpls_video_attributes_carry_no_aspect_ratio() {
        // MPLS video attributes carry only format + rate; on an HEVC entry the
        // next byte is dynamic_range_type/color_space. Reading its high nibble
        // as an aspect field would turn dynamic_range_type == 2 into
        // Aspect4_3 — the aspect must stay at its default instead.
        let streams = pl_stream(1, 0x1011, 0x24, [0x62, 0x20, 0, 0]); // HEVC, dr_type 2
        let item = build_item("00000", 2_700_000, 4_500_000, None, [1, 0, 0, 0, 0, 0, 0], &streams);
        let buf = build_mpls(0, &[item], &[]);
        let file = TsPlaylistFile::scan("p.mpls", &buf).unwrap();
        let mut video = TsVideoStream::default();
        video.set_video_format(TsVideoFormat::Videoformat1080p);
        video.set_frame_rate(TsFrameRate::Framerate24);
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::HevcVideo;
        assert_eq!(video.aspect_ratio, TsAspectRatio::Unknown);
        assert_eq!(file.playlist_streams.get(&0x1011), Some(&TsStream::Video(video)));
    }

    #[test]
    fn read_time_masks_the_sign_bit() {
        // Times with bit 31 set must mask down to the low 31 bits.
        let in_time = 0x8000_0000_u32 | 0x0029_32E0; // 2_700_000 -> 60.0s after masking
        let out_time = 0x8000_0000_u32 | 0x0044_AA20; // 4_500_000 -> 100.0s
        let v = pl_stream(1, 0x1011, 0x1B, [0x62, 0x30, 0, 0]);
        let item = build_item("00000", in_time, out_time, None, [1, 0, 0, 0, 0, 0, 0], &v);
        let buf = build_mpls(0, &[item], &[]);
        let file = TsPlaylistFile::scan("p.mpls", &buf).unwrap();
        let clip = file.stream_clips.first().unwrap();
        assert_eq!(clip.time_in.to_bits(), 60.0_f64.to_bits());
        assert_eq!(clip.time_out.to_bits(), 100.0_f64.to_bits());
    }

    #[test]
    fn scan_accepts_all_four_magics_and_rejects_others() {
        for magic in ["MPLS0100", "MPLS0200", "MPLS0240", "MPLS0300"] {
            let mut buf = comprehensive_mpls();
            buf.splice(0..8, magic.bytes());
            assert!(TsPlaylistFile::scan("p.mpls", &buf).is_ok(), "magic {magic}");
        }
        let mut bad = comprehensive_mpls();
        bad.splice(0..8, *b"MPLSXXXX");
        // `BdError` no longer derives `PartialEq` (its `Io` wraps `io::Error`); assert
        // on the failure's `Display` instead.
        assert_eq!(
            TsPlaylistFile::scan("p.mpls", &bad).unwrap_err().to_string(),
            "unknown file type: MPLSXXXX"
        );
    }

    #[test]
    fn scan_never_panics_on_truncation() {
        // Every prefix of a valid playlist parses to Ok or Err — never panics.
        // All three buffers are swept so the multiangle and PiP-PG reads' EOF
        // paths are exercised.
        for full in [comprehensive_mpls(), multiangle_mpls(), pip_pg_mpls()] {
            for len in 0..=full.len() {
                let chunk = full.get(..len).unwrap();
                drop(TsPlaylistFile::scan("t.mpls", chunk));
            }
            assert!(TsPlaylistFile::scan("t.mpls", &full).is_ok());
            // A clearly-incomplete prefix is rejected, not accepted.
            let half = full.get(..full.len() / 2).unwrap();
            assert_eq!(
                TsPlaylistFile::scan("t.mpls", half).unwrap_err().to_string(),
                "unexpected end of input"
            );
        }
    }

    #[test]
    fn scan_rejects_short_headers() {
        let eof = "unexpected end of input";
        assert_eq!(TsPlaylistFile::scan("e", &[]).unwrap_err().to_string(), eof);
        assert_eq!(TsPlaylistFile::scan("e", b"MPLS0300").unwrap_err().to_string(), eof);
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_input(data in any::<Vec<u8>>()) {
            drop(TsPlaylistFile::scan("fuzz.mpls", &data));
        }

        #[test]
        fn scan_round_trips_built_playlists(pid in any::<u16>(), in_time in 0_u32..0x7FFF_FFFF) {
            let out_time = in_time.saturating_add(2_700_000);
            let v = pl_stream(1, pid, 0x1B, [0x62, 0x30, 0, 0]);
            let item = build_item("00000", in_time, out_time, None, [1, 0, 0, 0, 0, 0, 0], &v);
            let buf = build_mpls(0, &[item], &[]);
            let file = TsPlaylistFile::scan("p.mpls", &buf).unwrap();
            let stream = file.playlist_streams.get(&pid).unwrap();
            assert_eq!(stream.pid(), Pid::new(pid));
            assert_eq!(stream.stream_type(), TsStreamType::AvcVideo);
        }
    }
}
