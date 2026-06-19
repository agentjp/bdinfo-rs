//! MPEG-H HEVC (H.265) video codec scanner — the headline HDR lane.
//!
//! [`scan`] runs a byte-at-a-time
//! NAL start-code search over one assembled access-unit buffer, parses the video /
//! sequence / picture parameter sets and the HDR supplemental-enhancement-information
//! messages, and fills the [`TsVideoStream`] `encoding_profile`
//! (`"Main 10 @ Level 5.1 @ High"`) plus the `extended_data`
//! ([`HevcExtendedData`]) whose `extended_format_info` list the report joins into the
//! `desc` field — chroma format, bit depth, the gated HDR label
//! (`HDR10` / `HDR10+` / `Dolby Vision`), colour range / primaries / transfer /
//! matrix, ST 2086 mastering display, and `MaxCLL` / `MaxFALL`. Resolution and
//! frame rate come from the CLPI/MPLS metadata.
//!
//! HDR detection is one gate: 10-bit +
//! 4:2:0 + signalled colour description with BT.2020 primaries (9) + PQ transfer
//! (16) + BT.2020 matrix (9/10) + a present mastering display ⇒ the label is
//! `Dolby Vision` when the stream `PID >= 4117`, else `HDR10+` when an ST 2094-40
//! ITU-T T.35 message was seen (`is_hdr10_plus`), else `HDR10`. The
//! mastering-display colour reading carries a known caveat — TODO: the colour
//! reading is sometimes off.
//!
//! The HDR SEI messages (mastering display, content light level, ST 2094-40
//! T.35) are *sparse* on some streams — they can recur only every few seconds
//! rather than in the first access unit that establishes the SPS — so the scan
//! keeps collecting them on every access unit (not just before initialisation)
//! and reassembles the `extended_format_info` from the accumulated state each
//! time. The reassembly clears and rebuilds the fragment list, so a metadata
//! fragment carried by hundreds of access units still appears exactly once (the
//! lineage's per-occurrence append is the bug behind its 30×-repeated lines).
//!
//! Because the outer loop repositions the cursor back to each NAL's start after
//! parsing it and the SEI dispatcher realigns to each message's declared end,
//! every sub-parse whose only effect is to advance the cursor is **output-dead**
//! and skipped wholesale — the VPS body past its id, the PPS body past
//! `num_extra_slice_header_bits`, the access-unit delimiter, the VUI past the
//! colour description, and the HRD / buffering-period / picture-timing /
//! recovery-point / active-parameter-sets / alternative-transfer SEI handlers
//! (which fall through to the default `payload_size` skip). The
//! `general_*_source/constraint` flags feed nothing the report emits and collapse
//! into a bit-skip. All fixed-width codec bit math uses `wrapping_*` so hostile
//! values cannot overflow; parameter-set ids are bounded to the HEVC spec ranges
//! so a malformed length can never over-allocate or panic.

use crate::bitstream::TsStreamBuffer;
use crate::primitives::Pid;
use crate::stream::TsVideoStream;

/// A mastering-display colour-volume table row — an ISO colour `code`
/// and its eight reference chromaticity coordinates (G, B, R, white-point x/y
/// pairs, in units of 1/50000).
struct MasteringDisplayColorVolumeValue {
    /// ISO colour primaries code returned when the SEI values match this row.
    code: u8,
    /// Reference G, B, R, white-point coordinates: `[Gx, Gy, Bx, By, Rx, Ry, Wx, Wy]`.
    values: [u16; 8],
}

/// The four recognised mastering-display colour volumes:
/// BT.709, BT.2020, DCI P3, Display P3.
const MASTERING_DISPLAY_COLOR_VOLUME_VALUES: [MasteringDisplayColorVolumeValue; 4] = [
    MasteringDisplayColorVolumeValue {
        code: 1,
        values: [15000, 30000, 7500, 3000, 32000, 16500, 15635, 16450],
    }, // BT.709
    MasteringDisplayColorVolumeValue {
        code: 9,
        values: [8500, 39850, 6550, 2300, 35400, 14600, 15635, 16450],
    }, // BT.2020
    MasteringDisplayColorVolumeValue {
        code: 11,
        values: [13250, 34500, 7500, 3000, 34000, 16000, 15700, 17550],
    }, // DCI P3
    MasteringDisplayColorVolumeValue {
        code: 12,
        values: [13250, 34500, 7500, 3000, 34000, 16000, 15635, 16450],
    }, // Display P3
];

/// The VUI colour description fields read from a sequence parameter set — the
/// subset that feeds the `desc` (everything past the colour description is
/// output-dead; see the module notes).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct VuiColour {
    /// `video_signal_type_present_flag`.
    video_signal_type_present_flag: bool,
    /// `colour_description_present_flag`.
    colour_description_present_flag: bool,
    /// `video_full_range_flag`.
    video_full_range_flag: u8,
    /// `colour_primaries` (2 = unspecified, the default).
    colour_primaries: u8,
    /// `transfer_characteristics` (2 = unspecified).
    transfer_characteristics: u8,
    /// `matrix_coeffs` (2 = unspecified).
    matrix_coefficients: u8,
}

impl VuiColour {
    /// The field defaults — `colour_primaries`,
    /// `transfer_characteristics`, and `matrix_coefficients` start at `2`
    /// (unspecified), the others at zero/false.
    const fn unspecified() -> Self {
        Self {
            video_signal_type_present_flag: false,
            colour_description_present_flag: false,
            video_full_range_flag: 0,
            colour_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
        }
    }
}

/// A parsed sequence parameter set — the subset of the SPS
/// that feeds observable output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SeqParameterSet {
    /// `general_profile_space`.
    profile_space: u32,
    /// `general_tier_flag`.
    tier_flag: bool,
    /// `general_profile_idc`.
    profile_idc: u16,
    /// `general_level_idc`.
    level_idc: u16,
    /// `chroma_format_idc` (1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4).
    chroma_format_idc: u32,
    /// `bit_depth_luma_minus8`.
    bit_depth_luma_minus8: u8,
    /// `bit_depth_chroma_minus8`.
    bit_depth_chroma_minus8: u8,
    /// The VUI colour description.
    vui: VuiColour,
}

/// A parsed picture parameter set — the subset of the PPS
/// the slice-segment header reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PicParameterSet {
    /// `num_extra_slice_header_bits`.
    num_extra_slice_header_bits: u8,
    /// `dependent_slice_segments_enabled_flag`.
    dependent_slice_segments_enabled_flag: bool,
}

/// `general_profile_space` / `general_tier_flag` / `general_profile_idc` /
/// `general_level_idc` — the profile-tier-level outputs the SPS records.
#[derive(Debug, Clone, Copy, Default)]
struct ProfileTierLevel {
    /// `general_profile_space`.
    profile_space: u32,
    /// `general_tier_flag`.
    tier_flag: bool,
    /// `general_profile_idc`.
    profile_idc: u16,
    /// `general_level_idc`.
    level_idc: u16,
}

/// Persistent HEVC analysis state carried on a [`TsVideoStream`] across access
/// units.
///
/// Populated by [`scan`]; its
/// [`extended_format_info`](Self::extended_format_info) is what the report joins
/// into the HEVC `desc`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HevcExtendedData {
    /// Number of registered video parameter sets — only existence/count is read
    /// (the bodies are output-dead), so the list collapses to a count.
    video_param_set_count: usize,
    /// Registered sequence parameter sets (index = `sps_seq_parameter_set_id`).
    seq_parameter_sets: Vec<SeqParameterSet>,
    /// Registered picture parameter sets (index = `pps_pic_parameter_set_id`).
    pic_parameter_sets: Vec<PicParameterSet>,
    /// The ST 2086 mastering-display colour-primaries string.
    mastering_display_color_primaries: String,
    /// The ST 2086 mastering-display luminance string.
    mastering_display_luminance: String,
    /// `MaximumContentLightLevel` (`MaxCLL`).
    maximum_content_light_level: u32,
    /// `MaximumFrameAverageLightLevel` (`MaxFALL`).
    maximum_frame_average_light_level: u32,
    /// Whether a `MaxCLL`/`MaxFALL` SEI was seen.
    light_level_available: bool,
    /// Whether an ST 2094-40 HDR10+ ITU-T T.35 SEI was seen.
    is_hdr10_plus: bool,
    /// The four-decimal spelling of the ST 2086 luminance (both bounds `F4`)
    /// the exact fragments substitute for
    /// [`mastering_display_luminance`](Self::mastering_display_luminance).
    mastering_display_luminance_exact: String,
    /// The `desc` extension fragments, joined by `" / "`.
    extended_format_info: Vec<String>,
}

impl HevcExtendedData {
    /// The extension fragments the report joins (with `" / "`) onto the
    /// HEVC stream `desc`.
    #[must_use]
    pub(crate) fn extended_format_info(&self) -> &[String] {
        &self.extended_format_info
    }

    /// The extension fragments with the ST 2086 luminance respelled to its
    /// exact four-decimal form — the full-description variant.
    #[must_use]
    pub(crate) fn extended_format_info_exact(&self) -> Vec<String> {
        self.extended_format_info
            .iter()
            .map(|entry| {
                if entry.starts_with("Mastering display luminance: ")
                    && !self.mastering_display_luminance_exact.is_empty()
                {
                    format!(
                        "Mastering display luminance: {}",
                        self.mastering_display_luminance_exact
                    )
                } else {
                    entry.clone()
                }
            })
            .collect()
    }
}

/// Maps `colour_primaries` to its display string.
const fn colour_primaries(colour_primaries: u8) -> &'static str {
    match colour_primaries {
        1 => "BT.709",
        4 => "BT.470 System M",
        5 => "BT.601 PAL",
        6 => "BT.601 NTSC",
        7 => "SMPTE 240M",
        8 => "Generic film",
        9 => "BT.2020",
        10 => "XYZ",
        11 => "DCI P3",
        12 => "Display P3",
        22 => "EBU Tech 3213",
        _ => "",
    }
}

/// Maps `transfer_characteristics` to its display string.
const fn transfer_characteristics(transfer_characteristics: u8) -> &'static str {
    match transfer_characteristics {
        1 => "BT.709",
        4 => "BT.470 System M",
        5 => "BT.470 System B/G",
        6 => "BT.601",
        7 => "SMPTE 240M",
        8 => "Linear",
        9 => "Logarithmic (100:1)",
        10 => "Logarithmic (316.22777:1)",
        11 => "xvYCC",
        12 => "BT.1361",
        13 => "sRGB/sYCC",
        14 => "BT.2020 (10-bit)",
        15 => "BT.2020 (12-bit)",
        16 => "PQ",
        17 => "SMPTE 428M",
        18 => "HLG",
        _ => "",
    }
}

/// Maps `matrix_coeffs` to its display string.
const fn matrix_coefficients(matrix_coefficients: u8) -> &'static str {
    match matrix_coefficients {
        0 => "Identity",
        1 => "BT.709",
        4 => "FCC 73.682",
        5 => "BT.470 System B/G",
        6 => "BT.601",
        7 => "SMPTE 240M",
        8 => "YCgCo",
        9 => "BT.2020 non-constant",
        10 => "BT.2020 constant",
        11 => "Y'D'zD'x",
        12 => "Chromaticity-derived non-constant",
        13 => "Chromaticity-derived constant",
        14 => "ICtCp",
        _ => "",
    }
}

/// HEVC reference-picture-set count ceilings (the spec maxima from H.265
/// §7.4.3.2.1). The SPS declares each as an `ue(v)`, which a *malformed* stream
/// can decode astronomically large (the leading-zero count saturates the reader's
/// wrapping `u8`, so `1 << (n & 31)` reaches ~2^31); looping on the raw value
/// would never terminate on hostile bytes — a hang the fuzz tier caught.
/// Clamping each count to its spec ceiling bounds every ref-pic-set loop
/// by construction — output-neutral, since a spec-conformant SPS never exceeds
/// these (the clamp is a no-op there), and DoS-proof, since the loops can no
/// longer run more than a few dozen times.
const MAX_SHORT_TERM_REF_PIC_SETS: u32 = 64; // num_short_term_ref_pic_sets ∈ [0, 64]
const MAX_DELTA_POCS_PER_SET: u32 = 16; // NumNegativePics / NumPositivePics ≤ MaxDpbSize ≤ 16
const MAX_LONG_TERM_REF_PICS: u32 = 32; // num_long_term_ref_pics_sps ∈ [0, 32]

/// The signed delta of two byte cursors — what the NAL/SEI boundary
/// scans hand to [`TsStreamBuffer::bs_skip_bytes`] to rewind. Cursors stay within
/// the 5 MiB buffer cap, so the conversion is total for all real input.
fn delta_bytes(a: u64, b: u64) -> i32 {
    let a = i64::try_from(a).unwrap_or(i64::MAX);
    let b = i64::try_from(b).unwrap_or(i64::MAX);
    i32::try_from(a.wrapping_sub(b)).unwrap_or(i32::MAX)
}

/// Scans one HEVC access unit from `buffer` into `stream`.
///
/// Sets `tag` to the slice picture type (`I`/`P`/`B`), fills
/// `stream.encoding_profile` and `stream.extended_data` from the first sequence
/// parameter set and the HDR SEI messages, and marks the stream
/// `is_vbr`/`is_initialized`. A buffer with no HEVC NAL units leaves `stream`
/// (apart from `is_vbr`) untouched. The parameter sets are parsed only until the
/// stream initialises, but the HDR SEIs keep accumulating on every access unit so
/// a sparse mastering / light-level / ST 2094-40 message that lands after the
/// first access unit is still folded into the format info.
pub fn scan(stream: &mut TsVideoStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    let is_initialized = stream.base.is_initialized;
    let mut ext = stream.extended_data.take().unwrap_or_default();

    Hevc { ext: &mut ext, buffer, is_initialized }.run(tag);

    finalize(stream, &mut ext, is_initialized);
    stream.extended_data = Some(ext);
}

/// The parameter-set / SEI scanner over one buffer — bundles the persistent
/// [`HevcExtendedData`], the bit reader, and the entry-time `is_initialized`
/// flag, which short-circuits the parameter-set parses once the stream is
/// analysed (the HDR SEIs keep being collected so sparse post-init messages
/// still register).
struct Hevc<'a, 'b> {
    /// Persistent analysis state loaded from / stored back to the stream.
    ext: &'a mut HevcExtendedData,
    /// The access-unit bit reader.
    buffer: &'b mut TsStreamBuffer,
    /// Entry-time `stream.base.is_initialized`.
    is_initialized: bool,
}

/// The four-byte (`00 00 00 01`) / three-byte (`00 00 01`) NAL start code outcome
/// of one [`Hevc::find_start_code`] step.
enum StartCode {
    /// A start code was found; the cursor sits on the NAL header.
    Found,
    /// No start code before the end-of-search; scanning is done.
    NotFound,
}

impl Hevc<'_, '_> {
    /// Runs the NAL loop: find a start code, read the NAL unit
    /// type, dispatch to the matching parser, then reposition to the NAL start.
    fn run(&mut self, tag: &mut Option<String>) {
        let mut frame_type_read = false;
        while self.scan_should_continue(frame_type_read) {
            let StartCode::Found = self.find_start_code(frame_type_read) else {
                break;
            };
            if self.buffer.position() >= self.buffer.length() {
                break;
            }
            let last_stream_pos = self.buffer.position();

            self.buffer.bs_skip_bits(1, true); // forbidden_zero_bit
            let nal_unit_type = self.buffer.read_bits2(6, true);
            self.buffer.bs_skip_bits(9, true); // nuh_layer_id, nuh_temporal_id_plus1

            match nal_unit_type {
                0..=9 | 16..=21 => {
                    *tag = self.slice_segment_layer(nal_unit_type);
                    frame_type_read = tag.is_some();
                }
                32 => self.video_parameter_set(),
                33 => self.seq_parameter_set(),
                34 => self.pic_parameter_set(),
                39 | 40 => self.sei(),
                // 35 (access-unit delimiter) and all other types: output-dead.
                _ => {}
            }

            self.buffer.bs_skip_next_byte();
            let back = delta_bytes(last_stream_pos, self.buffer.position());
            self.buffer.bs_skip_bytes(back, true);
        }
    }

    /// The shared outer/inner loop guard — keep scanning while there are at least
    /// three bytes left and the stream is not yet a finished, frame-typed unit.
    fn scan_should_continue(&self, frame_type_read: bool) -> bool {
        self.buffer.position() < self.buffer.length().saturating_sub(3)
            && (!self.is_initialized || !frame_type_read)
    }

    /// Probes for a NAL start code at the cursor — a byte-rewind probe,
    /// shared by the main scan and the SEI boundary scan. Returns `Some(4)` for a
    /// four-byte `00 00 00 01` or `Some(3)` for a three-byte `00 00 01` (cursor left
    /// just past it), or `None` after advancing one byte past the probe start to
    /// resync.
    fn probe_start_code(&mut self) -> Option<u64> {
        let stream_pos = self.buffer.position();
        if self.buffer.read_byte(false) == 0x0
            && self.buffer.read_byte(false) == 0x0
            && self.buffer.read_byte(false) == 0x0
            && self.buffer.read_byte(false) == 0x1
        {
            return Some(4);
        }
        self.buffer.bs_skip_bytes(delta_bytes(stream_pos, self.buffer.position()), false);

        if self.buffer.read_byte(false) == 0x0
            && self.buffer.read_byte(false) == 0x0
            && self.buffer.read_byte(false) == 0x1
        {
            return Some(3);
        }
        let resync = delta_bytes(stream_pos, self.buffer.position()).wrapping_add(1);
        self.buffer.bs_skip_bytes(resync, false);
        None
    }

    /// The inner start-code search — advances to just past the next `00 00 00 01`
    /// or `00 00 01`, giving up once there is no more to scan.
    fn find_start_code(&mut self, frame_type_read: bool) -> StartCode {
        loop {
            if self.probe_start_code().is_some() {
                return StartCode::Found;
            }
            if !self.scan_should_continue(frame_type_read) {
                return StartCode::NotFound;
            }
        }
    }

    /// Reads a slice-segment header far enough to recover the picture type.
    /// Returns the `I`/`P`/`B` tag, or `None`.
    fn slice_segment_layer(&mut self, nal_unit_type: u16) -> Option<String> {
        let first_slice_segment_in_pic_flag = self.buffer.read_bool(false);

        if (16..=23).contains(&nal_unit_type) {
            let _ = self.buffer.read_bool(false); // no_output_of_prior_pics_flag
        }

        let slice_pic_parameter_set_id = self.buffer.read_exp(true);
        let pps = usize::try_from(slice_pic_parameter_set_id)
            .ok()
            .and_then(|i| self.ext.pic_parameter_sets.get(i))?;
        let (dependent_enabled, num_extra_bits) =
            (pps.dependent_slice_segments_enabled_flag, pps.num_extra_slice_header_bits);

        if !first_slice_segment_in_pic_flag {
            if dependent_enabled {
                let _ = self.buffer.read_bool(true); // dependent_slice_segment_flag
            }
            return None;
        }

        self.buffer.bs_skip_bits(usize::from(num_extra_bits), false);
        match self.buffer.read_exp(true) {
            0 => Some("P".to_owned()),
            1 => Some("B".to_owned()),
            2 => Some("I".to_owned()),
            _ => None,
        }
    }

    /// Registers a video parameter set (packet 32).
    /// Only the id is recorded; the rest of the VPS is output-dead.
    fn video_parameter_set(&mut self) {
        if self.is_initialized {
            return;
        }
        let vps_id = usize::from(self.buffer.read_bits2(4, true));
        self.ext.video_param_set_count =
            self.ext.video_param_set_count.max(vps_id.saturating_add(1));
    }

    /// Parses a sequence parameter set through its VUI colour description
    /// (packet 33). The cursor position afterward is
    /// irrelevant (the NAL loop repositions), so parsing stops after the colour
    /// fields.
    fn seq_parameter_set(&mut self) {
        if self.is_initialized {
            return;
        }
        let video_parameter_set_id = usize::from(self.buffer.read_bits2(4, true));
        if video_parameter_set_id >= self.ext.video_param_set_count {
            return;
        }

        let max_sub_layers_minus1 = self.buffer.read_bits2(3, true);
        self.buffer.bs_skip_bits(1, true); // sps_temporal_id_nesting_flag
        let ptl = self.profile_tier_level(max_sub_layers_minus1);
        let sps_seq_parameter_set_id = self.buffer.read_exp(true);
        let chroma_format_idc = self.buffer.read_exp(true);
        if chroma_format_idc >= 4 {
            return;
        }
        if chroma_format_idc == 3 {
            self.buffer.bs_skip_bits(1, true); // separate_colour_plane_flag
        }
        self.buffer.skip_exp(true); // pic_width_in_luma_samples
        self.buffer.skip_exp(true); // pic_height_in_luma_samples
        if self.buffer.read_bool(true) {
            // conformance_window_flag
            self.buffer.skip_exp_multi(4, true); // conf_win l/r/t/b offsets
        }
        let bit_depth_luma_minus8 = self.buffer.read_exp(true);
        if bit_depth_luma_minus8 > 6 {
            return;
        }
        let bit_depth_chroma_minus8 = self.buffer.read_exp(true);
        if bit_depth_chroma_minus8 > 6 {
            return;
        }
        let log2_max_pic_order_cnt_lsb_minus4 = self.buffer.read_exp(true);
        if log2_max_pic_order_cnt_lsb_minus4 > 12 {
            return;
        }
        let sps_sub_layer_ordering_info_present_flag = self.buffer.read_bool(true);
        let first =
            if sps_sub_layer_ordering_info_present_flag { 0 } else { max_sub_layers_minus1 };
        let mut sub_layer_pos = first;
        while sub_layer_pos <= max_sub_layers_minus1 {
            self.buffer.skip_exp_multi(3, true); // max_dec_pic_buffering / reorder / latency
            sub_layer_pos = sub_layer_pos.wrapping_add(1);
        }
        self.buffer.skip_exp_multi(6, true); // log2 min/diff CB & TB sizes, max transform depths

        if self.buffer.read_bool(true) && self.buffer.read_bool(true) {
            // scaling_list_enabled_flag && sps_scaling_list_data_present_flag
            self.scaling_list_data();
        }
        self.buffer.bs_skip_bits(2, true); // amp_enabled_flag, sample_adaptive_offset_enabled_flag
        if self.buffer.read_bool(true) {
            // pcm_enabled_flag
            self.buffer.bs_skip_bits(8, true); // pcm bit depths, log2 min/diff pcm CB sizes split
            self.buffer.skip_exp_multi(2, true);
            self.buffer.bs_skip_bits(1, true); // pcm_loop_filter_disabled_flag
        }
        let num_short_term_ref_pic_sets =
            self.buffer.read_exp(true).min(MAX_SHORT_TERM_REF_PIC_SETS);
        self.short_term_ref_pic_sets(num_short_term_ref_pic_sets);
        if self.buffer.read_bool(true) {
            // long_term_ref_pics_present_flag
            // `num_long_term_ref_pics_sps` is an unbounded `ue(v)`; clamp a
            // malformed huge count to the spec ceiling.
            let num_long_term_ref_pics_sps = self.buffer.read_exp(true).min(MAX_LONG_TERM_REF_PICS);
            let mut i = 0;
            while i < num_long_term_ref_pics_sps {
                self.buffer.bs_skip_bits(
                    usize::try_from(log2_max_pic_order_cnt_lsb_minus4.wrapping_add(4))
                        .unwrap_or(usize::MAX),
                    true,
                ); // lt_ref_pic_poc_lsb_sps
                self.buffer.bs_skip_bits(1, true); // used_by_curr_pic_lt_sps_flag
                i = i.wrapping_add(1);
            }
        }
        self.buffer.bs_skip_bits(2, true); // sps_temporal_mvp / strong_intra_smoothing
        // vui_parameters_present_flag — read first (side effect), then parse if set.
        let vui = if self.buffer.read_bool(true) {
            self.vui_parameters()
        } else {
            VuiColour::unspecified()
        };

        let sps = SeqParameterSet {
            profile_space: ptl.profile_space,
            tier_flag: ptl.tier_flag,
            profile_idc: ptl.profile_idc,
            level_idc: ptl.level_idc,
            chroma_format_idc,
            bit_depth_luma_minus8: bit_depth_luma_minus8.to_le_bytes()[0],
            bit_depth_chroma_minus8: bit_depth_chroma_minus8.to_le_bytes()[0],
            vui,
        };
        register(&mut self.ext.seq_parameter_sets, sps_seq_parameter_set_id, sps);
    }

    /// Registers a picture parameter set through `num_extra_slice_header_bits`
    /// (packet 34). The tail of the PPS is output-dead.
    fn pic_parameter_set(&mut self) {
        if self.is_initialized {
            return;
        }
        let pps_pic_parameter_set_id = self.buffer.read_exp(true);
        if pps_pic_parameter_set_id >= 64 {
            return;
        }
        let pps_seq_parameter_set_id = self.buffer.read_exp(true);
        if pps_seq_parameter_set_id >= 16 {
            return;
        }
        let valid = usize::try_from(pps_seq_parameter_set_id)
            .is_ok_and(|i| i < self.ext.seq_parameter_sets.len());
        if !valid {
            return;
        }

        let dependent_slice_segments_enabled_flag = self.buffer.read_bool(true);
        self.buffer.bs_skip_bits(1, true); // output_flag_present_flag
        let num_extra_slice_header_bits = self.buffer.read_bits2(3, true).to_le_bytes()[0];

        let pps =
            PicParameterSet { num_extra_slice_header_bits, dependent_slice_segments_enabled_flag };
        register(&mut self.ext.pic_parameter_sets, pps_pic_parameter_set_id, pps);
    }

    /// `profile_tier_level` — reads the general profile space / tier / profile /
    /// level and skips the sub-layer profile/level data.
    fn profile_tier_level(&mut self, sub_layer_count: u16) -> ProfileTierLevel {
        let profile_space = u32::from(self.buffer.read_bits2(2, true));
        let tier_flag = self.buffer.read_bool(true);
        let mut profile_idc = self.buffer.read_bits2(5, true);
        // general_profile_compatibility_flag[0..32] (MSB-first: flag i is bit
        // 31 - i). When the coded profile_idc is 0, FFmpeg recovers it from the
        // lowest set flag at index 1..32 (hevc/ps.c:285-290): flag[i] == 1 means
        // the CVS also conforms to profile i, so the first set flag names the
        // profile. Index 0 maps to profile 0 (a no-op recovery), so the scan can
        // start there without a guard — the first *meaningful* set flag still wins.
        let compatibility_flags = self.buffer.read_bits4(32, true);
        let mut compat_idx: u16 = 0;
        while compat_idx < 32 {
            let bit = 31_u32.wrapping_sub(u32::from(compat_idx));
            let flag = compatibility_flags.wrapping_shr(bit) & 1 != 0;
            if profile_idc == 0 && flag {
                profile_idc = compat_idx;
            }
            compat_idx = compat_idx.wrapping_add(1);
        }
        // general_progressive/interlaced/non_packed/frame_only flags (nothing
        // reads them) plus general_reserved_zero_44bits.
        self.buffer.bs_skip_bits(4, true);
        self.buffer.bs_skip_bits(44, true);
        let level_idc = self.buffer.read_bits2(8, true);

        let mut sub_layer_flags: Vec<(bool, bool)> = Vec::new();
        let mut pos: u16 = 0;
        while pos < sub_layer_count {
            let profile_present = self.buffer.read_bool(true);
            let level_present = self.buffer.read_bool(true);
            sub_layer_flags.push((profile_present, level_present));
            pos = pos.wrapping_add(1);
        }
        if sub_layer_count > 0 {
            // reserved_zero_2bits[i] for i in [sub_layer_count, 8) — (8 - sub_layer_count)
            // two-bit fields, not a single one (H.265 §7.3.3; FFmpeg ps.c:360-362).
            let mut i = sub_layer_count;
            while i < 8 {
                self.buffer.bs_skip_bits(2, true);
                i = i.wrapping_add(1);
            }
        }
        for (profile_present, level_present) in sub_layer_flags {
            if profile_present {
                self.buffer.bs_skip_bits(88, true); // sub_layer profile data
            }
            if level_present {
                self.buffer.bs_skip_bits(8, true); // sub_layer_level_idc
            }
        }

        ProfileTierLevel { profile_space, tier_flag, profile_idc, level_idc }
    }

    /// `short_term_ref_pic_set` loop — skips the short-term reference picture sets
    /// so the SPS cursor lands on the long-term / VUI fields.
    ///
    /// `num_short_term_ref_pic_sets` arrives already clamped to
    /// [`MAX_SHORT_TERM_REF_PIC_SETS`]; the per-set `num_negative_pics` /
    /// `num_positive_pics` are clamped to [`MAX_DELTA_POCS_PER_SET`] here. Those
    /// are unbounded `ue(v)` codes a malformed SPS can decode astronomically large
    /// — looping on the raw values would never terminate on hostile bytes (a hang
    /// the fuzz tier caught); the clamps bound every loop by construction.
    /// Output-neutral: a spec-conformant SPS never exceeds the ceilings, so the
    /// clamps are no-ops and `num_pics` (at most twice [`MAX_DELTA_POCS_PER_SET`])
    /// keeps the inter-pred loop bounded too.
    fn short_term_ref_pic_sets(&mut self, num_short_term_ref_pic_sets: u32) {
        let mut num_pics: u32 = 0;
        let mut st_rps_idx: u32 = 0;
        while st_rps_idx < num_short_term_ref_pic_sets {
            let inter_ref_pic_set_prediction_flag = st_rps_idx > 0 && self.buffer.read_bool(true);

            if inter_ref_pic_set_prediction_flag {
                // delta_idx_minus1 would be present only when st_rps_idx == the set
                // count, which never happens inside this `0..count` loop (the SPS
                // always passes its own count) — so there is nothing to read here.
                self.buffer.bs_skip_bits(1, true); // delta_rps_sign
                self.buffer.skip_exp(true); // abs_delta_rps_minus1
                let mut num_pics_new: u32 = 0;
                let mut pic_pos: u32 = 0;
                // Bounded: `num_pics` came from a prior set's clamped
                // `num_negative_pics + num_positive_pics` (≤ 2·MAX_DELTA_POCS_PER_SET).
                while pic_pos <= num_pics {
                    // used_by_curr_pic_flag, else use_delta_flag (read only if the
                    // first is clear — a short-circuit, no bit otherwise).
                    let used_by_curr_pic = self.buffer.read_bool(true);
                    let use_delta = !used_by_curr_pic && self.buffer.read_bool(true);
                    if used_by_curr_pic || use_delta {
                        num_pics_new = num_pics_new.wrapping_add(1);
                    }
                    pic_pos = pic_pos.wrapping_add(1);
                }
                num_pics = num_pics_new;
            } else {
                let num_negative_pics = self.buffer.read_exp(true).min(MAX_DELTA_POCS_PER_SET);
                let num_positive_pics = self.buffer.read_exp(true).min(MAX_DELTA_POCS_PER_SET);
                num_pics = num_negative_pics.wrapping_add(num_positive_pics);
                let mut i: u32 = 0;
                while i < num_negative_pics {
                    self.buffer.skip_exp(true); // delta_poc_s0_minus1
                    self.buffer.bs_skip_bits(1, true); // used_by_curr_pic_s0_flag
                    i = i.wrapping_add(1);
                }
                let mut i: u32 = 0;
                while i < num_positive_pics {
                    self.buffer.skip_exp(true); // delta_poc_s1_minus1
                    self.buffer.bs_skip_bits(1, true); // used_by_curr_pic_s1_flag
                    i = i.wrapping_add(1);
                }
            }
            st_rps_idx = st_rps_idx.wrapping_add(1);
        }
    }

    /// `scaling_list_data` — skips an explicit scaling-list block.
    fn scaling_list_data(&mut self) {
        let mut size_id: u32 = 0;
        while size_id < 4 {
            let matrix_count: i32 = if size_id == 3 { 2 } else { 6 };
            let mut matrix_id: i32 = 0;
            while matrix_id < matrix_count {
                if self.buffer.read_bool(true) {
                    // scaling_list_pred_mode_flag
                    let shift = 4_u32.wrapping_add(size_id.wrapping_shl(1));
                    let coef_num = 64_i32.min(1_i32.wrapping_shl(shift));
                    if size_id > 1 {
                        self.buffer.skip_exp(true); // scaling_list_dc_coef_minus8
                    }
                    let mut i: i32 = 0;
                    while i < coef_num {
                        self.buffer.skip_exp(true); // scaling_list_delta_coef
                        i = i.wrapping_add(1);
                    }
                } else {
                    self.buffer.skip_exp(true); // scaling_list_pred_matrix_id_delta
                }
                matrix_id = matrix_id.wrapping_add(1);
            }
            size_id = size_id.wrapping_add(1);
        }
    }

    /// `vui_parameters` through the colour description — the rest of the VUI is
    /// output-dead, so parsing stops once the colour fields are read.
    fn vui_parameters(&mut self) -> VuiColour {
        let mut vui = VuiColour::unspecified();
        if self.buffer.read_bool(true) {
            // aspect_ratio_info_present_flag
            let aspect_ratio_idc = self.buffer.read_bits2(8, true);
            if aspect_ratio_idc == 0xFF {
                self.buffer.bs_skip_bits(32, true); // sar_width (16), sar_height (16)
            }
        }
        if self.buffer.read_bool(true) {
            // overscan_info_present_flag
            self.buffer.bs_skip_bits(1, true); // overscan_appropriate_flag
        }
        vui.video_signal_type_present_flag = self.buffer.read_bool(true);
        if vui.video_signal_type_present_flag {
            self.buffer.bs_skip_bits(3, true); // video_format
            vui.video_full_range_flag = self.buffer.read_bits2(1, true).to_le_bytes()[0];
            vui.colour_description_present_flag = self.buffer.read_bool(true);
            if vui.colour_description_present_flag {
                vui.colour_primaries = self.buffer.read_bits2(8, true).to_le_bytes()[0];
                vui.transfer_characteristics = self.buffer.read_bits2(8, true).to_le_bytes()[0];
                vui.matrix_coefficients = self.buffer.read_bits2(8, true).to_le_bytes()[0];
            }
        }
        vui
    }

    /// The SEI element — finds the element boundary, then dispatches each message
    /// to the HDR handlers (packets 39/40). Runs on every access unit, before and
    /// after initialisation, so a sparse HDR SEI that recurs only every few seconds
    /// is still collected (the parameter-set parses, by contrast, stop at init).
    fn sei(&mut self) {
        let element_start = self.buffer.position();
        let mut num_bytes: u64 = 0;

        loop {
            if let Some(n) = self.probe_start_code() {
                num_bytes = n;
                break;
            }
            if self.buffer.position() >= self.buffer.length() {
                break;
            }
        }

        let element_size = self.buffer.position().wrapping_sub(element_start);
        let rewind = delta_bytes(0, element_size); // element_size * -1
        self.buffer.bs_skip_bytes(rewind, false);
        let element_size = element_size.wrapping_sub(num_bytes.wrapping_add(1));
        let element_end = element_start.wrapping_add(element_size);

        loop {
            let payload_type = self.read_sei_extended();
            let payload_size = self.read_sei_extended();

            let saved_pos = self.buffer.position().wrapping_add(payload_size);
            if saved_pos > self.buffer.length() {
                return;
            }

            match payload_type {
                137 => self.sei_mastering_display_colour_volume(),
                144 => self.sei_light_level(),
                4 => self.sei_user_data_registered_itu_t_t35(payload_size),
                // 0/1/6/129/147 and all others skip the payload (landing on saved_pos).
                _ => self.buffer.bs_skip_bytes(sei_bytes(payload_size), true),
            }

            // Realign to the message's declared end if it under-read (skipping the
            // unread remainder). `saturating_sub` skips zero when the message read
            // exactly its payload — or read past it through emulation-prevention
            // bytes — so the cursor is never moved backward.
            let remainder = saved_pos.saturating_sub(self.buffer.position());
            self.buffer.bs_skip_bytes(sei_bytes(remainder), true);

            if self.buffer.position() >= element_end {
                break;
            }
        }
    }

    /// Reads one SEI `payload_type` / `payload_size` value — sums consecutive
    /// `0xFF` continuation bytes plus the final byte.
    fn read_sei_extended(&mut self) -> u64 {
        let mut value: u64 = 0;
        loop {
            let byte = self.buffer.read_byte(true);
            value = value.wrapping_add(u64::from(byte));
            if byte != 0xFF {
                return value;
            }
        }
    }

    /// SEI 137 — `mastering_display_colour_volume` (ST 2086). Reads the display
    /// primaries / white point / luminance, remaps the spec-fixed G, B, R wire
    /// order to RGB, and matches a known colour volume or formats the raw
    /// chromaticities.
    fn sei_mastering_display_colour_volume(&mut self) {
        self.buffer.bs_reset_bits();
        // The three display primaries (x, y) then the white point (x, y) arrive
        // as indices 0..8 in order, so a sequential fill matches the wire layout.
        let mut primaries = [0_u16; 8];
        for slot in &mut primaries {
            *slot = self.buffer.read_bits2(16, true); // display primaries / white point x,y
        }

        let luminance_max = self.buffer.read_bits4(32, true); // max_display_mastering_luminance
        let luminance_min = self.buffer.read_bits4(32, true); // min_display_mastering_luminance

        // H.265 §D.3.28 fixes the wire order at green, blue, red, so the RGB slots
        // are constant — FFmpeg's {2, 0, 1} remap (h2645_sei.c): red is wire[2],
        // green wire[0], blue wire[1]. No value-based guessing.
        let (red, green, blue) = (2_usize, 0_usize, 1_usize);

        let not_valid = primaries.contains(&u16::MAX);
        let matched =
            if not_valid { None } else { match_color_volume(&primaries, green, blue, red) };

        self.ext.mastering_display_color_primaries = matched.map_or_else(
            || format_raw_primaries(&primaries, red, green, blue),
            |code| colour_primaries(code).to_owned(),
        );
        self.ext.mastering_display_luminance = format_luminance(luminance_min, luminance_max);
        self.ext.mastering_display_luminance_exact =
            format_luminance_exact(luminance_min, luminance_max);
    }

    /// SEI 144 — `content_light_level_info` (`MaxCLL` / `MaxFALL`).
    fn sei_light_level(&mut self) {
        self.ext.maximum_content_light_level = u32::from(self.buffer.read_bits2(16, true));
        self.ext.maximum_frame_average_light_level = u32::from(self.buffer.read_bits2(16, true));
        self.ext.light_level_available = true;
    }

    /// SEI 4 — `user_data_registered_itu_t_t35`; detects the ST 2094-40 HDR10+
    /// registration (country `0xB5`, provider `0x003C`, oriented `0x0001`,
    /// application id 4). The registration is identified by that prefix alone;
    /// `application_version` (any value) and `num_windows` (1..3) are payload
    /// contents, not part of the registration identity (`itut35.c` +
    /// `hdr_dynamic_metadata.c`).
    fn sei_user_data_registered_itu_t_t35(&mut self, payload_size: u64) {
        let country_code = self.buffer.read_bits2(8, true);
        let terminal_provider_code = self.buffer.read_bits2(16, true);
        let terminal_provider_oriented_code = self.buffer.read_bits2(16, true);
        let application_id = self.buffer.read_bits4(8, true);
        self.buffer.bs_skip_bits(8, true); // application_version (FFmpeg accepts any value)
        let num_windows = self.buffer.read_bits4(2, true);
        self.buffer.bs_skip_bits(6, true);
        if country_code == 0xB5
            && terminal_provider_code == 0x003C
            && terminal_provider_oriented_code == 0x0001
            && application_id == 4
            && (1..=3).contains(&num_windows)
        {
            self.ext.is_hdr10_plus = true;
        }
        // The remaining skip is computed in 32-bit math on purpose: a declared size
        // < 8 wraps to a large u32 whose signed view is a small *negative* skip — an
        // O(1) backward reposition to the message's declared end. That truncation
        // means a crafted type-4 SEI with `payload_size` 0..7 cannot drive a
        // ~2.1e9-iteration forward skip (a multi-second hang); `sei_bytes` stays
        // saturating for the non-underflowing paths.
        let rest = u32::try_from(payload_size).unwrap_or(u32::MAX).wrapping_sub(8);
        self.buffer.bs_skip_bytes(rest.cast_signed(), true);
    }
}

/// `bs_skip_bytes` argument from a SEI byte count
/// (cursors stay within the buffer cap, so the conversion is total).
fn sei_bytes(bytes: u64) -> i32 {
    i32::try_from(bytes).unwrap_or(i32::MAX)
}

/// Grows `sets` with default placeholders up to `id` (inclusive) and stores
/// `value` at that index. Ids past the HEVC spec range are dropped so a
/// malformed length can never over-allocate.
fn register<T: Clone + Default>(sets: &mut Vec<T>, id: u32, value: T) {
    if id >= 64 {
        return;
    }
    // `id < 64` so the conversion is total; the fallback is never taken.
    let id = usize::try_from(id).unwrap_or(usize::MAX);
    while sets.len() <= id {
        sets.push(T::default());
    }
    #[expect(clippy::indexing_slicing, reason = "the loop above grows `sets` to id < len")]
    {
        sets[id] = value;
    }
}

/// Matches the reordered primaries against a known colour volume within the
/// fixed tolerances, returning its ISO code.
fn match_color_volume(primaries: &[u16; 8], green: usize, blue: usize, red: usize) -> Option<u8> {
    for row in &MASTERING_DISPLAY_COLOR_VOLUME_VALUES {
        let mut code = row.code;
        let mut j = 0_usize;
        while j < 2 {
            if !within(primaries, green, j, &row.values, 0) {
                code = 0;
            }
            if !within(primaries, blue, j, &row.values, 1) {
                code = 0;
            }
            if !within(primaries, red, j, &row.values, 2) {
                code = 0;
            }
            if !within_white(primaries, j, &row.values) {
                code = 0;
            }
            j = j.wrapping_add(1);
        }
        if code > 0 {
            return Some(code);
        }
    }
    None
}

/// Whether `primaries[channel*2 + j]` is within ±25 (≈±0.0005) of
/// `reference[ref_channel*2 + j]` — the primary-coordinate tolerance test.
fn within(
    primaries: &[u16; 8],
    channel: usize,
    j: usize,
    reference: &[u16; 8],
    ref_channel: usize,
) -> bool {
    let actual = primaries.get(channel.wrapping_mul(2).wrapping_add(j)).copied().unwrap_or(0);
    let expected = reference.get(ref_channel.wrapping_mul(2).wrapping_add(j)).copied().unwrap_or(0);
    let actual = i32::from(actual);
    let expected = i32::from(expected);
    actual >= expected.wrapping_sub(25) && actual < expected.wrapping_add(25)
}

/// Whether the white point `primaries[6 + j]` is within `-2..=+2` (≈±0.00005) of
/// the reference white point — the tighter white-point tolerance.
fn within_white(primaries: &[u16; 8], j: usize, reference: &[u16; 8]) -> bool {
    let actual = i32::from(primaries.get(6_usize.wrapping_add(j)).copied().unwrap_or(0));
    let expected = i32::from(reference.get(6_usize.wrapping_add(j)).copied().unwrap_or(0));
    actual >= expected.wrapping_sub(2) && actual < expected.wrapping_add(3)
}

/// Formats the raw display primaries as the
/// `R: x=… y=…, G: …, B: …, White point: …` string (each coordinate / 50000,
/// six fractional digits, locale-independent).
fn format_raw_primaries(primaries: &[u16; 8], red: usize, green: usize, blue: usize) -> String {
    let coord = |channel: usize, j: usize| -> f64 {
        f64::from(primaries.get(channel.wrapping_mul(2).wrapping_add(j)).copied().unwrap_or(0))
            / 50000.0
    };
    let white = |j: usize| -> f64 {
        f64::from(primaries.get(6_usize.wrapping_add(j)).copied().unwrap_or(0)) / 50000.0
    };
    format!(
        "R: x={:.6} y={:.6}, G: x={:.6} y={:.6}, B: x={:.6} y={:.6}, White point: x={:.6} y={:.6}",
        coord(red, 0),
        coord(red, 1),
        coord(green, 0),
        coord(green, 1),
        coord(blue, 0),
        coord(blue, 1),
        white(0),
        white(1),
    )
}

/// Formats the ST 2086 luminance as
/// `min: <4 decimals> cd/m2, max: <0 or 4 decimals> cd/m2` — the max uses no
/// fractional digits when its low-32-bit signed reinterpretation equals it
/// (always true for in-range values), else four.
fn format_luminance(luminance_min: u32, luminance_max: u32) -> String {
    let min = f64::from(luminance_min) / 10000.0;
    let max = f64::from(luminance_max) / 10000.0;
    // `cast_signed` reinterprets the low 32 bits, so the difference is zero
    // whenever the value fits i32 (every realistic luminance), selecting the
    // whole-number format.
    let whole = i64::from(luminance_max).wrapping_sub(i64::from(luminance_max.cast_signed())) == 0;
    if whole {
        format!("min: {min:.4} cd/m2, max: {max:.0} cd/m2")
    } else {
        format!("min: {min:.4} cd/m2, max: {max:.4} cd/m2")
    }
}

/// Formats the ST 2086 luminance with four fractional digits on both bounds —
/// the exact spelling the full description uses.
fn format_luminance_exact(luminance_min: u32, luminance_max: u32) -> String {
    let min = f64::from(luminance_min) / 10000.0;
    let max = f64::from(luminance_max) / 10000.0;
    format!("min: {min:.4} cd/m2, max: {max:.4} cd/m2")
}

/// Builds the encoding profile / extended format info from the first SPS and the
/// HDR SEI state, then marks the stream `is_vbr`/`is_initialized` — the tail of
/// the scan.
///
/// The `encoding_profile` is set once, from the first access unit that parses the
/// SPS. The `extended_format_info` is *reassembled from scratch* on every access
/// unit (it clears and rebuilds), so the HDR SEIs that accumulate across later
/// access units fold in while each fragment still appears exactly once.
fn finalize(stream: &mut TsVideoStream, ext: &mut HevcExtendedData, is_initialized: bool) {
    if let Some(sps) = ext.seq_parameter_sets.first().cloned()
        && sps.profile_space == 0
    {
        if !is_initialized {
            stream.encoding_profile = Some(encoding_profile(&sps));
        }
        build_extended_format_info(ext, &sps, stream.base.pid);
    }
    if !ext.seq_parameter_sets.is_empty() {
        stream.base.is_initialized = true;
    }
    stream.base.is_vbr = true;
}

/// The `encoding_profile` string from the SPS profile / level / tier.
fn encoding_profile(sps: &SeqParameterSet) -> String {
    let mut profile = String::new();
    // A `profile_idc` of 0 maps to `""` — pushing the empty string is a no-op,
    // so no separate zero gate is needed.
    profile.push_str(match sps.profile_idc {
        1 => "Main",
        2 => "Main 10",
        3 => "Main Still",
        _ => "",
    });
    if sps.level_idc > 0 {
        let calc_level = f32::from(sps.level_idc) / 30.0;
        let dec = sps.level_idc.wrapping_rem(10);
        let level = if dec >= 1 { format!("{calc_level:.1}") } else { format!("{calc_level:.0}") };
        profile.push_str(" @ Level ");
        profile.push_str(&level);
        profile.push_str(" @ ");
        profile.push_str(if sps.tier_flag { "High" } else { "Main" });
    }
    profile
}

/// Reassembles the chroma / bit-depth / HDR / colour / mastering / light-level
/// fragments of `extended_format_info` from the current SPS and HDR SEI state
/// (extended stream diagnostics are always on; nothing gates these fragments
/// off).
///
/// The list is cleared first and rebuilt whole, so calling this on every access
/// unit (to fold in sparse post-init HDR SEIs) keeps each fragment present
/// exactly once — never the lineage's per-occurrence duplication.
fn build_extended_format_info(ext: &mut HevcExtendedData, sps: &SeqParameterSet, pid: Pid) {
    ext.extended_format_info.clear();
    let info = &mut ext.extended_format_info;
    // A `chroma_format_idc` of 0 maps to `""` and is skipped — the empty-string
    // arm doubles as the zero gate.
    let chroma = match sps.chroma_format_idc {
        1 => "4:2:0",
        2 => "4:2:2",
        3 => "4:4:4",
        _ => "",
    };
    if !chroma.is_empty() {
        info.push(chroma.to_owned());
    }
    if sps.bit_depth_luma_minus8 == sps.bit_depth_chroma_minus8 {
        info.push(format!("{} bits", u16::from(sps.bit_depth_luma_minus8).wrapping_add(8)));
    }
    // The HDR label rides on the 10-bit BT.2020 PQ base plus *either* a present
    // mastering display *or* a signalled ST 2094-40 (HDR10+) message. Tying it to
    // the mastering SEI alone (as the BDInfo lineage does) drops the label on a
    // valid HDR10+ stream that carries dynamic metadata but no static mastering
    // display — so `is_hdr10_plus` alone is enough to claim the (HDR10+) label.
    if u16::from(sps.bit_depth_luma_minus8).wrapping_add(8) == 10
        && sps.chroma_format_idc == 1
        && sps.vui.video_signal_type_present_flag
        && sps.vui.colour_description_present_flag
        && sps.vui.colour_primaries == 9
        && sps.vui.transfer_characteristics == 16
        && (sps.vui.matrix_coefficients == 9 || sps.vui.matrix_coefficients == 10)
        && (!ext.mastering_display_color_primaries.is_empty() || ext.is_hdr10_plus)
    {
        info.push(
            if pid.get() >= 4117 {
                "Dolby Vision"
            } else if ext.is_hdr10_plus {
                "HDR10+"
            } else {
                "HDR10"
            }
            .to_owned(),
        );
    }
    if sps.vui.video_signal_type_present_flag {
        info.push(
            if sps.vui.video_full_range_flag == 1 { "Full Range" } else { "Limited Range" }
                .to_owned(),
        );
        if sps.vui.colour_description_present_flag {
            info.push(colour_primaries(sps.vui.colour_primaries).to_owned());
            info.push(transfer_characteristics(sps.vui.transfer_characteristics).to_owned());
            info.push(matrix_coefficients(sps.vui.matrix_coefficients).to_owned());
        }
    }
    if !ext.mastering_display_color_primaries.is_empty() {
        info.push(format!(
            "Mastering display color primaries: {}",
            ext.mastering_display_color_primaries
        ));
    }
    if !ext.mastering_display_luminance.is_empty() {
        info.push(format!("Mastering display luminance: {}", ext.mastering_display_luminance));
    }
    if ext.light_level_available && ext.maximum_content_light_level > 0 {
        info.push(format!(
            "Maximum Content Light Level: {} cd / m2",
            ext.maximum_content_light_level
        ));
        info.push(format!(
            "Maximum Frame-Average Light Level: {} cd/m2",
            ext.maximum_frame_average_light_level
        ));
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, proptest};

    use super::{
        Hevc, HevcExtendedData, StartCode, format_luminance_exact, match_color_volume, scan,
        sei_bytes, within, within_white,
    };
    use crate::bitstream::TsStreamBuffer;
    use crate::primitives::Pid;
    use crate::stream::{TsFrameRate, TsStreamType, TsVideoFormat, TsVideoStream};

    #[test]
    fn the_exact_luminance_spelling_keeps_four_decimals_on_both_bounds() {
        assert_eq!(
            format_luminance_exact(10, 100_000_000),
            "min: 0.0010 cd/m2, max: 10000.0000 cd/m2"
        );
    }

    #[test]
    fn the_exact_format_info_respells_only_the_luminance_entry() {
        let ext = HevcExtendedData {
            mastering_display_luminance_exact: "min: 0.0010 cd/m2, max: 1000.0000 cd/m2".to_owned(),
            extended_format_info: vec![
                "HDR10".to_owned(),
                "Mastering display luminance: min: 0.0010 cd/m2, max: 1000 cd/m2".to_owned(),
            ],
            ..HevcExtendedData::default()
        };
        assert_eq!(
            ext.extended_format_info_exact(),
            ["HDR10", "Mastering display luminance: min: 0.0010 cd/m2, max: 1000.0000 cd/m2"]
        );
        // Without a stored exact spelling the entries pass through unchanged.
        let plain = HevcExtendedData {
            extended_format_info: vec!["Mastering display luminance: raw".to_owned()],
            ..HevcExtendedData::default()
        };
        assert_eq!(plain.extended_format_info_exact(), ["Mastering display luminance: raw"]);
    }

    #[test]
    fn full_description_joins_the_exact_format_info() {
        let mut stream = TsVideoStream::default();
        stream.base.stream_type = TsStreamType::HevcVideo;
        stream.extended_data = Some(HevcExtendedData {
            mastering_display_luminance_exact: "min: 0.0010 cd/m2, max: 1000.0000 cd/m2".to_owned(),
            extended_format_info: vec![
                "Mastering display luminance: min: 0.0010 cd/m2, max: 1000 cd/m2".to_owned(),
            ],
            ..HevcExtendedData::default()
        });
        assert_eq!(
            stream.description(),
            "Mastering display luminance: min: 0.0010 cd/m2, max: 1000 cd/m2"
        );
        assert_eq!(
            stream.full_description(),
            "Mastering display luminance: min: 0.0010 cd/m2, max: 1000.0000 cd/m2"
        );
    }

    /// MSB-first bit writer for building HEVC RBSP test vectors.
    #[derive(Default)]
    struct Writer {
        bits: Vec<bool>,
    }

    impl Writer {
        fn new() -> Self {
            Self::default()
        }

        /// Appends one bit.
        fn bit(&mut self, value: bool) {
            self.bits.push(value);
        }

        /// Appends the low `n` bits of `value`, most-significant first (`u(n)`).
        fn u(&mut self, value: u64, n: u32) {
            let mut i = n;
            while i > 0 {
                i = i.wrapping_sub(1);
                self.bit((value.wrapping_shr(i) & 1) == 1);
            }
        }

        /// Appends an unsigned Exp-Golomb code `ue(v)`.
        fn ue(&mut self, v: u32) {
            let code = v.wrapping_add(1);
            let len = 32_u32.wrapping_sub(code.leading_zeros());
            let mut zeros = len.wrapping_sub(1);
            while zeros > 0 {
                self.bit(false);
                zeros = zeros.wrapping_sub(1);
            }
            self.u(u64::from(code), len);
        }

        /// Appends a whole byte (8 bits).
        fn byte(&mut self, value: u8) {
            self.u(u64::from(value), 8);
        }

        /// Appends every bit of another writer.
        fn append(&mut self, other: &Self) {
            for &b in &other.bits {
                self.bit(b);
            }
        }

        /// Packs the bits MSB-first into bytes, left-aligning a trailing partial.
        fn bytes(&self) -> Vec<u8> {
            let mut out = Vec::new();
            let mut cur: u8 = 0;
            let mut n: u32 = 0;
            for &b in &self.bits {
                cur = cur.wrapping_shl(1).wrapping_add(u8::from(b));
                n = n.wrapping_add(1);
                if n == 8 {
                    out.push(cur);
                    cur = 0;
                    n = 0;
                }
            }
            if n > 0 {
                out.push(cur.wrapping_shl(8_u32.wrapping_sub(n)));
            }
            out
        }
    }

    /// Inserts H.26x emulation-prevention bytes (`00 00` followed by `<= 03` gets a
    /// `03`) so a payload never forms a false start code.
    fn emulation_encode(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut zeros: u32 = 0;
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                out.push(0x03);
                zeros = 0;
            }
            out.push(b);
            zeros = if b == 0 { zeros.wrapping_add(1) } else { 0 };
        }
        out
    }

    /// Wraps an RBSP bit writer in a NAL unit: `00 00 01` start code, then the
    /// emulation-encoded NAL header (`forbidden=0`, type, layer 0, temporal 1) and
    /// RBSP payload.
    fn nal(nal_unit_type: u8, rbsp: &Writer) -> Vec<u8> {
        let mut w = Writer::new();
        w.bit(false); // forbidden_zero_bit
        w.u(u64::from(nal_unit_type), 6); // nal_unit_type
        w.u(0, 6); // nuh_layer_id
        w.u(1, 3); // nuh_temporal_id_plus1
        w.append(rbsp);
        let mut out = vec![0x00, 0x00, 0x01];
        out.extend_from_slice(&emulation_encode(&w.bytes()));
        out
    }

    /// Builds a single SEI message (`payload_type`, one-byte `payload_size`, body).
    fn sei_message(payload_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut m = vec![payload_type, u8::try_from(payload.len()).unwrap()];
        m.extend_from_slice(payload);
        m
    }

    /// Builds an SEI NAL (type 39) from concatenated messages plus the
    /// `rbsp_trailing_bits` (`0x80`) the boundary scan excludes.
    fn sei_nal(messages: &[Vec<u8>]) -> Vec<u8> {
        let mut w = Writer::new();
        for message in messages {
            for &b in message {
                w.byte(b);
            }
        }
        w.byte(0x80); // rbsp_trailing_bits
        nal(39, &w)
    }

    /// The 24-byte ST 2086 mastering-display payload: eight 16-bit display
    /// primaries / white-point coordinates (display order G, B, R, W), then the
    /// 32-bit max and 32-bit min luminance.
    fn mastering_payload(coords: [u16; 8], max_lum: u32, min_lum: u32) -> Vec<u8> {
        let mut p = Vec::new();
        for c in coords {
            p.extend_from_slice(&c.to_be_bytes());
        }
        p.extend_from_slice(&max_lum.to_be_bytes());
        p.extend_from_slice(&min_lum.to_be_bytes());
        p
    }

    /// The Display P3 mastering coordinates (display order G, B, R, white point) —
    /// matches table code 12.
    const DISPLAY_P3_COORDS: [u16; 8] = [13250, 34500, 7500, 3000, 34000, 16000, 15635, 16450];

    /// The 4-byte `content_light_level_info` payload (`MaxCLL`, `MaxFALL`).
    fn light_level_payload(max_cll: u16, max_fall: u16) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&max_cll.to_be_bytes());
        p.extend_from_slice(&max_fall.to_be_bytes());
        p
    }

    /// The 8-byte ST 2094-40 HDR10+ ITU-T T.35 payload (country `0xB5`, provider
    /// `0x003C`, oriented `0x0001`, application id 4, version 0, one window).
    fn hdr10_plus_payload() -> Vec<u8> {
        vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x00, 0x40]
    }

    /// A VPS NAL (type 32) registering video parameter set `id` — the body past the
    /// id is output-dead, so only the 4-bit id plus a stop bit is written.
    fn vps(id: u64) -> Vec<u8> {
        let mut w = Writer::new();
        w.u(id, 4); // vps_video_parameter_set_id
        w.byte(0x80); // filler / trailing
        nal(32, &w)
    }

    /// The VUI colour-description knobs an [`SpsConfig`] carries.
    #[derive(Clone, Copy)]
    #[expect(clippy::struct_excessive_bools, reason = "each bool is a distinct VUI bitstream flag")]
    struct VuiConfig {
        aspect_ratio_present: bool,
        aspect_ratio_idc: u64,
        overscan_present: bool,
        video_signal_type_present: bool,
        full_range: u64,
        colour_description_present: bool,
        colour_primaries: u64,
        transfer_characteristics: u64,
        matrix_coefficients: u64,
    }

    impl Default for VuiConfig {
        fn default() -> Self {
            Self {
                aspect_ratio_present: false,
                aspect_ratio_idc: 1,
                overscan_present: false,
                video_signal_type_present: true,
                full_range: 0,
                colour_description_present: true,
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 9,
            }
        }
    }

    /// A deliberately-malformed SPS shape the *faithful* high-level `sps()` writer
    /// cannot express — a reference-picture-set count declared astronomically large
    /// (an `ue(v)` ~2^31) with no/too-few matching entries. Drives the D15 #8
    /// exhaustion guards' false path (the loop must stop at end-of-content, not run
    /// the claimed count). `cargo mutants` catches a broken guard as a hang
    /// (nextest slow-timeout → FAILURE; see `.cargo/mutants.toml`).
    #[derive(Clone, Copy)]
    enum MalformedRps {
        /// `num_short_term_ref_pic_sets` ~2^31 (no sets follow) — drives the outer
        /// loop's [`MAX_SHORT_TERM_REF_PIC_SETS`] clamp.
        OuterCount,
        /// `num_short_term_ref_pic_sets = 1`, then the first set declares
        /// `num_negative_pics`/`num_positive_pics` ~2^31 each (no entries follow).
        InnerPicCounts,
        /// `long_term_ref_pics_present_flag = 1`, then `num_long_term_ref_pics_sps`
        /// ~2^31 (no entries follow).
        LongTermCount,
    }

    /// Knobs for building a sequence-parameter-set NAL — defaults describe a 10-bit
    /// 4:2:0 BT.2020 PQ Main-10 Level-5.1 High-tier HDR stream.
    #[derive(Clone)]
    #[expect(clippy::struct_excessive_bools, reason = "each bool toggles a distinct SPS field")]
    struct SpsConfig {
        vps_id: u64,
        max_sub_layers_minus1: u16,
        profile_space: u64,
        tier_flag: bool,
        profile_idc: u64,
        level_idc: u64,
        sub_layer_flags: Vec<(bool, bool)>,
        sps_id: u32,
        chroma_format_idc: u32,
        separate_colour_plane: bool,
        conformance_window: bool,
        bit_depth_luma_minus8: u32,
        bit_depth_chroma_minus8: u32,
        log2_max_poc_minus4: u32,
        sub_layer_ordering_present: bool,
        scaling_list_enabled: bool,
        scaling_list_present: bool,
        scaling_list_pred_mode: bool,
        pcm_enabled: bool,
        short_term_ref_pic_sets: u32,
        inter_pred_rps: bool,
        long_term_present: bool,
        num_long_term: u32,
        vui_present: bool,
        vui: VuiConfig,
        /// When set, emit a malformed ref-pic-set count (see [`MalformedRps`]).
        malformed_rps: Option<MalformedRps>,
    }

    impl Default for SpsConfig {
        fn default() -> Self {
            Self {
                vps_id: 0,
                max_sub_layers_minus1: 0,
                profile_space: 0,
                tier_flag: true,
                profile_idc: 2,
                level_idc: 153,
                sub_layer_flags: Vec::new(),
                sps_id: 0,
                chroma_format_idc: 1,
                separate_colour_plane: false,
                conformance_window: false,
                bit_depth_luma_minus8: 2,
                bit_depth_chroma_minus8: 2,
                log2_max_poc_minus4: 4,
                sub_layer_ordering_present: true,
                scaling_list_enabled: false,
                scaling_list_present: false,
                scaling_list_pred_mode: false,
                pcm_enabled: false,
                short_term_ref_pic_sets: 0,
                inter_pred_rps: false,
                long_term_present: false,
                num_long_term: 0,
                vui_present: true,
                vui: VuiConfig::default(),
                malformed_rps: None,
            }
        }
    }

    /// Writes `profile_tier_level` into `w`.
    fn write_profile_tier_level(w: &mut Writer, cfg: &SpsConfig) {
        w.u(cfg.profile_space, 2); // general_profile_space
        w.bit(cfg.tier_flag); // general_tier_flag
        w.u(cfg.profile_idc, 5); // general_profile_idc
        w.u(0, 32); // general_profile_compatibility_flags
        w.u(0, 4); // progressive/interlaced/non_packed/frame_only flags
        w.u(0, 32); // general_reserved_zero (first 32 of 44)
        w.u(0, 12); // general_reserved_zero (last 12 of 44)
        w.u(cfg.level_idc, 8); // general_level_idc
        for &(profile_present, level_present) in &cfg.sub_layer_flags {
            w.bit(profile_present);
            w.bit(level_present);
        }
        if cfg.max_sub_layers_minus1 > 0 {
            // reserved_zero_2bits[i] for i in [max_sub_layers_minus1, 8) — the reader
            // consumes (8 - max_sub_layers_minus1) two-bit fields, so emit that many.
            let mut i = cfg.max_sub_layers_minus1;
            while i < 8 {
                w.u(0, 2);
                i = i.wrapping_add(1);
            }
        }
        for &(profile_present, level_present) in &cfg.sub_layer_flags {
            if profile_present {
                w.u(0, 32); // sub_layer profile data (88 bits)
                w.u(0, 32);
                w.u(0, 24);
            }
            if level_present {
                w.u(0, 8); // sub_layer_level_idc
            }
        }
    }

    /// Builds a sequence-parameter-set NAL (type 33) from `cfg`.
    fn sps(cfg: &SpsConfig) -> Vec<u8> {
        let mut w = Writer::new();
        w.u(cfg.vps_id, 4); // sps_video_parameter_set_id
        w.u(u64::from(cfg.max_sub_layers_minus1), 3); // sps_max_sub_layers_minus1
        w.bit(false); // sps_temporal_id_nesting_flag
        write_profile_tier_level(&mut w, cfg);
        w.ue(cfg.sps_id); // sps_seq_parameter_set_id
        w.ue(cfg.chroma_format_idc); // chroma_format_idc
        if cfg.chroma_format_idc == 3 {
            w.bit(cfg.separate_colour_plane); // separate_colour_plane_flag
        }
        w.ue(0); // pic_width_in_luma_samples
        w.ue(0); // pic_height_in_luma_samples
        w.bit(cfg.conformance_window); // conformance_window_flag
        if cfg.conformance_window {
            w.ue(0); // conf_win_left_offset
            w.ue(0); // conf_win_right_offset
            w.ue(0); // conf_win_top_offset
            w.ue(0); // conf_win_bottom_offset
        }
        w.ue(cfg.bit_depth_luma_minus8); // bit_depth_luma_minus8
        w.ue(cfg.bit_depth_chroma_minus8); // bit_depth_chroma_minus8
        w.ue(cfg.log2_max_poc_minus4); // log2_max_pic_order_cnt_lsb_minus4
        w.bit(cfg.sub_layer_ordering_present); // sps_sub_layer_ordering_info_present_flag
        let iterations = if cfg.sub_layer_ordering_present {
            cfg.max_sub_layers_minus1.wrapping_add(1)
        } else {
            1
        };
        for _ in 0..iterations {
            w.ue(0); // sps_max_dec_pic_buffering_minus1
            w.ue(0); // sps_max_num_reorder_pics
            w.ue(0); // sps_max_latency_increase_plus1
        }
        for _ in 0..6 {
            w.ue(0); // log2 CB/TB sizes + max transform hierarchy depths
        }
        w.bit(cfg.scaling_list_enabled); // scaling_list_enabled_flag
        if cfg.scaling_list_enabled {
            w.bit(cfg.scaling_list_present); // sps_scaling_list_data_present_flag
            if cfg.scaling_list_present {
                if cfg.scaling_list_pred_mode {
                    write_scaling_list_pred(&mut w);
                } else {
                    write_scaling_list_data(&mut w);
                }
            }
        }
        w.u(0, 2); // amp_enabled_flag, sample_adaptive_offset_enabled_flag
        w.bit(cfg.pcm_enabled); // pcm_enabled_flag
        if cfg.pcm_enabled {
            w.u(0, 8); // pcm bit depths + log2 sizes split
            w.ue(0);
            w.ue(0);
            w.bit(false); // pcm_loop_filter_disabled_flag
        }
        if matches!(cfg.malformed_rps, Some(MalformedRps::OuterCount)) {
            w.ue(0x7FFF_FFFF); // num_short_term_ref_pic_sets — astronomically large, no sets
        } else if matches!(cfg.malformed_rps, Some(MalformedRps::InnerPicCounts)) {
            w.ue(1); // num_short_term_ref_pic_sets = 1 (one explicit set)
            w.ue(0x7FFF_FFFF); // num_negative_pics — astronomically large, no entries
            w.ue(0x7FFF_FFFF); // num_positive_pics — astronomically large, no entries
        } else if cfg.inter_pred_rps {
            w.ue(3); // num_short_term_ref_pic_sets (one explicit, two inter-predicted)
            write_inter_pred_rps(&mut w);
        } else {
            w.ue(cfg.short_term_ref_pic_sets); // num_short_term_ref_pic_sets
            write_short_term_ref_pic_sets(&mut w, cfg.short_term_ref_pic_sets);
        }
        if matches!(cfg.malformed_rps, Some(MalformedRps::LongTermCount)) {
            w.bit(true); // long_term_ref_pics_present_flag
            w.ue(0x7FFF_FFFF); // num_long_term_ref_pics_sps — astronomically large, no entries
        } else {
            w.bit(cfg.long_term_present); // long_term_ref_pics_present_flag
            if cfg.long_term_present {
                w.ue(cfg.num_long_term); // num_long_term_ref_pics_sps
                for _ in 0..cfg.num_long_term {
                    w.u(0, cfg.log2_max_poc_minus4.wrapping_add(4)); // lt_ref_pic_poc_lsb_sps
                    w.bit(false); // used_by_curr_pic_lt_sps_flag
                }
            }
        }
        w.u(0, 2); // sps_temporal_mvp_enabled, strong_intra_smoothing_enabled
        w.bit(cfg.vui_present); // vui_parameters_present_flag
        if cfg.vui_present {
            write_vui(&mut w, &cfg.vui);
        }
        w.byte(0x80); // trailing
        nal(33, &w)
    }

    /// Writes a minimal explicit `scaling_list_data` block (every
    /// `scaling_list_pred_mode_flag` clear → one `ue` per matrix).
    fn write_scaling_list_data(w: &mut Writer) {
        for size_id in 0..4 {
            let matrices = if size_id == 3 { 2 } else { 6 };
            for _ in 0..matrices {
                w.bit(false); // scaling_list_pred_mode_flag = 0
                w.ue(0); // scaling_list_pred_matrix_id_delta
            }
        }
    }

    /// Writes a `scaling_list_data` block with every `scaling_list_pred_mode_flag`
    /// set (the coefficient-list branch, with the DC coefficient for `size_id > 1`).
    fn write_scaling_list_pred(w: &mut Writer) {
        for size_id in 0..4_u32 {
            let matrices = if size_id == 3 { 2 } else { 6 };
            let coef_num =
                64_u32.min(1_u32.wrapping_shl(4_u32.wrapping_add(size_id.wrapping_mul(2))));
            for _ in 0..matrices {
                w.bit(true); // scaling_list_pred_mode_flag = 1
                if size_id > 1 {
                    w.ue(0); // scaling_list_dc_coef_minus8
                }
                for _ in 0..coef_num {
                    w.ue(0); // scaling_list_delta_coef
                }
            }
        }
    }

    /// Like [`nal`] but with a four-byte (`00 00 00 01`) start code, exercising the
    /// four-byte branch of both the main and SEI start-code scans.
    fn nal4(nal_unit_type: u8, rbsp: &Writer) -> Vec<u8> {
        let mut out = vec![0x00];
        out.extend_from_slice(&nal(nal_unit_type, rbsp));
        out
    }

    /// Writes `num` simple explicit short-term reference picture sets (one negative
    /// picture each; no inter-set prediction).
    fn write_short_term_ref_pic_sets(w: &mut Writer, num: u32) {
        for idx in 0..num {
            if idx > 0 {
                w.bit(false); // inter_ref_pic_set_prediction_flag = 0
            }
            w.ue(1); // num_negative_pics
            w.ue(0); // num_positive_pics
            w.ue(0); // delta_poc_s0_minus1
            w.bit(true); // used_by_curr_pic_s0_flag
        }
    }

    /// Writes three chained short-term reference picture sets — an explicit one
    /// (three pictures), then two inter-predicted ones. Set 1's per-picture flags
    /// cover the used / use-delta / neither branches and yield a picture count of 2,
    /// which set 2 then consumes — so a mutation that miscounts set 1 (e.g.
    /// `|| -> &&`) makes set 2 read the wrong number of bits and corrupts the VUI
    /// that follows.
    fn write_inter_pred_rps(w: &mut Writer) {
        // st_rps_idx 0: explicit, num_pics = 2 + 1 = 3.
        w.ue(2); // num_negative_pics
        w.ue(1); // num_positive_pics
        for _ in 0..3 {
            w.ue(0); // delta_poc_sN_minus1
            w.bit(true); // used_by_curr_pic_sN_flag
        }
        // st_rps_idx 1: inter-predicted (loops pic_pos 0..=3); num_pics_new = 2.
        w.bit(true); // inter_ref_pic_set_prediction_flag
        w.bit(false); // delta_rps_sign
        w.ue(0); // abs_delta_rps_minus1
        w.bit(true); // pic 0: used_by_curr_pic_flag = 1
        w.bit(true); // pic 1: used_by_curr_pic_flag = 1
        w.bit(false); // pic 2: used_by_curr_pic_flag = 0
        w.bit(false); //         use_delta_flag = 0
        w.bit(false); // pic 3: used_by_curr_pic_flag = 0
        w.bit(false); //         use_delta_flag = 0
        // st_rps_idx 2: inter-predicted (loops pic_pos 0..=2 with the count of 2).
        w.bit(true); // inter_ref_pic_set_prediction_flag
        w.bit(false); // delta_rps_sign
        w.ue(0); // abs_delta_rps_minus1
        w.bit(true); // pic 0: used_by_curr_pic_flag = 1
        w.bit(true); // pic 1: used_by_curr_pic_flag = 1
        w.bit(true); // pic 2: used_by_curr_pic_flag = 1
    }

    /// Writes `vui_parameters` through the colour description.
    fn write_vui(w: &mut Writer, vui: &VuiConfig) {
        w.bit(vui.aspect_ratio_present); // aspect_ratio_info_present_flag
        if vui.aspect_ratio_present {
            w.u(vui.aspect_ratio_idc, 8); // aspect_ratio_idc
            if vui.aspect_ratio_idc == 0xFF {
                w.u(0, 16); // sar_width
                w.u(0, 16); // sar_height
            }
        }
        w.bit(vui.overscan_present); // overscan_info_present_flag
        if vui.overscan_present {
            w.bit(false); // overscan_appropriate_flag
        }
        w.bit(vui.video_signal_type_present); // video_signal_type_present_flag
        if vui.video_signal_type_present {
            w.u(0, 3); // video_format
            w.u(vui.full_range, 1); // video_full_range_flag
            w.bit(vui.colour_description_present); // colour_description_present_flag
            if vui.colour_description_present {
                w.u(vui.colour_primaries, 8); // colour_primaries
                w.u(vui.transfer_characteristics, 8); // transfer_characteristics
                w.u(vui.matrix_coefficients, 8); // matrix_coeffs
            }
        }
        w.byte(0x80); // trailing past the colour description (parser stops earlier)
    }

    /// A picture-parameter-set NAL (type 34) with the given ids and slice header
    /// knobs.
    fn pps(pps_id: u32, sps_id: u32, dependent: bool, num_extra_bits: u64) -> Vec<u8> {
        let mut w = Writer::new();
        w.ue(pps_id); // pps_pic_parameter_set_id
        w.ue(sps_id); // pps_seq_parameter_set_id
        w.bit(dependent); // dependent_slice_segments_enabled_flag
        w.bit(false); // output_flag_present_flag
        w.u(num_extra_bits, 3); // num_extra_slice_header_bits
        w.byte(0x80); // trailing
        nal(34, &w)
    }

    /// A first-slice-segment NAL of `nal_unit_type` carrying `slice_type`
    /// (0 = P, 1 = B, 2 = I) for picture-parameter-set `pps_id`.
    fn slice(nal_unit_type: u8, pps_id: u32, slice_type: u32) -> Vec<u8> {
        slice_seg(nal_unit_type, true, pps_id, 0, slice_type)
    }

    /// A slice-segment NAL with full control over the first-slice flag and the
    /// picture-parameter-set's `num_extra_slice_header_bits` reserved bits.
    fn slice_seg(
        nal_unit_type: u8,
        first: bool,
        pps_id: u32,
        num_extra: u32,
        slice_type: u32,
    ) -> Vec<u8> {
        let mut w = Writer::new();
        w.bit(first); // first_slice_segment_in_pic_flag
        if (16..=23).contains(&nal_unit_type) {
            w.bit(false); // no_output_of_prior_pics_flag
        }
        w.ue(pps_id); // slice_pic_parameter_set_id
        if first {
            w.u(0, num_extra); // slice_reserved_flag[i]
            w.ue(slice_type); // slice_type
        } else {
            w.bit(false); // dependent_slice_segment_flag (read only if PPS dependent)
        }
        w.byte(0x80); // trailing
        nal(nal_unit_type, &w)
    }

    /// An SEI message with an explicit (possibly multi-byte / oversized)
    /// `declared_size` rather than the real payload length.
    fn sei_message_raw(payload_type: u8, declared_size: usize, payload: &[u8]) -> Vec<u8> {
        let mut m = vec![payload_type];
        let mut remaining = declared_size;
        while remaining >= 255 {
            m.push(0xFF);
            remaining = remaining.wrapping_sub(255);
        }
        m.push(u8::try_from(remaining).unwrap());
        m.extend_from_slice(payload);
        m
    }

    /// Concatenates NAL units plus trailing filler into one access-unit buffer.
    fn buffer_of(nals: &[Vec<u8>]) -> TsStreamBuffer {
        let mut data = Vec::new();
        for n in nals {
            data.extend_from_slice(n);
        }
        data.extend_from_slice(&[0xFF; 8]); // trailing filler (no start code)
        let mut b = TsStreamBuffer::new();
        b.add(&data, 0, data.len());
        b.begin_read();
        b
    }

    /// Runs [`scan`] over `nals` for a stream at `pid`, returning `(stream, tag)`.
    fn run(pid: u16, nals: &[Vec<u8>]) -> (TsVideoStream, Option<String>) {
        let mut s = TsVideoStream::default();
        s.base.stream_type = TsStreamType::HevcVideo;
        s.base.pid = Pid::new(pid);
        let mut b = buffer_of(nals);
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        (s, tag)
    }

    /// The standard HDR access unit (VPS, SPS, PPS, SEI, IDR slice) at `pid`, with
    /// the SEI messages built from `messages`.
    fn hdr_unit(pid: u16, messages: &[Vec<u8>]) -> (TsVideoStream, Option<String>) {
        run(
            pid,
            &[
                vps(0),
                sps(&SpsConfig::default()),
                pps(0, 0, false, 0),
                sei_nal(messages),
                slice(19, 0, 2),
            ],
        )
    }

    /// The two SEI messages of the reference HDR10 unit (mastering + light level)
    /// with a real disc's luminance / light values.
    fn hdr_messages() -> Vec<Vec<u8>> {
        vec![
            sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1)),
            sei_message(144, &light_level_payload(1033, 311)),
        ]
    }

    #[test]
    fn hdr10_unit_reproduces_the_expected_desc() {
        // A real UHD HDR10+Dolby Vision disc's base layer (PID 4113 < 4117,
        // no HDR10+ T.35 → "HDR10").
        let (mut s, tag) = hdr_unit(4113, &hdr_messages());
        s.set_video_format(TsVideoFormat::Videoformat2160p);
        s.set_frame_rate(TsFrameRate::Framerate23_976);
        s.aspect_ratio = crate::stream::TsAspectRatio::Aspect16_9;
        assert_eq!(tag.as_deref(), Some("I"));
        assert_eq!(s.codec_short_name(), "HEVC");
        assert_eq!(s.codec_name(), "MPEG-H HEVC Video");
        assert_eq!(
            s.description(),
            "2160p / 23.976 fps / 16:9 / Main 10 @ Level 5.1 @ High / 4:2:0 / 10 bits / HDR10 / \
             Limited Range / BT.2020 / PQ / BT.2020 non-constant / Mastering display color \
             primaries: Display P3 / Mastering display luminance: min: 0.0001 cd/m2, max: 1000 \
             cd/m2 / Maximum Content Light Level: 1033 cd / m2 / Maximum Frame-Average Light \
             Level: 311 cd/m2"
        );
    }

    #[test]
    fn dolby_vision_unit_uses_the_pid_gated_label() {
        // The same disc's enhancement layer (PID 4117 >= 4117
        // → "Dolby Vision"; the enhancement layer carries no light-level SEI).
        let (mut s, _) = hdr_unit(
            4117,
            &[sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1))],
        );
        s.set_video_format(TsVideoFormat::Videoformat1080p);
        s.set_frame_rate(TsFrameRate::Framerate23_976);
        s.aspect_ratio = crate::stream::TsAspectRatio::Aspect16_9;
        assert_eq!(
            s.description(),
            "1080p / 23.976 fps / 16:9 / Main 10 @ Level 5.1 @ High / 4:2:0 / 10 bits / Dolby \
             Vision / Limited Range / BT.2020 / PQ / BT.2020 non-constant / Mastering display \
             color primaries: Display P3 / Mastering display luminance: min: 0.0001 cd/m2, max: \
             1000 cd/m2"
        );
    }

    #[test]
    fn hdr10_plus_unit_uses_the_t35_gated_label() {
        // A real UHD HDR10+ disc sample (PID 4113,
        // ST 2094-40 T.35 present → "HDR10+").
        let (mut s, _) = hdr_unit(4113, &hdr_messages_hdr10_plus());
        s.set_video_format(TsVideoFormat::Videoformat2160p);
        s.set_frame_rate(TsFrameRate::Framerate23_976);
        s.aspect_ratio = crate::stream::TsAspectRatio::Aspect16_9;
        assert_eq!(
            s.description(),
            "2160p / 23.976 fps / 16:9 / Main 10 @ Level 5.1 @ High / 4:2:0 / 10 bits / HDR10+ / \
             Limited Range / BT.2020 / PQ / BT.2020 non-constant / Mastering display color \
             primaries: Display P3 / Mastering display luminance: min: 0.0010 cd/m2, max: 1000 \
             cd/m2 / Maximum Content Light Level: 969 cd / m2 / Maximum Frame-Average Light \
             Level: 230 cd/m2"
        );
    }

    /// The HDR10+ disc's SEI set (mastering min 10 → 0.0010, `MaxCLL` 969 / 311
    /// `MaxFALL` 230, plus the T.35).
    fn hdr_messages_hdr10_plus() -> Vec<Vec<u8>> {
        vec![
            sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 10)),
            sei_message(144, &light_level_payload(969, 230)),
            sei_message(4, &hdr10_plus_payload()),
        ]
    }

    #[test]
    fn no_start_codes_leaves_the_stream_uninitialized() {
        let (s, tag) = run(4113, &[]);
        assert!(s.encoding_profile.is_none());
        assert!(!s.base.is_initialized);
        assert_eq!(tag, None);
        // is_vbr is set unconditionally at the end of the scan.
        assert!(s.base.is_vbr);
    }

    /// Runs `[vps(0), sps(cfg)]` and returns the resulting stream.
    fn sps_stream(cfg: &SpsConfig) -> TsVideoStream {
        run(4113, &[vps(0), sps(cfg)]).0
    }

    /// The `desc` of a `[vps(0), sps(cfg)]` unit (no resolution / SEI).
    fn sps_desc(cfg: &SpsConfig) -> String {
        sps_stream(cfg).description()
    }

    #[test]
    fn colour_primaries_table_covers_every_arm() {
        let cases = [
            (1_u8, "BT.709"),
            (4, "BT.470 System M"),
            (5, "BT.601 PAL"),
            (6, "BT.601 NTSC"),
            (7, "SMPTE 240M"),
            (8, "Generic film"),
            (9, "BT.2020"),
            (10, "XYZ"),
            (11, "DCI P3"),
            (12, "Display P3"),
            (22, "EBU Tech 3213"),
            (0, ""),
            (2, ""),
            (255, ""),
        ];
        for (code, name) in cases {
            assert_eq!(super::colour_primaries(code), name, "primaries {code}");
        }
    }

    #[test]
    fn transfer_characteristics_table_covers_every_arm() {
        let cases = [
            (1_u8, "BT.709"),
            (4, "BT.470 System M"),
            (5, "BT.470 System B/G"),
            (6, "BT.601"),
            (7, "SMPTE 240M"),
            (8, "Linear"),
            (9, "Logarithmic (100:1)"),
            (10, "Logarithmic (316.22777:1)"),
            (11, "xvYCC"),
            (12, "BT.1361"),
            (13, "sRGB/sYCC"),
            (14, "BT.2020 (10-bit)"),
            (15, "BT.2020 (12-bit)"),
            (16, "PQ"),
            (17, "SMPTE 428M"),
            (18, "HLG"),
            (0, ""),
            (255, ""),
        ];
        for (code, name) in cases {
            assert_eq!(super::transfer_characteristics(code), name, "transfer {code}");
        }
    }

    #[test]
    fn matrix_coefficients_table_covers_every_arm() {
        let cases = [
            (0_u8, "Identity"),
            (1, "BT.709"),
            (4, "FCC 73.682"),
            (5, "BT.470 System B/G"),
            (6, "BT.601"),
            (7, "SMPTE 240M"),
            (8, "YCgCo"),
            (9, "BT.2020 non-constant"),
            (10, "BT.2020 constant"),
            (11, "Y'D'zD'x"),
            (12, "Chromaticity-derived non-constant"),
            (13, "Chromaticity-derived constant"),
            (14, "ICtCp"),
            (2, ""),
            (255, ""),
        ];
        for (code, name) in cases {
            assert_eq!(super::matrix_coefficients(code), name, "matrix {code}");
        }
    }

    #[test]
    fn default_sps_desc_has_no_hdr_label_without_mastering() {
        // The HDR gate needs a present mastering display; without the SEI the label
        // is absent but the chroma / bit-depth / colour fragments still appear.
        assert_eq!(
            sps_desc(&SpsConfig::default()),
            "Main 10 @ Level 5.1 @ High / 4:2:0 / 10 bits / Limited Range / BT.2020 / PQ / \
             BT.2020 non-constant"
        );
    }

    #[test]
    fn profile_idc_variants_map_to_their_names() {
        let profile = |idc: u64| {
            let cfg = SpsConfig { profile_idc: idc, ..SpsConfig::default() };
            sps_stream(&cfg).encoding_profile
        };
        assert_eq!(profile(1).as_deref(), Some("Main @ Level 5.1 @ High"));
        assert_eq!(profile(2).as_deref(), Some("Main 10 @ Level 5.1 @ High"));
        assert_eq!(profile(3).as_deref(), Some("Main Still @ Level 5.1 @ High"));
        // profile_idc > 0 but unrecognized → empty name, the level still appends.
        assert_eq!(profile(5).as_deref(), Some(" @ Level 5.1 @ High"));
        // profile_idc 0 → the name switch is skipped entirely.
        assert_eq!(profile(0).as_deref(), Some(" @ Level 5.1 @ High"));
    }

    /// Parses one `profile_tier_level` from a hand-built buffer: byte 0 carries
    /// `profile_space` 0 / `tier_flag` 0 / the 5-bit `profile_idc`, then the 32-bit
    /// `compatibility_flags` word, then 48 reserved bits (4 + 44) and the 8-bit
    /// `level_idc`. Raw bytes (no NAL emulation wrapping) keep the cursor clean so
    /// the parse is exercised end-to-end without `0x03` skips.
    fn parse_profile_tier_level(profile_idc: u8, flags: u32, level_idc: u8) -> (u16, u16) {
        let mut bytes = vec![profile_idc & 0x1F];
        bytes.extend_from_slice(&flags.to_be_bytes());
        bytes.extend_from_slice(&[0_u8; 6]); // 4 + 44 reserved bits
        bytes.push(level_idc);
        bytes.push(0x80); // trailing content so the cursor never reaches EOF
        let mut ext = HevcExtendedData::default();
        let mut b = TsStreamBuffer::new();
        let len = bytes.len();
        b.add(&bytes, 0, len);
        b.begin_read();
        let mut h = Hevc { ext: &mut ext, buffer: &mut b, is_initialized: false };
        let ptl = h.profile_tier_level(0);
        (ptl.profile_idc, ptl.level_idc)
    }

    #[test]
    fn profile_idc_zero_recovers_from_the_compatibility_flags() {
        // FFmpeg recovers general_profile_idc from the lowest set compatibility bit
        // (index 1..32) when the coded profile_idc is 0 (hevc/ps.c:285-290). Flag i
        // sits at bit (31 - i) of the 32-bit word; level_idc 153 confirms the
        // recovery consumes exactly the 32-bit field (the cursor stays aligned).
        let recover = |flags: u32| parse_profile_tier_level(0, flags, 153);
        // compatibility_flag[2] (bit 29) set → profile_idc 2, level intact.
        assert_eq!(recover(0x2000_0000), (2, 153));
        // compatibility_flag[1] (bit 30) set → profile_idc 1.
        assert_eq!(recover(0x4000_0000), (1, 153));
        // The lowest set index wins: flags 1 and 2 both set → index 1.
        assert_eq!(recover(0x6000_0000), (1, 153));
        // compatibility_flag[31] (bit 0) — the only set bit — recovers index 31.
        assert_eq!(recover(0x0000_0001), (31, 153));
        // Only the index-0 flag (bit 31) set → recovery is a no-op; profile_idc
        // stays 0 (it never becomes 32 — pins the loop's upper bound).
        assert_eq!(recover(0x8000_0000), (0, 153));
        // No compatibility flags → profile_idc stays 0.
        assert_eq!(recover(0x0000_0000), (0, 153));
        // A coded non-zero profile is never overwritten: coded 2 with the index-1
        // flag set stays 2 (recovery only triggers at profile_idc 0).
        assert_eq!(parse_profile_tier_level(2, 0x4000_0000, 153), (2, 153));
    }

    #[test]
    fn level_and_tier_variants() {
        let profile = |level: u64, tier: bool| {
            let cfg = SpsConfig { level_idc: level, tier_flag: tier, ..SpsConfig::default() };
            sps_stream(&cfg).encoding_profile
        };
        // dec == 0 → integer level; Main tier.
        assert_eq!(profile(90, false).as_deref(), Some("Main 10 @ Level 3 @ Main"));
        // dec >= 1 → one fractional digit.
        assert_eq!(profile(93, true).as_deref(), Some("Main 10 @ Level 3.1 @ High"));
        // level_idc 0 → no level suffix at all.
        assert_eq!(profile(0, true).as_deref(), Some("Main 10"));
    }

    #[test]
    fn profile_space_nonzero_skips_the_profile_but_still_initializes() {
        let cfg = SpsConfig { profile_space: 1, ..SpsConfig::default() };
        let s = sps_stream(&cfg);
        assert!(s.encoding_profile.is_none());
        assert!(s.base.is_initialized); // SeqParameterSets non-empty
    }

    #[test]
    fn sps_early_returns_register_no_parameter_set() {
        let none = |cfg: SpsConfig| {
            let s = sps_stream(&cfg);
            assert!(s.encoding_profile.is_none(), "expected no profile");
            assert!(!s.base.is_initialized, "expected uninitialized");
        };
        none(SpsConfig { vps_id: 1, ..SpsConfig::default() }); // video_parameter_set_id mismatch
        none(SpsConfig { chroma_format_idc: 4, ..SpsConfig::default() }); // chroma >= 4
        none(SpsConfig { bit_depth_luma_minus8: 7, ..SpsConfig::default() }); // luma > 6
        none(SpsConfig { bit_depth_chroma_minus8: 7, ..SpsConfig::default() }); // chroma depth > 6
        none(SpsConfig { log2_max_poc_minus4: 13, ..SpsConfig::default() }); // log2 > 12
        none(SpsConfig { sps_id: 64, ..SpsConfig::default() }); // id past the spec range (dropped)
    }

    #[test]
    fn an_undersized_sei_message_is_realigned_to_its_payload_size() {
        // A light-level message (four bytes read) whose declared payload size is
        // eight makes the dispatcher skip the four-byte remainder up to saved_pos so
        // the following mastering message stays aligned. (Light level avoids the
        // emulation-prevention bytes a zero-laden mastering payload would carry.)
        let light = [0x04, 0x09, 0x01, 0x37, 0x55, 0x66, 0x77, 0x88]; // MaxCLL 1033, MaxFALL 311
        let (s, _) = hdr_unit_with(
            4113,
            &[
                sei_message_raw(144, light.len(), &light),
                sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1)),
            ],
        );
        // The mastering message is reached only because the dispatcher realigned
        // past the four extra light-level bytes.
        let desc = s.description();
        assert!(desc.contains("Maximum Content Light Level: 1033"), "{desc}");
        assert!(desc.contains("Display P3"), "{desc}");
    }

    #[test]
    fn a_type4_sei_smaller_than_eight_bytes_realigns_backward_not_a_huge_skip() {
        // The type-4 (ITU-T T.35) handler reads a fixed 8-byte header, then
        // computes `payload_size - 8` in 32-bit math and signs the skip — so a
        // declared size < 8 wraps to a small *negative* (backward) reposition to the
        // message's end, NOT a ~2.1e9-iteration forward skip (which would hang). The
        // following light-level and mastering messages are reached only if that
        // realignment is correct, so their values appearing in `desc` proves it.
        let (s, _) = hdr_unit_with(
            4113,
            &[
                sei_message_raw(4, 2, &[0xB5, 0x00]), // declared size 2 (< 8) → underflow
                sei_message(144, &light_level_payload(1033, 311)),
                sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1)),
            ],
        );
        let desc = s.description();
        assert!(desc.contains("Maximum Content Light Level: 1033"), "{desc}");
        assert!(desc.contains("Display P3"), "{desc}");
    }

    #[test]
    fn chroma_format_strings_and_bit_depth() {
        let chroma = |idc: u32, sep: bool| {
            let cfg = SpsConfig {
                chroma_format_idc: idc,
                separate_colour_plane: sep,
                ..SpsConfig::default()
            };
            sps_desc(&cfg)
        };
        assert!(chroma(2, false).contains(" / 4:2:2 / "), "4:2:2");
        assert!(chroma(3, false).contains(" / 4:4:4 / "), "4:4:4");
        assert!(chroma(3, true).contains(" / 4:4:4 / "), "4:4:4 separate plane");
        // chroma_format_idc 0 → no chroma string (but the bit-depth one remains).
        let mono = chroma(0, false);
        assert!(!mono.contains("4:2:0"), "no chroma label");
        assert!(mono.contains(" 10 bits "), "bit depth still shown");
    }

    #[test]
    fn bit_depth_mismatch_omits_the_bits_fragment() {
        let cfg = SpsConfig { bit_depth_chroma_minus8: 0, ..SpsConfig::default() };
        let desc = sps_desc(&cfg);
        assert!(!desc.contains("bits"), "luma != chroma depth → no bits fragment: {desc}");
    }

    /// The `desc` the default HDR SPS yields with no SEI present.
    const DEFAULT_DESC: &str = "Main 10 @ Level 5.1 @ High / 4:2:0 / 10 bits / Limited Range / \
                                BT.2020 / PQ / BT.2020 non-constant";

    #[test]
    fn optional_sps_blocks_preserve_the_colour_position() {
        // Each optional block only advances the cursor; the colour description that
        // follows must still decode identically (proving correct bit positioning).
        let same = |cfg: SpsConfig| assert_eq!(sps_desc(&cfg), DEFAULT_DESC);
        same(SpsConfig { conformance_window: true, ..SpsConfig::default() });
        same(SpsConfig {
            scaling_list_enabled: true,
            scaling_list_present: true,
            ..SpsConfig::default()
        });
        same(SpsConfig {
            scaling_list_enabled: true,
            scaling_list_present: true,
            scaling_list_pred_mode: true,
            ..SpsConfig::default()
        });
        // scaling_list_enabled but not present → the inner block is skipped.
        same(SpsConfig { scaling_list_enabled: true, ..SpsConfig::default() });
        same(SpsConfig { pcm_enabled: true, ..SpsConfig::default() });
        same(SpsConfig { short_term_ref_pic_sets: 2, ..SpsConfig::default() });
        same(SpsConfig { inter_pred_rps: true, ..SpsConfig::default() });
        same(SpsConfig { long_term_present: true, num_long_term: 2, ..SpsConfig::default() });
        same(SpsConfig { sub_layer_ordering_present: false, ..SpsConfig::default() });
    }

    #[test]
    fn sub_layers_in_profile_tier_level_preserve_position() {
        // max_sub_layers_minus1 > 0 exercises the sub-layer present-flag loop, the
        // reserved_zero_2bits[i] block — (8 - max_sub_layers_minus1) two-bit fields,
        // per H.265 §7.3.3 / FFmpeg ps.c:360-362, NOT a single field — and the
        // per-sub-layer profile/level skips. Each `n` consumes a different reserved
        // count (14, 12, 2 bits for n = 1, 2, 7), so the colour description landing
        // on its BT.2020 PQ values pins that exact count: an under- or over-skip in
        // the reader (or writer) misaligns it and the desc changes.
        let with_sub_layers = |n: u16, flags: Vec<(bool, bool)>| {
            let cfg = SpsConfig {
                max_sub_layers_minus1: n,
                sub_layer_flags: flags,
                ..SpsConfig::default()
            };
            assert_eq!(sps_desc(&cfg), DEFAULT_DESC);
        };
        with_sub_layers(1, vec![(true, true)]); // profile + level data present
        with_sub_layers(1, vec![(false, false)]); // neither present
        with_sub_layers(2, vec![(true, false), (false, true)]); // two sub-layers
        with_sub_layers(7, vec![(false, false); 7]); // max sub-layers → minimal reserved (2 bits)
    }

    #[test]
    fn vui_variants_preserve_the_colour_position() {
        // Aspect-ratio / overscan presence shifts the colour description; it must
        // still decode to the same BT.2020 PQ values.
        let vui = |v: VuiConfig| {
            let cfg = SpsConfig { vui: v, ..SpsConfig::default() };
            assert_eq!(sps_desc(&cfg), DEFAULT_DESC);
        };
        vui(VuiConfig { aspect_ratio_present: true, aspect_ratio_idc: 1, ..VuiConfig::default() });
        vui(VuiConfig {
            aspect_ratio_present: true,
            aspect_ratio_idc: 0xFF,
            ..VuiConfig::default()
        });
        vui(VuiConfig { overscan_present: true, ..VuiConfig::default() });
    }

    #[test]
    fn vui_full_range_and_missing_colour_description() {
        // video_full_range_flag = 1 → "Full Range".
        let full = SpsConfig {
            vui: VuiConfig { full_range: 1, ..VuiConfig::default() },
            ..SpsConfig::default()
        };
        assert!(sps_desc(&full).contains(" / Full Range / "), "full range");

        // colour_description_present = 0 → range only, no primaries/transfer/matrix.
        let no_colour = SpsConfig {
            vui: VuiConfig { colour_description_present: false, ..VuiConfig::default() },
            ..SpsConfig::default()
        };
        assert_eq!(
            sps_desc(&no_colour),
            "Main 10 @ Level 5.1 @ High / 4:2:0 / 10 bits / Limited Range"
        );

        // video_signal_type_present = 0 → no range and no colour fragments at all.
        let no_signal = SpsConfig {
            vui: VuiConfig { video_signal_type_present: false, ..VuiConfig::default() },
            ..SpsConfig::default()
        };
        assert_eq!(sps_desc(&no_signal), "Main 10 @ Level 5.1 @ High / 4:2:0 / 10 bits");
    }

    #[test]
    fn bt2020_constant_matrix_still_gates_hdr() {
        // matrix_coefficients = 10 (BT.2020 constant) takes the second arm of the
        // HDR gate's `matrix == 9 || matrix == 10` and labels HDR10 all the same.
        let cfg = SpsConfig {
            vui: VuiConfig { matrix_coefficients: 10, ..VuiConfig::default() },
            ..SpsConfig::default()
        };
        let (s, _) = run(
            4113,
            &[
                vps(0),
                sps(&cfg),
                pps(0, 0, false, 0),
                sei_nal(&[sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1))]),
                slice(19, 0, 2),
            ],
        );
        let desc = s.description();
        assert!(desc.contains(" / HDR10 / "), "{desc}");
        assert!(desc.contains(" / BT.2020 constant"), "{desc}");
    }

    #[test]
    fn no_vui_present_still_initializes() {
        // vui_parameters_present_flag = 0 → VUI defaults (unspecified colour), so no
        // range/colour fragments, but the SPS still registers.
        let cfg = SpsConfig { vui_present: false, ..SpsConfig::default() };
        assert_eq!(sps_desc(&cfg), "Main 10 @ Level 5.1 @ High / 4:2:0 / 10 bits");
    }

    #[test]
    fn four_byte_start_codes_are_recognized() {
        // The first NAL and the slice use four-byte start codes, exercising the
        // four-byte branch of both the main and SEI start-code scans.
        let (s, tag) = run(
            4113,
            &[
                nal4(32, &{
                    let mut w = Writer::new();
                    w.u(0, 4);
                    w.byte(0x80);
                    w
                }),
                sps(&SpsConfig::default()),
                pps(0, 0, false, 0),
                sei_nal(&[sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1))]),
                nal4(19, &{
                    let mut w = Writer::new();
                    w.bit(true); // first_slice_segment_in_pic_flag
                    w.bit(false); // no_output_of_prior_pics_flag
                    w.ue(0); // slice_pic_parameter_set_id
                    w.ue(2); // slice_type = I
                    w.byte(0x80);
                    w
                }),
            ],
        );
        assert_eq!(tag.as_deref(), Some("I"));
        let desc = s.description();
        assert!(desc.contains("HDR10"), "{desc}");
    }

    /// Runs `[vps, sps, pps(dependent), …slices]` and returns the last tag.
    fn slice_unit(pps_dependent: bool, slices: &[Vec<u8>]) -> Option<String> {
        let mut nals = vec![vps(0), sps(&SpsConfig::default()), pps(0, 0, pps_dependent, 0)];
        nals.extend_from_slice(slices);
        run(4113, &nals).1
    }

    #[test]
    fn slice_type_maps_to_the_picture_tag() {
        assert_eq!(slice_unit(false, &[slice(19, 0, 0)]), Some("P".to_owned()));
        assert_eq!(slice_unit(false, &[slice(19, 0, 1)]), Some("B".to_owned()));
        assert_eq!(slice_unit(false, &[slice(19, 0, 2)]), Some("I".to_owned()));
        // slice_type > 2 → no tag.
        assert_eq!(slice_unit(false, &[slice(19, 0, 3)]), None);
    }

    #[test]
    fn non_irap_slice_skips_the_no_output_flag() {
        // nal_unit_type in 0..=9 (here 1, TRAIL_R) does not read
        // no_output_of_prior_pics_flag.
        assert_eq!(slice_unit(false, &[slice(1, 0, 2)]), Some("I".to_owned()));
    }

    #[test]
    fn num_extra_slice_header_bits_are_skipped() {
        // The PPS declares 3 reserved slice-header bits; the slice must skip them
        // before slice_type, or the type would be misread.
        let unit = |dependent_pps_extra: u64| {
            let nals = vec![
                vps(0),
                sps(&SpsConfig::default()),
                pps(0, 0, false, dependent_pps_extra),
                slice_seg(19, true, 0, u32::try_from(dependent_pps_extra).unwrap(), 2),
            ];
            run(4113, &nals).1
        };
        assert_eq!(unit(3), Some("I".to_owned()));
    }

    #[test]
    fn dependent_and_out_of_range_slices_yield_no_tag() {
        // first_slice = false with a dependent-enabled PPS reads the dependent flag.
        assert_eq!(slice_unit(true, &[slice_seg(19, false, 0, 0, 2)]), None);
        // first_slice = false with a non-dependent PPS returns immediately.
        assert_eq!(slice_unit(false, &[slice_seg(19, false, 0, 0, 2)]), None);
        // slice_pic_parameter_set_id past the registered PPS list → no tag.
        assert_eq!(slice_unit(false, &[slice(19, 7, 2)]), None);
    }

    #[test]
    fn pic_parameter_set_id_ranges_are_validated() {
        // pps_pic_parameter_set_id >= 64, pps_seq_parameter_set_id >= 16, and an
        // unregistered seq id each abort registration → the later slice finds no PPS.
        let unit = |pps_id: u32, seq_id: u32| {
            let nals = vec![
                vps(0),
                sps(&SpsConfig::default()),
                pps(pps_id, seq_id, false, 0),
                slice(19, 0, 2),
            ];
            run(4113, &nals).1
        };
        assert_eq!(unit(64, 0), None); // pps id out of range
        assert_eq!(unit(0, 16), None); // seq id out of range
        assert_eq!(unit(0, 5), None); // seq id not registered
        assert_eq!(unit(0, 1), None); // seq id == seq count (the `< len` boundary)
    }

    #[test]
    fn unknown_nal_types_and_unknown_sei_are_skipped() {
        // An access-unit delimiter (type 35) and an unknown SEI payload type are
        // both no-ops; the HDR SEI around them still applies.
        let (s, _) = run(
            4113,
            &[
                vps(0),
                sps(&SpsConfig::default()),
                pps(0, 0, false, 0),
                nal(35, &{
                    let mut w = Writer::new();
                    w.byte(0x80);
                    w
                }),
                sei_nal(&[
                    sei_message(100, &[1, 2, 3]), // unknown payload type → default skip
                    sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1)),
                    sei_message(144, &light_level_payload(1033, 311)),
                ]),
                slice(19, 0, 2),
            ],
        );
        let desc = s.description();
        assert!(desc.contains(" / HDR10 / "), "{desc}");
        assert!(desc.contains("Maximum Content Light Level: 1033"));
    }

    #[test]
    fn multi_byte_sei_payload_type_is_summed() {
        // payload_type 260 is encoded as 0xFF 0x05 (the continuation loop); it is an
        // unknown type, so it is skipped and the mastering SEI still applies.
        let unknown = vec![0xFF, 0x05, 0x03, 0x11, 0x22, 0x33];
        let (s, _) = hdr_unit_with(
            4113,
            &[unknown, sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1))],
        );
        let desc = s.description();
        assert!(desc.contains("Display P3"), "{desc}");
    }

    #[test]
    fn an_oversized_sei_payload_size_aborts_the_element() {
        // A declared payload size past the buffer end makes saved_pos exceed the
        // buffer length, so the SEI parse returns before reading anything.
        let nals = vec![
            vps(0),
            sps(&SpsConfig::default()),
            pps(0, 0, false, 0),
            sei_nal(&[sei_message_raw(137, 4096, &[0x00; 4])]),
            slice(19, 0, 2),
        ];
        let (s, _) = run(4113, &nals);
        // No mastering data was read → no HDR label.
        let desc = s.description();
        assert!(!desc.contains("HDR10"), "{desc}");
    }

    #[test]
    fn an_sei_at_the_buffer_end_terminates_the_scan() {
        // With no trailing start code, the boundary scan runs to the buffer end.
        let (s, _) = run(
            4113,
            &[
                vps(0),
                sps(&SpsConfig::default()),
                sei_nal(&[sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1))]),
            ],
        );
        let desc = s.description();
        assert!(desc.contains("Display P3"), "{desc}");
    }

    #[test]
    fn a_trailing_start_code_with_no_nal_body_ends_the_loop() {
        // A four-byte start code whose last byte is the final buffer byte is found,
        // but leaves the cursor at the end with no NAL header — the scan stops.
        // Must not panic or hang.
        let mut b = TsStreamBuffer::new();
        let data = [0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01];
        b.add(&data, 0, data.len());
        b.begin_read();
        let mut s = TsVideoStream::default();
        s.base.stream_type = TsStreamType::HevcVideo;
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        assert!(!s.base.is_initialized);
    }

    #[test]
    fn post_init_skips_parameter_sets_but_still_collects_hdr_sei() {
        // First scan initialises with no HDR SEI (just the colour line). The second
        // access unit's VPS/SPS/PPS hit the post-init early returns — so the SPS's
        // different profile is ignored — but its HDR SEIs are still collected and
        // fold into the format info (the sparse-metadata case).
        let mut s = TsVideoStream::default();
        s.base.stream_type = TsStreamType::HevcVideo;
        s.base.pid = Pid::new(4113);
        let mut b1 =
            buffer_of(&[vps(0), sps(&SpsConfig::default()), pps(0, 0, false, 0), slice(19, 0, 2)]);
        scan(&mut s, &mut b1, &mut None);
        assert!(s.base.is_initialized);
        assert_eq!(s.encoding_profile.as_deref(), Some("Main 10 @ Level 5.1 @ High"));
        let desc_before = s.description();
        assert!(!desc_before.contains("HDR10"), "no HDR label yet: {desc_before}");

        let mut b2 = buffer_of(&[
            vps(0),
            sps(&SpsConfig { profile_idc: 1, ..SpsConfig::default() }),
            pps(0, 0, false, 0),
            sei_nal(&hdr_messages_hdr10_plus()),
            slice(19, 0, 0),
        ]);
        let mut tag = None;
        scan(&mut s, &mut b2, &mut tag);
        // The slice tag updates; the post-init SPS is ignored (profile unchanged);
        // the post-init HDR SEIs surface the label and the mastering / light lines.
        assert_eq!(tag.as_deref(), Some("P"));
        assert_eq!(s.encoding_profile.as_deref(), Some("Main 10 @ Level 5.1 @ High"));
        let desc = s.description();
        assert!(desc.contains("HDR10+"), "{desc}");
        assert!(desc.contains("Mastering display color primaries: Display P3"), "{desc}");
        assert!(desc.contains("Maximum Content Light Level: 969 cd / m2"), "{desc}");
    }

    #[test]
    fn a_repeated_post_init_hdr_sei_is_not_duplicated() {
        // The same mastering SEI arriving on two access units must leave exactly one
        // mastering fragment — the reassembly clears before rebuilding, so it never
        // grows the lineage's per-occurrence duplicate run.
        let mut s = TsVideoStream::default();
        s.base.stream_type = TsStreamType::HevcVideo;
        s.base.pid = Pid::new(4113);
        let unit = || {
            vec![
                vps(0),
                sps(&SpsConfig::default()),
                pps(0, 0, false, 0),
                sei_nal(&[sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1))]),
                slice(19, 0, 2),
            ]
        };
        let mut b1 = buffer_of(&unit());
        scan(&mut s, &mut b1, &mut None);
        let mut b2 = buffer_of(&unit());
        scan(&mut s, &mut b2, &mut None);
        let desc = s.description();
        assert_eq!(
            desc.matches("Mastering display color primaries").count(),
            1,
            "mastering fragment must appear once: {desc}"
        );
        assert_eq!(desc.matches("4:2:0").count(), 1, "no fragment duplicated: {desc}");
    }

    #[test]
    fn an_hdr10_plus_stream_without_a_mastering_display_is_labelled_hdr10_plus() {
        // A valid HDR10+ stream (ST 2094-40 T.35) that carries no static mastering
        // display SEI still earns the HDR10+ label — the label rides on the dynamic
        // metadata, not on the mastering SEI the BDInfo lineage requires.
        let (s, _) = hdr_unit_with(4113, &[sei_message(4, &hdr10_plus_payload())]);
        let desc = s.description();
        assert!(desc.contains("HDR10+"), "{desc}");
        assert!(!desc.contains("Mastering display"), "no mastering line exists: {desc}");
        assert!(desc.contains("10 bits / HDR10+ / Limited Range"), "{desc}");
    }

    /// Like [`hdr_unit`] but the SEI messages are given directly (no light-level
    /// default), at `pid`.
    fn hdr_unit_with(pid: u16, messages: &[Vec<u8>]) -> (TsVideoStream, Option<String>) {
        run(
            pid,
            &[
                vps(0),
                sps(&SpsConfig::default()),
                pps(0, 0, false, 0),
                sei_nal(messages),
                slice(19, 0, 2),
            ],
        )
    }

    /// The `desc` of an HDR unit whose only SEI is a mastering message with the
    /// given coordinates / luminance.
    fn mastering_desc(coords: [u16; 8], max_lum: u32, min_lum: u32) -> String {
        hdr_unit_with(4113, &[sei_message(137, &mastering_payload(coords, max_lum, min_lum))])
            .0
            .description()
    }

    #[test]
    fn mastering_display_matches_each_known_colour_volume() {
        let primaries = |coords: [u16; 8]| {
            let d = mastering_desc(coords, 10_000_000, 1);
            d.split("Mastering display color primaries: ")
                .nth(1)
                .and_then(|s| s.split(" / ").next())
                .unwrap()
                .to_owned()
        };
        assert_eq!(primaries([15000, 30000, 7500, 3000, 32000, 16500, 15635, 16450]), "BT.709");
        assert_eq!(primaries([8500, 39850, 6550, 2300, 35400, 14600, 15635, 16450]), "BT.2020");
        assert_eq!(primaries([13250, 34500, 7500, 3000, 34000, 16000, 15700, 17550]), "DCI P3");
    }

    #[test]
    fn unrecognized_primaries_format_as_raw_coordinates() {
        // Wire G, B, R remaps to R = wire[2], G = wire[0], B = wire[1]; these
        // coordinates match no known colour volume → the raw chromaticity string.
        let desc = mastering_desc([1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000], 10_000_000, 1);
        assert!(
            desc.contains(
                "Mastering display color primaries: R: x=0.100000 y=0.120000, G: x=0.020000 \
                 y=0.040000, B: x=0.060000 y=0.080000, White point: x=0.140000 y=0.160000"
            ),
            "{desc}"
        );
    }

    #[test]
    fn a_max_value_primary_is_invalid_and_formats_raw() {
        // A 0xFFFF coordinate marks the data invalid → the raw format is used.
        let desc =
            mastering_desc([13250, 34500, 7500, 3000, 34000, 16000, 15635, 0xFFFF], 10_000_000, 1);
        assert!(desc.contains("White point: x=0.312700 y=1.310700"), "{desc}");
    }

    #[test]
    fn mastering_luminance_uses_fractional_max_above_the_int_range() {
        // A max luminance above i32::MAX fails the low-32-bit whole-number
        // reinterpretation test → four fractional digits.
        let desc = mastering_desc(DISPLAY_P3_COORDS, 0x8000_0000, 1);
        assert!(desc.contains("max: 214748.3648 cd/m2"), "{desc}");
    }

    #[test]
    fn hdr10_plus_registration_requires_the_prefix_and_a_window_in_range() {
        // The ST 2094-40 registration is identified by the country / provider /
        // oriented / application-id prefix alone; application_version is
        // unrestricted and num_windows must be 1..3 (matching FFmpeg). Each case
        // has a present mastering display + the 10-bit BT.2020 PQ gate, so the
        // label is HDR10, or HDR10+ with a valid T.35 (PID 4113).
        let label = |t35: Vec<u8>| {
            let (s, _) = hdr_unit_with(
                4113,
                &[
                    sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1)),
                    sei_message(4, &t35),
                ],
            );
            if s.description().contains("HDR10+") { "HDR10+" } else { "HDR10" }
        };
        // Canonical prefix; version 0/1/2 and num_windows 1/2/3 all → HDR10+
        // (version is ignored, the window count is in range).
        assert_eq!(label(vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x00, 0x40]), "HDR10+"); // v0, 1 win
        assert_eq!(label(vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x01, 0x40]), "HDR10+"); // version 1
        assert_eq!(label(vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x02, 0x40]), "HDR10+"); // version 2
        assert_eq!(label(vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x00, 0x80]), "HDR10+"); // 2 windows
        assert_eq!(label(vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x00, 0xC0]), "HDR10+"); // 3 windows
        // Each prefix field is still required, and num_windows 0 is rejected.
        assert_eq!(label(vec![0xB6, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x00, 0x40]), "HDR10"); // country
        assert_eq!(label(vec![0xB5, 0x00, 0x3D, 0x00, 0x01, 0x04, 0x00, 0x40]), "HDR10"); // provider
        assert_eq!(label(vec![0xB5, 0x00, 0x3C, 0x00, 0x02, 0x04, 0x00, 0x40]), "HDR10"); // oriented
        assert_eq!(label(vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x05, 0x00, 0x40]), "HDR10"); // app id
        assert_eq!(label(vec![0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04, 0x00, 0x00]), "HDR10"); // 0 windows
    }

    /// Calls [`Hevc::scan_should_continue`] over a `len`-byte buffer positioned at
    /// `pos`, with the given init / frame-type flags.
    fn should_continue(len: usize, pos: i64, is_initialized: bool, frame_type_read: bool) -> bool {
        let mut ext = HevcExtendedData::default();
        let mut b = TsStreamBuffer::new();
        b.add(&vec![0_u8; len], 0, len);
        b.begin_read();
        b.seek(pos, crate::bitstream::SeekOrigin::Begin);
        let h = Hevc { ext: &mut ext, buffer: &mut b, is_initialized };
        h.scan_should_continue(frame_type_read)
    }

    #[test]
    fn scan_should_continue_guards_position_and_init() {
        // Position strictly below length-3 with work left → keep scanning.
        assert!(should_continue(10, 0, false, false));
        // At exactly length-3 the `<` boundary stops (a `<=` mutant would not).
        assert!(!should_continue(10, 7, false, false));
        // An uninitialised stream keeps going even once a frame type is read.
        assert!(should_continue(10, 0, false, true));
        // An initialised stream stops as soon as a frame type is read (pins the
        // `!is_initialized` term — deleting the `!` would keep scanning).
        assert!(!should_continue(10, 0, true, true));
        // An initialised stream not yet frame-typed keeps scanning.
        assert!(should_continue(10, 0, true, false));
    }

    /// Runs [`Hevc::find_start_code`] over `data` and returns `(found, position)`.
    fn find_start(data: &[u8]) -> (bool, u64) {
        let mut ext = HevcExtendedData::default();
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        let mut h = Hevc { ext: &mut ext, buffer: &mut b, is_initialized: false };
        let found = matches!(h.find_start_code(false), StartCode::Found);
        (found, h.buffer.position())
    }

    #[test]
    fn find_start_code_locates_three_and_four_byte_codes() {
        // Three-byte `00 00 01` → cursor just past it.
        assert_eq!(find_start(&[0x00, 0x00, 0x01, 0xAA, 0xBB, 0xCC, 0xDD]), (true, 3));
        // Four-byte `00 00 00 01` → cursor just past it.
        assert_eq!(find_start(&[0x00, 0x00, 0x00, 0x01, 0xAA, 0xBB, 0xCC, 0xDD]), (true, 4));
        // A leading non-zero byte before a three-byte code.
        assert_eq!(find_start(&[0xFF, 0x00, 0x00, 0x01, 0xAA, 0xBB, 0xCC]), (true, 4));
    }

    #[test]
    fn find_start_code_rejects_near_miss_byte_patterns() {
        // None of these contains a real `00 00 01`; only a mutated byte comparison
        // in the four-byte probe (`00 00 00 01`) would spuriously match them.
        assert!(!find_start(&[0x00, 0x00, 0x00, 0x02, 0xFF, 0xFF, 0xFF, 0xFF]).0); // b3 != 1
        assert!(!find_start(&[0x00, 0x00, 0x02, 0x01, 0xFF, 0xFF, 0xFF, 0xFF]).0); // b2 != 0
        assert!(!find_start(&[0x00, 0x02, 0x00, 0x01, 0xFF, 0xFF, 0xFF, 0xFF]).0); // b1 != 0
    }

    /// Runs [`Hevc::probe_start_code`] over `data`, returning `(result, position)`.
    fn probe(data: &[u8]) -> (Option<u64>, u64) {
        let mut ext = HevcExtendedData::default();
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        let mut h = Hevc { ext: &mut ext, buffer: &mut b, is_initialized: false };
        let result = h.probe_start_code();
        (result, h.buffer.position())
    }

    #[test]
    fn probe_start_code_classifies_each_byte() {
        // The two real start codes, cursor left just past them.
        assert_eq!(probe(&[0x00, 0x00, 0x00, 0x01, 0xAA]), (Some(4), 4));
        assert_eq!(probe(&[0x00, 0x00, 0x01, 0xAA]), (Some(3), 3));
        // Each near-miss is one byte off the pattern; a mutated comparison in the
        // four- or three-byte probe would spuriously match it.
        assert_eq!(probe(&[0xFF, 0x00, 0x00, 0x01, 0xAA]).0, None); // 4-byte byte 0
        assert_eq!(probe(&[0x00, 0xFF, 0x00, 0x01, 0xAA]).0, None); // 4-byte byte 1
        assert_eq!(probe(&[0x00, 0x00, 0xFF, 0x01, 0xAA]).0, None); // 4-byte byte 2
        assert_eq!(probe(&[0x00, 0x00, 0x00, 0xFF, 0xAA]).0, None); // 4-byte byte 3
        assert_eq!(probe(&[0xFF, 0x00, 0x01, 0xAA]).0, None); // 3-byte byte 0
        assert_eq!(probe(&[0x00, 0xFF, 0x01, 0xAA]).0, None); // 3-byte byte 1
        assert_eq!(probe(&[0x00, 0x00, 0xFF, 0xAA]).0, None); // 3-byte byte 2
    }

    #[test]
    fn sei_bytes_converts_byte_counts() {
        assert_eq!(sei_bytes(0), 0);
        assert_eq!(sei_bytes(24), 24);
        assert_eq!(sei_bytes(4096), 4096);
    }

    #[test]
    fn match_color_volume_requires_every_coordinate_aligned() {
        // Display P3 in the standard G, B, R order matches code 12.
        assert_eq!(
            match_color_volume(&[13250, 34500, 7500, 3000, 34000, 16000, 15635, 16450], 0, 1, 2),
            Some(12)
        );
        // Display P3 with the green channel at index 2 still matches at the two
        // coordinate pairs the matcher checks (`j < 2`); a `<= 2` mutant would
        // read a third, misaligned coordinate and reject the match.
        assert_eq!(
            match_color_volume(&[7500, 3000, 34000, 16000, 13250, 34500, 15635, 16450], 2, 0, 1),
            Some(12)
        );
        // Coordinates far from any table row match nothing.
        assert_eq!(match_color_volume(&[0; 8], 0, 1, 2), None);
    }

    #[test]
    fn primary_and_white_point_tolerances_are_half_open() {
        let reference = [100, 0, 0, 0, 0, 0, 0, 0];
        // Just inside the upper bound (`expected + 25`) is within; the bound itself
        // is not (the test is `actual < expected + 25`).
        assert!(within(&[124, 0, 0, 0, 0, 0, 0, 0], 0, 0, &reference, 0));
        assert!(!within(&[125, 0, 0, 0, 0, 0, 0, 0], 0, 0, &reference, 0));
        // The white point is tighter (`< expected + 3`).
        let white = [0, 0, 0, 0, 0, 0, 100, 0];
        assert!(within_white(&[0, 0, 0, 0, 0, 0, 102, 0], 0, &white));
        assert!(!within_white(&[0, 0, 0, 0, 0, 0, 103, 0], 0, &white));
    }

    #[test]
    fn an_sei_message_ending_exactly_at_the_buffer_end_is_parsed() {
        // A 24-byte mastering payload with no zero byte → no emulation, so the
        // message ends exactly at the buffer end (saved_pos == length); the
        // `>` guard parses it (a `>=` mutant would wrongly reject it).
        let coords = [0x1111, 0x2222, 0x3333, 0x4444, 0x5555, 0x6666, 0x7777, 0x8888];
        let payload = mastering_payload(coords, 0x1122_3344, 0x5566_7788);
        let mut data = vec![137_u8, 24];
        data.extend_from_slice(&payload);
        assert_eq!(data.len(), 26);
        let mut ext = HevcExtendedData::default();
        let mut b = TsStreamBuffer::new();
        b.add(&data, 0, data.len());
        b.begin_read();
        Hevc { ext: &mut ext, buffer: &mut b, is_initialized: false }.sei();
        assert!(!ext.mastering_display_color_primaries.is_empty());
    }

    #[test]
    fn bit_depth_and_log2_at_their_maxima_still_register() {
        // bit_depth_luma/chroma_minus8 = 6 (→ "14 bits") and log2 = 12 are the
        // inclusive upper bounds; a `> -> >=` mutant on any guard would reject them.
        let cfg = SpsConfig {
            bit_depth_luma_minus8: 6,
            bit_depth_chroma_minus8: 6,
            log2_max_poc_minus4: 12,
            ..SpsConfig::default()
        };
        let desc = sps_desc(&cfg);
        assert!(desc.contains(" / 14 bits"), "{desc}");
    }

    #[test]
    fn zero_max_content_light_level_omits_the_light_level_lines() {
        // A light-level SEI with MaxCLL = 0 sets light_level_available, but the gate
        // also requires MaxCLL > 0 — so the light-level lines are omitted.
        let (s, _) = hdr_unit_with(
            4113,
            &[
                sei_message(137, &mastering_payload(DISPLAY_P3_COORDS, 10_000_000, 1)),
                sei_message(144, &light_level_payload(0, 0)),
            ],
        );
        let desc = s.description();
        assert!(!desc.contains("Maximum Content Light Level"), "{desc}");
        assert!(desc.contains("Display P3"), "{desc}"); // mastering still present
    }

    #[test]
    fn a_malformed_sps_ref_pic_set_count_terminates_quickly() {
        // The libFuzzer `codec` timeout artifact (selector byte 15 → HEVC dropped):
        // a crafted SPS whose RBSP emulation runs make `num_short_term_ref_pic_sets`
        // decode as a huge `ue(v)` (684_043 here), and the per-set pic counts grow
        // too — left unclamped, those loops would run unboundedly (a multi-minute
        // non-termination on hostile bytes). The spec-maxima clamps on the SPS
        // ref-pic-set counts bound every loop, so this returns promptly.
        // The test merely *completing* (nextest's slow-timeout would otherwise fail
        // it) is the assertion that the non-termination is fixed.
        let unit: &[u8] = &[
            0, 0, 1, 64, 1, 8, 0, 0, 0, 1, 66, 1, 0, 34, 0, 0, 3, 0, 0, 3, 0, 0, 3, 1, 0, 0, 0, 0,
            0, 0, 1, 127, 240, 137, 248, 183, 127, 174, 0, 0, 0, 1, 68, 1, 193, 0, 0, 0, 1, 78, 1,
            137, 24, 51, 194, 134, 196, 29, 76, 11, 126, 132, 208, 62, 128, 61, 19, 64, 66, 0, 152,
            150, 128, 0, 0, 3, 0, 1, 144, 4, 4, 9, 1, 55, 128, 0, 0, 1, 38, 1, 174, 0,
        ];
        let mut s = TsVideoStream::default();
        s.base.stream_type = TsStreamType::HevcVideo;
        let mut b = TsStreamBuffer::new();
        b.add(unit, 0, unit.len());
        b.begin_read();
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        // No panic, no hang. (The bit cursor never runs the ref-pic loops billions
        // of times — it stops at end-of-content.)
    }

    #[test]
    fn a_malformed_sps_outer_ref_pic_set_count_terminates() {
        // Drives the outer-loop clamp: `num_short_term_ref_pic_sets` ~2^31
        // with no sets. The `MAX_SHORT_TERM_REF_PIC_SETS` clamp caps the loop at 64;
        // removing it makes the loop run ~2^31 times → a hang nextest's slow-timeout
        // catches (so the mutation is killed, not missed).
        let cfg =
            SpsConfig { malformed_rps: Some(MalformedRps::OuterCount), ..SpsConfig::default() };
        let (stream, _) = run(0, &[vps(0), sps(&cfg), slice(19, 0, 2)]);
        assert_eq!(stream.base.stream_type, TsStreamType::HevcVideo); // completed: no hang, no panic
    }

    #[test]
    fn a_malformed_sps_inner_pic_counts_terminate() {
        // Drives the per-set inner clamps (D15 #8): one short-term set declaring
        // `num_negative_pics` AND `num_positive_pics` ~2^31 with no entries. Each is
        // clamped to `MAX_DELTA_POCS_PER_SET` (16); removing either clamp makes the
        // matching loop run ~2^31 times → a hang nextest's slow-timeout catches.
        let cfg =
            SpsConfig { malformed_rps: Some(MalformedRps::InnerPicCounts), ..SpsConfig::default() };
        let (stream, _) = run(0, &[vps(0), sps(&cfg), slice(19, 0, 2)]);
        assert_eq!(stream.base.stream_type, TsStreamType::HevcVideo); // completed: no hang, no panic
    }

    #[test]
    fn a_malformed_sps_long_term_count_terminates() {
        // Drives the long-term ref-pic clamp (D15 #8): `num_long_term_ref_pics_sps`
        // ~2^31 with no entries. The `MAX_LONG_TERM_REF_PICS` clamp caps the loop at
        // 32; removing it makes it run ~2^31 times → a hang nextest's slow-timeout
        // catches.
        let cfg =
            SpsConfig { malformed_rps: Some(MalformedRps::LongTermCount), ..SpsConfig::default() };
        let (stream, _) = run(0, &[vps(0), sps(&cfg), slice(19, 0, 2)]);
        assert_eq!(stream.base.stream_type, TsStreamType::HevcVideo); // completed: no hang, no panic
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>()) {
            let mut s = TsVideoStream::default();
            s.base.stream_type = TsStreamType::HevcVideo;
            let mut b = TsStreamBuffer::new();
            b.add(&data, 0, data.len());
            b.begin_read();
            let mut tag = None;
            scan(&mut s, &mut b, &mut tag);
        }
    }
}
