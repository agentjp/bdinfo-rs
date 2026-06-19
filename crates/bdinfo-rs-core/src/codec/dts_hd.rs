//! DTS-HD audio codec scanner — Master Audio / High-Res / Express + DTS:X.
//!
//! A DTS-HD elementary stream carries a
//! backward-compatible DTS **core** sub-stream interleaved with the extension
//! sub-stream's HD frames, so [`scan`] does both jobs: when the buffer holds no
//! DTS-HD sync (`0x64582025`) it tags the access unit `"CORE"` and runs
//! [`crate::codec::dts`] over the embedded
//! [`core_stream`](TsAudioStream::core_stream); when it does, it tags it `"HD"` and
//! reads the substream-asset header for the sample rate, bit depth, channel count +
//! LFE (from the speaker-activity mask), and scans the extension sub-stream sync
//! words for a DTS:X marker — the legacy `0x02000850` or the DTS:X IMAX
//! `0xF14000D0` (matched with its low bit ignored) — that raises `has_extensions`.
//!
//! `bitrate` is the demux-measured stream rate; Master Audio is always flagged VBR,
//! while High-Res / Express initialize from that measured rate (plus the core's).
//!
//! All fixed-width codec math uses `wrapping_*` so hostile field values cannot
//! overflow. The asset section is a single pass: only the first asset's
//! descriptor feeds the stream fields, and any further assets go unparsed.
//! Several descriptor arrays (the active-extension masks, mix-output masks, asset
//! sizes, and info text) are read purely for framing — their values feed nothing.
//! The substream-mask parity test is `j % 2 == 0`: every even substream index
//! contributes one mask byte.

use crate::bitstream::TsStreamBuffer;
use crate::codec::dts;
use crate::stream::{TsAudioMode, TsAudioStream, TsStreamType};

/// The DTS-HD extension-substream sync word scanned for in the access unit
/// (`0x64582025`).
const DTS_HD_SYNC: u32 = 0x6458_2025;

/// The legacy DTS:X extension pattern (`0x02000850`, `DCA_SYNCWORD_XLL_X`) that,
/// found after one of the extension sync words, raises `has_extensions`.
const DTS_X_PATTERN: u32 = 0x0200_0850;

/// The DTS:X IMAX extension pattern (`0xF14000D0`, `DCA_SYNCWORD_XLL_X_IMAX`). It is
/// matched with the low bit ignored (`extradata_syncword >> 1`), so both `0xF14000D0`
/// and `0xF14000D1` count as DTS:X IMAX — the two values that `>> 1` collapses to (per
/// `libavcodec/dca_xll.c`). A DTS:X IMAX stream carries this word and not the legacy
/// one, so without it the track loses its DTS:X label.
const DTS_X_IMAX_PATTERN: u32 = 0xF140_00D0;

/// The DTS:X IMAX pattern with its low bit set — the second value the LSB-ignored
/// match accepts. See [`DTS_X_IMAX_PATTERN`].
const DTS_X_IMAX_PATTERN_ODD: u32 = 0xF140_00D1;

/// Sample rate in Hz per the 4-bit `nuMaxSampleRate` code.
const SAMPLE_RATES: [i32; 16] = [
    0x1F40, 0x3E80, 0x7D00, 0xFA00, 0x1_F400, 0x5622, 0xAC44, 0x1_5888, 0x2_B110, 0x5_6220, 0x2EE0,
    0x5DC0, 0xBB80, 0x1_7700, 0x2_EE00, 0x5_DC00,
];

/// Scans one DTS-HD access unit from `buffer` into `stream`.
///
/// With no DTS-HD sync the embedded DTS core is scanned (and `tag` set to `"CORE"`);
/// with one, the HD substream header is read (`tag` set to `"HD"`). `bitrate` is the
/// demux-measured rate used for the non-Master-Audio init paths.
#[expect(
    clippy::too_many_lines,
    reason = "one linear substream-asset header parse; splitting it would obscure the field order"
)]
pub fn scan(
    stream: &mut TsAudioStream,
    buffer: &mut TsStreamBuffer,
    bitrate: i64,
    tag: &mut Option<String>,
) {
    if stream.base.is_initialized
        && (stream.base.stream_type == TsStreamType::DtsHdSecondaryAudio
            || stream.core_stream.as_ref().is_some_and(|c| c.base.is_initialized))
    {
        return;
    }

    let mut sync_found = false;
    let mut sync: u32 = 0;
    // A bounded byte-at-a-time scan for the sync over the whole buffer.
    for _ in 0..buffer.length() {
        sync = sync.wrapping_shl(8).wrapping_add(u32::from(buffer.read_byte(false)));
        if sync == DTS_HD_SYNC {
            sync_found = true;
            break;
        }
    }

    if !sync_found {
        *tag = Some("CORE".to_owned());
        if stream.core_stream.is_none() {
            // Seed the embedded DTS core stream on first sight.
            let mut core = TsAudioStream::default();
            core.base.stream_type = TsStreamType::DtsAudio;
            stream.core_stream = Some(Box::new(core));
        }
        if let Some(core) = stream.core_stream.as_mut()
            && !core.base.is_initialized
        {
            buffer.begin_read();
            dts::scan(core, buffer, bitrate, tag);
        }
        return;
    }

    *tag = Some("HD".to_owned());
    buffer.bs_skip_bits(8, false);
    let nu_sub_stream_index = buffer.read_bits4(2, false);
    let blown_up_header = buffer.read_bool(false);
    buffer.bs_skip_bits(if blown_up_header { 32 } else { 24 }, false);

    let mut nu_num_assets: u32 = 1;
    let static_fields_present = buffer.read_bool(false);
    if static_fields_present {
        buffer.bs_skip_bits(5, false);
        if buffer.read_bool(false) {
            buffer.bs_skip_bits(36, false);
        }
        let nu_num_audio_present = u32::from(buffer.read_bits2(3, false)).wrapping_add(1);
        nu_num_assets = u32::from(buffer.read_bits2(3, false)).wrapping_add(1);
        // `nuSubStreamIndex + 1`: the active-mask bit width and the per-presentation
        // substream loop bound (computed once, used at both sites).
        let sub_plus_1 = nu_sub_stream_index.wrapping_add(1);
        let mask_width = usize::try_from(sub_plus_1).unwrap_or(0);
        for _ in 0..nu_num_audio_present {
            // nuActiveExSsMask[i] — read for framing, value unused.
            let _ = buffer.read_bits4(mask_width, false);
        }
        for _ in 0..nu_num_audio_present {
            for j in 0..sub_plus_1 {
                // Every even substream index contributes one mask byte.
                if j.wrapping_rem(2) == 0 {
                    buffer.bs_skip_bits(8, false);
                }
            }
        }
        if buffer.read_bool(false) {
            buffer.bs_skip_bits(2, false);
            let nu_bits4_mix_out_mask =
                usize::from(buffer.read_bits2(2, false)).wrapping_mul(4).wrapping_add(4);
            let nu_num_mix_out_configs = u32::from(buffer.read_bits2(2, false)).wrapping_add(1);
            for _ in 0..nu_num_mix_out_configs {
                // nuMixOutChMask[i] — read for framing, value unused.
                let _ = buffer.read_bits4(nu_bits4_mix_out_mask, false);
            }
        }
    }

    // assetSizes[i] — read (20- or 16-bit) for framing, values unused.
    for _ in 0..nu_num_assets {
        let _ = buffer.read_bits4(if blown_up_header { 20 } else { 16 }, false);
    }

    // Only the first asset's descriptor feeds the stream fields, so the asset
    // section is a single pass; any further assets go unparsed.
    buffer.bs_skip_bits(12, false);
    if static_fields_present {
        if buffer.read_bool(false) {
            buffer.bs_skip_bits(4, false);
        }
        if buffer.read_bool(false) {
            buffer.bs_skip_bits(24, false);
        }
        if buffer.read_bool(false) {
            let nu_info_text_byte_size = u32::from(buffer.read_bits2(10, false)).wrapping_add(1);
            for _ in 0..nu_info_text_byte_size {
                // infoText[j] — read for framing, value unused.
                let _ = buffer.read_bits2(8, false);
            }
        }
        let nu_bit_resolution = i32::from(buffer.read_bits2(5, false)).wrapping_add(1);
        let nu_max_sample_rate = usize::from(buffer.read_bits2(4, false));
        let nu_total_num_chs = i32::from(buffer.read_bits2(8, false)).wrapping_add(1);
        let mut nu_spkr_activity_mask: u32 = 0;
        if buffer.read_bool(false) {
            if nu_total_num_chs > 2 {
                buffer.bs_skip_bits(1, false);
            }
            if nu_total_num_chs > 6 {
                buffer.bs_skip_bits(1, false);
            }
            if buffer.read_bool(false) {
                let nu_num_bits4_sa_mask =
                    usize::from(buffer.read_bits2(2, false)).wrapping_mul(4).wrapping_add(4);
                nu_spkr_activity_mask = buffer.read_bits4(nu_num_bits4_sa_mask, false);
            }
        }
        stream.sample_rate = SAMPLE_RATES.get(nu_max_sample_rate).copied().unwrap_or(0);
        stream.bit_depth = nu_bit_resolution;

        stream.lfe = 0;
        if (nu_spkr_activity_mask & 0x8) == 0x8 {
            stream.lfe = stream.lfe.wrapping_add(1);
        }
        if (nu_spkr_activity_mask & 0x1000) == 0x1000 {
            stream.lfe = stream.lfe.wrapping_add(1);
        }
        stream.channel_count = nu_total_num_chs.wrapping_sub(stream.lfe);
    }

    let mut temp2: u32 = 0;
    while buffer.position() < buffer.length() {
        temp2 = temp2.wrapping_shl(8).wrapping_add(u32::from(buffer.read_byte(false)));
        if matches!(
            temp2,
            0x41A2_9547 // XLL extended data
                | 0x655E_315E // XBR extended data
                | 0x0A80_1921 // XSA extended data
                | 0x1D95_F262 // X96k
                | 0x4700_4A03 // XXch
                | 0x5A5A_5A5A // Xch
        ) {
            let start = i64::try_from(buffer.position()).unwrap_or(i64::MAX);
            let len = i64::try_from(buffer.length()).unwrap_or(i64::MAX);
            let mut temp3: u32 = 0;
            let mut i = start;
            while i < len {
                temp3 = temp3.wrapping_shl(8).wrapping_add(u32::from(buffer.read_byte(false)));
                if matches!(temp3, DTS_X_PATTERN | DTS_X_IMAX_PATTERN | DTS_X_IMAX_PATTERN_ODD) {
                    stream.has_extensions = true;
                    break;
                }
                i = i.wrapping_add(1);
            }
        }
        if stream.has_extensions {
            break;
        }
    }

    if let Some(core) = stream.core_stream.as_ref()
        && core.audio_mode == TsAudioMode::Extended
        && stream.channel_count == 5
    {
        stream.audio_mode = TsAudioMode::Extended;
    }

    if stream.base.stream_type == TsStreamType::DtsHdMasterAudio {
        stream.base.is_vbr = true;
        stream.base.is_initialized = true;
    } else if bitrate > 0 {
        stream.base.is_vbr = false;
        stream.base.bit_rate = bitrate;
        if let Some(core) = stream.core_stream.as_ref() {
            stream.base.bit_rate = stream.base.bit_rate.wrapping_add(core.base.bit_rate);
        }
        // bit_rate = bitrate (> 0) + the core's rate (>= 0) is always positive
        // here, so the stream is unconditionally initialized.
        stream.base.is_initialized = true;
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

    /// A DTS-HD "HD" substream-header configuration, with a valid 5.1 / 48 kHz /
    /// 16-bit baseline; each test overrides only the fields it exercises.
    #[expect(
        clippy::struct_excessive_bools,
        reason = "each bool is a distinct substream-header presence flag this test builder toggles"
    )]
    struct Hd {
        nu_sub_stream_index: u64,
        blown_up: bool,
        static_present: bool,
        static_skip36: bool,
        num_audio_present_code: u64,
        num_assets_code: u64,
        mix_present: bool,
        mix_bits_code: u64,
        mix_configs_code: u64,
        asset_skip4: bool,
        asset_skip24: bool,
        info_present: bool,
        info_size_code: u64,
        bit_resolution_code: u64,
        max_sample_rate: u64,
        total_num_chs_code: u64,
        spkr_present: bool,
        sa_present: bool,
        sa_bits_code: u64,
        spkr_mask: u64,
        second_asset_chs_code: u64,
        /// Raw bytes appended after the (byte-aligned) packed header — the
        /// extension-substream region the post-asset sync scan reads. Raw (not
        /// bit-packed) so the 32-bit sync words land byte-aligned and are matchable.
        tail: Vec<u8>,
    }

    impl Default for Hd {
        fn default() -> Self {
            Self {
                nu_sub_stream_index: 0,
                blown_up: false,
                static_present: true,
                static_skip36: false,
                num_audio_present_code: 0,
                num_assets_code: 0,
                mix_present: false,
                mix_bits_code: 0,
                mix_configs_code: 0,
                asset_skip4: false,
                asset_skip24: false,
                info_present: false,
                info_size_code: 0,
                bit_resolution_code: 15, // → 16-bit
                max_sample_rate: 12,     // → 48 kHz
                total_num_chs_code: 5,   // → 6 channels
                spkr_present: true,
                sa_present: true,
                sa_bits_code: 0, // → 4-bit mask
                spkr_mask: 0x8,  // LFE bit → LFE 1
                second_asset_chs_code: 0,
                tail: vec![0; 8],
            }
        }
    }

    impl Hd {
        /// Emits the asset-header static fields (the second `if bStaticFieldsPresent`
        /// block) for `total_chs_code`.
        fn asset_static(&self, f: &mut Vec<(u64, u32)>, total_chs_code: u64) {
            f.push((u64::from(self.asset_skip4), 1));
            if self.asset_skip4 {
                f.push((0, 4));
            }
            f.push((u64::from(self.asset_skip24), 1));
            if self.asset_skip24 {
                f.push((0, 24));
            }
            f.push((u64::from(self.info_present), 1));
            if self.info_present {
                f.push((self.info_size_code, 10));
                for _ in 0..self.info_size_code.wrapping_add(1) {
                    f.push((0, 8));
                }
            }
            f.push((self.bit_resolution_code, 5));
            f.push((self.max_sample_rate, 4));
            f.push((total_chs_code, 8));
            let total_chs = total_chs_code.wrapping_add(1);
            f.push((u64::from(self.spkr_present), 1));
            if self.spkr_present {
                if total_chs > 2 {
                    f.push((0, 1));
                }
                if total_chs > 6 {
                    f.push((0, 1));
                }
                f.push((u64::from(self.sa_present), 1));
                if self.sa_present {
                    f.push((self.sa_bits_code, 2));
                    let width = self.sa_bits_code.wrapping_mul(4).wrapping_add(4);
                    f.push((self.spkr_mask, u32::try_from(width).unwrap_or(0)));
                }
            }
        }

        /// Builds the full access unit: the 4-byte `0x64582025` sync then the packed
        /// HD substream header.
        fn build(&self) -> Vec<u8> {
            let mut f: Vec<(u64, u32)> = vec![
                (0, 8),
                (self.nu_sub_stream_index, 2),
                (u64::from(self.blown_up), 1),
                (0, if self.blown_up { 32 } else { 24 }),
                (u64::from(self.static_present), 1),
            ];
            let num_assets = if self.static_present {
                f.push((0, 5));
                f.push((u64::from(self.static_skip36), 1));
                if self.static_skip36 {
                    f.push((0, 36));
                }
                f.push((self.num_audio_present_code, 3));
                f.push((self.num_assets_code, 3));
                let audio_present = self.num_audio_present_code.wrapping_add(1);
                let sub_plus_1 = self.nu_sub_stream_index.wrapping_add(1);
                for _ in 0..audio_present {
                    f.push((0, u32::try_from(sub_plus_1).unwrap_or(0)));
                }
                for _ in 0..audio_present {
                    for j in 0..sub_plus_1 {
                        if j % 2 == 0 {
                            f.push((0, 8));
                        }
                    }
                }
                f.push((u64::from(self.mix_present), 1));
                if self.mix_present {
                    f.push((0, 2));
                    f.push((self.mix_bits_code, 2));
                    f.push((self.mix_configs_code, 2));
                    let width = self.mix_bits_code.wrapping_mul(4).wrapping_add(4);
                    for _ in 0..self.mix_configs_code.wrapping_add(1) {
                        f.push((0, u32::try_from(width).unwrap_or(0)));
                    }
                }
                self.num_assets_code.wrapping_add(1)
            } else {
                1
            };

            // assetSizes (one per asset).
            for _ in 0..num_assets {
                f.push((0, if self.blown_up { 20 } else { 16 }));
            }

            // The single (first) asset pass.
            f.push((0, 12));
            if self.static_present {
                self.asset_static(&mut f, self.total_num_chs_code);
            }

            // Any further assets the scanner must *not* parse (present so an
            // accidental second pass would desync): their would-be skip12 + static
            // block.
            // (`num_assets > 1` only occurs inside the static-fields block, so static
            // fields are always present here.)
            for _ in 1..num_assets {
                f.push((0, 12));
                self.asset_static(&mut f, self.second_asset_chs_code);
            }

            let mut out = vec![0x64, 0x58, 0x20, 0x25];
            out.extend(pack(&f));
            // The extension region is appended as raw bytes after the byte-aligned
            // header, so its sync words are byte-aligned for the post-asset scan.
            out.extend_from_slice(&self.tail);
            out
        }
    }

    /// Runs [`scan`] over `bytes` for `stream_type` with `bitrate`, returning the
    /// mutated stream and the resulting tag.
    fn run(
        stream_type: TsStreamType,
        bytes: &[u8],
        bitrate: i64,
    ) -> (TsAudioStream, Option<String>) {
        let mut s = stream(stream_type);
        let mut b = buf(bytes);
        let mut tag = None;
        scan(&mut s, &mut b, bitrate, &mut tag);
        (s, tag)
    }

    /// A DTS core access unit (no `0x64582025`) yielding the embedded DTS Core line:
    /// 5.1 / 48 kHz / 1536 kbps, `pcmr` selecting the bit depth (0 → 16, 6 → 24).
    fn dts_core(pcmr: u64) -> Vec<u8> {
        pack(&[
            (0x7FFE_8001, 32),
            (0, 6),
            (0, 1), // crc absent
            (0, 7),
            (100, 14), // frame size
            (0, 6),
            (13, 4), // 48 kHz
            (24, 5), // 1536 kbps
            (0, 8),
            (0, 1), // ext_coding
            (0, 1),
            (1, 2), // lfe on
            (0, 1),
            (0, 7),
            (pcmr, 3),
            (0, 2),
            (0, 4), // dialnorm 0
            (0, 4),
            (4, 3), // channel base → 5
            (0, 64),
        ])
    }

    #[test]
    fn pack_handles_byte_aligned_and_partial_inputs() {
        // 16 bits → exactly two whole bytes (the trailing-partial path is skipped).
        assert_eq!(pack(&[(0xABCD, 16)]), vec![0xAB, 0xCD]);
        // 12 bits → one whole byte plus a left-aligned partial byte.
        assert_eq!(pack(&[(0xABC, 12)]), vec![0xAB, 0xC0]);
    }

    #[test]
    fn dts_hd_master_audio_over_core_matches_the_expected_line() {
        // The demux feeds a CORE access unit then an HD one; the result reproduces a
        // real disc's DTS-HD MA `desc` (16-bit variant).
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        let mut tag = None;

        let mut core_buf = buf(&dts_core(0));
        scan(&mut s, &mut core_buf, 0, &mut tag);
        assert_eq!(tag.as_deref(), Some("CORE"));
        assert_eq!(s.core_stream.as_deref().map(|c| c.channel_count), Some(5));
        assert!(!s.base.is_initialized);

        let mut hd_buf = buf(&Hd::default().build());
        scan(&mut s, &mut hd_buf, 0, &mut tag);
        assert_eq!(tag.as_deref(), Some("HD"));
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.channel_count, 5);
        assert_eq!(s.lfe, 1);
        assert_eq!(s.bit_depth, 16);
        assert!(!s.has_extensions);
        assert!(s.base.is_vbr);
        assert!(s.base.is_initialized);

        assert_eq!(s.codec_short_name(), "DTS-HD MA");
        assert_eq!(s.codec_name(), "DTS-HD Master Audio");
        assert_eq!(
            s.description(),
            "5.1 / 48 kHz / 16-bit (DTS Core: 5.1 / 48 kHz /  1536 kbps / 16-bit)"
        );
    }

    #[test]
    fn dts_x_master_audio_over_core_matches_the_expected_line() {
        // A real DTS:X disc sample: 7.1 / 48 kHz / 24-bit HD over a 24-bit 5.1 core,
        // with the DTS:X pattern after an extension sync word raising has_extensions.
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        let mut tag = None;
        let mut core_buf = buf(&dts_core(6)); // 24-bit core (even pcmr → not -ES)
        scan(&mut s, &mut core_buf, 0, &mut tag);

        // 7.1 → total 8, LFE 1; 24-bit; a tail with an XLL sync then the DTS:X pattern.
        let hd = Hd {
            bit_resolution_code: 23, // → 24-bit
            total_num_chs_code: 7,   // → 8 channels
            sa_bits_code: 3,         // → 16-bit mask (room for bit 12)
            spkr_mask: 0x8,          // LFE bit set (bit 3); bit 12 clear → LFE 1
            // An XLL extension sync word immediately followed by the DTS:X pattern.
            tail: vec![
                0x41, 0xA2, 0x95, 0x47, // XLL extension sync
                0x02, 0x00, 0x08, 0x50, // DTS:X pattern
                0x00, 0x00, 0x00, 0x00,
            ],
            ..Hd::default()
        };
        let mut hd_buf = buf(&hd.build());
        scan(&mut s, &mut hd_buf, 0, &mut tag);
        assert_eq!(s.channel_count, 7);
        assert_eq!(s.lfe, 1);
        assert_eq!(s.bit_depth, 24);
        assert!(s.has_extensions);
        assert_eq!(s.codec_short_name(), "DTS:X MA");
        assert_eq!(s.codec_name(), "DTS:X Master Audio");
        assert_eq!(
            s.description(),
            "7.1 / 48 kHz / 24-bit (DTS Core: 5.1 / 48 kHz /  1536 kbps / 24-bit)"
        );
    }

    #[test]
    fn dts_x_imax_syncword_raises_has_extensions() {
        // DTS:X IMAX streams carry the IMAX extension word `0xF14000D0` instead of the
        // legacy `0x02000850`, so bdinfo must flag them DTS:X too. An XLL
        // sub-sync arms the inner scan, then the IMAX word fires it — with NO legacy
        // word present, so this also pins the IMAX arm in isolation.
        let hd = Hd {
            tail: vec![
                0x41, 0xA2, 0x95, 0x47, // XLL extension sync (arms the inner scan)
                0xF1, 0x40, 0x00, 0xD0, // DTS:X IMAX pattern (even, low bit clear)
                0x00, 0x00, 0x00, 0x00,
            ],
            ..Hd::default()
        };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert!(s.has_extensions);
        assert_eq!(s.codec_short_name(), "DTS:X MA");
        assert_eq!(s.codec_name(), "DTS:X Master Audio");
    }

    #[test]
    fn dts_x_imax_syncword_low_bit_is_ignored() {
        // FFmpeg matches the IMAX word with its low bit ignored (`>> 1`), so the odd
        // value `0xF14000D1` counts as DTS:X IMAX too — pinning the second IMAX arm in
        // isolation (no legacy and no even-IMAX word present).
        let hd = Hd {
            tail: vec![
                0x41, 0xA2, 0x95, 0x47, // XLL extension sync
                0xF1, 0x40, 0x00, 0xD1, // DTS:X IMAX pattern (odd, low bit set)
                0x00, 0x00, 0x00, 0x00,
            ],
            ..Hd::default()
        };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert!(s.has_extensions);
        assert_eq!(s.codec_short_name(), "DTS:X MA");
    }

    #[test]
    fn core_path_creates_and_scans_then_skips_when_initialized() {
        // First CORE unit: creates and initializes the core.
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        let mut tag = None;
        let mut b = buf(&dts_core(0));
        scan(&mut s, &mut b, 0, &mut tag);
        assert_eq!(tag.as_deref(), Some("CORE"));
        let initialized = s.core_stream.as_deref().is_some_and(|c| c.base.is_initialized);
        assert!(initialized);
        assert_eq!(s.core_stream.as_deref().map(|c| c.bit_depth), Some(16));

        // A second CORE unit with a *different* core frame must not re-scan it.
        let mut b = buf(&dts_core(6)); // would be 24-bit if re-scanned
        scan(&mut s, &mut b, 0, &mut tag);
        assert_eq!(s.core_stream.as_deref().map(|c| c.bit_depth), Some(16));
    }

    #[test]
    fn core_path_reuses_an_existing_uninitialized_core() {
        // A pre-attached but uninitialized core is reused (not replaced) and scanned.
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        let mut core = TsAudioStream::default();
        core.base.stream_type = TsStreamType::DtsAudio;
        core.dial_norm = -3; // a marker the reused core carries
        s.core_stream = Some(Box::new(core));
        let mut b = buf(&dts_core(6));
        let mut tag = None;
        scan(&mut s, &mut b, 0, &mut tag);
        assert_eq!(s.core_stream.as_deref().map(|c| c.bit_depth), Some(24));
        assert!(s.core_stream.as_deref().is_some_and(|c| c.base.is_initialized));
    }

    #[test]
    fn high_res_and_express_initialize_from_the_measured_bitrate() {
        // DTS-HD High-Res with a measured bitrate and a core → bit_rate = bitrate +
        // the core's bit_rate, CBR, initialized.
        let mut s = stream(TsStreamType::DtsHdAudio);
        let mut tag = None;
        let mut core_buf = buf(&dts_core(0));
        scan(&mut s, &mut core_buf, 0, &mut tag); // core bit_rate 1536000
        let mut hd_buf = buf(&Hd::default().build());
        scan(&mut s, &mut hd_buf, 500_000, &mut tag);
        assert!(!s.base.is_vbr);
        assert_eq!(s.base.bit_rate, 500_000 + 1_536_000);
        assert!(s.base.is_initialized);
        assert_eq!(s.codec_short_name(), "DTS-HD HR");

        // DTS Express (secondary) with a measured bitrate but no core → just the rate.
        let (sec, _) = run(TsStreamType::DtsHdSecondaryAudio, &Hd::default().build(), 192_000);
        assert!(sec.base.is_initialized);
        assert_eq!(sec.base.bit_rate, 192_000);
        assert_eq!(sec.codec_short_name(), "DTS Express");
    }

    #[test]
    fn non_master_without_a_measured_bitrate_is_not_initialized() {
        // High-Res / Express with bitrate 0 → neither final branch runs → uninitialized.
        let (hr, _) = run(TsStreamType::DtsHdAudio, &Hd::default().build(), 0);
        assert!(!hr.base.is_initialized);
        let (ex, _) = run(TsStreamType::DtsHdSecondaryAudio, &Hd::default().build(), 0);
        assert!(!ex.base.is_initialized);
    }

    #[test]
    fn blown_up_header_widens_the_skips_and_asset_sizes() {
        // The blown-up header path (32-bit header skip, 20-bit asset sizes) still
        // decodes the same fields.
        let hd = Hd { blown_up: true, ..Hd::default() };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.channel_count, 5);
        assert_eq!(s.bit_depth, 16);
    }

    #[test]
    fn static_fields_absent_leaves_asset_fields_unset() {
        // Without static fields the asset header is not parsed, so sample_rate /
        // channel_count / bit_depth stay default; MA still initializes VBR.
        let hd = Hd { static_present: false, ..Hd::default() };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.sample_rate, 0);
        assert_eq!(s.channel_count, 0);
        assert_eq!(s.bit_depth, 0);
        assert!(s.base.is_vbr);
        assert!(s.base.is_initialized);
    }

    #[test]
    fn static_optional_blocks_are_consumed() {
        // Drive the static-field optional sub-blocks: the 36-bit skip, a multi-asset
        // active-mask, the mix-config block, and the per-asset skip4 / skip24 / info
        // blocks — the fields must still align to 5.1 / 48 kHz / 16-bit.
        let hd = Hd {
            nu_sub_stream_index: 2, // width 3 → j-loop covers even & odd j
            static_skip36: true,
            num_audio_present_code: 1, // 2 presentations
            mix_present: true,
            mix_bits_code: 2,    // mix mask width 12
            mix_configs_code: 1, // 2 mix configs
            asset_skip4: true,
            asset_skip24: true,
            info_present: true,
            info_size_code: 3, // 4 info-text bytes
            ..Hd::default()
        };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.channel_count, 5);
        assert_eq!(s.bit_depth, 16);
        assert_eq!(s.lfe, 1);
    }

    #[test]
    fn speaker_mask_drives_lfe_and_channel_count() {
        // No LFE bits → LFE 0, channel_count = total.
        let hd = Hd { total_num_chs_code: 5, spkr_mask: 0x0, ..Hd::default() };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.lfe, 0);
        assert_eq!(s.channel_count, 6);

        // Both LFE bits (0x8 and 0x1000) set → LFE 2 (needs a 16-bit mask).
        let hd = Hd {
            total_num_chs_code: 7, // total 8
            sa_bits_code: 3,       // 16-bit mask
            spkr_mask: 0x1008,     // bits 3 and 12
            ..Hd::default()
        };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.lfe, 2);
        assert_eq!(s.channel_count, 6); // 8 - 2

        // Only the 0x1000 bit → LFE 1.
        let hd = Hd { total_num_chs_code: 7, sa_bits_code: 3, spkr_mask: 0x1000, ..Hd::default() };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.lfe, 1);
    }

    #[test]
    fn small_channel_counts_skip_the_extra_position_bits() {
        // total ≤ 2 → neither extra position bit; the mask still aligns (LFE 1).
        let hd = Hd { total_num_chs_code: 1, spkr_mask: 0x8, ..Hd::default() };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.channel_count, 1); // 2 - 1
        assert_eq!(s.lfe, 1);

        // 2 < total ≤ 6 → one extra position bit only.
        let hd = Hd { total_num_chs_code: 3, spkr_mask: 0x8, ..Hd::default() };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.channel_count, 3); // 4 - 1
        assert_eq!(s.lfe, 1);
    }

    #[test]
    fn speaker_block_absent_leaves_mask_zero() {
        // spkr present but no SA-mask sub-block → mask stays 0 → LFE 0.
        let hd = Hd { sa_present: false, total_num_chs_code: 5, ..Hd::default() };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.lfe, 0);
        assert_eq!(s.channel_count, 6);

        // spkr block entirely absent → mask 0 likewise.
        let hd = Hd { spkr_present: false, total_num_chs_code: 5, ..Hd::default() };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.lfe, 0);
        assert_eq!(s.channel_count, 6);
    }

    #[test]
    fn sample_rate_table_is_indexed_by_the_code() {
        let rate = |code| {
            let hd = Hd { max_sample_rate: code, ..Hd::default() };
            run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0).0.sample_rate
        };
        assert_eq!(rate(0), 8_000);
        assert_eq!(rate(6), 44_100);
        assert_eq!(rate(12), 48_000);
        assert_eq!(rate(15), 384_000);
    }

    #[test]
    fn extension_sync_without_the_pattern_leaves_no_extension() {
        // An XBR sync word is matched (entering the inner scan) but no DTS:X pattern
        // follows → has_extensions stays false. This pins the inner
        // `matches!(temp3, …)` test: a flipped comparison would flag the first
        // non-pattern byte.
        let hd = Hd {
            tail: vec![
                0x65, 0x5E, 0x31, 0x5E, // XBR extension sync (matched)
                0x11, 0x11, 0x11, 0x11, // no DTS:X pattern here
                0x00, 0x00, 0x00, 0x00,
            ],
            ..Hd::default()
        };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert!(!s.has_extensions);
    }

    #[test]
    fn multi_asset_parses_only_the_first_asset() {
        // nuNumAssets = 2 with a *different* second asset (2.0 / 24-bit). The single
        // pass must keep the first asset's values (5.1 / 16-bit) — an accidental
        // second pass would surface the second asset's values.
        let hd = Hd {
            num_assets_code: 1,       // 2 assets
            second_asset_chs_code: 1, // would be total 2 if (wrongly) parsed
            ..Hd::default()
        };
        let (s, _) = run(TsStreamType::DtsHdMasterAudio, &hd.build(), 0);
        assert_eq!(s.channel_count, 5);
        assert_eq!(s.bit_depth, 16);
    }

    #[test]
    fn core_extended_5ch_propagates_the_extended_mode() {
        // A core in DTS-ES (Extended) mode with an HD channel_count of exactly 5
        // propagates Extended to the HD stream.
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        let mut core = TsAudioStream::default();
        core.base.stream_type = TsStreamType::DtsAudio;
        core.base.is_initialized = false;
        core.audio_mode = TsAudioMode::Extended;
        s.core_stream = Some(Box::new(core));
        let hd = Hd { total_num_chs_code: 5, spkr_mask: 0x8, ..Hd::default() }; // 6 - 1 = 5
        let mut hd_buf = buf(&hd.build());
        let mut tag = None;
        scan(&mut s, &mut hd_buf, 0, &mut tag);
        assert_eq!(s.channel_count, 5);
        assert_eq!(s.audio_mode, TsAudioMode::Extended);

        // The same core but channel_count != 5 → no propagation.
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        let mut core = TsAudioStream::default();
        core.base.stream_type = TsStreamType::DtsAudio;
        core.audio_mode = TsAudioMode::Extended;
        s.core_stream = Some(Box::new(core));
        let hd = Hd { total_num_chs_code: 7, spkr_mask: 0x8, ..Hd::default() }; // 8 - 1 = 7
        let mut hd_buf = buf(&hd.build());
        scan(&mut s, &mut hd_buf, 0, &mut tag);
        assert_eq!(s.audio_mode, TsAudioMode::Unknown);

        // A non-Extended core with channel_count 5 → no propagation either.
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        let mut core = TsAudioStream::default();
        core.base.stream_type = TsStreamType::DtsAudio;
        s.core_stream = Some(Box::new(core));
        let hd = Hd { total_num_chs_code: 5, spkr_mask: 0x8, ..Hd::default() };
        let mut hd_buf = buf(&hd.build());
        scan(&mut s, &mut hd_buf, 0, &mut tag);
        assert_eq!(s.audio_mode, TsAudioMode::Unknown);
    }

    #[test]
    fn already_initialized_streams_short_circuit() {
        // Secondary: initialized → returns immediately (no re-parse).
        let mut s = stream(TsStreamType::DtsHdSecondaryAudio);
        s.base.is_initialized = true;
        s.channel_count = 99;
        let mut b = buf(&Hd::default().build());
        scan(&mut s, &mut b, 192_000, &mut None);
        assert_eq!(s.channel_count, 99);

        // Master with an initialized core → returns immediately.
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        s.base.is_initialized = true;
        s.channel_count = 88;
        let mut core = TsAudioStream::default();
        core.base.stream_type = TsStreamType::DtsAudio;
        core.base.is_initialized = true;
        s.core_stream = Some(Box::new(core));
        let mut b = buf(&Hd::default().build());
        scan(&mut s, &mut b, 0, &mut None);
        assert_eq!(s.channel_count, 88);
    }

    #[test]
    fn initialized_master_without_an_initialized_core_still_re_parses() {
        // is_initialized but the core is not initialized (and not secondary) → the
        // guard does not fire, so the HD frame is parsed again.
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        s.base.is_initialized = true;
        let mut core = TsAudioStream::default();
        core.base.stream_type = TsStreamType::DtsAudio;
        core.base.is_initialized = false;
        s.core_stream = Some(Box::new(core));
        let mut b = buf(&Hd::default().build());
        scan(&mut s, &mut b, 0, &mut None);
        assert_eq!(s.channel_count, 5); // re-parsed

        // is_initialized, not secondary, no core at all → also re-parses.
        let mut s = stream(TsStreamType::DtsHdMasterAudio);
        s.base.is_initialized = true;
        let mut b = buf(&Hd::default().build());
        scan(&mut s, &mut b, 0, &mut None);
        assert_eq!(s.channel_count, 5);
    }

    #[test]
    fn missing_hd_sync_with_no_dts_core_creates_an_uninitialized_core() {
        // Neither the HD nor the DTS sync present → CORE path creates the core but the
        // DTS scan finds no sync, leaving it uninitialized.
        let (s, tag) = run(TsStreamType::DtsHdMasterAudio, &[0x11, 0x22, 0x33, 0x44, 0x55], 0);
        assert_eq!(tag.as_deref(), Some("CORE"));
        assert!(s.core_stream.is_some());
        assert!(!s.core_stream.as_deref().is_some_and(|c| c.base.is_initialized));
        assert!(!s.base.is_initialized);
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>(), bitrate in any::<i64>()) {
            for ty in [
                TsStreamType::DtsHdMasterAudio,
                TsStreamType::DtsHdAudio,
                TsStreamType::DtsHdSecondaryAudio,
            ] {
                let mut s = stream(ty);
                let mut b = buf(&data);
                let mut tag = None;
                scan(&mut s, &mut b, bitrate, &mut tag);
            }
        }
    }
}
