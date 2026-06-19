#![no_main]
//! End-to-end fuzz target: the whole `BdRom` pipeline plus the report renderer.
//!
//! The input bytes become a synthetic in-memory BDMV tree — `index.bdmv`,
//! `MovieObject.bdmv`, a playlist, a clip, a stream file, and
//! `META/DL/bdmt_eng.xml` (the roxmltree input — the one third-party parser fed
//! disc bytes) — which is opened resiliently, packet-scanned, and rendered to
//! the classic report. This holds the library's whole-pipeline no-panic /
//! no-hang contract on hostile discs, end to end: discovery → index → MPLS /
//! CLPI → M2TS demux → measured summaries → `report::text::render`.
//!
//! Input framing: a sequence of `u16` big-endian length-prefixed sections,
//! assigned in fixed order to the six files above; missing sections leave the
//! file absent (exercising the resilient-open absence paths too).

use std::io::{self, BufRead, BufReader, Cursor};
use std::sync::Arc;

use bdinfo_rs_core::bdrom::disc::BdRom;
use bdinfo_rs_core::report::text;
use bdinfo_rs_core::vfs::{BdDir, BdFile, ReadSeek, SearchOption};
use libfuzzer_sys::fuzz_target;

/// An in-memory file node.
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
        Some((b'*', rest)) => {
            (0..=name.len()).any(|skip| glob_match(rest, &name[skip..]))
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

/// Splits `data` into up to six `u16`-BE length-prefixed sections and builds
/// the synthetic disc tree around them.
fn build_tree(data: &[u8]) -> MemDir {
    let mut sections: Vec<Vec<u8>> = Vec::new();
    let mut rest = data;
    while sections.len() < 6 {
        let Some((len_bytes, tail)) = rest.split_first_chunk::<2>() else { break };
        let want = usize::from(u16::from_be_bytes(*len_bytes));
        let take = want.min(tail.len());
        sections.push(tail[..take].to_vec());
        rest = &tail[take..];
        if take < want {
            break;
        }
    }
    let mut next = sections.into_iter();
    let mut take = || next.next().unwrap_or_default();

    let index = take();
    let movie_object = take();
    let mpls = take();
    let clpi = take();
    let m2ts = take();
    let xml = take();

    let playlist = MemDir {
        name: "PLAYLIST".to_owned(),
        full: "FUZZDISC/BDMV/PLAYLIST".to_owned(),
        files: vec![file("FUZZDISC/BDMV/PLAYLIST", "00000.mpls", mpls)],
        ..MemDir::default()
    };
    let clipinf = MemDir {
        name: "CLIPINF".to_owned(),
        full: "FUZZDISC/BDMV/CLIPINF".to_owned(),
        files: vec![file("FUZZDISC/BDMV/CLIPINF", "00000.clpi", clpi)],
        ..MemDir::default()
    };
    let stream = MemDir {
        name: "STREAM".to_owned(),
        full: "FUZZDISC/BDMV/STREAM".to_owned(),
        files: vec![file("FUZZDISC/BDMV/STREAM", "00000.m2ts", m2ts)],
        ..MemDir::default()
    };
    let dl = MemDir {
        name: "DL".to_owned(),
        full: "FUZZDISC/BDMV/META/DL".to_owned(),
        files: vec![file("FUZZDISC/BDMV/META/DL", "bdmt_eng.xml", xml)],
        ..MemDir::default()
    };
    let meta = MemDir {
        name: "META".to_owned(),
        full: "FUZZDISC/BDMV/META".to_owned(),
        dirs: vec![dl],
        ..MemDir::default()
    };
    let bdmv = MemDir {
        name: "BDMV".to_owned(),
        full: "FUZZDISC/BDMV".to_owned(),
        dirs: vec![playlist, clipinf, stream, meta],
        files: vec![
            file("FUZZDISC/BDMV", "index.bdmv", index),
            file("FUZZDISC/BDMV", "MovieObject.bdmv", movie_object),
        ],
    };
    MemDir {
        name: "FUZZDISC".to_owned(),
        full: "FUZZDISC".to_owned(),
        dirs: vec![bdmv],
        ..MemDir::default()
    }
}

fuzz_target!(|data: &[u8]| {
    let root = build_tree(data);
    if let Ok(report) = BdRom::open_resilient(&root, true) {
        let _ = text::render(&report.bdrom, &report.errors);
    }
});
