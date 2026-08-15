//! Stream files the packet scan measured materially less of than the disc
//! declares.
//!
//! A `*.m2ts` whose bytes stop before the span its playlist declares reads to a
//! clean end of file: no io error is raised, nothing is recorded, and every
//! per-clip, per-chapter and per-stream number derived from it is simply
//! smaller than the disc says. Nothing else in a scan distinguishes that from a
//! healthy short clip, so [`short_stream_files`] compares the two spans the
//! summaries already carry and names the files where they disagree.
//!
//! This is a report ABOUT a scan, not a line IN the disc report: the classic
//! `BDInfo` format has no wording for a short file, so a surface raises this
//! beside the report (a banner, a stderr line) and the report bytes stay
//! untouched.

use std::collections::BTreeMap;

use super::disc::{ClipSummary, PlaylistSummary};

/// The share of a clip's declared span the demux may come up short by before
/// its file is named: 2%.
///
/// Paired with [`SHORTFALL_SECONDS`]: a file is named only when it misses both,
/// so neither a fraction of a second on a long clip nor a percent of a short
/// one is enough on its own.
const SHORTFALL_FRACTION: f64 = 0.02;

/// The absolute shortfall the same test also requires, in seconds: 1.
///
/// Undamaged media measures at or just past its declared span, and the largest
/// deficit seen on any of it is 0.083 s — two frames at 24 fps, over a
/// 1:45:36.958 commercial single-title disc (measured 2026-08-14). The
/// committed `BigBuckBunny` fixture demuxes 30.155 s of a 30.113 s clip and the
/// synthetic `MultiPlaylist` fixture lands exactly on its declared seconds, so
/// this floor sits ~12× above the worst honest shortfall, and the fraction
/// scales that headroom with the clip.
const SHORTFALL_SECONDS: f64 = 1.0;

/// One stream file whose demuxed span fell materially short of the span the
/// disc declares for it — a truncated or unreadably short `*.m2ts`.
///
/// Built by [`short_stream_files`]; the fields are private and the accessors
/// derive from them, so the check can grow what it reports without breaking a
/// caller.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ShortStreamFile {
    /// The stream file's upper-cased name, e.g. `00011.M2TS`.
    file: String,
    /// The seconds of that file the disc declares, over the clip window this
    /// shortfall was measured in.
    declared_seconds: f64,
    /// The seconds of it the demux actually attributed.
    measured_seconds: f64,
}

impl ShortStreamFile {
    /// The stream file's upper-cased name, e.g. `00011.M2TS`.
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// The seconds the disc declares for it.
    ///
    /// The declared length of one clip window over the file — the worst of them
    /// when several playlists play it (see [`short_stream_files`]), not the
    /// file's whole declared duration.
    #[must_use]
    pub const fn declared_seconds(&self) -> f64 {
        self.declared_seconds
    }

    /// The seconds of that same window the demux attributed to the file.
    #[must_use]
    pub const fn measured_seconds(&self) -> f64 {
        self.measured_seconds
    }

    /// The difference — how much of the declared window never arrived. Always
    /// positive: a file is only reported once the difference passes both
    /// thresholds.
    #[must_use]
    pub const fn missing_seconds(&self) -> f64 {
        self.declared_seconds - self.measured_seconds
    }

    /// The one-line notice a surface raises for this file, beside the report:
    /// `00011.M2TS is shorter than declared: measured 500.0 s of 1640.0 s
    /// (1140.0 s missing)`.
    ///
    /// The single home of that sentence — the CLI prints it on stderr, the
    /// desktop shell banners it and the browser mirror crosses it as a wire
    /// string, all through this method, so the three surfaces cannot word it
    /// differently. Seconds to one decimal, like every other span the surfaces
    /// show a human.
    #[must_use]
    pub fn notice(&self) -> String {
        format!(
            "{} is shorter than declared: measured {:.1} s of {:.1} s ({:.1} s missing)",
            self.file(),
            self.measured_seconds(),
            self.declared_seconds(),
            self.missing_seconds()
        )
    }
}

/// Every stream file in `playlists` whose demuxed span fell materially short of
/// its declared one, one entry per file, sorted by name.
///
/// A file played by several playlists is measured once per clip window; the
/// worst of those windows is the entry, so the number reported is the largest
/// loss the disc can demonstrate for that file. Only a strictly worse window
/// replaces the one held, so equal shortfalls keep the first — and since
/// [`BdRom::playlists`](super::disc::BdRom::playlists) is name-sorted, which
/// window that is does not depend on scan order.
///
/// Silent by construction for everything that was never measured — a
/// metadata-only or codec-only open, and every file a `scan_files` selection
/// left out (see [`is_measured`]) — so the result is evidence of a short file,
/// not of an unscanned one.
pub(crate) fn short_stream_files(playlists: &[PlaylistSummary]) -> Vec<ShortStreamFile> {
    let mut worst: BTreeMap<String, ShortStreamFile> = BTreeMap::new();
    for playlist in playlists {
        for clip in &playlist.clips {
            let Some(short) = clip_shortfall(clip) else {
                continue;
            };
            if worst
                .get(&short.file)
                .is_none_or(|kept| short.missing_seconds() > kept.missing_seconds())
            {
                worst.insert(short.file.clone(), short);
            }
        }
    }
    worst.into_values().collect()
}

/// The shortfall of one clip window, or `None` when the file is not measurably
/// short (or was not measured at all).
///
/// The playlist declares `length` seconds of this stream file and the demux
/// attributed `packet_seconds` of them. Both run from the clip's own declared
/// in-time, so they compare directly whether the play item covers the whole
/// file or a part of it: a play item that ends before the damage measures its
/// full window and stays quiet, while one that reaches past it comes up short.
///
/// Comparisons on the difference, not on a ratio, so a `length` of zero — or
/// any non-finite value a caller-assembled summary could hold — falls out as
/// "not short" instead of dividing.
fn clip_shortfall(clip: &ClipSummary) -> Option<ShortStreamFile> {
    if !is_measured(clip) {
        return None;
    }
    let missing = clip.length - clip.packet_seconds;
    (missing > SHORTFALL_SECONDS.max(clip.length * SHORTFALL_FRACTION)).then(|| ShortStreamFile {
        file: clip.name.clone(),
        declared_seconds: clip.length,
        measured_seconds: clip.packet_seconds,
    })
}

/// Whether `clip`'s stream file was measured at all.
///
/// A clip carries whole-file per-stream tallies exactly when the full
/// measurement pass demuxed its file, so an empty tally list means "not
/// measured" rather than "measured as empty" — which is what keeps an open that
/// measures nothing, and every file left out of a selective scan, out of the
/// result.
///
/// The blind spot it buys: a file too short for a single stream to register
/// leaves the same empty list as a file the scan never opened, so it goes
/// unreported.
const fn is_measured(clip: &ClipSummary) -> bool {
    !clip.streams.is_empty()
}

#[cfg(test)]
mod tests {
    use super::super::disc::{ClipStreamTally, ClipSummary, PlaylistSummary, fixtures};
    use super::{ShortStreamFile, short_stream_files};
    use crate::primitives::Pid;
    use crate::stream::TsStreamType;

    /// A measured clip of `name`: `declared` seconds of play item, `measured`
    /// seconds demuxed, and the one per-stream tally that marks the file as
    /// scanned.
    fn measured_clip(name: &str, declared: f64, measured: f64) -> ClipSummary {
        ClipSummary {
            packet_seconds: measured,
            streams: vec![ClipStreamTally {
                pid: Pid::new(0x1011),
                stream_type: TsStreamType::AvcVideo,
                codec_short_name: "AVC".to_owned(),
                payload_bytes: 1024,
                packet_count: 8,
            }],
            ..fixtures::clip(name, declared)
        }
    }

    /// One playlist named `00000.MPLS` presenting `clips`.
    fn disc_of(clips: Vec<ClipSummary>) -> Vec<PlaylistSummary> {
        vec![fixtures::playlist("00000.MPLS", 100.0, clips)]
    }

    /// The `(file, declared, measured)` triple of each reported file.
    fn reported(playlists: &[PlaylistSummary]) -> Vec<(String, f64, f64)> {
        short_stream_files(playlists)
            .iter()
            .map(|short| {
                (short.file().to_owned(), short.declared_seconds(), short.measured_seconds())
            })
            .collect()
    }

    #[test]
    fn a_file_measured_to_its_declared_span_is_not_reported() {
        // Exactly on, and the just-over a real scan usually lands on (the
        // demuxed span closes on the frame past the declared out-time).
        assert!(reported(&disc_of(vec![measured_clip("00000.M2TS", 100.0, 100.0)])).is_empty());
        assert!(reported(&disc_of(vec![measured_clip("00000.M2TS", 100.0, 100.5)])).is_empty());
    }

    #[test]
    fn a_file_the_demux_ran_out_of_is_reported_with_both_spans() {
        let clips = vec![measured_clip("00011.M2TS", 1640.0, 820.0)];
        assert_eq!(reported(&disc_of(clips)), vec![("00011.M2TS".to_owned(), 1640.0, 820.0)]);
    }

    #[test]
    fn both_thresholds_have_to_be_passed() {
        // Missing 0.5 s of 100 s clears neither the 1 s floor nor the 2 s share.
        assert!(reported(&disc_of(vec![measured_clip("00000.M2TS", 100.0, 99.5)])).is_empty());
        // Missing 15 s of 1000 s clears the 1 s floor but not the 20 s fraction.
        assert!(reported(&disc_of(vec![measured_clip("00000.M2TS", 1000.0, 985.0)])).is_empty());
        // Exactly on the fraction is still not past it; one second more is.
        assert!(reported(&disc_of(vec![measured_clip("00000.M2TS", 1000.0, 980.0)])).is_empty());
        assert_eq!(reported(&disc_of(vec![measured_clip("00000.M2TS", 1000.0, 979.0)])).len(), 1);
        // On a clip short enough for the floor to dominate, the floor is what
        // has to be passed: 2% of 10 s is 0.2 s, and 0.9 s missing is quiet.
        assert!(reported(&disc_of(vec![measured_clip("00000.M2TS", 10.0, 9.1)])).is_empty());
        assert_eq!(reported(&disc_of(vec![measured_clip("00000.M2TS", 10.0, 8.9)])).len(), 1);
    }

    #[test]
    fn a_clip_without_measured_tallies_is_never_reported() {
        // The shape every clip has after an open that measured nothing, and
        // every file a selective scan skipped: a declared length, no demuxed
        // seconds, and no per-stream tallies.
        let unmeasured = fixtures::clip("00000.M2TS", 1640.0);
        assert!(unmeasured.streams.is_empty());
        assert!(reported(&disc_of(vec![unmeasured])).is_empty());
    }

    #[test]
    fn a_zero_length_clip_is_never_reported() {
        // Nothing declared is nothing to miss — and no division to do.
        assert!(reported(&disc_of(vec![measured_clip("00000.M2TS", 0.0, 0.0)])).is_empty());
    }

    #[test]
    fn a_file_played_by_several_playlists_reports_its_worst_window() {
        // The same file under two play items of different lengths, one damaged
        // window each. Both orders, because which entry arrives first decides
        // which comparison arm runs.
        let big = measured_clip("00011.M2TS", 1640.0, 820.0);
        let small = measured_clip("00011.M2TS", 100.0, 40.0);
        let two = |first: &ClipSummary, second: &ClipSummary| {
            vec![
                fixtures::playlist("00000.MPLS", 100.0, vec![first.clone()]),
                fixtures::playlist("00001.MPLS", 100.0, vec![second.clone()]),
            ]
        };
        let worst = vec![("00011.M2TS".to_owned(), 1640.0, 820.0)];
        assert_eq!(reported(&two(&big, &small)), worst);
        assert_eq!(reported(&two(&small, &big)), worst);

        // Two windows missing the same 60 s: the first playlist's stays, so a
        // tie never depends on which window was walked last.
        let early = measured_clip("00011.M2TS", 100.0, 40.0);
        let late = measured_clip("00011.M2TS", 200.0, 140.0);
        assert_eq!(reported(&two(&early, &late)), vec![("00011.M2TS".to_owned(), 100.0, 40.0)]);
    }

    #[test]
    fn the_files_come_back_once_each_sorted_by_name() {
        let clips = vec![
            measured_clip("00033.M2TS", 100.0, 10.0),
            measured_clip("00011.M2TS", 100.0, 20.0),
            measured_clip("00033.M2TS", 100.0, 30.0),
        ];
        let names: Vec<String> =
            reported(&disc_of(clips)).into_iter().map(|(file, _, _)| file).collect();
        assert_eq!(names, vec!["00011.M2TS".to_owned(), "00033.M2TS".to_owned()]);
    }

    #[test]
    fn the_notice_spells_the_sentence_every_surface_raises() {
        // The authoritative byte pin of the shared wording: the CLI, the
        // desktop shell and the browser mirror all render this string through
        // `ShortStreamFile::notice`, and each asserts only that it delegates
        // here — this test is where the bytes are decided.
        let short: Vec<ShortStreamFile> =
            short_stream_files(&disc_of(vec![measured_clip("00011.M2TS", 1640.0, 500.0)]));
        let first = short.first().expect("the short file is reported");
        assert_eq!(
            first.notice(),
            "00011.M2TS is shorter than declared: measured 500.0 s of 1640.0 s (1140.0 s missing)"
        );
    }

    #[test]
    fn the_missing_span_is_the_difference_of_the_two() {
        let short: Vec<ShortStreamFile> =
            short_stream_files(&disc_of(vec![measured_clip("00000.M2TS", 100.0, 40.0)]));
        let first = short.first().expect("the short file is reported");
        assert_eq!(first.missing_seconds().to_bits(), 60.0_f64.to_bits());
    }

    // The two spans are `f64` fields of public, caller-constructible summaries,
    // so the check runs over values no scan produces as well as the ones it
    // does.
    mod prop {
        use proptest::prelude::{prop_assert, prop_assert_eq, proptest};

        use super::super::{SHORTFALL_FRACTION, SHORTFALL_SECONDS, short_stream_files};
        use super::{disc_of, measured_clip};

        proptest! {
            #[test]
            fn a_demux_that_reached_the_declared_span_is_never_reported(
                declared in 0.0_f64..100_000.0,
                over in 0.0_f64..1_000.0,
            ) {
                let clips = vec![measured_clip("00000.M2TS", declared, declared + over)];
                prop_assert!(short_stream_files(&disc_of(clips)).is_empty());
            }

            #[test]
            fn every_reported_file_passed_both_thresholds(
                declared in 0.0_f64..100_000.0,
                measured in -1_000.0_f64..100_000.0,
            ) {
                let clips = vec![measured_clip("00000.M2TS", declared, measured)];
                for short in short_stream_files(&disc_of(clips)) {
                    prop_assert_eq!(
                        short.missing_seconds().to_bits(),
                        (declared - measured).to_bits()
                    );
                    prop_assert!(short.missing_seconds() > SHORTFALL_SECONDS);
                    prop_assert!(short.missing_seconds() > declared * SHORTFALL_FRACTION);
                }
            }

            // Non-finite spans reach the check only from a caller-assembled
            // summary; they must fall out as "not short" rather than panic or
            // report a NaN shortfall.
            #[test]
            fn a_non_finite_span_is_never_reported(measured in -1_000.0_f64..1_000.0) {
                for declared in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                    let clips = vec![measured_clip("00000.M2TS", declared, measured)];
                    prop_assert!(short_stream_files(&disc_of(clips)).is_empty());
                }
            }
        }
    }
}
