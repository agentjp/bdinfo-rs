//! Disc-level orchestration: discovery plus the metadata scan.
//!
//! [`BdRom::open`] locates the BD directories case-insensitively through the
//! [`crate::vfs`] seam, reads the disc flags (3D/UHD/BD+/BD-Java/PSP), the
//! recursive byte [`size`](BdRom::size), the volume label and disc title, then
//! scans the clip and playlist metadata into the per-playlist
//! [`PlaylistSummary`] rows the report emits.
//!
//! No disc-level field depends on packet data — file sizes come from the
//! filesystem (`stat`), per-playlist stream counts from the clip-information
//! (`*.clpi`) files. The file parsing lives in [`super::clpi`]/[`super::mpls`];
//! this module is the disc-level glue over them. The optional M2TS packet scan
//! ([`super::m2ts`]) only enriches the per-stream codec detail.
//!
//! Input conventions:
//! * **Volume label** — for folder input the label is the disc-root directory name (a folder
//!   carries no real volume label). The genuine volume-label case is the `.iso`/UDF path, which
//!   reads the label from the UDF descriptors.
//! * **Disc-root resolution** — the input may be the disc root, the `BDMV` directory itself, or any
//!   directory inside it: [`walked_disc_root`] runs a bounded self/ancestor walk, committing only
//!   to a *scannable* `BDMV` — one holding `CLIPINF` + `PLAYLIST` — so a stray ancestor merely
//!   named `BDMV` can never break a scan the child lookup would satisfy. Child directories resolve
//!   case-insensitively (the `vfs` discovery).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};

use super::chapters::{ChapterClip, ChapterSummary, walk_chapters};
use super::clpi::TsStreamClipFile;
use super::interleaved::TsInterleavedFile;
use super::m2ts::{TsStreamFile, bytes_to_f64, round_long};
use super::mpls::TsPlaylistFile;
use super::order::{self, PlaylistFilter};
use crate::discovery::{BdFileKind, BdmvDir};
use crate::error::{BdError, ScanError, ScanStage};
use crate::index;
use crate::primitives::Pid;
use crate::stream::{TsAudioMode, TsFrameRate, TsStream, TsStreamType};
use crate::vfs::{self, BdDir, BdFile, SearchOption};

/// The XML namespace of the BDMV disc-info metadata (`bdmt_*.xml`), bound to the
/// `di:` prefix when reading the disc title.
const DISCINFO_NS: &str = "urn:BDA:bdmv;discinfo";

/// The report-level summary of one scanned playlist.
///
/// These are the fields the `playlists` diff level emits, plus the per-stream
/// [`StreamSummary`] rows the `streams` level emits.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistSummary {
    /// The upper-cased playlist file name, e.g. `00000.MPLS`.
    pub name: String,
    /// Total presentation length in seconds.
    pub total_length: f64,
    /// Sum of the clips' `*.m2ts` sizes in bytes.
    pub file_size: u64,
    /// Sum of the clips' interleaved `*.ssif` sizes in bytes.
    pub interleaved_file_size: u64,
    /// Number of chapter marks.
    pub chapter_count: usize,
    /// Number of presented streams, excluding SSIF-only rows
    /// ([]) — the count the report presents.
    pub stream_count: usize,
    /// The number of extra camera angles (0 for a single-angle playlist).
    pub angle_count: usize,
    /// Whether the playlist loops: two main (angle-0) clips replay the same
    /// clip file from the same in-time. The presentation order can filter
    /// looping playlists out (see [`super::order::PlaylistFilter`]).
    pub has_loops: bool,
    /// The presented streams, sorted by PID — the rows the `streams` diff level
    /// emits. Codec detail is only filled when the disc was opened with the
    /// packet scan (the disc/`playlists` levels are scan-free).
    pub streams: Vec<StreamSummary>,
    /// The sequenced clips with their measured tallies, in playlist order
    /// (including angle clips). One row per clip at every level; the demux
    /// tallies are zero without the packet scan.
    pub clips: Vec<ClipSummary>,
    /// Per-chapter measured video statistics, one row per chapter mark — the
    /// first video stream's per-frame diagnostics walked against the chapter
    /// boundaries (see [`walk_chapters`]). Without the packet scan the rows
    /// keep their times but all rates/sizes are `0`.
    pub chapters: Vec<ChapterSummary>,
}

impl PlaylistSummary {
    /// The playlist's packet-derived size in bytes: the sum of the main
    /// (angle-0) clips' [`ClipSummary::packet_size`] values.
    #[must_use]
    pub fn total_packet_size(&self) -> u64 {
        self.clips
            .iter()
            .filter(|clip| clip.angle_index == 0)
            .fold(0_u64, |sum, clip| sum.saturating_add(clip.packet_size()))
    }

    /// The packet-derived size in bytes over **all** clips, angle clips
    /// included.
    #[must_use]
    pub fn total_angle_packet_size(&self) -> u64 {
        self.clips.iter().fold(0_u64, |sum, clip| sum.saturating_add(clip.packet_size()))
    }

    /// The presentation length in seconds over **all** clips, angle clips
    /// included.
    #[must_use]
    pub fn total_angle_length(&self) -> f64 {
        let mut length = 0.0;
        for clip in &self.clips {
            length += clip.length;
        }
        length
    }

    /// The playlist's total bitrate in bits/s —
    /// [`total_packet_size`](Self::total_packet_size) at 8 bits per byte over
    /// `total_length`, rounded half-to-even; `0` when the playlist has no
    /// length.
    #[must_use]
    pub fn total_bit_rate(&self) -> u64 {
        rate_over(self.total_packet_size(), self.total_length)
    }

    /// The all-angles total bitrate in bits/s —
    /// [`total_angle_packet_size`](Self::total_angle_packet_size) at 8 bits
    /// per byte over [`total_angle_length`](Self::total_angle_length), rounded
    /// half-to-even; `0` when the playlist has no length.
    #[must_use]
    pub fn total_angle_bit_rate(&self) -> u64 {
        rate_over(self.total_angle_packet_size(), self.total_angle_length())
    }

    /// Whether any presented stream is hidden by this playlist (see
    /// [`StreamSummary::is_hidden`]).
    #[must_use]
    pub fn has_hidden_streams(&self) -> bool {
        self.streams.iter().any(|stream| stream.is_hidden)
    }

    /// Per-angle measurement totals, one entry per extra camera angle (angle 1
    /// first; empty for a single-angle playlist).
    ///
    /// Each angle's timeline is the main clips with the angle's own clips
    /// replacing them at the same playlist position (matched by exact start
    /// time, last write winning): `packet_size`/`length` sum the angle's own
    /// clips, `timeline_packet_size` the whole timeline.
    #[must_use]
    pub fn angle_totals(&self) -> Vec<AngleTotals> {
        let mut totals = Vec::new();
        for angle in 1..=self.angle_count {
            let angle_index = i32::try_from(angle).unwrap_or(i32::MAX);
            let mut timeline: Vec<(u64, &ClipSummary)> = Vec::new();
            for clip in &self.clips {
                if clip.angle_index == 0 || clip.angle_index == angle_index {
                    let key = clip.relative_time_in.to_bits();
                    match timeline.iter_mut().find(|(existing, _)| *existing == key) {
                        Some(slot) => slot.1 = clip,
                        None => timeline.push((key, clip)),
                    }
                }
            }
            let mut length = 0.0;
            let mut packet_size: u64 = 0;
            let mut timeline_packet_size: u64 = 0;
            for (_, clip) in &timeline {
                timeline_packet_size = timeline_packet_size.saturating_add(clip.packet_size());
                if clip.angle_index == angle_index {
                    packet_size = packet_size.saturating_add(clip.packet_size());
                    length += clip.length;
                }
            }
            totals.push(AngleTotals { length, packet_size, timeline_packet_size });
        }
        totals
    }
}

/// `size * 8 / seconds` rounded half-to-even as a bit rate, or `0` for a
/// non-positive duration.
fn rate_over(size: u64, seconds: f64) -> u64 {
    if seconds > 0.0 {
        u64::try_from(round_long(bytes_to_f64(size) * 8.0 / seconds)).unwrap_or(0)
    } else {
        0
    }
}

/// One sequenced clip's measured tallies — the per-clip slice of the packet
/// scan: what the demux attributed to this clip (packets/payload/seconds) plus
/// its stream file's whole-file per-stream tallies.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipSummary {
    /// The clip's upper-cased `*.m2ts` name, e.g. `00001.M2TS`.
    pub name: String,
    /// The display name: the interleaved `*.ssif` name when the clip has one,
    /// else [`name`](Self::name).
    pub display_name: String,
    /// Angle index; `0` is the main angle.
    pub angle_index: i32,
    /// Start time relative to the whole playlist, seconds.
    pub relative_time_in: f64,
    /// Clip duration in seconds.
    pub length: f64,
    /// Payload bytes the demux attributed to this clip.
    pub payload_bytes: u64,
    /// Transport packets the demux attributed to this clip.
    pub packet_count: u64,
    /// Demuxed clip duration in seconds.
    pub packet_seconds: f64,
    /// The stream file's whole-file presentation length in seconds (`0` when
    /// the file is absent).
    pub file_seconds: f64,
    /// The stream file's whole-file per-stream tallies, in the file's
    /// first-registration order, limited to the streams the playlist presents.
    pub streams: Vec<ClipStreamTally>,
}

impl ClipSummary {
    /// The clip's packet-derived size in bytes (`packet_count * 192`).
    #[must_use]
    pub const fn packet_size(&self) -> u64 {
        self.packet_count.wrapping_mul(192)
    }

    /// The clip's packet-derived bitrate in bits/s —
    /// [`packet_size`](Self::packet_size) at 8 bits per byte over
    /// `packet_seconds`, rounded half-to-even; `0` when nothing was demuxed.
    #[must_use]
    pub fn packet_bit_rate(&self) -> u64 {
        rate_over(self.packet_size(), self.packet_seconds)
    }
}

/// One stream's whole-file measured tallies within one clip's stream file.
///
/// Carries the demuxed payload byte and transport packet counts for that PID,
/// typed by the file's own registration (which can differ from the playlist's
/// declaration for the same PID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipStreamTally {
    /// The packet identifier (PID).
    pub pid: Pid,
    /// The stream type the file's table registered for this PID.
    pub stream_type: TsStreamType,
    /// The short codec label of the file's registered stream.
    pub codec_short_name: String,
    /// Payload bytes demuxed for this PID across the whole file.
    pub payload_bytes: u64,
    /// Transport packets seen on this PID across the whole file.
    pub packet_count: u64,
}

/// One extra camera angle's measurement totals (see
/// [`PlaylistSummary::angle_totals`]).
#[derive(Debug, Clone, PartialEq)]
pub struct AngleTotals {
    /// The summed length of the angle's own clips, seconds.
    pub length: f64,
    /// The summed packet-derived size of the angle's own clips, bytes.
    pub packet_size: u64,
    /// The packet-derived size of the angle's whole timeline (the main clips
    /// with the angle's replacements), bytes.
    pub timeline_packet_size: u64,
}

impl AngleTotals {
    /// The angle's own bitrate in bits/s — [`packet_size`](Self::packet_size)
    /// at 8 bits per byte over [`length`](Self::length), rounded half-to-even;
    /// `0` when the angle has no length.
    #[must_use]
    pub fn bit_rate(&self) -> u64 {
        rate_over(self.packet_size, self.length)
    }

    /// The angle's whole-timeline bitrate in bits/s —
    /// [`timeline_packet_size`](Self::timeline_packet_size) over the
    /// playlist's `total_length` (the timeline spans the whole playlist),
    /// rounded half-to-even; `0` for a zero-length playlist.
    #[must_use]
    pub fn timeline_bit_rate(&self, total_length: f64) -> u64 {
        rate_over(self.timeline_packet_size, total_length)
    }
}

/// One presented stream's report fields.
///
/// Carries the `streams` diff level's per-stream line group
/// (`stream.NNNNN.{type,codec,codecname,bitrate,lang,desc}`) plus
/// the columns the human-readable report renders (alternate codec name,
/// language code, audio channel detail, angle index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSummary {
    /// The packet identifier (PID), emitted zero-padded to five digits.
    pub pid: Pid,
    /// The elementary-stream type (its  is the report's  type cell).
    pub stream_type: TsStreamType,
    /// The short codec name, e.g. `AVC`, `DTS-HD MA`.
    pub codec_short_name: String,
    /// The long codec name, e.g. `MPEG-4 AVC Video`.
    pub codec_name: String,
    /// The alternate codec name some report tables print, e.g. `DD AC3`.
    pub codec_alt_name: &'static str,
    /// The stream's bitrate in bits/s, as the packet scan measured (or, for a
    /// constant-rate codec, decoded) it: variable-rate streams carry the
    /// demuxed payload over the playlist's demuxed seconds; constant-rate
    /// streams the coded nominal rate. An extra camera angle's video row
    /// carries that angle's own measured rate. `0` without the packet scan.
    pub bitrate: i64,
    /// The bitrate over the stream's active region in bits/s, as the packet
    /// scan measured it (video and `TrueHD` only). `0` without the packet
    /// scan.
    pub active_bitrate: i64,
    /// The language name, or empty when the stream has none.
    pub language_name: String,
    /// The ISO-639 language code (e.g. `eng`), or empty when the stream has
    /// none.
    pub language_code: String,
    /// The stream description — resolution/fps/HDR for video,
    /// channels/rate/depth for audio, empty for graphics/text on a quick scan.
    pub description: String,
    /// The description with the rates only the full packet scan knows folded
    /// in: a variable-rate audio stream's measured kbps (net of an embedded
    /// `TrueHD` core's rate), and the exact four-decimal HEVC luminance
    /// spelling. Equal to [`description`](Self::description) otherwise.
    pub full_description: String,
    /// The audio channel-layout string (e.g. `5.1`); empty for non-audio.
    pub channel_description: String,
    /// The audio sample rate in Hz; `0` for non-audio or unknown.
    pub sample_rate: i32,
    /// The audio bit depth; `0` for non-audio or unknown.
    pub bit_depth: i32,
    /// The audio channel count (LFE excluded); `0` for non-audio or unknown.
    pub channel_count: i32,
    /// The video pixel height (e.g. `1080`); `0` for non-video or unknown.
    pub height: i32,
    /// Which extra camera angle this row presents: `0` for the main
    /// presentation row, `1..=angle_count` for a video stream's per-angle
    /// copies (their measured rates are that angle's own).
    pub angle_index: usize,
    /// Whether the stream is hidden by the playlist: the clip carries it but
    /// the playlist's Stream-Number table does not declare it.
    pub is_hidden: bool,
    /// Whether the stream is presented only through the interleaved (`*.ssif`)
    /// dependent-view scan — the 3D MVC video the clip information omits
    /// ([] does not count these rows).
    pub ssif_only: bool,
}

/// A scanned Blu-ray disc: discovery plus the metadata scan.
///
/// Built by [`BdRom::open`]; its fields are the disc-level values the report emits
/// (`disc.*`) plus the sorted per-playlist [`PlaylistSummary`] rows.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the seven flags are independent disc properties (3D/50Hz/UHD/BD+/BD-Java/D-BOX/PSP), not a state machine"
)]
pub struct BdRom {
    /// Disc volume label — here the disc-root directory name (see the
    /// module-level note on folder input).
    pub volume_label: String,
    /// Disc title from `META/bdmt_eng.xml`; `None` when absent or the
    /// placeholder `"blu-ray"`.
    pub disc_title: Option<String>,
    /// Total disc size in bytes, excluding `*.ssif`.
    pub size: u64,
    /// Total byte size of the interleaved `*.ssif` files (the bytes
    /// [`size`](Self::size) excludes). Zero on a non-3D disc.
    pub interleaved_size: u64,
    /// 3D disc — a non-empty `STREAM/SSIF` directory.
    pub is_3d: bool,
    /// 50 Hz content — any playlist video stream at 25 or 50 fps.
    pub is_50hz: bool,
    /// 4K UHD disc — `index.bdmv` version magic `INDX0300`.
    pub is_uhd: bool,
    /// BD+ copy protection — a `BDSVM`/`SLYVM`/`ANYVM` directory.
    pub is_bd_plus: bool,
    /// BD-Java — a non-empty `BDJO` directory.
    pub is_bd_java: bool,
    /// D-BOX motion code — a `FilmIndex.xml` file in the disc root.
    pub is_dbox: bool,
    /// PSP / mobile content — a `*.mnv` file under `SNP`.
    pub is_psp: bool,
    /// Per-playlist summaries, sorted by name (ordinal).
    pub playlists: Vec<PlaylistSummary>,
}

/// The outcome of a resilient disc scan: the (possibly partial) [`BdRom`] plus
/// every per-file failure recorded along the way.
///
/// Built by [`BdRom::open_resilient`]. On healthy media `errors` is empty and
/// `bdrom` is identical to what [`BdRom::open`] returns; on damaged media each
/// unreadable/unparseable file is one [`ScanError`] and the readable rest is
/// still scanned — "scan completed with errors" rather than no scan at all.
#[derive(Debug)]
pub struct ScanReport {
    /// The scanned disc, missing whatever the recorded failures made unreadable.
    pub bdrom: BdRom,
    /// The per-file failures, in the order the scan hit them.
    pub errors: Vec<ScanError>,
}

/// One live-progress observation of the packet scan, handed to the callback
/// of [`BdRom::open_with`] as the demux pulls bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanProgress<'a> {
    /// The stream file currently being demuxed (its upper-cased name; the
    /// interleaved `*.ssif` source still reports its `*.M2TS` name).
    pub file: &'a str,
    /// Bytes demuxed so far across the reported (full) pass, never decreasing
    /// and never exceeding [`total`](Self::total).
    pub done: u64,
    /// Total bytes the reported pass will demux: every selected stream file's
    /// source, once. (The quick codec-init pass reads only file heads and is
    /// neither budgeted nor reported.)
    pub total: u64,
}

/// The live bookkeeping of one packet scan: the running byte count over the
/// full pass's total, reported through the caller's callback after every
/// demux read and at every file boundary.
struct Progress<'a> {
    /// The caller's observer; a no-op for the plain `open`s.
    callback: &'a mut dyn FnMut(ScanProgress<'_>),
    /// Bytes demuxed so far, kept within `0..=total`.
    done: u64,
    /// Total bytes the reported (full) demux pass will read over the
    /// selected files.
    total: u64,
}

impl Progress<'_> {
    /// Advances the counter by `bytes` demuxed from `file` and reports.
    fn advance(&mut self, file: &str, bytes: u64) {
        self.done = self.done.saturating_add(bytes).min(self.total);
        (self.callback)(ScanProgress { file, done: self.done, total: self.total });
    }

    /// Snaps the counter up to `target` once `file` completes (or fails), so
    /// a short, early-finishing, or failed read never skews the running
    /// percentage of the files after it.
    fn finish_file(&mut self, file: &str, target: u64) {
        self.done = self.done.max(target).min(self.total);
        (self.callback)(ScanProgress { file, done: self.done, total: self.total });
    }
}

/// A demux source that advances the scan progress as bytes are pulled
/// through it.
struct CountingReader<'a, 'b> {
    /// The wrapped source (the `*.m2ts`, or the interleaved `*.ssif`).
    inner: Box<dyn vfs::ReadSeek>,
    /// The upper-cased stream-file name reported with each advance.
    name: String,
    /// The scan's shared progress bookkeeping.
    progress: &'a mut Progress<'b>,
}

impl Read for CountingReader<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let bytes = self.inner.read(buf)?;
        self.progress.advance(&self.name, u64::try_from(bytes).unwrap_or(u64::MAX));
        Ok(bytes)
    }
}

/// The byte total the reported (full) demux pass will read: per selected
/// stream file, its interleaved `*.ssif` source when one exists, else the
/// file itself, each counted once. The quick pass reads only file heads and
/// is not budgeted (nor reported), so the display runs 0→100% at the full
/// pass's steady pace.
fn scan_total(
    stream_files: &BTreeMap<String, u64>,
    interleaved_files: &BTreeMap<String, u64>,
    scan_files: Option<&BTreeSet<String>>,
) -> u64 {
    let mut total: u64 = 0;
    for (name, size) in stream_files {
        if scan_files.is_some_and(|selected| !selected.contains(name)) {
            continue;
        }
        let stem = clip_stem(name);
        let source = interleaved_files.get(&format!("{stem}.SSIF")).copied().unwrap_or(*size);
        total = total.saturating_add(source);
    }
    total
}

/// The collect-and-continue switch threaded through one scan: strict mode
/// (`errors: None`) propagates the first failure; resilient mode records it and
/// substitutes a fallback so the scan continues over the readable rest.
struct Sink<'a> {
    /// The error list to record into, or `None` for strict (abort-first) mode.
    errors: Option<&'a mut Vec<ScanError>>,
}

impl Sink<'_> {
    /// Unwraps `result`, recording a failure against `file` at `stage` and
    /// yielding `fallback` in resilient mode, or propagating it in strict mode.
    fn absorb<T>(
        &mut self,
        stage: ScanStage,
        file: &str,
        fallback: T,
        result: Result<T, BdError>,
    ) -> Result<T, BdError> {
        match result {
            Ok(value) => Ok(value),
            Err(reason) => match self.errors.as_deref_mut() {
                Some(errors) => {
                    errors.push(ScanError { file: file.to_owned(), stage, reason });
                    Ok(fallback)
                }
                None => Err(reason),
            },
        }
    }

    /// Records a `file`/`stage`/`reason` note in resilient mode; a no-op in
    /// strict mode (which has no error list to collect into). Used for notes
    /// that don't substitute a value — a recovered-from-BACKUP primary, or a
    /// tolerated-but-malformed `index.bdmv`.
    fn record(&mut self, stage: ScanStage, file: &str, reason: BdError) {
        if let Some(errors) = self.errors.as_deref_mut() {
            errors.push(ScanError { file: file.to_owned(), stage, reason });
        }
    }

    /// Like [`absorb`](Self::absorb), with a `BDMV/BACKUP` recovery attempt
    /// between a primary failure and recording it.
    ///
    /// **Resilient mode only.** Strict `open` never reads BACKUP — it is the
    /// honest fail-fast path, so a recovery facility belongs to the resilient
    /// scan that `open_resilient` (the CLI default) runs. On a primary `Err` in
    /// resilient mode `backup` is invoked; `Some(Ok(value))` is returned with
    /// the *primary* failure still recorded against `file`/`stage`, so the
    /// report's `WARNING` block surfaces the bad primary even though the
    /// recovered data is present. A missing or also-failing backup falls
    /// through to the plain [`absorb`](Self::absorb) outcome (recorded
    /// `fallback`, or — in strict mode — propagation).
    fn absorb_with_backup<T>(
        &mut self,
        stage: ScanStage,
        file: &str,
        fallback: T,
        primary: Result<T, BdError>,
        backup: impl FnOnce() -> Option<Result<T, BdError>>,
    ) -> Result<T, BdError> {
        let reason = match primary {
            Ok(value) => return Ok(value),
            Err(reason) => reason,
        };
        if self.errors.is_some()
            && let Some(Ok(value)) = backup()
        {
            self.record(stage, file, reason);
            return Ok(value);
        }
        self.absorb(stage, file, fallback, Err(reason))
    }
}

/// Per-clip facts the reference-clip selection needs: the resolved clip-info and
/// stream-file facts, plus borrowed references to the clip's parsed clip-info
/// streams and (when the packet scan ran) its scanned stream file, so
/// the selected reference can build its presented streams without a second
/// lookup. Borrowing keeps [`select_reference`] pure and directly unit-testable.
struct ClipMeta<'a> {
    /// The clip's clip-info (`*.clpi`) streams.
    clip_streams: &'a BTreeMap<u16, TsStream>,
    /// The clip's scanned stream file, or `None` when the packet scan did
    /// not run (disc/`playlists` level) or the file was absent.
    scanned: Option<&'a TsStreamFile>,
    /// Whether any video stream in the clip is 25 or 50 fps (drives `is_50hz`).
    has_50hz_video: bool,
    /// The clip's duration as a fraction of the playlist.
    relative_length: f64,
    /// The clip's duration in seconds.
    length: f64,
    /// Whether the clip's `*.m2ts` exists on the disc.
    stream_file_present: bool,
}

impl BdRom {
    /// Opens and scans the Blu-ray disc rooted at `root`.
    ///
    /// `root` is normally the disc root (the folder that contains `BDMV/`), but
    /// pointing at the `BDMV` directory itself or any directory inside it also
    /// works: the scan walks up to the implied disc root first
    /// ([`walked_disc_root`]).
    ///
    /// When `run_packet_scan` is set, each `*.m2ts` is quick-scanned (the
    /// [`TsStreamFile::scan`] packet pass) so the per-stream codec detail fills
    /// the presented streams — what the `streams` diff level emits. The disc and
    /// `playlists` levels pass `false`; no field they emit depends on the packet
    /// scan.
    ///
    /// # Errors
    /// - [`BdError::StructureNotFound`] if `root` has no `BDMV`, or `BDMV` lacks
    ///   `CLIPINF`/`PLAYLIST` ("unable to locate BD structure").
    /// - [`BdError::MissingClipFile`] if a playlist references an absent `*.clpi`.
    /// - [`BdError::UnknownFileType`]/[`BdError::UnexpectedEof`] for malformed metadata, or
    ///   [`BdError::Io`] for a filesystem error.
    pub fn open(root: &dyn BdDir, run_packet_scan: bool) -> Result<Self, BdError> {
        Self::open_with(root, run_packet_scan, None, &mut |_| {})
    }

    /// Opens and scans the disc like [`open`](Self::open), with the packet
    /// scan narrowed to `scan_files` and observed by `progress`.
    ///
    /// `scan_files` — when `Some`, only the stream files whose upper-cased
    /// names (`00000.M2TS`) it contains are packet-scanned; the rest keep
    /// their metadata-only summaries (zero measured rates). `None` scans
    /// every stream file.
    ///
    /// `progress` is called after every demux read and at every file boundary
    /// with the running [`ScanProgress`]; it never fires without the packet
    /// scan.
    ///
    /// # Errors
    /// As [`open`](Self::open).
    pub fn open_with(
        root: &dyn BdDir,
        run_packet_scan: bool,
        scan_files: Option<&BTreeSet<String>>,
        progress: &mut dyn FnMut(ScanProgress<'_>),
    ) -> Result<Self, BdError> {
        Self::open_impl(root, run_packet_scan, scan_files, progress, &mut Sink { errors: None })
    }

    /// Opens and scans the disc like [`open`](Self::open), but **collects** per-file
    /// failures instead of aborting on the first one — the resilient scan for
    /// damaged or partially-unreadable media.
    ///
    /// A corrupt/unreadable clip-information file, playlist, or stream file (and
    /// any failed disc-level metadata read) is recorded as a [`ScanError`] and the
    /// scan continues over the readable rest; the affected playlist (or flag) is
    /// simply absent (or its default) in the returned [`BdRom`]. On healthy media
    /// the result is identical to [`open`](Self::open) with an empty error list.
    ///
    /// # Errors
    /// Only the failures with no readable rest to degrade to: locating
    /// `BDMV`/`CLIPINF`/`PLAYLIST` failed ([`BdError::StructureNotFound`], or
    /// [`BdError::Io`] if those lookups cannot enumerate).
    pub fn open_resilient(root: &dyn BdDir, run_packet_scan: bool) -> Result<ScanReport, BdError> {
        Self::open_resilient_with(root, run_packet_scan, None, &mut |_| {})
    }

    /// Opens and scans the disc like [`open_resilient`](Self::open_resilient),
    /// with the packet scan narrowed to `scan_files` and observed by
    /// `progress` (the [`open_with`](Self::open_with) extras).
    ///
    /// # Errors
    /// As [`open_resilient`](Self::open_resilient).
    pub fn open_resilient_with(
        root: &dyn BdDir,
        run_packet_scan: bool,
        scan_files: Option<&BTreeSet<String>>,
        progress: &mut dyn FnMut(ScanProgress<'_>),
    ) -> Result<ScanReport, BdError> {
        let mut errors = Vec::new();
        let bdrom = Self::open_impl(
            root,
            run_packet_scan,
            scan_files,
            progress,
            &mut Sink { errors: Some(&mut errors) },
        )?;
        Ok(ScanReport { bdrom, errors })
    }

    /// The shared scan body behind [`open`](Self::open) (strict `sink`) and
    /// [`open_resilient`](Self::open_resilient) (recording `sink`).
    fn open_impl(
        root: &dyn BdDir,
        run_packet_scan: bool,
        scan_files: Option<&BTreeSet<String>>,
        progress: &mut dyn FnMut(ScanProgress<'_>),
        sink: &mut Sink<'_>,
    ) -> Result<Self, BdError> {
        // --- locate directories ---------------------------------------------
        // Walk self+ancestors for a `BDMV` before trying the child lookup,
        // tolerating input that points at the BDMV directory itself or inside
        // it. When the walk finds a scannable BDMV ancestor, its parent becomes
        // the disc root; otherwise the input is the disc root.
        let walked = walked_disc_root(root);
        let root = walked.as_deref().unwrap_or(root);
        // Fatal in both modes: without BDMV/CLIPINF/PLAYLIST there is no readable
        // rest to degrade to.
        let bdmv = vfs::find_directory(root, BdmvDir::Bdmv)?.ok_or(BdError::StructureNotFound)?;
        let clipinf = vfs::find_directory(&*bdmv, BdmvDir::ClipInf)?;
        let playlist = vfs::find_directory(&*bdmv, BdmvDir::Playlist)?;
        let stream = vfs::find_directory(&*bdmv, BdmvDir::Stream)?;
        let ssif = match &stream {
            Some(s) => vfs::find_directory(&**s, BdmvDir::Ssif)?,
            None => None,
        };
        let meta = vfs::find_directory(&*bdmv, BdmvDir::Meta)?;
        let bdjo = vfs::find_directory(&*bdmv, BdmvDir::BdJo)?;
        let snp = vfs::find_directory(root, BdmvDir::Snp)?;

        let (Some(clipinf), Some(playlist)) = (clipinf, playlist) else {
            return Err(BdError::StructureNotFound);
        };

        // The `BDMV/BACKUP` recovery pools, consulted only when a primary read
        // fails. Strict `open` never recovers, so it skips the probe entirely
        // and the pools stay empty; on a healthy disc the primaries succeed and
        // the (resilient-built) pools are never consulted.
        let (backup_index, backup_playlist, backup_clipinf) = discover_backups(&*bdmv, sink);

        // --- disc-level flags + metadata (each isolated in resilient mode) --
        let root_name = root.name().to_owned();
        let volume_label = root_name.clone();
        let (size, interleaved_size) =
            sink.absorb(ScanStage::Discovery, &root_name, (0, 0), directory_size(root))?;
        let is_uhd = read_uhd_flag(&*bdmv, &backup_index, sink)?;
        let is_bd_plus =
            sink.absorb(ScanStage::Discovery, &root_name, false, has_bd_plus_dir(root))?;
        let is_bd_java = match &bdjo {
            Some(d) => sink.absorb(ScanStage::Discovery, d.name(), false, has_any_file(&**d))?,
            None => false,
        };
        let is_psp = match &snp {
            Some(s) => sink.absorb(ScanStage::Discovery, s.name(), false, has_mnv_file(&**s))?,
            None => false,
        };
        let is_dbox = sink.absorb(ScanStage::Discovery, &root_name, false, has_film_index(root))?;
        let is_3d = match &ssif {
            Some(s) => sink.absorb(ScanStage::Discovery, s.name(), false, has_any_file(&**s))?,
            None => false,
        };
        let disc_title = match &meta {
            Some(m) => {
                sink.absorb(ScanStage::Discovery, m.name(), None, read_disc_title_from(&**m))?
            }
            None => None,
        };

        // --- build the file maps (sizes keyed by upper-cased name) -----------
        let (stream_files, interleaved_files, clip_files) =
            build_file_maps(stream.as_deref(), ssif.as_deref(), &*clipinf, &backup_clipinf, sink)?;

        // --- parse playlists, sorted by name (ordinal) ---------------------
        let mut parsed = parse_playlists(&*playlist, &backup_playlist, sink)?;

        // --- the per-stream-file packet scans (streams level only) ---------
        let (scanned, measured) = if run_packet_scan {
            run_measurement_scan(
                stream.as_deref(),
                ssif.as_deref(),
                &mut parsed,
                &clip_files,
                &stream_files,
                &interleaved_files,
                scan_files,
                progress,
                sink,
            )?
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };

        // --- summarise each playlist (+ build its presented streams) -------
        let mut playlists = Vec::new();
        let mut is_50hz = false;
        for playlist in &parsed {
            let summary = build_summary(
                playlist,
                &clip_files,
                &stream_files,
                &interleaved_files,
                &scanned,
                &measured,
            );
            if let Some((summary, hz)) =
                sink.absorb(ScanStage::Playlist, &playlist.name, None, summary.map(Some))?
            {
                is_50hz = is_50hz || hz;
                playlists.push(summary);
            }
        }

        Ok(Self {
            volume_label,
            disc_title,
            size,
            interleaved_size,
            is_3d,
            is_50hz,
            is_uhd,
            is_bd_plus,
            is_bd_java,
            is_dbox,
            is_psp,
            playlists,
        })
    }
}

impl BdRom {
    /// The disc's whole-tree byte size: [`size`](Self::size) plus the
    /// interleaved `*.ssif` bytes it excludes.
    #[must_use]
    pub const fn full_size(&self) -> u64 {
        self.size.saturating_add(self.interleaved_size)
    }

    /// The playlists' presentation order under `filter` — indices into
    /// [`playlists`](Self::playlists), grouped by shared clip files and
    /// sorted longest-first (see [`order::presentation_order`]). The default
    /// filter drops short and looping playlists;
    /// [`PlaylistFilter::everything`] keeps them.
    #[must_use]
    pub fn presentation_order(&self, filter: &PlaylistFilter) -> Vec<usize> {
        order::presentation_order(&self.playlists, filter)
    }

    /// The disc's extra-feature labels, in presentation order: `Ultra HD`,
    /// `BD-Java`, `50Hz Content`, `Blu-ray 3D`, `D-BOX Motion Code`,
    /// `PSP Digital Copy` — one entry per set flag.
    #[must_use]
    pub fn extra_features(&self) -> Vec<&'static str> {
        let flags = [
            (self.is_uhd, "Ultra HD"),
            (self.is_bd_java, "BD-Java"),
            (self.is_50hz, "50Hz Content"),
            (self.is_3d, "Blu-ray 3D"),
            (self.is_dbox, "D-BOX Motion Code"),
            (self.is_psp, "PSP Digital Copy"),
        ];
        flags.into_iter().filter(|&(set, _)| set).map(|(_, label)| label).collect()
    }
}

/// Upper bound on the [`walked_disc_root`] ancestor walk — far deeper than any
/// real disc path, so a pathological [`BdDir::parent`] chain (or a cycle) can
/// never spin the scan.
const MAX_BDMV_ANCESTORS: usize = 64;

/// Whether `bdmv` is a scannable `BDMV` directory — it holds both the `CLIPINF`
/// and `PLAYLIST` children the scan requires (the same condition the main lookup
/// enforces). A lookup failure counts as "no": the probe is speculative, so an
/// unreadable candidate simply leaves the input treated as the disc root.
fn is_scannable_bdmv(bdmv: &dyn BdDir) -> bool {
    let has = |which| matches!(vfs::find_directory(bdmv, which), Ok(Some(_)));
    has(BdmvDir::ClipInf) && has(BdmvDir::Playlist)
}

/// The upward pass of the disc-root resolution: when `input` — or one of its
/// ancestors, nearest first — is a scannable `BDMV` directory, returns that
/// directory's parent, the implied disc root. Returns `None` for the common
/// disc-root input (no `BDMV` self/ancestor), when the found `BDMV` has no
/// parent, or once the walk exhausts [`MAX_BDMV_ANCESTORS`] levels.
///
/// A candidate is committed to only if it is *scannable*
/// ([`is_scannable_bdmv`]), so a stray ancestor that merely happens to be named
/// `BDMV` can never break a scan that treating the input as the disc root would
/// satisfy — e.g. a disc root itself named `BDMV` still scans. Name matching is
/// ASCII case-insensitive.
fn walked_disc_root(input: &dyn BdDir) -> Option<Box<dyn BdDir>> {
    if BdmvDir::from_name(input.name()) == Some(BdmvDir::Bdmv) && is_scannable_bdmv(input) {
        return input.parent();
    }
    let mut dir = input.parent()?;
    for _ in 0..MAX_BDMV_ANCESTORS {
        if BdmvDir::from_name(dir.name()) == Some(BdmvDir::Bdmv) && is_scannable_bdmv(&*dir) {
            return dir.parent();
        }
        dir = dir.parent()?;
    }
    None
}

/// Reads the bytes of `BDMV/index.bdmv` (case-insensitive), or `Ok(None)` when
/// the directory holds no such file. The IO failures of enumerating `bdmv` or
/// reading the file surface as `Err`.
fn read_index_bytes(bdmv: &dyn BdDir) -> Result<Option<Vec<u8>>, BdError> {
    for file in bdmv.get_files()? {
        if file.name().eq_ignore_ascii_case("index.bdmv") {
            return Ok(Some(read_file(&*file)?));
        }
    }
    Ok(None)
}

/// Resolves the disc's UHD flag from `index.bdmv`, with the resilient-path
/// extras: a failed *primary* read falls back to `BDMV/BACKUP/index.bdmv`
/// (recording the primary failure), and a present-but-untagged index — one
/// lacking the `INDX` magic — is surfaced as a `ScanReport` warning while still
/// being tolerated as non-UHD (a garbage index just means "not UHD", but a
/// corrupted one should not silently read as an SDR disc).
///
/// A *missing* `index.bdmv` (no file to open) is non-UHD with no warning and no
/// backup attempt — the same silent-false both products give "the disc says
/// nothing about UHD".
fn read_uhd_flag(
    bdmv: &dyn BdDir,
    backup_index: &BackupFiles,
    sink: &mut Sink<'_>,
) -> Result<bool, BdError> {
    let bytes = sink.absorb_with_backup(
        ScanStage::Discovery,
        "index.bdmv",
        None,
        read_index_bytes(bdmv),
        || recover_backup(backup_index, "index.bdmv", |_, bytes| Ok(Some(bytes.to_vec()))),
    )?;
    let Some(bytes) = bytes else {
        return Ok(false);
    };
    if !index::has_index_tag(&bytes) {
        sink.record(
            ScanStage::Discovery,
            "index.bdmv",
            BdError::UnknownFileType(index::read_index_version(&bytes).unwrap_or_default()),
        );
    }
    Ok(index::is_uhd(&bytes))
}

/// Whether `root` has a `BDSVM`/`SLYVM`/`ANYVM` child directory (`is_bd_plus`).
fn has_bd_plus_dir(root: &dyn BdDir) -> Result<bool, BdError> {
    for child in root.get_directories()? {
        if matches!(child.name().to_ascii_uppercase().as_str(), "BDSVM" | "SLYVM" | "ANYVM") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `dir` contains at least one file.
fn has_any_file(dir: &dyn BdDir) -> Result<bool, BdError> {
    Ok(!dir.get_files()?.is_empty())
}

/// Whether the disc root carries a `FilmIndex.xml` file (`is_dbox` — the
/// D-BOX motion-code marker), matched case-insensitively like the rest of the
/// discovery.
fn has_film_index(root: &dyn BdDir) -> Result<bool, BdError> {
    for file in root.get_files()? {
        if file.name().eq_ignore_ascii_case("FilmIndex.xml") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `dir` contains at least one `*.mnv` file (`is_psp`).
fn has_mnv_file(dir: &dyn BdDir) -> Result<bool, BdError> {
    for file in dir.get_files()? {
        if file.name().rsplit_once('.').is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("mnv")) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Recursive byte size of `dir` as `(size, interleaved_size)`: every
/// non-`*.ssif` byte in the first component, the `*.ssif` bytes in the second.
fn directory_size(dir: &dyn BdDir) -> Result<(u64, u64), BdError> {
    let mut size: u64 = 0;
    let mut interleaved: u64 = 0;
    for file in dir.get_files()? {
        if file.extension().eq_ignore_ascii_case(".ssif") {
            interleaved = interleaved.saturating_add(file.length());
        } else {
            size = size.saturating_add(file.length());
        }
    }
    for child in dir.get_directories()? {
        let (child_size, child_interleaved) = directory_size(&*child)?;
        size = size.saturating_add(child_size);
        interleaved = interleaved.saturating_add(child_interleaved);
    }
    Ok((size, interleaved))
}

/// Finds `META/**/bdmt_eng.xml` (recursively) and parses the disc title from
/// it. Returns `None` when the file is absent.
fn read_disc_title_from(meta: &dyn BdDir) -> Result<Option<String>, BdError> {
    let files = meta.get_files_pattern_option("bdmt_eng.xml", SearchOption::AllDirectories)?;
    let Some(file) = files.first() else {
        return Ok(None);
    };
    let bytes = read_file(&**file)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(read_disc_title(&text))
}

/// Parses the disc title from a `bdmt_*.xml` document: the inner text of
/// `di:discinfo/di:title/di:name` (in the `urn:BDA:bdmv;discinfo` namespace),
/// or `None` when the element is absent, the XML is malformed, or the title is
/// the placeholder `"blu-ray"`. An empty title is preserved as `Some("")` —
/// only the `"blu-ray"` placeholder is nulled.
fn read_disc_title(xml: &str) -> Option<String> {
    // Strip a leading UTF-8 BOM; roxmltree would reject it.
    let xml = xml.strip_prefix('\u{feff}').unwrap_or(xml);
    let doc = roxmltree::Document::parse(xml).ok()?;
    let name = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name((DISCINFO_NS, "discinfo")))?
        .children()
        .find(|n| n.has_tag_name((DISCINFO_NS, "title")))?
        .children()
        .find(|n| n.has_tag_name((DISCINFO_NS, "name")))?;
    // The title is the concatenation of every descendant text node.
    let title: String =
        name.descendants().filter(roxmltree::Node::is_text).filter_map(|n| n.text()).collect();
    if title.eq_ignore_ascii_case("blu-ray") {
        return None;
    }
    Some(title)
}

/// Builds a map from upper-cased file name to byte size for the files of `kind`
/// in `dir` — how clip names resolve to their `*.m2ts`/`*.ssif` sizes.
fn file_sizes(dir: Option<&dyn BdDir>, kind: BdFileKind) -> Result<BTreeMap<String, u64>, BdError> {
    let mut map = BTreeMap::new();
    if let Some(dir) = dir {
        for file in vfs::find_files(dir, kind)? {
            map.insert(file.name().to_ascii_uppercase(), file.length());
        }
    }
    Ok(map)
}

/// Scans every `*.clpi` in `clipinf` into a map keyed by upper-cased name. In
/// strict mode a malformed clip file propagates its [`BdError`]; in resilient
/// mode it is recorded and skipped, and the playlists that reference it degrade
/// accordingly.
fn scan_clip_files(
    clipinf: &dyn BdDir,
    backup: &BackupFiles,
    sink: &mut Sink<'_>,
) -> Result<BTreeMap<String, TsStreamClipFile>, BdError> {
    let mut map = BTreeMap::new();
    let files = sink.absorb(
        ScanStage::Discovery,
        clipinf.name(),
        Vec::new(),
        vfs::find_files(clipinf, BdFileKind::ClipInfo).map_err(BdError::from),
    )?;
    for file in files {
        let name = file.name();
        let scan = read_file(&*file).and_then(|bytes| TsStreamClipFile::scan(name, &bytes));
        let recovered =
            sink.absorb_with_backup(ScanStage::ClipInfo, name, None, scan.map(Some), {
                || recover_backup(backup, name, |n, b| TsStreamClipFile::scan(n, b).map(Some))
            })?;
        if let Some(parsed) = recovered {
            map.insert(name.to_ascii_uppercase(), parsed);
        }
    }
    Ok(map)
}

/// Parses every `*.mpls` under `playlist_dir`, sorted by upper-cased name. In
/// strict mode a malformed playlist propagates its [`BdError`]; in resilient
/// mode it is recorded and skipped.
fn parse_playlists(
    playlist_dir: &dyn BdDir,
    backup: &BackupFiles,
    sink: &mut Sink<'_>,
) -> Result<Vec<TsPlaylistFile>, BdError> {
    let mut playlist_handles = sink.absorb(
        ScanStage::Discovery,
        playlist_dir.name(),
        Vec::new(),
        vfs::find_files(playlist_dir, BdFileKind::Playlist).map_err(BdError::from),
    )?;
    playlist_handles.sort_by_key(|f| f.name().to_ascii_uppercase());
    let mut parsed = Vec::new();
    for handle in &playlist_handles {
        let name = handle.name();
        let scan = read_file(&**handle).and_then(|bytes| TsPlaylistFile::scan(name, &bytes));
        let recovered =
            sink.absorb_with_backup(ScanStage::Playlist, name, None, scan.map(Some), {
                || recover_backup(backup, name, |n, b| TsPlaylistFile::scan(n, b).map(Some))
            })?;
        if let Some(playlist) = recovered {
            parsed.push(playlist);
        }
    }
    Ok(parsed)
}

/// One pass's scanned stream files, keyed by upper-cased file name.
type ScannedFiles = BTreeMap<String, TsStreamFile>;

/// The scan's discovery maps: the `*.m2ts` sizes, the `*.ssif` sizes (both
/// keyed by upper-cased name), and the parsed clip-information files.
type FileMaps = (BTreeMap<String, u64>, BTreeMap<String, u64>, BTreeMap<String, TsStreamClipFile>);

/// Builds the scan's [`FileMaps`] (each lookup isolated per the `sink` mode).
///
/// # Errors
/// Propagates the per-directory failures per the `sink` mode.
fn build_file_maps(
    stream: Option<&dyn BdDir>,
    ssif: Option<&dyn BdDir>,
    clipinf: &dyn BdDir,
    backup_clipinf: &BackupFiles,
    sink: &mut Sink<'_>,
) -> Result<FileMaps, BdError> {
    let stream_files = sink.absorb(
        ScanStage::Discovery,
        stream.map_or("STREAM", BdDir::name),
        BTreeMap::new(),
        file_sizes(stream, BdFileKind::Stream),
    )?;
    let interleaved_files = sink.absorb(
        ScanStage::Discovery,
        ssif.map_or("SSIF", BdDir::name),
        BTreeMap::new(),
        file_sizes(ssif, BdFileKind::Interleaved),
    )?;
    let clip_files = scan_clip_files(clipinf, backup_clipinf, sink)?;
    Ok((stream_files, interleaved_files, clip_files))
}

/// The two-pass measurement scan over the stream files (the `streams` level's
/// packet scan): a quick pass initialises each file's codec detail and nominal
/// rates; the playlists then present their reference clip's streams (merged
/// with that detail); the clip tallies the quick pass touched are zeroed; and
/// the full pass — every packet to EOF — attributes its measurements to the
/// clips, the presented streams, and the per-angle copies. Returns the
/// `(quick, full)` scan maps, both keyed by upper-cased file name.
///
/// A playlist whose clips cannot resolve keeps empty presented streams here;
/// the summary pass reports the failure.
///
/// # Errors
/// Propagates [`scan_stream_files`]'s failures per the `sink` mode.
#[expect(
    clippy::too_many_arguments,
    reason = "the scan orchestration threads the per-open context (dirs, maps, selection, progress, sink) one level down"
)]
fn run_measurement_scan(
    stream: Option<&dyn BdDir>,
    ssif: Option<&dyn BdDir>,
    parsed: &mut [TsPlaylistFile],
    clip_files: &BTreeMap<String, TsStreamClipFile>,
    stream_files: &BTreeMap<String, u64>,
    interleaved_files: &BTreeMap<String, u64>,
    scan_files: Option<&BTreeSet<String>>,
    callback: &mut dyn FnMut(ScanProgress<'_>),
    sink: &mut Sink<'_>,
) -> Result<(ScannedFiles, ScannedFiles), BdError> {
    // The quick pass reads an unpredictable sliver of each file (codec
    // init), so it draws no progress; the reported budget is the full pass,
    // every selected byte once — a steady 0→100%.
    let mut warmup = |_: ScanProgress<'_>| {};
    let quick_progress = &mut Progress { callback: &mut warmup, done: 0, total: 0 };
    let quick = scan_stream_files(stream, ssif, parsed, scan_files, quick_progress, sink, false)?;
    for playlist in parsed.iter_mut() {
        if let Ok(metas) = collect_clip_metas(playlist, clip_files, stream_files, &quick) {
            resolve_playlist_streams(playlist, &metas);
        }
    }
    clear_measurements(parsed);
    let progress = &mut Progress {
        callback,
        done: 0,
        total: scan_total(stream_files, interleaved_files, scan_files),
    };
    let full = scan_stream_files(stream, ssif, parsed, scan_files, progress, sink, true)?;
    // The PGS caption tallies and dimensions only exist after the full pass
    // (the quick pass never reads the graphics payloads), so the presented
    // streams re-merge the reference clip's full-scanned graphics detail —
    // the classic GUI's post-scan refresh; without it every subtitle row
    // would present an empty description.
    for playlist in parsed.iter_mut() {
        if let Ok(metas) = collect_clip_metas(playlist, clip_files, stream_files, &full)
            && let Some(reference) = select_reference(&metas)
            && let Some(file) = reference.scanned
        {
            remerge_graphics_detail(playlist, &file.streams);
        }
    }
    Ok((quick, full))
}

/// Builds the reference-clip selection candidates for `playlist` — one
/// [`ClipMeta`] per clip, angle clips included (an angle clip without clip
/// info is left out; the main clips still resolve the playlist).
///
/// # Errors
/// [`BdError::MissingClipFile`] if a main (angle-0) clip has no `*.clpi`.
fn collect_clip_metas<'a>(
    playlist: &TsPlaylistFile,
    clip_files: &'a BTreeMap<String, TsStreamClipFile>,
    stream_files: &BTreeMap<String, u64>,
    scanned: &'a BTreeMap<String, TsStreamFile>,
) -> Result<Vec<ClipMeta<'a>>, BdError> {
    let mut clips: Vec<ClipMeta<'a>> = Vec::new();
    for clip in &playlist.stream_clips {
        let stem = clip_stem(&clip.name);
        let Some(clip_file) = clip_files.get(&format!("{stem}.CLPI")) else {
            if clip.angle_index == 0 {
                return Err(BdError::MissingClipFile(format!("{stem}.CLPI")));
            }
            continue;
        };
        clips.push(ClipMeta {
            clip_streams: &clip_file.streams,
            scanned: scanned.get(&clip.name),
            has_50hz_video: clip_has_50hz_video(&clip_file.streams),
            relative_length: clip.relative_length,
            length: clip.length,
            stream_file_present: stream_files.contains_key(&clip.name),
        });
    }
    Ok(clips)
}

/// The dependent-view (MVC) video PID of an interleaved 3D source. The clip
/// info omits it, so the resolution presents it from the demuxed file.
const MVC_PID: u16 = 0x1012;

/// Resolves `playlist`'s presented streams ahead of the full packet scan:
/// reset-copies of the reference clip's clip-info streams (plus the
/// interleaved dependent-view stream when the file carries one), merged with
/// the quick-scanned codec detail, and one reset-copy of each video stream per
/// extra camera angle. The full scan then accumulates its measurements into
/// these maps. A playlist with no reference clip keeps empty maps.
fn resolve_playlist_streams(playlist: &mut TsPlaylistFile, metas: &[ClipMeta<'_>]) {
    let Some(reference) = select_reference(metas) else {
        return;
    };
    let mut streams: BTreeMap<u16, TsStream> = BTreeMap::new();
    for (pid, stream) in reference.clip_streams {
        streams.insert(*pid, stream.ts_clone());
    }
    if let Some(file) = reference.scanned {
        if file.interleaved_file.is_some()
            && !streams.contains_key(&MVC_PID)
            && let Some(mvc) = file.streams.get(&MVC_PID)
        {
            streams.insert(MVC_PID, mvc.ts_clone());
        }
        for (pid, src) in &file.streams {
            if let Some(dst) = streams.get_mut(pid) {
                merge_stream(dst, src);
            }
        }
    }

    let angle_count = usize::try_from(playlist.angle_count).unwrap_or(0);
    let mut angle_streams: Vec<BTreeMap<u16, TsStream>> = vec![BTreeMap::new(); angle_count];
    for (pid, stream) in &streams {
        if stream.base().is_video_stream() {
            for (index, angle) in angle_streams.iter_mut().enumerate() {
                let mut clone = stream.ts_clone();
                clone.base_mut().angle_index =
                    i32::try_from(index.wrapping_add(1)).unwrap_or(i32::MAX);
                angle.insert(*pid, clone);
            }
        }
    }
    playlist.streams = streams;
    playlist.angle_streams = angle_streams;
}

/// Re-merges full-scanned graphics detail (PGS caption tallies, dimensions)
/// from the reference clip's `scanned` streams into the playlist's presented
/// streams. The resolution ahead of the full pass merged the quick pass's
/// detail, but the quick pass skips the PGS payloads — only the full pass
/// counts captions. Non-graphics streams keep their quick-merged detail.
fn remerge_graphics_detail(playlist: &mut TsPlaylistFile, scanned: &BTreeMap<u16, TsStream>) {
    for (pid, src) in scanned {
        if !matches!(src, TsStream::Graphics(_)) {
            continue;
        }
        if let Some(dst) = playlist.streams.get_mut(pid) {
            merge_stream(dst, src);
        }
    }
}

/// Zeroes every clip's demux tallies across `playlists` — the reset between
/// the quick pass (which attributed partial windows) and the full measurement
/// pass.
fn clear_measurements(playlists: &mut [TsPlaylistFile]) {
    for playlist in playlists.iter_mut() {
        for clip in &mut playlist.stream_clips {
            clip.payload_bytes = 0;
            clip.packet_count = 0;
            clip.packet_seconds = 0.0;
        }
    }
}

/// Builds the per-clip measurement rows for `playlist` from the full-scan
/// `measured` files: each clip's own demux tallies plus its stream file's
/// whole-file per-stream tallies, limited to the presented streams.
fn build_clip_summaries(
    playlist: &TsPlaylistFile,
    measured: &BTreeMap<String, TsStreamFile>,
) -> Vec<ClipSummary> {
    let mut clips = Vec::new();
    for clip in &playlist.stream_clips {
        let file = measured.get(&clip.name);
        let mut streams = Vec::new();
        if let Some(file) = file {
            for pid in &file.stream_order {
                let Some(file_stream) = file.streams.get(pid) else {
                    continue;
                };
                if playlist.streams.contains_key(pid) {
                    streams.push(ClipStreamTally {
                        pid: Pid::new(*pid),
                        stream_type: file_stream.stream_type(),
                        codec_short_name: file_stream.codec_short_name().to_owned(),
                        payload_bytes: file_stream.base().payload_bytes,
                        packet_count: file_stream.base().packet_count,
                    });
                }
            }
        }
        clips.push(ClipSummary {
            name: clip.name.clone(),
            display_name: file
                .map_or_else(|| clip.name.clone(), |f| f.display_name(true).to_owned()),
            angle_index: clip.angle_index,
            relative_time_in: clip.relative_time_in,
            length: clip.length,
            payload_bytes: clip.payload_bytes,
            packet_count: clip.packet_count,
            packet_seconds: clip.packet_seconds,
            file_seconds: file.map_or(0.0, |f| f.length),
            streams,
        });
    }
    clips
}

/// Summarises one parsed `playlist` into its [`PlaylistSummary`], returning
/// `(summary, is_50hz)` where the flag is the playlist's contribution to the
/// disc `is_50hz` (its reference clip carrying a 25/50 fps video stream) — the
/// cross-clip resolution over the scanned clip files.
///
/// # Errors
/// [`BdError::MissingClipFile`] if a main (angle-0) clip has no `*.clpi`.
fn build_summary(
    playlist: &TsPlaylistFile,
    clip_files: &BTreeMap<String, TsStreamClipFile>,
    stream_files: &BTreeMap<String, u64>,
    interleaved_files: &BTreeMap<String, u64>,
    scanned: &BTreeMap<String, TsStreamFile>,
    measured: &BTreeMap<String, TsStreamFile>,
) -> Result<(PlaylistSummary, bool), BdError> {
    let metas = collect_clip_metas(playlist, clip_files, stream_files, scanned)?;

    let mut file_size: u64 = 0;
    let mut interleaved_file_size: u64 = 0;
    let mut total_length: f64 = 0.0;
    for clip in &playlist.stream_clips {
        let stem = clip_stem(&clip.name);
        let stream_file_present = stream_files.contains_key(&clip.name);
        file_size = file_size.saturating_add(stream_files.get(&clip.name).copied().unwrap_or(0));
        if stream_file_present {
            interleaved_file_size = interleaved_file_size.saturating_add(
                interleaved_files.get(&format!("{stem}.SSIF")).copied().unwrap_or(0),
            );
        }
        // Angle clips (`angle_index != 0`) replay the main clip's streams; their
        // `*.m2ts` size is folded into `file_size` (above), but `total_length`
        // sums the main (angle-0) clips.
        if clip.angle_index == 0 {
            total_length += clip.length;
        }
    }

    let angle = usize::try_from(playlist.angle_count).unwrap_or(0);
    let (streams, is_50hz) = select_reference(&metas).map_or_else(
        || (Vec::new(), false),
        |r| {
            (
                build_sorted_streams(
                    r.clip_streams,
                    r.scanned.map(|f| &f.streams),
                    playlist,
                    angle,
                ),
                r.has_50hz_video,
            )
        },
    );

    Ok((
        PlaylistSummary {
            name: playlist.name.clone(),
            total_length,
            file_size,
            interleaved_file_size,
            chapter_count: playlist.chapters.len(),
            stream_count: streams.iter().filter(|stream| !stream.ssif_only).count(),
            has_loops: playlist_has_loops(playlist),
            angle_count: angle,
            streams,
            clips: build_clip_summaries(playlist, measured),
            chapters: build_chapter_summaries(playlist, measured, total_length),
        },
        is_50hz,
    ))
}

/// Walks `playlist`'s chapter marks against the measured per-frame video
/// diagnostics: the diagnostics PID is the first (lowest-PID) presented video
/// stream, and each sequenced clip contributes its measured file's list for
/// that PID (`None` — skipped whole — when the file or the PID is absent).
fn build_chapter_summaries(
    playlist: &TsPlaylistFile,
    measured: &BTreeMap<String, TsStreamFile>,
    total_length: f64,
) -> Vec<ChapterSummary> {
    let diag_pid = playlist
        .streams
        .iter()
        .find(|(_, stream)| stream.base().is_video_stream())
        .map(|(pid, _)| *pid);
    let clips: Vec<ChapterClip<'_>> = playlist
        .stream_clips
        .iter()
        .map(|clip| ChapterClip {
            angle_index: clip.angle_index,
            time_in: clip.time_in,
            relative_time_in: clip.relative_time_in,
            diagnostics: diag_pid.and_then(|pid| {
                measured
                    .get(&clip.name)
                    .and_then(|file| file.stream_diagnostics.get(&pid))
                    .map(Vec::as_slice)
            }),
        })
        .collect();
    walk_chapters(&playlist.chapters, total_length, &clips)
}

/// Whether `playlist` loops — two of its main (angle-0) clips replay the same
/// clip file from the same in-time (compared bitwise; the times come straight
/// from the parsed marks, so equal inputs are bit-equal).
fn playlist_has_loops(playlist: &TsPlaylistFile) -> bool {
    let mut seen = BTreeSet::new();
    playlist
        .stream_clips
        .iter()
        .filter(|clip| clip.angle_index == 0)
        .any(|clip| !seen.insert((clip.name.as_str(), clip.time_in.to_bits())))
}

/// Whether any video stream in `streams` is 25 or 50 fps — one playlist's
/// contribution to the disc `is_50hz` flag.
fn clip_has_50hz_video(streams: &BTreeMap<u16, TsStream>) -> bool {
    streams.values().any(|stream| {
        matches!(stream, TsStream::Video(video)
            if matches!(video.frame_rate(), TsFrameRate::Framerate25 | TsFrameRate::Framerate50))
    })
}

/// Builds a playlist's presented streams in PID order: clone the reference
/// clip's clip-info streams, merge the scanned `*.m2ts` codec detail into them,
/// repeat each video stream `1 + angle_count` times (one angle clone per extra
/// camera angle), ordered by PID (a video stream's angle clones follow its main
/// row). Each row's measured rates come from the playlist's resolved stream of
/// the same PID — the main map for a main row, the angle's own map for an
/// angle clone — and stay `0` when the playlist is unresolved (no packet
/// scan).
fn build_sorted_streams(
    clip_streams: &BTreeMap<u16, TsStream>,
    scanned: Option<&BTreeMap<u16, TsStream>>,
    playlist: &TsPlaylistFile,
    angle_count: usize,
) -> Vec<StreamSummary> {
    let mut streams = clip_streams.clone();
    if let Some(scanned) = scanned {
        for (pid, src) in scanned {
            if let Some(dst) = streams.get_mut(pid) {
                merge_stream(dst, src);
            }
        }
    }
    // Graphics detail (the PGS caption tallies and dimensions) only exists
    // after the full pass — the quick merge above carries none — so the rows
    // take it from the resolved presented streams, which
    // [`remerge_graphics_detail`] refreshed when the full pass finished (the
    // classic GUI shows the full scan's counts).
    for (pid, dst) in &mut streams {
        if !matches!(dst, TsStream::Graphics(_)) {
            continue;
        }
        if let Some(src) = playlist.streams.get(pid) {
            merge_stream(dst, src);
        }
    }
    // The interleaved dependent view: when the clip info omits the MVC PID but
    // the resolution presented it from the demuxed file (see
    // [`resolve_playlist_streams`]), it gets a row of its own — never marked
    // hidden (only clip-info streams take the hidden check).
    let ssif_pid = if !streams.contains_key(&MVC_PID)
        && let Some(mvc) = playlist.streams.get(&MVC_PID)
    {
        streams.insert(MVC_PID, mvc.ts_clone());
        Some(MVC_PID)
    } else {
        None
    };

    let mut rows = Vec::new();
    for (pid, stream) in &streams {
        let ssif_only = ssif_pid == Some(*pid);
        // A presented clip stream the playlist's Stream-Number table never
        // declared is hidden — the clip carries it, the playlist hides it.
        let is_hidden = !ssif_only && !playlist.playlist_streams.contains_key(pid);
        let mut row = stream_summary(stream);
        row.is_hidden = is_hidden;
        row.ssif_only = ssif_only;
        set_measured_rates(&mut row, playlist.streams.get(pid));
        row.full_description = full_description(stream, &row);
        rows.push(row);
        if stream.base().is_video_stream() {
            for index in 0..angle_count {
                let mut row = stream_summary(stream);
                row.is_hidden = is_hidden;
                row.ssif_only = ssif_only;
                row.angle_index = index.saturating_add(1);
                set_measured_rates(
                    &mut row,
                    playlist.angle_streams.get(index).and_then(|angle| angle.get(pid)),
                );
                row.full_description = full_description(stream, &row);
                rows.push(row);
            }
        }
    }
    rows
}

/// Copies the measured bitrates from the playlist's resolved `stream` (when
/// the playlist is resolved) into the report `row`.
const fn set_measured_rates(row: &mut StreamSummary, stream: Option<&TsStream>) {
    if let Some(stream) = stream {
        row.bitrate = stream.base().bit_rate;
        row.active_bitrate = stream.base().active_bit_rate;
    }
}

/// Composes the `row`'s full description from `stream`: an audio stream with
/// a resolved rate is respelled with that rate patched in (the description's
/// kbps then reflects the scan), a video stream takes its exact-luminance
/// variant, and everything else keeps the plain description.
fn full_description(stream: &TsStream, row: &StreamSummary) -> String {
    match stream {
        TsStream::Audio(audio) if row.bitrate > 0 => {
            let mut patched = audio.clone();
            patched.base.bit_rate = row.bitrate;
            patched.description()
        }
        TsStream::Video(video) => video.full_description(),
        _ => row.description.clone(),
    }
}

/// Merges the scanned `src` stream's codec detail into the clip-info `dst`
/// stream. The clip-info stream keeps its language and video
/// format/rate/aspect; the scan supplies the encoding profile,
/// channel/sample/bit detail, HDR extension data, and embedded core stream. A
/// type mismatch is a no-op — the clip-info declaration wins.
fn merge_stream(dst: &mut TsStream, src: &TsStream) {
    if dst.base().stream_type != src.base().stream_type {
        return;
    }
    let bit_rate = dst.base().bit_rate.max(src.base().bit_rate);
    let is_vbr = src.base().is_vbr;
    dst.base_mut().bit_rate = bit_rate;
    dst.base_mut().is_vbr = is_vbr;
    match (dst, src) {
        (TsStream::Video(dst), TsStream::Video(src)) => {
            dst.encoding_profile.clone_from(&src.encoding_profile);
            dst.extended_data.clone_from(&src.extended_data);
        }
        (TsStream::Audio(dst), TsStream::Audio(src)) => {
            dst.channel_count = dst.channel_count.max(src.channel_count);
            dst.lfe = dst.lfe.max(src.lfe);
            dst.sample_rate = dst.sample_rate.max(src.sample_rate);
            dst.bit_depth = dst.bit_depth.max(src.bit_depth);
            dst.dial_norm = dst.dial_norm.min(src.dial_norm);
            if src.audio_mode != TsAudioMode::Unknown {
                dst.audio_mode = src.audio_mode;
            }
            dst.has_extensions = src.has_extensions;
            dst.ext_data.clone_from(&src.ext_data);
            if src.core_stream.is_some() && dst.core_stream.is_none() {
                dst.core_stream.clone_from(&src.core_stream);
            }
        }
        (TsStream::Graphics(dst), TsStream::Graphics(src)) => {
            dst.captions = src.captions;
            dst.forced_captions = src.forced_captions;
            dst.width = src.width;
            dst.height = src.height;
            dst.caption_ids.clone_from(&src.caption_ids);
        }
        // Text streams carry no scanned detail (a type mismatch returned above).
        _ => {}
    }
}

/// Reads one presented stream's report fields — the type/codec/language/
/// description values the `streams` diff level emits, plus the report-table
/// columns (alternate codec name, language code, audio channel detail).
fn stream_summary(stream: &TsStream) -> StreamSummary {
    let (codec_short_name, codec_name, description) = match stream {
        TsStream::Video(video) => (
            video.codec_short_name().to_owned(),
            video.codec_name().to_owned(),
            video.description(),
        ),
        TsStream::Audio(audio) => (
            audio.codec_short_name().to_owned(),
            audio.codec_name().to_owned(),
            audio.description(),
        ),
        TsStream::Graphics(graphics) => (
            graphics.codec_short_name().to_owned(),
            graphics.codec_name().to_owned(),
            graphics.description(),
        ),
        TsStream::Text(text) => {
            (text.codec_short_name().to_owned(), text.codec_name().to_owned(), text.description())
        }
    };
    let (channel_description, sample_rate, bit_depth, channel_count) = match stream {
        TsStream::Audio(audio) => {
            (audio.channel_description(), audio.sample_rate, audio.bit_depth, audio.channel_count)
        }
        _ => (String::new(), 0, 0, 0),
    };
    let height = match stream {
        TsStream::Video(video) => video.height,
        _ => 0,
    };
    StreamSummary {
        pid: stream.pid(),
        stream_type: stream.stream_type(),
        codec_short_name,
        codec_name,
        codec_alt_name: stream.codec_alt_name(),
        bitrate: 0,
        active_bitrate: 0,
        language_name: stream.base().language_name.clone().unwrap_or_default(),
        language_code: stream.base().language_code().unwrap_or_default().to_owned(),
        full_description: description.clone(),
        description,
        channel_description,
        sample_rate,
        bit_depth,
        channel_count,
        height,
        angle_index: 0,
        is_hidden: false,
        ssif_only: false,
    }
}

/// Scans every `*.m2ts` under `stream_dir` (the [`TsStreamFile::scan`] packet
/// pass), keyed by upper-cased name. Each file is scanned once, with the
/// playlists handed in so the demux can attribute packets to their clips. A
/// file with an interleaved 3D counterpart (`SSIF/<stem>.SSIF` under
/// `ssif_dir`) is demuxed through it, so the dependent-view streams register.
/// `is_full_scan` selects the pass: the quick pass stops once every stream's
/// codec detail is initialised; the full pass demuxes every packet to EOF —
/// the measurement pass. Used only at the `streams` level; the
/// disc/`playlists` levels skip it entirely. In strict mode a failed open/scan
/// propagates; in resilient mode it is recorded and the file skipped — its
/// playlists fall back to the clip-info detail.
fn scan_stream_files(
    stream_dir: Option<&dyn BdDir>,
    ssif_dir: Option<&dyn BdDir>,
    playlists: &mut [TsPlaylistFile],
    scan_files: Option<&BTreeSet<String>>,
    progress: &mut Progress<'_>,
    sink: &mut Sink<'_>,
    is_full_scan: bool,
) -> Result<BTreeMap<String, TsStreamFile>, BdError> {
    let mut scanned = BTreeMap::new();
    let Some(dir) = stream_dir else {
        return Ok(scanned);
    };
    let files = sink.absorb(
        ScanStage::Discovery,
        dir.name(),
        Vec::new(),
        vfs::find_files(dir, BdFileKind::Stream).map_err(BdError::from),
    )?;
    let mut ssif_files: BTreeMap<String, Box<dyn BdFile>> = BTreeMap::new();
    if let Some(dir) = ssif_dir {
        let found = sink.absorb(
            ScanStage::Discovery,
            dir.name(),
            Vec::new(),
            vfs::find_files(dir, BdFileKind::Interleaved).map_err(BdError::from),
        )?;
        for file in found {
            ssif_files.insert(file.name().to_ascii_uppercase(), file);
        }
    }
    for file in files {
        let name = file.name().to_ascii_uppercase();
        if scan_files.is_some_and(|selected| !selected.contains(&name)) {
            continue;
        }
        let stem = clip_stem(&name);
        let interleaved = ssif_files.remove(&format!("{stem}.SSIF"));
        // The expected source size — what `scan_total` budgeted for this file
        // — so the progress can snap to the file boundary either way.
        let source_size = interleaved.as_ref().map_or_else(|| file.length(), |f| f.length());
        let target = progress.done.saturating_add(source_size);
        let scan = scan_one_stream_file(&*file, interleaved, playlists, is_full_scan, progress);
        progress.finish_file(&name, target);
        if let Some(stream_file) =
            sink.absorb(ScanStage::StreamFile, file.name(), None, scan.map(Some))?
        {
            scanned.insert(name, stream_file);
        }
    }
    Ok(scanned)
}

/// Opens and scans one `*.m2ts` (the per-file unit [`scan_stream_files`]
/// isolates), demuxing through its `interleaved` 3D source when it has one.
/// The demux pulls its bytes through the scan's [`Progress`].
fn scan_one_stream_file(
    file: &dyn BdFile,
    interleaved: Option<Box<dyn BdFile>>,
    playlists: &mut [TsPlaylistFile],
    is_full_scan: bool,
    progress: &mut Progress<'_>,
) -> Result<TsStreamFile, BdError> {
    let mut stream_file = TsStreamFile::new(file.name());
    stream_file.interleaved_file = interleaved.map(TsInterleavedFile::new);
    // Select the demux source the way `scan_source` would (the interleaved
    // 3D file when present), but wrapped so each read advances the progress.
    let inner = match &stream_file.interleaved_file {
        Some(interleaved) => interleaved.open_read().map_err(BdError::Io)?,
        None => file.open_read()?,
    };
    let mut reader = CountingReader { inner, name: file.name().to_ascii_uppercase(), progress };
    stream_file.scan(&mut reader, playlists, is_full_scan)?;
    Ok(stream_file)
}

/// Picks the playlist's reference clip — the clip whose streams become the
/// presented set. Returns `None` for an empty clip list (no reference, no
/// presented streams).
fn select_reference<'a, 'b>(clips: &'b [ClipMeta<'a>]) -> Option<&'b ClipMeta<'a>> {
    let mut reference = clips.first()?;
    for clip in clips {
        // Step A: a clip whose `*.m2ts` exists beats a reference without one.
        if !reference.stream_file_present && clip.stream_file_present {
            reference = clip;
        }
        // Step B: a clip with more clip-info streams wins if it is not
        // negligibly short (`relative_length > 0.01`); otherwise a longer clip
        // with a stream file wins. Both arms assign the reference, so the two
        // conditions collapse to one short-circuiting `||` (the first is still
        // evaluated first, preserving the else-if precedence).
        if (clip.clip_streams.len() > reference.clip_streams.len() && clip.relative_length > 0.01)
            || (clip.length > reference.length && clip.stream_file_present)
        {
            reference = clip;
        }
    }
    Some(reference)
}

/// The clip stem of a stream-file/clip name — the name without its `.M2TS` or
/// `.FMTS` extension. Clips pair with their `*.clpi`/`*.ssif` siblings by this
/// stem, which the siblings share but the extension does not; an `FMTS` clip
/// (libbluray `_fill_clip`) names a `*.FMTS` stream file yet still a `*.CLPI`.
fn clip_stem(name: &str) -> &str {
    name.strip_suffix(".M2TS").or_else(|| name.strip_suffix(".FMTS")).unwrap_or(name)
}

/// The file handles of one `BDMV/BACKUP` metadata directory, keyed by
/// upper-cased name — the recovery pool a damaged primary's replacement is
/// drawn from. Empty when the disc has no such backup directory (or the scan is
/// strict, which never recovers).
type BackupFiles = BTreeMap<String, Box<dyn BdFile>>;

/// The `BDMV/BACKUP` recovery pools — `(index.bdmv, PLAYLIST, CLIPINF)` — built
/// once per open and drawn from only when a primary read fails.
///
/// **Resilient mode only.** Strict `open` never recovers from BACKUP, so it
/// skips the probe entirely (three empty pools, no extra IO, no new failure
/// points). A directory-listing failure anywhere in the probe is recorded once
/// under `BACKUP` — surfaced, not silently swallowed — and leaves the pools
/// empty; a disc with no `BDMV/BACKUP` yields empty pools with nothing
/// recorded.
fn discover_backups(
    bdmv: &dyn BdDir,
    sink: &mut Sink<'_>,
) -> (BackupFiles, BackupFiles, BackupFiles) {
    let empty = || (BackupFiles::new(), BackupFiles::new(), BackupFiles::new());
    if sink.errors.is_none() {
        return empty();
    }
    match collect_backups(bdmv) {
        Ok(pools) => pools,
        Err(reason) => {
            sink.record(ScanStage::Discovery, "BACKUP", reason);
            empty()
        }
    }
}

/// Enumerates the three `BDMV/BACKUP` recovery pools, propagating the first
/// directory-listing IO error. A disc with no `BDMV/BACKUP` (or a BACKUP
/// missing the `PLAYLIST`/`CLIPINF` subdirectory) yields the corresponding
/// empty pool.
fn collect_backups(bdmv: &dyn BdDir) -> Result<(BackupFiles, BackupFiles, BackupFiles), BdError> {
    let Some(backup) = vfs::find_directory(bdmv, BdmvDir::Backup)? else {
        return Ok((BackupFiles::new(), BackupFiles::new(), BackupFiles::new()));
    };
    let index = list_backup_files(&*backup)?;
    let playlist = backup_subdir_files(&*backup, BdmvDir::Playlist)?;
    let clipinf = backup_subdir_files(&*backup, BdmvDir::ClipInf)?;
    Ok((index, playlist, clipinf))
}

/// The [`BackupFiles`] of the `which` subdirectory of a `BDMV/BACKUP` directory,
/// or an empty pool when that subdirectory is absent.
fn backup_subdir_files(backup: &dyn BdDir, which: BdmvDir) -> Result<BackupFiles, BdError> {
    vfs::find_directory(backup, which)?
        .map_or_else(|| Ok(BackupFiles::new()), |dir| list_backup_files(&*dir))
}

/// Lists `dir`'s file handles into a [`BackupFiles`] map keyed by upper-cased
/// name, propagating the directory-listing IO error.
fn list_backup_files(dir: &dyn BdDir) -> Result<BackupFiles, BdError> {
    let mut map = BackupFiles::new();
    for file in dir.get_files()? {
        map.insert(file.name().to_ascii_uppercase(), file);
    }
    Ok(map)
}

/// Reads and parses the BACKUP counterpart of `name` from a pre-listed
/// [`BackupFiles`] pool, matching the upper-cased name. Returns `None` when the
/// pool holds no such file, and `Some(Err)` when it does but the read or parse
/// failed. The backup is handed to the *same* `parse` the primary uses, so it
/// inherits every hostile-input cap — a malicious BACKUP is trusted no more
/// than a malicious primary.
fn recover_backup<T>(
    backup: &BackupFiles,
    name: &str,
    parse: impl FnOnce(&str, &[u8]) -> Result<T, BdError>,
) -> Option<Result<T, BdError>> {
    let file = backup.get(&name.to_ascii_uppercase())?;
    Some(read_file(&**file).and_then(|bytes| parse(file.name(), &bytes)))
}

/// Reads a VFS file fully into a byte vector, mapping IO errors to [`BdError::Io`].
fn read_file(file: &dyn BdFile) -> Result<Vec<u8>, BdError> {
    let mut reader = file.open_read()?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    use proptest::prelude::{prop_assert, prop_assert_eq, proptest};

    use super::{
        BdError, BdRom, ClipMeta, ClipSummary, MVC_PID, PlaylistFilter, PlaylistSummary, Progress,
        ScanProgress, ScanStage, Sink, TsPlaylistFile, TsStreamFile, backup_subdir_files,
        build_chapter_summaries, build_clip_summaries, build_sorted_streams, clear_measurements,
        clip_has_50hz_video, clip_stem, collect_backups, merge_stream, rate_over, read_disc_title,
        read_file, resolve_playlist_streams, scan_stream_files, scan_total, select_reference,
        stream_summary, walked_disc_root,
    };
    use crate::bdrom::clpi::TsStreamClip;
    use crate::bdrom::interleaved::{MemBdFile, TsInterleavedFile};
    use crate::bdrom::m2ts::packets::{packet, pat_payload, pes_dts, pes_pts, pmt_payload};
    use crate::discovery::BdmvDir;
    use crate::primitives::Pid;
    use crate::stream::{
        TsAudioMode, TsAudioStream, TsFrameRate, TsGraphicsStream, TsStream, TsStreamType,
        TsTextStream, TsVideoFormat, TsVideoStream,
    };
    use crate::vfs::fs::{FsDir, FsFile};
    use crate::vfs::{BdDir, BdFile, ReadSeek, SearchOption};

    // ── synthetic metadata builders (minimal valid `*.clpi` / `*.mpls`) ──────

    /// A valid `*.clpi`: `HDMV0300`, `ProgramInfo` at offset 16, one program
    /// sequence, one entry per `(pid, coding_type, 4-byte payload)`.
    fn clpi(entries: &[(u16, u8, [u8; 4])]) -> Vec<u8> {
        let mut clip_data = vec![0_u8, 1]; // reserved + num_prog = 1
        clip_data.extend_from_slice(&[0_u8; 6]); // spn start + program_map_pid
        clip_data.push(u8::try_from(entries.len()).unwrap()); // stream count at 8
        clip_data.push(0); // 9: num_groups; stream entries start at 10
        for &(pid, coding_type, payload) in entries {
            clip_data.extend_from_slice(&pid.to_be_bytes());
            clip_data.push(5); // coding-info length (type + 4 payload)
            clip_data.push(coding_type);
            clip_data.extend_from_slice(&payload);
        }
        let mut buf = b"HDMV0300".to_vec();
        buf.extend_from_slice(&[0_u8; 4]); // 8..12
        buf.extend_from_slice(&16_u32.to_be_bytes()); // 12..16 ProgramInfo addr
        buf.extend_from_slice(&u32::try_from(clip_data.len()).unwrap().to_be_bytes());
        buf.extend_from_slice(&clip_data);
        buf
    }

    /// A valid single-item `*.mpls` with no extra camera angles (the common case).
    fn mpls(item_name: &str, in_t: u32, out_t: u32, chapters: &[(u8, u16, u32)]) -> Vec<u8> {
        mpls_angles(item_name, in_t, out_t, &[], chapters)
    }

    /// A valid single-item `*.mpls` like [`mpls_full`], with an empty
    /// Stream-Number table.
    fn mpls_angles(
        item_name: &str,
        in_t: u32,
        out_t: u32,
        angle_names: &[&str],
        chapters: &[(u8, u16, u32)],
    ) -> Vec<u8> {
        mpls_items(&[item_name], in_t, out_t, angle_names, &[], chapters)
    }

    /// A valid single-item `*.mpls` with no angles that declares the given
    /// audio PIDs in its Stream-Number table.
    fn mpls_declared(
        item_name: &str,
        in_t: u32,
        out_t: u32,
        declared_audio: &[u16],
        chapters: &[(u8, u16, u32)],
    ) -> Vec<u8> {
        mpls_items(&[item_name], in_t, out_t, &[], declared_audio, chapters)
    }

    /// A valid `*.mpls`: `MPLS0300`, one identical `PlayItem` per name in
    /// `item_names` with the given in/out times (45 kHz ticks), `angle_names`
    /// extra camera angles, a Stream-Number table declaring `declared_audio`
    /// as AC3 audio entries, and the given `(chapter_type, file_index, tick)`
    /// marks. Repeating a name yields a looping playlist (the same clip
    /// replayed from the same in-time).
    fn mpls_items(
        item_names: &[&str],
        in_t: u32,
        out_t: u32,
        angle_names: &[&str],
        declared_audio: &[u16],
        chapters: &[(u8, u16, u32)],
    ) -> Vec<u8> {
        mpls_items_codec(*b"M2TS", item_names, in_t, out_t, angle_names, declared_audio, chapters)
    }

    /// A single-item-per-name `*.mpls` whose play items (and angles) carry the
    /// given 4-byte `codec` id — `*b"M2TS"` for ordinary clips, `*b"FMTS"` for the
    /// `*.FMTS` variant.
    fn mpls_items_codec(
        codec: [u8; 4],
        item_names: &[&str],
        in_t: u32,
        out_t: u32,
        angle_names: &[&str],
        declared_audio: &[u16],
        chapters: &[(u8, u16, u32)],
    ) -> Vec<u8> {
        let mut items = Vec::new();
        for item_name in item_names {
            let mut body = Vec::new();
            let mut name = item_name.as_bytes().to_vec();
            name.resize(5, 0);
            body.extend_from_slice(&name); // +2..7
            body.extend_from_slice(&codec); // +7..11
            body.push(0); // +11
            body.push(if angle_names.is_empty() { 0 } else { 0x10 }); // +12 multiangle flag
            body.push(0); // +13
            body.extend_from_slice(&in_t.to_be_bytes()); // +14..18
            body.extend_from_slice(&out_t.to_be_bytes()); // +18..22
            body.extend_from_slice(&[0_u8; 12]); // +22..34
            if !angle_names.is_empty() {
                body.push(u8::try_from(angle_names.len().wrapping_add(1)).unwrap()); // angle count
                body.push(0); // reserved
                for angle in angle_names {
                    let mut angle_bytes = angle.as_bytes().to_vec();
                    angle_bytes.resize(5, 0);
                    body.extend_from_slice(&angle_bytes); // angle name (5)
                    body.extend_from_slice(&codec); // angle type (4)
                    body.push(0); // reserved (1)
                }
            }
            body.extend_from_slice(&[0_u8; 2]); // stream-info length
            body.extend_from_slice(&[0_u8; 2]); // reserved
            body.push(0); // video count
            body.push(u8::try_from(declared_audio.len()).unwrap()); // audio count
            body.extend_from_slice(&[0_u8; 5]); // remaining 5 stream counts (all zero)
            body.extend_from_slice(&[0_u8; 5]); // reserved
            for pid in declared_audio {
                body.push(3); // header length (type + PID)
                body.push(1); // header type 1: the PID follows directly
                body.extend_from_slice(&pid.to_be_bytes());
                body.push(5); // stream length (type byte + format + language)
                body.push(0x81); // AC3 audio
                body.push(0x61); // 5.1 / 48 kHz
                body.extend_from_slice(b"eng");
            }
            items.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
            items.extend_from_slice(&body);
        }

        let playlist_offset: usize = 0x3C;
        let mut playlist = Vec::new();
        playlist.extend_from_slice(&[0_u8; 4]); // PlayList length
        playlist.extend_from_slice(&[0_u8; 2]); // reserved
        playlist.extend_from_slice(&u16::try_from(item_names.len()).unwrap().to_be_bytes());
        playlist.extend_from_slice(&[0_u8; 2]); // sub-item count
        playlist.extend_from_slice(&items);
        let chapters_offset = playlist_offset.wrapping_add(playlist.len());

        let mut mark = Vec::new();
        mark.extend_from_slice(&[0_u8; 4]); // PlayListMark length
        mark.extend_from_slice(&u16::try_from(chapters.len()).unwrap().to_be_bytes());
        for &(chapter_type, file_index, tick) in chapters {
            let mut entry = vec![0_u8, chapter_type];
            entry.extend_from_slice(&file_index.to_be_bytes());
            entry.extend_from_slice(&tick.to_be_bytes());
            entry.resize(14, 0);
            mark.extend_from_slice(&entry);
        }

        let mut buf = b"MPLS0300".to_vec();
        buf.extend_from_slice(&u32::try_from(playlist_offset).unwrap().to_be_bytes());
        buf.extend_from_slice(&u32::try_from(chapters_offset).unwrap().to_be_bytes());
        buf.extend_from_slice(&[0_u8; 4]); // extensions offset
        buf.resize(0x38, 0);
        buf.push(0); // misc flags at 0x38
        buf.resize(playlist_offset, 0);
        buf.extend_from_slice(&playlist);
        buf.extend_from_slice(&mark);
        buf
    }

    /// A valid `*.mpls` with zero play items — its `stream_clips` are empty, so
    /// the reference-clip selection finds nothing.
    fn empty_mpls() -> Vec<u8> {
        let playlist_offset: usize = 0x3C;
        let mut playlist = Vec::new();
        playlist.extend_from_slice(&[0_u8; 4]); // PlayList length
        playlist.extend_from_slice(&[0_u8; 2]); // reserved
        playlist.extend_from_slice(&0_u16.to_be_bytes()); // item count = 0
        playlist.extend_from_slice(&[0_u8; 2]); // sub-item count
        let chapters_offset = playlist_offset.wrapping_add(playlist.len());

        let mut mark = Vec::new();
        mark.extend_from_slice(&[0_u8; 4]); // PlayListMark length
        mark.extend_from_slice(&0_u16.to_be_bytes()); // zero marks

        let mut buf = b"MPLS0300".to_vec();
        buf.extend_from_slice(&u32::try_from(playlist_offset).unwrap().to_be_bytes());
        buf.extend_from_slice(&u32::try_from(chapters_offset).unwrap().to_be_bytes());
        buf.extend_from_slice(&[0_u8; 4]); // extensions offset
        buf.resize(0x38, 0);
        buf.push(0); // misc flags at 0x38
        buf.resize(playlist_offset, 0);
        buf.extend_from_slice(&playlist);
        buf.extend_from_slice(&mark);
        buf
    }

    /// `bdmt_eng.xml` with the given title in the `discinfo` namespace.
    fn title_xml(name: &str) -> Vec<u8> {
        format!(
            "<disclib xmlns:di=\"urn:BDA:bdmv;discinfo\">\
             <di:discinfo><di:title><di:name>{name}</di:name></di:title></di:discinfo></disclib>"
        )
        .into_bytes()
    }

    // ── throwaway on-disk BD fixture ────────────────────────────────────────

    struct TempDisc {
        root: PathBuf,
    }

    impl TempDisc {
        /// Creates `dirs` (possibly empty) and writes `files` (paths relative to the
        /// disc root) under a unique temp directory, removed on drop.
        fn build(dirs: &[&str], files: &[(&str, Vec<u8>)]) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut root = std::env::temp_dir();
            root.push(format!("bdinfo-rs-disc-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&root).expect("create root");
            for dir in dirs {
                std::fs::create_dir_all(root.join(dir)).expect("create dir");
            }
            for (path, bytes) in files {
                let full = root.join(path);
                std::fs::create_dir_all(full.parent().expect("file has parent")).expect("parents");
                std::fs::write(&full, bytes).expect("write file");
            }
            Self { root }
        }

        fn open(&self) -> Result<BdRom, BdError> {
            BdRom::open(&FsDir::new(self.root.clone()), false)
        }

        fn open_scanned(&self) -> Result<BdRom, BdError> {
            BdRom::open(&FsDir::new(self.root.clone()), true)
        }
    }

    impl Drop for TempDisc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root).is_ok();
        }
    }

    // ── pure helpers ────────────────────────────────────────────────────────

    #[test]
    fn read_disc_title_handles_every_case() {
        // Normal title, with a leading UTF-8 BOM (the parser strips it).
        let with_bom = format!("\u{feff}{}", String::from_utf8(title_xml("BROTHER")).unwrap());
        assert_eq!(read_disc_title(&with_bom).as_deref(), Some("BROTHER"));
        // Empty title is preserved as Some("") (only "blu-ray" is nulled).
        assert_eq!(
            read_disc_title(&String::from_utf8(title_xml("")).unwrap()).as_deref(),
            Some("")
        );
        // The "blu-ray" placeholder (any case) becomes None.
        assert_eq!(read_disc_title(&String::from_utf8(title_xml("Blu-ray")).unwrap()), None);
        // Malformed XML → None.
        assert_eq!(read_disc_title("<not xml"), None);
        // Missing discinfo / title / name nodes → None.
        assert_eq!(read_disc_title("<disclib></disclib>"), None);
        assert_eq!(
            read_disc_title(
                "<disclib xmlns:di=\"urn:BDA:bdmv;discinfo\"><di:discinfo></di:discinfo></disclib>"
            ),
            None
        );
        assert_eq!(
            read_disc_title(
                "<disclib xmlns:di=\"urn:BDA:bdmv;discinfo\">\
                 <di:discinfo><di:title></di:title></di:discinfo></disclib>"
            ),
            None
        );
    }

    /// A clip-info stream map of `n` dummy streams — controls `clip_streams.len()`,
    /// the only thing [`select_reference`] reads off it.
    fn dummy_streams(n: usize) -> BTreeMap<u16, TsStream> {
        (0..n)
            .map(|i| (u16::try_from(i).unwrap(), TsStream::Audio(TsAudioStream::default())))
            .collect()
    }

    /// A playlist with no clips and empty stream maps — the unresolved stand-in
    /// for the helpers that read `streams`/`angle_streams`.
    fn bare_playlist() -> TsPlaylistFile {
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

    /// Builds a [`ClipMeta`] borrowing `streams`, varying the reference-selection
    /// fields the selection loop reads.
    fn meta(
        streams: &BTreeMap<u16, TsStream>,
        length: f64,
        relative: f64,
        has_stream_file: bool,
    ) -> ClipMeta<'_> {
        ClipMeta {
            clip_streams: streams,
            scanned: None,
            has_50hz_video: false,
            relative_length: relative,
            length,
            stream_file_present: has_stream_file,
        }
    }

    #[test]
    fn select_reference_picks_per_the_selection_rules() {
        let s2 = dummy_streams(2);
        let s5 = dummy_streams(5);

        // Empty → None; single clip → itself.
        assert!(select_reference(&[]).is_none());
        let one = [meta(&s2, 10.0, 1.0, true)];
        assert_eq!(select_reference(&one).unwrap().clip_streams.len(), 2);

        // Step A: the first clip lacks a stream file, the second has one (equal
        // counts/length, so only step A can promote it).
        let step_a = [meta(&s2, 10.0, 1.0, false), meta(&s2, 10.0, 1.0, true)];
        assert!(select_reference(&step_a).unwrap().stream_file_present);

        // Step B (if): more clip-info streams and relative_length > 0.01 wins.
        let step_b_if = [meta(&s2, 10.0, 1.0, true), meta(&s5, 10.0, 1.0, true)];
        assert_eq!(select_reference(&step_b_if).unwrap().clip_streams.len(), 5);
        // …but relative_length == 0.01 (not > 0.01) does NOT win on stream count.
        let at_threshold = [meta(&s2, 10.0, 1.0, true), meta(&s5, 10.0, 0.01, true)];
        assert_eq!(select_reference(&at_threshold).unwrap().clip_streams.len(), 2);

        // Step B (else): equal stream counts but a longer clip with a stream file.
        let step_b_else = [meta(&s2, 10.0, 1.0, true), meta(&s2, 20.0, 1.0, true)];
        assert_eq!(select_reference(&step_b_else).unwrap().length.to_bits(), 20.0_f64.to_bits());
        // …a longer clip WITHOUT a stream file does not win.
        let longer_no_file = [meta(&s2, 10.0, 1.0, true), meta(&s2, 20.0, 1.0, false)];
        assert_eq!(select_reference(&longer_no_file).unwrap().length.to_bits(), 10.0_f64.to_bits());
    }

    #[test]
    fn clip_has_50hz_video_detects_25_and_50fps() {
        // A 25 fps video (alongside audio) → 50 Hz.
        let mut at25 = BTreeMap::new();
        let mut v = TsVideoStream::default();
        v.set_frame_rate(TsFrameRate::Framerate25);
        at25.insert(0x1011, TsStream::Video(v));
        at25.insert(0x1100, TsStream::Audio(TsAudioStream::default()));
        assert!(clip_has_50hz_video(&at25));

        // 50 fps too.
        let mut at50 = BTreeMap::new();
        let mut v = TsVideoStream::default();
        v.set_frame_rate(TsFrameRate::Framerate50);
        at50.insert(0x1011, TsStream::Video(v));
        assert!(clip_has_50hz_video(&at50));

        // A 24 fps video (and audio) → not 50 Hz.
        let mut at24 = BTreeMap::new();
        let mut v = TsVideoStream::default();
        v.set_frame_rate(TsFrameRate::Framerate24);
        at24.insert(0x1011, TsStream::Video(v));
        at24.insert(0x1100, TsStream::Audio(TsAudioStream::default()));
        assert!(!clip_has_50hz_video(&at24));
    }

    /// The video stream inside `s`, or `None` for any other variant.
    fn as_video(s: &TsStream) -> Option<&TsVideoStream> {
        if let TsStream::Video(video) = s { Some(video) } else { None }
    }

    /// The audio stream inside `s`, or `None` for any other variant.
    #[test]
    fn graphics_detail_remerges_after_the_full_pass() {
        // The presented PGS stream starts with the quick pass's empty detail;
        // the re-merge copies the full scan's caption tallies and dimensions.
        let mut playlist =
            TsPlaylistFile::scan("00000.mpls", &mpls("00000", 0, 4_500_000, &[])).unwrap();
        playlist.streams = BTreeMap::from([
            (0x1200_u16, TsStream::Graphics(TsGraphicsStream::default())),
            (0x1011, TsStream::Video(TsVideoStream::default())),
        ]);
        let scanned_pgs = TsGraphicsStream {
            captions: 837,
            width: 1920,
            height: 1080,
            ..TsGraphicsStream::default()
        };
        let mut scanned_video = TsVideoStream::default();
        scanned_video.set_video_format(TsVideoFormat::Videoformat2160p);
        let scanned = BTreeMap::from([
            (0x1200_u16, TsStream::Graphics(scanned_pgs)),
            // A non-graphics source is skipped (the quick-merged detail
            // stays), and a PID the playlist does not present adds nothing.
            (0x1011, TsStream::Video(scanned_video)),
            (0x1300, TsStream::Graphics(TsGraphicsStream::default())),
        ]);
        super::remerge_graphics_detail(&mut playlist, &scanned);

        // The summary view proves the merge: the PGS row carries the full
        // pass's dimensions and caption count; the video row kept its quick
        // detail (non-graphics sources are not re-merged).
        let graphics = stream_summary(playlist.streams.get(&0x1200).unwrap());
        assert_eq!(graphics.description, "1920x1080 / 837 Captions");
        let video = stream_summary(playlist.streams.get(&0x1011).unwrap());
        assert_eq!(video.height, 0, "non-graphics sources are not re-merged");
        assert!(!playlist.streams.contains_key(&0x1300));
    }

    fn as_audio(s: &TsStream) -> Option<&TsAudioStream> {
        if let TsStream::Audio(audio) = s { Some(audio) } else { None }
    }

    /// The graphics stream inside `s`, or `None` for any other variant.
    fn as_graphics(s: &TsStream) -> Option<&TsGraphicsStream> {
        if let TsStream::Graphics(graphics) = s { Some(graphics) } else { None }
    }

    #[test]
    fn merge_stream_copies_scanned_detail_per_kind() {
        // Video: the scanned encoding profile + extended data flow into the clip
        // stream; the clip stream's video format (not part of the merge) is kept.
        let mut dst = TsVideoStream::default();
        dst.base.stream_type = TsStreamType::AvcVideo;
        dst.set_video_format(TsVideoFormat::Videoformat1080p);
        let mut src = TsVideoStream::default();
        src.base.stream_type = TsStreamType::AvcVideo;
        src.encoding_profile = Some("High Profile 4.1".to_owned());
        src.base.bit_rate = 5;
        src.base.is_vbr = true;
        let mut video = TsStream::Video(dst);
        merge_stream(&mut video, &TsStream::Video(src));
        // The `None` arm of `as_audio` is exercised here (video is not audio).
        assert!(as_audio(&video).is_none());
        let merged = as_video(&video).unwrap();
        assert_eq!(merged.encoding_profile.as_deref(), Some("High Profile 4.1"));
        assert_eq!(merged.height, 1080); // clip-info format preserved
        assert_eq!(merged.base.bit_rate, 5); // max(0, 5)
        assert!(merged.base.is_vbr);

        // Audio: count/sample/depth take the max, dial-norm the min; mode/extension/
        // core flow in; the clip-info language survives.
        let mut dst = TsAudioStream::default();
        dst.base.stream_type = TsStreamType::DtsHdMasterAudio;
        dst.base.set_language_code("jpn");
        let mut src = TsAudioStream::default();
        src.base.stream_type = TsStreamType::DtsHdMasterAudio;
        src.channel_count = 6;
        src.lfe = 1;
        src.sample_rate = 48_000;
        src.bit_depth = 24;
        src.dial_norm = -27;
        src.audio_mode = TsAudioMode::Extended;
        src.has_extensions = true;
        src.ext_data = Some("x".to_owned());
        src.core_stream = Some(Box::new(TsAudioStream::default()));
        let mut audio = TsStream::Audio(dst);
        merge_stream(&mut audio, &TsStream::Audio(src));
        let merged = as_audio(&audio).unwrap();
        assert_eq!((merged.channel_count, merged.lfe), (6, 1));
        assert_eq!((merged.sample_rate, merged.bit_depth), (48_000, 24));
        assert_eq!(merged.dial_norm, -27); // min(0, -27)
        assert_eq!(merged.audio_mode, TsAudioMode::Extended);
        assert!(merged.has_extensions);
        assert_eq!(merged.ext_data.as_deref(), Some("x"));
        assert!(merged.core_stream.is_some());
        assert_eq!(merged.base.language_name.as_deref(), Some("Japanese"));

        // Graphics: caption tallies + resolution copied.
        let mut dst = TsGraphicsStream::default();
        dst.base.stream_type = TsStreamType::PresentationGraphics;
        let mut src = TsGraphicsStream::default();
        src.base.stream_type = TsStreamType::PresentationGraphics;
        src.width = 1920;
        src.height = 1080;
        src.captions = 5;
        src.forced_captions = 1;
        let mut graphics = TsStream::Graphics(dst);
        merge_stream(&mut graphics, &TsStream::Graphics(src));
        // The `None` arms of `as_video`/`as_graphics` are exercised across variants.
        assert!(as_video(&audio).is_none());
        assert!(as_graphics(&video).is_none());
        let merged = as_graphics(&graphics).unwrap();
        assert_eq!((merged.width, merged.height), (1920, 1080));
        assert_eq!((merged.captions, merged.forced_captions), (5, 1));
    }

    #[test]
    fn merge_stream_core_and_dialnorm_edge_cases() {
        // dial-norm min keeps the already-lower clip value; an existing core stream is
        // NOT overwritten by the source's (the `dst.core_stream.is_none()` guard). The
        // two cores carry distinct channel counts so a `&&`→`||` mutant — which would
        // overwrite — is caught: the merged core must stay the destination's.
        let mut dst = TsAudioStream::default();
        dst.base.stream_type = TsStreamType::Ac3TrueHdAudio;
        dst.dial_norm = -31; // already lower than the source's -27
        dst.core_stream =
            Some(Box::new(TsAudioStream { channel_count: 9, ..TsAudioStream::default() }));
        let mut src = TsAudioStream::default();
        src.base.stream_type = TsStreamType::Ac3TrueHdAudio;
        src.dial_norm = -27;
        src.core_stream =
            Some(Box::new(TsAudioStream { channel_count: 5, ..TsAudioStream::default() }));
        let mut audio = TsStream::Audio(dst);
        merge_stream(&mut audio, &TsStream::Audio(src));
        let merged = as_audio(&audio).unwrap();
        assert_eq!(merged.dial_norm, -31); // min(-31, -27)
        assert_eq!(merged.core_stream.as_ref().unwrap().channel_count, 9); // kept dst's core

        // A source with no core, onto a destination with no core → still none.
        let mut dst = TsAudioStream::default();
        dst.base.stream_type = TsStreamType::DtsAudio;
        let mut audio = TsStream::Audio(dst);
        merge_stream(&mut audio, &TsStream::Audio(TsAudioStream::default()));
        assert!(as_audio(&audio).unwrap().core_stream.is_none());
    }

    #[test]
    fn merge_stream_ignores_text_and_type_mismatches() {
        // Text → the `_` arm carries no codec detail, but the base bit-rate/VBR merge.
        let mut dst = TsTextStream::default();
        dst.base.stream_type = TsStreamType::Subtitle;
        let mut src = TsTextStream::default();
        src.base.stream_type = TsStreamType::Subtitle;
        src.base.is_vbr = true;
        let mut text = TsStream::Text(dst);
        merge_stream(&mut text, &TsStream::Text(src));
        assert!(text.base().is_vbr);

        // A type mismatch is a no-op — the merge bails before touching `dst`.
        let mut dst = TsAudioStream::default();
        dst.base.stream_type = TsStreamType::Ac3Audio;
        let mut src = TsVideoStream::default();
        src.base.stream_type = TsStreamType::AvcVideo;
        src.base.bit_rate = 999;
        let mut audio = TsStream::Audio(dst);
        merge_stream(&mut audio, &TsStream::Video(src));
        assert_eq!(audio.base().bit_rate, 0); // unchanged: types differ
    }

    #[test]
    fn stream_summary_reads_each_variant() {
        let mut video = TsVideoStream::default();
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::AvcVideo;
        video.height = 1080;
        let summary = stream_summary(&TsStream::Video(video));
        assert_eq!(summary.pid, Pid::new(0x1011));
        assert_eq!(summary.height, 1080);
        assert_eq!(summary.stream_type.name(), "AVC_VIDEO");
        assert_eq!(summary.codec_short_name, "AVC");
        assert_eq!(summary.codec_name, "MPEG-4 AVC Video");
        assert_eq!(summary.codec_alt_name, "AVC");
        // A video row carries no audio detail and starts on the main angle.
        assert_eq!(summary.channel_description, "");
        assert_eq!((summary.sample_rate, summary.bit_depth, summary.channel_count), (0, 0, 0));
        assert_eq!(summary.angle_index, 0);
        assert_eq!(summary.language_code, "");

        let mut audio = TsAudioStream::default();
        audio.base.stream_type = TsStreamType::LpcmAudio;
        audio.base.set_language_code("jpn");
        audio.channel_count = 2;
        audio.lfe = 0;
        audio.sample_rate = 48_000;
        audio.bit_depth = 16;
        let summary = stream_summary(&TsStream::Audio(audio));
        assert_eq!(summary.height, 0);
        assert_eq!(summary.codec_short_name, "LPCM");
        assert_eq!(summary.codec_alt_name, "LPCM");
        assert_eq!(summary.language_code, "jpn");
        assert_eq!(summary.language_name, "Japanese");
        assert_eq!(summary.channel_description, "2.0");
        assert_eq!(
            (summary.sample_rate, summary.bit_depth, summary.channel_count),
            (48_000, 16, 2)
        );

        let mut graphics = TsGraphicsStream::default();
        graphics.base.stream_type = TsStreamType::PresentationGraphics;
        let summary = stream_summary(&TsStream::Graphics(graphics));
        assert_eq!(summary.codec_short_name, "PGS");
        assert_eq!(summary.codec_alt_name, "PGS");
        assert_eq!(summary.description, "");

        let mut text = TsTextStream::default();
        text.base.stream_type = TsStreamType::Subtitle;
        text.base.set_language_code("eng");
        let summary = stream_summary(&TsStream::Text(text));
        assert_eq!(
            (summary.codec_short_name.as_str(), summary.codec_name.as_str()),
            ("SUB", "Subtitle")
        );
        assert_eq!(summary.language_name, "English");
        assert_eq!(summary.language_code, "eng");
        assert_eq!(summary.codec_alt_name, "SUB");
    }

    #[test]
    fn full_description_patches_audio_respells_video_and_falls_back() {
        // Audio with a measured rate: the description is respelled with that
        // rate patched in; without one, the row's own spelling is kept AS-IS
        // (not re-derived at rate zero — the stream's 512 kbps must survive).
        let mut audio = TsAudioStream::default();
        audio.base.stream_type = TsStreamType::LpcmAudio;
        audio.channel_count = 2;
        audio.sample_rate = 48_000;
        audio.bit_depth = 16;
        audio.base.bit_rate = 512_000;
        let mut patched = audio.clone();
        patched.base.bit_rate = 256_000;
        let respelled = patched.description();
        let plain = audio.description();
        assert_ne!(respelled, plain);
        let stream = TsStream::Audio(audio);
        let mut row = stream_summary(&stream);
        row.bitrate = 256_000;
        assert_eq!(super::full_description(&stream, &row), respelled);
        row.bitrate = 0;
        assert_eq!(super::full_description(&stream, &row), plain);

        // Video takes its own full (exact-luminance) description, never the
        // row's — a sentinel row description must not leak through.
        let mut video = TsVideoStream::default();
        video.base.stream_type = TsStreamType::AvcVideo;
        video.height = 1080;
        let expected = video.full_description();
        let stream = TsStream::Video(video);
        let mut row = stream_summary(&stream);
        row.description = "sentinel".to_owned();
        assert_eq!(super::full_description(&stream, &row), expected);

        // Everything else keeps the row's description verbatim.
        let mut graphics = TsGraphicsStream::default();
        graphics.base.stream_type = TsStreamType::PresentationGraphics;
        let stream = TsStream::Graphics(graphics);
        let mut row = stream_summary(&stream);
        row.description = "kept".to_owned();
        assert_eq!(super::full_description(&stream, &row), "kept");
    }

    #[test]
    fn build_sorted_streams_takes_graphics_detail_from_the_resolved_streams() {
        // The clip info (and the quick scan) carry no PGS detail; the
        // resolved presented stream holds the full pass's re-merged caption
        // tallies — the row's description comes from there.
        let mut clip = BTreeMap::new();
        let mut graphics = TsGraphicsStream::default();
        graphics.base.pid = Pid::new(0x1200);
        graphics.base.stream_type = TsStreamType::PresentationGraphics;
        graphics.base.set_language_code("deu");
        clip.insert(0x1200, TsStream::Graphics(graphics));

        let mut resolved = bare_playlist();
        let mut presented = TsGraphicsStream {
            captions: 837,
            width: 1920,
            height: 1080,
            ..TsGraphicsStream::default()
        };
        presented.base.pid = Pid::new(0x1200);
        presented.base.stream_type = TsStreamType::PresentationGraphics;
        resolved.streams = BTreeMap::from([(0x1200_u16, TsStream::Graphics(presented))]);

        let rows = build_sorted_streams(&clip, None, &resolved, 0);
        assert_eq!(rows.first().unwrap().description, "1920x1080 / 837 Captions");

        // An unresolved playlist keeps the quick detail: an empty description.
        let rows = build_sorted_streams(&clip, None, &bare_playlist(), 0);
        assert_eq!(rows.first().unwrap().description, "");
    }

    #[test]
    fn build_sorted_streams_orders_clones_and_merges() {
        // A clip with a 1080p AVC video (PID 0x1011) and an English AC3 audio (0x1100).
        let mut clip = BTreeMap::new();
        let mut video = TsVideoStream::default();
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::AvcVideo;
        video.set_video_format(TsVideoFormat::Videoformat1080p);
        clip.insert(0x1011, TsStream::Video(video));
        let mut audio = TsAudioStream::default();
        audio.base.pid = Pid::new(0x1100);
        audio.base.stream_type = TsStreamType::Ac3Audio;
        audio.base.set_language_code("eng");
        clip.insert(0x1100, TsStream::Audio(audio));

        // No scan, no angles → one row per stream, ordered by PID. An
        // unresolved playlist leaves the measured rates at zero.
        let unresolved = bare_playlist();
        let rows = build_sorted_streams(&clip, None, &unresolved, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.first().unwrap().pid, Pid::new(0x1011));
        assert_eq!(rows.first().unwrap().stream_type.name(), "AVC_VIDEO");
        assert_eq!(rows.first().unwrap().bitrate, 0);
        assert_eq!(rows.get(1).unwrap().pid, Pid::new(0x1100));
        assert_eq!(rows.get(1).unwrap().language_name, "English");

        // Two extra angles → the video PID appears three times (base + 2 clones),
        // audio once → four rows, still PID-ordered with the clones grouped first.
        // The clones carry their 1-based angle index; main rows stay 0.
        let rows = build_sorted_streams(&clip, None, &unresolved, 2);
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().take(3).all(|r| r.pid == Pid::new(0x1011)));
        assert_eq!(rows.get(3).unwrap().pid, Pid::new(0x1100));
        let angles: Vec<usize> = rows.iter().map(|r| r.angle_index).collect();
        assert_eq!(angles, [0, 1, 2, 0]);

        // A resolved playlist supplies each row's measured rates: the main map
        // for the main rows, each angle's own map for its clone.
        let mut resolved = bare_playlist();
        resolved.streams = clip.clone();
        let main_video = resolved.streams.get_mut(&0x1011).expect("the video is presented");
        main_video.base_mut().bit_rate = 26_030_000;
        main_video.base_mut().active_bit_rate = 26_031_000;
        resolved.streams.get_mut(&0x1100).expect("the audio is presented").base_mut().bit_rate =
            640_000;
        let mut angle = clip.clone();
        let angle_video = angle.get_mut(&0x1011).expect("the angle video is presented");
        angle_video.base_mut().bit_rate = 26_120_000;
        angle_video.base_mut().active_bit_rate = 26_121_000;
        resolved.angle_streams = vec![angle];
        let rows = build_sorted_streams(&clip, None, &resolved, 1);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.first().unwrap().bitrate, 26_030_000);
        assert_eq!(rows.first().unwrap().active_bitrate, 26_031_000);
        assert_eq!(rows.get(1).unwrap().bitrate, 26_120_000);
        assert_eq!(rows.get(1).unwrap().active_bitrate, 26_121_000);
        assert_eq!(rows.get(2).unwrap().bitrate, 640_000);
        assert_eq!(rows.get(2).unwrap().active_bitrate, 0);

        // More angle clones than resolved angle maps → the surplus clone keeps
        // zero rates rather than borrowing another angle's.
        let rows = build_sorted_streams(&clip, None, &resolved, 2);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows.get(1).unwrap().bitrate, 26_120_000);
        assert_eq!(rows.get(2).unwrap().bitrate, 0);

        // A scanned AVC stream supplies the encoding profile (the merge ran).
        let mut scanned = BTreeMap::new();
        let mut scanned_video = TsVideoStream::default();
        scanned_video.base.pid = Pid::new(0x1011);
        scanned_video.base.stream_type = TsStreamType::AvcVideo;
        scanned_video.encoding_profile = Some("High Profile 4.1".to_owned());
        scanned.insert(0x1011, TsStream::Video(scanned_video));
        let rows = build_sorted_streams(&clip, Some(&scanned), &unresolved, 0);
        assert!(rows.first().unwrap().description.contains("High Profile 4.1"));

        // A scanned stream whose PID is absent from the clip is ignored — neither
        // merged nor presented; the clip's own streams stay the presented set.
        let mut orphan = BTreeMap::new();
        let mut orphan_audio = TsAudioStream::default();
        orphan_audio.base.pid = Pid::new(0x9999);
        orphan_audio.base.stream_type = TsStreamType::Ac3Audio;
        orphan.insert(0x9999, TsStream::Audio(orphan_audio));
        assert_eq!(build_sorted_streams(&clip, Some(&orphan), &unresolved, 0).len(), 2);
    }

    #[test]
    fn build_sorted_streams_presents_the_resolved_dependent_view() {
        // A clip with only the base video; the resolved playlist also presents
        // the interleaved MVC dependent view with a measured rate.
        let mut clip = BTreeMap::new();
        let mut video = TsVideoStream::default();
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::AvcVideo;
        clip.insert(0x1011, TsStream::Video(video));

        let mut resolved = bare_playlist();
        resolved.streams = clip.clone();
        let mut mvc = TsVideoStream::default();
        mvc.base.pid = Pid::new(MVC_PID);
        mvc.base.stream_type = TsStreamType::MvcVideo;
        mvc.base.bit_rate = 7_579_000;
        resolved.streams.insert(MVC_PID, TsStream::Video(mvc));

        let rows = build_sorted_streams(&clip, None, &resolved, 0);
        assert_eq!(rows.len(), 2);
        let row = rows.get(1).unwrap();
        assert_eq!(row.pid, Pid::new(MVC_PID));
        assert_eq!(row.stream_type.name(), "MVC_VIDEO");
        assert_eq!(
            (row.codec_short_name.as_str(), row.codec_name.as_str()),
            ("MVC", "MPEG-4 MVC Video")
        );
        assert_eq!(row.description, "");
        assert_eq!(row.bitrate, 7_579_000);
        assert!(row.ssif_only);
        assert!(!row.is_hidden, "the dependent view is never marked hidden");
        // The base video row is an (undeclared) clip stream: hidden, not
        // SSIF-only.
        assert!(rows.first().unwrap().is_hidden);
        assert!(!rows.first().unwrap().ssif_only);

        // A clip that declares the MVC PID itself yields a plain clip-stream
        // row — no SSIF presentation, and the hidden check applies as usual.
        let mut declared = clip.clone();
        let mut own = TsVideoStream::default();
        own.base.pid = Pid::new(MVC_PID);
        own.base.stream_type = TsStreamType::MvcVideo;
        declared.insert(MVC_PID, TsStream::Video(own));
        let rows = build_sorted_streams(&declared, None, &resolved, 0);
        assert_eq!(rows.len(), 2);
        assert!(!rows.get(1).unwrap().ssif_only);
        assert!(rows.get(1).unwrap().is_hidden);
    }

    // ── orchestration: a full disc with every flag set ──────────────────────

    #[test]
    fn open_scans_a_full_disc() {
        let clip = clpi(&[
            (0x1011, 0x1B, [0x63, 0x30, 0, 0]),       // AVC 1080p / 25 fps
            (0x1100, 0x81, [0x61, b'e', b'n', b'g']), // AC3 audio
            (0x1200, 0x90, [b'e', b'n', b'g', 0]),    // PG graphics
            (0x1A00, 0x92, [0, b'f', b'r', b'a']),    // subtitle text
        ]);
        let play = mpls("00000", 2_700_000, 4_500_000, &[(1, 0, 2_700_000)]);
        let index = b"INDX0300\x00\x00".to_vec();
        let xml = title_xml("Movie");
        let m2ts = vec![0_u8; 1000];
        let ssif = vec![0_u8; 500];

        // The trailing three ones: the bdjo, the mnv, and FilmIndex.xml.
        let expected_size: u64 =
            [index.len(), play.len(), clip.len(), m2ts.len(), xml.len(), 1, 1, 1]
                .iter()
                .map(|&n| u64::try_from(n).unwrap())
                .sum();

        let disc = TempDisc::build(
            &["BDSVM"],
            &[
                ("BDMV/index.bdmv", index),
                ("BDMV/PLAYLIST/00000.mpls", play),
                ("BDMV/CLIPINF/00000.clpi", clip),
                ("BDMV/STREAM/00000.m2ts", m2ts),
                ("BDMV/STREAM/SSIF/00000.ssif", ssif), // excluded from Size, used for ifilesize
                ("BDMV/META/DL/bdmt_eng.xml", xml),
                ("BDMV/BDJO/00000.bdjo", vec![b'x']),
                ("SNP/clip.mnv", vec![b'x']),
                ("FilmIndex.xml", vec![b'x']),
            ],
        );
        let bd = disc.open().expect("scan full disc");

        assert!(bd.volume_label.starts_with("bdinfo-rs-disc-"));
        assert_eq!(bd.disc_title.as_deref(), Some("Movie"));
        assert_eq!(bd.size, expected_size); // the 500-byte .ssif is excluded
        assert!(bd.is_uhd);
        assert!(bd.is_3d);
        assert!(bd.is_bd_plus);
        assert!(bd.is_bd_java);
        assert!(bd.is_dbox);
        assert!(bd.is_psp);
        assert!(bd.is_50hz);
        // Every extras label, in the fixed presentation order.
        assert_eq!(
            bd.extra_features(),
            [
                "Ultra HD",
                "BD-Java",
                "50Hz Content",
                "Blu-ray 3D",
                "D-BOX Motion Code",
                "PSP Digital Copy"
            ]
        );
        // The 40 s playlist passes the default filter.
        assert_eq!(bd.presentation_order(&PlaylistFilter::default()), [0]);

        assert_eq!(bd.playlists.len(), 1);
        let pl = bd.playlists.first().unwrap();
        assert_eq!(pl.name, "00000.MPLS");
        assert_eq!(pl.total_length.to_bits(), 40.0_f64.to_bits());
        assert_eq!(pl.file_size, 1000);
        assert_eq!(pl.interleaved_file_size, 500);
        assert_eq!(pl.chapter_count, 1);
        assert_eq!(pl.stream_count, 4);
        // The scan-free open leaves the presented streams unfilled by codec detail
        // but still built from the clip-info entries (one per kind).
        assert_eq!(pl.streams.len(), 4);

        // The packet scan reads the (synthetic, codec-less) `*.m2ts`: the streams are
        // still the four clip-info entries (no codec detail to merge), in PID order.
        let scanned = disc.open_scanned().expect("packet-scan the disc");
        let pl = scanned.playlists.first().unwrap();
        assert_eq!(pl.stream_count, 4);
        let pids: Vec<u16> = pl.streams.iter().map(|s| s.pid.get()).collect();
        assert_eq!(pids, vec![0x1011, 0x1100, 0x1200, 0x1A00]);
        assert_eq!(pl.streams.first().unwrap().stream_type.name(), "AVC_VIDEO");

        // One chapter row per mark; the synthetic (codec-less) stream yields
        // no frames, so the row keeps its times with zero rates.
        assert_eq!(pl.chapters.len(), 1);
        let row = pl.chapters.first().unwrap();
        assert_eq!(row.time_in.to_bits(), 0.0_f64.to_bits());
        assert_eq!(row.length.to_bits(), 40.0_f64.to_bits());
        assert_eq!(row.avg_rate.to_bits(), 0.0_f64.to_bits());
    }

    // ── orchestration: present-but-empty dirs / absent stream file (false sides) ──

    #[test]
    fn open_scans_a_disc_with_empty_dirs_and_missing_stream() {
        let clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]); // 1080p / 24 fps
        let play = mpls("00000", 0, 4_500_000, &[]); // 100 s, no chapters
        let disc = TempDisc::build(
            &["BDMV/BDJO", "BDMV/STREAM/SSIF"], // present but empty
            &[
                ("BDMV/PLAYLIST/00000.mpls", play),
                ("BDMV/CLIPINF/00000.clpi", clip),
                ("BDMV/META/note.txt", vec![b'x']), // META present, no bdmt_eng.xml
                ("SNP/note.txt", vec![b'x']),       // SNP present, no *.mnv
                ("readme.txt", vec![b'x']),         // a root file that is not FilmIndex.xml
            ],
        );
        let bd = disc.open().expect("scan sparse disc");

        assert!(!bd.is_uhd); // no index.bdmv
        assert!(!bd.is_3d); // SSIF empty
        assert!(!bd.is_bd_plus); // no BDSVM/SLYVM/ANYVM
        assert!(!bd.is_bd_java); // BDJO empty
        assert!(!bd.is_dbox); // no FilmIndex.xml in the root
        assert!(!bd.is_psp); // SNP has no *.mnv
        assert!(bd.extra_features().is_empty()); // no flag set, no label
        assert!(!bd.is_50hz); // 24 fps
        assert_eq!(bd.disc_title, None); // META has no bdmt_eng.xml

        let pl = bd.playlists.first().unwrap();
        assert_eq!(pl.total_length.to_bits(), 100.0_f64.to_bits());
        assert_eq!(pl.file_size, 0); // no *.m2ts on disc
        assert_eq!(pl.interleaved_file_size, 0);
        assert_eq!(pl.chapter_count, 0);
        assert_eq!(pl.stream_count, 1);
    }

    // ── orchestration: the truly-minimal disc (None branches) ───────────────

    #[test]
    fn open_scans_a_minimal_disc() {
        let disc = TempDisc::build(
            &["BDMV/PLAYLIST", "BDMV/CLIPINF"],
            &[("BDMV/MovieObject.bdmv", vec![b'x'])], // a non-index file in BDMV
        );
        let bd = disc.open().expect("scan minimal disc");
        assert_eq!(bd.size, 1);
        assert!(!bd.is_uhd && !bd.is_3d && !bd.is_bd_plus && !bd.is_bd_java && !bd.is_psp);
        assert_eq!(bd.disc_title, None);
        assert!(bd.playlists.is_empty());
    }

    #[test]
    fn open_handles_a_playlist_with_no_clips() {
        // A zero-item playlist resolves no reference clip → no presented streams.
        let disc =
            TempDisc::build(&["BDMV/CLIPINF"], &[("BDMV/PLAYLIST/00000.mpls", empty_mpls())]);
        let bd = disc.open().expect("scan clip-less playlist");
        let pl = bd.playlists.first().unwrap();
        assert_eq!(pl.stream_count, 0);
        assert!(pl.streams.is_empty());
        assert_eq!(pl.total_length.to_bits(), 0.0_f64.to_bits());
        assert!(!bd.is_50hz);
    }

    #[test]
    fn open_marks_undeclared_streams_hidden() {
        // The clip carries video + two audio streams; the playlist declares
        // only the first audio PID — the video and the second audio are
        // presented but hidden by this playlist.
        let clip = clpi(&[
            (0x1011, 0x1B, [0x62, 0x30, 0, 0]),
            (0x1100, 0x81, [0x61, b'e', b'n', b'g']),
            (0x1101, 0x81, [0x61, b'f', b'r', b'a']),
        ]);
        let play = mpls_declared("00000", 0, 4_500_000, &[0x1100], &[]);
        let disc = TempDisc::build(
            &[],
            &[("BDMV/PLAYLIST/00000.mpls", play), ("BDMV/CLIPINF/00000.clpi", clip)],
        );
        let bd = disc.open().expect("scan disc with a declared stream");
        let pl = bd.playlists.first().unwrap();
        let hidden: Vec<(u16, bool)> =
            pl.streams.iter().map(|s| (s.pid.get(), s.is_hidden)).collect();
        assert_eq!(hidden, vec![(0x1011, true), (0x1100, false), (0x1101, true)]);
        assert!(pl.has_hidden_streams());
    }

    #[test]
    fn open_detects_a_looping_playlist() {
        // The same clip replayed from the same in-time → a loop; two different
        // clips (or different in-times) → no loop.
        let clip = clpi(&[(0x1100, 0x81, [0x61, b'e', b'n', b'g'])]);
        let looping = mpls_items(&["00000", "00000"], 0, 4_500_000, &[], &[], &[]);
        let chained = mpls_items(&["00000", "00001"], 0, 4_500_000, &[], &[], &[]);
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", looping),
                ("BDMV/PLAYLIST/00001.mpls", chained),
                ("BDMV/CLIPINF/00000.clpi", clip.clone()),
                ("BDMV/CLIPINF/00001.clpi", clip),
            ],
        );
        let bd = disc.open().expect("scan looping disc");
        assert_eq!(bd.playlists.len(), 2);
        assert!(bd.playlists.first().unwrap().has_loops);
        assert!(!bd.playlists.get(1).unwrap().has_loops);
        // The loop also doubles the playlist length (both items count).
        assert_eq!(bd.playlists.first().unwrap().total_length.to_bits(), 200.0_f64.to_bits());
        // The default presentation order drops the looping playlist, keeping
        // only the chained one; the everything filter keeps both in one group
        // (they share 00000.M2TS), name-ordered on the length tie.
        assert_eq!(bd.presentation_order(&PlaylistFilter::default()), [1]);
        assert_eq!(bd.presentation_order(&PlaylistFilter::everything()), [0, 1]);
    }

    #[test]
    fn open_leaves_fully_declared_streams_visible() {
        // The playlist declares every clip stream — nothing is hidden.
        let clip = clpi(&[(0x1100, 0x81, [0x61, b'e', b'n', b'g'])]);
        let play = mpls_declared("00000", 0, 4_500_000, &[0x1100], &[]);
        let disc = TempDisc::build(
            &[],
            &[("BDMV/PLAYLIST/00000.mpls", play), ("BDMV/CLIPINF/00000.clpi", clip)],
        );
        let bd = disc.open().expect("scan disc with all streams declared");
        let pl = bd.playlists.first().unwrap();
        assert!(pl.streams.iter().all(|s| !s.is_hidden));
        assert!(!pl.has_hidden_streams());
    }

    #[test]
    fn open_with_packet_scan_and_no_stream_directory() {
        // A packet scan requested on a disc with no STREAM/ dir → the per-stream-file
        // scan short-circuits; the streams are still built from the clip info.
        let clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let play = mpls("00000", 0, 4_500_000, &[]);
        let disc = TempDisc::build(
            &[],
            &[("BDMV/PLAYLIST/00000.mpls", play), ("BDMV/CLIPINF/00000.clpi", clip)],
        );
        let bd = disc.open_scanned().expect("packet-scan without a STREAM dir");
        let pl = bd.playlists.first().unwrap();
        assert_eq!(pl.stream_count, 1);
        assert_eq!(pl.file_size, 0);
    }

    #[test]
    fn open_counts_only_angle_zero_clips() {
        // A multi-angle playlist: the main clip resolves; the angle clips
        // contribute only to `file_size` and are skipped for the reference clip
        // + `total_length`.
        let clip =
            clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0]), (0x1100, 0x81, [0x61, b'e', b'n', b'g'])]);
        let play = mpls_angles("00000", 2_700_000, 4_500_000, &["00010", "00021"], &[]);
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", play),
                ("BDMV/CLIPINF/00000.clpi", clip),
                ("BDMV/STREAM/00000.m2ts", vec![0; 100]),
            ],
        );
        let bd = disc.open().expect("scan multi-angle disc");
        let pl = bd.playlists.first().unwrap();
        assert_eq!(pl.total_length.to_bits(), 40.0_f64.to_bits()); // main clip only
        assert_eq!(pl.file_size, 100); // only the present main *.m2ts
        // Presented streams = video × (1 + angle_count) + audio = 1×3 + 1.
        assert_eq!(pl.stream_count, 4);
    }

    // ── measurement model: totals, tallies, and the angle timeline ──────────

    /// A [`ClipSummary`] with the given angle/start/length/packet count and no
    /// per-stream tallies.
    fn clip_summary(angle_index: i32, time_in: f64, length: f64, packets: u64) -> ClipSummary {
        ClipSummary {
            name: "00000.M2TS".to_owned(),
            display_name: "00000.M2TS".to_owned(),
            angle_index,
            relative_time_in: time_in,
            length,
            payload_bytes: 0,
            packet_count: packets,
            packet_seconds: 0.0,
            file_seconds: 0.0,
            streams: Vec::new(),
        }
    }

    /// A [`PlaylistSummary`] carrying only the fields the totals math reads.
    fn playlist_summary(
        total_length: f64,
        angle_count: usize,
        clips: Vec<ClipSummary>,
    ) -> PlaylistSummary {
        PlaylistSummary {
            name: "00000.MPLS".to_owned(),
            total_length,
            file_size: 0,
            interleaved_file_size: 0,
            chapter_count: 0,
            stream_count: 0,
            angle_count,
            has_loops: false,
            streams: Vec::new(),
            clips,
            chapters: Vec::new(),
        }
    }

    #[test]
    fn chapter_summaries_walk_the_first_video_pid_diagnostics() {
        use crate::bdrom::m2ts::TsStreamDiagnostics;

        // An audio stream below the video PID proves the diagnostics PID is the
        // first *video* stream, not merely the first stream.
        let mut audio = TsAudioStream::default();
        audio.base.stream_type = TsStreamType::Ac3Audio;
        let mut video = TsVideoStream::default();
        video.base.stream_type = TsStreamType::AvcVideo;
        let mut playlist = TsPlaylistFile {
            file_type: "MPLS0200".to_owned(),
            name: "00000.MPLS".to_owned(),
            mvc_base_view_r: false,
            chapters: vec![0.0],
            playlist_streams: BTreeMap::new(),
            streams: BTreeMap::from([
                (0x1010, TsStream::Audio(audio)),
                (0x1011, TsStream::Video(video)),
            ]),
            angle_streams: Vec::new(),
            stream_clips: vec![
                TsStreamClip { name: "00000.M2TS".to_owned(), ..TsStreamClip::default() },
                // No measured file for this clip → it is skipped whole.
                TsStreamClip { name: "00001.M2TS".to_owned(), ..TsStreamClip::default() },
            ],
            angle_count: 0,
        };
        let mut file = TsStreamFile::new("00000.m2ts");
        file.stream_diagnostics.insert(
            0x1011,
            vec![
                TsStreamDiagnostics {
                    bytes: 1000,
                    packets: 1,
                    marker: 1.0,
                    interval: 1.0,
                    tag: Some("I".to_owned()),
                },
                TsStreamDiagnostics {
                    bytes: 1000,
                    packets: 1,
                    marker: 2.0,
                    interval: 1.0,
                    tag: None,
                },
            ],
        );
        let measured = BTreeMap::from([("00000.M2TS".to_owned(), file)]);

        let rows = build_chapter_summaries(&playlist, &measured, 4.0);
        assert_eq!(rows.len(), 1);
        let row = rows.first().unwrap();
        assert_eq!(row.length.to_bits(), 4.0_f64.to_bits());
        assert_eq!(row.avg_rate.to_bits(), 4000.0_f64.to_bits()); // 2000 B × 8 / 4 s
        assert_eq!(row.avg_frame_size.to_bits(), 2000.0_f64.to_bits()); // one tagged frame

        // Without a presented video stream there is no diagnostics PID: the
        // rows keep their times but measure nothing.
        playlist.streams.remove(&0x1011);
        let rows = build_chapter_summaries(&playlist, &measured, 4.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows.first().unwrap().avg_rate.to_bits(), 0.0_f64.to_bits());
        assert_eq!(rows.first().unwrap().length.to_bits(), 4.0_f64.to_bits());
    }

    #[test]
    fn clip_packet_size_and_bit_rate_derive_from_the_tallies() {
        let mut clip = clip_summary(0, 0.0, 10.0, 6);
        clip.packet_seconds = 4.0;
        assert_eq!(clip.packet_size(), 1152); // 6 packets × 192
        assert_eq!(clip.packet_bit_rate(), 2304); // 1152 × 8 / 4.0

        // Nothing demuxed → zero rate, not a division by zero.
        clip.packet_seconds = 0.0;
        assert_eq!(clip.packet_bit_rate(), 0);

        // The rate rounds half-to-even: 100 packets × 192 × 8 / 7 s = 21 942.857…
        clip.packet_count = 100;
        clip.packet_seconds = 7.0;
        assert_eq!(clip.packet_bit_rate(), 21_943);
    }

    #[test]
    fn rate_over_guards_the_zero_duration() {
        assert_eq!(rate_over(1152, 4.0), 2304);
        assert_eq!(rate_over(1152, 0.0), 0);
        assert_eq!(rate_over(1152, -1.0), 0);
    }

    #[test]
    fn playlist_totals_split_main_and_angle_clips() {
        // Main clips: 10 packets at t=0 (40 s) and 20 packets at t=40 (60 s);
        // angle 1 replaces the t=0 slot with 30 packets; angle 2 has no clips.
        let playlist = playlist_summary(
            100.0,
            2,
            vec![
                clip_summary(0, 0.0, 40.0, 10),
                clip_summary(1, 0.0, 40.0, 30),
                clip_summary(0, 40.0, 60.0, 20),
            ],
        );
        assert_eq!(playlist.total_packet_size(), 30 * 192); // main clips only
        assert_eq!(playlist.total_angle_packet_size(), 60 * 192); // every clip
        assert_eq!(playlist.total_angle_length().to_bits(), 140.0_f64.to_bits());
        // 30 × 192 × 8 / 100 s = 460.8 → 461 (half-to-even rounding).
        assert_eq!(playlist.total_bit_rate(), 461);
        // 60 × 192 × 8 / 140 s = 658.28… → 658.
        assert_eq!(playlist.total_angle_bit_rate(), 658);

        let angles = playlist.angle_totals();
        assert_eq!(angles.len(), 2);
        // Angle 1: its own clip (30 packets, 40 s); its timeline replaces the
        // t=0 main clip, keeping the t=40 one (30 + 20 packets).
        let one = angles.first().unwrap();
        assert_eq!(one.length.to_bits(), 40.0_f64.to_bits());
        assert_eq!(one.packet_size, 30 * 192);
        assert_eq!(one.timeline_packet_size, 50 * 192);
        // Angle 2: no clips of its own — the timeline is the main clips.
        let two = angles.get(1).unwrap();
        assert_eq!(two.length.to_bits(), 0.0_f64.to_bits());
        assert_eq!(two.packet_size, 0);
        assert_eq!(two.timeline_packet_size, 30 * 192);

        // No angles → no totals.
        assert!(playlist_summary(100.0, 0, Vec::new()).angle_totals().is_empty());

        // The per-angle rates: angle 1's own clips over its own length
        // (30 × 192 × 8 / 40 s = 1152), its timeline over the playlist length
        // (50 × 192 × 8 / 100 s = 768); the clip-less angle 2 rates zero.
        assert_eq!(one.bit_rate(), 1152);
        assert_eq!(one.timeline_bit_rate(100.0), 768);
        assert_eq!(two.bit_rate(), 0);
        assert_eq!(two.timeline_bit_rate(0.0), 0);
    }

    proptest! {
        /// The totals hold their set relations for arbitrary clip lists: the
        /// all-angles size counts every clip, the main size only angle 0, and
        /// each angle's own size never exceeds its timeline's.
        #[test]
        fn playlist_totals_invariants(
            clips in proptest::collection::vec(
                (0_i32..3, 0_u8..4, 1.0_f64..100.0, 0_u64..1000), 0..8,
            ),
        ) {
            let clips: Vec<ClipSummary> = clips
                .into_iter()
                .map(|(angle, slot, length, packets)| {
                    clip_summary(angle, f64::from(slot) * 50.0, length, packets)
                })
                .collect();
            let playlist = playlist_summary(100.0, 2, clips);
            let total = playlist.total_packet_size();
            let total_angle = playlist.total_angle_packet_size();
            prop_assert!(total <= total_angle);
            let angles = playlist.angle_totals();
            prop_assert_eq!(angles.len(), 2);
            for angle in &angles {
                prop_assert!(angle.packet_size <= angle.timeline_packet_size);
                prop_assert!(angle.timeline_packet_size <= total_angle);
            }
        }
    }

    /// Builds a parsed playlist whose clip list is `clips` and whose angle
    /// count is `angle_count`, with empty stream maps.
    fn playlist_with_clips(angle_count: i32, clips: Vec<TsStreamClip>) -> TsPlaylistFile {
        let mut playlist = bare_playlist();
        playlist.angle_count = angle_count;
        playlist.stream_clips = clips;
        playlist
    }

    #[test]
    fn clear_measurements_zeroes_the_clip_tallies() {
        let playlist = playlist_with_clips(
            0,
            vec![TsStreamClip {
                name: "00000.M2TS".to_owned(),
                payload_bytes: 9,
                packet_count: 8,
                packet_seconds: 7.0,
                ..TsStreamClip::default()
            }],
        );
        let mut playlists = vec![playlist];
        clear_measurements(&mut playlists);
        let clip = playlists.first().unwrap().stream_clips.first().unwrap();
        assert_eq!(clip.payload_bytes, 0);
        assert_eq!(clip.packet_count, 0);
        assert_eq!(clip.packet_seconds.to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn resolve_playlist_streams_presents_reset_copies_and_angle_maps() {
        // The reference clip declares a video and an audio stream; the playlist
        // has two extra angles.
        let mut clip_streams = BTreeMap::new();
        let mut video = TsVideoStream::default();
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::AvcVideo;
        clip_streams.insert(0x1011, TsStream::Video(video));
        let mut audio = TsAudioStream::default();
        audio.base.pid = Pid::new(0x1100);
        audio.base.stream_type = TsStreamType::Ac3Audio;
        audio.base.set_language_code("eng");
        clip_streams.insert(0x1100, TsStream::Audio(audio));

        // The quick-scanned file supplies codec detail to merge.
        let mut file = TsStreamFile::new("00000.m2ts");
        let mut scanned_video = TsVideoStream::default();
        scanned_video.base.pid = Pid::new(0x1011);
        scanned_video.base.stream_type = TsStreamType::AvcVideo;
        scanned_video.base.is_vbr = true;
        scanned_video.base.payload_bytes = 999; // demux counter, must not leak
        scanned_video.encoding_profile = Some("High Profile 4.1".to_owned());
        file.streams.insert(0x1011, TsStream::Video(scanned_video));

        let metas = [ClipMeta {
            clip_streams: &clip_streams,
            scanned: Some(&file),
            has_50hz_video: false,
            relative_length: 1.0,
            length: 10.0,
            stream_file_present: true,
        }];
        let mut playlist = playlist_with_clips(2, Vec::new());
        resolve_playlist_streams(&mut playlist, &metas);

        // The presented streams: reset copies with the merged detail.
        assert_eq!(playlist.streams.len(), 2);
        let video = playlist.streams.get(&0x1011).unwrap();
        assert!(video.base().is_vbr);
        assert_eq!(video.base().payload_bytes, 0);
        let v = as_video(video).expect("video stays video");
        assert_eq!(v.encoding_profile.as_deref(), Some("High Profile 4.1"));
        assert_eq!(
            playlist.streams.get(&0x1100).unwrap().base().language_name.as_deref(),
            Some("English")
        );

        // Two angle maps, video-only, with the angle index stamped.
        assert_eq!(playlist.angle_streams.len(), 2);
        for (index, angle) in playlist.angle_streams.iter().enumerate() {
            assert_eq!(angle.len(), 1);
            let clone = angle.get(&0x1011).unwrap();
            assert_eq!(clone.base().angle_index, i32::try_from(index).unwrap().wrapping_add(1));
            assert!(clone.base().is_vbr);
        }

        // No reference clip → the maps stay empty.
        let mut unresolved = playlist_with_clips(1, Vec::new());
        resolve_playlist_streams(&mut unresolved, &[]);
        assert!(unresolved.streams.is_empty());
        assert!(unresolved.angle_streams.is_empty());
    }

    #[test]
    fn resolve_playlist_streams_presents_the_interleaved_dependent_view() {
        let mut clip_streams = BTreeMap::new();
        let mut video = TsVideoStream::default();
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::AvcVideo;
        clip_streams.insert(0x1011, TsStream::Video(video));

        // The scanned file carries the MVC dependent view the clip info omits.
        let mvc = |pid: u16| {
            let mut stream = TsVideoStream::default();
            stream.base.pid = Pid::new(pid);
            stream.base.stream_type = TsStreamType::MvcVideo;
            TsStream::Video(stream)
        };
        let interleaved =
            || TsInterleavedFile::new(Box::new(MemBdFile::new("00000.ssif", Vec::new(), false)));
        let mut file = TsStreamFile::new("00000.m2ts");
        file.interleaved_file = Some(interleaved());
        file.streams.insert(MVC_PID, mvc(MVC_PID));

        let mut playlist = playlist_with_clips(0, Vec::new());
        let metas = [ClipMeta {
            clip_streams: &clip_streams,
            scanned: Some(&file),
            has_50hz_video: false,
            relative_length: 1.0,
            length: 10.0,
            stream_file_present: true,
        }];
        resolve_playlist_streams(&mut playlist, &metas);
        assert!(playlist.streams.contains_key(&MVC_PID), "dependent view presented");
        assert_eq!(playlist.streams.len(), 2);

        // Without the interleaved source the dependent view stays absent.
        let mut plain = TsStreamFile::new("00000.m2ts");
        plain.streams.insert(MVC_PID, mvc(MVC_PID));
        let mut playlist = playlist_with_clips(0, Vec::new());
        let metas = [ClipMeta {
            clip_streams: &clip_streams,
            scanned: Some(&plain),
            has_50hz_video: false,
            relative_length: 1.0,
            length: 10.0,
            stream_file_present: true,
        }];
        resolve_playlist_streams(&mut playlist, &metas);
        assert!(!playlist.streams.contains_key(&MVC_PID));

        // A clip-info-declared stream on the MVC PID is not overwritten.
        let mut declared = clip_streams.clone();
        let mut own = TsAudioStream::default();
        own.base.pid = Pid::new(MVC_PID);
        own.base.stream_type = TsStreamType::Ac3Audio;
        declared.insert(MVC_PID, TsStream::Audio(own));
        let mut file = TsStreamFile::new("00000.m2ts");
        file.interleaved_file = Some(interleaved());
        file.streams.insert(MVC_PID, mvc(MVC_PID));
        let metas = [ClipMeta {
            clip_streams: &declared,
            scanned: Some(&file),
            has_50hz_video: false,
            relative_length: 1.0,
            length: 10.0,
            stream_file_present: true,
        }];
        let mut playlist = playlist_with_clips(0, Vec::new());
        resolve_playlist_streams(&mut playlist, &metas);
        assert_eq!(playlist.streams.get(&MVC_PID).unwrap().stream_type(), TsStreamType::Ac3Audio);
    }

    // ── orchestration: the end-to-end measurement scan ──────────────────────

    /// A PES elementary payload opening with an AVC sequence parameter set
    /// (High Profile 4.1), padded to 100 bytes — enough for the AVC scanner to
    /// initialise the stream (and mark it VBR) during the quick pass.
    fn sps_payload() -> Vec<u8> {
        let mut data = vec![0x00, 0x00, 0x01, 0x67, 100, 0x00, 41];
        data.resize(100, 0xAA);
        data
    }

    #[test]
    fn open_scanned_measures_clips_streams_and_totals() {
        let clip =
            clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0]), (0x1100, 0x81, [0x61, b'e', b'n', b'g'])]);
        let play = mpls("00000", 0, 4_500_000, &[]); // 100 s
        let mut m2ts = packet(0, true, &pat_payload(0x0100));
        // The PMT also announces a graphics stream the clip info does not
        // declare: the demux registers it, but the playlist never presents it,
        // so it stays out of the per-clip tallies.
        m2ts.extend(packet(
            0x0100,
            true,
            &pmt_payload(&[(0x1B, 0x1011), (0x81, 0x1100), (0x90, 0x1200)]),
        ));
        let audio_data = [0xBB_u8; 100];
        m2ts.extend(packet(0x1100, true, &pes_pts(0xC0, 90_000, &audio_data)));
        m2ts.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &sps_payload())));
        m2ts.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &sps_payload())));
        m2ts.extend(packet(0x1011, true, &pes_dts(0xE0, 270_000, 270_000, &sps_payload())));
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", play),
                ("BDMV/CLIPINF/00000.clpi", clip),
                ("BDMV/STREAM/00000.m2ts", m2ts),
            ],
        );
        let bd = disc.open_scanned().expect("measured scan");
        let pl = bd.playlists.first().unwrap();

        // The video row carries the measured VBR and active rates: 200 payload
        // bytes were window-flushed over 4.0 clip seconds (VBR) and 2.0 stream
        // seconds (active); the junk AC3 never decoded a nominal rate.
        let video = pl.streams.first().unwrap();
        assert_eq!(video.bitrate, 400);
        assert_eq!(video.active_bitrate, 800);
        assert!(video.description.contains("High Profile 4.1"));
        assert_eq!(pl.streams.get(1).unwrap().bitrate, 0);

        // The per-clip measurements: every packet the demux attributed (PAT +
        // PMT + 1 audio + 3 video), the flushed payload, and the demuxed
        // whole-file seconds.
        assert_eq!(pl.clips.len(), 1);
        let clip = pl.clips.first().unwrap();
        assert_eq!(clip.name, "00000.M2TS");
        assert_eq!(clip.display_name, "00000.M2TS");
        assert_eq!(clip.angle_index, 0);
        assert_eq!(clip.relative_time_in.to_bits(), 0.0_f64.to_bits());
        assert_eq!(clip.length.to_bits(), 100.0_f64.to_bits());
        assert_eq!(clip.file_seconds.to_bits(), 1.0_f64.to_bits());
        assert_eq!(clip.payload_bytes, 300);
        assert_eq!(clip.packet_count, 6);
        assert_eq!(clip.packet_seconds.to_bits(), 4.0_f64.to_bits());
        assert_eq!(clip.packet_size(), 1152);
        assert_eq!(clip.packet_bit_rate(), 2304);

        // The whole-file per-stream tallies, PID-ordered.
        assert_eq!(clip.streams.len(), 2);
        let video_tally = clip.streams.first().unwrap();
        assert_eq!(video_tally.pid, Pid::new(0x1011));
        assert_eq!((video_tally.payload_bytes, video_tally.packet_count), (200, 3));
        let audio_tally = clip.streams.get(1).unwrap();
        assert_eq!(audio_tally.pid, Pid::new(0x1100));
        assert_eq!((audio_tally.payload_bytes, audio_tally.packet_count), (100, 1));

        // The playlist totals derive from the packet tallies.
        assert_eq!(pl.angle_count, 0);
        assert_eq!(pl.total_packet_size(), 1152);
        assert_eq!(pl.total_angle_packet_size(), 1152);
        assert_eq!(pl.total_bit_rate(), 92); // 1152 × 8 / 100 s
        assert!(pl.angle_totals().is_empty());
    }

    #[test]
    fn clip_summaries_skip_a_registration_order_entry_without_a_stream() {
        // The two registration fields are public: a caller can desync them, so
        // an order entry whose stream is gone is skipped, not trusted.
        let mut audio = TsAudioStream::default();
        audio.base.pid = Pid::new(0x1100);
        audio.base.stream_type = TsStreamType::Ac3Audio;
        audio.base.payload_bytes = 64;
        audio.base.packet_count = 2;
        let mut file = TsStreamFile::new("00000.m2ts");
        file.stream_order = vec![0x1011, 0x1100];
        file.streams = BTreeMap::from([(0x1100, TsStream::Audio(audio))]);
        let playlist = TsPlaylistFile {
            file_type: "MPLS0300".to_owned(),
            name: "00000.MPLS".to_owned(),
            mvc_base_view_r: false,
            chapters: Vec::new(),
            playlist_streams: BTreeMap::new(),
            streams: BTreeMap::from([(0x1100, TsStream::Audio(TsAudioStream::default()))]),
            angle_streams: Vec::new(),
            stream_clips: vec![TsStreamClip {
                name: "00000.M2TS".to_owned(),
                ..TsStreamClip::default()
            }],
            angle_count: 0,
        };
        let measured = BTreeMap::from([("00000.M2TS".to_owned(), file)]);
        let clips = build_clip_summaries(&playlist, &measured);
        let tallies = &clips.first().unwrap().streams;
        assert_eq!(tallies.len(), 1);
        assert_eq!(tallies.first().unwrap().pid, Pid::new(0x1100));
        assert_eq!(tallies.first().unwrap().codec_short_name, "AC3");
        assert_eq!(tallies.first().unwrap().stream_type, TsStreamType::Ac3Audio);
    }

    #[test]
    fn open_scanned_reads_the_interleaved_source_and_presents_the_dependent_view() {
        // The plain m2ts is a stub; the interleaved .ssif holds the real
        // packets, including the MVC dependent view the clip info omits.
        let clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let play = mpls("00000", 0, 4_500_000, &[]);
        let mut ssif = packet(0, true, &pat_payload(0x0100));
        ssif.extend(packet(0x0100, true, &pmt_payload(&[(0x1B, 0x1011), (0x20, 0x1012)])));
        ssif.extend(packet(0x1011, true, &pes_dts(0xE0, 90_000, 90_000, &sps_payload())));
        ssif.extend(packet(0x1012, true, &pes_pts(0xE1, 90_000, &[0xCC; 50])));
        ssif.extend(packet(0x1011, true, &pes_dts(0xE0, 180_000, 180_000, &sps_payload())));
        ssif.extend(packet(0x1011, true, &pes_dts(0xE0, 270_000, 270_000, &sps_payload())));
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", play),
                ("BDMV/CLIPINF/00000.clpi", clip),
                ("BDMV/STREAM/00000.m2ts", vec![0_u8; 192]),
                ("BDMV/STREAM/SSIF/00000.ssif", ssif),
            ],
        );
        let bd = disc.open_scanned().expect("interleaved scan");
        let pl = bd.playlists.first().unwrap();

        // The dependent view the clip info omits is presented as an SSIF-only
        // row with its demuxed rate — never hidden, and excluded from the
        // presented .
        assert_eq!(pl.streams.len(), 2);
        assert_eq!(pl.stream_count, 1);
        let mvc_row = pl.streams.iter().find(|s| s.pid == Pid::new(0x1012)).unwrap();
        assert_eq!(mvc_row.stream_type.name(), "MVC_VIDEO");
        assert_eq!(mvc_row.codec_name, "MPEG-4 MVC Video");
        assert_eq!(mvc_row.description, "");
        assert!(mvc_row.ssif_only && !mvc_row.is_hidden);
        assert!(mvc_row.bitrate > 0, "the dependent view carries its measured rate");
        let clip = pl.clips.first().unwrap();
        assert_eq!(clip.name, "00000.M2TS");
        assert_eq!(clip.display_name, "00000.SSIF");
        assert!(clip.streams.iter().any(|tally| tally.pid == Pid::new(0x1012)));
        // The base view was demuxed from the interleaved source.
        let base = clip.streams.iter().find(|t| t.pid == Pid::new(0x1011)).unwrap();
        assert_eq!((base.payload_bytes, base.packet_count), (200, 3));
    }

    // ── orchestration: error paths ──────────────────────────────────────────

    #[test]
    fn open_rejects_a_folder_without_bdmv() {
        let disc = TempDisc::build(&["random"], &[]);
        // `BdError` no longer derives `PartialEq` (its `Io` wraps `io::Error`); the
        // error paths assert on the failure's `Display` instead.
        assert_eq!(disc.open().unwrap_err().to_string(), "unable to locate BD structure");
    }

    #[test]
    fn open_rejects_bdmv_missing_clipinf_or_playlist() {
        // PLAYLIST present, CLIPINF absent.
        let disc = TempDisc::build(&["BDMV/PLAYLIST"], &[]);
        assert_eq!(disc.open().unwrap_err().to_string(), "unable to locate BD structure");
    }

    // ── the disc-root self/ancestor walk ────────────────────────────────────

    /// Opens the disc with the scan rooted at `rel` under the fixture root —
    /// exercising [`walked_disc_root`] over the real filesystem backend.
    fn open_at(disc: &TempDisc, rel: &str) -> Result<BdRom, BdError> {
        BdRom::open(&FsDir::new(disc.root.join(rel)), false)
    }

    #[test]
    fn open_accepts_the_bdmv_directory_itself() {
        let disc = TempDisc::build(&["BDMV/PLAYLIST", "BDMV/CLIPINF"], &[]);
        let bd = open_at(&disc, "BDMV").expect("scan from the BDMV directory");
        // The walk resolved the real disc root, so the volume label is its name.
        assert!(bd.volume_label.starts_with("bdinfo-rs-disc-"));
    }

    #[test]
    fn open_accepts_a_directory_inside_bdmv() {
        let disc = TempDisc::build(&["BDMV/PLAYLIST", "BDMV/CLIPINF"], &[]);
        let bd = open_at(&disc, "BDMV/PLAYLIST").expect("scan from inside BDMV");
        assert!(bd.volume_label.starts_with("bdinfo-rs-disc-"));
    }

    #[test]
    fn open_scans_a_disc_root_itself_named_bdmv() {
        // The input is named BDMV but is NOT a scannable BDMV (no CLIPINF/
        // PLAYLIST children) — it is a disc root that happens to carry the name.
        // The validated walk skips it and the child lookup scans it as the root.
        let disc = TempDisc::build(&["BDMV/BDMV/PLAYLIST", "BDMV/BDMV/CLIPINF"], &[]);
        let bd = open_at(&disc, "BDMV").expect("scan a disc root named BDMV");
        assert_eq!(bd.volume_label, "BDMV");
    }

    #[test]
    fn a_stray_bdmv_ancestor_does_not_break_the_scan() {
        // A valid disc nested under a directory named BDMV: the stray ancestor
        // holds no CLIPINF/PLAYLIST, so the walk passes it by and the input is
        // scanned as the disc root (an unvalidated walk would commit to the
        // stray and abort).
        let disc = TempDisc::build(&["BDMV/disc/BDMV/PLAYLIST", "BDMV/disc/BDMV/CLIPINF"], &[]);
        let bd = open_at(&disc, "BDMV/disc").expect("scan under a stray BDMV ancestor");
        assert_eq!(bd.volume_label, "disc");
    }

    #[test]
    fn a_stray_bdmv_ancestor_with_only_clipinf_does_not_win() {
        let disc = TempDisc::build(
            &["BDMV/CLIPINF", "BDMV/disc/BDMV/PLAYLIST", "BDMV/disc/BDMV/CLIPINF"],
            &[],
        );
        let bd = open_at(&disc, "BDMV/disc").expect("scan under a CLIPINF-only stray BDMV");
        assert_eq!(bd.volume_label, "disc");
    }

    #[test]
    fn a_stray_bdmv_ancestor_with_only_playlist_does_not_win() {
        let disc = TempDisc::build(
            &["BDMV/PLAYLIST", "BDMV/disc/BDMV/PLAYLIST", "BDMV/disc/BDMV/CLIPINF"],
            &[],
        );
        let bd = open_at(&disc, "BDMV/disc").expect("scan under a PLAYLIST-only stray BDMV");
        assert_eq!(bd.volume_label, "disc");
    }

    /// A minimal [`BdDir`] fake for the walk's pathological arms: a fixed name,
    /// a parent that is `None` or a clone of itself (an endless cycle), and a
    /// child listing that is empty or fails.
    #[derive(Clone)]
    struct WalkDir {
        name: &'static str,
        cyclic: bool,
        fail_listing: bool,
    }

    impl BdDir for WalkDir {
        fn name(&self) -> &str {
            self.name
        }

        fn full_name(&self) -> &str {
            self.name
        }

        fn parent(&self) -> Option<Box<dyn BdDir>> {
            if self.cyclic {
                let cycle: Box<dyn BdDir> = Box::new(self.clone());
                Some(cycle)
            } else {
                None
            }
        }

        fn get_files(&self) -> io::Result<Vec<Box<dyn BdFile>>> {
            Ok(Vec::new())
        }

        fn get_files_pattern(&self, _pattern: &str) -> io::Result<Vec<Box<dyn BdFile>>> {
            Ok(Vec::new())
        }

        fn get_files_pattern_option(
            &self,
            _pattern: &str,
            _option: SearchOption,
        ) -> io::Result<Vec<Box<dyn BdFile>>> {
            Ok(Vec::new())
        }

        fn get_directories(&self) -> io::Result<Vec<Box<dyn BdDir>>> {
            if self.fail_listing { Err(io::Error::other("listing failed")) } else { Ok(Vec::new()) }
        }
    }

    #[test]
    fn walked_disc_root_survives_a_cyclic_parent_chain() {
        // A BDMV-named dir whose parent chain cycles forever: the bounded walk
        // gives up instead of spinning, and the scan then fails cleanly on the
        // (empty) child lookup.
        let dir = WalkDir { name: "BDMV", cyclic: true, fail_listing: false };
        assert!(walked_disc_root(&dir).is_none());
        assert_eq!(
            BdRom::open(&dir, false).unwrap_err().to_string(),
            "unable to locate BD structure"
        );
    }

    #[test]
    fn walk_dir_fake_satisfies_the_full_seam() {
        // Exercise the fake's remaining trait surface so coverage sees it.
        let dir = WalkDir { name: "BDMV", cyclic: false, fail_listing: false };
        assert_eq!(dir.full_name(), "BDMV");
        assert!(dir.get_files().expect("files").is_empty());
        assert!(dir.get_files_pattern("*").expect("pattern files").is_empty());
        assert!(
            dir.get_files_pattern_option("*", SearchOption::TopDirectoryOnly)
                .expect("optioned files")
                .is_empty()
        );
    }

    #[test]
    fn walked_disc_root_treats_an_unreadable_candidate_as_not_scannable() {
        // The scannability probe is speculative: a BDMV-named input whose child
        // listing errors is skipped (the error is not propagated), and with no
        // parent the walk yields nothing.
        let dir = WalkDir { name: "BDMV", cyclic: false, fail_listing: true };
        assert!(walked_disc_root(&dir).is_none());
    }

    #[test]
    fn open_reports_a_playlist_referencing_a_missing_clip_file() {
        let play = mpls("00000", 0, 4_500_000, &[]);
        let disc = TempDisc::build(
            &["BDMV/CLIPINF"], // CLIPINF present but empty
            &[("BDMV/PLAYLIST/00000.mpls", play)],
        );
        assert_eq!(
            disc.open().unwrap_err().to_string(),
            "referenced missing clip file: 00000.CLPI"
        );
    }

    #[test]
    fn open_propagates_a_malformed_clip_file() {
        let disc = TempDisc::build(
            &["BDMV/PLAYLIST"],
            &[("BDMV/CLIPINF/00000.clpi", b"XXXX0100junk".to_vec())],
        );
        assert_eq!(disc.open().unwrap_err().to_string(), "unknown file type: XXXX0100");
    }

    #[test]
    fn open_propagates_a_malformed_playlist() {
        let disc = TempDisc::build(
            &["BDMV/CLIPINF"],
            &[("BDMV/PLAYLIST/00000.mpls", b"XXXXjunk".to_vec())],
        );
        assert_eq!(disc.open().unwrap_err().to_string(), "unknown file type: XXXXjunk");
    }

    #[test]
    fn read_file_surfaces_io_errors() {
        // Build a handle, delete the file, then read it: open_read fails → Io.
        let disc = TempDisc::build(&[], &[("gone.bin", vec![1, 2, 3])]);
        let file = FsFile::from_full_name(disc.root.join("gone.bin")).expect("handle");
        std::fs::remove_file(disc.root.join("gone.bin")).expect("remove");
        assert!(read_file(&file).is_err()); // io error → Err (always BdError::Io)
    }

    // ── in-memory mock BD tree with injectable io failures ───────────────────
    //
    // Real temp folders can't make a *valid* directory's enumeration or a present
    // file's read fail at a chosen point, so the io-error `?` arms inside `open`
    // and its helpers are exercised with this mock: every IO operation ticks a
    // shared counter and the (configurable) Nth one returns an error.

    /// A shared "fail the Nth io operation" trip-wire.
    #[derive(Clone)]
    struct Trip {
        count: Arc<AtomicUsize>,
        fail_at: usize,
    }

    impl Trip {
        fn new(fail_at: usize) -> Self {
            Self { count: Arc::new(AtomicUsize::new(0)), fail_at }
        }

        fn tick(&self) -> io::Result<()> {
            if self.count.fetch_add(1, Ordering::Relaxed) == self.fail_at {
                Err(io::Error::other("injected io failure"))
            } else {
                Ok(())
            }
        }

        fn used(&self) -> usize {
            self.count.load(Ordering::Relaxed)
        }
    }

    #[derive(Clone)]
    struct MockFile {
        name: String,
        extension: String,
        bytes: Vec<u8>,
        trip: Trip,
        /// When set, `open_read` succeeds but the returned reader errors — to
        /// exercise `read_file`'s `read_to_end` (not `open_read`) failure arm.
        fail_read: bool,
    }

    impl BdFile for MockFile {
        fn name(&self) -> &str {
            &self.name
        }

        fn full_name(&self) -> &str {
            &self.name
        }

        fn extension(&self) -> &str {
            &self.extension
        }

        fn length(&self) -> u64 {
            u64::try_from(self.bytes.len()).unwrap()
        }

        fn is_dir(&self) -> bool {
            false
        }

        fn open_read(&self) -> io::Result<Box<dyn ReadSeek>> {
            self.trip.tick()?;
            if self.fail_read {
                Ok(Box::new(FailingReader))
            } else {
                Ok(Box::new(Cursor::new(self.bytes.clone())))
            }
        }

        fn open_text(&self) -> io::Result<Box<dyn BufRead>> {
            // `open` reads metadata via `open_read`; `open_text` exists only to
            // satisfy the trait, so it never participates in the failure injection.
            Ok(Box::new(BufReader::new(Cursor::new(self.bytes.clone()))))
        }
    }

    /// A reader that opens fine but errors on the first read.
    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }

    impl Seek for FailingReader {
        fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    /// Recursively collects files named `pattern` (case-insensitive) under `dir`.
    fn collect_matching(dir: &MockDir, pattern: &str, out: &mut Vec<Box<dyn BdFile>>) {
        for file in &dir.files {
            if file.name.eq_ignore_ascii_case(pattern) {
                out.push(Box::new(file.clone()));
            }
        }
        for sub in &dir.dirs {
            collect_matching(sub, pattern, out);
        }
    }

    #[derive(Clone)]
    struct MockDir {
        name: String,
        dirs: Vec<Self>,
        files: Vec<MockFile>,
        trip: Trip,
    }

    impl BdDir for MockDir {
        fn name(&self) -> &str {
            &self.name
        }

        fn full_name(&self) -> &str {
            &self.name
        }

        fn parent(&self) -> Option<Box<dyn BdDir>> {
            None
        }

        fn get_files(&self) -> io::Result<Vec<Box<dyn BdFile>>> {
            self.trip.tick()?;
            Ok(self.files.iter().cloned().map(|f| -> Box<dyn BdFile> { Box::new(f) }).collect())
        }

        fn get_files_pattern(&self, _pattern: &str) -> io::Result<Vec<Box<dyn BdFile>>> {
            self.get_files()
        }

        fn get_files_pattern_option(
            &self,
            pattern: &str,
            _option: SearchOption,
        ) -> io::Result<Vec<Box<dyn BdFile>>> {
            self.trip.tick()?;
            let mut out: Vec<Box<dyn BdFile>> = Vec::new();
            collect_matching(self, pattern, &mut out);
            Ok(out)
        }

        fn get_directories(&self) -> io::Result<Vec<Box<dyn BdDir>>> {
            self.trip.tick()?;
            Ok(self.dirs.iter().cloned().map(|d| -> Box<dyn BdDir> { Box::new(d) }).collect())
        }
    }

    /// Builds a complete, valid mock disc (two playlists, all flag dirs) whose io
    /// operations all share `trip`.
    fn mock_disc(trip: &Trip) -> MockDir {
        let file = |name: &str, bytes: Vec<u8>| {
            let extension =
                name.rsplit_once('.').map_or_else(String::new, |(_, ext)| format!(".{ext}"));
            MockFile {
                name: name.to_owned(),
                extension,
                bytes,
                trip: trip.clone(),
                fail_read: false,
            }
        };
        let dir = |name: &str, dirs: Vec<MockDir>, files: Vec<MockFile>| MockDir {
            name: name.to_owned(),
            dirs,
            files,
            trip: trip.clone(),
        };

        let clip0 =
            clpi(&[(0x1011, 0x1B, [0x63, 0x30, 0, 0]), (0x1100, 0x81, [0x61, b'e', b'n', b'g'])]);
        let clip1 = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        dir(
            "disc",
            vec![
                dir(
                    "BDMV",
                    vec![
                        dir(
                            "CLIPINF",
                            vec![],
                            vec![file("00000.clpi", clip0), file("00001.clpi", clip1)],
                        ),
                        dir(
                            "PLAYLIST",
                            vec![],
                            vec![
                                file(
                                    "00000.mpls",
                                    mpls("00000", 2_700_000, 4_500_000, &[(1, 0, 2_700_000)]),
                                ),
                                file("00001.mpls", mpls("00001", 0, 4_500_000, &[])),
                            ],
                        ),
                        dir(
                            "STREAM",
                            vec![dir("SSIF", vec![], vec![file("00000.ssif", vec![0; 10])])],
                            vec![file("00000.m2ts", vec![0; 100]), file("00001.m2ts", vec![0; 50])],
                        ),
                        dir(
                            "META",
                            vec![dir(
                                "DL",
                                vec![],
                                // a non-matching sibling so the title search's
                                // name filter exercises both outcomes
                                vec![
                                    file("thumb.jpg", vec![0xFF]),
                                    file("bdmt_eng.xml", title_xml("Movie")),
                                ],
                            )],
                            vec![],
                        ),
                        dir("BDJO", vec![], vec![file("00000.bdjo", vec![b'x'])]),
                    ],
                    vec![file("index.bdmv", b"INDX0300".to_vec())],
                ),
                dir("SNP", vec![], vec![file("clip.mnv", vec![b'x'])]),
                dir("BDSVM", vec![], vec![]),
            ],
            vec![],
        )
    }

    #[test]
    fn open_propagates_io_errors_at_every_step() {
        // A clean scan establishes the IO-operation count. The packet scan is on, so
        // the count also covers the per-stream-file open in `scan_stream_files`.
        let probe = Trip::new(usize::MAX);
        let bd = BdRom::open(&mock_disc(&probe), true).expect("mock disc scans clean");
        assert_eq!(bd.playlists.len(), 2); // exercises the playlist sort comparator
        assert!(bd.is_uhd && bd.is_3d && bd.is_bd_plus && bd.is_bd_java && bd.is_psp);
        let total = probe.used();
        assert!(total > 12, "expected many io ops, got {total}");

        // Failing each successive IO op must surface as an error (never a panic),
        // walking every `?` propagation arm in `open` and its helpers.
        for fail_at in 0..total {
            let trip = Trip::new(fail_at);
            // Each injected io failure surfaces as an error (never a panic); the io
            // path only ever yields `BdError::Io`.
            assert!(BdRom::open(&mock_disc(&trip), true).is_err(), "io failure at op {fail_at}");
        }
    }

    #[test]
    fn scan_stream_files_propagates_a_reader_error() {
        // A STREAM dir with one `*.m2ts` whose reader fails after a successful open:
        // the packet scan's read error propagates out of `scan_stream_files`.
        let trip = Trip::new(usize::MAX);
        let m2ts = MockFile {
            name: "00000.m2ts".to_owned(),
            extension: ".m2ts".to_owned(),
            bytes: vec![0_u8; 200],
            trip: trip.clone(),
            fail_read: true,
        };
        let stream_dir =
            MockDir { name: "STREAM".to_owned(), dirs: Vec::new(), files: vec![m2ts], trip };
        // A non-empty playlist list so the scan proceeds past its empty-input guard.
        let mut playlists =
            vec![TsPlaylistFile::scan("00000.mpls", &mpls("00000", 0, 4_500_000, &[])).unwrap()];
        // Strict mode propagates the read failure…
        let mut noop = |_: ScanProgress<'_>| {};
        let mut progress = Progress { callback: &mut noop, done: 0, total: 200 };
        let strict = scan_stream_files(
            Some(&stream_dir),
            None,
            &mut playlists,
            None,
            &mut progress,
            &mut Sink { errors: None },
            false,
        );
        assert!(strict.is_err());
        // …resilient mode records it and skips the file; the failed file still
        // consumes its progress budget (the snap to the file boundary).
        let mut errors = Vec::new();
        let mut noop = |_: ScanProgress<'_>| {};
        let mut progress = Progress { callback: &mut noop, done: 0, total: 200 };
        let resilient = scan_stream_files(
            Some(&stream_dir),
            None,
            &mut playlists,
            None,
            &mut progress,
            &mut Sink { errors: Some(&mut errors) },
            false,
        )
        .expect("resilient scan continues");
        assert_eq!(progress.done, 200, "the failed file's bytes are snapped past");
        assert!(resilient.is_empty());
        assert_eq!(errors.len(), 1);
        let recorded = errors.first().expect("one recorded error");
        assert_eq!(recorded.stage, ScanStage::StreamFile);
        assert_eq!(recorded.file, "00000.m2ts");
    }

    // ── scan progress + selection ───────────────────────────────────────────

    #[test]
    fn scan_total_budgets_selected_sources_once_preferring_the_ssif() {
        let stream_files = BTreeMap::from([
            ("00000.M2TS".to_owned(), 1000_u64),
            ("00001.M2TS".to_owned(), 500_u64),
        ]);
        let interleaved_files = BTreeMap::from([("00000.SSIF".to_owned(), 4000_u64)]);
        // No selection: every file, its .ssif source preferred, once — the
        // reported budget is the full pass only.
        assert_eq!(scan_total(&stream_files, &interleaved_files, None), 4000 + 500);
        // A selection narrows the budget to the named files.
        let only_one = BTreeSet::from(["00001.M2TS".to_owned()]);
        assert_eq!(scan_total(&stream_files, &interleaved_files, Some(&only_one)), 500);
        // An empty selection budgets nothing.
        assert_eq!(scan_total(&stream_files, &interleaved_files, Some(&BTreeSet::new())), 0);
        // Without interleaved sources the file's own size counts.
        assert_eq!(scan_total(&stream_files, &BTreeMap::new(), None), 1500);
    }

    #[test]
    fn progress_advances_clamped_and_snaps_at_file_boundaries() {
        let mut events: Vec<(String, u64, u64)> = Vec::new();
        {
            let mut callback =
                |p: ScanProgress<'_>| events.push((p.file.to_owned(), p.done, p.total));
            let mut progress = Progress { callback: &mut callback, done: 0, total: 100 };
            progress.advance("A.M2TS", 30);
            // An over-read clamps at the total instead of overshooting.
            progress.advance("A.M2TS", 90);
            // A snap target below the current count never moves it backwards…
            progress.finish_file("A.M2TS", 50);
        }
        assert_eq!(
            events,
            [
                ("A.M2TS".to_owned(), 30, 100),
                ("A.M2TS".to_owned(), 100, 100),
                ("A.M2TS".to_owned(), 100, 100),
            ]
        );

        // …and a snap past a short read pulls the count up to the boundary.
        events.clear();
        {
            let mut callback =
                |p: ScanProgress<'_>| events.push((p.file.to_owned(), p.done, p.total));
            let mut progress = Progress { callback: &mut callback, done: 0, total: 100 };
            progress.advance("B.M2TS", 10);
            progress.finish_file("B.M2TS", 60);
        }
        assert_eq!(events, [("B.M2TS".to_owned(), 10, 100), ("B.M2TS".to_owned(), 60, 100)]);
    }

    #[test]
    fn open_with_reports_progress_and_narrows_the_scan_to_selected_files() {
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", mpls("00000", 2_700_000, 4_500_000, &[])),
                ("BDMV/CLIPINF/00000.clpi", clpi(&[(0x1011, 0x1B, [0x63, 0x30, 0, 0])])),
                ("BDMV/STREAM/00000.m2ts", vec![0_u8; 1000]),
                ("BDMV/PLAYLIST/00001.mpls", mpls("00001", 2_700_000, 4_500_000, &[])),
                ("BDMV/CLIPINF/00001.clpi", clpi(&[(0x1011, 0x1B, [0x63, 0x30, 0, 0])])),
                ("BDMV/STREAM/00001.m2ts", vec![0_u8; 500]),
            ],
        );
        let root = FsDir::new(disc.root.clone());

        // Full scan: both files — the count runs 0→total over the full pass
        // (the quick codec-init pass is neither budgeted nor reported).
        let mut events: Vec<(String, u64, u64)> = Vec::new();
        let mut collect = |p: ScanProgress<'_>| events.push((p.file.to_owned(), p.done, p.total));
        let bd = BdRom::open_with(&root, true, None, &mut collect).expect("scan with progress");
        assert_eq!(bd.playlists.len(), 2);
        assert!(!events.is_empty());
        assert!(events.iter().all(|(_, _, total)| *total == 1000 + 500));
        assert!(events.windows(2).all(|pair| {
            pair.first().is_none_or(|(_, a, _)| pair.get(1).is_none_or(|(_, b, _)| a <= b))
        }));
        assert_eq!(events.last().map(|(_, done, _)| *done), Some(1500));
        assert!(events.iter().any(|(file, _, _)| file == "00000.M2TS"));
        assert!(events.iter().any(|(file, _, _)| file == "00001.M2TS"));

        // A selection narrows the scan: only the named file is read (and
        // budgeted), the other playlist still summarises from metadata.
        let selected = BTreeSet::from(["00000.M2TS".to_owned()]);
        let mut events: Vec<(String, u64, u64)> = Vec::new();
        let mut collect = |p: ScanProgress<'_>| events.push((p.file.to_owned(), p.done, p.total));
        let bd = BdRom::open_with(&root, true, Some(&selected), &mut collect)
            .expect("scan the selection");
        assert_eq!(bd.playlists.len(), 2);
        assert!(events.iter().all(|(file, _, total)| file == "00000.M2TS" && *total == 1000));
        assert_eq!(events.last().map(|(_, done, _)| *done), Some(1000));

        // The resilient variant takes the same extras and records nothing on
        // healthy media.
        let mut last_total = 0;
        let mut observe = |p: ScanProgress<'_>| last_total = p.total;
        let report = BdRom::open_resilient_with(&root, true, Some(&selected), &mut observe)
            .expect("resilient scan with progress");
        assert!(report.errors.is_empty());
        assert_eq!(last_total, 1000);
        assert_eq!(report.bdrom, bd);

        // Without the packet scan the callback never fires.
        let mut fired = false;
        let mut observe = |_: ScanProgress<'_>| fired = true;
        drop(BdRom::open_with(&root, false, None, &mut observe).expect("metadata-only scan"));
        assert!(!fired);
    }

    // ── the resilient (collect-and-continue) scan ────────────────────────────

    #[test]
    fn open_resilient_equals_open_on_a_healthy_disc_with_no_errors() {
        // Resilience is additive: on healthy media the resilient scan returns the
        // exact same BdRom as the strict scan, with an empty error list.
        let clip = clpi(&[(0x1011, 0x1B, [0x63, 0x30, 0, 0])]);
        let play = mpls("00000", 2_700_000, 4_500_000, &[(1, 0, 2_700_000)]);
        let disc = TempDisc::build(
            &[],
            &[("BDMV/PLAYLIST/00000.mpls", play), ("BDMV/CLIPINF/00000.clpi", clip)],
        );
        let strict = disc.open().expect("strict scan");
        let report =
            BdRom::open_resilient(&FsDir::new(disc.root.clone()), false).expect("resilient scan");
        assert!(report.errors.is_empty());
        assert_eq!(report.bdrom, strict);
    }

    #[test]
    fn open_resilient_keeps_other_playlists_on_a_corrupt_clip_file() {
        // Playlist 00001 references the corrupt 00001.clpi; playlist 00000 is
        // healthy. The strict scan aborts everything; the resilient scan emits
        // 00000.MPLS and records both the clip-info parse failure and the playlist
        // that lost its clip.
        let good_clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", mpls("00000", 0, 4_500_000, &[])),
                ("BDMV/PLAYLIST/00001.mpls", mpls("00001", 0, 9_000_000, &[])),
                ("BDMV/CLIPINF/00000.clpi", good_clip),
                ("BDMV/CLIPINF/00001.clpi", b"XXXX0100junk".to_vec()),
            ],
        );
        assert!(disc.open().is_err(), "the strict scan aborts on the corrupt clip");

        let report = BdRom::open_resilient(&FsDir::new(disc.root.clone()), false)
            .expect("resilient scan continues");
        let names: Vec<&str> = report.bdrom.playlists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["00000.MPLS"]); // the readable rest is emitted
        assert_eq!(report.errors.len(), 2);
        let clip_err = report.errors.first().expect("clip error");
        assert_eq!(clip_err.stage, ScanStage::ClipInfo);
        assert_eq!(clip_err.file, "00001.clpi");
        assert_eq!(clip_err.reason.to_string(), "unknown file type: XXXX0100");
        let playlist_err = report.errors.get(1).expect("playlist error");
        assert_eq!(playlist_err.stage, ScanStage::Playlist);
        assert_eq!(playlist_err.file, "00001.MPLS");
        assert_eq!(playlist_err.reason.to_string(), "referenced missing clip file: 00001.CLPI");
    }

    #[test]
    fn open_resilient_keeps_other_playlists_on_a_corrupt_playlist() {
        let clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", mpls("00000", 0, 4_500_000, &[])),
                ("BDMV/PLAYLIST/00001.mpls", b"XXXXjunk".to_vec()),
                ("BDMV/CLIPINF/00000.clpi", clip),
            ],
        );
        assert!(disc.open().is_err(), "the strict scan aborts on the corrupt playlist");

        let report = BdRom::open_resilient(&FsDir::new(disc.root.clone()), false)
            .expect("resilient scan continues");
        let names: Vec<&str> = report.bdrom.playlists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["00000.MPLS"]);
        assert_eq!(report.errors.len(), 1);
        let err = report.errors.first().expect("playlist error");
        assert_eq!((err.stage, err.file.as_str()), (ScanStage::Playlist, "00001.mpls"));
        assert_eq!(err.reason.to_string(), "unknown file type: XXXXjunk");
    }

    #[test]
    fn open_resilient_defaults_the_flags_on_failed_metadata_reads() {
        // index.bdmv and bdmt_eng.xml open fine but fail to read: the resilient
        // scan records both and falls back to is_uhd=false / disc_title=None.
        let trip = Trip::new(usize::MAX);
        let broken = |name: &str| MockFile {
            name: name.to_owned(),
            extension: name.rsplit_once('.').map_or_else(String::new, |(_, e)| format!(".{e}")),
            bytes: vec![0],
            trip: trip.clone(),
            fail_read: true,
        };
        let dir = |name: &str, dirs: Vec<MockDir>, files: Vec<MockFile>| MockDir {
            name: name.to_owned(),
            dirs,
            files,
            trip: trip.clone(),
        };
        let root = dir(
            "disc",
            vec![dir(
                "BDMV",
                vec![
                    dir("CLIPINF", vec![], vec![]),
                    dir("PLAYLIST", vec![], vec![]),
                    dir("META", vec![], vec![broken("bdmt_eng.xml")]),
                ],
                vec![broken("index.bdmv")],
            )],
            vec![],
        );
        let report = BdRom::open_resilient(&root, false).expect("resilient scan continues");
        assert!(!report.bdrom.is_uhd);
        assert_eq!(report.bdrom.disc_title, None);
        assert_eq!(report.errors.len(), 2);
        let uhd = report.errors.first().expect("index error");
        assert_eq!((uhd.stage, uhd.file.as_str()), (ScanStage::Discovery, "index.bdmv"));
        let title = report.errors.get(1).expect("title error");
        assert_eq!((title.stage, title.file.as_str()), (ScanStage::Discovery, "META"));

        // The strict scan aborts on the first of the same failures.
        assert!(BdRom::open(&root, false).is_err());
    }

    #[test]
    fn open_resilient_records_an_io_failure_at_every_step() {
        // The resilient counterpart of `open_propagates_io_errors_at_every_step`:
        // failing each successive IO op either aborts (the fatal structure-locating
        // block) or completes with that failure recorded — never panics, never
        // silently swallows.
        let probe = Trip::new(usize::MAX);
        let clean = BdRom::open_resilient(&mock_disc(&probe), true).expect("mock disc scans");
        assert!(clean.errors.is_empty());
        let total = probe.used();

        let mut absorbed = 0_usize;
        for fail_at in 0..total {
            let trip = Trip::new(fail_at);
            // An `Err` is the fatal BDMV/CLIPINF/PLAYLIST block; everything else
            // must complete with the failure recorded.
            if let Ok(report) = BdRom::open_resilient(&mock_disc(&trip), true) {
                assert!(
                    !report.errors.is_empty(),
                    "io failure at op {fail_at} was swallowed without a record"
                );
                absorbed = absorbed.saturating_add(1);
            }
        }
        // Most ops are isolated (only the directory-locating block is fatal).
        assert!(absorbed > total.saturating_div(2), "absorbed {absorbed} of {total}");
    }

    #[test]
    fn read_file_surfaces_a_read_error_after_a_successful_open() {
        let unreadable = MockFile {
            name: "unreadable.bin".to_owned(),
            extension: ".bin".to_owned(),
            bytes: vec![1],
            trip: Trip::new(usize::MAX),
            fail_read: true,
        };
        assert!(read_file(&unreadable).is_err()); // open_read ok, read_to_end fails
        // Exercise the failing reader's seek.
        let mut reader = unreadable.open_read().expect("open");
        assert_eq!(reader.seek(SeekFrom::Start(0)).expect("seek"), 0);
    }

    #[test]
    fn mock_trait_surface_is_exercised() {
        // The mock's trait methods `open` never calls (`full_name`, `parent`,
        // `get_files_pattern`, `is_dir`, `open_text`) are touched here so the mock
        // itself stays fully covered.
        let trip = Trip::new(usize::MAX);
        let root = mock_disc(&trip);
        assert_eq!(root.full_name(), "disc");
        assert!(root.parent().is_none());
        assert_eq!(root.get_files_pattern("*").expect("pattern").len(), root.files.len());
        let file = root.get_directories().expect("dirs");
        assert!(!file.is_empty());
        let mock_file = MockFile {
            name: "x".to_owned(),
            extension: String::new(),
            bytes: vec![1],
            trip,
            fail_read: false,
        };
        assert_eq!(mock_file.full_name(), "x");
        assert!(!mock_file.is_dir());
        let mut text = String::new();
        mock_file.open_text().expect("open_text").read_to_string(&mut text).expect("read");
        assert_eq!(text, "\u{1}");
    }

    // ── FMTS clips ───────────────────────────────────────────────────────────

    #[test]
    fn clip_stem_strips_either_stream_extension() {
        assert_eq!(clip_stem("00000.M2TS"), "00000");
        assert_eq!(clip_stem("00000.FMTS"), "00000");
        assert_eq!(clip_stem("noext"), "noext");
    }

    #[test]
    fn open_scanned_resolves_an_fmts_clip_stream_file() {
        // A play item whose codec id is `FMTS` resolves a `*.FMTS` stream file
        // (not `*.M2TS`) while still pairing with its `*.CLPI` by stem, so an
        // FMTS disc scans instead of degrading to a missing-file warning.
        let clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let play = mpls_items_codec(*b"FMTS", &["00000"], 0, 4_500_000, &[], &[], &[]);
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", play),
                ("BDMV/CLIPINF/00000.clpi", clip),
                ("BDMV/STREAM/00000.fmts", vec![0; 200]),
            ],
        );
        let bd = disc.open_scanned().expect("fmts disc scans");
        let playlist = bd.playlists.first().expect("one playlist");
        let clip = playlist.clips.first().expect("one clip");
        assert_eq!(clip.name, "00000.FMTS"); // the clip resolved its `.fmts` file
        // The `.fmts` file was discovered as a stream file and folded into the size.
        assert_eq!(playlist.file_size, 200);
    }

    // ── BDMV/BACKUP recovery ─────────────────────────────────────────────────

    #[test]
    fn open_resilient_recovers_a_playlist_from_backup() {
        // The primary playlist is corrupt but `BDMV/BACKUP/PLAYLIST` carries an
        // intact copy: the resilient scan recovers the playlist and still notes
        // the bad primary. Strict open never reads BACKUP, so it aborts.
        let clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", b"XXXXjunk".to_vec()),
                ("BDMV/BACKUP/PLAYLIST/00000.mpls", mpls("00000", 0, 4_500_000, &[])),
                ("BDMV/CLIPINF/00000.clpi", clip),
            ],
        );
        assert!(disc.open().is_err(), "strict open ignores BACKUP");

        let report =
            BdRom::open_resilient(&FsDir::new(disc.root.clone()), false).expect("recovers");
        let names: Vec<&str> = report.bdrom.playlists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["00000.MPLS"]); // recovered from BACKUP
        assert_eq!(report.errors.len(), 1);
        let err = report.errors.first().expect("primary note");
        assert_eq!((err.stage, err.file.as_str()), (ScanStage::Playlist, "00000.mpls"));
        assert_eq!(err.reason.to_string(), "unknown file type: XXXXjunk");
    }

    #[test]
    fn open_resilient_recovers_a_clip_from_backup() {
        // The primary clip-info is corrupt but `BDMV/BACKUP/CLIPINF` is intact:
        // the clip recovers, so its playlist scans (no missing-clip failure) and
        // only the bad primary is noted.
        let good_clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", mpls("00000", 0, 4_500_000, &[])),
                ("BDMV/CLIPINF/00000.clpi", b"XXXX0100junk".to_vec()),
                ("BDMV/BACKUP/CLIPINF/00000.clpi", good_clip),
            ],
        );
        let report =
            BdRom::open_resilient(&FsDir::new(disc.root.clone()), false).expect("recovers");
        let names: Vec<&str> = report.bdrom.playlists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["00000.MPLS"]); // the clip recovered → playlist scans
        assert_eq!(report.errors.len(), 1);
        let err = report.errors.first().expect("clip note");
        assert_eq!((err.stage, err.file.as_str()), (ScanStage::ClipInfo, "00000.clpi"));
        assert_eq!(err.reason.to_string(), "unknown file type: XXXX0100");
    }

    #[test]
    fn open_resilient_recovers_the_index_from_backup() {
        // The primary `index.bdmv` is present but unreadable; the BACKUP copy
        // says UHD. The resilient scan recovers the UHD flag and notes the bad
        // primary. (A real on-disk file can't fail to read, so this uses the
        // io-injecting mock.)
        let trip = Trip::new(usize::MAX);
        let mk = |name: &str, bytes: Vec<u8>, fail_read: bool| MockFile {
            name: name.to_owned(),
            extension: name.rsplit_once('.').map_or_else(String::new, |(_, e)| format!(".{e}")),
            bytes,
            trip: trip.clone(),
            fail_read,
        };
        let dir = |name: &str, dirs: Vec<MockDir>, files: Vec<MockFile>| MockDir {
            name: name.to_owned(),
            dirs,
            files,
            trip: trip.clone(),
        };
        let root = dir(
            "disc",
            vec![dir(
                "BDMV",
                vec![
                    dir("CLIPINF", vec![], vec![]),
                    dir("PLAYLIST", vec![], vec![]),
                    dir("BACKUP", vec![], vec![mk("index.bdmv", b"INDX0300".to_vec(), false)]),
                ],
                vec![mk("index.bdmv", vec![0], true)], // primary unreadable
            )],
            vec![],
        );
        let report = BdRom::open_resilient(&root, false).expect("scans");
        assert!(report.bdrom.is_uhd, "UHD flag recovered from BACKUP");
        assert_eq!(report.errors.len(), 1);
        let err = report.errors.first().expect("index note");
        assert_eq!((err.stage, err.file.as_str()), (ScanStage::Discovery, "index.bdmv"));
    }

    #[test]
    fn open_resilient_does_not_recover_from_a_corrupt_backup() {
        // Both copies are corrupt: nothing recovers, and only the primary's
        // failure is recorded (the backup's is not double-counted).
        let clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", b"XXXXjunk".to_vec()),
                ("BDMV/BACKUP/PLAYLIST/00000.mpls", b"YYYYjunk".to_vec()),
                ("BDMV/CLIPINF/00000.clpi", clip),
            ],
        );
        let report = BdRom::open_resilient(&FsDir::new(disc.root.clone()), false).expect("scans");
        assert!(report.bdrom.playlists.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            report.errors.first().expect("primary note").reason.to_string(),
            "unknown file type: XXXXjunk"
        );
    }

    #[test]
    fn open_resilient_does_not_recover_when_backup_lacks_the_file() {
        // The BACKUP directory exists but holds a different file, so the corrupt
        // primary cannot be recovered.
        let clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let disc = TempDisc::build(
            &[],
            &[
                ("BDMV/PLAYLIST/00000.mpls", b"XXXXjunk".to_vec()),
                ("BDMV/BACKUP/PLAYLIST/00001.mpls", mpls("00001", 0, 4_500_000, &[])),
                ("BDMV/CLIPINF/00000.clpi", clip),
            ],
        );
        let report = BdRom::open_resilient(&FsDir::new(disc.root.clone()), false).expect("scans");
        assert!(report.bdrom.playlists.is_empty());
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn backup_subdir_files_lists_present_and_empties_absent_directories() {
        let ok = Trip::new(usize::MAX);
        let mk = |name: &str| MockFile {
            name: name.to_owned(),
            extension: ".mpls".to_owned(),
            bytes: vec![1],
            trip: ok.clone(),
            fail_read: false,
        };
        let playlist = MockDir {
            name: "PLAYLIST".to_owned(),
            dirs: Vec::new(),
            files: vec![mk("00000.mpls")],
            trip: ok.clone(),
        };
        let backup = MockDir {
            name: "BACKUP".to_owned(),
            dirs: vec![playlist],
            files: Vec::new(),
            trip: ok,
        };
        // A present subdirectory is listed keyed by upper-cased name…
        let pool = backup_subdir_files(&backup, BdmvDir::Playlist).expect("present");
        assert!(pool.contains_key("00000.MPLS"));
        // …an absent one yields an empty pool.
        assert!(backup_subdir_files(&backup, BdmvDir::ClipInf).expect("absent").is_empty());
    }

    #[test]
    fn collect_backups_lists_all_pools_and_propagates_every_listing_failure() {
        // A `BDMV` whose `BACKUP` (index.bdmv + PLAYLIST + CLIPINF) shares a trip
        // across its own listing ops, while `BDMV` itself never fails — so every
        // BACKUP-side listing failure can be injected one at a time, and none is
        // silently swallowed.
        let build = |bt: &Trip| {
            let mk = |name: &str| MockFile {
                name: name.to_owned(),
                extension: name.rsplit_once('.').map_or_else(String::new, |(_, e)| format!(".{e}")),
                bytes: vec![1],
                trip: bt.clone(),
                fail_read: false,
            };
            let sub = |name: &str, file: &str| MockDir {
                name: name.to_owned(),
                dirs: Vec::new(),
                files: vec![mk(file)],
                trip: bt.clone(),
            };
            let backup = MockDir {
                name: "BACKUP".to_owned(),
                dirs: vec![sub("PLAYLIST", "00000.mpls"), sub("CLIPINF", "00000.clpi")],
                files: vec![mk("index.bdmv")],
                trip: bt.clone(),
            };
            MockDir {
                name: "BDMV".to_owned(),
                dirs: vec![backup],
                files: Vec::new(),
                trip: Trip::new(usize::MAX), // locating BACKUP never fails
            }
        };
        // A clean probe collects all three pools and counts the BACKUP-side ops.
        let probe = Trip::new(usize::MAX);
        let (index, playlist, clipinf) = collect_backups(&build(&probe)).expect("clean probe");
        assert!(index.contains_key("INDEX.BDMV"));
        assert!(playlist.contains_key("00000.MPLS"));
        assert!(clipinf.contains_key("00000.CLPI"));
        let total = probe.used();
        assert!(total >= 5, "expected several BACKUP listing ops, got {total}");
        // Failing each successive BACKUP listing op surfaces as Err (none swallowed).
        for fail_at in 0..total {
            let bt = Trip::new(fail_at);
            assert!(collect_backups(&build(&bt)).is_err(), "listing failure at op {fail_at}");
        }
    }

    // ── index.bdmv sanity warning ────────────────────────────────────────────

    #[test]
    fn an_untagged_index_warns_in_resilient_and_is_tolerated_in_strict() {
        // A present `index.bdmv` lacking the `INDX` tag stays non-UHD (the
        // tolerance is kept), but the resilient scan surfaces it as a warning;
        // strict open tolerates it silently.
        let clip = clpi(&[(0x1011, 0x1B, [0x62, 0x30, 0, 0])]);
        let make = |index: &[u8]| {
            TempDisc::build(
                &[],
                &[
                    ("BDMV/PLAYLIST/00000.mpls", mpls("00000", 0, 4_500_000, &[])),
                    ("BDMV/CLIPINF/00000.clpi", clip.clone()),
                    ("BDMV/index.bdmv", index.to_vec()),
                ],
            )
        };

        // A long (>= 8 byte) garbage index: warned, version magic reported.
        let disc = make(b"XXXXjunk");
        let report = BdRom::open_resilient(&FsDir::new(disc.root.clone()), false).expect("scans");
        assert!(!report.bdrom.is_uhd);
        assert_eq!(report.errors.len(), 1);
        let err = report.errors.first().expect("index warning");
        assert_eq!((err.stage, err.file.as_str()), (ScanStage::Discovery, "index.bdmv"));
        assert_eq!(err.reason.to_string(), "unknown file type: XXXXjunk");
        // Strict open tolerates the same index silently — non-UHD, no error.
        assert!(!disc.open().expect("strict tolerates an untagged index").is_uhd);

        // A short (< 8 byte) garbage index: still warned, magic empty.
        let short = make(b"XX");
        let report = BdRom::open_resilient(&FsDir::new(short.root.clone()), false).expect("scans");
        assert!(!report.bdrom.is_uhd);
        assert_eq!(
            report.errors.first().expect("short index warning").reason.to_string(),
            "unknown file type: "
        );
    }
}
