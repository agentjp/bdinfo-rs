#![no_main]
//! Fuzz target: the whole-`.iso` UDF reader (`vfs::udf::source`) — `UdfSource::open`
//! over an arbitrary in-memory image, then a full tree walk and a bounded read of
//! every file. This is the integration surface the per-parser `udf` target does
//! not cover: partition resolution, the metadata-partition mapping, the directory
//! arena walk, and the lazy run-backed file reader — with all the hostile-input
//! caps (block size, extent chains, directory budgets, per-file runs) in the line
//! of fire.
//!
//! Input framing: the sparse-image description `shared/sparse_iso.rs` documents,
//! shared with the `wasm_iso` target.
//!
//! Amplifies the in-tree `open_never_panics_on_arbitrary_bytes` proptest, which
//! drives the same entry point (the always-on Windows-local mirror). That
//! proptest feeds its bytes as a raw short image and again mapped at the anchor
//! sector, so it and this target reach the descriptor chain by different routes.

use std::io::Read;

use bdinfo_rs_core::vfs::udf::source::{MemIso, UdfSource};
use bdinfo_rs_core::vfs::{BdDir, SearchOption};
use libfuzzer_sys::fuzz_target;

#[path = "shared/sparse_iso.rs"]
mod sparse_iso;

/// Per-file read budget. The library reads lazily; the harness bounds how much
/// it pulls so a hostile `InformationLength` can't make the HARNESS allocate
/// the claimed (sparse-zero) size.
const READ_BUDGET: u64 = 1 << 20;

fuzz_target!(|data: &[u8]| {
    let image = sparse_iso::build_image(data);
    let Ok(source) = UdfSource::open(MemIso::boxed(image)) else {
        return;
    };
    // Walk the parsed volume: label, root, every directory, every file.
    let _ = source.volume_label();
    let root = source.root();
    let _ = root.name();
    let _ = root.parent();
    let _ = root.get_directories();
    if let Ok(files) = root.get_files_pattern_option("*", SearchOption::AllDirectories) {
        for file in files {
            let _ = file.length();
            let _ = file.extension();
            let _ = file.full_name();
            if let Ok(reader) = file.open_read() {
                let mut sink = Vec::new();
                let _ = reader.take(READ_BUDGET).read_to_end(&mut sink);
            }
        }
    }
});
