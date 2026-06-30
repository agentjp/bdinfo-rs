//! Golden-tie: lock the GUI's scan → render wiring against the committed disc.
//!
//! Phase 1's UI only runs the structural scan, but the rendered report contract
//! is the product. This drives the **measured** seam
//! ([`scan::scan_folder_measured`], the `--whole` path) over the committed Big
//! Buck Bunny BD-ROM folder fixture and asserts the rendered bytes equal the
//! same golden the CLI's end-to-end test pins — so Phases 2–3 cannot silently
//! break the wiring. A second case checks the actual Phase-1 path (the
//! structural [`scan::scan_folder`]) lists the disc's one playlist in the table,
//! exactly what the window shows.

use std::path::PathBuf;

use bdinfo_rs_gui::scan;

/// Absolute path to the committed real-disc folder fixture (it lives under the
/// CLI crate's `tests/fixtures/`, one crate over).
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bdinfo-rs/tests/fixtures/BigBuckBunny")
}

/// The locked measured report for that folder — the CLI golden (Disc Label is
/// the directory name `BigBuckBunny`). Kept verbatim (CRLF) by `.gitattributes`.
const FOLDER_GOLDEN: &str = include_str!("../../bdinfo-rs/tests/fixtures/golden/folder.txt");

#[test]
fn the_measured_seam_matches_the_folder_golden_byte_for_byte() {
    let data = scan::scan_folder_measured(&fixture()).expect("the fixture scans");
    assert_eq!(data.report, FOLDER_GOLDEN, "the GUI's measured wiring drifted from the golden");
    assert_eq!(data.label, "BigBuckBunny", "the folder label is the directory name");
}

#[test]
fn the_structural_scan_lists_the_disc_playlist() {
    // The actual Phase-1 UI path: the fast structural scan populates the table.
    let data = scan::scan_folder(&fixture()).expect("the fixture scans");
    let row = data.table_rows.first().expect("one filtered playlist row");
    assert_eq!(row.number, "1");
    assert_eq!(row.group, "1");
    assert_eq!(row.file, "00000.MPLS");
    assert_eq!(row.length, "00:00:30");
    assert_eq!(data.table_rows.len(), 1, "the filtered table lists exactly the feature playlist");
    assert!(data.report.starts_with("Disc Label:"), "the report renders");
    assert!(!data.show_hidden_note, "the fixture hides no streams");
}
