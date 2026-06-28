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

use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::sync::Arc;

use bdinfo_rs_core::bdrom::disc::{BdRom, ScanProgress};
use bdinfo_rs_core::bdrom::order::PlaylistFilter;
use bdinfo_rs_core::report::text;
use bdinfo_rs_core::vfs::{BdDir, BdFile, ReadSeek, SearchOption};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};

/// The read-ahead window each [`WebReader`] fill pulls from `FileReaderSync` in
/// one go, so a front-to-back demux crosses the JS boundary once per MiB rather
/// than once per small parser read.
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
    dirs: Vec<Node<F>>,
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
        Some((b'*', rest)) => (0..=name.len()).any(|skip| glob_match(rest, &name[skip..])),
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
    name.rfind('.').map_or("", |i| &name[i..])
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
        sections.push(tail[..take].to_vec());
        rest = &tail[take..];
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
struct WebReader {
    file: web_sys::File,
    reader: web_sys::FileReaderSync,
    pos: u64,
    len: u64,
}

/// Renders a `JsValue` error to a short message for an [`io::Error`].
fn js_message(value: &JsValue) -> String {
    value.as_string().unwrap_or_else(|| "JavaScript exception".to_owned())
}

impl Read for WebReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        let end = self.pos.saturating_add(buf.len() as u64).min(self.len);
        let blob = self
            .file
            .slice_with_f64_and_f64(self.pos as f64, end as f64)
            .map_err(|e| io::Error::other(js_message(&e)))?;
        let array = self
            .reader
            .read_as_array_buffer(&blob)
            .map_err(|e| io::Error::other(js_message(&e)))?;
        let view = js_sys::Uint8Array::new(&array);
        let n = view.length() as usize;
        view.copy_to(&mut buf[..n]);
        self.pos = self.pos.saturating_add(n as u64);
        Ok(n)
    }
}

impl Seek for WebReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target: i128 = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::End(n) => i128::from(self.len) + i128::from(n),
            SeekFrom::Current(n) => i128::from(self.pos) + i128::from(n),
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start of file"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

/// A file backed by a browser `File` handle — the [`BdFile`] backend for the
/// `webkitdirectory` streaming path. Bytes are read on demand through
/// [`WebReader`]; only metadata (name, full path, length) is held eagerly.
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
fn path_components(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|s| !s.is_empty()).collect()
}

/// Inserts `file` under the directory chain `dirs` (relative to `node`),
/// creating intermediate [`Node`]s as needed.
fn insert_file(node: &mut Node<WebFile>, dirs: &[&str], file: WebFile) {
    match dirs.split_first() {
        None => node.files.push(file),
        Some((head, rest)) => {
            let full = format!("{}/{head}", node.full);
            let idx = if let Some(i) = node.dirs.iter().position(|d| d.name == *head) {
                i
            } else {
                node.dirs.push(Node::dir(head, &full));
                node.dirs.len() - 1
            };
            insert_file(&mut node.dirs[idx], rest, file);
        }
    }
}

/// Builds the file-backed disc tree from the parallel `(relativePath, File)`
/// lists. The first path component becomes the disc-root directory name (its
/// disc label); every file is inserted at its relative path.
///
/// # Errors
/// Returns a `JsValue` if the two lists differ in length or any entry is not a
/// `File`.
fn build_web_tree(paths: &[String], files: &js_sys::Array) -> Result<Node<WebFile>, JsValue> {
    let count = paths.len();
    if count != files.length() as usize {
        return Err(JsValue::from_str("paths and files differ in length"));
    }

    let mut root: Option<Node<WebFile>> = None;
    for (i, path) in paths.iter().enumerate() {
        let comps = path_components(path);
        let Some((name, dirs)) = comps.split_last() else { continue };
        let root_name = *comps.first().unwrap_or(name);

        let value = files.get(i as u32);
        let file: web_sys::File =
            value.dyn_into().map_err(|_| JsValue::from_str("entry is not a File"))?;
        let length = file.size() as u64;
        let web_file = WebFile { name: (*name).to_owned(), full: path.clone(), file, length };

        let tree = root.get_or_insert_with(|| Node::dir(root_name, root_name));
        // The first component is the root itself; only the components between it
        // and the file name are intermediate directories.
        let inner = dirs.split_first().map_or(&[][..], |(_, rest)| rest);
        insert_file(tree, inner, web_file);
    }

    Ok(root.unwrap_or_else(|| Node::dir("WASMDISC", "WASMDISC")))
}

// ── shared render path ──────────────────────────────────────────────────────

/// Runs the full **measured** scan over `root` and renders the classic disc
/// report, or an empty string if the structure is too damaged to open at all
/// (no `BDMV`/`CLIPINF`/`PLAYLIST`).
///
/// This is the byte-for-byte core shared by every export and the native parity
/// test: [`BdRom::open_resilient_with`] with the packet scan **on**, every
/// parsed playlist kept ([`PlaylistFilter::everything`] — the CLI's `--whole`),
/// then [`text`] rendering. `progress` observes the demux.
fn render_disc(root: &dyn BdDir, progress: &mut dyn FnMut(ScanProgress<'_>)) -> String {
    match BdRom::open_resilient_with(root, true, None, progress) {
        Ok(report) => {
            let order = report.bdrom.presentation_order(&PlaylistFilter::everything());
            text::render_with(&report.bdrom, &order, &report.errors)
        }
        Err(_) => String::new(),
    }
}

/// Renders the synthetic in-memory tree built from `data` (no progress).
#[must_use]
pub fn run_report(data: &[u8]) -> String {
    render_disc(&build_tree(data), &mut |_| {})
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
/// Returns a `JsValue` if `paths` and `files` differ in length, or any `files`
/// entry is not a `File`.
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
    Ok(render_disc(&root, &mut observe))
}
