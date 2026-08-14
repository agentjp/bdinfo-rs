//! The scan seam — a picked folder or `.iso` → the structural playlist list and,
//! after a selection, the measured report.
//!
//! Drives the identical `bdinfo-rs-core` API the CLI and the wasm crate use
//! ([`core_scan::open_folder`] / [`core_scan::open_iso`] →
//! [`text::render_with`]). The pure formatting / selection / progress math is the
//! Tier-A [`crate::model`] / `selection` / [`crate::progress`]; this
//! module is the thin IO glue, verified by the golden-tie parity test (byte-exact
//! report over the committed fixture) rather than line coverage.
//!
//! Both scans block for as long as the disc makes them (a damaged sector can
//! hold one read for minutes), so the iced shell runs each on a worker thread,
//! forwarding its [`ScanProgress`] callback over a channel; the structural
//! scan ([`scan_structural`]) merely reads far less (no full demux).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use bdinfo_rs_core::bdrom::disc::{BdRom, PlaylistSummary, ScanMode, ScanOptions, ScanProgress};
use bdinfo_rs_core::bdrom::order::selection_order;
use bdinfo_rs_core::error::{BdError, ScanError};
use bdinfo_rs_core::report::text::{self, RenderOptions};
use bdinfo_rs_core::scan as core_scan;

/// The disc the user picked: a Blu-ray **folder** or a single `.iso` image.
///
/// A folder is read through `std::fs`, an `.iso` through the in-house UDF 2.50
/// reader — the only branch that differs between the two scan backends. Both flow
/// into the identical structural → measured pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A disc folder: the disc root, its `BDMV` directory, or any directory
    /// inside it (the scan walks up to the disc root). The label is the
    /// directory name — or, when the disc root is a nameless Windows drive
    /// root, the real UDF label the [`bdinfo_rs_core::vfs::volume`] repair
    /// recovers; after a scan that recorded stream read errors the repair
    /// skips its raw-device read and the label degrades to the bare drive
    /// letter.
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

/// Opens `input` at the given scan depth, under `options`, through the core's
/// path-based scan, which merges the backend's own recorded failures
/// (filesystem enumeration, or `.iso` bad-sector recordings) into the scan's
/// and repairs a folder's label. This dispatch is the whole of what the two
/// input kinds do differently here.
fn open(
    input: &Input,
    mode: ScanMode,
    options: ScanOptions,
    scan_files: Option<&BTreeSet<String>>,
    progress: &mut dyn FnMut(ScanProgress<'_>),
    cancel: &AtomicBool,
) -> Result<(BdRom, Vec<ScanError>), BdError> {
    let report = match input {
        Input::Folder(path) => {
            core_scan::open_folder(path, mode, options, scan_files, progress, cancel)
        }
        Input::Iso(path) => core_scan::open_iso(path, mode, options, scan_files, progress, cancel),
    }?;
    Ok((report.bdrom, report.errors))
}

/// What one open of the disc yields: the scanned disc paired with the per-file
/// failures recorded getting there, or a user-facing message for a disc that
/// holds no readable structure at all.
type Opened = Result<(BdRom, Vec<ScanError>), String>;

/// Recorded scan failures as display strings, for the UI's warning surfaces.
fn error_lines(errors: &[ScanError]) -> Vec<String> {
    errors.iter().map(ToString::to_string).collect()
}

/// The no-op progress sink.
///
/// For any [`scan_structural`] / [`scan_measured`] caller that wants no live
/// progress — the golden ties pass it to both.
pub const fn no_progress(_: ScanProgress<'_>) {}

/// Runs the **structural** scan over `input` (no full M2TS demux) and returns
/// the playlist list + label for the selection table.
///
/// `progress` observes the quick codec pass ([`ScanMode::Codecs`]), which
/// climbs to 100% of the selected byte total in per-file jumps; the metadata
/// open that precedes it reports nothing, so the first event can arrive only
/// after that open completes. `cancel` aborts the codec pass at its next read
/// chunk — the flag is polled **before** each read, so a cancel honoured
/// before the first read produces no progress event at all; a caller must
/// never wait on one to conclude a cancelled listing is over.
///
/// # Errors
/// A short message when the input holds no readable Blu-ray structure (no
/// `BDMV`/`CLIPINF`/`PLAYLIST`, or a `.iso` that is not a readable UDF
/// volume), or when `cancel` was set mid-listing.
pub fn scan_structural(
    input: &Input,
    progress: &mut dyn FnMut(ScanProgress<'_>),
    cancel: &AtomicBool,
) -> Result<Structural, String> {
    let (bdrom, errors) = listing_scan(|mode| {
        // Always the default (retain), never the user's setting: the
        // partial-retention toggle scopes to the measured scan, which is the
        // one that renders a report. A listing open that hits a mid-file read
        // error therefore keeps that file's partial codec detail whatever the
        // setting says, so the panes stay as full as the disc allows.
        open(input, mode, ScanOptions::default(), None, progress, cancel)
            .map_err(|err: BdError| err.to_string())
    })?;
    let features = bdrom.extra_features().into_iter().map(str::to_owned).collect();
    Ok(Structural { bdrom, features, warnings: error_lines(&errors) })
}

/// Opens the disc for the selection table, choosing how deep to read: the
/// packet-free [`ScanMode::Metadata`] open alone for an AACS-encrypted disc,
/// else a second [`ScanMode::Codecs`] open over the same disc.
///
/// The quick codec pass stops at the packet that completes the last stream's
/// codec detail. On an encrypted disc the payloads are ciphertext, so no
/// scanner ever initializes, the pass has no stopping point, and it reads every
/// stream file to EOF — tens of gigabytes off an optical drive, for detail no
/// key can recover. `open` is therefore never called a second time for such a
/// disc.
///
/// For every other disc the `Codecs` open gives the panes their full codec
/// detail (profile / HDR / Dolby Vision) right away, like `BDInfo`, without
/// the whole-file bitrate scan; on readable streams it ends at codec init,
/// but damaged media can hold a single read for minutes, which is why the
/// pass is observable and cancellable through [`scan_structural`]'s
/// parameters. Reaching it costs a second parse of the clip metadata the
/// first open just read, which the filesystem cache serves.
///
/// # Errors
/// Whatever `open` reports, from either call.
fn listing_scan(mut open: impl FnMut(ScanMode) -> Opened) -> Opened {
    let cheap = open(ScanMode::Metadata)?;
    if cheap.0.is_aacs_encrypted { Ok(cheap) } else { open(ScanMode::Codecs) }
}

/// Runs the **measured** scan over `input`, narrowed to the selected clips.
///
/// Streams progress through `progress` and renders the classic report in
/// `selection` order — the GUI equivalent of `bdinfo-rs <disc> --mpls A,B`.
///
/// `selection` is the chosen playlist names in table order, and `scan_files` the
/// matching clip set ([`bdinfo_rs_core::bdrom::order::selection_stream_files`]),
/// both derived from
/// the structural scan. The packet scan reads only `scan_files`, so an unselected
/// (possibly multi-GB) playlist is never demuxed. `options` is the report's
/// section switches at scan start and `scan_options` the scan's own — the
/// partial-retention switch reaches the report only from here. Runs on the
/// iced shell's worker thread, where native demux keeps its `thread::scope`
/// parallelism.
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
    scan_options: ScanOptions,
    progress: &mut dyn FnMut(ScanProgress<'_>),
    cancel: &AtomicBool,
) -> Result<Measured, String> {
    let (bdrom, errors) =
        open(input, ScanMode::Full, scan_options, Some(scan_files), progress, cancel)
            .map_err(|err| err.to_string())?;
    let order = selection_order(&bdrom.playlists, selection);
    let report = text::render_with(&bdrom, &order, &errors, options);
    Ok(Measured {
        report,
        label: bdrom.volume_label,
        errors: Arc::new(errors),
        playlists: bdrom.playlists,
    })
}

/// Runs `scan`, mapping a panic — a violation of the core's fuzz-held
/// no-panic contract on some hostile disc — onto the same `Err` road an
/// ordinary scan failure takes.
///
/// The shell wraps [`scan_structural`] in this so a panic lands in the
/// friendly failure state instead of losing the completion message and
/// sticking the non-dismissable "scanning" modal. `AssertUnwindSafe` is
/// sound here: the closure and its captures are consumed either way, and the
/// caller only ever reads the returned `Result`.
///
/// # Errors
/// Whatever `scan` itself returned, passed through untouched — plus the one
/// mapping this exists for: a panic becomes `Err("scan terminated
/// unexpectedly")`.
pub fn catch_scan_panic<T>(scan: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(scan))
        .unwrap_or_else(|_| Err("scan terminated unexpectedly".to_owned()))
}

/// A defuse-on-success alarm for the measured-scan worker thread.
///
/// Unless [`defuse`](Self::defuse)d, dropping it runs `on_drop` — and a
/// panic's unwind drops locals, so arming one at the top of the worker
/// closure turns "the worker died and the message stream just ended, the UI
/// stuck at Scanning" into an ordinary terminal failure message. Pure safe
/// Rust; the shell's message carries the scan generation, so an alarm firing
/// after the flow moved on is dropped like any other stale event.
pub struct PanicAlarm<F: FnOnce()> {
    /// The armed payload; `None` once defused.
    on_drop: Option<F>,
}

impl<F: FnOnce()> PanicAlarm<F> {
    /// Arms the alarm.
    #[must_use]
    pub const fn new(on_drop: F) -> Self {
        Self { on_drop: Some(on_drop) }
    }

    /// Disarms it — the success path delivered its own terminal message.
    pub fn defuse(mut self) {
        self.on_drop = None;
    }
}

impl<F: FnOnce()> Drop for PanicAlarm<F> {
    fn drop(&mut self) {
        if let Some(fire) = self.on_drop.take() {
            fire();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    use bdinfo_rs_core::vfs::fs::FsDir;

    use super::{BdRom, Input, ScanMode, ScanOptions, Structural};

    /// [`super::scan_structural`] with the silent sink and a never-set cancel
    /// flag — every test that only cares about the scan's result goes through
    /// this.
    fn scan_structural(input: &Input) -> Result<Structural, String> {
        super::scan_structural(input, &mut super::no_progress, &AtomicBool::new(false))
    }

    /// A scratch directory under the crate's target dir (unique per test).
    fn scratch(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/scan-tests").join(name)
    }

    /// Recursively copies `src` into `dst` (the fixture tree is a handful of
    /// small files).
    fn copy_tree(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).expect("the scratch dir creates");
        for entry in std::fs::read_dir(src).expect("the source dir reads") {
            let entry = entry.expect("the entry reads");
            let target = dst.join(entry.file_name());
            if entry.path().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).expect("the file copies");
            }
        }
    }

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

    #[test]
    fn the_structural_progress_sink_is_a_no_op() {
        // The Codecs pass never reports progress; the named sink pins that
        // (it must do nothing, whatever event it is handed).
        super::no_progress(bdinfo_rs_core::bdrom::disc::ScanProgress {
            file: "00000.M2TS",
            done: 1,
            total: 2,
        });
    }

    #[test]
    fn a_garbage_iso_fails_the_structural_scan() {
        // Bytes that are not a UDF volume: the `.iso` backend's open fails and
        // the seam surfaces the same friendly message road as a bad folder.
        let dir = scratch("garbage-iso");
        std::fs::create_dir_all(&dir).expect("the scratch dir creates");
        let iso = dir.join("garbage.iso");
        std::fs::write(&iso, b"not a udf volume at all").expect("the scratch iso writes");
        let message = scan_structural(&Input::Iso(iso)).expect_err("no UDF volume to open");
        assert!(!message.is_empty(), "the failure carries a user-facing message");
    }

    #[test]
    fn a_udf_volume_without_a_disc_structure_fails_the_structural_scan() {
        // A VALID UDF image whose BDMV tree is unfindable: rename every BDMV
        // byte run in a copy of the fixture image. The volume still opens (the
        // anchor + descriptors are untouched), but the disc-structure walk
        // finds no BDMV and fails — the post-open error road for `.iso` input.
        let dir = scratch("bdmv-less-iso");
        std::fs::create_dir_all(&dir).expect("the scratch dir creates");
        let iso = dir.join("renamed.iso");
        let mut bytes = std::fs::read(fixture("BigBuckBunny.iso")).expect("the fixture reads");
        let needle = b"BDMV";
        let mut at = 0;
        while let Some(hit) =
            bytes.get(at..).and_then(|tail| tail.windows(needle.len()).position(|w| w == needle))
        {
            let start = at.checked_add(hit).expect("offset fits");
            let end = start.checked_add(needle.len()).expect("offset fits");
            bytes.get_mut(start..end).expect("the window is in range").copy_from_slice(b"XDMV");
            at = start.checked_add(1).expect("offset fits");
        }
        std::fs::write(&iso, bytes).expect("the scratch iso writes");
        let message =
            scan_structural(&Input::Iso(iso)).expect_err("a volume with no BDMV cannot list");
        assert!(!message.is_empty(), "the failure carries a user-facing message");
    }

    #[test]
    fn a_structureless_folder_fails_the_structural_scan() {
        // A directory with no BDMV structure at all: the hard-error road —
        // the same message a bad pick surfaces in the failure state.
        let dir = scratch("empty");
        std::fs::create_dir_all(&dir).expect("the scratch dir creates");
        let message =
            scan_structural(&Input::Folder(dir)).expect_err("no Blu-ray structure to open");
        assert!(!message.is_empty(), "the failure carries a user-facing message");
    }

    #[test]
    fn a_structureless_folder_fails_the_measured_scan_too() {
        // The measured road reports the same hard error instead of panicking
        // (unreachable in practice — the structural scan just listed the same
        // structure — but the result is a Result, not a hope).
        let dir = scratch("empty-measured");
        std::fs::create_dir_all(&dir).expect("the scratch dir creates");
        let message = super::scan_measured(
            &Input::Folder(dir),
            &[],
            &std::collections::BTreeSet::new(),
            bdinfo_rs_core::report::text::RenderOptions::default(),
            ScanOptions::default(),
            &mut super::no_progress,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect_err("no Blu-ray structure to open");
        assert!(!message.is_empty(), "the failure carries a user-facing message");
    }

    /// The committed fixture disc copied to `name` and made to look
    /// AACS-encrypted: the `AACS/Unit_Key_RO.inf` key file the probe looks for,
    /// and a stream file whose first two 6144-byte Aligned Units carry the
    /// encryption fingerprint — a sync byte at the head of the unit and none in
    /// the 31 source packets after it (AACS Blu-ray Prerecorded Book §3.10
    /// leaves only each unit's first 16 bytes in the clear).
    ///
    /// The disc's real packets follow that prefix, so the codec pass still has
    /// something to find if it runs — which is what lets a test tell a skipped
    /// pass from a completed one.
    fn encrypted_fixture(name: &str) -> PathBuf {
        const UNIT: usize = 6144;
        const PACKET: usize = 192;

        let root = scratch(name);
        let _ = std::fs::remove_dir_all(&root);
        copy_tree(&fixture("BigBuckBunny"), &root);
        std::fs::create_dir_all(root.join("AACS")).expect("the AACS dir creates");
        std::fs::write(root.join("AACS/Unit_Key_RO.inf"), b"key").expect("the key file writes");

        let stream = root.join("BDMV/STREAM/00000.m2ts");
        let real = std::fs::read(&stream).expect("the fixture stream reads");
        // 0xA5 fills the ciphertext: any byte but the 0x47 sync the probe
        // counts. Each unit then gets its one leading sync back.
        let mut bytes = vec![0xA5_u8; UNIT.checked_mul(2).expect("two units fit")];
        for unit in bytes.chunks_exact_mut(UNIT) {
            let sync = unit.get_mut(4).expect("a unit holds a first packet header");
            *sync = 0x47;
        }
        bytes.extend_from_slice(&real);
        std::fs::write(&stream, &bytes).expect("the encrypted-looking stream writes");
        // The prefix is a whole number of source packets, so the real packets
        // after it stay on the 192-byte grid the demux walks.
        assert_eq!(bytes.len() % PACKET, real.len() % PACKET, "the packet grid is preserved");
        root
    }

    #[test]
    fn the_crafted_encrypted_fixture_reads_as_encrypted() {
        // The fixture builder is only useful if the probe agrees with it, and
        // if the untouched fixture it is built from does NOT read as encrypted
        // — the key file alone must never be the verdict.
        let root = encrypted_fixture("aacs-probe");
        let encrypted =
            BdRom::open(&FsDir::new(&root), ScanMode::Metadata).expect("the crafted disc opens");
        assert!(encrypted.is_aacs_encrypted, "the crafted stream shows the fingerprint");

        let plain = BdRom::open(&FsDir::new(fixture("BigBuckBunny")), ScanMode::Metadata)
            .expect("the fixture disc opens");
        assert!(!plain.is_aacs_encrypted, "an ordinary disc is not encrypted");
    }

    /// The metadata open the listing seam decides from, flagged or not.
    fn cheap_open(is_aacs_encrypted: bool) -> (BdRom, Vec<super::ScanError>) {
        let bdrom = BdRom::open(&FsDir::new(fixture("BigBuckBunny")), ScanMode::Metadata)
            .expect("the fixture disc opens");
        (BdRom { is_aacs_encrypted, ..bdrom }, Vec::new())
    }

    /// Records every mode the seam asks for, answering each from `answer`.
    fn opened_modes<F>(answer: F) -> (super::Opened, Vec<ScanMode>)
    where
        F: Fn(ScanMode) -> super::Opened,
    {
        let modes = RefCell::new(Vec::new());
        let result = super::listing_scan(|mode| {
            modes.borrow_mut().push(mode);
            answer(mode)
        });
        (result, modes.into_inner())
    }

    #[test]
    fn the_listing_seam_stops_at_the_metadata_open_for_an_encrypted_disc() {
        // Recorded, not inferred from the result: the codec pass must never be
        // *asked for*, because asking is what reads the disc end to end.
        let (result, modes) = opened_modes(|_| Ok(cheap_open(true)));
        let (bdrom, errors) = result.expect("the metadata open stands in");
        assert_eq!(modes, [ScanMode::Metadata], "the codec pass must not be reached");
        assert!(bdrom.is_aacs_encrypted);
        assert!(errors.is_empty());
    }

    #[test]
    fn the_listing_seam_takes_the_codec_pass_for_an_ordinary_disc() {
        let (result, modes) = opened_modes(|mode| {
            let bdrom = cheap_open(false).0;
            let label = if mode == ScanMode::Codecs { "DEEP" } else { "CHEAP" };
            Ok((BdRom { volume_label: label.to_owned(), ..bdrom }, Vec::new()))
        });
        let (bdrom, _) = result.expect("the codec pass answers");
        assert_eq!(modes, [ScanMode::Metadata, ScanMode::Codecs], "both opens, in that order");
        assert_eq!(bdrom.volume_label, "DEEP", "the codec pass is what the listing keeps");
    }

    #[test]
    fn the_listing_seam_reports_a_failure_from_either_open() {
        // The first open failing is the ordinary "not a Blu-ray" road; the
        // second failing means the media went away between the two, and is the
        // listing's failure rather than a silently degraded table.
        let (result, modes) = opened_modes(|_| Err("no disc".to_owned()));
        assert_eq!(result.expect_err("the metadata failure propagates"), "no disc");
        assert_eq!(modes, [ScanMode::Metadata], "a failed first open reaches no further");

        let (result, modes) = opened_modes(|mode| match mode {
            ScanMode::Metadata => Ok(cheap_open(false)),
            _ => Err("the disc vanished".to_owned()),
        });
        assert_eq!(result.expect_err("the codec failure propagates"), "the disc vanished");
        assert_eq!(modes, [ScanMode::Metadata, ScanMode::Codecs]);
    }

    #[test]
    fn an_encrypted_disc_lists_without_running_the_codec_pass() {
        // The quick codec pass ends at the packet that completes the last
        // stream's codec detail. Ciphertext never completes one, so on an
        // encrypted disc the pass has no stopping point and reads every stream
        // file to EOF — tens of gigabytes off an optical drive, for detail no
        // key can recover. The listing must stop at the metadata pass instead.
        let root = encrypted_fixture("aacs-listing");
        let listed =
            scan_structural(&Input::Folder(root.clone())).expect("an encrypted disc still lists");
        assert!(listed.bdrom.is_aacs_encrypted);
        assert!(!listed.bdrom.playlists.is_empty(), "the structure still lists");

        let metadata =
            BdRom::open(&FsDir::new(&root), ScanMode::Metadata).expect("the metadata pass opens");
        assert_eq!(listed.bdrom.playlists, metadata.playlists, "the listing ran the codec pass");

        // The proof that the assertion above discriminates: over this very
        // disc the codec pass produces different streams, so a listing that ran
        // it could not have matched the metadata pass.
        let codecs =
            BdRom::open(&FsDir::new(&root), ScanMode::Codecs).expect("the codec pass opens");
        assert_ne!(
            codecs.playlists, metadata.playlists,
            "the fixture cannot tell the two passes apart, so the assertion above proves nothing"
        );
    }

    #[test]
    fn an_ordinary_disc_still_gets_the_codec_pass() {
        // The other side of the same gate: an unencrypted disc must keep the
        // scanned codec detail the panes show, so the listing has to be the
        // deeper pass rather than the metadata one.
        let root = fixture("BigBuckBunny");
        let listed = scan_structural(&Input::Folder(root.clone())).expect("the fixture lists");
        assert!(!listed.bdrom.is_aacs_encrypted);
        let codecs =
            BdRom::open(&FsDir::new(&root), ScanMode::Codecs).expect("the codec pass opens");
        assert_eq!(listed.bdrom.playlists, codecs.playlists, "the listing skipped the codec pass");
    }

    #[test]
    fn the_listing_reports_the_codec_pass_to_completion() {
        // The quick codec pass reports through the caller's callback (the
        // metadata open that precedes it reports nothing): first event at
        // zero, one fixed byte total throughout, monotonic, closing exactly
        // on the total — the shape the shell's progress bar projects.
        let mut events: Vec<(String, u64, u64)> = Vec::new();
        let listed = super::scan_structural(
            &Input::Folder(fixture("BigBuckBunny")),
            &mut |progress| events.push((progress.file.to_owned(), progress.done, progress.total)),
            &AtomicBool::new(false),
        )
        .expect("the fixture lists");
        assert!(!listed.bdrom.playlists.is_empty());
        let (file, first_done, total) = events.first().expect("the codec pass reports").clone();
        assert_eq!(file, "00000.M2TS", "the fixture's one stream file is what the pass reads");
        assert_eq!(first_done, 0, "the pass opens at zero");
        assert!(events.iter().all(|(_, _, each)| *each == total), "one fixed byte total");
        assert!(events.windows(2).all(|pair| pair[0].1 <= pair[1].1), "done never regresses");
        assert_eq!(events.last().map(|(_, done, _)| *done), Some(total), "the pass closes full");
    }

    #[test]
    fn a_preset_cancel_flag_aborts_the_listing_before_any_progress() {
        // The cancel flag is polled before each read, so a cancel honoured
        // before the first read produces NO progress event at all — a caller
        // waiting on one to conclude a cancelled listing is over would wait
        // forever.
        let mut events: Vec<u64> = Vec::new();
        let message = super::scan_structural(
            &Input::Folder(fixture("BigBuckBunny")),
            &mut |progress| events.push(progress.done),
            &AtomicBool::new(true),
        )
        .expect_err("a cancelled listing yields no table");
        assert_eq!(message, "scan cancelled");
        assert!(events.is_empty(), "no event precedes the cancel poll");
    }

    #[test]
    fn structural_warnings_surface_a_corrupt_playlist() {
        // Copy the committed fixture and corrupt its one playlist: the
        // resilient scan still opens the disc, records the parse failure, and
        // the seam surfaces it as a warning line naming the file.
        let root = scratch("corrupt");
        let _ = std::fs::remove_dir_all(&root);
        copy_tree(&fixture("BigBuckBunny"), &root);
        std::fs::write(root.join("BDMV/PLAYLIST/00000.mpls"), b"garbage")
            .expect("the playlist corrupts");
        let structural =
            scan_structural(&Input::Folder(root)).expect("the resilient scan still opens");
        assert!(!structural.warnings.is_empty(), "the corrupt playlist is recorded");
        assert!(
            structural.warnings.iter().any(|warning| warning.contains("00000.mpls")),
            "the warning names the file: {:?}",
            structural.warnings
        );
        // The disc still lists: the core recovered the playlist from the
        // disc's BDMV/BACKUP copy — resilience, not silence; the warning above
        // is the record of the primary's corruption.
        assert!(!structural.bdrom.playlists.is_empty());
    }

    #[test]
    fn catch_scan_panic_passes_results_through() {
        assert_eq!(super::catch_scan_panic(|| Ok(7_u8)), Ok(7));
        assert_eq!(
            super::catch_scan_panic::<u8>(|| Err("bad disc".to_owned())),
            Err("bad disc".to_owned())
        );
    }

    #[test]
    fn catch_scan_panic_maps_a_panic_to_the_error_road() {
        // The panic road: what a core no-panic-contract violation would take.
        let result = super::catch_scan_panic::<u8>(|| panic!("core contract broken"));
        assert_eq!(result, Err("scan terminated unexpectedly".to_owned()));
    }

    #[test]
    fn the_panic_alarm_fires_on_unwind_and_not_when_defused() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fired = AtomicUsize::new(0);
        // ONE closure (Copy — it captures a shared borrow) arms both alarms,
        // so the payload that must stay silent on the defuse road is the very
        // one proven to fire on the unwind road.
        let bump = || {
            fired.fetch_add(1, Ordering::Relaxed);
        };
        // The success road: defused, so the alarm never fires.
        super::PanicAlarm::new(bump).defuse();
        assert_eq!(fired.load(Ordering::Relaxed), 0);
        // The panic road: the unwind drops the armed alarm, which fires once.
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            let _alarm = super::PanicAlarm::new(bump);
            panic!("worker died");
        }));
        assert!(unwound.is_err());
        assert_eq!(fired.load(Ordering::Relaxed), 1);
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
