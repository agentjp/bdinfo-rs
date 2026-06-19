//! MPEG-1/2 audio (MPA, Layers I–III) codec scanner.
//!
//! [`scan`] reads the MPEG-audio frame
//! header (after the 11-bit `0x7FF` sync word) and fills the [`TsAudioStream`]
//! codec fields: bit rate, sample rate, channel count, audio mode, and the
//! `ext_data` string (`"<version> <layer>"`, e.g. `"MPEG 1 Layer III"`) shown
//! as the report's long codec name.
//!
//! The bit rate, sample rate, mode, version, and layer are direct table lookups
//! keyed by the 2-bit version / 2-bit layer / 4-bit bit-rate / 2-bit sample-rate /
//! 2-bit channel-mode fields — every index is within its table's bounds for the
//! field width, so there are no guards (the `.get()` chains stay bounds-safe). The
//! `* 1000` bit-rate scale uses `wrapping_mul`, though the table maxima keep it
//! far below overflow.

use crate::bitstream::TsStreamBuffer;
use crate::stream::{TsAudioMode, TsAudioStream};

/// Bit rate in kbps per `[audioVersionID][layerIndex][bitrateIndex]`.
/// The version axis is 2.5 / reserved / 2 / 1; the layer axis is reserved / III /
/// II / I; index `0` (free) and `15` (reserved) are `0`.
const MPA_BITRATE: [[[i32; 16]; 4]; 4] = [
    // MPEG Version 2.5
    [
        [0; 16],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0],
    ],
    // reserved
    [[0; 16], [0; 16], [0; 16], [0; 16]],
    // MPEG Version 2
    [
        [0; 16],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0],
    ],
    // MPEG Version 1
    [
        [0; 16],
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0],
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0],
    ],
];

/// Sample rate in Hz per `[audioVersionID][samplingRateIndex]`.
const MPA_SAMPLE_RATE: [[i32; 4]; 4] = [
    [11_025, 12_000, 8_000, 0],  // MPEG Version 2.5
    [0, 0, 0, 0],                // reserved
    [22_050, 24_000, 16_000, 0], // MPEG Version 2
    [44_100, 48_000, 32_000, 0], // MPEG Version 1
];

/// Audio mode per the 2-bit `channelMode`.
const MPA_CHANNEL_MODES: [TsAudioMode; 4] =
    [TsAudioMode::Stereo, TsAudioMode::JointStereo, TsAudioMode::DualMono, TsAudioMode::Mono];

/// Version label per the 2-bit `audioVersionID`.
const MPA_VERSION: [&str; 4] = ["MPEG 2.5", "Unknown MPEG", "MPEG 2", "MPEG 1"];

/// Layer label per the 2-bit `layerIndex`.
const MPA_LAYER: [&str; 4] = ["Unknown Layer", "Layer III", "Layer II", "Layer I"];

/// Channel count per the 2-bit `channelMode`.
const MPA_CHANNELS: [i32; 4] = [2, 2, 2, 1];

/// Scans one MPEG-audio access unit from `buffer` into `stream`.
///
/// A non-`0x7FF` sync word leaves `stream` untouched (an early return). `tag`
/// is part of the shared codec-scan signature and is never set here.
pub fn scan(stream: &mut TsAudioStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    // `tag` is part of the shared codec-scan signature; MPA never sets it (a
    // `pub fn` is exempt from `needless_pass_by_ref_mut`).
    let _ = tag;
    if stream.base.is_initialized {
        return;
    }

    // The 11-bit sync is compared after a 5-bit left shift (as a 16-bit pattern).
    let sync_word = u32::from(buffer.read_bits2(11, false)).wrapping_shl(5);
    if sync_word != 0b1111_1111_1110_0000 {
        return;
    }

    let audio_version_id = usize::from(buffer.read_bits2(2, false));
    let layer_index = usize::from(buffer.read_bits2(2, false));
    let _ = buffer.read_bool(false); // Protected Bit
    let bitrate_index = usize::from(buffer.read_bits2(4, false));
    let sampling_rate_index = usize::from(buffer.read_bits2(2, false));
    let _ = buffer.read_bool(false); // Padding
    let _ = buffer.read_bool(false); // Private Bit
    let channel_mode = usize::from(buffer.read_bits2(2, false));
    let _ = buffer.read_bits2(2, false); // Mode Extension
    let _ = buffer.read_bool(false); // Copyright Bit
    let _ = buffer.read_bool(false); // Original Bit
    let _ = buffer.read_bits2(2, false); // Emphasis

    let bitrate_kbps = MPA_BITRATE
        .get(audio_version_id)
        .and_then(|layers| layers.get(layer_index))
        .and_then(|rates| rates.get(bitrate_index))
        .copied()
        .unwrap_or(0);
    stream.base.bit_rate = i64::from(bitrate_kbps.wrapping_mul(1000));

    stream.sample_rate = MPA_SAMPLE_RATE
        .get(audio_version_id)
        .and_then(|rates| rates.get(sampling_rate_index))
        .copied()
        .unwrap_or(0);

    stream.audio_mode =
        MPA_CHANNEL_MODES.get(channel_mode).copied().unwrap_or(TsAudioMode::Unknown);
    stream.channel_count = MPA_CHANNELS.get(channel_mode).copied().unwrap_or(0);
    stream.lfe = 0;

    let version = MPA_VERSION.get(audio_version_id).copied().unwrap_or("");
    let layer = MPA_LAYER.get(layer_index).copied().unwrap_or("");
    stream.ext_data = Some(format!("{version} {layer}"));

    stream.base.is_vbr = false;
    stream.base.is_initialized = true;
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

    /// The MPEG-audio frame header field sequence (sync, then version/layer/…/
    /// emphasis), padded.
    fn mpa_frame(version: u64, layer: u64, bitrate: u64, sr: u64, chan: u64) -> Vec<u8> {
        pack(&[
            (0x7FF, 11),  // sync word (<< 5 == 0xFFE0)
            (version, 2), // audioVersionID
            (layer, 2),   // layerIndex
            (0, 1),       // protected bit
            (bitrate, 4), // bitrateIndex
            (sr, 2),      // samplingRateIndex
            (0, 1),       // padding
            (0, 1),       // private bit
            (chan, 2),    // channelMode
            (0, 2),       // mode extension
            (0, 1),       // copyright
            (0, 1),       // original
            (0, 2),       // emphasis
            (0, 13),      // padding tail (non-byte-aligned: exercises the partial byte)
        ])
    }

    /// Runs [`scan`] over an MPA frame and returns the mutated stream.
    fn run(
        stream_type: TsStreamType,
        version: u64,
        layer: u64,
        bitrate: u64,
        sr: u64,
        chan: u64,
    ) -> TsAudioStream {
        let mut s = TsAudioStream::default();
        s.base.stream_type = stream_type;
        let mut b = buf(&mpa_frame(version, layer, bitrate, sr, chan));
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        assert_eq!(tag, None, "MPA never sets the tag");
        s
    }

    #[test]
    fn decodes_mpeg1_layer3_stereo() {
        // version 3 (MPEG 1), layer 1 (Layer III), bitrate index 9 (128 kbps),
        // sample index 1 (48 kHz), channel mode 0 (Stereo).
        let s = run(TsStreamType::Mpeg1Audio, 3, 1, 9, 1, 0);
        assert_eq!(s.base.bit_rate, 128_000);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.channel_count, 2);
        assert_eq!(s.lfe, 0);
        assert_eq!(s.audio_mode, TsAudioMode::Stereo);
        assert_eq!(s.ext_data.as_deref(), Some("MPEG 1 Layer III"));
        assert!(!s.base.is_vbr);
        assert!(s.base.is_initialized);
        assert_eq!(s.codec_short_name(), "MP1");
        assert_eq!(s.codec_name(), "MPEG 1 Layer III");
        assert_eq!(s.description(), "2.0 / 48 kHz /   128 kbps");
    }

    #[test]
    fn version_and_layer_select_rates_and_labels() {
        // MPEG 2.5, Layer I, bitrate index 1 (32 kbps), sample index 0 (11025 Hz).
        let s = run(TsStreamType::Mpeg2Audio, 0, 3, 1, 0, 3);
        assert_eq!(s.base.bit_rate, 32_000);
        assert_eq!(s.sample_rate, 11_025);
        assert_eq!(s.ext_data.as_deref(), Some("MPEG 2.5 Layer I"));
        assert_eq!(s.codec_short_name(), "MP2");
        // MPEG 2, Layer II, bitrate index 14 (160 kbps for V2 LII), sample 2 (16 kHz).
        let s = run(TsStreamType::Mpeg2Audio, 2, 2, 14, 2, 0);
        assert_eq!(s.base.bit_rate, 160_000);
        assert_eq!(s.sample_rate, 16_000);
        assert_eq!(s.ext_data.as_deref(), Some("MPEG 2 Layer II"));
        // Reserved version 1 → zero rate / labels "Unknown MPEG Unknown Layer".
        let s = run(TsStreamType::Mpeg1Audio, 1, 0, 5, 1, 0);
        assert_eq!(s.base.bit_rate, 0);
        assert_eq!(s.sample_rate, 0);
        assert_eq!(s.ext_data.as_deref(), Some("Unknown MPEG Unknown Layer"));
    }

    #[test]
    fn channel_mode_maps_count_mode_and_description_tags() {
        // (channel mode, expected count, expected mode, expected desc tail).
        let cases = [
            (0_u64, 2_i32, TsAudioMode::Stereo, "2.0 / 48 kHz /   128 kbps"),
            (1, 2, TsAudioMode::JointStereo, "2.0 / 48 kHz /   128 kbps / Joint Stereo"),
            (2, 2, TsAudioMode::DualMono, "2.0 / 48 kHz /   128 kbps / Dual Mono"),
            (3, 1, TsAudioMode::Mono, "1.0 / 48 kHz /   128 kbps"),
        ];
        for (chan, count, mode, desc) in cases {
            let s = run(TsStreamType::Mpeg1Audio, 3, 1, 9, 1, chan);
            assert_eq!(s.channel_count, count, "channel mode {chan}");
            assert_eq!(s.lfe, 0, "channel mode {chan}");
            assert_eq!(s.audio_mode, mode, "channel mode {chan}");
            assert_eq!(s.description(), desc, "channel mode {chan}");
        }
    }

    #[test]
    fn rejects_bad_sync_and_initialized_streams() {
        // Wrong sync word (top 11 bits not all set) → untouched.
        let mut s = TsAudioStream::default();
        s.base.stream_type = TsStreamType::Mpeg1Audio;
        let mut b = buf(&pack(&[(0x7FE, 11), (0, 21)])); // 0x7FE << 5 ≠ 0xFFE0
        scan(&mut s, &mut b, &mut None);
        assert!(!s.base.is_initialized);
        assert_eq!(s.ext_data, None);

        // An already-initialized stream returns immediately without parsing.
        let mut s = TsAudioStream::default();
        s.base.stream_type = TsStreamType::Mpeg1Audio;
        s.base.is_initialized = true;
        let mut b = buf(&mpa_frame(3, 1, 9, 1, 0));
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.channel_count, 0); // never parsed
        assert_eq!(s.ext_data, None);
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>()) {
            for ty in [TsStreamType::Mpeg1Audio, TsStreamType::Mpeg2Audio] {
                let mut s = TsAudioStream::default();
                s.base.stream_type = ty;
                let mut b = buf(&data);
                let mut tag = None;
                scan(&mut s, &mut b, &mut tag);
            }
        }
    }
}
