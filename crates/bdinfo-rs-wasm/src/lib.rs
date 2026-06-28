//! WebAssembly browser bindings for the bdinfo-rs Blu-ray analyzer.
//!
//! This crate exposes the library's whole **measured** scan pipeline to the
//! browser. There are two entry points onto the very same render path:
//!
//! - [`scan_report`] — the Phase 1 in-memory export: BDMV bytes are framed into a synthetic disc
//!   tree (six `u32`-BE sections), opened with [`BdRom::open_resilient`] (packet scan **on**), and
//!   rendered to the classic report. Used by the native ⇄ in-browser byte-parity test.
//! - [`scan_files`] — the Phase 2 streaming export: a `webkitdirectory`-selected BDMV folder
//!   arrives as a flat list of `(relativePath, File)` pairs. The files stay on disk; their bytes
//!   are read **synchronously** at byte offsets through [`web_sys::FileReaderSync`] (no JSPI, no
//!   Asyncify), so a multi-GB `*.m2ts` never has to fit in memory. The export runs inside a Web
//!   Worker (the only scope where `FileReaderSync` exists).
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
//! length-prefixed sections, assigned in fixed order to the synthetic disc's six
//! files — `index.bdmv`, `MovieObject.bdmv`, the playlist, the clip, the stream
//! file, and `META/DL/bdmt_eng.xml`. This mirrors the synthetic tree the
//! `parse_report` fuzz target builds, widened from `u16` to `u32` so a
//! real-scale `*.m2ts` stream file (megabytes) fits in a section. A missing or
//! truncated section leaves its file empty (the resilient-open absence path).

// `BdmvDir`/`SeekFrom` are named only by the web-path logic and the reader math
// (`assemble_tree`/`seek_target`) — tested natively, but absent from a native
// NON-test build, so gate them to where they live to stay dead-code-clean.
// `Read`/`Seek`/`JsCast`/`JsValue` are named only by the wasm32 browser glue.
#[cfg(any(target_arch = "wasm32", test))]
use std::collections::BTreeSet;
#[cfg(any(target_arch = "wasm32", test))]
use std::io::SeekFrom;
use std::io::{self, BufRead, BufReader, Cursor};
#[cfg(target_arch = "wasm32")]
use std::io::{Read, Seek};
use std::sync::Arc;

#[cfg(any(target_arch = "wasm32", test))]
use bdinfo_rs_core::bdrom::chapters::seconds_to_ticks;
#[cfg(any(target_arch = "wasm32", test))]
use bdinfo_rs_core::bdrom::disc::PlaylistSummary;
use bdinfo_rs_core::bdrom::disc::{BdRom, ScanProgress};
use bdinfo_rs_core::bdrom::order::PlaylistFilter;
#[cfg(any(target_arch = "wasm32", test))]
use bdinfo_rs_core::bdrom::order::presentation_groups;
#[cfg(any(target_arch = "wasm32", test))]
use bdinfo_rs_core::discovery::BdmvDir;
use bdinfo_rs_core::error::BdError;
use bdinfo_rs_core::report::text;
use bdinfo_rs_core::vfs::{BdDir, BdFile, ReadSeek, SearchOption};
use wasm_bindgen::prelude::wasm_bindgen;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{JsCast, JsValue};

/// The read-ahead window each [`WebReader`] fill pulls from `FileReaderSync` in
/// one go, so a front-to-back demux crosses the JS boundary once per MiB rather
/// than once per small parser read.
#[cfg(target_arch = "wasm32")]
const READ_WINDOW: usize = 1_048_576; // 1 MiB

/// A node in a synthetic disc tree — a directory holding sub-directories and
/// files of one concrete [`BdFile`] backend (`F`).
///
/// Both backends ([`MemFile`] in-memory, [`WebFile`] file-backed) read through
/// this one [`BdDir`] implementation, so the directory walk, glob matching, and
/// recursion behave identically whatever the bytes are backed by.
#[derive(Clone)]
struct Node<F> {
    name: String,
    full: String,
    dirs: Vec<Self>,
    files: Vec<F>,
}

impl<F> Node<F> {
    fn dir(name: &str, full: &str) -> Self {
        Self { name: name.to_owned(), full: full.to_owned(), dirs: Vec::new(), files: Vec::new() }
    }
}

/// ASCII case-insensitive glob: `*` = any run, `?` = any one byte. Mirrors the
/// core's [`fs::glob_ci`](bdinfo_rs_core) so file-backed input matches patterns
/// exactly like folder input does.
fn glob_match(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => {
            (0..=name.len()).any(|skip| name.get(skip..).is_some_and(|tail| glob_match(rest, tail)))
        }
        Some((b'?', rest)) => match name.split_first() {
            Some((_, tail)) => glob_match(rest, tail),
            None => false,
        },
        Some((c, rest)) => match name.split_first() {
            Some((n, tail)) => c.eq_ignore_ascii_case(n) && glob_match(rest, tail),
            None => false,
        },
    }
}

impl<F: BdFile + Clone + 'static> Node<F> {
    fn collect_pattern(&self, pattern: &str, recurse: bool, out: &mut Vec<Box<dyn BdFile>>) {
        for f in &self.files {
            if glob_match(pattern.as_bytes(), f.name().as_bytes()) {
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

    fn get_files(&self) -> io::Result<Vec<Box<dyn BdFile>>> {
        Ok(self.files.iter().map(|f| Box::new(f.clone()) as Box<dyn BdFile>).collect())
    }

    fn get_files_pattern(&self, pattern: &str) -> io::Result<Vec<Box<dyn BdFile>>> {
        self.get_files_pattern_option(pattern, SearchOption::TopDirectoryOnly)
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

/// The extension *including* the leading dot, e.g. `.mpls`; the empty string
/// when the name has no `.`.
fn extension_of(name: &str) -> &str {
    name.rfind('.').and_then(|i| name.get(i..)).unwrap_or("")
}

// ── in-memory backend (the `scan_report` framing path) ──────────────────────

/// An in-memory file node backed by a shared byte buffer.
#[derive(Clone)]
struct MemFile {
    name: String,
    full: String,
    data: Arc<[u8]>,
}

impl BdFile for MemFile {
    fn name(&self) -> &str {
        &self.name
    }

    fn full_name(&self) -> &str {
        &self.full
    }

    fn extension(&self) -> &str {
        extension_of(&self.name)
    }

    fn length(&self) -> u64 {
        self.data.len() as u64
    }

    fn is_dir(&self) -> bool {
        false
    }

    fn open_read(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(Cursor::new(Arc::clone(&self.data))))
    }

    fn open_text(&self) -> io::Result<Box<dyn BufRead>> {
        Ok(Box::new(BufReader::new(Cursor::new(Arc::clone(&self.data)))))
    }
}

fn mem_file(dir: &str, name: &str, data: Vec<u8>) -> MemFile {
    MemFile { name: name.to_owned(), full: format!("{dir}/{name}"), data: Arc::from(data) }
}

/// Splits `data` into up to six `u32`-BE length-prefixed sections (see the
/// module-level framing docs). Missing or truncated trailing sections yield
/// empty buffers.
fn split_sections(data: &[u8]) -> Vec<Vec<u8>> {
    let mut sections: Vec<Vec<u8>> = Vec::new();
    let mut rest = data;
    while sections.len() < 6 {
        let Some((len_bytes, tail)) = rest.split_first_chunk::<4>() else { break };
        let want = u32::from_be_bytes(*len_bytes) as usize;
        let take = want.min(tail.len());
        // `take <= tail.len()`, so `split_at` cannot panic. A truncated section
        // (`take < want`) consumes the rest of the buffer, so the next iteration
        // finds no further 4-byte prefix and the loop ends — no explicit break.
        let (head, next) = tail.split_at(take);
        sections.push(head.to_vec());
        rest = next;
    }
    sections
}

/// Builds the synthetic in-memory disc tree around the six framed sections.
fn build_tree(data: &[u8]) -> Node<MemFile> {
    let mut next = split_sections(data).into_iter();
    let mut take = || next.next().unwrap_or_default();

    let index = take();
    let movie_object = take();
    let mpls = take();
    let clpi = take();
    let m2ts = take();
    let xml = take();

    let mut playlist = Node::dir("PLAYLIST", "WASMDISC/BDMV/PLAYLIST");
    playlist.files.push(mem_file("WASMDISC/BDMV/PLAYLIST", "00000.mpls", mpls));
    let mut clipinf = Node::dir("CLIPINF", "WASMDISC/BDMV/CLIPINF");
    clipinf.files.push(mem_file("WASMDISC/BDMV/CLIPINF", "00000.clpi", clpi));
    let mut stream = Node::dir("STREAM", "WASMDISC/BDMV/STREAM");
    stream.files.push(mem_file("WASMDISC/BDMV/STREAM", "00000.m2ts", m2ts));
    let mut dl = Node::dir("DL", "WASMDISC/BDMV/META/DL");
    dl.files.push(mem_file("WASMDISC/BDMV/META/DL", "bdmt_eng.xml", xml));
    let mut meta = Node::dir("META", "WASMDISC/BDMV/META");
    meta.dirs.push(dl);

    let mut bdmv = Node::dir("BDMV", "WASMDISC/BDMV");
    bdmv.dirs = vec![playlist, clipinf, stream, meta];
    bdmv.files = vec![
        mem_file("WASMDISC/BDMV", "index.bdmv", index),
        mem_file("WASMDISC/BDMV", "MovieObject.bdmv", movie_object),
    ];

    let mut root = Node::dir("WASMDISC", "WASMDISC");
    root.dirs.push(bdmv);
    root
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
/// the off-by-one and EOF-clamp edges are exercised on the native (Tier-A)
/// build. `end` is clamped to `len`, so a window that would cross EOF is
/// shortened rather than over-reading the caller's `buf`.
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
/// [`WebReader`]; only metadata (name, full path, length) is held eagerly.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
struct WebFile {
    name: String,
    full: String,
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
    "WebFile's Send impl is unsound with wasm threads (+atomics): a web_sys::File moved across \
     workers is undefined behavior. Revisit the FileReaderSync seam before enabling threads."
);

#[cfg(target_arch = "wasm32")]
impl WebFile {
    fn open(&self) -> io::Result<WebReader> {
        let reader = web_sys::FileReaderSync::new().map_err(|e| {
            io::Error::other(format!(
                "FileReaderSync unavailable (run the scan in a Worker): {}",
                js_message(&e)
            ))
        })?;
        Ok(WebReader { file: self.file.clone(), reader, pos: 0, len: self.length })
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
        extension_of(&self.name)
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
        let web_file = WebFile { name: name.to_owned(), full: path.clone(), file, length };
        entries.push((comps, web_file));
    }

    assemble_tree(entries).map_err(JsValue::from)
}

// ── shared render path ──────────────────────────────────────────────────────

// ── selection (CLI parity) ──────────────────────────────────────────────────
//
// The browser surfaces the CLI's flow: a structural scan lists the playlists
// ([`list_playlists`]), the user picks some, and a measured scan reads only the
// picked playlists' stream files ([`scan_files`] with a selection). These
// helpers mirror the selection logic of the `bdinfo-rs` binary — which this
// crate cannot import — over the public core types, so the browser behaves
// exactly like `--mpls` / the picker. They are `cfg(any(wasm32, test))`: the
// wasm exports use them and the native test build covers + mutates them, while a
// native non-test build omits them (so neither tier shows dead code).

/// One selection-table row — the machine-readable form of the CLI's
/// `#`/Group/Playlist File/Length/Estimated Bytes table, for the browser to
/// render as a checklist. Measured bytes are omitted: a [`list_playlists`] scan
/// is structural, so they would always be `-`.
#[cfg(any(target_arch = "wasm32", test))]
struct PlaylistRow {
    /// 1-based position in the table — the handle the user picks.
    position: usize,
    /// Shared-clip group number (1-based), the CLI's `Group` column.
    group: usize,
    /// The playlist file name, e.g. `00000.MPLS`.
    name: String,
    /// `hh:mm:ss` total length, truncated like the CLI table.
    length: String,
    /// Estimated bytes — interleaved `*.ssif` size, else `*.m2ts` size, else
    /// `None` (the `-` cell).
    estimated_bytes: Option<u64>,
    /// Whether the playlist hides any stream (the CLI's `(*)` note).
    has_hidden_streams: bool,
}

/// The playlist table rows as `(group number, playlist index)` pairs in table
/// order: the standard filtered set (short and looping playlists dropped),
/// grouped by shared clips, each group longest-first. Mirrors the CLI's
/// `table_rows`.
#[cfg(any(target_arch = "wasm32", test))]
fn table_rows(playlists: &[PlaylistSummary]) -> Vec<(usize, usize)> {
    presentation_groups(playlists, &PlaylistFilter::default())
        .into_iter()
        .enumerate()
        .flat_map(|(group, members)| {
            members.into_iter().map(move |index| (group.saturating_add(1), index))
        })
        .collect()
}

/// `hh:mm:ss` from playlist seconds, truncated to the tick like the CLI table
/// (hours wrap at 24; no day component).
#[cfg(any(target_arch = "wasm32", test))]
fn table_length(seconds: f64) -> String {
    let total = seconds_to_ticks(seconds).max(0).checked_div(10_000_000).unwrap_or(0);
    let h = total.checked_div(3600).and_then(|h| h.checked_rem(24)).unwrap_or(0);
    let m = total.checked_div(60).and_then(|m| m.checked_rem(60)).unwrap_or(0);
    let s = total.checked_rem(60).unwrap_or(0);
    format!("{h:02}:{m:02}:{s:02}")
}

/// The estimated byte size shown for a playlist: the interleaved `*.ssif` size
/// when known, else the `*.m2ts` size, else `None`.
#[cfg(any(target_arch = "wasm32", test))]
const fn estimated_bytes(playlist: &PlaylistSummary) -> Option<u64> {
    if playlist.interleaved_file_size > 0 {
        Some(playlist.interleaved_file_size)
    } else if playlist.file_size > 0 {
        Some(playlist.file_size)
    } else {
        None
    }
}

/// Builds the selection-table rows over the standard filtered set.
#[cfg(any(target_arch = "wasm32", test))]
fn playlist_rows(playlists: &[PlaylistSummary]) -> Vec<PlaylistRow> {
    table_rows(playlists)
        .into_iter()
        .enumerate()
        .filter_map(|(position, (group, index))| {
            playlists.get(index).map(|playlist| PlaylistRow {
                position: position.saturating_add(1),
                group,
                name: playlist.name.clone(),
                length: table_length(playlist.total_length),
                estimated_bytes: estimated_bytes(playlist),
                has_hidden_streams: playlist.has_hidden_streams(),
            })
        })
        .collect()
}

/// Appends `value` to `out` as a JSON string body (the surrounding quotes are
/// the caller's), escaping the characters JSON requires.
#[cfg(any(target_arch = "wasm32", test))]
fn json_escape(value: &str, out: &mut String) {
    use std::fmt::Write as _;
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
}

/// Serializes the selection rows to a JSON array string — the [`list_playlists`]
/// return value the browser `JSON.parse`s into its checklist model.
#[cfg(any(target_arch = "wasm32", test))]
fn rows_to_json(rows: &[PlaylistRow]) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("[");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"position\":{},\"group\":{},\"name\":\"", row.position, row.group);
        json_escape(&row.name, &mut out);
        out.push_str("\",\"length\":\"");
        json_escape(&row.length, &mut out);
        out.push_str("\",\"estimatedBytes\":");
        match row.estimated_bytes {
            Some(bytes) => {
                let _ = write!(out, "{bytes}");
            }
            None => out.push_str("null"),
        }
        out.push_str(",\"hasHidden\":");
        out.push_str(if row.has_hidden_streams { "true" } else { "false" });
        out.push('}');
    }
    out.push(']');
    out
}

/// Normalizes a requested playlist name to the model's spelling: upper-cased,
/// with `.MPLS` appended when no extension was given. Mirrors the CLI.
#[cfg(any(target_arch = "wasm32", test))]
fn normalize_playlist_name(name: &str) -> String {
    let upper = name.to_ascii_uppercase();
    if upper.contains('.') { upper } else { format!("{upper}.MPLS") }
}

/// Resolves the requested names against the disc, in the given order, first
/// occurrence wins, unknown names skipped — the CLI's `--mpls` selection.
#[cfg(any(target_arch = "wasm32", test))]
fn named_selection(playlists: &[PlaylistSummary], requested: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for raw in requested {
        let name = normalize_playlist_name(raw);
        if playlists.iter().any(|playlist| playlist.name == name) && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The stream files a selection's packet scan reads: every clip of every
/// selected playlist.
#[cfg(any(target_arch = "wasm32", test))]
fn selection_stream_files(playlists: &[PlaylistSummary], selection: &[String]) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    for name in selection {
        if let Some(playlist) = playlists.iter().find(|playlist| &playlist.name == name) {
            files.extend(playlist.clips.iter().map(|clip| clip.name.clone()));
        }
    }
    files
}

/// The report's playlist order for a selection: the selection order mapped to
/// indices into the scanned disc's playlists.
#[cfg(any(target_arch = "wasm32", test))]
fn selection_order(playlists: &[PlaylistSummary], selection: &[String]) -> Vec<usize> {
    selection
        .iter()
        .filter_map(|name| playlists.iter().position(|playlist| &playlist.name == name))
        .collect()
}

/// Runs the **measured** scan over just the playlists named in `selection` and
/// renders the classic report in selection order — the browser equivalent of
/// `bdinfo-rs <disc> --mpls A,B`.
///
/// A cheap structural scan (no packet scan) resolves the names to their stream
/// files first; the measured scan is then narrowed to those files, so an
/// unselected (possibly multi-GB) playlist is never demuxed. Names match
/// unfiltered — a short or looping playlist the `--whole` filter drops is still
/// selectable by name.
///
/// # Errors
/// The [`BdError`] from either open when the structure is too damaged to scan.
#[cfg(any(target_arch = "wasm32", test))]
fn render_selection(
    root: &dyn BdDir,
    selection: &[String],
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<String, BdError> {
    let structural = BdRom::open_resilient(root, false)?;
    let names = named_selection(&structural.bdrom.playlists, selection);
    let files = selection_stream_files(&structural.bdrom.playlists, &names);
    // The measured scan re-opens the same tree, narrowed to the selected clips.
    // It locates the same `BDMV`/`CLIPINF`/`PLAYLIST` the structural open just
    // found, so it cannot hit the only hard error (`StructureNotFound`); on that
    // unreachable failure it degrades to the structural disc (zero measured
    // tallies) rather than erroring.
    let measured =
        BdRom::open_resilient_with(root, true, Some(&files), progress).unwrap_or(structural);
    let order = selection_order(&measured.bdrom.playlists, &names);
    Ok(text::render_with(&measured.bdrom, &order, &measured.errors))
}

// ── shared render path ──────────────────────────────────────────────────────

/// Runs the full **measured** scan over `root` and renders the classic disc
/// report.
///
/// This is the byte-for-byte core shared by every export and the native parity
/// test: [`BdRom::open_resilient_with`] with the packet scan **on**, the CLI's
/// `--whole` selection ([`PlaylistFilter::default`] — the standard filtered set,
/// dropping playlists shorter than 20 s and looping ones), then [`text`]
/// rendering. `progress` observes the demux.
///
/// # Errors
/// Returns the [`BdError`] from [`BdRom::open_resilient_with`] when the
/// structure is too damaged to open at all (no `BDMV`/`CLIPINF`/`PLAYLIST`) —
/// the caller decides whether that is an empty disc or an error to report.
fn render_disc(
    root: &dyn BdDir,
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<String, BdError> {
    let report = BdRom::open_resilient_with(root, true, None, progress)?;
    let order = report.bdrom.presentation_order(&PlaylistFilter::default());
    Ok(text::render_with(&report.bdrom, &order, &report.errors))
}

/// Renders the synthetic in-memory tree built from `data` (no progress).
///
/// An unopenable structure renders as the empty string — the resilient-open
/// absence path the `parse_report` fuzz target and the parity test expect.
#[must_use]
pub fn run_report(data: &[u8]) -> String {
    render_disc(&build_tree(data), &mut |_| {}).unwrap_or_default()
}

/// The Phase 1 in-memory entry point: feed it BDMV bytes (the six `u32`-BE
/// framed sections — see the module-level docs), get back the classic report.
///
/// Runs the **measured** scan (M2TS demux + per-stream/per-chapter statistics),
/// identical to `bdinfo-rs <disc> --whole`, all inside the WebAssembly sandbox.
#[wasm_bindgen]
#[must_use]
pub fn scan_report(data: &[u8]) -> String {
    let report = run_report(data);
    #[cfg(target_arch = "wasm32")]
    web_sys::console::log_1(&format!("bdinfo-rs-wasm: rendered {} bytes", report.len()).into());
    report
}

/// The Phase 2 streaming entry point: hand it a `webkitdirectory`-selected BDMV
/// folder as parallel `(relativePath, File)` lists and get back the classic
/// disc report.
///
/// Runs the **full measured** scan — M2TS demux + per-stream/per-chapter
/// statistics, `run_packet_scan = true` — reading every file's bytes
/// synchronously at byte offsets through [`web_sys::FileReaderSync`]. This MUST
/// run in a Web Worker (the only scope where `FileReaderSync` exists). When
/// `on_progress` is supplied it is called as `(file, done, total)` after each
/// demux read. A non-empty `selection` names the playlists to measure (CLI
/// `--mpls` semantics — unfiltered, in order); an empty `selection` measures the
/// standard `--whole` set.
///
/// # Errors
/// Returns a `JsValue` if `paths` and `files` differ in length, any `files`
/// entry is not a `File`, the paths do not form one coherent disc selection, or
/// no readable Blu-ray structure is found (so a wrong folder pick is reported
/// rather than silently returning an empty report).
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn scan_files(
    paths: Vec<String>,
    files: js_sys::Array,
    selection: Vec<String>,
    on_progress: Option<js_sys::Function>,
) -> Result<String, JsValue> {
    let root = build_web_tree(&paths, &files)?;
    let mut observe = |p: ScanProgress<'_>| {
        if let Some(callback) = on_progress.as_ref() {
            let _ = callback.call3(
                &JsValue::NULL,
                &JsValue::from_str(p.file),
                &JsValue::from_f64(p.done as f64),
                &JsValue::from_f64(p.total as f64),
            );
        }
    };
    let rendered = if selection.is_empty() {
        render_disc(&root, &mut observe)
    } else {
        render_selection(&root, &selection, &mut observe)
    };
    rendered.map_err(|err| JsValue::from_str(&format!("no readable Blu-ray structure ({err})")))
}

/// The structural-scan entry point: the playlist selection table as JSON.
///
/// Hand it a `webkitdirectory`-selected BDMV folder and get back the rows the
/// browser renders as a multi-select checklist before the measured scan,
/// mirroring the CLI's playlist table.
///
/// Runs only the **structural** scan — no packet demux — so it is fast: it reads
/// the playlist/clip metadata, not the multi-GB stream files. Each element is
/// `{ position, group, name, length, estimatedBytes, hasHidden }`; pass the
/// chosen `name`s back to [`scan_files`] to measure just those playlists.
///
/// # Errors
/// As [`scan_files`]: `paths`/`files` length mismatch, a non-`File` entry, an
/// incoherent selection, or no readable Blu-ray structure.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn list_playlists(paths: Vec<String>, files: js_sys::Array) -> Result<String, JsValue> {
    let root = build_web_tree(&paths, &files)?;
    let report = BdRom::open_resilient(&root, false)
        .map_err(|err| JsValue::from_str(&format!("no readable Blu-ray structure ({err})")))?;
    Ok(rows_to_json(&playlist_rows(&report.bdrom.playlists)))
}

#[cfg(test)]
mod tests {
    use std::io::{self, SeekFrom};

    use super::{
        MAX_TREE_DEPTH, TreeError, assemble_tree, extension_of, glob_match, path_components,
        read_window, seek_target, split_sections,
    };

    /// Parses path strings into the `(components, id)` entries `assemble_tree`
    /// takes, tagging each file with its index so placement stays checkable.
    fn entries<'a>(paths: &[&'a str]) -> Vec<(Vec<&'a str>, usize)> {
        paths.iter().enumerate().map(|(i, path)| (path_components(path), i)).collect()
    }

    /// The committed Big Buck Bunny fixture framed into the six `u32`-BE sections
    /// the in-memory path expects (the bytes the parity golden is built from).
    fn fixture_blob() -> Vec<u8> {
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
    fn glob_match_is_literal_anchored_and_case_insensitive() {
        assert!(glob_match(b"00000.MPLS", b"00000.mpls"));
        assert!(glob_match(b"*.mpls", b"00000.MPLS"));
        assert!(glob_match(b"00???.clpi", b"00012.clpi"));
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b"", b""));
        assert!(!glob_match(b"*.mpls", b"00000.m2ts"));
        assert!(!glob_match(b"?", b""));
        assert!(!glob_match(b"a", b""));
    }

    #[test]
    fn split_sections_frames_up_to_six_and_stops_on_truncation() {
        // Two whole one-byte sections.
        assert_eq!(split_sections(&[0, 0, 0, 1, b'A', 0, 0, 0, 1, b'B']), [vec![b'A'], vec![b'B']]);
        // A length that overruns truncates to what is present, then stops.
        assert_eq!(split_sections(&[0, 0, 0, 4, b'X', b'Y']), [vec![b'X', b'Y']]);
        // No 4-byte length prefix at all yields no sections.
        assert!(split_sections(&[0, 0]).is_empty());
        // Never more than six sections, even with more length-prefixed data.
        let many: Vec<u8> = std::iter::repeat_n(0_u8, 4 * 8).collect();
        assert_eq!(split_sections(&many).len(), 6);
    }

    #[test]
    fn path_components_splits_on_both_separators_and_drops_empties() {
        assert_eq!(path_components("BDMV/PLAYLIST/00000.mpls"), ["BDMV", "PLAYLIST", "00000.mpls"]);
        assert_eq!(path_components("BDMV\\STREAM\\00000.m2ts"), ["BDMV", "STREAM", "00000.m2ts"]);
        assert_eq!(path_components("//a///b//"), ["a", "b"]);
        assert!(path_components("").is_empty());
    }

    #[test]
    fn extension_of_returns_the_dotted_suffix_or_empty() {
        assert_eq!(extension_of("00000.mpls"), ".mpls");
        assert_eq!(extension_of("archive.tar.gz"), ".gz");
        assert_eq!(extension_of("noext"), "");
        assert_eq!(extension_of(".hidden"), ".hidden");
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
        // live and covered on the native (Tier-A) build.
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
        use std::sync::Arc;

        use super::{MemFile, build_tree, render_disc};

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
            blob.extend_from_slice(&(section.len() as u32).to_be_bytes());
            blob.extend_from_slice(section);
        }
        let framed = render_disc(&build_tree(&blob), &mut |_| {}).expect("framed render");

        // The same disc handed over as a `webkitdirectory` pick of the BDMV
        // folder itself: the wrapper root makes it render identically.
        let picked: [(&str, &[u8]); 5] = [
            ("BDMV/index.bdmv", INDEX),
            ("BDMV/MovieObject.bdmv", MOVIE),
            ("BDMV/PLAYLIST/00000.mpls", MPLS),
            ("BDMV/CLIPINF/00000.clpi", CLPI),
            ("BDMV/STREAM/00000.m2ts", M2TS),
        ];
        let tree = assemble_tree(
            picked
                .iter()
                .map(|(path, data)| {
                    let comps = path_components(path);
                    let name = (*comps.last().expect("a file name")).to_owned();
                    (comps, MemFile { name, full: (*path).to_owned(), data: Arc::from(*data) })
                })
                .collect(),
        )
        .expect("assemble the BDMV-rooted selection");
        let from_bdmv = render_disc(&tree, &mut |_| {}).expect("BDMV-rooted render");

        assert_eq!(framed, from_bdmv, "a BDMV-rooted pick must render like the canonical framing");
    }

    #[test]
    fn the_render_drops_a_short_playlist_like_whole() {
        use std::sync::Arc;

        use super::{MemFile, render_disc};

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

        let files: [(&str, Vec<u8>); 6] = [
            ("DISC/BDMV/index.bdmv", INDEX.to_vec()),
            ("DISC/BDMV/MovieObject.bdmv", MOVIE.to_vec()),
            ("DISC/BDMV/PLAYLIST/00000.mpls", MPLS.to_vec()),
            ("DISC/BDMV/PLAYLIST/00001.mpls", short),
            ("DISC/BDMV/CLIPINF/00000.clpi", CLPI.to_vec()),
            ("DISC/BDMV/STREAM/00000.m2ts", M2TS.to_vec()),
        ];
        let tree = assemble_tree(
            files
                .iter()
                .map(|(path, data)| {
                    let comps = path_components(path);
                    let name = (*comps.last().expect("a file name")).to_owned();
                    (
                        comps,
                        MemFile { name, full: (*path).to_owned(), data: Arc::from(data.clone()) },
                    )
                })
                .collect(),
        )
        .expect("assemble the two-playlist disc");
        let report = render_disc(&tree, &mut |_| {}).expect("render");

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
    fn mem_file_exposes_metadata_and_reads_as_bytes_and_text() {
        use std::io::Read;

        use bdinfo_rs_core::vfs::BdFile;

        use super::mem_file;

        // The scan reaches MemFile only through `open_read`; `full_name`,
        // `is_dir`, and `open_text` are off the render path, so cover them here.
        let file = mem_file("WASMDISC/BDMV", "index.bdmv", b"hello".to_vec());
        assert_eq!(file.name(), "index.bdmv");
        assert_eq!(file.full_name(), "WASMDISC/BDMV/index.bdmv");
        assert_eq!(file.extension(), ".bdmv");
        assert_eq!(file.length(), 5);
        assert!(!file.is_dir());

        let mut bytes = Vec::new();
        file.open_read().expect("open_read").read_to_end(&mut bytes).expect("read bytes");
        assert_eq!(bytes, b"hello");

        let mut text = String::new();
        file.open_text().expect("open_text").read_to_string(&mut text).expect("read text");
        assert_eq!(text, "hello");
    }

    #[test]
    fn node_walk_matches_patterns_with_and_without_recursion() {
        use bdinfo_rs_core::vfs::{BdDir, SearchOption};

        use super::{Node, mem_file};

        let mut stream = Node::dir("STREAM", "DISC/BDMV/STREAM");
        stream.files.push(mem_file("DISC/BDMV/STREAM", "00000.m2ts", vec![0_u8; 4]));
        let mut root = Node::dir("BDMV", "DISC/BDMV");
        root.files.push(mem_file("DISC/BDMV", "index.bdmv", vec![0_u8; 2]));
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

        use super::{MemFile, Node, build_tree, render_disc};

        // One progress sink — a fn pointer, so it is generic over the progress
        // lifetime (a shared closure would pin it and fail to type-check across
        // the two calls). The good render drives it so its body is covered; the
        // unopenable tree (no BDMV/CLIPINF/PLAYLIST) then makes render_disc hit
        // the `?` early-return, the arm the parity `Ok` flow never reaches.
        let mut sink: for<'a> fn(ScanProgress<'a>) = |_| {};
        render_disc(&build_tree(&fixture_blob()), &mut sink).expect("the fixture opens");
        let empty: Node<MemFile> = Node::dir("EMPTY", "EMPTY");
        assert!(render_disc(&empty, &mut sink).is_err());
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

    /// CLI-parity selection helpers (Tier A): the structural playlist listing
    /// and the by-name measured scan, native-tested to the core bar.
    mod selection {
        use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary};

        use crate::{
            PlaylistRow, estimated_bytes, json_escape, named_selection, normalize_playlist_name,
            playlist_rows, rows_to_json, selection_order, selection_stream_files, table_length,
            table_rows,
        };

        /// A `ClipSummary` carrying just a name + length (the fields the
        /// selection helpers read); the measured tallies stay zero.
        fn sample_clip(name: &str, length: f64) -> ClipSummary {
            ClipSummary {
                name: name.to_owned(),
                display_name: name.to_owned(),
                angle_index: 0,
                relative_time_in: 0.0,
                length,
                payload_bytes: 0,
                packet_count: 0,
                packet_seconds: 0.0,
                file_seconds: 0.0,
                streams: Vec::new(),
            }
        }

        /// A `PlaylistSummary` carrying just what the selection/table helpers
        /// read: name, length, the two file sizes, and its clip names.
        fn sample_playlist(
            name: &str,
            total_length: f64,
            file_size: u64,
            interleaved_file_size: u64,
            clips: &[&str],
        ) -> PlaylistSummary {
            PlaylistSummary {
                name: name.to_owned(),
                total_length,
                file_size,
                interleaved_file_size,
                chapter_count: 0,
                stream_count: 0,
                angle_count: 0,
                has_loops: false,
                streams: Vec::new(),
                clips: clips.iter().map(|clip| sample_clip(clip, total_length)).collect(),
                chapters: Vec::new(),
            }
        }

        /// A four-playlist disc: 00000 (100 s) shares clip A with 00001 (50 s) →
        /// group 1; 00002 (70 s, clip B) → group 2; 00003 (5 s) is dropped by
        /// the short filter.
        fn disc() -> [PlaylistSummary; 4] {
            [
                sample_playlist("00000.MPLS", 100.0, 1000, 0, &["A.M2TS"]),
                sample_playlist("00001.MPLS", 50.0, 500, 0, &["A.M2TS"]),
                sample_playlist("00002.MPLS", 70.0, 0, 2000, &["B.M2TS"]),
                sample_playlist("00003.MPLS", 5.0, 100, 0, &["C.M2TS"]),
            ]
        }

        #[test]
        fn table_rows_groups_and_filters_like_the_cli() {
            // Sorted by length desc, grouped by shared clip, the short row dropped.
            assert_eq!(table_rows(&disc()), [(1, 0), (1, 1), (2, 2)]);
        }

        #[test]
        fn playlist_rows_carry_the_table_columns() {
            let rows = playlist_rows(&disc());
            let view: Vec<_> = rows
                .iter()
                .map(|row| {
                    (
                        row.position,
                        row.group,
                        row.name.as_str(),
                        row.length.as_str(),
                        row.estimated_bytes,
                        row.has_hidden_streams,
                    )
                })
                .collect();
            assert_eq!(
                view,
                [
                    (1, 1, "00000.MPLS", "00:01:40", Some(1000), false),
                    (2, 1, "00001.MPLS", "00:00:50", Some(500), false),
                    // group 2; the interleaved size is preferred over the m2ts size.
                    (3, 2, "00002.MPLS", "00:01:10", Some(2000), false),
                ]
            );
        }

        #[test]
        fn table_length_truncates_and_wraps_the_day() {
            assert_eq!(table_length(0.0), "00:00:00");
            assert_eq!(table_length(3661.0), "01:01:01");
            assert_eq!(table_length(-5.0), "00:00:00"); // a negative length clamps to zero
            assert_eq!(table_length(90_061.0), "01:01:01"); // 25h01m01s wraps the day
        }

        #[test]
        fn estimated_bytes_prefers_interleaved_then_m2ts_then_none() {
            assert_eq!(estimated_bytes(&sample_playlist("X", 1.0, 500, 2000, &[])), Some(2000));
            assert_eq!(estimated_bytes(&sample_playlist("X", 1.0, 500, 0, &[])), Some(500));
            assert_eq!(estimated_bytes(&sample_playlist("X", 1.0, 0, 0, &[])), None);
        }

        #[test]
        fn normalize_playlist_name_uppercases_and_appends_mpls() {
            assert_eq!(normalize_playlist_name("00000"), "00000.MPLS");
            assert_eq!(normalize_playlist_name("00000.mpls"), "00000.MPLS");
            assert_eq!(normalize_playlist_name("feature.m2ts"), "FEATURE.M2TS");
        }

        #[test]
        fn named_selection_normalizes_dedupes_and_keeps_order() {
            let playlists = [
                sample_playlist("00000.MPLS", 1.0, 0, 0, &[]),
                sample_playlist("00002.MPLS", 1.0, 0, 0, &[]),
            ];
            // unknown skipped, duplicate skipped, order preserved, name normalized.
            assert_eq!(
                named_selection(
                    &playlists,
                    &[
                        "00002".to_owned(),
                        "00000.mpls".to_owned(),
                        "99999".to_owned(),
                        "00002".to_owned(),
                    ],
                ),
                ["00002.MPLS", "00000.MPLS"]
            );
            assert!(named_selection(&playlists, &[]).is_empty());
        }

        #[test]
        fn selection_stream_files_collects_every_selected_clip() {
            let playlists = [
                sample_playlist("00000.MPLS", 1.0, 0, 0, &["A.M2TS", "B.M2TS"]),
                sample_playlist("00001.MPLS", 1.0, 0, 0, &["C.M2TS"]),
            ];
            let files = selection_stream_files(&playlists, &["00000.MPLS".to_owned()]);
            assert_eq!(files.into_iter().collect::<Vec<_>>(), ["A.M2TS", "B.M2TS"]);
            // an unknown name contributes nothing.
            assert!(selection_stream_files(&playlists, &["99999.MPLS".to_owned()]).is_empty());
        }

        #[test]
        fn selection_order_maps_names_to_indices_in_order() {
            let playlists = [
                sample_playlist("00000.MPLS", 1.0, 0, 0, &[]),
                sample_playlist("00001.MPLS", 1.0, 0, 0, &[]),
                sample_playlist("00002.MPLS", 1.0, 0, 0, &[]),
            ];
            assert_eq!(
                selection_order(&playlists, &["00002.MPLS".to_owned(), "00000.MPLS".to_owned()]),
                [2, 0]
            );
            // an unknown name maps to nothing.
            assert!(selection_order(&playlists, &["99999.MPLS".to_owned()]).is_empty());
        }

        #[test]
        fn json_escape_escapes_quotes_backslashes_controls_but_not_space() {
            let mut out = String::new();
            json_escape("a\"\\\n\t\r \u{1f}\u{0}z", &mut out);
            assert_eq!(out, "a\\\"\\\\\\n\\t\\r \\u001f\\u0000z");
        }

        #[test]
        fn rows_to_json_serializes_rows_commas_and_nulls() {
            let rows = [
                PlaylistRow {
                    position: 1,
                    group: 1,
                    name: "00000.MPLS".to_owned(),
                    length: "00:01:40".to_owned(),
                    estimated_bytes: Some(1000),
                    has_hidden_streams: false,
                },
                PlaylistRow {
                    position: 2,
                    group: 2,
                    name: "00001.MPLS".to_owned(),
                    length: "00:00:50".to_owned(),
                    estimated_bytes: None,
                    has_hidden_streams: true,
                },
            ];
            assert_eq!(
                rows_to_json(&rows),
                "[{\"position\":1,\"group\":1,\"name\":\"00000.MPLS\",\"length\":\"00:01:40\",\"estimatedBytes\":1000,\"hasHidden\":false},\
                 {\"position\":2,\"group\":2,\"name\":\"00001.MPLS\",\"length\":\"00:00:50\",\"estimatedBytes\":null,\"hasHidden\":true}]"
            );
            assert_eq!(rows_to_json(&[]), "[]");
        }

        #[test]
        fn render_selection_measures_only_the_named_playlists() {
            use std::sync::Arc;

            use bdinfo_rs_core::bdrom::disc::ScanProgress;

            use crate::{MemFile, Node, assemble_tree, path_components, render_selection};

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

            let files: [(&str, Vec<u8>); 6] = [
                ("DISC/BDMV/index.bdmv", INDEX.to_vec()),
                ("DISC/BDMV/MovieObject.bdmv", MOVIE.to_vec()),
                ("DISC/BDMV/PLAYLIST/00000.mpls", MPLS.to_vec()),
                ("DISC/BDMV/PLAYLIST/00001.mpls", short),
                ("DISC/BDMV/CLIPINF/00000.clpi", CLPI.to_vec()),
                ("DISC/BDMV/STREAM/00000.m2ts", M2TS.to_vec()),
            ];
            let tree = assemble_tree(
                files
                    .iter()
                    .map(|(path, data)| {
                        let comps = path_components(path);
                        let name = (*comps.last().expect("a file name")).to_owned();
                        (
                            comps,
                            MemFile {
                                name,
                                full: (*path).to_owned(),
                                data: Arc::from(data.clone()),
                            },
                        )
                    })
                    .collect(),
            )
            .expect("assemble the two-playlist disc");

            // One progress sink for every call — a fn pointer so it is generic
            // over the progress lifetime; the real demux below drives it (so it
            // is covered), and the unopenable case reuses it without firing it.
            let mut sink: for<'a> fn(ScanProgress<'a>) = |_| {};

            // Selecting only the feature renders it and drops the short sibling.
            let feature = render_selection(&tree, &["00000.MPLS".to_owned()], &mut sink)
                .expect("render the feature");
            assert!(feature.contains("00000.MPLS"), "the selected feature must be rendered");
            assert!(!feature.contains("00001.MPLS"), "an unselected playlist must not be rendered");

            // Selecting the short playlist by name keeps it — unfiltered, unlike
            // `--whole`; and only the named playlist is rendered.
            let short_only = render_selection(&tree, &["00001".to_owned()], &mut sink)
                .expect("render the short");
            assert!(short_only.contains("00001.MPLS"), "a by-name short playlist is kept");
            assert!(!short_only.contains("00000.MPLS"), "only the named playlist is rendered");

            // An unopenable tree (no BDMV) propagates the structural open's error.
            let empty: Node<MemFile> = Node::dir("EMPTY", "EMPTY");
            assert!(
                render_selection(&empty, &["00000.MPLS".to_owned()], &mut sink).is_err(),
                "a structure with no BDMV must error"
            );
        }
    }

    // The reader math is panic-safety-critical, so amplify the unit cases with
    // property tests. proptest's backend does not build for wasm32, so these run
    // only on the native (Tier-A) build.
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
