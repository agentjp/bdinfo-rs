#![no_main]
//! Fuzz target: the BD folder-structure classifiers fed arbitrary names —
//! `bdinfo_rs_core::discovery::{BdmvDir::from_name, BdFileKind::from_filename}`. A BD
//! parser's input is bytes, so the fuzzer's `&[u8]` is decoded lossily to a
//! `str` and pushed through the classifiers; asserts no panic on adversarial
//! directory / file names (the `*_classification_*` proptests cover this on
//! Windows; this is the adversarial amplifier — see fuzz/README.md).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let name = String::from_utf8_lossy(data);
    let _ = bdinfo_rs_core::discovery::BdmvDir::from_name(&name);
    let _ = bdinfo_rs_core::discovery::BdFileKind::from_filename(&name);
});
