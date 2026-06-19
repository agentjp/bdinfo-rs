//! SMPTE VC-1 video codec scanner.
//!
//! [`scan`] runs a byte-at-a-time start-code state machine over one assembled
//! access-unit buffer: a sequence header (`00 00 01 0F`) fills the
//! [`TsVideoStream`] `encoding_profile` (`"Advanced Profile N"` /
//! `"Main Profile N"`) and the interlaced flag and marks the stream
//! `is_vbr`/`is_initialized`; a frame header (`00 00 01 0D`) decodes the
//! picture type into the `tag` (`P`/`B`/`I`/`BI`, or cleared). Resolution / frame
//! rate / aspect ratio come from the CLPI/MPLS metadata.
//!
//! `parse` is a rolling 32-bit start-code window, accumulated with `wrapping_*`
//! (a hostile byte run shifts stale bits out harmlessly instead of overflowing).
//! The picture-type masks/shifts (`& 0x80000000`, `>> 14`, …) run on the
//! explicitly unsigned [`i32::cast_unsigned`] view so the right-shifts are
//! logical — the sign bit must never smear into the extracted type bits.
//! Picture-type coding depends on the frame-coding mode: progressive (FCM `0`)
//! and interlace-frame (FCM `10`) headers carry a unary picture-type code, but an
//! interlace-*field* header (FCM `11`) carries a fixed 3-bit FPTYPE that is mapped
//! to the field pair's representative type instead. The sequence-header countdown
//! match handles only steps 5 and 0; the other steps carry nothing of interest
//! and fall through as no-ops.

use crate::bitstream::TsStreamBuffer;
use crate::stream::TsVideoStream;

/// Scans one VC-1 access unit from `buffer` into `stream`.
///
/// Fills `stream.encoding_profile` and the interlaced flag from the sequence
/// header, and sets `tag` to the frame picture type; returns early once a frame
/// header is decoded on an already-initialised stream (nothing more can change). A
/// buffer with no VC-1 start codes leaves `stream` untouched.
pub fn scan(stream: &mut TsVideoStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    let mut parse: i32 = 0;
    let mut frame_header_parse: u8 = 0;
    let mut sequence_header_parse: u8 = 0;
    let mut is_interlaced = false;

    for _ in 0..buffer.length() {
        parse = parse.wrapping_shl(8).wrapping_add(i32::from(buffer.read_byte(false)));

        if parse == 0x0000_010D {
            frame_header_parse = 4;
        } else if frame_header_parse > 0 {
            frame_header_parse = frame_header_parse.wrapping_sub(1);
            if frame_header_parse == 0 {
                // The picture-type bits are extracted from the unsigned `parse` view
                // so the right-shifts are logical (no sign smear into the type bits).
                let p = parse.cast_unsigned();
                if is_interlaced && (p & 0xC000_0000) == 0xC000_0000 {
                    // FCM = "11" (ILACE_FIELD): the field pair's type is signalled by a
                    // fixed 3-bit FPTYPE at bits 29..=27 (SMPTE 421M §9.1.1.5), not a
                    // unary code. Collapse the pair to its representative type exactly
                    // as FFmpeg does (`fptype & 4` picks the B/BI half, `fptype & 2` the
                    // pair's second member).
                    let fptype = (p & 0x3800_0000).wrapping_shr(27);
                    *tag = Some(
                        match ((fptype & 0x4) == 0, (fptype & 0x2) == 0) {
                            (true, true) => "I",
                            (true, false) => "P",
                            (false, true) => "B",
                            (false, false) => "BI",
                        }
                        .to_owned(),
                    );
                } else {
                    let picture_type: u32 = if is_interlaced {
                        if (p & 0x8000_0000) == 0 {
                            (p & 0x7800_0000).wrapping_shr(13)
                        } else {
                            // FCM = "10" (ILACE_FRAME): unary picture type from bit 29.
                            (p & 0x3C00_0000).wrapping_shr(12)
                        }
                    } else {
                        (p & 0xF000_0000).wrapping_shr(14)
                    };

                    if (picture_type & 0x2_0000) == 0 {
                        *tag = Some("P".to_owned());
                    } else if (picture_type & 0x1_0000) == 0 {
                        *tag = Some("B".to_owned());
                    } else if (picture_type & 0x8000) == 0 {
                        *tag = Some("I".to_owned());
                    } else if (picture_type & 0x4000) == 0 {
                        *tag = Some("BI".to_owned());
                    } else {
                        *tag = None;
                    }
                }
                if stream.base.is_initialized {
                    return;
                }
            }
        } else if parse == 0x0000_010F {
            sequence_header_parse = 6;
        } else if sequence_header_parse > 0 {
            sequence_header_parse = sequence_header_parse.wrapping_sub(1);
            match sequence_header_parse {
                5 => {
                    let profile_level = (parse & 0x38).wrapping_shr(3);
                    if (parse & 0xC0).wrapping_shr(6) == 3 {
                        stream.encoding_profile = Some(format!("Advanced Profile {profile_level}"));
                    } else {
                        stream.encoding_profile = Some(format!("Main Profile {profile_level}"));
                    }
                }
                0 => {
                    is_interlaced = (parse & 0x40).wrapping_shr(6) > 0;
                }
                _ => {}
            }
            stream.base.is_vbr = true;
            stream.base.is_initialized = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, proptest};

    use super::scan;
    use crate::bitstream::TsStreamBuffer;
    use crate::stream::{TsAspectRatio, TsFrameRate, TsStreamType, TsVideoFormat, TsVideoStream};

    /// A rewound buffer holding `data`.
    fn buf(data: &[u8]) -> TsStreamBuffer {
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        b
    }

    /// A fresh VC-1 video stream.
    fn stream() -> TsVideoStream {
        let mut s = TsVideoStream::default();
        s.base.stream_type = TsStreamType::Vc1Video;
        s
    }

    /// Runs [`scan`] over `data` and returns `(stream, tag)`.
    fn run(data: &[u8]) -> (TsVideoStream, Option<String>) {
        let mut s = stream();
        let mut b = buf(data);
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        (s, tag)
    }

    /// A sequence header (`00 00 01 0F`). `profile_byte` is read at countdown step 5
    /// (its top two bits select Advanced/Main, bits 3..=5 the level); `case0_byte`
    /// is read at step 0 (bit 6 sets interlaced). The `0x00` filler in between never
    /// re-forms a start code over the rolling window.
    fn sequence_header(profile_byte: u8, case0_byte: u8) -> Vec<u8> {
        vec![0x00, 0x00, 0x01, 0x0F, profile_byte, 0x00, 0x00, 0x00, 0x00, case0_byte]
    }

    /// A frame header (`00 00 01 0D`) whose first picture byte is `a` (the rest
    /// zero), so the rolling window at decode time is `a << 24`.
    fn frame_header(a: u8) -> Vec<u8> {
        vec![0x00, 0x00, 0x01, 0x0D, a, 0x00, 0x00, 0x00]
    }

    #[test]
    fn sequence_header_advanced_profile_sets_encoding_profile() {
        // 0xD8 → top bits 11 (Advanced), bits 3..=5 = 011 (level 3).
        let (s, _) = run(&sequence_header(0xD8, 0x00));
        assert_eq!(s.encoding_profile.as_deref(), Some("Advanced Profile 3"));
        assert!(s.base.is_vbr);
        assert!(s.base.is_initialized);
    }

    #[test]
    fn sequence_header_main_profile_and_level_extraction() {
        // Top bits != 11 → Main Profile; bits 3..=5 give the level.
        let (s, _) = run(&sequence_header(0x18, 0x00)); // 00 → Main, level 3
        assert_eq!(s.encoding_profile.as_deref(), Some("Main Profile 3"));
        let (s, _) = run(&sequence_header(0x48, 0x00)); // 01 → Main, level 1
        assert_eq!(s.encoding_profile.as_deref(), Some("Main Profile 1"));
        let (s, _) = run(&sequence_header(0xC8, 0x00)); // 11 → Advanced, level 1
        assert_eq!(s.encoding_profile.as_deref(), Some("Advanced Profile 1"));
    }

    #[test]
    fn sequence_header_with_clpi_metadata_matches_the_expected_vc1_desc() {
        // encoding_profile from the codec; resolution/rate/aspect from CLPI
        // reproduce the desc of a real VC-1 disc sample (PID 4113).
        let mut s = stream();
        s.set_video_format(TsVideoFormat::Videoformat1080p);
        s.set_frame_rate(TsFrameRate::Framerate23_976);
        s.aspect_ratio = TsAspectRatio::Aspect16_9;
        let mut b = buf(&sequence_header(0xD8, 0x00));
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.description(), "1080p / 23.976 fps / 16:9 / Advanced Profile 3");
        assert_eq!(s.codec_short_name(), "VC-1");
        assert_eq!(s.codec_name(), "VC-1 Video");
    }

    #[test]
    fn progressive_frame_header_picture_types() {
        // Progressive: picture_type nibble = the top nibble of the first byte.
        // 0..=7 → P, 8..=11 → B, 12..=13 → I, 14 → BI, 15 → cleared.
        assert_eq!(run(&frame_header(0x00)).1.as_deref(), Some("P")); // nibble 0
        assert_eq!(run(&frame_header(0x80)).1.as_deref(), Some("B")); // nibble 8
        assert_eq!(run(&frame_header(0xC0)).1.as_deref(), Some("I")); // nibble 12
        assert_eq!(run(&frame_header(0xE0)).1.as_deref(), Some("BI")); // nibble 14
        assert_eq!(run(&frame_header(0xF0)).1, None); // nibble 15 → tag cleared
    }

    #[test]
    fn interlaced_frame_header_uses_the_interlaced_picture_bits() {
        // A sequence header with case0 bit 6 set makes the stream interlaced (and
        // initialised). A first picture byte of 0x40 decodes to B under the
        // interlaced bit-31-clear path but would be P under the progressive path —
        // proving the interlaced branch is taken.
        let mut data = sequence_header(0xD8, 0x40);
        data.extend_from_slice(&frame_header(0x40));
        let (s, tag) = run(&data);
        assert!(s.base.is_initialized);
        assert_eq!(tag.as_deref(), Some("B"));

        // Bit 31 set (first byte >= 0x80) takes the other interlaced sub-branch:
        // 0x80 → P interlaced (would be B progressive).
        let mut data = sequence_header(0xD8, 0x40);
        data.extend_from_slice(&frame_header(0x80));
        assert_eq!(run(&data).1.as_deref(), Some("P"));
    }

    #[test]
    fn interlaced_field_frame_header_uses_the_fixed_fptype() {
        // FCM = "11" (ILACE_FIELD): the top two picture bits are 11, so the 3-bit
        // FPTYPE at bits 29..=27 maps to the field pair's representative type per
        // SMPTE 421M §9.1.1.5 — NOT the unary code the other modes use. The first
        // picture byte's bits 7,6 are the FCM "11"; bits 5,4,3 are the FPTYPE.
        let field_tag = |a: u8| {
            let mut data = sequence_header(0xD8, 0x40); // interlaced + initialised
            data.extend_from_slice(&frame_header(a));
            run(&data).1
        };
        // FPTYPE 000 (I/I) and 001 (I/P) → I; 010 (P/I) and 011 (P/P) → P.
        assert_eq!(field_tag(0xC0).as_deref(), Some("I")); // 1100_0000 → fptype 000
        assert_eq!(field_tag(0xC8).as_deref(), Some("I")); // 1100_1000 → fptype 001
        assert_eq!(field_tag(0xD0).as_deref(), Some("P")); // 1101_0000 → fptype 010
        assert_eq!(field_tag(0xD8).as_deref(), Some("P")); // 1101_1000 → fptype 011
        // FPTYPE 100 (B/B) and 101 (B/BI) → B; 110 (BI/B) and 111 (BI/BI) → BI.
        assert_eq!(field_tag(0xE0).as_deref(), Some("B")); // 1110_0000 → fptype 100
        assert_eq!(field_tag(0xE8).as_deref(), Some("B")); // 1110_1000 → fptype 101
        assert_eq!(field_tag(0xF0).as_deref(), Some("BI")); // 1111_0000 → fptype 110
        assert_eq!(field_tag(0xF8).as_deref(), Some("BI")); // 1111_1000 → fptype 111
        // The same 0xE0/0xF0 bytes decode differently off the field path: progressive
        // 0xE0 → BI and 0xF0 → cleared, proving the FPTYPE branch is the one taken.
        assert_eq!(run(&frame_header(0xE0)).1.as_deref(), Some("BI"));
        assert_eq!(run(&frame_header(0xF0)).1, None);
    }

    #[test]
    fn progressive_after_a_non_interlaced_sequence_header_returns() {
        // case0 bit 6 clear → not interlaced; the header initialises, then the frame
        // header sets the tag and returns. 0x40 → P progressively (proving the
        // interlaced branch was NOT taken, which would give B).
        let mut data = sequence_header(0xD8, 0x00);
        data.extend_from_slice(&frame_header(0x40));
        let (s, tag) = run(&data);
        assert!(s.base.is_initialized);
        assert_eq!(tag.as_deref(), Some("P"));
    }

    #[test]
    fn a_frame_header_before_initialization_sets_the_tag_without_returning() {
        // No sequence header → not initialised: the frame header sets the tag but
        // the loop does not return, so a following sequence header still runs.
        let mut data = frame_header(0x00); // P
        data.extend_from_slice(&sequence_header(0xD8, 0x00));
        let (s, tag) = run(&data);
        assert_eq!(tag.as_deref(), Some("P"));
        assert_eq!(s.encoding_profile.as_deref(), Some("Advanced Profile 3"));
    }

    #[test]
    fn no_start_codes_leaves_the_stream_untouched() {
        let (s, tag) = run(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        assert!(!s.base.is_initialized);
        assert!(s.encoding_profile.is_none());
        assert_eq!(tag, None);
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>()) {
            let mut s = stream();
            let mut b = buf(&data);
            let mut tag = None;
            scan(&mut s, &mut b, &mut tag);
        }
    }
}
