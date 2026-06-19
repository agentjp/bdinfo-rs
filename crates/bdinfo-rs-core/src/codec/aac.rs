//! MPEG-2/4 AAC audio codec scanner.
//!
//! [`scan`] reads the ADTS fixed
//! header (after the 12-bit `0xFFF` sync word) and fills the [`TsAudioStream`]
//! codec fields: sample rate, channel count + LFE, audio mode, and the
//! `ext_data` string (`"<MPEG id> <profile>"`, e.g. `"MPEG-4 AAC LC"`) shown
//! as the report's long codec name. The stream is flagged variable-bit-rate.
//!
//! The lookups are sized to the raw field widths, so no range guards are needed.
//! The 16-entry [`AAC_SAMPLE_RATES`] table covers the whole 4-bit
//! `samplingRateIndex`, with the reserved indices 13–15 zeroed (a folded table
//! beats a boundary comparison that could never legitimately fire). A 3-bit
//! `channelMode` can never exceed the 8-entry channel tables, so there is no
//! unreachable fallback and the reads stay bounds-safe with `.get()`; mode 7 is
//! the only value that carries an LFE channel, so the LFE test is a plain `== 7`.

use crate::bitstream::TsStreamBuffer;
use crate::stream::{TsAudioMode, TsAudioStream};

/// MPEG audio-version label per the 1-bit `audioVersionID`.
const AAC_ID: [&str; 2] = ["MPEG-4", "MPEG-2"];

/// Sample rate in Hz per the 4-bit `samplingRateIndex`, folded over the whole
/// field — the reserved indices 13–15 are `0`.
const AAC_SAMPLE_RATES: [i32; 16] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350, 0, 0, 0,
];

/// Channel count per the 3-bit `channelMode`.
const AAC_CHANNELS: [i32; 8] = [0, 1, 2, 3, 4, 5, 6, 8];

/// Audio mode per the 3-bit `channelMode`.
const AAC_CHANNEL_MODES: [TsAudioMode; 8] = [
    TsAudioMode::Unknown,
    TsAudioMode::Mono,
    TsAudioMode::Stereo,
    TsAudioMode::Extended,
    TsAudioMode::Surround,
    TsAudioMode::Surround,
    TsAudioMode::Surround,
    TsAudioMode::Surround,
];

/// Maps a `profileObjectType` to its AAC profile label. The scan only reaches the
/// 2-bit `0..=3` profiles; the extended codes (`16`/`18`/`36`) and the unknown
/// fallback cover the wider object-type space and are exercised directly by the
/// unit tests.
const fn get_aac_profile(profile_type: u16) -> &'static str {
    match profile_type {
        0 => "AAC Main",
        1 => "AAC LC",
        2 => "AAC SSR",
        3 => "AAC LTP",
        16 => "ER AAC LC",
        18 => "ER AAC LTP",
        36 => "SLS",
        _ => "",
    }
}

/// Scans one AAC access unit from `buffer` into `stream`.
///
/// A non-`0xFFF` sync word leaves `stream` untouched (an early return). `tag`
/// is part of the shared codec-scan signature and is never set here.
pub fn scan(stream: &mut TsAudioStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    // `tag` is part of the shared codec-scan signature; AAC never sets it (a
    // `pub fn` is exempt from `needless_pass_by_ref_mut`).
    let _ = tag;
    if stream.base.is_initialized {
        return;
    }

    let sync_word = buffer.read_bits2(12, false);
    if sync_word != 0b1111_1111_1111 {
        return;
    }

    // Fixed header.
    let audio_version_id = buffer.read_bits2(1, false);
    let _ = buffer.read_bits2(2, false); // Layer Index
    let _ = buffer.read_bool(false); // Protection Absent
    let profile_object_type = buffer.read_bits2(2, false);
    let sampling_rate_index = buffer.read_bits2(4, false);
    let _ = buffer.read_bool(false); // Private Bit
    let channel_mode = buffer.read_bits2(3, false);
    let _ = buffer.read_bool(false); // Original Bit
    let _ = buffer.read_bool(false); // Home

    // The folded table returns `0` for the reserved indices 13–15.
    stream.sample_rate =
        AAC_SAMPLE_RATES.get(usize::from(sampling_rate_index)).copied().unwrap_or(0);

    // A 3-bit `channelMode` can never exceed the 8-entry tables; `.get()` keeps
    // the index bounds-safe regardless.
    stream.audio_mode =
        AAC_CHANNEL_MODES.get(usize::from(channel_mode)).copied().unwrap_or(TsAudioMode::Unknown);
    stream.channel_count = AAC_CHANNELS.get(usize::from(channel_mode)).copied().unwrap_or(0);

    // Mode 7 is the only 3-bit `channelMode` that carries an LFE channel.
    if channel_mode == 7 {
        stream.channel_count = stream.channel_count.wrapping_sub(1);
        stream.lfe = 1;
    } else {
        stream.lfe = 0;
    }

    let id = AAC_ID.get(usize::from(audio_version_id)).copied().unwrap_or("");
    stream.ext_data = Some(format!("{id} {}", get_aac_profile(profile_object_type)));

    stream.base.is_vbr = true;
    stream.base.is_initialized = true;
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, proptest};

    use super::{get_aac_profile, scan};
    use crate::bitstream::TsStreamBuffer;
    use crate::stream::{TsAudioMode, TsAudioStream, TsStreamType};

    /// A rewound buffer holding `data`.
    fn buf(data: &[u8]) -> TsStreamBuffer {
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        b
    }

    /// Packs `(value, bit_width)` fields MSB-first into bytes; a trailing partial
    /// byte is left-aligned, matching how the bit reader consumes them.
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

    /// The ADTS fixed-header field sequence for the given `version`/`profile`/
    /// `sr_idx`/`chan` (sync, then the fixed header up through `home`), padded.
    fn aac_frame(version: u64, profile: u64, sr_idx: u64, chan: u64) -> Vec<u8> {
        pack(&[
            (0xFFF, 12),  // sync word
            (version, 1), // audioVersionID
            (0, 2),       // layer index
            (0, 1),       // protection absent
            (profile, 2), // profileObjectType
            (sr_idx, 4),  // samplingRateIndex
            (0, 1),       // private bit
            (chan, 3),    // channelMode
            (0, 1),       // original bit
            (0, 1),       // home
            (0, 16),      // padding
        ])
    }

    /// Runs [`scan`] over an AAC frame and returns the mutated stream.
    fn run(
        stream_type: TsStreamType,
        version: u64,
        profile: u64,
        sr_idx: u64,
        chan: u64,
    ) -> TsAudioStream {
        let mut s = TsAudioStream::default();
        s.base.stream_type = stream_type;
        let mut b = buf(&aac_frame(version, profile, sr_idx, chan));
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        assert_eq!(tag, None, "AAC never sets the tag");
        s
    }

    #[test]
    fn decodes_mpeg4_aac_lc_stereo() {
        // version 0 (MPEG-4), profile 1 (LC), sr index 3 (48 kHz), channel mode 2.
        let s = run(TsStreamType::Mpeg4AacAudio, 0, 1, 3, 2);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.channel_count, 2);
        assert_eq!(s.lfe, 0);
        assert_eq!(s.audio_mode, TsAudioMode::Stereo);
        assert_eq!(s.ext_data.as_deref(), Some("MPEG-4 AAC LC"));
        assert!(s.base.is_vbr);
        assert!(s.base.is_initialized);
        assert_eq!(s.codec_short_name(), "MPEG-4 AAC");
        // codec_name() for the MPEG/AAC types is the ext_data string.
        assert_eq!(s.codec_name(), "MPEG-4 AAC LC");
        assert_eq!(s.description(), "2.0 / 48 kHz");
    }

    #[test]
    fn audio_version_and_profile_compose_extended_data() {
        // version 1 → "MPEG-2"; each 2-bit profile reachable by the scan.
        assert_eq!(
            run(TsStreamType::Mpeg2AacAudio, 1, 0, 3, 2).ext_data.as_deref(),
            Some("MPEG-2 AAC Main")
        );
        assert_eq!(
            run(TsStreamType::Mpeg4AacAudio, 0, 0, 3, 2).ext_data.as_deref(),
            Some("MPEG-4 AAC Main")
        );
        assert_eq!(
            run(TsStreamType::Mpeg4AacAudio, 0, 2, 3, 2).ext_data.as_deref(),
            Some("MPEG-4 AAC SSR")
        );
        assert_eq!(
            run(TsStreamType::Mpeg4AacAudio, 0, 3, 3, 2).ext_data.as_deref(),
            Some("MPEG-4 AAC LTP")
        );
        // The MPEG-2 AAC type reports its own short name.
        assert_eq!(run(TsStreamType::Mpeg2AacAudio, 1, 1, 3, 2).codec_short_name(), "MPEG-2 AAC");
    }

    #[test]
    fn channel_mode_maps_count_mode_and_lfe() {
        // (channel mode, expected count, expected LFE, expected audio mode).
        let cases = [
            (0_u64, 0_i32, 0_i32, TsAudioMode::Unknown),
            (1, 1, 0, TsAudioMode::Mono),
            (2, 2, 0, TsAudioMode::Stereo),
            (3, 3, 0, TsAudioMode::Extended),
            (4, 4, 0, TsAudioMode::Surround),
            (5, 5, 0, TsAudioMode::Surround),
            (6, 6, 0, TsAudioMode::Surround),
            // Mode 7: AAC_CHANNELS[7] = 8, decremented to 7 with LFE on (the `== 7` arm).
            (7, 7, 1, TsAudioMode::Surround),
        ];
        for (chan, count, lfe, mode) in cases {
            let s = run(TsStreamType::Mpeg4AacAudio, 0, 1, 3, chan);
            assert_eq!((s.channel_count, s.lfe), (count, lfe), "channel mode {chan}");
            assert_eq!(s.audio_mode, mode, "channel mode {chan}");
        }
    }

    #[test]
    fn sample_rate_index_folds_reserved_codes_to_zero() {
        // The first/last meaningful codes and the reserved 13–15 (all → 0).
        assert_eq!(run(TsStreamType::Mpeg4AacAudio, 0, 1, 0, 2).sample_rate, 96_000);
        assert_eq!(run(TsStreamType::Mpeg4AacAudio, 0, 1, 12, 2).sample_rate, 7_350);
        assert_eq!(run(TsStreamType::Mpeg4AacAudio, 0, 1, 13, 2).sample_rate, 0);
        assert_eq!(run(TsStreamType::Mpeg4AacAudio, 0, 1, 14, 2).sample_rate, 0);
        assert_eq!(run(TsStreamType::Mpeg4AacAudio, 0, 1, 15, 2).sample_rate, 0);
    }

    #[test]
    fn get_aac_profile_covers_every_arm() {
        assert_eq!(get_aac_profile(0), "AAC Main");
        assert_eq!(get_aac_profile(1), "AAC LC");
        assert_eq!(get_aac_profile(2), "AAC SSR");
        assert_eq!(get_aac_profile(3), "AAC LTP");
        assert_eq!(get_aac_profile(16), "ER AAC LC");
        assert_eq!(get_aac_profile(18), "ER AAC LTP");
        assert_eq!(get_aac_profile(36), "SLS");
        assert_eq!(get_aac_profile(99), "");
    }

    #[test]
    fn rejects_bad_sync_and_initialized_streams() {
        // Wrong sync word → untouched.
        let s = run(TsStreamType::Mpeg4AacAudio, 0, 1, 3, 2);
        assert!(s.base.is_initialized); // (sanity: the good frame initializes)
        let mut s = TsAudioStream::default();
        s.base.stream_type = TsStreamType::Mpeg4AacAudio;
        let mut b = buf(&pack(&[(0xFFE, 12), (0, 28)])); // sync 0xFFE ≠ 0xFFF
        scan(&mut s, &mut b, &mut None);
        assert!(!s.base.is_initialized);
        assert_eq!(s.ext_data, None);

        // An already-initialized stream returns immediately without parsing.
        let mut s = TsAudioStream::default();
        s.base.stream_type = TsStreamType::Mpeg4AacAudio;
        s.base.is_initialized = true;
        let mut b = buf(&aac_frame(0, 1, 3, 2));
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.channel_count, 0); // never parsed
        assert_eq!(s.ext_data, None);
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>()) {
            for ty in [TsStreamType::Mpeg2AacAudio, TsStreamType::Mpeg4AacAudio] {
                let mut s = TsAudioStream::default();
                s.base.stream_type = ty;
                let mut b = buf(&data);
                let mut tag = None;
                scan(&mut s, &mut b, &mut tag);
            }
        }
    }
}
