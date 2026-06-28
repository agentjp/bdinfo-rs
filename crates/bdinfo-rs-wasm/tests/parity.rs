//! WASM ⇄ native byte-parity for the measured-scan export.
//!
//! Frames the committed Big Buck Bunny BD-ROM fixture (the CC-BY disc the CLI's
//! end-to-end test scans) into the export's six-section layout, runs the full
//! **measured** scan through [`bdinfo_rs_wasm::scan_report`] — M2TS demux,
//! per-stream/per-chapter statistics, the classic report — and asserts the
//! bytes equal the pinned golden.
//!
//! The same `check()` runs on both targets: natively (the threaded
//! `scan_chunked` read-ahead path) and in headless Chrome (the
//! single-threaded wasm path). Identical golden ⇒ the wasm demux is
//! byte-for-byte the native demux.

const INDEX: &[u8] = include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/index.bdmv");
const MOVIE: &[u8] =
    include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/MovieObject.bdmv");
const MPLS: &[u8] =
    include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/PLAYLIST/00000.mpls");
const CLPI: &[u8] =
    include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/CLIPINF/00000.clpi");
const M2TS: &[u8] =
    include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/STREAM/00000.m2ts");
const XML: &[u8] = &[];

/// The expected report — the locked CRLF bytes (kept verbatim via the
/// `-text` `.gitattributes` rule, like the CLI's own golden).
const GOLDEN: &[u8] = include_bytes!("golden_report.txt");

fn push(buf: &mut Vec<u8>, sec: &[u8]) {
    buf.extend_from_slice(&(sec.len() as u32).to_be_bytes());
    buf.extend_from_slice(sec);
}

/// The fixture's six files in the export's fixed order, `u32`-BE length-prefixed.
fn blob() -> Vec<u8> {
    let mut b = Vec::new();
    for sec in [INDEX, MOVIE, MPLS, CLPI, M2TS, XML] {
        push(&mut b, sec);
    }
    b
}

fn check() {
    let report = bdinfo_rs_wasm::scan_report(&blob());
    assert_eq!(
        report.as_bytes(),
        GOLDEN,
        "measured-scan report diverged from the pinned golden (len {} vs {})",
        report.len(),
        GOLDEN.len()
    );
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::io::{self, Read};

    use bdinfo_rs_core::bdrom::m2ts::TsStreamFile;
    use bdinfo_rs_core::bdrom::mpls::TsPlaylistFile;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    fn measured_scan_matches_golden() {
        super::check();
    }

    /// A reader that fails on the first read — exercises the wasm sequential
    /// demux's read-error arm (`scan_chunked`'s `Err(e) => return Err(e)`),
    /// which only the `cfg(wasm32)` path runs and no other test reaches.
    struct Faulting;

    impl Read for Faulting {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("simulated read failure"))
        }
    }

    #[wasm_bindgen_test]
    fn scan_surfaces_a_reader_error() {
        let mut stream = TsStreamFile::new("00000.m2ts");
        let mut playlists =
            [TsPlaylistFile::scan("00000.mpls", super::MPLS).expect("parse the fixture playlist")];
        let err = stream
            .scan(&mut Faulting, &mut playlists, true)
            .expect_err("the sequential demux must surface the reader's error");
        assert!(err.to_string().contains("io error"), "unexpected error: {err}");
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    #[test]
    fn measured_scan_matches_golden() {
        super::check();
    }
}
