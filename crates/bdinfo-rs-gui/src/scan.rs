//! The scan seam — folder input → a renderable [`ScanData`].
//!
//! Turns a picked Blu-ray folder into the playlist table rows plus the rendered
//! classic report by driving the identical `bdinfo-rs-core` API the CLI and wasm
//! crate use (`FsDir` → [`BdRom::open_resilient_with`] → [`text::render_with`]).
//! The pure formatting is the Tier-A [`crate::model`]; this module is the thin
//! IO glue, verified by the golden-tie parity test (byte-exact report over the
//! committed fixture) rather than line coverage.

use std::path::Path;

use bdinfo_rs_core::bdrom::disc::BdRom;
use bdinfo_rs_core::bdrom::order::PlaylistFilter;
use bdinfo_rs_core::error::BdError;
use bdinfo_rs_core::report::text;
use bdinfo_rs_core::vfs::fs::FsDir;

use crate::model::{self, TableRow};

/// The result of scanning one disc folder: the playlist table rows, the
/// rendered classic report, the disc label, and whether to show the
/// hidden-tracks footer note.
#[derive(Debug, Clone)]
pub struct ScanData {
    /// The playlist table rows (the `#`/Group/Playlist File/Length/Estimated
    /// Bytes columns).
    pub table_rows: Vec<TableRow>,
    /// The rendered classic disc report (CRLF, UTF-8 no BOM — never re-encode).
    pub report: String,
    /// The disc label (the folder name for folder input).
    pub label: String,
    /// Whether any listed playlist hides a stream (the CLI's footer note).
    pub show_hidden_note: bool,
}

/// Opens `root` at the given scan depth, renders the whole-disc report, and
/// builds the playlist table — merging the filesystem-enumeration failures into
/// the report's errors. The shared body behind the structural ([`scan_folder`])
/// and measured ([`scan_folder_measured`]) entry points.
///
/// # Errors
/// [`BdError`] when the structure is too damaged to open at all (no
/// `BDMV`/`CLIPINF`/`PLAYLIST`).
fn open_and_render(root: &FsDir, run_packet_scan: bool) -> Result<ScanData, BdError> {
    let scanned = BdRom::open_resilient_with(root, run_packet_scan, None, &mut |_| {})?;
    let mut errors = scanned.errors;
    errors.extend(root.take_errors());
    let bdrom = scanned.bdrom;

    let rows = model::playlist_rows(&bdrom.playlists);
    let table_rows = model::display_rows(&rows);
    let show_hidden_note = model::any_hidden(&rows);
    let order = bdrom.presentation_order(&PlaylistFilter::default());
    let report = text::render_with(&bdrom, &order, &errors);

    Ok(ScanData { table_rows, report, label: bdrom.volume_label, show_hidden_note })
}

/// Runs the **structural** scan over the disc folder at `path` (fast — no M2TS
/// demux) and builds the playlist table + report. The Phase-1 app entry point.
///
/// # Errors
/// A short message when the folder holds no readable Blu-ray structure.
pub fn scan_folder(path: &Path) -> Result<ScanData, String> {
    open_and_render(&FsDir::new(path), false).map_err(|err| err.to_string())
}

/// Runs the **measured** scan over the disc folder at `path` and renders the
/// report (full M2TS demux, the standard `--whole` filtered set).
///
/// Not yet wired into the Phase-1 UI — the measured scan moves onto a worker
/// thread in Phase 2 — but the golden-tie test drives it now to lock the
/// rendered-report contract early.
///
/// # Errors
/// A short message when the folder holds no readable Blu-ray structure.
pub fn scan_folder_measured(path: &Path) -> Result<ScanData, String> {
    open_and_render(&FsDir::new(path), true).map_err(|err| err.to_string())
}
