//! Transport-stream (`*.m2ts`) demuxer — the 192-byte BDAV packet state machine.
//!
//! [`TsStreamFile::scan`] streams a `*.m2ts` (or `*.ssif`) in fixed chunks, walks
//! the 192-byte source packets (4-byte `TP_extra_header` + 188-byte TS packet,
//! resynchronising on the `0x47` sync byte), follows the PAT to the PMT and the
//! PMT to the elementary streams (registering each in
//! [`streams`](TsStreamFile::streams)), assembles each PID's PES payloads into
//! the [`TsStreamBuffer`], reads the PTS/DTS timestamps (as `i128`), and
//! accumulates per-clip / per-stream byte and bitrate diagnostics into the
//! playlists.
//!
//! The demux ends at the codec seam — the [`scan`](TsStreamFile::scan) point
//! where each assembled access unit is handed to the per-codec analysers
//! ([`crate::codec`]), which fill the stream's codec fields and its initialised
//! flag. SSIF 3D interleaving is layered on this core by the caller: the
//! per-file scan in [`super::disc`] opens the interleaved `*.ssif`
//! ([`super::interleaved`]) in preference to the plain `*.m2ts` and hands that
//! reader to [`scan`](TsStreamFile::scan). A `*.ssif` is just packet-aligned
//! base/dependent extents, so the same per-byte state machine de-interleaves
//! them onto the shared PID/PES path.
//!
//! Implementation notes:
//! * **`u64` everywhere for file scale** ([`size`](TsStreamFile::size)); the in-memory chunk index
//!   is `i64`, with every byte access bounds-checked ([`byte_at`]) — a malformed index yields `0`
//!   (disc bytes are never indexed raw).
//! * **Fixed-width packet math wraps** (`wrapping_*`); the BE shift-accumulate steps use
//!   `wrapping_add` over their **disjoint** bit ranges (identical to `|=`, and — unlike `|=` — not
//!   equivalent under the mutation gate's `| → ^`).
//! * **PTS/DTS via `i128`**; the first timestamp byte's `parse & 0xE` is widened *before* the `<<
//!   29`, so the full 33-bit timestamp is preserved. This diverges from classic `BDInfo`, which
//!   evaluates the shift in 32-bit and drops bit 32 (any PTS past ~13.26 h wraps); real discs never
//!   reach that range, so reports stay byte-identical. The other four byte-arms mask and shift
//!   exactly as before, with `wrapping_add` over their disjoint bit ranges.
//! * **No dead state.** Header fields the analysis never reads (the time code, the
//!   transport-error/priority bits, the adaptation-field PCR reconstruction, the transport-stream
//!   id, the PMT program/version bytes, and the program-info descriptors) are consumed by the same
//!   countdowns — framing is identical — but never stored: a dead store can be neither tested nor
//!   mutation-covered. The audio `bitrate` is computed for its observable side effect (the peak
//!   transfer rate) and threaded into the codec dispatch.

use std::collections::BTreeMap;
use std::io::Read;
// The cancel flag is polled on every target; the channel/thread machinery of
// the read-ahead pipeline exists only where threads do —
// `wasm32-unknown-unknown` has none, so the wasm build takes a sequential
// read-then-parse path instead (see `scan_sequential`) and never names them.
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc;
#[cfg(not(target_arch = "wasm32"))]
use std::thread;

use super::interleaved::TsInterleavedFile;
use super::measured::{self, MeasuredSnapshot};
use super::mpls::TsPlaylistFile;
use crate::bitstream::TsStreamBuffer;
use crate::error::BdError;
use crate::primitives::Pid;
use crate::stream::{StreamKind, TsStream, TsStreamType};

/// Read-chunk size (256 KiB) for each underlying full-pass read. The packet
/// state machine is chunk-boundary-agnostic, so the value changes no healthy
/// output byte (tests drive a small size to exercise the cross-chunk deferral
/// paths) — it sets read granularity, and with it three damaged-media bounds:
/// how long one blocking read can stall (a stalled optical request spans the
/// chunk's 128 optical sectors of in-device retries), how stale the per-chunk cancel
/// poll ([`fill_buffer`]) can get, and how many bytes a failed read discards
/// (a read error voids only its own chunk, so the demux keeps everything up
/// to the last completed chunk boundary). Tunable within those trade-offs.
/// The size is a deliberate divergence from classic `BDInfo` 0.8's 5 MiB —
/// see DIFFERENCES.md, "Damaged-media read granularity".
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const DATA_SIZE: usize = 262_144;

/// Read-chunk size (5 MiB) for each underlying full-pass read on
/// `wasm32-unknown-unknown`. The browser read seam pays one synchronous
/// `FileReaderSync` round trip per read call, so the wasm build keeps the
/// large chunk (20× fewer calls than the native 256 KiB); browser reads of
/// damaged media return promptly (a thrown exception, not an in-device
/// retry), so the native chunk's stall/cancel/loss bounds buy nothing there.
#[cfg(target_arch = "wasm32")]
pub(crate) const DATA_SIZE: usize = 5_242_880;

/// Read-chunk size (16 KiB) for each underlying quick-pass read. The quick
/// codec pass stops at the first completed access unit past which every
/// registered stream's codec detail is initialised, so its chunk size bounds
/// the read-ahead past that point — and on a file whose head is unreadable,
/// how many bytes one blocking read can stall on before the failure is
/// recorded and the pass moves to the next file. Diverges deliberately from
/// classic `BDInfo` 0.8 (5 MiB for both passes) — see DIFFERENCES.md,
/// "Damaged-media read granularity".
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const QUICK_DATA_SIZE: usize = 16_384;

/// Quick-pass read-chunk size on `wasm32-unknown-unknown`: [`DATA_SIZE`]'s
/// 5 MiB, for the same per-read round-trip economy.
#[cfg(target_arch = "wasm32")]
pub(crate) const QUICK_DATA_SIZE: usize = DATA_SIZE;

/// The read-chunk size the default scan entries select for a pass: the full
/// measurement pass reads [`DATA_SIZE`] chunks, the quick codec pass
/// [`QUICK_DATA_SIZE`].
const fn default_chunk_size(is_full_scan: bool) -> usize {
    if is_full_scan { DATA_SIZE } else { QUICK_DATA_SIZE }
}

/// Size of the fixed `PAT`/`PMT` section-assembly buffers.
const SECTION_SIZE: usize = 1024;

/// Reads `buf[i]` over the chunk, returning `0` for an out-of-range index —
/// the memory-safe read for the state machine's signed chunk index. Valid
/// indices produced by the state machine are always in bounds.
fn byte_at(buf: &[u8], i: i64) -> u8 {
    usize::try_from(i).ok().and_then(|u| buf.get(u)).copied().unwrap_or(0)
}

/// Truncates `x` to its low 8 bits (`x & 0xFF`), which is well-defined for
/// negative values too (two's-complement low byte).
fn to_byte(x: i64) -> u8 {
    u8::try_from(x & 0xFF).unwrap_or(0)
}

/// Widens an `i128` timestamp to `f64`. Real PTS/DTS values are ≤ 2^33 (the demux
/// keeps the full 33 bits), so this is exact in practice (f64's 52-bit
/// mantissa represents every integer ≤ 2^33).
#[expect(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "PTS/DTS values fit f64 exactly (≤ 2^33) (int→float; TryFrom inapplicable)"
)]
const fn pts_to_f64(v: i128) -> f64 {
    v as f64
}

/// Widens a `u64` byte count to `f64` for bitrate math. Payload byte counts
/// stay well under 2^53, so this is exact in practice.
#[expect(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "payload byte counts fit f64 exactly (< 2^53) (int→float; TryFrom inapplicable)"
)]
pub(crate) const fn bytes_to_f64(v: u64) -> f64 {
    v as f64
}

/// Rounds half-to-even and narrows to `i64` — the bitrate rounding rule. The
/// float→int cast saturates on the unreachable non-finite case (a guarded
/// division by zero would otherwise be `inf`).
#[expect(
    clippy::cast_possible_truncation,
    clippy::as_conversions,
    reason = "bitrates fit i64, saturating on the unreachable non-finite case (float→int; TryFrom inapplicable)"
)]
pub(crate) const fn round_long(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Sets the stream's bit rate from its accumulated payload over
/// `packet_seconds` when the stream is variable-bitrate — the VBR pass body.
fn apply_vbr_bitrate(stream: &mut TsStream, packet_seconds: f64) {
    if stream.base().is_vbr {
        stream.base_mut().bit_rate =
            round_long(bytes_to_f64(stream.base().payload_bytes) * 8.0 / packet_seconds);
    }
}

/// Fills `buf` from `reader`, returning the number of bytes read (0 at EOF),
/// looping to tolerate short reads.
///
/// Polls the cooperative `cancel` flag before reading: a set flag aborts with
/// [`BdError::ScanCancelled`] instead of pulling more bytes — the per-chunk
/// cancellation point both the threaded and the sequential scan paths share
/// (one relaxed load per chunk; the per-byte hot path never sees it).
fn fill_buffer(
    reader: &mut dyn Read,
    buf: &mut [u8],
    cancel: &AtomicBool,
) -> Result<usize, BdError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(BdError::ScanCancelled);
    }
    let mut total: usize = 0;
    // `filter` stops the loop once the destination slice is empty (buffer full),
    // so there is no `total < len` boundary to mutate equivalently.
    while let Some(dst) = buf.get_mut(total..).filter(|s| !s.is_empty()) {
        match reader.read(dst) {
            Ok(0) => break,
            Ok(n) => total = total.saturating_add(n),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(BdError::Io(e)),
        }
    }
    Ok(total)
}

/// One video-stream bitrate sample.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TsStreamDiagnostics {
    /// Payload bytes in this window.
    pub bytes: u64,
    /// Transport packets in this window.
    pub packets: u64,
    /// Time in seconds (`PTS / 90000`) of the frame that closed this window:
    /// one entry is emitted per completed window, stamped with its closing
    /// frame's time, not its start. Exception: the end-of-scan flush closing
    /// each stream's final window stamps it with the running maximum video
    /// timestamp across streams, so on an interleaved (MVC) scan a dependent
    /// view's last entry can carry the base view's time and interval.
    pub marker: f64,
    /// Window length in seconds (the PTS delta `/ 90000`).
    pub interval: f64,
    /// Codec stream tag; `None` until a codec scanner sets it.
    pub tag: Option<String>,
}

/// Per-PID demux state — the fields the core demux reads (write-only
/// diagnostics are omitted, see the module note).
#[derive(Debug, Default)]
struct TsStreamState {
    /// Transport packets seen on this PID.
    total_packets: u64,
    /// Transport packets in the current bitrate window.
    window_packets: u64,
    /// Payload bytes seen on this PID.
    total_bytes: u64,
    /// Payload bytes in the current bitrate window.
    window_bytes: u64,
    /// Completed-PES count.
    transfer_count: u64,
    /// Largest single-PES payload, bytes.
    peak_transfer_length: i64,
    /// Largest audio bitrate observed, bits/s.
    peak_transfer_rate: i64,
    /// The assembled PES payload for the current access unit.
    stream_buffer: TsStreamBuffer,
    /// Rolling last-four-bytes window for start-code detection.
    parse: u32,
    /// Whether the parser is mid-PES-payload transfer.
    transfer_state: bool,
    /// Bytes transferred by the last copy.
    transfer_length: i32,
    /// Remaining PES-packet length, bytes.
    packet_length: i32,
    /// Whether the PES declared an unbounded length.
    packet_length_variable: bool,
    /// Countdown over the 2-byte PES packet-length field.
    packet_length_parse: u8,
    /// Countdown over the 3-byte PES optional-header prefix.
    packet_parse: u8,
    /// Countdown over the 5-byte PTS field.
    pts_parse: u8,
    /// Current presentation timestamp.
    pts: i128,
    /// Accumulator for the timestamp being assembled.
    pts_temp: u64,
    /// Last/peak presentation timestamp.
    pts_last: i128,
    /// `pts - dts_prev` for the current access unit.
    pts_diff: i128,
    /// Number of timestamps seen.
    pts_count: u64,
    /// Inter-frame PTS delta for the audio bitrate estimate.
    pts_transfer: i128,
    /// Countdown over the 10-byte PTS+DTS field.
    dts_parse: u8,
    /// Current decode timestamp.
    dts_temp: i128,
    /// Previous access unit's decode timestamp.
    dts_prev: i128,
    /// Remaining PES optional-header length, bytes.
    pes_header_length: u8,
    /// PES optional-header flags byte.
    pes_header_flags: u8,
    /// The frame marker the codec seam set for the last completed PES (a video
    /// picture type), or `None` when it recognised no frame. Cleared before
    /// every codec dispatch; recorded into the bitrate diagnostics.
    stream_tag: Option<String>,
    /// Mirror of the stream's `is_initialized` flag (refreshed at the codec
    /// seam) so the per-byte loop needs no `streams` lookup.
    stream_initialized: bool,
    /// Mirror of the stream's kind (set at registration), for the same reason.
    /// A PID with no registered stream keeps the [`StreamKind`] default, which
    /// matches none of the three kinds the per-byte loop dispatches on.
    stream_kind: StreamKind,
}

/// Who owned a byte stepped through [`SectionAssembler::header_byte`].
enum SectionByte {
    /// The pointer-field, length-prefix, or a shared header step consumed it.
    Taken,
    /// A section-header byte at this post-decrement countdown position; the
    /// table's own walk owns the positions below `last_section_number`'s, and
    /// the decision to start the body copy.
    Header(u8),
    /// No section is in progress — inter-section padding, consumed unread.
    Idle,
}

/// One MPEG-2 PSI section-assembly state machine (ISO 13818-1 §2.4.4):
/// pointer-field skip, the 3-byte length prefix with its table-id check and
/// `> 1021` reject, the section-header countdown, and the body copy into a
/// section buffer. The PAT and PMT walks each drive one instance. The buffer
/// lives with the caller — the PMT keys its buffers by PID — and so do the
/// completed-table entry walks, whose layouts are per-table.
struct SectionAssembler {
    /// The only accepted `table_id` (`0x00` PAT, `0x02` PMT); any other value
    /// at the length-prefix step rejects the section.
    table_id: u8,
    /// Length of the section header behind the 3-byte prefix. Both table
    /// headers open with the same five bytes (table-id extension, version,
    /// `section_number`, `last_section_number`), so the shared header
    /// positions sit a fixed distance below this value.
    header: u8,
    /// Whether the next byte is the pointer field.
    section_start: bool,
    /// Remaining pointer-field bytes to skip.
    pointer_field: u8,
    /// Write cursor into the caller's section buffer.
    offset: u32,
    /// Countdown over the 3-byte section-length prefix.
    length_parse: u8,
    /// Remaining section bytes.
    section_length: u16,
    /// Countdown over the section header.
    section_parse: u8,
    /// Whether the section body is being copied.
    transfer_state: bool,
    /// Current section number.
    section_number: u8,
    /// Final section number.
    last_section_number: u8,
}

impl SectionAssembler {
    /// Builds an idle assembler accepting `table_id` sections with a
    /// `header`-byte section header.
    const fn new(table_id: u8, header: u8) -> Self {
        Self {
            table_id,
            header,
            section_start: false,
            pointer_field: 0,
            offset: 0,
            length_parse: 0,
            section_length: 0,
            section_parse: 0,
            transfer_state: false,
            section_number: 0,
            last_section_number: 0,
        }
    }

    /// Steps the pre-body walk with one byte: pointer field, length prefix,
    /// then section header. The shared header positions (`section_number`
    /// with its section-0 buffer reset, `last_section_number`) are applied
    /// here; every header position is also reported back so the table's own
    /// walk can read its tail fields.
    fn header_byte(&mut self, b: u8) -> SectionByte {
        if self.section_start {
            self.pointer_field = b;
            if self.pointer_field == 0 {
                self.length_parse = 3;
            }
            self.section_start = false;
            SectionByte::Taken
        } else if self.pointer_field > 0 {
            self.pointer_field = self.pointer_field.wrapping_sub(1);
            if self.pointer_field == 0 {
                self.length_parse = 3;
            }
            SectionByte::Taken
        } else if self.length_parse > 0 {
            self.length_parse = self.length_parse.wrapping_sub(1);
            match self.length_parse {
                2 => {
                    // A wrong table id abandons the section, leaving the
                    // machine idle until the next payload-unit start. Classic
                    // BDInfo checks only the PMT's id and assembles a
                    // mislabeled PAT — see DIFFERENCES.md.
                    if b != self.table_id {
                        self.length_parse = 0;
                    }
                }
                1 => self.section_length = u16::from(b & 0xF).wrapping_shl(8),
                // The 3-byte prefix countdown only ever reaches 0 here.
                _ => {
                    self.section_length = self.section_length.wrapping_add(u16::from(b));
                    if self.section_length > 1021 {
                        self.section_length = 0;
                    } else {
                        self.section_parse = self.header;
                    }
                }
            }
            SectionByte::Taken
        } else if self.section_parse > 0 {
            self.section_length = self.section_length.wrapping_sub(1);
            self.section_parse = self.section_parse.wrapping_sub(1);
            if self.section_parse == self.header.wrapping_sub(4) {
                self.section_number = b;
                if b == 0 {
                    self.offset = 0;
                }
            } else if self.section_parse == self.header.wrapping_sub(5) {
                self.last_section_number = b;
            }
            SectionByte::Header(self.section_parse)
        } else {
            SectionByte::Idle
        }
    }

    /// Copies section-body bytes from `buffer` (cursor `*i`, chunk length
    /// `bl`) into `section`, bounded by the remaining section, chunk, and
    /// packet bytes; `*offset` reports the count and `*i` lands on the last
    /// byte consumed. Returns whether this run completed the table (the final
    /// section finished) — the caller's cue to walk the assembled entries.
    fn transfer(
        &mut self,
        buffer: &[u8],
        section: &mut [u8],
        i: &mut i64,
        bl: i64,
        offset: &mut i64,
        packet_length: &mut u8,
    ) -> bool {
        // The PAT and PMT copies of this clamp compared the remaining chunk
        // with `>` and `>=` respectively — equality-identical, since at
        // equality both branches produce the same offset. `.min` keeps the
        // unified form free of that idempotent-comparison mutant.
        *offset =
            i64::from(self.section_length).min(bl.wrapping_sub(*i)).min(i64::from(*packet_length));
        let mut sink = 0_u8;
        let mut k: i64 = 0;
        while k < *offset {
            let bb = byte_at(buffer, *i);
            let po = usize::try_from(self.offset).unwrap_or(usize::MAX);
            // A malformed section overflowing the 1024-byte buffer writes to a
            // discard sink rather than panicking.
            *section.get_mut(po).unwrap_or(&mut sink) = bb;
            self.offset = self.offset.wrapping_add(1);
            *i = i.wrapping_add(1);
            self.section_length = self.section_length.wrapping_sub(1);
            *packet_length = packet_length.wrapping_sub(1);
            k = k.wrapping_add(1);
        }
        *i = i.wrapping_sub(1);
        if self.section_length == 0 {
            self.transfer_state = false;
            return self.section_number == self.last_section_number;
        }
        false
    }
}

/// The whole-file packet parser — the fields the core demux reads (write-only
/// diagnostics are omitted, see the module note).
///
/// A private scan-local; it deliberately omits a `Debug` derive (nothing formats
/// it, and the derive would be uncoverable dead code).
#[expect(
    clippy::struct_excessive_bools,
    reason = "the flags are independent positions in the packet state machine, not a single state"
)]
struct TsPacketParser {
    /// Whether a `0x47` sync byte has been locked.
    sync_state: bool,
    /// Countdown over the 4-byte `TP_extra_header` time code.
    time_code_parse: u8,
    /// Bytes remaining in the current 188-byte TS packet.
    packet_length: u8,
    /// Countdown over the 3-byte TS header.
    header_parse: u8,
    /// Whether the current TS packet starts a new payload unit.
    payload_unit_start_indicator: u8,
    /// The current TS packet's PID.
    pid: u16,
    /// The current TS packet's scrambling control bits.
    transport_scrambling_control: u8,
    /// The current TS packet's adaptation-field control bits.
    adaption_field_control: u8,
    /// Whether the next byte is the adaptation-field length.
    adaption_field_state: bool,
    /// Bytes remaining in the adaptation field.
    adaption_field_parse: u8,
    /// Whether a variable-length PES is being closed at this packet end.
    variable_packet_end: bool,
    /// The discovered PMT PID.
    pmt_pid: u16,
    /// PAT section assembly (5-byte header: through `last_section_number`).
    pat_assembler: SectionAssembler,
    /// PAT section-assembly buffer.
    pat: Vec<u8>,
    /// PMT section assembly (9-byte header: the shared five bytes, then the
    /// PCR PID and `program_info_length` pairs).
    pmt_assembler: SectionAssembler,
    /// PMT section-assembly buffers keyed by PID.
    pmt: BTreeMap<u16, Vec<u8>>,
    /// Remaining PMT program-info bytes.
    pmt_program_info_length: u16,
    /// Smallest DTS seen across video streams, for the clip length.
    pts_first: i128,
    /// Largest DTS seen across video streams, for the clip length.
    pts_last: i128,
    /// Whether the current PID is a registered elementary stream.
    stream_present: bool,
}

impl TsPacketParser {
    /// Builds a parser with its sentinel initial values: `pmt_pid` `0xFFFF`
    /// (undiscovered), `pts_first` at the `u64` maximum and `pts_last` at `0` so
    /// the running min/max work from the first sample.
    fn new() -> Self {
        Self {
            sync_state: false,
            time_code_parse: 4,
            packet_length: 0,
            header_parse: 0,
            payload_unit_start_indicator: 0,
            pid: 0,
            transport_scrambling_control: 0,
            adaption_field_control: 0,
            adaption_field_state: false,
            adaption_field_parse: 0,
            variable_packet_end: false,
            pmt_pid: 0xFFFF,
            pat_assembler: SectionAssembler::new(0x00, 5),
            pat: vec![0_u8; SECTION_SIZE],
            pmt_assembler: SectionAssembler::new(0x02, 9),
            pmt: BTreeMap::new(),
            pmt_program_info_length: 0,
            pts_first: i128::from(u64::MAX),
            pts_last: 0,
            stream_present: false,
        }
    }
}

/// A demuxed `*.m2ts` clip.
///
/// Built with [`TsStreamFile::new`] from the clip's (upper-cased) file name, then
/// filled by [`scan`](TsStreamFile::scan): the elementary [`streams`](Self::streams)
/// (PID/type/language registered from the PMT), the total [`size`](Self::size) and
/// presentation [`length`](Self::length), and the per-video-stream
/// [`stream_diagnostics`](Self::stream_diagnostics).
#[derive(Debug)]
pub struct TsStreamFile {
    /// The clip's upper-cased file name, e.g. `"00017.M2TS"`.
    pub name: String,
    /// Total bytes read from the clip.
    pub size: u64,
    /// Presentation length in seconds.
    pub length: f64,
    /// The interleaved 3D source (`*.ssif`), when this clip has one. When
    /// present, the per-file scan in [`super::disc`] opens it and streams it
    /// through [`scan`](Self::scan) instead of the plain `*.m2ts` (see
    /// [`super::interleaved`]).
    pub interleaved_file: Option<TsInterleavedFile>,
    /// The elementary streams keyed by PID, registered from the PMT.
    pub streams: BTreeMap<u16, TsStream>,
    /// The stream PIDs in first-registration order — the order the PMT walk
    /// first saw each stream in the file. For an interleaved (`*.ssif`) scan
    /// the dependent view's table arrives first, so its PID leads.
    pub stream_order: Vec<u16>,
    /// Per-PID demux state. Private — the parser's scratch.
    stream_states: BTreeMap<u16, TsStreamState>,
    /// Per-video-PID bitrate samples.
    pub stream_diagnostics: BTreeMap<u16, Vec<TsStreamDiagnostics>>,
}

impl TsStreamFile {
    /// Creates an empty demuxer for the clip named `name`, upper-casing it.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_ascii_uppercase(),
            size: 0,
            length: 0.0,
            interleaved_file: None,
            streams: BTreeMap::new(),
            stream_order: Vec::new(),
            stream_states: BTreeMap::new(),
            stream_diagnostics: BTreeMap::new(),
        }
    }

    /// Releases the per-PID demux scratch ([`stream_states`](Self::stream_states))
    /// once a completed scan's public outputs (`streams`, `stream_order`,
    /// `stream_diagnostics`, `size`, `length`) have been captured.
    ///
    /// Pure memory reclaim with no effect on any output: `stream_states` is
    /// private scratch the demux fills while parsing — including the per-PID PES
    /// reassembly buffers, which reach the 5 MiB PES clamp
    /// ([`TsStreamBuffer`]'s `BUFFER_SIZE`) for video — and nothing reads it once
    /// the scan returns. Each scanned clip is otherwise retained (in both
    /// measurement passes) in the disc scan's result map with this scratch alive,
    /// so a multi-clip disc holds every clip's buffers at once at peak. The demux
    /// unit tests inspect `stream_states` after a scan and so must not call this.
    pub(crate) fn release_scratch(&mut self) {
        self.stream_states = BTreeMap::new();
    }

    /// The name shown for this clip — the interleaved `*.ssif`
    /// [`name`](TsInterleavedFile::name) when present and SSIF reading is enabled,
    /// else the `*.m2ts` [`name`](Self::name).
    #[must_use]
    pub fn display_name(&self, enable_ssif: bool) -> &str {
        match &self.interleaved_file {
            Some(interleaved) if enable_ssif => interleaved.name(),
            _ => &self.name,
        }
    }

    /// Streams and demuxes the clip from `reader`, accumulating bitrate
    /// diagnostics into `playlists`.
    ///
    /// # Errors
    /// Returns [`BdError::Io`] if reading the underlying stream fails. Malformed
    /// packet data never errors: it is resynchronised on the next `0x47` or read
    /// as zero bytes.
    pub fn scan(
        &mut self,
        reader: &mut dyn Read,
        playlists: &mut [TsPlaylistFile],
        is_full_scan: bool,
    ) -> Result<(), BdError> {
        self.scan_chunked(
            reader,
            playlists,
            is_full_scan,
            default_chunk_size(is_full_scan),
            &mut |_, _| {},
        )
    }

    /// [`scan`](Self::scan) with a cooperative `cancel` flag and an optional
    /// measured-tally observer — the demux entry the disc-level open drives.
    /// The flag is polled once per read chunk ([`fill_buffer`]); when set, the
    /// scan aborts promptly with [`BdError::ScanCancelled`] instead of reading
    /// further. On a full scan, `measured` is handed an owned
    /// [`MeasuredSnapshot`] of the playlists this clip can touch after each
    /// parsed chunk (delivered on the calling thread); a non-full scan never
    /// snapshots — its tallies are a partial sliver a display must not show.
    pub(crate) fn scan_cancellable(
        &mut self,
        reader: &mut dyn Read,
        playlists: &mut [TsPlaylistFile],
        is_full_scan: bool,
        cancel: &AtomicBool,
        measured: Option<&mut (dyn FnMut(MeasuredSnapshot) + '_)>,
    ) -> Result<(), BdError> {
        self.scan_chunked_with(
            reader,
            playlists,
            is_full_scan,
            default_chunk_size(is_full_scan),
            cancel,
            &mut |_, _| {},
            measured,
        )
    }

    /// [`scan`](Self::scan) with an explicit read-chunk size and a codec-seam
    /// observer (the point where the assembled buffer is decoded);
    /// `observe(pid, buffer)` runs just before the buffer is reset. Used by tests
    /// to drive cross-chunk boundaries and inspect the assembled PES payload
    /// (never cancelled — the cancellation tests drive
    /// [`scan_cancellable`](Self::scan_cancellable) and the strategy entries
    /// directly).
    pub(crate) fn scan_chunked(
        &mut self,
        reader: &mut dyn Read,
        playlists: &mut [TsPlaylistFile],
        is_full_scan: bool,
        chunk_size: usize,
        observe: &mut (dyn FnMut(u16, &TsStreamBuffer) + Send),
    ) -> Result<(), BdError> {
        self.scan_chunked_with(
            reader,
            playlists,
            is_full_scan,
            chunk_size,
            &AtomicBool::new(false),
            observe,
            None,
        )
    }

    /// The full-parameter scan behind every entry above — explicit chunk
    /// size, cooperative `cancel` flag, codec-seam observer, and optional
    /// measured-tally observer — dispatched to the execution strategy the
    /// build target supports:
    /// [`scan_threaded`](Self::scan_threaded) wherever threads exist,
    /// [`scan_sequential`](Self::scan_sequential) on `wasm32-unknown-unknown`
    /// (which has none).
    #[expect(
        clippy::too_many_arguments,
        reason = "the one dispatch point threads every scan entry's parameters (source, targets, pass, chunking, cancellation, both observers) to the strategy"
    )]
    fn scan_chunked_with(
        &mut self,
        reader: &mut dyn Read,
        playlists: &mut [TsPlaylistFile],
        is_full_scan: bool,
        chunk_size: usize,
        cancel: &AtomicBool,
        observe: &mut (dyn FnMut(u16, &TsStreamBuffer) + Send),
        measured: Option<&mut (dyn FnMut(MeasuredSnapshot) + '_)>,
    ) -> Result<(), BdError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.scan_threaded_observed(
                reader,
                playlists,
                is_full_scan,
                chunk_size,
                cancel,
                observe,
                measured,
            )
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.scan_sequential_observed(
                reader,
                playlists,
                is_full_scan,
                chunk_size,
                cancel,
                observe,
                measured,
            )
        }
    }

    /// Shared per-clip setup for both scan strategies: resets the demux state
    /// accumulated by any previous scan, clamps `chunk_size` to at least one
    /// byte, and pre-filters `playlists` down to the ones this clip can touch.
    /// Returns `None` when `playlists` is empty — the scan is a no-op then,
    /// with the previous state (and the reader) left untouched.
    fn begin_scan<'p>(
        &mut self,
        playlists: &'p mut [TsPlaylistFile],
        chunk_size: usize,
    ) -> Option<(TsPacketParser, Vec<&'p mut TsPlaylistFile>, usize)> {
        if playlists.is_empty() {
            return None;
        }
        self.size = 0;
        self.length = 0.0;
        self.streams.clear();
        self.stream_order.clear();
        self.stream_states.clear();
        self.stream_diagnostics.clear();

        // Only the playlists that reference this clip can be touched by the
        // per-window bitrate attribution (the clip-name guard inside
        // `update_stream_bitrate`), so restrict the per-frame playlist walks
        // to them up front — the same per-file playlist map the classic tool
        // builds before scanning.
        let file_name = self.name.clone();
        let relevant: Vec<&'p mut TsPlaylistFile> = playlists
            .iter_mut()
            .filter(|p| p.stream_clips.iter().any(|c| c.name == file_name))
            .collect();
        Some((TsPacketParser::new(), relevant, chunk_size.max(1)))
    }

    /// The threaded scan strategy — the one every build with threads runs
    /// behind [`scan`](Self::scan): the reader stays on the calling thread
    /// (whose progress callback it drives), while the packet state machine
    /// runs on a scoped worker thread fed whole chunks through a bounded
    /// channel — the next chunk is read while the previous one is parsed.
    /// Spent buffers flow back on a second channel, so at most three are ever
    /// allocated. The threads only overlap the reader's IO with the parser's
    /// CPU: the demux output is byte-for-byte that of
    /// [`scan_sequential`](Self::scan_sequential), whose equivalence contract
    /// names the shared roads.
    ///
    /// Every chunk is pulled through the shared `fill_buffer`, whose `cancel`
    /// poll aborts the scan between chunks with [`BdError::ScanCancelled`] —
    /// here on the reader side: it stops feeding, the worker drains and exits
    /// without the bitrate tail (the read-failure road), and the cancelled
    /// error is returned. `observe(pid, buffer)` is the codec seam, run on the
    /// worker just before each assembled PES buffer is reset.
    ///
    /// # Errors
    /// Returns [`BdError::Io`] if reading fails and [`BdError::ScanCancelled`]
    /// when `cancel` is set. Malformed packet data never errors: it is
    /// resynchronised on the next `0x47`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn scan_threaded(
        &mut self,
        reader: &mut dyn Read,
        playlists: &mut [TsPlaylistFile],
        is_full_scan: bool,
        chunk_size: usize,
        cancel: &AtomicBool,
        observe: &mut (dyn FnMut(u16, &TsStreamBuffer) + Send),
    ) -> Result<(), BdError> {
        self.scan_threaded_observed(
            reader,
            playlists,
            is_full_scan,
            chunk_size,
            cancel,
            observe,
            None,
        )
    }

    /// [`scan_threaded`](Self::scan_threaded) with an optional measured-tally
    /// observer: on a full scan, each parsed chunk is followed by an owned
    /// [`MeasuredSnapshot`] of the playlists this clip can touch. The observer
    /// type is deliberately not `Send` (a browser caller's observer closes
    /// over a JavaScript function), so the worker — which owns the tallies —
    /// builds
    /// each owned snapshot and hands it over a channel to the calling thread,
    /// which runs the observer between its reads and drains the channel once
    /// the worker is done. Both strategies therefore hand the same snapshot
    /// sequence to the observer: one per parsed chunk, in parse order, on the
    /// calling thread — snapshots are state derived from the compared demux
    /// output, so the equivalence contract on
    /// [`scan_sequential`](Self::scan_sequential) needs no third comparison.
    ///
    /// # Errors
    /// As [`scan_threaded`](Self::scan_threaded); snapshots of the chunks
    /// parsed before a read failure or cancellation are still delivered.
    #[cfg(not(target_arch = "wasm32"))]
    #[expect(
        clippy::too_many_arguments,
        reason = "the strategy takes every scan entry's parameters (source, targets, pass, chunking, cancellation, both observers) in one call"
    )]
    pub(crate) fn scan_threaded_observed(
        &mut self,
        reader: &mut dyn Read,
        playlists: &mut [TsPlaylistFile],
        is_full_scan: bool,
        chunk_size: usize,
        cancel: &AtomicBool,
        observe: &mut (dyn FnMut(u16, &TsStreamBuffer) + Send),
        measured: Option<&mut (dyn FnMut(MeasuredSnapshot) + '_)>,
    ) -> Result<(), BdError> {
        let Some((mut parser, mut relevant, chunk_size)) = self.begin_scan(playlists, chunk_size)
        else {
            return Ok(());
        };
        // A non-full scan's tallies are the head sliver the quick pass happens
        // to read (and the disc-level open discards them before measuring), so
        // only the full pass reports them.
        let mut watcher = if is_full_scan { measured } else { None };
        let snapshotting = watcher.is_some();
        // Set when the reader fails: the worker then skips the bitrate tail,
        // abandoning the scan exactly where the classic sequential loop would.
        let read_failed = AtomicBool::new(false);
        // Set when a non-full scan finishes early: the classic loop returned
        // `Ok` there without reading any further, so a read-ahead failure past
        // that point must not surface either.
        let finished_early = AtomicBool::new(false);
        let mut read_result: Result<(), BdError> = Ok(());
        let this = &mut *self;
        let (full_tx, full_rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let (free_tx, free_rx) = mpsc::sync_channel::<Vec<u8>>(2);
        // Unbounded so the worker never blocks on a slow observer; at most the
        // chunks in flight (three buffers' worth) are ever pending.
        let (snapshot_tx, snapshot_rx) = mpsc::channel::<MeasuredSnapshot>();
        thread::scope(|scope| {
            let read_failed = &read_failed;
            let finished_early = &finished_early;
            scope.spawn(move || {
                while let Ok(buffer) = full_rx.recv() {
                    if this.parse_chunk(&buffer, &mut parser, &mut relevant, is_full_scan, observe)
                    {
                        // The early finish: dropping the receiver stops the
                        // reader, and the bitrate tail is skipped — the classic
                        // mid-scan return.
                        finished_early.store(true, Ordering::SeqCst);
                        return;
                    }
                    if snapshotting {
                        // Sent before the buffer is recycled, so once the
                        // reader holds this chunk's returned buffer the
                        // snapshot is already waiting for its next drain. The
                        // send cannot fail — the receiver outlives the worker
                        // (the scope joins it) — so `drop` only discards the
                        // must-use `Result`.
                        drop(snapshot_tx.send(measured::snapshot(
                            &this.name,
                            relevant.iter().map(|playlist| &**playlist),
                        )));
                    }
                    // Hand the spent buffer back for reuse (a no-op if the
                    // reader is already gone).
                    drop(free_tx.send(buffer));
                }
                if read_failed.load(Ordering::SeqCst) {
                    return;
                }
                this.finish_scan(&mut relevant);
            });
            // Two fresh buffers prime the pipeline — one in flight to the
            // parser, one being filled — which double-buffers the read against
            // the parse; afterwards each iteration blocks on a recycled one (a
            // closed channel means the worker finished early — stop reading).
            // The parse is the bottleneck (the reader outruns it on any disk),
            // so a deeper read-ahead only holds more idle buffers, not speed.
            let mut fresh: u8 = 2;
            loop {
                // Deliver the snapshots the worker has produced so far — the
                // mid-scan liveness this observer exists for; the rest arrive
                // in the drain after the loop.
                if let Some(watch) = watcher.as_deref_mut() {
                    while let Ok(snapshot) = snapshot_rx.try_recv() {
                        watch(snapshot);
                    }
                }
                let mut buffer = if fresh > 0 {
                    fresh = fresh.wrapping_sub(1);
                    vec![0_u8; chunk_size]
                } else {
                    match free_rx.recv() {
                        Ok(recycled) => recycled,
                        Err(_) => break,
                    }
                };
                buffer.resize(chunk_size, 0);
                match fill_buffer(reader, &mut buffer, cancel) {
                    Ok(0) => break,
                    Ok(n) => {
                        buffer.truncate(n);
                        // A send failure means the worker finished early; the
                        // next recv above then breaks the loop.
                        drop(full_tx.send(buffer));
                    }
                    Err(e) => {
                        read_failed.store(true, Ordering::SeqCst);
                        read_result = Err(e);
                        break;
                    }
                }
            }
            drop(full_tx); // end-of-stream for the worker
            // The worker may still be parsing chunks already read; deliver
            // their snapshots too (`recv` returns `Err` once the worker exits
            // and its sender drops), so the observer ends the scan holding
            // every parsed chunk's tallies whether the reader finished, failed
            // or was cancelled — exactly the sequential strategy's sequence.
            if let Some(watch) = watcher {
                while let Ok(snapshot) = snapshot_rx.recv() {
                    watch(snapshot);
                }
            }
        });
        if finished_early.load(Ordering::SeqCst) {
            // The scan finished before the reader did; any read-ahead error
            // happened past the classic stop point and never existed for the
            // sequential flow.
            return Ok(());
        }
        read_result
    }

    /// The sequential scan strategy — the one the `wasm32-unknown-unknown`
    /// build (which has no threads) runs behind [`scan`](Self::scan): read one
    /// chunk into a single reused buffer, parse it, repeat, all on the calling
    /// thread. It compiles and runs on every target, so a native build can
    /// drive it directly and hold it against the threaded strategy — the
    /// wasm32 demux is testable without a wasm runtime.
    ///
    /// Equivalence contract: both strategies feed `parse_chunk` the very same
    /// chunks in the very same order, poll `cancel` at the very same per-chunk
    /// boundary (`fill_buffer`), and call `finish_scan` under the very same
    /// condition, so the demux output is byte-for-byte identical whichever
    /// strategy a build runs. Held rather than asserted: the
    /// `m2ts_differential` fuzz target (`fuzz/fuzz_targets/`) runs both
    /// strategies over the same bytes and compares the result, the demuxed clip
    /// and the playlist writes — every `f64` by its bits — across read-chunk
    /// sizes, both scan modes and an injected read failure. Its in-tree mirror
    /// is the `sequential_demux_matches_the_threaded_demux` proptest below.
    ///
    /// # Errors
    /// Returns [`BdError::Io`] if reading fails and [`BdError::ScanCancelled`]
    /// when `cancel` is set. Malformed packet data never errors: it is
    /// resynchronised on the next `0x47`.
    pub fn scan_sequential(
        &mut self,
        reader: &mut dyn Read,
        playlists: &mut [TsPlaylistFile],
        is_full_scan: bool,
        chunk_size: usize,
        cancel: &AtomicBool,
        observe: &mut (dyn FnMut(u16, &TsStreamBuffer) + Send),
    ) -> Result<(), BdError> {
        self.scan_sequential_observed(
            reader,
            playlists,
            is_full_scan,
            chunk_size,
            cancel,
            observe,
            None,
        )
    }

    /// [`scan_sequential`](Self::scan_sequential) with an optional
    /// measured-tally observer, run inline after each parsed chunk of a full
    /// scan — the same snapshot sequence
    /// [`scan_threaded_observed`](Self::scan_threaded_observed) delivers, with
    /// no channel because everything already runs on the calling thread.
    ///
    /// # Errors
    /// As [`scan_sequential`](Self::scan_sequential); snapshots of the chunks
    /// parsed before a read failure or cancellation are still delivered.
    #[expect(
        clippy::too_many_arguments,
        reason = "the strategy takes every scan entry's parameters (source, targets, pass, chunking, cancellation, both observers) in one call"
    )]
    pub(crate) fn scan_sequential_observed(
        &mut self,
        reader: &mut dyn Read,
        playlists: &mut [TsPlaylistFile],
        is_full_scan: bool,
        chunk_size: usize,
        cancel: &AtomicBool,
        observe: &mut (dyn FnMut(u16, &TsStreamBuffer) + Send),
        measured: Option<&mut (dyn FnMut(MeasuredSnapshot) + '_)>,
    ) -> Result<(), BdError> {
        let Some((mut parser, mut relevant, chunk_size)) = self.begin_scan(playlists, chunk_size)
        else {
            return Ok(());
        };
        // A non-full scan's tallies are the head sliver the quick pass happens
        // to read (and the disc-level open discards them before measuring), so
        // only the full pass reports them.
        let mut watcher = if is_full_scan { measured } else { None };
        // Case for case against the threaded strategy: `parse_chunk` returning
        // `true` is the early finish (the worker's `finished_early` → `Ok`); a
        // read error returns without the bitrate tail (the worker's
        // `read_failed` road); a clean EOF runs `finish_scan` exactly as the
        // worker does on a closed channel with no read failure.
        //
        // One buffer for the whole scan, grown back to `chunk_size` each
        // iteration (mirroring the threaded worker's recycle of a single
        // `Vec`), so the sequential demux allocates once, not per chunk.
        let mut buffer = vec![0_u8; chunk_size];
        loop {
            buffer.resize(chunk_size, 0);
            match fill_buffer(reader, &mut buffer, cancel) {
                Ok(0) => break,
                Ok(n) => {
                    buffer.truncate(n);
                    if self.parse_chunk(&buffer, &mut parser, &mut relevant, is_full_scan, observe)
                    {
                        return Ok(());
                    }
                    if let Some(watch) = watcher.as_deref_mut() {
                        watch(measured::snapshot(
                            &self.name,
                            relevant.iter().map(|playlist| &**playlist),
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        self.finish_scan(&mut relevant);
        Ok(())
    }

    /// Walks one read chunk through the per-byte packet state machine,
    /// returning `true` when a non-full scan has finished early (every stream
    /// initialised — the caller stops feeding chunks and skips the tail).
    #[expect(
        clippy::too_many_lines,
        reason = "the per-byte packet state machine is one indivisible loop; splitting it would scatter the parser state"
    )]
    fn parse_chunk(
        &mut self,
        buffer: &[u8],
        parser: &mut TsPacketParser,
        relevant: &mut Vec<&mut TsPlaylistFile>,
        is_full_scan: bool,
        observe: &mut (dyn FnMut(u16, &TsStreamBuffer) + Send),
    ) -> bool {
        {
            let bl = i64::try_from(buffer.len()).unwrap_or(i64::MAX);
            let mut offset: i64 = 0;
            let mut i: i64 = 0;
            while i < bl {
                let b = byte_at(buffer, i);
                if !parser.sync_state {
                    if parser.time_code_parse > 0 {
                        // 4-byte TP_extra_header time code consumed; value unread.
                        parser.time_code_parse = parser.time_code_parse.wrapping_sub(1);
                    } else if b == 0x47 {
                        parser.sync_state = true;
                        parser.packet_length = 187;
                        parser.time_code_parse = 4;
                        parser.header_parse = 3;
                    }
                } else if parser.header_parse > 0 {
                    parser.packet_length = parser.packet_length.wrapping_sub(1);
                    parser.header_parse = parser.header_parse.wrapping_sub(1);
                    match parser.header_parse {
                        2 => {
                            parser.payload_unit_start_indicator = b.wrapping_shr(6) & 0x1;
                            parser.pid = u16::from(b & 0x1F).wrapping_shl(8);
                        }
                        1 => {
                            parser.pid = parser.pid.wrapping_add(u16::from(b));
                            parser.stream_present = self.streams.contains_key(&parser.pid);
                            let state = self.stream_states.entry(parser.pid).or_default();
                            state.total_packets = state.total_packets.wrapping_add(1);
                            state.window_packets = state.window_packets.wrapping_add(1);
                        }
                        // The countdown only ever reaches 0 here (it started at 3).
                        _ => {
                            parser.transport_scrambling_control = b.wrapping_shr(6) & 0x3;
                            parser.adaption_field_control = b.wrapping_shr(4) & 0x3;
                            if (parser.adaption_field_control & 0x2) == 0x2 {
                                parser.adaption_field_state = true;
                            }
                            if parser.payload_unit_start_indicator == 1 {
                                if parser.pid == 0 {
                                    parser.pat_assembler.section_start = true;
                                } else if parser.pid == parser.pmt_pid {
                                    parser.pmt_assembler.section_start = true;
                                } else {
                                    let ts = self
                                        .stream_states
                                        .get(&parser.pid)
                                        .is_some_and(|s| s.transfer_state);
                                    if ts {
                                        let state =
                                            self.stream_states.entry(parser.pid).or_default();
                                        state.transfer_state = false;
                                        state.transfer_count = state.transfer_count.wrapping_add(1);
                                        let finished = self.scan_stream(
                                            parser.pid,
                                            is_full_scan,
                                            &mut *observe,
                                        );
                                        if !is_full_scan && finished {
                                            return true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if parser.adaption_field_state {
                    parser.packet_length = parser.packet_length.wrapping_sub(1);
                    // Clamp the adaptation-field length to the bytes left in this
                    // packet. On a spec-valid packet (AF length <= 183) this is a
                    // no-op; on malformed input (length > the remaining packet) it
                    // stops the oversized field from bleeding its leftover countdown
                    // into the next packet's payload — libbluray rejects any payload
                    // offset >= 188 outright.
                    parser.adaption_field_parse = b.min(parser.packet_length);
                    parser.adaption_field_state = false;
                    parser.variable_packet_end = true;
                } else if parser.adaption_field_parse > 0 {
                    // Adaptation bytes (incl. any PCR, which the demux never reads)
                    // consumed; only the countdown matters for framing.
                    parser.packet_length = parser.packet_length.wrapping_sub(1);
                    parser.adaption_field_parse = parser.adaption_field_parse.wrapping_sub(1);
                    if parser.packet_length == 0 {
                        parser.sync_state = false;
                    }
                } else if parser.pid == 0 {
                    Self::parse_pat(parser, buffer, &mut i, bl, &mut offset);
                    if parser.packet_length == 0 {
                        parser.sync_state = false;
                    }
                } else if parser.pid == parser.pmt_pid {
                    self.parse_pmt(parser, buffer, &mut i, bl, &mut offset, is_full_scan);
                    if parser.packet_length == 0 {
                        parser.sync_state = false;
                    }
                } else if parser.stream_present && parser.transport_scrambling_control == 0 {
                    let pid = parser.pid;
                    // One state lookup per byte; the stream's kind/init flags
                    // are mirrored into the state by `create_stream` and the
                    // codec seam, so no `streams` lookup is needed here.
                    let state = self.stream_states.entry(pid).or_default();
                    state.parse = state.parse.wrapping_shl(8).wrapping_add(u32::from(b));
                    let (is_init, is_video, is_audio, is_graphics) = (
                        state.stream_initialized,
                        state.stream_kind == StreamKind::Video,
                        state.stream_kind == StreamKind::Audio,
                        state.stream_kind == StreamKind::Graphics,
                    );
                    if state.transfer_state {
                        let mut do_scan = false;
                        {
                            if (bl.wrapping_sub(i)) >= i64::from(state.packet_length)
                                && state.packet_length > 0
                                && !state.packet_length_variable
                            {
                                offset = i64::from(state.packet_length);
                            } else if (bl.wrapping_sub(i)) >= i64::from(parser.packet_length)
                                && parser.packet_length > 0
                                && state.packet_length_variable
                            {
                                offset = i64::from(parser.packet_length);
                            } else {
                                offset = bl.wrapping_sub(i);
                            }
                            if i64::from(parser.packet_length) <= offset {
                                offset = i64::from(parser.packet_length);
                            }
                            state.transfer_length = i32::try_from(offset).unwrap_or(i32::MAX);
                            let len = usize::try_from(offset).unwrap_or(0);
                            let pos = usize::try_from(i).unwrap_or(0);
                            if !is_init || is_video || is_graphics {
                                state.stream_buffer.add(buffer, pos, len);
                            } else {
                                state.stream_buffer.add_transfer_length(len);
                            }
                            i = i.wrapping_add(i64::from(state.transfer_length)).wrapping_sub(1);
                            state.packet_length =
                                state.packet_length.wrapping_sub(state.transfer_length);
                            parser.packet_length = parser
                                .packet_length
                                .wrapping_sub(to_byte(i64::from(state.transfer_length)));
                            let tl = u64::try_from(state.transfer_length).unwrap_or(0);
                            state.total_bytes = state.total_bytes.wrapping_add(tl);
                            state.window_bytes = state.window_bytes.wrapping_add(tl);
                            if parser.variable_packet_end && state.packet_length_variable {
                                parser.variable_packet_end = false;
                                state.packet_length_variable = false;
                            }
                            if state.packet_length == 0 {
                                state.transfer_state = false;
                                state.transfer_count = state.transfer_count.wrapping_add(1);
                                do_scan = true;
                            }
                        }
                        if do_scan {
                            let finished = self.scan_stream(pid, is_full_scan, &mut *observe);
                            if !is_full_scan && finished {
                                return true;
                            }
                        }
                    } else {
                        let mut do_update: Option<(i128, i128, i128)> = None;
                        {
                            parser.packet_length = parser.packet_length.wrapping_sub(1);
                            let parse = state.parse;
                            let header_found = (is_video
                                && (parse == 0x0000_01FD
                                    || (0x0000_01E0..=0x0000_01EF).contains(&parse)))
                                || (is_audio
                                    && (parse == 0x0000_01BD
                                        || (0x0000_01C0..=0x0000_01DF).contains(&parse)
                                        || parse == 0x0000_01FA
                                        || parse == 0x0000_01FD))
                                || (!is_video
                                    && !is_audio
                                    && (parse == 0x0000_01FA
                                        || parse == 0x0000_01FD
                                        || parse == 0x0000_01BD
                                        || (0x0000_01E0..=0x0000_01EF).contains(&parse)));
                            if header_found {
                                state.packet_length_parse = 2;
                            } else if state.packet_length_parse > 0 {
                                state.packet_length_parse =
                                    state.packet_length_parse.wrapping_sub(1);
                                if state.packet_length_parse == 0 {
                                    state.packet_length =
                                        i32::try_from(state.parse & 0xFFFF).unwrap_or(0);
                                    if state.packet_length == 0 {
                                        parser.variable_packet_end = false;
                                        state.packet_length_variable = true;
                                    }
                                    state.packet_parse = 3;
                                }
                            } else if state.packet_parse > 0 {
                                state.packet_length = state.packet_length.wrapping_sub(1);
                                state.packet_parse = state.packet_parse.wrapping_sub(1);
                                match state.packet_parse {
                                    1 => {
                                        state.pes_header_flags =
                                            u8::try_from(state.parse & 0xFF).unwrap_or(0);
                                    }
                                    0 => {
                                        state.pes_header_length =
                                            u8::try_from(state.parse & 0xFF).unwrap_or(0);
                                        if (state.pes_header_flags & 0xC0) == 0x80 {
                                            state.pts_parse = 5;
                                        } else if (state.pes_header_flags & 0xC0) == 0xC0 {
                                            state.dts_parse = 10;
                                        }
                                        if state.pes_header_length == 0 {
                                            state.transfer_state = true;
                                        }
                                    }
                                    _ => {}
                                }
                            } else if state.pts_parse > 0 {
                                state.packet_length = state.packet_length.wrapping_sub(1);
                                state.pes_header_length = state.pes_header_length.wrapping_sub(1);
                                state.pts_parse = state.pts_parse.wrapping_sub(1);
                                match state.pts_parse {
                                    4 => {
                                        state.pts_temp =
                                            u64::from(state.parse & 0xE).wrapping_shl(29);
                                    }
                                    3 => {
                                        state.pts_temp = state.pts_temp.wrapping_add(u64::from(
                                            (state.parse & 0xFF).wrapping_shl(22),
                                        ));
                                    }
                                    2 => {
                                        state.pts_temp = state.pts_temp.wrapping_add(u64::from(
                                            (state.parse & 0xFE).wrapping_shl(14),
                                        ));
                                    }
                                    1 => {
                                        state.pts_temp = state.pts_temp.wrapping_add(u64::from(
                                            (state.parse & 0xFF).wrapping_shl(7),
                                        ));
                                    }
                                    // The 5-byte PTS countdown only ever reaches 0 here.
                                    _ => {
                                        state.pts_temp = state.pts_temp.wrapping_add(u64::from(
                                            (state.parse & 0xFE).wrapping_shr(1),
                                        ));
                                        state.pts = i128::from(state.pts_temp);
                                        if state.pts != state.pts_last {
                                            if state.pts_last > 0 {
                                                state.pts_transfer =
                                                    state.pts.wrapping_sub(state.pts_last);
                                            }
                                            state.pts_last = state.pts;
                                        }
                                        state.pts_diff = state.pts.wrapping_sub(state.dts_prev);
                                        if state.pts_count > 0 && is_video {
                                            do_update =
                                                Some((state.pts, state.pts_diff, state.dts_temp));
                                        }
                                        state.dts_prev = state.pts;
                                        state.pts_count = state.pts_count.wrapping_add(1);
                                        if state.pes_header_length == 0 {
                                            state.transfer_state = true;
                                        }
                                    }
                                }
                            } else if state.dts_parse > 0 {
                                state.packet_length = state.packet_length.wrapping_sub(1);
                                state.pes_header_length = state.pes_header_length.wrapping_sub(1);
                                state.dts_parse = state.dts_parse.wrapping_sub(1);
                                match state.dts_parse {
                                    9 => {
                                        state.pts_temp =
                                            u64::from(state.parse & 0xE).wrapping_shl(29);
                                    }
                                    8 => {
                                        state.pts_temp = state.pts_temp.wrapping_add(u64::from(
                                            (state.parse & 0xFF).wrapping_shl(22),
                                        ));
                                    }
                                    7 => {
                                        state.pts_temp = state.pts_temp.wrapping_add(u64::from(
                                            (state.parse & 0xFE).wrapping_shl(14),
                                        ));
                                    }
                                    6 => {
                                        state.pts_temp = state.pts_temp.wrapping_add(u64::from(
                                            (state.parse & 0xFF).wrapping_shl(7),
                                        ));
                                    }
                                    5 => {
                                        state.pts_temp = state.pts_temp.wrapping_add(u64::from(
                                            (state.parse & 0xFE).wrapping_shr(1),
                                        ));
                                        state.pts = i128::from(state.pts_temp);
                                        // Running max as `.max` (not an idempotent `>`).
                                        state.pts_last = state.pts_last.max(state.pts);
                                    }
                                    4 => {
                                        state.dts_temp =
                                            i128::from(state.parse & 0xE).wrapping_shl(29);
                                    }
                                    3 => {
                                        state.dts_temp = state.dts_temp.wrapping_add(i128::from(
                                            (state.parse & 0xFF).wrapping_shl(22),
                                        ));
                                    }
                                    2 => {
                                        state.dts_temp = state.dts_temp.wrapping_add(i128::from(
                                            (state.parse & 0xFE).wrapping_shl(14),
                                        ));
                                    }
                                    1 => {
                                        state.dts_temp = state.dts_temp.wrapping_add(i128::from(
                                            (state.parse & 0xFF).wrapping_shl(7),
                                        ));
                                    }
                                    // The 10-byte PTS+DTS countdown only reaches 0 here.
                                    _ => {
                                        state.dts_temp = state.dts_temp.wrapping_add(i128::from(
                                            (state.parse & 0xFE).wrapping_shr(1),
                                        ));
                                        state.pts_diff =
                                            state.dts_temp.wrapping_sub(state.dts_prev);
                                        if state.pts_count > 0 && is_video {
                                            do_update = Some((
                                                state.dts_temp,
                                                state.pts_diff,
                                                state.dts_temp,
                                            ));
                                        }
                                        state.dts_prev = state.dts_temp;
                                        state.pts_count = state.pts_count.wrapping_add(1);
                                        if state.pes_header_length == 0 {
                                            state.transfer_state = true;
                                        }
                                    }
                                }
                            } else if state.pes_header_length > 0 {
                                state.packet_length = state.packet_length.wrapping_sub(1);
                                state.pes_header_length = state.pes_header_length.wrapping_sub(1);
                                if state.pes_header_length == 0 {
                                    state.transfer_state = true;
                                }
                            }
                        }
                        if let Some((marker, pdiff, dts_temp)) = do_update {
                            self.update_stream_bitrates(pid, marker, pdiff, &mut *relevant);
                            // Running min/max as `.min`/`.max`, avoiding idempotent
                            // `<`/`>` equivalents.
                            parser.pts_first = parser.pts_first.min(dts_temp);
                            parser.pts_last = parser.pts_last.max(dts_temp);
                            self.length =
                                pts_to_f64(parser.pts_last.wrapping_sub(parser.pts_first))
                                    / 90000.0;
                        }
                    }
                    if parser.packet_length == 0 {
                        parser.sync_state = false;
                    }
                } else {
                    parser.packet_length = parser.packet_length.wrapping_sub(1);
                    if (bl.wrapping_sub(i)) >= i64::from(parser.packet_length) {
                        i = i.wrapping_add(i64::from(parser.packet_length));
                        parser.packet_length = 0;
                    } else {
                        parser.packet_length = parser
                            .packet_length
                            .wrapping_sub(to_byte(bl.wrapping_sub(i).wrapping_add(1)));
                        i = bl;
                    }
                    if parser.packet_length == 0 {
                        parser.sync_state = false;
                    }
                }
                i = i.wrapping_add(1);
            }
            self.size = self.size.wrapping_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX));
        }
        false
    }

    /// The scan tail: the final bitrate pass over every video stream. Runs
    /// only when the whole stream was read (neither an early finish nor a
    /// failed read cut the scan short).
    fn finish_scan(&mut self, relevant: &mut Vec<&mut TsPlaylistFile>) {
        let mut pts_last: i128 = 0;
        let mut pts_diff: i128 = 0;
        let video_pids: Vec<u16> = self
            .streams
            .iter()
            .filter(|(_, s)| s.base().is_video_stream())
            .map(|(p, _)| *p)
            .collect();
        for pid in video_pids {
            if let Some(state) = self.stream_states.get(&pid)
                && state.pts_last > pts_last
            {
                pts_last = state.pts_last;
                pts_diff = pts_last.wrapping_sub(state.dts_prev);
            }
            self.update_stream_bitrates(pid, pts_last, pts_diff, relevant);
        }
    }

    /// Walks one PAT byte (PID 0): assembles the section, then on completion
    /// records the PMT PID.
    fn parse_pat(
        parser: &mut TsPacketParser,
        buffer: &[u8],
        i: &mut i64,
        bl: i64,
        offset: &mut i64,
    ) {
        if parser.pat_assembler.transfer_state {
            if parser.pat_assembler.transfer(
                buffer,
                &mut parser.pat,
                i,
                bl,
                offset,
                &mut parser.packet_length,
            ) {
                let bound = i64::from(parser.pat_assembler.offset).wrapping_sub(4);
                let mut k: i64 = 0;
                while k < bound {
                    let program_number = u32::from(byte_at(&parser.pat, k))
                        .wrapping_shl(8)
                        .wrapping_add(u32::from(byte_at(&parser.pat, k.wrapping_add(1))));
                    let program_pid = u16::from(byte_at(&parser.pat, k.wrapping_add(2)) & 0x1F)
                        .wrapping_shl(8)
                        .wrapping_add(u16::from(byte_at(&parser.pat, k.wrapping_add(3))));
                    if program_number == 1 {
                        parser.pmt_pid = program_pid;
                    }
                    k = k.wrapping_add(4);
                }
            }
        } else {
            parser.packet_length = parser.packet_length.wrapping_sub(1);
            // The PAT header ends with `last_section_number` and carries no
            // table-specific fields, so position 0 starts the body copy.
            if matches!(
                parser.pat_assembler.header_byte(byte_at(buffer, *i)),
                SectionByte::Header(0)
            ) {
                parser.pat_assembler.transfer_state = true;
            }
        }
    }

    /// Walks one PMT byte (PID == `pmt_pid`): assembles the section, then on
    /// completion registers the elementary streams via
    /// [`create_stream`](Self::create_stream).
    fn parse_pmt(
        &mut self,
        parser: &mut TsPacketParser,
        buffer: &[u8],
        i: &mut i64,
        bl: i64,
        offset: &mut i64,
        is_full_scan: bool,
    ) {
        if parser.pmt_assembler.transfer_state {
            let complete = {
                let pmt = parser.pmt.entry(parser.pid).or_insert_with(|| vec![0_u8; SECTION_SIZE]);
                parser.pmt_assembler.transfer(buffer, pmt, i, bl, offset, &mut parser.packet_length)
            };
            if complete {
                let bound = i64::from(parser.pmt_assembler.offset).wrapping_sub(4);
                let mut k: i64 = 0;
                while k < bound {
                    let (stream_type, stream_pid, stream_info_length) = {
                        let pmt = parser.pmt.get(&parser.pid);
                        let st = pmt.map_or(0, |p| byte_at(p, k));
                        let p1 = pmt.map_or(0, |p| byte_at(p, k.wrapping_add(1)));
                        let p2 = pmt.map_or(0, |p| byte_at(p, k.wrapping_add(2)));
                        let p3 = pmt.map_or(0, |p| byte_at(p, k.wrapping_add(3)));
                        let p4 = pmt.map_or(0, |p| byte_at(p, k.wrapping_add(4)));
                        let spid = u16::from(p1 & 0x1F).wrapping_shl(8).wrapping_add(u16::from(p2));
                        let sil = u16::from(p3 & 0xF).wrapping_shl(8).wrapping_add(u16::from(p4));
                        (st, spid, sil)
                    };
                    if !self.streams.contains_key(&stream_pid) {
                        self.create_stream(stream_pid, stream_type);
                        // An unrecognised type registers no stream, and the
                        // walk steps over that entry: ISO 13818-1 makes each
                        // entry self-delimiting through its ES_info_length,
                        // so the stride below lands on the next entry
                        // without interpreting the skipped one, and later
                        // entries keep their streams. Classic BDInfo
                        // dereferences the absent stream here instead.
                        if let Some(s) = self.streams.get_mut(&stream_pid)
                            && s.base().is_graphics_stream()
                        {
                            s.base_mut().is_initialized = !is_full_scan;
                        }
                    }
                    k = k.wrapping_add(5).wrapping_add(i64::from(stream_info_length));
                }
            }
        } else {
            parser.packet_length = parser.packet_length.wrapping_sub(1);
            let b = byte_at(buffer, *i);
            match parser.pmt_assembler.header_byte(b) {
                // The PMT header's tail: the `program_info_length` pair, whose
                // descriptors precede the stream entries; the body copy starts
                // only once they are drained.
                SectionByte::Header(1) => {
                    parser.pmt_program_info_length = u16::from(b & 0xF).wrapping_shl(8);
                }
                SectionByte::Header(0) => {
                    parser.pmt_program_info_length =
                        parser.pmt_program_info_length.wrapping_add(u16::from(b));
                    if parser.pmt_program_info_length == 0 {
                        parser.pmt_assembler.transfer_state = true;
                    }
                }
                SectionByte::Idle => {
                    // A body `if` spelled `!= 0`, not a `> 0` match guard: the
                    // guard's replace-with-true mutant (like `> 0`'s `>= 0`)
                    // is equivalent — at idle both countdowns are 0 and wrap
                    // in lockstep, so a spurious drain fires only zero-length
                    // pseudo-transfers that re-walk the stale section
                    // idempotently — and so could never be killed. `!= 0`'s
                    // `== 0` mutant dies on any descriptor-bearing PMT.
                    if parser.pmt_program_info_length != 0 {
                        // Program-info descriptors carry nothing the analysis
                        // reads; only the byte consumption (positioning the
                        // stream entries) matters.
                        parser.pmt_assembler.section_length =
                            parser.pmt_assembler.section_length.wrapping_sub(1);
                        parser.pmt_program_info_length =
                            parser.pmt_program_info_length.wrapping_sub(1);
                        if parser.pmt_program_info_length == 0 {
                            parser.pmt_assembler.transfer_state = true;
                        }
                    }
                }
                SectionByte::Taken | SectionByte::Header(_) => {}
            }
        }
    }

    /// Finalises one PES and runs the codec seam.
    ///
    /// Returns whether the whole clip is finished (every stream initialised, and
    /// not the MVC-without-AVC case), which lets a non-full scan stop early.
    #[expect(
        clippy::similar_names,
        reason = "is_avc/is_mvc are the paired finish-check flags; renaming one would obscure the pairing"
    )]
    fn scan_stream(
        &mut self,
        pid: u16,
        is_full_scan: bool,
        observe: &mut dyn FnMut(u16, &TsStreamBuffer),
    ) -> bool {
        let is_audio = self.streams.get(&pid).is_some_and(|s| s.base().is_audio_stream());
        let bitrate = {
            let state = self.stream_states.entry(pid).or_default();
            // Each PES starts with a clean frame marker; the codec dispatch
            // below sets it when this access unit carries a recognisable frame.
            state.stream_tag = None;
            // The audio bitrate doubles as the DTS decoders' "open" rate; tracked
            // here as the peak transfer rate and forwarded to the dispatch.
            let bitrate = if is_audio && state.pts_transfer > 0 {
                let bitrate = round_long(
                    bytes_to_f64(state.stream_buffer.transfer_length()) * 8.0
                        / (pts_to_f64(state.pts_transfer) / 90000.0),
                );
                // Running max as `.max`, so the comparison is not an idempotent
                // `>`/`>=` equivalent.
                state.peak_transfer_rate = state.peak_transfer_rate.max(bitrate);
                bitrate
            } else {
                0
            };
            let tl = i64::try_from(state.stream_buffer.transfer_length()).unwrap_or(i64::MAX);
            state.peak_transfer_length = state.peak_transfer_length.max(tl);
            state.stream_buffer.begin_read();
            bitrate
        };
        {
            // Codec seam: the assembled access unit is decoded here, filling the
            // stream's codec fields and (when it has decoded enough) its
            // `is_initialized` flag. `observe` is the test introspection hook
            // over the same buffer.
            let Self { streams, stream_states, .. } = self;
            let state = stream_states.entry(pid).or_default();
            observe(pid, &state.stream_buffer);
            // The PES's PID is always registered (PES assembly is gated on it),
            // so this single-key range query dispatches the one matching stream
            // without a fallible lookup's dead `None` arm — and without the
            // linear all-streams walk a `for … if *p == pid` loop costs per PES.
            for (_, stream) in streams.range_mut(pid..=pid) {
                crate::codec::scan_access_unit(
                    stream,
                    &mut state.stream_buffer,
                    bitrate,
                    is_full_scan,
                    &mut state.stream_tag,
                );
                // Refresh the state's mirror of the init flag the codec scan
                // may just have set.
                state.stream_initialized = stream.base().is_initialized;
            }
            state.stream_buffer.reset();
        }
        // The finish verdict only matters to the quick scan's early exit; the
        // full scan ignores it, so skip the all-streams walk entirely.
        if is_full_scan {
            return false;
        }
        // The all-initialised / MVC-without-AVC finish check: a clip is
        // "finished" once every stream is initialised, except an MVC stream
        // still waiting for its AVC base view.
        let mut is_avc = false;
        let mut is_mvc = false;
        for stream in self.streams.values() {
            if !stream.base().is_initialized {
                return false;
            }
            if stream.stream_type() == TsStreamType::AvcVideo {
                is_avc = true;
            }
            if stream.stream_type() == TsStreamType::MvcVideo {
                is_mvc = true;
            }
        }
        if is_mvc && !is_avc {
            return false;
        }
        true
    }

    /// Distributes a video PID's window across the playlists and recomputes the
    /// VBR stream bitrates.
    fn update_stream_bitrates(
        &mut self,
        pts_pid: u16,
        pts: i128,
        pts_diff: i128,
        playlists: &mut [&mut TsPlaylistFile],
    ) {
        let pids: Vec<u16> = self.stream_states.keys().copied().collect();
        for pid in pids {
            let skip_video = self
                .streams
                .get(&pid)
                .is_some_and(|s| s.base().is_video_stream() && pid != pts_pid);
            if skip_video {
                continue;
            }
            if self.stream_states.get(&pid).is_none_or(|s| s.window_packets == 0) {
                continue;
            }
            self.update_stream_bitrate(pid, pts_pid, pts, pts_diff, playlists);
        }

        for playlist in playlists.iter_mut() {
            let mut packet_seconds = 0.0;
            for clip in &playlist.stream_clips {
                if clip.angle_index == 0 {
                    packet_seconds += clip.packet_seconds;
                }
            }
            if packet_seconds > 0.0 {
                for stream in playlist.streams.values_mut() {
                    apply_vbr_bitrate(stream, packet_seconds);
                }
                for angle in &mut playlist.angle_streams {
                    for stream in angle.values_mut() {
                        apply_vbr_bitrate(stream, packet_seconds);
                    }
                }
            }
        }
    }

    /// Adds one PID's window to the clips, the playlist streams, and this clip's
    /// own [`streams`](Self::streams) (`_pts_pid` is accepted for the call shape
    /// but unused). Zeroes the window afterwards.
    fn update_stream_bitrate(
        &mut self,
        pid: u16,
        _pts_pid: u16,
        pts: i128,
        pts_diff: i128,
        playlists: &mut [&mut TsPlaylistFile],
    ) {
        let Self { name, streams, stream_states, stream_diagnostics, .. } = self;
        let (window_bytes, window_packets) =
            stream_states.get(&pid).map_or((0, 0), |s| (s.window_bytes, s.window_packets));
        let stream_time = pts_to_f64(pts) / 90000.0;
        let stream_interval = pts_to_f64(pts_diff) / 90000.0;
        let stream_offset = stream_time + stream_interval;

        for playlist in playlists.iter_mut() {
            let TsPlaylistFile { stream_clips, streams: pl_streams, angle_streams, .. } = playlist;
            let angle_count = angle_streams.len();
            for clip in stream_clips.iter_mut() {
                if clip.name != *name {
                    continue;
                }
                if stream_time == 0.0
                    || (stream_time >= clip.time_in && stream_time <= clip.time_out)
                {
                    clip.payload_bytes = clip.payload_bytes.wrapping_add(window_bytes);
                    clip.packet_count = clip.packet_count.wrapping_add(window_packets);
                    // Running max of the in-window offset; a negative arg (an
                    // offset before the clip starts) is a `.max` no-op, so a
                    // separate offset-past-time-in guard is subsumed — same
                    // behaviour with no idempotent comparisons.
                    clip.packet_seconds = clip.packet_seconds.max(stream_offset - clip.time_in);
                    let angle = usize::try_from(clip.angle_index).unwrap_or(usize::MAX);
                    let ps = if clip.angle_index > 0 && angle < angle_count.wrapping_add(1) {
                        angle_streams.get_mut(angle.wrapping_sub(1))
                    } else {
                        Some(&mut *pl_streams)
                    };
                    if let Some(stream) = ps.and_then(|m| m.get_mut(&pid)) {
                        stream.base_mut().payload_bytes =
                            stream.base().payload_bytes.wrapping_add(window_bytes);
                        stream.base_mut().packet_count =
                            stream.base().packet_count.wrapping_add(window_packets);
                        if stream.base().is_video_stream() {
                            stream.base_mut().packet_seconds += stream_interval;
                            stream.base_mut().active_bit_rate = round_long(
                                bytes_to_f64(stream.base().payload_bytes) * 8.0
                                    / stream.base().packet_seconds,
                            );
                        }
                        // A TrueHD stream's active rate excludes its embedded AC3
                        // core: the core's nominal rate is taken off at every
                        // window the stream participates in.
                        if let TsStream::Audio(audio) = stream
                            && audio.base.stream_type == TsStreamType::Ac3TrueHdAudio
                            && let Some(core) = &audio.core_stream
                        {
                            audio.base.active_bit_rate =
                                audio.base.active_bit_rate.wrapping_sub(core.base.bit_rate);
                        }
                    }
                }
            }
        }

        if let Some(stream) = streams.get_mut(&pid) {
            stream.base_mut().payload_bytes =
                stream.base().payload_bytes.wrapping_add(window_bytes);
            stream.base_mut().packet_count =
                stream.base().packet_count.wrapping_add(window_packets);
            if stream.base().is_video_stream() {
                stream_diagnostics.entry(pid).or_default().push(TsStreamDiagnostics {
                    bytes: window_bytes,
                    packets: window_packets,
                    marker: pts_to_f64(pts) / 90000.0,
                    interval: pts_to_f64(pts_diff) / 90000.0,
                    // The frame marker the codec seam set for the last
                    // completed PES — the picture type this window closed on.
                    tag: stream_states.get(&pid).and_then(|s| s.stream_tag.clone()),
                });
                stream.base_mut().packet_seconds += stream_interval;
            }
        }
        // The state always exists (created in the header parse); `or_default`
        // avoids an unreachable `None` arm.
        let state = stream_states.entry(pid).or_default();
        state.window_packets = 0;
        state.window_bytes = 0;
    }

    /// Registers a stream of `stream_type_byte` at `stream_pid`. Unhandled types
    /// create no stream but still ensure the PID has an (empty) diagnostics
    /// list.
    fn create_stream(&mut self, stream_pid: u16, stream_type_byte: u8) {
        let stream_type = TsStreamType::from_u8(stream_type_byte);
        if let Some(mut stream) = stream_type.default_stream() {
            // The caller registers a PID only once (`streams` is checked before
            // the call), so the order list records each PID exactly once;
            // `or_insert` keeps any existing entry and drops the duplicate.
            stream.base_mut().pid = Pid::new(stream_pid);
            stream.base_mut().stream_type = stream_type;
            stream.base_mut().descriptors = Some(Vec::new());
            self.stream_order.push(stream_pid);
            let registered = self.streams.entry(stream_pid).or_insert(stream);
            // Mirror the kind/init flags into the per-PID state so the
            // per-byte demux loop reads them without a `streams` lookup.
            let kind = registered.stream_type().kind();
            let initialized = registered.base().is_initialized;
            let state = self.stream_states.entry(stream_pid).or_default();
            state.stream_kind = kind;
            state.stream_initialized = initialized;
        }
        self.stream_diagnostics.entry(stream_pid).or_default();
    }
}

/// Synthetic BDAV packet builders shared by this module's and the disc
/// orchestration's tests: 192-byte source packets, PES payloads with
/// PTS/DTS timestamps, and PAT/PMT sections.
#[cfg(test)]
pub mod packets {
    /// Builds a 192-byte BDAV source packet with adaptation-field-control `afc`
    /// (2 bits): a 4-byte `TP_extra_header` time code, the `0x47` sync byte, a
    /// 3-byte TS header (`PUSI`/`PID`/`afc`), then `payload` padded to 184 bytes.
    pub(crate) fn packet_raw(pid: u16, pusi: bool, afc: u8, payload: &[u8]) -> Vec<u8> {
        let [hi, lo] = pid.to_be_bytes();
        let mut p = vec![0_u8, 0, 0, 0, 0x47];
        p.push((if pusi { 0x40 } else { 0 }) | (hi & 0x1F));
        p.push(lo);
        p.push((afc & 0x3).wrapping_shl(4)); // TSC=0, AFC, CC=0
        let mut pl = payload.to_vec();
        pl.resize(184, 0xFF);
        p.extend_from_slice(&pl);
        p
    }

    /// A payload-only (AFC = 01) packet — the common case.
    pub(crate) fn packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
        packet_raw(pid, pusi, 0x1, payload)
    }

    /// A variable-length PES (`PES_packet_length` = 0): unbounded, finalised only by
    /// the next payload-unit start on the PID.
    pub(crate) fn pes_variable(stream_id: u8, pts: u64, data: &[u8]) -> Vec<u8> {
        let mut p = vec![0x00, 0x00, 0x01, stream_id, 0x00, 0x00, 0x80, 0x80, 0x05];
        p.extend_from_slice(&encode_pts(pts));
        p.extend_from_slice(data);
        p
    }

    /// A PES with no PTS/DTS and a zero-length optional header.
    pub(crate) fn pes_none(stream_id: u8, data: &[u8]) -> Vec<u8> {
        let pes_len = data.len().wrapping_add(3);
        let [lhi, llo] = u16::try_from(pes_len).unwrap().to_be_bytes();
        let mut p = vec![0x00, 0x00, 0x01, stream_id, lhi, llo, 0x80, 0x00, 0x00];
        p.extend_from_slice(data);
        p
    }

    /// A PES carrying a PTS plus `pad` stuffing bytes in the optional header.
    pub(crate) fn pes_pts_padded(stream_id: u8, pts: u64, pad: usize, data: &[u8]) -> Vec<u8> {
        let header_len = pad.wrapping_add(5);
        let pes_len = data.len().wrapping_add(3).wrapping_add(header_len);
        let [lhi, llo] = u16::try_from(pes_len).unwrap().to_be_bytes();
        let mut p = vec![
            0x00,
            0x00,
            0x01,
            stream_id,
            lhi,
            llo,
            0x80,
            0x80,
            u8::try_from(header_len).unwrap(),
        ];
        p.extend_from_slice(&encode_pts(pts));
        p.resize(p.len().wrapping_add(pad), 0xFF); // stuffing
        p.extend_from_slice(data);
        p
    }

    /// Builds a PAT payload announcing a single program (number 1) on `pmt_pid`.
    pub(crate) fn pat_payload(pmt_pid: u16) -> Vec<u8> {
        let [hi, lo] = pmt_pid.to_be_bytes();
        vec![
            0x00, // pointer_field
            0x00, // table_id (PAT)
            0xB0,
            0x0D, // section_syntax + section_length = 13
            0x00,
            0x01, // transport_stream_id
            0xC1, // reserved + version + current_next
            0x00, // section_number
            0x00, // last_section_number
            0x00,
            0x01, // program_number = 1
            0xE0 | (hi & 0x1F),
            lo, // reserved + PMT PID
            0x00,
            0x00,
            0x00,
            0x00, // CRC (unvalidated)
        ]
    }

    /// Builds a PMT payload listing `streams` (each `(stream_type, pid)`),
    /// `table_id` `0x02`, no program-info, no per-stream ES-info.
    pub(crate) fn pmt_payload(streams: &[(u8, u16)]) -> Vec<u8> {
        let with_es: Vec<(u8, u16, &[u8])> =
            streams.iter().map(|&(st, pid)| (st, pid, &[][..])).collect();
        pmt_payload_es(&with_es)
    }

    /// Builds a PMT payload listing `streams` (each `(stream_type, pid, es_info)`,
    /// the ES-info descriptor bytes emitted verbatim behind their length field),
    /// `table_id` `0x02`, no program-info.
    pub(crate) fn pmt_payload_es(streams: &[(u8, u16, &[u8])]) -> Vec<u8> {
        let mut entries = Vec::new();
        for &(st, pid, es) in streams {
            let [hi, lo] = pid.to_be_bytes();
            let [eshi, eslo] = u16::try_from(es.len()).unwrap().to_be_bytes();
            entries.extend_from_slice(&[st, 0xE0 | (hi & 0x1F), lo, 0xF0 | (eshi & 0x0F), eslo]);
            entries.extend_from_slice(es);
        }
        let section_len = entries.len().wrapping_add(13); // 9 header + 4 CRC
        let [slhi, sllo] = u16::try_from(section_len).unwrap().to_be_bytes();
        let mut p = vec![
            0x00, // pointer_field
            0x02, // table_id (PMT)
            0xB0 | (slhi & 0x0F),
            sllo, // section_syntax + section_length
            0x00,
            0x01, // program_number
            0xC1, // version + current_next
            0x00, // section_number
            0x00, // last_section_number
            0xE0,
            0x00, // reserved + PCR_PID
            0xF0,
            0x00, // reserved + program_info_length = 0
        ];
        p.extend_from_slice(&entries);
        p.extend_from_slice(&[0, 0, 0, 0]); // CRC
        p
    }

    /// Builds a raw PMT section (no pointer field): `table_id`, a section length
    /// (`sec_len` override or computed), the 9-byte header with `prog_info` program
    /// descriptors, the stream entries, and a 4-byte CRC.
    pub(crate) fn pmt_section(
        table_id: u8,
        prog_info: &[u8],
        streams: &[(u8, u16)],
        sec_len: Option<u16>,
    ) -> Vec<u8> {
        let mut entries = Vec::new();
        for &(st, pid) in streams {
            let [hi, lo] = pid.to_be_bytes();
            entries.extend_from_slice(&[st, 0xE0 | (hi & 0x1F), lo, 0xF0, 0x00]);
        }
        let computed = entries.len().wrapping_add(prog_info.len()).wrapping_add(13);
        let section_len = sec_len.unwrap_or_else(|| u16::try_from(computed).unwrap());
        let [slhi, sllo] = section_len.to_be_bytes();
        let [pihi, pilo] = u16::try_from(prog_info.len()).unwrap().to_be_bytes();
        let mut p = vec![
            table_id,
            0xB0 | (slhi & 0x0F),
            sllo,
            0x00,
            0x01,
            0xC1,
            0x00,
            0x00,
            0xE0,
            0x00, // PCR_PID
            0xF0 | (pihi & 0x0F),
            pilo, // program_info_length
        ];
        p.extend_from_slice(prog_info);
        p.extend_from_slice(&entries);
        p.extend_from_slice(&[0, 0, 0, 0]); // CRC
        p
    }

    /// Encodes a 33-bit `pts` into the 5-byte MPEG PTS field (standard marker bits).
    /// The demuxer decodes this back to the full 33-bit `pts`, bit 32 included.
    pub(crate) fn encode_pts(pts: u64) -> [u8; 5] {
        [
            0x21 | u8::try_from(pts.wrapping_shr(29) & 0x0E).unwrap(),
            u8::try_from(pts.wrapping_shr(22) & 0xFF).unwrap(),
            0x01 | u8::try_from(pts.wrapping_shr(14) & 0xFE).unwrap(),
            u8::try_from(pts.wrapping_shr(7) & 0xFF).unwrap(),
            0x01 | u8::try_from(pts.wrapping_shl(1) & 0xFE).unwrap(),
        ]
    }

    /// A PES payload carrying a PTS (no DTS) and `data.len()` elementary bytes,
    /// with `stream_id` as the start-code's fourth byte (e.g. `0xE0` video, `0xC0`
    /// audio).
    pub(crate) fn pes_pts(stream_id: u8, pts: u64, data: &[u8]) -> Vec<u8> {
        let pes_len = data.len().wrapping_add(8); // 3 prefix + 5 PTS + data
        let [lhi, llo] = u16::try_from(pes_len).unwrap().to_be_bytes();
        let mut p = vec![0x00, 0x00, 0x01, stream_id, lhi, llo, 0x80, 0x80, 0x05];
        p.extend_from_slice(&encode_pts(pts));
        p.extend_from_slice(data);
        p
    }

    /// A PES payload carrying a PTS **and** DTS plus `data.len()` elementary bytes.
    pub(crate) fn pes_dts(stream_id: u8, pts: u64, dts: u64, data: &[u8]) -> Vec<u8> {
        let pes_len = data.len().wrapping_add(13); // 3 prefix + 10 PTS/DTS + data
        let [lhi, llo] = u16::try_from(pes_len).unwrap().to_be_bytes();
        let mut p = vec![0x00, 0x00, 0x01, stream_id, lhi, llo, 0x80, 0xC0, 0x0A];
        // The PTS marker nibble is 0x3 with DTS present; the demux ignores it.
        let mut pts_bytes = encode_pts(pts);
        pts_bytes[0] |= 0x10;
        p.extend_from_slice(&pts_bytes);
        p.extend_from_slice(&encode_pts(dts));
        p.extend_from_slice(data);
        p
    }

    /// A PES carrying PTS+DTS plus `pad` stuffing bytes after the timestamps.
    pub(crate) fn pes_dts_padded(
        stream_id: u8,
        pts: u64,
        dts: u64,
        pad: usize,
        data: &[u8],
    ) -> Vec<u8> {
        let header_len = pad.wrapping_add(10);
        let pes_len = data.len().wrapping_add(3).wrapping_add(header_len);
        let [lhi, llo] = u16::try_from(pes_len).unwrap().to_be_bytes();
        let mut p = vec![
            0x00,
            0x00,
            0x01,
            stream_id,
            lhi,
            llo,
            0x80,
            0xC0,
            u8::try_from(header_len).unwrap(),
        ];
        let mut pts_bytes = encode_pts(pts);
        pts_bytes[0] |= 0x10;
        p.extend_from_slice(&pts_bytes);
        p.extend_from_slice(&encode_pts(dts));
        p.resize(p.len().wrapping_add(pad), 0xFF); // stuffing
        p.extend_from_slice(data);
        p
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{self, Cursor, Read};
    use std::sync::atomic::{AtomicBool, Ordering};

    use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest};

    use super::packets::{
        encode_pts, packet, packet_raw, pat_payload, pes_dts, pes_dts_padded, pes_none, pes_pts,
        pes_pts_padded, pes_variable, pmt_payload, pmt_payload_es, pmt_section,
    };
    use super::{
        DATA_SIZE, MeasuredSnapshot, QUICK_DATA_SIZE, TsInterleavedFile, TsStreamDiagnostics,
        TsStreamFile, pts_to_f64, round_long,
    };
    use crate::bdrom::clpi::TsStreamClip;
    use crate::bdrom::interleaved::MemBdFile;
    use crate::bdrom::mpls::TsPlaylistFile;
    use crate::bitstream::TsStreamBuffer;
    use crate::error::BdError;
    use crate::primitives::Pid;
    use crate::stream::{TsAudioStream, TsStream, TsStreamType, TsVideoStream};
    /// A throwaway playlist with no clips — lets [`TsStreamFile::scan`] run (it
    /// early-returns on an empty list) without contributing any bitrate target.
    fn empty_playlist() -> TsPlaylistFile {
        TsPlaylistFile {
            file_type: "MPLS0300".to_owned(),
            name: "00000.MPLS".to_owned(),
            mvc_base_view_r: false,
            chapters: Vec::new(),
            playlist_streams: BTreeMap::new(),
            streams: BTreeMap::new(),
            angle_streams: Vec::new(),
            stream_clips: Vec::new(),
            angle_count: 0,
        }
    }

    /// Scans `bytes` as the clip `name` against `playlists`, returning the demuxer.
    fn scan(
        name: &str,
        bytes: &[u8],
        playlists: &mut [TsPlaylistFile],
        full: bool,
    ) -> TsStreamFile {
        let mut file = TsStreamFile::new(name);
        let mut cur = Cursor::new(bytes.to_vec());
        file.scan(&mut cur, playlists, full).expect("scan");
        file
    }

    #[test]
    fn round_long_is_half_to_even() {
        assert_eq!(round_long(2.5), 2);
        assert_eq!(round_long(3.5), 4);
        assert_eq!(round_long(2.4), 2);
        assert_eq!(round_long(-2.5), -2);
        assert_eq!(round_long(0.0), 0);
    }

    #[test]
    fn new_uppercases_the_name() {
        let file = TsStreamFile::new("00017.m2ts");
        assert_eq!(file.name, "00017.M2TS");
        assert_eq!(file.size, 0);
        assert_eq!(file.length.to_bits(), 0.0_f64.to_bits());
        assert!(file.streams.is_empty());
    }

    #[test]
    fn empty_playlists_is_a_noop() {
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut cur = Cursor::new(packet(0, true, &pat_payload(0x0100)));
        file.scan(&mut cur, &mut [], true).expect("scan");
        assert_eq!(file.size, 0);
        assert!(file.streams.is_empty());
    }

    #[test]
    fn a_repeated_pmt_records_the_registration_order_once() {
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011), (0x81, 0x1100)])));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011), (0x81, 0x1100)])));
        let file = scan("00000.m2ts", &bytes, &mut [video_playlist(0x1011)], false);
        assert_eq!(file.stream_order, vec![0x1011, 0x1100]);
    }

    #[test]
    fn registers_every_stream_kind_from_the_pmt() {
        // PAT → PMT listing one of each handled kind plus MVC and an unknown type.
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(
            pmt_pid,
            true,
            &pmt_payload(&[
                (0x1B, 0x1011), // AVC video
                (0x24, 0x1012), // HEVC video
                (0x81, 0x1100), // AC3 audio
                (0x90, 0x1200), // PG graphics
                (0x92, 0x1A00), // subtitle text
                (0x20, 0x1B00), // MVC → no stream
                (0x99, 0x1C00), // unknown → no stream
            ]),
        ));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);

        let kind = |pid: u16| file.streams.get(&pid).map(TsStream::stream_type);
        assert_eq!(kind(0x1011), Some(TsStreamType::AvcVideo));
        assert_eq!(kind(0x1012), Some(TsStreamType::HevcVideo));
        assert_eq!(kind(0x1100), Some(TsStreamType::Ac3Audio));
        assert_eq!(kind(0x1200), Some(TsStreamType::PresentationGraphics));
        assert_eq!(kind(0x1A00), Some(TsStreamType::Subtitle));
        // MVC registers as a video stream here (unlike the CLPI/MPLS parsers,
        // where it yields nothing); only a truly unknown type registers no
        // stream — yet every PID gets a diagnostics slot.
        assert_eq!(kind(0x1B00), Some(TsStreamType::MvcVideo));
        assert_eq!(kind(0x1C00), None);
        assert!(file.stream_diagnostics.contains_key(&0x1C00));
        // Registration sets an (empty) descriptor list.
        assert_eq!(file.streams.get(&0x1011).unwrap().base().descriptors, Some(Vec::new()));
        // A full scan leaves graphics uninitialised (PGS analysis is deferred to a
        // codec pass); the others are still pending too (no PES seen).
        assert!(!file.streams.get(&0x1200).unwrap().base().is_initialized);
    }

    #[test]
    fn an_unknown_pmt_stream_type_does_not_hide_the_entries_after_it() {
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(
            pmt_pid,
            true,
            &pmt_payload_es(&[
                // ES-info on the skipped entry: stepping over the entry means
                // stepping over its descriptor bytes too, so a stride that
                // ignores ES_info_length reads the next entry from the middle of
                // these three and registers 0x0107 instead of 0x1011.
                (0x99, 0x1C00, &[0x52, 0x01, 0x07][..]),
                (0x1B, 0x1011, &[][..]),
            ]),
        ));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);

        // The unknown type registers no stream but does get a diagnostics slot.
        assert_eq!(file.streams.get(&0x1C00).map(TsStream::stream_type), None);
        assert!(file.stream_diagnostics.contains_key(&0x1C00));
        assert_eq!(
            file.streams.get(&0x1011).map(TsStream::stream_type),
            Some(TsStreamType::AvcVideo)
        );
        assert!(!file.streams.contains_key(&0x0107));
    }

    #[test]
    fn graphics_is_initialized_on_a_non_full_scan() {
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x90, 0x1200)])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, false);
        assert!(file.streams.get(&0x1200).unwrap().base().is_initialized);
    }

    /// A playlist whose single main clip is `00000.M2TS` and whose `Streams` map
    /// holds one VBR AVC video at `pid` — the bitrate-update target.
    fn video_playlist(pid: u16) -> TsPlaylistFile {
        let mut vs = TsVideoStream::default();
        vs.base.pid = Pid::new(pid);
        vs.base.stream_type = TsStreamType::AvcVideo;
        vs.base.is_vbr = true;
        // A non-VBR audio stream rides along so the VBR pass exercises its
        // `is_vbr == false` skip too.
        let mut audio = TsAudioStream::default();
        audio.base.pid = Pid::new(0x1100);
        audio.base.stream_type = TsStreamType::Ac3Audio;
        let mut pl = empty_playlist();
        pl.stream_clips = vec![TsStreamClip {
            name: "00000.M2TS".to_owned(),
            time_in: 0.0,
            time_out: 1000.0,
            angle_index: 0,
            ..TsStreamClip::default()
        }];
        pl.streams = BTreeMap::from([(pid, TsStream::Video(vs)), (0x1100, TsStream::Audio(audio))]);
        pl
    }

    #[test]
    fn computes_length_bitrate_and_diagnostics_from_dts_video() {
        // Three bounded DTS video PES at 1s/2s/3s (90 kHz), 100 ES bytes each.
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let data = [0xAA_u8; 100];
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &data)));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &data)));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 270_000, 270_000, &data)));
        let mut pls = [video_playlist(0x1011)];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);

        // Length spans the second-to-last DTS update (the first PES has pts_count 0,
        // so DTS 90 000 is never recorded): (270 000 − 180 000) / 90 000 = 1.0s.
        assert_eq!(file.length.to_bits(), 1.0_f64.to_bits());

        // Demux Streams: two windows flushed (PES1 + PES2 data); the last PES's data
        // window has zero packets and is skipped by the final pass.
        let s = file.streams.get(&0x1011).unwrap().base();
        assert_eq!(s.payload_bytes, 200);
        assert_eq!(s.packet_count, 3);
        assert_eq!(s.packet_seconds.to_bits(), 2.0_f64.to_bits());

        // Two video diagnostics samples (one per flushed window).
        let diag = file.stream_diagnostics.get(&0x1011).unwrap();
        assert_eq!(diag.len(), 2);
        let d0 = diag.first().unwrap();
        let d1 = diag.get(1).unwrap();
        assert_eq!((d0.bytes, d0.packets), (100, 2));
        assert_eq!(d0.marker.to_bits(), 2.0_f64.to_bits()); // 180000/90000
        assert_eq!(d0.interval.to_bits(), 1.0_f64.to_bits());
        assert_eq!(d0.tag, None);
        assert_eq!((d1.bytes, d1.packets), (100, 1));

        // The clip counts every packet in its window (PAT + PMT + video).
        let clip = pls[0].stream_clips.first().unwrap();
        assert_eq!(clip.payload_bytes, 200);
        assert_eq!(clip.packet_count, 5);
        assert_eq!(clip.packet_seconds.to_bits(), 4.0_f64.to_bits());

        // The playlist's stream gets the active (video) and VBR bitrates.
        let ps = pls[0].streams.get(&0x1011).unwrap().base();
        assert_eq!(ps.payload_bytes, 200);
        assert_eq!(ps.packet_count, 3);
        assert_eq!(ps.active_bit_rate, 800); // round(200*8 / 2.0s)
        assert_eq!(ps.bit_rate, 400); // round(200*8 / 4.0s clip seconds)

        // Per-PID diagnostics counters (total counts are not window-flushed).
        let st = file.stream_states.get(&0x1011).unwrap();
        assert_eq!(st.total_packets, 3);
        assert_eq!(st.total_bytes, 300);
        assert_eq!(st.transfer_count, 3);
        assert_eq!(st.peak_transfer_length, 100);
        assert_eq!(st.peak_transfer_rate, 0); // video: no audio peak rate
    }

    #[test]
    fn release_scratch_frees_the_demux_state_and_keeps_the_public_outputs() {
        // release_scratch drops the private per-PID scratch (its PES buffers)
        // once the scan is done — the disc scan calls it before parking the
        // clip in its result map. Its effect is invisible to the report, so it
        // is pinned directly here: the scratch goes empty while the public
        // outputs the disc scan reads afterwards (`streams`, `stream_order`,
        // `stream_diagnostics`, `size`, `length`) are untouched.
        let mut file = TsStreamFile::new("00000.m2ts");
        file.stream_states.entry(0x1011).or_default();
        file.stream_states.entry(0x1100).or_default();
        file.stream_order.push(0x1011);
        file.stream_diagnostics.insert(0x1011, Vec::new());
        file.size = 4096;
        file.length = 2.5;
        assert!(!file.stream_states.is_empty(), "the scan populated the scratch");

        file.release_scratch();

        assert!(file.stream_states.is_empty(), "the scratch is freed");
        assert_eq!(file.stream_order, vec![0x1011], "stream order retained");
        assert!(file.stream_diagnostics.contains_key(&0x1011), "diagnostics retained");
        assert_eq!(file.size, 4096, "size retained");
        assert_eq!(file.length.to_bits(), 2.5_f64.to_bits(), "length retained");
    }

    #[test]
    fn dts_path_preserves_pts_dts_bit_32() {
        // A timestamp with bit 32 set (>= 2^32 ticks ≈ 13.26 h of 90 kHz time)
        // must keep its high bit. Three DTS video PES; the third's DTS is
        // 2^32 + 180 000. The length spans the 2nd→3rd marker (the DTS path's
        // `dts_temp`, arm 4), so a full-33-bit decode gives a 2^32-tick span —
        // classic BDInfo's 32-bit shift would drop bit 32, collapsing it to 0 s.
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let hi = (1_u64 << 32).wrapping_add(180_000);
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0xAA; 100])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[0xAA; 100])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, hi, hi, &[0xAA; 100])));
        let mut pls = [video_playlist(0x1011)];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        // (2^32) / 90 000 — non-zero only because bit 32 survives the decode.
        let expected = 4_294_967_296.0_f64 / 90_000.0;
        assert_eq!(file.length.to_bits(), expected.to_bits());
    }

    #[test]
    fn diagnostics_record_the_codec_frame_tag_per_window() {
        // PES1 carries an AVC access-unit delimiter (picture type I); PES2 and
        // PES3 carry no frame. The window that closes at PES2's timestamp gets
        // PES1's tag; the next window (PES2's frameless payload) records `None`
        // — the per-PES marker reset at the codec seam.
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let aud = [0x00, 0x00, 0x01, 0x09, 0x10];
        let plain = [0xAA_u8; 5];
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &aud)));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &plain)));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 270_000, 270_000, &plain)));
        let mut pls = [video_playlist(0x1011)];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);

        let diag = file.stream_diagnostics.get(&0x1011).unwrap();
        assert_eq!(diag.len(), 2);
        assert_eq!(diag.first().unwrap().tag.as_deref(), Some("I"));
        assert_eq!(diag.get(1).unwrap().tag, None);
    }

    #[test]
    fn truehd_active_rate_subtracts_the_embedded_core() {
        // A TrueHD playlist stream with an embedded AC3 core: every window flush
        // that lands in a clip takes the core's nominal rate off the active rate.
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011), (0x83, 0x1100)])));
        let data = [0xAA_u8; 100];
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &data)));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &data)));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &data)));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 270_000, 270_000, &data)));

        let mut pl = video_playlist(0x1011);
        let mut thd = TsAudioStream::default();
        thd.base.pid = Pid::new(0x1100);
        thd.base.stream_type = TsStreamType::Ac3TrueHdAudio;
        let mut core = TsAudioStream::default();
        core.base.bit_rate = 640_000;
        thd.core_stream = Some(Box::new(core));
        pl.streams.insert(0x1100, TsStream::Audio(thd));
        let mut pls = [pl];
        scan("00000.m2ts", &bytes, &mut pls, true);

        // The audio window flushed once (at the second video timestamp), inside
        // the clip: one core subtraction from the zero starting rate. A TrueHD
        // stream without a core (or a non-TrueHD stream — the AC3 ride-along in
        // `video_playlist`) is untouched.
        let thd = pls[0].streams.get(&0x1100).unwrap().base();
        assert_eq!(thd.active_bit_rate, -640_000);
        assert!(thd.payload_bytes > 0);
    }

    #[test]
    fn truehd_active_rate_without_a_core_is_untouched() {
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011), (0x83, 0x1100)])));
        let data = [0xAA_u8; 100];
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &data)));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &data)));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &data)));

        let mut pl = video_playlist(0x1011);
        let mut thd = TsAudioStream::default();
        thd.base.pid = Pid::new(0x1100);
        thd.base.stream_type = TsStreamType::Ac3TrueHdAudio;
        pl.streams.insert(0x1100, TsStream::Audio(thd));
        let mut pls = [pl];
        scan("00000.m2ts", &bytes, &mut pls, true);

        let thd = pls[0].streams.get(&0x1100).unwrap().base();
        assert_eq!(thd.active_bit_rate, 0);
        assert!(thd.payload_bytes > 0);
    }

    #[test]
    fn pts_only_video_leaves_length_zero_and_sets_peak() {
        // PTS-only video: the length math reads the DTS accumulator (never set on
        // a PTS-only stream), so it stays 0. The demux still accumulates payload.
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let data = [0x5A_u8; 64];
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 90_000, &data)));
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 180_000, &data)));
        let mut pls = [video_playlist(0x1011)];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);

        assert_eq!(file.length.to_bits(), 0.0_f64.to_bits());
        assert_eq!(file.streams.get(&0x1011).unwrap().base().payload_bytes, 64);
        assert_eq!(file.stream_states.get(&0x1011).unwrap().peak_transfer_length, 64);
    }

    #[test]
    fn audio_peak_transfer_rate_tracks_the_bitrate() {
        // Two bounded audio PES with a 1s PTS gap; the second triggers the audio
        // peak-rate estimate round(transfer_length*8 / (pts_transfer/90000)).
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x81, 0x1100)])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &[0u8; 50])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 180_000, &[0u8; 50])));
        // A trailing PUSI packet finalises the second PES so its window is scanned.
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 270_000, &[0u8; 50])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);

        // pts_transfer = 90 000 (1s); the second/third PES each transfer 50 bytes ⇒
        // round(50*8 / 1.0) = 400 bits/s.
        let st = file.stream_states.get(&0x1100).unwrap();
        assert_eq!(st.peak_transfer_rate, 400);
        assert!(st.transfer_count >= 2);
    }

    #[test]
    fn unregistered_pid_packets_are_skipped() {
        // A packet on a PID never announced by the PMT hits the skip branch; it is
        // counted only in Size, never registered.
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011)])));
        bytes.extend(packet(0x1FFF, true, &[0xCC; 20])); // unknown PID
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert!(file.streams.contains_key(&0x1011));
        assert!(!file.streams.contains_key(&0x1FFF));
        assert_eq!(file.size, u64::try_from(bytes.len()).unwrap());
    }

    /// PAT + PMT(one AVC video) prefix shared by several PES tests.
    fn pat_pmt_video() -> Vec<u8> {
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        bytes
    }

    /// A minimal AVC sequence-parameter-set access unit (`00 00 01 67`, High Profile
    /// 4.1), which the wired AVC scanner initialises from — used where a test needs
    /// the demuxed video stream to actually reach `is_initialized` (e.g. a non-full
    /// scan's early stop).
    fn sps_au() -> Vec<u8> {
        vec![0x00, 0x00, 0x01, 0x67, 100, 0x00, 41, 0x00]
    }

    #[test]
    fn variable_length_pes_finalizes_on_the_next_pusi() {
        // An unbounded PES (length 0) transfers to the TS-packet end and is closed
        // by the next payload-unit start (the PUSI finalise path).
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_variable(0xE0, 90_000, &[1, 2, 3, 4])));
        bytes.extend(packet(0x1011, true, &pes_variable(0xE0, 180_000, &[5, 6, 7, 8])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        // The synthetic ES does not initialise the AVC codec; the demux still
        // finalises the variable PES, which is what this test verifies.
        assert!(file.stream_states.get(&0x1011).unwrap().transfer_count >= 1);
    }

    #[test]
    fn audio_private_stream_pes_spans_a_partial_final_packet() {
        // A DTS-HD MA track carries its audio over the `0x01FD` ("extended" /
        // private-stream) PES start code with a *bounded* PES long enough (length
        // 600) that its trailing TS packets leave the buffer shorter than the PES
        // still owes but at least one TS packet long. That reaches the non-variable
        // `(bl - i) >= parser.packet_length` transfer arm (the buffer can't satisfy
        // the whole remaining PES, so the first arm's `>= state.packet_length` fails)
        // and the audio `parse == 0x0000_01FD` header — the two paths a real UHD
        // disc's DTS-HD audio reaches and no synthetic 0x01BD/0x01E0 stream does.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x86, 0x1100)]))); // DTS-HD MA
        // PES header: 0x01FD start code, PES_packet_length 600 (0x0258), a PTS, then
        // 156 payload bytes — exactly one packet's worth.
        let mut pes = vec![0x00, 0x00, 0x01, 0xFD, 0x02, 0x58, 0x80, 0x80, 0x05];
        pes.extend_from_slice(&encode_pts(90_000));
        pes.extend_from_slice(&[0x5A; 156]);
        bytes.extend(packet(0x1100, true, &pes));
        // Two continuation packets: the bounded PES still owes >184 bytes when the
        // buffer ends, so each spans the non-variable transfer arm.
        bytes.extend(packet(0x1100, false, &[0xBB; 184]));
        bytes.extend(packet(0x1100, false, &[0xCC; 184]));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert_eq!(
            file.streams.get(&0x1100).map(TsStream::stream_type),
            Some(TsStreamType::DtsHdMasterAudio)
        );
        assert!(file.stream_states.get(&0x1100).unwrap().total_bytes > 0);
    }

    #[test]
    fn non_full_scan_stops_after_a_bounded_pes_completes() {
        // One bounded video PES (carrying an SPS) completes within its packet → the
        // AVC scanner initialises it → the finish check reports the single stream
        // done → a non-full scan returns before the trailing data.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &sps_au())));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[0; 40])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, false);
        assert!(file.streams.get(&0x1011).unwrap().base().is_initialized);
        // The early exit returns mid-chunk, before the chunk's size is
        // accumulated — the trailing PES was never scanned.
        assert_eq!(file.size, 0);
    }

    #[test]
    fn non_full_scan_early_exit_stops_the_pipelined_reader() {
        // With one packet per tiny chunk, the finishing PES sits in the third
        // chunk and plenty of trailing chunks follow: the read side keeps
        // recycling buffers until the worker's early exit closes the free
        // channel (the pipeline's recv-disconnect stop). Only the two chunks
        // before the finishing one count toward the size.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &sps_au())));
        for n in 0..8_u64 {
            let pts = 180_000_u64.wrapping_add(n.wrapping_mul(3600));
            bytes.extend(packet(0x1011, true, &pes_dts(0xE0, pts, pts, &[0; 40])));
        }
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let mut cur = Cursor::new(bytes);
        file.scan_chunked(&mut cur, &mut pls, false, 192, &mut |_, _| {}).expect("scan");
        assert!(file.streams.get(&0x1011).unwrap().base().is_initialized);
        // The PAT and PMT chunks were parsed in full; the chunk holding the
        // finishing PES early-returned before its size was added.
        assert_eq!(file.size, 384);
    }

    #[test]
    fn non_full_scan_stops_at_a_pusi_finalize() {
        // The variable PES (carrying an SPS) is only finalised at the next PUSI; on a
        // non-full scan that finalise initialises the AVC stream, reports finished,
        // and returns (the header-case-0 exit).
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_variable(0xE0, 90_000, &sps_au())));
        bytes.extend(packet(0x1011, true, &pes_variable(0xE0, 180_000, &[2; 8])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, false);
        assert!(file.streams.get(&0x1011).unwrap().base().is_initialized);
        // The early exit returns mid-chunk, before the chunk's size is
        // accumulated — the finalising PUSI packet was the last one scanned.
        assert_eq!(file.size, 0);
    }

    #[test]
    fn pes_without_a_timestamp_transfers_immediately() {
        // A PES with no PTS/DTS and a zero optional header goes straight to transfer.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_none(0xE0, &[7; 30])));
        bytes.extend(packet(0x1011, true, &pes_none(0xE0, &[8; 30])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        // The synthetic ES does not initialise the AVC codec; the demux still
        // transfers the timestamp-free PES payload, which is what this test verifies.
        assert!(file.stream_states.get(&0x1011).unwrap().total_bytes > 0);
    }

    #[test]
    fn dts_pes_with_stuffing_and_non_increasing_pts() {
        // The second DTS PES has a lower PTS (the running-max `pts_last.max(pts)`
        // keeps the old value) and 2 stuffing bytes after the timestamps (the optional
        // header drain runs in the DTS path).
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_dts_padded(0xE0, 180_000, 170_000, 2, &[0; 30])));
        bytes.extend(packet(0x1011, true, &pes_dts_padded(0xE0, 90_000, 80_000, 2, &[0; 30])));
        bytes.extend(packet(0x1011, true, &pes_dts_padded(0xE0, 270_000, 260_000, 2, &[0; 30])));
        let mut pls = [video_playlist(0x1011)];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        // The non-increasing PTS / stuffing handling is the subject; the demux
        // transfers all three PES regardless of codec initialisation.
        assert!(file.stream_states.get(&0x1011).unwrap().transfer_count >= 1);
    }

    #[test]
    fn nonzero_section_number_does_not_reset_the_offset() {
        // section_number == last_section_number == 1: registration still runs, but
        // the `section_number == 0` offset reset is skipped for both PAT and PMT.
        let mut pat = pat_payload(0x0100);
        *pat.get_mut(7).unwrap() = 0x01; // section_number = 1
        *pat.get_mut(8).unwrap() = 0x01; // last_section_number = 1
        let mut bytes = packet(0, true, &pat);
        let mut section = vec![0x00];
        section.extend(pmt_section(0x02, &[], &[(0x1B, 0x1011)], None));
        *section.get_mut(7).unwrap() = 0x01; // PMT section_number = 1
        *section.get_mut(8).unwrap() = 0x01; // PMT last_section_number = 1
        bytes.extend(packet(0x0100, true, &section));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn pes_optional_header_stuffing_is_skipped() {
        // header_len = PTS(5) + 3 stuffing bytes exercises the pes_header_length drain.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_pts_padded(0xE0, 90_000, 3, &[9; 20])));
        bytes.extend(packet(0x1011, true, &pes_pts_padded(0xE0, 180_000, 3, &[9; 20])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        // The PES optional-header stuffing drain is the subject; the demux transfers
        // the payload regardless of codec initialisation.
        assert!(file.stream_states.get(&0x1011).unwrap().transfer_count >= 1);
    }

    #[test]
    fn text_pes_uses_the_non_video_non_audio_start_codes() {
        // A subtitle stream is neither video nor audio; a PES whose start code is in
        // the video range still matches via the `!is_video && !is_audio` branch, and —
        // being already initialised — takes the count-only transfer path.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x92, 0x1A00)])));
        bytes.extend(packet(0x1A00, true, &pes_pts(0xE5, 90_000, &[3; 30])));
        bytes.extend(packet(0x1A00, true, &pes_pts(0xE5, 180_000, &[4; 30])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        // Subtitle streams are initialised from construction; the demux still tracks
        // their transferred bytes.
        assert!(file.streams.get(&0x1A00).unwrap().base().is_initialized);
        assert!(file.stream_states.get(&0x1A00).unwrap().total_bytes > 0);
    }

    #[test]
    fn initialized_audio_takes_the_count_only_transfer_path() {
        // The first bounded AC-3 PES (a real 5.1 syncframe) initialises the stream
        // at its codec seam; the second PES must then arrive at the seam EMPTY —
        // the demux stops buffering an initialised audio stream's payload (the
        // count-only transfer path) while still tallying its bytes. An audio
        // stream misclassified as video or graphics would keep buffering.
        let ac3_frame = [0x0B, 0x77, 0x00, 0x00, 0x24, 0x40, 0xE1, 0xF8, 0x00, 0x00, 0x00];
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x81, 0x1100)])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &ac3_frame)));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 93_600, &ac3_frame)));
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let mut cur = Cursor::new(bytes);
        let mut seams: Vec<(u16, u64)> = Vec::new();
        file.scan_chunked(&mut cur, &mut pls, true, 5_242_880, &mut |pid, buffer| {
            seams.push((pid, buffer.length()));
        })
        .expect("scan");
        assert!(file.streams.get(&0x1100).unwrap().base().is_initialized);
        // First seam: the buffered not-yet-initialised payload; second: empty.
        assert_eq!(seams, vec![(0x1100, 11), (0x1100, 0)]);
        // The count-only path still tallies the transferred bytes.
        assert!(file.stream_states.get(&0x1100).unwrap().total_bytes >= 22);
    }

    #[test]
    fn adaptation_field_filling_the_packet_resyncs() {
        // An adaptation-only packet (AFC = 10) whose field spans the whole packet
        // drives the adaptation countdown to the packet end.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet_raw(0x1011, false, 0x2, &[183]));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 40])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn adaptation_then_payload_is_parsed() {
        // AFC = 11 (adaptation + payload): a 4-byte adaptation field then a PES.
        let mut bytes = pat_pmt_video();
        let mut pl = vec![4_u8, 0, 0, 0, 0]; // adaptation_field_length = 4, then 4 bytes
        pl.extend(pes_dts(0xE0, 90_000, 90_000, &[0; 20]));
        bytes.extend(packet_raw(0x1011, true, 0x3, &pl));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[0; 20])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        // The adaptation-then-payload framing is the subject; the demux transfers the
        // PES regardless of codec initialisation.
        assert!(file.stream_states.get(&0x1011).unwrap().transfer_count >= 1);
    }

    #[test]
    fn oversized_adaptation_field_does_not_bleed_into_the_next_packet() {
        // An adaptation-only packet (AFC = 10) whose length byte claims 255 bytes —
        // far past the 183 a 188-byte TS packet can hold — must be clamped to this
        // packet so its leftover countdown cannot consume the next packet's PES
        // payload. Without the clamp the following PES start code is eaten
        // and no transfer is ever registered.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet_raw(0x1011, false, 0x2, &[0xFF])); // AF length = 255 (> 183)
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 40])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert!(file.stream_states.get(&0x1011).unwrap().transfer_count >= 1);
    }

    #[test]
    fn pmt_pointer_field_and_program_info_are_skipped() {
        // A non-zero pointer field precedes the section, and a 4-byte program-info
        // block precedes the stream entries; both are consumed before registration.
        let mut section = vec![2_u8, 0xFF, 0xFF]; // pointer_field = 2 + 2 skip bytes
        section.extend(pmt_section(0x02, &[0xAA, 0xBB, 0xCC, 0xDD], &[(0x1B, 0x1011)], None));
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &section));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert_eq!(
            file.streams.get(&0x1011).map(TsStream::stream_type),
            Some(TsStreamType::AvcVideo)
        );
    }

    #[test]
    fn pmt_with_wrong_table_id_registers_nothing() {
        // A section whose table_id is not 0x02 is rejected at the length-prefix step.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        let mut section = vec![0x00]; // pointer field
        section.extend(pmt_section(0x05, &[], &[(0x1B, 0x1011)], None));
        bytes.extend(packet(0x0100, true, &section));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert!(file.streams.is_empty());
    }

    #[test]
    fn a_pat_with_a_wrong_table_id_registers_nothing() {
        // A PID-0 section whose table_id is not 0x00 is rejected at the
        // length-prefix step, so the PMT PID it announces is never adopted and
        // the PMT that follows registers nothing. Classic BDInfo assembles the
        // mislabeled section unchecked — see DIFFERENCES.md; no conforming
        // disc carries a wrong PAT table id.
        let mut pat = pat_payload(0x0100);
        *pat.get_mut(1).unwrap() = 0x05; // table_id (byte 1, behind the pointer field)
        let mut bytes = packet(0, true, &pat);
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.is_empty());

        // The reject leaves the machine idle, not wedged: a correct PAT in the
        // next payload unit parses, and the same PMT registers its stream.
        let mut bytes = packet(0, true, &pat);
        bytes.extend(packet(0, true, &pat_payload(0x0100)));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn oversized_section_lengths_are_rejected() {
        // A PMT and a PAT each declaring section_length > 1021 reset to zero.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        let mut section = vec![0x00];
        section.extend(pmt_section(0x02, &[], &[(0x1B, 0x1011)], Some(1022)));
        bytes.extend(packet(0x0100, true, &section));
        // A PAT with section_length 0x3FF+ (the high nibble alone exceeds 1021).
        let mut bad_pat = pat_payload(0x0100);
        *bad_pat.get_mut(2).unwrap() = 0xBF; // section_length high nibble = 0xF → > 1021
        bytes.extend(packet(0, true, &bad_pat));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert!(file.streams.is_empty());
    }

    #[test]
    fn pat_pointer_field_is_skipped() {
        let mut pat = vec![3_u8, 0xFF, 0xFF, 0xFF]; // pointer_field = 3 + 3 skip bytes
        pat.extend(pat_payload(0x0100).into_iter().skip(1)); // drop its 0 pointer
        let mut bytes = packet(0, true, &pat);
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn two_video_streams_skip_the_non_pts_source() {
        // With two video PIDs, an update for one skips the other (it is video and not
        // the PTS source) in the first update_stream_bitrates loop.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011), (0x1B, 0x1012)])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 30])));
        bytes.extend(packet(0x1012, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 30])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[0; 30])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert!(file.streams.contains_key(&0x1011));
        assert!(file.streams.contains_key(&0x1012));
    }

    #[test]
    fn mvc_only_program_never_finishes_a_non_full_scan() {
        // An MVC stream with no AVC base trips the `is_mvc && !is_avc` guard, so even a
        // non-full scan keeps reading (the finish check never reports done).
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x20, 0x1B00)])));
        bytes.extend(packet(0x1B00, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 30])));
        bytes.extend(packet(0x1B00, true, &pes_dts(0xE0, 180_000, 180_000, &[0; 30])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, false);
        // The MVC stream is registered and initialised, but the program is not
        // "finished" (MVC needs an AVC base), so the whole file was scanned.
        assert_eq!(
            file.streams.get(&0x1B00).map(TsStream::stream_type),
            Some(TsStreamType::MvcVideo)
        );
        assert_eq!(file.size, u64::try_from(bytes.len()).unwrap());
    }

    #[test]
    fn angle_streams_and_unmatched_clips_are_handled() {
        // The playlist carries an angle clip (its streams live in angle_streams), a
        // clip for a different m2ts, and a clip whose time window excludes the PTS.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 40])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[0; 40])));

        let mk_video = || {
            let mut vs = TsVideoStream::default();
            vs.base.pid = Pid::new(0x1011);
            vs.base.stream_type = TsStreamType::AvcVideo;
            vs.base.is_vbr = true;
            TsStream::Video(vs)
        };
        let clip = |name: &str, time_in: f64, time_out: f64, angle: i32| TsStreamClip {
            name: name.to_owned(),
            time_in,
            time_out,
            angle_index: angle,
            ..TsStreamClip::default()
        };
        let mut pl = empty_playlist();
        pl.stream_clips = vec![
            clip("00000.M2TS", 0.0, 1000.0, 0),  // main clip
            clip("00000.M2TS", 0.0, 1000.0, 1),  // angle clip → angle_streams[0]
            clip("OTHER.M2TS", 0.0, 1000.0, 0),  // different m2ts → name mismatch
            clip("00000.M2TS", 500.0, 600.0, 0), // outside the PTS window
        ];
        pl.streams = BTreeMap::from([(0x1011, mk_video())]);
        pl.angle_streams = vec![BTreeMap::from([(0x1011, mk_video())])];
        let mut pls = [pl];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);

        assert!(file.streams.contains_key(&0x1011));
        // Both the main and the angle stream accumulated payload + a VBR bitrate.
        let main = pls[0].streams.get(&0x1011).unwrap().base();
        let angle = pls[0].angle_streams.first().unwrap().get(&0x1011).unwrap().base();
        assert!(main.payload_bytes > 0 && main.bit_rate > 0);
        assert!(angle.payload_bytes > 0 && angle.bit_rate > 0);
    }

    #[test]
    fn large_section_spans_packets_and_chunks() {
        // A PMT whose section exceeds one TS packet forces the cross-packet (and,
        // with a small read size, cross-chunk) section-assembly paths.
        let streams: Vec<(u8, u16)> = (0..40).map(|n| (0x1B, 0x1011_u16.wrapping_add(n))).collect();
        let mut section = vec![0x00];
        section.extend(pmt_section(0x02, &[], &streams, None));
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        let split = 184.min(section.len());
        bytes.extend(packet(0x0100, true, section.get(..split).unwrap()));
        bytes.extend(packet(0x0100, false, section.get(split..).unwrap()));
        // A completing PES so the codec-seam observer fires at least once.
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 30])));

        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let mut cur = Cursor::new(bytes);
        let mut seen = 0_u32;
        file.scan_chunked(&mut cur, &mut pls, true, 100, &mut |_, _| seen = seen.wrapping_add(1))
            .expect("scan");
        assert!(seen > 0);
        // Every announced stream registered despite the split section.
        assert_eq!(file.streams.len(), 40);
        assert!(file.streams.contains_key(&0x1011));
        assert!(file.streams.contains_key(&0x1038)); // 0x1011 + 39
    }

    #[test]
    fn variable_pes_continuing_into_an_adaptation_packet_clears_both_flags() {
        // A variable PES (packet_length_variable set, variable_packet_end cleared
        // on detect) continues into a PUSI-less AFC=11 packet whose adaptation
        // re-sets variable_packet_end — so the transfer's
        // `variable_packet_end && packet_length_variable` arm fires and both
        // flags clear.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_variable(0xE0, 90_000, &[1; 8])));
        bytes.extend(packet_raw(0x1011, false, 0x3, &[2, 0, 0])); // continuation + adaptation
        bytes.extend(packet(0x1011, true, &pes_variable(0xE0, 180_000, &[2; 8])));
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        // The variable-end flag-clearing path is the subject; the demux
        // transfers the PES regardless of codec initialisation.
        assert!(file.stream_states.get(&0x1011).unwrap().transfer_count >= 1);
    }

    /// A raw PAT section (with pointer field) listing `programs` (`(number, pid)`).
    fn pat_section_big(programs: &[(u16, u16)]) -> Vec<u8> {
        let mut entries = Vec::new();
        for &(num, pid) in programs {
            let [nhi, nlo] = num.to_be_bytes();
            let [phi, plo] = pid.to_be_bytes();
            entries.extend_from_slice(&[nhi, nlo, 0xE0 | (phi & 0x1F), plo]);
        }
        let section_len = entries.len().wrapping_add(9); // 5 header + 4 CRC
        let [slhi, sllo] = u16::try_from(section_len).unwrap().to_be_bytes();
        let mut p = vec![0x00, 0x00, 0xB0 | (slhi & 0x0F), sllo, 0x00, 0x01, 0xC1, 0x00, 0x00];
        p.extend_from_slice(&entries);
        p.extend_from_slice(&[0, 0, 0, 0]); // CRC
        p
    }

    #[test]
    fn large_pat_spans_packets_and_chunks() {
        // A PAT with 50 programs exceeds one TS packet; the section assembly is
        // exercised with both a whole-file read (section capped by the TS packet)
        // and a tiny read (section capped by the chunk).
        let mut programs = vec![(1_u16, 0x0100_u16)];
        programs.extend((0..49_u16).map(|n| (n.wrapping_add(2), 0x0200_u16.wrapping_add(n))));
        let pat = pat_section_big(&programs);
        let split = 184.min(pat.len());
        let mut bytes = Vec::new();
        bytes.extend(packet(0, true, pat.get(..split).unwrap()));
        bytes.extend(packet(0, false, pat.get(split..).unwrap()));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 30])));

        for chunk in [5_242_880_usize, 100] {
            let mut file = TsStreamFile::new("00000.m2ts");
            let mut pls = [empty_playlist()];
            let mut cur = Cursor::new(bytes.clone());
            let mut seen = 0_u32;
            file.scan_chunked(&mut cur, &mut pls, true, chunk, &mut |_, _| {
                seen = seen.wrapping_add(1);
            })
            .expect("scan");
            // The large PAT located program 1's PMT, which registered the video.
            assert!(file.streams.contains_key(&0x1011), "chunk {chunk}");
            assert!(seen > 0, "chunk {chunk}");
        }
    }

    #[test]
    fn duplicate_pmt_and_repeated_pts_are_idempotent() {
        // A repeated PMT must not re-register; a repeated identical PTS must take the
        // `pts == pts_last` path.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)]))); // duplicate PMT
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 90_000, &[0; 30])));
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 90_000, &[0; 30]))); // same PTS
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 90_000, &[0; 30]))); // same PTS again
        let mut pls = [empty_playlist()];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert_eq!(file.streams.len(), 1);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn multi_section_tables_skip_registration() {
        // A PAT/PMT whose section_number ≠ last_section_number is assembled but
        // not acted on (the section-number == last-section-number guard fails).
        let mut bad_pat = pat_payload(0x0100);
        *bad_pat.get_mut(8).unwrap() = 0x01; // last_section_number = 1, section_number = 0
        let pat_only =
            scan("00000.m2ts", &packet(0, true, &bad_pat), &mut [empty_playlist()], true);
        assert_eq!(pat_only.streams.len(), 0); // no PMT PID learned → nothing registered

        // A valid PAT, then a PMT whose section_number ≠ last_section_number.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        let mut section = vec![0x00];
        section.extend(pmt_section(0x02, &[], &[(0x1B, 0x1011)], None));
        *section.get_mut(8).unwrap() = 0x01; // last_section_number = 1
        bytes.extend(packet(0x0100, true, &section));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.is_empty()); // PMT section ignored
    }

    #[test]
    fn public_types_support_the_derived_traits() {
        // Exercise the derived Debug/Clone/PartialEq/Default on the public types.
        let d = TsStreamDiagnostics::default();
        assert_eq!(d.clone(), d);
        assert_ne!(d, TsStreamDiagnostics { bytes: 1, ..TsStreamDiagnostics::default() });
        assert!(format!("{d:?}").contains("TsStreamDiagnostics"));
        // A scanned file with a video stream populates the Debug-recursed maps.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 30])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[0; 30])));
        let file = scan("00000.m2ts", &bytes, &mut [video_playlist(0x1011)], true);
        let dump = format!("{file:?}");
        assert!(dump.starts_with("TsStreamFile"));
    }

    /// A reader that yields one `Interrupted` error before delegating to its data —
    /// exercises `fill_buffer`'s retry path.
    struct InterruptOnce {
        inner: Cursor<Vec<u8>>,
        tripped: bool,
    }

    impl Read for InterruptOnce {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.tripped {
                self.tripped = true;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
            }
            self.inner.read(buf)
        }
    }

    /// A reader that always errors — exercises `fill_buffer`'s IO-error path.
    struct AlwaysError;

    impl Read for AlwaysError {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }

    #[test]
    fn fill_buffer_retries_on_interrupted_and_propagates_errors() {
        let bytes = pat_pmt_video();
        let mut reader = InterruptOnce { inner: Cursor::new(bytes), tripped: false };
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        file.scan(&mut reader, &mut pls, true).expect("interrupted read retried");
        assert!(file.streams.contains_key(&0x1011));

        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let err = file.scan(&mut AlwaysError, &mut pls, true).unwrap_err();
        assert_eq!(err.to_string(), "io error: read failed");
    }

    #[test]
    fn a_preset_cancel_flag_aborts_before_the_first_read() {
        // The reader always errors, so reaching it at all would surface `Io` —
        // the returned `ScanCancelled` proves the flag is polled BEFORE the
        // first read (a pre-cancelled scan pulls no bytes at all).
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let err = file
            .scan_cancellable(&mut AlwaysError, &mut pls, true, &AtomicBool::new(true), None)
            .unwrap_err();
        assert_eq!(err.to_string(), "scan cancelled");
    }

    /// A reader that trips the cancel flag as it serves each chunk — the shape
    /// every real driver has (a UI button, a Ctrl+C handler): the flag flips
    /// while a chunk is in flight, and the scan must stop at the next chunk
    /// boundary instead of reading on to EOF.
    struct TripAfterServing<'a> {
        inner: Cursor<Vec<u8>>,
        cancel: &'a AtomicBool,
    }

    impl Read for TripAfterServing<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf);
            self.cancel.store(true, Ordering::Relaxed);
            n
        }
    }

    #[test]
    fn a_mid_scan_cancel_stops_at_the_next_chunk_boundary() {
        // A stream one byte longer than a read chunk; serving the first chunk
        // trips the flag, so the scan aborts with `ScanCancelled` before the
        // second chunk and the tail is never read — a cancel raised while a
        // chunk is in flight takes effect within that one chunk's bytes.
        let cancel = AtomicBool::new(false);
        let bytes = vec![0_u8; DATA_SIZE.wrapping_add(1)];
        let mut reader = TripAfterServing { inner: Cursor::new(bytes), cancel: &cancel };
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let err = file.scan_cancellable(&mut reader, &mut pls, true, &cancel, None).unwrap_err();
        assert_eq!(err.to_string(), "scan cancelled");
        let consumed = usize::try_from(reader.inner.position()).expect("position fits");
        assert!(consumed <= DATA_SIZE, "at most the in-flight chunk is read after the trip");
    }

    /// A reader recording the destination length of each read call — how the
    /// chunk size a scan entry selected reaches the underlying source.
    struct SizeRecorder {
        inner: Cursor<Vec<u8>>,
        sizes: Vec<usize>,
    }

    impl Read for SizeRecorder {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.sizes.push(buf.len());
            self.inner.read(buf)
        }
    }

    #[test]
    fn the_default_scan_entries_select_the_chunk_size_by_pass() {
        // The payload does not matter here — only the destination length the
        // demux hands the reader, which for a fresh chunk is the whole chunk.
        // Both default entries (`scan` and `scan_cancellable`) must select the
        // same size per pass: the disc-level open drives the cancellable one,
        // and the plain one must not diverge from it.
        let bytes = vec![0_u8; 4096];
        let first_size = |cancellable: bool, full: bool| {
            let mut recorder =
                SizeRecorder { inner: Cursor::new(bytes.clone()), sizes: Vec::new() };
            let mut file = TsStreamFile::new("00000.m2ts");
            let mut pls = [empty_playlist()];
            if cancellable {
                file.scan_cancellable(&mut recorder, &mut pls, full, &AtomicBool::new(false), None)
                    .expect("scan");
            } else {
                file.scan(&mut recorder, &mut pls, full).expect("scan");
            }
            recorder.sizes.first().copied()
        };
        assert_eq!(first_size(false, true), Some(DATA_SIZE));
        assert_eq!(first_size(true, true), Some(DATA_SIZE));
        assert_eq!(first_size(false, false), Some(QUICK_DATA_SIZE));
        assert_eq!(first_size(true, false), Some(QUICK_DATA_SIZE));
    }

    // ── the sequential scan strategy (the wasm32 build's path) ──────────────

    /// The shared no-op codec-seam observer for the sequential-strategy tests
    /// (one named function rather than per-test `|_, _| {}` literals: the
    /// no-data tests never reach a seam, and an unexecuted closure is an
    /// uncovered function of its own).
    fn no_seam(_pid: u16, _buffer: &TsStreamBuffer) {}

    /// Runs [`TsStreamFile::scan_sequential`] over `bytes` with an explicit
    /// chunk size and an unset cancel flag, returning the demuxer.
    fn seq_scan(
        name: &str,
        bytes: &[u8],
        playlists: &mut [TsPlaylistFile],
        full: bool,
        chunk: usize,
    ) -> TsStreamFile {
        let mut file = TsStreamFile::new(name);
        let mut cur = Cursor::new(bytes.to_vec());
        file.scan_sequential(
            &mut cur,
            playlists,
            full,
            chunk,
            &AtomicBool::new(false),
            &mut no_seam,
        )
        .expect("sequential scan");
        file
    }

    #[test]
    fn a_sequential_scan_on_empty_playlists_is_a_noop() {
        // The reader always errors, so returning `Ok` proves the empty-playlist
        // early return fires before any read.
        let mut file = TsStreamFile::new("00000.m2ts");
        file.scan_sequential(
            &mut AlwaysError,
            &mut [],
            true,
            4096,
            &AtomicBool::new(false),
            &mut no_seam,
        )
        .expect("no-op scan");
        assert_eq!(file.size, 0);
        assert!(file.streams.is_empty());
    }

    #[test]
    fn a_sequential_non_full_scan_stops_at_the_finishing_chunk() {
        // One packet per tiny chunk; the PES that finishes the quick scan sits
        // in the third chunk and eight trailing chunks follow. Mirrors
        // `non_full_scan_early_exit_stops_the_pipelined_reader` on the
        // sequential strategy, where the stop is directly observable on the
        // reader: no read-ahead, so the cursor rests exactly at the finishing
        // chunk's end.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &sps_au())));
        for n in 0..8_u64 {
            let pts = 180_000_u64.wrapping_add(n.wrapping_mul(3600));
            bytes.extend(packet(0x1011, true, &pes_dts(0xE0, pts, pts, &[0; 40])));
        }
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let mut cur = Cursor::new(bytes);
        file.scan_sequential(&mut cur, &mut pls, false, 192, &AtomicBool::new(false), &mut no_seam)
            .expect("scan");
        assert!(file.streams.get(&0x1011).unwrap().base().is_initialized);
        // The PAT and PMT chunks were parsed in full; the chunk holding the
        // finishing PES early-returned before its size was added.
        assert_eq!(file.size, 384);
        assert_eq!(cur.position(), 576);
    }

    #[test]
    fn a_sequential_scan_propagates_read_errors() {
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let err = file
            .scan_sequential(
                &mut AlwaysError,
                &mut pls,
                true,
                4096,
                &AtomicBool::new(false),
                &mut no_seam,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "io error: read failed");
    }

    #[test]
    fn a_sequential_preset_cancel_aborts_before_the_first_read() {
        // The reader always errors, so reaching it at all would surface `Io` —
        // the returned `ScanCancelled` proves the flag is polled BEFORE the
        // first read, exactly as on the threaded strategy.
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let err = file
            .scan_sequential(
                &mut AlwaysError,
                &mut pls,
                true,
                4096,
                &AtomicBool::new(true),
                &mut no_seam,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "scan cancelled");
    }

    // ── the measured-snapshot chunk cadence ──────────────────────────────────

    /// A demuxable stream whose per-clip tallies move across several 192-byte
    /// chunks: PAT, PMT, then one bounded video PES per second of `1..=frames`
    /// (each closing the previous frame's window, so the playlists tally as
    /// the parse proceeds).
    fn framed_video(frames: u64) -> Vec<u8> {
        let mut bytes = pat_pmt_video();
        for second in 1..=frames {
            let ticks = second.wrapping_mul(90_000);
            bytes.extend(packet(0x1011, true, &pes_dts(0xE0, ticks, ticks, &[0x5A; 40])));
        }
        bytes
    }

    /// Runs the strategy under test over `bytes` with a snapshot-collecting
    /// measured observer, returning the collected snapshots and the scan's
    /// result — the shared driver of the chunk-cadence tests, which compare
    /// the two strategies' sequences.
    fn observed_strategy_scan(
        threaded: bool,
        reader: &mut dyn Read,
        full: bool,
        chunk: usize,
        cancel: &AtomicBool,
    ) -> (Vec<MeasuredSnapshot>, Result<(), BdError>) {
        let mut snapshots: Vec<MeasuredSnapshot> = Vec::new();
        let mut watch = |snapshot: MeasuredSnapshot| snapshots.push(snapshot);
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [video_playlist(0x1011)];
        let result = if threaded {
            file.scan_threaded_observed(
                reader,
                &mut pls,
                full,
                chunk,
                cancel,
                &mut no_seam,
                Some(&mut watch),
            )
        } else {
            file.scan_sequential_observed(
                reader,
                &mut pls,
                full,
                chunk,
                cancel,
                &mut no_seam,
                Some(&mut watch),
            )
        };
        (snapshots, result)
    }

    #[test]
    fn both_strategies_snapshot_every_chunk_of_a_full_scan_identically() {
        // One 192-byte packet per chunk over a stream whose windows close as
        // frames arrive: the observer sees one snapshot per parsed chunk, and
        // the threaded pipeline hands over exactly the sequence the
        // sequential loop produces inline.
        let bytes = framed_video(6);
        let chunks = bytes.len() / 192;
        let run = |threaded: bool| {
            let mut cur = Cursor::new(bytes.clone());
            let (snapshots, result) =
                observed_strategy_scan(threaded, &mut cur, true, 192, &AtomicBool::new(false));
            result.expect("scan");
            snapshots
        };
        let threaded = run(true);
        let sequential = run(false);
        assert_eq!(threaded.len(), chunks, "one snapshot per parsed chunk");
        assert_eq!(threaded, sequential);
        // Every snapshot is taken over this clip and covers the one playlist
        // that plays it, and the byte tallies only ever grow.
        assert!(threaded.iter().all(|s| s.file == "00000.M2TS"));
        let tallies: Vec<u64> = threaded
            .iter()
            .map(|s| s.playlists.first().expect("the playlist plays the clip").measured_bytes)
            .collect();
        assert!(
            tallies
                .windows(2)
                .all(|pair| pair.first().is_none_or(|a| pair.get(1).is_none_or(|b| a <= b))),
            "tallies never regress"
        );
        assert!(tallies.last().is_some_and(|&bytes| bytes > 0), "the demuxed packets tally");
    }

    #[test]
    fn a_non_full_scan_never_snapshots_even_with_an_observer_installed() {
        // A quick pass reads an unpredictable head sliver whose tallies the
        // disc-level open discards before measuring, so neither strategy
        // reports it: the same early-finishing stream that drives the
        // non-full stop tests yields no snapshot at all.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &sps_au())));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[0; 40])));
        for threaded in [true, false] {
            let mut cur = Cursor::new(bytes.clone());
            let (snapshots, result) =
                observed_strategy_scan(threaded, &mut cur, false, 192, &AtomicBool::new(false));
            result.expect("scan");
            assert!(snapshots.is_empty(), "threaded={threaded} snapshotted a non-full scan");
        }
    }

    #[test]
    fn a_cancelled_scan_still_delivers_the_snapshots_of_the_parsed_chunks() {
        // Serving the first chunk trips the flag, so the scan aborts before
        // the second — but the first chunk was parsed, and its snapshot
        // reaches the observer before the error returns (on the threaded
        // strategy that is the post-loop channel drain), identically in both
        // strategies.
        let bytes = framed_video(7);
        let chunk = 192 * 4;
        let run = |threaded: bool| {
            let cancel = AtomicBool::new(false);
            let mut reader =
                TripAfterServing { inner: Cursor::new(bytes.clone()), cancel: &cancel };
            let (snapshots, result) =
                observed_strategy_scan(threaded, &mut reader, true, chunk, &cancel);
            assert_eq!(result.expect_err("the tripped flag cancels").to_string(), "scan cancelled");
            snapshots
        };
        let threaded = run(true);
        let sequential = run(false);
        assert_eq!(threaded.len(), 1, "exactly the one parsed chunk is snapshotted");
        assert_eq!(threaded, sequential);
    }

    /// A reader appending `"read"` to a shared log as each call is served —
    /// paired with an observer appending `"snap"`, the log shows how snapshot
    /// deliveries interleave with reads on the calling thread.
    struct LogReader<'a> {
        inner: Cursor<Vec<u8>>,
        log: &'a std::cell::RefCell<Vec<&'static str>>,
    }

    impl Read for LogReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.log.borrow_mut().push("read");
            self.inner.read(buf)
        }
    }

    #[test]
    fn the_threaded_strategy_delivers_snapshots_while_still_reading() {
        // The liveness the per-chunk cadence exists for: a snapshot must not
        // wait for the whole file. The reader's third iteration blocks on the
        // recycle channel, and the worker sends each chunk's snapshot before
        // recycling its buffer, so by the fourth read at the latest a
        // snapshot has been delivered — deterministically, despite the
        // pipeline's two-buffer read-ahead.
        let bytes = framed_video(4);
        assert!(bytes.len() / 192 >= 3, "the interleave argument needs three chunks");
        let log = std::cell::RefCell::new(Vec::new());
        let mut reader = LogReader { inner: Cursor::new(bytes), log: &log };
        let mut watch = |_: MeasuredSnapshot| log.borrow_mut().push("snap");
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [video_playlist(0x1011)];
        file.scan_threaded_observed(
            &mut reader,
            &mut pls,
            true,
            192,
            &AtomicBool::new(false),
            &mut no_seam,
            Some(&mut watch),
        )
        .expect("scan");
        let entries = log.into_inner();
        let first_snap = entries.iter().position(|&e| e == "snap").expect("snapshots delivered");
        let last_read = entries.iter().rposition(|&e| e == "read").expect("chunks read");
        assert!(
            first_snap < last_read,
            "no snapshot before the last read: {entries:?} (all deliveries waited for EOF)"
        );
    }

    /// The decoded marker the demux records for a video update whose timestamp is
    /// `ts` — `pts_to_f64(ts) / 90000` (the `TsStreamDiagnostics::marker` formula).
    fn expect_marker(ts: u64) -> u64 {
        (pts_to_f64(i128::from(ts)) / 90000.0).to_bits()
    }

    #[test]
    fn peak_transfer_length_is_the_running_maximum() {
        // PES payloads 100 then 50 ⇒ the peak stays 100 (a `.max` taking the last
        // value would report 50; one that never updates would report 0).
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0; 100])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[0; 50])));
        let file = scan("00000.m2ts", &bytes, &mut [video_playlist(0x1011)], true);
        assert_eq!(file.stream_states.get(&0x1011).unwrap().peak_transfer_length, 100);
    }

    #[test]
    fn peak_transfer_rate_is_the_running_maximum() {
        // Audio bitrates 800 then 400 (constant PTS gap, payloads 100 then 50) ⇒
        // the peak stays 800; pins the audio bitrate `/` and the running `.max`.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x81, 0x1100)])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &[0; 100])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 180_000, &[0; 100]))); // gap 1s ⇒ 800
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 270_000, &[0; 50]))); // gap 1s ⇒ 400
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 360_000, &[0; 50])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert_eq!(file.stream_states.get(&0x1100).unwrap().peak_transfer_rate, 800);
    }

    #[test]
    fn dts_path_pts_portion_decodes_exactly() {
        // The PTS portion of a PTS+DTS PES (dts_parse cases 9..5) lands in
        // state.pts/pts_last; adversarial values pin those masks/shifts, and the DTS
        // portion (cases 4..0) lands in state.dts_prev.
        let mut bytes = pat_pmt_video();
        let pts = 0x6AAA_AAAA_u64;
        let dts = 0x1555_5555_u64;
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, pts, dts, &[0; 30])));
        let file = scan("00000.m2ts", &bytes, &mut [video_playlist(0x1011)], true);
        let st = file.stream_states.get(&0x1011).unwrap();
        assert_eq!(st.pts, i128::from(pts));
        assert_eq!(st.pts_last, i128::from(pts)); // case 5 running max from 0
        assert_eq!(st.dts_prev, i128::from(dts)); // DTS case 0
    }

    #[test]
    fn sync_consumes_the_time_code_before_locking() {
        // A `0x47` byte inside the 4-byte TP_extra_header must NOT be mistaken for
        // the sync byte — the time-code countdown consumes it first. A packet whose
        // time code contains 0x47 still frames correctly and registers its streams.
        let pmt_pid = 0x0100;
        let mut first = packet(0, true, &pat_payload(pmt_pid));
        // Overwrite the PAT packet's time code with bytes including 0x47.
        first.splice(0..4, [0x47, 0x47, 0x47, 0x47]);
        let mut bytes = first;
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn pat_program_number_selects_the_pmt_pid() {
        // The PAT program loop only adopts the PMT PID for program_number == 1; a
        // first program with a different number must be ignored (pinning that `== 1`
        // and the program-number `<< 8` accumulation).
        // section_length = 9 header + 8 entries + 4 CRC = 21 = 0x15 (byte index 3).
        let pat = vec![
            0x00, 0x00, 0xB0, 0x15, 0x00, 0x01, 0xC1, 0x00, 0x00, // header
            0x00, 0x02, 0xE2, 0x00, // program_number 2 → PID 0x0200 (ignored)
            0x00, 0x01, 0xE1, 0x00, // program_number 1 → PID 0x0100 (adopted)
            0x00, 0x00, 0x00, 0x00, // CRC
        ];
        let mut bytes = packet(0, true, &pat);
        bytes.extend(packet(0x0200, true, &pmt_payload(&[(0x1B, 0x9999)]))); // wrong PMT
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)]))); // right PMT
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
        assert!(!file.streams.contains_key(&0x9999));
    }

    #[test]
    fn dts_timestamps_decode_bit_exactly() {
        // Adversarial DTS values with bits across every mask/shift position; the
        // recorded markers pin the full DTS decode (cases 4..0), and the length pins
        // the pts_first/pts_last running min/max.
        let d = [0x6AAA_AAAA_u64, 0x5555_5555, 0x4CCC_CCCC];
        let mut bytes = pat_pmt_video();
        for &dts in &d {
            bytes.extend(packet(0x1011, true, &pes_dts(0xE0, dts, dts, &[0; 30])));
        }
        let file = scan("00000.m2ts", &bytes, &mut [video_playlist(0x1011)], true);
        let diag = file.stream_diagnostics.get(&0x1011).unwrap();
        assert_eq!(diag.len(), 2);
        assert_eq!(diag.first().unwrap().marker.to_bits(), expect_marker(d[1]));
        assert_eq!(diag.get(1).unwrap().marker.to_bits(), expect_marker(d[2]));
        // pts_first = min(d1,d2) = d2, pts_last = max = d1 ⇒ length = (d1 - d2)/90000.
        let want = (pts_to_f64(i128::from(d[1]) - i128::from(d[2])) / 90000.0).to_bits();
        assert_eq!(file.length.to_bits(), want);
    }

    #[test]
    fn pts_only_timestamps_decode_bit_exactly() {
        // Same adversarial vectors through the PTS-only path (cases 4..0); the video
        // update records marker = PTS/90000, pinning every PTS mask/shift.
        let p = [0x6AAA_AAAA_u64, 0x5555_5555, 0x4CCC_CCCC];
        let mut bytes = pat_pmt_video();
        for &pts in &p {
            bytes.extend(packet(0x1011, true, &pes_pts(0xE0, pts, &[0; 30])));
        }
        let file = scan("00000.m2ts", &bytes, &mut [video_playlist(0x1011)], true);
        let diag = file.stream_diagnostics.get(&0x1011).unwrap();
        assert_eq!(diag.len(), 2);
        assert_eq!(diag.first().unwrap().marker.to_bits(), expect_marker(p[1]));
        assert_eq!(diag.get(1).unwrap().marker.to_bits(), expect_marker(p[2]));
    }

    #[test]
    fn pid_field_masks_strip_the_high_bits() {
        // PAT/PMT/TS-header PID fields keep only their low bits; using PIDs whose
        // declared value collides with the stripped (reserved/PUSI) bits pins the
        // `& 0x1F`/`& 0x0F` masks — an OR/XOR would resolve a different PID and the
        // stream would not register where expected.
        let pmt_pid = 0x1F55; // top bits set in both PAT PID nibble and TS header
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1FAA)])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert_eq!(
            file.streams.get(&0x1FAA).map(TsStream::stream_type),
            Some(TsStreamType::AvcVideo)
        );
        assert_eq!(file.streams.len(), 1);
    }

    /// A VBR AVC video stream at `pid`, for a playlist's `Streams`/clips.
    fn vbr_video(pid: u16) -> TsStream {
        let mut vs = TsVideoStream::default();
        vs.base.pid = Pid::new(pid);
        vs.base.stream_type = TsStreamType::AvcVideo;
        vs.base.is_vbr = true;
        TsStream::Video(vs)
    }

    /// A main clip named `00000.M2TS` spanning `[time_in, time_out]`.
    fn window_clip(time_in: f64, time_out: f64) -> TsStreamClip {
        TsStreamClip { name: "00000.M2TS".to_owned(), time_in, time_out, ..TsStreamClip::default() }
    }

    #[test]
    fn clip_time_window_excludes_outside_timestamps() {
        // Updates at stream_time 10 and 11 (non-zero): a clip spanning 5..100 is
        // credited, a clip spanning 50..60 is not — pinning the window comparisons
        // (`>=`/`<=`/`&&`) and the `stream_offset > clip.time_in` guard.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 900_000, 900_000, &[0xAB; 40])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 990_000, 990_000, &[0xCD; 40])));
        let mut pl = empty_playlist();
        pl.stream_clips = vec![window_clip(5.0, 100.0), window_clip(50.0, 60.0)];
        pl.streams = BTreeMap::from([(0x1011, vbr_video(0x1011))]);
        let mut pls = [pl];
        drop(scan("00000.m2ts", &bytes, &mut pls, true));
        // stream_time 10 ∈ [5,100] but ∉ [50,60].
        assert!(pls[0].stream_clips.first().unwrap().payload_bytes > 0);
        assert_eq!(pls[0].stream_clips.get(1).unwrap().payload_bytes, 0);
    }

    #[test]
    fn zero_timestamp_credits_clips_outside_their_window() {
        // A marker of 0 (DTS 0) credits every name-matching clip via the
        // `stream_time == 0` clause even though 0 is outside its window — pinning
        // that `==` against `!=`.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[0xAB; 40])));
        bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 0, 0, &[0xCD; 40])));
        let mut pl = empty_playlist();
        pl.stream_clips = vec![window_clip(5.0, 100.0)]; // window excludes 0
        pl.streams = BTreeMap::from([(0x1011, vbr_video(0x1011))]);
        let mut pls = [pl];
        let file = scan("00000.m2ts", &bytes, &mut pls, true);
        assert!(pls[0].stream_clips.first().unwrap().payload_bytes > 0);
        assert!(file.streams.contains_key(&0x1011));
    }

    /// An AVC video stream at `pid` with `initialized` already decided — the
    /// direct-construction seed for the scan-verdict and bitrate-pass tests.
    fn seeded_video(pid: u16, initialized: bool) -> TsStream {
        let mut vs = TsVideoStream::default();
        vs.base.pid = Pid::new(pid);
        vs.base.stream_type = TsStreamType::AvcVideo;
        vs.base.is_initialized = initialized;
        TsStream::Video(vs)
    }

    #[test]
    fn finish_scan_keeps_the_running_maximum_video_timestamp() {
        // Two video PIDs whose states carry EQUAL last timestamps but different
        // previous-DTS values: the running max must NOT adopt the equal second
        // sample, so the second PID's window reuses the first PID's interval
        // (an `>=` would re-derive it from the second's own previous DTS).
        let mut file = TsStreamFile::new("00000.m2ts");
        for (pid, dts_prev) in [(0x1011_u16, 45_000_i128), (0x1012, 30_000)] {
            file.streams.insert(pid, seeded_video(pid, false));
            let state = file.stream_states.entry(pid).or_default();
            state.pts_last = 90_000;
            state.dts_prev = dts_prev;
            state.window_packets = 1;
            state.window_bytes = 100;
        }
        let mut pl = video_playlist(0x1011);
        let mut relevant: Vec<&mut TsPlaylistFile> = vec![&mut pl];
        file.finish_scan(&mut relevant);
        let d1 = file.stream_diagnostics.get(&0x1011).unwrap().first().unwrap();
        let d2 = file.stream_diagnostics.get(&0x1012).unwrap().first().unwrap();
        // Both windows close at the 1s marker with the first PID's 0.5s
        // interval; a max that ignored, inverted or re-took the comparison
        // would shift the marker to 0 or the second interval to 2/3.
        assert_eq!(d1.marker.to_bits(), 1.0_f64.to_bits());
        assert_eq!(d1.interval.to_bits(), 0.5_f64.to_bits());
        assert_eq!(d2.marker.to_bits(), 1.0_f64.to_bits());
        assert_eq!(d2.interval.to_bits(), 0.5_f64.to_bits());
    }

    #[test]
    fn an_initialized_mvc_clip_with_its_avc_base_finishes_the_quick_scan() {
        // Every stream initialised and the MVC dependent view has its AVC base
        // ⇒ the quick scan is finished. Killing the verdict's masked mutants
        // needs the direct call: an inverted `is_initialized` or an OR'd
        // MVC-without-AVC guard both flip this exact verdict.
        let mut file = TsStreamFile::new("00000.m2ts");
        file.streams.insert(0x1011, seeded_video(0x1011, true));
        let mut mvc = TsVideoStream::default();
        mvc.base.pid = Pid::new(0x1012);
        mvc.base.stream_type = TsStreamType::MvcVideo;
        mvc.base.is_initialized = true;
        file.streams.insert(0x1012, TsStream::Video(mvc));
        assert!(file.scan_stream(0x1011, false, &mut |_, _| {}));

        // The same MVC stream WITHOUT its AVC base must keep scanning.
        let mut lone = TsStreamFile::new("00001.m2ts");
        let mut dependent = TsVideoStream::default();
        dependent.base.pid = Pid::new(0x1012);
        dependent.base.stream_type = TsStreamType::MvcVideo;
        dependent.base.is_initialized = true;
        lone.streams.insert(0x1012, TsStream::Video(dependent));
        assert!(!lone.scan_stream(0x1012, false, &mut |_, _| {}));
    }

    #[test]
    fn audio_peak_rate_scales_inversely_with_the_pts_gap() {
        // 100-byte payloads 0.5s apart ⇒ 1600 b/s. The 1s-gap sibling test is
        // invariant under a divide→multiply flip of the interval term (×1.0);
        // the half-second gap is not (1600 vs 400).
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x81, 0x1100)])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &[0; 100])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 135_000, &[0; 100])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 180_000, &[0; 100])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert_eq!(file.stream_states.get(&0x1100).unwrap().peak_transfer_rate, 1600);
    }

    #[test]
    fn clip_packet_seconds_run_from_the_clip_time_in() {
        // A window closing at the 1s marker with a 0.5s interval against a clip
        // starting at 0.5s: packet-seconds = (1.0 + 0.5) − 0.5 = 1.0 — the
        // subtraction direction is observable only with a non-zero time-in.
        let mut file = TsStreamFile::new("00000.m2ts");
        file.streams.insert(0x1011, seeded_video(0x1011, false));
        let state = file.stream_states.entry(0x1011).or_default();
        state.window_packets = 1;
        state.window_bytes = 100;
        let mut pl = video_playlist(0x1011);
        pl.stream_clips = vec![window_clip(0.5, 1000.5)];
        let mut relevant: Vec<&mut TsPlaylistFile> = vec![&mut pl];
        file.update_stream_bitrate(0x1011, 0x1011, 90_000, 45_000, &mut relevant);
        assert_eq!(pl.stream_clips.first().unwrap().packet_seconds.to_bits(), 1.0_f64.to_bits());
    }

    #[test]
    fn an_angle_index_past_the_angle_maps_updates_the_main_streams() {
        // angle_index == angle_count + 1 is OUT of the angle maps (their valid
        // indices are 1..=angle_count): the window must land on the main
        // streams; an inclusive bound would look up a missing angle map and
        // drop the update entirely.
        let mut file = TsStreamFile::new("00000.m2ts");
        file.streams.insert(0x1011, seeded_video(0x1011, false));
        let state = file.stream_states.entry(0x1011).or_default();
        state.window_packets = 1;
        state.window_bytes = 100;
        let mut pl = video_playlist(0x1011);
        pl.angle_streams = vec![BTreeMap::new()];
        pl.stream_clips.first_mut().unwrap().angle_index = 2;
        let mut relevant: Vec<&mut TsPlaylistFile> = vec![&mut pl];
        file.update_stream_bitrate(0x1011, 0x1011, 90_000, 45_000, &mut relevant);
        assert_eq!(pl.streams.get(&0x1011).unwrap().base().payload_bytes, 100);
    }

    #[test]
    fn vbr_bitrates_are_left_alone_without_packet_seconds() {
        // No clips ⇒ zero accumulated packet-seconds ⇒ the VBR pass must not
        // touch the preset bitrate (an inclusive zero bound would divide the
        // accumulated payload by 0.0 and saturate the rate).
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pl = video_playlist(0x1011);
        pl.stream_clips.clear();
        let stream = pl.streams.get_mut(&0x1011).unwrap();
        stream.base_mut().payload_bytes = 100;
        stream.base_mut().bit_rate = 1234;
        let mut relevant: Vec<&mut TsPlaylistFile> = vec![&mut pl];
        file.update_stream_bitrates(0x1011, 90_000, 45_000, &mut relevant);
        assert_eq!(pl.streams.get(&0x1011).unwrap().base().bit_rate, 1234);
    }

    #[test]
    fn a_bounded_pes_spanning_packets_and_chunks_assembles_identically() {
        // A 300-byte bounded video PES spans two TS packets; scanned whole and
        // with reads that split inside both packets, the codec seam must
        // observe the IDENTICAL payload-length sequence — a chunk boundary may
        // change where the demux pauses, never what it assembles. (The
        // chunk-invariance proptest compares only byte totals, which a
        // wrongly split window selection can leave intact.)
        let data: Vec<u8> = (0_u16..300).map(|i| u8::try_from(i & 0xFF).unwrap_or(0)).collect();
        let mut bytes = pat_pmt_video();
        let pes = pes_pts(0xE0, 90_000, &data);
        bytes.extend(packet(0x1011, true, pes.get(..184).unwrap()));
        bytes.extend(packet(0x1011, false, pes.get(184..).unwrap()));
        // A trailing short PES closes the stream with a second seam.
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 180_000, &[0xEE; 8])));

        let seams = |chunk: usize| -> Vec<(u16, u64)> {
            let mut file = TsStreamFile::new("00000.m2ts");
            let mut pls = [empty_playlist()];
            let mut cur = Cursor::new(bytes.clone());
            let mut seen = Vec::new();
            file.scan_chunked(&mut cur, &mut pls, true, chunk, &mut |pid, buffer| {
                seen.push((pid, buffer.length()));
            })
            .expect("scan");
            seen
        };
        let whole = seams(5_242_880);
        // The first seam carries the full 300-byte payload: nothing lost or
        // duplicated across the packet split.
        assert_eq!(whole, vec![(0x1011, 300), (0x1011, 8)]);
        for chunk in [100_usize, 191, 192] {
            assert_eq!(seams(chunk), whole, "chunk {chunk}");
        }
    }

    #[test]
    fn a_variable_pes_spanning_packets_and_chunks_assembles_identically() {
        // The unbounded (PES_packet_length = 0) twin: the payload runs to the
        // next unit start, so the demux consumes the continuation packet's
        // stuffing too. The seam sequence must again be chunk-invariant, and
        // the trailing flush must carry everything the spec makes it carry.
        let data: Vec<u8> = (0_u16..300).map(|i| u8::try_from(i & 0xFF).unwrap_or(0)).collect();
        let mut bytes = pat_pmt_video();
        let pes = pes_variable(0xE0, 90_000, &data);
        bytes.extend(packet(0x1011, true, pes.get(..184).unwrap()));
        bytes.extend(packet(0x1011, false, pes.get(184..).unwrap()));
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 180_000, &[0xEE; 8])));

        let seams = |chunk: usize| -> Vec<(u16, u64)> {
            let mut file = TsStreamFile::new("00000.m2ts");
            let mut pls = [empty_playlist()];
            let mut cur = Cursor::new(bytes.clone());
            let mut seen = Vec::new();
            file.scan_chunked(&mut cur, &mut pls, true, chunk, &mut |pid, buffer| {
                seen.push((pid, buffer.length()));
            })
            .expect("scan");
            seen
        };
        let whole = seams(5_242_880);
        // 300 payload bytes + the continuation packet's 54 bytes of 0xFF
        // padding (the unbounded length makes padding indistinguishable from
        // payload), flushed by the next unit start. The trailing PES itself
        // never seams: the variable-length flag persists until an
        // adaptation-field packet boundary, so its bounded length is ignored —
        // the classic demux behaviour, pinned as-is.
        assert_eq!(whole, vec![(0x1011, 354)]);
        for chunk in [100_usize, 191, 192] {
            assert_eq!(seams(chunk), whole, "chunk {chunk}");
        }
    }

    /// Scans PAT + PMT(`stream_type`@`pid`) + one PES built by `pes`, returning
    /// the demuxer and the codec-seam `(pid, payload-length)` sequence.
    fn scan_one_pes(
        stream_type: u8,
        pid: u16,
        pes: &[u8],
        full: bool,
    ) -> (TsStreamFile, Vec<(u16, u64)>) {
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(stream_type, pid)])));
        bytes.extend(packet(pid, true, pes));
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let mut cur = Cursor::new(bytes);
        let mut seams = Vec::new();
        file.scan_chunked(&mut cur, &mut pls, full, 5_242_880, &mut |p, buffer| {
            seams.push((p, buffer.length()));
        })
        .expect("scan");
        (file, seams)
    }

    #[test]
    fn every_pes_stream_id_alternative_is_recognised() {
        // The PES start-code dispatch is an OR-chain per stream kind; each
        // alternative needs its own exact transfer so no single alternative
        // can be AND-folded away. (0x1B video / 0x81 audio / 0x92 subtitle.)
        let data = [0xAB_u8; 10];
        for (stream_type, ids) in [
            (0x1B_u8, &[0xFD_u8, 0xE0][..]),
            (0x81, &[0xBD, 0xC0, 0xFA, 0xFD][..]),
            (0x92, &[0xFA, 0xFD, 0xBD, 0xE0][..]),
        ] {
            for &id in ids {
                let (file, _) =
                    scan_one_pes(stream_type, 0x1234, &pes_pts(id, 90_000, &data), true);
                let st = file.stream_states.get(&0x1234).unwrap();
                assert_eq!(st.total_bytes, 10, "type {stream_type:#X} id {id:#X}");
                assert_eq!(st.transfer_count, 1, "type {stream_type:#X} id {id:#X}");
            }
        }
    }

    #[test]
    fn scrambled_or_unregistered_packets_transfer_nothing() {
        // A scrambled packet on a registered PID: counted, never transferred.
        let mut scrambled = packet(0x1011, true, &pes_pts(0xE0, 90_000, &[0xAB; 10]));
        *scrambled.get_mut(7).unwrap() |= 0x80; // transport_scrambling_control ≠ 0
        let mut bytes = pat_pmt_video();
        bytes.extend(scrambled);
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        let st = file.stream_states.get(&0x1011).unwrap();
        assert_eq!(st.total_packets, 1);
        assert_eq!(st.total_bytes, 0);
        assert_eq!(st.transfer_count, 0);

        // A clean PES on an UNREGISTERED PID: also counted, never transferred.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1300, true, &pes_pts(0xE0, 90_000, &[0xAB; 10])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        let st = file.stream_states.get(&0x1300).unwrap();
        assert_eq!(st.total_packets, 1);
        assert_eq!(st.total_bytes, 0);
        assert_eq!(st.transfer_count, 0);
    }

    #[test]
    fn adaptation_bytes_never_reach_the_pes_hunt() {
        // The adaptation field carries a byte string that LOOKS like a PES
        // start (00 00 01 E0). The demux must consume it as adaptation data —
        // a state machine that lets it into the start-code hunt frames a
        // phantom PES and ruins the real one's seam.
        let mut payload = vec![4_u8, 0x00, 0x00, 0x01, 0xE0]; // AF length 4 + fake start
        payload.extend(pes_pts(0xE0, 90_000, &[0xAB; 10]));
        let mut bytes = pat_pmt_video();
        bytes.extend(packet_raw(0x1011, true, 0x3, &payload));
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let mut cur = Cursor::new(bytes);
        let mut seams = Vec::new();
        file.scan_chunked(&mut cur, &mut pls, true, 5_242_880, &mut |p, buffer| {
            seams.push((p, buffer.length()));
        })
        .expect("scan");
        assert_eq!(seams, vec![(0x1011, 10)]);
        assert_eq!(file.stream_states.get(&0x1011).unwrap().total_bytes, 10);

        // An adaptation-ONLY packet (AFC = 10) whose field hides the same fake
        // start, followed by a real PES packet: the real PES must still frame.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet_raw(0x1011, false, 0x2, &[183, 0x00, 0x00, 0x01, 0xE0]));
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 90_000, &[0xCD; 10])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert_eq!(file.stream_states.get(&0x1011).unwrap().total_bytes, 10);
    }

    #[test]
    fn audio_peak_rate_ignores_the_first_pes_interval() {
        // The first PES has no predecessor, so its seam must report rate 0 —
        // a guard that admits the zero last-timestamp would credit the first
        // (largest) payload with a full interval and inflate the peak.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x81, 0x1100)])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &[0; 100])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 180_000, &[0; 25])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 270_000, &[0; 25])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert_eq!(file.stream_states.get(&0x1100).unwrap().peak_transfer_rate, 200);
    }

    #[test]
    fn a_pes_without_timestamps_transfers_its_exact_payload() {
        // Flags 0x00 with a zero-length optional header: the transfer starts
        // immediately and carries exactly the payload — a flags decode that
        // hallucinates a PTS+DTS would eat the payload as timestamps.
        let (file, seams) = scan_one_pes(0x1B, 0x1011, &pes_none(0xE0, &[0xAB; 10]), true);
        assert_eq!(seams, vec![(0x1011, 10)]);
        assert_eq!(file.stream_states.get(&0x1011).unwrap().total_bytes, 10);
    }

    #[test]
    fn header_stuffing_is_consumed_exactly() {
        // Seven stuffing bytes after the PTS: the transfer must start exactly
        // after the LAST one (early ⇒ stuffing pollutes the payload; a dead
        // countdown ⇒ nothing ever transfers).
        let (file, seams) =
            scan_one_pes(0x1B, 0x1011, &pes_pts_padded(0xE0, 90_000, 7, &[0xAB; 10]), true);
        assert_eq!(seams, vec![(0x1011, 10)]);
        assert_eq!(file.stream_states.get(&0x1011).unwrap().total_bytes, 10);

        // The same stuffed header with flags 0x00 (no PTS, no DTS): only the
        // stuffing countdown may consume it — a flags decode that hallucinates
        // a timestamp parse here would eat into the payload and never seam.
        let mut pes = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 20, 0x80, 0x00, 7];
        pes.resize(16, 0xFF); // 7 stuffing bytes
        pes.extend_from_slice(&[0xAB; 10]);
        let (file, seams) = scan_one_pes(0x1B, 0x1011, &pes, true);
        assert_eq!(seams, vec![(0x1011, 10)]);
        assert_eq!(file.stream_states.get(&0x1011).unwrap().total_bytes, 10);
    }

    #[test]
    fn a_quick_scan_stops_only_when_every_stream_is_initialized() {
        // Two uninitialised audio streams: the flush of the FIRST stream's
        // PES must NOT end the quick scan (its verdict is "not finished"), so
        // the second stream's payload is still demuxed — exercising both
        // flush sites (the mid-packet bounded completion and the
        // next-unit-start flush of a variable PES).
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x81, 0x1100), (0x81, 0x1101)])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &[0xAA; 10])));
        bytes.extend(packet(0x1100, true, &pes_variable(0xC0, 180_000, &[0xAB; 10])));
        bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 270_000, &[0xAC; 10])));
        bytes.extend(packet(0x1101, true, &pes_pts(0xC0, 90_000, &[0xBB; 10])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], false);
        assert_eq!(file.stream_states.get(&0x1101).map_or(0, |s| s.total_bytes), 10);
    }

    #[test]
    fn a_zero_chunk_size_still_scans() {
        // The read size is clamped to at least one byte — a zero request must
        // degrade to byte-at-a-time reads, not to an empty no-op scan.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 90_000, &[0xAB; 10])));
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let mut cur = Cursor::new(bytes);
        file.scan_chunked(&mut cur, &mut pls, true, 0, &mut |_, _| {}).expect("scan");
        assert_eq!(file.stream_states.get(&0x1011).unwrap().total_bytes, 10);
    }

    #[test]
    fn packet_padding_never_reawakens_a_closed_pes() {
        // After a bounded PES closes, packet padding (no PES start) must stay
        // inert. 255+ padding bytes would wrap a dead header countdown back
        // to zero and spuriously reopen the transfer — the totals must show
        // exactly the one real payload.
        let mut bytes = pat_pmt_video();
        bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 90_000, &[0xAB; 10])));
        bytes.extend(packet(0x1011, false, &[0xFF; 184]));
        bytes.extend(packet(0x1011, false, &[0xFF; 184]));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        let st = file.stream_states.get(&0x1011).unwrap();
        assert_eq!(st.total_bytes, 10);
        assert_eq!(st.transfer_count, 1);
    }

    #[test]
    fn the_pat_walk_excludes_the_crc_bytes() {
        // A PAT whose trailing CRC spells a plausible "program 1 → PID 0x555"
        // entry: the program walk must stop BEFORE the CRC — an inclusive
        // bound would adopt the bogus PMT PID and lose the real PMT.
        let mut pat = pat_payload(0x0100);
        let n = pat.len();
        pat.splice(n.wrapping_sub(4).., [0x00, 0x01, 0xE5, 0x55]);
        let mut bytes = packet(0, true, &pat);
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
    }

    /// A raw PAT section (no pointer byte): `section_length` computed from the
    /// `(program_number, pid)` entries, zeroed CRC.
    fn pat_section(number: u8, last: u8, entries: &[(u16, u16)]) -> Vec<u8> {
        let len = entries.len().wrapping_mul(4).wrapping_add(9);
        let [lhi, llo] = u16::try_from(len).unwrap().to_be_bytes();
        let mut s = vec![0x00, 0xB0 | (lhi & 0x0F), llo, 0x00, 0x01, 0xC1, number, last];
        for &(prog, pid) in entries {
            let [phi, plo] = prog.to_be_bytes();
            let [hi, lo] = pid.to_be_bytes();
            s.extend_from_slice(&[phi, plo, 0xE0 | (hi & 0x1F), lo]);
        }
        s.extend_from_slice(&[0, 0, 0, 0]); // CRC
        s
    }

    /// Splits `pointer + section` across as many PID-0 packets as it needs
    /// (PUSI on the first), then appends a PMT at `pmt_pid` carrying one AVC
    /// stream at 0x1011, and scans the lot.
    fn scan_pat(section: &[u8], pmt_pid: u16) -> TsStreamFile {
        let mut payload = vec![0_u8]; // pointer_field
        payload.extend_from_slice(section);
        let mut bytes = Vec::new();
        let mut first = true;
        for chunk in payload.chunks(184) {
            bytes.extend(packet(0, first, chunk));
            first = false;
        }
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011)])));
        scan("00000.m2ts", &bytes, &mut [empty_playlist()], true)
    }

    #[test]
    fn a_pat_behind_a_pointer_field_registers() {
        // pointer_field = 1: the section starts right after the one skipped
        // byte, so the length parse must fire exactly when the countdown
        // reaches zero — keyed on any other value it never fires at all.
        let mut payload = vec![1_u8, 0xFF];
        payload.extend(pat_section(0, 0, &[(1, 0x0100)]));
        let mut bytes = packet(0, true, &payload);
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn a_long_pat_keeps_its_high_length_bits_and_late_entries() {
        // 65 entries ⇒ section_length 0x10D: the high length nibble matters,
        // and the program-1 entry sits LAST — truncating the length to its
        // low byte never reaches it.
        let mut entries = vec![(2_u16, 0x0300_u16); 64];
        entries.push((1, 0x0100));
        let file = scan_pat(&pat_section(0, 0, &entries), 0x0100);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn the_pat_section_length_cap_is_exactly_1021() {
        // 253 entries ⇒ section_length exactly 1021: the largest legal
        // section must still parse (the cap is a rejection of larger only)…
        let mut entries = vec![(2_u16, 0x0300_u16); 252];
        entries.push((1, 0x0100));
        let file = scan_pat(&pat_section(0, 0, &entries), 0x0100);
        assert!(file.streams.contains_key(&0x1011));

        // …and 254 entries ⇒ 1025: over the cap, the whole section (and so
        // the PMT it would announce) is rejected.
        let mut entries = vec![(2_u16, 0x0300_u16); 253];
        entries.push((1, 0x0100));
        let file = scan_pat(&pat_section(0, 0, &entries), 0x0100);
        assert!(file.streams.is_empty());
    }

    #[test]
    fn the_pmt_walk_excludes_the_crc_bytes() {
        // A PMT whose trailing CRC spells a plausible AVC entry (0x1B → PID
        // 0x555): the entry walk must stop BEFORE the CRC — an inclusive
        // bound registers a phantom stream from checksum bytes.
        let mut pmt = pmt_payload(&[(0x1B, 0x1011)]);
        let n = pmt.len();
        pmt.splice(n.wrapping_sub(4).., [0x1B, 0xE5, 0x55, 0xF0]);
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
        assert!(!file.streams.contains_key(&0x0555));
    }

    #[test]
    fn a_pmt_behind_a_one_byte_pointer_registers() {
        // The PMT twin of the pointer test: pointer_field = 1 is the value
        // that separates "fire the length parse at zero" from any other
        // trigger — larger pointers hide the distinction behind the
        // countdown's chain priority.
        let mut section = vec![1_u8, 0xFF]; // pointer_field = 1 + 1 skip byte
        section.extend(pmt_section(0x02, &[], &[(0x1B, 0x1011)], None));
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &section));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
    }

    /// Splits `pointer + PMT section` across PID-0x0100 packets after a PAT,
    /// and scans the lot.
    fn scan_pmt_section(section: &[u8]) -> TsStreamFile {
        let mut payload = vec![0_u8];
        payload.extend_from_slice(section);
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        let mut first = true;
        for chunk in payload.chunks(184) {
            bytes.extend(packet(0x0100, first, chunk));
            first = false;
        }
        scan("00000.m2ts", &bytes, &mut [empty_playlist()], true)
    }

    #[test]
    fn the_pmt_section_length_cap_is_exactly_1021() {
        // 201 entries + 3 program-info bytes ⇒ section_length exactly 1021:
        // the largest legal section still parses (the late AVC entry lands)…
        let mut entries = vec![(0x90_u8, 0x1200_u16); 200];
        entries.push((0x1B, 0x1011));
        let file = scan_pmt_section(&pmt_section(0x02, &[0xAA, 0xBB, 0xCC], &entries, None));
        assert!(file.streams.contains_key(&0x1011));

        // …and 8 program-info bytes ⇒ 1026: over the cap, rejected whole.
        let mut entries = vec![(0x90_u8, 0x1200_u16); 200];
        entries.push((0x1B, 0x1011));
        let file = scan_pmt_section(&pmt_section(0x02, &[0xAA; 8], &entries, None));
        assert!(!file.streams.contains_key(&0x1011));
    }

    #[test]
    fn a_long_program_info_block_is_skipped_whole() {
        // 260 program-info bytes (0x104 — the high nibble matters): dropping
        // it to the low byte skips only 4 and reads the remaining 0xFF
        // program-info bytes as stream entries, aborting before the real one.
        let file = scan_pmt_section(&pmt_section(0x02, &[0xFF; 260], &[(0x1B, 0x1011)], None));
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn post_section_pmt_padding_stays_inert() {
        // 65535+ padding bytes on the PMT PID after a complete section: a
        // dead program-info countdown wrapped back to life would eventually
        // fire a spurious transfer and swallow the NEXT section. The later
        // PMT must still register its stream.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        for _ in 0..357 {
            bytes.extend(packet(0x0100, false, &[0xFF; 184]));
        }
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x90, 0x1200)])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
        assert!(file.streams.contains_key(&0x1200));
    }

    #[test]
    fn a_two_section_pmt_keeps_every_section() {
        // The PMT twin of the two-section PAT: section 0's AVC entry must
        // survive section 1's arrival (the assembly offset resets only at
        // section number 0).
        let mut sec0 = pmt_section(0x02, &[], &[(0x1B, 0x1011)], None);
        *sec0.get_mut(6).unwrap() = 0; // section_number 0
        *sec0.get_mut(7).unwrap() = 1; // last_section_number 1
        let mut sec1 = pmt_section(0x02, &[], &[(0x90, 0x1200)], None);
        *sec1.get_mut(6).unwrap() = 1;
        *sec1.get_mut(7).unwrap() = 1;
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        let mut payload = vec![0_u8];
        payload.extend_from_slice(&sec0);
        bytes.extend(packet(0x0100, true, &payload));
        let mut payload = vec![0_u8];
        payload.extend_from_slice(&sec1);
        bytes.extend(packet(0x0100, true, &payload));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn a_two_section_pat_keeps_every_section() {
        // Sections 0 and 1 of one PAT: section 0 resets the assembly offset,
        // section 1 appends, and the walk at the last section still sees
        // section 0's program-1 entry. A reset keyed on the wrong section
        // number drops section 0 entirely.
        let mut bytes = Vec::new();
        let mut payload = vec![0_u8];
        payload.extend(pat_section(0, 1, &[(1, 0x0100)]));
        bytes.extend(packet(0, true, &payload));
        let mut payload = vec![0_u8];
        payload.extend(pat_section(1, 1, &[(2, 0x0300)]));
        bytes.extend(packet(0, true, &payload));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011)])));
        let file = scan("00000.m2ts", &bytes, &mut [empty_playlist()], true);
        assert!(file.streams.contains_key(&0x1011));
    }

    #[test]
    fn an_initialized_graphics_stream_keeps_buffering() {
        // Graphics are initialised at registration on a quick scan, but the
        // demux's per-PID init mirror only refreshes at the first codec seam —
        // so it is the SECOND PES that proves an initialised graphics stream
        // still buffers its payload (unlike the count-only initialised-audio
        // path). An uninitialised audio stream rides along so the quick scan
        // does not finish at the first graphics seam.
        let mut bytes = packet(0, true, &pat_payload(0x0100));
        bytes.extend(packet(0x0100, true, &pmt_payload(&[(0x90, 0x1200), (0x81, 0x1100)])));
        bytes.extend(packet(0x1200, true, &pes_pts(0xBD, 90_000, &[0xAB; 10])));
        bytes.extend(packet(0x1200, true, &pes_pts(0xBD, 180_000, &[0xCD; 12])));
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut pls = [empty_playlist()];
        let mut cur = Cursor::new(bytes);
        let mut seams = Vec::new();
        file.scan_chunked(&mut cur, &mut pls, false, 5_242_880, &mut |p, buffer| {
            seams.push((p, buffer.length()));
        })
        .expect("scan");
        assert!(file.streams.get(&0x1200).unwrap().base().is_initialized);
        assert_eq!(seams, vec![(0x1200, 10), (0x1200, 12)]);
    }

    // ── SSIF interleaved 3D reading ──────────────────────────────────────────

    /// A synthetic `.ssif`: PAT, a PMT announcing the 3D pair (AVC base-view at
    /// 0x1011, MVC dependent-view at 0x1012), then base/dependent video PES
    /// **interleaved** extent-by-extent — the 192-byte BDAV packet layout a 3D disc
    /// stores. Read byte-sequentially, the demux must de-interleave both views.
    fn interleaved_ssif() -> Vec<u8> {
        let pmt_pid = 0x0100;
        let mut bytes = packet(0, true, &pat_payload(pmt_pid));
        bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011), (0x20, 0x1012)])));
        for n in 1..=3_u64 {
            let pts = 90_000_u64.wrapping_mul(n);
            // base-view extent (AVC, PID 0x1011), then dependent-view extent
            // (MVC, PID 0x1012) — alternating, as the interleaved units are stored.
            bytes.extend(packet(0x1011, true, &pes_dts(0xE0, pts, pts, &[0xAA; 60])));
            bytes.extend(packet(0x1012, true, &pes_dts(0xE0, pts, pts, &[0xBB; 40])));
        }
        bytes
    }

    #[test]
    fn de_interleaves_base_and_dependent_views_from_synthetic_ssif() {
        // The whole `.ssif` runs through the same scan; the interleaved base and
        // dependent packets must register as TWO streams on their own PIDs.
        let bytes = interleaved_ssif();
        let file = scan("00000.ssif", &bytes, &mut [empty_playlist()], true);

        assert_eq!(
            file.streams.get(&0x1011).map(TsStream::stream_type),
            Some(TsStreamType::AvcVideo),
            "base-view AVC registered"
        );
        assert_eq!(
            file.streams.get(&0x1012).map(TsStream::stream_type),
            Some(TsStreamType::MvcVideo),
            "dependent-view MVC registered"
        );
        // Both views' payloads were demuxed onto their own PID (de-interleaved).
        let base = file.stream_states.get(&0x1011).unwrap();
        let dependent = file.stream_states.get(&0x1012).unwrap();
        assert!(base.total_bytes > 0, "base-view payload accumulated");
        assert!(dependent.total_bytes > 0, "dependent-view payload accumulated");
        // The base view carries 60-byte ES payloads, the dependent 40-byte — proof
        // the two interleaved extents were kept apart, not merged.
        assert_eq!(base.peak_transfer_length, 60);
        assert_eq!(dependent.peak_transfer_length, 40);
    }

    #[test]
    fn display_name_prefers_the_interleaved_name_when_ssif_enabled() {
        // No interleaved file → always the m2ts name (both settings).
        let mut file = TsStreamFile::new("00000.m2ts");
        assert_eq!(file.display_name(true), "00000.M2TS");
        assert_eq!(file.display_name(false), "00000.M2TS");
        // With an interleaved file → the .ssif name only when SSIF is enabled.
        file.interleaved_file =
            Some(TsInterleavedFile::new(Box::new(MemBdFile::new("00000.ssif", Vec::new(), false))));
        assert_eq!(file.display_name(true), "00000.SSIF");
        assert_eq!(file.display_name(false), "00000.M2TS");
    }

    proptest! {
        #[test]
        fn chunk_size_does_not_change_the_result(chunk in 64_usize..=400) {
            // The state machine is chunk-boundary agnostic: a tiny read size must
            // yield the same registered streams/length as one big read.
            let pmt_pid = 0x0100;
            let mut bytes = packet(0, true, &pat_payload(pmt_pid));
            bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011), (0x81, 0x1100)])));
            bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[1; 80])));
            bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[2; 80])));
            bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &[3; 40])));

            let mut big_pls = [video_playlist(0x1011)];
            let big = scan("00000.m2ts", &bytes, &mut big_pls, true);
            let mut small_pls = [video_playlist(0x1011)];
            let mut small = TsStreamFile::new("00000.m2ts");
            let mut cur = Cursor::new(bytes.clone());
            small.scan_chunked(&mut cur, &mut small_pls, true, chunk, &mut |_, _| {}).unwrap();

            prop_assert_eq!(big.size, small.size);
            prop_assert_eq!(big.length.to_bits(), small.length.to_bits());
            let big_pids: Vec<u16> = big.streams.keys().copied().collect();
            let small_pids: Vec<u16> = small.streams.keys().copied().collect();
            prop_assert_eq!(big_pids, small_pids);
            prop_assert_eq!(
                big.streams.get(&0x1011).unwrap().base().payload_bytes,
                small.streams.get(&0x1011).unwrap().base().payload_bytes
            );
            prop_assert_eq!(
                big_pls[0].streams.get(&0x1011).unwrap().base().bit_rate,
                small_pls[0].streams.get(&0x1011).unwrap().base().bit_rate
            );
        }

        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>(), full in any::<bool>()) {
            let mut file = TsStreamFile::new("fuzz.m2ts");
            let mut pls = [empty_playlist()];
            let mut cur = Cursor::new(data);
            drop(file.scan(&mut cur, &mut pls, full));
        }

        #[test]
        fn sequential_scan_never_panics_on_arbitrary_bytes(
            data in any::<Vec<u8>>(),
            full in any::<bool>(),
        ) {
            let mut file = TsStreamFile::new("fuzz.m2ts");
            let mut pls = [empty_playlist()];
            let mut cur = Cursor::new(data);
            drop(file.scan_sequential(
                &mut cur,
                &mut pls,
                full,
                DATA_SIZE,
                &AtomicBool::new(false),
                &mut no_seam,
            ));
        }

        #[test]
        fn sequential_demux_matches_the_threaded_demux(chunk in 64_usize..=400) {
            // The equivalence contract on `scan_sequential`: same chunks, same
            // order, same finish condition ⇒ byte-identical demux state,
            // whichever strategy runs.
            let pmt_pid = 0x0100;
            let mut bytes = packet(0, true, &pat_payload(pmt_pid));
            bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011), (0x81, 0x1100)])));
            bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &[1; 80])));
            bytes.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &[2; 80])));
            bytes.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &[3; 40])));

            let mut threaded_pls = [video_playlist(0x1011)];
            let mut threaded = TsStreamFile::new("00000.m2ts");
            let mut cur = Cursor::new(bytes.clone());
            threaded
                .scan_threaded(
                    &mut cur,
                    &mut threaded_pls,
                    true,
                    chunk,
                    &AtomicBool::new(false),
                    &mut no_seam,
                )
                .unwrap();
            let mut seq_pls = [video_playlist(0x1011)];
            let seq = seq_scan("00000.m2ts", &bytes, &mut seq_pls, true, chunk);

            prop_assert_eq!(threaded.size, seq.size);
            prop_assert_eq!(threaded.length.to_bits(), seq.length.to_bits());
            prop_assert_eq!(&threaded.stream_order, &seq.stream_order);
            prop_assert_eq!(&threaded.stream_diagnostics, &seq.stream_diagnostics);
            let threaded_pids: Vec<u16> = threaded.streams.keys().copied().collect();
            let seq_pids: Vec<u16> = seq.streams.keys().copied().collect();
            prop_assert_eq!(threaded_pids, seq_pids);
            prop_assert_eq!(
                threaded.streams.get(&0x1011).unwrap().base().payload_bytes,
                seq.streams.get(&0x1011).unwrap().base().payload_bytes
            );
            let threaded_state = threaded.stream_states.get(&0x1011).unwrap();
            let seq_state = seq.stream_states.get(&0x1011).unwrap();
            prop_assert_eq!(threaded_state.total_packets, seq_state.total_packets);
            prop_assert_eq!(threaded_state.total_bytes, seq_state.total_bytes);
            prop_assert_eq!(
                threaded_state.peak_transfer_length,
                seq_state.peak_transfer_length
            );
            prop_assert_eq!(
                threaded_pls[0].streams.get(&0x1011).unwrap().base().bit_rate,
                seq_pls[0].streams.get(&0x1011).unwrap().base().bit_rate
            );
        }

        #[test]
        fn observer_is_only_a_test_seam(_unused in any::<u8>()) {
            // The public scan uses a no-op observer; assert the seam type lines up.
            let mut observed = 0_usize;
            let mut obs = |_pid: u16, _buf: &TsStreamBuffer| observed = observed.wrapping_add(1);
            let pmt_pid = 0x0100;
            let mut bytes = packet(0, true, &pat_payload(pmt_pid));
            bytes.extend(packet(pmt_pid, true, &pmt_payload(&[(0x1B, 0x1011)])));
            bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 900_000, &[1, 2, 3, 4])));
            bytes.extend(packet(0x1011, true, &pes_pts(0xE0, 1_800_000, &[5, 6, 7, 8])));
            let mut file = TsStreamFile::new("00000.m2ts");
            let mut pls = [empty_playlist()];
            let mut cur = Cursor::new(bytes);
            file.scan_chunked(&mut cur, &mut pls, true, 4096, &mut obs).expect("scan");
            prop_assert!(observed > 0);
        }
    }
}
