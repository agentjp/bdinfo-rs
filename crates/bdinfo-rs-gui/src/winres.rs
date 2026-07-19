//! Windows resources — icon, version info, `.ico` — built byte-by-byte in
//! pure Rust.
//!
//! `build.rs` renders the aperture mark ([`crate::icon`]) at every size in
//! [`SIZES`], packs each into an icon DIB ([`dib`]), assembles the classic
//! `.res` container ([`res`]) — `RT_ICON` entries plus one `RT_GROUP_ICON`
//! directory — appends a `VS_VERSIONINFO` resource ([`version_info`] via
//! [`entry`]) carrying the crate version, and hands the file straight to the
//! MSVC linker, which places it in the executable's `.rsrc` section. The icon
//! group is what Explorer, the taskbar (pinned shortcuts), and Alt-Tab read;
//! the version block is what the file-Properties Details tab and Task Manager
//! read; the running window's icon is set separately at runtime from the same
//! renderer. [`ico`] packs the same DIBs into a standalone `.ico` file for the
//! packaging assets (the MSI's Apps-list icon). Hand-rolled for the same
//! reason the renderer is: no image encoder, no resource compiler, no new
//! dependency, no C.
//!
//! Layout references: `RESOURCEHEADER` (the `.res` format), the icon resource
//! shapes `BITMAPINFOHEADER` / `GRPICONDIRENTRY` / `ICONDIR`, and the
//! `VS_VERSIONINFO` block tree — all little-endian by spec, unlike the
//! big-endian disc structures the analyzer parses.

/// The icon sizes embedded in the executable.
///
/// The classic ladder from the 16 px list view to the 256 px tile, plus the
/// fractional-DPI rungs (20, 28, 40, 96) the Windows shell requests at
/// 125/150/175/200 % scale — without them it nearest-neighbour-upscales a
/// smaller rung into visible jaggies.
pub const SIZES: [u32; 11] = [16, 20, 24, 28, 32, 40, 48, 64, 96, 128, 256];

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
/// `RT_VERSION` — the `VS_VERSIONINFO` block ([`version_info`]).
pub const RT_VERSION: u16 = 16;

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
/// in a `.res` file is DWORD-aligned, as is every block in a `VS_VERSIONINFO`
/// tree.
fn align(out: &mut Vec<u8>) {
    let rem = out.len().wrapping_rem(4);
    if rem != 0 {
        out.resize(out.len().wrapping_add(4_usize.wrapping_sub(rem)), 0);
    }
}

/// One complete `.res` entry: the all-ordinal header, `data`, and the
/// trailing DWORD padding.
///
/// Entries concatenate into a valid `.res` file (after the 32-byte empty
/// marker resource), which is how `build.rs` appends the [`RT_VERSION`]
/// resource after the icon container [`res`] builds.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "every payload is a resource this module built — a DIB under \
              dib()'s 256 px ceiling (< 2^19 bytes), an icon directory, or a \
              version_info() tree (< 2^16 bytes) — so the u32 data-size cast \
              is lossless"
)]
pub fn entry(type_id: u16, name_id: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    header(&mut out, data.len() as u32, type_id, name_id);
    out.extend_from_slice(data);
    align(&mut out);
    out
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
        out.extend_from_slice(&entry(RT_ICON, id, data));
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
    out.extend_from_slice(&entry(RT_GROUP_ICON, 1, &group));
    Some(out)
}

/// Assembles a standalone `.ico` file from `(size, dib)` pairs — the `ICONDIR`
/// directory followed by the images, which are byte-identical to the `RT_ICON`
/// DIBs ([`dib`]), the two formats' shared shape.
///
/// Not embedded in the binary (the `.res` route above owns that): this is the
/// packaging-asset form — the WiX installer's Apps-list icon
/// (`ARPPRODUCTICON`) and any future manifest that wants a plain `.ico`.
///
/// Returns `None` when `icons` is empty or holds more images than the 16-bit
/// count field.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "each DIB is bounded by dib()'s 256 px ceiling (< 2^19 bytes) and \
              the directory by the u16 count checked first, so the u32 \
              size/offset casts are lossless and the u8 extent cast wraps 256 \
              to the format's own 0 marker exactly"
)]
pub fn ico(icons: &[(u32, Vec<u8>)]) -> Option<Vec<u8>> {
    if icons.is_empty() || u16::try_from(icons.len()).is_err() {
        return None;
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0_u16.to_le_bytes()); // idReserved
    out.extend_from_slice(&1_u16.to_le_bytes()); // idType: icon
    out.extend_from_slice(&(icons.len() as u16).to_le_bytes()); // idCount

    // The images follow the directory back-to-back; each entry records its
    // absolute file offset.
    let mut offset = 6_usize.wrapping_add(icons.len().wrapping_mul(16));
    for (size, data) in icons {
        // A 256 px extent is stored as 0, as in the RT_GROUP_ICON directory.
        out.push(*size as u8); // bWidth
        out.push(*size as u8); // bHeight
        out.push(0); // bColorCount: not palettized
        out.push(0); // bReserved
        out.extend_from_slice(&1_u16.to_le_bytes()); // wPlanes
        out.extend_from_slice(&32_u16.to_le_bytes()); // wBitCount
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // dwBytesInRes
        out.extend_from_slice(&(offset as u32).to_le_bytes()); // dwImageOffset
        offset = offset.wrapping_add(data.len());
    }
    for (_, data) in icons {
        out.extend_from_slice(data);
    }
    Some(out)
}

/// Encodes `text` as NUL-terminated UTF-16LE into `out`, counting the code
/// units written (terminator included).
///
/// Returns `None` on an interior NUL — it would truncate the string for every
/// Win32 reader.
fn utf16z(text: &str, out: &mut Vec<u8>) -> Option<u16> {
    let mut units: u16 = 0;
    for unit in text.encode_utf16() {
        if unit == 0 {
            return None;
        }
        out.extend_from_slice(&unit.to_le_bytes());
        units = units.checked_add(1)?;
    }
    out.extend_from_slice(&0_u16.to_le_bytes());
    units.checked_add(1)
}

/// Pads `body` with zeros so the next byte lands on a 32-bit boundary of the
/// final block, whose 6-byte header `body` does not yet include.
fn align_body(body: &mut Vec<u8>) {
    let rem = body.len().wrapping_add(6).wrapping_rem(4);
    if rem != 0 {
        body.resize(body.len().wrapping_add(4_usize.wrapping_sub(rem)), 0);
    }
}

/// One block of the `VS_VERSIONINFO` tree: the `wLength` / `wValueLength` /
/// `wType` header, the NUL-terminated UTF-16 `key`, DWORD padding, `value`,
/// DWORD padding, then `children` (pre-joined via [`join`]). `wLength` spans
/// the whole block; the caller supplies `w_value_length` because its unit
/// depends on `w_type` (bytes when binary, UTF-16 units when text).
///
/// Returns `None` on an interior NUL in `key` or a block outgrowing the
/// 16-bit length field.
fn block(
    key: &str,
    w_type: u16,
    w_value_length: u16,
    value: &[u8],
    children: &[u8],
) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    utf16z(key, &mut body)?;
    align_body(&mut body);
    body.extend_from_slice(value);
    if !children.is_empty() {
        align_body(&mut body);
        body.extend_from_slice(children);
    }
    // The header is 6 bytes; a body big enough to wrap usize cannot exist in
    // memory, so the only real failure is the u16 field itself.
    let length = u16::try_from(body.len().wrapping_add(6)).ok()?;
    let mut out = Vec::new();
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&w_value_length.to_le_bytes());
    out.extend_from_slice(&w_type.to_le_bytes());
    out.append(&mut body);
    Some(out)
}

/// Concatenates sibling blocks, DWORD-aligning the start of each — the
/// inter-sibling padding the parent's `wLength` includes but each sibling's
/// own `wLength` excludes.
fn join(blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for b in blocks {
        align(&mut out);
        out.extend_from_slice(b);
    }
    out
}

/// Builds the `VS_VERSIONINFO` block — the data of an [`RT_VERSION`] resource.
///
/// `version` is the four-part `FILEVERSION`/`PRODUCTVERSION` quad (the crate's
/// `major.minor.patch` plus a zero fourth part, in `build.rs`); `strings` are
/// the `StringFileInfo` name/value pairs (`FileDescription`, `ProductName`,
/// …), emitted under the conventional `040904b0` table (en-US, Unicode) with
/// the matching `VarFileInfo` translation entry. The fixed block declares
/// `VOS_NT_WINDOWS32` / `VFT_APP`, no flags, no date.
///
/// Returns `None` on an interior NUL in any string or a tree outgrowing a
/// block's 16-bit length field.
#[must_use]
// allow, not expect: build.rs shares this file, and there the module is
// private, so the pub-only panics-doc lint never fires and an expectation
// would be unfulfilled.
#[allow(
    clippy::missing_panics_doc,
    reason = "the only expect() sites take constant inputs whose success the \
              byte-tie tests pin; no caller input can reach them"
)]
pub fn version_info(version: [u16; 4], strings: &[(&str, &str)]) -> Option<Vec<u8>> {
    let [major, minor, patch, build] = version;
    // The two-dword version encoding packs disjoint 16-bit halves;
    // wrapping_add (not `|`) so the composition stays mutation-distinguishable.
    let ms = u32::from(major).wrapping_shl(16).wrapping_add(u32::from(minor));
    let ls = u32::from(patch).wrapping_shl(16).wrapping_add(u32::from(build));

    // VS_FIXEDFILEINFO: signature, structure version 1.0, file + product
    // version, a full flags mask with no flags set, VOS_NT_WINDOWS32, VFT_APP,
    // no subtype, no date — 13 dwords, 52 bytes.
    let mut fixed = Vec::new();
    for dword in [0xFEEF_04BD_u32, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0] {
        fixed.extend_from_slice(&dword.to_le_bytes());
    }

    let mut table_children = Vec::new();
    for (name, value) in strings {
        let mut encoded = Vec::new();
        let units = utf16z(value, &mut encoded)?;
        table_children.push(block(name, 1, units, &encoded, &[])?);
    }
    let table = block("040904b0", 1, 0, &[], &join(&table_children))?;
    let string_file_info = block("StringFileInfo", 1, 0, &[], &table)?;

    // The translation pair mirroring the table key: language 0x0409 (en-US),
    // charset 0x04B0 (Unicode) — binary value, so wValueLength is in bytes.
    // Constant inputs, so the length checks cannot fail here.
    let translation =
        block("Translation", 0, 4, &[0x09, 0x04, 0xB0, 0x04], &[]).expect("a constant block");
    let var_file_info = block("VarFileInfo", 1, 0, &[], &translation).expect("a constant block");

    // wValueLength 52: the fixed block's size, a format constant (13 dwords).
    block("VS_VERSION_INFO", 0, 52, &fixed, &join(&[string_file_info, var_file_info]))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{EMPTY_RESOURCE, RT_VERSION, SIZES, dib, entry, ico, res, version_info};
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
        assert_eq!(out.len(), 422_808, "update the pinned length deliberately");
        let sum: u64 = out.iter().map(|&byte| u64::from(byte)).sum();
        assert_eq!(sum, 62_004_853, "update the pinned checksum deliberately");
    }

    #[test]
    fn a_bare_entry_matches_the_header_data_pad_layout() {
        // RT_VERSION id 1 with 3 data bytes: the 32-byte all-ordinal header,
        // the data, and 1 pad byte to the DWORD boundary — 36 bytes.
        let expected = [
            3, 0, 0, 0, // DataSize
            32, 0, 0, 0, // HeaderSize
            0xFF, 0xFF, 16, 0, // TYPE: RT_VERSION
            0xFF, 0xFF, 1, 0, // NAME: ordinal 1
            0, 0, 0, 0, // DataVersion
            0x10, 0x10, // MemoryFlags
            0, 0, // LanguageId
            0, 0, 0, 0, // Version
            0, 0, 0, 0, // Characteristics
            0xAA, 0xBB, 0xCC, // the data
            0,    // DWORD pad
        ];
        assert_eq!(entry(RT_VERSION, 1, &[0xAA, 0xBB, 0xCC]), expected.as_slice());
    }

    #[test]
    fn an_empty_and_an_overlong_ico_list_are_rejected() {
        assert_eq!(ico(&[]), None, "no images");
        let too_many: Vec<(u32, Vec<u8>)> =
            (0..=u32::from(u16::MAX)).map(|_| (1, Vec::new())).collect();
        assert_eq!(ico(&too_many), None, "past the u16 count field");
    }

    #[test]
    fn a_single_icon_ico_matches_the_container_byte_for_byte() {
        // One 16 px image with a 4-byte stand-in DIB: ICONDIR, one 16-byte
        // entry whose offset is 6 + 16 = 22, then the data — 26 bytes.
        let expected = [
            0, 0, // idReserved
            1, 0, // idType: icon
            1, 0, // idCount
            16, 16, // bWidth, bHeight
            0, 0, // bColorCount, bReserved
            1, 0, // wPlanes
            32, 0, // wBitCount
            4, 0, 0, 0, // dwBytesInRes
            22, 0, 0, 0, // dwImageOffset
            1, 2, 3, 4, // the DIB stand-in
        ];
        assert_eq!(ico(&[(16, vec![1, 2, 3, 4])]).as_deref(), Some(expected.as_slice()));
    }

    #[test]
    fn ico_offsets_accumulate_and_the_256_px_extent_stores_as_zero() {
        // Two images, 5 and 8 bytes: entries at 6 and 22, images at
        // 6 + 32 = 38 and 38 + 5 = 43; the 256 px extent wraps to 0.
        let out = ico(&[(3, vec![0xAA; 5]), (256, vec![0xBB; 8])]).expect("valid list");
        assert_eq!(out.len(), 51);
        assert_eq!(u16_at(&out, 4), Some(2), "idCount");
        assert_eq!(out.get(6).copied(), Some(3), "first entry's width byte");
        assert_eq!(u32_at(&out, 14), Some(5), "first dwBytesInRes");
        assert_eq!(u32_at(&out, 18), Some(38), "first dwImageOffset");
        assert_eq!(out.get(22).copied(), Some(0), "256 stores as 0");
        assert_eq!(u32_at(&out, 30), Some(8), "second dwBytesInRes");
        assert_eq!(u32_at(&out, 34), Some(43), "second dwImageOffset");
        assert_eq!(out.get(38..43), Some([0xAA_u8; 5].as_slice()), "first image");
        assert_eq!(out.get(43..51), Some([0xBB_u8; 8].as_slice()), "second image");
    }

    #[test]
    fn the_shipped_ico_matches_the_pinned_checksum() {
        // The exact bytes the packaging .ico carries: every size in SIZES
        // through the exact-arithmetic renderer and this container (the same
        // cross-arch byte-identity argument as the .res pin above).
        let icons: Vec<(u32, Vec<u8>)> =
            SIZES.iter().map(|&s| (s, dib(s, &icon::rgba(s)).expect("shipped size"))).collect();
        let out = ico(&icons).expect("the shipped set is valid");
        assert_eq!(out.len(), 422_414, "update the pinned length deliberately");
        let sum: u64 = out.iter().map(|&byte| u64::from(byte)).sum();
        assert_eq!(sum, 61_990_912, "update the pinned checksum deliberately");
    }

    #[test]
    fn a_minimal_version_info_matches_the_block_tree_byte_for_byte() {
        // version 1.2.3.4 with one string pair ("A" -> "B"): the root block,
        // its 52-byte VS_FIXEDFILEINFO, one StringFileInfo/StringTable/String
        // chain, and the VarFileInfo/Translation chain — 236 bytes,
        // hand-assembled.
        let expected: Vec<u8> = [
            // ── root: VS_VERSION_INFO ──────────────────────────────────────
            &[0xEC, 0, 0x34, 0, 0, 0][..], // wLength 236, wValueLength 52, binary
            &[
                0x56, 0, 0x53, 0, 0x5F, 0, 0x56, 0, 0x45, 0, 0x52, 0, 0x53, 0, 0x49, 0, 0x4F, 0,
                0x4E, 0, 0x5F, 0, 0x49, 0, 0x4E, 0, 0x46, 0, 0x4F, 0, 0, 0, // "VS_VERSION_INFO"
            ],
            &[0, 0], // padding to the 32-bit boundary
            // VS_FIXEDFILEINFO
            &[0xBD, 0x04, 0xEF, 0xFE], // dwSignature
            &[0, 0, 0x01, 0], // dwStrucVersion 1.0
            &[0x02, 0, 0x01, 0], // dwFileVersionMS: 1.2
            &[0x04, 0, 0x03, 0], // dwFileVersionLS: 3.4
            &[0x02, 0, 0x01, 0], // dwProductVersionMS
            &[0x04, 0, 0x03, 0], // dwProductVersionLS
            &[0x3F, 0, 0, 0], // dwFileFlagsMask
            &[0, 0, 0, 0], // dwFileFlags
            &[0x04, 0, 0x04, 0], // dwFileOS: VOS_NT_WINDOWS32
            &[0x01, 0, 0, 0], // dwFileType: VFT_APP
            &[0, 0, 0, 0], // dwFileSubtype
            &[0, 0, 0, 0, 0, 0, 0, 0], // dwFileDateMS/LS
            // ── StringFileInfo (at 92, length 76) ──────────────────────────
            &[0x4C, 0, 0, 0, 0x01, 0],
            &[
                0x53, 0, 0x74, 0, 0x72, 0, 0x69, 0, 0x6E, 0, 0x67, 0, 0x46, 0, 0x69, 0, 0x6C, 0,
                0x65, 0, 0x49, 0, 0x6E, 0, 0x66, 0, 0x6F, 0, 0, 0, // "StringFileInfo"
            ],
            // StringTable "040904b0" (length 40)
            &[0x28, 0, 0, 0, 0x01, 0],
            &[
                0x30, 0, 0x34, 0, 0x30, 0, 0x39, 0, 0x30, 0, 0x34, 0, 0x62, 0, 0x30, 0, 0, 0,
            ], // "040904b0"
            // String "A" -> "B" (length 16; wValueLength 2 UTF-16 units)
            &[0x10, 0, 0x02, 0, 0x01, 0],
            &[0x41, 0, 0, 0], // "A"
            &[0, 0], // padding
            &[0x42, 0, 0, 0], // "B"
            // ── VarFileInfo (at 168, length 68) ────────────────────────────
            &[0x44, 0, 0, 0, 0x01, 0],
            &[
                0x56, 0, 0x61, 0, 0x72, 0, 0x46, 0, 0x69, 0, 0x6C, 0, 0x65, 0, 0x49, 0, 0x6E, 0,
                0x66, 0, 0x6F, 0, 0, 0, // "VarFileInfo"
            ],
            &[0, 0], // padding
            // Translation (length 36; a 4-byte binary value)
            &[0x24, 0, 0x04, 0, 0, 0],
            &[
                0x54, 0, 0x72, 0, 0x61, 0, 0x6E, 0, 0x73, 0, 0x6C, 0, 0x61, 0, 0x74, 0, 0x69, 0,
                0x6F, 0, 0x6E, 0, 0, 0, // "Translation"
            ],
            &[0, 0], // padding
            &[0x09, 0x04, 0xB0, 0x04], // en-US, Unicode
        ]
        .concat();
        assert_eq!(version_info([1, 2, 3, 4], &[("A", "B")]).as_deref(), Some(&expected[..]));
    }

    #[test]
    fn the_version_quad_packs_into_the_fixed_dwords() {
        // Distinct halves so a shl/add slip in the MS/LS packing shows up in
        // the dwords at their documented offsets (40 + 8 / + 12).
        let out = version_info([0x1234, 0x5678, 0x9ABC, 0xDEF0], &[]).expect("valid");
        assert_eq!(u32_at(&out, 40), Some(0xFEEF_04BD), "signature");
        assert_eq!(u32_at(&out, 48), Some(0x1234_5678), "dwFileVersionMS");
        assert_eq!(u32_at(&out, 52), Some(0x9ABC_DEF0), "dwFileVersionLS");
        assert_eq!(u32_at(&out, 56), Some(0x1234_5678), "dwProductVersionMS");
        assert_eq!(u32_at(&out, 60), Some(0x9ABC_DEF0), "dwProductVersionLS");
    }

    #[test]
    fn sibling_string_blocks_start_on_32_bit_boundaries() {
        // ("A" -> "AB") is an 18-byte block, so the next sibling needs 2 pad
        // bytes: it must start at table-children offset 20 — absolute 172
        // (root header+fixed 92, StringFileInfo header 36, StringTable header
        // 24, first String 18, pad 2).
        let out = version_info([1, 1, 1, 1], &[("A", "AB"), ("A", "B")]).expect("valid");
        assert_eq!(u16_at(&out, 152), Some(18), "first String block length");
        assert_eq!(out.get(170..172), Some([0, 0].as_slice()), "sibling padding");
        assert_eq!(u16_at(&out, 172), Some(16), "second String starts aligned");
    }

    #[test]
    fn an_interior_nul_in_a_name_or_value_is_rejected() {
        assert_eq!(version_info([1, 1, 1, 1], &[("a\0b", "x")]), None, "name");
        assert_eq!(version_info([1, 1, 1, 1], &[("a", "x\0y")]), None, "value");
    }

    #[test]
    fn a_string_outgrowing_the_16_bit_fields_is_rejected() {
        // 40 000 UTF-16 units fit the unit counter but push the String block
        // past the u16 wLength; 65 535 units overflow the counter at the NUL
        // terminator; 65 536 overflow it mid-string.
        for len in [40_000, 65_535, 65_536] {
            let value = "a".repeat(len);
            assert_eq!(version_info([1, 1, 1, 1], &[("k", &value)]), None, "len {len}");
        }
    }

    #[test]
    fn a_tree_outgrowing_a_parent_block_is_rejected() {
        // Each ancestor's u16 wLength can be the first to overflow. A 32 760
        // char value keeps its String block at 65 534 but pushes the table to
        // 65 558; 32 740 keeps the table at 65 518 but pushes StringFileInfo
        // to 65 554.
        for len in [32_760, 32_740] {
            let value = "a".repeat(len);
            assert_eq!(version_info([1, 1, 1, 1], &[("k", &value)]), None, "len {len}");
        }
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

        /// Any well-formed string set builds a version block whose root
        /// wLength spans the whole buffer and whose fixed block sits at the
        /// documented offset.
        #[test]
        fn any_version_info_root_length_spans_the_buffer(
            pairs in prop::collection::vec(("[a-zA-Z]{1,12}", "[ -~&&[^\0]]{0,24}"), 0..4),
            quad in any::<[u16; 4]>(),
        ) {
            let strings: Vec<(&str, &str)> =
                pairs.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
            let out = version_info(quad, &strings).expect("well-formed by construction");
            prop_assert_eq!(u16_at(&out, 0).map(usize::from), Some(out.len()));
            prop_assert_eq!(u16_at(&out, 2), Some(52), "wValueLength");
            prop_assert_eq!(u32_at(&out, 40), Some(0xFEEF_04BD), "signature at 40");
            prop_assert_eq!(u32_at(&out, 64), Some(0x3F), "flags mask at 64");
        }
    }
}
