//! `index.bdmv` version detection.
//!
//! The first eight bytes of `index.bdmv` are an ASCII version magic; a disc is
//! UHD (4K) when that magic equals `INDX0300`, while a regular Blu-ray carries
//! `INDX0200`. The read goes through the fixed-length ASCII/magic reader
//! ([`bytes::read_ascii`]).

use crate::bytes;

/// The 8-byte `index.bdmv` magic identifying a UHD (4K) Blu-ray.
const UHD_INDEX_VERSION: &str = "INDX0300";

/// The 4-byte type tag every well-formed `index.bdmv` begins with.
const INDEX_TAG: &[u8; 4] = b"INDX";

/// Whether `index` begins with the 4-byte `INDX` type tag.
///
/// A file that lacks it is still tolerated as non-UHD by [`is_uhd`] (a garbage
/// or truncated `index.bdmv` simply means "not UHD"), but the resilient scan
/// surfaces the missing tag as a warning — where libbluray would hard-reject
/// the file — so a corrupted index is reported instead of silently reading as
/// an SDR disc.
#[must_use]
pub fn has_index_tag(index: &[u8]) -> bool {
    index.get(0..4) == Some(INDEX_TAG.as_slice())
}

/// Reads the 8-byte ASCII version magic at the start of an `index.bdmv` buffer.
///
/// The first eight bytes are decoded as ASCII (any byte `>= 0x80` becomes `'?'`).
/// Returns `None` when fewer than eight bytes are present — a buffer shorter
/// than the 8-character magic can never equal it, so a short or empty read
/// simply leaves the disc classified as non-UHD.
#[must_use]
pub fn read_index_version(index: &[u8]) -> Option<String> {
    bytes::read_ascii(index, 0, 8)
}

/// Returns `true` when `index.bdmv` identifies a UHD (4K) Blu-ray — i.e. its
/// version magic is `INDX0300`.
#[must_use]
pub fn is_uhd(index: &[u8]) -> bool {
    read_index_version(index).as_deref() == Some(UHD_INDEX_VERSION)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest};

    use super::{has_index_tag, is_uhd, read_index_version};

    #[test]
    fn uhd_magic_is_detected() {
        // `INDX0300` (with trailing index bytes) marks a 4K UHD disc.
        let buf = *b"INDX0300\x00\x01\x02\x03";
        assert!(is_uhd(&buf));
        assert_eq!(read_index_version(&buf).as_deref(), Some("INDX0300"));
    }

    #[test]
    fn exact_eight_byte_magic_is_detected() {
        assert!(is_uhd(b"INDX0300"));
        assert_eq!(read_index_version(b"INDX0300").as_deref(), Some("INDX0300"));
    }

    #[test]
    fn regular_bluray_is_not_uhd() {
        // A standard Blu-ray carries `INDX0200`.
        let buf = *b"INDX0200\x00\x00";
        assert!(!is_uhd(&buf));
        assert_eq!(read_index_version(&buf).as_deref(), Some("INDX0200"));
    }

    #[test]
    fn short_or_empty_buffer_is_not_uhd() {
        // Fewer than 8 bytes => no version => not UHD.
        assert!(!is_uhd(b"INDX030"));
        assert_eq!(read_index_version(b"INDX030"), None);
        assert!(!is_uhd(b""));
        assert_eq!(read_index_version(b""), None);
    }

    #[test]
    fn index_tag_is_detected_independently_of_the_version() {
        // The tag gate accepts any `INDX` magic — SDR (`INDX0200`), UHD
        // (`INDX0300`), even an unknown version — and rejects garbage or a
        // buffer too short to carry the 4-byte tag.
        assert!(has_index_tag(b"INDX0200"));
        assert!(has_index_tag(b"INDX0300"));
        assert!(has_index_tag(b"INDX9999"));
        assert!(has_index_tag(b"INDX")); // exactly the tag, nothing more
        assert!(!has_index_tag(b"XXXXjunk"));
        assert!(!has_index_tag(b"IND")); // one byte short of the tag
        assert!(!has_index_tag(b""));
    }

    proptest! {
        #[test]
        fn is_uhd_iff_first_eight_bytes_are_the_magic(buf in any::<Vec<u8>>()) {
            // The magic is pure ASCII (no byte >= 0x80), so `read_ascii`'s '?'
            // substitution can never forge a match: UHD exactly when the first
            // eight bytes equal `INDX0300`.
            let expected = buf.get(0..8) == Some(b"INDX0300".as_slice());
            prop_assert_eq!(is_uhd(&buf), expected);
        }

        #[test]
        fn magic_prefix_is_always_uhd(suffix in any::<Vec<u8>>()) {
            let mut buf = b"INDX0300".to_vec();
            buf.extend_from_slice(&suffix);
            prop_assert!(is_uhd(&buf));
            let version = read_index_version(&buf);
            prop_assert_eq!(version.as_deref(), Some("INDX0300"));
        }
    }
}
