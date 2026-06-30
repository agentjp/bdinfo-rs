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

use bdinfo_rs_core::bdrom::disc::PlaylistSummary;

use crate::model::{self, PlaylistRow, SelectableRow};
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
/// and [`Stage::Reported`] states: the picked input, the disc label, the parsed
/// playlists, the filtered table rows, and the live row selection over them.
#[derive(Debug, Clone)]
struct Listing {
    /// The disc the table came from (folder or `.iso`).
    input: Input,
    /// The disc label (folder name, or `.iso` UDF volume label).
    label: String,
    /// Every parsed playlist — the measured scan resolves the selection's clip
    /// set + report order against these.
    playlists: Vec<PlaylistSummary>,
    /// The standard filtered table rows; the selection parallels this list.
    rows: Vec<PlaylistRow>,
    /// One checkbox flag per row.
    selection: Selection,
    /// Whether any listed playlist hides a stream (the CLI's footer note).
    show_hidden: bool,
    /// Structural-scan failures (a corrupt playlist) — a non-blocking banner.
    warnings: Vec<String>,
    /// A failed measured scan returns here with its message — a banner over the
    /// still-usable table, so the user can re-select and retry.
    error: Option<String>,
}

impl Listing {
    /// Builds the listing from a successful structural scan: the filtered rows
    /// and an empty selection over them.
    fn build(input: Input, structural: Structural) -> Self {
        let rows = model::playlist_rows(&structural.playlists);
        let show_hidden = model::any_hidden(&rows);
        let selection = Selection::new(rows.len());
        Self {
            input,
            label: structural.label,
            playlists: structural.playlists,
            rows,
            selection,
            show_hidden,
            warnings: structural.warnings,
            error: None,
        }
    }

    /// The checked rows' playlist names, in table order.
    fn selected_names(&self) -> Vec<String> {
        self.selection.selected_names(&self.rows)
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
    Listing(Input),
    Listed(Listing),
    Scanning { listing: Listing, generation: u64, progress: Option<ProgressModel> },
    Reported { listing: Listing, report: String, label: String, errors: Vec<String> },
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

    // ── transitions (update calls these) ────────────────────────────────────

    /// A folder/`.iso` was picked — begin its structural scan.
    #[must_use]
    pub const fn start_listing(input: Input) -> Self {
        Self { inner: Inner::Listing(input) }
    }

    /// The structural scan for `input` finished. Applied only when this flow is
    /// still listing **that** input (a newer pick supersedes a stale result):
    /// success → the table; failure → the fatal error state.
    #[must_use]
    pub fn listed(self, input: &Input, result: Result<Structural, String>) -> Self {
        match &self.inner {
            Inner::Listing(pending) if pending == input => match result {
                Ok(structural) => {
                    Self { inner: Inner::Listed(Listing::build(input.clone(), structural)) }
                }
                Err(message) => Self { inner: Inner::Failed(message) },
            },
            // A superseded or unexpected result — keep the current state.
            _ => self,
        }
    }

    /// Toggles the checkbox at `index` (only while the table is editable —
    /// [`Stage::Listed`] / [`Stage::Reported`]).
    pub fn toggle(&mut self, index: usize) {
        if let Some(listing) = self.editable_listing_mut() {
            listing.selection.toggle(index);
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

    /// Begins a measured scan of the current selection, tagged with `generation`.
    /// Applied only from an editable state with a non-empty selection (the same
    /// gate as [`Flow::scan_request`]); otherwise a no-op.
    #[must_use]
    pub fn start_scanning(self, generation: u64) -> Self {
        match self.inner {
            Inner::Listed(mut listing) | Inner::Reported { mut listing, .. }
                if listing.selection.any() =>
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
    /// in-flight scan; moves [`Stage::Scanning`] → [`Stage::Reported`].
    #[must_use]
    pub fn finished(
        self,
        generation: u64,
        report: String,
        label: String,
        errors: Vec<String>,
    ) -> Self {
        match self.inner {
            Inner::Scanning { listing, generation: active, .. } if active == generation => {
                Self { inner: Inner::Reported { listing, report, label, errors } }
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

    /// Cancels the in-flight scan, returning to the table. The worker thread
    /// keeps running in the background (the native scan has no mid-scan abort),
    /// but its later messages are ignored — the generation no longer matches.
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
            Inner::Idle => Stage::Idle,
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
            Inner::Listing(input) => Some(input.display()),
            _ => None,
        })
    }

    /// The disc currently loaded or being listed (folder or `.iso`), if any —
    /// the source the toolbar shows and "Rescan" re-lists. A pure read; it never
    /// drives a transition.
    #[must_use]
    pub fn current_input(&self) -> Option<&Input> {
        match &self.inner {
            Inner::Listing(input) => Some(input),
            _ => self.any_listing().map(|listing| &listing.input),
        }
    }

    /// The disc label, when a disc is loaded.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.any_listing().map(|listing| listing.label.as_str())
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
        model::display_rows(&listing.rows)
            .into_iter()
            .enumerate()
            .map(|(index, cells)| SelectableRow {
                index,
                selected: listing.selection.is_checked(index),
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

    /// Whether a measured scan can start now (an editable state with a non-empty
    /// selection) — the "Scan selected" button's enabled state.
    #[must_use]
    pub fn can_scan(&self) -> bool {
        self.editable() && self.any_listing().is_some_and(|listing| listing.selection.any())
    }

    /// The measured-scan request for the current selection, when [`Flow::can_scan`].
    #[must_use]
    pub fn scan_request(&self) -> Option<ScanRequest> {
        if !self.can_scan() {
            return None;
        }
        let listing = self.any_listing()?;
        let selection = listing.selected_names();
        let scan_files = selection::selection_stream_files(&listing.playlists, &selection);
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

    /// The rendered report, in [`Stage::Reported`].
    #[must_use]
    pub fn report(&self) -> Option<&str> {
        match &self.inner {
            Inner::Reported { report, .. } => Some(report),
            _ => None,
        }
    }

    /// The report's disc label — the `BDINFO.{label}.txt` save name.
    #[must_use]
    pub fn report_label(&self) -> Option<&str> {
        match &self.inner {
            Inner::Reported { label, .. } => Some(label),
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

    use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary};

    use super::{Flow, Stage};
    use crate::scan::{Input, Structural};

    fn clip(name: &str) -> ClipSummary {
        ClipSummary {
            name: name.to_owned(),
            display_name: name.to_owned(),
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
        PlaylistSummary {
            name: name.to_owned(),
            total_length: length,
            file_size: 1000,
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

    fn structural() -> Structural {
        Structural {
            label: "DISC".to_owned(),
            playlists: vec![
                playlist("00000.MPLS", 100.0, &["A.M2TS"]),
                playlist("00001.MPLS", 70.0, &["B.M2TS"]),
            ],
            warnings: Vec::new(),
        }
    }

    /// A flow listed from the two-playlist disc.
    fn listed() -> Flow {
        Flow::start_listing(input()).listed(&input(), Ok(structural()))
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
        let flow = flow.listed(&input(), Ok(structural()));
        assert_eq!(flow.stage(), Stage::Listed);
        assert_eq!(flow.row_count(), 2);
        assert_eq!(flow.table().len(), 2);
        assert!(flow.editable());
        assert!(!flow.can_scan()); // nothing selected yet
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
    fn the_table_marks_hidden_streams_per_row() {
        // The two-playlist fixture hides nothing, so no row is flagged.
        assert!(listed().table().iter().all(|row| !row.has_hidden));
    }

    #[test]
    fn a_failed_structural_scan_is_fatal() {
        let flow = Flow::start_listing(input()).listed(&input(), Err("no BD".to_owned()));
        assert_eq!(flow.stage(), Stage::Failed);
        assert_eq!(flow.error_message(), Some("no BD"));
    }

    #[test]
    fn a_superseded_structural_result_is_ignored() {
        // Pick A, then pick B before A's scan returns; A's result must not apply.
        let other = Input::Folder("other".into());
        let flow = Flow::start_listing(other);
        let flow = flow.listed(&input(), Ok(structural())); // result for the OLD pick
        assert_eq!(flow.stage(), Stage::Listing); // still listing the new pick
        assert_eq!(flow.input_display(), Some("other".to_owned()));
    }

    #[test]
    fn selection_drives_can_scan_and_the_request() {
        let mut flow = listed();
        flow.toggle(0);
        assert_eq!(flow.selected_count(), 1);
        assert!(flow.can_scan());
        let request = flow.scan_request().expect("a request once a row is checked");
        assert_eq!(request.selection, ["00000.MPLS"]);
        assert_eq!(request.scan_files.into_iter().collect::<Vec<_>>(), ["A.M2TS"]);
    }

    #[test]
    fn select_all_then_none() {
        let mut flow = listed();
        flow.select_all();
        assert_eq!(flow.selected_count(), 2);
        assert!(flow.all_selected());
        flow.select_none();
        assert_eq!(flow.selected_count(), 0);
        assert!(!flow.can_scan());
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

        let flow = flow.finished(1, "REPORT".to_owned(), "DISC".to_owned(), Vec::new());
        assert_eq!(flow.stage(), Stage::Reported);
        assert_eq!(flow.report(), Some("REPORT"));
        assert_eq!(flow.report_label(), Some("DISC"));
        assert!(flow.report_errors().is_empty());
        // Reported is editable again — the user can re-select and re-scan.
        assert!(flow.editable());
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
        let probe = flow.finished(4, "R".to_owned(), "D".to_owned(), Vec::new());
        assert_eq!(probe.stage(), Stage::Scanning, "stale finish ignored");
    }

    #[test]
    fn cancel_returns_to_the_table_and_drops_late_messages() {
        let mut flow = listed();
        flow.toggle(0);
        let flow = flow.start_scanning(7).cancel();
        assert_eq!(flow.stage(), Stage::Listed);
        // A finish from the cancelled scan's generation must not reach Reported.
        let flow = flow.finished(7, "R".to_owned(), "D".to_owned(), Vec::new());
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
    fn edits_and_scans_are_refused_outside_their_states() {
        // Toggling while idle / scanning changes nothing.
        let mut idle = Flow::idle();
        idle.toggle(0);
        idle.select_all();
        assert_eq!(idle.selected_count(), 0);
        // start_scanning with no selection is a no-op.
        let flow = listed().start_scanning(1);
        assert_eq!(flow.stage(), Stage::Listed);
    }

    // The async message ordering is the crate's trickiest seam, so amplify the
    // unit cases: NO sequence of (possibly stale, possibly reordered) scan
    // messages may drive the flow into an inconsistent state.
    mod prop {
        use proptest::prelude::*;

        use super::super::{Flow, Stage};
        use super::{input, listed, structural};

        /// One driver event — the messages `update` would feed the flow.
        #[derive(Debug, Clone)]
        enum Event {
            Toggle(usize),
            SelectAll,
            SelectNone,
            StartScan,
            Progress(u64),
            Finished(u64),
            Failed(u64),
            Cancel,
            Relist,
        }

        fn event() -> impl Strategy<Value = Event> {
            prop_oneof![
                (0_usize..4).prop_map(Event::Toggle),
                Just(Event::SelectAll),
                Just(Event::SelectNone),
                Just(Event::StartScan),
                (0_u64..4).prop_map(Event::Progress),
                (0_u64..4).prop_map(Event::Finished),
                (0_u64..4).prop_map(Event::Failed),
                Just(Event::Cancel),
                Just(Event::Relist),
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
                            flow = flow.finished(g, "R".to_owned(), "D".to_owned(), Vec::new());
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
                        Event::Relist => flow = Flow::start_listing(input()).listed(&input(), Ok(structural())),
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
                    // can_scan implies an editable disc with a selection.
                    if flow.can_scan() {
                        prop_assert!(flow.editable());
                        prop_assert!(flow.selected_count() > 0);
                    }
                }
            }
        }
    }
}
