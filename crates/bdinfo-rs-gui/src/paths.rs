//! The copy-path helpers behind the Ctrl+C shortcut.
//!
//! `BDInfo`'s `ProcessCmdKey` copies the highlighted playlist's or stream
//! file's real OS path. For **folder** input the disc's `BDMV` directory is
//! resolved from the picked path by pure path math ([`bdmv_dir`]) and the
//! copied path is `<BDMV>/PLAYLIST/<name>` or `<BDMV>/STREAM/<clip>` in the
//! platform's separators. For **`.iso`** input there is no OS path (`BDInfo`
//! never had isos), so the copied form is the stable virtual shape
//! `<iso-path>::BDMV/PLAYLIST/<name>` (and `…::BDMV/STREAM/<clip>`): the
//! image's OS path, a literal `::` separator, then the in-image path with `/`
//! separators — unambiguous, and greppable back to the image.
//!
//! The report's save paths live here too: the autosave destination implied
//! by the input ([`autosave_dir`]) and the disc-label-safe report filename
//! ([`report_file_name`]).

use std::path::{Path, PathBuf};

use crate::scan::Input;

/// The disc's `BDMV` directory implied by the picked `input` path.
///
/// The nearest self-or-ancestor component named `BDMV` (ASCII
/// case-insensitive, matching the core's disc-root walk), else `input` is
/// taken as the disc root and `BDMV` is joined beneath it. Pure path math —
/// the structural scan already proved the structure exists, so no filesystem
/// probe is needed here.
#[must_use]
pub fn bdmv_dir(input: &Path) -> PathBuf {
    input
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name.eq_ignore_ascii_case("BDMV")))
        .map_or_else(|| input.join("BDMV"), Path::to_path_buf)
}

/// The Ctrl+C copy target for a playlist row: `<BDMV>/PLAYLIST/<name>` — the
/// real OS path for a folder disc, the module's virtual `::` form for an
/// `.iso`.
#[must_use]
pub fn playlist_path(input: &Input, name: &str) -> String {
    disc_path(input, "PLAYLIST", name)
}

/// The Ctrl+C copy target for a Stream File row: `<BDMV>/STREAM/<clip>`.
///
/// `clip` is the real `xxxxx.m2ts` stream-file name. The path is the real OS
/// path for a folder disc, the module's virtual `::` form for an `.iso`.
#[must_use]
pub fn stream_path(input: &Input, clip: &str) -> String {
    disc_path(input, "STREAM", clip)
}

/// The autosave destination for a finished measured scan's report.
///
/// `BDInfo`'s `AutosaveReport` writes without asking, so the destination must
/// be implied by the input: the **disc folder** (the directory holding
/// `BDMV`) for folder input, the directory **next to the image** for `.iso`
/// input. Pure path math; `None` when the input path has no parent to write
/// into (not the case for any real disc path — the caller surfaces it as an
/// ordinary save failure).
#[must_use]
pub fn autosave_dir(input: &Input) -> Option<PathBuf> {
    let dir = match input {
        Input::Folder(path) => bdmv_dir(path).parent().map(Path::to_path_buf),
        Input::Iso(path) => path.parent().map(Path::to_path_buf),
    };
    // A bare relative name yields an empty parent — no destination, never an
    // accidental write into the current directory.
    dir.filter(|parent| !parent.as_os_str().is_empty())
}

/// Characters illegal in a Windows filename component, plus the separators
/// that would re-root the report path. Replaced so `BDINFO.<stem>.txt` is
/// always one writable file; applied uniformly on every platform so the
/// report filename is identical everywhere. Replicates the CLI's
/// `volume::ILLEGAL` (`crates/bdinfo-rs/src/volume.rs`) — keep in lock-step.
const ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// The report's save-file name for a disc labelled `label`:
/// `BDINFO.<stem>.txt`, the stem being `label` with each illegal or control
/// character replaced by `_`.
///
/// The same name the CLI writes for the same disc (its
/// `volume::safe_report_stem`, replicated here because the crates share no
/// code; keep the two in lock-step). A disc controls its label bytes and
/// autosave writes with no user in the loop, so a hostile label (separators,
/// `..\`, control chars) must never re-root the path or break the write; a
/// clean label passes through unchanged.
#[must_use]
pub fn report_file_name(label: &str) -> String {
    let stem: String = label
        .chars()
        .map(|c| if c.is_control() || ILLEGAL.contains(&c) { '_' } else { c })
        .collect();
    format!("BDINFO.{stem}.txt")
}

/// Builds the copy target for the `BDMV/<dir>/<name>` disc entry.
fn disc_path(input: &Input, dir: &str, name: &str) -> String {
    match input {
        Input::Folder(path) => {
            let mut full = bdmv_dir(path);
            full.push(dir);
            full.push(name);
            full.display().to_string()
        }
        Input::Iso(path) => format!("{}::BDMV/{dir}/{name}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{MAIN_SEPARATOR as SEP, Path, PathBuf};

    use super::{bdmv_dir, playlist_path, stream_path};
    use crate::scan::Input;

    #[test]
    fn bdmv_dir_joins_beneath_a_disc_root() {
        let root = Path::new("discs").join("MY_DISC");
        assert_eq!(bdmv_dir(&root), root.join("BDMV"));
    }

    #[test]
    fn bdmv_dir_takes_the_bdmv_directory_itself() {
        let bdmv = Path::new("discs").join("MY_DISC").join("BDMV");
        assert_eq!(bdmv_dir(&bdmv), bdmv);
    }

    #[test]
    fn bdmv_dir_walks_up_from_a_directory_inside_bdmv() {
        let bdmv = Path::new("discs").join("MY_DISC").join("BDMV");
        assert_eq!(bdmv_dir(&bdmv.join("PLAYLIST")), bdmv);
        assert_eq!(bdmv_dir(&bdmv.join("STREAM").join("SSIF")), bdmv);
    }

    #[test]
    fn bdmv_dir_matches_the_component_case_insensitively() {
        let lower = Path::new("discs").join("MY_DISC").join("bdmv");
        assert_eq!(bdmv_dir(&lower.join("PLAYLIST")), lower);
    }

    #[test]
    fn bdmv_dir_ignores_a_lookalike_component() {
        // "BDMV-backup" is not "BDMV": the picked path reads as a disc root.
        let root = Path::new("BDMV-backup").join("MY_DISC");
        assert_eq!(bdmv_dir(&root), root.join("BDMV"));
    }

    #[test]
    fn folder_paths_are_real_os_paths() {
        let input = Input::Folder(Path::new("discs").join("MY_DISC"));
        assert_eq!(
            playlist_path(&input, "00000.MPLS"),
            format!("discs{SEP}MY_DISC{SEP}BDMV{SEP}PLAYLIST{SEP}00000.MPLS")
        );
        assert_eq!(
            stream_path(&input, "00001.M2TS"),
            format!("discs{SEP}MY_DISC{SEP}BDMV{SEP}STREAM{SEP}00001.M2TS")
        );
    }

    #[test]
    fn iso_paths_use_the_documented_virtual_form() {
        let input = Input::Iso(PathBuf::from("disc.iso"));
        assert_eq!(playlist_path(&input, "00000.MPLS"), "disc.iso::BDMV/PLAYLIST/00000.MPLS");
        assert_eq!(stream_path(&input, "00001.M2TS"), "disc.iso::BDMV/STREAM/00001.M2TS");
    }

    #[test]
    fn autosave_lands_in_the_disc_folder_for_folder_input() {
        let root = Path::new("discs").join("MY_DISC");
        // Whether the pick was the disc root, BDMV itself, or a directory
        // inside it, the report autosaves into the disc folder — the CLI's
        // default REPORT_DEST.
        assert_eq!(super::autosave_dir(&Input::Folder(root.clone())), Some(root.clone()));
        assert_eq!(super::autosave_dir(&Input::Folder(root.join("BDMV"))), Some(root.clone()));
        assert_eq!(
            super::autosave_dir(&Input::Folder(root.join("BDMV").join("PLAYLIST"))),
            Some(root)
        );
    }

    #[test]
    fn autosave_lands_next_to_the_image_for_iso_input() {
        let iso = Path::new("rips").join("disc.iso");
        assert_eq!(super::autosave_dir(&Input::Iso(iso)), Some(PathBuf::from("rips")));
        // A bare relative file name has no usable parent — no destination.
        assert_eq!(super::autosave_dir(&Input::Iso(PathBuf::from("disc.iso"))), None);
    }

    #[test]
    fn report_file_name_passes_a_clean_label_through() {
        // The same fixtures the CLI's `safe_report_stem` tests pin.
        assert_eq!(super::report_file_name("MY_DISC"), "BDINFO.MY_DISC.txt");
        assert_eq!(super::report_file_name("BigBuckBunny"), "BDINFO.BigBuckBunny.txt");
        assert_eq!(super::report_file_name("Blu-Ray"), "BDINFO.Blu-Ray.txt");
        assert_eq!(super::report_file_name(""), "BDINFO..txt");
    }

    #[test]
    fn report_file_name_replaces_illegal_and_control_characters() {
        assert_eq!(super::report_file_name(r"J:\"), "BDINFO.J__.txt");
        assert_eq!(super::report_file_name("a/b"), "BDINFO.a_b.txt");
        assert_eq!(super::report_file_name("x<y>z|?*\""), "BDINFO.x_y_z____.txt");
        assert_eq!(super::report_file_name("tab\there"), "BDINFO.tab_here.txt");
        // The disc-controlled relative-write shape: a label that would
        // lexically escape the chosen directory stays one flat component.
        assert_eq!(super::report_file_name(r"..\..\x"), "BDINFO..._.._x.txt");
    }

    mod prop {
        use proptest::prelude::*;

        proptest! {
            // The write-safety contract: whatever bytes a disc puts in its
            // label, the report filename is one flat component — no
            // separators, no control characters, no illegal-on-Windows chars.
            #[test]
            fn report_file_name_is_always_one_safe_component(label in any::<String>()) {
                let name = super::super::report_file_name(&label);
                // Byte-exact prefix and suffix (strip, not ends_with — the
                // `.txt` must match exactly, never case-insensitively).
                let stem = name.strip_prefix("BDINFO.").and_then(|rest| rest.strip_suffix(".txt"));
                prop_assert!(stem.is_some(), "the name keeps its fixed shape: {name}");
                prop_assert!(
                    !name.chars().any(|c| c.is_control() || super::super::ILLEGAL.contains(&c))
                );
                // Joining it onto a directory can never re-root the path.
                prop_assert_eq!(std::path::Path::new(&name).components().count(), 1);
            }
        }
    }
}
