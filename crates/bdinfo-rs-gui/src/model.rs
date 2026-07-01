//! Tier A — the pure playlist view-model.
//!
//! These constructors mirror the CLI's playlist table (and the wasm crate's
//! `table_rows` / `table_length` / `estimated_bytes` / `playlist_rows`) over the
//! public [`PlaylistSummary`] type — the GUI cannot import the CLI binary's
//! private helpers, so it reimplements them the same way. No widgets, no IO:
//! plain data in, plain data out, so `view()` is a thin projection and Phase 4
//! can hold this layer to the core bar (100% coverage + 0 missed mutants).

use bdinfo_rs_core::bdrom::chapters::seconds_to_ticks;
use bdinfo_rs_core::bdrom::disc::PlaylistSummary;
use bdinfo_rs_core::bdrom::order::{PlaylistFilter, presentation_groups};

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
    /// `hh:mm:ss` total length, truncated like the CLI table.
    pub(crate) length: String,
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
/// order: the standard filtered set (short and looping playlists dropped),
/// grouped by shared clips, each group longest-first. Mirrors the CLI's
/// `table_rows`.
fn table_rows(playlists: &[PlaylistSummary]) -> Vec<(usize, usize)> {
    presentation_groups(playlists, &PlaylistFilter::default())
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

/// The `Estimated Bytes` cell: thousands-grouped when known, else `-`.
fn estimated_cell(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "-".to_owned(), group_n0)
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

/// Builds the structured selection-table rows over the standard filtered set.
/// Mirrors the wasm crate's `playlist_rows`.
pub(crate) fn playlist_rows(playlists: &[PlaylistSummary]) -> Vec<PlaylistRow> {
    table_rows(playlists)
        .into_iter()
        .enumerate()
        .filter_map(|(position, (group, index))| {
            playlists.get(index).map(|playlist| PlaylistRow {
                position: position.saturating_add(1),
                playlist_index: index,
                group,
                name: playlist.name.clone(),
                length: table_length(playlist.total_length),
                estimated_bytes: estimated_bytes(playlist),
                measured_bytes: playlist.total_angle_packet_size(),
                has_hidden_streams: playlist.has_hidden_streams(),
            })
        })
        .collect()
}

/// Formats one structured row into its display cells.
fn display_row(row: &PlaylistRow) -> TableRow {
    TableRow {
        number: row.position.to_string(),
        group: row.group.to_string(),
        file: row.name.clone(),
        length: row.length.clone(),
        estimated_bytes: estimated_cell(row.estimated_bytes),
        measured_bytes: group_n0(row.measured_bytes),
    }
}

/// Formats the structured rows into the display rows the table renders.
pub(crate) fn display_rows(rows: &[PlaylistRow]) -> Vec<TableRow> {
    rows.iter().map(display_row).collect()
}

/// Whether any listed playlist hides a stream — drives the CLI's footer note.
pub(crate) fn any_hidden(rows: &[PlaylistRow]) -> bool {
    rows.iter().any(|row| row.has_hidden_streams)
}

#[cfg(test)]
mod tests {
    use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary};

    use super::{
        PlaylistRow, TableRow, any_hidden, display_rows, estimated_bytes, estimated_cell,
        format_file_size, group_n0, playlist_rows, table_length, table_rows,
    };

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
        assert_eq!(table_rows(&disc()), [(1, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn playlist_rows_carry_the_structured_columns() {
        let rows = playlist_rows(&disc());
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
            display_rows(&playlist_rows(&disc())),
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
        assert_eq!(estimated_cell(None), "-");
        assert_eq!(estimated_cell(Some(2_000_000)), "2,000,000");
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
        let rows = playlist_rows(&disc());
        assert!(!any_hidden(&rows));
        let hidden = [PlaylistRow {
            position: 1,
            playlist_index: 0,
            group: 1,
            name: "00000.MPLS".to_owned(),
            length: "00:00:30".to_owned(),
            estimated_bytes: None,
            measured_bytes: 0,
            has_hidden_streams: true,
        }];
        assert!(any_hidden(&hidden));
        assert!(!any_hidden(&[]));
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
