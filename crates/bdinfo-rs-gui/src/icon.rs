//! The app mark, rendered to RGBA pixels for the window icon.
//!
//! A single recognizable **aperture**: a bold amber ring with a centre dot on a
//! transparent field — a lens/disc read-point that reads cleanly on a light or
//! dark taskbar and matches the in-app brand mark. Drawn procedurally (no image
//! decoder, no new dependency, no C) with 4×4 supersampled anti-aliasing, so the
//! binary stays self-contained. The vector source of the same mark is kept in
//! `assets/icon.svg` for the Phase-5 installer/bundling work.

/// The mark's amber, matching `Palette::DARK.accent` — vivid enough to carry on
/// either a light or dark window chrome.
const AMBER: (f32, f32, f32) = (242.0 / 255.0, 166.0 / 255.0, 90.0 / 255.0);

/// Supersampling factor per axis (4×4 = 16 samples/pixel) for smooth edges.
const SS: u32 = 4;

/// The normalized radii (fraction of the icon's half-extent) of the mark: a
/// filled centre dot, a gap, then the ring.
const DOT_R: f32 = 0.16;
const RING_INNER: f32 = 0.36;
const RING_OUTER: f32 = 0.49;

/// Whether a point at normalized radius `r` (0 at centre, ~0.5 at an edge) is
/// inside the amber mark.
fn in_mark(r: f32) -> bool {
    r <= DOT_R || (RING_INNER..=RING_OUTER).contains(&r)
}

/// The normalized radius of the subpixel at `(dx, dy)` from the centre.
///
/// `sqrt(dx² + dy²)` from IEEE-exact multiply/add/sqrt — every step is
/// correctly rounded on every architecture, so the rendered buffer is
/// byte-identical across the 6 shipped targets and the pinned checksum in the
/// tests holds everywhere. (`hypot` would be a libm call with
/// platform-varying last-ulp behaviour.)
#[expect(
    clippy::imprecise_flops,
    reason = "hypot's extra precision is irrelevant at |dx|,|dy| <= 0.5 with no \
              overflow risk, and its platform-varying rounding would break the \
              cross-arch byte-identity the pinned checksum asserts"
)]
fn radius(dx: f32, dy: f32) -> f32 {
    (dx * dx + dy * dy).sqrt()
}

/// Renders the aperture mark to a `size`×`size` RGBA8 buffer (row-major, 4 bytes
/// per pixel), amber on transparent. The returned length is always
/// `size * size * 4`.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "icon size is small (<= 256) so pixel indices are exact in f32, and \
              each averaged channel is clamped to 0.0..=1.0 before the u8 cast"
)]
pub fn rgba(size: u32) -> Vec<u8> {
    let samples = f32::from(u16::try_from(SS.saturating_mul(SS)).unwrap_or(u16::MAX));
    let extent = size as f32;
    let mut pixels = Vec::new();
    for y in 0..size {
        for x in 0..size {
            // Average the mark's coverage over the SS×SS subpixel grid.
            let mut coverage = 0.0_f32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let fy = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    let dx = fx / extent - 0.5;
                    let dy = fy / extent - 0.5;
                    if in_mark(radius(dx, dy)) {
                        coverage += 1.0;
                    }
                }
            }
            let alpha = coverage / samples;
            pixels.push((AMBER.0 * 255.0).round() as u8);
            pixels.push((AMBER.1 * 255.0).round() as u8);
            pixels.push((AMBER.2 * 255.0).round() as u8);
            pixels.push((alpha * 255.0).round() as u8);
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::{DOT_R, RING_INNER, RING_OUTER, in_mark, radius, rgba};

    /// The expected buffer length, `size * size * 4`, via checked math (the gate
    /// forbids bare arithmetic operators).
    fn rgba_len(size: usize) -> usize {
        size.pow(2).checked_mul(4).expect("icon size fits")
    }

    /// The flat byte index of channel `c` (0=R..3=A) of pixel `(x, y)` in a
    /// `size`-wide RGBA8 buffer — checked, so the gate's no-bare-arithmetic /
    /// no-indexing rules hold even in tests.
    fn channel(x: usize, y: usize, c: usize, size: usize) -> usize {
        y.checked_mul(size)
            .and_then(|row| row.checked_add(x))
            .and_then(|px| px.checked_mul(4))
            .and_then(|base| base.checked_add(c))
            .expect("index fits")
    }

    #[test]
    fn buffer_is_rgba8_of_the_requested_size() {
        for size in [16_u32, 32, 64, 128] {
            let w = usize::try_from(size).expect("fits");
            assert_eq!(rgba(size).len(), rgba_len(w));
        }
    }

    #[test]
    fn the_centre_is_opaque_amber_and_the_corner_is_transparent() {
        let size = 64_u32;
        let w = usize::try_from(size).expect("fits");
        let buf = rgba(size);
        // Centre pixel (32, 32): inside the dot → fully opaque amber.
        assert_eq!(buf.get(channel(32, 32, 0, w)).copied(), Some(242), "red is amber");
        assert_eq!(buf.get(channel(32, 32, 3, w)).copied(), Some(255), "centre is opaque");
        // Corner pixel (0, 0): well outside the ring → fully transparent.
        assert_eq!(buf.get(channel(0, 0, 3, w)).copied(), Some(0), "the corner is transparent");
    }

    #[test]
    fn the_mid_gap_between_dot_and_ring_is_transparent() {
        // At size 128 the dot ends ~20 px and the ring starts ~46 px from centre,
        // so pixel x = 97 (= 64 + 33) sits squarely in the empty gap on the centre
        // row — it must be fully transparent.
        let size = 128_u32;
        let w = usize::try_from(size).expect("fits");
        let buf = rgba(size);
        assert_eq!(buf.get(channel(97, 64, 3, w)).copied(), Some(0), "the gap is transparent");
        // Sanity: the outer ring radius is the largest filled band (compile-time).
        const { assert!(RING_OUTER < 0.5) };
    }

    #[test]
    fn the_mark_bands_include_their_exact_boundaries() {
        // The dot is a closed disc and the ring a closed band: each boundary
        // radius is IN the mark, the gap and the outside are not.
        assert!(in_mark(0.0));
        assert!(in_mark(DOT_R));
        assert!(!in_mark(0.26)); // squarely in the dot-to-ring gap
        assert!(in_mark(RING_INNER));
        assert!(in_mark(0.42)); // mid-ring
        assert!(in_mark(RING_OUTER));
        assert!(!in_mark(0.4999)); // just outside the ring
        assert!(!in_mark(0.75));
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "a 3-4-5 triangle scaled by a power of two is exact in f32 end to end"
    )]
    fn the_radius_is_the_euclidean_distance() {
        // A 3-4-5 triangle in exactly-representable f32.
        assert_eq!(radius(0.3, 0.4), 0.5);
        assert_eq!(radius(-0.3, 0.4), 0.5); // sign-independent
        assert_eq!(radius(0.0, 0.25), 0.25); // each axis alone
        assert_eq!(radius(0.25, 0.0), 0.25);
    }

    #[test]
    fn the_edges_are_anti_aliased_and_the_channels_are_amber() {
        let size = 64_u32;
        let w = usize::try_from(size).expect("fits");
        let buf = rgba(size);
        // Every pixel carries the same amber RGB, whatever its alpha — the
        // three channels in their exact order.
        for pixel in buf.chunks_exact(4) {
            assert_eq!(pixel.first().copied(), Some(242));
            assert_eq!(pixel.get(1).copied(), Some(166));
            assert_eq!(pixel.get(2).copied(), Some(90));
        }
        // The supersampled edges produce partial coverage: some alpha strictly
        // between transparent and opaque exists (the anti-aliasing itself).
        let partial = (0..w)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .any(|(x, y)| buf.get(channel(x, y, 3, w)).copied().is_some_and(|a| a > 0 && a < 255));
        assert!(partial, "the mark's edges are anti-aliased");
    }

    #[test]
    fn the_rendered_buffer_matches_the_pinned_checksum() {
        // The exact-arithmetic pipeline (see `radius`) renders byte-identically
        // on every arch, so the whole 64 px buffer is pinned by one checksum —
        // any change to the mark geometry, the supersampling, or the alpha
        // math shows up here.
        let sum: u64 = rgba(64).iter().map(|&byte| u64::from(byte)).sum();
        assert_eq!(sum, 2_485_612, "update the pinned checksum deliberately");
    }
}
