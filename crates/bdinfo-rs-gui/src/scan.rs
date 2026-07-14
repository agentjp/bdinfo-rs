//! The scan seam — a picked folder or `.iso` → the structural playlist list and,
//! after a selection, the measured report.
//!
//! Drives the identical `bdinfo-rs-core` API the CLI (`main.rs`) and the wasm
//! crate use, dispatching folder vs `.iso` input exactly like the CLI's
//! `scan_disc` (`FsDir` / `UdfSource` → [`BdRom::open_resilient_with`] →
//! [`text::render_with`]). The pure formatting / selection / progress math is the
//! Tier-A [`crate::model`] / `selection` / [`crate::progress`]; this
//! module is the thin IO glue, verified by the golden-tie parity test (byte-exact
//! report over the committed fixture) rather than line coverage.
//!
//! The long, blocking measured scan ([`scan_measured`]) is what the iced shell
//! runs on a worker thread, forwarding its [`ScanProgress`] callback over a
//! channel; the structural scan ([`scan_structural`]) is fast (no demux).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use bdinfo_rs_core::bdrom::disc::{BdRom, PlaylistSummary, ScanMode, ScanProgress};
use bdinfo_rs_core::error::{BdError, ScanError};
use bdinfo_rs_core::report::text::{self, RenderOptions};
use bdinfo_rs_core::vfs::fs::FsDir;
use bdinfo_rs_core::vfs::udf::source::{PathIso, UdfSource};

use crate::selection;

/// The disc the user picked: a Blu-ray **folder** or a single `.iso` image.
///
/// A folder is read through `std::fs`, an `.iso` through the in-house UDF 2.50
/// reader — the only branch that differs between the two scan backends. Both flow
/// into the identical structural → measured pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A disc folder: the disc root, its `BDMV` directory, or any directory
    /// inside it (the scan walks up to the disc root). The label is the
    /// directory name.
    Folder(PathBuf),
    /// A single `.iso` image. The label is the real UDF volume label.
    Iso(PathBuf),
}

impl Input {
    /// Classifies a path arriving from outside the pickers — a drag-and-drop
    /// item or a command-line argument — into the input the open flow takes:
    /// a directory is a [`Input::Folder`], a file with a `.iso` extension
    /// (case-insensitive) a [`Input::Iso`]. The one classifier both roads
    /// share, so a dropped disc and `bdinfo-rs-gui <path>` can never diverge.
    ///
    /// The extension check compares [`std::ffi::OsStr`] bytes directly, so a
    /// non-UTF-8 path classifies (and fails) without a lossy conversion.
    ///
    /// # Errors
    /// A short user-facing message when the path is neither a directory nor a
    /// `.iso` file (or does not exist at all) — shown by the same friendly
    /// failure state a bad pick reaches.
    pub fn classify(path: PathBuf) -> Result<Self, String> {
        if path.is_dir() {
            return Ok(Self::Folder(path));
        }
        if path.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("iso")) {
            return Ok(Self::Iso(path));
        }
        if path.exists() {
            Err(format!("Not a Blu-ray disc folder or .iso image: {}", path.display()))
        } else {
            Err(format!("The path does not exist: {}", path.display()))
        }
    }

    /// The picked path, whichever input it is.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Folder(path) | Self::Iso(path) => path,
        }
    }

    /// The path as a display string, for the "Scanning …" line.
    #[must_use]
    pub fn display(&self) -> String {
        self.path().display().to_string()
    }
}

/// The structural-scan result: enough to populate the playlist selection table
/// without demuxing a single stream file.
#[derive(Debug, Clone)]
pub struct Structural {
    /// The scanned disc from the codec pass — no measured bitrate. It carries
    /// the disc-level facts (label, title, size, flags) and the parsed
    /// playlists the view-model builds its table from, and is enough to render
    /// the classic report with zero (unmeasured) bitrates: the pre-scan "View
    /// Report", exactly as `BDInfo` shows it before the bitrate scan.
    pub bdrom: BdRom,
    /// The detected-feature labels (`Ultra HD`, `BD-Java`, …) in the core's
    /// fixed order — the info box's `Detected Features` line.
    pub features: Vec<String>,
    /// Any failures recorded while reading the structure (a corrupt playlist,
    /// an unreadable directory) — shown as a non-blocking warning banner.
    pub warnings: Vec<String>,
}

/// The measured-scan result: the rendered classic report plus what the UI needs
/// to save it and warn about partial reads.
#[derive(Debug, Clone)]
pub struct Measured {
    /// The rendered classic disc report (CRLF, UTF-8 no BOM — never re-encode).
    pub report: String,
    /// The disc label — the `BDINFO.{label}.txt` save name uses it.
    pub label: String,
    /// Any failures recorded during the measured scan — a non-empty list is the
    /// CLI's resilient "completed with errors" posture (the report still saves).
    /// Typed (and shared — [`ScanError`] is not `Clone`, and the shell's
    /// messages are) so the flow can re-render the report's `WARNING` block
    /// when a render option changes.
    pub errors: Arc<Vec<ScanError>>,
    /// The measured playlists — the scanned disc's playlists with their packet
    /// sizes and bitrates filled in, so the master-detail panes refresh in place.
    pub playlists: Vec<PlaylistSummary>,
}

/// Opens `input` at the given scan depth, merging the backend's recorded
/// failures (filesystem enumeration, or `.iso` bad-sector recordings) into the
/// scan's own. The shared body behind both entry points — the one place folder
/// vs `.iso` input diverges, mirroring the CLI's `scan_disc`.
fn open(
    input: &Input,
    mode: ScanMode,
    scan_files: Option<&BTreeSet<String>>,
    progress: &mut dyn FnMut(ScanProgress<'_>),
    cancel: &AtomicBool,
) -> Result<(BdRom, Vec<ScanError>), BdError> {
    match input {
        Input::Folder(path) => {
            let root = FsDir::new(path);
            let report = BdRom::open_resilient_with(&root, mode, scan_files, progress, cancel)?;
            let mut errors = report.errors;
            errors.extend(root.take_errors());
            Ok((report.bdrom, errors))
        }
        Input::Iso(path) => {
            let source = UdfSource::open_resilient(Box::new(PathIso::new(path)))?;
            let report =
                BdRom::open_resilient_with(&source.root(), mode, scan_files, progress, cancel)?;
            let mut errors = report.errors;
            errors.extend(source.take_errors());
            Ok((report.bdrom, errors))
        }
    }
}

/// Recorded scan failures as display strings, for the UI's warning surfaces.
fn error_lines(errors: &[ScanError]) -> Vec<String> {
    errors.iter().map(ToString::to_string).collect()
}

/// Runs the **structural** scan over `input` (fast — no M2TS demux) and returns
/// the playlist list + label for the selection table.
///
/// # Errors
/// A short message when the input holds no readable Blu-ray structure (no
/// `BDMV`/`CLIPINF`/`PLAYLIST`, or a `.iso` that is not a readable UDF volume).
pub fn scan_structural(input: &Input) -> Result<Structural, String> {
    // `Codecs` mode runs the bounded quick pass — reads only the head of each
    // stream file — so the panes show full codec detail (profile / HDR / Dolby
    // Vision) right after opening, like BDInfo, without the whole-file bitrate scan.
    // Bounded and fast, so it carries no cancel affordance: the flag never trips.
    let (bdrom, errors) = open(input, ScanMode::Codecs, None, &mut |_| {}, &AtomicBool::new(false))
        .map_err(|err| err.to_string())?;
    let features = bdrom.extra_features().into_iter().map(str::to_owned).collect();
    Ok(Structural { bdrom, features, warnings: error_lines(&errors) })
}

/// Runs the **measured** scan over `input`, narrowed to the selected clips.
///
/// Streams progress through `progress` and renders the classic report in
/// `selection` order — the GUI equivalent of `bdinfo-rs <disc> --mpls A,B`.
///
/// `selection` is the chosen playlist names in table order, and `scan_files` the
/// matching clip set (`selection::selection_stream_files`), both derived from
/// the structural scan. The packet scan reads only `scan_files`, so an unselected
/// (possibly multi-GB) playlist is never demuxed. `options` is the report's
/// section switches at scan start. Runs on the iced shell's worker
/// thread, where native demux keeps its `thread::scope` parallelism.
///
/// `cancel` is the shell's Cancel affordance: set it (from the UI thread) and
/// the demux aborts at its next read chunk, so the worker returns within
/// moments instead of scanning a possibly-90-GB disc to completion.
///
/// # Errors
/// A short message when the structure is too damaged to open at all — the same
/// hard error [`scan_structural`] surfaces (it cannot occur in practice, since
/// the structural scan just located the same structure, but the result is
/// reported rather than panicked) — or when `cancel` was set mid-scan (the
/// message is dropped by the shell's generation guard; no report escapes).
pub fn scan_measured(
    input: &Input,
    selection: &[String],
    scan_files: &BTreeSet<String>,
    options: RenderOptions,
    progress: &mut dyn FnMut(ScanProgress<'_>),
    cancel: &AtomicBool,
) -> Result<Measured, String> {
    let (bdrom, errors) = open(input, ScanMode::Full, Some(scan_files), progress, cancel)
        .map_err(|err| err.to_string())?;
    let order = selection::selection_order(&bdrom.playlists, selection);
    let report = text::render_with(&bdrom, &order, &errors, options);
    Ok(Measured {
        report,
        label: bdrom.volume_label,
        errors: Arc::new(errors),
        playlists: bdrom.playlists,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Input;

    #[test]
    fn path_and_display_read_through() {
        let folder = Input::Folder(PathBuf::from("a/b"));
        assert_eq!(folder.path(), PathBuf::from("a/b"));
        assert_eq!(folder.display(), PathBuf::from("a/b").display().to_string());
        let iso = Input::Iso(PathBuf::from("x.iso"));
        assert_eq!(iso.path(), PathBuf::from("x.iso"));
    }

    /// Absolute path to a committed real-disc fixture (they live under the CLI
    /// crate's `tests/fixtures/`, one crate over).
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../bdinfo-rs/tests/fixtures").join(name)
    }

    #[test]
    fn classify_takes_a_directory_as_a_folder() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(Input::classify(dir.clone()), Ok(Input::Folder(dir)));
        // A trailing separator classifies the same directory.
        let mut trailing = env!("CARGO_MANIFEST_DIR").to_owned();
        trailing.push(std::path::MAIN_SEPARATOR);
        let trailing = PathBuf::from(trailing);
        assert_eq!(Input::classify(trailing.clone()), Ok(Input::Folder(trailing)));
    }

    #[test]
    fn classify_takes_an_iso_file_by_extension() {
        let iso = fixture("BigBuckBunny.iso");
        assert_eq!(Input::classify(iso.clone()), Ok(Input::Iso(iso)));
    }

    #[test]
    fn classify_ignores_the_extension_case() {
        // `.ISO` must classify like `.iso`; the fixture is lower-case, so make a
        // scratch file (the classifier requires the file to exist).
        let path = std::env::temp_dir().join("bdinfo-rs-gui-classify-case.ISO");
        std::fs::write(&path, b"x").expect("the scratch file writes");
        let result = Input::classify(path.clone());
        let _ = std::fs::remove_file(&path);
        assert_eq!(result, Ok(Input::Iso(path)));
    }

    #[test]
    fn classify_rejects_a_file_that_is_not_an_iso() {
        let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let message = Input::classify(file).expect_err("a stray file is rejected");
        assert!(message.contains("Not a Blu-ray disc folder or .iso image"));
    }

    #[test]
    fn classify_rejects_a_nonexistent_path() {
        let message =
            Input::classify(fixture("no-such-thing.iso")).expect_err("a missing path is rejected");
        assert!(message.contains("does not exist"));
    }

    #[cfg(windows)]
    #[test]
    fn classify_survives_a_non_utf8_path() {
        use std::os::windows::ffi::OsStringExt;

        // An unpaired surrogate — representable in a Windows path, not in UTF-8.
        let bogus = std::ffi::OsString::from_wide(&[0xD800, 0x002E, 0x0069, 0x0073, 0x006F]);
        assert!(Input::classify(PathBuf::from(bogus)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn classify_survives_a_non_utf8_path() {
        use std::os::unix::ffi::OsStringExt;

        // Raw bytes that are not UTF-8, ending in `.iso`.
        let bogus = std::ffi::OsString::from_vec(vec![0xFF, 0xFE, b'.', b'i', b's', b'o']);
        assert!(Input::classify(PathBuf::from(bogus)).is_err());
    }
}
