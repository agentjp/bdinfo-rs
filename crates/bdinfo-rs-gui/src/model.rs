//! Tier A — the pure playlist view-model.
//!
//! These constructors mirror the CLI's playlist table (and the wasm crate's
//! `table_rows` / `table_length` / `estimated_bytes` / `playlist_rows`) over the
//! public [`PlaylistSummary`] type — the GUI cannot import the CLI binary's
//! private helpers, so it reimplements them the same way. No widgets, no IO:
//! plain data in, plain data out, so `view()` is a thin projection and Phase 4
//! can hold this layer to the core bar (100% coverage + 0 missed mutants).

use std::cmp::Ordering;

use bdinfo_rs_core::bdrom::chapters::seconds_to_ticks;
use bdinfo_rs_core::bdrom::disc::PlaylistSummary;
use bdinfo_rs_core::bdrom::order::{PlaylistFilter, presentation_groups};

use crate::settings::Settings;

/// The table's presentation settings — the slice of the persisted
/// [`Settings`] the view-model reads.
///
/// The playlist filter switches and the two display toggles. It travels with
/// the loaded disc (the flow's `Listing`), so a settings change re-derives
/// the rows from the retained `BdRom` — no rescan, mirroring `BDInfo`'s
/// `LoadPlaylists()` re-run.
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
        }
    }

    /// The core playlist filter these settings build — what the row
    /// construction applies in place of the old hard-coded
    /// `PlaylistFilter::default()`.
    #[must_use]
    pub fn filter(&self) -> PlaylistFilter {
        PlaylistFilter {
            filter_short_playlists: self.filter_short_playlists,
            short_playlist_seconds: f64::from(self.short_playlist_seconds),
            filter_looping_playlists: self.filter_looping_playlists,
        }
    }
}

/// One structured playlist row — the machine-readable form of the CLI's
/// `#`/Group/Playlist File/Length/Estimated Bytes table. Mirrors the wasm
/// crate's `PlaylistRow`. (Measured Bytes is omitted: a structural scan never
/// measures, so that column would always read `-` this phase.)
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
    /// Estimated bytes — interleaved `*.ssif` size, else `*.m2ts` size, else
    /// `None` (the `-` cell).
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

/// The playlist table rows as `(group number, playlist index)` pairs in table
/// order: the `filter`-kept set (by default short and looping playlists are
/// dropped), grouped by shared clips, each group longest-first. Mirrors the
/// CLI's `table_rows`.
fn table_rows(playlists: &[PlaylistSummary], filter: &PlaylistFilter) -> Vec<(usize, usize)> {
    presentation_groups(playlists, filter)
        .into_iter()
        .enumerate()
        .flat_map(|(group, members)| {
            members.into_iter().map(move |index| (group.saturating_add(1), index))
        })
        .collect()
}

/// `hh:mm:ss` from playlist seconds, truncated to the tick like the CLI table
/// (hours wrap at 24; no day component).
pub(crate) fn table_length(seconds: f64) -> String {
    let total = seconds_to_ticks(seconds).max(0).checked_div(10_000_000).unwrap_or(0);
    let h = total.checked_div(3600).and_then(|h| h.checked_rem(24)).unwrap_or(0);
    let m = total.checked_div(60).and_then(|m| m.checked_rem(60)).unwrap_or(0);
    let s = total.checked_rem(60).unwrap_or(0);
    format!("{h:02}:{m:02}:{s:02}")
}

/// The estimated byte size shown for a playlist: the interleaved `*.ssif` size
/// when known, else the `*.m2ts` size, else `None`. Mirrors the CLI/wasm.
const fn estimated_bytes(playlist: &PlaylistSummary) -> Option<u64> {
    if playlist.interleaved_file_size > 0 {
        Some(playlist.interleaved_file_size)
    } else if playlist.file_size > 0 {
        Some(playlist.file_size)
    } else {
        None
    }
}

/// `N0` thousands grouping for a byte count (`1234567` → `1,234,567`).
///
/// Mirrors the CLI's `group_n0`. Public so the binary's info box can group the
/// exact disc-size byte count alongside its [`format_file_size`] short form.
#[must_use]
pub fn group_n0(value: u64) -> String {
    let mut grouped = Vec::new();
    for (position, digit) in value.to_string().chars().rev().enumerate() {
        if position > 0 && position.checked_rem(3) == Some(0) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped.into_iter().rev().collect()
}

/// One byte-count cell body: the human-readable short form when the toggle is
/// on (`BDInfo`'s `SizeFormatHR`), else the thousands-grouped exact count.
pub(crate) fn byte_cell(bytes: u64, human: bool) -> String {
    if human { format_file_size(bytes) } else { group_n0(bytes) }
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
    format!("{}.{frac:02} {label}", group_n0(whole))
}

/// Builds the structured selection-table rows over the `filter`-kept set.
/// Mirrors the wasm crate's `playlist_rows`.
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
                length: table_length(playlist.total_length),
                length_ticks: seconds_to_ticks(playlist.total_length),
                estimated_bytes: estimated_bytes(playlist),
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
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| match (rows.get(a), rows.get(b)) {
        (Some(x), Some(y)) => compare(x, y, sort),
        _ => Ordering::Equal,
    });
    *rows = order.iter().filter_map(|&index| rows.get(index).cloned()).collect();
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

#[cfg(test)]
mod tests {
    use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary};
    use bdinfo_rs_core::bdrom::order::PlaylistFilter;

    use super::{
        PlaylistRow, Sort, SortColumn, TableRow, ViewSettings, any_hidden, byte_cell, display_rows,
        estimated_bytes, estimated_cell, format_file_size, group_n0, playlist_display_name,
        playlist_rows, sort_rows, table_length, table_rows,
    };
    use crate::settings::Settings;

    /// A `ClipSummary` carrying just a name + length (the fields the view-model
    /// reads); the measured tallies stay zero.
    fn sample_clip(name: &str, length: f64) -> ClipSummary {
        ClipSummary {
            name: name.to_owned(),
            display_name: name.to_owned(),
            file_size: 0,
            interleaved_file_size: 0,
            angle_index: 0,
            relative_time_in: 0.0,
            length,
            payload_bytes: 0,
            packet_count: 0,
            packet_seconds: 0.0,
            file_seconds: 0.0,
            streams: Vec::new(),
        }
    }

    /// A `PlaylistSummary` carrying just what the view-model reads: name, length,
    /// the two file sizes, and its clip names.
    fn sample_playlist(
        name: &str,
        total_length: f64,
        file_size: u64,
        interleaved_file_size: u64,
        clips: &[&str],
    ) -> PlaylistSummary {
        PlaylistSummary {
            name: name.to_owned(),
            total_length,
            file_size,
            interleaved_file_size,
            chapter_count: 0,
            stream_count: 0,
            angle_count: 0,
            has_loops: false,
            streams: Vec::new(),
            clips: clips.iter().map(|clip| sample_clip(clip, total_length)).collect(),
            chapters: Vec::new(),
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
    fn table_rows_groups_and_filters_like_the_cli() {
        // Sorted by length desc, grouped by shared clip, the short row dropped.
        assert_eq!(table_rows(&disc(), &PlaylistFilter::default()), [(1, 0), (1, 1), (2, 2)]);
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
            ..Settings::default()
        };
        let view = ViewSettings::from_settings(&stored);
        assert!(!view.filter_short_playlists);
        assert_eq!(view.short_playlist_seconds, 45);
        assert!(!view.filter_looping_playlists);
        assert!(view.human_readable_sizes);
        assert!(!view.display_chapter_count);
        let filter = view.filter();
        assert!(!filter.filter_short_playlists);
        assert!((filter.short_playlist_seconds - 45.0).abs() < f64::EPSILON);
        assert!(!filter.filter_looping_playlists);
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
    fn table_length_truncates_and_wraps_the_day() {
        assert_eq!(table_length(0.0), "00:00:00");
        assert_eq!(table_length(3661.0), "01:01:01");
        assert_eq!(table_length(-5.0), "00:00:00"); // a negative length clamps to zero
        assert_eq!(table_length(90_061.0), "01:01:01"); // 25h01m01s wraps the day
    }

    #[test]
    fn estimated_bytes_prefers_interleaved_then_m2ts_then_none() {
        assert_eq!(estimated_bytes(&sample_playlist("X", 1.0, 500, 2000, &[])), Some(2000));
        assert_eq!(estimated_bytes(&sample_playlist("X", 1.0, 500, 0, &[])), Some(500));
        assert_eq!(estimated_bytes(&sample_playlist("X", 1.0, 0, 0, &[])), None);
    }

    #[test]
    fn group_n0_groups_every_three_digits_from_the_right() {
        assert_eq!(group_n0(0), "0");
        assert_eq!(group_n0(7), "7");
        assert_eq!(group_n0(1000), "1,000");
        assert_eq!(group_n0(1_234_567), "1,234,567");
        assert_eq!(group_n0(u64::MAX), "18,446,744,073,709,551,615");
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
        if let Some(playlist) = playlists.get_mut(0) {
            playlist.chapter_count = 12;
        }
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
        // The defaults render exactly as before the settings existed.
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
        if let Some(playlist) = playlists.get_mut(2) {
            playlist.file_size = 0;
            playlist.interleaved_file_size = 0;
        }
        for ascending in [true, false] {
            let mut sorted = playlist_rows(&playlists, &PlaylistFilter::default());
            sort_rows(&mut sorted, Sort { column: SortColumn::Estimated, ascending });
            assert_eq!(
                sorted.last().map(|row| row.name.as_str()),
                Some("00002.MPLS"),
                "the dash sorts last (ascending: {ascending})"
            );
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

    // The cell formatting is panic-safety-critical (it formats hostile disc
    // values), so amplify the unit cases with property tests.
    mod prop {
        use proptest::prelude::*;

        use super::super::{group_n0, table_length};

        proptest! {
            #[test]
            fn group_n0_round_trips_and_commas_every_three(value: u64) {
                let grouped = group_n0(value);
                // Stripping the commas yields the plain decimal.
                let plain: String = grouped.chars().filter(|&c| c != ',').collect();
                prop_assert_eq!(&plain, &value.to_string());
                // One comma for every full group of three past the first digit.
                let digits = value.to_string().chars().count();
                let commas = grouped.chars().filter(|&c| c == ',').count();
                prop_assert_eq!(commas, digits.saturating_sub(1).checked_div(3).unwrap_or(0));
            }

            #[test]
            fn table_length_is_well_formed_hms(seconds in 0.0_f64..1_000_000.0) {
                let formatted = table_length(seconds);
                let mut parts = formatted.split(':');
                let hours = parts.next().and_then(|p| p.parse::<u64>().ok());
                let minutes = parts.next().and_then(|p| p.parse::<u64>().ok());
                let secs = parts.next().and_then(|p| p.parse::<u64>().ok());
                prop_assert!(parts.next().is_none(), "exactly three segments");
                prop_assert_eq!(formatted.chars().count(), 8); // NN:NN:NN
                prop_assert!(hours.is_some_and(|h| h < 24));
                prop_assert!(minutes.is_some_and(|m| m < 60));
                prop_assert!(secs.is_some_and(|s| s < 60));
            }
        }
    }
}
