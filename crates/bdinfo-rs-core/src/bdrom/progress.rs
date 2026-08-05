//! The scan-progress line's arithmetic.
//!
//! A surface that draws live progress over a packet scan — a terminal line, a
//! progress bar — turns each [`ScanProgress`](super::disc::ScanProgress)
//! observation plus the wall time since the scan started into the same three
//! numbers ([`progress_stats`]) and spells the two times the same way
//! ([`hms`]). The line format and the redraw throttle are the surface's own;
//! only the arithmetic is shared, so two surfaces watching one scan cannot
//! disagree about how far along it is.
//!
//! The functions take plain counts rather than a `ScanProgress`, so a caller
//! whose progress events carry `(done, total)` in some other shape uses them
//! unchanged.

use std::time::Duration;

/// The `(percent, elapsed seconds, remaining seconds)` triple a progress line
/// draws.
///
/// Derived from the bytes read so far, the bytes the pass will read, and the
/// wall time since it started.
///
/// The percent is the byte ratio; a scan with nothing to read (`total == 0`)
/// reads 100%, not a division by zero. The remaining estimate scales the
/// elapsed time by the bytes still to read, and is `0` before the first byte
/// arrives (`done == 0` gives it nothing to extrapolate from).
///
/// The estimate works in **milliseconds**: whole-second math would read zero
/// for the entire first second, exactly when the first plausible estimate
/// should already show.
#[must_use]
pub fn progress_stats(done: u64, total: u64, elapsed: Duration) -> (u64, u64, u64) {
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

/// `hh:mm:ss` from whole seconds — the elapsed and remaining spellings.
///
/// Hours **accumulate**: a scan past a day reads `25:01:01`, not `01:01:01`.
/// That is deliberately unlike the report's own
/// [`time_hh_short`](crate::report::text::time_hh_short), which wraps at 24
/// because a playlist runtime never legitimately exceeds a day and a wall-clock
/// estimate can.
#[must_use]
pub fn hms(seconds: u64) -> String {
    let h = seconds.checked_div(3600).unwrap_or(0);
    let m = seconds.checked_div(60).and_then(|minutes| minutes.checked_rem(60)).unwrap_or(0);
    let s = seconds.checked_rem(60).unwrap_or(0);
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{hms, progress_stats};

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
    fn an_estimate_past_a_u64_of_seconds_saturates() {
        // One byte read of u64::MAX after a very long wait: the u128 estimate
        // exceeds u64, and saturates rather than wrapping to a small time.
        let (_, _, remaining) =
            progress_stats(1, u64::MAX, Duration::from_secs(u64::from(u32::MAX)));
        assert_eq!(remaining, u64::MAX);
    }

    #[test]
    fn hms_formats_hours_minutes_seconds() {
        assert_eq!(hms(0), "00:00:00");
        assert_eq!(hms(61), "00:01:01");
        assert_eq!(hms(3661), "01:01:01");
        // Hours accumulate past a day (no wrap) — a long scan estimate stays legible.
        assert_eq!(hms(90_061), "25:01:01");
    }

    // The estimate math runs over hostile-shaped byte counts and durations, so
    // amplify the unit cases with property tests.
    mod prop {
        use std::time::Duration;

        use proptest::prelude::{Just, Strategy, any, prop_assert, prop_assert_eq, proptest};

        use super::super::{hms, progress_stats};

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

            // Total over any counts and any duration, including the shapes a
            // real scan cannot produce (done past total, a huge elapsed).
            #[test]
            fn the_triple_is_total_over_any_counts(
                done in any::<u64>(),
                total in any::<u64>(),
                millis in any::<u64>(),
            ) {
                let (percent, elapsed, remaining) =
                    progress_stats(done, total, Duration::from_millis(millis));
                prop_assert!(percent <= 100 || total < done);
                let _ = (elapsed, remaining);
            }

            // The spelling is always three colon-separated fields, the minutes
            // and seconds two digits under 60.
            #[test]
            fn hms_is_well_formed(seconds in any::<u64>()) {
                let formatted = hms(seconds);
                let mut parts = formatted.split(':');
                let hours = parts.next().and_then(|part| part.parse::<u64>().ok());
                let minutes = parts.next().and_then(|part| part.parse::<u64>().ok());
                let secs = parts.next().and_then(|part| part.parse::<u64>().ok());
                prop_assert!(parts.next().is_none(), "exactly three segments");
                prop_assert_eq!(hours, seconds.checked_div(3600));
                prop_assert!(minutes.is_some_and(|value| value < 60));
                prop_assert!(secs.is_some_and(|value| value < 60));
            }
        }
    }
}
