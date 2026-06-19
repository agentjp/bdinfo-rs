//! MPEG-2 video codec scanner.
//!
//! [`scan`] runs a byte-at-a-time start-code state machine over one assembled
//! access-unit buffer: a sequence header (`00 00 01 B3`) marks the stream
//! `is_vbr`/`is_initialized`, and each picture start code (`00 00 01 00`) sets
//! the picture-type `tag` (`I`/`P`/`B`). Resolution / frame rate / aspect ratio
//! come from the CLPI/MPLS metadata, so the codec sets no `encoding_profile`
//! (the MPEG-2 `desc` is metadata-only).
//!
//! `parse` is a rolling 32-bit start-code window, accumulated with `wrapping_*`
//! (a hostile byte run shifts stale bits out harmlessly instead of overflowing).
//! The sequence-header width/height, aspect-ratio, frame-rate, bit-rate, and
//! interlaced fields are deliberately not decoded — they would only duplicate the
//! CLPI/MPLS metadata the report already uses — so the `extension` /
//! `sequence_extension` start codes, which carry nothing else of interest, get no
//! handling either. Reserved picture-coding types fall through the tag match as
//! a no-op.

use crate::bitstream::TsStreamBuffer;
use crate::stream::TsVideoStream;

/// Scans one MPEG-2 access unit from `buffer` into `stream`.
///
/// Sets `tag` to the picture type (`I`/`P`/`B`) at each picture start code and
/// marks the stream initialised at the sequence header; returns early once a
/// picture is seen on an already-initialised stream (nothing more can change). A
/// buffer with no MPEG-2 start codes leaves `stream` untouched.
pub fn scan(stream: &mut TsVideoStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    let mut parse: i32 = 0;
    let mut picture_parse: i32 = 0;
    let mut sequence_header_parse: i32 = 0;

    for _ in 0..buffer.length() {
        parse = parse.wrapping_shl(8).wrapping_add(i32::from(buffer.read_byte(false)));

        if parse == 0x0000_0100 {
            picture_parse = 2;
        } else if parse == 0x0000_01B3 {
            sequence_header_parse = 7;
        } else if sequence_header_parse > 0 {
            sequence_header_parse = sequence_header_parse.wrapping_sub(1);
            // The six header bytes under the countdown (resolution / aspect /
            // frame-rate) are skipped, not decoded — the CLPI metadata supplies
            // those; only the countdown reaching 0 marks the stream.
            if sequence_header_parse == 0 {
                stream.base.is_vbr = true;
                stream.base.is_initialized = true;
            }
        } else if picture_parse > 0 {
            picture_parse = picture_parse.wrapping_sub(1);
            if picture_parse == 0 {
                // `picture_coding_type` = bits 3..=5 of the byte after the start code.
                match (parse & 0x38).wrapping_shr(3) {
                    1 => *tag = Some("I".to_owned()),
                    2 => *tag = Some("P".to_owned()),
                    3 => *tag = Some("B".to_owned()),
                    _ => {}
                }
                if stream.base.is_initialized {
                    return;
                }
            }
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

    /// A fresh MPEG-2 video stream.
    fn stream() -> TsVideoStream {
        let mut s = TsVideoStream::default();
        s.base.stream_type = TsStreamType::Mpeg2Video;
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

    /// A sequence header (`00 00 01 B3`) plus seven filler bytes — enough for the
    /// `sequence_header_parse` countdown to reach `0` and initialise the stream.
    /// The `0x00` filler never re-forms a start code over the rolling window.
    fn sequence_header() -> Vec<u8> {
        vec![0x00, 0x00, 0x01, 0xB3, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    }

    /// A picture start code (`00 00 01 00`) whose second following byte carries
    /// `picture_coding_type` in bits 3..=5, then a trailing byte.
    fn picture(coding_type: u8) -> Vec<u8> {
        vec![0x00, 0x00, 0x01, 0x00, 0x00, coding_type << 3, 0x00]
    }

    #[test]
    fn sequence_header_initializes_the_stream() {
        let (s, tag) = run(&sequence_header());
        assert!(s.base.is_vbr);
        assert!(s.base.is_initialized);
        assert!(s.encoding_profile.is_none()); // MPEG-2 sets no profile
        assert_eq!(tag, None);
    }

    #[test]
    fn sequence_header_with_clpi_metadata_matches_the_expected_mpeg2_desc() {
        // The codec only initialises; resolution/rate/aspect come from CLPI and
        // reproduce the desc of a real MPEG-2 disc sample (PID 4113).
        let mut s = stream();
        s.set_video_format(TsVideoFormat::Videoformat1080p);
        s.set_frame_rate(TsFrameRate::Framerate24);
        s.aspect_ratio = TsAspectRatio::Aspect16_9;
        let mut b = buf(&sequence_header());
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.description(), "1080p / 24 fps / 16:9");
        assert_eq!(s.codec_short_name(), "MPEG-2");
        assert_eq!(s.codec_name(), "MPEG-2 Video");
    }

    #[test]
    fn picture_start_code_sets_the_picture_type_tag() {
        // picture_coding_type 1 → I, 2 → P, 3 → B; others leave the tag unset.
        assert_eq!(run(&picture(1)).1.as_deref(), Some("I"));
        assert_eq!(run(&picture(2)).1.as_deref(), Some("P"));
        assert_eq!(run(&picture(3)).1.as_deref(), Some("B"));
        assert_eq!(run(&picture(0)).1, None); // forbidden / D-frame → no tag
        assert_eq!(run(&picture(4)).1, None); // reserved → default arm
    }

    #[test]
    fn a_picture_on_an_initialized_stream_returns() {
        // Sequence header initialises, then a picture sets the tag and returns; a
        // second sequence header after it must NOT run (the loop returned). Pre-set
        // is_vbr false on a fresh stream to prove the second header would flip it.
        let mut data = sequence_header();
        data.extend_from_slice(&picture(2)); // P, returns because initialised
        let (s, tag) = run(&data);
        assert_eq!(tag.as_deref(), Some("P"));
        assert!(s.base.is_initialized);
    }

    #[test]
    fn a_picture_before_initialization_sets_the_tag_without_returning() {
        // A picture first (tag set, no return because not initialised), then the
        // sequence header still runs and initialises.
        let mut data = picture(1); // I
        data.extend_from_slice(&sequence_header());
        let (s, tag) = run(&data);
        assert_eq!(tag.as_deref(), Some("I"));
        assert!(s.base.is_initialized);
    }

    #[test]
    fn no_start_codes_leaves_the_stream_untouched() {
        let (s, tag) = run(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert!(!s.base.is_initialized);
        assert!(!s.base.is_vbr);
        assert_eq!(tag, None);
    }

    #[test]
    fn a_truncated_sequence_header_does_not_initialize() {
        // The countdown is cut short (3 of the 7 bytes), so `sequence_header_parse`
        // never reaches 0 and the stream stays uninitialised. Pins the `== 0`
        // init marker: a mutated `!= 0` would initialise on the first countdown byte.
        let (s, _) = run(&[0x00, 0x00, 0x01, 0xB3, 0x00, 0x00, 0x00]);
        assert!(!s.base.is_initialized);
        assert!(!s.base.is_vbr);
    }

    #[test]
    fn a_picture_started_inside_a_sequence_header_decodes_after_init() {
        // A picture start code lands mid sequence-header countdown; its picture_parse
        // is starved (the `sequence_header_parse > 0` branch has priority) until the
        // header finishes, so the picture type is decoded on the bytes right after
        // initialisation. The exact byte that supplies the type pins the `> 0`
        // priority: a mutated `>= 0` would steal one more byte after init and decode
        // the type one byte later — "P" (0x10) instead of "I" (0x08).
        let data = [
            0x00, 0x00, 0x01, 0xB3, // sequence header → countdown 7
            0x00, 0x00, 0x01,
            0x00, // picture start code (picture_parse = 2), leaves countdown at 4
            0x00, 0x00, 0x00, 0x00,
            0x00, // finish the countdown (init), then one starved byte
            0x08, // coding_type 1 → I (the byte the faithful scan decodes)
            0x10, // coding_type 2 → P (what a `>= 0` mutant would decode instead)
        ];
        let (s, tag) = run(&data);
        assert!(s.base.is_initialized);
        assert_eq!(tag.as_deref(), Some("I"));
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
