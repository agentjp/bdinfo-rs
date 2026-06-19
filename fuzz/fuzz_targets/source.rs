#![no_main]
//! Fuzz target: the whole-`.iso` UDF reader (`vfs::udf::source`) — `UdfSource::open`
//! over an arbitrary in-memory image, then a full tree walk and a bounded read of
//! every file. This is the integration surface the per-parser `udf` target does
//! not cover: partition resolution, the metadata-partition mapping, the directory
//! arena walk, and the lazy run-backed file reader — with all the hostile-input
//! caps (block size, extent chains, directory budgets, per-file runs) in the line
//! of fire.
//!
//! The AVDP lives at the fixed byte 512 KiB (sector 256 × the 2048-byte bootstrap
//! sector), so the fuzz input is mapped there — `image = 512 KiB of zeros ++ data` —
//! letting small inputs reach the descriptor chain directly. Seed corpus entries
//! are therefore images with the first 256 sectors stripped (see
//! `vfs::udf::source`'s test fixtures, which the committed seeds mirror).
//!
//! Amplifies the in-tree `open_never_panics_on_arbitrary_bytes` proptest, which
//! maps its input the same way (the always-on Windows-local mirror).

use std::io::{self, Cursor, Read};
use std::sync::Arc;

use bdinfo_rs_core::vfs::udf::source::{IsoReader, UdfSource};
use bdinfo_rs_core::vfs::{BdDir, ReadSeek, SearchOption};
use libfuzzer_sys::fuzz_target;

/// The byte offset of the Anchor Volume Descriptor Pointer (sector 256 × the
/// 2048-byte bootstrap sector).
const ANCHOR_OFFSET: usize = 256 * 2048;

/// Per-file read budget. The library reads lazily; the harness bounds how much
/// it pulls so a hostile `InformationLength` can't make the HARNESS allocate
/// the claimed (sparse-zero) size.
const READ_BUDGET: u64 = 1 << 20;

/// An in-memory [`IsoReader`] — each handle gets an independent cursor.
#[derive(Debug)]
struct MemIso {
    data: Arc<[u8]>,
}

impl IsoReader for MemIso {
    fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(Cursor::new(self.data.to_vec())))
    }
}

fuzz_target!(|data: &[u8]| {
    let mut image = vec![0_u8; ANCHOR_OFFSET];
    image.extend_from_slice(data);
    let Ok(source) = UdfSource::open(Box::new(MemIso { data: Arc::from(image) })) else {
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
