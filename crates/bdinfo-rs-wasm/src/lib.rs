//! WebAssembly browser bindings for the bdinfo-rs Blu-ray analyzer.
//!
//! This crate exposes the library's whole **measured** scan pipeline to the
//! browser. Several entry points feed the very same render path:
//!
//! - [`scan_report`] — the in-memory export: BDMV bytes are framed into a synthetic disc tree (six
//!   `u32`-BE sections), opened with [`BdRom::open_resilient_with`] (packet scan **on**), and
//!   rendered to the classic report. Used by the native ⇄ in-browser byte-parity test.
//! - [`scan_files`] — the streaming export: a `webkitdirectory`-selected BDMV folder arrives as a
//!   flat list of `(relativePath, File)` pairs. The files stay on disk; their bytes are read
//!   **synchronously** at byte offsets through [`web_sys::FileReaderSync`] (no JSPI, no Asyncify),
//!   so a multi-GB `*.m2ts` never has to fit in memory. The export runs inside a Web Worker (the
//!   only scope where `FileReaderSync` exists). [`inspect_files`] is its structural twin: the fast
//!   metadata-only scan (`bdinfo-rs <disc> --list`) over the same pairs.
//! - [`scan_iso`] / [`inspect_iso`] — the streaming `.iso` exports: a single OS-picked `.iso`
//!   `File` is opened through the core read-only UDF 2.50 reader ([`UdfSource`]) over the same
//!   windowed [`web_sys::FileReaderSync`] cursor ([`WebIso`] is the [`IsoReader`] factory), so a
//!   multi-GB disc image is never staged in memory either — then rendered through the identical
//!   path, producing the classic report `bdinfo-rs <disc>.iso` writes.
//! - [`render_report`] — the classic report rendered from a [`mirror::Disc`] a scan already
//!   produced, with no media read at all.
//!
//! Both build a [`Node`] tree behind the [`BdDir`]/[`BdFile`] seam and feed it to
//! one shared [`render_disc`] — so the in-memory and the file-backed paths render
//! byte-for-byte identically, and one golden (this crate's own
//! `tests/golden_report.txt`, rendered from the same Big Buck Bunny fixture the
//! native end-to-end test scans) pins both.
//!
//! It is an INDEPENDENT workspace (see `Cargo.toml`): wasm-bindgen's generated
//! glue uses `unsafe`, so this crate sits OUTSIDE the `forbid(unsafe_code)`
//! posture of `bdinfo-rs-core` / `bdinfo-rs`. The core library itself stays
//! memory-safe; only the thin browser shim here is exempt.
//!
//! ## `scan_report` input framing
//!
//! [`scan_report`] takes one byte buffer holding up to six `u32` big-endian
//! length-prefixed sections, assigned in fixed order to the synthetic disc's
//! six files — the framing and BDMV layout defined by the core in-memory
//! backend ([`mem`]), the same framing the `parse_report` fuzz target reads,
//! so a seed means the same disc to both harnesses. A missing or truncated
//! section leaves its file empty (the resilient-open absence path).

// `BdmvDir`/`SeekFrom`/`io`/`glob_ci`/`BdFile`/`SearchOption` are named only by
// the web-path logic and the reader math (`Node`/`assemble_tree`/`seek_target`)
// — tested natively, but absent from a native NON-test build, so gate them to
// where they live to stay dead-code-clean. `BufRead`/`BufReader`/`Read`/`Seek`/
// `ReadSeek`/`extension_of_name`/`JsCast`/`JsValue` are named only by the wasm32
// browser glue.
#[cfg(any(target_arch = "wasm32", test))]
use std::io::{self, SeekFrom};
#[cfg(target_arch = "wasm32")]
use std::io::{BufRead, BufReader, Read, Seek};
use std::sync::atomic::AtomicBool;

// The core scan switches are aliased because the browser API publishes a
// `ScanOptions` of its own — `mirror::ScanOptions`, the object the scanning
// exports take, which carries the retention switch this type holds plus the
// report and classification choices a disc open knows nothing about.
use bdinfo_rs_core::bdrom::disc::{
    BdRom, ScanMode, ScanOptions as CoreScanOptions, ScanProgress, ScanReport,
};
use bdinfo_rs_core::bdrom::order::PlaylistFilter;
#[cfg(any(target_arch = "wasm32", test))]
use bdinfo_rs_core::bdrom::order::{named_selection, selection_order, selection_stream_files};
#[cfg(any(target_arch = "wasm32", test))]
use bdinfo_rs_core::discovery::BdmvDir;
use bdinfo_rs_core::error::BdError;
use bdinfo_rs_core::report::text::{self, RenderOptions};
#[cfg(any(target_arch = "wasm32", test))]
use bdinfo_rs_core::vfs::fs::glob_ci;
use bdinfo_rs_core::vfs::udf::source::{IsoReader, UdfSource};
use bdinfo_rs_core::vfs::{BdDir, mem};
#[cfg(any(target_arch = "wasm32", test))]
use bdinfo_rs_core::vfs::{BdFile, SearchOption};
#[cfg(target_arch = "wasm32")]
use bdinfo_rs_core::vfs::{ReadSeek, extension_of_name};
use wasm_bindgen::prelude::wasm_bindgen;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{JsCast, JsValue};

// The structured disc model handed to JavaScript. `pub` because its types are
// the published API surface: `tsify` emits their TypeScript declarations into
// the generated `.d.ts`, which the npm package exposes at its `/wasm` subpath.
pub mod mirror;

// Named by the browser exports that return the disc model and by their docs,
// and by the measured-scan pairing (`measured_result`) those exports return —
// which is target-agnostic, hence the same gate as the rest of that logic.
#[cfg(any(target_arch = "wasm32", test))]
use mirror::{Disc, ScanOptions, ScanResult};

/// The read-ahead window each [`WebReader`] fill pulls from `FileReaderSync` in
/// one go, so a front-to-back demux crosses the JS boundary once per MiB rather
/// than once per small parser read.
#[cfg(target_arch = "wasm32")]
const READ_WINDOW: usize = 1_048_576; // 1 MiB

/// A node in a synthetic disc tree — a directory holding sub-directories and
/// files of one concrete [`BdFile`] backend (`F`).
///
/// Production fills it with the file-backed [`WebFile`]; the native tests
/// drive the same walk with the core in-memory file ([`mem::MemFile`]), so
/// the directory walk, glob matching, and recursion behave identically
/// whatever the bytes are backed by — hence the same
/// `cfg(any(wasm32, test))` gate as the rest of the web-path logic.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone)]
struct Node<F> {
    name: String,
    full: String,
    dirs: Vec<Self>,
    files: Vec<F>,
}

#[cfg(any(target_arch = "wasm32", test))]
impl<F> Node<F> {
    fn dir(name: &str, full: &str) -> Self {
        Self { name: name.to_owned(), full: full.to_owned(), dirs: Vec::new(), files: Vec::new() }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
impl<F: BdFile + Clone + 'static> Node<F> {
    fn collect_pattern(&self, pattern: &str, recurse: bool, out: &mut Vec<Box<dyn BdFile>>) {
        for f in &self.files {
            if glob_ci(pattern.as_bytes(), f.name().as_bytes()) {
                out.push(Box::new(f.clone()));
            }
        }
        if recurse {
            for d in &self.dirs {
                d.collect_pattern(pattern, recurse, out);
            }
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
impl<F: BdFile + Clone + 'static> BdDir for Node<F> {
    fn name(&self) -> &str {
        &self.name
    }

    fn full_name(&self) -> &str {
        &self.full
    }

    fn parent(&self) -> Option<Box<dyn BdDir>> {
        None
    }

    fn get_files_pattern_option(
        &self,
        pattern: &str,
        option: SearchOption,
    ) -> io::Result<Vec<Box<dyn BdFile>>> {
        let mut out = Vec::new();
        self.collect_pattern(pattern, option == SearchOption::AllDirectories, &mut out);
        Ok(out)
    }

    fn get_directories(&self) -> io::Result<Vec<Box<dyn BdDir>>> {
        Ok(self.dirs.iter().map(|d| Box::new(d.clone()) as Box<dyn BdDir>).collect())
    }
}

// ── in-memory backend (the `scan_report` framing path) ──────────────────────

/// The framed synthetic tree [`scan_report`] scans: the shared six-section
/// framing and BDMV layout of the core in-memory backend ([`mem`]), built
/// under the `WASMDISC` root the goldens pin.
fn framed_tree(data: &[u8]) -> mem::MemDir {
    mem::build_tree("WASMDISC", mem::split_sections(data, mem::SECTION_COUNT))
}

// ── file-backed backend (the `scan_files` FileReaderSync path) ──────────────

/// A `Read + Seek` cursor over one browser `File`/`Blob`, reading each window
/// **synchronously** via [`web_sys::FileReaderSync`].
///
/// One [`read`](Read::read) slices `[pos, pos+len)` off the blob and reads just
/// that window — never the whole (possibly multi-GB) file. Wrapping the raw
/// cursor in a [`READ_WINDOW`]-sized [`BufReader`] (see
/// [`WebFile::open_read`]) coalesces the parser's small reads into one
/// `FileReaderSync` call per window.
#[cfg(target_arch = "wasm32")]
struct WebReader {
    file: web_sys::File,
    reader: web_sys::FileReaderSync,
    pos: u64,
    len: u64,
}

/// Renders a `JsValue` error to a short message for an [`io::Error`].
///
/// A thrown string comes back directly; an `Error`/`DOMException` is an object
/// whose `message` property carries the human-readable text (e.g. a
/// `NotFoundError` from a revoked `File`), so reach for that before falling
/// back to a generic label.
#[cfg(target_arch = "wasm32")]
fn js_message(value: &JsValue) -> String {
    if let Some(text) = value.as_string() {
        return text;
    }
    if let Ok(message) = js_sys::Reflect::get(value, &JsValue::from_str("message"))
        && let Some(text) = message.as_string()
        && !text.is_empty()
    {
        return text;
    }
    "JavaScript exception".to_owned()
}

/// The byte window `[start, end)` a [`WebReader`] read of `buf_len` bytes at
/// `pos` should slice off a `len`-byte blob, or `None` when there is nothing to
/// read (an empty caller buffer, or a cursor at/after EOF).
///
/// The panic-safety-critical arithmetic split out of the `FileReaderSync` I/O so
/// the off-by-one and EOF-clamp edges are exercised on the native build. `end` is clamped to `len`,
/// so a window that would cross EOF is shortened rather than over-reading the caller's `buf`.
#[cfg(any(target_arch = "wasm32", test))]
fn read_window(pos: u64, buf_len: usize, len: u64) -> Option<(u64, u64)> {
    if buf_len == 0 || pos >= len {
        return None;
    }
    let end = pos.saturating_add(buf_len as u64).min(len);
    Some((pos, end))
}

/// Resolves a [`SeekFrom`] against a cursor at `pos` over a `len`-byte file to an
/// absolute offset, rejecting a target before byte 0 and saturating a target
/// past `u64::MAX` to `u64::MAX` (the next read then returns 0) rather than
/// wrapping.
///
/// The `i128` intermediate cannot overflow — `len`/`pos` are `u64` and the
/// offset is `i64`, whose sum is far inside `i128` — so `wrapping_add` is exact.
///
/// # Errors
/// [`io::ErrorKind::InvalidInput`] when the resolved target is before the start.
#[cfg(any(target_arch = "wasm32", test))]
fn seek_target(from: SeekFrom, pos: u64, len: u64) -> io::Result<u64> {
    let target: i128 = match from {
        SeekFrom::Start(n) => i128::from(n),
        SeekFrom::End(n) => i128::from(len).wrapping_add(i128::from(n)),
        SeekFrom::Current(n) => i128::from(pos).wrapping_add(i128::from(n)),
    };
    if target < 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start of file"));
    }
    Ok(u64::try_from(target).unwrap_or(u64::MAX))
}

#[cfg(target_arch = "wasm32")]
impl Read for WebReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some((start, end)) = read_window(self.pos, buf.len(), self.len) else {
            return Ok(0);
        };
        let blob = self
            .file
            .slice_with_f64_and_f64(start as f64, end as f64)
            .map_err(|e| io::Error::other(js_message(&e)))?;
        let array = self
            .reader
            .read_as_array_buffer(&blob)
            .map_err(|e| io::Error::other(js_message(&e)))?;
        let view = js_sys::Uint8Array::new(&array);
        let n = view.length() as usize;
        // `n` is the length of a blob sliced to at most `buf.len()` bytes, so the
        // destination window always exists; copy into it when non-empty.
        if let Some(dst) = buf.get_mut(..n) {
            view.copy_to(dst);
        }
        self.pos = self.pos.saturating_add(n as u64);
        Ok(n)
    }
}

#[cfg(target_arch = "wasm32")]
impl Seek for WebReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = seek_target(from, self.pos, self.len)?;
        Ok(self.pos)
    }
}

/// A file backed by a browser `File` handle — the [`BdFile`] backend for the
/// `webkitdirectory` streaming path. Bytes are read on demand through
/// [`WebReader`]; only metadata (name, full path, extension, length) is held
/// eagerly.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
struct WebFile {
    name: String,
    full: String,
    /// Derived once at construction by the core seam's `extension_of_name`, the
    /// rule every backend shares, and borrowed by [`BdFile::extension`] — which
    /// returns a `&str`, so the owned `String` has to be stored.
    extension: String,
    file: web_sys::File,
    length: u64,
}

// SAFETY: `wasm32-unknown-unknown` in the browser is single-threaded — there is
// no other thread to send the `web_sys::File` handle to. The analyzer's demux
// uses its `cfg(wasm)` sequential path (no `thread::scope`), so the `Send`
// supertrait on `BdFile` is satisfied for the type system but never exercised
// across a real thread boundary. The handle is only ever touched on the Worker
// thread that created it.
//
// The impl is gated to the single-threaded wasm build. Enable wasm threads
// (`+atomics`, shared memory, a worker pool) and that premise collapses — a
// `web_sys::File` moved across workers would be UB — so the `compile_error!`
// below forces this seam to be revisited rather than silently miscompiling. On
// native (the `rlib` the parity test links) the demux path that needs `Send` is
// never reached, so no hand-written impl is required there.
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl Send for WebFile {}

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
compile_error!(
    "WebFile's Send and WebIso's Send + Sync impls are unsound with wasm threads (+atomics): a \
     web_sys::File moved across workers is undefined behavior. Revisit the FileReaderSync seam \
     before enabling threads."
);

/// Opens a [`WebReader`] over `file` (a `length`-byte blob).
///
/// The one place a `FileReaderSync` is acquired, shared by
/// [`WebFile::open_read`] (folder input) and [`WebIso::open`] (`.iso` input) so
/// there is no duplicated I/O setup.
///
/// # Errors
/// [`io::Error`] when `FileReaderSync` is unavailable — i.e. the call is not on a
/// Worker thread, the only scope the API exists in.
#[cfg(target_arch = "wasm32")]
fn open_web_reader(file: &web_sys::File, length: u64) -> io::Result<WebReader> {
    let reader = web_sys::FileReaderSync::new().map_err(|e| {
        io::Error::other(format!(
            "FileReaderSync unavailable (run the scan in a Worker): {}",
            js_message(&e)
        ))
    })?;
    Ok(WebReader { file: file.clone(), reader, pos: 0, len: length })
}

#[cfg(target_arch = "wasm32")]
impl WebFile {
    fn open(&self) -> io::Result<WebReader> {
        open_web_reader(&self.file, self.length)
    }
}

#[cfg(target_arch = "wasm32")]
impl BdFile for WebFile {
    fn name(&self) -> &str {
        &self.name
    }

    fn full_name(&self) -> &str {
        &self.full
    }

    fn extension(&self) -> &str {
        &self.extension
    }

    fn length(&self) -> u64 {
        self.length
    }

    fn is_dir(&self) -> bool {
        false
    }

    fn open_read(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(BufReader::with_capacity(READ_WINDOW, self.open()?)))
    }

    fn open_text(&self) -> io::Result<Box<dyn BufRead>> {
        Ok(Box::new(BufReader::with_capacity(READ_WINDOW, self.open()?)))
    }
}

/// The [`IsoReader`] factory backing the `.iso` streaming path.
///
/// The `.iso` counterpart of [`WebFile`]: it wraps one browser `File` (the
/// picked `.iso`) and its byte length; each [`open`](IsoReader::open) yields a
/// fresh [`READ_WINDOW`]-buffered [`WebReader`] cursor over it (via the shared
/// [`open_web_reader`]), so the core UDF reader streams the image through
/// `FileReaderSync` windowed reads — a multi-GB `.iso` never has to fit in memory.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone)]
struct WebIso {
    /// The picked `.iso` `File`; its bytes are read lazily, never staged.
    file: web_sys::File,
    /// The image length in bytes (`File.size`).
    length: u64,
}

// SAFETY: the same single-threaded-wasm argument as [`WebFile`]'s `Send` impl
// above. `wasm32-unknown-unknown` in the browser is single-threaded, so the
// `web_sys::File` a `WebIso` holds is never moved to, or shared with, another
// thread. [`IsoReader`] requires `Send + Sync` (a `UdfSource` keeps its factory
// behind an `Arc` so every file handle reopens through it), and a
// `web_sys::File` — wrapping a `!Send`/`!Sync` `JsValue` — is neither by default;
// on the single-threaded target both marker impls are vacuously sound. The handle
// is only ever touched on the Worker thread that created it.
//
// Both impls are gated to the single-threaded wasm build; the `+atomics`
// `compile_error!` above forces this seam (and `WebFile`'s) to be revisited
// before threads are enabled, rather than silently miscompiling.
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl Send for WebIso {}
// SAFETY: as for the `Send` impl directly above — the `web_sys::File` is never
// touched off the single Worker thread, so `Sync` is vacuously sound on the
// single-threaded wasm target (and gated to it).
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl Sync for WebIso {}

#[cfg(target_arch = "wasm32")]
impl IsoReader for WebIso {
    fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(BufReader::with_capacity(
            READ_WINDOW,
            open_web_reader(&self.file, self.length)?,
        )))
    }
}

/// Splits a `webkitRelativePath` into its non-empty components, tolerating both
/// `/` and `\` separators.
#[cfg(any(target_arch = "wasm32", test))]
fn path_components(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|s| !s.is_empty()).collect()
}

/// Why a `(relativePath, File)` selection could not be assembled into a disc
/// tree. Surfaced to the caller (see [`scan_files`]) so a wrong pick reads as a
/// clear error rather than a silent empty scan.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, PartialEq, Eq)]
enum TreeError {
    /// A relative path named only a file, with no wrapping folder — it cannot be
    /// placed under a shared disc root.
    BareFile(String),
    /// Two entries disagreed on the first path component. A `webkitdirectory`
    /// pick always shares one wrapping folder, so a mismatch means the inputs
    /// are not one coherent selection.
    MixedRoots(String, String),
    /// No usable entries at all (every path was empty, or the list was empty).
    Empty,
}

#[cfg(any(target_arch = "wasm32", test))]
impl TreeError {
    fn message(&self) -> String {
        match self {
            Self::BareFile(path) => {
                format!("path {path:?} has no directory component; select a disc folder")
            }
            Self::MixedRoots(first, other) => {
                format!("selection spans more than one root folder ({first:?} and {other:?})")
            }
            Self::Empty => "no files to scan".to_owned(),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl From<TreeError> for JsValue {
    fn from(error: TreeError) -> Self {
        Self::from_str(&error.message())
    }
}

/// The deepest directory chain the tree builder descends. A real disc tree is
/// `disc/BDMV/<dir>/<sub>` — single digits deep — so this only bites a crafted
/// path list, capping the tree depth so the (recursive) [`Node::collect_pattern`]
/// walk over it can never overflow the stack.
#[cfg(any(target_arch = "wasm32", test))]
const MAX_TREE_DEPTH: usize = 64;

/// Assembles a synthetic disc tree from parsed `(components, file)` entries —
/// the backend-agnostic core of [`build_web_tree`], unit-tested on its own.
///
/// `components` is a relative path already split by [`path_components`]: the
/// last element is the file name, the first is the shared disc-root folder.
/// Validates that every entry shares one root folder and names a directory,
/// synthesizes a wrapper root above a picked `BDMV` folder (so the core's
/// disc-root walk-up resolves — selecting `BDMV` directly is the README's
/// primary instruction), then inserts each file iteratively (no recursion on
/// caller-controlled depth).
///
/// # Errors
/// [`TreeError`] when the entries span more than one root folder, a path is a
/// bare file name, or there are no usable entries.
#[cfg(any(target_arch = "wasm32", test))]
fn assemble_tree<F>(entries: Vec<(Vec<&str>, F)>) -> Result<Node<F>, TreeError> {
    let mut shared_root: Option<&str> = None;
    for (comps, _) in &entries {
        let Some((_, dirs)) = comps.split_last() else { continue };
        let Some((&first, _)) = dirs.split_first() else {
            return Err(TreeError::BareFile(comps.join("/")));
        };
        match shared_root {
            None => shared_root = Some(first),
            Some(root) if root == first => {}
            Some(root) => return Err(TreeError::MixedRoots(root.to_owned(), first.to_owned())),
        }
    }
    let Some(shared_root) = shared_root else { return Err(TreeError::Empty) };

    // A `webkitdirectory` pick of the `BDMV` folder itself yields paths rooted
    // at `BDMV`; the core resolves the disc root by walking *up* from `BDMV`, so
    // wrap it in a synthetic disc root (mirroring the in-memory `WASMDISC`) and
    // let `BDMV` become the root's first child.
    let wrap = BdmvDir::from_name(shared_root) == Some(BdmvDir::Bdmv);
    let root_name = if wrap { "WASMDISC" } else { shared_root };
    let mut root = Node::dir(root_name, root_name);
    for (comps, file) in entries {
        let Some((_, dirs)) = comps.split_last() else { continue };
        // The directories to descend from the root to the file's parent: when
        // wrapping, the shared root (`BDMV`) is the first child of the synthetic
        // root; otherwise the shared root *is* the root, so only the components
        // between it and the file name are intermediate directories.
        let chain =
            if wrap { dirs } else { dirs.split_first().map_or([].as_slice(), |(_, rest)| rest) };
        insert_file(&mut root, chain, file);
    }
    Ok(root)
}

/// Inserts `file` at `chain` (the directory names below `root`), creating
/// intermediate [`Node`]s as needed. Iterative — it descends in a loop so a
/// crafted deep path list cannot overflow the stack — and bounded by
/// [`MAX_TREE_DEPTH`]: a path deeper than any real disc is dropped rather than
/// growing the tree without limit.
#[cfg(any(target_arch = "wasm32", test))]
fn insert_file<F>(root: &mut Node<F>, chain: &[&str], file: F) {
    if chain.len() > MAX_TREE_DEPTH {
        return;
    }
    let mut node = root;
    for &dir in chain {
        let idx = if let Some(i) = node.dirs.iter().position(|d| d.name == dir) {
            i
        } else {
            let full = format!("{}/{dir}", node.full);
            node.dirs.push(Node::dir(dir, &full));
            node.dirs.len().saturating_sub(1)
        };
        // `idx` is a position just found in `node.dirs` or the index of the entry
        // just pushed, so it is always in bounds.
        #[expect(
            clippy::indexing_slicing,
            reason = "idx is freshly found-or-pushed in node.dirs, always in bounds"
        )]
        let next = &mut node.dirs[idx];
        node = next;
    }
    node.files.push(file);
}

/// Builds the file-backed disc tree from the parallel `(relativePath, File)`
/// lists: each file's `webkitRelativePath` is split into components and handed
/// with its [`WebFile`] to [`assemble_tree`].
///
/// # Errors
/// Returns a `JsValue` if the two lists differ in length, any entry is not a
/// `File`, or the paths do not form one coherent disc selection (see
/// [`TreeError`]).
#[cfg(target_arch = "wasm32")]
fn build_web_tree(paths: &[String], files: &js_sys::Array) -> Result<Node<WebFile>, JsValue> {
    let count = paths.len();
    if count != files.length() as usize {
        return Err(JsValue::from_str("paths and files differ in length"));
    }

    let mut entries: Vec<(Vec<&str>, WebFile)> = Vec::with_capacity(count);
    for (i, path) in paths.iter().enumerate() {
        let comps = path_components(path);
        let Some(&name) = comps.last() else { continue };
        let value = files.get(i as u32);
        let file: web_sys::File =
            value.dyn_into().map_err(|_| JsValue::from_str("entry is not a File"))?;
        let length = file.size() as u64;
        let web_file = WebFile {
            name: name.to_owned(),
            full: path.clone(),
            extension: extension_of_name(name),
            file,
            length,
        };
        entries.push((comps, web_file));
    }

    assemble_tree(entries).map_err(JsValue::from)
}

// ── shared render path ──────────────────────────────────────────────────────

// ── selection (CLI parity) ──────────────────────────────────────────────────
//
// The browser surfaces the CLI's flow: a structural scan reads the disc
// ([`inspect_files`]), the user picks some playlists, and a measured scan reads
// only the picked playlists' stream files ([`scan_files`] with a selection).
// These helpers mirror the selection logic of the `bdinfo-rs` binary — which
// this crate cannot import — over the public core types, so the browser behaves
// exactly like `--mpls` / the picker. They are `cfg(any(wasm32, test))`: the
// wasm exports use them and the native test build covers + mutates them, while a
// native non-test build omits them (so neither tier shows dead code).

/// The filter the disc model is classified against: the standard rules with
/// `short_seconds` as the length under which a playlist counts as short.
///
/// A present, finite, non-negative `short_seconds` is taken literally, so `0`
/// leaves no playlist short and disables the short rule — the meaning the CLI's
/// `--short-playlist-seconds 0` and the desktop app's threshold setting carry.
/// An absent, negative or non-finite one means the 20 s default, so a caller
/// that does not care about the threshold passes `None` and gets the classic
/// behaviour. Only the threshold is read downstream — no export filters the
/// playlists any more — so the two switches keep their defaults.
#[cfg(any(target_arch = "wasm32", test))]
fn classification_filter(short_seconds: Option<f64>) -> PlaylistFilter {
    let standard = PlaylistFilter::default();
    PlaylistFilter {
        short_playlist_seconds: short_seconds
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .unwrap_or(standard.short_playlist_seconds),
        ..standard
    }
}

/// Why a by-name [`scan_selection`] could not produce a report.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug)]
enum SelectionError {
    /// The disc structure could not be opened at all (no `BDMV`/`CLIPINF`/`PLAYLIST`).
    Open(BdError),
    /// `selection` was non-empty but named no playlist present on the disc — the
    /// CLI's `--mpls` "No matching playlists found on BD".
    NoMatch,
}

#[cfg(any(target_arch = "wasm32", test))]
impl From<BdError> for SelectionError {
    fn from(err: BdError) -> Self {
        Self::Open(err)
    }
}

#[cfg(any(target_arch = "wasm32", test))]
impl std::fmt::Display for SelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(err) => write!(f, "no readable Blu-ray structure ({err})"),
            Self::NoMatch => f.write_str("No matching playlists found on BD"),
        }
    }
}

/// Runs the **measured** scan over just the playlists named in `selection`, in
/// selection order — the browser equivalent of `bdinfo-rs <disc> --mpls A,B`.
///
/// A cheap structural scan (no packet scan) resolves the names to their stream
/// files first; the measured scan is then narrowed to those files, so an
/// unselected (possibly multi-GB) playlist is never demuxed. Names match
/// unfiltered — a short or looping playlist the `--whole` filter drops is still
/// selectable by name.
///
/// `options` governs the measured scan alone: the structural scan reads no
/// packets, so no switch it carries can change what that scan finds.
///
/// # Errors
/// [`SelectionError::Open`] if the structure is too damaged to scan, or
/// [`SelectionError::NoMatch`] if `selection` names no playlist on the disc
/// (mirroring the CLI's `--mpls`, which exits with "No matching playlists found
/// on BD" rather than rendering a header-only report).
#[cfg(any(target_arch = "wasm32", test))]
fn scan_selection(
    root: &dyn BdDir,
    selection: &[String],
    options: CoreScanOptions,
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<Scan, SelectionError> {
    let structural = BdRom::open_resilient(root, ScanMode::Metadata)?;
    let names = named_selection(&structural.bdrom.playlists, selection);
    if names.is_empty() {
        return Err(SelectionError::NoMatch);
    }
    let files = selection_stream_files(&structural.bdrom.playlists, &names);
    // The measured scan re-opens the same tree, narrowed to the selected clips.
    // It locates the same `BDMV`/`CLIPINF`/`PLAYLIST` the structural open just
    // found, so it cannot hit the only hard error (`StructureNotFound`); on that
    // unreachable failure it degrades to the structural disc (zero measured
    // tallies) rather than erroring.
    // The browser exports carry no cancel affordance: the flag is never set.
    let measured = BdRom::open_resilient_with(
        root,
        ScanMode::Full,
        options,
        Some(&files),
        progress,
        &never_cancelled(),
    )
    .unwrap_or(structural);
    let order = selection_order(&measured.bdrom.playlists, &names);
    Ok(Scan { report: measured, order })
}

// ── shared render path ──────────────────────────────────────────────────────

/// A cancel flag that never trips — the browser exports have no cancel
/// affordance (a worker-side scan ends with the page), so every open passes
/// this never-set flag.
const fn never_cancelled() -> AtomicBool {
    AtomicBool::new(false)
}

/// One completed scan: the disc the scan produced, plus the playlist `order`
/// its report renders — which the whole-disc and by-name paths derive
/// differently and everything downstream then treats alike.
///
/// Holding the two together is what lets one scan serve both outputs a caller
/// can ask for: the rendered report ([`render`](Self::render)) and the
/// structured disc ([`measured_result`]), each derived from this scan rather
/// than from the other.
#[derive(Debug)]
struct Scan {
    /// The scanned disc and the per-file failures recorded along the way.
    report: ScanReport,
    /// Indices into the disc's playlists, in the order the report prints them.
    order: Vec<usize>,
}

impl Scan {
    /// Renders the classic disc report for this scan, with `options` choosing
    /// which optional sections it carries.
    fn render(&self, options: RenderOptions) -> String {
        text::render_with(&self.report.bdrom, &self.order, &self.report.errors, options)
    }
}

/// Runs the full **measured** scan over `root`, in the CLI's `--whole` playlist
/// order.
///
/// This is the byte-for-byte core shared by every export and the native parity
/// test: [`BdRom::open_resilient_with`] with the packet scan **on** and the
/// scan's behaviour switches set by `options`, ordered by the standard filtered
/// set ([`PlaylistFilter::default`], which drops playlists shorter than 20 s and
/// looping ones). `progress` observes the demux.
///
/// # Errors
/// Returns the [`BdError`] from [`BdRom::open_resilient_with`] when the
/// structure is too damaged to open at all (no `BDMV`/`CLIPINF`/`PLAYLIST`) —
/// the caller decides whether that is an empty disc or an error to report.
fn scan_whole(
    root: &dyn BdDir,
    options: CoreScanOptions,
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<Scan, BdError> {
    let report = BdRom::open_resilient_with(
        root,
        ScanMode::Full,
        options,
        None,
        progress,
        &never_cancelled(),
    )?;
    let order = report.bdrom.presentation_order(&PlaylistFilter::default());
    Ok(Scan { report, order })
}

/// [`scan_whole`] rendered to the classic report with `options`.
///
/// The scan itself runs with [`CoreScanOptions::default`]: the report-only
/// entries this serves ([`run_report`], [`run_iso_report`]) take no options, so
/// they scan the way an unconfigured call does.
///
/// # Errors
/// As [`scan_whole`].
fn render_disc(
    root: &dyn BdDir,
    options: RenderOptions,
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<String, BdError> {
    Ok(scan_whole(root, CoreScanOptions::default(), progress)?.render(options))
}

/// The report sections `options` asks for: each option switches its own section
/// off, and an absent option — or no options object at all — leaves it on, so a
/// call that asks for neither renders the whole report, the locked bytes
/// [`RenderOptions::default`] produces.
#[cfg(any(target_arch = "wasm32", test))]
fn render_options(options: Option<&ScanOptions>) -> RenderOptions {
    RenderOptions {
        stream_diagnostics: options.and_then(|options| options.stream_diagnostics).unwrap_or(true),
        quick_summary: options.and_then(|options| options.quick_summary).unwrap_or(true),
    }
}

/// The scan switches `options` asks for. An absent `keepPartial` — or no options
/// object at all — leaves [`CoreScanOptions::default`] in force, which keeps the
/// measured state of a stream file whose read fails partway.
#[cfg(any(target_arch = "wasm32", test))]
fn scan_options(options: Option<&ScanOptions>) -> CoreScanOptions {
    let mut scan = CoreScanOptions::default();
    if let Some(keep_partial) = options.and_then(|options| options.keep_partial) {
        scan.keep_partial = keep_partial;
    }
    scan
}

/// Both outputs of one measured `scan`: the report rendered with `options`, and
/// the same scan mirrored as a [`Disc`] whose playlists are classified against
/// `filter`.
///
/// The two are derived side by side from the scan, never one from the other:
/// the report comes straight off the scanned disc, and the mirror is built from
/// that same disc. Rendering by way of the mirror instead would leave nothing
/// to compare the mirror against.
#[cfg(any(target_arch = "wasm32", test))]
fn measured_result(scan: &Scan, options: RenderOptions, filter: &PlaylistFilter) -> ScanResult {
    ScanResult {
        report: scan.render(options),
        // A `Scan` is always the packet scan, so the mirror says measured.
        disc: Disc::from_scan(&scan.report.bdrom, &scan.report.errors, true, filter),
    }
}

/// Runs a measured scan of `root`, mapping any failure to a `JsValue`.
///
/// Dispatches to the whole-disc ([`scan_whole`]) or by-name
/// ([`scan_selection`]) path — the shared head of the streaming exports, which
/// differ only in how they obtain `root` and what they do with the scan.
///
/// # Errors
/// A `JsValue` carrying the structure-open failure (empty `selection`) or the
/// [`SelectionError`] text (named `selection`).
#[cfg(target_arch = "wasm32")]
fn scan_root(
    root: &dyn BdDir,
    selection: &[String],
    options: CoreScanOptions,
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<Scan, JsValue> {
    if selection.is_empty() {
        scan_whole(root, options, progress)
            .map_err(|err| JsValue::from_str(&format!("no readable Blu-ray structure ({err})")))
    } else {
        scan_selection(root, selection, options, progress)
            .map_err(|err| JsValue::from_str(&err.to_string()))
    }
}

/// Calls the caller's progress `callback`, when it supplied one, as
/// `(file, done, total)`. A callback that throws is ignored: a scan is not
/// abandoned because the page could not draw a progress bar.
#[cfg(target_arch = "wasm32")]
fn notify(callback: Option<&js_sys::Function>, progress: &ScanProgress<'_>) {
    if let Some(callback) = callback {
        let _ = callback.call3(
            &JsValue::NULL,
            &JsValue::from_str(progress.file),
            &JsValue::from_f64(progress.done as f64),
            &JsValue::from_f64(progress.total as f64),
        );
    }
}

/// Renders the synthetic in-memory tree built from `data` (no progress).
///
/// An unopenable structure renders as the empty string — the resilient-open
/// absence path the `parse_report` fuzz target and the parity test expect.
#[must_use]
pub fn run_report(data: &[u8]) -> String {
    render_disc(&framed_tree(data), RenderOptions::default(), &mut |_| {}).unwrap_or_default()
}

/// Opens `reader` as a UDF `.iso` and renders the whole-disc report.
///
/// No selection and no progress — the backend-agnostic `.iso` entry the native
/// and in-browser parity tests drive over an in-memory image. Mirrors
/// [`run_report`] for the streaming `.iso` path; [`scan_iso`] is the
/// `wasm_bindgen` export that adds progress, selection, and error reporting
/// around the same wiring.
///
/// An image that is not a readable UDF volume, or that opens but holds no
/// `BDMV` structure, renders as the empty string — matching [`run_report`]'s
/// resilient-open absence path.
#[must_use]
pub fn run_iso_report(reader: Box<dyn IsoReader>) -> String {
    let Ok(source) = UdfSource::open_resilient(reader) else { return String::new() };
    render_disc(&source.root(), RenderOptions::default(), &mut |_| {}).unwrap_or_default()
}

/// The in-memory entry point: feed it BDMV bytes (the six `u32`-BE
/// framed sections — see the module-level docs), get back the classic report.
///
/// Runs the **measured** scan (M2TS demux + per-stream/per-chapter statistics),
/// identical to `bdinfo-rs <disc> --whole`, all inside the WebAssembly sandbox.
#[wasm_bindgen]
#[must_use]
pub fn scan_report(data: &[u8]) -> String {
    run_report(data)
}

/// The structural-scan entry point for the whole disc model: a
/// `webkitdirectory`-selected BDMV folder in, a [`Disc`] out.
///
/// Runs only the **structural** scan — no packet demux, so it reads the
/// playlist and clip metadata rather than the multi-GB stream files — and
/// returns everything the report prints that a metadata scan can know, plus
/// every column of the selection table.
///
/// `disc.measured` is false: the bitrates, packet counts and chapter rates are
/// zero because nothing measured them.
///
/// `disc.playlists` holds every playlist on the disc, each carrying the rules
/// that withhold it in `hiddenBy`, so a caller renders the standard selection
/// table by keeping the playlists whose `hiddenBy` is empty and re-applies
/// either rule without scanning again. `short_playlist_seconds` sets the length
/// under which a playlist counts as short, `0` leaving no playlist short;
/// absent, negative or non-finite means the 20 s default.
///
/// # Errors
/// As [`scan_files`]: `paths`/`files` length mismatch, a non-`File` entry, an
/// incoherent selection, or no readable Blu-ray structure.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn inspect_files(
    paths: Vec<String>,
    files: js_sys::Array,
    short_playlist_seconds: Option<f64>,
) -> Result<Disc, JsValue> {
    let root = build_web_tree(&paths, &files)?;
    let report = BdRom::open_resilient(&root, ScanMode::Metadata)
        .map_err(|err| JsValue::from_str(&format!("no readable Blu-ray structure ({err})")))?;
    Ok(Disc::from_scan(
        &report.bdrom,
        &report.errors,
        false,
        &classification_filter(short_playlist_seconds),
    ))
}

/// The structural-scan `.iso` entry point for the whole disc model: a single
/// OS-picked Blu-ray `.iso` `File` in, a [`Disc`] out.
///
/// Opens the image through the core read-only UDF 2.50 reader ([`UdfSource`])
/// instead of a `(relativePath, File)` list, then behaves exactly as
/// [`inspect_files`] — same structural scan, same `measured: false`, same
/// meaning for `short_playlist_seconds`.
///
/// # Errors
/// Returns a `JsValue` if the image is not a readable UDF `.iso`, or holds no
/// readable Blu-ray structure.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn inspect_iso(
    file: web_sys::File,
    short_playlist_seconds: Option<f64>,
) -> Result<Disc, JsValue> {
    let length = file.size() as u64;
    let source = UdfSource::open_resilient(Box::new(WebIso { file, length }))
        .map_err(|err| JsValue::from_str(&format!("not a readable Blu-ray .iso ({err})")))?;
    let report = BdRom::open_resilient(&source.root(), ScanMode::Metadata)
        .map_err(|err| JsValue::from_str(&format!("no readable Blu-ray structure ({err})")))?;
    Ok(Disc::from_scan(
        &report.bdrom,
        &report.errors,
        false,
        &classification_filter(short_playlist_seconds),
    ))
}

/// Renders the classic disc report from a [`Disc`] — no media, no rescan.
///
/// Hand back a disc a measured scan produced and the report comes out again,
/// with whichever optional sections `options` asks for. Omitting an option — or
/// the whole object — renders its section, so `renderReport(disc)` alone
/// reproduces the report the scan itself returned, byte for byte. This is what
/// re-rendering with different sections costs: nothing but the render.
///
/// The options that govern what a scan reads are ignored here: nothing is read.
///
/// The disc model carries every value the report prints, so this is a render
/// rather than an approximation — but it is the only supported way to turn a
/// [`Disc`] into report text. Formatting the model by hand produces something
/// that merely resembles the locked format.
///
/// The playlists print in the disc's presentation order — the standard
/// `--whole` set, grouped by shared clip files and longest first, the same
/// order a whole-disc scan renders. Any disc renders, including one whose
/// values were never measured: a disc from a by-name scan holds every playlist
/// but measured values only for the ones that scan named, and one from
/// [`inspect_files`] holds none at all, so both print the rest at zero.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
#[must_use]
pub fn render_report(disc: Disc, options: Option<ScanOptions>) -> String {
    let (bdrom, errors) = disc.into_scan();
    let order = bdrom.presentation_order(&PlaylistFilter::default());
    text::render_with(&bdrom, &order, &errors, render_options(options.as_ref()))
}

/// The measured-scan entry point: a `webkitdirectory`-selected BDMV folder in,
/// the classic report **and** the scanned [`Disc`] out.
///
/// Runs the **full measured** scan — M2TS demux + per-stream/per-chapter
/// statistics, `run_packet_scan = true` — reading every file's bytes
/// synchronously at byte offsets through [`web_sys::FileReaderSync`]. This MUST
/// run in a Web Worker (see the module docs). Both results come from that one
/// demux, so a consumer that wants the report and the model never reads a
/// multi-GB disc twice.
///
/// When `on_progress` is supplied it is called as `(file, done, total)` after
/// each demux read. A non-empty `selection` names the playlists to measure (CLI
/// `--mpls` semantics — unfiltered, in order); an empty `selection` measures the
/// standard `--whole` set. `options` carries the rest: the optional report
/// sections, the short-playlist threshold `disc.playlists` is classified against
/// (exactly as in [`inspect_files`]), and whether a stream file whose read fails
/// partway keeps what was measured of it. Omitting an option, or the whole
/// object, is what the classic report is rendered from.
///
/// `result.disc.measured` is true: this scan measured the values, so a zero in
/// it is a genuine zero. Re-render `result.disc` with other sections through
/// [`render_report`] without scanning again.
///
/// # Errors
/// Returns a `JsValue` if `paths` and `files` differ in length, any `files`
/// entry is not a `File`, the paths do not form one coherent disc selection,
/// no readable Blu-ray structure is found (so a wrong folder pick is reported
/// rather than silently returning an empty report), or a non-empty `selection`
/// names no playlist on the disc (the CLI's "No matching playlists found on BD").
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn scan_files(
    paths: Vec<String>,
    files: js_sys::Array,
    selection: Vec<String>,
    on_progress: Option<js_sys::Function>,
    options: Option<ScanOptions>,
) -> Result<ScanResult, JsValue> {
    let root = build_web_tree(&paths, &files)?;
    let mut observe = |p: ScanProgress<'_>| notify(on_progress.as_ref(), &p);
    let scan = scan_root(&root, &selection, scan_options(options.as_ref()), &mut observe)?;
    Ok(measured_result(
        &scan,
        render_options(options.as_ref()),
        &classification_filter(options.and_then(|options| options.short_playlist_seconds)),
    ))
}

/// The measured-scan `.iso` entry point: a single OS-picked Blu-ray `.iso`
/// `File` in, the classic report **and** the scanned [`Disc`] out.
///
/// Opens the image through the core read-only UDF 2.50 reader ([`UdfSource`])
/// over the same windowed [`web_sys::FileReaderSync`] cursor ([`WebIso`] is the
/// [`IsoReader`] factory), so a multi-GB image is never staged in memory, then
/// behaves exactly as [`scan_files`] — one demux, both results, and the same
/// meaning for every parameter it shares.
///
/// # Errors
/// Returns a `JsValue` if the image is not a readable UDF `.iso`, no readable
/// Blu-ray structure is found inside it, or a non-empty `selection` names no
/// playlist on the disc (the CLI's "No matching playlists found on BD").
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn scan_iso(
    file: web_sys::File,
    selection: Vec<String>,
    on_progress: Option<js_sys::Function>,
    options: Option<ScanOptions>,
) -> Result<ScanResult, JsValue> {
    let length = file.size() as u64;
    let source = UdfSource::open_resilient(Box::new(WebIso { file, length }))
        .map_err(|err| JsValue::from_str(&format!("not a readable Blu-ray .iso ({err})")))?;
    let scan = scan_root(&source.root(), &selection, scan_options(options.as_ref()), &mut |p| {
        notify(on_progress.as_ref(), &p);
    })?;
    Ok(measured_result(
        &scan,
        render_options(options.as_ref()),
        &classification_filter(options.and_then(|options| options.short_playlist_seconds)),
    ))
}

#[cfg(test)]
mod tests {
    use std::io::{self, SeekFrom};

    use bdinfo_rs_core::vfs::mem::MemFile;

    use super::{
        MAX_TREE_DEPTH, RenderOptions, TreeError, assemble_tree, framed_tree, path_components,
        read_window, seek_target,
    };

    /// Parses path strings into the `(components, id)` entries `assemble_tree`
    /// takes, tagging each file with its index so placement stays checkable.
    fn entries<'a>(paths: &[&'a str]) -> Vec<(Vec<&'a str>, usize)> {
        paths.iter().enumerate().map(|(i, path)| (path_components(path), i)).collect()
    }

    /// Builds the `(components, MemFile)` entries `assemble_tree` takes from
    /// `(path, bytes)` pairs, deriving each file's name and parent directory
    /// from its path.
    fn mem_entries<'a>(files: &[(&'a str, &[u8])]) -> Vec<(Vec<&'a str>, MemFile)> {
        files
            .iter()
            .map(|&(path, data)| {
                let (dir, name) = path.rsplit_once('/').expect("a file path has a directory");
                (path_components(path), MemFile::new(dir, name, data.to_vec()))
            })
            .collect()
    }

    /// The committed Big Buck Bunny fixture framed into the six `u32`-BE sections
    /// the in-memory path expects (the bytes the parity golden is built from).
    ///
    /// Reached from the mirror's mapping tests too, which scan the same
    /// fixture — hence `pub` inside this private module.
    pub fn fixture_blob() -> Vec<u8> {
        const INDEX: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/index.bdmv");
        const MOVIE: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/MovieObject.bdmv");
        const MPLS: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/PLAYLIST/00000.mpls");
        const CLPI: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/CLIPINF/00000.clpi");
        const M2TS: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/STREAM/00000.m2ts");
        let mut blob = Vec::new();
        for section in [INDEX, MOVIE, MPLS, CLPI, M2TS, [].as_slice()] {
            blob.extend_from_slice(&(section.len() as u32).to_be_bytes());
            blob.extend_from_slice(section);
        }
        blob
    }

    #[test]
    fn path_components_splits_on_both_separators_and_drops_empties() {
        assert_eq!(path_components("BDMV/PLAYLIST/00000.mpls"), ["BDMV", "PLAYLIST", "00000.mpls"]);
        assert_eq!(path_components("BDMV\\STREAM\\00000.m2ts"), ["BDMV", "STREAM", "00000.m2ts"]);
        assert_eq!(path_components("//a///b//"), ["a", "b"]);
        assert!(path_components("").is_empty());
    }

    #[test]
    fn assemble_wraps_a_bdmv_rooted_selection() {
        // Picking the BDMV folder itself yields paths rooted at BDMV; the
        // builder wraps it in a synthetic disc root so the core's walk-up
        // resolves, with BDMV as the root's first child.
        let tree = assemble_tree(entries(&["BDMV/index.bdmv", "BDMV/PLAYLIST/00000.mpls"]))
            .expect("assemble a BDMV-rooted selection");
        assert_eq!(tree.name, "WASMDISC");
        assert_eq!(tree.dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["BDMV"]);
    }

    #[test]
    fn assemble_keeps_a_disc_rooted_selection() {
        // A disc-root pick already wraps BDMV, so its folder name is kept as the
        // disc label verbatim.
        let tree = assemble_tree(entries(&["MyDisc/BDMV/index.bdmv"]))
            .expect("assemble a disc-rooted selection");
        assert_eq!(tree.name, "MyDisc");
        assert_eq!(tree.dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["BDMV"]);
    }

    #[test]
    fn assemble_rejects_an_incoherent_selection() {
        // `Node` is not `Debug`, so compare via `.err()` rather than `expect_err`.
        assert_eq!(
            assemble_tree(entries(&["A/BDMV/x", "B/BDMV/y"])).err(),
            Some(TreeError::MixedRoots("A".to_owned(), "B".to_owned()))
        );
        assert_eq!(
            assemble_tree(entries(&["loose.mpls"])).err(),
            Some(TreeError::BareFile("loose.mpls".to_owned()))
        );
        let empty: Vec<(Vec<&str>, usize)> = Vec::new();
        assert_eq!(assemble_tree(empty).err(), Some(TreeError::Empty));
    }

    #[test]
    fn tree_error_messages_describe_each_variant() {
        // `message` is consumed natively only here: its `From<TreeError> for
        // JsValue` caller is wasm32-only, so this is the test that keeps it both
        // live and covered on the native build.
        assert!(TreeError::BareFile("loose.mpls".to_owned()).message().contains("loose.mpls"));
        assert!(
            TreeError::MixedRoots("A".to_owned(), "B".to_owned())
                .message()
                .contains("more than one root")
        );
        assert_eq!(TreeError::Empty.message(), "no files to scan");
    }

    #[test]
    fn assemble_skips_entries_with_no_path_components() {
        // An entry whose path has no components (e.g. "") is skipped in both the
        // root-scan and the insert pass; a coherent sibling still assembles. The
        // mix is needed: the empty entry exercises the skip, the real one keeps
        // the run past the empty-selection guard so the second skip is reached.
        let tree = assemble_tree(entries(&["", "DISC/BDMV/index.bdmv"]))
            .expect("the empty-path entry is skipped, the real one assembles");
        assert_eq!(tree.name, "DISC");
        assert_eq!(tree.dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["BDMV"]);
    }

    #[test]
    fn a_bdmv_rooted_selection_renders_like_the_canonical_framing() {
        use super::render_disc;

        // The committed fixture's files — the same bytes the parity golden is
        // built from.
        const INDEX: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/index.bdmv");
        const MOVIE: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/MovieObject.bdmv");
        const MPLS: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/PLAYLIST/00000.mpls");
        const CLPI: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/CLIPINF/00000.clpi");
        const M2TS: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/STREAM/00000.m2ts");

        // The canonical in-memory framing (`WASMDISC`-rooted) the golden pins.
        let mut blob = Vec::new();
        for section in [INDEX, MOVIE, MPLS, CLPI, M2TS, [].as_slice()] {
            let len = u32::try_from(section.len()).expect("fixture sections are < 2^32 bytes");
            blob.extend_from_slice(&len.to_be_bytes());
            blob.extend_from_slice(section);
        }
        let framed = render_disc(&framed_tree(&blob), RenderOptions::default(), &mut |_| {})
            .expect("framed render");

        // The same disc handed over as a `webkitdirectory` pick of the BDMV
        // folder itself: the wrapper root makes it render identically.
        let tree = assemble_tree(mem_entries(&[
            ("BDMV/index.bdmv", INDEX),
            ("BDMV/MovieObject.bdmv", MOVIE),
            ("BDMV/PLAYLIST/00000.mpls", MPLS),
            ("BDMV/CLIPINF/00000.clpi", CLPI),
            ("BDMV/STREAM/00000.m2ts", M2TS),
        ]))
        .expect("assemble the BDMV-rooted selection");
        let from_bdmv =
            render_disc(&tree, RenderOptions::default(), &mut |_| {}).expect("BDMV-rooted render");

        assert_eq!(framed, from_bdmv, "a BDMV-rooted pick must render like the canonical framing");
    }

    #[test]
    fn the_render_drops_a_short_playlist_like_whole() {
        use super::render_disc;

        const INDEX: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/index.bdmv");
        const MOVIE: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/MovieObject.bdmv");
        const MPLS: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/PLAYLIST/00000.mpls");
        const CLPI: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/CLIPINF/00000.clpi");
        const M2TS: &[u8] =
            include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/STREAM/00000.m2ts");

        // A second playlist over the same clip, patched to ~10 s so the `--whole`
        // filter (`PlaylistFilter::default`) drops it: the first PlayItem's
        // OUT_time is a u32-BE at file offset 86 (IN_time is 27_000_000 at 82).
        let mut short = MPLS.to_vec();
        short
            .get_mut(86..90)
            .expect("the fixture playlist has an OUT_time at offset 86")
            .copy_from_slice(&(27_000_000_u32 + 45_000 * 10).to_be_bytes());

        let tree = assemble_tree(mem_entries(&[
            ("DISC/BDMV/index.bdmv", INDEX),
            ("DISC/BDMV/MovieObject.bdmv", MOVIE),
            ("DISC/BDMV/PLAYLIST/00000.mpls", MPLS),
            ("DISC/BDMV/PLAYLIST/00001.mpls", &short),
            ("DISC/BDMV/CLIPINF/00000.clpi", CLPI),
            ("DISC/BDMV/STREAM/00000.m2ts", M2TS),
        ]))
        .expect("assemble the two-playlist disc");
        let report = render_disc(&tree, RenderOptions::default(), &mut |_| {}).expect("render");

        assert!(report.contains("00000.MPLS"), "the 30s feature playlist must be kept");
        assert!(
            !report.contains("00001.MPLS"),
            "the 10s playlist must be dropped by the --whole filter"
        );
    }

    #[test]
    fn assemble_drops_a_pathologically_deep_path_without_overflowing() {
        // A path far deeper than any real disc is dropped rather than grown into
        // the tree — and the iterative descent returns instead of recursing to a
        // stack overflow.
        let deep = std::iter::once("DISC".to_owned())
            .chain((0..MAX_TREE_DEPTH + 100).map(|i| format!("d{i}")))
            .chain(std::iter::once("file.m2ts".to_owned()))
            .collect::<Vec<_>>()
            .join("/");
        let tree = assemble_tree(vec![(path_components(&deep), 0_usize)]).expect("assemble");
        assert_eq!(tree.name, "DISC");
        assert!(tree.dirs.is_empty(), "the over-deep file should be dropped");
    }

    #[test]
    fn assemble_keeps_a_path_exactly_at_the_depth_cap() {
        // A chain exactly MAX_TREE_DEPTH deep is kept; the over-deep test above
        // drops one past it. Together they pin the exact `chain.len() > cap`
        // boundary (a `>=` would wrongly drop the at-cap path).
        let at_cap = std::iter::once("DISC".to_owned())
            .chain((0..MAX_TREE_DEPTH).map(|i| format!("d{i}")))
            .chain(std::iter::once("file.m2ts".to_owned()))
            .collect::<Vec<_>>()
            .join("/");
        let kept =
            assemble_tree(vec![(path_components(&at_cap), 0_usize)]).expect("assemble at cap");
        assert!(!kept.dirs.is_empty(), "a path at the depth cap must be kept");
    }

    #[test]
    fn node_walk_matches_patterns_with_and_without_recursion() {
        use bdinfo_rs_core::vfs::{BdDir, SearchOption};

        use super::Node;

        let mut stream = Node::dir("STREAM", "DISC/BDMV/STREAM");
        stream.files.push(MemFile::new("DISC/BDMV/STREAM", "00000.m2ts", vec![0_u8; 4]));
        let mut root = Node::dir("BDMV", "DISC/BDMV");
        root.files.push(MemFile::new("DISC/BDMV", "index.bdmv", vec![0_u8; 2]));
        root.dirs.push(stream);

        assert_eq!(root.name(), "BDMV");
        assert_eq!(root.full_name(), "DISC/BDMV");
        assert!(root.parent().is_none());
        assert_eq!(root.get_files().expect("files").len(), 1);
        assert_eq!(root.get_directories().expect("dirs").len(), 1);

        // TopDirectoryOnly does not descend into STREAM; AllDirectories does
        // (covers `collect_pattern`'s `if recurse` both ways).
        assert_eq!(root.get_files_pattern("*.m2ts").expect("shallow").len(), 0);
        assert_eq!(
            root.get_files_pattern_option("*.m2ts", SearchOption::AllDirectories)
                .expect("deep")
                .len(),
            1
        );
        // A top-level match is found by either search.
        assert_eq!(root.get_files_pattern("*.bdmv").expect("top match").len(), 1);
    }

    #[test]
    fn render_disc_renders_then_errors_without_bdmv() {
        use bdinfo_rs_core::bdrom::disc::ScanProgress;

        use super::{Node, render_disc};

        // One progress sink — a fn pointer, so it is generic over the progress
        // lifetime (a shared closure would pin it and fail to type-check across
        // the two calls). The good render drives it so its body is covered; the
        // unopenable tree (no BDMV/CLIPINF/PLAYLIST) then makes render_disc hit
        // the `?` early-return, the arm the parity `Ok` flow never reaches.
        let mut sink: for<'a> fn(ScanProgress<'a>) = |_| {};
        render_disc(&framed_tree(&fixture_blob()), RenderOptions::default(), &mut sink)
            .expect("the fixture opens");
        let empty: Node<MemFile> = Node::dir("EMPTY", "EMPTY");
        assert!(render_disc(&empty, RenderOptions::default(), &mut sink).is_err());
    }

    #[test]
    fn one_measured_scan_yields_both_the_report_and_the_disc() {
        use bdinfo_rs_core::bdrom::disc::ScanProgress;

        use super::{CoreScanOptions, measured_result, scan_whole};

        /// The pinned report for the fixture disc — what the report-only
        /// exports render from the same scan.
        const GOLDEN: &[u8] = include_bytes!("../tests/golden_report.txt");

        let mut sink: for<'a> fn(ScanProgress<'a>) = |_| {};
        let scan = scan_whole(&framed_tree(&fixture_blob()), CoreScanOptions::default(), &mut sink)
            .expect("the fixture opens");
        // One scan, both outputs: the report is the pinned bytes and the disc
        // is the same scan mirrored, neither derived from the other.
        let result =
            measured_result(&scan, RenderOptions::default(), &super::classification_filter(None));
        assert_eq!(result.report.as_bytes(), GOLDEN);
        assert!(result.disc.measured, "the packet scan ran, so the values were measured");
        assert_eq!(result.disc.volume_label, "WASMDISC");
    }

    /// An options object with every option left out — the base each test below
    /// sets the one option it is about on.
    fn no_options() -> super::ScanOptions {
        super::ScanOptions {
            stream_diagnostics: None,
            quick_summary: None,
            short_playlist_seconds: None,
            keep_partial: None,
        }
    }

    #[test]
    fn an_omitted_report_option_leaves_its_section_on() {
        use super::{ScanOptions, render_options};

        // No object at all and an object asking for nothing are the same call.
        assert_eq!(render_options(None), RenderOptions::default());
        assert_eq!(render_options(Some(&no_options())), RenderOptions::default());
        assert_eq!(
            render_options(Some(&ScanOptions { stream_diagnostics: Some(false), ..no_options() })),
            RenderOptions { stream_diagnostics: false, quick_summary: true }
        );
        assert_eq!(
            render_options(Some(&ScanOptions { quick_summary: Some(false), ..no_options() })),
            RenderOptions { stream_diagnostics: true, quick_summary: false }
        );
        assert_eq!(
            render_options(Some(&ScanOptions {
                stream_diagnostics: Some(true),
                quick_summary: Some(true),
                ..no_options()
            })),
            RenderOptions::default()
        );
    }

    #[test]
    fn an_omitted_keep_partial_keeps_a_partially_read_stream_file() {
        use super::{ScanOptions, scan_options};

        // Absent either way — no object, or an object that leaves it out.
        assert!(scan_options(None).keep_partial);
        assert!(scan_options(Some(&no_options())).keep_partial);
        assert!(
            !scan_options(Some(&ScanOptions { keep_partial: Some(false), ..no_options() }))
                .keep_partial
        );
        assert!(
            scan_options(Some(&ScanOptions { keep_partial: Some(true), ..no_options() }))
                .keep_partial
        );
    }

    #[test]
    fn run_iso_report_renders_the_iso_golden_and_empties_a_bad_image() {
        use std::io::Cursor;
        use std::sync::Arc;

        use bdinfo_rs_core::vfs::ReadSeek;
        use bdinfo_rs_core::vfs::udf::source::IsoReader;

        use super::run_iso_report;

        /// A trivially `Send + Sync` in-memory [`IsoReader`] over a shared byte
        /// buffer — the `.iso` analogue of the in-memory file backend, proving
        /// the UDF-reader → report wiring with no `web_sys` (the browser
        /// `WebIso` is irreducible glue, held by the parity tests instead).
        #[derive(Debug)]
        struct MemIso(Arc<[u8]>);
        impl IsoReader for MemIso {
            fn open(&self) -> std::io::Result<Box<dyn ReadSeek>> {
                Ok(Box::new(Cursor::new(Arc::clone(&self.0))))
            }
        }

        // The committed BigBuckBunny `.iso` the native CLI e2e test scans, with
        // its pinned golden (UDF volume label `Blu-Ray`). Read through the UDF
        // reader, the report must be byte-identical to what `bdinfo-rs <disc>.iso`
        // writes — the same render path the folder input renders through.
        const ISO: &[u8] = include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny.iso");
        const ISO_GOLDEN: &[u8] = include_bytes!("../../bdinfo-rs/tests/fixtures/golden/iso.txt");

        let report = run_iso_report(Box::new(MemIso(Arc::from(ISO))));
        assert_eq!(report.as_bytes(), ISO_GOLDEN, "the .iso wiring must match the native golden");

        // A buffer that is not a UDF volume opens to nothing — the resilient-open
        // absence path (the `else { String::new() }` arm).
        assert!(
            run_iso_report(Box::new(MemIso(Arc::from(vec![0_u8; 1 << 16])))).is_empty(),
            "a non-UDF image renders as the empty string"
        );
    }

    #[test]
    fn read_window_is_none_on_empty_buffer_or_at_eof() {
        assert_eq!(read_window(0, 0, 100), None); // empty buf at start (the `||`, not `&&`)
        assert_eq!(read_window(50, 0, 100), None); // empty buf mid-file
        assert_eq!(read_window(100, 8, 100), None); // pos == len (boundary: `>=`, not `>`)
        assert_eq!(read_window(101, 8, 100), None); // pos > len
    }

    #[test]
    fn read_window_clamps_to_eof() {
        assert_eq!(read_window(0, 8, 100), Some((0, 8))); // fully inside
        assert_eq!(read_window(10, 8, 100), Some((10, 18)));
        assert_eq!(read_window(99, 8, 100), Some((99, 100))); // one byte left
        assert_eq!(read_window(96, 8, 100), Some((96, 100))); // crosses EOF -> clamped to len
    }

    #[test]
    fn seek_target_resolves_each_anchor() {
        let ok = |from| seek_target(from, 5, 100).expect("a non-negative target");
        assert_eq!(ok(SeekFrom::Start(0)), 0);
        assert_eq!(ok(SeekFrom::Start(42)), 42);
        assert_eq!(ok(SeekFrom::End(0)), 100);
        assert_eq!(ok(SeekFrom::End(-10)), 90);
        assert_eq!(ok(SeekFrom::End(10)), 110); // past EOF is allowed
        assert_eq!(ok(SeekFrom::Current(0)), 5);
        assert_eq!(ok(SeekFrom::Current(20)), 25);
        assert_eq!(ok(SeekFrom::Current(-5)), 0);
    }

    #[test]
    fn seek_target_allows_zero_but_rejects_before_start() {
        // Landing exactly on byte 0 is valid (boundary: `< 0`, not `<= 0`).
        assert_eq!(seek_target(SeekFrom::End(-100), 0, 100).expect("zero is valid"), 0);
        assert_eq!(seek_target(SeekFrom::Current(-5), 5, 100).expect("zero is valid"), 0);
        for from in [SeekFrom::End(-101), SeekFrom::Current(-6)] {
            let err = seek_target(from, 5, 100).expect_err("a pre-start target must error");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn seek_target_saturates_a_past_u64_target() {
        // try_from succeeds at exactly u64::MAX (the success-at-max path)...
        assert_eq!(seek_target(SeekFrom::Start(u64::MAX), 0, 0).expect("max"), u64::MAX);
        // ...and the i128 sum can exceed u64::MAX, exercising the unwrap_or(u64::MAX) fallback.
        assert_eq!(
            seek_target(SeekFrom::End(i64::MAX), u64::MAX, u64::MAX).expect("sat"),
            u64::MAX
        );
        assert_eq!(seek_target(SeekFrom::Current(i64::MAX), u64::MAX, 0).expect("sat"), u64::MAX);
    }

    /// CLI-parity selection helpers: the classification threshold and the
    /// by-name measured scan, native-tested.
    mod selection {
        use bdinfo_rs_core::bdrom::disc::{PlaylistSummary, fixtures};
        use bdinfo_rs_core::bdrom::order::PlaylistFilter;

        use crate::classification_filter;

        /// A `PlaylistSummary` carrying just what the selection/table helpers
        /// read: name, length, the two file sizes, and its clip names (each
        /// clip as long as the playlist).
        fn sample_playlist(
            name: &str,
            total_length: f64,
            file_size: u64,
            interleaved_file_size: u64,
            clips: &[&str],
        ) -> PlaylistSummary {
            PlaylistSummary {
                file_size,
                interleaved_file_size,
                ..fixtures::playlist(name, total_length, fixtures::clips(clips, total_length))
            }
        }

        #[test]
        fn classification_filter_takes_the_threshold_and_falls_back_to_twenty_seconds() {
            // Any finite threshold from zero up is used as given; a value no
            // caller can have meant — absent, negative, non-finite — leaves the
            // default 20 s in force, so a caller that does not set one gets the
            // classic classification. Whole filters are compared, so the two
            // switches the exports do not offer are pinned at their defaults.
            for (short_seconds, in_force) in [
                (Some(5.0), 5.0),
                (Some(0.5), 0.5),
                (Some(0.0), 0.0),
                (None, 20.0),
                (Some(-1.0), 20.0),
                (Some(f64::NAN), 20.0),
                (Some(f64::INFINITY), 20.0),
            ] {
                assert_eq!(
                    classification_filter(short_seconds),
                    PlaylistFilter {
                        filter_short_playlists: true,
                        filter_looping_playlists: true,
                        short_playlist_seconds: in_force,
                    },
                    "{short_seconds:?}"
                );
            }
        }

        #[test]
        fn a_lowered_threshold_reclassifies_the_playlists_between_it_and_the_default() {
            // A 5 s playlist is short under the default 20 s threshold and not
            // short under a 5 s one, which is the whole of what the threshold
            // changes — the looping rule does not read it.
            let short = sample_playlist("00002.MPLS", 5.0, 100, 0, &["C.M2TS"]);
            let looping = PlaylistSummary {
                has_loops: true,
                ..sample_playlist("00003.MPLS", 5.0, 0, 0, &[])
            };
            assert_eq!(
                classification_filter(None).classify(&short),
                [bdinfo_rs_core::bdrom::order::HiddenRule::Short]
            );
            assert!(classification_filter(Some(5.0)).classify(&short).is_empty());
            assert_eq!(
                classification_filter(Some(5.0)).classify(&looping),
                [bdinfo_rs_core::bdrom::order::HiddenRule::Looping]
            );
        }

        #[test]
        fn a_zero_threshold_leaves_no_playlist_short() {
            // Nothing is strictly shorter than zero seconds, so a zero
            // threshold is how a caller switches the short rule off — the
            // looping rule keeps judging independently.
            let brief = sample_playlist("00002.MPLS", 1.0, 100, 0, &["C.M2TS"]);
            let looping = PlaylistSummary {
                has_loops: true,
                ..sample_playlist("00003.MPLS", 1.0, 0, 0, &[])
            };
            assert!(classification_filter(Some(0.0)).classify(&brief).is_empty());
            assert_eq!(
                classification_filter(Some(0.0)).classify(&looping),
                [bdinfo_rs_core::bdrom::order::HiddenRule::Looping]
            );
        }

        #[test]
        fn scan_selection_measures_only_the_named_playlists() {
            use bdinfo_rs_core::bdrom::disc::ScanProgress;
            use bdinfo_rs_core::report::text::RenderOptions;
            use bdinfo_rs_core::vfs::mem::MemFile;

            use super::mem_entries;
            use crate::{CoreScanOptions, Node, assemble_tree, scan_selection};

            const INDEX: &[u8] =
                include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/index.bdmv");
            const MOVIE: &[u8] =
                include_bytes!("../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/MovieObject.bdmv");
            const MPLS: &[u8] = include_bytes!(
                "../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/PLAYLIST/00000.mpls"
            );
            const CLPI: &[u8] = include_bytes!(
                "../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/CLIPINF/00000.clpi"
            );
            const M2TS: &[u8] = include_bytes!(
                "../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/STREAM/00000.m2ts"
            );

            // A second playlist over the same clip, patched to ~10 s — `--whole`
            // would drop it, but a by-name selection keeps it (unfiltered).
            let mut short = MPLS.to_vec();
            short
                .get_mut(86..90)
                .expect("the fixture playlist has an OUT_time at offset 86")
                .copy_from_slice(&(27_000_000_u32 + 45_000 * 10).to_be_bytes());

            let tree = assemble_tree(mem_entries(&[
                ("DISC/BDMV/index.bdmv", INDEX),
                ("DISC/BDMV/MovieObject.bdmv", MOVIE),
                ("DISC/BDMV/PLAYLIST/00000.mpls", MPLS),
                ("DISC/BDMV/PLAYLIST/00001.mpls", &short),
                ("DISC/BDMV/CLIPINF/00000.clpi", CLPI),
                ("DISC/BDMV/STREAM/00000.m2ts", M2TS),
            ]))
            .expect("assemble the two-playlist disc");

            // One progress sink for every call — a fn pointer so it is generic
            // over the progress lifetime; the real demux below drives it (so it
            // is covered), and the unopenable case reuses it without firing it.
            let mut sink: for<'a> fn(ScanProgress<'a>) = |_| {};
            let whole = RenderOptions::default();

            // Selecting only the feature renders it and drops the short sibling.
            let feature = scan_selection(
                &tree,
                &["00000.MPLS".to_owned()],
                CoreScanOptions::default(),
                &mut sink,
            )
            .expect("scan the feature")
            .render(whole);
            assert!(feature.contains("00000.MPLS"), "the selected feature must be rendered");
            assert!(!feature.contains("00001.MPLS"), "an unselected playlist must not be rendered");

            // Selecting the short playlist by name keeps it — unfiltered, unlike
            // `--whole`; and only the named playlist is rendered.
            let short_only =
                scan_selection(&tree, &["00001".to_owned()], CoreScanOptions::default(), &mut sink)
                    .expect("scan the short")
                    .render(whole);
            assert!(short_only.contains("00001.MPLS"), "a by-name short playlist is kept");
            assert!(!short_only.contains("00000.MPLS"), "only the named playlist is rendered");

            // An unopenable tree (no BDMV) surfaces the structural open's error,
            // formatted through SelectionError's Display (the same text the wasm
            // `scan_files` export emits — which is what reads the inner error).
            let empty: Node<MemFile> = Node::dir("EMPTY", "EMPTY");
            let open_err = scan_selection(
                &empty,
                &["00000.MPLS".to_owned()],
                CoreScanOptions::default(),
                &mut sink,
            )
            .expect_err("a structure with no BDMV must error");
            assert!(
                open_err.to_string().starts_with("no readable Blu-ray structure"),
                "the open failure formats as a structure error"
            );

            // A non-empty selection that names no playlist on the disc errors like
            // the CLI's `--mpls`, rather than rendering a header-only report.
            let no_match = scan_selection(
                &tree,
                &["99999.MPLS".to_owned()],
                CoreScanOptions::default(),
                &mut sink,
            )
            .expect_err("an all-unknown selection must error");
            assert_eq!(no_match.to_string(), "No matching playlists found on BD");
        }
    }

    // The reader math is panic-safety-critical, so amplify the unit cases with
    // property tests. proptest's backend does not build for wasm32, so these run
    // only on the native build.
    #[cfg(not(target_arch = "wasm32"))]
    mod prop {
        use std::io::{self, SeekFrom};

        use proptest::prelude::*;

        use crate::{read_window, seek_target};

        proptest! {
            #[test]
            fn read_window_is_a_bounded_nonempty_slice(pos: u64, buf_len in 0_usize..=4096, len: u64) {
                match read_window(pos, buf_len, len) {
                    None => prop_assert!(buf_len == 0 || pos >= len),
                    Some((start, end)) => {
                        prop_assert!(buf_len != 0 && pos < len);
                        prop_assert_eq!(start, pos);
                        prop_assert!(start < end); // non-empty
                        prop_assert!(end <= len); // never past EOF
                        let want = u64::try_from(buf_len).unwrap_or(u64::MAX);
                        prop_assert!(end.saturating_sub(start) <= want); // <= requested
                        // ...and took as much as possible (all the way, or hit EOF).
                        prop_assert!(end == len || end.saturating_sub(start) == want);
                    }
                }
            }

            #[test]
            fn seek_target_errs_iff_target_is_negative(
                pos: u64,
                len: u64,
                kind in 0_u8..3,
                off: i64,
                start: u64,
            ) {
                let from = match kind {
                    0 => SeekFrom::Start(start),
                    1 => SeekFrom::End(off),
                    _ => SeekFrom::Current(off),
                };
                let target: i128 = match from {
                    SeekFrom::Start(n) => i128::from(n),
                    SeekFrom::End(n) => i128::from(len).wrapping_add(i128::from(n)),
                    SeekFrom::Current(n) => i128::from(pos).wrapping_add(i128::from(n)),
                };
                match seek_target(from, pos, len) {
                    Err(e) => {
                        prop_assert!(target < 0);
                        prop_assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
                    }
                    Ok(got) => {
                        prop_assert!(target >= 0);
                        prop_assert_eq!(got, u64::try_from(target).unwrap_or(u64::MAX));
                    }
                }
            }
        }
    }
}
