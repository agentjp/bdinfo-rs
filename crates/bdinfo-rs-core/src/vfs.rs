//! Virtual filesystem seam — the IO abstraction every BD/BDMV parser reads through.
//!
//! The [`BdFile`]/[`BdDir`] pair is the *entire platform surface* of the
//! analyzer: behind it the parse core never knows whether bytes come from
//! `std::fs` (folder input — see [`fs`]) or the UDF reader (`.iso` input — see
//! [`udf`]). A ~12-method file/directory pair is all the parsers need.
//!
//! Case-insensitive BDMV lookup is folded in here, done once and correctly and
//! routed through [`crate::discovery`] — see [`find_directory`] and
//! [`find_files`].

use std::io::{self, BufRead, Read, Seek};

use crate::discovery::{BdFileKind, BdmvDir};

pub mod fs;
pub mod udf;

/// A readable, seekable byte stream — the return type of [`BdFile::open_read`].
///
/// BD parsers seek within `.m2ts`/`.mpls`/`.clpi` files, so the seam yields
/// `Read + Seek`. It is blanket-implemented for every `Read + Seek`, so
/// [`std::fs::File`] — and the UDF reader's file streams — qualify without
/// extra glue.
pub trait ReadSeek: Read + Seek {}

impl<T: Read + Seek> ReadSeek for T {}

/// Whether a directory search recurses, used by
/// [`BdDir::get_files_pattern_option`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOption {
    /// Search only the top directory.
    TopDirectoryOnly,
    /// Recurse into every subdirectory.
    AllDirectories,
}

/// A file in a Blu-ray structure.
///
/// `Send` is a supertrait: the demux's read-ahead pipeline holds file handles
/// across a scoped-thread boundary, so every backend's handle must be safe to
/// move between threads (both backends hold only owned paths/`Arc`s).
pub trait BdFile: Send {
    /// The file name including extension, e.g. `00000.mpls`.
    fn name(&self) -> &str;

    /// The full path as opened.
    fn full_name(&self) -> &str;

    /// The extension *including* the leading dot, e.g. `.mpls`;
    /// the empty string when the name has no extension.
    fn extension(&self) -> &str;

    /// The file size in bytes.
    fn length(&self) -> u64;

    /// Whether this entry is a directory.
    fn is_dir(&self) -> bool;

    /// Opens the file for seekable reading.
    ///
    /// # Errors
    /// Propagates the underlying IO error if the file cannot be opened.
    fn open_read(&self) -> io::Result<Box<dyn ReadSeek>>;

    /// Opens the file as a buffered text reader.
    ///
    /// # Errors
    /// Propagates the underlying IO error if the file cannot be opened.
    fn open_text(&self) -> io::Result<Box<dyn BufRead>>;
}

/// A directory in a Blu-ray structure.
pub trait BdDir {
    /// The directory name, e.g. `PLAYLIST`.
    fn name(&self) -> &str;

    /// The full path as opened.
    fn full_name(&self) -> &str;

    /// The parent directory, or `None` at the filesystem root.
    fn parent(&self) -> Option<Box<dyn BdDir>>;

    /// Every file directly in this directory.
    ///
    /// # Errors
    /// Propagates the underlying IO error if the directory cannot be read.
    fn get_files(&self) -> io::Result<Vec<Box<dyn BdFile>>>;

    /// Files in this directory matching `pattern`.
    ///
    /// `pattern` is an ASCII case-insensitive glob (`*` = any run, `?` = any one
    /// character); see [`get_files_pattern_option`](BdDir::get_files_pattern_option).
    ///
    /// # Errors
    /// Propagates the underlying IO error if the directory cannot be read.
    fn get_files_pattern(&self, pattern: &str) -> io::Result<Vec<Box<dyn BdFile>>>;

    /// Files matching `pattern`, optionally recursing into subdirectories.
    ///
    /// Matching is ASCII case-insensitive — one pass finds `*.mpls` and
    /// `*.MPLS` spellings alike, no per-case retries needed.
    ///
    /// # Errors
    /// Propagates the underlying IO error if the directory cannot be read.
    fn get_files_pattern_option(
        &self,
        pattern: &str,
        option: SearchOption,
    ) -> io::Result<Vec<Box<dyn BdFile>>>;

    /// Every subdirectory of this directory.
    ///
    /// # Errors
    /// Propagates the underlying IO error if the directory cannot be read.
    fn get_directories(&self) -> io::Result<Vec<Box<dyn BdDir>>>;
}

/// Finds the child directory of `dir` classified as `which`, matching ASCII
/// case-insensitively via [`BdmvDir::from_name`].
///
/// This is the one place BDMV-directory names are resolved (the wiring of
/// [`crate::discovery`] into the VFS lookup), so the case rule lives in a single
/// spot. Returns `Ok(None)` when no such directory exists.
///
/// # Errors
/// Propagates the underlying IO error if `dir` cannot be enumerated.
pub fn find_directory(dir: &dyn BdDir, which: BdmvDir) -> io::Result<Option<Box<dyn BdDir>>> {
    for child in dir.get_directories()? {
        if BdmvDir::from_name(child.name()) == Some(which) {
            return Ok(Some(child));
        }
    }
    Ok(None)
}

/// Returns the files in `dir` whose names are of the given `kind`, matching ASCII
/// case-insensitively via [`BdFileKind::from_filename`].
///
/// One pass finds `*.mpls` and `*.MPLS` spellings alike — no double glob.
///
/// # Errors
/// Propagates the underlying IO error if `dir` cannot be enumerated.
pub fn find_files(dir: &dyn BdDir, kind: BdFileKind) -> io::Result<Vec<Box<dyn BdFile>>> {
    let mut out: Vec<Box<dyn BdFile>> = Vec::new();
    for file in dir.get_files()? {
        if BdFileKind::from_filename(file.name()) == Some(kind) {
            out.push(file);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::SearchOption;

    #[test]
    fn search_option_is_debug_and_eq() {
        // Exercise the derived Debug + PartialEq so coverage sees them.
        assert_eq!(format!("{:?}", SearchOption::AllDirectories), "AllDirectories");
        assert_eq!(SearchOption::TopDirectoryOnly, SearchOption::TopDirectoryOnly);
        assert_ne!(SearchOption::TopDirectoryOnly, SearchOption::AllDirectories);
    }
}
