#![no_main]
//! Fuzz target: `bdinfo_rs_wasm::run_iso_report` — the backend-agnostic `.iso`
//! parity seam behind the browser package's `scan_iso` export, which wraps it
//! with progress, selection and error reporting. It opens the image through the
//! core UDF 2.50 reader **resiliently**, scans the disc it finds and renders the
//! classic report.
//!
//! Input framing: the sparse-image description `shared/sparse_iso.rs` documents,
//! shared with the `source` target.
//!
//! What this drives that `source` does not is the resilient open
//! (`UdfSource::open_resilient`, which recovers from per-file failures instead
//! of refusing the volume) and, behind it, the BDMV discovery, scan and render
//! the strict-open tree walk in `source` stops short of. The committed seeds
//! reach the open and the no-structure path only: they mirror the core UDF
//! reader's test fixtures, and none of those images carries a `BDMV` tree, so
//! everything past discovery is left to the fuzzer.
//!
//! Amplifies that crate's own `iso_scan_matches_golden` parity test and its
//! `run_iso_report_renders_the_iso_golden_and_empties_a_bad_image` unit test,
//! which drive this entry point over one fixture image and one image that is not
//! a UDF volume at all.

use bdinfo_rs_core::vfs::udf::source::MemIso;
use bdinfo_rs_wasm::run_iso_report;
use libfuzzer_sys::fuzz_target;

#[path = "shared/sparse_iso.rs"]
mod sparse_iso;

fuzz_target!(|data: &[u8]| {
    let image = sparse_iso::build_image(data);
    let _ = run_iso_report(MemIso::boxed(image));
});
