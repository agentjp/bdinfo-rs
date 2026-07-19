//! The pure live-progress model.
//!
//! Ports the CLI's (`main.rs`) `progress_stats` + `hms` math: from a scan's
//! `done` / `total` byte counts and the elapsed wall time it derives the
//! percent, the elapsed seconds, and a remaining-time estimate. The model is
//! pure — `(file, done, total, elapsed) -> ProgressModel` — so the iced progress
//! bar + text line are a thin projection of it, and the boundary cases the CLI
//! pins (an empty scan reads 100%, the first-second estimate already shows) are
//! unit-testable here without a window. The two other pieces of progress-line
//! decision logic live here for the same reason: the indeterminate bar's
//! triangle wave ([`pulse_fraction`] / [`pulse_advance`]) and the worker's
//! event-coalescing rule ([`emit_due`]).

use std::time::Duration;

/// The indeterminate-bar animation period (one full 0→1→0 breath).
pub const PULSE_PERIOD: u16 = 1000;
/// Half the period — the peak of the triangle wave.
const PULSE_HALF: u16 = 500;
/// The phase advance per animation frame (larger = faster breathing).
const PULSE_STEP: u16 = 16;

/// Advances the indeterminate-bar phase by one animation frame, wrapping at
/// [`PULSE_PERIOD`] so the bar breathes forever.
#[must_use]
pub fn pulse_advance(phase: u16) -> u16 {
    phase.wrapping_add(PULSE_STEP).checked_rem(PULSE_PERIOD).unwrap_or(0)
}

/// The indeterminate progress-bar fill for `phase` — a 0→1→0 triangle over the
/// animation period, so the bar "breathes" rather than implying a measured
/// percentage. Always in `0.0..=1.0`.
#[must_use]
pub fn pulse_fraction(phase: u16) -> f32 {
    let phase = phase.checked_rem(PULSE_PERIOD).unwrap_or(0);
    let rise = if phase <= PULSE_HALF { phase } else { PULSE_PERIOD.saturating_sub(phase) };
    f32::from(rise) / f32::from(PULSE_HALF)
}

/// How often a mid-file progress event is worth a UI message — the worker's
/// coalescing interval.
const EMIT_EVERY: Duration = Duration::from_millis(100);

/// Whether a scan-progress event should become a UI message.
///
/// The scan fires its callback every few MB — tens of thousands of times on a
/// feature disc — so the worker coalesces to one message per `EMIT_EVERY`
/// (100 ms), plus every file boundary and the final event, exactly like the
/// CLI throttles its terminal redraw. `since_last` is the time since the last
/// emitted message (`None` before the first, which always emits).
#[must_use]
pub fn emit_due(file_changed: bool, done: u64, total: u64, since_last: Option<Duration>) -> bool {
    file_changed || done == total || since_last.is_none_or(|elapsed| elapsed >= EMIT_EVERY)
}

/// The percent / elapsed-seconds / remaining-seconds triple the progress line
/// draws.
///
/// The percent comes from the byte counts (an empty scan — `total == 0` — reads
/// 100%); the remaining estimate scales the elapsed time by the bytes still to
/// read. The estimate works in **milliseconds**: whole-second math would read
/// zero for the entire first second, exactly when the first plausible estimate
/// should already show.
fn progress_stats(done: u64, total: u64, elapsed: Duration) -> (u64, u64, u64) {
    let percent =
        if total == 0 { 100 } else { done.saturating_mul(100).checked_div(total).unwrap_or(100) };
    let elapsed_seconds = elapsed.as_secs();
    let remaining = total.saturating_sub(done);
    let remaining_seconds = elapsed
        .as_millis()
        .saturating_mul(u128::from(remaining))
        .checked_div(u128::from(done).saturating_mul(1000))
        .map_or(0, |seconds| u64::try_from(seconds).unwrap_or(u64::MAX));
    (percent, elapsed_seconds, remaining_seconds)
}

/// `hh:mm:ss` from whole seconds (no day component — hours accumulate).
fn hms(seconds: u64) -> String {
    let h = seconds.checked_div(3600).unwrap_or(0);
    let m = seconds.checked_div(60).and_then(|m| m.checked_rem(60)).unwrap_or(0);
    let s = seconds.checked_rem(60).unwrap_or(0);
    format!("{h:02}:{m:02}:{s:02}")
}

/// A live-progress snapshot: the current file, the percent, and the estimates.
///
/// The iced shell stores the latest of these while a scan is in flight and
/// projects it onto the bar + text line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressModel {
    /// The stream file currently being demuxed (the CLI line's middle field).
    pub(crate) file: String,
    /// Completion percent (0–100; 100 when there is nothing to read).
    pub(crate) percent: u64,
    /// Whole seconds elapsed since the scan started.
    pub(crate) elapsed_seconds: u64,
    /// The remaining-time estimate, in whole seconds.
    pub(crate) remaining_seconds: u64,
}

impl ProgressModel {
    /// Computes the snapshot from one scan-progress event (`file`/`done`/`total`)
    /// and the wall time elapsed since the scan started.
    pub(crate) fn compute(file: String, done: u64, total: u64, elapsed: Duration) -> Self {
        let (percent, elapsed_seconds, remaining_seconds) = progress_stats(done, total, elapsed);
        Self { file, percent, elapsed_seconds, remaining_seconds }
    }

    /// The bar fill, a fraction in `0.0..=1.0` (the progress bar's value over a
    /// `0.0..=1.0` range), derived from the same percent the text shows.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "percent is 0..=100, exactly representable as f32"
    )]
    pub fn fraction(&self) -> f32 {
        self.percent.min(100) as f32 / 100.0
    }

    /// The stream file currently being demuxed (the bottom bar's lead line).
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// The elapsed wall time as `hh:mm:ss`.
    #[must_use]
    pub fn elapsed_hms(&self) -> String {
        hms(self.elapsed_seconds)
    }

    /// The remaining-time estimate as `hh:mm:ss`.
    #[must_use]
    pub fn remaining_hms(&self) -> String {
        hms(self.remaining_seconds)
    }

    /// The progress text line: `NN% - file | Elapsed: hh:mm:ss | Remaining:
    /// hh:mm:ss`, mirroring the CLI's plain progress spelling (minus the
    /// terminal-only `Scanning` frame the bar widget replaces).
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "{:>3}% - {} | Elapsed: {} | Remaining: {}",
            self.percent,
            self.file,
            hms(self.elapsed_seconds),
            hms(self.remaining_seconds)
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ProgressModel, hms, progress_stats};

    #[test]
    fn an_empty_scan_reads_one_hundred_percent() {
        // total == 0 (a disc with no measurable bytes) is complete, not a
        // divide-by-zero.
        let (percent, _, remaining) = progress_stats(0, 0, Duration::from_secs(5));
        assert_eq!(percent, 100);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn percent_is_the_byte_ratio() {
        assert_eq!(progress_stats(0, 200, Duration::ZERO).0, 0);
        assert_eq!(progress_stats(50, 200, Duration::ZERO).0, 25);
        assert_eq!(progress_stats(200, 200, Duration::ZERO).0, 100);
    }

    #[test]
    fn the_first_second_already_estimates() {
        // Half-read after 500 ms: the ms-based math estimates ~500 ms remaining
        // (reads 0 s), where whole-second math would have shown nothing yet. The
        // point is it does not panic and rounds down to 0 s, not that it is
        // non-zero — the next event past 1 s is the first whole-second estimate.
        let (_, elapsed, remaining) = progress_stats(100, 200, Duration::from_millis(500));
        assert_eq!(elapsed, 0);
        assert_eq!(remaining, 0);
        // Half-read after 4 s → ~4 s remaining.
        assert_eq!(progress_stats(100, 200, Duration::from_secs(4)).2, 4);
    }

    #[test]
    fn remaining_is_zero_before_any_bytes() {
        // done == 0 → the estimate would divide by zero; it reads 0 instead.
        assert_eq!(progress_stats(0, 200, Duration::from_secs(3)).2, 0);
    }

    #[test]
    fn hms_formats_hours_minutes_seconds() {
        assert_eq!(hms(0), "00:00:00");
        assert_eq!(hms(61), "00:01:01");
        assert_eq!(hms(3661), "01:01:01");
        // Hours accumulate past a day (no wrap) — a long scan estimate stays legible.
        assert_eq!(hms(90_061), "25:01:01");
    }

    #[test]
    fn model_projects_fraction_and_line() {
        let model =
            ProgressModel::compute("00000.M2TS".to_owned(), 50, 200, Duration::from_secs(4));
        assert_eq!(model.percent, 25);
        assert!((model.fraction() - 0.25).abs() < f32::EPSILON);
        assert_eq!(model.line(), " 25% - 00000.M2TS | Elapsed: 00:00:04 | Remaining: 00:00:12");
    }

    #[test]
    fn model_exposes_file_and_hms_fields_to_the_shell() {
        // The bottom bar reads these three instead of the combined `line`.
        let model =
            ProgressModel::compute("00000.M2TS".to_owned(), 50, 200, Duration::from_secs(4));
        assert_eq!(model.file(), "00000.M2TS");
        assert_eq!(model.elapsed_hms(), "00:00:04");
        assert_eq!(model.remaining_hms(), "00:00:12");
    }

    #[test]
    fn model_fraction_is_full_on_an_empty_scan() {
        let model = ProgressModel::compute("x".to_owned(), 0, 0, Duration::ZERO);
        assert_eq!(model.percent, 100);
        assert!((model.fraction() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the asserted phases divide the half period exactly in f32"
    )]
    fn the_pulse_rises_to_the_half_period_and_falls_back() {
        // The triangle: 0 at rest, 1.0 exactly at the half period, 0 again at
        // the wrap, symmetric on the way down.
        assert_eq!(super::pulse_fraction(0), 0.0);
        assert!((super::pulse_fraction(250) - 0.5).abs() < f32::EPSILON);
        assert!((super::pulse_fraction(500) - 1.0).abs() < f32::EPSILON);
        assert!((super::pulse_fraction(750) - 0.5).abs() < f32::EPSILON);
        // A phase at (or past) the period reads like its wrapped remainder.
        assert_eq!(super::pulse_fraction(1000), super::pulse_fraction(0));
        assert_eq!(super::pulse_fraction(1250), super::pulse_fraction(250));
    }

    #[test]
    fn the_pulse_phase_advances_and_wraps() {
        assert_eq!(super::pulse_advance(0), 16);
        assert_eq!(super::pulse_advance(984), 0); // 984 + 16 wraps the period
        assert_eq!(super::pulse_advance(992), 8);
    }

    #[test]
    fn progress_events_coalesce_to_the_emit_interval() {
        // The first event of a scan always emits.
        assert!(super::emit_due(false, 1, 100, None));
        // A mid-file event inside the interval is dropped…
        assert!(!super::emit_due(false, 2, 100, Some(Duration::from_millis(50))));
        // …and emits once the interval has passed.
        assert!(super::emit_due(false, 3, 100, Some(Duration::from_millis(100))));
        // A file boundary and the final byte always emit, however recent.
        assert!(super::emit_due(true, 4, 100, Some(Duration::ZERO)));
        assert!(super::emit_due(false, 100, 100, Some(Duration::ZERO)));
    }

    // The estimate math runs over hostile-shaped byte counts and durations, so
    // amplify the unit cases with property tests.
    mod prop {
        use std::time::Duration;

        use proptest::prelude::*;

        use super::super::{ProgressModel, progress_stats};

        /// A `(done, total)` pair with `done <= total` — the real scan invariant
        /// (a packet scan never reports more bytes read than the disc holds).
        fn done_and_total() -> impl Strategy<Value = (u64, u64)> {
            (0_u64..1_000_000).prop_flat_map(|total| (0..=total, Just(total)))
        }

        proptest! {
            #[test]
            fn percent_is_bounded_and_monotone(
                (done, total) in done_and_total(),
                millis in 0_u64..10_000,
            ) {
                let (percent, _, _) = progress_stats(done, total, Duration::from_millis(millis));
                prop_assert!(percent <= 100);
                // Reading one more byte (still within total) never lowers the percent.
                let more = progress_stats(done.saturating_add(1), total, Duration::ZERO).0;
                prop_assert!(more >= percent || done >= total);
            }

            #[test]
            fn fraction_stays_in_unit_range(
                done in 0_u64..1_000_000,
                total in 0_u64..1_000_000,
                millis in 0_u64..10_000,
            ) {
                let model = ProgressModel::compute(
                    String::new(),
                    done,
                    total,
                    Duration::from_millis(millis),
                );
                let fraction = model.fraction();
                prop_assert!((0.0..=1.0).contains(&fraction));
            }

            // The indeterminate bar's fill is a unit-range triangle from ANY
            // phase (the shell only ever feeds wrapped phases, but the math
            // stays total), and advancing stays inside the period.
            #[test]
            fn pulse_stays_in_range_from_any_phase(phase: u16) {
                let fraction = super::super::pulse_fraction(phase);
                prop_assert!((0.0..=1.0).contains(&fraction));
                let next = super::super::pulse_advance(phase);
                prop_assert!(next < super::super::PULSE_PERIOD);
            }
        }
    }
}
