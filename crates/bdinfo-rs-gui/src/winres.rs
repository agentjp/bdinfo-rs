//! The Windows icon resource, built byte-by-byte in pure Rust.
//!
//! `build.rs` renders the aperture mark ([`crate::icon`]) at every size in
//! [`SIZES`], packs each into an icon DIB ([`dib`]), assembles the classic
//! `.res` container ([`res`]) — `RT_ICON` entries plus one `RT_GROUP_ICON`
//! directory — and hands the file straight to the MSVC linker, which places it
//! in the executable's `.rsrc` section. That embedded group is what Explorer,
//! the taskbar (pinned shortcuts), and Alt-Tab read; the running window's icon
//! is set separately at runtime from the same renderer. Hand-rolled for the
//! same reason the renderer is: no image encoder, no resource compiler, no new
//! dependency, no C.
//!
//! Layout references: `RESOURCEHEADER` (the `.res` format) and the icon
//! resource shapes `BITMAPINFOHEADER` and `GRPICONDIRENTRY` — all
//! little-endian by spec, unlike the big-endian disc structures the analyzer
//! parses.

/// The icon sizes embedded in the executable — the classic Windows ladder from
/// the 16 px list view up to the 256 px Explorer tile.
pub const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// `MOVEABLE | DISCARDABLE` — the 16-bit-era memory flags `rc` stamps on icon
/// resources; Win32 ignores them but tools expect the conventional value.
const MEMORY_FLAGS: u16 = 0x1010;

/// The 32-byte all-ordinal empty resource that must open every `.res` file
/// (type 0, name 0 — the marker 32-bit tools use to detect the format).
const EMPTY_RESOURCE: [u8; 32] = [
    0, 0, 0, 0, // DataSize 0
    32, 0, 0, 0, // HeaderSize 32
    0xFF, 0xFF, 0, 0, // TYPE: ordinal 0
    0xFF, 0xFF, 0, 0, // NAME: ordinal 0
    0, 0, 0, 0, // DataVersion
    0, 0, // MemoryFlags
    0, 0, // LanguageId
    0, 0, 0, 0, // Version
    0, 0, 0, 0, // Characteristics
];

/// `RT_ICON` — one image's DIB.
const RT_ICON: u16 = 3;
/// `RT_GROUP_ICON` — the directory tying the images into one logical icon.
const RT_GROUP_ICON: u16 = 14;

/// Packs one `size`×`size` RGBA8 buffer into an icon DIB.
///
/// The input is row-major top-down, as [`crate::icon::rgba`] renders; the DIB
/// is a `BITMAPINFOHEADER` with doubled height, the 32-bpp BGRA pixel rows
/// bottom-up, then the all-zero 1-bpp AND mask (32-bpp icons mask via the
/// alpha channel).
///
/// Returns `None` when `size` is outside `1..=256` (the icon-resource ceiling)
/// or `rgba` is not exactly `size * size * 4` bytes.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "size <= 256 is checked first, so every derived quantity — the \
              pixel/mask/image byte counts (< 2^19) and their usize/u32 \
              round-trips — fits losslessly on every shipped target"
)]
pub fn dib(size: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if size == 0 || size > 256 {
        return None;
    }
    let edge = size as usize;
    let row_len = edge.wrapping_mul(4);
    if rgba.len() != edge.wrapping_mul(row_len) {
        return None;
    }
    // The 1-bpp AND mask rows pad to a 32-bit boundary.
    let mask_row = size.wrapping_add(31).wrapping_div(32).wrapping_mul(4);
    let image_size = (rgba.len() as u32).wrapping_add(mask_row.wrapping_mul(size));

    let mut out = Vec::new();
    out.extend_from_slice(&40_u32.to_le_bytes()); // biSize
    out.extend_from_slice(&size.to_le_bytes()); // biWidth
    out.extend_from_slice(&size.wrapping_mul(2).to_le_bytes()); // biHeight: XOR + AND
    out.extend_from_slice(&1_u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32_u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0_u32.to_le_bytes()); // biCompression: BI_RGB
    out.extend_from_slice(&image_size.to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0_u8; 16]); // resolution + palette fields: unused

    // The XOR image: rows bottom-up, each RGBA pixel re-ordered to BGRA.
    for row in rgba.chunks_exact(row_len).rev() {
        for px in row.chunks_exact(4) {
            out.push(px.get(2).copied().unwrap_or_default()); // B
            out.push(px.get(1).copied().unwrap_or_default()); // G
            out.push(px.first().copied().unwrap_or_default()); // R
            out.push(px.get(3).copied().unwrap_or_default()); // A
        }
    }
    // The AND mask: all zeros (fully "draw"), one padded row per pixel row.
    let mask_len = (mask_row as usize).wrapping_mul(edge);
    out.resize(out.len().wrapping_add(mask_len), 0);
    Some(out)
}

/// The 32-byte all-ordinal `RESOURCEHEADER` preceding each resource's data.
fn header(out: &mut Vec<u8>, data_size: u32, type_id: u16, name_id: u16) {
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(&32_u32.to_le_bytes()); // HeaderSize: all-ordinal
    out.extend_from_slice(&0xFFFF_u16.to_le_bytes());
    out.extend_from_slice(&type_id.to_le_bytes());
    out.extend_from_slice(&0xFFFF_u16.to_le_bytes());
    out.extend_from_slice(&name_id.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&MEMORY_FLAGS.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes()); // LanguageId: neutral
    out.extend_from_slice(&0_u32.to_le_bytes()); // Version
    out.extend_from_slice(&0_u32.to_le_bytes()); // Characteristics
}

/// Pads `out` with zeros to the next 32-bit boundary — every resource's data
/// in a `.res` file is DWORD-aligned.
fn align(out: &mut Vec<u8>) {
    let rem = out.len().wrapping_rem(4);
    if rem != 0 {
        out.resize(out.len().wrapping_add(4_usize.wrapping_sub(rem)), 0);
    }
}

/// Assembles the full `.res` file from `(size, dib)` pairs.
///
/// The container holds the empty marker resource, one `RT_ICON` per image
/// (ordinal ids `1..`), then the `RT_GROUP_ICON` directory (ordinal id 1)
/// tying them together — the shape Explorer and `LoadIcon` resolve.
///
/// Returns `None` when `icons` is empty or holds more images than the 16-bit
/// ordinal id space.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "each DIB is bounded by dib()'s 256 px ceiling (< 2^19 bytes) and \
              the group directory by the u16 count checked first, so the u32 \
              data-size and u8 extent casts are lossless"
)]
pub fn res(icons: &[(u32, Vec<u8>)]) -> Option<Vec<u8>> {
    if icons.is_empty() || u16::try_from(icons.len()).is_err() {
        return None;
    }
    let mut out = Vec::new();
    out.extend_from_slice(&EMPTY_RESOURCE);

    // The images, ordinal ids 1..=count.
    for (index, (_, data)) in icons.iter().enumerate() {
        let id = (index as u16).wrapping_add(1);
        header(&mut out, data.len() as u32, RT_ICON, id);
        out.extend_from_slice(data);
        align(&mut out);
    }

    // The GRPICONDIR + one GRPICONDIRENTRY per image.
    let mut group = Vec::new();
    group.extend_from_slice(&0_u16.to_le_bytes()); // idReserved
    group.extend_from_slice(&1_u16.to_le_bytes()); // idType: icon
    group.extend_from_slice(&(icons.len() as u16).to_le_bytes()); // idCount
    for (index, (size, data)) in icons.iter().enumerate() {
        // A 256 px extent is stored as 0 (the u8 wraps to it exactly).
        group.push(*size as u8); // bWidth
        group.push(*size as u8); // bHeight
        group.push(0); // bColorCount: not palettized
        group.push(0); // bReserved
        group.extend_from_slice(&1_u16.to_le_bytes()); // wPlanes
        group.extend_from_slice(&32_u16.to_le_bytes()); // wBitCount
        group.extend_from_slice(&(data.len() as u32).to_le_bytes()); // dwBytesInRes
        group.extend_from_slice(&(index as u16).wrapping_add(1).to_le_bytes()); // nID
    }
    header(&mut out, group.len() as u32, RT_GROUP_ICON, 1);
    out.extend_from_slice(&group);
    align(&mut out);
    Some(out)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{EMPTY_RESOURCE, SIZES, dib, res};
    use crate::icon;

    /// The little-endian u32 at `offset` — checked access, like the icon
    /// tests' `channel` helper, so the no-indexing rule holds in tests too.
    fn u32_at(buf: &[u8], offset: usize) -> Option<u32> {
        let end = offset.checked_add(4).expect("offset fits");
        buf.get(offset..end).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes)
    }

    /// The little-endian u16 at `offset`.
    fn u16_at(buf: &[u8], offset: usize) -> Option<u16> {
        let end = offset.checked_add(2).expect("offset fits");
        buf.get(offset..end).and_then(|b| b.try_into().ok()).map(u16::from_le_bytes)
    }

    /// A distinct-byte RGBA buffer for `size`×`size` (values wrap, which is
    /// fine — only ordering matters to the tests).
    fn test_rgba(size: usize) -> Vec<u8> {
        let len = size.pow(2).checked_mul(4).expect("test size fits");
        (0..len)
            .map(|i| u8::try_from(i.checked_rem(251).expect("modulus")).expect("< 251"))
            .collect()
    }

    #[test]
    fn a_zero_size_an_oversize_and_a_length_mismatch_are_rejected() {
        // Buffers sized to MATCH each rejected size, so the size checks (not
        // the length check) are what rejects them.
        assert_eq!(dib(0, &[]), None, "size 0");
        assert_eq!(dib(257, &test_rgba(257)), None, "past the 256 px ceiling");
        assert_eq!(dib(2, &[0; 15]), None, "length mismatch");
    }

    #[test]
    fn the_one_pixel_dib_matches_the_layout_byte_for_byte() {
        // One RGBA pixel [10, 20, 30, 40] → header + one BGRA pixel + one
        // padded all-zero mask row: the full 48 bytes, hand-assembled.
        let expected = [
            40, 0, 0, 0, // biSize
            1, 0, 0, 0, // biWidth
            2, 0, 0, 0, // biHeight: doubled
            1, 0, // biPlanes
            32, 0, // biBitCount
            0, 0, 0, 0, // biCompression
            8, 0, 0, 0, // biSizeImage: 4 XOR + 4 mask
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // unused fields
            30, 20, 10, 40, // the pixel, RGBA → BGRA
            0, 0, 0, 0, // the padded mask row
        ];
        assert_eq!(dib(1, &[10, 20, 30, 40]).as_deref(), Some(expected.as_slice()));
    }

    #[test]
    fn the_pixel_rows_flip_bottom_up_and_reorder_to_bgra() {
        // 2×2, top row [A B], bottom row [C D] — the XOR section must hold
        // C' D' A' B' (bottom-up), each pixel BGRA.
        let rgba = [
            1, 2, 3, 4, 5, 6, 7, 8, // A, B
            9, 10, 11, 12, 13, 14, 15, 16, // C, D
        ];
        let xor = [
            11, 10, 9, 12, 15, 14, 13, 16, // C', D'
            3, 2, 1, 4, 7, 6, 5, 8, // A', B'
        ];
        let out = dib(2, &rgba).expect("valid 2x2");
        assert_eq!(out.get(40..56), Some(xor.as_slice()), "XOR section");
        assert_eq!(out.get(56..64), Some([0_u8; 8].as_slice()), "two mask rows");
        assert_eq!(out.len(), 64);
    }

    #[test]
    fn the_256_px_boundary_is_accepted_with_the_documented_length() {
        // 40 header + 256·256·4 XOR + 32·256 mask = 270,376 — and the header
        // records the image size (XOR + mask) and doubled height.
        let out = dib(256, &icon::rgba(256)).expect("the ceiling size is valid");
        assert_eq!(out.len(), 270_376);
        assert_eq!(u32_at(&out, 8), Some(512), "biHeight is doubled");
        assert_eq!(u32_at(&out, 20), Some(270_336), "biSizeImage: XOR + mask");
    }

    #[test]
    fn an_empty_and_an_overlong_icon_list_are_rejected() {
        assert_eq!(res(&[]), None, "no images");
        let too_many: Vec<(u32, Vec<u8>)> =
            (0..=u32::from(u16::MAX)).map(|_| (1, Vec::new())).collect();
        assert_eq!(res(&too_many), None, "past the u16 ordinal-id space");
    }

    #[test]
    fn a_single_icon_res_matches_the_container_byte_for_byte() {
        // One 16 px image with a 4-byte stand-in DIB: empty marker, RT_ICON
        // id 1, the data, RT_GROUP_ICON id 1 with one entry — 120 bytes total,
        // hand-assembled.
        let expected: Vec<u8> = [
            EMPTY_RESOURCE.as_slice(),
            &[
                4, 0, 0, 0, // DataSize
                32, 0, 0, 0, // HeaderSize
                0xFF, 0xFF, 3, 0, // TYPE: RT_ICON
                0xFF, 0xFF, 1, 0, // NAME: ordinal 1
                0, 0, 0, 0, // DataVersion
                0x10, 0x10, // MemoryFlags
                0, 0, // LanguageId
                0, 0, 0, 0, // Version
                0, 0, 0, 0, // Characteristics
                1, 2, 3, 4, // the DIB stand-in (already aligned)
            ],
            &[
                20, 0, 0, 0, // DataSize: GRPICONDIR + 1 entry
                32, 0, 0, 0, // HeaderSize
                0xFF, 0xFF, 14, 0, // TYPE: RT_GROUP_ICON
                0xFF, 0xFF, 1, 0, // NAME: ordinal 1
                0, 0, 0, 0, // DataVersion
                0x10, 0x10, // MemoryFlags
                0, 0, // LanguageId
                0, 0, 0, 0, // Version
                0, 0, 0, 0, // Characteristics
                0, 0, // idReserved
                1, 0, // idType: icon
                1, 0, // idCount
                16, 16, // bWidth, bHeight
                0, 0, // bColorCount, bReserved
                1, 0, // wPlanes
                32, 0, // wBitCount
                4, 0, 0, 0, // dwBytesInRes
                1, 0, // nID
            ],
        ]
        .concat();
        assert_eq!(res(&[(16, vec![1, 2, 3, 4])]), Some(expected));
    }

    #[test]
    fn odd_data_pads_to_dword_and_the_256_px_extent_stores_as_zero() {
        // Two images: a 3-byte DIB (needs 1 pad byte) and a 256 px one — the
        // second RT_ICON header must start aligned, ids must run 1, 2, and the
        // group entry for 256 must store extent 0.
        let out = res(&[(3, vec![0xAA, 0xBB, 0xCC]), (256, vec![0xDD; 8])]).expect("valid list");
        assert_eq!(out.len().checked_rem(4), Some(0), "the container ends aligned");
        // Empty(32) + header(32) + 3 data + 1 pad = 68: the second header.
        assert_eq!(u32_at(&out, 68), Some(8), "second image's DataSize");
        assert_eq!(u16_at(&out, 82), Some(2), "second image's ordinal id");
        // 68 + 32 + 8 = 108: the group header; its data begins at 140.
        assert_eq!(u16_at(&out, 144), Some(2), "idCount");
        // First entry at 146: extent bytes for the 3-px stand-in "size".
        assert_eq!(out.get(146).copied(), Some(3), "first entry's width byte");
        // Second entry at 160: the 256 px extent wraps to the 0 marker.
        assert_eq!(out.get(160).copied(), Some(0), "256 stores as 0");
        assert_eq!(u16_at(&out, 172), Some(2), "second entry's nID");
    }

    #[test]
    fn the_shipped_icon_set_matches_the_pinned_checksum() {
        // The exact bytes the build script embeds: every size in SIZES through
        // the exact-arithmetic renderer (byte-identical on every shipped arch —
        // see icon::radius) and this container. Any change to the ladder, the
        // DIB layout, or the container shows up here.
        let icons: Vec<(u32, Vec<u8>)> =
            SIZES.iter().map(|&s| (s, dib(s, &icon::rgba(s)).expect("shipped size"))).collect();
        let out = res(&icons).expect("the shipped set is valid");
        assert_eq!(out.len(), 372_800, "update the pinned length deliberately");
        let sum: u64 = out.iter().map(|&byte| u64::from(byte)).sum();
        assert_eq!(sum, 54_754_185, "update the pinned checksum deliberately");
    }

    proptest! {
        /// Any valid size/buffer pair packs to the documented DIB length, and
        /// the header always leads with biSize 40 and the true width.
        #[test]
        fn any_valid_dib_has_the_documented_length(size in 1_u32..=48, seed in any::<u8>()) {
            let edge = usize::try_from(size).expect("small");
            let rgba: Vec<u8> = test_rgba(edge).iter().map(|&b| b.wrapping_add(seed)).collect();
            let out = dib(size, &rgba).expect("valid by construction");
            let mask = usize::try_from(size.wrapping_add(31).wrapping_div(32).wrapping_mul(4))
                .expect("small")
                .checked_mul(edge)
                .expect("fits");
            let expected = 40_usize
                .checked_add(rgba.len())
                .and_then(|n| n.checked_add(mask))
                .expect("fits");
            prop_assert_eq!(out.len(), expected);
            prop_assert_eq!(u32_at(&out, 0), Some(40));
            prop_assert_eq!(u32_at(&out, 4), Some(size));
        }

        /// Any icon list packs to an aligned container whose group directory
        /// counts every image.
        #[test]
        fn any_icon_list_packs_aligned_with_a_full_directory(lens in prop::collection::vec(0_usize..9, 1..5)) {
            let icons: Vec<(u32, Vec<u8>)> =
                lens.iter().map(|&n| (16, vec![0x5A; n])).collect();
            let out = res(&icons).expect("non-empty");
            prop_assert_eq!(out.len().checked_rem(4), Some(0));
            // The group directory sits at the tail, zero-padded to DWORD: the
            // count is 2 bytes before its entries, past that padding.
            let entries = lens.len().checked_mul(14).expect("small");
            let group_len = entries.checked_add(6).expect("small");
            let pad = group_len.wrapping_neg().checked_rem(4).expect("modulus");
            let count_at = out
                .len()
                .checked_sub(pad)
                .and_then(|n| n.checked_sub(entries))
                .and_then(|n| n.checked_sub(2))
                .expect("fits");
            prop_assert_eq!(u16_at(&out, count_at), u16::try_from(lens.len()).ok());
        }
    }
}
