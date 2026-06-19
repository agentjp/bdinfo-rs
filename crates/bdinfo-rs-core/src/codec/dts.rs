//! DTS (DCA) core audio codec scanner.
//!
//! [`scan`] reads one assembled audio
//! access unit and fills the [`TsAudioStream`] codec fields from the DTS core
//! sub-stream frame header (after the `0x7FFE8001` sync): sample rate, channel
//! count + LFE, source PCM resolution (bit depth), dialogue normalization, the
//! DTS-ES `Extended` audio mode, and the nominal bit rate (with the open /
//! variable / lossless markers handled by the bit-rate match).
//!
//! `bitrate` is the demux-measured stream bit rate handed to every DTS scan; it
//! is used only for the "open" bit-rate marker. The DTS core never sets `tag`;
//! the wrapping [`crate::codec::dts_hd`] scanner sets it.
//!
//! All fixed-width codec math uses `wrapping_*` so hostile field values cannot
//! overflow. The 4-bit sample-rate and 5-bit bit-rate codes can never reach their
//! 16- and 32-entry table bounds, so those lookups carry no range guard; the
//! 3-bit source-PCM-resolution code *can* reach the 7-entry bit-depth table's
//! length (code 7), so that lookup is a bounds-checked `.get()` whose miss is an
//! early return.

use crate::bitstream::TsStreamBuffer;
use crate::stream::{TsAudioMode, TsAudioStream};

/// The DTS core sub-stream sync word scanned for in the access unit
/// (`0x7FFE8001`).
const DTS_SYNC: u32 = 0x7FFE_8001;

/// Sample rate in Hz per the 4-bit `sfreq` code; reserved codes are `0`.
const DCA_SAMPLE_RATES: [i32; 16] = [
    0, 8_000, 16_000, 32_000, 0, 0, 11_025, 22_050, 44_100, 0, 0, 12_000, 24_000, 48_000, 96_000,
    192_000,
];

/// Nominal bit rate in bits/s per the 5-bit `rate` code. The last
/// three entries are not rates but markers: `1` = open, `2` = variable, `3` =
/// lossless — handled by the bit-rate `match` below.
const DCA_BIT_RATES: [i32; 32] = [
    32_000, 56_000, 64_000, 96_000, 112_000, 128_000, 192_000, 224_000, 256_000, 320_000, 384_000,
    448_000, 512_000, 576_000, 640_000, 768_000, 896_000, 1_024_000, 1_152_000, 1_280_000,
    1_344_000, 1_408_000, 1_411_200, 1_472_000, 1_536_000, 1_920_000, 2_048_000, 3_072_000,
    3_840_000, 1, 2, 3,
];

/// Source PCM resolution (bit depth) per the 3-bit `pcmr` code;
/// an odd code additionally signals the DTS-ES `Extended` mode.
const DCA_BITS_PER_SAMPLE: [i32; 7] = [16, 16, 20, 20, 0, 24, 24];

/// Scans one DTS core access unit from `buffer` into `stream`.
///
/// A missing `0x7FFE8001` sync, a frame size below 95, or an out-of-range source
/// PCM resolution leaves `stream` (largely) untouched (early returns).
/// `bitrate` is the demux-measured rate (used only for the open-rate marker);
/// `tag` is part of the shared codec-scan signature and is never set here.
pub fn scan(
    stream: &mut TsAudioStream,
    buffer: &mut TsStreamBuffer,
    bitrate: i64,
    tag: &mut Option<String>,
) {
    // `tag` is part of the shared codec-scan signature; the DTS core never sets it
    // (a `pub fn` is exempt from `needless_pass_by_ref_mut`).
    let _ = tag;
    if stream.base.is_initialized {
        return;
    }

    let mut sync_found = false;
    let mut sync: u32 = 0;
    // A bounded byte-at-a-time scan for the sync over the whole buffer.
    for _ in 0..buffer.length() {
        sync = sync.wrapping_shl(8).wrapping_add(u32::from(buffer.read_byte(false)));
        if sync == DTS_SYNC {
            sync_found = true;
            break;
        }
    }
    if !sync_found {
        return;
    }

    buffer.bs_skip_bits(6, false);
    let crc_present = buffer.read_bits4(1, false);
    buffer.bs_skip_bits(7, false);
    let frame_size = buffer.read_bits4(14, false);
    if frame_size < 95 {
        return;
    }
    buffer.bs_skip_bits(6, false);
    let sample_rate_code = buffer.read_bits4(4, false);
    // (A 4-bit code is always < 16, within the sample-rate table — no guard
    // needed.)
    let bit_rate_code = buffer.read_bits4(5, false);
    // (A 5-bit code is always < 32, within the bit-rate table — no guard needed.)
    buffer.bs_skip_bits(8, false);
    let ext_coding = buffer.read_bits4(1, false);
    buffer.bs_skip_bits(1, false);
    let lfe = buffer.read_bits4(2, false);
    buffer.bs_skip_bits(1, false);
    if crc_present == 1 {
        buffer.bs_skip_bits(16, false);
    }
    buffer.bs_skip_bits(7, false);
    let source_pcm_res = buffer.read_bits4(3, false);
    buffer.bs_skip_bits(2, false);
    let dialog_norm = buffer.read_bits4(4, false);
    // A 3-bit code reaches 7 — exactly the bit-depth table's length — so this
    // bound is live; `.get()` expresses it without a separate comparison.
    let pcm_idx = usize::try_from(source_pcm_res).unwrap_or(usize::MAX);
    let Some(&bit_depth) = DCA_BITS_PER_SAMPLE.get(pcm_idx) else {
        return;
    };
    buffer.bs_skip_bits(4, false);
    let total_channels = buffer.read_bits4(3, false).wrapping_add(1).wrapping_add(ext_coding);

    let sr_idx = usize::try_from(sample_rate_code).unwrap_or(usize::MAX);
    stream.sample_rate = DCA_SAMPLE_RATES.get(sr_idx).copied().unwrap_or(0);
    stream.channel_count = i32::try_from(total_channels).unwrap_or(0);
    stream.lfe = i32::from(lfe > 0);
    stream.bit_depth = bit_depth;
    stream.dial_norm = i32::try_from(dialog_norm).unwrap_or(0).wrapping_mul(-1);
    if (source_pcm_res & 0x1) == 0x1 {
        stream.audio_mode = TsAudioMode::Extended;
    }

    let br_idx = usize::try_from(bit_rate_code).unwrap_or(usize::MAX);
    stream.base.bit_rate = i64::from(DCA_BIT_RATES.get(br_idx).copied().unwrap_or(0));
    match stream.base.bit_rate {
        // Open: take the demux-measured rate when one is known, else leave it unset.
        1 => {
            if bitrate > 0 {
                stream.base.bit_rate = bitrate;
                stream.base.is_vbr = false;
                stream.base.is_initialized = true;
            } else {
                stream.base.bit_rate = 0;
            }
        }
        // Variable / lossless.
        2 | 3 => {
            stream.base.is_vbr = true;
            stream.base.is_initialized = true;
        }
        // A fixed nominal rate.
        _ => {
            stream.base.is_vbr = false;
            stream.base.is_initialized = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, proptest};

    use super::scan;
    use crate::bitstream::TsStreamBuffer;
    use crate::stream::{TsAudioMode, TsAudioStream, TsStreamType};

    /// A rewound buffer holding `data`.
    fn buf(data: &[u8]) -> TsStreamBuffer {
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        b
    }

    /// A fresh DTS audio stream.
    fn stream() -> TsAudioStream {
        let mut s = TsAudioStream::default();
        s.base.stream_type = TsStreamType::DtsAudio;
        s
    }

    /// Packs `(value, bit_width)` fields MSB-first into bytes; a trailing partial
    /// byte is left-aligned (low bits zero), matching how the bit reader consumes
    /// them.
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

    /// One DTS core frame: the `0x7FFE8001` sync followed by the header fields up to
    /// the channel-count base, plus zero tail padding. `crc_present` 0 keeps the
    /// 16-bit CRC field absent.
    #[expect(clippy::too_many_arguments, reason = "spells one DTS header field-by-field")]
    fn dts_frame(
        crc_present: u64,
        frame_size: u64,
        sample_rate_code: u64,
        bit_rate_code: u64,
        ext_coding: u64,
        lfe: u64,
        source_pcm_res: u64,
        dialog_norm: u64,
        total_ch_base: u64,
    ) -> Vec<u8> {
        let mut f: Vec<(u64, u32)> = vec![
            (0x7FFE_8001, 32),
            (0, 6),
            (crc_present, 1),
            (0, 7),
            (frame_size, 14),
            (0, 6),
            (sample_rate_code, 4),
            (bit_rate_code, 5),
            (0, 8),
            (ext_coding, 1),
            (0, 1),
            (lfe, 2),
            (0, 1),
        ];
        if crc_present == 1 {
            f.push((0, 16));
        }
        f.push((0, 7));
        f.push((source_pcm_res, 3));
        f.push((0, 2));
        f.push((dialog_norm, 4));
        f.push((0, 4));
        f.push((total_ch_base, 3));
        f.push((0, 64));
        pack(&f)
    }

    /// Runs [`scan`] over the bytes and returns the mutated stream.
    fn run(bytes: &[u8], bitrate: i64) -> TsAudioStream {
        let mut s = stream();
        let mut b = buf(bytes);
        let mut tag = None;
        scan(&mut s, &mut b, bitrate, &mut tag);
        assert_eq!(tag, None, "the DTS core never sets the tag");
        s
    }

    #[test]
    fn pack_handles_byte_aligned_and_partial_inputs() {
        // 16 bits → exactly two whole bytes (the trailing-partial path is skipped).
        assert_eq!(pack(&[(0xABCD, 16)]), vec![0xAB, 0xCD]);
        // 12 bits → one whole byte plus a left-aligned partial byte.
        assert_eq!(pack(&[(0xABC, 12)]), vec![0xAB, 0xC0]);
    }

    #[test]
    fn core_5_1_48k_1536_kbps_matches_the_embedded_core_line() {
        // The DTS core line embedded in every DTS-HD MA `desc` (the 16-bit variant
        // from a real disc sample): 5.1 / 48 kHz / 1536 kbps. Code 24 is the
        // spec/FFmpeg rate (ETSI TS 102 114); BDInfo historically printed 1509.
        // sample_rate 13 → 48 kHz, bit_rate 24 → 1536000, pcmr 0 → 16-bit (not -ES),
        // total_ch_base 4 (+1) → 5 channels, lfe on.
        let s = run(&dts_frame(0, 100, 13, 24, 0, 1, 0, 0, 4), 0);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.channel_count, 5);
        assert_eq!(s.lfe, 1);
        assert_eq!(s.bit_depth, 16);
        assert_eq!(s.dial_norm, 0);
        assert_eq!(s.base.bit_rate, 1_536_000);
        assert_eq!(s.audio_mode, TsAudioMode::Unknown);
        assert!(s.base.is_initialized);
        assert!(!s.base.is_vbr);
        assert_eq!(s.codec_short_name(), "DTS");
        assert_eq!(s.codec_name(), "DTS Audio");
        assert_eq!(s.description(), "5.1 / 48 kHz /  1536 kbps / 16-bit");
    }

    #[test]
    fn core_24bit_es_and_dialnorm() {
        // pcmr 5 → 24-bit and odd → DTS-ES `Extended`; pcmr 5 is bit-depth 24.
        // dialog_norm 6 → DN -6. sample_rate 13 → 48 kHz, total_ch_base 1 (+1) → 2.
        let s = run(&dts_frame(0, 100, 13, 24, 0, 0, 5, 6, 1), 0);
        assert_eq!(s.bit_depth, 24);
        assert_eq!(s.audio_mode, TsAudioMode::Extended);
        assert_eq!(s.lfe, 0);
        assert_eq!(s.channel_count, 2);
        assert_eq!(s.dial_norm, -6);
        assert_eq!(s.codec_short_name(), "DTS-ES");
        assert_eq!(s.codec_name(), "DTS-ES Audio");
        // The `-ES` channel suffix and the DN tag both render.
        assert_eq!(s.description(), "2.0-ES / 48 kHz /  1536 kbps / 24-bit / DN -6dB");
    }

    #[test]
    fn core_24bit_non_es_uses_even_pcmr() {
        // pcmr 6 → 24-bit but even → not Extended (the 24-bit DTS core seen on
        // real discs). ext_coding 1 adds one to the channel total.
        let s = run(&dts_frame(0, 100, 13, 24, 1, 1, 6, 0, 3), 0);
        assert_eq!(s.bit_depth, 24);
        assert_eq!(s.audio_mode, TsAudioMode::Unknown);
        // total = 3 + 1 + ext_coding(1) = 5.
        assert_eq!(s.channel_count, 5);
        assert_eq!(s.description(), "5.1 / 48 kHz /  1536 kbps / 24-bit");
    }

    #[test]
    fn crc_present_skips_the_extra_field() {
        // crc_present 1 → the 16-bit CRC field is consumed; the header still aligns
        // and the same fields decode (5.1 / 48 kHz / 1536 kbps / 16-bit).
        let s = run(&dts_frame(1, 100, 13, 24, 0, 1, 0, 0, 4), 0);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.channel_count, 5);
        assert_eq!(s.bit_depth, 16);
        assert_eq!(s.base.bit_rate, 1_536_000);
    }

    #[test]
    fn sample_rate_codes() {
        let rate = |code| run(&dts_frame(0, 100, code, 24, 0, 0, 0, 0, 1), 0).sample_rate;
        assert_eq!(rate(1), 8_000);
        assert_eq!(rate(8), 44_100);
        assert_eq!(rate(13), 48_000);
        assert_eq!(rate(15), 192_000);
        // A reserved code (4) yields 0.
        assert_eq!(rate(4), 0);
    }

    #[test]
    fn lfe_field_is_clamped_to_zero_or_one() {
        // Any nonzero `lfe` field → LFE 1; zero → LFE 0.
        assert_eq!(run(&dts_frame(0, 100, 13, 24, 0, 0, 0, 0, 1), 0).lfe, 0);
        assert_eq!(run(&dts_frame(0, 100, 13, 24, 0, 1, 0, 0, 1), 0).lfe, 1);
        assert_eq!(run(&dts_frame(0, 100, 13, 24, 0, 3, 0, 0, 1), 0).lfe, 1);
    }

    #[test]
    fn frame_size_boundary_at_95() {
        // < 95 → early return (untouched); >= 95 → parsed.
        let small = run(&dts_frame(0, 94, 13, 24, 0, 1, 0, 0, 4), 0);
        assert!(!small.base.is_initialized);
        assert_eq!(small.channel_count, 0);
        let boundary = run(&dts_frame(0, 95, 13, 24, 0, 1, 0, 0, 4), 0);
        assert!(boundary.base.is_initialized);
        assert_eq!(boundary.channel_count, 5);
        let big = run(&dts_frame(0, 16_383, 13, 24, 0, 1, 0, 0, 4), 0);
        assert!(big.base.is_initialized);
    }

    #[test]
    fn source_pcm_res_out_of_range_returns() {
        // pcmr 7 is past the 7-entry bit-depth table → early return (untouched).
        let s = run(&dts_frame(0, 100, 13, 24, 0, 1, 7, 0, 4), 0);
        assert!(!s.base.is_initialized);
        assert_eq!(s.bit_depth, 0);
        assert_eq!(s.channel_count, 0);
        // pcmr 6 (the largest in-range code) parses normally.
        let ok = run(&dts_frame(0, 100, 13, 24, 0, 1, 6, 0, 4), 0);
        assert!(ok.base.is_initialized);
        assert_eq!(ok.bit_depth, 24);
    }

    #[test]
    fn bit_rate_open_marker_uses_the_measured_rate() {
        // rate code 29 → marker 1 (open). With a measured bitrate it is adopted and
        // the stream initializes CBR; without one the rate stays 0 and the stream is
        // left uninitialized.
        let with = run(&dts_frame(0, 100, 13, 29, 0, 1, 0, 0, 4), 768_000);
        assert_eq!(with.base.bit_rate, 768_000);
        assert!(with.base.is_initialized);
        assert!(!with.base.is_vbr);

        let without = run(&dts_frame(0, 100, 13, 29, 0, 1, 0, 0, 4), 0);
        assert_eq!(without.base.bit_rate, 0);
        assert!(!without.base.is_initialized);
    }

    #[test]
    fn bit_rate_variable_and_lossless_markers_are_vbr() {
        // rate codes 30 / 31 → markers 2 / 3 (variable / lossless) → VBR, init.
        for code in [30_u64, 31] {
            let s = run(&dts_frame(0, 100, 13, code, 0, 1, 0, 0, 4), 0);
            assert!(s.base.is_vbr, "rate code {code} is VBR");
            assert!(s.base.is_initialized);
        }
    }

    #[test]
    fn bit_rate_fixed_rate_is_cbr() {
        // A fixed nominal rate (code 14 → 640000) → CBR, init.
        let s = run(&dts_frame(0, 100, 13, 14, 0, 1, 0, 0, 4), 0);
        assert_eq!(s.base.bit_rate, 640_000);
        assert!(!s.base.is_vbr);
        assert!(s.base.is_initialized);
    }

    #[test]
    fn rejects_missing_sync_and_already_initialized() {
        // No DTS sync anywhere → untouched.
        let s = run(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55], 0);
        assert!(!s.base.is_initialized);
        assert_eq!(s.channel_count, 0);

        // An already-initialized stream returns immediately.
        let mut s = stream();
        s.base.is_initialized = true;
        let mut b = buf(&dts_frame(0, 100, 13, 24, 0, 1, 0, 0, 4));
        scan(&mut s, &mut b, 0, &mut None);
        assert_eq!(s.channel_count, 0);
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>(), bitrate in any::<i64>()) {
            let mut s = stream();
            let mut b = buf(&data);
            let mut tag = None;
            scan(&mut s, &mut b, bitrate, &mut tag);
        }
    }
}
