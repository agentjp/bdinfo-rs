//! Big-endian byte readers — the bounded, panic-free primitives every BDMV and
//! transport-stream parser is built from.
//!
//! Blu-ray on-disc structures are big-endian and host-independent, so these
//! readers assemble values through [`u16::from_be_bytes`] / [`u32::from_be_bytes`]
//! and bounds-checked slicing — they can never panic or read out of bounds.
//! A read past the end of `buf` returns `None`; the caller decides whether
//! that's legal EOF or corruption.

/// Reads a single `u8` from `buf` at `offset`.
///
/// Bounds-checked: returns `None` if `offset` is past the end of `buf`.
#[must_use]
pub fn read_u8(buf: &[u8], offset: usize) -> Option<u8> {
    buf.get(offset).copied()
}

/// Reads a big-endian `u16` from `buf` at `offset`.
///
/// Returns `None` if the two bytes don't fit within `buf`.
#[must_use]
pub fn read_u16_be(buf: &[u8], offset: usize) -> Option<u16> {
    let chunk = buf.get(offset..)?.first_chunk::<2>()?;
    Some(u16::from_be_bytes(*chunk))
}

/// Reads a big-endian 24-bit value from `buf` at `offset` into a `u32`.
///
/// The three bytes occupy the low 24 bits; the top byte of the result is zero.
/// Returns `None` if the three bytes don't fit within `buf`.
#[must_use]
pub fn read_u24_be(buf: &[u8], offset: usize) -> Option<u32> {
    let &[a, b, c] = buf.get(offset..)?.first_chunk::<3>()?;
    Some(u32::from_be_bytes([0, a, b, c]))
}

/// Reads a big-endian `u32` from `buf` at `offset`.
///
/// Returns `None` if the four bytes don't fit within `buf`.
#[must_use]
pub fn read_u32_be(buf: &[u8], offset: usize) -> Option<u32> {
    let chunk = buf.get(offset..)?.first_chunk::<4>()?;
    Some(u32::from_be_bytes(*chunk))
}

/// Reads a big-endian `u64` from `buf` at `offset`.
///
/// Returns `None` if the eight bytes don't fit within `buf`.
#[must_use]
pub fn read_u64_be(buf: &[u8], offset: usize) -> Option<u64> {
    let chunk = buf.get(offset..)?.first_chunk::<8>()?;
    Some(u64::from_be_bytes(*chunk))
}

/// Reads a big-endian unsigned integer of `n` bytes from `buf` at `offset`.
///
/// This is the variable-width generalisation of the fixed readers above.
/// `n` must be in `1..=8`; any other width returns `None`, as does
/// a read that runs past the end of `buf`.
#[must_use]
pub fn read_uint_be(buf: &[u8], offset: usize, n: usize) -> Option<u64> {
    // A `u64` holds at most 8 bytes; 0 bytes has no value. Reject both.
    if n == 0 || n > 8 {
        return None;
    }
    let end = offset.checked_add(n)?;
    let slice = buf.get(offset..end)?;
    // Big-endian shift-accumulate, one byte at a time; the width guard above
    // means the `wrapping_*` fixed-width ops can never actually wrap.
    Some(slice.iter().fold(0_u64, |acc, &b| acc.wrapping_shl(8).wrapping_add(u64::from(b))))
}

/// Reads a fixed-length ASCII string of `count` bytes from `buf` at `offset`.
///
/// Each byte `0x00..=0x7F` decodes to its ASCII character and any byte `>= 0x80`
/// decodes to the replacement character `'?'`. This
/// is also the fixed-length magic reader used by `index.bdmv` (`INDX0300`),
/// `*.mpls` (`MPLS0100`), and `*.clpi` (`HDMV0100`). Returns `None` if the
/// `count` bytes don't fit within `buf`.
#[must_use]
pub fn read_ascii(buf: &[u8], offset: usize, count: usize) -> Option<String> {
    let end = offset.checked_add(count)?;
    let slice = buf.get(offset..end)?;
    Some(slice.iter().map(|&b| if b.is_ascii() { char::from(b) } else { '?' }).collect())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest};

    use super::{
        read_ascii, read_u8, read_u16_be, read_u24_be, read_u32_be, read_u64_be, read_uint_be,
    };

    #[test]
    fn read_u8_reads_single_byte() {
        let buf = [0x12_u8, 0x34];
        assert_eq!(read_u8(&buf, 0), Some(0x12));
        assert_eq!(read_u8(&buf, 1), Some(0x34));
    }

    #[test]
    fn read_u8_out_of_bounds_is_none() {
        let buf = [0x12_u8];
        assert_eq!(read_u8(&buf, 1), None);
        assert_eq!(read_u8(&buf, usize::MAX), None);
    }

    #[test]
    fn read_u16_be_reads_big_endian() {
        let buf = [0x12_u8, 0x34, 0x56];
        assert_eq!(read_u16_be(&buf, 0), Some(0x1234));
        assert_eq!(read_u16_be(&buf, 1), Some(0x3456));
    }

    #[test]
    fn read_u16_be_out_of_bounds_is_none() {
        let buf = [0x12_u8];
        assert_eq!(read_u16_be(&buf, 0), None);
        assert_eq!(read_u16_be(&buf, 5), None);
        assert_eq!(read_u16_be(&buf, usize::MAX), None);
    }

    #[test]
    fn read_u24_be_reads_big_endian() {
        let buf = [0x12_u8, 0x34, 0x56, 0x78];
        assert_eq!(read_u24_be(&buf, 0), Some(0x0012_3456));
        assert_eq!(read_u24_be(&buf, 1), Some(0x0034_5678));
    }

    #[test]
    fn read_u24_be_out_of_bounds_is_none() {
        let buf = [0x12_u8, 0x34];
        assert_eq!(read_u24_be(&buf, 0), None);
        assert_eq!(read_u24_be(&buf, usize::MAX), None);
    }

    #[test]
    fn read_u32_be_reads_big_endian() {
        let buf = [0xDE_u8, 0xAD, 0xBE, 0xEF, 0x00];
        assert_eq!(read_u32_be(&buf, 0), Some(0xDEAD_BEEF));
        assert_eq!(read_u32_be(&buf, 1), Some(0xADBE_EF00));
    }

    #[test]
    fn read_u32_be_out_of_bounds_is_none() {
        let buf = [0x00_u8, 0x11, 0x22];
        assert_eq!(read_u32_be(&buf, 0), None);
        assert_eq!(read_u32_be(&buf, usize::MAX), None);
    }

    #[test]
    fn read_u64_be_reads_big_endian() {
        let buf = [0x01_u8, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x00];
        assert_eq!(read_u64_be(&buf, 0), Some(0x0123_4567_89AB_CDEF));
        assert_eq!(read_u64_be(&buf, 1), Some(0x2345_6789_ABCD_EF00));
    }

    #[test]
    fn read_u64_be_out_of_bounds_is_none() {
        let buf = [0x00_u8; 7];
        assert_eq!(read_u64_be(&buf, 0), None);
        assert_eq!(read_u64_be(&buf, usize::MAX), None);
    }

    #[test]
    fn read_uint_be_reads_each_width() {
        let buf = [0x11_u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        assert_eq!(read_uint_be(&buf, 0, 1), Some(0x11));
        assert_eq!(read_uint_be(&buf, 0, 2), Some(0x1122));
        assert_eq!(read_uint_be(&buf, 0, 3), Some(0x0011_2233));
        assert_eq!(read_uint_be(&buf, 0, 4), Some(0x1122_3344));
        assert_eq!(read_uint_be(&buf, 0, 8), Some(0x1122_3344_5566_7788));
    }

    #[test]
    fn read_uint_be_rejects_zero_and_overwide_widths() {
        let buf = [0x11_u8; 16];
        assert_eq!(read_uint_be(&buf, 0, 0), None);
        assert_eq!(read_uint_be(&buf, 0, 9), None);
    }

    #[test]
    fn read_uint_be_out_of_bounds_is_none() {
        let buf = [0x11_u8, 0x22];
        assert_eq!(read_uint_be(&buf, 0, 3), None);
        assert_eq!(read_uint_be(&buf, usize::MAX, 1), None);
    }

    #[test]
    fn read_ascii_reads_index_magic() {
        // The 8-byte magic `index.bdmv` carries; `INDX0300` marks a UHD disc.
        let buf = *b"INDX0300\x00\x00";
        assert_eq!(read_ascii(&buf, 0, 8).as_deref(), Some("INDX0300"));
    }

    #[test]
    fn read_ascii_reads_offset_magic() {
        // A fixed-length read partway into a buffer, as the MPLS/CLPI parsers do.
        let buf = *b"....MPLS0100";
        assert_eq!(read_ascii(&buf, 4, 8).as_deref(), Some("MPLS0100"));
    }

    #[test]
    fn read_ascii_replaces_non_ascii_with_question_mark() {
        // Any byte >= 0x80 decodes to '?'.
        let buf = [b'A', 0x80, 0xFF, b'Z'];
        assert_eq!(read_ascii(&buf, 0, 4).as_deref(), Some("A??Z"));
    }

    #[test]
    fn read_ascii_zero_count_is_empty() {
        let buf = [b'A', b'B'];
        assert_eq!(read_ascii(&buf, 0, 0).as_deref(), Some(""));
    }

    #[test]
    fn read_ascii_out_of_bounds_is_none() {
        let buf = [b'A', b'B'];
        assert_eq!(read_ascii(&buf, 0, 3), None);
        assert_eq!(read_ascii(&buf, usize::MAX, 1), None);
    }

    proptest! {
        #[test]
        fn read_u8_never_panics(buf in any::<Vec<u8>>(), offset in any::<usize>()) {
            let got = read_u8(&buf, offset);
            prop_assert_eq!(got.is_some(), offset < buf.len());
        }

        #[test]
        fn read_u16_be_never_panics(buf in any::<Vec<u8>>(), offset in any::<usize>()) {
            let got = read_u16_be(&buf, offset);
            // In-bounds reads must produce a value; out-of-bounds must be None.
            prop_assert_eq!(got.is_some(), offset.checked_add(2).is_some_and(|e| e <= buf.len()));
        }

        #[test]
        fn read_u16_be_matches_from_be_bytes(prefix in any::<Vec<u8>>(), a: u8, b: u8) {
            let offset = prefix.len();
            let mut buf = prefix;
            buf.push(a);
            buf.push(b);
            prop_assert_eq!(read_u16_be(&buf, offset), Some(u16::from_be_bytes([a, b])));
        }

        #[test]
        fn read_u24_be_never_panics(buf in any::<Vec<u8>>(), offset in any::<usize>()) {
            let got = read_u24_be(&buf, offset);
            prop_assert_eq!(got.is_some(), offset.checked_add(3).is_some_and(|e| e <= buf.len()));
        }

        #[test]
        fn read_u24_be_matches_from_be_bytes(prefix in any::<Vec<u8>>(), a: u8, b: u8, c: u8) {
            let offset = prefix.len();
            let mut buf = prefix;
            buf.extend_from_slice(&[a, b, c]);
            prop_assert_eq!(read_u24_be(&buf, offset), Some(u32::from_be_bytes([0, a, b, c])));
        }

        #[test]
        fn read_u32_be_never_panics(buf in any::<Vec<u8>>(), offset in any::<usize>()) {
            let got = read_u32_be(&buf, offset);
            prop_assert_eq!(got.is_some(), offset.checked_add(4).is_some_and(|e| e <= buf.len()));
        }

        #[test]
        fn read_u64_be_never_panics(buf in any::<Vec<u8>>(), offset in any::<usize>()) {
            let got = read_u64_be(&buf, offset);
            prop_assert_eq!(got.is_some(), offset.checked_add(8).is_some_and(|e| e <= buf.len()));
        }

        #[test]
        fn read_u64_be_matches_from_be_bytes(
            prefix in any::<Vec<u8>>(),
            chunk in any::<[u8; 8]>(),
        ) {
            let offset = prefix.len();
            let mut buf = prefix;
            buf.extend_from_slice(&chunk);
            prop_assert_eq!(read_u64_be(&buf, offset), Some(u64::from_be_bytes(chunk)));
        }

        #[test]
        fn read_uint_be_never_panics(
            buf in any::<Vec<u8>>(),
            offset in any::<usize>(),
            n in 0_usize..12,
        ) {
            let got = read_uint_be(&buf, offset, n);
            let valid = (1..=8).contains(&n)
                && offset.checked_add(n).is_some_and(|e| e <= buf.len());
            prop_assert_eq!(got.is_some(), valid);
        }

        #[test]
        fn read_uint_be_matches_fixed_width_readers(
            buf in any::<Vec<u8>>(),
            offset in any::<usize>(),
        ) {
            prop_assert_eq!(read_uint_be(&buf, offset, 1), read_u8(&buf, offset).map(u64::from));
            prop_assert_eq!(read_uint_be(&buf, offset, 2), read_u16_be(&buf, offset).map(u64::from));
            prop_assert_eq!(read_uint_be(&buf, offset, 3), read_u24_be(&buf, offset).map(u64::from));
            prop_assert_eq!(read_uint_be(&buf, offset, 4), read_u32_be(&buf, offset).map(u64::from));
            prop_assert_eq!(read_uint_be(&buf, offset, 8), read_u64_be(&buf, offset));
        }

        #[test]
        fn read_ascii_never_panics(
            buf in any::<Vec<u8>>(),
            offset in any::<usize>(),
            count in 0_usize..64,
        ) {
            let got = read_ascii(&buf, offset, count);
            let valid = offset.checked_add(count).is_some_and(|e| e <= buf.len());
            prop_assert_eq!(got.is_some(), valid);
            if let Some(s) = got {
                // One char emitted per source byte (ASCII or the '?' replacement).
                prop_assert_eq!(s.chars().count(), count);
            }
        }

        #[test]
        fn read_ascii_roundtrips_ascii(s in "[ -~]{0,32}") {
            let bytes = s.as_bytes();
            let got = read_ascii(bytes, 0, bytes.len());
            prop_assert_eq!(got.as_deref(), Some(s.as_str()));
        }

        #[test]
        fn read_ascii_high_bytes_become_question_mark(byte in 0x80_u8..=0xFF) {
            let buf = [byte];
            let s = read_ascii(&buf, 0, 1);
            prop_assert_eq!(s.as_deref(), Some("?"));
            prop_assert!(s.is_some());
        }
    }
}
