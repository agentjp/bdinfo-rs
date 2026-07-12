//! Golden-tie: lock the GUI's full structural → select → measured wiring against
//! the committed disc, byte-for-byte, for both input kinds.
//!
//! The rendered report is the product, and it is a locked byte contract. This
//! drives the whole Phase-2 seam through the **public** API the iced shell uses —
//! [`scan::scan_structural`] → [`Flow`] (select every row) →
//! [`scan::scan_measured`] — over the committed Big Buck Bunny BD-ROM **folder**
//! and **`.iso`** fixtures, and asserts the rendered bytes equal the same goldens
//! the CLI's end-to-end test pins. Selecting every listed playlist is the GUI's
//! equivalent of the CLI's `--whole`, so the bytes must match exactly; the only
//! difference between the two inputs is the `Disc Label:` line (the folder name
//! vs the `.iso`'s UDF volume label), which the goldens already encode.

use std::path::PathBuf;

use bdinfo_rs_gui::flow::Flow;
use bdinfo_rs_gui::model::ViewSettings;
use bdinfo_rs_gui::scan::{self, Input, Structural};

/// Absolute path to a committed real-disc fixture (they live under the CLI
/// crate's `tests/fixtures/`, one crate over).
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bdinfo-rs/tests/fixtures").join(name)
}

/// The locked measured reports — the CLI goldens, kept verbatim (CRLF) by
/// `.gitattributes`. Folder input labels the disc by directory name; `.iso` input
/// reads the real UDF volume label `Blu-Ray`.
const FOLDER_GOLDEN: &str = include_str!("../../bdinfo-rs/tests/fixtures/golden/folder.txt");
const ISO_GOLDEN: &str = include_str!("../../bdinfo-rs/tests/fixtures/golden/iso.txt");

/// Lists `input`, selects every row, and runs the measured scan — the exact
/// Tier-A flow the shell drives, returning the rendered report + disc label.
fn list_select_all_and_measure(input: &Input) -> (String, String) {
    let structural: Structural = scan::scan_structural(input).expect("the fixture lists");
    // Default view settings — the byte contract holds for a fresh config.
    let mut flow =
        Flow::start_listing(input.clone()).listed(input, Ok(structural), ViewSettings::default());
    flow.select_all();
    let request = flow.scan_request().expect("a request once every row is selected");
    let measured =
        scan::scan_measured(&request.input, &request.selection, &request.scan_files, &mut |_| {})
            .expect("the fixture scans");
    (measured.report, measured.label)
}

#[test]
fn the_folder_seam_matches_the_folder_golden_byte_for_byte() {
    let (report, label) = list_select_all_and_measure(&Input::Folder(fixture("BigBuckBunny")));
    assert_eq!(report, FOLDER_GOLDEN, "the GUI's folder wiring drifted from the golden");
    assert_eq!(label, "BigBuckBunny", "the folder label is the directory name");
}

#[test]
fn the_iso_seam_matches_the_iso_golden_byte_for_byte() {
    let (report, label) = list_select_all_and_measure(&Input::Iso(fixture("BigBuckBunny.iso")));
    assert_eq!(report, ISO_GOLDEN, "the GUI's .iso wiring drifted from the golden");
    assert_eq!(label, "Blu-Ray", "the .iso label is the UDF volume label");
}

#[test]
fn the_structural_scan_lists_the_disc_playlist() {
    // The Listed-state projection the table renders: the fast structural scan
    // populates exactly the one filtered feature playlist.
    let input = Input::Folder(fixture("BigBuckBunny"));
    let structural = scan::scan_structural(&input).expect("the fixture lists");
    assert!(structural.warnings.is_empty(), "the fixture lists cleanly");
    let flow =
        Flow::start_listing(input.clone()).listed(&input, Ok(structural), ViewSettings::default());
    let rows = flow.table();
    let row = rows.first().expect("one filtered playlist row");
    assert_eq!(rows.len(), 1, "the filtered table lists exactly the feature playlist");
    assert_eq!(row.cells.number, "1");
    assert_eq!(row.cells.group, "1");
    assert_eq!(row.cells.file, "00000.MPLS");
    assert_eq!(row.cells.length, "00:00:30");
    assert!(!row.selected, "rows start unselected");
    assert!(!flow.show_hidden_note(), "the fixture hides no streams");
    assert!(flow.can_scan(), "scan-all is available once a disc is listed");
}

#[test]
fn the_pre_scan_report_matches_the_disc_but_reads_zero_bitrate() {
    // BDInfo's pre-scan "View Report": the full report is offered straight after
    // the structural scan, rendered from the disc with zero (unmeasured)
    // bitrates. Nothing is selected, so it covers the whole disc.
    let input = Input::Folder(fixture("BigBuckBunny"));
    let structural = scan::scan_structural(&input).expect("the fixture lists");
    let flow =
        Flow::start_listing(input.clone()).listed(&input, Ok(structural), ViewSettings::default());
    assert!(flow.report_available(), "View Report is offered before the scan");
    let report = flow.report().expect("a structural report before scanning");
    // The disc-level facts and the feature playlist render.
    assert!(report.contains("Disc Label:"), "the disc label renders");
    assert!(report.contains("BigBuckBunny"), "the disc label is the directory name");
    assert!(report.contains("00000.MPLS"), "the feature playlist renders");
    // The bitrate reads zero (no measurement) where the measured golden reads
    // the real 2.94 Mbps — the whole point of the pre-scan report.
    assert!(report.contains("0.00 Mbps"), "the pre-scan bitrate is zero");
    assert!(!report.contains("2.94 Mbps"), "no measured bitrate before the scan");
    assert_ne!(report, FOLDER_GOLDEN, "measuring the disc changes the report");
}
