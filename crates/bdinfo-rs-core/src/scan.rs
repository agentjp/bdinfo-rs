//! Opening a disc from a path — the one place folder and `.iso` input diverge.
//!
//! [`BdRom::open_resilient_with`] reads through the [`crate::vfs`] seam and
//! knows nothing about paths. A caller holding one must pick the backend, open
//! it, and merge that backend's own recorded failures into the scan's — and for
//! a folder repair the label the scan derived. These two functions are that
//! composition, so every path-driven surface shares one implementation of it.
//! A browser caller has no paths and builds its [`BdDir`](crate::vfs::BdDir)
//! itself, so it uses neither.
//!
//! How deep to read stays the caller's choice: `mode` is passed through
//! untouched.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::bdrom::disc::{BdRom, ScanMode, ScanOptions, ScanProgress, ScanReport};
use crate::error::BdError;
use crate::vfs::fs::FsDir;
use crate::vfs::udf::source::{PathIso, UdfSource};
use crate::vfs::volume;

/// Opens the Blu-ray folder at `path` through `std::fs`, resiliently.
///
/// `path` is the disc root, its `BDMV` directory, or any directory inside it
/// (the scan walks up). [`ScanReport::errors`] carries the scan's own recorded
/// failures **plus** the enumeration's per-entry ones
/// ([`FsDir::take_errors`]) — a directory that could not be read is damage the
/// scan itself never sees.
///
/// The label is the [`volume`] repair's: a folder scan names the disc after
/// its root directory, which a bare Windows drive root (`J:\`) does not have.
/// When the scan recorded stream read failures the repair skips its
/// raw-device read — the drive just failed mid-stream, and another raw read
/// could stall — so the label degrades to the bare drive letter there.
///
/// `mode`, `options`, `scan_files`, `progress` and `cancel` are
/// [`BdRom::open_resilient_with`]'s, passed through unchanged.
///
/// # Errors
/// As [`BdRom::open_resilient_with`]: only the failures with no readable rest
/// to degrade to.
pub fn open_folder(
    path: &Path,
    mode: ScanMode,
    options: ScanOptions,
    scan_files: Option<&BTreeSet<String>>,
    progress: &mut dyn FnMut(ScanProgress<'_>),
    cancel: &AtomicBool,
) -> Result<ScanReport, BdError> {
    let root = FsDir::new(path);
    let mut report =
        BdRom::open_resilient_with(&root, mode, options, scan_files, progress, cancel)?;
    report.errors.extend(root.take_errors());
    report.bdrom.volume_label =
        volume::resolve_folder_label_after_scan(&report.bdrom.volume_label, &report.errors);
    Ok(report)
}

/// Opens the Blu-ray `.iso` image at `path` through the UDF reader,
/// resiliently.
///
/// Yields the same [`BdRom`] the folder scan would over the same disc — only
/// the IO backend differs — except for the label: an `.iso` carries the genuine
/// UDF volume label, so it needs no repair. [`ScanReport::errors`] carries the
/// scan's own recorded failures **plus** the reader's bad-sector recordings
/// ([`UdfSource::take_errors`]).
///
/// `mode`, `options`, `scan_files`, `progress` and `cancel` are
/// [`BdRom::open_resilient_with`]'s, passed through unchanged.
///
/// # Errors
/// [`BdError::StructureNotFound`] or [`BdError::Io`] when `path` holds no
/// readable UDF volume, else as [`BdRom::open_resilient_with`].
pub fn open_iso(
    path: &Path,
    mode: ScanMode,
    options: ScanOptions,
    scan_files: Option<&BTreeSet<String>>,
    progress: &mut dyn FnMut(ScanProgress<'_>),
    cancel: &AtomicBool,
) -> Result<ScanReport, BdError> {
    let source = UdfSource::open_resilient(Box::new(PathIso::new(path)))?;
    let mut report =
        BdRom::open_resilient_with(&source.root(), mode, options, scan_files, progress, cancel)?;
    report.errors.extend(source.take_errors());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use super::{BdError, ScanMode, ScanOptions, ScanReport, open_folder, open_iso};

    /// The committed real-disc fixtures, one crate over: `BigBuckBunny` (a BDMV
    /// folder) and `BigBuckBunny.iso` (the same disc as a UDF image, whose UDF
    /// volume label is `Blu-Ray`).
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bdinfo-rs/tests/fixtures").join(name)
    }

    /// A throwaway directory under the system temp dir, removed on drop.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("bdinfo-rs-scan-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&root).expect("create the scratch dir");
            Self { root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // Best-effort: a stray temp directory is harmless if removal races.
            let _ = std::fs::remove_dir_all(&self.root).is_ok();
        }
    }

    /// Opens `path` as a folder at `mode`, with no selection and no
    /// cancellation, asserting that the progress sink fires exactly when a
    /// packet scan ran — the pass-through arguments have to reach the demux,
    /// and [`ScanMode::Metadata`] reads no packets to report.
    fn folder(path: &Path, mode: ScanMode) -> Result<ScanReport, BdError> {
        let mut reported = 0_usize;
        let report = open_folder(
            path,
            mode,
            ScanOptions::default(),
            None,
            &mut |_| reported = reported.saturating_add(1),
            &AtomicBool::new(false),
        );
        assert_eq!(reported != 0, report.is_ok() && mode != ScanMode::Metadata);
        report
    }

    /// [`folder`]'s `.iso` twin.
    fn iso(path: &Path, mode: ScanMode) -> Result<ScanReport, BdError> {
        let mut reported = 0_usize;
        let report = open_iso(
            path,
            mode,
            ScanOptions::default(),
            None,
            &mut |_| reported = reported.saturating_add(1),
            &AtomicBool::new(false),
        );
        assert_eq!(reported != 0, report.is_ok() && mode != ScanMode::Metadata);
        report
    }

    #[test]
    fn a_folder_scan_names_the_disc_after_its_root_directory() {
        let report =
            folder(&fixture("BigBuckBunny"), ScanMode::Full).expect("the fixture folder opens");
        // The label repair is identity for a named directory, so this pins both
        // that it runs and that an ordinary folder keeps its own name.
        assert_eq!(report.bdrom.volume_label, "BigBuckBunny");
        assert!(!report.bdrom.playlists.is_empty(), "the structure lists");
        assert!(report.errors.is_empty(), "healthy media records no failure");
    }

    #[test]
    fn an_iso_scan_reads_the_real_udf_volume_label() {
        // The same disc as the folder above, whose label is a directory name;
        // this one's is the UDF volume's. (The two reports are compared
        // byte-for-byte against their goldens by the CLI's end-to-end test.)
        let report =
            iso(&fixture("BigBuckBunny.iso"), ScanMode::Full).expect("the fixture image opens");
        assert_eq!(report.bdrom.volume_label, "Blu-Ray");
        assert!(!report.bdrom.playlists.is_empty(), "the structure lists");
        assert!(report.errors.is_empty(), "healthy media records no failure");
    }

    #[test]
    fn a_folder_without_a_bd_structure_fails() {
        let scratch = Scratch::new();
        let err = folder(&scratch.root, ScanMode::Metadata).expect_err("no BDMV to locate");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn a_file_that_is_no_udf_volume_fails() {
        let scratch = Scratch::new();
        let path = scratch.root.join("garbage.iso");
        std::fs::write(&path, b"not a udf volume at all").expect("the scratch image writes");
        let err = iso(&path, ScanMode::Metadata).expect_err("no UDF volume to open");
        assert!(!err.to_string().is_empty(), "the failure carries a message");
    }

    #[test]
    fn a_udf_volume_without_a_bd_structure_fails() {
        // A VALID UDF image whose BDMV tree is unfindable: rename every `BDMV`
        // byte run in a copy of the fixture image. The volume still opens (the
        // anchor and descriptors are untouched), so this reaches the structure
        // walk — the `.iso` failure road garbage bytes never get to.
        let scratch = Scratch::new();
        let path = scratch.root.join("renamed.iso");
        let mut bytes = std::fs::read(fixture("BigBuckBunny.iso")).expect("the fixture reads");
        let needle = b"BDMV";
        let mut at = 0;
        while let Some(hit) =
            bytes.get(at..).and_then(|tail| tail.windows(needle.len()).position(|w| w == needle))
        {
            let start = at.checked_add(hit).expect("the offset fits");
            let end = start.checked_add(needle.len()).expect("the offset fits");
            bytes.get_mut(start..end).expect("the window is in range").copy_from_slice(b"XDMV");
            at = start.checked_add(1).expect("the offset fits");
        }
        std::fs::write(&path, bytes).expect("the scratch image writes");
        let err =
            iso(&path, ScanMode::Metadata).expect_err("a volume with no BDMV cannot be scanned");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }
}
