//! In-memory backend for the [`vfs`](crate::vfs) seam — synthetic discs held
//! entirely in RAM.
//!
//! The third backend beside [`fs`](super::fs) (folder input) and
//! [`udf`](super::udf) (`.iso` input): [`MemFile`]/[`MemDir`] implement
//! [`BdFile`]/[`BdDir`] over shared byte buffers, for harnesses whose disc
//! never touches a filesystem — the browser (`bdinfo-rs-wasm`'s `scan_report`
//! export is handed BDMV bytes by JavaScript) and fuzzing (the `parse_report`
//! fuzz target turns each seed into a disc).
//!
//! ## Section framing
//!
//! Those two harnesses agree on one byte framing, defined here so a fuzz seed
//! means the same disc to both:
//!
//! - [`split_sections`] cuts a buffer into `u32` big-endian length-prefixed sections. The prefix is
//!   `u32` (not `u16`) so a real-scale `*.m2ts` stream section — megabytes — is expressible; a
//!   truncated trailing section yields the bytes present rather than an error.
//! - [`build_tree`] lays the first [`SECTION_COUNT`] sections, in order, onto a fixed BDMV skeleton
//!   under a caller-named root: `BDMV/index.bdmv`, `BDMV/MovieObject.bdmv`, `PLAYLIST/00000.mpls`,
//!   `CLIPINF/00000.clpi`, `STREAM/00000.m2ts`, and `META/DL/bdmt_eng.xml` (the [`roxmltree`] input
//!   — the one third-party parser fed disc bytes). A missing section leaves its file present but
//!   empty, exercising the resilient-open paths.
//!
//! A harness needing more than the skeleton appends entries through
//! [`MemDir::add_file`] — e.g. a `BDMV/STREAM/SSIF/00000.ssif` file, whose
//! directory is what marks a disc 3D.

use std::io::{self, BufRead, Cursor};
use std::sync::Arc;

use super::fs::glob_ci;
use super::{BdDir, BdFile, ReadSeek, SearchOption, extension_of_name};

/// A file over a shared byte buffer — the [`BdFile`] implementation for
/// in-memory input.
///
/// Cloning shares the buffer (an `Arc`), so handing a file into a tree walk or
/// opening it twice never copies the bytes.
#[derive(Debug, Clone)]
pub struct MemFile {
    name: String,
    full_name: String,
    /// Derived once at construction by [`extension_of_name`], the rule every
    /// built-in backend shares, and borrowed by [`BdFile::extension`].
    extension: String,
    data: Arc<[u8]>,
}

impl MemFile {
    /// Builds the file `name` under `dir` (its parent's full path — the full
    /// name becomes `dir`/`name`), holding `data`.
    #[must_use]
    pub fn new(dir: &str, name: &str, data: Vec<u8>) -> Self {
        Self {
            full_name: format!("{dir}/{name}"),
            extension: extension_of_name(name),
            name: name.to_owned(),
            data: Arc::from(data),
        }
    }
}

impl BdFile for MemFile {
    fn name(&self) -> &str {
        &self.name
    }

    fn full_name(&self) -> &str {
        &self.full_name
    }

    fn extension(&self) -> &str {
        &self.extension
    }

    fn length(&self) -> u64 {
        // usize -> u64 cannot lose width on any supported target; saturate
        // rather than unwrap so the conversion stays panic-free by construction.
        u64::try_from(self.data.len()).unwrap_or(u64::MAX)
    }

    fn is_dir(&self) -> bool {
        false
    }

    fn open_read(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(Cursor::new(Arc::clone(&self.data))))
    }

    fn open_text(&self) -> io::Result<Box<dyn BufRead>> {
        Ok(Box::new(Cursor::new(Arc::clone(&self.data))))
    }
}

/// A directory of in-memory entries — the [`BdDir`] implementation for
/// in-memory input.
///
/// Listings enumerate in insertion order, which the builder fixes at
/// construction — deterministic, with no filesystem involved.
#[derive(Debug, Clone)]
pub struct MemDir {
    name: String,
    full_name: String,
    dirs: Vec<Self>,
    files: Vec<MemFile>,
}

impl MemDir {
    /// Builds an empty root directory named `name`. Its full name is the bare
    /// `name` — a synthetic tree has no path above its root.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            full_name: name.to_owned(),
            dirs: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Adds the file `name` holding `data` at the end of the directory chain
    /// `dirs` below this directory, creating intermediate directories as
    /// needed; a directory already present under its chain name is descended
    /// into, never duplicated. An empty chain places the file directly here.
    pub fn add_file(&mut self, dirs: &[&str], name: &str, data: Vec<u8>) {
        let mut node = self;
        for &dir in dirs {
            let idx = if let Some(found) = node.dirs.iter().position(|d| d.name == dir) {
                found
            } else {
                let full_name = format!("{}/{dir}", node.full_name);
                node.dirs.push(Self {
                    name: dir.to_owned(),
                    full_name,
                    dirs: Vec::new(),
                    files: Vec::new(),
                });
                node.dirs.len().saturating_sub(1)
            };
            #[expect(
                clippy::indexing_slicing,
                reason = "idx is freshly found-or-pushed in node.dirs, always in bounds"
            )]
            let next = &mut node.dirs[idx];
            node = next;
        }
        let file = MemFile::new(&node.full_name, name, data);
        node.files.push(file);
    }

    /// Appends this directory's files matching `pattern` to `out`, recursing
    /// into subdirectories when `recurse` is set. Recursion depth equals tree
    /// depth, which only [`add_file`](Self::add_file) callers control — the
    /// framed disc bytes never shape the tree, so no depth cap is needed.
    fn collect_pattern(&self, pattern: &str, recurse: bool, out: &mut Vec<Box<dyn BdFile>>) {
        for file in &self.files {
            if glob_ci(pattern.as_bytes(), file.name.as_bytes()) {
                out.push(Box::new(file.clone()));
            }
        }
        if recurse {
            for dir in &self.dirs {
                dir.collect_pattern(pattern, recurse, out);
            }
        }
    }
}

impl BdDir for MemDir {
    fn name(&self) -> &str {
        &self.name
    }

    fn full_name(&self) -> &str {
        &self.full_name
    }

    fn parent(&self) -> Option<Box<dyn BdDir>> {
        // Handles are clones detached from the tree, so a child cannot point
        // back up. The scan's disc-root walk-up tolerates this: every built
        // tree wraps `BDMV` in a named root, so the walk finds the root before
        // it needs a parent.
        None
    }

    fn get_files(&self) -> io::Result<Vec<Box<dyn BdFile>>> {
        Ok(self.files.iter().map(|f| -> Box<dyn BdFile> { Box::new(f.clone()) }).collect())
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
        Ok(self.dirs.iter().map(|d| -> Box<dyn BdDir> { Box::new(d.clone()) }).collect())
    }
}

/// The number of sections [`build_tree`] lays onto the skeleton — one per
/// synthetic file, in the order the module docs list them.
pub const SECTION_COUNT: usize = 6;

/// Splits `data` into up to `max` `u32` big-endian length-prefixed sections
/// (the framing in the module docs).
///
/// A truncated trailing section yields the bytes present; anything after the
/// last complete 4-byte prefix is dropped.
#[must_use]
pub fn split_sections(data: &[u8], max: usize) -> Vec<Vec<u8>> {
    let mut sections: Vec<Vec<u8>> = Vec::new();
    let mut rest = data;
    while sections.len() < max {
        let Some((len_bytes, tail)) = rest.split_first_chunk::<4>() else { break };
        // Saturating on a <32-bit target (mirroring `udf::as_offset`); the
        // `min` below then clamps to what is actually present.
        let want = usize::try_from(u32::from_be_bytes(*len_bytes)).unwrap_or(usize::MAX);
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

/// Builds the synthetic disc tree: the first [`SECTION_COUNT`] of `sections`
/// laid in order onto the fixed BDMV skeleton (see the module docs) under a
/// root directory named `root_name`.
///
/// A missing section leaves its file present but empty. Sections beyond
/// [`SECTION_COUNT`] are ignored — a harness with a private extension (the
/// `parse_report` fuzz target's seventh, SSIF section) takes them off the
/// [`split_sections`] result before calling and places them through
/// [`MemDir::add_file`].
#[must_use]
pub fn build_tree(root_name: &str, sections: Vec<Vec<u8>>) -> MemDir {
    let mut next = sections.into_iter();
    let mut take = || next.next().unwrap_or_default();
    let mut root = MemDir::new(root_name);
    root.add_file(&["BDMV"], "index.bdmv", take());
    root.add_file(&["BDMV"], "MovieObject.bdmv", take());
    root.add_file(&["BDMV", "PLAYLIST"], "00000.mpls", take());
    root.add_file(&["BDMV", "CLIPINF"], "00000.clpi", take());
    root.add_file(&["BDMV", "STREAM"], "00000.m2ts", take());
    root.add_file(&["BDMV", "META", "DL"], "bdmt_eng.xml", take());
    root
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use proptest::prelude::{any, prop, prop_assert, prop_assert_eq, proptest};

    use super::{MemDir, MemFile, SECTION_COUNT, build_tree, split_sections};
    use crate::vfs::{BdDir, BdFile, SearchOption};

    /// Every file under `dir` as `(full_name, bytes)`, in walk order.
    fn all_files(dir: &MemDir) -> Vec<(String, Vec<u8>)> {
        dir.get_files_pattern_option("*", SearchOption::AllDirectories)
            .expect("in-memory walks cannot fail")
            .iter()
            .map(|file| {
                let mut bytes = Vec::new();
                file.open_read().expect("open").read_to_end(&mut bytes).expect("read");
                (file.full_name().to_owned(), bytes)
            })
            .collect()
    }

    #[test]
    fn mem_file_exposes_metadata_and_reads_as_bytes_and_text() {
        let file = MemFile::new("DISC/BDMV", "index.bdmv", b"hello".to_vec());
        assert_eq!(file.name(), "index.bdmv");
        assert_eq!(file.full_name(), "DISC/BDMV/index.bdmv");
        assert_eq!(file.extension(), ".bdmv");
        assert_eq!(file.length(), 5);
        assert!(!file.is_dir());

        // The reader is seekable (the parsers seek within disc files).
        let mut reader = file.open_read().expect("open_read");
        assert_eq!(reader.seek(SeekFrom::Start(1)).expect("seek"), 1);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("read bytes");
        assert_eq!(bytes, b"ello");

        let mut text = String::new();
        file.open_text().expect("open_text").read_to_string(&mut text).expect("read text");
        assert_eq!(text, "hello");

        // An extensionless name yields the empty extension, per the shared rule.
        assert_eq!(MemFile::new("DISC", "NOEXT", Vec::new()).extension(), "");
    }

    #[test]
    fn mem_types_render_debug() {
        // Exercise the derived Debug impls so coverage sees them.
        let mut root = MemDir::new("DISC");
        root.add_file(&["BDMV"], "index.bdmv", vec![1]);
        let rendered = format!("{root:?}");
        assert!(rendered.contains("BDMV"));
        assert!(rendered.contains("index.bdmv"));
    }

    #[test]
    fn dir_walk_matches_patterns_with_and_without_recursion() {
        let mut root = MemDir::new("DISC");
        root.add_file(&["BDMV"], "index.bdmv", vec![0_u8; 2]);
        root.add_file(&["BDMV", "STREAM"], "00000.m2ts", vec![0_u8; 4]);
        let bdmv = root.get_directories().expect("dirs").into_iter().next().expect("BDMV");

        assert_eq!(bdmv.name(), "BDMV");
        assert_eq!(bdmv.full_name(), "DISC/BDMV");
        assert!(bdmv.parent().is_none());
        assert_eq!(bdmv.get_files().expect("files").len(), 1);
        assert_eq!(bdmv.get_directories().expect("dirs").len(), 1);

        // TopDirectoryOnly does not descend into STREAM; AllDirectories does
        // (covers `collect_pattern`'s `if recurse` both ways). Matching is
        // ASCII case-insensitive, so the uppercase pattern finds the file too.
        assert_eq!(bdmv.get_files_pattern("*.m2ts").expect("shallow").len(), 0);
        assert_eq!(
            bdmv.get_files_pattern_option("*.M2TS", SearchOption::AllDirectories)
                .expect("deep")
                .len(),
            1
        );
        // A top-level match is found by either search.
        assert_eq!(bdmv.get_files_pattern("*.bdmv").expect("top match").len(), 1);
    }

    #[test]
    fn add_file_reuses_directories_and_derives_full_names() {
        let mut root = MemDir::new("DISC");
        root.add_file(&["BDMV", "PLAYLIST"], "00000.mpls", vec![b'a']);
        root.add_file(&["BDMV", "PLAYLIST"], "00001.mpls", vec![b'b']);
        root.add_file(&["BDMV", "CLIPINF"], "00000.clpi", vec![b'c']);
        // An empty chain places the file directly in this directory.
        root.add_file(&[], "readme.txt", vec![b'd']);

        // One BDMV, reused across all three chains, holding its subdirectories
        // in insertion order.
        assert_eq!(root.dirs.len(), 1);
        assert_eq!(root.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["readme.txt"]);
        let bdmv = root.dirs.first().expect("BDMV");
        assert_eq!(bdmv.full_name, "DISC/BDMV");
        assert_eq!(
            bdmv.dirs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["PLAYLIST", "CLIPINF"]
        );
        let playlist = bdmv.dirs.first().expect("PLAYLIST");
        assert_eq!(
            playlist.files.iter().map(|f| f.full_name.as_str()).collect::<Vec<_>>(),
            ["DISC/BDMV/PLAYLIST/00000.mpls", "DISC/BDMV/PLAYLIST/00001.mpls"]
        );
    }

    #[test]
    fn split_sections_frames_prefixed_sections_up_to_max() {
        // Two whole one-byte sections.
        assert_eq!(
            split_sections(&[0, 0, 0, 1, b'A', 0, 0, 0, 1, b'B'], 6),
            [vec![b'A'], vec![b'B']]
        );
        // A length that overruns truncates to what is present, then stops.
        assert_eq!(split_sections(&[0, 0, 0, 4, b'X', b'Y'], 6), [vec![b'X', b'Y']]);
        // No 4-byte length prefix at all yields no sections.
        assert!(split_sections(&[0, 0], 6).is_empty());
        // Never more than `max` sections, even with more length-prefixed data —
        // and `max` is exact, not one-off in either direction (32 zero bytes =
        // eight empty sections' worth of prefixes).
        let many: Vec<u8> = std::iter::repeat_n(0_u8, 32).collect();
        assert_eq!(split_sections(&many, 6).len(), 6);
        assert_eq!(split_sections(&many, 7).len(), 7);
        assert!(split_sections(&many, 0).is_empty());
    }

    #[test]
    fn build_tree_lays_the_sections_onto_the_skeleton_in_order() {
        let sections: Vec<Vec<u8>> = (1_u8..=6).map(|i| vec![i]).collect();
        let root = build_tree("WASMDISC", sections);
        assert_eq!(root.name(), "WASMDISC");
        assert_eq!(
            all_files(&root),
            [
                ("WASMDISC/BDMV/index.bdmv".to_owned(), vec![1]),
                ("WASMDISC/BDMV/MovieObject.bdmv".to_owned(), vec![2]),
                ("WASMDISC/BDMV/PLAYLIST/00000.mpls".to_owned(), vec![3]),
                ("WASMDISC/BDMV/CLIPINF/00000.clpi".to_owned(), vec![4]),
                ("WASMDISC/BDMV/STREAM/00000.m2ts".to_owned(), vec![5]),
                ("WASMDISC/BDMV/META/DL/bdmt_eng.xml".to_owned(), vec![6]),
            ]
        );
    }

    #[test]
    fn build_tree_leaves_missing_sections_empty_and_ignores_extras() {
        // Two sections: the remaining four files exist with zero bytes.
        let partial = build_tree("FUZZDISC", vec![vec![9], vec![8]]);
        let files = all_files(&partial);
        assert_eq!(files.len(), SECTION_COUNT);
        assert_eq!(
            files.iter().map(|(_, bytes)| bytes.len()).collect::<Vec<_>>(),
            [1, 1, 0, 0, 0, 0]
        );
        assert!(files.iter().all(|(full, _)| full.starts_with("FUZZDISC/BDMV")));

        // Sections beyond SECTION_COUNT never become files.
        let overfull = build_tree("DISC", vec![vec![7]; 8]);
        assert_eq!(all_files(&overfull).len(), SECTION_COUNT);
    }

    #[test]
    fn a_built_tree_is_extendable_with_an_ssif_entry() {
        // The extra-entry hook the fuzz harness's seventh section rides: the
        // SSIF directory appends under the existing STREAM, after its file.
        let mut root = build_tree("FUZZDISC", Vec::new());
        root.add_file(&["BDMV", "STREAM", "SSIF"], "00000.ssif", vec![b's']);
        let files = all_files(&root);
        assert_eq!(files.len(), SECTION_COUNT.saturating_add(1));
        assert!(
            files.iter().any(
                |(full, bytes)| full == "FUZZDISC/BDMV/STREAM/SSIF/00000.ssif" && bytes == b"s"
            )
        );
    }

    proptest! {
        #[test]
        fn split_sections_never_panics_and_bounds_hold(
            data in any::<Vec<u8>>(),
            max in 0_usize..10,
        ) {
            let sections = split_sections(&data, max);
            prop_assert!(sections.len() <= max);
            // Every returned byte was cut out of `data`, so the total payload
            // never exceeds the input.
            let payload: usize = sections.iter().map(Vec::len).sum();
            prop_assert!(payload <= data.len());
        }

        #[test]
        fn split_sections_round_trips_whole_sections(
            sections in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..8),
        ) {
            let mut data = Vec::new();
            for section in &sections {
                let len = u32::try_from(section.len()).expect("sections are < 2^32 bytes");
                data.extend_from_slice(&len.to_be_bytes());
                data.extend_from_slice(section);
            }
            prop_assert_eq!(split_sections(&data, sections.len()), sections);
        }
    }
}
