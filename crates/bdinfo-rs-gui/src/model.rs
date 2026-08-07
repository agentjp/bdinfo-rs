//! The pure playlist view-model.
//!
//! These constructors format the playlist table over
//! [`bdinfo_rs_core::bdrom::order::table_rows`] and the public
//! [`PlaylistSummary`] type, producing the same columns the CLI table prints.
//! No widgets, no IO: plain data in, plain data out, so `view()` is a thin
//! projection.

use std::cmp::Ordering;

use bdinfo_rs_core::bdrom::chapters::seconds_to_ticks;
use bdinfo_rs_core::bdrom::disc::{PlaylistSummary, ScanOptions};
use bdinfo_rs_core::bdrom::order::{HiddenRule, PlaylistFilter, hidden_by, table_rows};
use bdinfo_rs_core::report::text::{self, RenderOptions};

use crate::settings::Settings;

/// The table's presentation settings — the slice of the persisted
/// [`Settings`] the view-model reads.
///
/// The playlist filter switches and the two display toggles, plus the
/// switches the scan and the report render read off it
/// ([`render_options`](Self::render_options), [`scan_options`](Self::scan_options)).
/// It travels with the loaded disc (the flow's `Listing`), so a settings
/// change re-derives the rows from the retained `BdRom` — no rescan,
/// mirroring `BDInfo`'s `LoadPlaylists()` re-run.
///
/// `Default` is the fresh-config table: the standard filter (on at 20 s),
/// grouped-byte size cells, the chapter suffix shown.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is a distinct persisted on/off setting, mirroring BDInfo's"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewSettings {
    /// Drop playlists shorter than the threshold.
    pub filter_short_playlists: bool,
    /// The short-playlist threshold, in whole seconds.
    pub short_playlist_seconds: u32,
    /// Drop looping playlists.
    pub filter_looping_playlists: bool,
    /// Human-readable size cells (`72.64 GB`) instead of grouped bytes.
    pub human_readable_sizes: bool,
    /// Show the ` [NN Chapters]` suffix in the Playlist File column.
    pub display_chapter_count: bool,
    /// Render the report's `STREAM DIAGNOSTICS` sections.
    pub report_stream_diagnostics: bool,
    /// Render the report's `QUICK SUMMARY` blocks.
    pub report_quick_summary: bool,
    /// Keep the measurements of a stream file whose read failed partway.
    pub keep_partial: bool,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self::from_settings(&Settings::default())
    }
}

impl ViewSettings {
    /// The view-facing slice of the persisted settings.
    #[must_use]
    pub const fn from_settings(settings: &Settings) -> Self {
        Self {
            filter_short_playlists: settings.filter_short_playlists,
            short_playlist_seconds: settings.short_playlist_seconds,
            filter_looping_playlists: settings.filter_looping_playlists,
            human_readable_sizes: settings.human_readable_sizes,
            display_chapter_count: settings.display_chapter_count,
            report_stream_diagnostics: settings.report_stream_diagnostics,
            report_quick_summary: settings.report_quick_summary,
            keep_partial: settings.keep_partial,
        }
    }

    /// The core playlist filter these settings build — what the row
    /// construction applies instead of `PlaylistFilter::default()`.
    #[must_use]
    pub fn filter(&self) -> PlaylistFilter {
        PlaylistFilter {
            filter_short_playlists: self.filter_short_playlists,
            short_playlist_seconds: f64::from(self.short_playlist_seconds),
            filter_looping_playlists: self.filter_looping_playlists,
        }
    }

    /// The core render options these settings build — the two report-section
    /// switches every report render (structural and measured) applies.
    #[must_use]
    pub const fn render_options(&self) -> RenderOptions {
        RenderOptions {
            stream_diagnostics: self.report_stream_diagnostics,
            quick_summary: self.report_quick_summary,
        }
    }

    /// The core scan options these settings build — the behaviour switches the
    /// measured scan runs under.
    ///
    /// Built from the default rather than a struct literal: [`ScanOptions`] is
    /// `#[non_exhaustive]`, so literal construction is crate-internal to the
    /// core (and the default is `Default::default`, not a `const fn`).
    #[must_use]
    pub fn scan_options(&self) -> ScanOptions {
        let mut options = ScanOptions::default();
        options.keep_partial = self.keep_partial;
        options
    }
}

/// One structured playlist row — the machine-readable form of the CLI's
/// `#`/Group/Playlist File/Length/Estimated Bytes table. (Measured Bytes is
/// omitted: a structural scan never measures, so that column would always
/// read `-` this phase.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlaylistRow {
    /// 1-based position in the table — the handle a later phase's picker uses.
    pub(crate) position: usize,
    /// Index into the scanned disc's `playlists` slice — the handle the
    /// master-detail panes use to resolve this row's clips and streams.
    pub(crate) playlist_index: usize,
    /// Shared-clip group number (1-based) — the CLI's `Group` column.
    pub(crate) group: usize,
    /// The playlist file name, e.g. `00000.MPLS`.
    pub(crate) name: String,
    /// The playlist's chapter count — appended to the displayed name as
    /// ` [NN Chapters]` when it exceeds one, mirroring `BDInfo`.
    pub(crate) chapter_count: usize,
    /// `hh:mm:ss` total length, truncated like the CLI table.
    pub(crate) length: String,
    /// The playlist length as the report's 100 ns ticks — the underlying sort
    /// key behind the formatted `length` cell (hours wrap in the cell, ticks
    /// don't).
    pub(crate) length_ticks: i64,
    /// Estimated bytes per [`PlaylistSummary::estimated_bytes`]; `None` is the
    /// `-` cell.
    pub(crate) estimated_bytes: Option<u64>,
    /// Measured (packet-derived) bytes over all angle clips — `0` until the
    /// measured scan fills it.
    pub(crate) measured_bytes: u64,
    /// Whether the playlist hides any stream — drives the CLI's footer note.
    pub(crate) has_hidden_streams: bool,
}

/// One display row: every cell pre-formatted to the exact string the CLI table
/// prints.
///
/// So the iced view is a trivial projection — it drops each cell into a `text`
/// widget. The columns are `#`/Group/Playlist File/Length/Estimated Bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    /// The `#` column — the 1-based table position.
    pub number: String,
    /// The `Group` column — the shared-clip group number.
    pub group: String,
    /// The `Playlist File` column — the playlist name.
    pub file: String,
    /// The `Length` column — `hh:mm:ss`.
    pub length: String,
    /// The `Estimated Bytes` column — thousands-grouped, or `-`.
    pub estimated_bytes: String,
    /// The `Measured Bytes` column — thousands-grouped (`0` until measured).
    pub measured_bytes: String,
}

/// One selectable table row: the display [`TableRow`] cells plus the row's
/// 0-based position and checked flag.
///
/// What the iced table's leading `Scan` checkbox column renders and toggles — a
/// pub projection so the (separate) binary crate can build the widget from it;
/// the selection logic itself stays in the crate's `selection` module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectableRow {
    /// 0-based position in the table — the index a toggle message carries.
    pub index: usize,
    /// Whether this row is currently checked (queued for the measured scan).
    pub selected: bool,
    /// Whether this is the active (highlighted) row — the one whose clips and
    /// streams the master-detail panes show.
    pub active: bool,
    /// Whether this playlist hides any stream — the table marks it with the
    /// CLI's `*` and shows the footer note.
    pub has_hidden: bool,
    /// The pre-formatted display cells.
    pub cells: TableRow,
}

/// One byte-count cell body: the human-readable short form when the toggle is
/// on (`BDInfo`'s `SizeFormatHR`), else the report's thousands-grouped exact
/// count.
pub(crate) fn byte_cell(bytes: u64, human: bool) -> String {
    if human { format_file_size(bytes) } else { text::group(u128::from(bytes)) }
}

/// The `Estimated Bytes` cell: a [`byte_cell`] when known, else `-`.
fn estimated_cell(bytes: Option<u64>, human: bool) -> String {
    bytes.map_or_else(|| "-".to_owned(), |value| byte_cell(value, human))
}

/// A human-readable byte size, `N.NN <unit>`.
///
/// Mirrors `BDInfo`'s `ToolBox.FormatFileSize(size, true)`: the largest binary
/// (1024) unit, labelled `B`/`KB`/`MB`/`GB`/… (its convention — powers of 1024
/// under decimal labels), with two decimals rounded to nearest. `0` bytes renders
/// as `0`, like `BDInfo`. The GUI's disc-info box shows this next to the exact
/// byte count.
#[must_use]
pub fn format_file_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes == 0 {
        return "0".to_owned();
    }
    // The largest unit whose divisor (1024^unit) still fits in `bytes`.
    let mut unit = 0_usize;
    let mut divisor = 1_u64;
    while unit.saturating_add(1) < UNITS.len() && divisor.saturating_mul(1024) <= bytes {
        divisor = divisor.saturating_mul(1024);
        unit = unit.saturating_add(1);
    }
    // value * 100, rounded to nearest hundredth: (bytes*100 + divisor/2) / divisor.
    // Widened to u128 so the *100 never overflows in the EB range.
    let divisor = u128::from(divisor);
    let half = divisor.checked_div(2).unwrap_or(0);
    let hundredths = u128::from(bytes)
        .saturating_mul(100)
        .saturating_add(half)
        .checked_div(divisor)
        .unwrap_or(0);
    let whole = u64::try_from(hundredths.checked_div(100).unwrap_or(0)).unwrap_or(u64::MAX);
    let frac = hundredths.checked_rem(100).unwrap_or(0);
    let label = UNITS.get(unit).copied().unwrap_or("B");
    format!("{}.{frac:02} {label}", text::group(u128::from(whole)))
}

/// Builds the structured selection-table rows over the `filter`-kept set.
pub(crate) fn playlist_rows(
    playlists: &[PlaylistSummary],
    filter: &PlaylistFilter,
) -> Vec<PlaylistRow> {
    table_rows(playlists, filter)
        .into_iter()
        .enumerate()
        .filter_map(|(position, (group, index))| {
            playlists.get(index).map(|playlist| PlaylistRow {
                position: position.saturating_add(1),
                playlist_index: index,
                group,
                name: playlist.name.clone(),
                chapter_count: playlist.chapter_count,
                length: text::time_hh_short(seconds_to_ticks(playlist.total_length)),
                length_ticks: seconds_to_ticks(playlist.total_length),
                estimated_bytes: playlist.estimated_bytes(),
                measured_bytes: playlist.total_angle_packet_size(),
                has_hidden_streams: playlist.has_hidden_streams(),
            })
        })
        .collect()
}

/// A sortable playlist-table column. The sort key is the row's underlying
/// value — the playlist name (ordinal), the group number, the length seconds,
/// the byte counts — never the formatted cell string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    /// The `Playlist File` column — ordinal on the playlist name.
    File,
    /// The `Group` column — the shared-clip group number.
    Group,
    /// The `Length` column — the underlying playlist length (as ticks).
    Length,
    /// The `Estimated Size` column — bytes; an absent size (the `-` cell)
    /// orders last in both directions.
    Estimated,
    /// The `Measured Size` column — packet-derived bytes (`0` until measured).
    Measured,
}

/// The playlist table's active sort: a column and a direction, built by
/// `BDInfo`'s header-click rule (the crate-private `Sort::click`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    /// The column the table is sorted by.
    pub column: SortColumn,
    /// Ascending (`true`) or descending.
    pub ascending: bool,
}

impl Sort {
    /// `BDInfo`'s header-click rule (`listViewPlaylistFiles_ColumnClick`): a
    /// click on the current sort column flips its direction; a click on any
    /// other column starts it ascending — unless `rows` already read ascending
    /// by that column, in which case it starts descending. That keeps every
    /// click a visible re-order (`BDInfo`'s click always toggles off the
    /// current order, so it never looks like a no-op either).
    pub(crate) fn click(current: Option<Self>, column: SortColumn, rows: &[PlaylistRow]) -> Self {
        match current {
            Some(sort) if sort.column == column => Self { column, ascending: !sort.ascending },
            _ => Self { column, ascending: !is_ascending(rows, column) },
        }
    }
}

/// Whether `rows` already read ascending by `column` (ties allowed) — the
/// first-click direction probe behind [`Sort::click`].
fn is_ascending(rows: &[PlaylistRow], column: SortColumn) -> bool {
    let sort = Sort { column, ascending: true };
    rows.is_sorted_by(|a, b| compare(a, b, sort) != Ordering::Greater)
}

/// Stable-sorts `rows` by `sort` and returns the applied permutation
/// (`order[new] = old`), so index-parallel state — the selection flags, the
/// active row — can travel with the rows. Ties keep their current relative
/// order, like a repeated `ListView` sort.
pub(crate) fn sort_rows(rows: &mut Vec<PlaylistRow>, sort: Sort) -> Vec<usize> {
    // Sort the rows PAIRED with their old indices — no fallible index lookup
    // (and no unreachable not-found arm), and the stable sort carries each
    // row's origin along for the returned permutation.
    let mut keyed: Vec<(usize, PlaylistRow)> = rows.drain(..).enumerate().collect();
    keyed.sort_by(|(_, x), (_, y)| compare(x, y, sort));
    let order = keyed.iter().map(|&(index, _)| index).collect();
    *rows = keyed.into_iter().map(|(_, row)| row).collect();
    order
}

/// Compares two rows under `sort`, on the underlying values. The absent
/// estimated size (the `-` cell) orders after every present value in BOTH
/// directions, so the dashes sit at the bottom whichever way the column sorts.
fn compare(a: &PlaylistRow, b: &PlaylistRow, sort: Sort) -> Ordering {
    let dir = |ordering: Ordering| if sort.ascending { ordering } else { ordering.reverse() };
    match sort.column {
        SortColumn::File => dir(a.name.cmp(&b.name)),
        SortColumn::Group => dir(a.group.cmp(&b.group)),
        SortColumn::Length => dir(a.length_ticks.cmp(&b.length_ticks)),
        SortColumn::Measured => dir(a.measured_bytes.cmp(&b.measured_bytes)),
        SortColumn::Estimated => match (a.estimated_bytes, b.estimated_bytes) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => dir(x.cmp(&y)),
        },
    }
}

/// The playlist name shown in the `Playlist File` column: the file name, plus a
/// ` [NN Chapters]` suffix (zero-padded to two digits) when `show_chapters` is
/// on and the playlist has more than one chapter — the same annotation
/// `BDInfo` appends under its `DisplayChapterCount` setting.
fn playlist_display_name(name: &str, chapter_count: usize, show_chapters: bool) -> String {
    if show_chapters && chapter_count > 1 {
        format!("{name} [{chapter_count:02} Chapters]")
    } else {
        name.to_owned()
    }
}

/// Formats one structured row into its display cells, under `view`'s display
/// toggles (the chapter suffix, human-readable sizes).
fn display_row(row: &PlaylistRow, view: ViewSettings) -> TableRow {
    TableRow {
        number: row.position.to_string(),
        group: row.group.to_string(),
        file: playlist_display_name(&row.name, row.chapter_count, view.display_chapter_count),
        length: row.length.clone(),
        estimated_bytes: estimated_cell(row.estimated_bytes, view.human_readable_sizes),
        measured_bytes: byte_cell(row.measured_bytes, view.human_readable_sizes),
    }
}

/// Formats the structured rows into the display rows the table renders.
pub(crate) fn display_rows(rows: &[PlaylistRow], view: ViewSettings) -> Vec<TableRow> {
    rows.iter().map(|row| display_row(row, view)).collect()
}

/// Whether any listed playlist hides a stream — drives the CLI's footer note.
pub(crate) fn any_hidden(rows: &[PlaylistRow]) -> bool {
    rows.iter().any(|row| row.has_hidden_streams)
}

/// What a [`PlaylistFilter`] withholds from the playlist table — the count,
/// the rules that withheld it, and the withheld names.
///
/// The two rules are judged **independently**, each against the whole disc: a
/// playlist that is both short and looping is counted once and names both
/// rules, and revealing it takes switching both off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenPlaylists {
    /// The withheld playlists' names, in the order the table would have listed
    /// them (length descending, then name ascending).
    names: Vec<String>,
    /// Whether the short rule withheld at least one of them.
    short: bool,
    /// Whether the looping rule withheld at least one of them.
    looping: bool,
}

impl HiddenPlaylists {
    /// The withheld playlists' names, table order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// How many playlists are withheld — one per playlist, however many rules
    /// withheld it.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.names.len()
    }

    /// The rules that withheld something, as the line's parenthesised list:
    /// `short`, `looping`, or `short, looping`. Never empty — a withheld
    /// playlist failed at least one rule.
    #[must_use]
    pub fn reasons(&self) -> String {
        let mut reasons = Vec::new();
        if self.short {
            reasons.push(HiddenRule::Short.label());
        }
        if self.looping {
            reasons.push(HiddenRule::Looping.label());
        }
        reasons.join(", ")
    }

    /// The transient filter that reveals exactly these playlists: `filter` with
    /// the rules that withheld something switched off, the rest untouched — so
    /// a reveal adds these rows and no others.
    #[must_use]
    pub const fn revealing(&self, filter: &PlaylistFilter) -> PlaylistFilter {
        PlaylistFilter {
            filter_short_playlists: filter.filter_short_playlists && !self.short,
            short_playlist_seconds: filter.short_playlist_seconds,
            filter_looping_playlists: filter.filter_looping_playlists && !self.looping,
        }
    }

    /// The hidden-count line under the table: what the filters withhold, or —
    /// while the transient reveal is on (`revealed`) — what they would.
    #[must_use]
    pub fn line(&self, revealed: bool) -> String {
        let count = self.count();
        let plural = if count == 1 { "" } else { "s" };
        let reasons = self.reasons();
        if revealed {
            format!("Showing {count} filtered playlist{plural} ({reasons})")
        } else {
            format!("{count} playlist{plural} hidden by filters ({reasons})")
        }
    }
}

/// The playlists `filter` withholds from `playlists` — `None` when it
/// withholds none (there is then no line to draw).
///
/// One aggregate view of [`hidden_by`]: every withheld playlist counted once,
/// and each rule named if it withheld anything. A rule that is switched OFF
/// never names itself — the short threshold keeps its configured value while
/// its switch is off, so a looping playlist under that threshold must not be
/// reported as short.
#[must_use]
pub fn hidden_playlists(
    playlists: &[PlaylistSummary],
    filter: &PlaylistFilter,
) -> Option<HiddenPlaylists> {
    let hidden = hidden_by(playlists, filter);
    if hidden.is_empty() {
        return None;
    }
    Some(HiddenPlaylists {
        names: hidden.iter().map(|(playlist, _)| playlist.name.clone()).collect(),
        short: hidden.iter().any(|(_, rules)| rules.contains(&HiddenRule::Short)),
        looping: hidden.iter().any(|(_, rules)| rules.contains(&HiddenRule::Looping)),
    })
}

#[cfg(test)]
mod tests {
    use bdinfo_rs_core::bdrom::disc::{PlaylistSummary, fixtures};
    use bdinfo_rs_core::bdrom::order::{PlaylistFilter, table_rows};

    use super::{
        PlaylistRow, Sort, SortColumn, TableRow, ViewSettings, any_hidden, byte_cell, compare,
        display_rows, estimated_cell, format_file_size, hidden_playlists, playlist_display_name,
        playlist_rows, sort_rows,
    };
    use crate::settings::Settings;

    /// A `PlaylistSummary` carrying just what the view-model reads: name, length,
    /// the two file sizes, and its clip names (each clip as long as the
    /// playlist).
    fn sample_playlist(
        name: &str,
        total_length: f64,
        file_size: u64,
        interleaved_file_size: u64,
        clips: &[&str],
    ) -> PlaylistSummary {
        PlaylistSummary {
            file_size,
            interleaved_file_size,
            ..fixtures::playlist(name, total_length, fixtures::clips(clips, total_length))
        }
    }

    /// A four-playlist disc: 00000 (100 s) shares clip A with 00001 (50 s) →
    /// group 1; 00002 (70 s, clip B) → group 2; 00003 (5 s) is dropped by the
    /// short filter.
    fn disc() -> [PlaylistSummary; 4] {
        [
            sample_playlist("00000.MPLS", 100.0, 1000, 0, &["A.M2TS"]),
            sample_playlist("00001.MPLS", 50.0, 500, 0, &["A.M2TS"]),
            sample_playlist("00002.MPLS", 70.0, 0, 2000, &["B.M2TS"]),
            sample_playlist("00003.MPLS", 5.0, 100, 0, &["C.M2TS"]),
        ]
    }

    #[test]
    fn the_filter_settings_reach_the_row_construction() {
        // Filters off: the 5 s playlist joins the table (last — shortest).
        let everything = ViewSettings {
            filter_short_playlists: false,
            filter_looping_playlists: false,
            ..ViewSettings::default()
        };
        assert_eq!(table_rows(&disc(), &everything.filter()), [(1, 0), (1, 1), (2, 2), (3, 3)]);
        // A zeroed threshold keeps it too, even with the switch on.
        let zero = ViewSettings { short_playlist_seconds: 0, ..ViewSettings::default() };
        assert_eq!(table_rows(&disc(), &zero.filter()).len(), 4);
        // A raised threshold drops more: only the 100 s and 70 s rows survive.
        let raised = ViewSettings { short_playlist_seconds: 60, ..ViewSettings::default() };
        let rows = playlist_rows(&disc(), &raised.filter());
        let names: Vec<_> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["00000.MPLS", "00002.MPLS"]);
    }

    #[test]
    fn view_settings_project_the_persisted_settings() {
        // The defaults are today's exact table…
        assert_eq!(ViewSettings::default(), ViewSettings::from_settings(&Settings::default()));
        assert_eq!(ViewSettings::default().filter(), PlaylistFilter::default());
        // …and every field reads through from the store.
        let stored = Settings {
            filter_short_playlists: false,
            short_playlist_seconds: 45,
            filter_looping_playlists: false,
            human_readable_sizes: true,
            display_chapter_count: false,
            report_stream_diagnostics: false,
            report_quick_summary: false,
            keep_partial: false,
            ..Settings::default()
        };
        let view = ViewSettings::from_settings(&stored);
        assert!(!view.filter_short_playlists);
        assert_eq!(view.short_playlist_seconds, 45);
        assert!(!view.filter_looping_playlists);
        assert!(view.human_readable_sizes);
        assert!(!view.display_chapter_count);
        assert!(!view.report_stream_diagnostics);
        assert!(!view.report_quick_summary);
        assert!(!view.keep_partial);
        let filter = view.filter();
        assert!(!filter.filter_short_playlists);
        assert!((filter.short_playlist_seconds - 45.0).abs() < f64::EPSILON);
        assert!(!filter.filter_looping_playlists);
    }

    #[test]
    fn render_options_project_the_two_report_switches() {
        // The defaults build the locked default render (both sections on)…
        let defaults = ViewSettings::default().render_options();
        assert!(defaults.stream_diagnostics);
        assert!(defaults.quick_summary);
        // …and each switch reads through independently.
        let trimmed = ViewSettings { report_stream_diagnostics: false, ..ViewSettings::default() };
        assert!(!trimmed.render_options().stream_diagnostics);
        assert!(trimmed.render_options().quick_summary);
        let bare = ViewSettings { report_quick_summary: false, ..ViewSettings::default() };
        assert!(bare.render_options().stream_diagnostics);
        assert!(!bare.render_options().quick_summary);
    }

    #[test]
    fn scan_options_project_the_retention_switch() {
        // The defaults keep a partially read stream file's measurements…
        assert!(ViewSettings::default().scan_options().keep_partial);
        // …and the switch off reaches the core options that drop them.
        let dropping = ViewSettings { keep_partial: false, ..ViewSettings::default() };
        assert!(!dropping.scan_options().keep_partial);
    }

    #[test]
    fn playlist_rows_carry_the_structured_columns() {
        let rows = playlist_rows(&disc(), &PlaylistFilter::default());
        let view: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row.position,
                    row.group,
                    row.name.as_str(),
                    row.length.as_str(),
                    row.estimated_bytes,
                    row.has_hidden_streams,
                )
            })
            .collect();
        assert_eq!(
            view,
            [
                (1, 1, "00000.MPLS", "00:01:40", Some(1000), false),
                (2, 1, "00001.MPLS", "00:00:50", Some(500), false),
                // group 2; the interleaved size is preferred over the m2ts size.
                (3, 2, "00002.MPLS", "00:01:10", Some(2000), false),
            ]
        );
    }

    #[test]
    fn display_rows_pre_format_every_cell() {
        // The display rows are exactly the CLI table cells: positions/groups as
        // decimals, lengths as hh:mm:ss, estimated bytes thousands-grouped.
        assert_eq!(
            display_rows(
                &playlist_rows(&disc(), &PlaylistFilter::default()),
                ViewSettings::default()
            ),
            [
                TableRow {
                    number: "1".to_owned(),
                    group: "1".to_owned(),
                    file: "00000.MPLS".to_owned(),
                    length: "00:01:40".to_owned(),
                    estimated_bytes: "1,000".to_owned(),
                    measured_bytes: "0".to_owned(),
                },
                TableRow {
                    number: "2".to_owned(),
                    group: "1".to_owned(),
                    file: "00001.MPLS".to_owned(),
                    length: "00:00:50".to_owned(),
                    estimated_bytes: "500".to_owned(),
                    measured_bytes: "0".to_owned(),
                },
                TableRow {
                    number: "3".to_owned(),
                    group: "2".to_owned(),
                    file: "00002.MPLS".to_owned(),
                    length: "00:01:10".to_owned(),
                    estimated_bytes: "2,000".to_owned(),
                    measured_bytes: "0".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn estimated_cell_is_a_dash_when_absent() {
        assert_eq!(estimated_cell(None, false), "-");
        assert_eq!(estimated_cell(Some(2_000_000), false), "2,000,000");
        // The dash is format-independent; a present size follows the toggle.
        assert_eq!(estimated_cell(None, true), "-");
        assert_eq!(estimated_cell(Some(2_000_000), true), "1.91 MB");
    }

    #[test]
    fn byte_cells_follow_the_human_readable_toggle() {
        assert_eq!(byte_cell(78_000_000_000, false), "78,000,000,000");
        assert_eq!(byte_cell(78_000_000_000, true), "72.64 GB");
        // A zero (unmeasured) count renders "0" either way — today's cell.
        assert_eq!(byte_cell(0, false), "0");
        assert_eq!(byte_cell(0, true), "0");
    }

    #[test]
    fn the_display_toggles_reach_the_cells() {
        let mut playlists = disc();
        playlists.get_mut(0).expect("the first playlist").chapter_count = 12;
        let rows = playlist_rows(&playlists, &PlaylistFilter::default());
        let human = ViewSettings {
            human_readable_sizes: true,
            display_chapter_count: false,
            ..ViewSettings::default()
        };
        let cells = display_rows(&rows, human);
        let first = cells.first().expect("the feature row");
        // The chapter suffix is gated off, the sizes read human-readable.
        assert_eq!(first.file, "00000.MPLS");
        assert_eq!(first.estimated_bytes, "1,000.00 B");
        assert_eq!(first.measured_bytes, "0");
        // The defaults render the chapter suffix and grouped-byte sizes.
        let default_cells = display_rows(&rows, ViewSettings::default());
        let first = default_cells.first().expect("the feature row");
        assert_eq!(first.file, "00000.MPLS [12 Chapters]");
        assert_eq!(first.estimated_bytes, "1,000");
    }

    #[test]
    fn format_file_size_picks_the_binary_unit_with_two_decimals() {
        // Zero is the special "0" case, like BDInfo.
        assert_eq!(format_file_size(0), "0");
        // Under 1 KiB stays in bytes.
        assert_eq!(format_file_size(512), "512.00 B");
        // Exact powers of 1024 promote to the next unit.
        assert_eq!(format_file_size(1024), "1.00 KB");
        assert_eq!(format_file_size(1_048_576), "1.00 MB");
        assert_eq!(format_file_size(1_073_741_824), "1.00 GB");
        // Halfway values round to nearest.
        assert_eq!(format_file_size(1536), "1.50 KB");
        // A feature-disc-scale size lands in GB, grouped and rounded.
        assert_eq!(format_file_size(78_000_000_000), "72.64 GB");
        // The unit ladder tops out at EB rather than indexing past the table.
        assert_eq!(format_file_size(u64::MAX), "16.00 EB");
    }

    /// The shared vector table this app and the browser demo both assert; its
    /// own header documents the columns and the three deliberate divergences.
    /// The demo asserts the same file from its Node test suite, so a formatter
    /// change on either side that is not mirrored on the other fails there.
    const SIZE_VECTORS: &str = include_str!("../tests/size-vectors.tsv");

    #[test]
    fn byte_cells_match_the_shared_vector_table() {
        let mut rows = 0_usize;
        for line in SIZE_VECTORS.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split('\t');
            let mut next = || fields.next().unwrap_or_default();
            let (bytes, exact, human) = (next(), next(), next());
            let bytes: u64 = bytes.parse().unwrap_or_default();
            assert_eq!(byte_cell(bytes, false), exact, "exact cell for {bytes} bytes");
            assert_eq!(byte_cell(bytes, true), human, "human-readable cell for {bytes} bytes");
            rows = rows.saturating_add(1);
        }
        // A mis-parsed or truncated table would assert nothing and still pass.
        assert_eq!(rows, 14, "every vector in the shared table is asserted");
    }

    #[test]
    fn any_hidden_reflects_the_rows() {
        let rows = playlist_rows(&disc(), &PlaylistFilter::default());
        assert!(!any_hidden(&rows));
        let hidden = [PlaylistRow {
            position: 1,
            playlist_index: 0,
            group: 1,
            name: "00000.MPLS".to_owned(),
            chapter_count: 0,
            length: "00:00:30".to_owned(),
            length_ticks: 300_000_000,
            estimated_bytes: None,
            measured_bytes: 0,
            has_hidden_streams: true,
        }];
        assert!(any_hidden(&hidden));
        assert!(!any_hidden(&[]));
    }

    /// The filtered `disc()` rows: 00000 (100 s, est 1000), 00001 (50 s,
    /// est 500), 00002 (70 s, est 2000) — see [`disc`].
    fn rows() -> Vec<PlaylistRow> {
        playlist_rows(&disc(), &PlaylistFilter::default())
    }

    /// The row names in order, for a compact order assertion.
    fn names(rows: &[PlaylistRow]) -> Vec<&str> {
        rows.iter().map(|row| row.name.as_str()).collect()
    }

    #[test]
    fn click_starts_ascending_and_flips_on_repeat() {
        // The disc rows' lengths (100, 50, 70 s) are NOT ascending, so the
        // first click on Length sorts ascending.
        let first = Sort::click(None, SortColumn::Length, &rows());
        assert_eq!(first, Sort { column: SortColumn::Length, ascending: true });
        // A repeat click on the same column flips, whatever the rows read.
        let second = Sort::click(Some(first), SortColumn::Length, &rows());
        assert_eq!(second, Sort { column: SortColumn::Length, ascending: false });
        let third = Sort::click(Some(second), SortColumn::Length, &rows());
        assert_eq!(third, Sort { column: SortColumn::Length, ascending: true });
    }

    #[test]
    fn a_first_click_on_an_already_ascending_column_goes_descending() {
        // The presentation order is already ascending by name (00000, 00001,
        // 00002), so the first click on File must visibly re-order: descending.
        let by_name = Sort::click(None, SortColumn::File, &rows());
        assert_eq!(by_name, Sort { column: SortColumn::File, ascending: false });
        // Same probe when arriving from another column's sort.
        let from_length = Sort { column: SortColumn::Length, ascending: false };
        let moved = Sort::click(Some(from_length), SortColumn::File, &rows());
        assert_eq!(moved, Sort { column: SortColumn::File, ascending: false });
        // And a not-ascending target column still starts ascending.
        let other = Sort::click(Some(from_length), SortColumn::Estimated, &rows());
        assert_eq!(other, Sort { column: SortColumn::Estimated, ascending: true });
    }

    #[test]
    fn sort_rows_orders_by_the_underlying_values_and_returns_the_permutation() {
        // Length ascending: 00001 (50 s), 00002 (70 s), 00000 (100 s).
        let mut by_length = rows();
        let order = sort_rows(&mut by_length, Sort { column: SortColumn::Length, ascending: true });
        assert_eq!(names(&by_length), ["00001.MPLS", "00002.MPLS", "00000.MPLS"]);
        assert_eq!(order, [1, 2, 0]);
        // Descending reverses it.
        let mut desc = rows();
        sort_rows(&mut desc, Sort { column: SortColumn::Length, ascending: false });
        assert_eq!(names(&desc), ["00000.MPLS", "00002.MPLS", "00001.MPLS"]);
        // File descending: ordinal on the name.
        let mut by_file = rows();
        sort_rows(&mut by_file, Sort { column: SortColumn::File, ascending: false });
        assert_eq!(names(&by_file), ["00002.MPLS", "00001.MPLS", "00000.MPLS"]);
        // Estimated ascending: 500, 1000, 2000.
        let mut by_est = rows();
        sort_rows(&mut by_est, Sort { column: SortColumn::Estimated, ascending: true });
        assert_eq!(names(&by_est), ["00001.MPLS", "00000.MPLS", "00002.MPLS"]);
    }

    #[test]
    fn sort_rows_is_stable_on_ties() {
        // Every measured size is 0 (nothing scanned), so a measured sort is all
        // ties — the current (presentation) order must hold, both directions.
        for ascending in [true, false] {
            let mut tied = rows();
            let order = sort_rows(&mut tied, Sort { column: SortColumn::Measured, ascending });
            assert_eq!(names(&tied), ["00000.MPLS", "00001.MPLS", "00002.MPLS"]);
            assert_eq!(order, [0, 1, 2]);
        }
    }

    #[test]
    fn an_absent_estimated_size_orders_last_in_both_directions() {
        // 00002's sizes zeroed → its estimate is the absent `-` cell.
        let mut playlists = disc();
        let dashed = playlists.get_mut(2).expect("the third playlist");
        dashed.file_size = 0;
        dashed.interleaved_file_size = 0;
        for ascending in [true, false] {
            let mut sorted = playlist_rows(&playlists, &PlaylistFilter::default());
            sort_rows(&mut sorted, Sort { column: SortColumn::Estimated, ascending });
            assert_eq!(
                sorted.last().map(|row| row.name.as_str()),
                Some("00002.MPLS"),
                "the dash sorts last (ascending: {ascending})"
            );
        }
        // Every comparator arm with an absent side: a dash is Greater than any
        // present value, Less the other way round, and two dashes tie —
        // whatever the direction.
        let rows = playlist_rows(&playlists, &PlaylistFilter::default());
        let (some, none) = (rows.first().expect("a sized row"), rows.last().expect("the dash row"));
        assert_eq!(none.estimated_bytes, None);
        for ascending in [true, false] {
            let sort = Sort { column: SortColumn::Estimated, ascending };
            assert_eq!(compare(none, some, sort), std::cmp::Ordering::Greater);
            assert_eq!(compare(some, none, sort), std::cmp::Ordering::Less);
            assert_eq!(compare(none, none, sort), std::cmp::Ordering::Equal);
        }
    }

    #[test]
    fn playlist_name_gains_a_chapter_suffix_only_past_one_chapter() {
        // Zero or one chapter: just the name (BDInfo shows no suffix for a single
        // or absent chapter mark).
        assert_eq!(playlist_display_name("00000.MPLS", 0, true), "00000.MPLS");
        assert_eq!(playlist_display_name("00000.MPLS", 1, true), "00000.MPLS");
        // More than one: the zero-padded ` [NN Chapters]` annotation.
        assert_eq!(playlist_display_name("00002.MPLS", 12, true), "00002.MPLS [12 Chapters]");
        assert_eq!(playlist_display_name("00002.MPLS", 5, true), "00002.MPLS [05 Chapters]");
        // The DisplayChapterCount toggle gates the suffix off entirely.
        assert_eq!(playlist_display_name("00002.MPLS", 12, false), "00002.MPLS");
    }

    /// The four-playlist disc with a looping row added: 00004 (80 s) loops,
    /// so the default filter withholds it AND the 5 s 00003 — one row per
    /// rule, plus 00005 (3 s) which loops and is short, failing both.
    fn filtered_disc() -> Vec<PlaylistSummary> {
        let mut looping = sample_playlist("00004.MPLS", 80.0, 400, 0, &["D.M2TS"]);
        looping.has_loops = true;
        let mut both = sample_playlist("00005.MPLS", 3.0, 90, 0, &["E.M2TS"]);
        both.has_loops = true;
        let mut playlists = disc().to_vec();
        playlists.push(looping);
        playlists.push(both);
        playlists
    }

    #[test]
    fn nothing_is_hidden_when_the_filters_withhold_nothing() {
        // Every playlist passes: no line to draw.
        assert!(hidden_playlists(&disc(), &PlaylistFilter::everything()).is_none());
        // …and a disc whose every playlist passes the DEFAULT filter likewise.
        let long: Vec<_> = disc().into_iter().filter(|p| p.total_length > 20.0).collect();
        assert!(hidden_playlists(&long, &PlaylistFilter::default()).is_none());
    }

    #[test]
    fn each_rule_names_itself_and_a_playlist_failing_both_counts_once() {
        let hidden = hidden_playlists(&filtered_disc(), &PlaylistFilter::default())
            .expect("the default filter withholds three rows");
        // 00003 (short), 00004 (looping), 00005 (both) — counted once each,
        // ordered as the table would list them: length desc, then name asc.
        assert_eq!(hidden.count(), 3);
        assert_eq!(hidden.names(), ["00004.MPLS", "00003.MPLS", "00005.MPLS"]);
        assert_eq!(hidden.reasons(), "short, looping");
    }

    #[test]
    fn a_playlist_exactly_at_the_threshold_is_not_short() {
        // The filter keeps a playlist of exactly the threshold length, so a
        // 20 s looping one is withheld for looping ALONE.
        let mut edge = sample_playlist("00006.MPLS", 20.0, 200, 0, &["F.M2TS"]);
        edge.has_loops = true;
        let hidden =
            hidden_playlists(&[edge], &PlaylistFilter::default()).expect("the loop is withheld");
        assert_eq!(hidden.reasons(), "looping");
    }

    #[test]
    fn withheld_playlists_of_equal_length_order_by_name() {
        // Three 5 s rows: length descending leaves them tied, so the name
        // decides — ascending, like the table's own presentation order.
        let mut playlists = disc().to_vec();
        playlists.push(sample_playlist("00009.MPLS", 5.0, 60, 0, &["F.M2TS"]));
        playlists.push(sample_playlist("00007.MPLS", 5.0, 60, 0, &["G.M2TS"]));
        let hidden = hidden_playlists(&playlists, &PlaylistFilter::default())
            .expect("three short rows withheld");
        assert_eq!(hidden.names(), ["00003.MPLS", "00007.MPLS", "00009.MPLS"]);
    }

    #[test]
    fn a_switched_off_rule_never_names_itself() {
        // Looping only: 00004 and 00005 are withheld — 00005 is ALSO under the
        // (retained) 20 s threshold, but the short switch is off, so `short`
        // must not appear.
        let looping_only =
            PlaylistFilter { filter_short_playlists: false, ..PlaylistFilter::default() };
        let hidden = hidden_playlists(&filtered_disc(), &looping_only).expect("two loops withheld");
        assert_eq!(hidden.names(), ["00004.MPLS", "00005.MPLS"]);
        assert_eq!(hidden.reasons(), "looping");
        // Short only: 00003 and 00005 — 00005 loops, but that switch is off.
        let short_only =
            PlaylistFilter { filter_looping_playlists: false, ..PlaylistFilter::default() };
        let hidden = hidden_playlists(&filtered_disc(), &short_only).expect("two shorts withheld");
        assert_eq!(hidden.names(), ["00003.MPLS", "00005.MPLS"]);
        assert_eq!(hidden.reasons(), "short");
    }

    #[test]
    fn the_reveal_filter_drops_only_the_withholding_rules() {
        // Both rules withheld something: both switch off, the threshold stays.
        let filter = PlaylistFilter::default();
        let both = hidden_playlists(&filtered_disc(), &filter).expect("three rows withheld");
        let revealed = both.revealing(&filter);
        assert!(!revealed.filter_short_playlists);
        assert!(!revealed.filter_looping_playlists);
        assert!(
            (revealed.short_playlist_seconds - filter.short_playlist_seconds).abs() < f64::EPSILON
        );
        // Only the short rule withheld anything: the looping switch stays ON,
        // so the reveal adds the short rows and nothing else.
        let shorts_only = hidden_playlists(&disc(), &filter).expect("the 5 s row is withheld");
        let revealed = shorts_only.revealing(&filter);
        assert!(!revealed.filter_short_playlists);
        assert!(revealed.filter_looping_playlists);
    }

    #[test]
    fn the_line_counts_pluralizes_and_flips_to_showing_when_revealed() {
        let one = hidden_playlists(&disc(), &PlaylistFilter::default()).expect("one row withheld");
        assert_eq!(one.line(false), "1 playlist hidden by filters (short)");
        assert_eq!(one.line(true), "Showing 1 filtered playlist (short)");
        let three = hidden_playlists(&filtered_disc(), &PlaylistFilter::default())
            .expect("three rows withheld");
        assert_eq!(three.line(false), "3 playlists hidden by filters (short, looping)");
        assert_eq!(three.line(true), "Showing 3 filtered playlists (short, looping)");
    }

    // The cell formatting is panic-safety-critical (it formats hostile disc
    // values), so amplify the unit cases with property tests.
    mod prop {
        use bdinfo_rs_core::bdrom::order::PlaylistFilter;
        use proptest::prelude::*;

        use super::super::{
            Sort, SortColumn, ViewSettings, display_rows, playlist_rows, sort_rows,
        };
        use super::sample_playlist;

        proptest! {
            // The whole table pipeline — build, format, sort — is total over
            // arbitrary disc shapes: it never panics, positions stay 1-based
            // and dense, every cell renders non-empty, and a sort is a true
            // permutation of the same rows (the returned order re-derives it).
            #[test]
            fn the_table_pipeline_is_total_over_arbitrary_discs(
                discs in proptest::collection::vec(
                    (
                        "[0-9]{5}\\.MPLS",
                        -1.0e6_f64..1.0e6,
                        any::<u64>(),
                        any::<u64>(),
                        0_usize..40,
                        proptest::collection::vec("[A-D]\\.M2TS", 0..3),
                    ),
                    0..8,
                ),
                human in any::<bool>(),
                chapters in any::<bool>(),
                column_pick in 0_usize..5,
                ascending in any::<bool>(),
            ) {
                let playlists: Vec<_> = discs
                    .into_iter()
                    .map(|(name, length, size, interleaved, chapter_count, clips)| {
                        let clip_names: Vec<&str> =
                            clips.iter().map(String::as_str).collect();
                        let mut playlist =
                            sample_playlist(&name, length, size, interleaved, &clip_names);
                        playlist.chapter_count = chapter_count;
                        playlist
                    })
                    .collect();
                let mut rows = playlist_rows(&playlists, &PlaylistFilter::default());
                for (index, row) in rows.iter().enumerate() {
                    prop_assert_eq!(row.position, index.saturating_add(1));
                    prop_assert!(row.playlist_index < playlists.len());
                }
                let view = ViewSettings {
                    human_readable_sizes: human,
                    display_chapter_count: chapters,
                    ..ViewSettings::default()
                };
                for cells in display_rows(&rows, view) {
                    prop_assert!(!cells.number.is_empty());
                    prop_assert!(!cells.group.is_empty());
                    prop_assert!(!cells.file.is_empty());
                    prop_assert_eq!(cells.length.chars().count(), 8);
                    prop_assert!(!cells.estimated_bytes.is_empty());
                    prop_assert!(!cells.measured_bytes.is_empty());
                }
                let column = [
                    SortColumn::File,
                    SortColumn::Group,
                    SortColumn::Length,
                    SortColumn::Estimated,
                    SortColumn::Measured,
                ];
                let sort = Sort {
                    column: column.get(column_pick).copied().unwrap_or(SortColumn::File),
                    ascending,
                };
                let before = rows.clone();
                let order = sort_rows(&mut rows, sort);
                // The order is a permutation of 0..n and re-derives the sort.
                let mut sorted_order = order.clone();
                sorted_order.sort_unstable();
                prop_assert_eq!(sorted_order, (0..before.len()).collect::<Vec<_>>());
                let expected: Vec<_> =
                    order.iter().filter_map(|&old| before.get(old).cloned()).collect();
                prop_assert_eq!(rows, expected);
            }
        }
    }
}
