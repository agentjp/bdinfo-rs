#![no_main]
//! Fuzz target: `bdinfo_rs_gui::settings::Settings::parse_reporting` — the
//! hand-rolled `key = value` reader the desktop app loads its configuration
//! file with, plus the writer that saves it back.
//!
//! Input framing: the raw bytes as lossy UTF-8, which is how `settings::load_from`
//! hands a config file to the parser — a file that is not valid UTF-8 is read
//! lossily so the intact lines still count, rather than being rejected whole.
//!
//! The target holds the parser's two contracts, not just no-panic. Whatever the
//! bytes, the parse yields a usable `Settings`; and what it yields is **stable
//! through a render cycle**, so a partially-salvaged file re-saves losslessly
//! instead of degrading a little more on every launch. The second is what the
//! `assert_eq!` below pins, and it is what makes this target more than a
//! smoke test.
//!
//! Amplifies the in-tree `any_text_parses_and_restabilizes` proptest, which
//! states the same round-trip property over arbitrary `String`s.

use bdinfo_rs_gui::settings::Settings;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (settings, unknown) = Settings::parse_reporting(&text);
    // The unknown-key list is what the loader logs; walk it so a hostile key
    // cannot hide behind a value nothing reads.
    for key in &unknown {
        let _ = key.len();
    }
    assert_eq!(Settings::parse(&settings.render()), settings, "settings did not restabilize");
});
