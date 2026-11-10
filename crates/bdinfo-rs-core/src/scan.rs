//! Opening a disc from a path — the one place folder and `.iso` input diverge.
//!
//! [`BdRom::open_resilient`] reads through the [`crate::vfs`] seam and knows
//! nothing about paths. A caller holding one must pick the backend, open it,
//! and merge that backend's own recorded failures into the scan's — and for a
//! folder repair the label the scan derived. These functions are that
//! composition, one per backend, so every path-driven surface shares one
//! implementation of it.
//! A browser caller has no paths and builds its [`BdDir`](crate::vfs::BdDir)
//! itself, so it uses neither.
//!
//! How deep to read stays the caller's choice: `mode` is passed through
//! untouched.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use crate::bdrom::disc::{BdRom, ScanMode, ScanObservers, ScanOptions, ScanReport};
use crate::error::BdError;
use crate::vfs::fs::FsDir;
use crate::vfs::udf::source::{PathIso, UdfSource};
use crate::vfs::volume;

/// Opens the Blu-ray folder at `path` through `std::fs`, resiliently.
///
/// `path` is the disc root, its `BDMV` directory, or any directory inside it
/// (the scan walks up). It is made absolute and lexically normalised
/// ([`absolute_lexical`]) before anything reads it, so `.`, `..`, `BDMV` and a
/// trailing separator all name the disc after its real root directory, the
/// name .NET's always-absolute `DirectoryInfo` gives `BDInfo`; every path the
/// scan records is then absolute too. [`ScanReport::errors`] carries the scan's own recorded
/// failures **plus** the enumeration's per-entry ones
/// ([`FsDir::take_errors`]) — a directory that could not be read is damage the
/// scan itself never sees.
///
/// The label is the [`volume`] repair's: a folder scan names the disc after
/// its root directory, which a bare Windows drive root (`J:\`) does not have.
/// The repair's raw-device read runs **before** the scan, on the letter the
/// input path spells, so a disc whose scan goes on to fail reads still carries
/// its real name; what that read found is applied only if the finished scan's
/// own label turned out to be a bare drive root.
///
/// `mode`, `options`, `scan_files` and `observers` are
/// [`BdRom::open_resilient`]'s, passed through unchanged.
///
/// # Errors
/// As [`BdRom::open_resilient`]: only the failures with no readable rest to
/// degrade to.
pub fn open_folder(
    path: &Path,
    mode: ScanMode,
    options: ScanOptions,
    scan_files: Option<&BTreeSet<String>>,
    observers: ScanObservers<'_>,
) -> Result<ScanReport, BdError> {
    // Ahead of the scan, and only for an input path that can walk up to a
    // nameless drive root: the read grinds the very device the scan is about to
    // read, and one raw read on damaged media can sit for minutes in the
    // drive's retries — so it belongs where no read has failed yet. Re-walking
    // the tree afterwards to recover the same letter is not an option: `FsDir`
    // shares its error sink across derived handles, so a second walk would
    // record a damaged listing's failures into the report twice.
    let path = absolute_lexical(path);
    let probed = volume::drive_root_probe(&path).and_then(volume::real_volume_label);
    let root = FsDir::new(path);
    let mut report = BdRom::open_resilient(&root, mode, options, scan_files, observers)?;
    report.errors.extend(root.take_errors());
    report.bdrom.volume_label = volume::apply_drive_root_probe(&report.bdrom.volume_label, probed);
    Ok(report)
}

/// `path` made absolute against the current directory, then with every `.`
/// dropped and every `..` popping the component before it, so its final
/// component is the directory the input names (`.` inside `J:\` gives `J:\`,
/// which the drive-root probe recognises).
///
/// [`std::path::absolute`] already folds `..` on Windows but keeps it on Unix,
/// where `absolute("..")` has no `file_name`; the lexical pass covers both.
/// Never `canonicalize`: it resolves symlinks, which `DirectoryInfo.Name` does
/// not, and spells Windows paths with a `\\?\` prefix. An input `absolute`
/// rejects (the empty path) is returned as given, and the scan then fails on
/// that path's own listing error.
fn absolute_lexical(path: &Path) -> PathBuf {
    std::path::absolute(path).map_or_else(|_| path.to_path_buf(), |absolute| lexical(&absolute))
}

/// `path` with every `.` component dropped and every `..` popping the
/// component before it; a `..` with nothing left to pop is dropped.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
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
/// `mode`, `options`, `scan_files` and `observers` are
/// [`BdRom::open_resilient`]'s, passed through unchanged.
///
/// # Errors
/// [`BdError::StructureNotFound`] or [`BdError::Io`] when `path` holds no
/// readable UDF volume, else as [`BdRom::open_resilient`].
pub fn open_iso(
    path: &Path,
    mode: ScanMode,
    options: ScanOptions,
    scan_files: Option<&BTreeSet<String>>,
    observers: ScanObservers<'_>,
) -> Result<ScanReport, BdError> {
    let source = UdfSource::open_resilient(Box::new(PathIso::new(path)))?;
    let mut report = BdRom::open_resilient(&source.root(), mode, options, scan_files, observers)?;
    report.errors.extend(source.take_errors());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use super::{
        BdError, ScanMode, ScanObservers, ScanOptions, ScanReport, absolute_lexical, lexical,
        open_folder, open_iso,
    };
    use crate::bdrom::disc::ScanProgress;
    use crate::bdrom::measured::MeasuredSnapshot;

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
        let cancel = AtomicBool::new(false);
        let report = {
            let mut count = |_: ScanProgress<'_>| reported = reported.saturating_add(1);
            open_folder(
                path,
                mode,
                ScanOptions::default(),
                None,
                ScanObservers::new(&mut count, &cancel),
            )
        };
        assert_eq!(reported != 0, report.is_ok() && mode != ScanMode::Metadata);
        report
    }

    /// [`folder`]'s `.iso` twin.
    fn iso(path: &Path, mode: ScanMode) -> Result<ScanReport, BdError> {
        let mut reported = 0_usize;
        let cancel = AtomicBool::new(false);
        let report = {
            let mut count = |_: ScanProgress<'_>| reported = reported.saturating_add(1);
            open_iso(
                path,
                mode,
                ScanOptions::default(),
                None,
                ScanObservers::new(&mut count, &cancel),
            )
        };
        assert_eq!(reported != 0, report.is_ok() && mode != ScanMode::Metadata);
        report
    }

    /// Asserts that the last snapshot an observed open delivered carries the
    /// very numbers the finished scan reports: same stream file, same playlist,
    /// same measured bytes. A display that ticks the snapshots and then takes
    /// the summaries therefore never jumps at the handover.
    fn assert_the_live_tallies_land_on_the_summaries(
        report: &ScanReport,
        snapshots: &[MeasuredSnapshot],
    ) {
        let playlist = report.bdrom.playlists.first().expect("the fixture lists one playlist");
        let last = snapshots.last().expect("the measurement pass reported");
        assert_eq!(last.file, "00000.M2TS", "the fixture's one stream file");
        let measured = last.playlists.first().expect("the playlist plays that file");
        assert_eq!(measured.name, playlist.name);
        assert_eq!(measured.measured_bytes, playlist.total_angle_packet_size());
    }

    #[test]
    fn an_observed_folder_open_streams_the_measured_tallies() {
        // A measured observer is what a live display opens through — the same
        // scan the other folder tests run, plus the snapshots.
        let mut snapshots: Vec<MeasuredSnapshot> = Vec::new();
        let cancel = AtomicBool::new(false);
        let mut quiet = |_: ScanProgress<'_>| {};
        let report = {
            let mut watch = |snapshot| snapshots.push(snapshot);
            open_folder(
                &fixture("BigBuckBunny"),
                ScanMode::Full,
                ScanOptions::default(),
                None,
                ScanObservers::new(&mut quiet, &cancel).with_measured(&mut watch),
            )
        }
        .expect("the fixture folder opens");
        assert_eq!(report.bdrom.volume_label, "BigBuckBunny", "the label repair still runs");
        assert_the_live_tallies_land_on_the_summaries(&report, &snapshots);
    }

    #[test]
    fn an_observed_iso_open_streams_the_measured_tallies() {
        let mut snapshots: Vec<MeasuredSnapshot> = Vec::new();
        let cancel = AtomicBool::new(false);
        let mut quiet = |_: ScanProgress<'_>| {};
        let report = {
            let mut watch = |snapshot| snapshots.push(snapshot);
            open_iso(
                &fixture("BigBuckBunny.iso"),
                ScanMode::Full,
                ScanOptions::default(),
                None,
                ScanObservers::new(&mut quiet, &cancel).with_measured(&mut watch),
            )
        }
        .expect("the fixture image opens");
        assert_eq!(report.bdrom.volume_label, "Blu-Ray", "the UDF volume label still reads");
        assert_the_live_tallies_land_on_the_summaries(&report, &snapshots);
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
    fn every_spelling_of_the_disc_folder_names_the_disc_after_it() {
        let disc = fixture("BigBuckBunny");
        let mut trailing = disc.clone().into_os_string();
        trailing.push(std::path::MAIN_SEPARATOR_STR);
        for path in [
            disc.join("BDMV").join(".."),
            disc.join("BDMV").join("STREAM").join("..").join(".."),
            disc.join("BDMV").join("STREAM").join("."),
            disc.join("BDMV"),
            PathBuf::from(trailing),
        ] {
            let report = folder(&path, ScanMode::Metadata).expect("the fixture folder opens");
            assert_eq!(report.bdrom.volume_label, "BigBuckBunny", "{}", path.display());
        }
    }

    #[test]
    fn the_lexical_pass_drops_dots_and_pops_parents() {
        let base = Path::new("a").join("b");
        assert_eq!(
            lexical(&Path::new(".").join(&base).join("..").join("c")),
            Path::new("a").join("c")
        );
        assert_eq!(lexical(&Path::new("..").join("a")), Path::new("a"), "nothing left to pop");
        assert_eq!(lexical(&base.join("..").join("..")), PathBuf::new());
    }

    #[test]
    fn a_path_absolute_rejects_is_kept_as_given() {
        assert_eq!(absolute_lexical(Path::new("")), PathBuf::new());
        let named = absolute_lexical(Path::new("disc"));
        assert!(named.is_absolute());
        assert_eq!(named.file_name(), Some(std::ffi::OsStr::new("disc")));
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
    fn a_healthy_disc_holds_no_stream_file_short_of_its_declared_span() {
        // Authored media demuxes at or just past every declared clip span, so
        // the short-file check stays silent over the whole disc — on both
        // backends of the same one, since only the IO differs.
        let folder_scan =
            folder(&fixture("BigBuckBunny"), ScanMode::Full).expect("the fixture folder opens");
        assert_eq!(folder_scan.bdrom.short_stream_files(), []);
        let iso_scan =
            iso(&fixture("BigBuckBunny.iso"), ScanMode::Full).expect("the fixture image opens");
        assert_eq!(iso_scan.bdrom.short_stream_files(), []);
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
