#![no_main]
//! Fuzz target: the clip-info (CLPI) parser — `TsStreamClipFile::scan` — over the
//! untrusted bytes of a `*.clpi` file. Amplifies the no-panic / no-out-of-bounds
//! contract that the `scan_never_panics_on_arbitrary_input` proptest holds on
//! Windows; here it runs adversarially on nightly/Linux (see fuzz/README.md). The
//! file name is irrelevant to the parse, so a fixed one is used.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = bdinfo_rs_core::bdrom::clpi::TsStreamClipFile::scan("fuzz.clpi", data);
});
