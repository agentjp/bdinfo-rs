//! The sparse-image input framing shared by the two `.iso` targets (`source`
//! over the core reader, `wasm_iso` over the browser package's entry point), so
//! one seed describes the same image to both and neither corpus can drift into
//! meaning something the other does not.
//!
//! The bytes *describe* an image rather than being one: a `u16` big-endian
//! header sets the size in [`SECTOR`]-byte sectors, then each following record
//! is a `u16`-BE sector number, a `u16`-BE content length, and that many bytes
//! written at that sector — the same `Iso::new(sectors)` + `write(sector, data)`
//! shape the core reader's own test fixtures build images with. A sector number
//! past the end of the image clamps to its last sector, which puts **both**
//! anchor positions the reader probes within reach of a two-byte mutation: the
//! fixed sector 256 and the end-of-image fallback (UDF 2.50 §2.2.3 records an
//! AVDP at two of sector 256, N − 256 and N). Every input byte therefore becomes
//! descriptor content or its placement, and none of it is the zero padding a
//! whole-image framing must carry to reach the anchor at all.

/// The logical sector every placement in the framing is scaled by — the
/// 2048-byte bootstrap sector a UDF image is addressed in.
pub const SECTOR: usize = 2048;

/// The largest image the harness will materialise, in sectors (1 MiB). It bounds
/// what a hostile size field can make the HARNESS allocate, and it sits above
/// the 330 sectors the largest committed seed describes.
pub const MAX_IMAGE_SECTORS: usize = 512;

/// Materialises the image `data` describes (see the module docs).
///
/// Bytes too short to carry the header describe no image at all, which is the
/// not-a-UDF-volume path.
pub fn build_image(data: &[u8]) -> Vec<u8> {
    let Some((header, mut rest)) = data.split_first_chunk::<2>() else { return Vec::new() };
    let sectors = 1 + usize::from(u16::from_be_bytes(*header)) % MAX_IMAGE_SECTORS;
    let mut image = vec![0_u8; sectors * SECTOR];
    while let Some(([sector_hi, sector_lo, len_hi, len_lo], tail)) = rest.split_first_chunk::<4>() {
        let sector = usize::from(u16::from_be_bytes([*sector_hi, *sector_lo])).min(sectors - 1);
        let want = usize::from(u16::from_be_bytes([*len_hi, *len_lo]));
        let take = want.min(tail.len());
        // A record whose content runs past the end of the image is truncated
        // there rather than dropped, so the last sector stays writable.
        let at = sector * SECTOR;
        let fits = take.min(image.len() - at);
        image[at..at + fits].copy_from_slice(&tail[..fits]);
        rest = &tail[take..];
    }
    image
}
