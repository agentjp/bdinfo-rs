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

use bdinfo_rs_core::bdrom::disc::{BdRom, PlaylistSummary, ScanMode, ScanProgress};
use bdinfo_rs_core::error::{BdError, ScanError};
use bdinfo_rs_core::report::text;
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
    /// The disc label (the folder name, or the `.iso`'s UDF volume label).
    pub label: String,
    /// The disc title from `META/bdmt_eng.xml`, when present — the info box's
    /// `Disc Title` line.
    pub disc_title: Option<String>,
    /// The total disc size in bytes (excluding `*.ssif`) — the info box's
    /// `Disc Size` line.
    pub size: u64,
    /// The detected-feature labels (`Ultra HD`, `BD-Java`, …) in the core's
    /// fixed order — the info box's `Detected Features` line.
    pub features: Vec<String>,
    /// Every parsed playlist — the view-model builds the filtered table rows
    /// from these, and the measured scan resolves the selection against them.
    pub playlists: Vec<PlaylistSummary>,
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
    pub errors: Vec<String>,
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
) -> Result<(BdRom, Vec<ScanError>), BdError> {
    match input {
        Input::Folder(path) => {
            let root = FsDir::new(path);
            let report = BdRom::open_resilient_with(&root, mode, scan_files, progress)?;
            let mut errors = report.errors;
            errors.extend(root.take_errors());
            Ok((report.bdrom, errors))
        }
        Input::Iso(path) => {
            let source = UdfSource::open_resilient(Box::new(PathIso::new(path)))?;
            let report = BdRom::open_resilient_with(&source.root(), mode, scan_files, progress)?;
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
    let (bdrom, errors) =
        open(input, ScanMode::Codecs, None, &mut |_| {}).map_err(|err| err.to_string())?;
    let features = bdrom.extra_features().into_iter().map(str::to_owned).collect();
    Ok(Structural {
        label: bdrom.volume_label,
        disc_title: bdrom.disc_title,
        size: bdrom.size,
        features,
        playlists: bdrom.playlists,
        warnings: error_lines(&errors),
    })
}

/// Runs the **measured** scan over `input`, narrowed to the selected clips.
///
/// Streams progress through `progress` and renders the classic report in
/// `selection` order — the GUI equivalent of `bdinfo-rs <disc> --mpls A,B`.
///
/// `selection` is the chosen playlist names in table order, and `scan_files` the
/// matching clip set (`selection::selection_stream_files`), both derived from
/// the structural scan. The packet scan reads only `scan_files`, so an unselected
/// (possibly multi-GB) playlist is never demuxed. Runs on the iced shell's worker
/// thread, where native demux keeps its `thread::scope` parallelism.
///
/// # Errors
/// A short message when the structure is too damaged to open at all — the same
/// hard error [`scan_structural`] surfaces (it cannot occur in practice, since
/// the structural scan just located the same structure, but the result is
/// reported rather than panicked).
pub fn scan_measured(
    input: &Input,
    selection: &[String],
    scan_files: &BTreeSet<String>,
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<Measured, String> {
    let (bdrom, errors) =
        open(input, ScanMode::Full, Some(scan_files), progress).map_err(|err| err.to_string())?;
    let order = selection::selection_order(&bdrom.playlists, selection);
    let report = text::render_with(&bdrom, &order, &errors);
    Ok(Measured {
        report,
        label: bdrom.volume_label,
        errors: error_lines(&errors),
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
}
