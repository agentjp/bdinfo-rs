//! Tier A — the app's flow state machine.
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

use std::collections::BTreeSet;
use std::time::Duration;

use bdinfo_rs_core::bdrom::disc::{BdRom, PlaylistSummary};
use bdinfo_rs_core::report::text;

use crate::model::{self, PlaylistRow, SelectableRow, Sort, SortColumn, ViewSettings};
use crate::panes::{self, CodecRow, StreamFileRow};
use crate::progress::ProgressModel;
use crate::scan::{Input, Structural};
use crate::selection::{self, Selection};

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
    /// The table's presentation settings — the filter the rows were derived
    /// under and the display toggles the cells render with. Retained so a
    /// settings change re-derives the rows from `bdrom` without a rescan.
    view: ViewSettings,
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
}

impl Listing {
    /// Builds the listing from a successful structural scan: the rows filtered
    /// under `view` and an empty selection over them.
    fn build(input: Input, structural: Structural, view: ViewSettings) -> Self {
        let rows = model::playlist_rows(&structural.bdrom.playlists, &view.filter());
        let show_hidden = model::any_hidden(&rows);
        let selection = Selection::new(rows.len());
        Self {
            input,
            bdrom: structural.bdrom,
            features: structural.features,
            view,
            rows,
            selection,
            active: 0,
            sort: None,
            show_hidden,
            warnings: structural.warnings,
            error: None,
        }
    }

    /// Applies a changed view configuration (the Settings dialog's OK). A
    /// display-only change (sizes / chapter suffix) just re-renders; a
    /// **filter** change re-derives the rows from the retained disc — no disk
    /// IO, mirroring `BDInfo`'s `LoadPlaylists()` re-run — resetting the
    /// selection (the row set changed, as `BDInfo`'s does) and clamping the
    /// active row into the new range.
    fn set_view(&mut self, view: ViewSettings) {
        let filter_changed = self.view.filter() != view.filter();
        self.view = view;
        if !filter_changed {
            return;
        }
        let mut rows = model::playlist_rows(&self.bdrom.playlists, &self.view.filter());
        if let Some(sort) = self.sort {
            let _ = model::sort_rows(&mut rows, sort);
        }
        self.selection = Selection::new(rows.len());
        self.active = self.active.min(rows.len().saturating_sub(1));
        self.show_hidden = model::any_hidden(&rows);
        self.rows = rows;
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
        let mut rows = model::playlist_rows(&playlists, &self.view.filter());
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
        let order = selection::selection_order(&self.bdrom.playlists, &self.scan_names());
        text::render_with(&self.bdrom, &order, &[])
    }
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
    /// Idle, but with last session's source remembered — shown in the Source
    /// field with "Rescan" live, opened only when the user asks.
    Recalled(Input),
    Listing(Input),
    Listed(Listing),
    Scanning {
        listing: Listing,
        generation: u64,
        progress: Option<ProgressModel>,
    },
    Reported {
        listing: Listing,
        report: String,
        errors: Vec<String>,
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
        Self { inner: Inner::Listing(input) }
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
            Inner::Listing(pending) if pending == input => match result {
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
    /// cells. Only while the table is editable ([`Stage::Listed`] /
    /// [`Stage::Reported`]) — the dialog cannot open mid-scan, and this
    /// enforces the same rule.
    pub fn set_view(&mut self, view: ViewSettings) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.set_view(view);
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
            Inner::Listing(_) | Inner::Scanning { .. } => self,
            _ => Self { inner: Inner::Failed(message) },
        }
    }

    /// Toggles the checkbox at `index` (only while the table is editable —
    /// [`Stage::Listed`] / [`Stage::Reported`]).
    pub fn toggle(&mut self, index: usize) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.selection.toggle(index);
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

    /// Checks every row ("Select all").
    pub fn select_all(&mut self) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.selection.set_all();
        }
    }

    /// Unchecks every row ("Select none").
    pub fn select_none(&mut self) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.selection.clear();
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
                Self { inner: Inner::Scanning { listing, generation, progress: None } }
            }
            other => Self { inner: other },
        }
    }

    /// Records a progress event — the file currently demuxing, its `done`/`total`
    /// byte counts, and the wall time elapsed since the scan started. Applied only
    /// while scanning **and** when `generation` matches the in-flight scan (a
    /// stale event is dropped); the pure progress math is [`ProgressModel`].
    pub fn progress(
        &mut self,
        generation: u64,
        file: String,
        done: u64,
        total: u64,
        elapsed: Duration,
    ) {
        if let Inner::Scanning { generation: active, progress, .. } = &mut self.inner
            && *active == generation
        {
            *progress = Some(ProgressModel::compute(file, done, total, elapsed));
        }
    }

    /// The measured scan finished — applied only when `generation` matches the
    /// in-flight scan; moves [`Stage::Scanning`] → [`Stage::Reported`]. The disc
    /// label comes from the (unchanged) loaded disc, so it is not threaded here.
    #[must_use]
    pub fn finished(
        self,
        generation: u64,
        report: String,
        errors: Vec<String>,
        playlists: Vec<PlaylistSummary>,
    ) -> Self {
        match self.inner {
            Inner::Scanning { mut listing, generation: active, .. } if active == generation => {
                listing.apply_measured(playlists);
                Self { inner: Inner::Reported { listing, report, errors } }
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

    /// Cancels the in-flight scan, returning to the table. The shell trips the
    /// worker's cooperative stop flag alongside this transition, so the demux
    /// aborts within moments; the worker's late "cancelled" failure message is
    /// ignored — a Listed flow is no longer Scanning, so `scan_failed` (and any
    /// straggling progress) drops it. Starting a new scan immediately is safe:
    /// it gets a fresh generation and its own cancel flag, and the old worker
    /// only ever reads the disc.
    #[must_use]
    pub fn cancel(self) -> Self {
        match self.inner {
            Inner::Scanning { listing, .. } => Self { inner: Inner::Listed(listing) },
            other => Self { inner: other },
        }
    }

    // ── accessors (view projects these) ─────────────────────────────────────

    /// The coarse stage, for the view's top-level branch.
    #[must_use]
    pub const fn stage(&self) -> Stage {
        match &self.inner {
            Inner::Idle | Inner::Recalled(_) => Stage::Idle,
            Inner::Listing(_) => Stage::Listing,
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
            Inner::Listing(input) | Inner::Recalled(input) => Some(input.display()),
            _ => None,
        })
    }

    /// The disc currently loaded or being listed (folder or `.iso`), if any —
    /// the source the toolbar shows and "Rescan" re-lists. A pure read; it never
    /// drives a transition.
    #[must_use]
    pub fn current_input(&self) -> Option<&Input> {
        match &self.inner {
            Inner::Listing(input) | Inner::Recalled(input) => Some(input),
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
    #[must_use]
    pub fn stream_file_rows(&self) -> Vec<StreamFileRow> {
        self.any_listing()
            .and_then(|listing| {
                listing.active_playlist().map(|playlist| {
                    panes::stream_file_rows(playlist, listing.view.human_readable_sizes)
                })
            })
            .unwrap_or_default()
    }

    /// The "Streams" (codec) pane rows for the active playlist (empty when none).
    #[must_use]
    pub fn codec_rows(&self) -> Vec<CodecRow> {
        self.any_listing()
            .and_then(Listing::active_playlist)
            .map(panes::codec_rows)
            .unwrap_or_default()
    }

    /// The structural-scan warnings (a corrupt playlist found while listing).
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        self.any_listing().map_or(&[], |listing| listing.warnings.as_slice())
    }

    /// The selectable table rows (empty unless a disc is loaded).
    #[must_use]
    pub fn table(&self) -> Vec<SelectableRow> {
        let Some(listing) = self.any_listing() else { return Vec::new() };
        model::display_rows(&listing.rows, listing.view)
            .into_iter()
            .enumerate()
            .map(|(index, cells)| SelectableRow {
                index,
                selected: listing.selection.is_checked(index),
                active: index == listing.active,
                has_hidden: listing.rows.get(index).is_some_and(|row| row.has_hidden_streams),
                cells,
            })
            .collect()
    }

    /// Whether to show the hidden-tracks footer note.
    #[must_use]
    pub fn show_hidden_note(&self) -> bool {
        self.any_listing().is_some_and(|listing| listing.show_hidden)
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

    /// Whether a measured scan can start now — an editable state with at least one
    /// listed playlist. The selection may be empty: scanning with nothing checked
    /// scans the whole disc (`BDInfo`'s behaviour), so the "Scan Bitrates" button
    /// is live as soon as a disc is listed.
    #[must_use]
    pub fn can_scan(&self) -> bool {
        self.editable() && self.any_listing().is_some_and(|listing| !listing.rows.is_empty())
    }

    /// The measured-scan request, when [`Flow::can_scan`]. Scans the checked
    /// playlists, or — when nothing is checked — every listed playlist (scan-all).
    #[must_use]
    pub fn scan_request(&self) -> Option<ScanRequest> {
        if !self.can_scan() {
            return None;
        }
        let listing = self.any_listing()?;
        let selection = listing.scan_names();
        let scan_files = selection::selection_stream_files(&listing.bdrom.playlists, &selection);
        Some(ScanRequest { input: listing.input.clone(), selection, scan_files })
    }

    /// The latest progress snapshot, while scanning.
    #[must_use]
    pub const fn progress_view(&self) -> Option<&ProgressModel> {
        match &self.inner {
            Inner::Scanning { progress, .. } => progress.as_ref(),
            _ => None,
        }
    }

    /// Whether a report can be shown / saved / copied now — true whenever a disc
    /// is loaded ([`Stage::Listed`] / [`Stage::Scanning`] / [`Stage::Reported`]).
    /// Before the measured scan that report is the structural (zero-bitrate) one;
    /// after, the measured one. A cheap predicate the view gates the "View Report"
    /// button and Save / Copy on — unlike [`report`](Self::report) it never
    /// renders, so it is safe to call every frame.
    #[must_use]
    pub const fn report_available(&self) -> bool {
        matches!(self.inner, Inner::Listed(_) | Inner::Scanning { .. } | Inner::Reported { .. })
    }

    /// The report to display / save / copy for the current stage: the measured
    /// report once a scan has finished ([`Stage::Reported`], byte-for-byte what
    /// the scan rendered), else the structural (zero-bitrate) report rendered
    /// from the loaded disc over the current selection ([`Stage::Listed`] /
    /// [`Stage::Scanning`]) — `BDInfo`'s pre-scan "View Report".
    ///
    /// The structural case renders on demand, so call this only when actually
    /// showing or writing the report; test availability with
    /// [`report_available`](Self::report_available) instead.
    #[must_use]
    pub fn report(&self) -> Option<String> {
        match &self.inner {
            Inner::Reported { report, .. } => Some(report.clone()),
            Inner::Listed(listing) | Inner::Scanning { listing, .. } => {
                Some(listing.render_structural())
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
    use std::time::Duration;

    use bdinfo_rs_core::bdrom::disc::{BdRom, ClipSummary, PlaylistSummary};

    use super::{Flow, Stage};
    use crate::model::{Sort, SortColumn, ViewSettings};
    use crate::scan::{Input, Structural};

    fn clip(name: &str) -> ClipSummary {
        ClipSummary {
            name: name.to_owned(),
            display_name: name.to_owned(),
            file_size: 0,
            interleaved_file_size: 0,
            angle_index: 0,
            relative_time_in: 0.0,
            length: 0.0,
            payload_bytes: 0,
            packet_count: 0,
            packet_seconds: 0.0,
            file_seconds: 0.0,
            streams: Vec::new(),
        }
    }

    fn playlist(name: &str, length: f64, clips: &[&str]) -> PlaylistSummary {
        playlist_sized(name, length, 1000, clips)
    }

    fn playlist_sized(name: &str, length: f64, file_size: u64, clips: &[&str]) -> PlaylistSummary {
        PlaylistSummary {
            name: name.to_owned(),
            total_length: length,
            file_size,
            interleaved_file_size: 0,
            chapter_count: 0,
            stream_count: 0,
            angle_count: 0,
            has_loops: false,
            streams: Vec::new(),
            clips: clips.iter().map(|c| clip(c)).collect(),
            chapters: Vec::new(),
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
        // Idle has no disc, so none of them resolve.
        assert_eq!(Flow::idle().disc_title(), None);
        assert_eq!(Flow::idle().disc_size(), None);
        assert!(Flow::idle().disc_features().is_empty());
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
    fn select_all_then_none() {
        let mut flow = listed();
        flow.select_all();
        assert_eq!(flow.selected_count(), 2);
        assert!(flow.all_selected());
        flow.select_none();
        assert_eq!(flow.selected_count(), 0);
        // Nothing checked no longer blocks scanning — it becomes a whole-disc scan.
        assert!(flow.can_scan());
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

        let flow = flow.finished(1, "REPORT".to_owned(), Vec::new(), structural().bdrom.playlists);
        assert_eq!(flow.stage(), Stage::Reported);
        // Reported returns the measured report byte-for-byte (not a re-render).
        assert_eq!(flow.report().as_deref(), Some("REPORT"));
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
        let whole = flow.report().expect("a structural report before scanning");
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
        let flow = flow.finished(1, "R".to_owned(), Vec::new(), structural3().bdrom.playlists);
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
        if let Some(clip) = measured.first_mut().and_then(|p| p.clips.first_mut()) {
            clip.packet_count = 1000; // 1000 * 192 = 192,000 bytes
        }
        let flow = flow.finished(1, "R".to_owned(), Vec::new(), measured);
        // The active (first) row's stream-files pane shows the measured size.
        let sizes: Vec<_> = flow.stream_file_rows().into_iter().map(|row| row.measured).collect();
        assert_eq!(sizes, ["192,000"]);
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
        let probe = flow.finished(4, "R".to_owned(), Vec::new(), Vec::new());
        assert_eq!(probe.stage(), Stage::Scanning, "stale finish ignored");
    }

    #[test]
    fn cancel_returns_to_the_table_and_drops_late_messages() {
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(7).cancel();
        assert_eq!(flow.stage(), Stage::Listed);
        // A finish from the cancelled scan's generation must not reach Reported.
        let flow = flow.finished(7, "R".to_owned(), Vec::new(), Vec::new());
        assert_eq!(flow.stage(), Stage::Listed);
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
        let flow = flow.finished(1, "R".to_owned(), Vec::new(), structural3().bdrom.playlists);
        assert_eq!(flow.stage(), Stage::Reported);
        // The rebuilt table still filters under the retained view settings.
        assert_eq!(row_names(&flow), ["00000.MPLS", "00001.MPLS"]);
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

    // The async message ordering is the crate's trickiest seam, so amplify the
    // unit cases: NO sequence of (possibly stale, possibly reordered) scan
    // messages may drive the flow into an inconsistent state.
    mod prop {
        use proptest::prelude::*;

        use super::super::{Flow, Stage};
        use super::{
            SortColumn, ViewSettings, checked_set, input, listed, listed3, row_names, structural,
        };

        /// One driver event — the messages `update` would feed the flow.
        #[derive(Debug, Clone)]
        enum Event {
            Toggle(usize),
            SelectAll,
            SelectNone,
            Sort(SortColumn),
            StartScan,
            Progress(u64),
            Finished(u64),
            Failed(u64),
            Cancel,
            Relist,
            OpenRejected,
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
                Just(Event::SelectAll),
                Just(Event::SelectNone),
                sort_column().prop_map(Event::Sort),
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
                        Event::SelectAll => flow.select_all(),
                        Event::SelectNone => flow.select_none(),
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
                                Vec::new(),
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
        }
    }
}
