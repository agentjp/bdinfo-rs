//! Elementary-stream codec scanners.
//!
//! Each module decodes one assembled access-unit buffer (a [`crate::bitstream::TsStreamBuffer`]
//! filled by the [`crate::bdrom::m2ts`] demux) far enough to fill the
//! codec-derived fields of its [`crate::stream`] model — channel count, sample
//! rate, bit rate / depth, dialogue normalization, and the Atmos/DTS:X extension
//! flag — from which the report's `codec`/`codecname`/`desc` strings are derived.
//! [`scan_access_unit`] hands every completed PES to the matching scanner.
//!
//! The audio lanes: [`ac3`] (Dolby Digital / DD+ / `DialNorm` / Atmos), [`truehd`]
//! (Dolby `TrueHD` / Atmos, wrapping the AC3 core), [`dts`] and [`dts_hd`] (DTS core /
//! DTS-HD MA·HR·Express / DTS:X, the HD scanner wrapping the DTS core), and
//! [`lpcm`], [`aac`], and [`mpa`] (linear PCM, MPEG-2/4 AAC, and MPEG-1/2 audio —
//! the remaining simple audio). The video lanes: [`avc`] (H.264 SPS profile/level
//! and picture-type tag), then [`mpeg2`], [`vc1`], and [`mvc`] (MPEG-2
//! init/picture-type, VC-1 profile/interlacing/picture-type, and the MVC
//! stereo-extension stub) — the non-HEVC lanes, which fill the
//! [`crate::stream::TsVideoStream`] `encoding_profile` and the picture-type `tag` —
//! and [`hevc`] (HEVC core profile/level plus the HDR lane — HDR10 / HDR10+ /
//! Dolby Vision, ST 2086 mastering display, and `MaxCLL`/`MaxFALL`). [`pgs`] is
//! Presentation Graphics image subtitles — the caption tally / resolution that fill
//! the [`crate::stream::TsGraphicsStream`] `desc`; interactive graphics and `TextST`
//! text subtitles carry only their type-mapped `codec`/`codecname`, no scanner.
//!
//! Every scanner is panic-free over arbitrary bytes (the bit reader bounds-checks,
//! and all fixed-width codec math uses `wrapping_*` so hostile values cannot
//! overflow) — the shared `codec` fuzz target (`fuzz/`) amplifies that contract
//! adversarially.

pub mod aac;
pub mod ac3;
pub mod avc;
pub mod dts;
pub mod dts_hd;
pub mod hevc;
pub mod lpcm;
pub mod mpa;
pub mod mpeg2;
pub mod mvc;
pub mod pgs;
pub mod truehd;
pub mod vc1;

use crate::bitstream::TsStreamBuffer;
use crate::stream::{TsStream, TsStreamType};

/// Decodes one assembled access-unit `buffer` into `stream`, dispatching by stream
/// type to the matching codec scanner.
///
/// `bitrate` is the demux's per-PES audio bitrate estimate (only the DTS scanners
/// take it; the others ignore it). `is_full_scan` gates the PGS caption analysis:
/// a quick scan marks Presentation Graphics initialised without decoding it.
/// `tag` is the caller's per-stream frame marker: the scanners that recognise a
/// frame in this access unit set it (the video codecs' picture type feeds the
/// per-frame bitrate diagnostics); the others leave it as passed in — resetting
/// between access units is the caller's job.
pub(crate) fn scan_access_unit(
    stream: &mut TsStream,
    buffer: &mut TsStreamBuffer,
    bitrate: i64,
    is_full_scan: bool,
    tag: &mut Option<String>,
) {
    match stream {
        TsStream::Video(video) => match video.base.stream_type {
            TsStreamType::Mpeg2Video => mpeg2::scan(video, buffer, tag),
            TsStreamType::AvcVideo => avc::scan(video, buffer, tag),
            TsStreamType::MvcVideo => mvc::scan(video, buffer, tag),
            TsStreamType::HevcVideo => hevc::scan(video, buffer, tag),
            TsStreamType::Vc1Video => vc1::scan(video, buffer, tag),
            _ => video.base.is_initialized = true,
        },
        TsStream::Audio(audio) => match audio.base.stream_type {
            TsStreamType::Mpeg1Audio | TsStreamType::Mpeg2Audio => {
                mpa::scan(audio, buffer, tag);
            }
            TsStreamType::Mpeg2AacAudio | TsStreamType::Mpeg4AacAudio => {
                aac::scan(audio, buffer, tag);
            }
            TsStreamType::Ac3Audio
            | TsStreamType::Ac3PlusAudio
            | TsStreamType::Ac3PlusSecondaryAudio => ac3::scan(audio, buffer, tag),
            TsStreamType::Ac3TrueHdAudio => truehd::scan(audio, buffer, tag),
            TsStreamType::LpcmAudio => lpcm::scan(audio, buffer, tag),
            TsStreamType::DtsAudio => dts::scan(audio, buffer, bitrate, tag),
            TsStreamType::DtsHdAudio
            | TsStreamType::DtsHdMasterAudio
            | TsStreamType::DtsHdSecondaryAudio => dts_hd::scan(audio, buffer, bitrate, tag),
            _ => audio.base.is_initialized = true,
        },
        TsStream::Graphics(graphics) => {
            if is_full_scan && graphics.base.stream_type == TsStreamType::PresentationGraphics {
                pgs::scan(graphics, buffer, tag);
            } else {
                graphics.base.is_initialized = true;
            }
        }
        TsStream::Text(text) => text.base.is_initialized = true,
    }
}

#[cfg(test)]
mod tests {
    use super::scan_access_unit;
    use crate::bitstream::TsStreamBuffer;
    use crate::stream::{
        TsAudioStream, TsGraphicsStream, TsStream, TsStreamType, TsTextStream, TsVideoStream,
    };

    /// A rewound buffer holding `data`.
    fn buf(data: &[u8]) -> TsStreamBuffer {
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        b
    }

    /// Packs `(value, bit_width)` fields MSB-first into bytes (trailing partial byte
    /// left-aligned) — the same field-spelling helper the codec modules' tests use.
    fn pack(fields: &[(u64, u32)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut cur: u8 = 0;
        let mut nbits: u32 = 0;
        for &(val, width) in fields {
            let mut b = width;
            while b > 0 {
                b = b.wrapping_sub(1);
                let bit = u8::try_from(val.wrapping_shr(b) & 1).unwrap_or(0);
                cur = cur.wrapping_shl(1).wrapping_add(bit);
                nbits = nbits.wrapping_add(1);
                if nbits == 8 {
                    bytes.push(cur);
                    cur = 0;
                    nbits = 0;
                }
            }
        }
        if nbits > 0 {
            bytes.push(cur.wrapping_shl(8_u32.wrapping_sub(nbits)));
        }
        bytes
    }

    /// A legacy AC-3 5.1 / 48 kHz / 640 kbps core frame (also the `TrueHD`/`DTS-HD`
    /// embedded-core access unit, which carries no HD major-sync).
    fn ac3_core() -> Vec<u8> {
        pack(&[
            (0x0B77, 16),
            (0, 16),
            (0, 2),
            (36, 6),
            (8, 5),
            (0, 3),
            (7, 3),
            (0, 2),
            (0, 2),
            (1, 1),
            (31, 5),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 2),
            (0, 16),
        ])
    }

    /// A DTS core frame: 5.1 / 48 kHz / 1536 kbps / 16-bit.
    fn dts_core() -> Vec<u8> {
        pack(&[
            (0x7FFE_8001, 32),
            (0, 6),
            (0, 1),
            (0, 7),
            (100, 14),
            (0, 6),
            (13, 4),
            (24, 5),
            (0, 8),
            (0, 1),
            (0, 1),
            (1, 2),
            (0, 1),
            (0, 7),
            (0, 3),
            (0, 2),
            (0, 4),
            (0, 4),
            (4, 3),
            (0, 64),
        ])
    }

    /// An MPEG-1 Layer III stereo audio frame.
    fn mpa_frame() -> Vec<u8> {
        pack(&[
            (0x7FF, 11),
            (3, 2),
            (1, 2),
            (0, 1),
            (9, 4),
            (1, 2),
            (0, 1),
            (0, 1),
            (0, 2),
            (0, 2),
            (0, 1),
            (0, 1),
            (0, 2),
            (0, 13),
        ])
    }

    /// An MPEG-4 AAC-LC stereo ADTS frame.
    fn aac_frame() -> Vec<u8> {
        pack(&[
            (0xFFF, 12),
            (0, 1),
            (0, 2),
            (0, 1),
            (1, 2),
            (3, 4),
            (0, 1),
            (2, 3),
            (0, 1),
            (0, 1),
            (0, 16),
        ])
    }

    /// A Presentation-Graphics composition segment declaring a 1920×1080 frame.
    fn pcs() -> Vec<u8> {
        let mut v = vec![0x16, 0, 0];
        v.extend_from_slice(&1920_u16.to_be_bytes());
        v.extend_from_slice(&1080_u16.to_be_bytes());
        v.push(0);
        v.extend_from_slice(&7_u16.to_be_bytes());
        v.extend_from_slice(&[0, 0, 0]);
        v.push(1); // one composition object
        v.extend_from_slice(&[0, 0]); // object id
        v.push(0); // window id
        v.push(0); // not forced
        v.extend_from_slice(&[0_u8; 12]);
        v
    }

    /// Dispatches `stream` over `data` and hands the (mutated) stream back.
    fn run(mut stream: TsStream, data: &[u8], bitrate: i64, is_full_scan: bool) -> TsStream {
        let mut tag = None;
        let mut b = buf(data);
        scan_access_unit(&mut stream, &mut b, bitrate, is_full_scan, &mut tag);
        stream
    }

    fn video(stream_type: TsStreamType) -> TsStream {
        let mut v = TsVideoStream::default();
        v.base.stream_type = stream_type;
        TsStream::Video(v)
    }

    fn audio(stream_type: TsStreamType) -> TsStream {
        let mut a = TsAudioStream::default();
        a.base.stream_type = stream_type;
        TsStream::Audio(a)
    }

    fn graphics(stream_type: TsStreamType) -> TsStream {
        let mut g = TsGraphicsStream::default();
        g.base.stream_type = stream_type;
        TsStream::Graphics(g)
    }

    /// Whether `stream` is an audio stream that decoded an embedded `core_stream`.
    fn has_core(stream: &TsStream) -> bool {
        matches!(stream, TsStream::Audio(audio) if audio.core_stream.is_some())
    }

    /// Whether `stream` is a video stream carrying HEVC extended (HDR) data.
    fn has_extended_data(stream: &TsStream) -> bool {
        matches!(stream, TsStream::Video(video) if video.extended_data.is_some())
    }

    /// The decoded channel count of an audio stream, or `-1` for any other variant —
    /// the observable that tells "the audio codec ran" (channels set) from a deleted
    /// dispatch arm falling through to the default (channels stay 0).
    fn audio_channels(stream: &TsStream) -> i32 {
        if let TsStream::Audio(audio) = stream { audio.channel_count } else { -1 }
    }

    #[test]
    fn pack_handles_aligned_and_partial_inputs() {
        // 16 bits → exactly two whole bytes (the trailing-partial push is skipped).
        assert_eq!(pack(&[(0xABCD, 16)]), vec![0xAB, 0xCD]);
        // 12 bits → one whole byte plus a left-aligned partial byte.
        assert_eq!(pack(&[(0xABC, 12)]), vec![0xAB, 0xC0]);
    }

    #[test]
    fn video_arms_route_to_their_codec() {
        // Each video codec sets `is_vbr`; the default `_` arm only sets
        // `is_initialized`. Asserting `is_vbr` therefore distinguishes "the codec ran"
        // from a deleted arm that falls through to the default.
        assert!(
            run(video(TsStreamType::Mpeg2Video), &[0, 0, 1, 0xB3, 0, 0, 0, 0, 0, 0, 0], 0, true)
                .base()
                .is_vbr
        );
        assert!(
            run(video(TsStreamType::AvcVideo), &[0, 0, 1, 0x67, 100, 0, 41, 0], 0, true)
                .base()
                .is_vbr
        );
        assert!(run(video(TsStreamType::MvcVideo), &[], 0, true).base().is_vbr);
        assert!(
            run(video(TsStreamType::Vc1Video), &[0, 0, 1, 0x0F, 0xD8, 0, 0, 0, 0, 0], 0, true)
                .base()
                .is_vbr
        );

        // HEVC always stores its (persistent) extended-data set, even over an empty
        // buffer — the observable a deleted HEVC call would drop.
        assert!(has_extended_data(&run(video(TsStreamType::HevcVideo), &[], 0, true)));

        // MPEG-1 video has no codec arm → the default just initialises it (no `is_vbr`,
        // and being non-HEVC, no extended data).
        let mpeg1 = run(video(TsStreamType::Mpeg1Video), &[], 0, true);
        assert!(mpeg1.base().is_initialized);
        assert!(!mpeg1.base().is_vbr);
        assert!(!has_extended_data(&mpeg1));
    }

    #[test]
    fn audio_arms_route_to_their_codec() {
        // Each audio codec decodes a nonzero channel count; the default `_` arm only
        // marks the stream initialised (channels stay 0). Channel count therefore
        // distinguishes "the codec ran" from a deleted arm falling through.
        for ty in [TsStreamType::Mpeg1Audio, TsStreamType::Mpeg2Audio] {
            assert!(audio_channels(&run(audio(ty), &mpa_frame(), 0, true)) > 0, "{ty:?}");
        }
        for ty in [TsStreamType::Mpeg2AacAudio, TsStreamType::Mpeg4AacAudio] {
            assert!(audio_channels(&run(audio(ty), &aac_frame(), 0, true)) > 0, "{ty:?}");
        }
        for ty in [
            TsStreamType::Ac3Audio,
            TsStreamType::Ac3PlusAudio,
            TsStreamType::Ac3PlusSecondaryAudio,
        ] {
            assert!(audio_channels(&run(audio(ty), &ac3_core(), 0, true)) > 0, "{ty:?}");
        }
        assert!(
            audio_channels(&run(audio(TsStreamType::LpcmAudio), &[0, 0, 0x31, 0x40, 0], 0, true))
                > 0
        );
        assert!(audio_channels(&run(audio(TsStreamType::DtsAudio), &dts_core(), 0, true)) > 0);
        // TrueHD: the AC-3-core access unit (no HD sync) creates the embedded core.
        assert!(has_core(&run(audio(TsStreamType::Ac3TrueHdAudio), &ac3_core(), 0, true)));
        // DTS-HD: the DTS-core access unit (no HD sync) creates the embedded core.
        for ty in [
            TsStreamType::DtsHdAudio,
            TsStreamType::DtsHdMasterAudio,
            TsStreamType::DtsHdSecondaryAudio,
        ] {
            assert!(has_core(&run(audio(ty), &dts_core(), 0, true)), "{ty:?}");
        }
        // A (degenerate) audio stream with no audio type hits the default arm — it is
        // initialised but never decodes channels or grows a core stream.
        let unknown = run(audio(TsStreamType::Unknown), &[], 0, true);
        assert!(unknown.base().is_initialized);
        assert_eq!(audio_channels(&unknown), 0);
        assert!(!has_core(&unknown));
        // The `else` arm of `audio_channels` (a non-audio stream).
        assert_eq!(audio_channels(&run(video(TsStreamType::Mpeg1Video), &[], 0, true)), -1);
    }

    /// The decoded resolution width of a graphics stream, or `-1` for any other
    /// variant — the observable that tells "PGS decoded" (width set) from the
    /// quick-scan / non-PGS marker (width stays 0).
    fn graphics_width(s: &TsStream) -> i32 {
        if let TsStream::Graphics(graphics) = s { graphics.width } else { -1 }
    }

    #[test]
    fn presentation_graphics_decode_only_on_a_full_scan() {
        // Full scan + Presentation Graphics → PGS decodes (records the resolution);
        // every case is still marked initialised. Width pins the guard's three cases.
        let decoded = run(graphics(TsStreamType::PresentationGraphics), &pcs(), 0, true);
        assert!(decoded.base().is_initialized);
        assert_eq!(graphics_width(&decoded), 1920); // PGS ran

        // Quick scan + Presentation Graphics → marked initialised WITHOUT decoding.
        let quick = run(graphics(TsStreamType::PresentationGraphics), &pcs(), 0, false);
        assert!(quick.base().is_initialized);
        assert_eq!(graphics_width(&quick), 0);

        // Interactive Graphics → never decoded, even on a full scan.
        let interactive = run(graphics(TsStreamType::InteractiveGraphics), &pcs(), 0, true);
        assert!(interactive.base().is_initialized);
        assert_eq!(graphics_width(&interactive), 0);

        // The helper's `else` arm (a non-graphics stream).
        assert_eq!(graphics_width(&run(audio(TsStreamType::Unknown), &[], 0, true)), -1);
    }

    #[test]
    fn text_streams_are_marked_initialized() {
        let mut tag = None;
        let mut s = TsStream::Text(TsTextStream::default());
        let mut b = buf(&[]);
        scan_access_unit(&mut s, &mut b, 0, true, &mut tag);
        assert!(s.base().is_initialized);
        assert_eq!(tag, None);
    }

    #[test]
    fn the_dispatch_writes_the_frame_tag_through_to_the_caller() {
        // An AVC access-unit delimiter (primary_pic_type 1 → "P") proves the
        // dispatch hands the caller's slot to the codec scanner.
        let mut tag = None;
        let mut s = video(TsStreamType::AvcVideo);
        let mut b = buf(&[0x00, 0x00, 0x01, 0x09, 0x30]);
        scan_access_unit(&mut s, &mut b, 0, true, &mut tag);
        assert_eq!(tag.as_deref(), Some("P"));

        // A frameless access unit leaves the slot as passed in — the reset
        // between access units is the caller's job, not the dispatch's.
        let mut b2 = buf(&[0xFF, 0xFF]);
        scan_access_unit(&mut s, &mut b2, 0, true, &mut tag);
        assert_eq!(tag.as_deref(), Some("P"));
    }
}
