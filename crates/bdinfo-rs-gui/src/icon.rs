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
                    if in_mark(dx.hypot(dy)) {
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
    use super::{RING_OUTER, rgba};

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
}
