//! WebAssembly browser bindings for the bdinfo-rs Blu-ray analyzer.
//!
//! This crate exposes the library's whole **measured** scan pipeline to the
//! browser: in-memory BDMV bytes become a synthetic disc tree, that tree is
//! opened with [`BdRom::open_resilient`] (packet scan **on**), and the result is
//! rendered to the classic human-readable report — the very same bytes the CLI
//! writes to `BDINFO.<label>.txt`.
//!
//! It is an INDEPENDENT workspace (see `Cargo.toml`): wasm-bindgen's generated
//! glue uses `unsafe`, so this crate sits OUTSIDE the `forbid(unsafe_code)`
//! posture of `bdinfo-rs-core` / `bdinfo-rs`. The core library itself stays
//! memory-safe; only the thin browser shim here is exempt.
//!
//! ## Input framing
//!
//! [`scan_report`] takes one byte buffer holding up to six `u32` big-endian
//! length-prefixed sections, assigned in fixed order to the synthetic disc's six
//! files — `index.bdmv`, `MovieObject.bdmv`, the playlist, the clip, the stream
//! file, and `META/DL/bdmt_eng.xml`. This mirrors the synthetic tree the
//! `parse_report` fuzz target builds, widened from `u16` to `u32` so a
//! real-scale `*.m2ts` stream file (megabytes) fits in a section. A missing or
//! truncated section leaves its file empty (the resilient-open absence path).

use std::io::{self, BufRead, BufReader, Cursor};
use std::sync::Arc;

use bdinfo_rs_core::bdrom::disc::BdRom;
use bdinfo_rs_core::bdrom::order::PlaylistFilter;
use bdinfo_rs_core::report::text;
use bdinfo_rs_core::vfs::{BdDir, BdFile, ReadSeek, SearchOption};
use wasm_bindgen::prelude::wasm_bindgen;

/// An in-memory file node backed by a shared byte buffer.
#[derive(Clone)]
struct MemFile {
    name: String,
    full: String,
    data: Arc<Vec<u8>>,
}

impl BdFile for MemFile {
    fn name(&self) -> &str {
        &self.name
    }

    fn full_name(&self) -> &str {
        &self.full
    }

    fn extension(&self) -> &str {
        self.name.rfind('.').map_or("", |i| &self.name[i..])
    }

    fn length(&self) -> u64 {
        self.data.len() as u64
    }

    fn is_dir(&self) -> bool {
        false
    }

    fn open_read(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(Cursor::new(self.data.as_ref().clone())))
    }

    fn open_text(&self) -> io::Result<Box<dyn BufRead>> {
        Ok(Box::new(BufReader::new(Cursor::new(self.data.as_ref().clone()))))
    }
}

/// An in-memory directory node.
#[derive(Clone, Default)]
struct MemDir {
    name: String,
    full: String,
    dirs: Vec<MemDir>,
    files: Vec<MemFile>,
}

/// ASCII case-insensitive glob: `*` = any run, `?` = any one byte.
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

impl MemDir {
    fn collect_pattern(&self, pattern: &str, recurse: bool, out: &mut Vec<Box<dyn BdFile>>) {
        for f in &self.files {
            if glob_match(pattern.as_bytes(), f.name.as_bytes()) {
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

impl BdDir for MemDir {
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

fn file(dir: &str, name: &str, data: Vec<u8>) -> MemFile {
    MemFile { name: name.to_owned(), full: format!("{dir}/{name}"), data: Arc::new(data) }
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

/// Builds the synthetic disc tree around the six framed sections.
fn build_tree(data: &[u8]) -> MemDir {
    let mut next = split_sections(data).into_iter();
    let mut take = || next.next().unwrap_or_default();

    let index = take();
    let movie_object = take();
    let mpls = take();
    let clpi = take();
    let m2ts = take();
    let xml = take();

    let playlist = MemDir {
        name: "PLAYLIST".to_owned(),
        full: "WASMDISC/BDMV/PLAYLIST".to_owned(),
        files: vec![file("WASMDISC/BDMV/PLAYLIST", "00000.mpls", mpls)],
        ..MemDir::default()
    };
    let clipinf = MemDir {
        name: "CLIPINF".to_owned(),
        full: "WASMDISC/BDMV/CLIPINF".to_owned(),
        files: vec![file("WASMDISC/BDMV/CLIPINF", "00000.clpi", clpi)],
        ..MemDir::default()
    };
    let stream = MemDir {
        name: "STREAM".to_owned(),
        full: "WASMDISC/BDMV/STREAM".to_owned(),
        files: vec![file("WASMDISC/BDMV/STREAM", "00000.m2ts", m2ts)],
        ..MemDir::default()
    };
    let dl = MemDir {
        name: "DL".to_owned(),
        full: "WASMDISC/BDMV/META/DL".to_owned(),
        files: vec![file("WASMDISC/BDMV/META/DL", "bdmt_eng.xml", xml)],
        ..MemDir::default()
    };
    let meta = MemDir {
        name: "META".to_owned(),
        full: "WASMDISC/BDMV/META".to_owned(),
        dirs: vec![dl],
        ..MemDir::default()
    };
    let bdmv = MemDir {
        name: "BDMV".to_owned(),
        full: "WASMDISC/BDMV".to_owned(),
        dirs: vec![playlist, clipinf, stream, meta],
        files: vec![
            file("WASMDISC/BDMV", "index.bdmv", index),
            file("WASMDISC/BDMV", "MovieObject.bdmv", movie_object),
        ],
    };
    MemDir {
        name: "WASMDISC".to_owned(),
        full: "WASMDISC".to_owned(),
        dirs: vec![bdmv],
        ..MemDir::default()
    }
}

/// Runs the full measured scan over the synthetic tree built from `data` and
/// returns the classic disc report, or an empty string if the disc structure is
/// too damaged to open at all (no `BDMV`/`CLIPINF`/`PLAYLIST`).
///
/// This is the byte-for-byte core shared by the wasm [`scan_report`] export and
/// the native parity test: `BdRom::open_resilient(&tree, true)` (packet scan on)
/// then [`text`] rendering. Every parsed playlist is rendered
/// ([`PlaylistFilter::everything`]) — the browser scanner mirrors the CLI's
/// `--whole`, the same `render_with` call the CLI uses to save its report — so a
/// short fixture clip still shows its full measured stream tables.
#[must_use]
pub fn run_report(data: &[u8]) -> String {
    let root = build_tree(data);
    match BdRom::open_resilient(&root, true) {
        Ok(report) => {
            let order = report.bdrom.presentation_order(&PlaylistFilter::everything());
            text::render_with(&report.bdrom, &order, &report.errors)
        }
        Err(_) => String::new(),
    }
}

/// The browser entry point: feed it BDMV bytes (the six `u32`-BE framed sections
/// — see the module-level docs), get back the classic disc report.
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
