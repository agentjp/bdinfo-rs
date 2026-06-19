//! Linear PCM (LPCM) audio codec scanner.
//!
//! [`scan`] reads the four-byte LPCM
//! audio header and fills the [`TsAudioStream`] codec fields: channel count + LFE,
//! bit depth, sample rate, and the derived nominal bit rate. The three matches
//! decode the packed 16-bit `flags` word (the third and fourth header bytes):
//! the channel-assignment nibble (`flags & 0xF000`), the bit-depth code
//! (`flags & 0xC0`), and the sample-rate nibble (`flags & 0xF00`).
//!
//! The nominal bit rate is `sample_rate * bit_depth * (channel_count + lfe)`,
//! computed with `wrapping_*` and truncated through the unsigned 32-bit view
//! before widening to the `i64` bit-rate field — the decoded factors are small,
//! so the product never actually wraps. A buffer too short for the four-byte
//! header returns early and leaves the stream untouched.

use crate::bitstream::TsStreamBuffer;
use crate::stream::TsAudioStream;

/// Scans one LPCM access unit from `buffer` into `stream`.
///
/// A buffer too short for the four-byte header leaves `stream` untouched (an
/// early return). `tag` is part of the shared codec-scan signature and is never
/// set here.
pub fn scan(stream: &mut TsAudioStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    // `tag` is part of the shared codec-scan signature; LPCM never sets it (a
    // `pub fn` is exempt from `needless_pass_by_ref_mut`).
    let _ = tag;
    if stream.base.is_initialized {
        return;
    }

    let Some(header) = buffer.read_bytes(4) else {
        return;
    };
    // `flags` = the third/fourth header bytes as a big-endian 16-bit word.
    let b2 = header.get(2).copied().unwrap_or(0);
    let b3 = header.get(3).copied().unwrap_or(0);
    let flags = u16::from_be_bytes([b2, b3]);

    // Channel assignment nibble → channel count + LFE.
    match (flags & 0xF000).wrapping_shr(12) {
        1 => {
            // 1/0/0
            stream.channel_count = 1;
            stream.lfe = 0;
        }
        3 => {
            // 2/0/0
            stream.channel_count = 2;
            stream.lfe = 0;
        }
        4 => {
            // 3/0/0
            stream.channel_count = 3;
            stream.lfe = 0;
        }
        5 => {
            // 2/1/0
            stream.channel_count = 3;
            stream.lfe = 0;
        }
        6 => {
            // 3/1/0
            stream.channel_count = 4;
            stream.lfe = 0;
        }
        7 => {
            // 2/2/0
            stream.channel_count = 4;
            stream.lfe = 0;
        }
        8 => {
            // 3/2/0
            stream.channel_count = 5;
            stream.lfe = 0;
        }
        9 => {
            // 3/2/1
            stream.channel_count = 5;
            stream.lfe = 1;
        }
        10 => {
            // 3/4/0
            stream.channel_count = 7;
            stream.lfe = 0;
        }
        11 => {
            // 3/4/1
            stream.channel_count = 7;
            stream.lfe = 1;
        }
        _ => {
            stream.channel_count = 0;
            stream.lfe = 0;
        }
    }

    // Bit-depth code.
    match (flags & 0xC0).wrapping_shr(6) {
        1 => stream.bit_depth = 16,
        2 => stream.bit_depth = 20,
        3 => stream.bit_depth = 24,
        _ => stream.bit_depth = 0,
    }

    // Sample-rate nibble.
    match (flags & 0xF00).wrapping_shr(8) {
        1 => stream.sample_rate = 48_000,
        4 => stream.sample_rate = 96_000,
        5 => stream.sample_rate = 192_000,
        _ => stream.sample_rate = 0,
    }

    // `sample_rate * bit_depth * (channel_count + lfe)` — a wrapping product,
    // truncated through the unsigned 32-bit view, then widened to the i64 bit rate.
    let product = stream
        .sample_rate
        .wrapping_mul(stream.bit_depth)
        .wrapping_mul(stream.channel_count.wrapping_add(stream.lfe));
    stream.base.bit_rate = i64::from(product.cast_unsigned());

    stream.base.is_vbr = false;
    stream.base.is_initialized = true;
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

    /// Builds a ≥5-byte LPCM access unit whose `flags` word packs the channel,
    /// sample-rate, and bit-depth fields (the third/fourth header bytes). The
    /// trailing byte keeps the buffer longer than four bytes so `read_bytes(4)`
    /// succeeds (the bit reader's `end >= len` rule rejects an exact 4-byte read).
    fn lpcm(channel: u16, rate: u16, depth: u16) -> Vec<u8> {
        let flags = channel.wrapping_shl(12) | rate.wrapping_shl(8) | depth.wrapping_shl(6);
        let [b2, b3] = flags.to_be_bytes();
        vec![0x00, 0x00, b2, b3, 0x00]
    }

    /// Runs [`scan`] over a packed LPCM header and returns the mutated stream.
    fn run(channel: u16, rate: u16, depth: u16) -> TsAudioStream {
        let mut s = TsAudioStream::default();
        s.base.stream_type = TsStreamType::LpcmAudio;
        let mut b = buf(&lpcm(channel, rate, depth));
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        assert_eq!(tag, None, "LPCM never sets the tag");
        s
    }

    #[test]
    fn channel_assignment_maps_count_and_lfe() {
        // (channel nibble, expected channel count, expected LFE) — every switch arm
        // plus a sampling of the default (0/2/12..15 → 0 channels, no LFE).
        let cases = [
            (0_u16, 0_i32, 0_i32),
            (1, 1, 0),
            (2, 0, 0),
            (3, 2, 0),
            (4, 3, 0),
            (5, 3, 0),
            (6, 4, 0),
            (7, 4, 0),
            (8, 5, 0),
            (9, 5, 1),
            (10, 7, 0),
            (11, 7, 1),
            (12, 0, 0),
            (15, 0, 0),
        ];
        for (channel, count, lfe) in cases {
            let s = run(channel, 1, 1);
            assert_eq!((s.channel_count, s.lfe), (count, lfe), "channel nibble {channel}");
        }
    }

    #[test]
    fn bit_depth_code_maps_depth() {
        assert_eq!(run(3, 1, 0).bit_depth, 0);
        assert_eq!(run(3, 1, 1).bit_depth, 16);
        assert_eq!(run(3, 1, 2).bit_depth, 20);
        assert_eq!(run(3, 1, 3).bit_depth, 24);
    }

    #[test]
    fn sample_rate_nibble_maps_rate() {
        assert_eq!(run(3, 0, 1).sample_rate, 0);
        assert_eq!(run(3, 1, 1).sample_rate, 48_000);
        assert_eq!(run(3, 2, 1).sample_rate, 0);
        assert_eq!(run(3, 4, 1).sample_rate, 96_000);
        assert_eq!(run(3, 5, 1).sample_rate, 192_000);
        assert_eq!(run(3, 6, 1).sample_rate, 0);
    }

    #[test]
    fn matches_the_expected_lpcm_stream_line() {
        // 2/0/0, 48 kHz, 16-bit — a real LPCM disc sample (PID 4352).
        let s = run(3, 1, 1);
        assert_eq!(s.channel_count, 2);
        assert_eq!(s.lfe, 0);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.bit_depth, 16);
        // bit_rate = 48000 * 16 * (2 + 0) = 1_536_000.
        assert_eq!(s.base.bit_rate, 1_536_000);
        assert!(!s.base.is_vbr);
        assert!(s.base.is_initialized);
        assert_eq!(s.codec_short_name(), "LPCM");
        assert_eq!(s.codec_name(), "LPCM Audio");
        assert_eq!(s.description(), "2.0 / 48 kHz /  1536 kbps / 16-bit");
    }

    #[test]
    fn bit_rate_folds_lfe_into_the_channel_count() {
        // 3/2/1 (5 channels + LFE), 48 kHz, 24-bit → 48000 * 24 * (5 + 1).
        let s = run(9, 1, 3);
        assert_eq!((s.channel_count, s.lfe, s.sample_rate, s.bit_depth), (5, 1, 48_000, 24));
        assert_eq!(s.base.bit_rate, 6_912_000);
        // A zero sample rate (reserved nibble) collapses the product to 0.
        assert_eq!(run(9, 0, 3).base.bit_rate, 0);
    }

    #[test]
    fn short_buffer_and_initialized_stream_are_left_untouched() {
        // Exactly four bytes is too short for `read_bytes(4)` (the `end >= len`
        // rule) → the stream is untouched.
        let mut s = TsAudioStream::default();
        s.base.stream_type = TsStreamType::LpcmAudio;
        let mut b = buf(&[0x00, 0x00, 0x31, 0x40]);
        scan(&mut s, &mut b, &mut None);
        assert!(!s.base.is_initialized);
        assert_eq!(s.channel_count, 0);

        // An already-initialized stream returns immediately without parsing.
        let mut s = TsAudioStream::default();
        s.base.stream_type = TsStreamType::LpcmAudio;
        s.base.is_initialized = true;
        let mut b = buf(&lpcm(9, 5, 3));
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.channel_count, 0); // never parsed
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>()) {
            let mut s = TsAudioStream::default();
            s.base.stream_type = TsStreamType::LpcmAudio;
            let mut b = buf(&data);
            let mut tag = None;
            scan(&mut s, &mut b, &mut tag);
        }
    }
}
