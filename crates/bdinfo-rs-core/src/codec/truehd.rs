//! Dolby `TrueHD` (MLP) codec scanner — wraps the AC-3 core.
//!
//! A `TrueHD` elementary stream
//! carries a backward-compatible AC-3 core interleaved with the MLP major-sync
//! frames, so [`scan`] does both jobs: when the buffer holds no MLP major sync
//! (`0xF8726FBA`) it tags the access unit `"CORE"` and runs [`crate::codec::ac3`]
//! over the embedded [`core_stream`](TsAudioStream::core_stream); when it does, it
//! tags it `"HD"` and reads the major-sync header for the sample rate, the
//! channel-assignment bits (channel count + LFE), the peak bit depth (16 vs 24),
//! and the Atmos substream-extension (`has_extensions`) flag. The whole stream is
//! initialized only once both the `TrueHD` header and its AC-3 core are.
//!
//! All fixed-width codec math uses `wrapping_*` so hostile field values cannot
//! overflow; the peak bit rate `(peak_bitrate * sample_rate) >> 4` is
//! computed in `i64` then deliberately truncated to its low 32 bits, and the
//! peak-bit-depth division is `f64` (a zero channel/sample-rate divisor yields a
//! non-finite value that simply fails the `> 14` test).

use crate::bitstream::TsStreamBuffer;
use crate::codec::ac3;
use crate::stream::{TsAudioStream, TsStreamType};

/// The MLP major-sync signature scanned for in the access unit (`0xF8726FBA`).
const TRUEHD_MAJOR_SYNC: u32 = 0xF872_6FBA;

/// Scales the 15-bit peak-bit-rate field by the sample rate.
///
/// `(peak_bitrate * sample_rate) >> 4`, computed in `i64` and truncated to its
/// low 32 bits.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::as_conversions,
    reason = "deliberate low-32-bits truncation of the i64 (peak_bitrate * sample_rate) >> 4; TryFrom would wrongly reject it"
)]
fn scale_peak_bitrate(peak_bitrate: u32, sample_rate: i32) -> u32 {
    i64::from(peak_bitrate).wrapping_mul(i64::from(sample_rate)).wrapping_shr(4) as u32
}

/// Scans one `TrueHD` access unit from `buffer` into `stream`.
///
/// With no MLP major sync the embedded AC-3 core is scanned (and `tag` set to
/// `"CORE"`); with one, the `TrueHD` header is read (`tag` set to `"HD"`).
/// `stream` is marked initialized only when its core also is.
#[expect(
    clippy::too_many_lines,
    reason = "one linear major-sync header parse; the 13 channel-assignment reads are clearest inline"
)]
pub fn scan(stream: &mut TsAudioStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    if stream.base.is_initialized
        && stream.core_stream.as_ref().is_some_and(|c| c.base.is_initialized)
    {
        return;
    }

    let mut sync_found = false;
    let mut sync: u32 = 0;
    // A bounded byte-at-a-time scan for the sync over the whole buffer.
    for _ in 0..buffer.length() {
        sync = sync.wrapping_shl(8).wrapping_add(u32::from(buffer.read_byte(false)));
        if sync == TRUEHD_MAJOR_SYNC {
            sync_found = true;
            break;
        }
    }

    if !sync_found {
        *tag = Some("CORE".to_owned());
        if stream.core_stream.is_none() {
            // Seed the embedded AC-3 core stream on first sight.
            let mut core = TsAudioStream::default();
            core.base.stream_type = TsStreamType::Ac3Audio;
            stream.core_stream = Some(Box::new(core));
        }
        if let Some(core) = stream.core_stream.as_mut()
            && !core.base.is_initialized
        {
            buffer.begin_read();
            ac3::scan(core, buffer, tag);
        }
        return;
    }

    *tag = Some("HD".to_owned());
    let ratebits = buffer.read_bits2(4, false);
    if ratebits != 0xF {
        let base_rate: i32 = if (ratebits & 8) > 0 { 44_100 } else { 48_000 };
        stream.sample_rate = base_rate.wrapping_shl(u32::from(ratebits & 7));
    }
    buffer.bs_skip_bits(15, false);

    stream.channel_count = 0;
    stream.lfe = 0;
    if buffer.read_bool(false) {
        stream.lfe = stream.lfe.wrapping_add(1);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(1);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(2);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(2);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(1);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(1);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(2);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(2);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(2);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(2);
    }
    if buffer.read_bool(false) {
        stream.lfe = stream.lfe.wrapping_add(1);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(1);
    }
    if buffer.read_bool(false) {
        stream.channel_count = stream.channel_count.wrapping_add(2);
    }

    buffer.bs_skip_bits(49, false);

    let peak_bitrate = scale_peak_bitrate(buffer.read_bits4(15, false), stream.sample_rate);
    let peak_bitdepth = f64::from(peak_bitrate)
        / f64::from(stream.channel_count.wrapping_add(stream.lfe))
        / f64::from(stream.sample_rate);
    stream.bit_depth = if peak_bitdepth > 14.0 { 24 } else { 16 };

    buffer.bs_skip_bits(79, false);

    let has_extensions = buffer.read_bool(false);
    let num_extensions = i32::from(buffer.read_bits2(4, false)).wrapping_mul(2).wrapping_add(1);
    let mut has_content = buffer.read_bits4(4, false) != 0;

    if has_extensions {
        // Any nonzero byte among the extension substreams counts as content.
        for _ in 0..num_extensions {
            if buffer.read_bits2(8, false) != 0 {
                has_content = true;
            }
        }
        if has_content {
            stream.has_extensions = true;
        }
    }

    stream.base.is_vbr = true;
    if stream.core_stream.as_ref().is_some_and(|c| c.base.is_initialized) {
        stream.base.is_initialized = true;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, proptest};

    use super::scan;
    use crate::bitstream::TsStreamBuffer;
    use crate::stream::{TsAudioStream, TsStreamType};

    /// A rewound buffer holding `data`.
    fn buf(data: &[u8]) -> TsStreamBuffer {
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        b
    }

    /// A fresh audio stream of `stream_type`.
    fn stream(stream_type: TsStreamType) -> TsAudioStream {
        let mut s = TsAudioStream::default();
        s.base.stream_type = stream_type;
        s
    }

    /// Packs `(value, bit_width)` fields MSB-first into bytes (trailing partial byte
    /// left-aligned).
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

    /// The legacy AC-3 5.1 / 48 kHz / 640 kbps / DN -31 core frame the demux feeds
    /// before the `TrueHD` major sync (no `0xF8726FBA` anywhere).
    fn ac3_core_5_1() -> Vec<u8> {
        pack(&[
            (0x0B77, 16),
            (0, 16), // sync + CRC
            (0, 2),
            (36, 6),
            (8, 5),
            (0, 3), // fscod, frmsizecod, bsid 8, bsmod
            (7, 3),
            (0, 2),
            (0, 2),
            (1, 1), // acmod 5.1, mix levels, lfe
            (31, 5),
            (0, 1),
            (0, 1),
            (0, 1), // dialnorm 31, optional flags absent
            (0, 2),
            (0, 16),
        ])
    }

    /// Builds an MLP major-sync (`TrueHD` "HD") frame: the `0xF8726FBA` sync, the
    /// `ratebits`, the 13 channel-assignment bits, the peak bit rate, and the
    /// extension-substream fields.
    fn hd_frame(
        ratebits: u64,
        chan_bits: [u64; 13],
        peak_bitrate: u64,
        has_extensions: u64,
        num_ext_field: u64,
        has_content_field: u64,
        ext_bytes: &[u64],
    ) -> Vec<u8> {
        let mut fields: Vec<(u64, u32)> = vec![(ratebits, 4), (0, 15)];
        for &bit in &chan_bits {
            fields.push((bit, 1));
        }
        fields.push((0, 49));
        fields.push((peak_bitrate, 15));
        fields.push((0, 79));
        fields.push((has_extensions, 1));
        fields.push((num_ext_field, 4));
        fields.push((has_content_field, 4));
        for &e in ext_bytes {
            fields.push((e, 8));
        }
        fields.push((0, 64));
        let mut out = vec![0xF8, 0x72, 0x6F, 0xBA];
        out.extend(pack(&fields));
        out
    }

    /// The 7.1 channel assignment (`channel_count` 7, LFE 1).
    const CHAN_7_1: [u64; 13] = [1, 1, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];

    #[test]
    fn truehd_over_ac3_core_reproduces_the_expected_line() {
        // Mirror the demux: a "CORE" access unit (the AC-3 core) then an "HD" one.
        let mut thd = stream(TsStreamType::Ac3TrueHdAudio);
        let mut tag = None;

        let mut core_buf = buf(&ac3_core_5_1());
        scan(&mut thd, &mut core_buf, &mut tag);
        assert_eq!(tag.as_deref(), Some("CORE"));
        assert_eq!(thd.core_stream.as_deref().map(|c| c.channel_count), Some(5));
        assert_eq!(thd.core_stream.as_deref().map(|c| c.base.bit_rate), Some(640_000));
        assert!(!thd.base.is_initialized); // the HD header has not been seen yet

        let mut hd_buf = buf(&hd_frame(0, CHAN_7_1, 0, 0, 0, 0, &[]));
        scan(&mut thd, &mut hd_buf, &mut tag);
        assert_eq!(tag.as_deref(), Some("HD"));
        assert_eq!(thd.channel_count, 7);
        assert_eq!(thd.lfe, 1);
        assert_eq!(thd.sample_rate, 48_000);
        assert_eq!(thd.bit_depth, 16);
        assert!(!thd.has_extensions);
        assert!(thd.base.is_vbr);
        assert!(thd.base.is_initialized); // core + HD both seen

        assert_eq!(thd.codec_short_name(), "TrueHD");
        assert_eq!(thd.codec_name(), "Dolby TrueHD Audio");
        assert_eq!(
            thd.description(),
            "7.1 / 48 kHz / 16-bit (AC3 Embedded: 5.1 / 48 kHz /   640 kbps / DN -31dB)"
        );
    }

    #[test]
    fn hd_sample_rate_codes() {
        let rate = |ratebits| {
            let mut s = stream(TsStreamType::Ac3TrueHdAudio);
            let mut b = buf(&hd_frame(ratebits, CHAN_7_1, 0, 0, 0, 0, &[]));
            scan(&mut s, &mut b, &mut None);
            s.sample_rate
        };
        assert_eq!(rate(0), 48_000); // base 48k, shift 0
        assert_eq!(rate(1), 96_000); // base 48k << 1
        assert_eq!(rate(8), 44_100); // ratebits & 8 → base 44.1k
        assert_eq!(rate(0xF), 0); // 0xF → sample_rate left unset
    }

    #[test]
    fn hd_all_channel_bits_and_24_bit_depth() {
        // Every channel-assignment bit set exercises all 13 add arms; a large peak
        // bit rate drives the 24-bit branch (peak_bitdepth = 5000/320 ≈ 15.6 > 14).
        let mut s = stream(TsStreamType::Ac3TrueHdAudio);
        let mut b = buf(&hd_frame(0, [1; 13], 5000, 0, 0, 0, &[]));
        scan(&mut s, &mut b, &mut None);
        // LFE bits at positions 1 and 11 (=2); the remaining eleven add to channels.
        assert_eq!(s.lfe, 2);
        assert_eq!(s.channel_count, 18);
        assert_eq!(s.bit_depth, 24);
    }

    #[test]
    fn hd_bit_depth_threshold_is_exact() {
        // 7.1 (channel_count + LFE = 8): peak_bitdepth = scale_peak_bitrate(peak,48k)
        // / 8 / 48k, which simplifies to peak/128. The `> 14` boundary is exact at
        // peak 1792 (14.0, still 16-bit). These pin every operator in the formula.
        let depth = |peak| {
            let mut s = stream(TsStreamType::Ac3TrueHdAudio);
            let mut b = buf(&hd_frame(0, CHAN_7_1, peak, 0, 0, 0, &[]));
            scan(&mut s, &mut b, &mut None);
            s.bit_depth
        };
        assert_eq!(depth(1), 16);
        assert_eq!(depth(256), 16);
        assert_eq!(depth(1792), 16); // peak_bitdepth == 14.0 exactly → not > 14
        assert_eq!(depth(1793), 24); // just over the threshold
        assert_eq!(depth(5000), 24);
    }

    #[test]
    fn hd_all_channel_bits_clear() {
        // No channel-assignment bits set → the false arm of every add; a zero
        // channel/LFE count makes peak_bitdepth NaN (not > 14) → 16-bit.
        let mut s = stream(TsStreamType::Ac3TrueHdAudio);
        let mut b = buf(&hd_frame(0, [0; 13], 0, 0, 0, 0, &[]));
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.channel_count, 0);
        assert_eq!(s.lfe, 0);
        assert_eq!(s.bit_depth, 16);
    }

    #[test]
    fn hd_atmos_extension_detection() {
        // has_extensions with a nonzero extension byte in range flags the stream.
        let mut s = stream(TsStreamType::Ac3TrueHdAudio);
        // num_ext_field 1 → num_extensions = 1*2 + 1 = 3; the 3rd byte is nonzero.
        let mut b = buf(&hd_frame(0, CHAN_7_1, 0, 1, 1, 0, &[0, 0, 1]));
        scan(&mut s, &mut b, &mut None);
        assert!(s.has_extensions);
        assert_eq!(s.codec_short_name(), "Atmos");
        assert_eq!(s.codec_name(), "Dolby TrueHD/Atmos Audio");

        // has_extensions with the initial has_content bit set (nonzero read_bits4(4)),
        // even with all-zero extension bytes, still flags the stream.
        let mut s = stream(TsStreamType::Ac3TrueHdAudio);
        let mut b = buf(&hd_frame(0, CHAN_7_1, 0, 1, 0, 1, &[0]));
        scan(&mut s, &mut b, &mut None);
        assert!(s.has_extensions);

        // has_extensions but no content in range → not flagged. num_ext_field 0 →
        // num_extensions 1, so only the first (zero) byte is read; the nonzero second
        // byte sits *past* the loop bound (a guard against an off-by-one loop end).
        let mut s = stream(TsStreamType::Ac3TrueHdAudio);
        let mut b = buf(&hd_frame(0, CHAN_7_1, 0, 1, 0, 0, &[0, 1]));
        scan(&mut s, &mut b, &mut None);
        assert!(!s.has_extensions);
    }

    #[test]
    fn core_scan_skips_when_core_already_initialized() {
        // A second "CORE" access unit when the core is already initialized must not
        // re-run the AC-3 scan (the not-yet-initialized guard on the core).
        let mut thd = stream(TsStreamType::Ac3TrueHdAudio);
        let mut tag = None;
        let mut first = buf(&ac3_core_5_1());
        scan(&mut thd, &mut first, &mut tag);
        assert_eq!(thd.core_stream.as_deref().map(|c| c.channel_count), Some(5));
        // Feed a *different* AC-3 frame as a second CORE unit; the core is untouched.
        let other = pack(&[
            (0x0B77, 16),
            (0, 16),
            (0, 2),
            (36, 6),
            (8, 5),
            (0, 3),
            (1, 3),
            (0, 1),
            (0, 5),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 2),
            (0, 16),
        ]);
        let mut second = buf(&other);
        scan(&mut thd, &mut second, &mut tag);
        assert_eq!(thd.core_stream.as_deref().map(|c| c.channel_count), Some(5));
    }

    #[test]
    fn fully_initialized_stream_returns_immediately() {
        // is_initialized AND an initialized core → the scan is a no-op.
        let mut thd = stream(TsStreamType::Ac3TrueHdAudio);
        thd.channel_count = 99;
        thd.base.is_initialized = true;
        let mut core = TsAudioStream::default();
        core.base.stream_type = TsStreamType::Ac3Audio;
        core.base.is_initialized = true;
        thd.core_stream = Some(Box::new(core));

        let mut b = buf(&hd_frame(0, CHAN_7_1, 0, 0, 0, 0, &[]));
        scan(&mut thd, &mut b, &mut None);
        assert_eq!(thd.channel_count, 99); // untouched
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>()) {
            let mut s = stream(TsStreamType::Ac3TrueHdAudio);
            let mut b = buf(&data);
            let mut tag = None;
            scan(&mut s, &mut b, &mut tag);
        }
    }
}
