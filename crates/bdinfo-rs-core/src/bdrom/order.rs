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
//!
//! The projections every surface builds on that order live here too:
//! [`table_rows`] (the grouped rows a selection table prints), the by-name
//! selection family — [`normalize_playlist_name`] and [`named_selection`]
//! (which names a request resolves to), [`selection_order`] and
//! [`selection_stream_files`] (what that selection renders and reads) — and
//! the [`HiddenRule`] classification behind the filter switches.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use super::disc::PlaylistSummary;

/// One reason the playlist filter can withhold a playlist from the
/// presentation order — what a surface names when it reports a hidden
/// playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenRule {
    /// Strictly shorter than the threshold in force — a playlist of exactly
    /// the threshold length matches no rule.
    Short,
    /// Repeats a clip ([`PlaylistSummary::has_loops`]).
    Looping,
}

impl HiddenRule {
    /// The rule's user-facing name, `"short"` or `"looping"` — the string
    /// every surface prints in its hidden-playlist line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::Looping => "looping",
        }
    }
}

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

    /// The rules that match `playlist`, in declaration order
    /// ([`HiddenRule::Short`] first). Judged against
    /// [`short_playlist_seconds`](Self::short_playlist_seconds) alone — the
    /// two switches play no part, so the classification is the same whether
    /// the caller lists the standard set or a widened one, and a caller
    /// holding it can re-apply either rule itself.
    #[must_use]
    pub fn classify(&self, playlist: &PlaylistSummary) -> Vec<HiddenRule> {
        let mut rules = Vec::new();
        if playlist.total_length < self.short_playlist_seconds {
            rules.push(HiddenRule::Short);
        }
        if playlist.has_loops {
            rules.push(HiddenRule::Looping);
        }
        rules
    }

    /// Whether `playlist` passes this filter: true exactly when no rule in
    /// [`classify`](Self::classify) has its filter switch on.
    #[must_use]
    pub fn keeps(&self, playlist: &PlaylistSummary) -> bool {
        !self.classify(playlist).into_iter().any(|rule| self.drops(rule))
    }

    /// Whether this filter's switch for `rule` is on — a matching playlist
    /// is withheld.
    const fn drops(&self, rule: HiddenRule) -> bool {
        match rule {
            HiddenRule::Short => self.filter_short_playlists,
            HiddenRule::Looping => self.filter_looping_playlists,
        }
    }
}

/// Compares two playlists in presentation order: total length descending,
/// then name ascending (ordinal byte order).
///
/// A non-comparable length pair (NaN, impossible for parsed playlists) falls
/// through to the name, mirroring a `>`-based three-way comparison.
///
/// Public because a surface that lists playlists *outside* the presentation
/// order — the hidden-playlist lines, which name what the filter withheld —
/// must still list them in the order the table would have used.
#[must_use]
pub fn presentation_cmp(a: &PlaylistSummary, b: &PlaylistSummary) -> Ordering {
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

/// The playlist table rows as `(group number, playlist index)` pairs in table
/// order.
///
/// [`presentation_groups`] flattened, each member paired with its group's
/// 1-based number — the form a selection table prints.
#[must_use]
pub fn table_rows(playlists: &[PlaylistSummary], filter: &PlaylistFilter) -> Vec<(usize, usize)> {
    presentation_groups(playlists, filter)
        .into_iter()
        .enumerate()
        .flat_map(|(group, members)| {
            members.into_iter().map(move |index| (group.saturating_add(1), index))
        })
        .collect()
}

/// The stream files a selection's packet scan reads: every clip of every
/// selected playlist.
///
/// Sorted and deduplicated — a clip two selected playlists share is read
/// once, and an unknown name contributes nothing.
#[must_use]
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

/// The report's playlist order for a selection: each selected name mapped to
/// its index into `playlists`, in selection order — a repeated name renders
/// its playlist again, an unknown name is skipped.
#[must_use]
pub fn selection_order(playlists: &[PlaylistSummary], selection: &[String]) -> Vec<usize> {
    selection
        .iter()
        .filter_map(|name| playlists.iter().position(|playlist| &playlist.name == name))
        .collect()
}

/// Normalizes a user-typed playlist name to the spelling
/// [`PlaylistSummary::name`] carries: upper-cased, with `.MPLS` appended when
/// the name holds no `.` at all.
///
/// So `00800`, `00800.mpls` and `00800.MPLS` all name the same playlist. A name
/// that already has *some* extension is only upper-cased, never re-suffixed —
/// `feature.m2ts` normalizes to `FEATURE.M2TS` and then matches no playlist,
/// which is the intended outcome for a name that is not a playlist file.
#[must_use]
pub fn normalize_playlist_name(name: &str) -> String {
    let upper = name.to_ascii_uppercase();
    if upper.contains('.') { upper } else { format!("{upper}.MPLS") }
}

/// A by-name playlist selection: each requested name
/// [normalized](normalize_playlist_name) and matched against `playlists`, in
/// the given order, first occurrence winning.
///
/// A name that matches no playlist is skipped and a repeat is dropped, so the
/// result is a duplicate-free subset of the disc's names — the classic
/// selection behaviour, and unfiltered (any parsed playlist is addressable by
/// name, whatever a [`PlaylistFilter`] would withhold).
#[must_use]
pub fn named_selection(playlists: &[PlaylistSummary], requested: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for raw in requested {
        let name = normalize_playlist_name(raw);
        if playlists.iter().any(|playlist| playlist.name == name) && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use proptest::prelude::{prop_assert, prop_assert_eq, proptest};

    use super::{
        HiddenRule, PlaylistFilter, named_selection, normalize_playlist_name, presentation_cmp,
        presentation_groups, presentation_order, selection_order, selection_stream_files,
        table_rows,
    };
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
                    file_size: 0,
                    interleaved_file_size: 0,
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
    fn table_rows_pair_each_playlist_with_its_group_number() {
        // A (100 s) and B (50 s) share a clip — group 1; C (70 s) is group 2;
        // the 5 s D is dropped by the default filter.
        let playlists = [
            playlist("A.MPLS", 100.0, false, &["X.M2TS", "Y.M2TS"]),
            playlist("B.MPLS", 50.0, false, &["Y.M2TS"]),
            playlist("C.MPLS", 70.0, false, &["Z.M2TS"]),
            playlist("D.MPLS", 5.0, false, &["W.M2TS"]),
        ];
        assert_eq!(table_rows(&playlists, &PlaylistFilter::default()), [(1, 0), (1, 1), (2, 2)]);
        assert!(table_rows(&[], &PlaylistFilter::default()).is_empty());
    }

    #[test]
    fn table_rows_follow_the_given_filter() {
        let playlists = [
            playlist("00001.MPLS", 3600.0, true, &["X.M2TS"]),
            playlist("00002.MPLS", 5.0, false, &["Y.M2TS"]),
            playlist("00003.MPLS", 60.0, false, &["Z.M2TS"]),
        ];
        // The default filter leaves only the plain 60-second playlist…
        assert_eq!(table_rows(&playlists, &PlaylistFilter::default()), [(1, 2)]);
        // …turning off one rule admits exactly that rule's playlist, which
        // shares no clip with the keeper, so it opens its own group…
        let keep_loops =
            PlaylistFilter { filter_looping_playlists: false, ..PlaylistFilter::default() };
        assert_eq!(table_rows(&playlists, &keep_loops), [(1, 0), (2, 2)]);
        let keep_short =
            PlaylistFilter { filter_short_playlists: false, ..PlaylistFilter::default() };
        assert_eq!(table_rows(&playlists, &keep_short), [(1, 2), (2, 1)]);
        // …and turning off both lists the whole disc.
        assert_eq!(table_rows(&playlists, &PlaylistFilter::everything()).len(), 3);
    }

    /// A three-playlist disc for the selection projections: 00000 and 00001
    /// share clip A; 00002 reads clips B and C.
    fn selection_disc() -> [PlaylistSummary; 3] {
        [
            playlist("00000.MPLS", 100.0, false, &["A.M2TS"]),
            playlist("00001.MPLS", 50.0, false, &["A.M2TS"]),
            playlist("00002.MPLS", 70.0, false, &["B.M2TS", "C.M2TS"]),
        ]
    }

    #[test]
    fn stream_files_union_the_selected_clips_once() {
        let files = selection_stream_files(&selection_disc(), &["00002.MPLS".to_owned()]);
        assert_eq!(files.into_iter().collect::<Vec<_>>(), ["B.M2TS", "C.M2TS"]);
        // A shared clip lands once, and an unknown name contributes nothing.
        let files = selection_stream_files(
            &selection_disc(),
            &["00000.MPLS".to_owned(), "00001.MPLS".to_owned(), "99999.MPLS".to_owned()],
        );
        assert_eq!(files.into_iter().collect::<Vec<_>>(), ["A.M2TS"]);
        assert!(selection_stream_files(&selection_disc(), &[]).is_empty());
    }

    #[test]
    fn selection_order_repeats_a_pick_and_skips_an_unknown_name() {
        let selection = ["00002.MPLS".to_owned(), "00000.MPLS".to_owned(), "00002.MPLS".to_owned()];
        assert_eq!(selection_order(&selection_disc(), &selection), [2, 0, 2]);
        assert_eq!(selection_order(&selection_disc(), &["99999.MPLS".to_owned()]), [0_usize; 0]);
    }

    #[test]
    fn playlist_names_normalize_to_the_model_spelling() {
        // Bare number, lower-cased and upper-cased `.mpls` all name one playlist.
        assert_eq!(normalize_playlist_name("00800"), "00800.MPLS");
        assert_eq!(normalize_playlist_name("00800.mpls"), "00800.MPLS");
        assert_eq!(normalize_playlist_name("00800.MPLS"), "00800.MPLS");
        // Some other extension is upper-cased, never re-suffixed.
        assert_eq!(normalize_playlist_name("feature.m2ts"), "FEATURE.M2TS");
    }

    #[test]
    fn named_selection_normalizes_dedupes_and_keeps_order() {
        let disc = selection_disc();
        let requested =
            ["00002".to_owned(), "00000.mpls".to_owned(), "99999".to_owned(), "00002".to_owned()];
        // Normalized, unknown skipped, repeat dropped, request order kept.
        assert_eq!(named_selection(&disc, &requested), ["00002.MPLS", "00000.MPLS"]);
        // A request naming nothing on the disc, and an empty request, select nothing.
        assert!(named_selection(&disc, &["X".to_owned()]).is_empty());
        assert!(named_selection(&disc, &[]).is_empty());
    }

    #[test]
    fn presentation_cmp_orders_longest_first_then_by_name() {
        let long = playlist("00010.MPLS", 100.0, false, &[]);
        let short = playlist("00001.MPLS", 50.0, false, &[]);
        let tie = playlist("00020.MPLS", 100.0, false, &[]);
        assert_eq!(presentation_cmp(&long, &short), Ordering::Less);
        assert_eq!(presentation_cmp(&short, &long), Ordering::Greater);
        // Equal lengths fall through to the ordinal name…
        assert_eq!(presentation_cmp(&long, &tie), Ordering::Less);
        // …as does a non-comparable pair, which no parsed playlist produces:
        // `00005` sorts ahead of `00010` however their lengths compare.
        let nan = playlist("00005.MPLS", f64::NAN, false, &[]);
        assert_eq!(presentation_cmp(&nan, &long), Ordering::Less);
        assert_eq!(presentation_cmp(&nan, &nan), Ordering::Equal);
    }

    #[test]
    fn classify_names_the_matching_rules_short_first() {
        let filter = PlaylistFilter::default();
        assert_eq!(
            filter.classify(&playlist("A.MPLS", 5.0, true, &[])),
            [HiddenRule::Short, HiddenRule::Looping]
        );
        assert_eq!(filter.classify(&playlist("B.MPLS", 5.0, false, &[])), [HiddenRule::Short]);
        assert_eq!(filter.classify(&playlist("C.MPLS", 100.0, true, &[])), [HiddenRule::Looping]);
        assert!(filter.classify(&playlist("D.MPLS", 100.0, false, &[])).is_empty());
        // Exactly the threshold is not short — the filter drops strictly
        // shorter playlists, so the classification agrees.
        assert!(filter.classify(&playlist("E.MPLS", 20.0, false, &[])).is_empty());
    }

    #[test]
    fn classify_reads_the_threshold_but_not_the_switches() {
        // Both switches off: the classification is unchanged…
        let off = PlaylistFilter {
            filter_short_playlists: false,
            filter_looping_playlists: false,
            ..PlaylistFilter::default()
        };
        assert_eq!(
            off.classify(&playlist("A.MPLS", 5.0, true, &[])),
            [HiddenRule::Short, HiddenRule::Looping]
        );
        // …and the threshold in force decides "short": 50 s is short under a
        // raised 60 s threshold.
        let raised = PlaylistFilter { short_playlist_seconds: 60.0, ..PlaylistFilter::default() };
        assert_eq!(raised.classify(&playlist("B.MPLS", 50.0, false, &[])), [HiddenRule::Short]);
    }

    #[test]
    fn labels_are_the_printed_rule_names() {
        assert_eq!(HiddenRule::Short.label(), "short");
        assert_eq!(HiddenRule::Looping.label(), "looping");
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

        /// `keeps` is true exactly when every rule in `classify` has its
        /// filter switch off — the filter and the classification never drift.
        #[test]
        fn keeps_iff_no_classified_rule_switch_is_on(
            length in 0.0_f64..40.0,
            has_loops in proptest::bool::ANY,
            filter_short in proptest::bool::ANY,
            filter_looping in proptest::bool::ANY,
            threshold in 0.0_f64..40.0,
        ) {
            let subject = playlist("00000.MPLS", length, has_loops, &[]);
            let filter = PlaylistFilter {
                filter_short_playlists: filter_short,
                short_playlist_seconds: threshold,
                filter_looping_playlists: filter_looping,
            };
            let via_rules = filter.classify(&subject).into_iter().all(|rule| match rule {
                HiddenRule::Short => !filter.filter_short_playlists,
                HiddenRule::Looping => !filter.filter_looping_playlists,
            });
            prop_assert_eq!(filter.keeps(&subject), via_rules);
            // The same verdict from the raw fields, pinning both sides.
            let dropped_short = filter_short && length < threshold;
            let dropped_looping = filter_looping && has_loops;
            prop_assert_eq!(filter.keeps(&subject), !(dropped_short || dropped_looping));
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
