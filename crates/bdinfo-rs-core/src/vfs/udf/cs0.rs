//! OSTA Compressed Unicode (CS0) string decoding — UDF file/volume identifiers.
//!
//! UDF stores identifiers in the OSTA CS0 character set (UDF 2.50 §2.1.1). A CS0
//! byte run begins with a **compression ID**:
//! - `8`  — each following byte is one character (code points `U+0000..=U+00FF`, i.e. Latin-1).
//! - `16` — each following pair of bytes is one UTF-16 **big-endian** code unit.
//!
//! Two field shapes use it:
//! - [`decode_dchars`] — a "d-characters" run whose length is given externally (the FID
//!   `FileIdentifier`, length `L_FI`).
//! - [`decode_dstring`] — a fixed-size "dstring" field whose **last byte** holds the number of
//!   bytes used at the front (the volume/file-set identifiers).
//!
//! Both are total and panic-free over untrusted bytes: an unknown compression ID
//! or a malformed length yields an empty string rather than an error (a read-only
//! analyzer has no use for failing a scan over a cosmetic identifier), and a
//! UTF-16 surrogate that does not form a valid scalar value decodes to U+FFFD.

/// Decodes an OSTA CS0 "d-characters" run: `data[0]` is the compression ID and
/// the remaining bytes are the characters (UDF 2.50 §2.1.1).
///
/// Returns the empty string for an empty run, a compression-ID-only run, or an
/// unrecognized compression ID. Used for the FID `FileIdentifier` field, whose
/// length is the FID's `LengthofFileIdentifier`.
#[must_use]
pub fn decode_dchars(data: &[u8]) -> String {
    let Some(&comp_id) = data.first() else {
        return String::new();
    };
    let body = data.get(1..).unwrap_or_default();
    match comp_id {
        8 => body.iter().map(|&b| char::from(b)).collect(),
        16 => {
            // 16-bit code units, big-endian; pair up the bytes (an odd trailing
            // byte is dropped) and decode as UTF-16 so surrogate pairs join.
            let units = body.chunks_exact(2).map(|pair| {
                let hi = pair.first().copied().unwrap_or(0);
                let lo = pair.get(1).copied().unwrap_or(0);
                u16::from_be_bytes([hi, lo])
            });
            char::decode_utf16(units).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
        }
        // UDF only defines 8 and 16 here (254/255 carry no characters); anything
        // else is malformed input — decode to nothing rather than panicking.
        _ => String::new(),
    }
}

/// Decodes a fixed-size OSTA CS0 "dstring" field of `field.len()` bytes, whose
/// **last byte** records how many leading bytes are used (UDF 2.50 §2.1.3).
///
/// The used bytes (compression ID + characters) are decoded via
/// [`decode_dchars`]. Returns the empty string for an empty field or a used
/// length that runs past the field (malformed). Used for the volume / file-set
/// identifiers in the LVD and FSD.
#[must_use]
pub fn decode_dstring(field: &[u8]) -> String {
    let Some(&used_len) = field.last() else {
        return String::new();
    };
    let used = usize::from(used_len);
    // The used bytes (compression ID + characters) sit at the front of the
    // field; an out-of-range used length is malformed → empty.
    field.get(..used).map_or_else(String::new, decode_dchars)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest};

    use super::{decode_dchars, decode_dstring};

    #[test]
    fn dchars_compression_8_is_latin1() {
        // compId 8, then the bytes for "BDMV".
        let data = [8_u8, b'B', b'D', b'M', b'V'];
        assert_eq!(decode_dchars(&data), "BDMV");
    }

    #[test]
    fn dchars_compression_8_decodes_high_bytes_as_latin1() {
        // 0xE9 → U+00E9 'é' under the 8-bit code-point mapping.
        let data = [8_u8, 0xE9];
        assert_eq!(decode_dchars(&data), "é");
    }

    #[test]
    fn dchars_compression_16_is_utf16_be() {
        // compId 16, then UTF-16BE for "Aé" (U+0041, U+00E9).
        let data = [16_u8, 0x00, 0x41, 0x00, 0xE9];
        assert_eq!(decode_dchars(&data), "Aé");
    }

    #[test]
    fn dchars_compression_16_joins_surrogate_pairs() {
        // U+1F600 😀 = surrogate pair D83D DE00, big-endian.
        let data = [16_u8, 0xD8, 0x3D, 0xDE, 0x00];
        assert_eq!(decode_dchars(&data), "😀");
    }

    #[test]
    fn dchars_compression_16_lone_surrogate_is_replacement() {
        // A lone high surrogate decodes to U+FFFD rather than panicking.
        let data = [16_u8, 0xD8, 0x3D];
        assert_eq!(decode_dchars(&data), "\u{FFFD}");
    }

    #[test]
    fn dchars_compression_16_drops_odd_trailing_byte() {
        // 'A' then a dangling byte — the incomplete code unit is ignored.
        let data = [16_u8, 0x00, 0x41, 0x00];
        assert_eq!(decode_dchars(&data), "A");
    }

    #[test]
    fn dchars_empty_and_id_only_are_empty() {
        assert_eq!(decode_dchars(&[]), "");
        assert_eq!(decode_dchars(&[8]), "");
        assert_eq!(decode_dchars(&[16]), "");
    }

    #[test]
    fn dchars_unknown_compression_is_empty() {
        assert_eq!(decode_dchars(&[0, b'x', b'y']), "");
        assert_eq!(decode_dchars(&[255, 1, 2, 3]), "");
    }

    #[test]
    fn dstring_uses_trailing_length_byte() {
        // A 12-byte field: compId 8 + "PLAYLIST" (8 chars) → 9 used bytes; the
        // last byte stores 9, and the slack between is padding.
        let field = [8_u8, b'P', b'L', b'A', b'Y', b'L', b'I', b'S', b'T', 0, 0, 9];
        assert_eq!(decode_dstring(&field), "PLAYLIST");
        // The same field with a used-length of 7 keeps only compId + 6 chars.
        let shorter = [8_u8, b'P', b'L', b'A', b'Y', b'L', b'I', b'S', b'T', 0, 0, 7];
        assert_eq!(decode_dstring(&shorter), "PLAYLI");
    }

    #[test]
    fn dstring_zero_used_is_empty() {
        let field = [8_u8, b'X', b'Y', 0];
        assert_eq!(decode_dstring(&field), "");
    }

    #[test]
    fn dstring_empty_or_overlong_used_is_empty() {
        assert_eq!(decode_dstring(&[]), "");
        // used-length 200 in a 4-byte field → past the end → empty (no panic).
        assert_eq!(decode_dstring(&[8, b'A', b'B', 200]), "");
    }

    proptest! {
        #[test]
        fn dchars_never_panics(data in any::<Vec<u8>>()) {
            let s = decode_dchars(&data);
            // compId 8 emits exactly one char per body byte.
            if data.first() == Some(&8) {
                prop_assert_eq!(s.chars().count(), data.len().saturating_sub(1));
            }
            // Every decoding emits at most one character per input byte.
            prop_assert!(s.chars().count() <= data.len());
        }

        #[test]
        fn dstring_never_panics(field in any::<Vec<u8>>()) {
            let s = decode_dstring(&field);
            prop_assert!(s.chars().count() <= field.len());
        }

        #[test]
        fn dstring_equals_dchars_of_used_prefix(body in any::<Vec<u8>>(), used in 0_usize..40) {
            // Build a field whose trailing byte names `used`, capped to the body.
            let used = used.min(body.len());
            let mut field = body.clone();
            field.push(u8::try_from(used).unwrap_or(0));
            let expected = body.get(..used).map(decode_dchars).unwrap_or_default();
            prop_assert_eq!(decode_dstring(&field), expected);
        }
    }
}
