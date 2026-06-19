//! Playlist presentation order — filtering, grouping, and sorting.
//!
//! Reports do not list playlists by file name. The presentation order is built
//! in three steps over the scanned [`PlaylistSummary`] rows:
//!
//! 1. **Sort** every playlist by total length descending, then name ascending (ordinal) —
//!    [`presentation_cmp`].
//! 2. **Filter + group** in that order: a playlist dropped by the [`PlaylistFilter`] is skipped; a
//!    kept playlist joins the first existing group containing a playlist that shares any clip file
//!    with it, or starts a new group. Unparsable playlists never reach the model, so they are
//!    filtered upstream by construction.
//! 3. **Concatenate** the groups in creation order. Because members join in the sorted scan order,
//!    every group is itself already sorted by the same comparison — the longest playlist's group
//!    comes first, and each group runs longest-first.
//!
//! The result is a list of indices into the playlist slice, so callers keep
//! the name-ordered [`crate::bdrom::disc::BdRom::playlists`] untouched and
//! apply the presentation order on top.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use super::disc::PlaylistSummary;

/// The playlist filter switches for the presentation order. The defaults drop
/// short and looping playlists — the standard report behaviour;
/// [`PlaylistFilter::everything`] keeps both.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistFilter {
    /// Drop playlists shorter than
    /// [`short_playlist_seconds`](Self::short_playlist_seconds). Default `true`.
    pub filter_short_playlists: bool,
    /// The short-playlist threshold in seconds; a playlist of exactly this
    /// length is kept. Default `20.0`.
    pub short_playlist_seconds: f64,
    /// Drop looping playlists ([`PlaylistSummary::has_loops`]). Default `true`.
    pub filter_looping_playlists: bool,
}

impl Default for PlaylistFilter {
    fn default() -> Self {
        Self {
            filter_short_playlists: true,
            short_playlist_seconds: 20.0,
            filter_looping_playlists: true,
        }
    }
}

impl PlaylistFilter {
    /// A filter that keeps every playlist — no short or looping filtering.
    #[must_use]
    pub const fn everything() -> Self {
        Self {
            filter_short_playlists: false,
            short_playlist_seconds: 0.0,
            filter_looping_playlists: false,
        }
    }

    /// Whether `playlist` passes this filter.
    #[must_use]
    pub fn keeps(&self, playlist: &PlaylistSummary) -> bool {
        if self.filter_short_playlists && playlist.total_length < self.short_playlist_seconds {
            return false;
        }
        !(self.filter_looping_playlists && playlist.has_loops)
    }
}

/// Compares two playlists in presentation order: total length descending,
/// then name ascending (ordinal byte order). A non-comparable length pair
/// (NaN, impossible for parsed playlists) falls through to the name, mirroring
/// a `>`-based three-way comparison.
fn presentation_cmp(a: &PlaylistSummary, b: &PlaylistSummary) -> Ordering {
    b.total_length
        .partial_cmp(&a.total_length)
        .unwrap_or(Ordering::Equal)
        .then_with(|| a.name.cmp(&b.name))
}

/// Builds the presentation *groups* over `playlists` under `filter`.
///
/// See the module docs for the three steps: each inner list is one
/// shared-clip group of indices into `playlists`, groups in creation order
/// and each group sorted longest-first. [`presentation_order`] is the
/// concatenation.
#[must_use]
pub fn presentation_groups(
    playlists: &[PlaylistSummary],
    filter: &PlaylistFilter,
) -> Vec<Vec<usize>> {
    // The clip-file name sets, index-aligned with `playlists` — what "shares
    // any clip" tests against.
    let clip_names: Vec<BTreeSet<&str>> = playlists
        .iter()
        .map(|playlist| playlist.clips.iter().map(|clip| clip.name.as_str()).collect())
        .collect();

    let mut sorted: Vec<(usize, &PlaylistSummary, &BTreeSet<&str>)> =
        playlists.iter().zip(&clip_names).enumerate().map(|(i, (p, n))| (i, p, n)).collect();
    sorted.sort_by(|x, y| presentation_cmp(x.1, y.1));

    let mut groups: Vec<Vec<(usize, &BTreeSet<&str>)>> = Vec::new();
    for (index, playlist, names) in sorted {
        if !filter.keeps(playlist) {
            continue;
        }
        let target = groups
            .iter_mut()
            .find(|group| group.iter().any(|(_, member)| !names.is_disjoint(member)));
        match target {
            Some(group) => group.push((index, names)),
            None => groups.push(vec![(index, names)]),
        }
    }

    // Members joined each group in sorted scan order, so the groups are
    // already internally sorted.
    groups.into_iter().map(|group| group.into_iter().map(|(index, _)| index).collect()).collect()
}

/// Builds the presentation order over `playlists` under `filter` — the
/// [`presentation_groups`] concatenated in creation order, returning indices
/// into `playlists`.
#[must_use]
pub fn presentation_order(playlists: &[PlaylistSummary], filter: &PlaylistFilter) -> Vec<usize> {
    presentation_groups(playlists, filter).into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{prop_assert, prop_assert_eq, proptest};

    use super::{PlaylistFilter, presentation_groups, presentation_order};
    use crate::bdrom::disc::{ClipSummary, PlaylistSummary};

    /// A playlist summary carrying only what the presentation order reads:
    /// name, total length, loop flag, and its clip names.
    fn playlist(name: &str, total_length: f64, has_loops: bool, clips: &[&str]) -> PlaylistSummary {
        PlaylistSummary {
            name: name.to_owned(),
            total_length,
            file_size: 0,
            interleaved_file_size: 0,
            chapter_count: 0,
            stream_count: 0,
            angle_count: 0,
            has_loops,
            streams: Vec::new(),
            clips: clips
                .iter()
                .map(|clip| ClipSummary {
                    name: (*clip).to_owned(),
                    display_name: (*clip).to_owned(),
                    angle_index: 0,
                    relative_time_in: 0.0,
                    length: total_length,
                    payload_bytes: 0,
                    packet_count: 0,
                    packet_seconds: 0.0,
                    file_seconds: 0.0,
                    streams: Vec::new(),
                })
                .collect(),
            chapters: Vec::new(),
        }
    }

    /// Maps ordered indices back to playlist names for readable assertions.
    fn names(playlists: &[PlaylistSummary], order: &[usize]) -> Vec<String> {
        order.iter().filter_map(|&i| playlists.get(i).map(|p| p.name.clone())).collect()
    }

    #[test]
    fn orders_groups_by_first_appearance_of_a_shared_clip() {
        // Sorted by length: A (100), C (70), B (50). B shares a clip with A, so
        // it joins A's group and precedes the longer, unrelated C.
        let playlists = [
            playlist("A.MPLS", 100.0, false, &["X.M2TS", "Y.M2TS"]),
            playlist("B.MPLS", 50.0, false, &["Y.M2TS"]),
            playlist("C.MPLS", 70.0, false, &["Z.M2TS"]),
        ];
        let order = presentation_order(&playlists, &PlaylistFilter::default());
        assert_eq!(names(&playlists, &order), ["A.MPLS", "B.MPLS", "C.MPLS"]);
    }

    #[test]
    fn groups_chain_through_a_shared_member() {
        // D shares no clip with A but shares one with B, which already joined
        // A's group — D lands in that group too (the match is against any
        // member, not the group founder).
        let playlists = [
            playlist("A.MPLS", 100.0, false, &["X.M2TS", "Y.M2TS"]),
            playlist("B.MPLS", 50.0, false, &["Y.M2TS", "W.M2TS"]),
            playlist("C.MPLS", 70.0, false, &["Z.M2TS"]),
            playlist("D.MPLS", 30.0, false, &["W.M2TS"]),
        ];
        let order = presentation_order(&playlists, &PlaylistFilter::default());
        assert_eq!(names(&playlists, &order), ["A.MPLS", "B.MPLS", "D.MPLS", "C.MPLS"]);
    }

    #[test]
    fn equal_lengths_fall_back_to_ordinal_names() {
        let playlists = [
            playlist("00010.MPLS", 60.0, false, &["B.M2TS"]),
            playlist("00002.MPLS", 60.0, false, &["A.M2TS"]),
            playlist("00001.MPLS", 30.0, false, &["C.M2TS"]),
        ];
        let order = presentation_order(&playlists, &PlaylistFilter::default());
        assert_eq!(names(&playlists, &order), ["00002.MPLS", "00010.MPLS", "00001.MPLS"]);
    }

    #[test]
    fn short_filter_drops_below_the_threshold_only() {
        // 19.999 s is dropped; exactly 20 s is kept (the filter drops strictly
        // shorter playlists).
        let playlists = [
            playlist("SHORT.MPLS", 19.999, false, &["A.M2TS"]),
            playlist("EDGE.MPLS", 20.0, false, &["B.M2TS"]),
        ];
        let order = presentation_order(&playlists, &PlaylistFilter::default());
        assert_eq!(names(&playlists, &order), ["EDGE.MPLS"]);

        // With the short filter off, both stay.
        let keep_short =
            PlaylistFilter { filter_short_playlists: false, ..PlaylistFilter::default() };
        assert_eq!(presentation_order(&playlists, &keep_short).len(), 2);
    }

    #[test]
    fn looping_filter_is_independent_of_the_short_filter() {
        let playlists = [
            playlist("LOOP.MPLS", 100.0, true, &["A.M2TS"]),
            playlist("PLAIN.MPLS", 90.0, false, &["B.M2TS"]),
        ];
        let order = presentation_order(&playlists, &PlaylistFilter::default());
        assert_eq!(names(&playlists, &order), ["PLAIN.MPLS"]);

        // With only the looping filter off, the loop returns in length order.
        let keep_loops =
            PlaylistFilter { filter_looping_playlists: false, ..PlaylistFilter::default() };
        let order = presentation_order(&playlists, &keep_loops);
        assert_eq!(names(&playlists, &order), ["LOOP.MPLS", "PLAIN.MPLS"]);

        // `everything` keeps a playlist that is both short and looping.
        let playlists =
            [playlist("TINY.MPLS", 1.0, true, &["A.M2TS"]), playlist("B.MPLS", 2.0, false, &[])];
        let order = presentation_order(&playlists, &PlaylistFilter::everything());
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn empty_input_yields_an_empty_order() {
        assert!(presentation_order(&[], &PlaylistFilter::default()).is_empty());
        assert!(presentation_groups(&[], &PlaylistFilter::default()).is_empty());
    }

    #[test]
    fn presentation_groups_exposes_the_group_boundaries() {
        // Same disc as the first-appearance test: A+B share a clip and form
        // group 1; the unrelated C is group 2 on its own.
        let playlists = [
            playlist("A.MPLS", 100.0, false, &["X.M2TS", "Y.M2TS"]),
            playlist("B.MPLS", 50.0, false, &["Y.M2TS"]),
            playlist("C.MPLS", 70.0, false, &["Z.M2TS"]),
        ];
        let groups = presentation_groups(&playlists, &PlaylistFilter::default());
        assert_eq!(groups, [vec![0, 1], vec![2]]);
    }

    proptest! {
        /// The order is always a permutation of exactly the kept indices.
        #[test]
        fn order_is_a_permutation_of_the_kept_indices(
            lengths in proptest::collection::vec(0.0_f64..200.0, 0..12),
            loops in proptest::collection::vec(proptest::bool::ANY, 0..12),
        ) {
            let playlists: Vec<PlaylistSummary> = lengths
                .iter()
                .zip(loops.iter().chain(std::iter::repeat(&false)))
                .enumerate()
                .map(|(i, (&len, &lp))| playlist(&format!("{i:05}.MPLS"), len, lp, &[]))
                .collect();
            let filter = PlaylistFilter::default();
            let mut order = presentation_order(&playlists, &filter);
            order.sort_unstable();
            let kept: Vec<usize> = playlists
                .iter()
                .enumerate()
                .filter(|(_, p)| filter.keeps(p))
                .map(|(i, _)| i)
                .collect();
            prop_assert_eq!(order, kept);
        }

        /// `everything` keeps every playlist, and the first entry is the
        /// longest (ties broken by name) — the head of the first group.
        #[test]
        fn everything_keeps_all_and_leads_with_the_longest(
            lengths in proptest::collection::vec(0.0_f64..200.0, 1..12),
        ) {
            let playlists: Vec<PlaylistSummary> = lengths
                .iter()
                .enumerate()
                .map(|(i, &len)| playlist(&format!("{i:05}.MPLS"), len, false, &[]))
                .collect();
            let order = presentation_order(&playlists, &PlaylistFilter::everything());
            prop_assert_eq!(order.len(), playlists.len());
            let first = order.first().and_then(|&i| playlists.get(i));
            let leads = first.is_some_and(|p| {
                playlists.iter().all(|q| {
                    p.total_length > q.total_length
                        || (p.total_length.to_bits() == q.total_length.to_bits()
                            && p.name <= q.name)
                })
            });
            prop_assert!(leads);
        }

        /// Deterministic: the same input always yields the same order.
        #[test]
        fn order_is_deterministic(
            lengths in proptest::collection::vec(0.0_f64..200.0, 0..12),
        ) {
            let playlists: Vec<PlaylistSummary> = lengths
                .iter()
                .enumerate()
                .map(|(i, &len)| playlist(&format!("{i:05}.MPLS"), len, false, &["S.M2TS"]))
                .collect();
            let filter = PlaylistFilter::default();
            prop_assert_eq!(
                presentation_order(&playlists, &filter),
                presentation_order(&playlists, &filter)
            );
        }
    }
}
