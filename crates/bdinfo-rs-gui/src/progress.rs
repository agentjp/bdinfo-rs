//! The pure live-progress model.
//!
//! Wraps the core's [`progress_stats`] + [`hms`] math — percent, elapsed
//! seconds, remaining estimate — in a pure
//! `(file, done, total, elapsed) -> ProgressModel` projection, so the iced
//! progress bar + text line are a thin read of it and are unit-testable without
//! a window. The two other pieces of progress-line decision logic live here for
//! the same reason: the indeterminate bar's triangle wave ([`pulse_fraction`] /
//! [`pulse_advance`]) and the worker's event-coalescing rule ([`emit_due`]).

use std::time::Duration;

use bdinfo_rs_core::bdrom::progress::{hms, progress_stats};

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

/// How long the scan may go without a progress event before the display calls
/// the current read stalled. Progress normally arrives many times a second
/// (a heartbeat before and a count after every read, coalesced to
/// [`EMIT_EVERY`]), so a multi-second silence means one read syscall has not
/// returned — damaged optical media held in drive-firmware retries, which can
/// block for minutes with no error.
const STALL_AFTER: Duration = Duration::from_secs(5);

/// Whether `since_progress` — the time since the last progress event — has
/// crossed the stall threshold ([`STALL_AFTER`], inclusive), so the status
/// line should say the drive is struggling rather than imply normal progress.
#[must_use]
pub(crate) fn stalled(since_progress: Duration) -> bool {
    since_progress >= STALL_AFTER
}

/// Whether a scan-progress event should become a UI message.
///
/// The scan fires its callback before and after every read — hundreds of
/// thousands of times on a feature disc — so the worker coalesces to one message per `EMIT_EVERY`
/// (100 ms), plus every file boundary and the final event, exactly like the
/// CLI throttles its terminal redraw. `since_last` is the time since the last
/// emitted message (`None` before the first, which always emits).
#[must_use]
pub fn emit_due(file_changed: bool, done: u64, total: u64, since_last: Option<Duration>) -> bool {
    file_changed || done == total || since_last.is_none_or(|elapsed| elapsed >= EMIT_EVERY)
}

/// How often the measured tallies are worth a UI message — the worker's
/// grid-tick interval.
///
/// The scan reports them once per demuxed read chunk, which on a fast source is
/// many times a second and on a stalled one not for minutes; the grids are
/// re-rendered per message, so the cells tick on a wall clock instead. One
/// second is the rate classic `BDInfo` samples its live grids at.
const MEASURE_EVERY: Duration = Duration::from_secs(1);

/// Whether a measured snapshot should become a UI message.
///
/// `since_last` is the time since the last emitted one (`None` before the
/// first, which always emits).
#[must_use]
pub fn measure_due(since_last: Option<Duration>) -> bool {
    since_last.is_none_or(|elapsed| elapsed >= MEASURE_EVERY)
}

/// A live-progress snapshot: the current file, the percent, and the estimates.
///
/// The iced shell stores the latest of these while a scan is in flight and
/// projects it onto the bar + text line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressModel {
    /// The stream file currently being demuxed (the CLI line's middle field).
    pub(crate) file: String,
    /// The bytes read as of the last progress event — retained so a
    /// wall-clock tick can re-derive the estimate
    /// ([`set_elapsed`](Self::set_elapsed)).
    done: u64,
    /// The bytes the pass will read, as of the last progress event — the other
    /// half of that re-derivation.
    total: u64,
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
        Self { file, done, total, percent, elapsed_seconds, remaining_seconds }
    }

    /// Re-stamps the wall clock, re-deriving the estimate from the retained
    /// byte counts at the new elapsed time — the same cumulative-average math
    /// [`compute`](Self::compute) ran, so a tick during a stall makes the
    /// estimate CLIMB (more time, no more bytes) instead of holding the value a
    /// now-stale event left. Classic `BDInfo` recomputes its remaining time on
    /// every 1 Hz tick the same way. `file` and `percent` cannot move without a
    /// progress event and do not. Whole seconds, truncated, like every
    /// displayed time.
    pub(crate) fn set_elapsed(&mut self, elapsed: Duration) {
        let (_, elapsed_seconds, remaining_seconds) =
            progress_stats(self.done, self.total, elapsed);
        self.elapsed_seconds = elapsed_seconds;
        self.remaining_seconds = remaining_seconds;
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

    use super::ProgressModel;

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
    fn set_elapsed_restamps_the_clock_and_re_derives_the_estimate() {
        let mut model =
            ProgressModel::compute("00000.M2TS".to_owned(), 50, 200, Duration::from_secs(4));
        // 65.999 s truncates to 00:01:05, and the estimate re-derives from the
        // retained counts at that time: 65.999 s for a quarter of the bytes
        // projects 3 * 65.999 s = 00:03:17 still to read. The file and the
        // percent cannot move without a progress event.
        model.set_elapsed(Duration::from_millis(65_999));
        assert_eq!(model.elapsed_hms(), "00:01:05");
        assert_eq!(model.percent, 25);
        assert_eq!(model.remaining_hms(), "00:03:17");
        assert_eq!(model.file(), "00000.M2TS");
    }

    #[test]
    fn the_estimate_climbs_while_a_read_is_stuck() {
        // No progress event arrives, so `done` stands still while the clock
        // runs: every tick projects a longer remaining time, never the frozen
        // value the last event computed.
        let mut model =
            ProgressModel::compute("00000.M2TS".to_owned(), 50, 200, Duration::from_secs(4));
        let mut previous = model.remaining_seconds;
        assert_eq!(previous, 12);
        for seconds in 5..=10 {
            model.set_elapsed(Duration::from_secs(seconds));
            assert!(
                model.remaining_seconds > previous,
                "the estimate must climb at {seconds} s: {} is not past {previous}",
                model.remaining_seconds
            );
            previous = model.remaining_seconds;
        }
    }

    #[test]
    fn a_tick_before_any_bytes_estimates_nothing() {
        // `done == 0` gives the cumulative average nothing to extrapolate from,
        // so the readout stays at zero however long the clock runs — the same
        // blind window `progress_stats` defines for the first event.
        let mut model = ProgressModel::compute("00000.M2TS".to_owned(), 0, 200, Duration::ZERO);
        model.set_elapsed(Duration::from_secs(30));
        assert_eq!(model.elapsed_hms(), "00:00:30");
        assert_eq!(model.remaining_hms(), "00:00:00");
    }

    #[test]
    fn the_stall_threshold_is_five_seconds_inclusive() {
        assert!(!super::stalled(Duration::from_millis(4_999)));
        assert!(super::stalled(Duration::from_secs(5)));
        assert!(super::stalled(Duration::from_secs(6)));
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

    #[test]
    fn measured_snapshots_tick_once_a_second() {
        // The first snapshot of a scan always emits; the rest wait out the
        // interval, whose boundary is inclusive.
        assert!(super::measure_due(None));
        assert!(!super::measure_due(Some(Duration::from_millis(999))));
        assert!(super::measure_due(Some(Duration::from_secs(1))));
        assert!(super::measure_due(Some(Duration::from_secs(5))));
    }

    // The projection runs over hostile-shaped byte counts and durations, so
    // amplify the unit cases with property tests.
    mod prop {
        use std::time::Duration;

        use proptest::prelude::*;

        use super::super::ProgressModel;

        proptest! {
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
