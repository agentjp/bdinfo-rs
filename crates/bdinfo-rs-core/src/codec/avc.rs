//! MPEG-4 AVC (H.264) video codec scanner.
//!
//! [`scan`] runs a byte-at-a-time start-code state machine over one assembled
//! access-unit buffer, filling the [`TsVideoStream`] `encoding_profile`
//! (`"{profile} {level}"`) from the sequence parameter set and setting the
//! picture-type `tag` (`I`/`P`/`B`) from each access unit delimiter. Resolution /
//! frame rate / aspect ratio are **not** decoded here (they come from the
//! CLPI/MPLS metadata); the codec only contributes the profile string plus
//! `is_vbr`/`is_initialized`.
//!
//! `parse` is a rolling 32-bit start-code window, accumulated with `wrapping_*`
//! (a hostile byte run shifts stale bits out harmlessly instead of overflowing);
//! the bytes are read raw, with no H.264 emulation-prevention unescaping — the
//! three SPS header bytes decoded here are taken as-is. The constraint-set-0/1/2
//! flags carry nothing the profile string needs and are skipped over; only
//! `constraint_set_3` (the `1b` level marker) is extracted. The picture-type match
//! covers all eight 3-bit codes, with the remaining `2 | 7` group (B) as the
//! terminal catch-all, and the SPS countdown match only ever sees 2/1/0, with `0`
//! as its terminal arm.

use crate::bitstream::TsStreamBuffer;
use crate::stream::TsVideoStream;

/// Scans one AVC access unit from `buffer` into `stream`.
///
/// Sets `tag` to the access-unit picture type (`I`/`P`/`B`) and, from the first
/// sequence parameter set, `stream.encoding_profile`; returns early once an access
/// unit delimiter is seen on an already-initialised stream (nothing more can
/// change). A buffer with no AVC start codes leaves `stream` untouched.
pub fn scan(stream: &mut TsVideoStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    let mut parse: u32 = 0;
    let mut access_unit_delimiter_parse: u8 = 0;
    let mut sequence_parameter_set_parse: u8 = 0;
    // Empty until SPS countdown step 2 names it — always before the step-0 read
    // that formats the profile string.
    let mut profile: &str = "";
    let mut constraint_set3_flag: u32 = 0;

    for _ in 0..buffer.length() {
        // Raw bytes (no emulation-prevention skip) feed the rolling 32-bit
        // start-code window; `wrapping_*` lets stale bits shift out harmlessly.
        parse = parse.wrapping_shl(8).wrapping_add(u32::from(buffer.read_byte(false)));

        if parse == 0x0000_0109 {
            access_unit_delimiter_parse = 1;
        } else if access_unit_delimiter_parse > 0 {
            // The countdown is only ever set to 1, so this single decrement always
            // lands on the decode step — no zero check needed.
            access_unit_delimiter_parse = access_unit_delimiter_parse.wrapping_sub(1);
            // `primary_pic_type` = the top 3 bits of the byte after the AUD. The
            // 3-bit code is exhaustive: I (0/3/5), P (1/4/6), and the remaining
            // `2 | 7` group is B — the catch-all.
            match (parse & 0xFF).wrapping_shr(5) {
                0 | 3 | 5 => *tag = Some("I".to_owned()),
                1 | 4 | 6 => *tag = Some("P".to_owned()),
                _ => *tag = Some("B".to_owned()),
            }
            if stream.base.is_initialized {
                return;
            }
        } else if parse == 0x0000_0127 || parse == 0x0000_0167 {
            sequence_parameter_set_parse = 3;
        } else if sequence_parameter_set_parse > 0 {
            sequence_parameter_set_parse = sequence_parameter_set_parse.wrapping_sub(1);
            if !stream.base.is_initialized {
                // The countdown is set to 3 and decremented before this match, so
                // it is 2, 1, or 0 here; `_` is the terminal step 0.
                match sequence_parameter_set_parse {
                    2 => {
                        // `profile_idc` → name, matching the BDInfo lineage table.
                        // 244 (High 4:4:4 Predictive, the live H.264 code) shares the
                        // legacy 144 (old High 4:4:4) spelling so it is caught with an
                        // existing report line instead of falling to "Unknown Profile".
                        // Codes BDInfo has no string for (44 CAVLC 4:4:4, and the
                        // Constrained-Baseline / High-Intra constraint-flag refinements)
                        // are deliberately left as their lineage outcome — emitting a
                        // brand-new report string is out of scope for the locked report.
                        profile = match parse & 0xFF {
                            66 => "Baseline Profile",
                            77 => "Main Profile",
                            88 => "Extended Profile",
                            100 => "High Profile",
                            110 => "High 10 Profile",
                            122 => "High 4:2:2 Profile",
                            144 | 244 => "High 4:4:4 Profile",
                            _ => "Unknown Profile",
                        };
                    }
                    1 => {
                        // Constraint-set 0/1/2 flags don't affect the profile string;
                        // only constraint_set_3 (the `1b` level marker) is kept.
                        constraint_set3_flag = (parse & 0x10).wrapping_shr(4);
                    }
                    _ => {
                        let b = parse & 0xFF;
                        let level = if b == 11 && constraint_set3_flag == 1 {
                            "1b".to_owned()
                        } else {
                            // The tens and units digits of the level byte
                            // (41 → "4.1").
                            format!("{}.{}", b.wrapping_div(10), b.wrapping_rem(10))
                        };
                        stream.encoding_profile = Some(format!("{profile} {level}"));
                        stream.base.is_vbr = true;
                        stream.base.is_initialized = true;
                    }
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

    /// A fresh AVC video stream.
    fn stream() -> TsVideoStream {
        let mut s = TsVideoStream::default();
        s.base.stream_type = TsStreamType::AvcVideo;
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

    /// A sequence-parameter-set access unit: the `00 00 01 67` start code, the
    /// `profile_idc` byte, the constraint/reserved byte, the `level_idc` byte, then
    /// a trailing byte so the loop processes the level byte.
    fn sps(profile_idc: u8, constraint: u8, level_idc: u8) -> Vec<u8> {
        vec![0x00, 0x00, 0x01, 0x67, profile_idc, constraint, level_idc, 0x00]
    }

    #[test]
    fn sps_sets_the_high_profile_4_1_encoding_profile() {
        // 0x64 = 100 (High Profile), constraint 0, 0x29 = 41 → "4.1".
        let (s, tag) = run(&sps(100, 0x00, 41));
        assert_eq!(s.encoding_profile.as_deref(), Some("High Profile 4.1"));
        assert!(s.base.is_vbr);
        assert!(s.base.is_initialized);
        assert_eq!(tag, None);
    }

    #[test]
    fn sps_with_clpi_metadata_matches_the_expected_avc_desc() {
        // The codec sets encoding_profile; resolution/rate/aspect come from CLPI.
        // Together they reproduce the desc of a real AVC disc sample (PID 4113).
        let mut s = stream();
        s.set_video_format(TsVideoFormat::Videoformat1080p);
        s.set_frame_rate(TsFrameRate::Framerate23_976);
        s.aspect_ratio = TsAspectRatio::Aspect16_9;
        let mut b = buf(&sps(100, 0x00, 41));
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.description(), "1080p / 23.976 fps / 16:9 / High Profile 4.1");
        assert_eq!(s.codec_short_name(), "AVC");
        assert_eq!(s.codec_name(), "MPEG-4 AVC Video");
    }

    #[test]
    fn every_profile_idc_maps_to_its_name() {
        let cases = [
            (66_u8, "Baseline Profile"),
            (77, "Main Profile"),
            (88, "Extended Profile"),
            (100, "High Profile"),
            (110, "High 10 Profile"),
            (122, "High 4:2:2 Profile"),
            (144, "High 4:4:4 Profile"),
            // 244 (High 4:4:4 Predictive) reuses the legacy 144 spelling.
            (244, "High 4:4:4 Profile"),
            // 44 (CAVLC 4:4:4) has no BDInfo line, so it stays Unknown.
            (44, "Unknown Profile"),
            (0, "Unknown Profile"),
            (200, "Unknown Profile"),
        ];
        for (idc, name) in cases {
            let (s, _) = run(&sps(idc, 0x00, 41));
            assert_eq!(s.encoding_profile.as_deref(), Some(format!("{name} 4.1").as_str()));
        }
    }

    #[test]
    fn level_1b_needs_both_level_11_and_constraint_set_3() {
        // level_idc 11 with constraint_set_3 (bit 4, 0x10) → "1b".
        let (s, _) = run(&sps(66, 0x10, 11));
        assert_eq!(s.encoding_profile.as_deref(), Some("Baseline Profile 1b"));
        // level_idc 11 but constraint_set_3 clear → the numeric "1.1".
        let (s, _) = run(&sps(66, 0x00, 11));
        assert_eq!(s.encoding_profile.as_deref(), Some("Baseline Profile 1.1"));
        // constraint_set_3 set but level_idc != 11 → numeric (the `&&` is a conjunction).
        let (s, _) = run(&sps(66, 0x10, 30));
        assert_eq!(s.encoding_profile.as_deref(), Some("Baseline Profile 3.0"));
    }

    #[test]
    fn level_byte_splits_into_tens_and_units() {
        // 51 → "5.1", 30 → "3.0", 0 → "0.0".
        for (level, text) in [(51_u8, "5.1"), (30, "3.0"), (0, "0.0")] {
            let (s, _) = run(&sps(77, 0x00, level));
            assert_eq!(
                s.encoding_profile.as_deref(),
                Some(format!("Main Profile {text}").as_str())
            );
        }
    }

    #[test]
    fn access_unit_delimiter_sets_the_picture_type_tag() {
        // primary_pic_type = the top 3 bits of the byte after `00 00 01 09`.
        // 0/3/5 → I, 1/4/6 → P, 2/7 → B.
        let aud = |pic: u8| vec![0x00, 0x00, 0x01, 0x09, pic << 5, 0x00];
        for code in [0_u8, 3, 5] {
            assert_eq!(run(&aud(code)).1.as_deref(), Some("I"), "pic {code}");
        }
        for code in [1_u8, 4, 6] {
            assert_eq!(run(&aud(code)).1.as_deref(), Some("P"), "pic {code}");
        }
        for code in [2_u8, 7] {
            assert_eq!(run(&aud(code)).1.as_deref(), Some("B"), "pic {code}");
        }
    }

    #[test]
    fn an_initialized_stream_returns_at_the_next_access_unit_delimiter() {
        // SPS initialises, then an AUD sets the tag and returns; a second SPS after
        // the AUD must NOT overwrite the profile (the loop returned).
        let mut data = sps(100, 0x00, 41);
        data.extend_from_slice(&[0x00, 0x00, 0x01, 0x09, 0x40]); // AUD, pic_type 2 → B
        data.extend_from_slice(&sps(77, 0x00, 51)); // would be Main 5.1 if reached
        let (s, tag) = run(&data);
        assert_eq!(tag.as_deref(), Some("B"));
        assert_eq!(s.encoding_profile.as_deref(), Some("High Profile 4.1"));
    }

    #[test]
    fn sps_on_an_already_initialized_stream_is_skipped() {
        // A pre-initialised stream keeps its profile: the SPS `!is_initialized`
        // guard skips the parse entirely.
        let mut s = stream();
        s.base.is_initialized = true;
        s.encoding_profile = Some("Existing".to_owned());
        let mut b = buf(&sps(100, 0x00, 41));
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.encoding_profile.as_deref(), Some("Existing"));
    }

    #[test]
    fn an_access_unit_delimiter_on_an_uninitialized_stream_continues() {
        // The AUD sets the tag but, not initialised, the loop does NOT return — a
        // following SPS is still parsed.
        let mut data = vec![0x00, 0x00, 0x01, 0x09, 0x00, 0x00]; // AUD pic 0 → I
        data.extend_from_slice(&sps(77, 0x00, 51));
        let (s, tag) = run(&data);
        assert_eq!(tag.as_deref(), Some("I"));
        assert_eq!(s.encoding_profile.as_deref(), Some("Main Profile 5.1"));
    }

    #[test]
    fn the_alternate_sps_start_code_is_recognized() {
        // 0x00000127 (nal_ref_idc 1) is an SPS start code too.
        let data = vec![0x00, 0x00, 0x01, 0x27, 100, 0x00, 41, 0x00];
        let (s, _) = run(&data);
        assert_eq!(s.encoding_profile.as_deref(), Some("High Profile 4.1"));
    }

    #[test]
    fn no_start_codes_leaves_the_stream_untouched() {
        let (s, tag) = run(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05]);
        assert!(s.encoding_profile.is_none());
        assert!(!s.base.is_initialized);
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
