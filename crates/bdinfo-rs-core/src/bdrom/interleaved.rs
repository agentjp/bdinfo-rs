//! Interleaved 3D stream file (`*.ssif`) — a named handle for the demux.
//!
//! A 3D Blu-ray stores the base-view and dependent-view (MVC) transport streams
//! interleaved in `BDMV/STREAM/SSIF/*.ssif` as packet-aligned extents.
//! [`TsInterleavedFile`] is only a name/handle holder because the actual reading
//! needs nothing special: it is the same 192-byte BDAV TS demux run over the whole
//! `.ssif`. The interleaved base/dependent extents are just more source packets, so
//! the byte-sequential parser (which resynchronises on the `0x47` sync byte)
//! de-interleaves them transparently onto the same PID/PES path that registers the
//! disc's elementary streams.
//!
//! [`TsStreamFile::scan_source`](super::m2ts::TsStreamFile::scan_source) selects
//! this interleaved file over the plain `*.m2ts` when it is present and SSIF reading
//! is enabled, so the dependent-view streams reach the demux at all.

use core::fmt;
use std::io;

use crate::vfs::{BdFile, ReadSeek};

/// A 3D interleaved stream file (`*.ssif`).
///
/// Holds the upper-cased [`name`](Self::name) and the VFS handle the demux
/// streams the interleaved base/dependent packets from.
pub struct TsInterleavedFile {
    /// The upper-cased file name, e.g. `"00000.SSIF"`.
    name: String,
    /// The VFS handle to the `.ssif` file.
    file: Box<dyn BdFile>,
}

impl TsInterleavedFile {
    /// Wraps `file` as an interleaved stream file, upper-casing its name.
    #[must_use]
    pub fn new(file: Box<dyn BdFile>) -> Self {
        Self { name: file.name().to_ascii_uppercase(), file }
    }

    /// The upper-cased file name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Opens the interleaved `.ssif` for seekable streaming.
    ///
    /// # Errors
    /// Propagates the underlying IO error if the file cannot be opened.
    pub fn open_read(&self) -> io::Result<Box<dyn ReadSeek>> {
        self.file.open_read()
    }
}

/// Manual `Debug` (the VFS handle is not `Debug`): shows only the name.
impl fmt::Debug for TsInterleavedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TsInterleavedFile").field("name", &self.name).finish_non_exhaustive()
    }
}

/// An in-memory [`BdFile`] over a fixed byte buffer — the test stand-in for a
/// `.ssif`/`.m2ts` handle, shared by this module's and [`super::m2ts`]'s tests so
/// only one mock backs the SSIF source-selection coverage.
#[cfg(test)]
pub(crate) struct MemBdFile {
    /// The file name (e.g. `"00000.SSIF"`).
    name: String,
    /// The extension including its leading dot (e.g. `".SSIF"`).
    extension: String,
    /// The file's bytes, handed out by [`open_read`](BdFile::open_read).
    bytes: Vec<u8>,
    /// When set, [`open_read`](BdFile::open_read) returns an error instead of a
    /// reader — to exercise the SSIF-open failure arm.
    fail: bool,
}

#[cfg(test)]
impl MemBdFile {
    /// Builds an in-memory file named `name` over `bytes`; `fail` makes `open_read`
    /// error.
    pub(crate) fn new(name: &str, bytes: Vec<u8>, fail: bool) -> Self {
        let extension =
            name.rsplit_once('.').map_or_else(String::new, |(_, ext)| format!(".{ext}"));
        Self { name: name.to_owned(), extension, bytes, fail }
    }
}

#[cfg(test)]
impl BdFile for MemBdFile {
    fn name(&self) -> &str {
        &self.name
    }

    fn full_name(&self) -> &str {
        &self.name
    }

    fn extension(&self) -> &str {
        &self.extension
    }

    fn length(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    fn is_dir(&self) -> bool {
        false
    }

    fn open_read(&self) -> io::Result<Box<dyn ReadSeek>> {
        if self.fail {
            Err(io::Error::other("injected ssif open failure"))
        } else {
            Ok(Box::new(io::Cursor::new(self.bytes.clone())))
        }
    }

    fn open_text(&self) -> io::Result<Box<dyn io::BufRead>> {
        Ok(Box::new(io::BufReader::new(io::Cursor::new(self.bytes.clone()))))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::{BdFile, MemBdFile, TsInterleavedFile};

    #[test]
    fn new_uppercases_the_name_and_streams_the_bytes() {
        let file = MemBdFile::new("00000.ssif", vec![1, 2, 3, 4], false);
        let interleaved = TsInterleavedFile::new(Box::new(file));
        assert_eq!(interleaved.name(), "00000.SSIF");

        let mut reader = interleaved.open_read().expect("open the interleaved file");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("read the interleaved file");
        assert_eq!(bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn open_read_propagates_the_io_error() {
        let file = MemBdFile::new("00000.ssif", vec![1], true);
        let interleaved = TsInterleavedFile::new(Box::new(file));
        assert!(interleaved.open_read().is_err());
    }

    #[test]
    fn debug_shows_the_name_without_the_handle() {
        let file = MemBdFile::new("00000.ssif", vec![0; 8], false);
        let interleaved = TsInterleavedFile::new(Box::new(file));
        let dump = format!("{interleaved:?}");
        assert!(dump.contains("TsInterleavedFile"));
        assert!(dump.contains("00000.SSIF"));
    }

    #[test]
    fn mem_bd_file_trait_surface_is_exercised() {
        // Touch the mock's trait methods that the SSIF demux path does not, so the
        // shared stand-in stays fully covered.
        let file = MemBdFile::new("00000.ssif", vec![9, 8, 7], false);
        assert_eq!(file.name(), "00000.ssif");
        assert_eq!(file.full_name(), "00000.ssif");
        assert_eq!(file.extension(), ".ssif");
        assert_eq!(file.length(), 3);
        assert!(!file.is_dir());
        let extensionless = MemBdFile::new("ssifnodot", Vec::new(), false);
        assert_eq!(extensionless.extension(), "");

        let mut text = String::new();
        file.open_text().expect("open_text").read_to_string(&mut text).expect("read text");
        assert_eq!(text, "\u{9}\u{8}\u{7}");
    }
}
