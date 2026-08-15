#![no_main]
//! End-to-end fuzz target: the whole `BdRom` pipeline plus the report renderer.
//!
//! The input bytes become a synthetic in-memory BDMV tree through
//! `bdinfo_rs_core::vfs::mem` — the `u32`-BE section framing and the six-file
//! skeleton it lays them onto, documented there. The tree is opened resiliently,
//! packet-scanned, and rendered to the classic report. This holds the library's
//! whole-pipeline no-panic / no-hang contract on hostile discs, end to end:
//! discovery → index → MPLS / CLPI → M2TS demux → measured summaries →
//! `report::text::render`.
//!
//! That framing is the one `bdinfo-rs-wasm`'s `scan_report` export reads, so a
//! seed means the same disc to both harnesses and neither has to carry its own
//! corpus dialect.
//!
//! This target extends it by one section: a seventh becomes
//! `BDMV/STREAM/SSIF/00000.ssif`. The browser export reads six and ignores
//! anything past them, so seeds stay portable in both directions. It exists
//! because a `BDMV/STREAM/SSIF` directory is what makes a disc 3D, and with no
//! way to express one the interleaved-stream handling and every `is_3d` branch
//! behind it were unreachable from this harness. The directory is absent when
//! the seed supplies no seventh section, so the 2D shape is unchanged.

use bdinfo_rs_core::bdrom::disc::{BdRom, ScanMode, ScanObservers, ScanOptions};
use bdinfo_rs_core::report::text;
use bdinfo_rs_core::vfs::mem::{self, MemDir};
use libfuzzer_sys::fuzz_target;

/// Builds the synthetic disc `data` describes (see the module docs).
fn build_tree(data: &[u8]) -> MemDir {
    let mut sections = mem::split_sections(data, mem::SECTION_COUNT + 1);
    // The seventh section comes off before the skeleton is laid: `build_tree`
    // consumes the vec and ignores anything past `SECTION_COUNT`.
    let ssif = (sections.len() > mem::SECTION_COUNT).then(|| sections.remove(mem::SECTION_COUNT));
    let mut root = mem::build_tree("FUZZDISC", sections);
    if let Some(bytes) = ssif {
        root.add_file(&["BDMV", "STREAM", "SSIF"], "00000.ssif", bytes);
    }
    root
}

fuzz_target!(|data: &[u8]| {
    let root = build_tree(data);
    let opened = BdRom::open_resilient(
        &root,
        ScanMode::Full,
        ScanOptions::default(),
        None,
        ScanObservers::none(),
    );
    if let Ok(report) = opened {
        let _ = text::render(&report.bdrom, &report.errors);
    }
});
