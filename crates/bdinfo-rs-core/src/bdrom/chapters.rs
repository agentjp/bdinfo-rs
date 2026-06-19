//! Per-chapter video-rate aggregation — the sliding-window walk over the
//! demux's per-frame diagnostics.
//!
//! [`walk_chapters`] consumes the playlist's chapter marks and, clip by clip,
//! the first video stream's [`TsStreamDiagnostics`] list (one entry per frame:
//! payload bytes, timestamp marker, inter-frame interval, picture-type tag) and
//! aggregates one [`ChapterSummary`] per chapter: the average video rate, the
//! peak 1/5/10-second window rates with the time each peak starts, and the
//! average/peak frame sizes.
//!
//! Window semantics (the chapter-table contract):
//! * Each frame enqueues its `(bits, interval)` pair into all three windows. Once a window's
//!   interval sum **exceeds** its span (strictly), its rate `bits_sum / seconds_sum` is a peak
//!   candidate and exactly one entry is dequeued. A candidate is recorded only when it beats the
//!   current peak (strictly) *and* the window start `position - seconds_sum` is strictly positive.
//! * A frame's playlist position is `marker - clip_time_in + clip_relative_in`; entries timestamped
//!   before the clip's in-time are consumed but ignored.
//! * Every per-chapter accumulator — windows, peaks, frame stats — resets at each chapter boundary;
//!   the playlist position does not.
//! * Only main-angle (`angle_index == 0`) clips contribute; angle clips (and clips with no demuxed
//!   diagnostics) are skipped whole.
//!
//! All accumulation is sequential `f64` in a fixed order, so the values are
//! deterministic across runs and platforms. The walk is total: every iteration
//! either advances to the next clip or closes a chapter, so it terminates on
//! every input — including non-finite times, which simply close the chapter —
//! and it never panics.

use std::collections::VecDeque;

use super::m2ts::{TsStreamDiagnostics, bytes_to_f64};

/// One chapter's measured video statistics.
///
/// The raw (unrounded, untruncated) values behind one chapter-table row.
/// Times are seconds relative to the whole playlist; rates are bits per
/// second; sizes are bytes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChapterSummary {
    /// Chapter start time, seconds.
    pub time_in: f64,
    /// Chapter length in seconds — up to the next chapter mark, or to the
    /// playlist's total length for the last chapter.
    pub length: f64,
    /// Average video rate over the chapter, bits/s (`0` for a zero-length
    /// chapter).
    pub avg_rate: f64,
    /// Peak 1-second-window rate, bits/s.
    pub max_1sec_rate: f64,
    /// Start time of the peak 1-second window, seconds.
    pub max_1sec_time: f64,
    /// Peak 5-second-window rate, bits/s.
    pub max_5sec_rate: f64,
    /// Start time of the peak 5-second window, seconds.
    pub max_5sec_time: f64,
    /// Peak 10-second-window rate, bits/s.
    pub max_10sec_rate: f64,
    /// Start time of the peak 10-second window, seconds.
    pub max_10sec_time: f64,
    /// Average frame size in bytes over the frames the codec seam tagged (`0`
    /// when none were).
    pub avg_frame_size: f64,
    /// Largest single-frame payload, bytes.
    pub max_frame_size: f64,
    /// Time of the largest frame, seconds.
    pub max_frame_time: f64,
}

/// One sequenced clip's view for the chapter walk.
///
/// Carries the clip's angle, its in-time, its position on the playlist
/// timeline, and the first video stream's per-frame diagnostics (`None` when
/// the clip was not demuxed or carries no such stream).
#[derive(Debug, Clone, Copy)]
pub struct ChapterClip<'a> {
    /// Angle index; `0` is the main angle (only main-angle clips contribute).
    pub angle_index: i32,
    /// The clip's own in-time, seconds (frame markers are clip-relative).
    pub time_in: f64,
    /// The clip's start on the playlist timeline, seconds.
    pub relative_time_in: f64,
    /// The clip's per-frame video diagnostics, in demux order.
    pub diagnostics: Option<&'a [TsStreamDiagnostics]>,
}

/// Truncates `seconds` to 100 ns ticks — the chapter-table time rule:
/// truncation toward zero, never rounding, wherever seconds are formatted as
/// `h:mm:ss.fff`.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::as_conversions,
    reason = "truncation toward zero is the contract; non-finite input saturates (float→int; TryFrom inapplicable)"
)]
pub fn seconds_to_ticks(seconds: f64) -> i64 {
    (seconds * 10_000_000.0) as i64
}

/// One sliding rate window: queued `(bits, interval)` pairs, their running
/// sums, and the recorded peak.
#[derive(Debug)]
struct Window {
    /// The window span in seconds (1/5/10) the interval sum must exceed.
    span: f64,
    /// Queued per-frame bit counts, oldest first.
    bits: VecDeque<f64>,
    /// Queued per-frame intervals, oldest first.
    seconds: VecDeque<f64>,
    /// Running sum of [`bits`](Self::bits).
    bits_sum: f64,
    /// Running sum of [`seconds`](Self::seconds).
    seconds_sum: f64,
    /// The recorded peak rate, bits/s.
    peak_rate: f64,
    /// Start time of the recorded peak window, seconds.
    peak_location: f64,
}

impl Window {
    /// An empty window over `span` seconds.
    const fn new(span: f64) -> Self {
        Self {
            span,
            bits: VecDeque::new(),
            seconds: VecDeque::new(),
            bits_sum: 0.0,
            seconds_sum: 0.0,
            peak_rate: 0.0,
            peak_location: 0.0,
        }
    }

    /// Enqueues one frame's `(bits, seconds)` at playlist `position`; once the
    /// interval sum strictly exceeds the span, considers the window rate as a
    /// peak (recorded only while the window start is strictly past zero) and
    /// dequeues exactly one entry.
    fn push(&mut self, bits: f64, seconds: f64, position: f64) {
        self.seconds_sum += seconds;
        self.seconds.push_back(seconds);
        self.bits_sum += bits;
        self.bits.push_back(bits);
        if self.seconds_sum > self.span {
            let rate = self.bits_sum / self.seconds_sum;
            if rate > self.peak_rate && position - self.seconds_sum > 0.0 {
                self.peak_rate = rate;
                self.peak_location = position - self.seconds_sum;
            }
            self.bits_sum -= self.bits.pop_front().unwrap_or(0.0);
            self.seconds_sum -= self.seconds.pop_front().unwrap_or(0.0);
        }
    }

    /// Clears the queues, sums, and peak — the per-chapter reset.
    fn reset(&mut self) {
        self.bits.clear();
        self.seconds.clear();
        self.bits_sum = 0.0;
        self.seconds_sum = 0.0;
        self.peak_rate = 0.0;
        self.peak_location = 0.0;
    }
}

/// Walks `clips` against the `chapters` marks (seconds, playlist-relative; the
/// last chapter ends at `total_length`) and aggregates one [`ChapterSummary`]
/// per mark. Returns exactly `chapters.len()` rows.
#[must_use]
pub fn walk_chapters(
    chapters: &[f64],
    total_length: f64,
    clips: &[ChapterClip<'_>],
) -> Vec<ChapterSummary> {
    let mut rows: Vec<ChapterSummary> = Vec::with_capacity(chapters.len());

    let mut window1 = Window::new(1.0);
    let mut window5 = Window::new(5.0);
    let mut window10 = Window::new(10.0);

    let mut chapter_bits = 0.0_f64;
    let mut chapter_frame_count: u64 = 0;
    let mut chapter_max_frame_size = 0.0_f64;
    let mut chapter_max_frame_location = 0.0_f64;

    let mut chapter_position = 0.0_f64;
    let mut chapter_index: usize = 0;
    let mut clip_index: usize = 0;
    let mut diag_index: usize = 0;

    while let Some(&chapter_start) = chapters.get(chapter_index) {
        let chapter_end =
            chapters.get(chapter_index.saturating_add(1)).copied().unwrap_or(total_length);

        let clip = clips.get(clip_index);
        let diag_list = clip.and_then(|c| if c.angle_index == 0 { c.diagnostics } else { None });

        if let (Some(c), Some(diags)) = (clip, diag_list) {
            #[expect(
                clippy::while_float,
                reason = "the chapter close is a float boundary by design; the loop is bounded \
                          by the diagnostics list, and a non-finite end must compare false \
                          (closing the chapter), which an integer recast would lose"
            )]
            while chapter_position < chapter_end {
                let Some(diag) = diags.get(diag_index) else { break };
                diag_index = diag_index.saturating_add(1);
                if diag.marker < c.time_in {
                    continue;
                }
                chapter_position = diag.marker - c.time_in + c.relative_time_in;

                let seconds = diag.interval;
                let bits = bytes_to_f64(diag.bytes) * 8.0;
                chapter_bits += bits;
                if diag.tag.is_some() {
                    chapter_frame_count = chapter_frame_count.saturating_add(1);
                }

                window1.push(bits, seconds, chapter_position);
                window5.push(bits, seconds, chapter_position);
                window10.push(bits, seconds, chapter_position);

                if bits > chapter_max_frame_size * 8.0 {
                    chapter_max_frame_size = bits / 8.0;
                    chapter_max_frame_location = chapter_position;
                }
            }
        }

        // A clip whose usable diagnostics ran dry steps to the next clip;
        // otherwise (frames remain past the chapter end, or no clips remain)
        // the chapter closes. Exactly one of the two arms runs per iteration,
        // which is what makes the walk terminate on every input.
        if diag_list.is_none_or(|d| diag_index >= d.len()) && clip.is_some() {
            clip_index = clip_index.saturating_add(1);
            diag_index = 0;
        } else {
            let chapter_length = chapter_end - chapter_start;
            rows.push(ChapterSummary {
                time_in: chapter_start,
                length: chapter_length,
                avg_rate: if chapter_length > 0.0 { chapter_bits / chapter_length } else { 0.0 },
                max_1sec_rate: window1.peak_rate,
                max_1sec_time: window1.peak_location,
                max_5sec_rate: window5.peak_rate,
                max_5sec_time: window5.peak_location,
                max_10sec_rate: window10.peak_rate,
                max_10sec_time: window10.peak_location,
                avg_frame_size: if chapter_frame_count > 0 {
                    chapter_bits / bytes_to_f64(chapter_frame_count) / 8.0
                } else {
                    0.0
                },
                max_frame_size: chapter_max_frame_size,
                max_frame_time: chapter_max_frame_location,
            });
            chapter_index = chapter_index.saturating_add(1);

            window1.reset();
            window5.reset();
            window10.reset();
            chapter_bits = 0.0;
            chapter_frame_count = 0;
            chapter_max_frame_size = 0.0;
            chapter_max_frame_location = 0.0;
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{ChapterClip, ChapterSummary, seconds_to_ticks, walk_chapters};
    use crate::bdrom::m2ts::TsStreamDiagnostics;

    /// One diagnostics entry: `bytes` of payload at `marker` seconds after an
    /// `interval`-second gap, frame-`tagged` or not.
    fn d(bytes: u64, marker: f64, interval: f64, tagged: bool) -> TsStreamDiagnostics {
        TsStreamDiagnostics {
            bytes,
            packets: 1,
            marker,
            interval,
            tag: tagged.then(|| "I".to_owned()),
        }
    }

    /// A main-angle clip over `diags` starting at `time_in` (clip time) /
    /// `relative_time_in` (playlist time).
    fn clip(time_in: f64, relative_time_in: f64, diags: &[TsStreamDiagnostics]) -> ChapterClip<'_> {
        ChapterClip { angle_index: 0, time_in, relative_time_in, diagnostics: Some(diags) }
    }

    #[test]
    fn ticks_truncate_toward_zero_and_saturate() {
        assert_eq!(seconds_to_ticks(1.5), 15_000_000);
        // Truncation, not rounding: .9999999 of a tick is dropped.
        assert_eq!(seconds_to_ticks(1.999_999_99), 19_999_999);
        assert_eq!(seconds_to_ticks(-1.5), -15_000_000);
        assert_eq!(seconds_to_ticks(0.0), 0);
        assert_eq!(seconds_to_ticks(f64::NAN), 0);
        assert_eq!(seconds_to_ticks(f64::INFINITY), i64::MAX);
    }

    #[test]
    fn a_single_chapter_aggregates_rates_frames_and_window_peaks() {
        // Three frames at 1 s spacing: 1000 B (tagged), 2000 B (tagged),
        // 500 B (untagged).
        let diags = [d(1000, 1.0, 1.0, true), d(2000, 2.0, 1.0, true), d(500, 3.0, 1.0, false)];
        let rows = walk_chapters(&[0.0], 4.0, &[clip(0.0, 0.0, &diags)]);
        assert_eq!(rows.len(), 1);
        let r = rows.first().unwrap();
        assert_eq!(r.time_in.to_bits(), 0.0_f64.to_bits());
        assert_eq!(r.length.to_bits(), 4.0_f64.to_bits());
        // 3500 bytes * 8 over 4 s.
        assert_eq!(r.avg_rate.to_bits(), 7000.0_f64.to_bits());
        // Two tagged frames: 28000 bits / 2 / 8.
        assert_eq!(r.avg_frame_size.to_bits(), 1750.0_f64.to_bits());
        assert_eq!(r.max_frame_size.to_bits(), 2000.0_f64.to_bits());
        assert_eq!(r.max_frame_time.to_bits(), 2.0_f64.to_bits());
        // 1-second window: at frame 2 the sum (2.0 s) first exceeds 1 s, but
        // its start (position − sum = 0) is not strictly positive, so the
        // first recorded peak is frame 3's window: (16000+4000)/2 bits/s
        // starting at 3.0 − 2.0 = 1.0 s.
        assert_eq!(r.max_1sec_rate.to_bits(), 10_000.0_f64.to_bits());
        assert_eq!(r.max_1sec_time.to_bits(), 1.0_f64.to_bits());
        // The interval sum never exceeds 5/10 s: those peaks stay zero.
        assert_eq!(r.max_5sec_rate.to_bits(), 0.0_f64.to_bits());
        assert_eq!(r.max_5sec_time.to_bits(), 0.0_f64.to_bits());
        assert_eq!(r.max_10sec_rate.to_bits(), 0.0_f64.to_bits());
        assert_eq!(r.max_10sec_time.to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn an_equal_window_rate_keeps_the_first_peak() {
        // Constant 1000 B frames at 0.5 s intervals from 1.0 s. At frame 2 the
        // 1-second sum is exactly 1.0 — not strictly above the span, so no
        // drain. Frames 3 and 4 both produce a 16000 bits/s window; the equal
        // later rate must NOT displace the first peak's location.
        let diags = [
            d(1000, 1.0, 0.5, true),
            d(1000, 1.5, 0.5, true),
            d(1000, 2.0, 0.5, true),
            d(1000, 2.5, 0.5, true),
        ];
        let rows = walk_chapters(&[0.0], 3.0, &[clip(0.0, 0.0, &diags)]);
        let r = rows.first().unwrap();
        assert_eq!(r.max_1sec_rate.to_bits(), 16_000.0_f64.to_bits());
        assert_eq!(r.max_1sec_time.to_bits(), 0.5_f64.to_bits());
    }

    #[test]
    fn chapter_boundaries_reset_the_accumulators_but_not_the_position() {
        // Two chapters over one clip; the second chapter's stats must not see
        // the first's frames, windows, or peaks.
        let diags = [
            d(1000, 1.0, 1.0, true),
            d(4000, 2.0, 1.0, true),
            d(1000, 3.0, 1.0, true),
            d(1000, 4.0, 1.0, true),
        ];
        let rows = walk_chapters(&[0.0, 2.0], 4.0, &[clip(0.0, 0.0, &diags)]);
        assert_eq!(rows.len(), 2);
        let (r0, r1) = (rows.first().unwrap(), rows.get(1).unwrap());
        // Chapter 1 takes frames 1-2 (the frame landing exactly on the
        // boundary closes it): 5000 B * 8 / 2 s.
        assert_eq!(r0.avg_rate.to_bits(), 20_000.0_f64.to_bits());
        assert_eq!(r0.max_frame_size.to_bits(), 4000.0_f64.to_bits());
        assert_eq!(r0.max_frame_time.to_bits(), 2.0_f64.to_bits());
        assert_eq!(r0.length.to_bits(), 2.0_f64.to_bits());
        // Chapter 2 takes frames 3-4 only: 2000 B * 8 / 2 s.
        assert_eq!(r1.time_in.to_bits(), 2.0_f64.to_bits());
        assert_eq!(r1.avg_rate.to_bits(), 8000.0_f64.to_bits());
        assert_eq!(r1.avg_frame_size.to_bits(), 1000.0_f64.to_bits());
        assert_eq!(r1.max_frame_size.to_bits(), 1000.0_f64.to_bits());
        assert_eq!(r1.max_frame_time.to_bits(), 3.0_f64.to_bits());
        // Its 1-second window starts fresh: frame 4 closes a 2-second window
        // (16000 bits / 2 s) starting at 4.0 − 2.0 = 2.0 s.
        assert_eq!(r1.max_1sec_rate.to_bits(), 8000.0_f64.to_bits());
        assert_eq!(r1.max_1sec_time.to_bits(), 2.0_f64.to_bits());
    }

    #[test]
    fn frames_before_the_clip_in_time_are_consumed_but_ignored() {
        // The 7000-byte frame at 5.0 s predates the clip's 10.0 s in-time and
        // contributes nothing; the frame exactly AT the in-time counts (the
        // skip is strictly-before, and the distinct sizes make keeping the
        // wrong one observable).
        let diags = [d(7000, 5.0, 1.0, true), d(1000, 10.0, 1.0, true), d(1000, 11.0, 1.0, true)];
        let rows = walk_chapters(&[0.0], 2.0, &[clip(10.0, 0.0, &diags)]);
        let r = rows.first().unwrap();
        // 2000 bytes * 8 over 2 s — 8000, not the 32000 the skipped frame
        // would add or the 4000 of dropping the at-in-time frame.
        assert_eq!(r.avg_rate.to_bits(), 8000.0_f64.to_bits());
        assert_eq!(r.avg_frame_size.to_bits(), 1000.0_f64.to_bits());
    }

    #[test]
    fn the_playlist_position_offsets_the_clip_relative_start() {
        // A second clip starting at playlist time 2.0 whose own timeline
        // begins at 100.0: its frame at marker 101.0 lands at playlist
        // position 101 − 100 + 2 = 3.0.
        let first = [d(1000, 1.0, 1.0, true)];
        let second = [d(4000, 101.0, 1.0, true)];
        let clips = [clip(0.0, 0.0, &first), clip(100.0, 2.0, &second)];
        let rows = walk_chapters(&[0.0], 4.0, &clips);
        let r = rows.first().unwrap();
        assert_eq!(r.max_frame_size.to_bits(), 4000.0_f64.to_bits());
        assert_eq!(r.max_frame_time.to_bits(), 3.0_f64.to_bits());
    }

    #[test]
    fn a_zero_length_chapter_keeps_a_zero_average_rate() {
        // Two marks at the same time: the first chapter is zero-length and its
        // average rate must stay 0, not divide by zero.
        let diags = [d(1000, 1.0, 1.0, true)];
        let rows = walk_chapters(&[0.0, 0.0], 2.0, &[clip(0.0, 0.0, &diags)]);
        assert_eq!(rows.len(), 2);
        let r0 = rows.first().unwrap();
        assert_eq!(r0.length.to_bits(), 0.0_f64.to_bits());
        assert_eq!(r0.avg_rate.to_bits(), 0.0_f64.to_bits());
        // The second chapter still measures the frame.
        assert_eq!(rows.get(1).unwrap().avg_rate.to_bits(), 4000.0_f64.to_bits());
    }

    #[test]
    fn angle_clips_and_undemuxed_clips_are_skipped_whole() {
        let angle_diags = [d(9000, 0.5, 0.5, true)];
        let main_diags = [d(1000, 0.5, 0.5, true), d(1000, 1.5, 1.0, true)];
        let clips = [
            ChapterClip {
                angle_index: 1,
                time_in: 0.0,
                relative_time_in: 0.0,
                diagnostics: Some(&angle_diags),
            },
            ChapterClip { angle_index: 0, time_in: 0.0, relative_time_in: 0.0, diagnostics: None },
            clip(0.0, 0.0, &main_diags),
        ];
        let rows = walk_chapters(&[0.0], 2.0, &clips);
        assert_eq!(rows.len(), 1);
        // Only the third clip's 2000 bytes count: 16000 bits / 2 s.
        assert_eq!(rows.first().unwrap().avg_rate.to_bits(), 8000.0_f64.to_bits());
    }

    #[test]
    fn no_chapters_yield_no_rows_and_no_clips_yield_zero_rows() {
        assert_eq!(walk_chapters(&[], 10.0, &[clip(0.0, 0.0, &[])]), Vec::new());
        let rows = walk_chapters(&[0.0, 4.0], 10.0, &[]);
        assert_eq!(
            rows,
            vec![
                ChapterSummary { time_in: 0.0, length: 4.0, ..ChapterSummary::default() },
                ChapterSummary { time_in: 4.0, length: 6.0, ..ChapterSummary::default() },
            ]
        );
    }

    #[test]
    fn a_non_finite_chapter_end_closes_the_chapter_instead_of_spinning() {
        // With a NaN total length the position comparisons can never advance;
        // the walk must still emit one row per chapter and terminate.
        let diags = [d(1000, 1.0, 1.0, true)];
        let rows = walk_chapters(&[0.0], f64::NAN, &[clip(0.0, 0.0, &diags)]);
        assert_eq!(rows.len(), 1);
        let r = rows.first().unwrap();
        assert!(r.length.is_nan());
        assert_eq!(r.avg_rate.to_bits(), 0.0_f64.to_bits());
    }

    proptest! {
        /// The walk is total and shape-stable over arbitrary inputs — hostile
        /// values (NaN, infinities, huge counts) neither panic nor hang, and
        /// every chapter mark yields exactly one row.
        #[test]
        fn walk_yields_one_row_per_chapter_and_never_panics(
            chapters in proptest::collection::vec(any::<f64>(), 0..6),
            total in any::<f64>(),
            spec in proptest::collection::vec(
                (
                    any::<i32>(),
                    any::<f64>(),
                    any::<f64>(),
                    proptest::option::of(proptest::collection::vec(
                        (any::<u64>(), any::<f64>(), any::<f64>(), any::<bool>()),
                        0..8,
                    )),
                ),
                0..4,
            ),
        ) {
            let stores: Vec<Option<Vec<TsStreamDiagnostics>>> = spec
                .iter()
                .map(|(_, _, _, diags)| {
                    diags.as_ref().map(|list| {
                        list.iter()
                            .map(|&(bytes, marker, interval, tagged)| d(bytes, marker, interval, tagged))
                            .collect()
                    })
                })
                .collect();
            let clips: Vec<ChapterClip<'_>> = spec
                .iter()
                .zip(&stores)
                .map(|(&(angle_index, time_in, relative_time_in, _), store)| ChapterClip {
                    angle_index,
                    time_in,
                    relative_time_in,
                    diagnostics: store.as_deref(),
                })
                .collect();
            let rows = walk_chapters(&chapters, total, &clips);
            prop_assert_eq!(rows.len(), chapters.len());
        }
    }
}
