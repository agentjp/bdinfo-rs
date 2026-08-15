//! The app's flow state machine.
//!
//! The window moves through real states: nothing picked ([`Stage::Idle`]) → a
//! fast structural scan in flight ([`Stage::Listing`]) → the playlist table with
//! a live selection ([`Stage::Listed`]) → a measured scan streaming progress on a
//! worker thread ([`Stage::Scanning`]) → the rendered report
//! ([`Stage::Reported`]), plus a fatal-pick error state ([`Stage::Failed`]).
//!
//! [`Flow`] models them as one opaque value with **pure** transition methods the
//! iced shell's `update` calls and **pure** accessor methods its `view` projects
//! — no widgets and no IO here. The one genuinely tricky part is concurrency: a
//! measured scan runs on a worker thread, and its progress / completion messages
//! arrive asynchronously. Each scan carries a **generation** id; a `Progress` /
//! `Finished` / `ScanFailed` whose generation no longer matches the in-flight
//! scan (because the user cancelled, or started another) is ignored, so no stray
//! late message can drive the UI into an inconsistent state. That invariant is
//! proptested.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bdinfo_rs_core::bdrom::disc::{BdRom, PlaylistSummary, ScanOptions};
use bdinfo_rs_core::bdrom::order::{PlaylistFilter, selection_order, selection_stream_files};
use bdinfo_rs_core::bdrom::shortfall::ShortStreamFile;
use bdinfo_rs_core::error::ScanError;
use bdinfo_rs_core::report::text::{self, RenderOptions};

use crate::live::LivePlaylist;
use crate::model::{
    self, HiddenPlaylists, PlaylistRow, SelectableRow, Sort, SortColumn, ViewSettings,
};
use crate::panes::{self, CodecRow, StreamFileRow};
use crate::progress::ProgressModel;
use crate::scan::{Input, Structural};
use crate::selection::Selection;

/// The coarse stage the window is in — the discriminant the iced `view` branches
/// on before reading the stage-specific accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Nothing picked yet — the opening prompt.
    Idle,
    /// A structural scan is running for the picked input.
    Listing,
    /// The playlist table is up; the user is choosing rows.
    Listed,
    /// A measured scan is running on the worker thread.
    Scanning,
    /// A measured scan finished; the report is shown.
    Reported,
    /// The pick / structural scan failed fatally — a short message, no disc.
    Failed,
}

/// The structural-scan payload shared by the [`Stage::Listed`], [`Stage::Scanning`],
/// and [`Stage::Reported`] states: the picked input, the scanned disc, the
/// filtered table rows, and the live row selection over them.
#[derive(Debug, Clone)]
struct Listing {
    /// The disc the table came from (folder or `.iso`).
    input: Input,
    /// The scanned disc — the disc-level facts (label / title / size / flags)
    /// and the parsed playlists. The measured scan resolves the selection's
    /// clip set + report order against `bdrom.playlists`, and the pre-scan
    /// "View Report" renders straight from this disc (zero, unmeasured
    /// bitrates). A finished measured scan swaps in its measured playlists.
    bdrom: BdRom,
    /// The detected-feature labels, in the core's fixed order.
    features: Vec<String>,
    /// The table's presentation settings — the persisted playlist filter (the
    /// rows derive under it unless `reveal` overrides it) and the display
    /// toggles the cells render with. Retained so a settings change re-derives
    /// the rows from `bdrom` without a rescan.
    view: ViewSettings,
    /// The transient "Show" reveal over `view`'s filter: the same filter with
    /// the rules that are withholding a playlist switched off, or `None` for
    /// the plain settings view. Session-only — it never reaches the persisted
    /// settings, and an explicit settings apply
    /// ([`set_view`](Self::set_view)) drops it.
    reveal: Option<PlaylistFilter>,
    /// The filtered table rows; the selection parallels this list.
    rows: Vec<PlaylistRow>,
    /// One checkbox flag per row.
    selection: Selection,
    /// The active (highlighted) row — the playlist whose clips and streams the
    /// master-detail panes show. Clamped into the row range (`0` when empty).
    active: usize,
    /// The active column sort, replacing the grouped presentation order —
    /// `None` until a header is clicked.
    sort: Option<Sort>,
    /// Whether any listed playlist hides a stream (the CLI's footer note).
    show_hidden: bool,
    /// Structural-scan failures (a corrupt playlist) — a non-blocking banner.
    warnings: Vec<String>,
    /// A failed measured scan returns here with its message — a banner over the
    /// still-usable table, so the user can re-select and retry.
    error: Option<String>,
    /// The rendered structural (zero-bitrate) report over the playlists a scan
    /// would cover now — `BDInfo`'s pre-scan "View Report", stored so the view
    /// borrows it per rebuild instead of re-rendering per frame. Kept fresh by
    /// [`refresh_report`](Self::refresh_report) at every mutation that could
    /// change it.
    structural_report: String,
}

impl Listing {
    /// Builds the listing from a successful structural scan: the rows filtered
    /// under `view` and an empty selection over them.
    fn build(input: Input, structural: Structural, view: ViewSettings) -> Self {
        let rows = model::playlist_rows(&structural.bdrom.playlists, &view.filter());
        let show_hidden = model::any_hidden(&rows);
        let selection = Selection::new(rows.len());
        let mut listing = Self {
            input,
            bdrom: structural.bdrom,
            features: structural.features,
            view,
            reveal: None,
            rows,
            selection,
            active: 0,
            sort: None,
            show_hidden,
            warnings: structural.warnings,
            error: None,
            structural_report: String::new(),
        };
        listing.refresh_report();
        listing
    }

    /// Re-renders the retained structural report
    /// ([`render_structural`](Self::render_structural)). Called
    /// **unconditionally** by every mutation that could change it — selection
    /// edits, sorting, a view change, the measured rebuild — mirroring the
    /// settled `set_view` rule: a "did anything change?" guard is a known
    /// equivalent-mutant trap, and re-rendering identical bytes on a click is
    /// the accepted cost.
    fn refresh_report(&mut self) {
        self.structural_report = self.render_structural();
    }

    /// The filter the table rows are derived under: the transient reveal when
    /// one is on, else the settings' own filter.
    fn active_filter(&self) -> PlaylistFilter {
        self.reveal.clone().unwrap_or_else(|| self.view.filter())
    }

    /// Re-derives the table rows from the retained disc under the active
    /// filter — no disk IO, mirroring `BDInfo`'s `LoadPlaylists()` re-run.
    /// The row set changed, so the selection resets (as `BDInfo`'s does) and
    /// the active row clamps into the new range; the active column sort is
    /// re-applied over the new rows.
    fn rederive_rows(&mut self) {
        let mut rows = model::playlist_rows(&self.bdrom.playlists, &self.active_filter());
        if let Some(sort) = self.sort {
            let _ = model::sort_rows(&mut rows, sort);
        }
        self.selection = Selection::new(rows.len());
        self.active = self.active.min(rows.len().saturating_sub(1));
        self.show_hidden = model::any_hidden(&rows);
        self.rows = rows;
    }

    /// Whether a measured scan of this disc is worth running: it has at least
    /// one listed playlist, and its stream content is not AACS-encrypted.
    ///
    /// Encryption leaves only the first 16 bytes of each 6144-byte Aligned Unit
    /// in the clear (AACS Blu-ray Prerecorded Book §3.10), so a demux of the
    /// rest reads ciphertext and every value it measures is meaningless. The
    /// core scan runs on such a disc if asked — refusing is this application's
    /// policy, and the CLI applies the same one.
    const fn scannable(&self) -> bool {
        !self.rows.is_empty() && !self.bdrom.is_aacs_encrypted
    }

    /// The playlists the **settings'** filter withholds — what the
    /// hidden-count line reports whether or not the transient reveal is on
    /// (with it on, the line names what restoring the settings view would
    /// withhold again).
    fn hidden(&self) -> Option<HiddenPlaylists> {
        model::hidden_playlists(&self.bdrom.playlists, &self.view.filter())
    }

    /// Applies a changed view configuration (the Settings dialog's OK). A
    /// display-only change (sizes / chapter suffix) just re-renders; a
    /// **filter** change re-derives the rows ([`rederive_rows`](Self::rederive_rows)).
    /// An explicit settings apply is the global preference, so it also drops
    /// the transient reveal — which is itself a filter change when one was on.
    fn set_view(&mut self, view: ViewSettings) {
        let previous = self.active_filter();
        self.view = view;
        self.reveal = None;
        if previous != self.active_filter() {
            self.rederive_rows();
        }
        // Display-only changes alter the render options too, so the retained
        // report refreshes on EVERY view change, filtered or not.
        self.refresh_report();
    }

    /// Flips the transient "Show" reveal: on, the rows are re-derived under a
    /// filter with exactly the withholding rules switched off, so the withheld
    /// playlists join the table as ordinary rows; off, the settings' view
    /// returns. Both directions take [`rederive_rows`](Self::rederive_rows), so
    /// a reveal costs the selection exactly what a settings filter change does.
    ///
    /// A no-op when nothing is withheld and no reveal is on — there is then no
    /// row set to change.
    fn toggle_reveal(&mut self) {
        let next = if self.reveal.is_some() {
            None
        } else {
            self.hidden().map(|hidden| hidden.revealing(&self.view.filter()))
        };
        if next.is_none() && self.reveal.is_none() {
            return;
        }
        self.reveal = next;
        self.rederive_rows();
        self.refresh_report();
    }

    /// The checked rows' playlist names, in table order.
    fn selected_names(&self) -> Vec<String> {
        self.selection.selected_names(&self.rows)
    }

    /// Every listed playlist's name, in table order — the whole-disc scan set
    /// used when the user scans with nothing checked (`BDInfo`'s default).
    fn all_names(&self) -> Vec<String> {
        self.rows.iter().map(|row| row.name.clone()).collect()
    }

    /// The names to scan: the checked rows, or — when nothing is checked — every
    /// listed playlist (scan-all, like the CLI's `--whole`).
    fn scan_names(&self) -> Vec<String> {
        if self.selection.any() { self.selected_names() } else { self.all_names() }
    }

    /// Applies `BDInfo`'s header-click rule for `column`, then re-sorts the
    /// rows. Check-marks and the active-row highlight belong to PLAYLISTS, not
    /// row positions: the selection flags and `active` are permuted along with
    /// the rows, so the checked set (and thus [`scan_names`](Self::scan_names))
    /// is unchanged by any sorting.
    fn sort_by(&mut self, column: SortColumn) {
        let sort = Sort::click(self.sort, column, &self.rows);
        self.sort = Some(sort);
        let order = model::sort_rows(&mut self.rows, sort);
        self.selection.permute(&order);
        self.active = order.iter().position(|&old| old == self.active).unwrap_or(0);
        // The report renders the scan set in table order, so a re-sort
        // re-orders the retained report too.
        self.refresh_report();
    }

    /// Replaces the disc's playlists with the measured ones, rebuilding the rows
    /// so the table + panes show the measured sizes and bitrates. The measured
    /// scan re-opened the same disc with the same filter, so the same playlists
    /// come back; the rebuild starts from the grouped presentation order and
    /// re-applies the active column sort (measured values may have changed, so
    /// the order is recomputed, not reused). Check-marks and the highlight are
    /// re-attached by playlist NAME, so they survive any reordering. The
    /// disc-level facts are unchanged by measurement, so only `bdrom.playlists`
    /// is swapped.
    fn apply_measured(&mut self, playlists: Vec<PlaylistSummary>) {
        let checked: BTreeSet<String> = self.selected_names().into_iter().collect();
        let active_name = self.rows.get(self.active).map(|row| row.name.clone());
        let mut rows = model::playlist_rows(&playlists, &self.active_filter());
        if let Some(sort) = self.sort {
            let _ = model::sort_rows(&mut rows, sort);
        }
        self.selection =
            Selection::from_flags(rows.iter().map(|row| checked.contains(&row.name)).collect());
        self.active =
            active_name.and_then(|name| rows.iter().position(|row| row.name == name)).unwrap_or(0);
        self.show_hidden = model::any_hidden(&rows);
        self.bdrom.playlists = playlists;
        self.rows = rows;
        // The retained structural render must follow the measured disc — it
        // is what a LATER scan serves while in flight.
        self.refresh_report();
    }

    /// The active row's playlist, resolved through its `playlist_index`.
    fn active_playlist(&self) -> Option<&PlaylistSummary> {
        let row = self.rows.get(self.active)?;
        self.bdrom.playlists.get(row.playlist_index)
    }

    /// Renders the classic disc report straight from the scanned disc, over the
    /// playlists that would be scanned now — the checked rows, or every listed
    /// playlist when none are checked ([`scan_names`](Self::scan_names), the same
    /// set `BDInfo`'s report covers). Before the measured scan the disc's packet
    /// tallies are zero, so the sizes and bitrates render as `0` / `0.00` — the
    /// pre-scan "View Report". (Structural warnings surface in the UI's banner,
    /// so the report's `WARNING` block is left to the measured scan.)
    fn render_structural(&self) -> String {
        let order = selection_order(&self.bdrom.playlists, &self.scan_names());
        text::render_with(&self.bdrom, &order, &[], self.view.render_options())
    }

    /// Renders the measured report from the retained disc — the scanned
    /// playlists in their scan order (`scanned`, not the live selection, which
    /// the user may have changed since) and the scan's recorded failures,
    /// under the current render options. A finished scan swapped the measured
    /// playlists into `bdrom`, so this reproduces the worker's render — the
    /// road a report-section toggle re-renders on, with no rescan.
    fn render_measured(&self, scanned: &[String], errors: &[ScanError]) -> String {
        let order = selection_order(&self.bdrom.playlists, scanned);
        text::render_with(&self.bdrom, &order, errors, self.view.render_options())
    }
}

/// The scan-complete confirmation text for a scan that recorded `errors`
/// failures.
///
/// A clean success, or a note that the resilient scan still wrote the report
/// — the message the shell shows in its completion modal.
#[must_use]
pub fn scan_notice(errors: usize) -> String {
    if errors == 0 {
        "Scan completed successfully.".to_owned()
    } else {
        format!("Scan completed with {errors} error(s).\nThe report below was still written.")
    }
}

/// The notice for one stream file the demux measured shorter than the disc
/// declares — raised beside the report, because the locked report format has no
/// line for it ([`bdinfo_rs_core::bdrom::shortfall`]).
///
/// The wording is shared with the other surfaces' copies of this format
/// (the `bdinfo-rs` CLI's `short_stream_notice`, `bdinfo-rs-wasm`'s
/// `mirror::short_stream_notice`); each copy is pinned by an identical test, so
/// a change here reworks all three together or fails a sibling gate.
fn short_stream_notice(short: &ShortStreamFile) -> String {
    format!(
        "{} is shorter than declared: measured {:.1} s of {:.1} s ({:.1} s missing)",
        short.file(),
        short.measured_seconds(),
        short.declared_seconds(),
        short.missing_seconds()
    )
}

/// What a measured scan needs, read from the [`Flow`] before spawning the worker.
///
/// The input to re-open, the selected playlist names (the report order), and the
/// clip set the packet scan is narrowed to.
#[derive(Debug, Clone)]
pub struct ScanRequest {
    /// The disc to re-open for the measured scan.
    pub input: Input,
    /// The selected playlist names, in table order (the report order).
    pub selection: Vec<String>,
    /// The selected playlists' clips — the packet scan reads only these.
    pub scan_files: BTreeSet<String>,
    /// The report's section switches at scan start.
    pub options: RenderOptions,
    /// The scan's own behaviour switches at scan start.
    pub scan_options: ScanOptions,
}

/// The flow state — an opaque value the shell drives and projects.
///
/// The transition methods (called by `update`) advance it; the accessor methods
/// (read by `view`) project it. The heavy data is private so the (separate)
/// binary crate cannot bypass the transitions.
#[derive(Debug, Clone)]
pub struct Flow {
    inner: Inner,
}

/// The private state graph behind [`Flow`].
#[derive(Debug, Clone)]
enum Inner {
    Idle,
    /// Idle, but with the source remembered — shown in the Source field with
    /// "Rescan" live, opened only when the user asks. Reached from a boot
    /// that recalls last session's path, and from a cancelled listing (the
    /// input stays one click from a retry).
    Recalled(Input),
    Listing {
        input: Input,
        /// The structural scan's live progress — `None` until the quick codec
        /// pass's first event (the metadata open that precedes the pass
        /// reports nothing, and an AACS-encrypted disc never runs the pass).
        progress: Option<ProgressModel>,
        /// Whether progress events have gone quiet past the stall threshold —
        /// the same slot, threshold and wording the measured scan uses
        /// (`Scanning::stalled`). The listing reads the head of every stream
        /// file, so on damaged media it is the FIRST place a read sticks,
        /// minutes before any measured scan starts.
        stalled: bool,
    },
    Listed(Listing),
    Scanning {
        listing: Listing,
        generation: u64,
        progress: Option<ProgressModel>,
        /// The names being scanned, in table order — the report's selection
        /// order, carried into `Reported` for option-change re-renders.
        scanned: Vec<String>,
        /// Whether progress events have gone quiet past the stall threshold
        /// ([`crate::progress::stalled`]) — set by [`Flow::tick`], cleared by
        /// every applied progress event. The status line then reads "still
        /// reading" instead of implying normal progress.
        stalled: bool,
        /// The measured cells the scan has reported so far, by playlist name —
        /// what the table's `Measured Bytes` column and the two panes show
        /// while the scan runs ([`crate::live`]). Merged per playlist by
        /// [`Flow::measured`] and read only here: the rows, their order and the
        /// retained report never see it, and the finished scan's summaries
        /// replace it rather than adding to it. Empty until the first snapshot,
        /// so an unreported playlist keeps showing the summaries' zeros.
        live: BTreeMap<String, LivePlaylist>,
    },
    Reported {
        listing: Listing,
        report: String,
        errors: Vec<String>,
        /// The scanned playlist names — see `Scanning::scanned`.
        scanned: Vec<String>,
        /// The scan's typed failures — the report's `WARNING` block on an
        /// option-change re-render (shared: `ScanError` is not `Clone`).
        scan_errors: Arc<Vec<ScanError>>,
    },
    Failed(String),
}

impl Default for Flow {
    fn default() -> Self {
        Self::idle()
    }
}

impl Flow {
    /// The opening state — nothing picked.
    #[must_use]
    pub const fn idle() -> Self {
        Self { inner: Inner::Idle }
    }

    /// The opening state with last session's source remembered ([`Stage::Idle`],
    /// like [`Flow::idle`]): nothing is loaded and nothing scans, but the input
    /// shows in the Source field ([`Flow::input_display`]) and "Rescan" is live
    /// ([`Flow::current_input`]), so reopening it is one click — `BDInfo` seeds
    /// its source box from `LastPath` the same way, without scanning.
    #[must_use]
    pub const fn recall(input: Input) -> Self {
        Self { inner: Inner::Recalled(input) }
    }

    // ── transitions (update calls these) ────────────────────────────────────

    /// A folder/`.iso` was picked — begin its structural scan.
    #[must_use]
    pub const fn start_listing(input: Input) -> Self {
        Self { inner: Inner::Listing { input, progress: None, stalled: false } }
    }

    /// The structural scan for `input` finished. Applied only when this flow is
    /// still listing **that** input (a newer pick supersedes a stale result):
    /// success → the table, filtered and rendered under `view`; failure → the
    /// fatal error state.
    #[must_use]
    pub fn listed(
        self,
        input: &Input,
        result: Result<Structural, String>,
        view: ViewSettings,
    ) -> Self {
        match &self.inner {
            Inner::Listing { input: pending, .. } if pending == input => match result {
                Ok(structural) => {
                    Self { inner: Inner::Listed(Listing::build(input.clone(), structural, view)) }
                }
                Err(message) => Self { inner: Inner::Failed(message) },
            },
            // A superseded or unexpected result — keep the current state.
            _ => self,
        }
    }

    /// Applies a changed view configuration (the Settings dialog's OK): a
    /// filter change rebuilds the rows from the retained disc (the selection
    /// resets, the active row clamps); a display-only change re-renders the
    /// cells. On [`Stage::Reported`] the measured report is re-rendered from
    /// the retained disc too, so a report-section toggle takes effect with no
    /// rescan. Only while the table is editable ([`Stage::Listed`] /
    /// [`Stage::Reported`]) — the dialog cannot open mid-scan, and this
    /// enforces the same rule.
    pub fn set_view(&mut self, view: ViewSettings) {
        match &mut self.inner {
            Inner::Reported { listing, report, scanned, scan_errors, .. } => {
                listing.set_view(view);
                *report = listing.render_measured(scanned, scan_errors);
            }
            Inner::Listed(listing) => listing.set_view(view),
            _ => {}
        }
    }

    /// Flips the transient "Show" / "Hide" reveal on the hidden-count line:
    /// the withheld playlists join the table as ordinary rows (selectable and
    /// scannable), or leave it again. Session-only — the persisted settings
    /// are untouched, and the next settings apply drops the reveal
    /// ([`Flow::set_view`]).
    ///
    /// Only while the table is editable ([`Stage::Listed`] /
    /// [`Stage::Reported`]): the row set must not change under an in-flight
    /// scan, the same rule the selection edits follow.
    pub fn toggle_reveal(&mut self) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.toggle_reveal();
        }
    }

    /// A path arriving from outside the pickers (a drop or the boot argument)
    /// was not a disc folder or `.iso` — enter the same fatal state a failed
    /// pick reaches, carrying the classifier's message. A no-op while a scan
    /// is in flight ([`Stage::Listing`] / [`Stage::Scanning`]): the shell
    /// ignores drops while busy, and this transition enforces the same rule.
    #[must_use]
    pub fn open_failed(self, message: String) -> Self {
        match self.inner {
            Inner::Listing { .. } | Inner::Scanning { .. } => self,
            _ => Self { inner: Inner::Failed(message) },
        }
    }

    /// Toggles the checkbox at `index` (only while the table is editable —
    /// [`Stage::Listed`] / [`Stage::Reported`]). The scan set changed, so the
    /// retained pre-scan report refreshes with it.
    pub fn toggle(&mut self, index: usize) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.selection.toggle(index);
            listing.refresh_report();
        }
    }

    /// Sets the active (highlighted) row — the playlist whose clips and streams
    /// the master-detail panes show. A row click drives this; it is independent
    /// of the scan-selection checkboxes. An out-of-range index is ignored.
    pub const fn set_active(&mut self, index: usize) {
        if let Some(listing) = self.editable_listing_mut()
            && index < listing.rows.len()
        {
            listing.active = index;
        }
    }

    /// Sorts the playlist table by `column` — `BDInfo`'s header-click rule:
    /// the first click sorts ascending (descending when the rows already read
    /// ascending by that column, so a click always visibly re-orders), and a
    /// repeat click on the same column flips. Only while the table is editable
    /// ([`Stage::Listed`] / [`Stage::Reported`]); the check-marks and the
    /// active-row highlight travel with their playlists, so the scan set never
    /// changes.
    pub fn sort_by(&mut self, column: SortColumn) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.sort_by(column);
        }
    }

    /// Checks every row ("Select all"). Refreshes the retained pre-scan
    /// report like any selection edit.
    pub fn select_all(&mut self) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.selection.set_all();
            listing.refresh_report();
        }
    }

    /// Unchecks every row ("Select none"). Refreshes the retained pre-scan
    /// report like any selection edit.
    pub fn select_none(&mut self) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.selection.clear();
            listing.refresh_report();
        }
    }

    /// Begins a measured scan, tagged with `generation`. Applied from an editable
    /// state that has at least one listed playlist (the same gate as
    /// [`Flow::scan_request`]); the selection may be empty (scan-all). No-op
    /// otherwise.
    #[must_use]
    pub fn start_scanning(self, generation: u64) -> Self {
        match self.inner {
            Inner::Listed(mut listing) | Inner::Reported { mut listing, .. }
                if !listing.rows.is_empty() =>
            {
                listing.error = None;
                let scanned = listing.scan_names();
                Self {
                    inner: Inner::Scanning {
                        listing,
                        generation,
                        progress: None,
                        scanned,
                        stalled: false,
                        live: BTreeMap::new(),
                    },
                }
            }
            other => Self { inner: other },
        }
    }

    /// Records a progress event — the file currently demuxing, its `done`/`total`
    /// byte counts, and the wall time elapsed since the scan started. Applied only
    /// while scanning **and** when `generation` matches the in-flight scan (a
    /// stale event is dropped); the pure progress math is [`ProgressModel`].
    /// A real event is proof the read loop is moving, so it also clears the
    /// stall flag [`Flow::tick`] may have raised.
    pub fn progress(
        &mut self,
        generation: u64,
        file: String,
        done: u64,
        total: u64,
        elapsed: Duration,
    ) {
        if let Inner::Scanning { generation: active, progress, stalled, .. } = &mut self.inner
            && *active == generation
        {
            *progress = Some(ProgressModel::compute(file, done, total, elapsed));
            *stalled = false;
        }
    }

    /// Records the measured cells a scan in flight has reported.
    ///
    /// `playlists` is one entry per playlist the snapshot covered
    /// ([`crate::live::updates`]). Applied only while scanning **and** when
    /// `generation` matches the in-flight scan, like [`Flow::progress`].
    ///
    /// **Cells only.** Each entry replaces that playlist's previous one and
    /// nothing else moves: the row set, the sort order, the checked set, the
    /// active row and the retained report are all untouched, so the table the
    /// user is watching cannot reorder or rebuild under a running scan. A
    /// playlist no snapshot has covered yet keeps showing the (zero) summary
    /// values, and the scan's completion ([`Flow::finished`]) replaces every
    /// cell with the measured summaries.
    pub fn measured(&mut self, generation: u64, playlists: Vec<(String, LivePlaylist)>) {
        if let Inner::Scanning { generation: active, live, .. } = &mut self.inner
            && *active == generation
        {
            live.extend(playlists);
        }
    }

    /// Records a structural-scan progress event — the stream file the quick
    /// codec pass is reading, its `done`/`total` byte counts, and the wall
    /// time elapsed since the listing started. Applied only while listing;
    /// the shell keeps events from a superseded listing worker away from
    /// here (its listing messages carry a generation stamp), so this needs
    /// no generation of its own. Like the measured scan's [`Flow::progress`],
    /// a real event is proof the read loop is moving, so it also clears the
    /// stall flag [`Flow::tick`] may have raised.
    pub fn list_progress(&mut self, file: String, done: u64, total: u64, elapsed: Duration) {
        if let Inner::Listing { progress, stalled, .. } = &mut self.inner {
            *progress = Some(ProgressModel::compute(file, done, total, elapsed));
            *stalled = false;
        }
    }

    /// Advances the wall-clock readout while a scan is in flight — the shell's
    /// display tick. `elapsed` (since the scan started) re-stamps the progress
    /// model's elapsed seconds so the readout climbs even when no progress
    /// event arrives (a read stuck in drive-firmware retries emits none), and
    /// `since_progress` (since the last progress event) drives the stall flag
    /// (its fixed threshold lives in [`crate::progress`], beside the model's
    /// other display constants). The remaining estimate is untouched —
    /// it is meaningful only against real progress.
    ///
    /// Both in-flight stages take it, on identical terms: the measured scan
    /// ([`Stage::Scanning`]) and the structural listing ([`Stage::Listing`]),
    /// which reads every stream file's head and stalls on damaged media the
    /// same way. A no-op in every other stage, and before the first progress
    /// event only the stall flag moves (there is no model to re-stamp) — the
    /// listing's clock therefore runs from its start, since a cancelled or
    /// pre-cancelled listing can emit no event at all.
    pub fn tick(&mut self, elapsed: Duration, since_progress: Duration) {
        if let Inner::Scanning { progress, stalled, .. }
        | Inner::Listing { progress, stalled, .. } = &mut self.inner
        {
            if let Some(model) = progress.as_mut() {
                model.set_elapsed(elapsed);
            }
            *stalled = crate::progress::stalled(since_progress);
        }
    }

    /// The measured scan finished — applied only when `generation` matches the
    /// in-flight scan; moves [`Stage::Scanning`] → [`Stage::Reported`]. The disc
    /// label comes from the (unchanged) loaded disc, so it is not threaded here;
    /// the scan's typed `errors` render the banner lines and are retained for
    /// option-change re-renders.
    #[must_use]
    pub fn finished(
        self,
        generation: u64,
        report: String,
        scan_errors: Arc<Vec<ScanError>>,
        playlists: Vec<PlaylistSummary>,
    ) -> Self {
        match self.inner {
            Inner::Scanning { mut listing, generation: active, scanned, .. }
                if active == generation =>
            {
                listing.apply_measured(playlists);
                let errors = scan_errors.iter().map(ToString::to_string).collect();
                Self { inner: Inner::Reported { listing, report, errors, scanned, scan_errors } }
            }
            other => Self { inner: other },
        }
    }

    /// The measured scan failed — applied only when `generation` matches; returns
    /// to [`Stage::Listed`] with the message as a banner over the still-usable
    /// table, so the user can retry (mirroring the CLI's resilient posture).
    #[must_use]
    pub fn scan_failed(self, generation: u64, message: String) -> Self {
        match self.inner {
            Inner::Scanning { mut listing, generation: active, .. } if active == generation => {
                listing.error = Some(message);
                Self { inner: Inner::Listed(listing) }
            }
            other => Self { inner: other },
        }
    }

    /// Cancels the in-flight scan. A measured scan returns to the table; a
    /// structural scan (the listing) returns to the source prompt with the
    /// input remembered, so a retry is one "Rescan" click. The shell trips
    /// the worker's cooperative stop flag alongside this transition, so the
    /// read loop aborts within moments — and this transition never waits for
    /// it: a cancelled listing can produce no further event at all (the
    /// cancel flag is polled before each read), so teardown is driven from
    /// here, with the worker's late result message ignored — a Listed or
    /// Recalled flow is no longer scanning or listing, so `scan_failed` /
    /// `listed` (and any straggling progress) drop it. Starting a new scan
    /// immediately is safe: it gets a fresh generation and its own cancel
    /// flag, and the old worker only ever reads the disc.
    #[must_use]
    pub fn cancel(self) -> Self {
        match self.inner {
            Inner::Scanning { listing, .. } => Self { inner: Inner::Listed(listing) },
            Inner::Listing { input, .. } => Self { inner: Inner::Recalled(input) },
            other => Self { inner: other },
        }
    }

    // ── accessors (view projects these) ─────────────────────────────────────

    /// The coarse stage, for the view's top-level branch.
    #[must_use]
    pub const fn stage(&self) -> Stage {
        match &self.inner {
            Inner::Idle | Inner::Recalled(_) => Stage::Idle,
            Inner::Listing { .. } => Stage::Listing,
            Inner::Listed(_) => Stage::Listed,
            Inner::Scanning { .. } => Stage::Scanning,
            Inner::Reported { .. } => Stage::Reported,
            Inner::Failed(_) => Stage::Failed,
        }
    }

    /// The picked input's path as a display string (for the "Scanning …" line),
    /// when a disc is involved.
    #[must_use]
    pub fn input_display(&self) -> Option<String> {
        self.any_listing().map(|listing| listing.input.display()).or_else(|| match &self.inner {
            Inner::Listing { input, .. } | Inner::Recalled(input) => Some(input.display()),
            _ => None,
        })
    }

    /// The disc currently loaded or being listed (folder or `.iso`), if any —
    /// the source the toolbar shows and "Rescan" re-lists. A pure read; it never
    /// drives a transition.
    #[must_use]
    pub fn current_input(&self) -> Option<&Input> {
        match &self.inner {
            Inner::Listing { input, .. } | Inner::Recalled(input) => Some(input),
            _ => self.any_listing().map(|listing| &listing.input),
        }
    }

    /// The disc label, when a disc is loaded.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.any_listing().map(|listing| listing.bdrom.volume_label.as_str())
    }

    /// The disc title (`META/bdmt_eng.xml`), when a disc is loaded and it has one
    /// — the info box's `Disc Title` line.
    #[must_use]
    pub fn disc_title(&self) -> Option<&str> {
        self.any_listing().and_then(|listing| listing.bdrom.disc_title.as_deref())
    }

    /// The total disc size in bytes, when a disc is loaded — the info box's
    /// `Disc Size` line.
    #[must_use]
    pub fn disc_size(&self) -> Option<u64> {
        self.any_listing().map(|listing| listing.bdrom.size)
    }

    /// The detected-feature labels, when a disc is loaded — the info box's
    /// `Detected Features` line (empty slice when the disc has none).
    #[must_use]
    pub fn disc_features(&self) -> &[String] {
        self.any_listing().map_or(&[], |listing| listing.features.as_slice())
    }

    /// The active (highlighted) row index, when a disc is loaded.
    #[must_use]
    pub fn active_index(&self) -> Option<usize> {
        self.any_listing().map(|listing| listing.active)
    }

    /// The playlist table's active column sort — drives the header's
    /// direction marker (`None` until a header is clicked).
    #[must_use]
    pub fn sort(&self) -> Option<Sort> {
        self.any_listing().and_then(|listing| listing.sort)
    }

    /// The active playlist's `index`-th clip name — the real `xxxxx.m2ts`
    /// stream-file name behind the Stream File pane's row `index` (its rows
    /// are one per clip, in clip order) — the Ctrl+C copy target for that row.
    #[must_use]
    pub fn active_clip_name(&self, index: usize) -> Option<String> {
        self.any_listing()
            .and_then(Listing::active_playlist)
            .and_then(|playlist| playlist.clips.get(index))
            .map(|clip| clip.name.clone())
    }

    /// The active playlist's name (for the master-detail pane headers).
    #[must_use]
    pub fn active_playlist_name(&self) -> Option<String> {
        self.any_listing().and_then(Listing::active_playlist).map(|playlist| playlist.name.clone())
    }

    /// The "Stream Files" pane rows for the active playlist (empty when none),
    /// their size cells rendered under the human-readable toggle.
    ///
    /// While a scan is reporting that playlist, the measured column carries its
    /// sizes as of the last snapshot.
    #[must_use]
    pub fn stream_file_rows(&self) -> Vec<StreamFileRow> {
        self.any_listing()
            .and_then(|listing| {
                listing.active_playlist().map(|playlist| {
                    panes::stream_file_rows(
                        playlist,
                        listing.view.human_readable_sizes,
                        self.live_playlist(&playlist.name),
                    )
                })
            })
            .unwrap_or_default()
    }

    /// The "Streams" (codec) pane rows for the active playlist (empty when
    /// none), their rates ticking with the scan like the sizes above.
    #[must_use]
    pub fn codec_rows(&self) -> Vec<CodecRow> {
        self.any_listing()
            .and_then(Listing::active_playlist)
            .map(|playlist| panes::codec_rows(playlist, self.live_playlist(&playlist.name)))
            .unwrap_or_default()
    }

    /// The structural-scan warnings (a corrupt playlist found while listing).
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        self.any_listing().map_or(&[], |listing| listing.warnings.as_slice())
    }

    /// The short-stream notices — one sentence per stream file the measured
    /// scan demuxed materially less of than the disc declares
    /// ([`BdRom::short_stream_files`]), banner material beside the structural
    /// warnings. Empty before a measured scan: an unmeasured disc carries no
    /// per-stream tallies, so the derivation stays silent.
    ///
    /// Derived from the retained disc on every call rather than stored, so a
    /// later scan that replaces the measured playlists can never leave a stale
    /// notice behind.
    #[must_use]
    pub fn short_stream_notices(&self) -> Vec<String> {
        self.any_listing().map_or_else(Vec::new, |listing| {
            listing.bdrom.short_stream_files().iter().map(short_stream_notice).collect()
        })
    }

    /// The selectable table rows (empty unless a disc is loaded).
    ///
    /// A row whose playlist a scan in flight has reported shows that scan's
    /// running `Measured Bytes` count in place of the summary's; every other
    /// cell, and the row order, is the same one a settled table renders.
    #[must_use]
    pub fn table(&self) -> Vec<SelectableRow> {
        let Some(listing) = self.any_listing() else { return Vec::new() };
        model::display_rows(&listing.rows, listing.view)
            .into_iter()
            .enumerate()
            .map(|(index, mut cells)| {
                let row = listing.rows.get(index);
                if let Some(live) = row.and_then(|row| self.live_playlist(&row.name)) {
                    cells.measured_bytes =
                        model::byte_cell(live.measured_bytes, listing.view.human_readable_sizes);
                }
                SelectableRow {
                    index,
                    selected: listing.selection.is_checked(index),
                    active: index == listing.active,
                    has_hidden: row.is_some_and(|row| row.has_hidden_streams),
                    cells,
                }
            })
            .collect()
    }

    /// Whether to show the hidden-tracks footer note.
    #[must_use]
    pub fn show_hidden_note(&self) -> bool {
        self.any_listing().is_some_and(|listing| listing.show_hidden)
    }

    /// The playlists the settings' filters withhold from the table — the
    /// hidden-count line drawn under it, `None` when they withhold none (no
    /// line, no toggle). Unaffected by the transient reveal, which changes
    /// only the line's wording ([`Flow::revealing`]), never its subject.
    #[must_use]
    pub fn hidden_playlists(&self) -> Option<HiddenPlaylists> {
        self.any_listing().and_then(Listing::hidden)
    }

    /// Whether the transient reveal is on — the withheld playlists are in the
    /// table, and the line's toggle restores the settings view.
    #[must_use]
    pub fn revealing(&self) -> bool {
        self.any_listing().is_some_and(|listing| listing.reveal.is_some())
    }

    /// Whether the table checkboxes + selection controls are live (the table is
    /// frozen while a scan is in flight).
    #[must_use]
    pub const fn editable(&self) -> bool {
        matches!(self.inner, Inner::Listed(_) | Inner::Reported { .. })
    }

    /// The number of checked rows.
    #[must_use]
    pub fn selected_count(&self) -> usize {
        self.any_listing().map_or(0, |listing| listing.selection.count())
    }

    /// The number of listed rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.any_listing().map_or(0, |listing| listing.selection.len())
    }

    /// Whether every row is checked (drives the select-all / select-none label).
    #[must_use]
    pub fn all_selected(&self) -> bool {
        self.any_listing().is_some_and(|listing| listing.selection.all())
    }

    /// Whether the loaded disc's stream content is AACS-encrypted — the
    /// encrypted-disc indicator in the info box, and the reason
    /// [`can_scan`](Self::can_scan) is false for a disc that lists perfectly
    /// well.
    #[must_use]
    pub fn disc_encrypted(&self) -> bool {
        self.any_listing().is_some_and(|listing| listing.bdrom.is_aacs_encrypted)
    }

    /// Whether a measured scan can start now — an editable state holding a disc
    /// with at least one listed playlist that is not
    /// [encrypted](Self::disc_encrypted). The selection may be empty: scanning
    /// with nothing checked scans the whole disc (`BDInfo`'s behaviour), so the
    /// "Scan Bitrates" button is live as soon as a disc is listed.
    #[must_use]
    pub fn can_scan(&self) -> bool {
        self.editable_listing().is_some_and(Listing::scannable)
    }

    /// The measured-scan request, when [`Flow::can_scan`]. Scans the checked
    /// playlists, or — when nothing is checked — every listed playlist (scan-all).
    #[must_use]
    pub fn scan_request(&self) -> Option<ScanRequest> {
        // The same gate as can_scan, through one fallible read — so a scan
        // message that reaches `update` without the button (a stale event, a
        // future shortcut) is refused on exactly the same terms.
        let listing = self.editable_listing().filter(|listing| listing.scannable())?;
        let selection = listing.scan_names();
        let scan_files = selection_stream_files(&listing.bdrom.playlists, &selection);
        // The listing open already probed the stream heads for this verdict;
        // carrying it spares the measured open a re-probe
        // ([`ScanOptions::aacs_encrypted`]).
        let mut scan_options = listing.view.scan_options();
        scan_options.aacs_encrypted = Some(listing.bdrom.is_aacs_encrypted);
        Some(ScanRequest {
            input: listing.input.clone(),
            selection,
            scan_files,
            options: listing.view.render_options(),
            scan_options,
        })
    }

    /// The latest progress snapshot, while a measured scan — or the listing's
    /// quick codec pass — is in flight.
    #[must_use]
    pub const fn progress_view(&self) -> Option<&ProgressModel> {
        match &self.inner {
            Inner::Scanning { progress, .. } | Inner::Listing { progress, .. } => progress.as_ref(),
            _ => None,
        }
    }

    /// Whether the in-flight scan's progress events have gone quiet past the
    /// stall threshold — the status line then says "still reading" so a read
    /// stuck on damaged media looks like a struggling drive, not a dead app.
    /// One flag for both reading stages, the structural listing
    /// ([`Stage::Listing`]) and the measured scan ([`Stage::Scanning`]), so
    /// they share one stall vocabulary; always false in the stages that read
    /// nothing.
    #[must_use]
    pub const fn scan_stalled(&self) -> bool {
        matches!(
            self.inner,
            Inner::Scanning { stalled: true, .. } | Inner::Listing { stalled: true, .. }
        )
    }

    /// Whether a report can be shown / saved / copied now — true whenever a disc
    /// is loaded ([`Stage::Listed`] / [`Stage::Scanning`] / [`Stage::Reported`]).
    /// Before the measured scan that report is the structural (zero-bitrate) one;
    /// after, the measured one. The cheap predicate the view gates the "View
    /// Report" button and Save / Copy on.
    #[must_use]
    pub const fn report_available(&self) -> bool {
        matches!(self.inner, Inner::Listed(_) | Inner::Scanning { .. } | Inner::Reported { .. })
    }

    /// The report to display / save / copy for the current stage: the measured
    /// report once a scan has finished ([`Stage::Reported`], byte-for-byte what
    /// the scan rendered), else the retained structural (zero-bitrate) render
    /// over the current selection ([`Stage::Listed`] / [`Stage::Scanning`]) —
    /// `BDInfo`'s pre-scan "View Report".
    ///
    /// Every road borrows a string stored at transition/refresh time (nothing
    /// renders here), so the view calls this per rebuild for free.
    #[must_use]
    pub fn report(&self) -> Option<&str> {
        match &self.inner {
            Inner::Reported { report, .. } => Some(report),
            Inner::Listed(listing) | Inner::Scanning { listing, .. } => {
                Some(&listing.structural_report)
            }
            _ => None,
        }
    }

    /// The recorded measured-scan errors (the resilient "completed with errors"
    /// banner) — empty on a clean scan.
    #[must_use]
    pub fn report_errors(&self) -> &[String] {
        match &self.inner {
            Inner::Reported { errors, .. } => errors,
            _ => &[],
        }
    }

    /// A user-facing error message: the fatal pick error ([`Stage::Failed`]) or a
    /// failed measured scan's banner over the table ([`Stage::Listed`]).
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        match &self.inner {
            Inner::Failed(message) => Some(message),
            Inner::Listed(listing) => listing.error.as_deref(),
            _ => None,
        }
    }

    // ── internals ───────────────────────────────────────────────────────────

    /// The listing behind any disc-loaded state (Listed / Scanning / Reported).
    const fn any_listing(&self) -> Option<&Listing> {
        match &self.inner {
            Inner::Listed(listing)
            | Inner::Scanning { listing, .. }
            | Inner::Reported { listing, .. } => Some(listing),
            _ => None,
        }
    }

    /// The named playlist's cells as the in-flight scan last reported them, or
    /// `None` outside a scan and for a playlist no snapshot has covered yet.
    /// Only [`Stage::Scanning`] answers: a settled table's numbers come from
    /// the summaries themselves.
    fn live_playlist(&self, name: &str) -> Option<&LivePlaylist> {
        match &self.inner {
            Inner::Scanning { live, .. } => live.get(name),
            _ => None,
        }
    }

    /// The listing behind an **editable** state (Listed / Reported) — the
    /// read side of [`editable_listing_mut`](Self::editable_listing_mut).
    const fn editable_listing(&self) -> Option<&Listing> {
        match &self.inner {
            Inner::Listed(listing) | Inner::Reported { listing, .. } => Some(listing),
            _ => None,
        }
    }

    /// The mutable listing behind an **editable** state (Listed / Reported) —
    /// selection edits are refused while a scan is in flight.
    const fn editable_listing_mut(&mut self) -> Option<&mut Listing> {
        match &mut self.inner {
            Inner::Listed(listing) | Inner::Reported { listing, .. } => Some(listing),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bdinfo_rs_core::bdrom::disc::{BdRom, PlaylistSummary, StreamSummary, fixtures};
    use bdinfo_rs_core::primitives::Pid;
    use bdinfo_rs_core::stream::TsStreamType;

    use super::{Flow, Stage};
    use crate::live::{LivePlaylist, LiveStream};
    use crate::model::{Sort, SortColumn, ViewSettings};
    use crate::scan::{Input, Structural};

    /// A presented video stream on camera angle `angle_index`, unmeasured — the
    /// fields the Streams pane reads, and the `(pid, angle_index)` pair a live
    /// rate is keyed on.
    fn stream_summary(pid: Pid, angle_index: usize) -> StreamSummary {
        StreamSummary {
            pid,
            stream_type: TsStreamType::default(),
            codec_short_name: String::new(),
            codec_name: "MPEG-4 AVC Video".to_owned(),
            codec_alt_name: "",
            bitrate: 0,
            active_bitrate: 0,
            language_name: String::new(),
            language_code: String::new(),
            description: String::new(),
            full_description: "1080p / 23.976 fps / 16:9".to_owned(),
            channel_description: String::new(),
            sample_rate: 0,
            bit_depth: 0,
            channel_count: 0,
            height: 0,
            angle_index,
            is_hidden: false,
            ssif_only: false,
        }
    }

    /// The Stream Files pane's `Measured Size` cells, in row order.
    fn pane_sizes(flow: &Flow) -> Vec<String> {
        flow.stream_file_rows().into_iter().map(|row| row.measured).collect()
    }

    /// The Streams pane's `Bit Rate` cells, in row order.
    fn pane_rates(flow: &Flow) -> Vec<String> {
        flow.codec_rows().into_iter().map(|row| row.bitrate).collect()
    }

    fn playlist(name: &str, length: f64, clips: &[&str]) -> PlaylistSummary {
        playlist_sized(name, length, 1000, clips)
    }

    /// A playlist with a name, a length, an estimated `*.m2ts` size, and clips
    /// carrying a name only — nothing here reads their lengths.
    fn playlist_sized(name: &str, length: f64, file_size: u64, clips: &[&str]) -> PlaylistSummary {
        PlaylistSummary {
            file_size,
            ..fixtures::playlist(name, length, fixtures::clips(clips, 0.0))
        }
    }

    fn input() -> Input {
        Input::Folder("disc".into())
    }

    /// A `BdRom` carrying the disc-level facts the flow reads (label / title /
    /// size / flags) over `playlists`; the flags stay off (a plain 2D disc).
    fn bdrom(playlists: Vec<PlaylistSummary>) -> BdRom {
        BdRom {
            volume_label: "DISC".to_owned(),
            disc_title: Some("A Movie".to_owned()),
            size: 78_000_000_000,
            interleaved_size: 0,
            is_3d: false,
            is_50hz: false,
            is_uhd: false,
            is_aacs_encrypted: false,
            is_bd_plus: false,
            is_bd_java: false,
            is_dbox: false,
            is_psp: false,
            playlists,
        }
    }

    fn structural() -> Structural {
        Structural {
            bdrom: bdrom(vec![
                playlist("00000.MPLS", 100.0, &["A.M2TS"]),
                playlist("00001.MPLS", 70.0, &["B.M2TS"]),
            ]),
            features: vec!["Ultra HD".to_owned()],
            warnings: Vec::new(),
        }
    }

    /// A flow listed from the two-playlist disc.
    fn listed() -> Flow {
        Flow::start_listing(input()).listed(&input(), Ok(structural()), ViewSettings::default())
    }

    /// A three-playlist disc with varied lengths and sizes, so every sort
    /// column has a meaningful order: 00000 (100 s, est 1000), 00001 (70 s,
    /// absent estimate — the `-` cell), 00002 (50 s, est 500). Distinct clips,
    /// so the presentation order is longest-first: 00000, 00001, 00002.
    fn structural3() -> Structural {
        Structural {
            bdrom: bdrom(vec![
                playlist_sized("00000.MPLS", 100.0, 1000, &["A.M2TS"]),
                playlist_sized("00001.MPLS", 70.0, 0, &["B.M2TS"]),
                playlist_sized("00002.MPLS", 50.0, 500, &["C.M2TS"]),
            ]),
            features: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// A flow listed from the three-playlist disc.
    fn listed3() -> Flow {
        Flow::start_listing(input()).listed(&input(), Ok(structural3()), ViewSettings::default())
    }

    /// A disc the default filter withholds two playlists from — one per rule:
    /// 00000 (100 s) and 00003 (70 s) list, 00001 (80 s) LOOPS, and 00002
    /// (10 s) is short. Distinct clips, so every listed row is its own group.
    fn structural_filtered() -> Structural {
        let mut looping = playlist("00001.MPLS", 80.0, &["B.M2TS"]);
        looping.has_loops = true;
        Structural {
            bdrom: bdrom(vec![
                playlist("00000.MPLS", 100.0, &["A.M2TS"]),
                looping,
                playlist("00002.MPLS", 10.0, &["C.M2TS"]),
                playlist("00003.MPLS", 70.0, &["D.M2TS"]),
            ]),
            features: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// A flow listed from the two-withheld-playlist disc.
    fn listed_filtered() -> Flow {
        Flow::start_listing(input()).listed(
            &input(),
            Ok(structural_filtered()),
            ViewSettings::default(),
        )
    }

    /// The settings both filter switches off — the persisted equivalent of the
    /// transient reveal.
    fn no_filters() -> ViewSettings {
        ViewSettings {
            filter_short_playlists: false,
            filter_looping_playlists: false,
            ..ViewSettings::default()
        }
    }

    /// The hidden-count line's text for the flow's current state, `None` when
    /// nothing is withheld.
    fn hidden_line(flow: &Flow) -> Option<String> {
        flow.hidden_playlists().map(|hidden| hidden.line(flow.revealing()))
    }

    /// The table's row names in display order.
    fn row_names(flow: &Flow) -> Vec<String> {
        flow.any_listing()
            .map(|listing| listing.rows.iter().map(|row| row.name.clone()).collect())
            .unwrap_or_default()
    }

    /// The checked playlist names as a set (order-independent).
    fn checked_set(flow: &Flow) -> std::collections::BTreeSet<String> {
        flow.any_listing()
            .map(|listing| listing.selected_names().into_iter().collect())
            .unwrap_or_default()
    }

    #[test]
    fn the_scan_notice_names_the_error_count() {
        assert_eq!(super::scan_notice(0), "Scan completed successfully.");
        assert_eq!(
            super::scan_notice(2),
            "Scan completed with 2 error(s).\nThe report below was still written."
        );
    }

    #[test]
    fn idle_is_the_default() {
        assert_eq!(Flow::default().stage(), Stage::Idle);
        assert!(Flow::idle().table().is_empty());
        assert!(!Flow::idle().can_scan());
    }

    #[test]
    fn listing_then_listed_populates_the_table() {
        let flow = Flow::start_listing(input());
        assert_eq!(flow.stage(), Stage::Listing);
        assert_eq!(flow.input_display(), Some("disc".to_owned()));
        let flow = flow.listed(&input(), Ok(structural()), ViewSettings::default());
        assert_eq!(flow.stage(), Stage::Listed);
        assert_eq!(flow.row_count(), 2);
        assert_eq!(flow.table().len(), 2);
        assert!(flow.editable());
        assert!(flow.can_scan()); // scan-all is available as soon as a disc is listed
    }

    #[test]
    fn a_recalled_source_idles_with_the_input_available() {
        let flow = Flow::recall(input());
        // Idle to the view (the opening prompt), so nothing scans at boot…
        assert_eq!(flow.stage(), Stage::Idle);
        assert!(flow.table().is_empty());
        assert!(!flow.can_scan());
        assert!(!flow.report_available());
        // …but the source shows and "Rescan" resolves it, one click to reopen.
        assert_eq!(flow.input_display(), Some("disc".to_owned()));
        assert_eq!(flow.current_input(), Some(&input()));
        // The reopen takes the ordinary listing road.
        let flow = Flow::start_listing(input()).listed(
            &input(),
            Ok(structural()),
            ViewSettings::default(),
        );
        assert_eq!(flow.stage(), Stage::Listed);
    }

    #[test]
    fn current_input_tracks_the_loaded_or_listing_disc() {
        // None until something is picked.
        assert_eq!(Flow::idle().current_input(), None);
        // The input being listed.
        assert_eq!(Flow::start_listing(input()).current_input(), Some(&input()));
        // The input behind a loaded disc.
        assert_eq!(listed().current_input(), Some(&input()));
    }

    #[test]
    fn disc_info_accessors_expose_the_structural_facts() {
        let flow = listed();
        assert_eq!(flow.disc_title(), Some("A Movie"));
        assert_eq!(flow.disc_size(), Some(78_000_000_000));
        assert_eq!(flow.disc_features(), ["Ultra HD"]);
        assert_eq!(flow.input_display(), Some("disc".to_owned()));
        assert!(flow.warnings().is_empty());
        assert!(!flow.show_hidden_note());
        // Idle has no disc, so none of them resolve.
        assert_eq!(Flow::idle().disc_title(), None);
        assert_eq!(Flow::idle().disc_size(), None);
        assert!(Flow::idle().disc_features().is_empty());
        assert_eq!(Flow::idle().input_display(), None);
    }

    #[test]
    fn the_table_marks_hidden_streams_per_row() {
        // The two-playlist fixture hides nothing, so no row is flagged.
        assert!(listed().table().iter().all(|row| !row.has_hidden));
    }

    #[test]
    fn a_failed_structural_scan_is_fatal() {
        let flow = Flow::start_listing(input()).listed(
            &input(),
            Err("no BD".to_owned()),
            ViewSettings::default(),
        );
        assert_eq!(flow.stage(), Stage::Failed);
        assert_eq!(flow.error_message(), Some("no BD"));
    }

    #[test]
    fn a_rejected_open_is_fatal_like_a_bad_pick() {
        // From idle and from a loaded disc alike, the rejection lands in the
        // same failure state a bad pick reaches.
        let flow = Flow::idle().open_failed("not a disc".to_owned());
        assert_eq!(flow.stage(), Stage::Failed);
        assert_eq!(flow.error_message(), Some("not a disc"));
        assert_eq!(listed().open_failed("not a disc".to_owned()).stage(), Stage::Failed);
    }

    #[test]
    fn a_rejected_open_never_interrupts_a_running_scan() {
        // While listing, the drop is ignored — the listing continues.
        let flow = Flow::start_listing(input()).open_failed("nope".to_owned());
        assert_eq!(flow.stage(), Stage::Listing);
        // While a measured scan runs, likewise.
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(1).open_failed("nope".to_owned());
        assert_eq!(flow.stage(), Stage::Scanning);
    }

    #[test]
    fn a_superseded_structural_result_is_ignored() {
        // Pick A, then pick B before A's scan returns; A's result must not apply.
        let other = Input::Folder("other".into());
        let flow = Flow::start_listing(other);
        let flow = flow.listed(&input(), Ok(structural()), ViewSettings::default()); // result for the OLD pick
        assert_eq!(flow.stage(), Stage::Listing); // still listing the new pick
        assert_eq!(flow.input_display(), Some("other".to_owned()));
    }

    #[test]
    fn scan_request_covers_the_selection_or_the_whole_disc() {
        // A listed disc can be scanned even before anything is checked.
        let mut flow = listed();
        assert!(flow.can_scan());
        // Nothing checked ⇒ scan every listed playlist (scan-all, like --whole).
        let whole = flow.scan_request().expect("a scan-all request");
        assert_eq!(whole.selection, ["00000.MPLS", "00001.MPLS"]);
        // Checking a row narrows the scan to that playlist.
        flow.toggle(0);
        assert_eq!(flow.selected_count(), 1);
        let one = flow.scan_request().expect("a request once a row is checked");
        assert_eq!(one.selection, ["00000.MPLS"]);
        assert_eq!(one.scan_files.into_iter().collect::<Vec<_>>(), ["A.M2TS"]);
    }

    #[test]
    fn a_scan_request_carries_the_views_scan_options() {
        // The request snapshots the retention switch the same way it snapshots
        // the report sections — the worker reads neither from the settings.
        assert!(listed().scan_request().expect("a scan-all request").scan_options.keep_partial);
        let mut flow = listed();
        flow.set_view(ViewSettings { keep_partial: false, ..ViewSettings::default() });
        let request = flow.scan_request().expect("a scan-all request");
        assert!(!request.scan_options.keep_partial);
        assert!(request.options.stream_diagnostics, "the report switches are untouched");
    }

    #[test]
    fn an_encrypted_disc_lists_but_never_scans() {
        // The same two-playlist disc, encrypted: it lists, browses and reports
        // structurally exactly as the plain one does…
        let mut structural = structural();
        structural.bdrom.is_aacs_encrypted = true;
        let mut flow =
            Flow::start_listing(input()).listed(&input(), Ok(structural), ViewSettings::default());
        assert!(flow.disc_encrypted());
        assert_eq!(flow.row_count(), 2);
        assert!(flow.editable());
        assert!(flow.report_available());
        // …and the measured scan is refused, checked rows or not.
        assert!(!flow.can_scan());
        assert!(flow.scan_request().is_none());
        flow.select_all();
        assert!(!flow.can_scan());
        assert!(flow.scan_request().is_none());
        // The plain disc is the control: same rows, scan available.
        let plain = listed();
        assert!(!plain.disc_encrypted());
        assert!(plain.can_scan());
        assert!(plain.scan_request().is_some());
        // A state with no disc reports neither.
        assert!(!Flow::idle().disc_encrypted());
    }

    #[test]
    fn select_all_then_none() {
        let mut flow = listed();
        flow.select_all();
        assert_eq!(flow.selected_count(), 2);
        assert!(flow.all_selected());
        flow.select_none();
        assert_eq!(flow.selected_count(), 0);
        // Nothing checked does not block scanning — it is a whole-disc scan.
        assert!(flow.can_scan());
    }

    #[test]
    fn a_tick_advances_elapsed_without_a_progress_event() {
        let mut flow = listed().start_scanning(1);
        flow.progress(1, "A.M2TS".to_owned(), 1, 2, Duration::from_secs(4));
        let before = flow.progress_view().expect("a progress model");
        assert_eq!(before.elapsed_hms(), "00:00:04");
        assert_eq!(before.remaining_hms(), "00:00:04");
        // The wall clock moves on with no progress event in between…
        flow.tick(Duration::from_secs(65), Duration::from_secs(2));
        let after = flow.progress_view().expect("the model survives the tick");
        assert_eq!(after.elapsed_hms(), "00:01:05");
        // …and only elapsed follows: the percent, the remaining estimate and
        // the file hold, and 2 s of quiet is not yet a stall.
        assert_eq!(after.percent, 50);
        assert_eq!(after.remaining_hms(), "00:00:04");
        assert_eq!(after.file(), "A.M2TS");
        assert!(!flow.scan_stalled());
    }

    #[test]
    fn quiet_ticks_flag_the_stall_and_a_progress_event_clears_it() {
        let mut flow = listed().start_scanning(1);
        flow.progress(1, "A.M2TS".to_owned(), 1, 2, Duration::from_secs(1));
        assert!(!flow.scan_stalled());
        // The threshold is 5 s inclusive; just under it is not a stall.
        flow.tick(Duration::from_secs(6), Duration::from_millis(4_999));
        assert!(!flow.scan_stalled());
        flow.tick(Duration::from_secs(7), Duration::from_secs(5));
        assert!(flow.scan_stalled());
        // Well past the threshold reads stalled too (the flag is a range,
        // not a single instant).
        flow.tick(Duration::from_secs(8), Duration::from_secs(6));
        assert!(flow.scan_stalled());
        // A fresh progress event is proof of life and clears the flag.
        flow.progress(1, "A.M2TS".to_owned(), 1, 2, Duration::from_secs(9));
        assert!(!flow.scan_stalled());
    }

    #[test]
    fn a_tick_before_any_progress_or_off_stage_is_harmless() {
        // Scanning with no model yet: nothing to re-stamp, but the stall flag
        // still tracks (the very first read can be the one that sticks).
        let mut flow = listed().start_scanning(1);
        flow.tick(Duration::from_secs(9), Duration::from_secs(9));
        assert!(flow.progress_view().is_none());
        assert!(flow.scan_stalled());
        // Off Stage::Scanning a stray tick changes nothing.
        let mut flow = listed();
        flow.tick(Duration::from_secs(9), Duration::from_secs(9));
        assert!(!flow.scan_stalled());
        assert_eq!(flow.stage(), Stage::Listed);
    }

    #[test]
    fn quiet_ticks_flag_a_stalled_listing_and_an_event_clears_it() {
        let mut flow = Flow::start_listing(input());
        assert!(!flow.scan_stalled(), "a listing starts with the flag down");
        flow.list_progress("A.M2TS".to_owned(), 1, 4, Duration::from_secs(1));
        assert!(!flow.scan_stalled());
        // The listing takes the measured scan's threshold unchanged — 5 s,
        // inclusive — so one silence means the same thing in both stages.
        flow.tick(Duration::from_secs(6), Duration::from_millis(4_999));
        assert!(!flow.scan_stalled());
        flow.tick(Duration::from_secs(7), Duration::from_secs(5));
        assert!(flow.scan_stalled());
        flow.tick(Duration::from_secs(8), Duration::from_secs(6));
        assert!(flow.scan_stalled());
        // The listing's readout climbs across those ticks, and nothing else
        // in its snapshot moves.
        let model = flow.progress_view().expect("the listing keeps its snapshot");
        assert_eq!(model.elapsed_hms(), "00:00:08");
        assert_eq!(model.file(), "A.M2TS");
        assert_eq!(model.percent, 25);
        // A fresh event is proof the read loop is moving.
        flow.list_progress("A.M2TS".to_owned(), 2, 4, Duration::from_secs(9));
        assert!(!flow.scan_stalled());
    }

    #[test]
    fn a_listing_stalls_before_its_first_progress_event() {
        // The listing's stall clock runs from its start, never from a first
        // event: the metadata open reports nothing, and a listing cancelled
        // early emits no event at all — so the flag must rise with no
        // snapshot to re-stamp and no file to name.
        let mut flow = Flow::start_listing(input());
        flow.tick(Duration::from_secs(9), Duration::from_secs(9));
        assert!(flow.progress_view().is_none());
        assert!(flow.scan_stalled());
        // Cancelling out of the stalled listing leaves nothing flagged.
        let flow = flow.cancel();
        assert_eq!(flow.stage(), Stage::Idle);
        assert!(!flow.scan_stalled());
    }

    #[test]
    fn the_scan_lifecycle_reaches_reported() {
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(1);
        assert_eq!(flow.stage(), Stage::Scanning);
        assert!(!flow.editable()); // frozen while scanning
        assert!(flow.scan_request().is_none());

        let mut flow = flow;
        flow.progress(1, "A.M2TS".to_owned(), 1, 2, Duration::ZERO);
        assert_eq!(flow.progress_view().map(|p| p.percent), Some(50));

        let flow = flow.finished(
            1,
            "REPORT".to_owned(),
            Arc::new(Vec::new()),
            structural().bdrom.playlists,
        );
        assert_eq!(flow.stage(), Stage::Reported);
        // Reported returns the measured report byte-for-byte (not a re-render).
        assert_eq!(flow.report(), Some("REPORT"));
        assert_eq!(flow.label(), Some("DISC"));
        assert!(flow.report_errors().is_empty());
        assert!(flow.report_available());
        // Reported is editable again — the user can re-select and re-scan.
        assert!(flow.editable());
    }

    #[test]
    fn the_report_is_available_before_the_scan_and_follows_the_selection() {
        // A freshly listed disc already offers a report — BDInfo's pre-scan
        // "View Report", the structural (zero-bitrate) one.
        let mut flow = listed();
        assert!(flow.report_available());
        assert_eq!(flow.label(), Some("DISC")); // the disc label is available pre-scan
        let whole = flow.report().expect("a structural report before scanning").to_owned();
        // The disc-level facts render even before measuring.
        assert!(whole.contains("Disc Label:"));
        assert!(whole.contains("DISC"));
        // Nothing checked ⇒ every listed playlist, exactly like BDInfo's report.
        assert!(whole.contains("00000.MPLS"));
        assert!(whole.contains("00001.MPLS"));
        // Checking one row narrows the report to that playlist's block, so it is
        // strictly shorter than the whole-disc report (it tracks the selection).
        flow.toggle(0);
        let one = flow.report().expect("a structural report for the selection");
        assert!(one.len() < whole.len());
        assert!(one.contains("00000.MPLS"));
    }

    #[test]
    fn selection_edits_keep_the_pre_scan_report_fresh() {
        // The pre-scan report is STORED (the view borrows it), so every
        // selection edit must refresh it: a toggle narrows it, select-all
        // widens it back, select-none returns it to scan-all.
        let mut flow = listed();
        let whole = flow.report().expect("a structural report").to_owned();
        flow.toggle(0);
        let one = flow.report().expect("a structural report").to_owned();
        assert!(one.contains("00000.MPLS") && !one.contains("00001.MPLS"));
        flow.select_all();
        assert_eq!(flow.report(), Some(whole.as_str()), "select-all covers the disc again");
        flow.toggle(0); // only 00001 stays checked
        let second = flow.report().expect("a structural report").to_owned();
        assert!(second.contains("00001.MPLS") && !second.contains("00000.MPLS"));
        flow.select_none(); // nothing checked scans the whole disc
        assert_eq!(flow.report(), Some(whole.as_str()), "select-none is a whole-disc report");
    }

    #[test]
    fn sorting_reorders_the_pre_scan_report_with_the_table() {
        // Presentation order is longest-first (00000, 00001); the first
        // Length click sorts ascending and flips the table — the stored
        // report renders the scan set in table order, so it must follow.
        let mut flow = listed();
        flow.sort_by(SortColumn::Length);
        assert_eq!(row_names(&flow), ["00001.MPLS", "00000.MPLS"]);
        let report = flow.report().expect("a structural report");
        let first = report.find("00001.MPLS").expect("00001 is in the report");
        let second = report.find("00000.MPLS").expect("00000 is in the report");
        assert!(first < second, "the report lists playlists in table order");
    }

    #[test]
    fn the_measured_rebuild_refreshes_the_retained_structural_report() {
        // Scan-all, then a measured result that (for the test) lists only one
        // playlist: the retained structural render — what a LATER in-flight
        // scan serves as its pre-scan report — must follow the measured disc,
        // not the original structural one.
        let flow = listed().start_scanning(1);
        let measured = vec![playlist("00000.MPLS", 100.0, &["A.M2TS"])];
        let flow = flow.finished(1, "M".to_owned(), Arc::new(Vec::new()), measured);
        assert_eq!(flow.stage(), Stage::Reported);
        let flow = flow.start_scanning(2);
        assert_eq!(flow.stage(), Stage::Scanning);
        let report = flow.report().expect("the pre-scan render stays available mid-scan");
        assert!(report.contains("00000.MPLS"));
        assert!(!report.contains("00001.MPLS"), "the render follows the measured disc");
    }

    #[test]
    fn the_structural_report_is_available_while_scanning() {
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(1);
        assert_eq!(flow.stage(), Stage::Scanning);
        // View Report stays offered during the scan (the structural report).
        assert!(flow.report_available());
        assert!(flow.report().is_some_and(|report| report.contains("00000.MPLS")));
    }

    #[test]
    fn no_report_before_a_disc_is_loaded() {
        assert!(!Flow::idle().report_available());
        assert!(Flow::idle().report().is_none());
        assert_eq!(Flow::idle().label(), None);
        // No recorded scan errors outside Reported either.
        assert!(Flow::idle().report_errors().is_empty());
        assert!(listed().report_errors().is_empty());
        // A fatal pick failure has no disc, hence no report either.
        let failed = Flow::start_listing(input()).listed(
            &input(),
            Err("no BD".to_owned()),
            ViewSettings::default(),
        );
        assert!(!failed.report_available());
        assert!(failed.report().is_none());
    }

    #[test]
    fn the_active_playlist_drives_the_panes() {
        let mut flow = listed();
        // The first row is active by default; its clip drives the stream-files pane.
        assert_eq!(flow.active_index(), Some(0));
        assert_eq!(flow.active_playlist_name().as_deref(), Some("00000.MPLS"));
        assert!(flow.table().first().is_some_and(|row| row.active));
        let files: Vec<_> = flow.stream_file_rows().into_iter().map(|row| row.file).collect();
        assert_eq!(files, ["A.M2TS"]);
        // Activating the second row switches the panes to its playlist.
        flow.set_active(1);
        assert_eq!(flow.active_playlist_name().as_deref(), Some("00001.MPLS"));
        let files: Vec<_> = flow.stream_file_rows().into_iter().map(|row| row.file).collect();
        assert_eq!(files, ["B.M2TS"]);
        // An out-of-range activation is ignored.
        flow.set_active(99);
        assert_eq!(flow.active_index(), Some(1));
    }

    #[test]
    fn sorting_permutes_the_selection_and_the_active_row_with_the_rows() {
        let mut flow = listed3();
        flow.toggle(0); // check 00000
        flow.set_active(2); // highlight 00002
        // Length ascending: 00002 (50 s), 00001 (70 s), 00000 (100 s).
        flow.sort_by(SortColumn::Length);
        assert_eq!(flow.sort(), Some(Sort { column: SortColumn::Length, ascending: true }));
        assert_eq!(row_names(&flow), ["00002.MPLS", "00001.MPLS", "00000.MPLS"]);
        // The check-mark still belongs to 00000, the highlight to 00002.
        assert_eq!(checked_set(&flow).into_iter().collect::<Vec<_>>(), ["00000.MPLS"]);
        assert_eq!(flow.active_index(), Some(0));
        assert_eq!(flow.active_playlist_name().as_deref(), Some("00002.MPLS"));
        // A second click flips to descending; everything still travels.
        flow.sort_by(SortColumn::Length);
        assert_eq!(flow.sort(), Some(Sort { column: SortColumn::Length, ascending: false }));
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS", "00002.MPLS"]);
        assert_eq!(checked_set(&flow).into_iter().collect::<Vec<_>>(), ["00000.MPLS"]);
        assert_eq!(flow.active_playlist_name().as_deref(), Some("00002.MPLS"));
        // The scan request covers exactly the checked playlist, sort or no sort.
        let request = flow.scan_request().expect("a request for the checked row");
        assert_eq!(request.selection, ["00000.MPLS"]);
        // Sorting is a view concern: the disc's playlist order is untouched.
        let disc_order: Vec<_> = flow
            .any_listing()
            .expect("a loaded disc")
            .bdrom
            .playlists
            .iter()
            .map(|playlist| playlist.name.clone())
            .collect();
        assert_eq!(disc_order, ["00000.MPLS", "00001.MPLS", "00002.MPLS"]);
    }

    #[test]
    fn sorting_is_refused_while_a_scan_is_in_flight() {
        let flow = listed3();
        let mut flow = flow.start_scanning(1);
        flow.sort_by(SortColumn::File);
        assert_eq!(flow.sort(), None);
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS", "00002.MPLS"]);
    }

    #[test]
    fn a_measured_rebuild_reapplies_the_sort_and_keeps_the_checked_names() {
        let mut flow = listed3();
        flow.toggle(1); // check 00001
        // The presentation order already reads ascending by name, so the first
        // File click goes straight to descending: 00002, 00001, 00000.
        flow.sort_by(SortColumn::File);
        assert_eq!(flow.sort(), Some(Sort { column: SortColumn::File, ascending: false }));
        assert_eq!(row_names(&flow), ["00002.MPLS", "00001.MPLS", "00000.MPLS"]);
        flow.set_active(0); // highlight 00002
        let flow = flow.start_scanning(1);
        let flow =
            flow.finished(1, "R".to_owned(), Arc::new(Vec::new()), structural3().bdrom.playlists);
        assert_eq!(flow.stage(), Stage::Reported);
        // The rebuilt table is still sorted, the check-mark still on 00001,
        // the highlight still on 00002.
        assert_eq!(row_names(&flow), ["00002.MPLS", "00001.MPLS", "00000.MPLS"]);
        assert_eq!(checked_set(&flow).into_iter().collect::<Vec<_>>(), ["00001.MPLS"]);
        assert_eq!(flow.active_playlist_name().as_deref(), Some("00002.MPLS"));
    }

    #[test]
    fn active_clip_name_resolves_the_stream_file_rows_clip() {
        let mut flow = listed();
        assert_eq!(flow.active_clip_name(0).as_deref(), Some("A.M2TS"));
        assert_eq!(flow.active_clip_name(9), None);
        flow.set_active(1);
        assert_eq!(flow.active_clip_name(0).as_deref(), Some("B.M2TS"));
        assert_eq!(Flow::idle().active_clip_name(0), None);
    }

    #[test]
    fn finishing_a_scan_refreshes_the_panes_with_measured_data() {
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(1);
        // The measured playlists carry the first clip's packet tally.
        let mut measured = structural().bdrom.playlists;
        measured
            .first_mut()
            .and_then(|p| p.clips.first_mut())
            .expect("the first clip")
            .packet_count = 1000; // 1000 * 192 = 192,000 bytes
        let flow = flow.finished(1, "R".to_owned(), Arc::new(Vec::new()), measured);
        // The active (first) row's stream-files pane shows the measured size.
        let sizes: Vec<_> = flow.stream_file_rows().into_iter().map(|row| row.measured).collect();
        assert_eq!(sizes, ["192,000"]);
    }

    /// The live cells one snapshot carries for a playlist: its total, its
    /// per-clip counts, and one presented stream's rate on the main angle.
    fn live_cells(
        name: &str,
        measured_bytes: u64,
        clips: &[u64],
        bitrate: i64,
    ) -> (String, LivePlaylist) {
        (
            name.to_owned(),
            LivePlaylist {
                measured_bytes,
                clips: clips.to_vec(),
                streams: vec![LiveStream { pid: Pid::new(0x1011), angle_index: 0, bitrate }],
            },
        )
    }

    /// The table's `Measured Bytes` cells, in row order.
    fn measured_cells(flow: &Flow) -> Vec<String> {
        flow.table().into_iter().map(|row| row.cells.measured_bytes).collect()
    }

    #[test]
    fn a_measured_update_ticks_the_cells_and_moves_nothing_else() {
        let mut flow = listed3();
        flow.toggle(0);
        flow.set_active(1);
        let mut flow = flow.start_scanning(1);
        let before = flow.report().expect("the pre-scan report").to_owned();
        assert_eq!(measured_cells(&flow), ["0", "0", "0"], "nothing measured yet");

        flow.measured(1, vec![live_cells("00001.MPLS", 960, &[960], 19_000)]);

        // The reported playlist's cell ticks; the others hold their zeros.
        assert_eq!(measured_cells(&flow), ["0", "960", "0"]);
        // Everything else about the table is exactly as it was: the same rows
        // in the same order, the same checked row, the same highlight, the same
        // frozen state and the same retained report.
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS", "00002.MPLS"]);
        assert_eq!(flow.selected_count(), 1);
        assert!(flow.table().first().is_some_and(|row| row.selected));
        assert_eq!(flow.active_index(), Some(1));
        assert_eq!(flow.stage(), Stage::Scanning);
        assert!(!flow.editable());
        assert_eq!(flow.report(), Some(before.as_str()), "the report is not regenerated");

        // A later snapshot replaces that playlist's cell and still leaves the
        // playlists it did not cover alone.
        flow.measured(1, vec![live_cells("00001.MPLS", 1_920, &[1_920], 19_000)]);
        assert_eq!(measured_cells(&flow), ["0", "1,920", "0"]);
    }

    #[test]
    fn the_active_playlists_panes_tick_with_the_scan() {
        // The active playlist plays two clips and presents one stream, so both
        // panes have a cell to move.
        let mut structural = structural();
        let feature = structural.bdrom.playlists.first_mut().expect("the feature playlist");
        feature.clips = fixtures::clips(&["A.M2TS", "B.M2TS"], 50.0);
        feature.streams = vec![stream_summary(Pid::new(0x1011), 0)];
        let flow =
            Flow::start_listing(input()).listed(&input(), Ok(structural), ViewSettings::default());
        let mut flow = flow.start_scanning(1);
        assert_eq!(pane_sizes(&flow), ["-", "-"], "nothing demuxed yet");
        assert_eq!(pane_rates(&flow), ["-"]);

        flow.measured(1, vec![live_cells("00000.MPLS", 960, &[768, 192], 19_000)]);
        assert_eq!(pane_sizes(&flow), ["768", "192"], "one cell per clip row, in clip order");
        assert_eq!(pane_rates(&flow), ["19 kbps"]);

        // A playlist the snapshot did not cover keeps showing its own (unread)
        // tallies — the panes never borrow another playlist's numbers.
        let mut other = listed();
        other.set_active(1);
        let mut other = other.start_scanning(1);
        other.measured(1, vec![live_cells("00000.MPLS", 960, &[768, 192], 19_000)]);
        assert_eq!(pane_sizes(&other), ["-"]);
    }

    #[test]
    fn measured_cells_reach_only_the_scan_they_belong_to() {
        // A snapshot from a superseded scan is dropped, like a stale progress
        // event…
        let mut flow = listed3().start_scanning(5);
        flow.measured(4, vec![live_cells("00001.MPLS", 960, &[960], 19_000)]);
        assert_eq!(measured_cells(&flow), ["0", "0", "0"], "stale cells ignored");
        // …and so is one that arrives when no scan is running at all, in either
        // stage that shows the table.
        let mut listed = listed3();
        listed.measured(5, vec![live_cells("00001.MPLS", 960, &[960], 19_000)]);
        assert_eq!(measured_cells(&listed), ["0", "0", "0"]);
        let mut reported = listed3().start_scanning(5).finished(
            5,
            "R".to_owned(),
            Arc::new(Vec::new()),
            structural3().bdrom.playlists,
        );
        reported.measured(5, vec![live_cells("00001.MPLS", 960, &[960], 19_000)]);
        assert_eq!(measured_cells(&reported), ["0", "0", "0"]);
    }

    #[test]
    fn the_scans_end_takes_the_live_cells_off_the_grids() {
        // Finishing swaps in the measured summaries — the live cells are a
        // display of the same numbers on their way there, never an addition to
        // them.
        let mut flow = listed().start_scanning(1);
        flow.measured(1, vec![live_cells("00000.MPLS", 960, &[960], 19_000)]);
        assert_eq!(measured_cells(&flow), ["960", "0"]);
        let mut measured = structural().bdrom.playlists;
        measured
            .first_mut()
            .and_then(|playlist| playlist.clips.first_mut())
            .expect("the first clip")
            .packet_count = 1000; // 1000 * 192 = 192,000 bytes
        let done = flow.clone().finished(1, "R".to_owned(), Arc::new(Vec::new()), measured);
        assert_eq!(measured_cells(&done), ["192,000", "0"]);
        // Cancelling ends the scan without measured summaries to show, so the
        // table returns to the pre-scan zeros rather than keeping the numbers
        // of a scan that never completed.
        assert_eq!(measured_cells(&flow.cancel()), ["0", "0"]);
    }

    #[test]
    fn a_stale_generation_never_advances_the_state() {
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(5);
        // A progress/finished for a DIFFERENT generation is dropped.
        let mut probe = flow.clone();
        probe.progress(4, "x".to_owned(), 1, 1, Duration::ZERO);
        assert!(probe.progress_view().is_none(), "stale progress ignored");
        let probe = flow.finished(4, "R".to_owned(), Arc::new(Vec::new()), Vec::new());
        assert_eq!(probe.stage(), Stage::Scanning, "stale finish ignored");
    }

    #[test]
    fn cancel_returns_to_the_table_and_drops_late_messages() {
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(7).cancel();
        assert_eq!(flow.stage(), Stage::Listed);
        // A finish from the cancelled scan's generation must not reach Reported.
        let flow = flow.finished(7, "R".to_owned(), Arc::new(Vec::new()), Vec::new());
        assert_eq!(flow.stage(), Stage::Listed);
    }

    #[test]
    fn cancelling_a_listing_recalls_the_input_and_drops_the_stray_result() {
        let flow = Flow::start_listing(input()).cancel();
        // Back to the source prompt, with the input one "Rescan" click away.
        assert_eq!(flow.stage(), Stage::Idle);
        assert_eq!(flow.current_input(), Some(&input()));
        assert_eq!(flow.input_display(), Some("disc".to_owned()));
        assert!(flow.table().is_empty());
        // The cancelled worker still returns — successfully if the cancel
        // landed after the last read, with the cancellation error otherwise.
        // Neither outcome may apply.
        let stray_ok = flow.clone().listed(&input(), Ok(structural()), ViewSettings::default());
        assert_eq!(stray_ok.stage(), Stage::Idle, "a late success must not resurface the table");
        assert!(stray_ok.table().is_empty());
        let stray_err =
            flow.listed(&input(), Err("scan cancelled".to_owned()), ViewSettings::default());
        assert_eq!(stray_err.stage(), Stage::Idle, "a late failure must not turn fatal");
        assert_eq!(stray_err.error_message(), None);
    }

    #[test]
    fn listing_progress_projects_only_while_listing() {
        let mut flow = Flow::start_listing(input());
        assert!(flow.progress_view().is_none(), "no event yet — nothing to project");
        flow.list_progress("A.M2TS".to_owned(), 1, 4, Duration::from_secs(2));
        let model = flow.progress_view().expect("the listing projects its progress");
        assert_eq!(model.file(), "A.M2TS");
        assert!((model.fraction() - 0.25).abs() < f32::EPSILON);
        assert_eq!(model.elapsed_hms(), "00:00:02");
        // A later event replaces the snapshot.
        flow.list_progress("B.M2TS".to_owned(), 4, 4, Duration::from_secs(3));
        assert_eq!(
            flow.progress_view().map(|model| model.file().to_owned()),
            Some("B.M2TS".to_owned())
        );
        // Cancelling drops the snapshot with the listing…
        let mut flow = flow.cancel();
        flow.list_progress("A.M2TS".to_owned(), 1, 4, Duration::ZERO);
        assert!(flow.progress_view().is_none(), "a cancelled listing projects nothing");
        // …and off the listing stage the event is dropped entirely.
        let mut flow = listed();
        flow.list_progress("A.M2TS".to_owned(), 1, 4, Duration::ZERO);
        assert!(flow.progress_view().is_none(), "a listed table has no listing progress");
    }

    #[test]
    fn a_failed_measured_scan_returns_to_the_table_with_a_banner() {
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(2).scan_failed(2, "disc vanished".to_owned());
        assert_eq!(flow.stage(), Stage::Listed);
        assert_eq!(flow.error_message(), Some("disc vanished"));
        assert!(flow.editable());
    }

    #[test]
    fn a_filter_change_rebuilds_the_rows_and_resets_the_selection() {
        // structural3 lists 3 playlists (100/70/50 s) under the default 20 s
        // filter; raising the threshold to 60 s drops 00002.
        let mut flow = listed3();
        flow.toggle(2); // check 00002 — the row the new filter will drop
        flow.set_active(2);
        assert_eq!(flow.selected_count(), 1);
        let raised = ViewSettings { short_playlist_seconds: 60, ..ViewSettings::default() };
        flow.set_view(raised);
        // The row set changed: 00002 is gone, the selection reset, the active
        // row clamped into the new range.
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS"]);
        assert_eq!(flow.selected_count(), 0);
        assert_eq!(flow.active_index(), Some(1));
        // Loosening the filter to everything brings the short row back —
        // still straight from the retained disc, no rescan anywhere.
        let everything = ViewSettings {
            filter_short_playlists: false,
            filter_looping_playlists: false,
            ..ViewSettings::default()
        };
        flow.set_view(everything);
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS", "00002.MPLS"]);
    }

    #[test]
    fn a_filter_that_drops_every_row_empties_the_panes_too() {
        // Raise the threshold past every playlist: no rows, nothing to scan,
        // and the master-detail panes resolve no active playlist.
        let mut flow = listed3();
        flow.set_view(ViewSettings { short_playlist_seconds: 86_400, ..ViewSettings::default() });
        assert_eq!(flow.row_count(), 0);
        assert!(!flow.can_scan());
        assert!(flow.scan_request().is_none());
        assert_eq!(flow.active_playlist_name(), None);
        assert!(flow.stream_file_rows().is_empty());
        assert!(flow.codec_rows().is_empty());
        // And start_scanning refuses the empty table — the same gate as
        // can_scan, on the transition itself.
        let flow = flow.start_scanning(9);
        assert_eq!(flow.stage(), Stage::Listed);
    }

    #[test]
    fn warnings_hidden_streams_and_codec_rows_surface_through_the_flow() {
        // A disc whose structural scan recorded a warning and whose feature
        // playlist presents one HIDDEN stream: all three accessors the shell
        // banners/marks from must surface it.
        let mut structural = structural();
        structural.warnings = vec!["BDMV/PLAYLIST/00000.mpls: unreadable".to_owned()];
        structural.bdrom.playlists.first_mut().expect("the feature playlist").streams =
            vec![StreamSummary { is_hidden: true, ..stream_summary(Pid::new(0x1011), 0) }];
        let flow =
            Flow::start_listing(input()).listed(&input(), Ok(structural), ViewSettings::default());
        // The structural warning reaches the banner accessor verbatim.
        assert_eq!(flow.warnings(), ["BDMV/PLAYLIST/00000.mpls: unreadable"]);
        // The hidden stream drives the footer note and the row's `*` marker.
        assert!(flow.show_hidden_note());
        assert!(flow.table().first().is_some_and(|row| row.has_hidden));
        // The Streams pane lists the presented codec, flagged hidden.
        let rows = flow.codec_rows();
        assert_eq!(rows.len(), 1);
        assert!(
            rows.first().is_some_and(|row| row.hidden && row.codec == "MPEG-4 AVC Video"),
            "the codec row carries the name and the hidden flag: {rows:?}"
        );
    }

    #[test]
    fn a_measured_short_stream_file_surfaces_the_notice_with_the_shared_wording() {
        use bdinfo_rs_core::bdrom::disc::ClipStreamTally;

        // No disc, and an unmeasured (structural) disc: silent.
        assert!(Flow::idle().short_stream_notices().is_empty());
        let flow = listed().start_scanning(1);
        assert!(flow.short_stream_notices().is_empty(), "unmeasured clips raise no notice");

        // The measured result comes back with the first clip demuxed to 500 of
        // its declared 1640 seconds, carrying the per-stream tally that marks
        // the file as measured.
        let mut measured = structural().bdrom.playlists;
        let clip = measured.first_mut().and_then(|p| p.clips.first_mut()).expect("the first clip");
        clip.length = 1640.0;
        clip.packet_seconds = 500.0;
        clip.streams = vec![ClipStreamTally {
            pid: Pid::new(0x1011),
            stream_type: TsStreamType::AvcVideo,
            codec_short_name: "AVC".to_owned(),
            payload_bytes: 1024,
            packet_count: 8,
        }];
        let flow = flow.finished(1, "R".to_owned(), Arc::new(Vec::new()), measured);
        // The exact sentence, pinned: the CLI and wasm crates carry the same
        // format and pin the same bytes, which is what keeps the three
        // surfaces' wording identical.
        assert_eq!(
            flow.short_stream_notices(),
            ["A.M2TS is shorter than declared: measured 500.0 s of 1640.0 s (1140.0 s missing)"]
        );
    }

    #[test]
    fn a_display_only_change_keeps_the_rows_and_the_selection() {
        let mut flow = listed3();
        flow.toggle(0);
        let human = ViewSettings { human_readable_sizes: true, ..ViewSettings::default() };
        flow.set_view(human);
        // Same filter ⇒ same row set, the check-mark untouched…
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS", "00002.MPLS"]);
        assert_eq!(flow.selected_count(), 1);
        // …but the cells now render human-readable (est 1000 B on row 0).
        let table = flow.table();
        let first = table.first().expect("the first row");
        assert_eq!(first.cells.estimated_bytes, "1,000.00 B");
    }

    #[test]
    fn a_filter_change_reapplies_the_active_sort() {
        let mut flow = listed3();
        // File descending (the names already read ascending): 00002, 00001, 00000.
        flow.sort_by(SortColumn::File);
        assert_eq!(row_names(&flow), ["00002.MPLS", "00001.MPLS", "00000.MPLS"]);
        // Dropping 00002 via the threshold keeps the sort over the survivors.
        let raised = ViewSettings { short_playlist_seconds: 60, ..ViewSettings::default() };
        flow.set_view(raised);
        assert_eq!(row_names(&flow), ["00001.MPLS", "00000.MPLS"]);
        assert_eq!(flow.sort(), Some(Sort { column: SortColumn::File, ascending: false }));
    }

    #[test]
    fn set_view_is_refused_while_a_scan_is_in_flight() {
        let mut flow = listed3().start_scanning(1);
        let raised = ViewSettings { short_playlist_seconds: 60, ..ViewSettings::default() };
        flow.set_view(raised);
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS", "00002.MPLS"]);
        // And on stages with no listing it is simply a no-op.
        let mut idle = Flow::idle();
        idle.set_view(raised);
        assert_eq!(idle.stage(), Stage::Idle);
    }

    #[test]
    fn a_measured_rebuild_keeps_the_view_settings() {
        // Scan with the filter loosened: the short 00003-style row (00002 at
        // 50 s under a 60 s threshold) must survive the measured rebuild too.
        let mut flow = listed3();
        let raised = ViewSettings { short_playlist_seconds: 60, ..ViewSettings::default() };
        flow.set_view(raised);
        assert_eq!(flow.row_count(), 2);
        let flow = flow.start_scanning(1);
        let flow =
            flow.finished(1, "R".to_owned(), Arc::new(Vec::new()), structural3().bdrom.playlists);
        assert_eq!(flow.stage(), Stage::Reported);
        // The rebuilt table still filters under the retained view settings.
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS"]);
    }

    #[test]
    fn the_pre_scan_report_honors_the_report_section_toggles() {
        let mut flow = listed();
        let report = flow.report().expect("a structural report");
        assert!(report.contains("STREAM DIAGNOSTICS:"));
        assert!(report.contains("QUICK SUMMARY:"));
        flow.set_view(ViewSettings {
            report_stream_diagnostics: false,
            report_quick_summary: false,
            ..ViewSettings::default()
        });
        let report = flow.report().expect("a structural report");
        assert!(!report.contains("STREAM DIAGNOSTICS:"));
        assert!(!report.contains("QUICK SUMMARY:"));
    }

    #[test]
    fn a_report_section_toggle_rerenders_the_measured_report_without_a_rescan() {
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(1);
        // The worker's rendered report arrives (a placeholder here — the
        // re-render below replaces it from the retained disc).
        let mut flow = flow.finished(
            1,
            "MEASURED".to_owned(),
            Arc::new(Vec::new()),
            structural().bdrom.playlists,
        );
        assert_eq!(flow.report(), Some("MEASURED"));
        // Turning a section off re-renders from the retained measured disc —
        // still Reported, no rescan, and exactly that section is gone.
        flow.set_view(ViewSettings { report_stream_diagnostics: false, ..ViewSettings::default() });
        assert_eq!(flow.stage(), Stage::Reported);
        let report = flow.report().expect("a report stays available");
        assert!(!report.contains("STREAM DIAGNOSTICS:"));
        assert!(report.contains("QUICK SUMMARY:"));
        assert!(report.contains("00000.MPLS"));
        // Turning it back on brings the section back.
        flow.set_view(ViewSettings::default());
        let report = flow.report().expect("a report stays available");
        assert!(report.contains("STREAM DIAGNOSTICS:"));
    }

    #[test]
    fn the_rerendered_report_keeps_the_scanned_selection_not_the_live_one() {
        let mut flow = listed();
        flow.toggle(0); // scan only 00000.MPLS
        let flow = flow.start_scanning(1);
        let mut flow =
            flow.finished(1, "M".to_owned(), Arc::new(Vec::new()), structural().bdrom.playlists);
        // Re-select for a NEXT scan: check the second row too. The displayed
        // report still describes what was scanned.
        flow.toggle(1);
        flow.set_view(ViewSettings { report_quick_summary: false, ..ViewSettings::default() });
        let report = flow.report().expect("a report stays available");
        assert!(report.contains("00000.MPLS"));
        assert!(!report.contains("00001.MPLS"));
        assert!(!report.contains("QUICK SUMMARY:"));
    }

    #[test]
    fn an_option_change_rerender_keeps_the_warning_block_and_banner() {
        use bdinfo_rs_core::error::{BdError, ScanError, ScanStage};

        let flow = listed().start_scanning(1);
        let errors = Arc::new(vec![ScanError {
            file: "A.M2TS".to_owned(),
            stage: ScanStage::StreamFile,
            reason: BdError::StructureNotFound,
        }]);
        let mut flow = flow.finished(1, "M".to_owned(), errors, structural().bdrom.playlists);
        // The banner lines derive from the typed errors…
        assert_eq!(flow.report_errors().len(), 1);
        // …and the re-render reproduces the report's WARNING block from them.
        flow.set_view(ViewSettings { report_quick_summary: false, ..ViewSettings::default() });
        let report = flow.report().expect("a report stays available");
        assert!(report.contains("WARNING: File errors were encountered during scan:"));
        assert_eq!(flow.report_errors().len(), 1);
    }

    #[test]
    fn edits_and_scans_are_refused_outside_their_states() {
        // Toggling while idle / scanning changes nothing.
        let mut idle = Flow::idle();
        idle.toggle(0);
        idle.select_all();
        assert_eq!(idle.selected_count(), 0);
        // start_scanning outside an editable state is a no-op.
        assert_eq!(Flow::idle().start_scanning(1).stage(), Stage::Idle);
        // A listed disc scans even with nothing checked (scan-all).
        assert_eq!(listed().start_scanning(1).stage(), Stage::Scanning);
    }

    #[test]
    fn the_hidden_line_reports_only_what_the_active_filters_withhold() {
        // Nothing withheld — no line at all.
        assert_eq!(hidden_line(&listed3()), None);
        // Two withheld, one per rule: the count is playlists, the parenthesis
        // is rules, and the names come in table order (length desc).
        let flow = listed_filtered();
        assert_eq!(
            hidden_line(&flow).as_deref(),
            Some("2 playlists hidden by filters (short, looping)")
        );
        let hidden = flow.hidden_playlists().expect("two withheld");
        assert_eq!(hidden.names(), ["00001.MPLS", "00002.MPLS"]);
        // Switching both rules off in the SETTINGS retires the line entirely.
        let mut flow = flow;
        flow.set_view(no_filters());
        assert_eq!(hidden_line(&flow), None);
        // Stages with no disc have nothing to report.
        assert_eq!(hidden_line(&Flow::idle()), None);
        assert!(!Flow::idle().revealing());
    }

    #[test]
    fn show_reveals_the_withheld_rows_and_hide_restores_them() {
        let mut flow = listed_filtered();
        assert_eq!(row_names(&flow), ["00000.MPLS", "00003.MPLS"]);
        flow.toggle_reveal();
        // Both withheld playlists join the table, in the ordinary presentation
        // order (length descending), and the line flips to "Showing".
        assert!(flow.revealing());
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS", "00003.MPLS", "00002.MPLS"]);
        assert_eq!(
            hidden_line(&flow).as_deref(),
            Some("Showing 2 filtered playlists (short, looping)")
        );
        // The reveal is transient: the persisted view settings are untouched,
        // so the Settings dialog still opens on both filters ON.
        assert_eq!(flow.any_listing().map(|listing| listing.view), Some(ViewSettings::default()));
        // Hide restores the settings' own view.
        flow.toggle_reveal();
        assert!(!flow.revealing());
        assert_eq!(row_names(&flow), ["00000.MPLS", "00003.MPLS"]);
        assert_eq!(
            hidden_line(&flow).as_deref(),
            Some("2 playlists hidden by filters (short, looping)")
        );
    }

    #[test]
    fn a_reveal_switches_off_only_the_withholding_rules() {
        // Only the looping rule withholds anything here (00002 is 10 s but
        // the short switch is off already), so the reveal must leave the
        // short rule alone rather than widening the table further.
        let looping_only =
            ViewSettings { filter_short_playlists: false, ..ViewSettings::default() };
        let mut flow =
            Flow::start_listing(input()).listed(&input(), Ok(structural_filtered()), looping_only);
        assert_eq!(hidden_line(&flow).as_deref(), Some("1 playlist hidden by filters (looping)"));
        flow.toggle_reveal();
        let filter =
            flow.any_listing().and_then(|listing| listing.reveal.clone()).expect("a reveal filter");
        assert!(!filter.filter_looping_playlists);
        assert!(!filter.filter_short_playlists, "the settings' own switch was already off");
        // Conversely, with only the short rule withholding, the looping switch
        // stays ON through the reveal.
        let mut flow = listed3();
        flow.set_view(ViewSettings { short_playlist_seconds: 60, ..ViewSettings::default() });
        flow.toggle_reveal();
        let filter =
            flow.any_listing().and_then(|listing| listing.reveal.clone()).expect("a reveal filter");
        assert!(!filter.filter_short_playlists);
        assert!(filter.filter_looping_playlists);
    }

    #[test]
    fn a_settings_apply_clears_the_transient_reveal() {
        let mut flow = listed_filtered();
        flow.toggle_reveal();
        assert_eq!(flow.row_count(), 4);
        // A DISPLAY-only apply still wins over the reveal: explicit settings
        // are the global preference, so the withheld rows leave again.
        flow.set_view(ViewSettings { human_readable_sizes: true, ..ViewSettings::default() });
        assert!(!flow.revealing());
        assert_eq!(row_names(&flow), ["00000.MPLS", "00003.MPLS"]);
        // And an apply that switches the filters off itself leaves no reveal
        // to clear — the rows are listed by the settings, not the toggle.
        flow.set_view(no_filters());
        assert!(!flow.revealing());
        assert_eq!(flow.row_count(), 4);
        assert_eq!(hidden_line(&flow), None);
    }

    #[test]
    fn a_reveal_costs_the_selection_exactly_what_a_filter_change_does() {
        let mut flow = listed_filtered();
        // File descending (the names already read ascending): 00003, 00000.
        flow.sort_by(SortColumn::File);
        flow.toggle(0);
        flow.set_active(1);
        assert_eq!(flow.selected_count(), 1);
        flow.toggle_reveal();
        // The row set changed, so — exactly as on a settings filter change —
        // the selection resets and the active row clamps, while the active
        // column sort is re-applied over the wider set.
        assert_eq!(flow.selected_count(), 0);
        assert_eq!(flow.active_index(), Some(1));
        assert_eq!(row_names(&flow), ["00003.MPLS", "00002.MPLS", "00001.MPLS", "00000.MPLS"]);
        assert_eq!(flow.sort(), Some(Sort { column: SortColumn::File, ascending: false }));
    }

    #[test]
    fn revealed_rows_are_ordinary_rows_the_scan_covers() {
        let mut flow = listed_filtered();
        flow.toggle_reveal();
        // A revealed row checks like any other and reaches the scan request…
        flow.toggle(1); // 00001.MPLS, the looping playlist
        let request = flow.scan_request().expect("a revealed row can scan");
        assert_eq!(request.selection, ["00001.MPLS"]);
        assert!(request.scan_files.contains("B.M2TS"));
        // …and it survives the measured rebuild, which re-derives the rows
        // under the same reveal.
        let flow = flow.start_scanning(1).finished(
            1,
            "R".to_owned(),
            Arc::new(Vec::new()),
            structural_filtered().bdrom.playlists,
        );
        assert_eq!(flow.stage(), Stage::Reported);
        assert!(row_names(&flow).contains(&"00001.MPLS".to_owned()));
        assert!(flow.revealing());
    }

    #[test]
    fn a_revealed_table_renders_the_same_report_as_the_settings_road() {
        // The reveal is a filter, not a second code path: the report over a
        // revealed table is byte-identical to the same table reached by
        // switching both filters off in the settings.
        let mut revealed = listed_filtered();
        revealed.toggle_reveal();
        let mut configured = listed_filtered();
        configured.set_view(no_filters());
        assert_eq!(row_names(&revealed), row_names(&configured));
        assert_eq!(revealed.report(), configured.report());
    }

    #[test]
    fn the_reveal_is_refused_where_the_row_set_must_not_move() {
        // Mid-scan: the table is frozen, so the toggle changes nothing.
        let mut scanning = listed_filtered().start_scanning(1);
        scanning.toggle_reveal();
        assert!(!scanning.revealing());
        assert_eq!(row_names(&scanning), ["00000.MPLS", "00003.MPLS"]);
        // With nothing withheld there is no row set to change: the toggle is a
        // no-op down to the selection it would otherwise have reset.
        let mut nothing_hidden = listed3();
        nothing_hidden.toggle(0);
        nothing_hidden.toggle_reveal();
        assert!(!nothing_hidden.revealing());
        assert_eq!(nothing_hidden.selected_count(), 1);
        // And a stage with no listing is untouched.
        let mut idle = Flow::idle();
        idle.toggle_reveal();
        assert_eq!(idle.stage(), Stage::Idle);
    }

    // The async message ordering is the crate's trickiest seam, so amplify the
    // unit cases: NO sequence of (possibly stale, possibly reordered) scan
    // messages may drive the flow into an inconsistent state.
    mod prop {
        use std::sync::Arc;

        use proptest::prelude::*;

        use super::super::{Flow, Stage};
        use super::{
            SortColumn, ViewSettings, checked_set, input, listed, listed_filtered, listed3,
            row_names, structural,
        };

        /// One driver event — the messages `update` would feed the flow.
        #[derive(Debug, Clone)]
        enum Event {
            Toggle(usize),
            Activate(usize),
            SelectAll,
            SelectNone,
            Sort(SortColumn),
            SetView(u8),
            StartScan,
            Progress(u64),
            Finished(u64),
            Failed(u64),
            Cancel,
            Relist,
            OpenRejected,
        }

        /// One of a few representative view configurations — a filter change,
        /// a looser filter, and display-only toggles.
        fn view_variant(pick: u8) -> ViewSettings {
            match pick {
                0 => ViewSettings::default(),
                1 => ViewSettings { short_playlist_seconds: 90, ..ViewSettings::default() },
                2 => ViewSettings {
                    filter_short_playlists: false,
                    filter_looping_playlists: false,
                    ..ViewSettings::default()
                },
                _ => ViewSettings {
                    human_readable_sizes: true,
                    display_chapter_count: false,
                    report_quick_summary: false,
                    ..ViewSettings::default()
                },
            }
        }

        /// One sorting-invariant op — a checkbox toggle or a header click.
        #[derive(Debug, Clone)]
        enum Op {
            Toggle(usize),
            Sort(SortColumn),
        }

        fn sort_column() -> impl Strategy<Value = SortColumn> {
            prop_oneof![
                Just(SortColumn::File),
                Just(SortColumn::Group),
                Just(SortColumn::Length),
                Just(SortColumn::Estimated),
                Just(SortColumn::Measured),
            ]
        }

        fn event() -> impl Strategy<Value = Event> {
            prop_oneof![
                (0_usize..4).prop_map(Event::Toggle),
                (0_usize..5).prop_map(Event::Activate),
                Just(Event::SelectAll),
                Just(Event::SelectNone),
                sort_column().prop_map(Event::Sort),
                (0_u8..4).prop_map(Event::SetView),
                Just(Event::StartScan),
                (0_u64..4).prop_map(Event::Progress),
                (0_u64..4).prop_map(Event::Finished),
                (0_u64..4).prop_map(Event::Failed),
                Just(Event::Cancel),
                Just(Event::Relist),
                Just(Event::OpenRejected),
            ]
        }

        proptest! {
            #[test]
            fn no_event_sequence_breaks_the_invariants(events in proptest::collection::vec(event(), 0..64)) {
                let mut flow = listed();
                // The shell bumps the generation each time a scan starts; the
                // model mirrors that here.
                let mut generation: u64 = 0;
                for ev in events {
                    let scanning_gen = matches!(flow.stage(), Stage::Scanning).then_some(generation);
                    match ev {
                        Event::Toggle(i) => flow.toggle(i),
                        Event::Activate(i) => flow.set_active(i),
                        Event::SelectAll => flow.select_all(),
                        Event::SelectNone => flow.select_none(),
                        Event::SetView(pick) => flow.set_view(view_variant(pick)),
                        Event::Sort(column) => {
                            let before = checked_set(&flow);
                            flow.sort_by(column);
                            // Sorting is a pure view permutation — the checked
                            // playlist set never changes.
                            prop_assert_eq!(checked_set(&flow), before);
                        }
                        Event::StartScan => {
                            let could = flow.can_scan();
                            if could {
                                generation = generation.wrapping_add(1);
                            }
                            let before = flow.stage();
                            flow = flow.start_scanning(generation);
                            // start_scanning advances exactly when can_scan held.
                            if could {
                                prop_assert_eq!(flow.stage(), Stage::Scanning);
                            } else {
                                prop_assert_eq!(flow.stage(), before);
                            }
                        }
                        Event::Progress(g) => {
                            let before = flow.progress_view().cloned();
                            flow.progress(g, String::new(), 1, 2, std::time::Duration::ZERO);
                            // A progress event for any other generation leaves the
                            // recorded snapshot untouched (a stale event is dropped).
                            if scanning_gen != Some(g) {
                                prop_assert_eq!(flow.progress_view().cloned(), before);
                            }
                        }
                        Event::Finished(g) => {
                            let was = scanning_gen;
                            let before = flow.stage();
                            flow = flow.finished(
                                g,
                                "R".to_owned(),
                                Arc::new(Vec::new()),
                                structural().bdrom.playlists,
                            );
                            // The ONLY way to enter Reported is finishing the
                            // in-flight scan; a stale finish changes nothing.
                            if before != Stage::Reported && flow.stage() == Stage::Reported {
                                prop_assert_eq!(was, Some(g));
                            }
                            if scanning_gen != Some(g) {
                                prop_assert_eq!(flow.stage(), before);
                            }
                        }
                        Event::Failed(g) => {
                            let before = flow.stage();
                            flow = flow.scan_failed(g, "boom".to_owned());
                            // A failure for any other generation is a no-op.
                            if scanning_gen != Some(g) {
                                prop_assert_eq!(flow.stage(), before);
                            }
                        }
                        Event::Cancel => flow = flow.cancel(),
                        Event::Relist => flow = Flow::start_listing(input()).listed(&input(), Ok(structural()), ViewSettings::default()),
                        Event::OpenRejected => {
                            let before = flow.stage();
                            flow = flow.open_failed("nope".to_owned());
                            // A rejected open fails from any settled state but
                            // never interrupts an in-flight scan.
                            if matches!(before, Stage::Listing | Stage::Scanning) {
                                prop_assert_eq!(flow.stage(), before);
                            } else {
                                prop_assert_eq!(flow.stage(), Stage::Failed);
                            }
                        }
                    }
                    // The stage is always one of the legal states, and an
                    // editable state is never mid-scan.
                    let stage = flow.stage();
                    prop_assert!(matches!(
                        stage,
                        Stage::Idle | Stage::Listing | Stage::Listed | Stage::Scanning | Stage::Reported | Stage::Failed
                    ));
                    if flow.editable() {
                        prop_assert!(matches!(stage, Stage::Listed | Stage::Reported));
                    }
                    // can_scan implies an editable disc with at least one row
                    // (the selection may be empty — that is a whole-disc scan).
                    if flow.can_scan() {
                        prop_assert!(flow.editable());
                        prop_assert!(flow.row_count() > 0);
                    }
                    // The selection always parallels the rows, and the active
                    // row stays inside the (possibly re-filtered) row range.
                    prop_assert!(flow.selected_count() <= flow.row_count());
                    if let Some(active) = flow.active_index() {
                        prop_assert!(active < flow.row_count().max(1));
                    }
                }
            }

            // The sorting invariant: under ANY sequence of sorts and toggles,
            // the set of checked playlist names tracks exactly the toggles (a
            // sort never changes it), and the scan set is order-independent —
            // a measured scan started after any amount of sorting scans
            // exactly the same names as before sorting.
            #[test]
            fn sorts_and_toggles_preserve_the_checked_name_set(
                ops in proptest::collection::vec(
                    prop_oneof![
                        (0_usize..4).prop_map(Op::Toggle),
                        sort_column().prop_map(Op::Sort),
                    ],
                    0..64,
                ),
            ) {
                let mut flow = listed3();
                let all: std::collections::BTreeSet<String> =
                    row_names(&flow).into_iter().collect();
                let mut expected = std::collections::BTreeSet::new();
                for op in ops {
                    match op {
                        // A toggle flips the playlist NAME currently at that
                        // row position (an out-of-range index flips nothing).
                        Op::Toggle(index) => {
                            if let Some(name) = row_names(&flow).get(index).cloned()
                                && !expected.remove(&name)
                            {
                                expected.insert(name);
                            }
                            flow.toggle(index);
                        }
                        Op::Sort(column) => flow.sort_by(column),
                    }
                    prop_assert_eq!(checked_set(&flow), expected.clone());
                    // The sorted table still lists exactly the same playlists.
                    let listed: std::collections::BTreeSet<String> =
                        row_names(&flow).into_iter().collect();
                    prop_assert_eq!(&listed, &all);
                    // scan_names as a set: the checked names, or every listed
                    // playlist when nothing is checked (scan-all).
                    let request = flow.scan_request().expect("a listed disc can scan");
                    let scan: std::collections::BTreeSet<String> =
                        request.selection.into_iter().collect();
                    let want = if expected.is_empty() { all.clone() } else { expected.clone() };
                    prop_assert_eq!(scan, want);
                }
            }

            // The reveal invariant: the transient toggle and the Settings
            // dialog are the only two things that move the row set, and the
            // reveal is exactly "the withheld playlists, all of them, or none
            // of them" — under any interleaving of the two, plus the sorts and
            // toggles that run alongside them.
            #[test]
            fn the_reveal_shows_all_or_none_of_the_withheld_playlists(
                events in proptest::collection::vec(
                    prop_oneof![
                        Just(RevealEvent::ToggleReveal),
                        (0_u8..4).prop_map(RevealEvent::SetView),
                        sort_column().prop_map(RevealEvent::Sort),
                        (0_usize..5).prop_map(RevealEvent::Toggle),
                    ],
                    0..48,
                ),
            ) {
                let mut flow = listed_filtered();
                for event in events {
                    match event {
                        RevealEvent::ToggleReveal => flow.toggle_reveal(),
                        // An explicit settings apply always wins: no reveal
                        // may survive one, whatever it changed.
                        RevealEvent::SetView(pick) => {
                            flow.set_view(view_variant(pick));
                            prop_assert!(!flow.revealing());
                        }
                        RevealEvent::Sort(column) => flow.sort_by(column),
                        RevealEvent::Toggle(index) => flow.toggle(index),
                    }
                    let listed: std::collections::BTreeSet<String> =
                        row_names(&flow).into_iter().collect();
                    let withheld = flow.hidden_playlists();
                    // The line reports what the SETTINGS withhold, so it is
                    // present exactly when the table under those settings
                    // would be short of a playlist — reveal or no reveal.
                    match &withheld {
                        Some(hidden) => {
                            prop_assert!(hidden.count() > 0);
                            // Revealed: every withheld playlist is a row.
                            // Hidden: not one of them is.
                            for name in hidden.names() {
                                prop_assert_eq!(listed.contains(name), flow.revealing());
                            }
                        }
                        // Nothing withheld ⇒ nothing to reveal.
                        None => prop_assert!(!flow.revealing()),
                    }
                }
            }
        }

        /// One driver event for the reveal property — the two roads that move
        /// the row set, plus the table edits that ride alongside them.
        #[derive(Debug, Clone)]
        enum RevealEvent {
            ToggleReveal,
            SetView(u8),
            Sort(SortColumn),
            Toggle(usize),
        }
    }
}
