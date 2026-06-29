//! WASM ⇄ native byte-parity for the measured-scan exports.
//!
//! Frames the committed Big Buck Bunny BD-ROM fixture (the CC-BY disc the CLI's
//! end-to-end test scans) into the export's six-section layout, runs the full
//! **measured** scan through [`bdinfo_rs_wasm::scan_report`] — M2TS demux,
//! per-stream/per-chapter statistics, the classic report — and asserts the
//! bytes equal the pinned golden.
//!
//! [`check_iso`] does the same for the `.iso` path: it drives
//! [`bdinfo_rs_wasm::run_iso_report`] over an in-memory [`IsoReader`] backed by
//! the committed `.iso` of the same disc, and asserts the bytes equal the
//! native `.iso` golden — so the read-only UDF reader → report wiring is proven
//! to run, and render identically, on the wasm target too (the browser `WebIso`
//! `FileReaderSync` glue is irreducible and covered by the Node/Chrome parity).
//!
//! The same `check()`/`check_iso()` run on both targets: natively (the threaded
//! `scan_chunked` read-ahead path) and in headless Chrome/Firefox (the
//! single-threaded wasm path). Identical golden ⇒ the wasm demux is
//! byte-for-byte the native demux.

use std::io::Cursor;
use std::sync::Arc;

use bdinfo_rs_core::vfs::ReadSeek;
use bdinfo_rs_core::vfs::udf::source::IsoReader;

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

/// The committed Big Buck Bunny `.iso` (the disc the CLI's end-to-end test scans
/// as an `.iso`) and its pinned native golden — UDF volume label `Blu-Ray`, the
/// one report line that differs from the folder scan's directory-name label.
const ISO: &[u8] = include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny.iso");
const ISO_GOLDEN: &[u8] = include_bytes!("../../bdinfo-rs/tests/fixtures/golden/iso.txt");

/// A trivially `Send + Sync` in-memory [`IsoReader`] over the `.iso` bytes — the
/// browser-free stand-in for `WebIso`, so the UDF reader → report wiring is
/// exercised on every target without `web_sys`.
#[derive(Debug)]
struct MemIso(Arc<[u8]>);

impl IsoReader for MemIso {
    fn open(&self) -> std::io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(Cursor::new(Arc::clone(&self.0))))
    }
}

fn check_iso() {
    let report = bdinfo_rs_wasm::run_iso_report(Box::new(MemIso(Arc::from(ISO))));
    assert_eq!(
        report.as_bytes(),
        ISO_GOLDEN,
        "`.iso` measured-scan report diverged from the native golden (len {} vs {})",
        report.len(),
        ISO_GOLDEN.len()
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

    #[wasm_bindgen_test]
    fn iso_scan_matches_golden() {
        super::check_iso();
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

    #[test]
    fn iso_scan_matches_golden() {
        super::check_iso();
    }
}
