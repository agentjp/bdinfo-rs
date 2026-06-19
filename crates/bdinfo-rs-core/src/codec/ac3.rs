//! Dolby Digital (AC-3) and Dolby Digital Plus (E-AC-3) codec scanner.
//!
//! [`scan`] reads one assembled audio access unit and fills the
//! [`TsAudioStream`] codec fields: sample rate, channel count + LFE, bit rate,
//! audio mode (stereo / dual-mono / Dolby Surround / AC3-EX), dialogue
//! normalization, and — for E-AC-3 with a JOC payload — the Atmos `has_extensions`
//! flag. The two top-level branches split on `bsid <= 10`: the legacy
//! AC-3 bit-stream-information header, and the E-AC-3 header (frame size, an
//! optional dependent-substream core via [`TsAudioStream::ts_clone`], and the EMD
//! framework scan that detects Atmos).
//!
//! All fixed-width codec math uses `wrapping_*` so hostile field values cannot
//! overflow; the `f64`→`i64` bit-rate conversion truncates toward zero. A buffer
//! too short for a header read returns early and leaves the stream untouched.
//! The two `dheadphonmod` bits are read purely for framing (nothing acts on
//! them), and inside the first-payload block the EMDF payload id is always
//! `1..=15`, so the `0x1F` escape handling lives only in the payload loop.

use crate::bitstream::{SeekOrigin, TsStreamBuffer};
use crate::stream::{TsAudioMode, TsAudioStream, TsStreamType};

/// AC-3 nominal bit rates in kbps, indexed by `frmsizecod >> 1`.
const AC3_BITRATE: [i32; 19] =
    [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];

/// Channel count per `acmod` audio-coding-mode code.
const AC3_CHANNELS: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// Counts the extra channels signalled by an E-AC-3 dependent-substream channel
/// map.
///
/// Each set bit (MSB-first) whose position is one of the side/rear/top object
/// groups (5, 6, 9, 10, 11) contributes 2.
#[must_use]
pub fn ac3_chan_map(chan_map: u32) -> u8 {
    let mut channels: u8 = 0;
    // Walk the 16-bit channel map MSB-first.
    for i in 0..16_u8 {
        let bit = 1_u32.wrapping_shl(15_u32.wrapping_sub(u32::from(i)));
        if (chan_map & bit) != 0 && matches!(i, 5 | 6 | 9 | 10 | 11) {
            channels = channels.wrapping_add(2);
        }
    }
    channels
}

/// Truncates a `f64` bit-rate toward zero into an `i64`.
///
/// Saturating on the unreachable non-finite case (e.g. a malformed zero
/// block-count divisor).
#[expect(
    clippy::cast_possible_truncation,
    clippy::as_conversions,
    reason = "deliberate truncate-toward-zero float→int; E-AC-3 bit rates fit i64, saturating on the non-finite case (TryFrom inapplicable)"
)]
const fn to_i64_trunc(x: f64) -> i64 {
    x as i64
}

/// Scans one AC-3 / E-AC-3 access unit from `buffer` into `stream`.
///
/// A non-AC-3 sync word, or a buffer too short to hold the header, leaves `stream`
/// untouched (early returns). `tag` is part of the shared
/// codec-scan signature; this codec never sets it.
#[expect(
    clippy::too_many_lines,
    reason = "one linear bit-stream-information parse; splitting it would obscure the header layout"
)]
pub fn scan(stream: &mut TsAudioStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    // `tag` is part of the shared codec-scan signature; AC-3 never sets it (and a
    // `pub fn` is exempt from `needless_pass_by_ref_mut`).
    let _ = tag;
    if stream.base.is_initialized {
        return;
    }

    let Some(sync) = buffer.read_bytes(2) else {
        return;
    };
    if sync.first().copied() != Some(0x0B) || sync.get(1).copied() != Some(0x77) {
        return;
    }

    let second_frame = stream.channel_count > 0;

    let mut frame_size: u32 = 0;
    let mut frame_size_code: u32 = 0;
    let mut dial_norm: u32 = 0;
    let mut dial_norm_ext: u32 = 0;
    let mut num_blocks: u32 = 0;
    let mut sr_code: u32;
    let channel_mode: u32;
    let mut lfe_on: u32;
    let mut half_rate = false;

    let Some(hdr) = buffer.read_bytes(4) else {
        return;
    };
    let mut bsid = (u32::from(hdr.get(3).copied().unwrap_or(0)) & 0xF8).wrapping_shr(3);
    buffer.seek(-4, SeekOrigin::Current);
    if bsid <= 10 {
        buffer.bs_skip_bytes(2, false);
        sr_code = u32::from(buffer.read_bits2(2, false));
        frame_size_code = u32::from(buffer.read_bits2(6, false));
        bsid = u32::from(buffer.read_bits2(5, false));
        buffer.bs_skip_bits(3, false);

        channel_mode = u32::from(buffer.read_bits2(3, false));
        if (channel_mode & 0x1) > 0 && channel_mode != 0x1 {
            buffer.bs_skip_bits(2, false);
        }
        if (channel_mode & 0x4) > 0 {
            buffer.bs_skip_bits(2, false);
        }
        if channel_mode == 0x2 {
            let dsurmod = buffer.read_bits2(2, false);
            if dsurmod == 0x2 {
                stream.audio_mode = TsAudioMode::Surround;
            }
        }
        lfe_on = u32::from(buffer.read_bits2(1, false));
        dial_norm = u32::from(buffer.read_bits2(5, false));
        if buffer.read_bool(false) {
            buffer.bs_skip_bits(8, false);
        }
        if buffer.read_bool(false) {
            buffer.bs_skip_bits(8, false);
        }
        if buffer.read_bool(false) {
            buffer.bs_skip_bits(7, false);
        }
        if channel_mode == 0 {
            buffer.bs_skip_bits(5, false);
            if buffer.read_bool(false) {
                buffer.bs_skip_bits(8, false);
            }
            if buffer.read_bool(false) {
                buffer.bs_skip_bits(8, false);
            }
            if buffer.read_bool(false) {
                buffer.bs_skip_bits(7, false);
            }
        }
        buffer.bs_skip_bits(2, false);
        if bsid == 6 {
            if buffer.read_bool(false) {
                buffer.bs_skip_bits(14, false);
            }
            if buffer.read_bool(false) {
                let dsurexmod = buffer.read_bits2(2, false);
                // dheadphonmod: two bits read purely for framing; nothing acts
                // on the value.
                let _ = buffer.read_bits2(2, false);
                buffer.bs_skip_bits(10, false);
                if dsurexmod == 2 {
                    stream.audio_mode = TsAudioMode::Extended;
                }
            }
        }
    } else {
        let frame_type = buffer.read_bits2(2, false);
        buffer.bs_skip_bits(3, false);

        frame_size = buffer.read_bits4(11, false).wrapping_add(1).wrapping_shl(1);

        sr_code = u32::from(buffer.read_bits2(2, false));
        if sr_code == 3 {
            // fscod == 3: the 2-bit `fscod2` field selects a reduced sample rate
            // that is exactly half the matching `fscod` rate, and `numblkscod` is
            // absent — a reduced-rate frame always carries 6 audio blocks.
            sr_code = u32::from(buffer.read_bits2(2, false));
            num_blocks = 6;
            half_rate = true;
        } else {
            num_blocks = u32::from(buffer.read_bits2(2, false));
        }
        channel_mode = u32::from(buffer.read_bits2(3, false));
        lfe_on = u32::from(buffer.read_bits2(1, false));
        bsid = u32::from(buffer.read_bits2(5, false));
        dial_norm_ext = u32::from(buffer.read_bits2(5, false));

        if buffer.read_bool(false) {
            buffer.bs_skip_bits(8, false);
        }
        if channel_mode == 0 {
            buffer.bs_skip_bits(5, false);
            if buffer.read_bool(false) {
                buffer.bs_skip_bits(8, false);
            }
        }
        if frame_type == 1 {
            let mut core = stream.ts_clone();
            core.base.stream_type = TsStreamType::Ac3Audio;
            stream.core_stream = Some(Box::new(core));

            if buffer.read_bool(false) {
                let chanmap = buffer.read_bits4(16, false);
                let (core_count, core_lfe) =
                    stream.core_stream.as_ref().map_or((0, 0), |c| (c.channel_count, c.lfe));
                stream.channel_count = core_count.wrapping_add(i32::from(ac3_chan_map(chanmap)));
                lfe_on = u32::try_from(core_lfe).unwrap_or(0);
            }
        }

        let mut emdf_found = false;
        loop {
            let emdf_sync = buffer.read_bits4(16, false);
            if emdf_sync == 0x5838 {
                emdf_found = true;
                break;
            }
            buffer.seek(-2, SeekOrigin::Current);
            buffer.bs_skip_bits(1, false);
            if buffer.position() >= buffer.length() {
                break;
            }
        }

        if emdf_found {
            let emdf_container_size = buffer.read_bits4(16, false);
            let remain_after_emdf = buffer
                .data_bit_stream_remain()
                .wrapping_sub(i64::from(emdf_container_size).wrapping_mul(8));

            let mut emdf_version = u32::from(buffer.read_bits2(2, false));
            if emdf_version == 3 {
                emdf_version = emdf_version.wrapping_add(u32::from(buffer.read_bits2(2, false)));
            }

            if emdf_version > 0 {
                let skip = buffer.data_bit_stream_remain().wrapping_sub(remain_after_emdf);
                buffer.bs_skip_bits(usize::try_from(skip).unwrap_or(0), false);
            } else {
                let temp = buffer.read_bits2(3, false);
                if temp == 0x7 {
                    buffer.bs_skip_bits(2, false);
                }

                let mut emdf_payload_id = buffer.read_bits2(5, false);
                if emdf_payload_id > 0 && emdf_payload_id < 16 {
                    // (the id here is 1..=15, so the 0x1F escape code can never
                    // appear in this block.)
                    emdf_payload_config(buffer);
                    let payload_size = usize::from(buffer.read_bits2(8, false)).wrapping_mul(8);
                    buffer.bs_skip_bits(payload_size.wrapping_add(1), false);
                }

                loop {
                    emdf_payload_id = buffer.read_bits2(5, false);
                    if emdf_payload_id == 14 || buffer.position() >= buffer.length() {
                        break;
                    }
                    if emdf_payload_id == 0x1F {
                        buffer.bs_skip_bits(5, false);
                    }
                    emdf_payload_config(buffer);
                    let payload_size = usize::from(buffer.read_bits2(8, false)).wrapping_mul(8);
                    let _ = buffer.read_bits4(payload_size.wrapping_add(1), false);
                }

                // No end-of-buffer guard needed: past the content the JOC reads
                // zero-fill, so `joc_num_objects_bits` is 0 and nothing is set —
                // identical to skipping the block.
                if emdf_payload_id == 14 {
                    emdf_payload_config(buffer);
                    buffer.bs_skip_bits(12, false);
                    let joc_num_objects_bits = buffer.read_bits2(6, false);
                    if joc_num_objects_bits > 0 {
                        stream.has_extensions = true;
                    }
                }
            }
        }
    }

    // A 3-bit `acmod` always fits the 8-entry channel table, so the only live
    // guard is channel-count-not-yet-set; `.get()` keeps the index bounds-safe.
    if stream.channel_count == 0 {
        let idx = usize::try_from(channel_mode).unwrap_or(usize::MAX);
        stream.channel_count = i32::from(AC3_CHANNELS.get(idx).copied().unwrap_or(0));
    }

    if stream.audio_mode == TsAudioMode::Unknown {
        match channel_mode {
            0 => stream.audio_mode = TsAudioMode::DualMono,
            2 => stream.audio_mode = TsAudioMode::Stereo,
            // Any other mode leaves the field at Unknown (its current value).
            _ => {}
        }
    }

    stream.sample_rate = match sr_code {
        0 => 48_000,
        1 => 44_100,
        2 => 32_000,
        _ => 0,
    };
    if half_rate {
        // Reduced-rate E-AC-3 (`fscod2`): the decoded rate is half the table value
        // (24 / 22.05 / 16 kHz), which also feeds the derived bit-rate below.
        stream.sample_rate = stream.sample_rate.wrapping_div(2);
    }

    if bsid <= 10 {
        // Legacy AC-3 `bsid` 9 / 10 are the half / quarter "low-sample-rate"
        // variants: `sr_shift = max(bsid, 8) - 8` right-shifts both the sample rate
        // and the table bit rate (ATSC A/52 / ETSI TS 102 366 §5.4.1; FFmpeg
        // `ac3_parser.c`). Conforming Blu-ray AC-3 is always `bsid 8` (shift 0).
        let sr_shift = bsid.max(8).wrapping_sub(8);
        stream.sample_rate = stream.sample_rate.wrapping_shr(sr_shift);
        // `frmsizecod >> 1` indexes the 19-entry bit-rate table; `.get()` is the
        // range guard (an out-of-range code leaves the bit rate unset).
        let idx = usize::try_from(frame_size_code.wrapping_shr(1)).unwrap_or(usize::MAX);
        if let Some(&br) = AC3_BITRATE.get(idx) {
            stream.base.bit_rate = i64::from(br.wrapping_mul(1000)).wrapping_shr(sr_shift);
        }
    } else {
        stream.base.bit_rate = to_i64_trunc(
            4.0 * f64::from(frame_size) * f64::from(stream.sample_rate)
                / f64::from(num_blocks.wrapping_mul(256)),
        );
        if let Some(core_bit_rate) = stream.core_stream.as_ref().map(|c| c.base.bit_rate) {
            stream.base.bit_rate = stream.base.bit_rate.wrapping_add(core_bit_rate);
        }
    }

    stream.lfe = i32::try_from(lfe_on).unwrap_or(0);
    if stream.base.stream_type != TsStreamType::Ac3PlusSecondaryAudio {
        if (stream.base.stream_type == TsStreamType::Ac3PlusAudio && bsid == 6)
            || stream.base.stream_type == TsStreamType::Ac3Audio
        {
            stream.dial_norm = i32::try_from(dial_norm).unwrap_or(0).wrapping_mul(-1);
        } else if stream.base.stream_type == TsStreamType::Ac3PlusAudio && second_frame {
            stream.dial_norm = i32::try_from(dial_norm_ext).unwrap_or(0).wrapping_mul(-1);
        }
    }
    stream.base.is_vbr = false;
    stream.base.is_initialized =
        !(stream.base.stream_type == TsStreamType::Ac3PlusAudio && bsid == 6 && !second_frame);
}

/// Skips one `emdf_payload_config()` structure (the EMD-framework payload
/// descriptor whose fields the AC-3 scanner only needs to step over).
fn emdf_payload_config(buffer: &mut TsStreamBuffer) {
    let sample_offset_e = buffer.read_bool(false);
    if sample_offset_e {
        buffer.bs_skip_bits(12, false);
    }
    if buffer.read_bool(false) {
        buffer.bs_skip_bits(11, false);
    }
    if buffer.read_bool(false) {
        buffer.bs_skip_bits(2, false);
    }
    if buffer.read_bool(false) {
        buffer.bs_skip_bits(8, false);
    }
    if !buffer.read_bool(false) {
        buffer.bs_skip_bits(1, false);
        if !sample_offset_e && buffer.read_bool(false) {
            buffer.bs_skip_bits(9, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, proptest};

    use super::{ac3_chan_map, scan};
    use crate::bitstream::TsStreamBuffer;
    use crate::stream::{TsAudioMode, TsAudioStream, TsStreamType};

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

    /// Packs `(value, bit_width)` fields MSB-first into bytes; a trailing partial
    /// byte is left-aligned (low bits zero), matching how the bit reader consumes
    /// them. Lets each test spell its AC-3 frame out field-by-field.
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

    /// Runs [`scan`] over a packed frame and returns the mutated stream.
    fn run(stream_type: TsStreamType, fields: &[(u64, u32)]) -> TsAudioStream {
        let mut s = stream(stream_type);
        let mut b = buf(&pack(fields));
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        assert_eq!(tag, None, "AC-3 never sets the tag");
        s
    }

    /// The legacy-AC-3 (`bsid <= 10`) field prefix up to `acmod`: sync, CRC,
    /// `fscod`, `frmsizecod`, `bsid`, `bsmod`. The caller appends the channel
    /// fields. (`fscod` 0 = 48 kHz, `frmsizecod` 36 → 640 kbps.)
    fn legacy_prefix(fscod: u64, frmsizecod: u64, bsid: u64) -> Vec<(u64, u32)> {
        vec![(0x0B77, 16), (0, 16), (fscod, 2), (frmsizecod, 6), (bsid, 5), (0, 3)]
    }

    #[test]
    fn legacy_ac3_5_1_core_fields() {
        // acmod 7 (5.1): cmixlev + surmixlev present, LFE on, dialnorm 31 (DN -31).
        let mut f = legacy_prefix(0, 36, 8);
        f.extend_from_slice(&[
            (7, 3),  // acmod = 3/2 (5.1)
            (0, 2),  // cmixlev (acmod & 1, acmod != 1)
            (0, 2),  // surmixlev (acmod & 4)
            (1, 1),  // lfeon
            (31, 5), // dialnorm
            (0, 1),
            (0, 1),
            (0, 1),  // compre / langcode / audprodie absent
            (0, 2),  // copyright + orig
            (0, 16), // tail padding
        ]);
        let s = run(TsStreamType::Ac3Audio, &f);
        assert_eq!(s.channel_count, 5);
        assert_eq!(s.lfe, 1);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.base.bit_rate, 640_000);
        assert_eq!(s.dial_norm, -31);
        assert_eq!(s.audio_mode, TsAudioMode::Unknown);
        assert!(s.base.is_initialized);
        assert!(!s.base.is_vbr);
        // The headline strings (the AC-3 core line of a real TrueHD+AC-3 disc).
        assert_eq!(s.description(), "5.1 / 48 kHz /   640 kbps / DN -31dB");
        assert_eq!(s.codec_short_name(), "AC3");
        assert_eq!(s.codec_name(), "Dolby Digital Audio");
    }

    #[test]
    fn legacy_stereo_dolby_surround_and_plain() {
        // acmod 2 (2/0): a dsurmod of 2 means Dolby Surround.
        let mut surround = legacy_prefix(0, 36, 8);
        surround.extend_from_slice(&[
            (2, 3), // acmod = 2/0
            (2, 2), // dsurmod = 2 → Surround
            (0, 1), // lfeon
            (0, 5), // dialnorm
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 2),
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3Audio, &surround);
        assert_eq!(s.channel_count, 2);
        assert_eq!(s.audio_mode, TsAudioMode::Surround);

        // dsurmod != 2 → the audio-mode default match picks Stereo for acmod 2.
        let mut stereo = legacy_prefix(0, 36, 8);
        stereo.extend_from_slice(&[
            (2, 3),
            (0, 2),
            (0, 1),
            (0, 5),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 2),
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3Audio, &stereo);
        assert_eq!(s.audio_mode, TsAudioMode::Stereo);
    }

    #[test]
    fn legacy_dual_mono_runs_the_1plus1_block() {
        // acmod 0 (1+1): the channel_mode == 0 sub-block (skip 5 + three optional
        // fields), and the audio-mode default match picks DualMono.
        let mut f = legacy_prefix(0, 36, 8);
        f.extend_from_slice(&[
            (0, 3), // acmod = 1+1 (no cmixlev/surmixlev/dsurmod)
            (0, 1), // lfeon
            (0, 5), // dialnorm
            (0, 1),
            (0, 1),
            (0, 1), // first triple of optional bools
            (0, 5), // channel_mode==0: skipped 5
            (0, 1),
            (0, 1),
            (0, 1), // second triple of optional bools
            (0, 2),
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3Audio, &f);
        assert_eq!(s.channel_count, 2); // AC3_CHANNELS[0]
        assert_eq!(s.audio_mode, TsAudioMode::DualMono);

        // The same 1+1 block with the second triple of optional fields present →
        // each true arm skips (8 / 8 / 7).
        let mut f = legacy_prefix(0, 36, 8);
        f.extend_from_slice(&[
            (0, 3), // acmod 1+1
            (0, 1),
            (0, 5),
            (0, 1),
            (0, 1),
            (0, 1), // first triple absent
            (0, 5), // channel_mode==0 skip 5
            (1, 1),
            (0, 8), // skip 8
            (1, 1),
            (0, 8), // skip 8
            (1, 1),
            (0, 7), // skip 7
            (0, 2),
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3Audio, &f);
        assert_eq!(s.channel_count, 2);
    }

    #[test]
    fn eac3_compr_field_present_is_skipped() {
        // The E-AC-3 `compr` flag present → the 8-bit field is skipped.
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2),
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2),
            (2, 3),
            (0, 1),
            (16, 5),
            (0, 5),
            (1, 1),
            (0, 8), // compr present → skip 8
        ];
        f.push((0, 256));
        let s = run(TsStreamType::Ac3PlusAudio, &f);
        assert_eq!(s.channel_count, 2);
    }

    #[test]
    fn legacy_mono_takes_no_mix_skips() {
        // acmod 1 (1/0): channel_mode & 1 is set but == 1, so no cmixlev skip; the
        // AudioMode default switch leaves it Unknown.
        let mut f = legacy_prefix(0, 36, 8);
        f.extend_from_slice(&[
            (1, 3), // acmod = 1/0 mono
            (0, 1), // lfeon
            (0, 5), // dialnorm
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 2),
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3Audio, &f);
        assert_eq!(s.channel_count, 1); // AC3_CHANNELS[1]
        assert_eq!(s.audio_mode, TsAudioMode::Unknown);
    }

    #[test]
    fn legacy_optional_bitstream_fields_are_skipped_when_present() {
        // All three optional bools set in both triples (acmod 0 to reach the
        // second triple): each true arm skips its field. Surmixlev also engaged
        // via acmod 4 (3/0 → has a centre/surround mix the first `if` skips).
        let mut f = legacy_prefix(0, 36, 8);
        f.extend_from_slice(&[
            (4, 3), // acmod 3/0: channel_mode & 4 → surmixlev skip
            (0, 2), // surmixlev
            (0, 1), // lfeon
            (0, 5), // dialnorm
            (1, 1),
            (0, 8), // compre present → skip 8
            (1, 1),
            (0, 8), // langcode present → skip 8
            (1, 1),
            (0, 7), // audprodie present → skip 7
            (0, 2),
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3Audio, &f);
        assert_eq!(s.channel_count, 3); // AC3_CHANNELS[4]
    }

    #[test]
    fn legacy_bsid6_extension_sets_audio_modes() {
        // bsid 6 (the AC3-EX header): the xbsi2 fields decode dsurexmod 2 → Extended.
        let mut ex = legacy_prefix(0, 36, 6);
        ex.extend_from_slice(&[
            (7, 3), // acmod 5.1
            (0, 2),
            (0, 2), // cmixlev + surmixlev
            (1, 1), // lfeon
            (0, 5), // dialnorm
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 2),  // copyright + orig
            (0, 1),  // xbsi1e absent → no 14-bit skip
            (1, 1),  // xbsi2e present
            (2, 2),  // dsurexmod = 2 → Extended
            (0, 2),  // dheadphonmod
            (0, 10), // remaining xbsi2 bits
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3Audio, &ex);
        assert_eq!(s.audio_mode, TsAudioMode::Extended);
        assert_eq!(s.codec_short_name(), "AC3-EX");
        assert_eq!(s.channel_description(), "5.1-EX");

        // xbsi1e present (14-bit skip), xbsi2e present but dsurexmod != 2 → not
        // Extended (the audio-mode default match then leaves Unknown for acmod 7).
        let mut not_ex = legacy_prefix(0, 36, 6);
        not_ex.extend_from_slice(&[
            (7, 3),
            (0, 2),
            (0, 2),
            (1, 1),
            (0, 5),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 2),
            (1, 1),
            (0, 14), // xbsi1e present → skip 14
            (1, 1),
            (0, 2),
            (0, 2),
            (0, 10), // xbsi2e present, dsurexmod 0
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3Audio, &not_ex);
        assert_eq!(s.audio_mode, TsAudioMode::Unknown);

        // Both xbsi flags absent → the bsid==6 block skips both inner branches.
        let mut bare = legacy_prefix(0, 36, 6);
        bare.extend_from_slice(&[
            (7, 3),
            (0, 2),
            (0, 2),
            (1, 1),
            (0, 5),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 2),
            (0, 1),
            (0, 1),
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3Audio, &bare);
        assert_eq!(s.audio_mode, TsAudioMode::Unknown);
        assert_eq!(s.channel_count, 5);
    }

    #[test]
    fn legacy_sample_rate_codes_and_reset() {
        let with_fscod = |fscod| {
            let mut f = legacy_prefix(fscod, 36, 8);
            f.extend_from_slice(&[(1, 3), (0, 1), (0, 5), (0, 1), (0, 1), (0, 1), (0, 2), (0, 16)]);
            run(TsStreamType::Ac3Audio, &f).sample_rate
        };
        assert_eq!(with_fscod(0), 48_000);
        assert_eq!(with_fscod(1), 44_100);
        assert_eq!(with_fscod(2), 32_000);
        // fscod 3 (reserved) → sample_rate 0. Pre-set the field to prove it resets.
        let mut s = stream(TsStreamType::Ac3Audio);
        s.sample_rate = 99_999;
        let mut f = legacy_prefix(3, 36, 8);
        f.extend_from_slice(&[(1, 3), (0, 1), (0, 5), (0, 1), (0, 1), (0, 1), (0, 2), (0, 16)]);
        let mut b = buf(&pack(&f));
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        assert_eq!(s.sample_rate, 0);
    }

    #[test]
    fn legacy_high_frame_size_code_leaves_bitrate_unset() {
        // frmsizecod 62 → index 31 (past the 19-entry table): bit_rate stays 0.
        let mut f = legacy_prefix(0, 62, 8);
        f.extend_from_slice(&[(1, 3), (0, 1), (0, 5), (0, 1), (0, 1), (0, 1), (0, 2), (0, 16)]);
        let s = run(TsStreamType::Ac3Audio, &f);
        assert_eq!(s.base.bit_rate, 0);
    }

    #[test]
    fn legacy_low_sample_rate_shift_by_bsid() {
        // sr_shift = max(bsid, 8) - 8 right-shifts BOTH the sample rate and the table
        // bit rate: bsid <= 8 → no shift; bsid 9 → halve; bsid 10 → quarter (the AC-3
        // "low-sample-rate" variants; FFmpeg parity, ATSC A/52 §5.4.1).
        let rate_and_bitrate = |bsid| {
            // fscod 0 (48 kHz) / frmsizecod 36 (640 kbps), acmod 5.1.
            let mut f = legacy_prefix(0, 36, bsid);
            f.extend_from_slice(&[
                (7, 3),
                (0, 2),
                (0, 2),
                (1, 1),
                (0, 5),
                (0, 1),
                (0, 1),
                (0, 1),
                (0, 2),
                (0, 16),
            ]);
            let s = run(TsStreamType::Ac3Audio, &f);
            (s.sample_rate, s.base.bit_rate)
        };
        // bsid < 8 → sr_shift 0: the unshifted 48 kHz / 640 kbps (no low-rate variant).
        assert_eq!(rate_and_bitrate(7), (48_000, 640_000));
        // bsid 9 → sr_shift 1: 24 kHz / 320 kbps.
        assert_eq!(rate_and_bitrate(9), (24_000, 320_000));
        // bsid 10 → sr_shift 2: 12 kHz / 160 kbps.
        assert_eq!(rate_and_bitrate(10), (12_000, 160_000));
    }

    #[test]
    fn rejects_non_ac3_sync_and_short_buffers() {
        // Wrong sync word → untouched.
        let mut s = stream(TsStreamType::Ac3Audio);
        let mut b = buf(&[0x00, 0x00, 0x24, 0x40, 0xE1, 0xF8, 0x00, 0x00]);
        scan(&mut s, &mut b, &mut None);
        assert!(!s.base.is_initialized);
        assert_eq!(s.channel_count, 0);

        // Exactly one sync byte wrong is still rejected (each half of the `||`).
        for [b0, b1] in [[0x0B_u8, 0x00], [0x00, 0x77]] {
            let mut s = stream(TsStreamType::Ac3Audio);
            let mut b = buf(&[b0, b1, 0x24, 0x40, 0xE1, 0xF8, 0x00, 0x00]);
            scan(&mut s, &mut b, &mut None);
            assert_eq!(s.channel_count, 0, "sync {b0:02X} {b1:02X} must be rejected");
        }

        // Valid sync but too short for the 4-byte header read → untouched.
        let mut s = stream(TsStreamType::Ac3Audio);
        let mut b = buf(&[0x0B, 0x77, 0x24, 0x40]);
        scan(&mut s, &mut b, &mut None);
        assert!(!s.base.is_initialized);

        // An already-initialized stream returns immediately.
        let mut s = stream(TsStreamType::Ac3Audio);
        s.base.is_initialized = true;
        let mut b = buf(&pack(&legacy_prefix(0, 36, 8)));
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.channel_count, 0); // never parsed
    }

    /// The E-AC-3 (`bsid > 10`) field prefix up to `dialnorm_ext`, with no
    /// dependent core (`frame_type` 0). `bsid` 16 keeps it on the E-AC-3 branch.
    fn eac3_prefix(
        frame_type: u64,
        frame_size_field: u64,
        sr_code: u64,
        num_blocks: u64,
        acmod: u64,
        lfe: u64,
    ) -> Vec<(u64, u32)> {
        vec![
            (0x0B77, 16),
            (frame_type, 2),
            (0, 3),
            (frame_size_field, 11),
            (sr_code, 2),
            (num_blocks, 2),
            (acmod, 3),
            (lfe, 1),
            (16, 5), // bsid (> 10 → E-AC-3)
            (0, 5),  // dialnorm_ext
            (0, 1),  // compr absent
        ]
    }

    #[test]
    fn eac3_basic_stereo_no_emdf() {
        let mut f = eac3_prefix(0, 0, 0, 1, 2, 0);
        f.push((0, 256)); // zero tail: the EMDF sync scan never matches
        let s = run(TsStreamType::Ac3PlusAudio, &f);
        assert_eq!(s.channel_count, 2); // AC3_CHANNELS[2]
        assert_eq!(s.audio_mode, TsAudioMode::Stereo);
        assert_eq!(s.sample_rate, 48_000);
        // bit_rate = trunc(4.0 * frame_size * sample_rate / (num_blocks * 256));
        // frame_size = (0 + 1) << 1 = 2 → 4*2*48000/256 = 1500.
        assert_eq!(s.base.bit_rate, 1500);
        assert!(s.base.is_initialized);
        assert!(!s.has_extensions);
        assert_eq!(s.codec_short_name(), "AC3+");
        assert_eq!(s.codec_name(), "Dolby Digital Plus Audio");
    }

    #[test]
    fn eac3_sr_code_3_halves_rate_and_uses_six_blocks() {
        // sr_code 3 → read the fscod2 field (here 1 = 44.1 kHz table value) and,
        // per the reduced-rate spec, report half of it with the fixed 6 blocks.
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2), // frame_type
            (0, 3),
            (0, 11), // frame_size_field → frameSize 2
            (3, 2),  // sr_code == 3
            (1, 2),  // fscod2 → 44.1 kHz table value, halved to 22.05 kHz
            (2, 3),  // acmod 2/0
            (0, 1),  // lfe
            (16, 5), // bsid
            (0, 5),  // dialnorm_ext
            (0, 1),  // compr
        ];
        f.push((0, 256));
        let s = run(TsStreamType::Ac3PlusAudio, &f);
        assert_eq!(s.sample_rate, 22_050);
        // num_blocks = 6 → trunc(4*2*22050/(6*256)) = trunc(114.84) = 114.
        assert_eq!(s.base.bit_rate, 114);
    }

    #[test]
    fn eac3_sr_code_3_fscod2_zero_is_24khz() {
        // fscod2 == 0 selects the 48 kHz table value → reduced rate 24 kHz.
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2), // frame_type
            (0, 3),
            (0, 11), // frame_size_field → frameSize 2
            (3, 2),  // sr_code == 3
            (0, 2),  // fscod2 → 48 kHz table value, halved to 24 kHz
            (2, 3),  // acmod 2/0
            (0, 1),  // lfe
            (16, 5), // bsid
            (0, 5),  // dialnorm_ext
            (0, 1),  // compr
        ];
        f.push((0, 256));
        let s = run(TsStreamType::Ac3PlusAudio, &f);
        assert_eq!(s.sample_rate, 24_000);
        // num_blocks = 6 → trunc(4*2*24000/(6*256)) = trunc(125.0) = 125.
        assert_eq!(s.base.bit_rate, 125);
    }

    #[test]
    fn eac3_channel_mode_zero_runs_the_extra_skip() {
        // acmod 0 (1+1): the E-AC-3 channel_mode==0 sub-block (skip 5 + one bool).
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2),
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2),  // num_blocks 1
            (0, 3),  // acmod 0
            (0, 1),  // lfe
            (16, 5), // bsid
            (0, 5),  // dialnorm_ext
            (0, 1),  // compr absent
            (0, 5),  // channel_mode==0 skip 5
            (1, 1),
            (0, 8), // the inner bool present → skip 8
        ];
        f.push((0, 256));
        let s = run(TsStreamType::Ac3PlusAudio, &f);
        assert_eq!(s.channel_count, 2); // AC3_CHANNELS[0]
        assert_eq!(s.audio_mode, TsAudioMode::DualMono);

        // The same 1+1 block with the inner optional bool absent (its false arm).
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2),
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2),
            (0, 3), // acmod 0
            (0, 1),
            (16, 5),
            (0, 5),
            (0, 1),
            (0, 5), // channel_mode==0 skip 5
            (0, 1), // inner bool absent → no skip
        ];
        f.push((0, 256));
        let s = run(TsStreamType::Ac3PlusAudio, &f);
        assert_eq!(s.channel_count, 2);
    }

    #[test]
    fn eac3_dependent_substream_clones_core_and_remaps_channels() {
        // frame_type 1 (dependent): clone the core (carrying the pre-set bit rate),
        // then channel_remapping present → channels from the chanmap + the core's
        // bit rate folded into the total.
        let mut s = stream(TsStreamType::Ac3PlusAudio);
        s.base.bit_rate = 5000; // the clone copies this → the core's bit_rate
        let chanmap: u64 = 0x0400; // one object group (bit 5 MSB-first) → ac3_chan_map = 2
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (1, 2), // frame_type = dependent
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2), // frameSize 2, sr 48k, numBlocks 1
            // acmod 3 (3/0): AC3_CHANNELS[3]=3 != the remapped 2, so the post-remap
            // `channel_count == 0` guard must NOT overwrite the remapped count.
            (3, 3),
            (0, 1),  // lfe
            (16, 5), // bsid
            (0, 5),  // dialnorm_ext
            (0, 1),  // compr absent
            (1, 1),  // channel_remapping present
            (chanmap, 16),
        ];
        f.push((0, 256)); // zero tail → no EMDF
        let mut b = buf(&pack(&f));
        scan(&mut s, &mut b, &mut None);
        assert!(s.core_stream.is_some());
        assert_eq!(s.channel_count, 2); // 0 (core) + ac3_chan_map = 2 (not AC3_CHANNELS[3])
        assert_eq!(s.lfe, 0); // taken from the core's LFE
        assert_eq!(s.audio_mode, TsAudioMode::Unknown); // acmod 3 → default switch arm
        // bit_rate = frame calc (1500) + core bit_rate (5000).
        assert_eq!(s.base.bit_rate, 6500);
        assert!(s.base.is_initialized);

        // frame_type 1 but no channel_remapping → channels from acmod, LFE from the
        // field; the core is still cloned (so the bit rate still folds in 0).
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (1, 2),
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2),
            (2, 3),
            (1, 1),
            (16, 5),
            (0, 5),
            (0, 1),
            (0, 1), // channel_remapping absent
        ];
        f.push((0, 256));
        let s = run(TsStreamType::Ac3PlusAudio, &f);
        assert_eq!(s.channel_count, 2); // AC3_CHANNELS[2]
        assert_eq!(s.lfe, 1); // from the lfe field
        assert!(s.core_stream.is_some());
    }

    /// An E-AC-3 frame whose body (`frame_type` 0, acmod 2) is followed by an EMDF
    /// container starting with the `0x5838` sync; `emdf_tail` supplies the bytes
    /// after that sync.
    fn eac3_with_emdf(emdf_tail: &[(u64, u32)]) -> Vec<(u64, u32)> {
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2),
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2),
            (2, 3),
            (0, 1),
            (16, 5),
            (0, 5),
            (0, 1),
            (0x5838, 16), // emdf_sync → found on the first window
        ];
        f.extend_from_slice(emdf_tail);
        f
    }

    #[test]
    fn eac3_emdf_versioned_payload_is_skipped() {
        // emdf_version 1 (> 0): the whole container is skipped by bit count.
        let mut tail = vec![(2, 16), (1, 2)]; // container_size 2, version 1
        tail.push((0, 64));
        let s = run(TsStreamType::Ac3PlusAudio, &eac3_with_emdf(&tail));
        assert!(!s.has_extensions);
        assert_eq!(s.channel_count, 2);

        // emdf_version field 3 → an extra 2-bit read is added to the version.
        let mut tail = vec![(2, 16), (3, 2), (0, 2)];
        tail.push((0, 64));
        let s = run(TsStreamType::Ac3PlusAudio, &eac3_with_emdf(&tail));
        assert!(!s.has_extensions);
    }

    #[test]
    fn eac3_emdf_joc_payload_sets_atmos_extension() {
        // version 0 path: the temp==7 skip, a first payload (id 1..15), the payload
        // loop body (id 0x1F), then the terminating id-14 JOC payload whose nonzero
        // object-count bits raise has_extensions (Atmos).
        let tail = vec![
            (0, 16),   // container_size
            (0, 2),    // emdf_version = 0
            (7, 3),    // temp == 7 → skip 2
            (0, 2),    // [skipped]
            (1, 5),    // emdf_payload_id = 1 (in 1..16)
            (0, 7),    // emdf_payload_config (all flags clear)
            (0, 8),    // payload_size = 0
            (0, 1),    // bs_skip_bits(payload_size + 1)
            (0x1F, 5), // loop: id 0x1F → skip 5
            (0, 5),    // [skipped]
            (0, 7),    // emdf_payload_config
            (0, 8),    // payload_size = 0
            (0, 1),    // read_bits4(payload_size + 1)
            (14, 5),   // loop: id 14 → exit
            (0, 7),    // final emdf_payload_config
            (0, 12),   // bs_skip_bits(12)
            (1, 6),    // joc_num_objects_bits = 1 (> 0) → has_extensions
            (0, 64),
        ];
        let s = run(TsStreamType::Ac3PlusAudio, &eac3_with_emdf(&tail));
        assert!(s.has_extensions);
        // With Atmos the codec strings switch to the /Atmos spellings.
        assert_eq!(s.codec_short_name(), "AC3+");
        assert_eq!(s.codec_name(), "Dolby Digital Plus/Atmos Audio");
    }

    #[test]
    fn eac3_emdf_payload_config_with_all_fields_present() {
        // Drive every branch of emdf_payload_config: sample_offset present (skip 12),
        // each of the three optional descriptors present, and discard_unknown set
        // (so the trailing alignment branch is skipped).
        let tail = vec![
            (0, 16), // container_size
            (0, 2),  // version 0
            (0, 3),  // temp != 7
            (1, 5),  // payload id 1
            // emdf_payload_config, all flags set:
            (1, 1),
            (0, 12), // sample_offset_e → skip 12
            (1, 1),
            (0, 11), // duration → skip 11
            (1, 1),
            (0, 2), // group id → skip 2
            (1, 1),
            (0, 8),  // reserved → skip 8
            (1, 1),  // discard_unknown_payload = 1 → no alignment branch
            (0, 8),  // payload_size = 0
            (0, 1),  // bs_skip_bits(1)
            (14, 5), // loop exits immediately on id 14
            // final JOC block with zero objects → no extension:
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (1, 1), // emdf_payload_config (discard set)
            (0, 8),
            (0, 1),
            (0, 12), // bs_skip_bits(12)
            (0, 6),  // joc_num_objects_bits = 0
            (0, 64),
        ];
        let s = run(TsStreamType::Ac3PlusAudio, &eac3_with_emdf(&tail));
        assert!(!s.has_extensions);
    }

    #[test]
    fn eac3_emdf_payload_loop_runs_to_end_of_buffer() {
        // version 0, a first payload id of 0 (skips the `id in 1..16` block), then a
        // payload loop whose ids are never 0x1F nor 14: it consumes payloads until
        // the buffer is exhausted, leaving the final `id == 14` guard false.
        let tail = vec![
            (0, 16), // container_size
            (0, 2),  // version 0
            (0, 3),  // temp != 7
            (0, 5),  // emdf_payload_id 0 → the 1..16 block is skipped
            (0, 48), // zero payloads (id 0, never 0x1F/14) until position >= length
        ];
        let s = run(TsStreamType::Ac3PlusAudio, &eac3_with_emdf(&tail));
        assert!(!s.has_extensions);
    }

    #[test]
    fn eac3_emdf_payload_config_alignment_branch() {
        // sample_offset_e clear + discard_unknown clear → the payload_frame_aligned
        // inner branch runs (skip 9).
        let tail = vec![
            (0, 16), // container_size
            (0, 2),  // version 0
            (0, 3),  // temp != 7
            (1, 5),  // payload id 1
            // emdf_payload_config: sample_offset clear, descriptors clear, discard 0:
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 1), // discard_unknown_payload = 0 → bs_skip_bits(1) then …
            (0, 1), // [the bs_skip_bits(1)]
            (1, 1),
            (0, 9), // payload_frame_aligned present → skip 9
            (0, 8),
            (0, 1),  // payload_size 0, bs_skip_bits(1)
            (14, 5), // loop exits on id 14
            (0, 7),  // final emdf_payload_config (discard clear, sample_offset clear)
            (0, 12),
            (0, 6),
            (0, 64),
        ];
        let s = run(TsStreamType::Ac3PlusAudio, &eac3_with_emdf(&tail));
        assert!(!s.has_extensions);
    }

    /// The EMDF payload sequence that, parsed at `version == 0`, reaches an id-14
    /// JOC payload with a nonzero object count (→ `has_extensions`). `temp` selects the
    /// 3-bit `temp` field (7 triggers the extra 2-bit skip).
    fn joc_payload_seq(temp: u64) -> Vec<(u64, u32)> {
        vec![
            (temp, 3),
            (0, 2), // skipped only when temp == 7
            (1, 5), // first payload id (1..16)
            (0, 7),
            (0, 8),
            (0, 1),
            (0x1F, 5), // loop body: id 0x1F → skip 5
            (0, 5),
            (0, 7),
            (0, 8),
            (0, 1),
            (14, 5), // loop exit on id 14
            (0, 7),
            (0, 12),
            (1, 6), // joc_num_objects_bits = 1 → has_extensions
        ]
    }

    #[test]
    fn eac3_emdf_skipped_first_payload_id_still_reaches_joc() {
        // A first payload id of 0 (or 16) is outside the `id in 1..16` block, so it
        // is skipped and the loop reads the id-14 JOC payload directly. Pins the
        // `> 0 && < 16` bounds: a mutated comparison would wrongly *consume* the
        // payload and lose the JOC.
        for first_id in [0_u64, 16] {
            let mut tail = vec![
                (0, 16), // container_size
                (0, 2),  // version 0
                (0, 3),  // temp != 7
                (first_id, 5),
                (14, 5), // the loop immediately reads the id-14 JOC payload
                (0, 7),
                (0, 12),
                (1, 6), // joc = 1 → has_extensions
                (0, 64),
            ];
            tail.push((0, 64));
            let s = run(TsStreamType::Ac3PlusAudio, &eac3_with_emdf(&tail));
            assert!(s.has_extensions, "first payload id {first_id} should skip to the JOC");
        }
    }

    #[test]
    fn eac3_emdf_versioned_container_hides_a_latent_joc() {
        // A version-1 container is skipped wholesale, so even a byte sequence that
        // *would* parse as a JOC at version 0 raises nothing. Pins the `version > 0`
        // test (a mutated `< 0` would fall through and parse the latent JOC).
        let mut tail = vec![(16, 16), (1, 2)]; // container_size 16, version 1 (skip 126 bits)
        tail.extend(joc_payload_seq(7));
        tail.push((0, 128));
        let s = run(TsStreamType::Ac3PlusAudio, &eac3_with_emdf(&tail));
        assert!(!s.has_extensions);
    }

    #[test]
    fn eac3_emdf_sync_found_at_odd_bit_offset() {
        // The EMDF sync sits one bit past the container start, so only the
        // bit-by-bit rewind scan (`seek(-2)` + skip 1) finds it; a forward seek or an
        // early break would miss it and leave the stream un-flagged.
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2),
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2),
            (2, 3),
            (0, 1),
            (16, 5),
            (0, 5),
            (0, 1),
            (0, 1),       // one junk bit → the sync is not byte-aligned
            (0x5838, 16), // emdf_sync at bit offset 1
            (0, 16),      // container_size
            (0, 2),       // version 0
            (0, 3),       // temp != 7
            (0, 5),       // first payload id 0 → skip to the loop
            (14, 5),      // JOC payload
            (0, 7),
            (0, 12),
            (1, 6), // joc = 1 → has_extensions
        ];
        f.push((0, 128));
        let s = run(TsStreamType::Ac3PlusAudio, &f);
        assert!(s.has_extensions);
    }

    #[test]
    fn dialnorm_paths_by_stream_type() {
        // E-AC-3 (bsid 16) on a *second* frame (channel_count already > 0):
        // dial_norm comes from dialnorm_ext.
        let mut s = stream(TsStreamType::Ac3PlusAudio);
        s.channel_count = 6; // second_frame = true
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2),
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2),
            (2, 3),
            (0, 1),
            (16, 5),
            (27, 5), // dialnorm_ext = 27 → DN -27
            (0, 1),
        ];
        f.push((0, 256));
        let mut b = buf(&pack(&f));
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.dial_norm, -27);

        // Secondary E-AC-3 never carries dial_norm (the outer not-secondary guard).
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2),
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2),
            (2, 3),
            (0, 1),
            (16, 5),
            (27, 5),
            (0, 1),
        ];
        f.push((0, 256));
        let s = run(TsStreamType::Ac3PlusSecondaryAudio, &f);
        assert_eq!(s.dial_norm, 0);

        // E-AC-3 on a *first* frame (no prior channels → second_frame false): neither
        // dial_norm arm applies, so the field stays 0 even with a nonzero dialnorm_ext
        // (the `Ac3PlusAudio && second_frame` guard must be a conjunction).
        let mut f: Vec<(u64, u32)> = vec![
            (0x0B77, 16),
            (0, 2),
            (0, 3),
            (0, 11),
            (0, 2),
            (1, 2),
            (2, 3),
            (0, 1),
            (16, 5),
            (27, 5), // dialnorm_ext = 27, but second_frame is false
            (0, 1),
        ];
        f.push((0, 256));
        let s = run(TsStreamType::Ac3PlusAudio, &f);
        assert_eq!(s.dial_norm, 0);
    }

    #[test]
    fn eac3_plus_bsid6_first_frame_defers_initialization() {
        // An AC3+ stream whose first frame is the legacy (bsid 6) core: with no
        // prior channels (second_frame false) the codec defers initialization, and
        // dial_norm still comes from the legacy dialnorm field.
        let mut first = legacy_prefix(0, 36, 6);
        first.extend_from_slice(&[
            (7, 3),
            (0, 2),
            (0, 2),
            (1, 1),
            (20, 5),
            (0, 1),
            (0, 1),
            (0, 1),
            (0, 2),
            (0, 1),
            (0, 1),
            (0, 16),
        ]);
        let s = run(TsStreamType::Ac3PlusAudio, &first);
        assert!(!s.base.is_initialized); // deferred
        assert_eq!(s.dial_norm, -20);

        // A later frame (channels already present → second_frame) initializes.
        let mut s = stream(TsStreamType::Ac3PlusAudio);
        s.channel_count = 5;
        let mut b = buf(&pack(&first));
        scan(&mut s, &mut b, &mut None);
        assert!(s.base.is_initialized);
    }

    #[test]
    fn ac3_chan_map_counts_object_groups() {
        // The MSB-first bit for channel-map position `pos`.
        let bit = |pos: u32| 1_u32.wrapping_shl(15_u32.wrapping_sub(pos));
        // Only bit positions 5, 6, 9, 10, 11 each contribute 2.
        assert_eq!(ac3_chan_map(0), 0);
        for pos in [5_u32, 6, 9, 10, 11] {
            assert_eq!(ac3_chan_map(bit(pos)), 2, "pos {pos}");
        }
        // A position that is not an object group contributes nothing.
        assert_eq!(ac3_chan_map(bit(0)), 0);
        assert_eq!(ac3_chan_map(bit(8)), 0);
        // All five object groups together → 10.
        let all = [5_u32, 6, 9, 10, 11].iter().fold(0_u32, |a, &p| a | bit(p));
        assert_eq!(ac3_chan_map(all), 10);
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>()) {
            for ty in [
                TsStreamType::Ac3Audio,
                TsStreamType::Ac3PlusAudio,
                TsStreamType::Ac3PlusSecondaryAudio,
            ] {
                let mut s = stream(ty);
                let mut b = buf(&data);
                let mut tag = None;
                scan(&mut s, &mut b, &mut tag);
            }
        }
    }
}
