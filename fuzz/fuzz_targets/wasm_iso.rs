#![no_main]
//! Fuzz target: `bdinfo_rs_wasm::run_iso_report` — the backend-agnostic `.iso`
//! entry point behind the browser package's `scanIso` export. It opens the image
//! through the core UDF 2.50 reader **resiliently**, scans the disc it finds and
//! renders the classic report.
//!
//! Input framing: the sparse-image description `shared/sparse_iso.rs` documents,
//! shared with the `source` target.
//!
//! What this reaches that `source` does not is the resilient open
//! (`UdfSource::open_resilient`, which recovers from per-file failures instead
//! of refusing the volume) and everything downstream of it — the BDMV discovery,
//! scan and render the strict-open walk in `source` stops short of.
//!
//! Amplifies that crate's own `iso_scan_matches_golden` parity test and its
//! `run_iso_report_renders_the_iso_golden_and_empties_a_bad_image` unit test,
//! which drive this entry point over one fixture image and one image that is not
//! a UDF volume at all.

use bdinfo_rs_wasm::run_iso_report;
use libfuzzer_sys::fuzz_target;

#[path = "shared/sparse_iso.rs"]
mod sparse_iso;

fuzz_target!(|data: &[u8]| {
    let image = sparse_iso::build_image(data);
    let _ = run_iso_report(sparse_iso::MemIso::boxed(image));
});
