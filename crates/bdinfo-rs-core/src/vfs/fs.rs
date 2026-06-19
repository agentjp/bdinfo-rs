//! `std::fs` backend for the [`vfs`](crate::vfs) seam — extracted-folder input.
//!
//! [`FsFile`] and [`FsDir`] implement [`BdFile`]/[`BdDir`] over the host
//! filesystem. The contract:
//! - Lengths are `u64` — `std::fs` reports sizes as `u64`, and >4 GB streams are routine on BD
//!   media, so sizes stay `u64` everywhere, cast-free.
//! - Glob matching ([`glob_ci`]) is ASCII case-insensitive in one pass, so `*.mpls` and `*.MPLS`
//!   need no separate retries.
//! - Per-entry IO errors during enumeration are **recorded** into a shared sink
//!   ([`FsDir::take_errors`]) and the enumeration continues — surfaced, never silently dropped. The
//!   top-level read error still propagates as `Err`. Directory-ness comes from the listing's own
//!   [`file_type`](std::fs::DirEntry::file_type) (no extra follow-stat per entry — one fewer
//!   failure point on flaky media).
//! - The extension ([`extension_of`]) is the text after the last `.` verbatim (a lone `.` for a
//!   trailing-dot name); no further path normalization is applied.
//! - The full name ([`full_name_of`]) is the path as opened, not canonicalized to a rooted absolute
//!   path.

use std::fs::{File, metadata, read_dir};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use super::{BdDir, BdFile, ReadSeek, SearchOption};
use crate::error::{BdError, ScanError, ScanStage};

/// A file backed by [`std::fs`] — the [`BdFile`] implementation for folder
/// input.
///
/// Name, extension, length, and directory-ness are captured eagerly at
/// construction (one stat), so the [`BdFile`] accessors are infallible.
#[derive(Debug)]
pub struct FsFile {
    path: PathBuf,
    name: String,
    full_name: String,
    extension: String,
    length: u64,
    is_dir: bool,
}

impl FsFile {
    /// Builds an [`FsFile`] for `path`, reading its metadata once.
    ///
    /// # Errors
    /// Propagates the underlying IO error if `path`'s metadata cannot be read
    /// (e.g. it does not exist).
    pub fn from_full_name(path: PathBuf) -> io::Result<Self> {
        let meta = metadata(&path)?;
        Ok(Self {
            name: name_of(&path),
            full_name: full_name_of(&path),
            extension: extension_of(&path),
            length: meta.len(),
            is_dir: meta.is_dir(),
            path,
        })
    }
}

impl BdFile for FsFile {
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
        self.length
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }

    fn open_read(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(open_sequential(&self.path)?))
    }

    fn open_text(&self) -> io::Result<Box<dyn BufRead>> {
        Ok(Box::new(BufReader::new(File::open(&self.path)?)))
    }
}

/// Opens `path` for reading with the platform's sequential-access hint.
///
/// On Windows the open carries `FILE_FLAG_SEQUENTIAL_SCAN` (`0x0800_0000`), the
/// documented hint that makes the cache manager read ahead aggressively for a
/// front-to-back scan — exactly the demux's access pattern. Elsewhere a plain
/// open is used (POSIX readahead heuristics already favour sequential reads).
#[cfg(windows)]
pub(crate) fn open_sequential(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    /// `FILE_FLAG_SEQUENTIAL_SCAN` from `CreateFileW`'s `dwFlagsAndAttributes`.
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    std::fs::OpenOptions::new().read(true).custom_flags(FILE_FLAG_SEQUENTIAL_SCAN).open(path)
}

/// Opens `path` for reading (the non-Windows arm of the sequential-hint open).
#[cfg(not(windows))]
pub(crate) fn open_sequential(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// A directory backed by [`std::fs`] — the [`BdDir`] implementation for folder
/// input.
///
/// Every handle derived from one root (children via
/// [`get_directories`](BdDir::get_directories), the parent, recursion) shares one error sink: a
/// per-entry enumeration failure is recorded there and the enumeration continues,
/// so a damaged entry is surfaced — never silently dropped. Drain it with
/// [`take_errors`](FsDir::take_errors) after a scan.
#[derive(Debug)]
pub struct FsDir {
    path: PathBuf,
    name: String,
    full_name: String,
    /// The shared per-entry enumeration-failure sink (see the type docs).
    errors: Arc<Mutex<Vec<ScanError>>>,
}

/// One enumerated entry, reduced to what the walk needs: its path and whether the
/// listing says it is a directory (from [`std::fs::DirEntry::file_type`] — no
/// extra follow-stat).
type RawEntry = (PathBuf, bool);

/// The directory-listing seam [`walk_files`] reads through — production passes
/// [`list_dir`]; tests inject failing listers to exercise every error arm
/// deterministically (a real filesystem cannot fail a *chosen* entry on demand).
type Lister<'a> =
    &'a mut dyn FnMut(&Path) -> io::Result<Box<dyn Iterator<Item = io::Result<RawEntry>>>>;

/// Lists `path` as [`RawEntry`] items — the production [`Lister`]: the
/// `read_dir` failure surfaces as the outer `Err`, and each entry's own
/// listing/`file_type` failure flows through as an `Err` item.
fn list_dir(path: &Path) -> io::Result<Box<dyn Iterator<Item = io::Result<RawEntry>>>> {
    Ok(Box::new(
        read_dir(path)?
            .map(|entry| entry.and_then(|e| e.file_type().map(|t| (e.path(), t.is_dir())))),
    ))
}

/// Recursively collects the files under `dir` matching `pattern` into `out`,
/// reading the filesystem through `list` and reporting each per-entry failure
/// (a failed entry read, a failed stat, an unenumerable subdirectory) through
/// `record` — the collect-and-continue walk behind
/// [`BdDir::get_files_pattern_option`].
///
/// # Errors
/// Only the **top-level** `list(dir)` failure propagates (an unreadable
/// starting directory is the caller's error); every failure below it is
/// recorded and the walk continues.
fn walk_files(
    dir: &Path,
    pattern: &[u8],
    option: SearchOption,
    list: Lister<'_>,
    record: &mut dyn FnMut(PathBuf, io::Error),
    out: &mut Vec<Box<dyn BdFile>>,
) -> io::Result<()> {
    for entry in list(dir)? {
        match entry {
            Ok((path, true)) => {
                if option == SearchOption::AllDirectories
                    && let Err(err) =
                        walk_files(&path, pattern, option, &mut *list, &mut *record, out)
                {
                    record(path, err);
                }
            }
            Ok((path, false)) => {
                if glob_ci(pattern, name_of(&path).as_bytes()) {
                    match FsFile::from_full_name(path.clone()) {
                        Ok(file) => out.push(Box::new(file)),
                        Err(err) => record(path, err),
                    }
                }
            }
            Err(err) => record(dir.to_path_buf(), err),
        }
    }
    Ok(())
}

/// Collects the directory entries of one listing of `dir` into `push`, reporting
/// each per-entry failure through `record` — the collect-and-continue core behind
/// [`BdDir::get_directories`].
fn collect_directories(
    dir: &Path,
    entries: impl Iterator<Item = io::Result<RawEntry>>,
    record: &mut dyn FnMut(PathBuf, io::Error),
    mut push: impl FnMut(PathBuf),
) {
    for entry in entries {
        match entry {
            Ok((path, true)) => push(path),
            Ok((_, false)) => {}
            Err(err) => record(dir.to_path_buf(), err),
        }
    }
}

/// A walk recorder appending each per-entry failure to `sink` as a
/// [`ScanStage::Discovery`] [`ScanError`] — the one recording path shared by
/// [`walk_files`] and [`collect_directories`] callers.
fn record_into(sink: &Arc<Mutex<Vec<ScanError>>>) -> impl FnMut(PathBuf, io::Error) + '_ {
    move |path, err| {
        sink.lock().unwrap_or_else(PoisonError::into_inner).push(ScanError {
            file: full_name_of(&path),
            stage: ScanStage::Discovery,
            reason: BdError::Io(err),
        });
    }
}

impl FsDir {
    /// Opens the directory at `path`. Construction is pure — it touches the
    /// filesystem only when the directory is later enumerated or opened.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            name: name_of(&path),
            full_name: full_name_of(&path),
            path,
            errors: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A handle on `path` sharing this directory's error sink — children, parents,
    /// and recursion all record into the one root sink.
    fn related(&self, path: PathBuf) -> Self {
        Self {
            name: name_of(&path),
            full_name: full_name_of(&path),
            path,
            errors: Arc::clone(&self.errors),
        }
    }

    /// Drains the per-entry enumeration failures recorded since the last call
    /// (across this handle and every child/parent handle derived from it).
    ///
    /// Empty after any walk over healthy media; on damaged media each entry that
    /// could not be read or stat-ed is one [`ScanError`] at
    /// [`ScanStage::Discovery`].
    #[must_use]
    pub fn take_errors(&self) -> Vec<ScanError> {
        std::mem::take(&mut *self.errors.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl BdDir for FsDir {
    fn name(&self) -> &str {
        &self.name
    }

    fn full_name(&self) -> &str {
        &self.full_name
    }

    fn parent(&self) -> Option<Box<dyn BdDir>> {
        let parent = self.path.parent()?;
        // A bare relative name's parent is the empty path; open it as `.` so the
        // handle stays enumerable (the disc-root ancestor walk lands here when
        // the input is a bare `BDMV`).
        let parent = if parent.as_os_str().is_empty() { Path::new(".") } else { parent };
        Some(Box::new(self.related(parent.to_path_buf())))
    }

    fn get_files(&self) -> io::Result<Vec<Box<dyn BdFile>>> {
        self.get_files_pattern("*")
    }

    fn get_files_pattern(&self, pattern: &str) -> io::Result<Vec<Box<dyn BdFile>>> {
        self.get_files_pattern_option(pattern, SearchOption::TopDirectoryOnly)
    }

    fn get_files_pattern_option(
        &self,
        pattern: &str,
        option: SearchOption,
    ) -> io::Result<Vec<Box<dyn BdFile>>> {
        let mut out: Vec<Box<dyn BdFile>> = Vec::new();
        walk_files(
            &self.path,
            pattern.as_bytes(),
            option,
            &mut list_dir,
            &mut record_into(&self.errors),
            &mut out,
        )?;
        Ok(out)
    }

    fn get_directories(&self) -> io::Result<Vec<Box<dyn BdDir>>> {
        let mut out: Vec<Box<dyn BdDir>> = Vec::new();
        collect_directories(
            &self.path,
            list_dir(&self.path)?,
            &mut record_into(&self.errors),
            |path| out.push(Box::new(self.related(path))),
        );
        Ok(out)
    }
}

/// The file name including extension, or — only for a root/prefix path with no
/// final component — the path itself.
fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The path as opened, rendered to a `String`. Not canonicalized to a rooted
/// absolute path: BD parsing never depends on absoluteness, and skipping it
/// stays panic-free.
fn full_name_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The extension *including* the leading dot, e.g. `.mpls`; the empty string
/// when there is no name or no `.` in it. The text after the last `.` is
/// returned verbatim (a lone `.` for a trailing-dot name like `00000.`); no
/// further path normalization is applied.
fn extension_of(path: &Path) -> String {
    let Some(name) = path.file_name() else {
        return String::new();
    };
    match name.to_string_lossy().rsplit_once('.') {
        Some((_, ext)) => format!(".{ext}"),
        None => String::new(),
    }
}

/// ASCII case-insensitive glob match of `name` against `pattern`, where `*`
/// matches any run (including empty) and `?` matches exactly one byte. One
/// case-folded pass finds `*.mpls` and `*.MPLS` spellings alike.
///
/// Shared with the UDF backend ([`super::udf`]) so `.iso` and folder input
/// match patterns identically.
pub(crate) fn glob_ci(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((&b'*', rest)) => {
            glob_ci(rest, name)
                || matches!(name.split_first(), Some((_, tail)) if glob_ci(pattern, tail))
        }
        Some((&b'?', rest)) => {
            matches!(name.split_first(), Some((_, tail)) if glob_ci(rest, tail))
        }
        Some((&pc, rest)) => {
            matches!(
                name.split_first(),
                Some((&nc, tail)) if pc.eq_ignore_ascii_case(&nc) && glob_ci(rest, tail)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read, Seek, SeekFrom};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use proptest::prelude::{any, prop_assert, proptest};

    use super::{FsDir, FsFile, RawEntry, collect_directories, glob_ci, record_into, walk_files};
    use crate::discovery::{BdFileKind, BdmvDir};
    use crate::error::ScanStage;
    use crate::vfs::{BdDir, BdFile, SearchOption, find_directory, find_files};

    /// A throwaway BD-shaped folder under the system temp dir, removed on drop.
    /// Directory names deliberately use the wrong case to prove case-insensitive
    /// resolution on case-sensitive filesystems.
    struct TempDisc {
        root: PathBuf,
    }

    impl TempDisc {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut root = std::env::temp_dir();
            root.push(format!("bdinfo-rs-vfs-{}-{unique}", std::process::id()));

            let bdmv = root.join("bdmv"); // lowercase on purpose
            let playlist = bdmv.join("PlayList"); // mixed case on purpose
            let stream = bdmv.join("STREAM");
            let stream_sub = stream.join("sub");
            let clipinf = bdmv.join("clipinf");
            for dir in [&playlist, &stream, &stream_sub, &clipinf] {
                std::fs::create_dir_all(dir).expect("create fixture dir");
            }

            std::fs::write(bdmv.join("NOEXT"), b"x").expect("write NOEXT");
            std::fs::write(playlist.join("00000.mpls"), b"ABCDEFGH").expect("write mpls");
            std::fs::write(playlist.join("00001.MPLS"), b"second").expect("write MPLS");
            std::fs::write(playlist.join("readme.txt"), b"note").expect("write readme");
            std::fs::write(stream.join("00000.m2ts"), b"ts0").expect("write m2ts");
            std::fs::write(stream.join("notes.txt"), b"n").expect("write notes");
            std::fs::write(stream_sub.join("00001.m2ts"), b"ts1").expect("write sub m2ts");
            std::fs::write(clipinf.join("00000.clpi"), b"clip").expect("write clpi");

            Self { root }
        }

        fn bdmv(&self) -> FsDir {
            FsDir::new(self.root.join("bdmv"))
        }
    }

    impl Drop for TempDisc {
        fn drop(&mut self) {
            // Best-effort: a stray temp dir is harmless if removal races.
            let _ = std::fs::remove_dir_all(&self.root).is_ok();
        }
    }

    /// The filesystem root (`/` or `C:\`): exists, but has no final component.
    fn filesystem_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .last()
            .expect("a path has a root ancestor")
            .to_path_buf()
    }

    fn names(files: &[Box<dyn BdFile>]) -> Vec<String> {
        files.iter().map(|f| f.name().to_owned()).collect()
    }

    #[test]
    fn finds_bdmv_case_insensitively_then_subdirs() {
        let disc = TempDisc::new();
        let root = FsDir::new(disc.root.clone());

        let bdmv =
            find_directory(&root, BdmvDir::Bdmv).expect("enumerate root").expect("bdmv present");
        assert_eq!(bdmv.name(), "bdmv");

        // A kind that is not a child of root → Ok(None).
        assert!(find_directory(&root, BdmvDir::Playlist).expect("enumerate root").is_none());

        for which in [BdmvDir::Playlist, BdmvDir::Stream, BdmvDir::ClipInf] {
            assert!(
                find_directory(&*bdmv, which).expect("enumerate bdmv").is_some(),
                "expected {which:?} under BDMV"
            );
        }
    }

    #[test]
    fn find_files_filters_by_kind() {
        let disc = TempDisc::new();
        let bdmv = disc.bdmv();
        let playlist =
            find_directory(&bdmv, BdmvDir::Playlist).expect("enumerate").expect("playlist");

        let mut mpls = names(&find_files(&*playlist, BdFileKind::Playlist).expect("find mpls"));
        mpls.sort();
        assert_eq!(mpls, vec!["00000.mpls".to_owned(), "00001.MPLS".to_owned()]);

        // No stream files live in PLAYLIST.
        assert!(find_files(&*playlist, BdFileKind::Stream).expect("find none").is_empty());
    }

    #[test]
    fn get_files_returns_files_get_directories_returns_dirs() {
        let disc = TempDisc::new();
        let bdmv = disc.bdmv();

        // GetFiles on BDMV yields only the file (the three subdirs are skipped).
        let files = bdmv.get_files().expect("get_files");
        assert_eq!(names(&files), vec!["NOEXT".to_owned()]);

        // GetDirectories on BDMV yields only the subdirs (the file is skipped).
        let mut dirs: Vec<String> = bdmv
            .get_directories()
            .expect("get_directories")
            .iter()
            .map(|d| d.name().to_owned())
            .collect();
        dirs.sort();
        assert_eq!(dirs, vec!["PlayList".to_owned(), "STREAM".to_owned(), "clipinf".to_owned()]);
    }

    #[test]
    fn get_files_pattern_is_case_insensitive() {
        let disc = TempDisc::new();
        let playlist =
            find_directory(&disc.bdmv(), BdmvDir::Playlist).expect("enum").expect("playlist");

        // Lowercase pattern matches the uppercase-extension file too.
        let mut hits = names(&playlist.get_files_pattern("*.mpls").expect("glob"));
        hits.sort();
        assert_eq!(hits, vec!["00000.mpls".to_owned(), "00001.MPLS".to_owned()]);

        // "*" matches everything in the directory.
        assert_eq!(playlist.get_files_pattern("*").expect("glob all").len(), 3);
    }

    #[test]
    fn all_directories_recurses_top_does_not() {
        let disc = TempDisc::new();
        let stream = find_directory(&disc.bdmv(), BdmvDir::Stream).expect("enum").expect("stream");

        let top =
            stream.get_files_pattern_option("*.m2ts", SearchOption::TopDirectoryOnly).expect("top");
        assert_eq!(names(&top), vec!["00000.m2ts".to_owned()]);

        let mut deep = names(
            &stream.get_files_pattern_option("*.m2ts", SearchOption::AllDirectories).expect("deep"),
        );
        deep.sort();
        assert_eq!(deep, vec!["00000.m2ts".to_owned(), "00001.m2ts".to_owned()]);
    }

    #[test]
    fn file_metadata_and_open_read_seek() {
        let disc = TempDisc::new();
        let mpls = disc.root.join("bdmv").join("PlayList").join("00000.mpls");
        let file = FsFile::from_full_name(mpls).expect("from_full_name");

        assert_eq!(file.name(), "00000.mpls");
        assert_eq!(file.extension(), ".mpls");
        assert_eq!(file.length(), 8);
        assert!(!file.is_dir());
        assert!(file.full_name().ends_with("00000.mpls"));

        let mut reader = file.open_read().expect("open_read");
        assert_eq!(reader.seek(SeekFrom::Start(4)).expect("seek"), 4);
        let mut buf = [0_u8; 4];
        reader.read_exact(&mut buf).expect("read");
        assert_eq!(buf, *b"EFGH");

        let mut text = String::new();
        file.open_text().expect("open_text").read_to_string(&mut text).expect("read text");
        assert_eq!(text, "ABCDEFGH");
    }

    #[test]
    fn directory_as_file_is_dir_and_has_no_extension() {
        let disc = TempDisc::new();
        let file = FsFile::from_full_name(disc.root.join("bdmv")).expect("from_full_name dir");
        assert!(file.is_dir());
        assert_eq!(file.extension(), "");
    }

    #[test]
    fn parent_of_dir_is_some_root_is_none() {
        let disc = TempDisc::new();
        let bdmv = disc.bdmv();
        let parent = bdmv.parent().expect("bdmv has a parent");
        assert!(
            parent.full_name().ends_with(
                disc.root.file_name().expect("root has a name").to_string_lossy().as_ref()
            )
        );

        assert!(FsDir::new(filesystem_root()).parent().is_none());
    }

    #[test]
    fn parent_of_a_bare_relative_name_is_the_current_directory() {
        // `Path::parent` yields the EMPTY path for a single relative component;
        // the seam opens it as `.` so the parent stays enumerable.
        let parent = FsDir::new("BDMV").parent().expect("bare name has a parent");
        assert_eq!(parent.full_name(), ".");
        assert!(parent.get_directories().is_ok());
    }

    #[test]
    fn filesystem_root_name_falls_back_and_has_no_extension() {
        let root = filesystem_root();
        assert!(!FsDir::new(root.clone()).name().is_empty());
        // FromFullName on the root: no final component → empty extension, infallible name.
        let as_file = FsFile::from_full_name(root).expect("root metadata");
        assert!(as_file.is_dir());
        assert_eq!(as_file.extension(), "");
        assert!(!as_file.name().is_empty());
    }

    #[test]
    fn open_errors_after_file_is_removed() {
        let disc = TempDisc::new();
        let path = disc.root.join("bdmv").join("PlayList").join("00000.mpls");
        let file = FsFile::from_full_name(path.clone()).expect("from_full_name");
        std::fs::remove_file(&path).expect("remove file");
        assert!(file.open_read().is_err());
        assert!(file.open_text().is_err());
    }

    #[test]
    fn from_full_name_missing_path_errors() {
        let disc = TempDisc::new();
        assert!(FsFile::from_full_name(disc.root.join("nope")).is_err());
    }

    #[test]
    fn enumerating_a_missing_directory_errors() {
        let disc = TempDisc::new();
        let missing = FsDir::new(disc.root.join("nope"));
        assert!(missing.get_directories().is_err());
        assert!(missing.get_files_pattern_option("*", SearchOption::TopDirectoryOnly).is_err());
        assert!(find_directory(&missing, BdmvDir::Bdmv).is_err());
        assert!(find_files(&missing, BdFileKind::Playlist).is_err());
    }

    // ── per-entry fault injection (a real filesystem cannot fail a chosen entry,
    //    so the walk's error arms are exercised through the lister seam) ────────

    /// The recorded `(file, message)` pairs drained from a sink.
    fn drained(sink: &Arc<Mutex<Vec<crate::error::ScanError>>>) -> Vec<(String, String)> {
        std::mem::take(&mut *sink.lock().expect("sink lock"))
            .into_iter()
            .map(|e| (e.file, e.reason.to_string()))
            .collect()
    }

    #[test]
    fn walk_files_records_every_per_entry_failure_and_keeps_walking() {
        let disc = TempDisc::new();
        let good = disc.root.join("bdmv").join("PlayList").join("00000.mpls");
        let missing = disc.root.join("vanished.mpls");
        let ghost_dir = disc.root.join("ghost");
        let healthy_dir = disc.root.join("healthy");

        // A lister that fails to enumerate `ghost` (the subdirectory-recursion
        // failure) and, for the root, yields: a failed entry read, an entry whose
        // stat will fail (the path does not exist), a good file, the ghost
        // subdirectory, and a healthy (empty) subdirectory.
        let entries = move |root: bool| -> Vec<io::Result<RawEntry>> {
            if root {
                vec![
                    Err(io::Error::other("entry unreadable")),
                    Ok((missing.clone(), false)),
                    Ok((good.clone(), false)),
                    Ok((ghost_dir.clone(), true)),
                    Ok((healthy_dir.clone(), true)),
                ]
            } else {
                Vec::new() // the healthy subdirectory is empty
            }
        };
        let root = disc.root.clone();
        let mut lister =
            |path: &Path| -> io::Result<Box<dyn Iterator<Item = io::Result<RawEntry>>>> {
                if path.ends_with("ghost") {
                    Err(io::Error::other("subdir unreadable"))
                } else {
                    Ok(Box::new(entries(path == root).into_iter()))
                }
            };

        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut out: Vec<Box<dyn BdFile>> = Vec::new();
        walk_files(
            &disc.root,
            b"*.mpls",
            SearchOption::AllDirectories,
            &mut lister,
            &mut record_into(&sink),
            &mut out,
        )
        .expect("top-level listing succeeds");

        // The readable file is collected; the three failures are recorded.
        assert_eq!(names(&out), vec!["00000.mpls".to_owned()]);
        let recorded = drained(&sink);
        assert_eq!(recorded.len(), 3);
        // The failed entry read is attributed to the directory being walked.
        assert!(recorded.first().expect("entry error").0.ends_with(&disc_root_name(&disc)));
        assert!(recorded.first().expect("entry error").1.contains("entry unreadable"));
        // The failed stat is attributed to the vanished file.
        assert!(recorded.get(1).expect("stat error").0.ends_with("vanished.mpls"));
        // The unenumerable subdirectory is attributed to itself.
        assert!(recorded.get(2).expect("subdir error").0.ends_with("ghost"));
        assert!(recorded.get(2).expect("subdir error").1.contains("subdir unreadable"));
    }

    /// The final component of the fixture root (the walked directory's name).
    fn disc_root_name(disc: &TempDisc) -> String {
        disc.root.file_name().expect("fixture root has a name").to_string_lossy().into_owned()
    }

    #[test]
    fn walk_files_skips_subdirectories_and_non_matches_at_top_level() {
        // TopDirectoryOnly never invokes the lister for a subdirectory (even one
        // that would fail), and a non-matching name is neither collected nor a
        // recorded error.
        let disc = TempDisc::new();
        let good = disc.root.join("bdmv").join("PlayList").join("00000.mpls");
        let ghost_dir = disc.root.join("ghost");
        let mut lister =
            |path: &Path| -> io::Result<Box<dyn Iterator<Item = io::Result<RawEntry>>>> {
                assert!(!path.ends_with("ghost"), "TopDirectoryOnly must not recurse");
                Ok(Box::new(
                    vec![Ok((good.clone(), false)), Ok((ghost_dir.clone(), true))].into_iter(),
                ))
            };
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut out: Vec<Box<dyn BdFile>> = Vec::new();
        walk_files(
            &disc.root,
            b"*.clpi", // does not match 00000.mpls
            SearchOption::TopDirectoryOnly,
            &mut lister,
            &mut record_into(&sink),
            &mut out,
        )
        .expect("walk");
        assert!(out.is_empty());
        assert!(drained(&sink).is_empty());
    }

    #[test]
    fn walk_files_propagates_only_the_top_level_listing_failure() {
        let mut lister = |_: &Path| -> io::Result<Box<dyn Iterator<Item = io::Result<RawEntry>>>> {
            Err(io::Error::other("root unreadable"))
        };
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut out: Vec<Box<dyn BdFile>> = Vec::new();
        let err = walk_files(
            Path::new("x"),
            b"*",
            SearchOption::AllDirectories,
            &mut lister,
            &mut record_into(&sink),
            &mut out,
        )
        .expect_err("top-level failure propagates");
        assert_eq!(err.to_string(), "root unreadable");
        assert!(drained(&sink).is_empty());
    }

    #[test]
    fn collect_directories_records_entry_failures_and_keeps_collecting() {
        let entries: Vec<io::Result<RawEntry>> = vec![
            Err(io::Error::other("entry unreadable")),
            Ok((PathBuf::from("sub"), true)),
            Ok((PathBuf::from("file.bin"), false)),
        ];
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut dirs = Vec::new();
        collect_directories(
            Path::new("the-dir"),
            entries.into_iter(),
            &mut record_into(&sink),
            |path| dirs.push(path),
        );
        assert_eq!(dirs, vec![PathBuf::from("sub")]); // the file entry is skipped
        let recorded = drained(&sink);
        assert_eq!(recorded.len(), 1);
        // The entry failure is attributed to the directory being listed.
        assert_eq!(recorded.first().expect("recorded").0, "the-dir");
    }

    #[test]
    fn error_sink_is_shared_across_related_handles_and_drains() {
        let disc = TempDisc::new();
        let root = FsDir::new(disc.root.clone());
        assert!(root.take_errors().is_empty()); // clean before any walk

        // A clean enumeration over healthy media records nothing.
        let bdmv = root.get_directories().expect("dirs");
        assert!(!bdmv.is_empty());
        assert!(root.take_errors().is_empty());

        // A child handle records into the root's sink (one shared sink per tree).
        let child = root.related(disc.root.join("bdmv"));
        record_into(&child.errors)(disc.root.join("hurt.mpls"), io::Error::other("bad entry"));
        let errors = root.take_errors();
        assert_eq!(errors.len(), 1);
        let first = errors.first().expect("one recorded error");
        assert_eq!(first.stage, ScanStage::Discovery);
        assert!(first.file.ends_with("hurt.mpls"));
        assert_eq!(first.reason.to_string(), "io error: bad entry");
        // Draining empties the sink for the whole tree.
        assert!(child.take_errors().is_empty());
    }

    #[test]
    fn glob_ci_covers_every_arm() {
        // Literal match, case-insensitive.
        assert!(glob_ci(b"00000.MPLS", b"00000.mpls"));
        // Literal mismatch (eq false) and trailing-pattern mismatch (None arm false).
        assert!(!glob_ci(b"a", b"b"));
        assert!(!glob_ci(b"ab", b"abc"));
        // Exact literal, both consumed (None arm true).
        assert!(glob_ci(b"ab", b"ab"));
        // '*' — zero, some, and "no progress possible" cases.
        assert!(glob_ci(b"*", b""));
        assert!(glob_ci(b"*.mpls", b"00000.mpls"));
        assert!(glob_ci(b"*x", b"yx"));
        assert!(!glob_ci(b"*a", b""));
        assert!(!glob_ci(b"*.mpls", b"00000.m2ts"));
        // '?' — match, mismatch, and empty-name cases.
        assert!(glob_ci(b"?", b"x"));
        assert!(!glob_ci(b"?x", b"yz"));
        assert!(!glob_ci(b"?", b""));
        // Literal vs empty name (None on the name side).
        assert!(!glob_ci(b"a", b""));
        // Literal match but rest mismatches (eq true, recurse false).
        assert!(!glob_ci(b"ab", b"ax"));
    }

    proptest! {
        #[test]
        fn glob_ci_never_panics(pattern in any::<Vec<u8>>(), name in any::<Vec<u8>>()) {
            let _ = glob_ci(&pattern, &name);
        }

        #[test]
        fn star_matches_anything(name in any::<Vec<u8>>()) {
            prop_assert!(glob_ci(b"*", &name));
        }
    }
}
