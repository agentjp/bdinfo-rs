#![no_main]
//! Fuzz target: the byte readers — an always-present untrusted-input entry
//! point. Amplifies the no-panic / no-out-of-bounds contract of EVERY
//! `bdinfo_rs_core::bytes` reader (`read_u8`, `read_u16_be`, `read_u24_be`,
//! `read_u32_be`, `read_u64_be`, `read_uint_be`, `read_ascii`) across every
//! offset, including past-the-end, plus past-width `read_uint_be` requests and
//! a spread of `read_ascii` lengths (the `*_never_panics` proptests hold this
//! on Windows; this fuzzes adversarially on nightly/Linux — see fuzz/README.md).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let len = data.len();
    for off in 0..=len {
        let _ = bdinfo_rs_core::bytes::read_u8(data, off);
        let _ = bdinfo_rs_core::bytes::read_u16_be(data, off);
        let _ = bdinfo_rs_core::bytes::read_u24_be(data, off);
        let _ = bdinfo_rs_core::bytes::read_u32_be(data, off);
        let _ = bdinfo_rs_core::bytes::read_u64_be(data, off);
        // Every width incl. past-u64 (9) — the reader must reject, not wrap.
        for n in 0..=9 {
            let _ = bdinfo_rs_core::bytes::read_uint_be(data, off, n);
        }
        // A spread of ASCII lengths incl. zero and past-the-end.
        for count in [0, 1, 8, len, len.wrapping_add(1)] {
            let _ = bdinfo_rs_core::bytes::read_ascii(data, off, count);
        }
    }
});
