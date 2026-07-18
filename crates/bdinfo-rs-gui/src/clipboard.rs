//! Tier A — clipboard text sanitizing.
//!
//! The rendered report can carry literal NUL bytes: a degenerate disc with a
//! zeroed 3-letter language code renders those bytes verbatim (the report is
//! byte-locked, so they stay). The OS clipboard cannot: Windows'
//! `CF_UNICODETEXT` is NUL-terminated, so every consumer truncates the paste
//! at the first NUL (verified live — a whole-report copy of the committed
//! fixture delivered 2135 of 4242 chars). The fix lives at the clipboard
//! boundary only: [`sanitize`] substitutes each NUL before the text is handed
//! to the clipboard, on **every** platform (iced's clipboard is
//! cross-platform; behavior must not diverge per OS). The report itself —
//! rendered, displayed, and saved — is untouched.

/// What a NUL becomes on the clipboard: U+FFFD REPLACEMENT CHARACTER.
///
/// The Unicode-designated stand-in for unrepresentable data. It is visible
/// (a pasted report shows *something* was there, unlike a space) and one
/// char per NUL, so the report's monospace column alignment survives the
/// paste.
pub const NUL_REPLACEMENT: char = '\u{FFFD}';

/// Prepares `text` for the OS clipboard: every NUL becomes
/// [`NUL_REPLACEMENT`]; everything else passes through unchanged. NUL-free
/// text (every real disc) is returned as-is, allocation-free.
#[must_use]
pub fn sanitize(text: String) -> String {
    if text.contains('\0') {
        // The literal is NUL_REPLACEMENT spelled as a &str (`replace` wants
        // one); the proptest below pins the two together.
        text.replace('\0', "\u{FFFD}")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{NUL_REPLACEMENT, sanitize};

    #[test]
    fn nul_free_text_passes_through_unchanged() {
        assert_eq!(sanitize(String::new()), "");
        let report = "Disc Label: DISC\r\nAudio: DTS-HD Master Audio\r\n".to_owned();
        assert_eq!(sanitize(report.clone()), report);
    }

    #[test]
    fn every_nul_is_substituted() {
        // The report shape that bit: a zeroed 3-letter language code inline.
        let line = "Audio: \0\0\0 (DTS-HD Master Audio)".to_owned();
        assert_eq!(sanitize(line), "Audio: \u{FFFD}\u{FFFD}\u{FFFD} (DTS-HD Master Audio)");
        // Boundary positions too, not just the middle.
        assert_eq!(sanitize("\0mid\0end\0".to_owned()), "\u{FFFD}mid\u{FFFD}end\u{FFFD}");
        assert_eq!(sanitize("\0".to_owned()), NUL_REPLACEMENT.to_string());
    }

    proptest! {
        // The clipboard contract: no NUL survives, the char count is
        // preserved (one replacement per NUL — alignment holds), and every
        // non-NUL char passes through untouched.
        #[test]
        fn sanitized_text_maps_chars_one_to_one(text in any::<String>()) {
            let out = sanitize(text.clone());
            prop_assert!(!out.contains('\0'));
            prop_assert_eq!(out.chars().count(), text.chars().count());
            for (before, after) in text.chars().zip(out.chars()) {
                let expected = if before == '\0' { NUL_REPLACEMENT } else { before };
                prop_assert_eq!(after, expected);
            }
            // Already-clean text is a fixed point (sanitizing twice is safe).
            prop_assert_eq!(sanitize(out.clone()), out);
        }
    }
}
