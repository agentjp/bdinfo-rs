//! BDMV metadata parsers — the small structural files under `BDMV/`.
//!
//! One module per BDMV file kind: [`clpi`] parses clip-information (`*.clpi`)
//! files (plus the per-clip data model) and [`mpls`] parses playlist (`*.mpls`)
//! files. Both consume a `&[u8]` already read from the VFS and decode it into
//! the [`crate::stream`] data model, returning [`crate::error::BdError`] on
//! malformed input.
//!
//! The disc-level orchestration that wires these together — the [`disc`] scan
//! plus the cross-clip resolution that feeds the [`m2ts`] demux — builds on
//! these standalone parsers.

use crate::bytes;
use crate::error::BdError;

pub mod chapters;
pub mod clpi;
pub mod disc;
pub mod interleaved;
pub mod m2ts;
pub mod mpls;
pub mod order;

// Shared fallible reads: every byte access in the parsers goes through these
// bounds-checked helpers, mapping an out-of-bounds read (a short/truncated
// file) to [`BdError::UnexpectedEof`].

/// Reads the byte at `off`, or [`BdError::UnexpectedEof`] past the end.
pub(crate) fn byte(buf: &[u8], off: usize) -> Result<u8, BdError> {
    bytes::read_u8(buf, off).ok_or(BdError::UnexpectedEof)
}

/// Reads a big-endian `u16` at `off`, or [`BdError::UnexpectedEof`].
pub(crate) fn u16_be(buf: &[u8], off: usize) -> Result<u16, BdError> {
    bytes::read_u16_be(buf, off).ok_or(BdError::UnexpectedEof)
}

/// Reads a big-endian `u32` at `off`, or [`BdError::UnexpectedEof`].
pub(crate) fn u32_be(buf: &[u8], off: usize) -> Result<u32, BdError> {
    bytes::read_u32_be(buf, off).ok_or(BdError::UnexpectedEof)
}

/// Reads a big-endian `u32` at `off` as a `usize` offset, or
/// [`BdError::UnexpectedEof`].
///
/// The full unsigned 32-bit value is taken as an offset; one larger than
/// `usize` (only reachable on a <32-bit target) saturates to `usize::MAX`,
/// which then fails the next bounds check as EOF — the same "malformed" outcome
/// a truncated file produces.
pub(crate) fn u32_off(buf: &[u8], off: usize) -> Result<usize, BdError> {
    let value = bytes::read_u32_be(buf, off).ok_or(BdError::UnexpectedEof)?;
    Ok(usize::try_from(value).unwrap_or(usize::MAX))
}

/// Reads `count` ASCII bytes at `off` (non-ASCII bytes decode to `'?'`), or
/// [`BdError::UnexpectedEof`].
pub(crate) fn ascii(buf: &[u8], off: usize, count: usize) -> Result<String, BdError> {
    bytes::read_ascii(buf, off, count).ok_or(BdError::UnexpectedEof)
}

#[cfg(test)]
mod tests {
    use super::{ascii, byte, u16_be, u32_be, u32_off};

    #[test]
    fn helpers_read_in_bounds_values() {
        let buf = [0x12_u8, 0x34, 0x56, 0x78, b'e', b'n', b'g'];
        // `BdError` is no longer `PartialEq` (its `Io` variant wraps `io::Error`),
        // so the result tests compare the `Ok`/`Err` shape via `.ok()`/`matches!`.
        assert_eq!(byte(&buf, 0).ok(), Some(0x12));
        assert_eq!(u16_be(&buf, 0).ok(), Some(0x1234));
        assert_eq!(u32_be(&buf, 0).ok(), Some(0x1234_5678));
        assert_eq!(u32_off(&buf, 0).ok(), Some(0x1234_5678));
        assert_eq!(ascii(&buf, 4, 3).ok().as_deref(), Some("eng"));
    }

    #[test]
    fn helpers_map_short_reads_to_eof() {
        let buf = [0x12_u8, 0x34];
        // Each short read fails with `UnexpectedEof`; assert on its `Display` (a
        // region-clean check that also pins the error message).
        let eof = "unexpected end of input";
        assert_eq!(byte(&buf, 2).unwrap_err().to_string(), eof);
        assert_eq!(u16_be(&buf, 1).unwrap_err().to_string(), eof);
        assert_eq!(u32_be(&buf, 0).unwrap_err().to_string(), eof);
        assert_eq!(u32_off(&buf, 0).unwrap_err().to_string(), eof);
        assert_eq!(ascii(&buf, 1, 3).unwrap_err().to_string(), eof);
    }
}
