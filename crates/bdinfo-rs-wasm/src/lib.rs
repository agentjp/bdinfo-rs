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
//! byte-for-byte identically, and the same pinned golden pins both.
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
use std::io::SeekFrom;
use std::io::{self, BufRead, BufReader, Cursor};
#[cfg(target_arch = "wasm32")]
use std::io::{Read, Seek};
use std::sync::Arc;

use bdinfo_rs_core::bdrom::disc::{BdRom, ScanProgress};
use bdinfo_rs_core::bdrom::order::PlaylistFilter;
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
const READ_WINDOW: usize = 1 << 20;

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
        // `take <= tail.len()`, so `split_at` cannot panic.
        let (head, next) = tail.split_at(take);
        sections.push(head.to_vec());
        rest = next;
        if take < want {
            break;
        }
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
/// demux read.
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
    render_disc(&root, &mut observe)
        .map_err(|err| JsValue::from_str(&format!("no readable Blu-ray structure ({err})")))
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
