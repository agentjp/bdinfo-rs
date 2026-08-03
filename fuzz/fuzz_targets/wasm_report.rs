#![no_main]
//! Fuzz target: `bdinfo_rs_wasm::run_report` — the in-memory entry point behind
//! the browser package's `scanReport` export. It frames the caller's bytes into
//! a synthetic BDMV tree of its own, runs the measured scan over it, and renders
//! the classic report.
//!
//! Input framing: up to six `u32` big-endian length-prefixed sections, the same
//! framing `parse_report` uses, so one seed describes the same synthetic disc to
//! both harnesses.
//!
//! What this reaches that `parse_report` does not is the browser crate's own
//! half of the pipeline: its `Node`/`MemFile` tree implementation (a separate
//! `BdDir`/`BdFile` backend from the core-side one the other target builds),
//! its glob matcher, and its `render_disc` wrapper — all of it shipped to
//! browsers over npm, and none of it linked by any other target.
//!
//! Amplifies that crate's own `measured_scan_matches_golden` parity test, which
//! drives this entry point over one fixture disc on both the native and the
//! wasm32 runtime.

use bdinfo_rs_wasm::run_report;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = run_report(data);
});
