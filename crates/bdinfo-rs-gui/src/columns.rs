//! Tier A — the resizable-column weight math.
//!
//! The three data panes render their columns as proportional weights (the
//! `BDInfo` ratios), and the user re-weights them by dragging a header
//! boundary. This module is the pure half of that: shifting weight across one
//! boundary under a per-column floor ([`adjust`]), converting a cursor
//! movement into a weight delta ([`drag_delta`]), and scaling a weight into
//! the integer fill portion the layout takes ([`portion`]). No widgets, no
//! IO — the iced shell feeds cursor positions in and hands the weights back
//! to the layout.

/// The minimum weight a column can be dragged down to (keeps every column
/// visible). Weights are `BDInfo`-ratio units (each set sums to ~98).
pub const MIN_WEIGHT: f32 = 4.0;

/// Shifts `delta` weight across the boundary between column `boundary` and
/// `boundary + 1`, clamped so neither drops below [`MIN_WEIGHT`].
///
/// The pair's sum is preserved, so only these two columns change width — the
/// rest hold still. An out-of-range boundary is a no-op.
pub fn adjust(weights: &mut [f32], boundary: usize, delta: f32) {
    let (Some(&a), Some(&b)) = (weights.get(boundary), weights.get(boundary.wrapping_add(1)))
    else {
        return;
    };
    let pair = a + b;
    let new_a = (a + delta).clamp(MIN_WEIGHT, (pair - MIN_WEIGHT).max(MIN_WEIGHT));
    if let Some(slot) = weights.get_mut(boundary) {
        *slot = new_a;
    }
    if let Some(slot) = weights.get_mut(boundary.wrapping_add(1)) {
        *slot = pair - new_a;
    }
}

/// The weight delta for a cursor moving from `prev_x` to `x` while a boundary
/// is held.
///
/// The pixel movement as a fraction of the columns' shared pixel span, scaled
/// by the weight total — so the boundary tracks the cursor at any window
/// size. Zero when the span is not positive (a degenerate layout must not
/// produce a jump).
#[must_use]
pub fn drag_delta(x: f32, prev_x: f32, span: f32, total: f32) -> f32 {
    if span > 0.0 { (x - prev_x) / span * total } else { 0.0 }
}

/// A column weight as the integer fill portion the layout takes, scaled up so
/// fractional weights keep resolution and clamped into the valid range.
#[must_use]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the weight is clamped to a small positive range before the cast"
)]
pub fn portion(weight: f32) -> u16 {
    (weight * 10.0).round().clamp(1.0, 60000.0) as u16
}

#[cfg(test)]
mod tests {
    use super::{MIN_WEIGHT, adjust, drag_delta, portion};

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "every expected value comes out of exact small-integer f32 arithmetic"
    )]
    fn adjust_moves_weight_across_one_boundary_only() {
        let mut weights = [30.0, 7.0, 19.0, 21.0, 21.0];
        // +5 asks for 35/2, but the right column floors at MIN_WEIGHT, so the
        // pair (sum 37) settles at 33/4 — and the other columns never move.
        adjust(&mut weights, 0, 5.0);
        assert_eq!(weights, [33.0, MIN_WEIGHT, 19.0, 21.0, 21.0]);
        // An unclamped shift moves exactly the delta.
        adjust(&mut weights, 2, 2.0);
        assert_eq!(weights, [33.0, MIN_WEIGHT, 21.0, 19.0, 21.0]);
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "every expected value comes out of exact small-integer f32 arithmetic"
    )]
    fn adjust_clamps_both_directions_to_the_floor() {
        let mut weights = [10.0, 10.0];
        adjust(&mut weights, 0, -100.0);
        assert_eq!(weights, [MIN_WEIGHT, 16.0]);
        adjust(&mut weights, 0, 100.0);
        assert_eq!(weights, [16.0, MIN_WEIGHT]);
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "an out-of-range adjust returns before touching anything, bit-identically"
    )]
    fn adjust_out_of_range_is_a_no_op() {
        let mut weights = [10.0, 10.0];
        adjust(&mut weights, 1, 3.0); // boundary 1 needs a column 2
        assert_eq!(weights, [10.0, 10.0]);
        adjust(&mut weights, 9, 3.0);
        assert_eq!(weights, [10.0, 10.0]);
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the degenerate-span guard returns an exact 0.0")]
    fn drag_delta_scales_pixels_into_weight_units() {
        // 49 px of movement over a 490 px span moves 10% of the total weight.
        assert!((drag_delta(149.0, 100.0, 490.0, 98.0) - 9.8).abs() < 1e-4);
        // Leftward movement is negative.
        assert!((drag_delta(100.0, 149.0, 490.0, 98.0) + 9.8).abs() < 1e-4);
        // A degenerate span produces no jump.
        assert_eq!(drag_delta(149.0, 100.0, 0.0, 98.0), 0.0);
        assert_eq!(drag_delta(149.0, 100.0, -5.0, 98.0), 0.0);
    }

    #[test]
    fn portion_scales_rounds_and_clamps() {
        assert_eq!(portion(30.0), 300);
        assert_eq!(portion(7.26), 73); // rounds to nearest
        assert_eq!(portion(0.0), 1); // never a zero portion
        assert_eq!(portion(-3.0), 1);
        assert_eq!(portion(1e9), 60000); // capped
    }

    // The weight math runs on every cursor move of a drag, over whatever the
    // layout reports — amplify the unit cases with property tests.
    mod prop {
        use proptest::prelude::*;

        use super::super::{MIN_WEIGHT, adjust, drag_delta};

        proptest! {
            // The invariants a drag must keep: the pair's sum is preserved,
            // no column ever drops below the floor, and columns away from the
            // boundary never move.
            #[test]
            fn adjust_preserves_the_sum_and_holds_the_floor(
                mut weights in proptest::collection::vec(MIN_WEIGHT..60.0_f32, 2..6),
                boundary in 0_usize..6,
                delta in -200.0_f32..200.0,
            ) {
                let before = weights.clone();
                adjust(&mut weights, boundary, delta);
                if let (Some(&a0), Some(&b0), Some(&a1), Some(&b1)) = (
                    before.get(boundary),
                    before.get(boundary.wrapping_add(1)),
                    weights.get(boundary),
                    weights.get(boundary.wrapping_add(1)),
                ) {
                    prop_assert!((a0 + b0 - (a1 + b1)).abs() < 1e-3);
                    prop_assert!(a1 >= MIN_WEIGHT);
                    // The far side can dip below the floor only when the pair
                    // itself holds less than two floors' worth.
                    prop_assert!(b1 >= MIN_WEIGHT || a0 + b0 < MIN_WEIGHT * 2.0);
                } else {
                    // Out of range: nothing changed at all (bit-identical).
                    prop_assert!(
                        weights.iter().zip(&before).all(|(now, was)| now.to_bits() == was.to_bits())
                    );
                }
                for (index, (now, was)) in weights.iter().zip(&before).enumerate() {
                    if index != boundary && index != boundary.wrapping_add(1) {
                        // Untouched columns keep their exact bits.
                        prop_assert!(now.to_bits() == was.to_bits());
                    }
                }
            }

            // The delta is finite and sign-follows the cursor for any sane span.
            #[test]
            fn drag_delta_is_finite_and_directional(
                x in -1e4_f32..1e4,
                prev in -1e4_f32..1e4,
                span in 1.0_f32..1e4,
                total in 1.0_f32..500.0,
            ) {
                let delta = drag_delta(x, prev, span, total);
                prop_assert!(delta.is_finite());
                if x > prev {
                    prop_assert!(delta >= 0.0);
                } else {
                    prop_assert!(delta <= 0.0);
                }
            }
        }
    }
}
