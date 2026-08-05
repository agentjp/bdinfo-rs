//! Disc report composition.
//!
//! The library composes reports as `String`s — printing or saving them is the
//! caller's job (the library never writes to stdout). The one report format is
//! the classic human-readable disc report in [`text`].
//!
//! The file a caller saves it as is part of the same contract, so the name is
//! composed here too: [`file_name`].

pub mod text;

/// Characters illegal in a Windows filename component, plus the separators that
/// would re-root the report path. Replaced so [`file_name`] is always one
/// writable file; applied uniformly on every platform so the report filename is
/// identical everywhere.
const ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// The report's save-file name for a disc labelled `label`: `BDINFO.<stem>.txt`.
///
/// The stem is `label` with each illegal or control character replaced by `_`.
/// A clean label — a folder name, a `.iso` volume identifier, a resolved drive
/// label — passes through unchanged.
///
/// The disc controls its own label bytes and a caller may save without a user in
/// the loop, so the result is always **one flat path component**: a label
/// carrying separators, `..\`, or control characters can neither re-root the
/// path nor break the write.
#[must_use]
pub fn file_name(label: &str) -> String {
    let stem: String = label
        .chars()
        .map(|c| if c.is_control() || ILLEGAL.contains(&c) { '_' } else { c })
        .collect();
    format!("BDINFO.{stem}.txt")
}

#[cfg(test)]
mod tests {
    use super::file_name;

    #[test]
    fn a_clean_label_passes_through() {
        assert_eq!(file_name("MY_DISC"), "BDINFO.MY_DISC.txt");
        assert_eq!(file_name("BigBuckBunny"), "BDINFO.BigBuckBunny.txt");
        assert_eq!(file_name("Blu-Ray"), "BDINFO.Blu-Ray.txt");
        assert_eq!(file_name(""), "BDINFO..txt");
    }

    #[test]
    fn illegal_and_control_characters_become_underscores() {
        assert_eq!(file_name(r"J:\"), "BDINFO.J__.txt");
        assert_eq!(file_name("a/b"), "BDINFO.a_b.txt");
        assert_eq!(file_name("x<y>z|?*\""), "BDINFO.x_y_z____.txt");
        assert_eq!(file_name("tab\there"), "BDINFO.tab_here.txt");
        // The disc-controlled relative-write shape: a label that would
        // lexically escape the chosen directory stays one flat component.
        assert_eq!(file_name(r"..\..\x"), "BDINFO..._.._x.txt");
    }

    mod prop {
        use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest};

        use super::super::{ILLEGAL, file_name};

        proptest! {
            // The write-safety contract: whatever bytes a disc puts in its
            // label, the report filename is one flat component — no
            // separators, no control characters, no illegal-on-Windows chars.
            #[test]
            fn the_name_is_always_one_safe_component(label in any::<String>()) {
                let name = file_name(&label);
                // Byte-exact prefix and suffix (strip, not ends_with — the
                // `.txt` must match exactly, never case-insensitively).
                let stem = name.strip_prefix("BDINFO.").and_then(|rest| rest.strip_suffix(".txt"));
                prop_assert!(stem.is_some(), "the name keeps its fixed shape: {name}");
                // Two assertions, not one `||` predicate: the combined form's
                // second operand is unreachable by the very property being
                // asserted, so it would read as a permanently uncovered branch.
                prop_assert!(!name.chars().any(char::is_control));
                prop_assert!(!name.chars().any(|c| ILLEGAL.contains(&c)));
                // Joining it onto a directory can never re-root the path.
                prop_assert_eq!(std::path::Path::new(&name).components().count(), 1);
            }
        }
    }
}
