//! Tier A — the pure playlist selection model.
//!
//! The window lists the standard filtered playlist rows ([`crate::model`]); the
//! user checks the ones to measure, and the measured scan is narrowed to exactly
//! those playlists' clips. This module is the pure half of that: a [`Selection`]
//! of one flag per row (`toggle` / `set_all` / `clear`) plus the two derivations
//! the measured scan needs — the **selected clip set**
//! ([`selection_stream_files`]) and the report **order** ([`selection_order`]),
//! reimplemented over the public core types exactly as the CLI (`main.rs`) and
//! the wasm crate do (the GUI cannot import the CLI binary's private helpers).
//!
//! No widgets, no IO: plain data in, plain data out, so the iced shell is a thin
//! projection and Phase 4 can hold this layer to the core bar (100% coverage + 0
//! missed mutants).

use std::collections::BTreeSet;

use bdinfo_rs_core::bdrom::disc::PlaylistSummary;

use crate::model::PlaylistRow;

/// One checkbox per listed playlist row, parallel to the [`PlaylistRow`] list in
/// table order. The model the table's `Scan` column toggles and the
/// "Select all" / "Select none" actions drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// One flag per row, `true` when the row is checked, in table order.
    checked: Vec<bool>,
}

impl Selection {
    /// A fresh selection over `len` rows, all unchecked — the CLI picker's
    /// starting point (an empty selection means "nothing to scan").
    pub fn new(len: usize) -> Self {
        Self { checked: vec![false; len] }
    }

    /// The number of rows the selection spans.
    pub const fn len(&self) -> usize {
        self.checked.len()
    }

    /// Flips the flag at `index`; an out-of-range index is a no-op (the table
    /// never produces one, but the model stays total).
    pub fn toggle(&mut self, index: usize) {
        if let Some(flag) = self.checked.get_mut(index) {
            *flag = !*flag;
        }
    }

    /// Checks every row ("Select all").
    pub fn set_all(&mut self) {
        for flag in &mut self.checked {
            *flag = true;
        }
    }

    /// Unchecks every row ("Select none").
    pub fn clear(&mut self) {
        for flag in &mut self.checked {
            *flag = false;
        }
    }

    /// Whether the row at `index` is checked (`false` for an out-of-range index).
    pub fn is_checked(&self, index: usize) -> bool {
        self.checked.get(index).copied().unwrap_or(false)
    }

    /// The number of checked rows — the "N selected" count the UI shows.
    pub fn count(&self) -> usize {
        self.checked.iter().filter(|&&flag| flag).count()
    }

    /// Whether any row is checked — gates the "Scan selected" action (an empty
    /// selection scans nothing, like the CLI picker finishing with no input).
    pub fn any(&self) -> bool {
        self.checked.iter().any(|&flag| flag)
    }

    /// Whether every row is checked and there is at least one — drives the
    /// "Select all" / "Select none" toggle's state.
    pub fn all(&self) -> bool {
        !self.checked.is_empty() && self.checked.iter().all(|&flag| flag)
    }

    /// The checked rows' playlist names, in table order — the by-name selection
    /// the measured scan resolves (CLI `--mpls` / picker semantics).
    pub fn selected_names(&self, rows: &[PlaylistRow]) -> Vec<String> {
        rows.iter()
            .enumerate()
            .filter(|&(index, _)| self.is_checked(index))
            .map(|(_, row)| row.name.clone())
            .collect()
    }
}

/// The stream files a selection's packet scan reads: every clip of every selected
/// playlist. Mirrors the CLI's / wasm crate's `selection_stream_files` — the
/// `scan_files` set that narrows [`bdinfo_rs_core::bdrom::disc::BdRom::open_resilient_with`]
/// so an unselected (possibly multi-GB) playlist is never demuxed.
pub fn selection_stream_files(
    playlists: &[PlaylistSummary],
    selection: &[String],
) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    for name in selection {
        if let Some(playlist) = playlists.iter().find(|playlist| &playlist.name == name) {
            files.extend(playlist.clips.iter().map(|clip| clip.name.clone()));
        }
    }
    files
}

/// The report's playlist order for a selection: each selected name mapped to its
/// index into the scanned disc's playlists, in selection order (an unknown name
/// is skipped). Mirrors the CLI's / wasm crate's `selection_order`.
pub fn selection_order(playlists: &[PlaylistSummary], selection: &[String]) -> Vec<usize> {
    selection
        .iter()
        .filter_map(|name| playlists.iter().position(|playlist| &playlist.name == name))
        .collect()
}

#[cfg(test)]
mod tests {
    use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary};

    use super::{Selection, selection_order, selection_stream_files};
    use crate::model::playlist_rows;

    /// A `ClipSummary` carrying just a name (the field the selection reads).
    fn sample_clip(name: &str) -> ClipSummary {
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

    /// A `PlaylistSummary` carrying just a name, length, the two file sizes, and
    /// its clip names — what the view-model + selection read.
    fn sample_playlist(
        name: &str,
        total_length: f64,
        file_size: u64,
        clips: &[&str],
    ) -> PlaylistSummary {
        PlaylistSummary {
            name: name.to_owned(),
            total_length,
            file_size,
            interleaved_file_size: 0,
            chapter_count: 0,
            stream_count: 0,
            angle_count: 0,
            has_loops: false,
            streams: Vec::new(),
            clips: clips.iter().map(|clip| sample_clip(clip)).collect(),
            chapters: Vec::new(),
        }
    }

    /// A three-playlist disc whose filtered table lists all three (each ≥ 20 s),
    /// longest-first: 00000 (100 s, clip A) / 00002 (70 s, clips B + C) / 00001
    /// (50 s, clip A). Clip A is shared, so 00000 and 00001 land in one group.
    fn disc() -> [PlaylistSummary; 3] {
        [
            sample_playlist("00000.MPLS", 100.0, 1000, &["A.M2TS"]),
            sample_playlist("00001.MPLS", 50.0, 500, &["A.M2TS"]),
            sample_playlist("00002.MPLS", 70.0, 700, &["B.M2TS", "C.M2TS"]),
        ]
    }

    #[test]
    fn new_selection_is_empty() {
        let selection = Selection::new(3);
        assert_eq!(selection.len(), 3);
        assert_eq!(selection.count(), 0);
        assert!(!selection.any());
        assert!(!selection.all());
        assert!(!selection.is_checked(0));
    }

    #[test]
    fn toggle_flips_one_row() {
        let mut selection = Selection::new(3);
        selection.toggle(1);
        assert!(selection.is_checked(1));
        assert!(!selection.is_checked(0));
        assert_eq!(selection.count(), 1);
        assert!(selection.any());
        assert!(!selection.all());
        selection.toggle(1);
        assert!(!selection.is_checked(1));
        assert_eq!(selection.count(), 0);
    }

    #[test]
    fn toggle_out_of_range_is_a_no_op() {
        let mut selection = Selection::new(2);
        selection.toggle(9);
        assert_eq!(selection.count(), 0);
    }

    #[test]
    fn set_all_then_clear() {
        let mut selection = Selection::new(3);
        selection.set_all();
        assert!(selection.all());
        assert_eq!(selection.count(), 3);
        selection.clear();
        assert!(!selection.any());
        assert_eq!(selection.count(), 0);
    }

    #[test]
    fn all_is_false_for_an_empty_selection() {
        // An all-unchecked zero-row selection is not "all selected".
        assert!(!Selection::new(0).all());
    }

    #[test]
    fn selected_names_follow_table_order() {
        let rows = playlist_rows(&disc());
        // Table order is 00000, 00001 (group 1), then 00002 (group 2).
        let mut selection = Selection::new(rows.len());
        selection.toggle(2); // 00002
        selection.toggle(0); // 00000
        assert_eq!(selection.selected_names(&rows), ["00000.MPLS", "00002.MPLS"]);
    }

    #[test]
    fn stream_files_are_the_selected_playlists_clips() {
        let files = selection_stream_files(&disc(), &["00002.MPLS".to_owned()]);
        // A BTreeSet — sorted + deduped, the scan_files contract.
        assert_eq!(files.into_iter().collect::<Vec<_>>(), ["B.M2TS", "C.M2TS"]);
    }

    #[test]
    fn stream_files_dedupe_a_shared_clip() {
        // 00000 and 00001 share clip A — selecting both yields it once.
        let files =
            selection_stream_files(&disc(), &["00000.MPLS".to_owned(), "00001.MPLS".to_owned()]);
        assert_eq!(files.into_iter().collect::<Vec<_>>(), ["A.M2TS"]);
    }

    #[test]
    fn stream_files_skip_an_unknown_name() {
        assert!(selection_stream_files(&disc(), &["99999.MPLS".to_owned()]).is_empty());
    }

    #[test]
    fn order_maps_names_to_indices_in_selection_order() {
        let order = selection_order(&disc(), &["00002.MPLS".to_owned(), "00000.MPLS".to_owned()]);
        assert_eq!(order, [2, 0]);
    }

    #[test]
    fn order_skips_an_unknown_name() {
        let order = selection_order(&disc(), &["99999.MPLS".to_owned(), "00001.MPLS".to_owned()]);
        assert_eq!(order, [1]);
    }

    // The selection model formats hostile-shaped input (any row count, any
    // toggle index), so amplify the unit cases with property tests.
    mod prop {
        use proptest::prelude::*;

        use super::super::Selection;

        proptest! {
            // A sequence of toggles leaves `count` equal to the number of rows
            // toggled an odd number of times, and `any`/`all` stay consistent.
            #[test]
            fn toggles_keep_count_any_all_consistent(
                len in 0_usize..16,
                toggles in proptest::collection::vec(0_usize..24, 0..64),
            ) {
                let mut selection = Selection::new(len);
                let mut parity = vec![false; len];
                for &index in &toggles {
                    selection.toggle(index);
                    if let Some(flag) = parity.get_mut(index) {
                        *flag = !*flag;
                    }
                }
                let expected = parity.iter().filter(|&&flag| flag).count();
                prop_assert_eq!(selection.count(), expected);
                prop_assert_eq!(selection.any(), expected > 0);
                prop_assert_eq!(selection.all(), len > 0 && expected == len);
                for (index, &flag) in parity.iter().enumerate() {
                    prop_assert_eq!(selection.is_checked(index), flag);
                }
            }

            // set_all then clear is always the empty selection, whatever the
            // prior toggles.
            #[test]
            fn set_all_then_clear_is_empty(
                len in 0_usize..16,
                toggles in proptest::collection::vec(0_usize..16, 0..32),
            ) {
                let mut selection = Selection::new(len);
                for &index in &toggles {
                    selection.toggle(index);
                }
                selection.set_all();
                prop_assert_eq!(selection.count(), len);
                prop_assert_eq!(selection.all(), len > 0);
                selection.clear();
                prop_assert_eq!(selection.count(), 0);
                prop_assert!(!selection.any());
            }
        }
    }
}
