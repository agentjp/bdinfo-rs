#![no_main]
//! Fuzz target: the reverse mapping of the browser package's disc model —
//! `bdinfo_rs_wasm::mirror::Disc` deserialized from caller bytes, rebuilt into a
//! `BdRom` by `Disc::into_scan`, and rendered through the core report renderer.
//!
//! This is the one boundary where a value the report is rendered from arrives
//! from outside the scanner. The `renderReport` export takes a `Disc` the
//! *caller* supplies and renders it with no media read at all, so in a browser
//! that object comes from arbitrary JavaScript — `u64::MAX` sizes, counts no
//! disc could hold, playlist and clip cross-references no scan would produce.
//! The crate's round-trip tests prove a disc a scan produced survives
//! scan → `Disc` → render byte-exact; nothing there constrains a disc a scan did
//! not produce.
//!
//! Input framing: the raw bytes are JSON. `serde_json` stands in for the browser
//! deserializer, which is what that crate's own wire-format tests use it for:
//! both read the same `Deserialize` derive and the same `camelCase` field names,
//! so a `Disc` this target constructs is one a `renderReport` caller can
//! construct. Bytes that are not a `Disc` return without rendering — **that is
//! the harness working, not a finding**. A panic, hang or allocation blow-up
//! anywhere past the parse is a real bug.
//!
//! Rendering repeats `render_report`'s body rather than calling it: that export
//! is `cfg(target_arch = "wasm32")`-gated, and `text::render` is exactly what it
//! reduces to for a caller who names neither optional section.
//!
//! Amplifies the crate's `every_field_survives_a_round_trip_through_the_wire_form`
//! and `every_scan_stage_survives_the_round_trip` tests, which cover the same
//! mapping in the one direction a scan can reach.

use bdinfo_rs_core::report::text;
use bdinfo_rs_wasm::mirror::Disc;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(disc) = serde_json::from_slice::<Disc>(data) else {
        return;
    };
    let (bdrom, errors) = disc.into_scan();
    let _ = text::render(&bdrom, &errors);
});
