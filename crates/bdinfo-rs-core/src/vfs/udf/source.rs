//! `UdfSource` — the [`vfs`](crate::vfs) `BdDir`/`BdFile` backend over a seekable
//! `.iso`.
//!
//! This is the integration layer that puts the UDF parsing core
//! ([`descriptor`](super::descriptor), [`icb`](super::icb), [`fid`](super::fid),
//! [`cs0`](super::cs0)) behind the same [`BdDir`]/[`BdFile`] seam
//! the folder backend ([`super::super::fs`]) implements, so **every BD/M2TS parser
//! runs unchanged over an `.iso`** — the vfs seam is the only surface either side
//! sees. The open sequence: AVDP (sector 256, falling back to the end-of-image
//! anchors) → the Main Volume Descriptor
//! Sequence (Logical Volume Descriptor + Partition Descriptors) → resolve each
//! partition reference (physical, or a UDF 2.50 Metadata partition through the
//! metadata file's extents) → the File Set Descriptor → the root directory's File
//! Entry → its directory tree.
//!
//! The volume label this exposes as the root
//! directory's [`name`](BdDir::name) is the real UDF descriptor label
//! (`LogicalVolumeIdentifier`), so `BdRom`'s `volume_label = root.name()` reads the
//! genuine label for `.iso` input (folder input falls back to the directory name).
//! That is the same string Windows shows for a mounted UDF volume — and therefore
//! what classic `BDInfo` displayed — verified by mounting a label-patched image:
//! `udfs.sys` reports the File Set Descriptor's `LogicalVolumeIdentifier` copy,
//! which UDF 2.50 §2.3.2 requires to equal the LVD's. When the LVD identifier is
//! empty, the PVD's `VolumeIdentifier` (the field libudfread reports) is the
//! fallback.
//!
//! ## Design
//!
//! [`UdfSource::open`] parses the volume once and walks the **entire** directory
//! tree into a flat arena of [`Node`]s (resolving every file's allocation extents
//! to absolute byte runs), so the [`BdDir`]/[`BdFile`] accessors are infallible
//! arena lookups — directory data is read at open, file data is read lazily through
//! [`UdfFileReader`]. The walk is bounded ([`Limits::max_nodes`], a visited-ICB set,
//! and [`Limits::max_depth`]) so malformed or cyclic input can never hang — and,
//! because the depth cap bounds how deep the arena's directory chain can run, the
//! recursive arena consumers (glob recursion here, `directory_size` in the BDROM
//! scan) cannot overflow the thread stack either (the crate-wide contract: never
//! panic/hang on disc bytes). `u64` byte offsets throughout (a Blu-ray `.iso` is
//! tens of GB, its `.m2ts`/`.ssif` streams routinely >4 GB).
//!
//! All numeric *parsing* stays in the little-endian [`super`] core; this module
//! only does byte-offset arithmetic over
//! the resolved structures, so it holds no endianness of its own.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use super::descriptor::{
    Avdp, Fsd, Lvd, PartitionDescriptor, PartitionMap, Pvd, Vdp, physical_partition_start,
};
use super::fid::parse_directory;
use super::icb::{FileEntry, parse_allocation_area};
use super::{
    Extent, ExtentAd, ExtentKind, LongAd, TAG_ALLOCATION_EXTENT, TAG_TERMINATING, Tag, as_offset,
    u32_le,
};
use crate::error::{BdError, ScanError, ScanStage};
use crate::vfs::fs::glob_ci;
use crate::vfs::{BdDir, BdFile, ReadSeek, SearchOption};

/// The Anchor Volume Descriptor Pointer's fixed logical sector (ECMA-167 §3/10.2).
const ANCHOR_SECTOR: u64 = 256;
/// The bootstrap logical-sector size used to read the AVDP and the volume
/// descriptor sequence, before the LVD's `LogicalBlockSize` is known (2048 for a
/// Blu-ray — the universal UDF assumption).
const BOOTSTRAP_SECTOR_SIZE: u64 = 2048;
/// A sane cap on how many descriptor sectors of the Main Volume Descriptor
/// Sequence to scan (generously sized — real discs use a handful).
const MAX_VDS_SECTORS: u64 = 256;
/// The largest `LogicalBlockSize` the reader accepts. Every authored Blu-ray uses
/// 2048 (libudfread hard-rejects anything else); 32 KiB is a conservative ceiling
/// that keeps a hostile block-size field from sizing multi-GB sector buffers
/// ([`read_sector_io`] allocates `block_size` bytes per read).
const MAX_BLOCK_SIZE: u64 = 32 << 10;
/// The bounds that keep the directory-tree walk finite over malformed or hostile
/// input — never panic/hang on disc bytes. Carried as a value (not bare
/// consts) so the caps are exercisable in tests with small values.
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_field_names,
    reason = "the shared `max_` prefix names these uniformly as the bounding caps"
)]
struct Limits {
    /// Upper bound on arena nodes — the walk stops adding past this.
    max_nodes: usize,
    /// Upper bound on bytes read for one directory's data (a malformed
    /// `InformationLength` can't force an unbounded read).
    max_dir_bytes: u64,
    /// Upper bound on `NextExtent` continuations followed for one extent list (a
    /// malformed continuation chain can't loop forever).
    max_continuations: usize,
    /// Upper bound on extents collected for one extent list — and on the byte
    /// runs resolved for one file — so a hostile continuation chain or a
    /// shattered metadata mapping can't amass an unbounded list.
    max_extents: usize,
    /// Upper bound on the arena's directory nesting depth (root = depth 0): a
    /// subdirectory deeper than this is dropped, so the recursive arena consumers
    /// ([`UdfDir::collect_from`] here, `directory_size` in the BDROM scan) recurse a
    /// bounded number of frames and a hostile chain of ~1M nested directories cannot
    /// overflow the thread stack.
    max_depth: usize,
    /// Upper bound on the File Set Descriptor extent blocks scanned for the FSD
    /// (a hostile `LongAd` length can't drive an unbounded search; an authored
    /// disc records the FSD in the extent's first block).
    max_fsd_sectors: u64,
}

impl Limits {
    /// The production caps: a Blu-ray has at most a few thousand files, tiny
    /// directories, short extent lists, and a handful (~3–4) of nesting levels, so
    /// these never bite a real disc.
    const DEFAULT: Self = Self {
        max_nodes: 1 << 20,
        max_dir_bytes: 64 << 20,
        max_continuations: 4096,
        max_extents: 1 << 16,
        max_depth: 1 << 10,
        max_fsd_sectors: 256,
    };
}

/// Opens fresh, independent seekable readers over the same `.iso` bytes, so each
/// VFS file handle reads through its own cursor (no shared seek state).
///
/// The CLI backs this with a file path ([`PathIso`], reopening the `.iso`); tests
/// back it with an in-memory buffer. Each [`BdFile::open_read`] calls [`open`] once.
/// It is object-safe (a [`Box<dyn ReadSeek>`](ReadSeek) return, no associated type)
/// so the whole backend is **non-generic** — one monomorphization, smaller static
/// binary (the prime directive), and a single coverage surface over all readers.
///
/// [`open`]: IsoReader::open
pub trait IsoReader: fmt::Debug + Send + Sync {
    /// Opens a new independent reader positioned at the start of the `.iso`.
    ///
    /// # Errors
    /// Propagates the underlying IO error if the `.iso` cannot be opened.
    fn open(&self) -> io::Result<Box<dyn ReadSeek>>;
}

/// A path-backed [`IsoReader`] — reopens the `.iso` file for each handle.
///
/// This is the CLI backend. Reopening shares the OS file with an independent
/// cursor, so large streams stay `u64` and nothing is staged to memory.
#[derive(Debug, Clone)]
pub struct PathIso {
    /// The `.iso` path that each [`open`](IsoReader::open) reopens.
    path: PathBuf,
}

impl PathIso {
    /// Wraps `path` as an [`IsoReader`] factory.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl IsoReader for PathIso {
    fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
        // The dominant consumer is the demux streaming `.m2ts` runs front to
        // back, so the sequential-access hint applies to the image too.
        Ok(Box::new(crate::vfs::fs::open_sequential(&self.path)?))
    }
}

/// Where a partition reference's logical blocks physically live — the resolved
/// partition map.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PartitionLoc {
    /// A physical partition: logical block `b` is at absolute sector `start + b`.
    Physical {
        /// The partition's first physical sector (`PartitionStartingLocation`).
        start: u64,
    },
    /// A UDF 2.50 Metadata partition: blocks are scattered through the physical
    /// partition (starting at `phys_start`) per the metadata file's `extents`.
    Metadata {
        /// The backing physical partition's first sector.
        phys_start: u64,
        /// The metadata file's allocation extents (physical-partition-relative).
        extents: Vec<Extent>,
        /// The metadata **mirror** file's extents (UDF 2.50 §2.2.13.2) — the
        /// redundant copy a failed metadata read retries through.
        /// Empty when the mirror file is unreadable.
        mirror_extents: Vec<Extent>,
    },
}

/// One contiguous run of a file's data: bytes `[off, off+len)` of the `.iso` for a
/// recorded run, or `len` zero bytes for a sparse (not-recorded) run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    /// Absolute byte offset in the `.iso`, or `None` for a sparse (zero) run.
    src: Option<u64>,
    /// The run length in bytes.
    len: u64,
}

/// A file's content: either inline (embedded) bytes or a list of byte [`Run`]s read
/// lazily from the `.iso`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    /// Inline bytes stored in the File Entry (an embedded entry).
    Embedded(Vec<u8>),
    /// Byte runs resolved from the File Entry's allocation extents.
    Runs(Vec<Run>),
}

/// A file's size + content, shared (`Arc`) so a [`UdfFile`] handle is cheap to make.
#[derive(Debug, PartialEq, Eq)]
struct FileBody {
    /// `InformationLength` — the authoritative byte length (the `.length()` value).
    length: u64,
    /// The file's content (embedded bytes or resolved runs).
    content: Content,
}

/// One node of the parsed directory tree. Children are not stored — a directory's
/// children are the nodes whose [`parent`](Node::parent) is its index, in arena
/// (on-disc enumeration) order — so the walk never indexes back into the arena.
#[derive(Debug)]
struct Node {
    /// The entry name (a directory/file identifier decoded from OSTA CS0).
    name: String,
    /// The synthesized full path (`/`-joined; internal only).
    full_name: String,
    /// The parent node index, or `None` for the root.
    parent: Option<usize>,
    /// The node payload.
    kind: NodeKind,
}

/// A [`Node`]'s payload — a directory or a file body.
#[derive(Debug)]
enum NodeKind {
    /// A directory (its children are found by scanning [`Node::parent`]).
    Dir,
    /// A file: its shared size + content.
    File(Arc<FileBody>),
}

/// The shared parsed `.iso`: the reader factory plus the directory-tree arena.
#[derive(Debug)]
struct UdfInner {
    /// The factory that reopens the `.iso` for each file handle.
    factory: Box<dyn IsoReader>,
    /// The directory-tree arena; node 0 is the root.
    nodes: Vec<Node>,
    /// The UDF volume label (the root directory's [`name`](BdDir::name)).
    label: String,
    /// Whether a failed mid-file run read is recorded + zero-filled (resilient)
    /// instead of propagated (strict) — see [`UdfSource::open_resilient`].
    resilient: bool,
    /// The bad-sector failures recorded by resilient file reads
    /// (drained by [`UdfSource::take_errors`]).
    errors: Mutex<Vec<ScanError>>,
}

/// A read-only Blu-ray `.iso` opened through its UDF 2.50 filesystem — the
/// `.iso` `vfs` backend.
///
/// Built by [`open`](UdfSource::open); [`root`](UdfSource::root) yields the disc
/// root as a [`BdDir`] every BD/M2TS parser reads through unchanged.
#[derive(Debug)]
pub struct UdfSource {
    /// The shared parsed image.
    inner: Arc<UdfInner>,
}

impl UdfSource {
    /// Opens the `.iso` `factory` produces, parsing the UDF volume and walking its
    /// entire directory tree into the arena.
    ///
    /// # Errors
    /// - [`BdError::Io`] if the `.iso` cannot be opened or read.
    /// - [`BdError::StructureNotFound`] if it has no readable UDF volume (no AVDP at any anchor
    ///   sector, no Logical Volume Descriptor, no File Set Descriptor, or no root directory) — i.e.
    ///   it is not a UDF `.iso`.
    pub fn open(factory: Box<dyn IsoReader>) -> Result<Self, BdError> {
        Self::open_with(factory, false)
    }

    /// Opens the `.iso` like [`open`](Self::open), but **tolerates bad sectors**
    /// under file data: a mid-file read/seek failure is recorded (file + byte
    /// position) and the unreadable range is served as zeros, so the scan over the
    /// readable rest continues — unreadable spans zero-fill, and every failure
    /// is reported. Drain the recordings with
    /// [`take_errors`](Self::take_errors). The volume structures themselves stay
    /// strict (a damaged VDS/FSD has no readable rest to degrade to).
    ///
    /// # Errors
    /// As [`open`](Self::open) — the volume open is identical in both modes.
    pub fn open_resilient(factory: Box<dyn IsoReader>) -> Result<Self, BdError> {
        Self::open_with(factory, true)
    }

    /// The shared open behind [`open`](Self::open) (strict) and
    /// [`open_resilient`](Self::open_resilient).
    fn open_with(factory: Box<dyn IsoReader>, resilient: bool) -> Result<Self, BdError> {
        let mut reader = factory.open()?;
        let limits = Limits::DEFAULT;
        let volume = parse_volume(&mut *reader, limits)?;
        let label = volume.label.clone();
        let nodes = build_tree(&mut *reader, &volume, limits)?;
        Ok(Self {
            inner: Arc::new(UdfInner {
                factory,
                nodes,
                label,
                resilient,
                errors: Mutex::new(Vec::new()),
            }),
        })
    }

    /// Drains the bad-sector failures recorded by resilient file reads since the
    /// last call (one per damaged file handle, at the first unreadable position).
    /// Always empty for a strictly-opened source or healthy media.
    #[must_use]
    pub fn take_errors(&self) -> Vec<ScanError> {
        std::mem::take(&mut *self.inner.errors.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// The disc root as a [`BdDir`]. Its [`name`](BdDir::name) is the UDF volume
    /// label, so a `BdRom` opened over it reads the genuine `.iso` label.
    ///
    /// Node 0 is always the root directory (an invariant of [`build_tree`]), so the
    /// root is constructed directly from the stored label rather than looked up.
    #[must_use]
    pub fn root(&self) -> UdfDir {
        UdfDir {
            inner: Arc::clone(&self.inner),
            node: 0,
            name: self.inner.label.clone(),
            full_name: String::new(),
            parent: None,
        }
    }

    /// The UDF volume label (`LogicalVolumeIdentifier`).
    #[must_use]
    pub fn volume_label(&self) -> &str {
        &self.inner.label
    }
}

/// Builds a [`UdfDir`] for the directory node at `idx`, or `None` if `idx` is not a
/// directory node — the shared constructor for [`UdfSource::root`],
/// [`UdfDir::get_directories`], and [`UdfDir::parent`].
fn dir_at(inner: &Arc<UdfInner>, idx: usize) -> Option<UdfDir> {
    let node = inner.nodes.get(idx)?;
    match node.kind {
        NodeKind::Dir => Some(UdfDir {
            inner: Arc::clone(inner),
            node: idx,
            name: node.name.clone(),
            full_name: node.full_name.clone(),
            parent: node.parent,
        }),
        NodeKind::File(_) => None,
    }
}

/// A directory in a UDF `.iso` — the counterpart of the folder backend's `FsDir`,
/// served from the parsed arena.
#[derive(Debug)]
pub struct UdfDir {
    /// The shared parsed image.
    inner: Arc<UdfInner>,
    /// This directory's arena index.
    node: usize,
    /// The directory name (the volume label for the root).
    name: String,
    /// The synthesized full path.
    full_name: String,
    /// The parent node index, or `None` for the root.
    parent: Option<usize>,
}

impl UdfDir {
    /// The arena indices of this directory's children (the nodes whose parent is
    /// this one, in arena order), or an IO error if this node is missing or is not a
    /// directory (unreachable for a `UdfDir`, which always wraps a directory node —
    /// a defensive guard, never panic).
    fn children(&self) -> io::Result<Vec<usize>> {
        match self.inner.nodes.get(self.node) {
            Some(Node { kind: NodeKind::Dir, .. }) => {
                Ok(child_indices(&self.inner.nodes, self.node))
            }
            _ => Err(io::Error::other("udf node is not a directory")),
        }
    }

    /// Builds a [`UdfFile`] for the file node at `idx`, or `None` if `idx` is not a
    /// file node (so iterating a directory's children with this filters out the
    /// subdirectories).
    fn file_at(&self, idx: usize) -> Option<UdfFile> {
        let node = self.inner.nodes.get(idx)?;
        match &node.kind {
            NodeKind::File(body) => Some(UdfFile {
                inner: Arc::clone(&self.inner),
                name: node.name.clone(),
                full_name: node.full_name.clone(),
                extension: extension_of(&node.name),
                body: Arc::clone(body),
            }),
            NodeKind::Dir => None,
        }
    }

    /// Collects this directory's files matching `pattern` (ASCII case-insensitive
    /// glob) into `out`, recursing into subdirectories when `option` is
    /// [`SearchOption::AllDirectories`].
    fn collect_files(
        &self,
        pattern: &[u8],
        option: SearchOption,
        out: &mut Vec<Box<dyn BdFile>>,
    ) -> io::Result<()> {
        // The one fallible check (this node is a directory); the scan below cannot
        // fail, so the recursion carries no error path.
        self.children()?;
        self.collect_from(self.node, pattern, option, out);
        Ok(())
    }

    /// Appends `dir`'s glob-matching files to `out`, recursing into subdirectories
    /// for [`SearchOption::AllDirectories`]. Infallible — `dir` is a known
    /// directory index and the arena scan never errors.
    fn collect_from(
        &self,
        dir: usize,
        pattern: &[u8],
        option: SearchOption,
        out: &mut Vec<Box<dyn BdFile>>,
    ) {
        for idx in child_indices(&self.inner.nodes, dir) {
            if let Some(file) = self.file_at(idx) {
                if glob_ci(pattern, file.name().as_bytes()) {
                    out.push(Box::new(file));
                }
            } else if option == SearchOption::AllDirectories {
                self.collect_from(idx, pattern, option, out);
            }
        }
    }
}

/// The arena indices whose parent is `dir`, in arena (on-disc enumeration) order.
fn child_indices(nodes: &[Node], dir: usize) -> Vec<usize> {
    nodes.iter().enumerate().filter(|(_, n)| n.parent == Some(dir)).map(|(idx, _)| idx).collect()
}

impl BdDir for UdfDir {
    fn name(&self) -> &str {
        &self.name
    }

    fn full_name(&self) -> &str {
        &self.full_name
    }

    fn parent(&self) -> Option<Box<dyn BdDir>> {
        let pidx = self.parent?;
        dir_at(&self.inner, pidx).map(|d| -> Box<dyn BdDir> { Box::new(d) })
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
        self.collect_files(pattern.as_bytes(), option, &mut out)?;
        Ok(out)
    }

    fn get_directories(&self) -> io::Result<Vec<Box<dyn BdDir>>> {
        let mut out: Vec<Box<dyn BdDir>> = Vec::new();
        for idx in self.children()? {
            if let Some(dir) = dir_at(&self.inner, idx) {
                out.push(Box::new(dir));
            }
        }
        Ok(out)
    }
}

/// A file in a UDF `.iso` — the counterpart of the folder backend's `FsFile`.
///
/// Name, extension, and length
/// are captured at construction; the bytes are read lazily by
/// [`open_read`](BdFile::open_read).
#[derive(Debug)]
pub struct UdfFile {
    /// The shared parsed image (its factory reopens the `.iso`).
    inner: Arc<UdfInner>,
    /// The file name including extension.
    name: String,
    /// The synthesized full path.
    full_name: String,
    /// The extension including the leading dot, or empty (derived from the name
    /// exactly as the folder backend derives it, so the `.SSIF` disc-size skip
    /// is identical).
    extension: String,
    /// The shared size + content.
    body: Arc<FileBody>,
}

impl BdFile for UdfFile {
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
        self.body.length
    }

    fn is_dir(&self) -> bool {
        false
    }

    fn open_read(&self) -> io::Result<Box<dyn ReadSeek>> {
        match &self.body.content {
            Content::Embedded(bytes) => Ok(Box::new(EmbeddedReader {
                bytes: bytes.clone(),
                length: self.body.length,
                pos: 0,
            })),
            Content::Runs(runs) => {
                let stream = self.inner.factory.open()?;
                Ok(Box::new(UdfFileReader {
                    inner: Arc::clone(&self.inner),
                    name: self.name.clone(),
                    reported: false,
                    stream,
                    runs: runs.clone(),
                    length: self.body.length,
                    pos: 0,
                }))
            }
        }
    }

    fn open_text(&self) -> io::Result<Box<dyn BufRead>> {
        Ok(Box::new(BufReader::new(self.open_read()?)))
    }
}

/// A seekable reader over a file's resolved byte [`Run`]s — reads run data from the
/// `.iso` `stream`, fills sparse runs with zeros, and serves exactly `length`
/// (`InformationLength`) bytes. The M2TS demux reads it sequentially; metadata
/// parsers read it whole; `Seek` rounds out the [`ReadSeek`] contract.
struct UdfFileReader {
    /// The shared parsed image (the resilient flag + the bad-sector error sink).
    inner: Arc<UdfInner>,
    /// The file name, for the recorded bad-sector errors.
    name: String,
    /// Whether this handle already recorded a bad-sector failure (one record per
    /// damaged file — a long run of bad sectors cannot flood the sink).
    reported: bool,
    /// The independent `.iso` stream (from [`IsoReader::open`]).
    stream: Box<dyn ReadSeek>,
    /// The file's byte runs, in order.
    runs: Vec<Run>,
    /// The logical file length (the authoritative cap on bytes served).
    length: u64,
    /// The current logical read position.
    pos: u64,
}

impl UdfFileReader {
    /// Handles a failed run read at the current position: in resilient mode the
    /// failure is recorded (once per handle, at its first unreadable position) and
    /// the `want` unreadable bytes are served as zeros so the read continues —
    /// zero-fill, always reported;
    /// in strict mode the error propagates (aborting the file's scan).
    fn recover(
        &mut self,
        err: io::Error,
        buf: &mut [u8],
        want: u64,
        want_us: usize,
    ) -> io::Result<usize> {
        if !self.inner.resilient {
            return Err(err);
        }
        if !self.reported {
            self.reported = true;
            self.inner.errors.lock().unwrap_or_else(PoisonError::into_inner).push(ScanError {
                file: format!("{} @ byte {}", self.name, self.pos),
                stage: ScanStage::SectorRead,
                reason: BdError::Io(err),
            });
        }
        for slot in buf.iter_mut().take(want_us) {
            *slot = 0;
        }
        self.pos = self.pos.saturating_add(want);
        Ok(want_us)
    }
}

impl Read for UdfFileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.length {
            return Ok(0);
        }
        let mut start: u64 = 0;
        for run in &self.runs {
            let end = start.saturating_add(run.len);
            if self.pos < end {
                let into_run = self.pos.saturating_sub(start);
                let run_avail = run.len.saturating_sub(into_run);
                let file_avail = self.length.saturating_sub(self.pos);
                let cap = u64::try_from(buf.len()).unwrap_or(u64::MAX);
                let want = run_avail.min(file_avail).min(cap);
                let want_us = usize::try_from(want).unwrap_or(usize::MAX);
                return if let Some(off) = run.src {
                    let outcome = off
                        .checked_add(into_run)
                        .ok_or_else(|| io::Error::other("udf physical byte offset overflow"))
                        .and_then(|phys| {
                            self.stream.seek(SeekFrom::Start(phys))?;
                            (&mut self.stream).take(want).read(buf)
                        });
                    match outcome {
                        Ok(read) => {
                            self.pos = self.pos.saturating_add(u64::try_from(read).unwrap_or(0));
                            Ok(read)
                        }
                        Err(err) => self.recover(err, buf, want, want_us),
                    }
                } else {
                    for slot in buf.iter_mut().take(want_us) {
                        *slot = 0;
                    }
                    self.pos = self.pos.saturating_add(want);
                    Ok(want_us)
                };
            }
            start = end;
        }
        // `pos` is past every run (a truncated/under-covered file) — report EOF.
        Ok(0)
    }
}

impl Seek for UdfFileReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = seek_within(self.pos, self.length, from)?;
        Ok(self.pos)
    }
}

/// Applies a signed `delta` to a `u64` base, or `None` on overflow / a negative
/// result — the bounds-checked core of the UDF readers' `Seek`.
const fn offset_by(base: u64, delta: i64) -> Option<u64> {
    // `unsigned_abs` yields the magnitude as a `u64` (correct even for `i64::MIN`),
    // so the only failure is the over/underflow the `checked_*` reports.
    let magnitude = delta.unsigned_abs();
    if delta >= 0 { base.checked_add(magnitude) } else { base.checked_sub(magnitude) }
}

/// Resolves a [`Seek`] request against a logical `pos`/`length` pair — the shared
/// core of both UDF readers' `Seek`. Returns the new absolute position, or an
/// invalid-seek error when the target over/underflows (a negative result).
fn seek_within(pos: u64, length: u64, from: SeekFrom) -> io::Result<u64> {
    let target = match from {
        SeekFrom::Start(off) => Some(off),
        SeekFrom::Current(delta) => offset_by(pos, delta),
        SeekFrom::End(delta) => offset_by(length, delta),
    };
    target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid udf seek"))
}

/// A seekable reader over an embedded File Entry's inline bytes that serves
/// exactly `length` (`InformationLength`) bytes — the `min(L_AD, length)` recorded
/// inline bytes, then zero-fill — so an embedded file reads the same length its
/// [`length`](BdFile::length) reports, matching the [`UdfFileReader`] (runs)
/// contract. Only the block-bounded inline `bytes` are held; the zero tail is
/// produced on the fly, so a hostile `InformationLength` cannot force a large
/// allocation.
struct EmbeddedReader {
    /// The File Entry's inline bytes (the `L_AD` allocation-descriptor area).
    bytes: Vec<u8>,
    /// The logical file length (`InformationLength`) — the authoritative cap on
    /// bytes served.
    length: u64,
    /// The current logical read position.
    pos: u64,
}

impl Read for EmbeddedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // No early `is_empty`/EOF guard is needed: an empty `buf` or a position
        // at/past `length` both drive `want` to 0 below, yielding `Ok(0)`.
        let file_avail = self.length.saturating_sub(self.pos);
        let cap = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        let want = file_avail.min(cap);
        let want_us = usize::try_from(want).unwrap_or(usize::MAX);
        let pos_us = usize::try_from(self.pos).unwrap_or(usize::MAX);
        for (i, slot) in buf.iter_mut().take(want_us).enumerate() {
            // The recorded inline byte while within `bytes`; zero past it (the
            // `InformationLength > L_AD` tail).
            *slot = pos_us.checked_add(i).and_then(|p| self.bytes.get(p)).copied().unwrap_or(0);
        }
        self.pos = self.pos.saturating_add(want);
        Ok(want_us)
    }
}

impl Seek for EmbeddedReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = seek_within(self.pos, self.length, from)?;
        Ok(self.pos)
    }
}

/// The extension *including* the leading dot (e.g. `.SSIF`), or the empty string
/// when `name` has no `.` — derived exactly as the folder backend derives it, so
/// the disc-size `.SSIF` skip behaves identically over either input.
fn extension_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((_, ext)) => format!(".{ext}"),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Volume parsing + partition resolution (no IO beyond the passed reader)
// ---------------------------------------------------------------------------

/// The parsed UDF volume: the logical block size, the resolved partition locations,
/// the volume label, and the root directory's ICB.
struct Volume {
    /// `LogicalBlockSize` — the logical sector size in bytes (2048 for a Blu-ray).
    block_size: u64,
    /// Resolved partition locations, indexed by partition reference number.
    locs: Vec<PartitionLoc>,
    /// `LogicalVolumeIdentifier` — the disc volume label.
    label: String,
    /// `(partition reference, logical block)` of the root directory's File Entry.
    root_icb: (u16, u32),
}

/// Reads one `block_size`-byte logical sector at `sector` from `reader` (the IO
/// primitive every sector read shares).
fn read_sector_io(reader: &mut dyn ReadSeek, sector: u64, block_size: u64) -> io::Result<Vec<u8>> {
    let offset = sector
        .checked_mul(block_size)
        .ok_or_else(|| io::Error::other("udf sector byte offset overflow"))?;
    reader.seek(SeekFrom::Start(offset))?;
    let len = usize::try_from(block_size).unwrap_or(usize::MAX);
    let mut buf = vec![0_u8; len];
    reader.read_exact(&mut buf)?;
    Ok(buf)
}

/// [`read_sector_io`] with its IO error mapped into [`BdError`].
fn read_sector(
    reader: &mut dyn ReadSeek,
    sector: u64,
    block_size: u64,
) -> Result<Vec<u8>, BdError> {
    Ok(read_sector_io(reader, sector, block_size)?)
}

/// Resolves a `(partition_ref, logical_block)` pair to an absolute sector — physical
/// partitions add the start; Metadata partitions walk the metadata file's extents.
fn resolve_sector(
    locs: &[PartitionLoc],
    partition_ref: u16,
    block: u64,
    block_size: u64,
) -> Option<u64> {
    match locs.get(usize::from(partition_ref))? {
        PartitionLoc::Physical { start } => start.checked_add(block),
        PartitionLoc::Metadata { phys_start, extents, .. } => {
            metadata_sector(*phys_start, extents, block, block_size)
        }
    }
}

/// Resolves a `(partition_ref, logical_block)` pair through the metadata
/// **mirror** mapping — the alternative location a failed metadata read retries.
/// `None` for a physical partition (no alternative exists) or when the
/// mirror does not map the block.
fn resolve_mirror_sector(
    locs: &[PartitionLoc],
    partition_ref: u16,
    block: u64,
    block_size: u64,
) -> Option<u64> {
    match locs.get(usize::from(partition_ref))? {
        PartitionLoc::Physical { .. } => None,
        PartitionLoc::Metadata { phys_start, mirror_extents, .. } => {
            metadata_sector(*phys_start, mirror_extents, block, block_size)
        }
    }
}

/// Reads the already-resolved `sector` backing `(partition_ref, block)`; when
/// the read fails and the block also maps through a metadata mirror, retries
/// there before failing (libudfread recovers metadata blocks the same
/// way). File *data* runs carry no such retry: like libudfread, the mirror
/// covers only the metadata partition (File Entries, directories), and file
/// data lives in the physical partition on authored discs.
fn read_sector_with_mirror(
    reader: &mut dyn ReadSeek,
    locs: &[PartitionLoc],
    partition_ref: u16,
    block: u64,
    sector: u64,
    block_size: u64,
) -> io::Result<Vec<u8>> {
    match read_sector_io(reader, sector, block_size) {
        Ok(buf) => Ok(buf),
        Err(err) => {
            if let Some(alt) = resolve_mirror_sector(locs, partition_ref, block, block_size)
                && let Ok(buf) = read_sector_io(reader, alt, block_size)
            {
                return Ok(buf);
            }
            Err(err)
        }
    }
}

/// Maps a Metadata-partition logical `block` to a physical sector by walking the
/// metadata file's allocation `extents` (each spanning `length / block_size` blocks
/// within the physical partition starting at `phys_start`).
fn metadata_sector(
    phys_start: u64,
    extents: &[Extent],
    block: u64,
    block_size: u64,
) -> Option<u64> {
    let mut consumed: u64 = 0;
    for extent in extents {
        // `checked_div` is the one genuine guard (a zero block size is malformed);
        // the rest cannot overflow for `u32`-derived block counts, so they saturate.
        let blocks = u64::from(extent.length).checked_div(block_size)?;
        let next = consumed.saturating_add(blocks);
        if block >= consumed && block < next {
            let within = block.saturating_sub(consumed);
            let phys_block = u64::from(extent.block).saturating_add(within);
            return Some(phys_start.saturating_add(phys_block));
        }
        consumed = next;
    }
    None
}

/// Locates the Anchor Volume Descriptor Pointer: the fixed [`ANCHOR_SECTOR`]
/// (256) first, then the end-of-image fallback anchors — the last sector and
/// the last sector − 256 (UDF 2.50 §2.2.3 records an AVDP at two of sector 256,
/// N − 256, N) — so a truncated or oddly mastered image whose sector-256 anchor
/// is damaged still opens. When no candidate parses, the primary
/// attempt's failure is returned.
fn locate_avdp(reader: &mut dyn ReadSeek) -> Result<Avdp, BdError> {
    let primary_err = match read_sector(reader, ANCHOR_SECTOR, BOOTSTRAP_SECTOR_SIZE) {
        Ok(anchor) => match Avdp::parse(&anchor) {
            Some(avdp) => return Ok(avdp),
            None => BdError::StructureNotFound,
        },
        Err(err) => err,
    };
    if let Ok(len) = reader.seek(SeekFrom::End(0)) {
        let sectors = len.checked_div(BOOTSTRAP_SECTOR_SIZE).unwrap_or(0);
        for candidate in [sectors.saturating_sub(1), sectors.saturating_sub(257)] {
            if let Ok(buf) = read_sector_io(reader, candidate, BOOTSTRAP_SECTOR_SIZE)
                && let Some(avdp) = Avdp::parse(&buf)
            {
                return Ok(avdp);
            }
        }
    }
    Err(primary_err)
}

/// Parses the UDF volume from `reader`: AVDP → the Main Volume Descriptor Sequence
/// (LVD + Partition Descriptors, falling back to the Reserve sequence) → the
/// resolved partition map → the File Set Descriptor → the root directory ICB.
///
/// # Errors
/// [`BdError::StructureNotFound`] if any required descriptor is absent (not a UDF
/// `.iso`), or [`BdError::Io`] for a read failure.
fn parse_volume(reader: &mut dyn ReadSeek, limits: Limits) -> Result<Volume, BdError> {
    // 1. The AVDP → the Main Volume Descriptor Sequence extent.
    let avdp = locate_avdp(reader)?;

    // 2. Walk the Main VDS; when it fails or yields no usable LVD + PD set, retry on the Reserve
    //    VDS.
    let scan = {
        let usable = |s: &VdsScan| s.lvd.is_some() && !s.partitions.is_empty();
        match walk_vds(reader, avdp.main_vds) {
            Ok(scan) if usable(&scan) => scan,
            main => match walk_vds(reader, avdp.reserve_vds) {
                Ok(reserve) if usable(&reserve) => reserve,
                _ => main?,
            },
        }
    };
    let pvd_label = scan.pvd.map(|(_, pvd)| pvd.volume_identifier);
    let (_, lvd) = scan.lvd.ok_or(BdError::StructureNotFound)?;
    let partitions: Vec<PartitionDescriptor> =
        scan.partitions.into_values().map(|(_, pd)| pd).collect();
    // The label stays the LVD identifier — the string Windows shows for a
    // mounted UDF volume (via the FSD's mandatory copy of it, UDF 2.50 §2.3.2),
    // hence what classic BDInfo reported — with the PVD's VolumeIdentifier as
    // the fallback when the LVD identifier is empty.
    let label = if lvd.logical_volume_identifier.is_empty() {
        pvd_label.unwrap_or_default()
    } else {
        lvd.logical_volume_identifier.clone()
    };
    let block_size = u64::from(lvd.logical_block_size);
    if block_size == 0 || block_size > MAX_BLOCK_SIZE {
        return Err(BdError::StructureNotFound);
    }

    // 3. Resolve every partition reference to its physical location (reading the metadata file's
    //    File Entry for a Metadata partition).
    let locs = resolve_partitions(reader, &lvd, &partitions, block_size, limits)?;

    // 4. File Set Descriptor → the root directory's ICB.
    let fsd = find_fsd(reader, &locs, lvd.file_set_descriptor, block_size, limits)?;
    let root = fsd.root_directory_icb.location;

    Ok(Volume { block_size, locs, label, root_icb: (root.partition, root.block) })
}

/// Searches the File Set Descriptor extent for the FSD, returning the parsed
/// [`Fsd`]. The LVD's `LongAd` gives the extent's first block and its byte
/// length; an authored Blu-ray records the FSD in that first block (the fast
/// path), but ECMA-167 §4/8.4 lets the File Set Descriptor Sequence place it
/// anywhere in the extent, ended by a Terminating Descriptor — so each block is
/// read in turn, the first [`Fsd`] is taken, and a Terminating Descriptor stops
/// the search (libudfread does the same). [`Limits::max_fsd_sectors`] bounds the
/// scan so a hostile `LongAd` length can't drive an unbounded walk.
///
/// # Errors
/// [`BdError::StructureNotFound`] if no FSD is found before a Terminating
/// Descriptor, the cap, or the extent's end; [`BdError::Io`] for a read failure.
fn find_fsd(
    reader: &mut dyn ReadSeek,
    locs: &[PartitionLoc],
    fsd_ad: LongAd,
    block_size: u64,
    limits: Limits,
) -> Result<Fsd, BdError> {
    let partition = fsd_ad.location.partition;
    let first_block = u64::from(fsd_ad.location.block);
    // ceil(length / block_size) blocks — `block_size` is non-zero (the caller
    // guards it) — always at least one (the fast path reads the first block,
    // even for a malformed zero length), capped so a hostile length cannot drive
    // an unbounded scan.
    let extent_blocks = u64::from(fsd_ad.length_bytes()).div_ceil(block_size);
    let blocks = extent_blocks.max(1).min(limits.max_fsd_sectors);
    for offset in 0..blocks {
        let block = first_block.saturating_add(offset);
        let sector =
            resolve_sector(locs, partition, block, block_size).ok_or(BdError::StructureNotFound)?;
        let buf = read_sector_with_mirror(reader, locs, partition, block, sector, block_size)?;
        if let Some(fsd) = Fsd::parse(&buf) {
            return Ok(fsd);
        }
        // A Terminating Descriptor ends the File Set Descriptor Sequence — stop
        // (any descriptor that is neither is skipped to the bounded end).
        if Tag::parse(&buf, 0).is_some_and(|t| t.identifier == TAG_TERMINATING) {
            break;
        }
    }
    Err(BdError::StructureNotFound)
}

/// The descriptors harvested from one volume-descriptor-sequence walk, each
/// carried with its `VolumeDescriptorSequenceNumber` (the prevailing key).
struct VdsScan {
    /// The prevailing Logical Volume Descriptor.
    lvd: Option<(u32, Lvd)>,
    /// The prevailing Primary Volume Descriptor (the label fallback).
    pvd: Option<(u32, Pvd)>,
    /// The prevailing Partition Descriptor per `PartitionNumber` (a `BTreeMap`
    /// for deterministic order — the house rule).
    partitions: BTreeMap<u16, (u32, PartitionDescriptor)>,
}

/// Walks one volume descriptor sequence from `extent`: follows a Volume
/// Descriptor Pointer to its continuation extent, stops at a Terminating
/// Descriptor, and keeps the **prevailing** LVD / PVD / Partition Descriptors —
/// highest `VolumeDescriptorSequenceNumber` wins, the later-read descriptor on a
/// tie (ECMA-167 §3/8.4.3). A single [`MAX_VDS_SECTORS`] budget spans the whole
/// walk **including VDP hops**, so a self-referencing VDP chain (which hangs
/// libudfread) terminates here.
fn walk_vds(reader: &mut dyn ReadSeek, extent: ExtentAd) -> Result<VdsScan, BdError> {
    let mut scan = VdsScan { lvd: None, pvd: None, partitions: BTreeMap::new() };
    let mut budget = MAX_VDS_SECTORS;
    let mut sector = u64::from(extent.location);
    let mut remaining = u64::from(extent.length).checked_div(BOOTSTRAP_SECTOR_SIZE).unwrap_or(0);
    while remaining > 0 && budget > 0 {
        budget = budget.saturating_sub(1);
        remaining = remaining.saturating_sub(1);
        let buf = read_sector(reader, sector, BOOTSTRAP_SECTOR_SIZE)?;
        sector = sector.saturating_add(1);
        // Every VDS descriptor carries its VolumeDescriptorSequenceNumber at
        // offset 16 (right after the tag) — the prevailing-descriptor key.
        let vdsn = u32_le(&buf, 16).unwrap_or(0);
        if let Some(parsed) = Lvd::parse(&buf) {
            if scan.lvd.as_ref().is_none_or(|(prev, _)| vdsn >= *prev) {
                scan.lvd = Some((vdsn, parsed));
            }
        } else if let Some(parsed) = PartitionDescriptor::parse(&buf) {
            let keep =
                scan.partitions.get(&parsed.partition_number).is_none_or(|(prev, _)| vdsn >= *prev);
            if keep {
                scan.partitions.insert(parsed.partition_number, (vdsn, parsed));
            }
        } else if let Some(parsed) = Pvd::parse(&buf) {
            if scan.pvd.as_ref().is_none_or(|(prev, _)| vdsn >= *prev) {
                scan.pvd = Some((vdsn, parsed));
            }
        } else if let Some(vdp) = Vdp::parse(&buf) {
            // Continue the sequence in the pointed-at extent; only the shared
            // budget bounds the chain.
            sector = u64::from(vdp.next.location);
            remaining = u64::from(vdp.next.length).checked_div(BOOTSTRAP_SECTOR_SIZE).unwrap_or(0);
        } else if Tag::parse(&buf, 0).is_some_and(|t| t.identifier == TAG_TERMINATING) {
            break;
        }
    }
    Ok(scan)
}

/// Resolves the LVD's partition map into a [`PartitionLoc`] per reference, reading
/// the metadata file's File Entry (its extents define a Metadata partition's
/// physical layout). An unresolved (`Other`) map becomes a zero-based physical
/// placeholder — never referenced by a well-formed Blu-ray.
fn resolve_partitions(
    reader: &mut dyn ReadSeek,
    lvd: &Lvd,
    partitions: &[PartitionDescriptor],
    block_size: u64,
    limits: Limits,
) -> Result<Vec<PartitionLoc>, BdError> {
    let mut locs: Vec<PartitionLoc> = Vec::new();
    for (index, map) in lvd.partition_maps.iter().enumerate() {
        let partition_ref = u16::try_from(index).unwrap_or(u16::MAX);
        let start = physical_partition_start(&lvd.partition_maps, partitions, partition_ref);
        match map {
            PartitionMap::Physical { .. } => {
                let start = start.ok_or(BdError::StructureNotFound)?;
                locs.push(PartitionLoc::Physical { start: u64::from(start) });
            }
            PartitionMap::Metadata(meta) => {
                let phys_start = u64::from(start.ok_or(BdError::StructureNotFound)?);
                // `meta.physical_partition` is a UDF *PartitionNumber* — a
                // different namespace from the partition-REFERENCE indices that
                // `collect_extents`/`extent_runs` use to index `locs` — so the
                // metadata file's short-form descriptors are owned by the
                // reference index of the type-1 map carrying that number (the
                // two coincide on authored BDs, where both are 0).
                let phys_ref = lvd
                    .partition_maps
                    .iter()
                    .position(|m| {
                        matches!(m, PartitionMap::Physical { partition_number }
                            if *partition_number == meta.physical_partition)
                    })
                    .and_then(|idx| u16::try_from(idx).ok())
                    .ok_or(BdError::StructureNotFound)?;
                // Load both the metadata file's mapping and the mirror
                // file's — the primary is preferred, the other kept as the
                // per-read retry path; the open fails only when neither is
                // readable (libudfread uses the mirror FE the same way).
                // `phys_start` is `u32`-derived and the locations are `u32`, so
                // the FE sectors cannot overflow `u64` (saturating regardless).
                let primary = load_metadata_extents(
                    reader,
                    &locs,
                    phys_start.saturating_add(u64::from(meta.metadata_file_location)),
                    FILE_TYPE_METADATA,
                    phys_ref,
                    block_size,
                    limits,
                );
                let mirror = load_metadata_extents(
                    reader,
                    &locs,
                    phys_start.saturating_add(u64::from(meta.metadata_mirror_file_location)),
                    FILE_TYPE_METADATA_MIRROR,
                    phys_ref,
                    block_size,
                    limits,
                );
                let (extents, mirror_extents) = match (primary, mirror) {
                    (Some(p), Some(m)) => (p, m),
                    (Some(p), None) => (p, Vec::new()),
                    (None, Some(m)) => (m, Vec::new()),
                    (None, None) => return Err(BdError::StructureNotFound),
                };
                locs.push(PartitionLoc::Metadata { phys_start, extents, mirror_extents });
            }
            PartitionMap::Other { .. } => {
                locs.push(PartitionLoc::Physical { start: 0 });
            }
        }
    }
    Ok(locs)
}

/// `FileType` of a Metadata File's File Entry (UDF 2.50 §2.2.13.1 / ECMA-167
/// §4/14.6.6) — the primary metadata mapping.
const FILE_TYPE_METADATA: u8 = 250;
/// `FileType` of a Metadata Mirror File's File Entry (UDF 2.50 §2.2.13.1) — the
/// redundant metadata mapping.
const FILE_TYPE_METADATA_MIRROR: u8 = 251;

/// Reads + parses a metadata-file File Entry at absolute `fe_sector` and collects
/// its allocation extents — one candidate mapping for a Metadata partition (the
/// primary or the mirror file). `expected_file_type` is the metadata (250) or
/// metadata-mirror (251) `FileType` this FE must declare.
///
/// `None` when the sector is unreadable, not a File Entry, declares the wrong
/// `FileType`, or maps no blocks (an embedded entry — inline bytes, no extents —
/// or an empty extent list): a metadata file always addresses its partition
/// through allocation extents, so a non-mapping FE cannot stand in for it, and
/// the caller falls back to the mirror.
fn load_metadata_extents(
    reader: &mut dyn ReadSeek,
    locs: &[PartitionLoc],
    fe_sector: u64,
    expected_file_type: u8,
    phys_ref: u16,
    block_size: u64,
    limits: Limits,
) -> Option<Vec<Extent>> {
    let fe_buf = read_sector_io(reader, fe_sector, block_size).ok()?;
    let fe = FileEntry::parse(&fe_buf)?;
    if fe.icb_tag.file_type != expected_file_type {
        return None;
    }
    let extents = collect_extents(reader, locs, &fe, phys_ref, block_size, limits).ok()?;
    // An embedded entry yields no extents (its bytes are inline), so the empty
    // check rejects it too — a metadata file that maps nothing is not a layout.
    if extents.is_empty() {
        return None;
    }
    Some(extents)
}

// ---------------------------------------------------------------------------
// Extent resolution → byte runs
// ---------------------------------------------------------------------------

/// The byte offset of `LengthOfAllocationDescriptors` in an Allocation Extent
/// Descriptor (ECMA-167 §4/14.5): the 16-byte tag, then
/// `PreviousAllocationExtentLocation` (4 bytes).
const AED_L_AD_OFFSET: usize = 20;
/// The byte offset where an Allocation Extent Descriptor's allocation
/// descriptors begin (ECMA-167 §4/14.5) — right after `L_AD`.
const AED_AD_START: usize = 24;

/// The allocation-descriptor bytes of a `NextExtent` continuation block, or
/// `None` if the block does not begin with an Allocation Extent Descriptor.
///
/// ECMA-167 §4/14.5: an allocation extent recorded as a `NextExtent` comprises
/// an AED — a tag with identifier [`TAG_ALLOCATION_EXTENT`] (258), then
/// `PreviousAllocationExtentLocation`, then `L_AD` — followed by the allocation
/// descriptors. The declared `L_AD` bounds the descriptors (slack bytes past it
/// are not descriptors), clamped to the block.
fn aed_allocation_area(area: &[u8]) -> Option<&[u8]> {
    let tag = Tag::parse(area, 0)?;
    if tag.identifier != TAG_ALLOCATION_EXTENT {
        return None;
    }
    let l_ad = as_offset(u32_le(area, AED_L_AD_OFFSET)?);
    let end = AED_AD_START.saturating_add(l_ad).min(area.len());
    area.get(AED_AD_START..end)
}

/// Collects a File Entry's data extents, following `NextExtent` continuation blocks
/// (ECMA-167 §4/12.1) up to [`Limits::max_continuations`], the combined collected +
/// queued count capped at [`Limits::max_extents`]. `locs` is the partition map
/// resolved so far (it may be empty when resolving the metadata file itself, which
/// is physical); `owning_ref` is the partition *reference* the entry's short-form
/// descriptors are relative to.
fn collect_extents(
    reader: &mut dyn ReadSeek,
    locs: &[PartitionLoc],
    fe: &FileEntry,
    owning_ref: u16,
    block_size: u64,
    limits: Limits,
) -> Result<Vec<Extent>, BdError> {
    let alloc_type = fe.icb_tag.allocation_type;
    let mut out: Vec<Extent> = Vec::new();
    let mut pending: VecDeque<Extent> =
        fe.extents().iter().copied().take(limits.max_extents).collect();
    let mut follows = 0_usize;
    while let Some(extent) = pending.pop_front() {
        if extent.kind == ExtentKind::NextExtent {
            follows = follows.saturating_add(1);
            if follows > limits.max_continuations {
                break;
            }
            // An allocation-extent continuation occupies at most one logical
            // block (ECMA-167 §4/12.1), so its read is clamped to `block_size` —
            // a hostile 30-bit length can't force a giant read here.
            let area =
                read_extent_bytes(reader, locs, owning_ref, &extent, block_size, block_size)?;
            // A continuation block must be an Allocation Extent Descriptor;
            // anything else is malformed and contributes no extents.
            let Some(ads) = aed_allocation_area(&area) else { continue };
            for more in parse_allocation_area(ads, alloc_type) {
                if out.len().saturating_add(pending.len()) >= limits.max_extents {
                    break;
                }
                pending.push_back(more);
            }
        } else {
            out.push(extent);
        }
    }
    Ok(out)
}

/// Resolves one data `extent` (not a `NextExtent`) to absolute byte [`Run`]s within
/// `block_size` sectors. A physical partition yields one contiguous run; a Metadata
/// partition splits at the metadata file's extent boundaries; a not-recorded extent
/// yields a single sparse (zero) run. `None` if the extent's blocks do not resolve.
fn extent_runs(
    locs: &[PartitionLoc],
    owning_ref: u16,
    extent: &Extent,
    block_size: u64,
) -> Option<Vec<Run>> {
    let length = u64::from(extent.length);
    if length == 0 {
        return Some(Vec::new());
    }
    if matches!(extent.kind, ExtentKind::NotRecordedAllocated | ExtentKind::NotRecordedNotAllocated)
    {
        // Allocated-but-not-recorded or unallocated: reads as zeros. (A
        // `NextExtent` continuation block, by contrast, holds real recorded bytes,
        // so it resolves like recorded data below — `collect_extents` reads it.)
        return Some(vec![Run { src: None, len: length }]);
    }
    let partition_ref = extent.partition_ref.unwrap_or(owning_ref);
    match locs.get(usize::from(partition_ref))? {
        PartitionLoc::Physical { start } => {
            // `u32`-derived sector/byte math cannot overflow `u64` → saturate.
            let sector = start.saturating_add(u64::from(extent.block));
            let off = sector.saturating_mul(block_size);
            Some(vec![Run { src: Some(off), len: length }])
        }
        PartitionLoc::Metadata { phys_start, extents, .. } => {
            metadata_runs(*phys_start, extents, extent.block, length, block_size)
        }
    }
}

/// Resolves a data `extent` through the metadata **mirror** mapping — the
/// alternative byte runs a failed metadata read retries. `None` for a
/// physical or sparse extent (no alternative location) or when the mirror does
/// not cover the span.
fn mirror_extent_runs(
    locs: &[PartitionLoc],
    owning_ref: u16,
    extent: &Extent,
    block_size: u64,
) -> Option<Vec<Run>> {
    let length = u64::from(extent.length);
    if length == 0
        || matches!(
            extent.kind,
            ExtentKind::NotRecordedAllocated | ExtentKind::NotRecordedNotAllocated
        )
    {
        return None;
    }
    let partition_ref = extent.partition_ref.unwrap_or(owning_ref);
    match locs.get(usize::from(partition_ref))? {
        PartitionLoc::Physical { .. } => None,
        PartitionLoc::Metadata { phys_start, mirror_extents, .. } => {
            metadata_runs(*phys_start, mirror_extents, extent.block, length, block_size)
        }
    }
}

/// Splits a Metadata-partition data extent (`length` bytes starting at logical
/// `start_block`) into physical byte [`Run`]s, coalescing blocks that map to
/// consecutive physical sectors. One pass over the metadata file's `extents`
/// (never per-block, so a hostile 30-bit length can't make this quadratic).
/// `None` if any block of the span fails to resolve.
fn metadata_runs(
    phys_start: u64,
    extents: &[Extent],
    start_block: u32,
    length: u64,
    block_size: u64,
) -> Option<Vec<Run>> {
    // A zero block size is malformed — the one genuine guard; past it the
    // arithmetic cannot divide by zero (the `unwrap_or` is unreachable) nor
    // overflow for `u32`-derived block counts, so it saturates.
    if block_size == 0 {
        return None;
    }
    let want_start = u64::from(start_block);
    let want_end = want_start.saturating_add(length.div_ceil(block_size));
    let mut runs: Vec<Run> = Vec::new();
    // Metadata blocks mapped by the extents walked so far / bytes emitted so far.
    let mut consumed: u64 = 0;
    let mut emitted: u64 = 0;
    for extent in extents {
        let blocks = u64::from(extent.length).checked_div(block_size).unwrap_or(0);
        let next = consumed.saturating_add(blocks);
        // The slice of the wanted block range [want_start, want_end) that this
        // extent (spanning metadata blocks [consumed, next)) covers.
        let lo = want_start.max(consumed);
        let hi = want_end.min(next);
        if lo < hi {
            let within = lo.saturating_sub(consumed);
            let phys_block = u64::from(extent.block).saturating_add(within);
            let off = phys_start.saturating_add(phys_block).saturating_mul(block_size);
            let span = hi.saturating_sub(lo).saturating_mul(block_size);
            let this = span.min(length.saturating_sub(emitted));
            match runs.last_mut() {
                Some(Run { src: Some(prev), len }) if prev.saturating_add(*len) == off => {
                    *len = len.saturating_add(this);
                }
                _ => runs.push(Run { src: Some(off), len: this }),
            }
            emitted = emitted.saturating_add(this);
        }
        consumed = next;
    }
    // A wanted block past the whole mapping leaves `length` uncovered → `None`.
    if emitted < length { None } else { Some(runs) }
}

/// Reads at most `cap` bytes of `extent` from `reader` (used for directory data
/// and continuation blocks) — the extent's self-declared 30-bit length is clamped
/// to the caller's byte budget BEFORE resolving runs, so a hostile length can't
/// force an allocation or read past the budget.
fn read_extent_bytes(
    reader: &mut dyn ReadSeek,
    locs: &[PartitionLoc],
    owning_ref: u16,
    extent: &Extent,
    block_size: u64,
    cap: u64,
) -> Result<Vec<u8>, BdError> {
    let cap32 = u32::try_from(cap).unwrap_or(u32::MAX);
    let length = extent.length.min(cap32);
    let clamped = Extent { length, ..*extent };
    let runs =
        extent_runs(locs, owning_ref, &clamped, block_size).ok_or(BdError::StructureNotFound)?;
    match read_runs(reader, &runs, u64::from(length)) {
        Ok(bytes) => Ok(bytes),
        Err(err) => {
            // A failed metadata-mapped read retries through the mirror's
            // alternative runs before failing.
            if let Some(mirror_runs) = mirror_extent_runs(locs, owning_ref, &clamped, block_size)
                && let Ok(bytes) = read_runs(reader, &mirror_runs, u64::from(length))
            {
                return Ok(bytes);
            }
            Err(err)
        }
    }
}

/// Reads up to `cap` bytes of `runs` from `reader` into a buffer (sparse runs
/// contribute zeros).
fn read_runs(reader: &mut dyn ReadSeek, runs: &[Run], cap: u64) -> Result<Vec<u8>, BdError> {
    let mut out: Vec<u8> = Vec::new();
    for run in runs {
        let have = u64::try_from(out.len()).unwrap_or(u64::MAX);
        let remaining = cap.saturating_sub(have);
        if remaining == 0 {
            break;
        }
        let take = run.len.min(remaining);
        let take_us = usize::try_from(take).unwrap_or(usize::MAX);
        match run.src {
            Some(off) => {
                reader.seek(SeekFrom::Start(off))?;
                let mut buf = vec![0_u8; take_us];
                reader.read_exact(&mut buf)?;
                out.extend_from_slice(&buf);
            }
            None => out.extend(std::iter::repeat_n(0_u8, take_us)),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Directory-tree walk → arena
// ---------------------------------------------------------------------------

/// Walks the whole directory tree from the root ICB into a flat [`Node`] arena
/// (node 0 = root). Bounded by [`Limits::max_nodes`], a visited-ICB set, and
/// [`Limits::max_depth`], so malformed or cyclic input cannot hang.
///
/// The depth cap is what keeps the *recursive* arena consumers
/// ([`UdfDir::collect_from`], `directory_size`) safe: the root sits at depth 0 and
/// each directory's children at depth `d + 1`, so a subdirectory at depth
/// `max_depth` is the deepest one this walk ever expands — its own subdirectories
/// (depth `max_depth + 1`) are dropped (not added, not enqueued; truncation is the
/// accepted hostile-input outcome, no error). No directory node is therefore deeper
/// than `max_depth`, bounding both consumers' recursion. Files are leaves and never
/// recurse, so they are always kept.
fn build_tree(
    reader: &mut dyn ReadSeek,
    volume: &Volume,
    limits: Limits,
) -> Result<Vec<Node>, BdError> {
    let (root_ref, root_block) = volume.root_icb;
    let mut nodes: Vec<Node> = vec![Node {
        name: volume.label.clone(),
        full_name: String::new(),
        parent: None,
        kind: NodeKind::Dir,
    }];
    let mut visited: BTreeSet<(u16, u32)> = BTreeSet::new();
    visited.insert((root_ref, root_block));
    // Directories still to expand: (arena index, partition ref, block, full path,
    // depth). The path travels in the queue so the walk never reads back into
    // `nodes`; the depth bounds the arena's directory nesting (root = depth 0).
    let mut queue: VecDeque<(usize, u16, u32, String, usize)> = VecDeque::new();
    queue.push_back((0, root_ref, root_block, String::new(), 0));

    while let Some((dir_idx, dir_ref, dir_block, parent_path, depth)) = queue.pop_front() {
        let children = expand_directory(reader, volume, &parent_path, dir_ref, dir_block, limits)?;
        for child in children {
            if nodes.len() >= limits.max_nodes {
                break;
            }
            let is_dir = matches!(child.kind, ChildKind::Dir);
            // Drop a subdirectory past the depth cap: its node is the start of a
            // chain the recursive consumers would descend, so truncating it here
            // (the parent at `depth` is the deepest expanded level once
            // `depth == max_depth`) keeps their recursion bounded. Files are leaves
            // — always kept.
            if is_dir && depth >= limits.max_depth {
                continue;
            }
            let icb = (child.partition, child.block);
            if !visited.insert(icb) {
                continue;
            }
            let idx = nodes.len();
            let child_path = child.full_name.clone();
            nodes.push(Node {
                name: child.name,
                full_name: child.full_name,
                parent: Some(dir_idx),
                kind: match child.kind {
                    ChildKind::Dir => NodeKind::Dir,
                    ChildKind::File(body) => NodeKind::File(Arc::new(body)),
                },
            });
            if is_dir {
                queue.push_back((
                    idx,
                    child.partition,
                    child.block,
                    child_path,
                    depth.saturating_add(1),
                ));
            }
        }
    }
    Ok(nodes)
}

/// A resolved child of a directory, before it is placed in the arena.
struct Child {
    /// The entry name.
    name: String,
    /// The synthesized full path.
    full_name: String,
    /// The partition reference of the child's File Entry.
    partition: u16,
    /// The logical block of the child's File Entry.
    block: u32,
    /// Directory or file (with its resolved body).
    kind: ChildKind,
}

/// A [`Child`]'s kind: a directory, or a file with its resolved body.
enum ChildKind {
    /// A subdirectory.
    Dir,
    /// A file with its size + content.
    File(FileBody),
}

/// Reads the directory File Entry at `(dir_ref, dir_block)`, enumerates its File
/// Identifier Descriptors, and resolves each non-parent, non-deleted child into a
/// [`Child`]. A directory whose File Entry or data cannot be read yields no
/// children (it stays an empty directory) rather than failing the whole scan.
fn expand_directory(
    reader: &mut dyn ReadSeek,
    volume: &Volume,
    parent_path: &str,
    dir_ref: u16,
    dir_block: u32,
    limits: Limits,
) -> Result<Vec<Child>, BdError> {
    let block_size = volume.block_size;
    let Some(sector) = resolve_sector(&volume.locs, dir_ref, u64::from(dir_block), block_size)
    else {
        return Ok(Vec::new());
    };
    let fe_buf = read_sector_with_mirror(
        reader,
        &volume.locs,
        dir_ref,
        u64::from(dir_block),
        sector,
        block_size,
    )?;
    let Some(fe) = FileEntry::parse(&fe_buf) else {
        return Ok(Vec::new());
    };
    if !fe.is_directory() {
        return Ok(Vec::new());
    }
    let dir_bytes = directory_bytes(reader, volume, &fe, dir_ref, limits)?;
    let mut children: Vec<Child> = Vec::new();
    for fid in parse_directory(&dir_bytes) {
        if fid.is_parent() || fid.is_deleted() {
            continue;
        }
        let child_ref = fid.icb.location.partition;
        let child_block = fid.icb.location.block;
        let Some(child_sector) =
            resolve_sector(&volume.locs, child_ref, u64::from(child_block), block_size)
        else {
            continue;
        };
        let Ok(child_buf) = read_sector_with_mirror(
            reader,
            &volume.locs,
            child_ref,
            u64::from(child_block),
            child_sector,
            block_size,
        ) else {
            continue;
        };
        let Some(child_fe) = FileEntry::parse(&child_buf) else {
            continue;
        };
        let full_name = format!("{parent_path}/{}", fid.name);
        let kind = if child_fe.is_directory() {
            ChildKind::Dir
        } else {
            ChildKind::File(file_body(reader, volume, &child_fe, child_ref, limits)?)
        };
        children.push(Child {
            name: fid.name,
            full_name,
            partition: child_ref,
            block: child_block,
            kind,
        });
    }
    Ok(children)
}

/// Reads a directory's raw FID bytes: the inline data of an embedded entry, or the
/// concatenation of its allocation extents, each read clamped to the *remaining*
/// [`Limits::max_dir_bytes`] budget (a hostile `InformationLength`/extent length
/// can't force an over-budget allocation even for a single extent).
fn directory_bytes(
    reader: &mut dyn ReadSeek,
    volume: &Volume,
    fe: &FileEntry,
    own_ref: u16,
    limits: Limits,
) -> Result<Vec<u8>, BdError> {
    if let Some(embedded) = fe.embedded_data() {
        return Ok(embedded.to_vec());
    }
    let extents = collect_extents(reader, &volume.locs, fe, own_ref, volume.block_size, limits)?;
    let mut bytes: Vec<u8> = Vec::new();
    for extent in &extents {
        let have = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let remaining = limits.max_dir_bytes.saturating_sub(have);
        if remaining == 0 {
            break;
        }
        let chunk =
            read_extent_bytes(reader, &volume.locs, own_ref, extent, volume.block_size, remaining)?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Resolves a file's [`FileBody`] — its `InformationLength` plus either its embedded
/// bytes or the byte runs of its (continuation-followed) allocation extents.
fn file_body(
    reader: &mut dyn ReadSeek,
    volume: &Volume,
    fe: &FileEntry,
    own_ref: u16,
    limits: Limits,
) -> Result<FileBody, BdError> {
    let length = fe.information_length;
    if let Some(embedded) = fe.embedded_data() {
        return Ok(FileBody { length, content: Content::Embedded(embedded.to_vec()) });
    }
    let extents = collect_extents(reader, &volume.locs, fe, own_ref, volume.block_size, limits)?;
    let mut runs: Vec<Run> = Vec::new();
    for extent in &extents {
        if let Some(mut resolved) = extent_runs(&volume.locs, own_ref, extent, volume.block_size) {
            // The per-file run list shares the extent cap — a hostile metadata
            // mapping can shatter one extent into many runs.
            let room = limits.max_extents.saturating_sub(runs.len());
            resolved.truncate(room);
            runs.append(&mut resolved);
        }
    }
    Ok(FileBody { length, content: Content::Runs(runs) })
}

#[cfg(test)]
#[expect(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::as_conversions,
    reason = "test scaffolding builds .iso byte layouts with controlled, in-range offsets/sizes"
)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use proptest::prelude::{any, prop_assert_eq, proptest};

    use super::{
        BdDir, BdFile, Content, EmbeddedReader, ExtentKind, FileEntry, IsoReader, Limits, Node,
        NodeKind, PartitionLoc, PathIso, Run, UdfDir, UdfFileReader, UdfInner, UdfSource, Volume,
        build_tree, collect_extents, directory_bytes, expand_directory, extension_of, extent_runs,
        file_body, metadata_runs, metadata_sector, offset_by, parse_volume, read_runs,
        resolve_partitions, resolve_sector,
    };
    use crate::error::ScanStage;
    use crate::vfs::SearchOption;
    use crate::vfs::udf::descriptor::{
        Lvd, MetadataPartitionMap, PartitionDescriptor, PartitionMap,
    };
    use crate::vfs::udf::{Extent, LbAddr, LongAd};

    /// The logical sector size every fixture uses (the universal UDF/BD value).
    const SS: usize = 2048;

    /// Writes `data` at `off` in `buf` (branchless — an out-of-range tail is simply
    /// not written, so it adds no uncovered region).
    fn put(buf: &mut [u8], off: usize, data: &[u8]) {
        for (dst, &src) in buf.iter_mut().skip(off).zip(data) {
            *dst = src;
        }
    }

    /// Sets byte 4 of the 16-byte tag at `off` to its checksum.
    fn fix_tag(buf: &mut [u8], off: usize) {
        let mut sum: u8 = 0;
        for (i, b) in buf.iter().skip(off).take(16).enumerate() {
            if i != 4 {
                sum = sum.wrapping_add(*b);
            }
        }
        put(buf, off + 4, &[sum]);
    }

    /// An 8-byte `short_ad` with the given extent kind, length, and block.
    fn sad(kind: u8, len: u32, block: u32) -> Vec<u8> {
        let raw = u32::from(kind).wrapping_shl(30) | (len & 0x3FFF_FFFF);
        let mut v = raw.to_le_bytes().to_vec();
        v.extend_from_slice(&block.to_le_bytes());
        v
    }

    /// A 16-byte `long_ad` with the given kind, length, block, and partition ref.
    fn lad(kind: u8, len: u32, block: u32, part: u16) -> Vec<u8> {
        let raw = u32::from(kind).wrapping_shl(30) | (len & 0x3FFF_FFFF);
        let mut v = raw.to_le_bytes().to_vec();
        v.extend_from_slice(&block.to_le_bytes());
        v.extend_from_slice(&part.to_le_bytes());
        v.extend_from_slice(&[0_u8; 6]);
        v
    }

    /// A sector-sized File Entry: tag `id` (261 FE / 266 EFE), `file_type`, ICB
    /// `flags` (alloc type in the low 3 bits), `info_len`, and the allocation /
    /// embedded `ad` bytes placed in the AD area.
    fn fe(id: u16, file_type: u8, flags: u16, info_len: u64, ad: &[u8]) -> Vec<u8> {
        let mut buf = vec![0_u8; SS];
        put(&mut buf, 0, &id.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        put(&mut buf, 20, &4_u16.to_le_bytes()); // ICB tag StrategyType (16 + 4)
        put(&mut buf, 27, &[file_type]); // ICB tag FileType (16 + 11)
        put(&mut buf, 34, &flags.to_le_bytes()); // ICB tag flags (16 + 18)
        put(&mut buf, 56, &info_len.to_le_bytes());
        let (l_ea, l_ad, ad_base) = if id == 266 { (208, 212, 216) } else { (168, 172, 176) };
        put(&mut buf, l_ea, &0_u32.to_le_bytes());
        put(&mut buf, l_ad, &u32::try_from(ad.len()).unwrap_or(0).to_le_bytes());
        put(&mut buf, ad_base, ad);
        fix_tag(&mut buf, 0);
        buf
    }

    /// A padded File Identifier Descriptor naming `name` (Latin-1) at child ICB
    /// `(part, block)`, with the given `FileCharacteristics`.
    fn fid(chars: u8, name: &str, block: u32, part: u16) -> Vec<u8> {
        let mut cs0 = Vec::new();
        if !name.is_empty() {
            cs0.push(8_u8); // OSTA CS0 compression id 8 (Latin-1)
            cs0.extend_from_slice(name.as_bytes());
        }
        let raw = 38 + cs0.len();
        let padded = (raw + 3) & !3_usize;
        let mut buf = vec![0_u8; padded];
        put(&mut buf, 0, &257_u16.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        put(&mut buf, 18, &[chars]);
        put(&mut buf, 19, &[u8::try_from(cs0.len()).unwrap_or(0)]);
        put(&mut buf, 20, &0x800_u32.to_le_bytes()); // icb length
        put(&mut buf, 24, &block.to_le_bytes());
        put(&mut buf, 28, &part.to_le_bytes());
        put(&mut buf, 36, &0_u16.to_le_bytes()); // L_IU
        put(&mut buf, 38, &cs0);
        fix_tag(&mut buf, 0);
        buf
    }

    /// An Allocation Extent Descriptor continuation block (ECMA-167 §4/14.5):
    /// tag 258 + `L_AD` = `ads.len()` + the allocation descriptors at offset 24.
    fn aed(ads: &[u8]) -> Vec<u8> {
        let mut buf = vec![0_u8; 24 + ads.len()];
        put(&mut buf, 0, &258_u16.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        put(&mut buf, 20, &u32::try_from(ads.len()).unwrap_or(0).to_le_bytes());
        put(&mut buf, 24, ads);
        fix_tag(&mut buf, 0);
        buf
    }

    /// Concatenates FIDs into a directory data block.
    fn dir_data(fids: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for f in fids {
            out.extend_from_slice(f);
        }
        out
    }

    /// An AVDP sector pointing the Main VDS extent at `loc` for `len` bytes.
    fn avdp(loc: u32, len: u32) -> Vec<u8> {
        let mut buf = vec![0_u8; SS];
        put(&mut buf, 0, &2_u16.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        put(&mut buf, 16, &len.to_le_bytes());
        put(&mut buf, 20, &loc.to_le_bytes());
        fix_tag(&mut buf, 0);
        buf
    }

    /// An AVDP sector with both the Main and Reserve VDS extents populated
    /// (the reserve fields sit outside the checksummed tag bytes).
    fn avdp_full(loc: u32, len: u32, res_loc: u32, res_len: u32) -> Vec<u8> {
        let mut buf = avdp(loc, len);
        put(&mut buf, 24, &res_len.to_le_bytes());
        put(&mut buf, 28, &res_loc.to_le_bytes());
        buf
    }

    /// A Volume Descriptor Pointer sector → the continuation VDS extent.
    fn vdp(loc: u32, len: u32) -> Vec<u8> {
        let mut buf = vec![0_u8; SS];
        put(&mut buf, 0, &3_u16.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        put(&mut buf, 20, &len.to_le_bytes());
        put(&mut buf, 24, &loc.to_le_bytes());
        fix_tag(&mut buf, 0);
        buf
    }

    /// A Primary Volume Descriptor sector with the given `VolumeIdentifier`.
    fn pvd(label: &str) -> Vec<u8> {
        let mut buf = vec![0_u8; SS];
        put(&mut buf, 0, &1_u16.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        put(&mut buf, 24, &[8]); // dstring[32] at 24: compId 8 + Latin-1 label
        put(&mut buf, 25, label.as_bytes());
        put(&mut buf, 55, &[u8::try_from(1 + label.len()).unwrap_or(0)]);
        fix_tag(&mut buf, 0);
        buf
    }

    /// A Terminating Descriptor sector (tag 8) — ends a VDS.
    fn term() -> Vec<u8> {
        let mut buf = vec![0_u8; SS];
        put(&mut buf, 0, &8_u16.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        fix_tag(&mut buf, 0);
        buf
    }

    /// Stamps a VDS descriptor's `VolumeDescriptorSequenceNumber` (offset 16 —
    /// outside the checksummed tag bytes, so no checksum refix is needed).
    fn with_vdsn(mut desc: Vec<u8>, vdsn: u32) -> Vec<u8> {
        put(&mut desc, 16, &vdsn.to_le_bytes());
        desc
    }

    /// A Partition Descriptor sector.
    fn pd(number: u16, start: u32, length: u32) -> Vec<u8> {
        let mut buf = vec![0_u8; SS];
        put(&mut buf, 0, &5_u16.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        put(&mut buf, 22, &number.to_le_bytes());
        put(&mut buf, 188, &start.to_le_bytes());
        put(&mut buf, 192, &length.to_le_bytes());
        fix_tag(&mut buf, 0);
        buf
    }

    /// A type-1 (physical) partition map.
    fn phys_map(number: u16) -> Vec<u8> {
        let mut v = vec![1_u8, 6, 0, 0];
        v.extend_from_slice(&number.to_le_bytes());
        v
    }

    /// A type-2 UDF Metadata partition map.
    fn meta_map(phys: u16, meta_loc: u32, mirror: u32) -> Vec<u8> {
        let mut v = vec![0_u8; 64];
        put(&mut v, 0, &[2, 64]);
        put(&mut v, 5, b"*UDF Metadata Partition");
        put(&mut v, 38, &phys.to_le_bytes());
        put(&mut v, 40, &meta_loc.to_le_bytes());
        put(&mut v, 44, &mirror.to_le_bytes());
        v
    }

    /// A Logical Volume Descriptor sector (the FSD extent spans one block).
    fn lvd(
        label: &str,
        bs: u32,
        fsd_block: u32,
        fsd_part: u16,
        maps: &[u8],
        num_maps: u32,
    ) -> Vec<u8> {
        lvd_with_fsd_len(label, bs, fsd_block, fsd_part, 0x800, maps, num_maps)
    }

    /// A Logical Volume Descriptor sector with an explicit FSD extent byte length
    /// `fsd_len` (to emit a multi-block File Set Descriptor extent).
    fn lvd_with_fsd_len(
        label: &str,
        bs: u32,
        fsd_block: u32,
        fsd_part: u16,
        fsd_len: u32,
        maps: &[u8],
        num_maps: u32,
    ) -> Vec<u8> {
        let mut buf = vec![0_u8; SS];
        put(&mut buf, 0, &6_u16.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        // LogicalVolumeIdentifier dstring[128] at 84: compId 8 + Latin-1 label.
        put(&mut buf, 84, &[8]);
        put(&mut buf, 85, label.as_bytes());
        put(&mut buf, 211, &[u8::try_from(1 + label.len()).unwrap_or(0)]); // used length
        put(&mut buf, 212, &bs.to_le_bytes());
        // LogicalVolumeContentsUse (248): a long_ad to the FSD.
        put(&mut buf, 248, &fsd_len.to_le_bytes());
        put(&mut buf, 252, &fsd_block.to_le_bytes());
        put(&mut buf, 256, &fsd_part.to_le_bytes());
        put(&mut buf, 264, &u32::try_from(maps.len()).unwrap_or(0).to_le_bytes()); // MapTableLength
        put(&mut buf, 268, &num_maps.to_le_bytes());
        put(&mut buf, 440, maps);
        fix_tag(&mut buf, 0);
        buf
    }

    /// A File Set Descriptor sector pointing the root directory ICB at
    /// `(root_part, root_block)`.
    fn fsd(root_part: u16, root_block: u32) -> Vec<u8> {
        let mut buf = vec![0_u8; SS];
        put(&mut buf, 0, &0_u16.to_le_bytes()); // FSD tag id 256, written below
        put(&mut buf, 0, &256_u16.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        put(&mut buf, 400, &0x800_u32.to_le_bytes());
        put(&mut buf, 404, &root_block.to_le_bytes());
        put(&mut buf, 408, &root_part.to_le_bytes());
        fix_tag(&mut buf, 0);
        buf
    }

    /// An in-memory `.iso` builder placing sectors into a byte image.
    struct Iso {
        bytes: Vec<u8>,
    }

    impl Iso {
        fn new(sectors: usize) -> Self {
            Self { bytes: vec![0_u8; sectors * SS] }
        }

        /// Writes `data` starting at the byte offset of `sector`.
        fn write(&mut self, sector: usize, data: &[u8]) {
            put(&mut self.bytes, sector * SS, data);
        }

        fn into_bytes(self) -> Vec<u8> {
            self.bytes
        }
    }

    /// An [`IsoReader`] over an in-memory image (each handle gets its own cursor).
    #[derive(Debug, Clone)]
    struct MemIso {
        data: Arc<[u8]>,
    }

    impl MemIso {
        fn boxed(bytes: Vec<u8>) -> Box<dyn IsoReader> {
            Box::new(Self { data: Arc::from(bytes) })
        }
    }

    impl IsoReader for MemIso {
        fn open(&self) -> std::io::Result<Box<dyn super::ReadSeek>> {
            Ok(Box::new(std::io::Cursor::new(self.data.to_vec())))
        }
    }

    /// A bare **strict** [`UdfFileReader`] over `stream` (a dummy inner) — for the
    /// direct reader tests; the resilient path is exercised through [`FaultyIso`].
    fn raw_reader(
        stream: Box<dyn super::ReadSeek>,
        runs: Vec<Run>,
        length: u64,
        pos: u64,
    ) -> UdfFileReader {
        UdfFileReader {
            inner: Arc::new(UdfInner {
                factory: MemIso::boxed(Vec::new()),
                nodes: Vec::new(),
                label: String::new(),
                resilient: false,
                errors: std::sync::Mutex::new(Vec::new()),
            }),
            name: "raw".to_owned(),
            reported: false,
            stream,
            runs,
            length,
            pos,
        }
    }

    /// Reads a VFS file fully.
    fn read_all(file: &dyn BdFile) -> Vec<u8> {
        let mut reader = file.open_read().expect("open_read");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).expect("read_to_end");
        bytes
    }

    /// Sorted names of a directory's files.
    fn file_names(dir: &dyn BdDir) -> Vec<String> {
        let mut names: Vec<String> =
            dir.get_files().expect("get_files").iter().map(|f| f.name().to_owned()).collect();
        names.sort();
        names
    }

    /// Sorted names of a directory's subdirectories.
    fn dir_names(dir: &dyn BdDir) -> Vec<String> {
        let mut names: Vec<String> = dir
            .get_directories()
            .expect("get_directories")
            .iter()
            .map(|d| d.name().to_owned())
            .collect();
        names.sort();
        names
    }

    // ── A physical-partition disc exercising the full tree + reader ───────────

    /// Builds a single-physical-partition `.iso`:
    /// - root (extent-based dir) → `SUB` (embedded EFE dir) + `data.bin` (two-extent file) +
    ///   `tiny.txt` (embedded file) + `hide.x` (hidden short-extent file) + a deleted FID + a
    ///   duplicate FID (visited-guard) + `sparse.bin` (a not-recorded extent → zero-filled).
    /// - `SUB` → `leaf.txt` (embedded file).
    fn physical_iso() -> Vec<u8> {
        let mut iso = Iso::new(300);
        // Volume chain. The VDS spans 3 sectors: PD, LVD, and a zero sector (which
        // parses as neither — exercising the descriptor-skip path).
        iso.write(256, &avdp(257, 3 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        let mut maps = phys_map(0);
        maps.extend_from_slice(&[9, 6, 0, 0, 0, 0]); // an unknown (Other) map at ref 1
        iso.write(258, &lvd("VOLPHYS", SS as u32, 1, 0, &maps, 2));
        // FSD at partition 0 block 1 → sector 261; root ICB part 0 block 2.
        iso.write(261, &fsd(0, 2));
        // Partition 0 starts at sector 260, so block N → sector 260 + N. Root dir
        // FE at block 2 (sector 262); its FID data at block 3 (sector 263).
        let root_fids = dir_data(&[
            fid(0x0A, "", 2, 0),            // parent
            fid(0x02, "SUB", 5, 0),         // subdirectory (sector 265)
            fid(0x00, "data.bin", 6, 0),    // two-extent file (sector 266)
            fid(0x00, "tiny.txt", 7, 0),    // embedded file (sector 267)
            fid(0x05, "gone", 9, 0),        // deleted (0x04) → skipped
            fid(0x01, "hide.x", 8, 0),      // hidden → kept (sector 268)
            fid(0x00, "dup", 6, 0),         // same ICB as data.bin → visited skip
            fid(0x00, "sparse.bin", 10, 0), // not-recorded extent (sector 270)
        ]);
        let root_len = root_fids.len() as u64;
        iso.write(262, &fe(261, 4, 0, root_len, &sad(0, root_len as u32, 3)));
        iso.write(263, &root_fids);
        // SUB: embedded EFE directory with one child (leaf.txt at block 11 → 271).
        let sub_fids = dir_data(&[fid(0x0A, "", 5, 0), fid(0x00, "leaf.txt", 11, 0)]);
        iso.write(265, &fe(266, 4, 3, sub_fids.len() as u64, &sub_fids));
        // data.bin: two short extents (2048 + 100 bytes) at blocks 20, 21.
        let mut data_ads = sad(0, SS as u32, 20);
        data_ads.extend_from_slice(&sad(0, 100, 21));
        iso.write(266, &fe(261, 5, 0, SS as u64 + 100, &data_ads));
        iso.write(280, &vec![0xAB_u8; SS]); // block 20 → sector 280
        iso.write(281, &vec![0xCD_u8; SS]); // block 21 → sector 281
        // tiny.txt: embedded "hello".
        iso.write(267, &fe(261, 5, 3, 5, b"hello"));
        // hide.x: one short extent of 10 bytes at block 22 → sector 282.
        iso.write(268, &fe(261, 5, 0, 10, &sad(0, 10, 22)));
        iso.write(282, b"0123456789");
        // sparse.bin: one not-recorded (kind 1) extent of 16 bytes → zeros.
        iso.write(270, &fe(261, 5, 0, 16, &sad(1, 16, 99)));
        // leaf.txt: embedded "leaf".
        iso.write(271, &fe(261, 5, 3, 4, b"leaf"));
        // The deleted `gone` FID (block 9 → sector 269) points at a *valid* file FE,
        // so it is excluded only because it is deleted (not because its FE is bad).
        iso.write(269, &fe(261, 5, 3, 4, b"gone"));
        iso.into_bytes()
    }

    fn open_physical() -> UdfSource {
        UdfSource::open(MemIso::boxed(physical_iso())).expect("open physical iso")
    }

    #[test]
    fn physical_disc_volume_label_and_root_listing() {
        let src = open_physical();
        assert_eq!(src.volume_label(), "VOLPHYS");
        let root = src.root();
        assert_eq!(root.name(), "VOLPHYS");
        assert_eq!(root.full_name(), "");
        assert!(root.parent().is_none());
        // `dup` collapses onto data.bin (visited), `gone` is deleted, `SUB` is a dir.
        assert_eq!(
            file_names(&root),
            vec![
                "data.bin".to_owned(),
                "hide.x".to_owned(),
                "sparse.bin".to_owned(),
                "tiny.txt".to_owned(),
            ]
        );
        assert_eq!(dir_names(&root), vec!["SUB".to_owned()]);
    }

    #[test]
    fn physical_disc_reads_multi_extent_embedded_and_sparse_files() {
        let src = open_physical();
        let root = src.root();
        let files = root.get_files().expect("files");
        let by = |name: &str| files.iter().find(|f| f.name() == name).expect("file present");

        // Two-extent file: 2048 bytes of 0xAB then 100 of 0xCD.
        let data = by("data.bin");
        assert_eq!(data.length(), SS as u64 + 100);
        assert_eq!(data.extension(), ".bin");
        assert_eq!(data.full_name(), "/data.bin");
        assert!(!data.is_dir());
        let bytes = read_all(&**data);
        assert_eq!(bytes.len(), SS + 100);
        assert!(bytes.iter().take(SS).all(|&b| b == 0xAB));
        assert!(bytes.iter().skip(SS).all(|&b| b == 0xCD));

        // Embedded file via open_text.
        let tiny = by("tiny.txt");
        assert_eq!(tiny.length(), 5);
        let mut text = String::new();
        tiny.open_text().expect("open_text").read_to_string(&mut text).expect("read text");
        assert_eq!(text, "hello");

        // Hidden file kept, read fully.
        assert_eq!(read_all(&**by("hide.x")), b"0123456789");

        // Not-recorded extent reads as zeros.
        assert_eq!(read_all(&**by("sparse.bin")), vec![0_u8; 16]);
    }

    #[test]
    fn physical_disc_nested_dir_parent_and_glob_recursion() {
        let src = open_physical();
        let root = src.root();
        let dirs = root.get_directories().expect("dirs");
        let sub = dirs.iter().find(|d| d.name() == "SUB").expect("SUB present");
        assert_eq!(sub.full_name(), "/SUB");
        assert_eq!(file_names(&**sub), vec!["leaf.txt".to_owned()]);
        // Parent of SUB is the root; the root has no parent.
        assert_eq!(sub.parent().expect("parent").name(), "VOLPHYS");

        // Top-only glob: only the root's *.txt (tiny.txt).
        let top = root.get_files_pattern("*.txt").expect("glob top");
        assert_eq!(top.iter().map(|f| f.name().to_owned()).collect::<Vec<_>>(), vec!["tiny.txt"]);

        // Recursive glob: tiny.txt + leaf.txt across the tree.
        let mut deep: Vec<String> = root
            .get_files_pattern_option("*.txt", SearchOption::AllDirectories)
            .expect("glob deep")
            .iter()
            .map(|f| f.name().to_owned())
            .collect();
        deep.sort();
        assert_eq!(deep, vec!["leaf.txt".to_owned(), "tiny.txt".to_owned()]);
    }

    // ── Embedded-file length parity (InformationLength vs L_AD) ──────────────

    /// Builds a single-physical-partition `.iso` whose root holds one embedded
    /// file `F` with `info_len` `InformationLength` but only the inline bytes
    /// `inline` (its `L_AD`). Partition 0 starts at sector 260 (block N → sector
    /// 260 + N): FSD at block 1, embedded root directory at block 2, `F` at
    /// block 3.
    fn embedded_file_iso(info_len: u64, inline: &[u8]) -> Vec<u8> {
        let mut iso = Iso::new(264);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &lvd("VOLEMB", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(261, &fsd(0, 2));
        let fids = dir_data(&[fid(0x0A, "", 2, 0), fid(0x00, "F", 3, 0)]);
        iso.write(262, &fe(261, 4, 3, fids.len() as u64, &fids));
        iso.write(263, &fe(261, 5, 3, info_len, inline));
        iso.into_bytes()
    }

    #[test]
    fn embedded_file_zero_extends_past_its_inline_bytes() {
        // InformationLength 8 but only "hi" (L_AD 2) inline → reads "hi" then
        // zero-fills to the reported length, matching the runs path.
        let src = UdfSource::open(MemIso::boxed(embedded_file_iso(8, b"hi"))).expect("open");
        let files = src.root().get_files().expect("files");
        let f = files.iter().find(|f| f.name() == "F").expect("F present");
        assert_eq!(f.length(), 8);
        assert_eq!(read_all(&**f), b"hi\0\0\0\0\0\0");
    }

    #[test]
    fn embedded_file_truncates_to_a_short_information_length() {
        // InformationLength 3 but five inline bytes → only the first three are
        // served (the report never sees the extra inline slack).
        let src = UdfSource::open(MemIso::boxed(embedded_file_iso(3, b"abcde"))).expect("open");
        let files = src.root().get_files().expect("files");
        let f = files.iter().find(|f| f.name() == "F").expect("F present");
        assert_eq!(f.length(), 3);
        assert_eq!(read_all(&**f), b"abc");
    }

    #[test]
    fn embedded_reader_serves_recorded_then_zero_fill_in_chunks() {
        // A 2-byte buffer over a length-5 reader with 3 inline bytes: the first
        // read returns the recorded "AB", the second "C\0", the third "\0\0",
        // and the fourth (at EOF) returns 0.
        let mut reader = EmbeddedReader { bytes: b"ABC".to_vec(), length: 5, pos: 0 };
        let mut buf = [0xFF_u8; 2];
        assert_eq!(reader.read(&mut buf).expect("read 1"), 2);
        assert_eq!(&buf, b"AB");
        assert_eq!(reader.read(&mut buf).expect("read 2"), 2);
        assert_eq!(&buf, b"C\0");
        assert_eq!(reader.read(&mut buf).expect("read 3"), 1);
        assert_eq!(buf[0], 0);
        assert_eq!(reader.read(&mut buf).expect("read 4"), 0);
    }

    #[test]
    fn embedded_reader_empty_buffer_reads_zero_without_advancing() {
        let mut reader = EmbeddedReader { bytes: b"ABC".to_vec(), length: 3, pos: 0 };
        assert_eq!(reader.read(&mut []).expect("empty read"), 0);
        // The position is untouched, so a real read still starts at the front.
        let mut buf = [0_u8; 3];
        assert_eq!(reader.read(&mut buf).expect("read"), 3);
        assert_eq!(&buf, b"ABC");
    }

    #[test]
    fn embedded_reader_seek_covers_each_anchor_and_eof() {
        let mut reader = EmbeddedReader { bytes: b"ABCD".to_vec(), length: 6, pos: 0 };
        // SeekFrom::End lands at the zero-fill tail.
        assert_eq!(reader.seek(SeekFrom::End(-1)).expect("seek end"), 5);
        let mut buf = [0xFF_u8; 4];
        assert_eq!(reader.read(&mut buf).expect("read tail"), 1);
        assert_eq!(buf[0], 0);
        // SeekFrom::Start then SeekFrom::Current re-anchor at a recorded byte.
        assert_eq!(reader.seek(SeekFrom::Start(1)).expect("seek start"), 1);
        assert_eq!(reader.seek(SeekFrom::Current(1)).expect("seek current"), 2);
        assert_eq!(reader.read(&mut buf).expect("read mid"), 4);
        assert_eq!(&buf, b"CD\0\0");
        // Seeking before the start is an error (the position is left unchanged).
        assert!(reader.seek(SeekFrom::Current(-100)).is_err());
    }

    // ── A metadata-partition disc (UDF 2.50) ─────────────────────────────────

    /// Builds an `.iso` whose root tree lives in a UDF 2.50 Metadata partition; a
    /// file's data lives in the physical partition (the authored-BD layout).
    fn metadata_iso() -> Vec<u8> {
        let mut iso = Iso::new(330);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 300, 100));
        let mut maps = phys_map(0);
        maps.extend_from_slice(&meta_map(0, 2, 3)); // metadata file at physical block 2
        iso.write(258, &lvd("VOLMETA", SS as u32, 1, 1, &maps, 2));
        // Metadata file FE (FileType 250) at physical block 2 → sector 302: maps
        // metadata blocks 0..6 onto physical blocks 10..16 (sectors 310..316).
        iso.write(302, &fe(261, 250, 0, 6 * SS as u64, &sad(0, 6 * SS as u32, 10)));
        // FSD at metadata block 1 → sector 311; root ICB metadata block 2.
        iso.write(311, &fsd(1, 2));
        // Root dir FE at metadata block 2 → sector 312; data at metadata block 3.
        let root_fids = dir_data(&[fid(0x0A, "", 2, 1), fid(0x00, "FILE.BIN", 4, 1)]);
        iso.write(312, &fe(261, 4, 0, root_fids.len() as u64, &sad(0, root_fids.len() as u32, 3)));
        iso.write(313, &root_fids);
        // FILE.BIN FE at metadata block 4 → sector 314; data via long_ad to the
        // physical partition (ref 0) block 20 → sector 320.
        iso.write(314, &fe(261, 5, 1, 64, &lad(0, 64, 20, 0)));
        iso.write(320, b"physical-partition-data-bytes-payload-0123456789ABCDEF0123456789");
        iso.into_bytes()
    }

    #[test]
    fn metadata_partition_disc_resolves_through_the_metadata_file() {
        let src = UdfSource::open(MemIso::boxed(metadata_iso())).expect("open metadata iso");
        assert_eq!(src.volume_label(), "VOLMETA");
        let root = src.root();
        assert_eq!(file_names(&root), vec!["FILE.BIN".to_owned()]);
        let files = root.get_files().expect("files");
        let file = files.first().expect("FILE.BIN");
        assert_eq!(file.length(), 64);
        assert_eq!(
            read_all(&**file),
            b"physical-partition-data-bytes-payload-0123456789ABCDEF0123456789"
        );
    }

    // ── The metadata mirror ──────────────────────────────────────────────────

    /// Like [`metadata_iso`] but with a real **mirror** metadata file: the
    /// primary FE (physical block 2 → sector 302) maps metadata blocks 0..6
    /// onto physical blocks 10..16 (sectors 310..316); the mirror FE (block 3 →
    /// sector 303) maps them onto blocks 40..46 (sectors 340..346). The
    /// metadata content (FSD, root FE, FIDs, child FE) is written through both
    /// mappings, so either copy alone can serve every metadata read.
    fn mirrored_metadata_iso() -> Vec<u8> {
        let mut iso = Iso::new(360);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 300, 100));
        let mut maps = phys_map(0);
        maps.extend_from_slice(&meta_map(0, 2, 3));
        iso.write(258, &lvd("VOLMIR", SS as u32, 1, 1, &maps, 2));
        iso.write(302, &fe(261, 250, 0, 6 * SS as u64, &sad(0, 6 * SS as u32, 10)));
        iso.write(303, &fe(261, 251, 0, 6 * SS as u64, &sad(0, 6 * SS as u32, 40)));
        let root_fids = dir_data(&[fid(0x0A, "", 2, 1), fid(0x00, "FILE.BIN", 4, 1)]);
        for base in [310_usize, 340] {
            iso.write(base + 1, &fsd(1, 2));
            iso.write(
                base + 2,
                &fe(261, 4, 0, root_fids.len() as u64, &sad(0, root_fids.len() as u32, 3)),
            );
            iso.write(base + 3, &root_fids);
            iso.write(base + 4, &fe(261, 5, 1, 64, &lad(0, 64, 20, 0)));
        }
        iso.write(320, b"physical-partition-data-bytes-payload-0123456789ABCDEF0123456789");
        iso.into_bytes()
    }

    /// Opens `bytes` with the absolute byte range of `sector` injected as a bad
    /// sector, and asserts the volume still serves its full tree (the mirror
    /// recovery contract).
    fn assert_opens_via_mirror(bytes: Vec<u8>, sector: u64) {
        let bad = (sector * SS as u64)..((sector + 1) * SS as u64);
        let src = UdfSource::open(FaultyIso::boxed(bytes, bad))
            .expect("the mirror serves every injected bad metadata sector");
        assert_eq!(src.volume_label(), "VOLMIR");
        let root = src.root();
        assert_eq!(file_names(&root), vec!["FILE.BIN".to_owned()]);
        let files = root.get_files().expect("files");
        assert_eq!(
            read_all(&**files.first().expect("FILE.BIN")),
            b"physical-partition-data-bytes-payload-0123456789ABCDEF0123456789"
        );
    }

    #[test]
    fn mirror_extent_runs_covers_each_arm() {
        use super::mirror_extent_runs;
        let locs = vec![
            PartitionLoc::Physical { start: 100 },
            PartitionLoc::Metadata {
                phys_start: 200,
                extents: Vec::new(),
                mirror_extents: vec![Extent {
                    partition_ref: None,
                    block: 7,
                    length: 4 * SS as u32,
                    kind: ExtentKind::RecordedAllocated,
                }],
            },
        ];
        let rec = |length: u32, kind| Extent { partition_ref: Some(1), block: 1, length, kind };
        // A metadata extent the mirror covers → the alternative runs.
        assert_eq!(
            mirror_extent_runs(&locs, 0, &rec(SS as u32, ExtentKind::RecordedAllocated), SS as u64),
            Some(vec![Run { src: Some(208 * SS as u64), len: SS as u64 }])
        );
        // Zero-length and sparse extents have nothing to retry.
        assert_eq!(
            mirror_extent_runs(&locs, 0, &rec(0, ExtentKind::RecordedAllocated), SS as u64),
            None
        );
        assert_eq!(
            mirror_extent_runs(&locs, 0, &rec(16, ExtentKind::NotRecordedAllocated), SS as u64),
            None
        );
        assert_eq!(
            mirror_extent_runs(&locs, 0, &rec(16, ExtentKind::NotRecordedNotAllocated), SS as u64),
            None
        );
        // A physical extent has no alternative location.
        let phys = Extent {
            partition_ref: Some(0),
            block: 1,
            length: 16,
            kind: ExtentKind::RecordedAllocated,
        };
        assert_eq!(mirror_extent_runs(&locs, 0, &phys, SS as u64), None);
        // An out-of-range partition reference resolves to nothing.
        let oob = Extent {
            partition_ref: Some(9),
            block: 1,
            length: 16,
            kind: ExtentKind::RecordedAllocated,
        };
        assert_eq!(mirror_extent_runs(&locs, 0, &oob, SS as u64), None);
    }

    #[test]
    fn mirrored_metadata_iso_opens_cleanly() {
        // The builder itself is sound: no fault, everything reads via the
        // primary mapping.
        let src = UdfSource::open(MemIso::boxed(mirrored_metadata_iso())).expect("open");
        assert_eq!(src.volume_label(), "VOLMIR");
        assert_eq!(file_names(&src.root()), vec!["FILE.BIN".to_owned()]);
    }

    #[test]
    fn mirror_recovers_each_unreadable_metadata_sector() {
        // One bad sector under each metadata structure read through the primary
        // mapping — the FSD (311), the root directory FE (312), its FID data
        // (313), and the child FE (314) — each recovered through the mirror's
        // copy.
        for sector in [311_u64, 312, 313, 314] {
            assert_opens_via_mirror(mirrored_metadata_iso(), sector);
        }
    }

    #[test]
    fn mirror_fe_recovers_an_unreadable_primary_metadata_fe() {
        // The primary metadata file FE itself (sector 302) is the bad sector →
        // the mirror FE's mapping serves the whole metadata partition.
        assert_opens_via_mirror(mirrored_metadata_iso(), 302);
    }

    #[test]
    fn mirror_fe_recovers_an_unparsable_primary_metadata_fe() {
        // The primary FE sector reads fine but holds garbage → same recovery.
        let mut bytes = mirrored_metadata_iso();
        put(&mut bytes, 302 * SS, &vec![0xFF_u8; SS]);
        let src = UdfSource::open(MemIso::boxed(bytes)).expect("open via mirror fe");
        assert_eq!(src.volume_label(), "VOLMIR");
        assert_eq!(file_names(&src.root()), vec!["FILE.BIN".to_owned()]);
    }

    #[test]
    fn unreadable_metadata_sector_without_a_mirror_fails() {
        // metadata_iso has no mirror content (its mirror FE sector is zeros):
        // a bad sector under the root FID data is unrecoverable → the open
        // fails instead of fabricating an empty tree.
        let bad = (313 * SS as u64)..(314 * SS as u64);
        assert!(UdfSource::open(FaultyIso::boxed(metadata_iso(), bad)).is_err());
    }

    #[test]
    fn identical_mirror_mapping_cannot_recover_and_fails() {
        // The mirror FE maps onto the SAME physical blocks as the primary —
        // the retry rereads the bad sector and the original failure surfaces.
        let mut bytes = mirrored_metadata_iso();
        // Rewrite the mirror FE to duplicate the primary mapping (blocks 10..16).
        put(&mut bytes, 303 * SS, &fe(261, 251, 0, 6 * SS as u64, &sad(0, 6 * SS as u32, 10)));
        let bad = (313 * SS as u64)..(314 * SS as u64);
        assert!(UdfSource::open(FaultyIso::boxed(bytes, bad)).is_err());
    }

    // ── Metadata File Entry validation (FileType + mapping) ──────────────────

    /// The starting blocks of an extent list (the `Extent::block` of each),
    /// the field these tests assert to tell the primary mapping (block 10) from
    /// the mirror mapping (block 40).
    fn extent_blocks(extents: &[Extent]) -> Vec<u32> {
        extents.iter().map(|e| e.block).collect()
    }

    /// Resolves a Metadata partition whose primary File Entry is `primary` (at
    /// physical block 2) and whose mirror File Entry is `mirror` (at physical
    /// block 3), returning the metadata loc's `(extents, mirror_extents)` — the
    /// mappings the open would actually use. The single physical partition
    /// starts at sector 0, so physical block N reads at sector N.
    fn resolved_metadata_loc(primary: &[u8], mirror: &[u8]) -> (Vec<Extent>, Vec<Extent>) {
        let lvd = Lvd {
            logical_volume_identifier: String::new(),
            logical_block_size: SS as u32,
            file_set_descriptor: LongAd {
                raw_length: 0,
                location: LbAddr { block: 0, partition: 0 },
            },
            partition_maps: vec![
                PartitionMap::Physical { partition_number: 0 },
                PartitionMap::Metadata(MetadataPartitionMap {
                    physical_partition: 0,
                    metadata_file_location: 2,
                    metadata_mirror_file_location: 3,
                }),
            ],
        };
        let pds =
            vec![PartitionDescriptor { partition_number: 0, starting_location: 0, length: 100 }];
        let mut image = vec![0_u8; 8 * SS];
        put(&mut image, 2 * SS, primary);
        put(&mut image, 3 * SS, mirror);
        let mut cursor = std::io::Cursor::new(image);
        let locs = resolve_partitions(&mut cursor, &lvd, &pds, SS as u64, Limits::DEFAULT)
            .expect("resolve");
        locs.into_iter()
            .find_map(|loc| match loc {
                PartitionLoc::Metadata { extents, mirror_extents, .. } => {
                    Some((extents, mirror_extents))
                }
                PartitionLoc::Physical { .. } => None,
            })
            .expect("a metadata partition")
    }

    #[test]
    fn metadata_primary_with_wrong_file_type_falls_back_to_mirror() {
        // The primary FE is a plain file (FileType 5), not a Metadata File
        // (250) — it is rejected and the mirror (251) is promoted to the
        // mapping, so the resolved extents are the mirror's (block 40).
        let primary = fe(261, 5, 0, SS as u64, &sad(0, SS as u32, 10));
        let mirror = fe(261, 251, 0, SS as u64, &sad(0, SS as u32, 40));
        let (extents, _) = resolved_metadata_loc(&primary, &mirror);
        assert_eq!(extent_blocks(&extents), vec![40]);
    }

    #[test]
    fn metadata_mirror_with_wrong_file_type_is_dropped() {
        // Symmetrically, a mirror FE that is not a Metadata Mirror File (251) is
        // not kept: the primary (250) stands alone with an empty mirror mapping.
        let primary = fe(261, 250, 0, SS as u64, &sad(0, SS as u32, 10));
        let mirror = fe(261, 5, 0, SS as u64, &sad(0, SS as u32, 40));
        let (extents, mirror_extents) = resolved_metadata_loc(&primary, &mirror);
        assert_eq!(extent_blocks(&extents), vec![10]);
        assert!(mirror_extents.is_empty(), "the wrong-type mirror is dropped");
    }

    #[test]
    fn metadata_primary_without_extents_falls_back_to_mirror() {
        // A FileType-250 primary that maps no blocks (L_AD = 0, an empty extent
        // list) is not a usable layout — the mirror takes over.
        let primary = fe(261, 250, 0, SS as u64, &[]);
        let mirror = fe(261, 251, 0, SS as u64, &sad(0, SS as u32, 40));
        let (extents, _) = resolved_metadata_loc(&primary, &mirror);
        assert_eq!(extent_blocks(&extents), vec![40]);
    }

    #[test]
    fn metadata_primary_embedded_falls_back_to_mirror() {
        // An embedded primary FE carries inline bytes, not allocation extents, so
        // it maps no partition blocks — the empty-extents guard rejects it and
        // the mirror serves the mapping instead.
        let primary = fe(261, 250, 3, 4, &[1, 2, 3, 4]);
        let mirror = fe(261, 251, 0, SS as u64, &sad(0, SS as u32, 40));
        let (extents, _) = resolved_metadata_loc(&primary, &mirror);
        assert_eq!(extent_blocks(&extents), vec![40]);
    }

    // ── PathIso (the CLI backend) ────────────────────────────────────────────

    #[test]
    fn path_iso_opens_a_real_file_and_parses() {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("bdinfo-rs-udf-{}-{unique}.iso", std::process::id()));
        std::fs::File::create(&path)
            .expect("create iso")
            .write_all(&physical_iso())
            .expect("write iso");
        let factory = PathIso::new(&path);
        // open() yields an independent File handle.
        assert!(factory.open().is_ok());
        let src = UdfSource::open(Box::new(factory)).expect("open path iso");
        assert_eq!(src.volume_label(), "VOLPHYS");
        let _ = std::fs::remove_file(&path).is_ok();
    }

    // ── Error paths in volume parsing ────────────────────────────────────────

    #[test]
    fn open_too_short_image_is_io_error() {
        // Smaller than sector 256 → reading the AVDP fails (an IO error, not the
        // StructureNotFound a malformed-but-readable image yields).
        let err = UdfSource::open(MemIso::boxed(vec![0_u8; 16])).expect_err("too short");
        assert!(err.to_string().starts_with("io error"));
    }

    #[test]
    fn open_without_descriptors_is_structure_not_found() {
        // Long enough to read sector 256, but it is all zeros (no AVDP tag).
        let err = UdfSource::open(MemIso::boxed(vec![0_u8; 258 * SS])).expect_err("no avdp");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn open_with_anchor_only_at_the_last_sector_succeeds() {
        // Sector 256 is zeroed; the only AVDP sits at the image's last sector
        // (N − 1) — the first end-of-image fallback anchor.
        let mut bytes = physical_iso(); // 300 sectors
        put(&mut bytes, 299 * SS, &avdp(257, 3 * SS as u32));
        for slot in bytes.iter_mut().skip(256 * SS).take(SS) {
            *slot = 0;
        }
        let src = UdfSource::open(MemIso::boxed(bytes)).expect("open via last-sector anchor");
        assert_eq!(src.volume_label(), "VOLPHYS");
        assert!(file_names(&src.root()).contains(&"data.bin".to_owned()));
    }

    #[test]
    fn open_with_anchor_only_at_last_minus_256_succeeds() {
        // The only AVDP sits 256 sectors before the last one (N − 257, i.e. the
        // spec's N − 256 anchor) — the second fallback candidate.
        let mut bytes = physical_iso(); // 300 sectors → candidate sector 43
        put(&mut bytes, 43 * SS, &avdp(257, 3 * SS as u32));
        for slot in bytes.iter_mut().skip(256 * SS).take(SS) {
            *slot = 0;
        }
        let src = UdfSource::open(MemIso::boxed(bytes)).expect("open via N-256 anchor");
        assert_eq!(src.volume_label(), "VOLPHYS");
    }

    #[test]
    fn locate_avdp_keeps_the_primary_error_when_the_length_is_unknown() {
        // The primary anchor read fails AND the stream length cannot be
        // determined (seek errors) → the primary failure is returned.
        let err = super::locate_avdp(&mut FailIo { fail_seek: true }).expect_err("no avdp");
        assert!(err.to_string().starts_with("io error"));
    }

    // ── The VDS walk: VDP chaining, terminator, reserve fallback, prevailing ─

    /// A minimal valid volume body for the VDS-walk tests: the partition at
    /// sector 260, the FSD at block 1 (sector 261), an embedded empty root
    /// directory at block 2 (sector 262). Callers lay down their own AVDP/VDS.
    fn write_minimal_partition_body(iso: &mut Iso) {
        iso.write(261, &fsd(0, 2));
        iso.write(262, &fe(261, 4, 3, 0, &[]));
    }

    /// An image whose main VDS is a single sector holding only a VDP; the
    /// LVD + PD live in the continuation extent it points at.
    fn vdp_iso() -> Vec<u8> {
        let mut iso = Iso::new(300);
        iso.write(256, &avdp(257, SS as u32));
        iso.write(257, &vdp(280, 2 * SS as u32));
        iso.write(280, &pd(0, 260, 30));
        iso.write(281, &lvd("VOLVDP", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        iso.into_bytes()
    }

    #[test]
    fn vds_continues_through_a_volume_descriptor_pointer() {
        let src = UdfSource::open(MemIso::boxed(vdp_iso())).expect("open via vdp");
        assert_eq!(src.volume_label(), "VOLVDP");
        assert!(file_names(&src.root()).is_empty());
    }

    #[test]
    fn self_referencing_vdp_chain_is_bounded() {
        // A VDP pointing back at its own sector: libudfread hangs on this; the
        // shared sector budget must end the walk (and the open must fail
        // cleanly — no descriptors were found).
        let mut iso = Iso::new(300);
        iso.write(256, &avdp(257, SS as u32));
        iso.write(257, &vdp(257, SS as u32));
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("no lvd");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn terminating_descriptor_stops_the_vds_walk() {
        // Descriptors after a Terminating Descriptor are dead sectors: the
        // equal-VDSN LVD past it must NOT displace the one before it.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp(257, 4 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &lvd("VOLTERM", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(259, &term());
        iso.write(260, &lvd("VOLDEAD", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert_eq!(src.volume_label(), "VOLTERM");
    }

    #[test]
    fn reserve_vds_recovers_an_unusable_main_vds() {
        // The main VDS extent points at blank (taggable but empty) sectors; the
        // reserve sequence carries the real LVD + PD.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp_full(40, 2 * SS as u32, 257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 30));
        iso.write(258, &lvd("VOLRES", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open via reserve");
        assert_eq!(src.volume_label(), "VOLRES");
    }

    #[test]
    fn reserve_vds_recovers_an_unreadable_main_vds() {
        // The main VDS extent points past the image (its read errors); the
        // reserve sequence still opens the volume.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp_full(9000, 2 * SS as u32, 257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 30));
        iso.write(258, &lvd("VOLRES2", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open via reserve");
        assert_eq!(src.volume_label(), "VOLRES2");
    }

    #[test]
    fn reserve_vds_recovers_a_main_vds_with_no_partition_descriptor() {
        // The main VDS carries an LVD but NO Partition Descriptor — unusable
        // (both halves of the usability test must hold); the reserve sequence
        // carries the complete set and must win.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp_full(257, SS as u32, 270, 2 * SS as u32));
        iso.write(257, &lvd("VOLMAIN", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(270, &pd(0, 260, 30));
        iso.write(271, &lvd("VOLRES3", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open via reserve");
        assert_eq!(src.volume_label(), "VOLRES3");
    }

    #[test]
    fn a_usable_main_vds_is_preferred_over_the_reserve() {
        // Both sequences are usable: the main one must be taken, not the
        // reserve (the reserve is a fallback, not a peer).
        let mut iso = Iso::new(300);
        iso.write(256, &avdp_full(257, 2 * SS as u32, 270, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 30));
        iso.write(258, &lvd("VOLMAIN", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(270, &pd(0, 260, 30));
        iso.write(271, &lvd("VOLSPARE", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert_eq!(src.volume_label(), "VOLMAIN");
    }

    #[test]
    fn unreadable_main_with_a_blank_reserve_propagates_the_main_error() {
        // The main walk errors; the reserve walks fine but yields nothing
        // usable → the main failure (not the empty reserve scan) surfaces.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp_full(9000, 2 * SS as u32, 40, 2 * SS as u32));
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("main io error");
        assert!(err.to_string().starts_with("io error"));
    }

    #[test]
    fn unreadable_main_and_reserve_vds_propagates_the_main_failure() {
        // Both sequences point past the image → the main walk's error surfaces.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp_full(9000, 2 * SS as u32, 9500, 2 * SS as u32));
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("both vds dead");
        assert!(err.to_string().starts_with("io error"));
    }

    #[test]
    fn highest_sequence_number_lvd_prevails() {
        // Two LVDs: the higher VolumeDescriptorSequenceNumber wins regardless
        // of read order (ECMA-167 §3/8.4.3) — not last-read-wins.
        for (first, second) in [(("VOLHI", 2), ("VOLLO", 1)), (("VOLLO", 1), ("VOLHI", 2))] {
            let mut iso = Iso::new(300);
            iso.write(256, &avdp(257, 3 * SS as u32));
            iso.write(257, &pd(0, 260, 50));
            let mk = |(label, n): (&str, u32)| {
                with_vdsn(lvd(label, SS as u32, 1, 0, &phys_map(0), 1), n)
            };
            iso.write(258, &mk(first));
            iso.write(259, &mk(second));
            write_minimal_partition_body(&mut iso);
            let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
            assert_eq!(src.volume_label(), "VOLHI");
        }
    }

    #[test]
    fn highest_sequence_number_partition_descriptor_prevails() {
        // Two PDs for partition number 0: only the higher-VDSN one (start 260,
        // where the FSD really lives) resolves the volume; if the stale one
        // (start 290 — blank sectors) prevailed, the open would fail.
        for (first, second) in [((260, 2), (290, 1)), ((290, 1), (260, 2))] {
            let mut iso = Iso::new(300);
            iso.write(256, &avdp(257, 3 * SS as u32));
            let mk = |(start, n): (u32, u32)| with_vdsn(pd(0, start, 50), n);
            iso.write(257, &mk(first));
            iso.write(258, &mk(second));
            iso.write(259, &lvd("VOLPD", SS as u32, 1, 0, &phys_map(0), 1));
            write_minimal_partition_body(&mut iso);
            let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
            assert_eq!(src.volume_label(), "VOLPD");
        }
    }

    #[test]
    fn empty_lvd_identifier_falls_back_to_the_pvd_label() {
        // The LVD's LogicalVolumeIdentifier is empty → the PVD's
        // VolumeIdentifier names the volume.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp(257, 3 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &pvd("PVDLBL"));
        iso.write(259, &lvd("", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert_eq!(src.volume_label(), "PVDLBL");
        assert_eq!(src.root().name(), "PVDLBL");
    }

    #[test]
    fn non_empty_lvd_identifier_wins_over_the_pvd() {
        // Both identifiers present → the LVD's stays primary.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp(257, 3 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &pvd("PVDLBL"));
        iso.write(259, &lvd("LVDLBL", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert_eq!(src.volume_label(), "LVDLBL");
    }

    #[test]
    fn empty_lvd_identifier_without_a_pvd_is_an_empty_label() {
        // Neither identifier usable → the label degrades to empty, the open
        // still succeeds.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &lvd("", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert_eq!(src.volume_label(), "");
    }

    #[test]
    fn highest_sequence_number_pvd_prevails() {
        // Two PVDs: the higher-VDSN identifier is the fallback label,
        // regardless of read order.
        for (first, second) in [(("PVDHI", 2), ("PVDLO", 1)), (("PVDLO", 1), ("PVDHI", 2))] {
            let mut iso = Iso::new(300);
            iso.write(256, &avdp(257, 4 * SS as u32));
            iso.write(257, &pd(0, 260, 50));
            let mk = |(label, n): (&str, u32)| with_vdsn(pvd(label), n);
            iso.write(258, &mk(first));
            iso.write(259, &mk(second));
            iso.write(260, &lvd("", SS as u32, 1, 0, &phys_map(0), 1));
            write_minimal_partition_body(&mut iso);
            let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
            assert_eq!(src.volume_label(), "PVDHI");
        }
    }

    #[test]
    fn equal_sequence_number_pvds_keep_the_later_one() {
        // A VDSN tie between two PVDs → the later-read identifier wins.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp(257, 4 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &pvd("PVDOLD"));
        iso.write(259, &pvd("PVDNEW"));
        iso.write(260, &lvd("", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert_eq!(src.volume_label(), "PVDNEW");
    }

    #[test]
    fn equal_sequence_numbers_keep_the_later_descriptor() {
        // On a VDSN tie the later-read descriptor wins (the universal
        // single-copy case): the second PD carries the real start and the
        // second LVD the reported label.
        let mut iso = Iso::new(300);
        iso.write(256, &avdp(257, 5 * SS as u32));
        iso.write(257, &pd(0, 290, 50)); // stale twin (blank sectors)
        iso.write(258, &pd(0, 260, 50)); // later tie-winner — the real start
        iso.write(259, &lvd("VOLOLD", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(260, &lvd("VOLNEW", SS as u32, 1, 0, &phys_map(0), 1));
        write_minimal_partition_body(&mut iso);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert_eq!(src.volume_label(), "VOLNEW");
    }

    #[test]
    fn open_without_lvd_is_structure_not_found() {
        let mut iso = Iso::new(260);
        iso.write(256, &avdp(257, SS as u32));
        iso.write(257, &pd(0, 1, 1)); // a PD but no LVD in the VDS
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("no lvd");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn open_with_zero_block_size_is_structure_not_found() {
        let mut iso = Iso::new(260);
        iso.write(256, &avdp(257, SS as u32));
        iso.write(257, &lvd("X", 0, 1, 0, &phys_map(0), 1));
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("bs 0");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn open_with_unmatched_partition_is_structure_not_found() {
        // The physical map names partition 7, but the only PD is #0 → no start.
        let mut iso = Iso::new(265);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 10));
        iso.write(258, &lvd("X", SS as u32, 1, 0, &phys_map(7), 1));
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("no match");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn open_with_bad_fsd_is_structure_not_found() {
        // Valid up to the FSD sector, which holds a wrong tag.
        let mut iso = Iso::new(265);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 10));
        iso.write(258, &lvd("X", SS as u32, 1, 0, &phys_map(0), 1));
        // sector 261 (FSD location) left as zeros → Fsd::parse fails.
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("bad fsd");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn open_with_bad_metadata_file_entry_is_structure_not_found() {
        let mut iso = Iso::new(330);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 300, 100));
        let mut maps = phys_map(0);
        maps.extend_from_slice(&meta_map(0, 2, 3));
        iso.write(258, &lvd("X", SS as u32, 1, 1, &maps, 2));
        // sector 302 (metadata file FE) left zero → FileEntry::parse fails.
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("bad meta fe");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn open_with_unresolvable_fsd_partition_is_structure_not_found() {
        // The FSD long_ad cites partition ref 5, which has no map entry.
        let mut iso = Iso::new(265);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 10));
        iso.write(258, &lvd("X", SS as u32, 1, 5, &phys_map(0), 1));
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("bad fsd ref");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    // ── Searching the FSD extent for the File Set Descriptor ──────────────────

    /// Builds a valid single-physical-partition `.iso` whose File Set Descriptor
    /// extent (block 1 of partition 0, `fsd_len` bytes) is left for the caller to
    /// populate — the harness for the FSD-extent search. Partition 0 starts at
    /// sector 260 (block N → sector 260 + N); the root directory (an embedded EFE
    /// dir holding one file `FILE.BIN`) sits at block 5, so a valid FSD must point
    /// its root ICB at `(part 0, block 5)`.
    fn fsd_search_iso(fsd_len: u32) -> Iso {
        let mut iso = Iso::new(280);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &lvd_with_fsd_len("VOLFSD", SS as u32, 1, 0, fsd_len, &phys_map(0), 1));
        let fids = dir_data(&[fid(0x0A, "", 5, 0), fid(0x00, "FILE.BIN", 6, 0)]);
        iso.write(265, &fe(266, 4, 3, fids.len() as u64, &fids));
        iso.write(266, &fe(261, 5, 3, 5, b"hello"));
        iso
    }

    #[test]
    fn fsd_search_finds_the_descriptor_past_the_first_block() {
        // An 8-block FSD extent (blocks 1..8) whose first two blocks are zero; the
        // FSD is at block 3 (sector 263) — the search skips past the empty blocks.
        let mut iso = fsd_search_iso(8 * SS as u32);
        iso.write(263, &fsd(0, 5));
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open via FSD search");
        assert_eq!(src.volume_label(), "VOLFSD");
        assert_eq!(file_names(&src.root()), vec!["FILE.BIN".to_owned()]);
    }

    #[test]
    fn fsd_search_reads_the_first_block_with_a_zero_length_extent() {
        // A malformed FSD long_ad whose length is 0 still reads the first block
        // (the fast path), where an authored disc records the FSD.
        let mut iso = fsd_search_iso(0);
        iso.write(261, &fsd(0, 5)); // block 1 → sector 261, the extent's first block
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open via fast path");
        assert_eq!(file_names(&src.root()), vec!["FILE.BIN".to_owned()]);
    }

    #[test]
    fn fsd_search_stops_at_a_terminating_descriptor() {
        // A Terminating Descriptor at block 1 (the extent's first block) ends the
        // sequence before the FSD planted at block 3 is ever reached.
        let mut iso = fsd_search_iso(8 * SS as u32);
        iso.write(261, &term());
        iso.write(263, &fsd(0, 5));
        let err = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect_err("terminated search");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn fsd_search_stops_at_the_cap() {
        // The FSD sits at block 3 (offset 2) of the extent. The cap bounds how many
        // blocks are scanned: a cap of 2 (blocks 1..2) never reaches it; a cap of 3
        // (blocks 1..3) does — pinning the boundary exactly at the cap.
        let mut iso = fsd_search_iso(8 * SS as u32);
        iso.write(263, &fsd(0, 5));
        let bytes = iso.into_bytes();
        let mut cursor = std::io::Cursor::new(bytes.clone());
        let capped = Limits { max_fsd_sectors: 2, ..Limits::DEFAULT };
        assert!(parse_volume(&mut cursor, capped).is_err());
        let mut cursor = std::io::Cursor::new(bytes);
        let reaching = Limits { max_fsd_sectors: 3, ..Limits::DEFAULT };
        let volume = parse_volume(&mut cursor, reaching).expect("fsd found within the cap");
        assert_eq!(volume.root_icb, (0, 5));
    }

    // ── Tree-walk caps + a malformed child ───────────────────────────────────

    #[test]
    fn max_nodes_cap_stops_the_walk() {
        let bytes = physical_iso();
        let mut cursor = std::io::Cursor::new(bytes);
        let volume = parse_volume(&mut cursor, Limits::DEFAULT).expect("volume");
        let tiny = Limits { max_nodes: 1, ..Limits::DEFAULT };
        let nodes = build_tree(&mut cursor, &volume, tiny).expect("tree");
        assert_eq!(nodes.len(), 1); // only the root — no children added
    }

    #[test]
    fn max_dir_bytes_cap_yields_no_children() {
        let bytes = physical_iso();
        let mut cursor = std::io::Cursor::new(bytes);
        let volume = parse_volume(&mut cursor, Limits::DEFAULT).expect("volume");
        let no_dir = Limits { max_dir_bytes: 0, ..Limits::DEFAULT };
        let nodes = build_tree(&mut cursor, &volume, no_dir).expect("tree");
        // The root's extent dir data is never read, so it has no children.
        assert_eq!(nodes.len(), 1);
    }

    /// Builds a single-physical-partition `.iso` whose root nests `depth`
    /// embedded-EFE directories (root = depth 0, the deepest directory at depth
    /// `depth`), each naming the next `D`; the deepest directory holds one embedded
    /// file `leaf.bin` (a depth-`depth + 1` leaf). Drives the [`Limits::max_depth`]
    /// cap with a controllable chain length. Block `2 + k` → sector `262 + k` holds
    /// the depth-`k` directory.
    fn deep_chain_iso(depth: usize) -> Vec<u8> {
        let mut iso = Iso::new(270 + depth);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, (depth + 16) as u32));
        iso.write(258, &lvd("DEEP", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(261, &fsd(0, 2)); // root ICB part 0 block 2 → sector 262
        for k in 0..=depth {
            let own_block = 2 + k;
            let child_block = own_block + 1;
            let child = if k < depth {
                fid(0x02, "D", child_block as u32, 0) // subdirectory → next level
            } else {
                fid(0x00, "leaf.bin", child_block as u32, 0) // a leaf file at the bottom
            };
            let fids = dir_data(&[fid(0x0A, "", own_block as u32, 0), child]);
            iso.write(260 + own_block, &fe(266, 4, 3, fids.len() as u64, &fids));
        }
        // The deepest directory's embedded file (one byte) at block `3 + depth`.
        iso.write(263 + depth, &fe(261, 5, 3, 1, b"x"));
        iso.into_bytes()
    }

    /// Count of the directory nodes in an arena.
    fn dir_node_count(nodes: &[Node]) -> usize {
        nodes.iter().filter(|n| matches!(n.kind, NodeKind::Dir)).count()
    }

    #[test]
    fn max_depth_cap_keeps_a_chain_exactly_at_the_cap() {
        // A chain whose deepest directory sits at depth == max_depth is fully kept:
        // that directory is the deepest one the walk expands, and its embedded file
        // (depth max_depth + 1, a leaf) is retained.
        const DEPTH: usize = 4;
        let bytes = deep_chain_iso(DEPTH);
        let mut cursor = std::io::Cursor::new(bytes);
        let volume = parse_volume(&mut cursor, Limits::DEFAULT).expect("volume");
        let at_cap = Limits { max_depth: DEPTH, ..Limits::DEFAULT };
        let nodes = build_tree(&mut cursor, &volume, at_cap).expect("tree");
        assert_eq!(dir_node_count(&nodes), DEPTH + 1); // root + DEPTH nested dirs
        // The directory at depth == max_depth is present (only it has this path —
        // files are named `leaf.bin`), and its leaf file (depth max_depth + 1) is
        // kept: a file under it proves the deepest directory was expanded as a dir.
        let deepest = "/D".repeat(DEPTH);
        assert!(
            nodes.iter().any(|n| n.full_name == deepest),
            "the directory at depth == max_depth must be present"
        );
        assert!(
            nodes.iter().any(|n| n.full_name == format!("{deepest}/leaf.bin")),
            "the leaf file under the deepest expanded directory must be kept"
        );
    }

    #[test]
    fn max_depth_cap_drops_subdirectories_past_the_cap() {
        // A chain one level deeper than max_depth: the directory at depth
        // max_depth + 1 is dropped (silently truncated, no panic), so the arena
        // holds only the directories at depths 0..=max_depth.
        const DEPTH: usize = 5; // deepest directory at depth 5
        let cap = DEPTH - 1; // 4
        let bytes = deep_chain_iso(DEPTH);
        let mut cursor = std::io::Cursor::new(bytes);
        let volume = parse_volume(&mut cursor, Limits::DEFAULT).expect("volume");
        let limited = Limits { max_depth: cap, ..Limits::DEFAULT };
        let nodes = build_tree(&mut cursor, &volume, limited).expect("tree");
        assert_eq!(dir_node_count(&nodes), cap + 1); // root + `cap` nested dirs
        let dropped = "/D".repeat(cap + 1); // depth cap + 1 == DEPTH
        assert!(
            !nodes.iter().any(|n| n.full_name == dropped),
            "the subdirectory just past the cap must be truncated"
        );
        // Its file is gone too — its parent directory was never added.
        assert!(!nodes.iter().any(|n| n.full_name.ends_with("/leaf.bin")));
    }

    #[test]
    fn a_deep_hostile_chain_yields_a_bounded_arena_without_panicking() {
        // A directory chain far deeper than a tiny max_depth: the walk truncates it
        // to the cap, so the arena stays small and `build_tree` returns normally —
        // the recursion-bounding contract that keeps `collect_from`/`directory_size`
        // from overflowing the stack on a ~1M-deep hostile `.iso`.
        const DEPTH: usize = 64;
        let cap = 3;
        let bytes = deep_chain_iso(DEPTH);
        let mut cursor = std::io::Cursor::new(bytes);
        let volume = parse_volume(&mut cursor, Limits::DEFAULT).expect("volume");
        let limited = Limits { max_depth: cap, ..Limits::DEFAULT };
        let nodes = build_tree(&mut cursor, &volume, limited).expect("tree");
        // root + `cap` directories, the remaining 61 levels truncated; no files
        // survive (the only leaf sits far below the cap).
        assert_eq!(dir_node_count(&nodes), cap + 1);
        assert_eq!(nodes.len(), cap + 1);
    }

    #[test]
    fn deep_chain_recursive_glob_walks_the_whole_tree_under_the_default_cap() {
        // End-to-end: a nested chain opened through `UdfSource::open` (default cap
        // 1024 ≫ this depth) lists the bottom `leaf.bin` via the recursive glob,
        // exercising the arena consumer over a genuinely chained tree.
        let src = UdfSource::open(MemIso::boxed(deep_chain_iso(6))).expect("open chain");
        let hits = src
            .root()
            .get_files_pattern_option("leaf.bin", SearchOption::AllDirectories)
            .expect("recursive glob");
        assert_eq!(hits.iter().map(|f| f.name().to_owned()).collect::<Vec<_>>(), vec!["leaf.bin"]);
    }

    #[test]
    fn directory_with_unreadable_child_skips_it() {
        // Root names a child whose FE sector is out of the image → child skipped,
        // the rest still enumerate.
        let mut iso = Iso::new(280);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &lvd("X", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(261, &fsd(0, 2));
        let fids = dir_data(&[
            fid(0x0A, "", 2, 0),
            fid(0x00, "ok.txt", 4, 0),
            fid(0x00, "bad.txt", 9000, 0), // FE sector far out of range → read fails
            fid(0x00, "off.txt", 0, 9),    // partition ref 9 → resolve fails
        ]);
        iso.write(262, &fe(261, 4, 0, fids.len() as u64, &sad(0, fids.len() as u32, 3)));
        iso.write(263, &fids);
        iso.write(264, &fe(261, 5, 3, 2, b"ok")); // ok.txt FE at block 4 → sector 264
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert_eq!(file_names(&src.root()), vec!["ok.txt".to_owned()]);
    }

    #[test]
    fn child_with_a_non_direct_icb_strategy_is_skipped() {
        // A child whose File Entry records ICB strategy 4096 (not the flat
        // strategy 4) is rejected and skipped; its strategy-4 sibling still
        // enumerates.
        let mut iso = Iso::new(280);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &lvd("X", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(261, &fsd(0, 2));
        let fids = dir_data(&[
            fid(0x0A, "", 2, 0),
            fid(0x00, "ok.txt", 4, 0),
            fid(0x00, "chained.txt", 5, 0), // strategy-4096 FE → skipped
        ]);
        iso.write(262, &fe(261, 4, 0, fids.len() as u64, &sad(0, fids.len() as u32, 3)));
        iso.write(263, &fids);
        iso.write(264, &fe(261, 5, 3, 2, b"ok"));
        let mut chained = fe(261, 5, 3, 2, b"no");
        put(&mut chained, 20, &4096_u16.to_le_bytes()); // outside the tag checksum
        iso.write(265, &chained);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert_eq!(file_names(&src.root()), vec!["ok.txt".to_owned()]);
    }

    #[test]
    fn root_with_a_non_direct_icb_strategy_opens_empty() {
        // The ROOT directory's FE records strategy 4096 → the rejection
        // degrades to a childless root, not a failed open.
        let mut iso = Iso::new(280);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &lvd("X", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(261, &fsd(0, 2));
        let mut root = fe(261, 4, 3, 0, &[]);
        put(&mut root, 20, &4096_u16.to_le_bytes());
        iso.write(262, &root);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert!(file_names(&src.root()).is_empty());
        assert!(dir_names(&src.root()).is_empty());
    }

    #[test]
    fn child_with_unparsable_entry_is_skipped() {
        // The child FE sector is in range but holds an invalid tag.
        let mut iso = Iso::new(280);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &lvd("X", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(261, &fsd(0, 2));
        let fids = dir_data(&[fid(0x0A, "", 2, 0), fid(0x00, "junk", 4, 0)]);
        iso.write(262, &fe(261, 4, 0, fids.len() as u64, &sad(0, fids.len() as u32, 3)));
        iso.write(263, &fids);
        // sector 264 (child FE at block 4) is left as 0xFF → bad checksum.
        iso.write(264, &vec![0xFF_u8; SS]);
        let src = UdfSource::open(MemIso::boxed(iso.into_bytes())).expect("open");
        assert!(file_names(&src.root()).is_empty());
    }

    // ── White-box guards (defensive arms unreachable from a well-formed disc) ─

    #[test]
    fn udf_dir_on_a_non_directory_node_errors() {
        let src = open_physical();
        // A UdfDir pointing at a missing node index → children() errors.
        let bogus = UdfDir {
            inner: Arc::clone(&src.inner),
            node: 999_999,
            name: String::new(),
            full_name: String::new(),
            parent: None,
        };
        assert!(bogus.get_files().is_err());
        assert!(bogus.get_directories().is_err());
    }

    // ── Pure helpers ─────────────────────────────────────────────────────────

    #[test]
    fn extension_of_handles_dotted_and_bare_names() {
        assert_eq!(extension_of("00000.MPLS"), ".MPLS");
        assert_eq!(extension_of("a.b.ssif"), ".ssif");
        assert_eq!(extension_of("README"), "");
        assert_eq!(extension_of("trailing."), ".");
    }

    #[test]
    fn offset_by_clamps_and_overflows() {
        assert_eq!(offset_by(10, 5), Some(15));
        assert_eq!(offset_by(10, -4), Some(6));
        assert_eq!(offset_by(3, -4), None); // negative result
        assert_eq!(offset_by(u64::MAX, 1), None); // add overflow
        assert_eq!(offset_by(10, i64::MIN), None); // checked_neg overflow
    }

    #[test]
    fn metadata_sector_walks_extents() {
        let extents = vec![
            Extent {
                partition_ref: None,
                block: 10,
                length: 2 * SS as u32,
                kind: ExtentKind::RecordedAllocated,
            },
            Extent {
                partition_ref: None,
                block: 50,
                length: 2 * SS as u32,
                kind: ExtentKind::RecordedAllocated,
            },
        ];
        // phys_start 100: metadata block 0,1 → 110,111; block 2,3 → 150,151.
        assert_eq!(metadata_sector(100, &extents, 0, SS as u64), Some(110));
        assert_eq!(metadata_sector(100, &extents, 1, SS as u64), Some(111));
        assert_eq!(metadata_sector(100, &extents, 2, SS as u64), Some(150));
        assert_eq!(metadata_sector(100, &extents, 3, SS as u64), Some(151));
        // Past the last extent → None.
        assert_eq!(metadata_sector(100, &extents, 4, SS as u64), None);
    }

    #[test]
    fn metadata_runs_coalesces_and_splits() {
        // Contiguous mapping → one coalesced run over the whole length.
        let contiguous = vec![Extent {
            partition_ref: None,
            block: 10,
            length: 4 * SS as u32,
            kind: ExtentKind::RecordedAllocated,
        }];
        let runs = metadata_runs(100, &contiguous, 0, 2 * SS as u64, SS as u64).expect("runs");
        assert_eq!(runs, vec![Run { src: Some(110 * SS as u64), len: 2 * SS as u64 }]);

        // Split mapping → two runs at the discontinuity, last block partial.
        let split = vec![
            Extent {
                partition_ref: None,
                block: 10,
                length: SS as u32,
                kind: ExtentKind::RecordedAllocated,
            },
            Extent {
                partition_ref: None,
                block: 80,
                length: SS as u32,
                kind: ExtentKind::RecordedAllocated,
            },
        ];
        let runs = metadata_runs(100, &split, 0, SS as u64 + 50, SS as u64).expect("runs");
        assert_eq!(
            runs,
            vec![
                Run { src: Some(110 * SS as u64), len: SS as u64 },
                Run { src: Some(180 * SS as u64), len: 50 },
            ]
        );

        // An unresolvable block → None.
        assert_eq!(metadata_runs(100, &contiguous, 0, 100 * SS as u64, SS as u64), None);
    }

    #[test]
    fn extent_runs_covers_each_kind() {
        let phys = vec![PartitionLoc::Physical { start: 100 }];
        // Recorded physical extent → one run.
        let rec = Extent {
            partition_ref: None,
            block: 5,
            length: 2048,
            kind: ExtentKind::RecordedAllocated,
        };
        assert_eq!(
            extent_runs(&phys, 0, &rec, SS as u64),
            Some(vec![Run { src: Some(105 * SS as u64), len: 2048 }])
        );
        // Zero-length extent → empty.
        let zero = Extent {
            partition_ref: None,
            block: 5,
            length: 0,
            kind: ExtentKind::RecordedAllocated,
        };
        assert_eq!(extent_runs(&phys, 0, &zero, SS as u64), Some(Vec::new()));
        // Both not-recorded kinds → one sparse run.
        let sparse = Extent {
            partition_ref: None,
            block: 5,
            length: 32,
            kind: ExtentKind::NotRecordedAllocated,
        };
        assert_eq!(
            extent_runs(&phys, 0, &sparse, SS as u64),
            Some(vec![Run { src: None, len: 32 }])
        );
        let unalloc = Extent {
            partition_ref: None,
            block: 5,
            length: 8,
            kind: ExtentKind::NotRecordedNotAllocated,
        };
        assert_eq!(
            extent_runs(&phys, 0, &unalloc, SS as u64),
            Some(vec![Run { src: None, len: 8 }])
        );
        // A NextExtent continuation pointer resolves to real recorded bytes.
        let next =
            Extent { partition_ref: None, block: 5, length: 16, kind: ExtentKind::NextExtent };
        assert_eq!(
            extent_runs(&phys, 0, &next, SS as u64),
            Some(vec![Run { src: Some(105 * SS as u64), len: 16 }])
        );
        // long_ad partition ref out of range → None.
        let oob = Extent {
            partition_ref: Some(9),
            block: 1,
            length: 16,
            kind: ExtentKind::RecordedAllocated,
        };
        assert_eq!(extent_runs(&phys, 0, &oob, SS as u64), None);
        // A Metadata partition extent routes through metadata_runs.
        let meta = vec![PartitionLoc::Metadata {
            phys_start: 200,
            extents: vec![Extent {
                partition_ref: None,
                block: 0,
                length: 4 * SS as u32,
                kind: ExtentKind::RecordedAllocated,
            }],
            mirror_extents: Vec::new(),
        }];
        let mrec = Extent {
            partition_ref: None,
            block: 1,
            length: 2048,
            kind: ExtentKind::RecordedAllocated,
        };
        assert_eq!(
            extent_runs(&meta, 0, &mrec, SS as u64),
            Some(vec![Run { src: Some(201 * SS as u64), len: 2048 }])
        );
    }

    #[test]
    fn resolve_sector_covers_physical_metadata_and_oob() {
        let locs = vec![
            PartitionLoc::Physical { start: 100 },
            PartitionLoc::Metadata {
                phys_start: 200,
                extents: vec![Extent {
                    partition_ref: None,
                    block: 0,
                    length: 4 * SS as u32,
                    kind: ExtentKind::RecordedAllocated,
                }],
                mirror_extents: Vec::new(),
            },
        ];
        assert_eq!(resolve_sector(&locs, 0, 5, SS as u64), Some(105));
        assert_eq!(resolve_sector(&locs, 1, 2, SS as u64), Some(202));
        assert_eq!(resolve_sector(&locs, 9, 0, SS as u64), None);
        // The mirror resolver: no alternative for physical, none for an
        // empty mirror mapping, none for an out-of-range reference.
        assert_eq!(super::resolve_mirror_sector(&locs, 0, 5, SS as u64), None);
        assert_eq!(super::resolve_mirror_sector(&locs, 1, 2, SS as u64), None);
        assert_eq!(super::resolve_mirror_sector(&locs, 9, 0, SS as u64), None);
    }

    #[test]
    fn read_runs_reads_data_zeros_and_caps() {
        let mut cursor = std::io::Cursor::new(b"ABCDEFGH".to_vec());
        let runs = vec![
            Run { src: Some(0), len: 4 },
            Run { src: None, len: 2 },
            Run { src: Some(4), len: 4 },
        ];
        // Cap below the total: 4 data + 2 zero + 2 data = 8 bytes.
        let out = read_runs(&mut cursor, &runs, 8).expect("read");
        assert_eq!(out, b"ABCD\0\0EF");
        // Zero cap → nothing.
        assert!(read_runs(&mut cursor, &runs, 0).expect("read").is_empty());
    }

    #[test]
    fn udf_file_reader_reads_across_runs_and_seeks() {
        let cursor = std::io::Cursor::new(b"ABCDEFGHIJ".to_vec());
        let mut reader = raw_reader(
            Box::new(cursor),
            vec![
                Run { src: Some(0), len: 3 }, // ABC
                Run { src: None, len: 2 },    // \0\0
                Run { src: Some(5), len: 3 }, // FGH
            ],
            8,
            0,
        );
        let mut all = Vec::new();
        reader.read_to_end(&mut all).expect("read");
        assert_eq!(all, b"ABC\0\0FGH");

        // Seek variants.
        assert_eq!(reader.seek(SeekFrom::Start(1)).expect("start"), 1);
        let mut one = [0_u8; 2];
        reader.read_exact(&mut one).expect("read");
        assert_eq!(&one, b"BC");
        assert_eq!(reader.seek(SeekFrom::End(-3)).expect("end"), 5);
        assert_eq!(reader.seek(SeekFrom::Current(-2)).expect("cur"), 3);
        // Past EOF reads nothing.
        assert_eq!(reader.seek(SeekFrom::Start(100)).expect("far"), 100);
        let mut none = [0_u8; 4];
        assert_eq!(reader.read(&mut none).expect("eof"), 0);
        // An invalid (negative) seek errors.
        assert!(reader.seek(SeekFrom::Start(0)).is_ok());
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
        // An empty buffer reads zero.
        assert_eq!(reader.read(&mut []).expect("empty"), 0);
    }

    #[test]
    fn udf_file_reader_truncated_runs_report_eof() {
        // length exceeds the runs' total coverage → reads stop at EOF.
        let cursor = std::io::Cursor::new(b"AB".to_vec());
        // pos 5: past the only run.
        let mut reader = raw_reader(Box::new(cursor), vec![Run { src: Some(0), len: 2 }], 10, 5);
        let mut buf = [0_u8; 4];
        assert_eq!(reader.read(&mut buf).expect("eof"), 0);
    }

    // ── NextExtent continuation following ────────────────────────────────────

    #[test]
    fn collect_extents_follows_a_next_extent_continuation() {
        // FE alloc area: a real short extent, then a NextExtent pointing to an
        // AED continuation block holding one more real extent + a terminator.
        let mut ads = sad(0, 4096, 77); // real extent in the continuation
        ads.extend_from_slice(&sad(0, 0, 0)); // terminator
        let cont = aed(&ads);
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, 2 * SS, &cont); // continuation at block 2 → sector 2
        let mut cursor = std::io::Cursor::new(image);

        let mut ad = sad(0, 2048, 40); // real extent
        ad.extend_from_slice(&sad(3, SS as u32, 2)); // NextExtent → block 2 (one block)
        let entry_buf = fe(261, 5, 0, 6144, &ad);
        let entry = FileEntry::parse(&entry_buf).expect("fe");

        let locs = vec![PartitionLoc::Physical { start: 0 }];
        let extents = collect_extents(&mut cursor, &locs, &entry, 0, SS as u64, Limits::DEFAULT)
            .expect("extents");
        // The real extent from the FE, then the one from the continuation.
        assert_eq!(extents.len(), 2);
        assert_eq!(extents.first().map(|e| e.block), Some(40));
        assert_eq!(extents.get(1).map(|e| e.block), Some(77));
    }

    #[test]
    fn collect_extents_skips_a_continuation_without_an_aed_header() {
        // The continuation block holds bare allocation descriptors with no AED
        // header (the layout a less strict parser would misread): its bytes
        // fail the tag checksum, so the continuation contributes nothing — and the FE's
        // remaining queued descriptors are still processed (skip, not stop).
        let mut cont = sad(0, 4096, 77);
        cont.extend_from_slice(&sad(0, 0, 0));
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, 2 * SS, &cont); // no AED header
        let mut cursor = std::io::Cursor::new(image);

        let mut ad = sad(3, SS as u32, 2); // NextExtent → the headerless block
        ad.extend_from_slice(&sad(0, 2048, 40)); // a real extent queued after it
        let entry = FileEntry::parse(&fe(261, 5, 0, 2048, &ad)).expect("fe");

        let locs = vec![PartitionLoc::Physical { start: 0 }];
        let extents = collect_extents(&mut cursor, &locs, &entry, 0, SS as u64, Limits::DEFAULT)
            .expect("extents");
        assert_eq!(extents.iter().map(|e| e.block).collect::<Vec<_>>(), vec![40]);
    }

    #[test]
    fn collect_extents_skips_a_continuation_with_a_wrong_tag_id() {
        // The continuation block carries a checksum-valid tag whose id is a File
        // Entry (261), not an AED (258) — rejected by the id check alone.
        let mut not_aed = aed(&sad(0, 4096, 77));
        put(&mut not_aed, 0, &261_u16.to_le_bytes());
        fix_tag(&mut not_aed, 0);
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, 2 * SS, &not_aed);
        let mut cursor = std::io::Cursor::new(image);

        let entry = FileEntry::parse(&fe(261, 5, 0, 2048, &sad(3, SS as u32, 2))).expect("fe");
        let locs = vec![PartitionLoc::Physical { start: 0 }];
        let extents = collect_extents(&mut cursor, &locs, &entry, 0, SS as u64, Limits::DEFAULT)
            .expect("extents");
        assert!(extents.is_empty());
    }

    #[test]
    fn collect_extents_honors_the_aed_l_ad_bound() {
        // The AED declares L_AD = one short_ad; a second non-zero descriptor
        // sits in the block right after the declared area — slack bytes, not a
        // descriptor, so only the declared extent is collected.
        let mut cont = aed(&sad(0, 4096, 77));
        put(&mut cont, 32, &sad(0, 2048, 99)); // past L_AD: must be ignored
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, 2 * SS, &cont);
        let mut cursor = std::io::Cursor::new(image);

        let entry = FileEntry::parse(&fe(261, 5, 0, 4096, &sad(3, SS as u32, 2))).expect("fe");
        let locs = vec![PartitionLoc::Physical { start: 0 }];
        let extents = collect_extents(&mut cursor, &locs, &entry, 0, SS as u64, Limits::DEFAULT)
            .expect("extents");
        assert_eq!(extents.iter().map(|e| e.block).collect::<Vec<_>>(), vec![77]);
    }

    #[test]
    fn aed_allocation_area_clamps_and_rejects_short_blocks() {
        use super::aed_allocation_area;
        // A full AED yields exactly its declared descriptor bytes.
        let ads = sad(0, 4096, 77);
        assert_eq!(aed_allocation_area(&aed(&ads)), Some(ads.as_slice()));
        // L_AD past the block end is clamped to the available bytes.
        let mut overlong = aed(&ads);
        put(&mut overlong, 20, &u32::MAX.to_le_bytes());
        fix_tag(&mut overlong, 0);
        assert_eq!(aed_allocation_area(&overlong), Some(ads.as_slice()));
        // 16..24 bytes: the tag parses but the L_AD read fails.
        let header_only = aed(&[]);
        assert_eq!(aed_allocation_area(header_only.get(..18).unwrap_or_default()), None);
        // Under 16 bytes: the tag itself fails.
        assert_eq!(aed_allocation_area(header_only.get(..8).unwrap_or_default()), None);
    }

    #[test]
    fn collect_extents_bounds_a_self_referential_continuation() {
        // An AED continuation block whose only descriptor is a NextExtent back
        // to itself — the follow count must bound the loop.
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, 2 * SS, &aed(&sad(3, SS as u32, 2))); // self-referential NextExtent
        let mut cursor = std::io::Cursor::new(image);

        let ad = sad(3, SS as u32, 2); // FE points straight at the self-loop
        let entry_buf = fe(261, 5, 0, 0, &ad);
        let entry = FileEntry::parse(&entry_buf).expect("fe");

        let locs = vec![PartitionLoc::Physical { start: 0 }];
        let bounded = Limits { max_continuations: 3, ..Limits::DEFAULT };
        let extents =
            collect_extents(&mut cursor, &locs, &entry, 0, SS as u64, bounded).expect("extents");
        assert!(extents.is_empty()); // no real data extents, loop bounded
    }

    #[test]
    fn collect_extents_follows_exactly_max_continuations() {
        // A chain FE → A → B → C, each continuation holding one real extent then a
        // pointer to the next. With max_continuations = 2, exactly two continuations
        // (A, B) are followed — C is not — proving the cap is `>` (not `>=`/`==`).
        let mut image = vec![0_u8; 8 * SS];
        let mut block_a = sad(0, 2048, 50); // continuation A: real extent block 50
        block_a.extend_from_slice(&sad(3, SS as u32, 3)); // → continuation B at block 3
        put(&mut image, 2 * SS, &aed(&block_a));
        let mut block_b = sad(0, 2048, 51); // continuation B: real extent block 51
        block_b.extend_from_slice(&sad(3, SS as u32, 4)); // → continuation C at block 4
        put(&mut image, 3 * SS, &aed(&block_b));
        let mut block_c = sad(0, 2048, 52); // continuation C: real extent block 52
        block_c.extend_from_slice(&sad(0, 0, 0)); // terminator
        put(&mut image, 4 * SS, &aed(&block_c));
        let mut cursor = std::io::Cursor::new(image);

        let mut ad = sad(0, 2048, 40); // the FE's own real extent (block 40)
        ad.extend_from_slice(&sad(3, SS as u32, 2)); // → continuation A at block 2
        let entry = FileEntry::parse(&fe(261, 5, 0, 9000, &ad)).expect("fe");

        let locs = vec![PartitionLoc::Physical { start: 0 }];
        let two = Limits { max_continuations: 2, ..Limits::DEFAULT };
        let extents =
            collect_extents(&mut cursor, &locs, &entry, 0, SS as u64, two).expect("extents");
        // FE extent + the two followed continuations (A, B); C's extent is dropped.
        assert_eq!(extents.iter().map(|e| e.block).collect::<Vec<_>>(), vec![40, 50, 51]);
    }

    /// A `Volume` over a single physical partition starting at sector 0 (so block
    /// N → sector N), for direct-calling the private walk helpers.
    fn flat_volume() -> Volume {
        Volume {
            block_size: SS as u64,
            locs: vec![PartitionLoc::Physical { start: 0 }],
            label: String::new(),
            root_icb: (0, 0),
        }
    }

    #[test]
    fn expand_directory_guards_reject_bad_directories() {
        let volume = flat_volume();
        let mut image = vec![0_u8; 8 * SS];
        put(&mut image, 0, &fe(261, 5, 3, 4, b"file")); // block 0: a file, not a dir
        put(&mut image, SS, &vec![0xFF_u8; SS]); // block 1: unparsable FE
        let mut cursor = std::io::Cursor::new(image);
        // Unresolvable partition ref → empty.
        assert!(
            expand_directory(&mut cursor, &volume, "", 9, 0, Limits::DEFAULT)
                .expect("ok")
                .is_empty()
        );
        // FE parses but is a file, not a directory → empty.
        assert!(
            expand_directory(&mut cursor, &volume, "", 0, 0, Limits::DEFAULT)
                .expect("ok")
                .is_empty()
        );
        // FE does not parse → empty.
        assert!(
            expand_directory(&mut cursor, &volume, "", 0, 1, Limits::DEFAULT)
                .expect("ok")
                .is_empty()
        );
    }

    #[test]
    fn directory_bytes_propagates_a_bad_continuation() {
        let volume = flat_volume();
        // Extent-based dir FE whose only descriptor is a NextExtent → out-of-range
        // block: following it fails the read, which propagates.
        let buf = fe(261, 4, 0, 100, &sad(3, 16, 9000));
        let entry = FileEntry::parse(&buf).expect("fe");
        let mut cursor = std::io::Cursor::new(vec![0_u8; 4 * SS]);
        assert!(directory_bytes(&mut cursor, &volume, &entry, 0, Limits::DEFAULT).is_err());
    }

    #[test]
    fn file_body_propagates_a_bad_continuation() {
        let volume = flat_volume();
        let buf = fe(261, 5, 0, 100, &sad(3, 16, 9000));
        let entry = FileEntry::parse(&buf).expect("fe");
        let mut cursor = std::io::Cursor::new(vec![0_u8; 4 * SS]);
        assert!(file_body(&mut cursor, &volume, &entry, 0, Limits::DEFAULT).is_err());
    }

    /// A reader whose seek (or read) always fails — to exercise the IO `?` error
    /// arms that an in-memory `Cursor` never triggers.
    #[derive(Debug)]
    struct FailIo {
        /// `true` → `seek` errors (and `read` is a no-op); `false` → `read` errors.
        fail_seek: bool,
    }

    impl Read for FailIo {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("read failed"))
        }
    }

    impl Seek for FailIo {
        fn seek(&mut self, _from: SeekFrom) -> std::io::Result<u64> {
            if self.fail_seek { Err(std::io::Error::other("seek failed")) } else { Ok(0) }
        }
    }

    #[test]
    fn read_sector_io_error_arms() {
        use super::read_sector_io;
        let mut cursor = std::io::Cursor::new(vec![0_u8; SS]);
        // sector * block_size overflows u64 → the overflow closure errors.
        assert!(read_sector_io(&mut cursor, u64::MAX, 2048).is_err());
        // seek failure propagates.
        assert!(read_sector_io(&mut FailIo { fail_seek: true }, 0, 2048).is_err());
        // read failure propagates.
        assert!(read_sector_io(&mut FailIo { fail_seek: false }, 0, 2048).is_err());
    }

    #[test]
    fn read_runs_propagates_a_seek_failure() {
        let runs = [Run { src: Some(0), len: 4 }];
        assert!(read_runs(&mut FailIo { fail_seek: true }, &runs, 4).is_err());
    }

    #[test]
    fn udf_file_reader_error_arms() {
        // Physical byte offset overflows u64 → the overflow closure errors.
        let mut overflow = raw_reader(
            Box::new(std::io::Cursor::new(vec![0_u8; 8])),
            vec![Run { src: Some(u64::MAX), len: 10 }],
            10,
            1,
        );
        assert!(overflow.read(&mut [0_u8; 4]).is_err());

        // Seek failure propagates.
        let mut seek_fail = raw_reader(
            Box::new(FailIo { fail_seek: true }),
            vec![Run { src: Some(0), len: 4 }],
            4,
            0,
        );
        assert!(seek_fail.read(&mut [0_u8; 4]).is_err());

        // Read failure (after a successful seek) propagates.
        let mut read_fail = raw_reader(
            Box::new(FailIo { fail_seek: false }),
            vec![Run { src: Some(0), len: 4 }],
            4,
            0,
        );
        assert!(read_fail.read(&mut [0_u8; 4]).is_err());

        // An empty buffer short-circuits *before* any seek — proven on a
        // failing-seek stream (where reaching the seek would error).
        let mut empty_buf = raw_reader(
            Box::new(FailIo { fail_seek: true }),
            vec![Run { src: Some(0), len: 4 }],
            4,
            0,
        );
        assert_eq!(empty_buf.read(&mut []).expect("empty buffer short-circuits"), 0);
    }

    #[test]
    fn read_extent_bytes_rejects_an_unresolvable_extent() {
        use super::read_extent_bytes;
        let locs = [PartitionLoc::Physical { start: 0 }];
        // A long_ad extent citing partition ref 9 (no such partition) → None → err.
        let extent = Extent {
            partition_ref: Some(9),
            block: 0,
            length: 16,
            kind: ExtentKind::RecordedAllocated,
        };
        let mut cursor = std::io::Cursor::new(vec![0_u8; SS]);
        assert!(read_extent_bytes(&mut cursor, &locs, 0, &extent, SS as u64, 16).is_err());
    }

    #[test]
    fn metadata_sector_rejects_a_zero_block_size() {
        let extents = [Extent {
            partition_ref: None,
            block: 1,
            length: 2048,
            kind: ExtentKind::RecordedAllocated,
        }];
        assert_eq!(metadata_sector(0, &extents, 0, 0), None);
    }

    /// A factory that succeeds for the first `remaining` opens then fails — so a
    /// source constructs (one open) but a later `open_read` errors.
    #[derive(Debug)]
    struct OpenOnce {
        remaining: AtomicU32,
        bytes: Arc<[u8]>,
    }

    impl IsoReader for OpenOnce {
        fn open(&self) -> std::io::Result<Box<dyn super::ReadSeek>> {
            if self.remaining.load(Ordering::Relaxed) == 0 {
                return Err(std::io::Error::other("no more opens"));
            }
            self.remaining.fetch_sub(1, Ordering::Relaxed);
            Ok(Box::new(std::io::Cursor::new(self.bytes.to_vec())))
        }
    }

    #[test]
    fn path_iso_open_propagates_a_missing_file() {
        // PathIso::open and UdfSource::open both surface the File::open error.
        let factory = PathIso::new("no/such/bdinfo-rs-udf-xyzzy.iso");
        assert!(factory.open().is_err());
        assert!(
            UdfSource::open(Box::new(PathIso::new("no/such/bdinfo-rs-udf-xyzzy.iso"))).is_err()
        );
    }

    #[test]
    fn open_read_and_open_text_propagate_a_factory_failure() {
        // The source constructs with the single allowed open; a later open_read of
        // a runs-backed file then fails.
        let factory = OpenOnce { remaining: AtomicU32::new(1), bytes: Arc::from(physical_iso()) };
        let src = UdfSource::open(Box::new(factory)).expect("open");
        let files = src.root().get_files().expect("files");
        let data = files.iter().find(|f| f.name() == "data.bin").expect("data.bin");
        assert!(data.open_read().is_err());
        assert!(data.open_text().is_err());
    }

    #[test]
    fn dir_at_and_file_at_reject_out_of_range_indices() {
        let src = open_physical();
        assert!(super::dir_at(&src.inner, 999_999).is_none());
        assert!(src.root().file_at(999_999).is_none());
    }

    #[test]
    fn open_with_oversized_vds_fails_the_read() {
        // The AVDP claims a VDS far larger than the image → a VDS sector read fails.
        let mut iso = Iso::new(262);
        iso.write(256, &avdp(257, 50 * SS as u32));
        iso.write(257, &pd(0, 1, 1));
        assert!(UdfSource::open(MemIso::boxed(iso.into_bytes())).is_err());
    }

    #[test]
    fn open_with_unreadable_fsd_sector_fails() {
        // Valid up to the LVD; the FSD long_ad resolves to a sector past the image.
        let mut iso = Iso::new(265);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 10));
        iso.write(258, &lvd("X", SS as u32, 9000, 0, &phys_map(0), 1));
        assert!(UdfSource::open(MemIso::boxed(iso.into_bytes())).is_err());
    }

    #[test]
    fn open_with_unreadable_metadata_file_entry_fails() {
        // The metadata file location resolves past the image.
        let lvd = Lvd {
            logical_volume_identifier: String::new(),
            logical_block_size: SS as u32,
            file_set_descriptor: LongAd {
                raw_length: 0,
                location: LbAddr { block: 0, partition: 0 },
            },
            partition_maps: vec![
                PartitionMap::Physical { partition_number: 0 },
                PartitionMap::Metadata(MetadataPartitionMap {
                    physical_partition: 0,
                    metadata_file_location: 9000,
                    metadata_mirror_file_location: 3,
                }),
            ],
        };
        let pds =
            vec![PartitionDescriptor { partition_number: 0, starting_location: 0, length: 100 }];
        let mut cursor = std::io::Cursor::new(vec![0_u8; 8 * SS]);
        assert!(resolve_partitions(&mut cursor, &lvd, &pds, SS as u64, Limits::DEFAULT).is_err());
    }

    #[test]
    fn open_with_a_bad_root_continuation_fails_the_build() {
        // The root dir FE points its allocation list at an out-of-image
        // continuation block → build_tree (hence open) errors.
        let mut iso = Iso::new(270);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(0, 260, 50));
        iso.write(258, &lvd("X", SS as u32, 1, 0, &phys_map(0), 1));
        iso.write(261, &fsd(0, 2));
        // Root dir FE at block 2 → sector 262, NextExtent → block 9000 (out of image).
        iso.write(262, &fe(261, 4, 0, 100, &sad(3, 16, 9000)));
        assert!(UdfSource::open(MemIso::boxed(iso.into_bytes())).is_err());
    }

    #[test]
    fn expand_directory_io_and_propagation_arms() {
        let volume = flat_volume();
        let mut image = vec![0_u8; 8 * SS];
        // block 2: a directory FE with a NextExtent → out-of-image (directory_bytes errors).
        put(&mut image, 2 * SS, &fe(261, 4, 0, 100, &sad(3, 16, 9000)));
        // block 3: an embedded directory FE naming a file child at block 4.
        let fids = dir_data(&[fid(0x0A, "", 3, 0), fid(0x00, "f", 4, 0)]);
        put(&mut image, 3 * SS, &fe(261, 4, 3, fids.len() as u64, &fids));
        // block 4: that file's FE with a NextExtent → out-of-image (file_body errors).
        put(&mut image, 4 * SS, &fe(261, 5, 0, 100, &sad(3, 16, 9000)));
        let mut cursor = std::io::Cursor::new(image);
        // Directory FE read fails (sector out of the image).
        assert!(expand_directory(&mut cursor, &volume, "", 0, 9000, Limits::DEFAULT).is_err());
        // directory_bytes error propagates.
        assert!(expand_directory(&mut cursor, &volume, "", 0, 2, Limits::DEFAULT).is_err());
        // file_body error (a child file's bad continuation) propagates.
        assert!(expand_directory(&mut cursor, &volume, "", 0, 3, Limits::DEFAULT).is_err());
    }

    #[test]
    fn directory_bytes_propagates_a_failed_extent_read() {
        let volume = flat_volume();
        // A directory with a valid (non-continuation) extent whose block is out of
        // the image → reading its bytes fails.
        let buf = fe(261, 4, 0, 2048, &sad(0, 2048, 9000));
        let entry = FileEntry::parse(&buf).expect("fe");
        let mut cursor = std::io::Cursor::new(vec![0_u8; 4 * SS]);
        assert!(directory_bytes(&mut cursor, &volume, &entry, 0, Limits::DEFAULT).is_err());
    }

    #[test]
    fn file_body_skips_an_unresolvable_extent() {
        let volume = flat_volume();
        // A file whose long_ad extent cites partition ref 9 (no such partition) →
        // the extent resolves to nothing and is skipped (no runs), not an error.
        let buf = fe(261, 5, 1, 64, &lad(0, 64, 0, 9));
        let entry = FileEntry::parse(&buf).expect("fe");
        let mut cursor = std::io::Cursor::new(vec![0_u8; 4 * SS]);
        let body = file_body(&mut cursor, &volume, &entry, 0, Limits::DEFAULT).expect("body");
        assert_eq!(body.length, 64);
    }

    #[test]
    fn resolve_partitions_propagates_a_bad_metadata_continuation() {
        let lvd = Lvd {
            logical_volume_identifier: String::new(),
            logical_block_size: SS as u32,
            file_set_descriptor: LongAd {
                raw_length: 0,
                location: LbAddr { block: 0, partition: 0 },
            },
            partition_maps: vec![
                PartitionMap::Physical { partition_number: 0 },
                PartitionMap::Metadata(MetadataPartitionMap {
                    physical_partition: 0,
                    metadata_file_location: 2,
                    metadata_mirror_file_location: 3,
                }),
            ],
        };
        let pds =
            vec![PartitionDescriptor { partition_number: 0, starting_location: 0, length: 100 }];
        // Metadata file FE (FileType 250) at physical block 2 → sector 2, with a
        // NextExtent that points out of the image → collect_extents fails → the
        // candidate is dropped and (no mirror) resolve_partitions errors.
        let mut image = vec![0_u8; 8 * SS];
        put(&mut image, 2 * SS, &fe(261, 250, 0, 100, &sad(3, 16, 9000)));
        let mut cursor = std::io::Cursor::new(image);
        assert!(resolve_partitions(&mut cursor, &lvd, &pds, SS as u64, Limits::DEFAULT).is_err());
    }

    #[test]
    fn resolve_partitions_rejects_a_metadata_map_with_no_backing_partition() {
        // The metadata map names physical partition 7, but the only PD is #0.
        let lvd = Lvd {
            logical_volume_identifier: String::new(),
            logical_block_size: SS as u32,
            file_set_descriptor: LongAd {
                raw_length: 0,
                location: LbAddr { block: 0, partition: 0 },
            },
            partition_maps: vec![PartitionMap::Metadata(MetadataPartitionMap {
                physical_partition: 7,
                metadata_file_location: 2,
                metadata_mirror_file_location: 3,
            })],
        };
        let pds =
            vec![PartitionDescriptor { partition_number: 0, starting_location: 0, length: 100 }];
        let mut cursor = std::io::Cursor::new(vec![0_u8; 8 * SS]);
        assert!(resolve_partitions(&mut cursor, &lvd, &pds, SS as u64, Limits::DEFAULT).is_err());
    }

    #[test]
    fn duplicate_fid_is_visited_once() {
        // Two FIDs naming the same child ICB → the second is skipped (visited).
        let src = open_physical();
        // `dup` and `data.bin` share ICB (0, 6); only one file node exists for it.
        let files = src.root().get_files().expect("files");
        assert_eq!(files.iter().filter(|f| f.name() == "data.bin" || f.name() == "dup").count(), 1);
    }

    // ── Hostile-input caps (hardening) ───────────────────────────────────────

    /// A minimal, fully valid single-partition volume with the given
    /// `LogicalBlockSize`: an embedded (childless) root directory — small enough
    /// that the block-size cap tests prove acceptance/rejection comes from the
    /// cap alone, not from a downstream parse failure.
    fn minimal_iso_with_block_size(bs: u32) -> Vec<u8> {
        let mut bytes = vec![0_u8; 262 * SS];
        put(&mut bytes, 256 * SS, &avdp(257, 2 * SS as u32));
        put(&mut bytes, 257 * SS, &pd(0, 2, 10));
        put(&mut bytes, 258 * SS, &lvd("CAPBS", bs, 1, 0, &phys_map(0), 1));
        // Partition 0 starts at logical sector 2: the FSD (block 1) lives at
        // byte 3 × bs, the embedded root directory FE (block 2) at byte 4 × bs.
        let bs_us = bs as usize;
        put(&mut bytes, 3 * bs_us, &fsd(0, 2));
        put(&mut bytes, 4 * bs_us, &fe(261, 4, 3, 0, &[]));
        bytes
    }

    #[test]
    fn open_at_exactly_max_block_size_is_accepted() {
        // 32 KiB is the cap itself — accepted (the guard is `>`, not `>=`).
        let src = UdfSource::open(MemIso::boxed(minimal_iso_with_block_size(32 << 10)))
            .expect("open at the 32 KiB cap");
        assert_eq!(src.volume_label(), "CAPBS");
        assert!(file_names(&src.root()).is_empty());
    }

    #[test]
    fn open_with_oversized_block_size_is_structure_not_found() {
        // The same volume one doubling past the cap is rejected by the cap
        // alone (the 32 KiB twin above proves it would otherwise open fine) —
        // a hostile block size can no longer size every sector read.
        let err = UdfSource::open(MemIso::boxed(minimal_iso_with_block_size(64 << 10)))
            .expect_err("oversized block size");
        assert_eq!(err.to_string(), "unable to locate BD structure");
    }

    #[test]
    fn collect_extents_caps_direct_extents_at_max_extents() {
        // Three real descriptors with a cap of two → exactly two collected.
        let mut ad = sad(0, 2048, 40);
        ad.extend_from_slice(&sad(0, 2048, 41));
        ad.extend_from_slice(&sad(0, 2048, 42));
        let entry = FileEntry::parse(&fe(261, 5, 0, 6144, &ad)).expect("fe");
        let locs = vec![PartitionLoc::Physical { start: 0 }];
        let mut cursor = std::io::Cursor::new(vec![0_u8; 4 * SS]);
        let limits = Limits { max_extents: 2, ..Limits::DEFAULT };
        let extents =
            collect_extents(&mut cursor, &locs, &entry, 0, SS as u64, limits).expect("extents");
        assert_eq!(extents.iter().map(|e| e.block).collect::<Vec<_>>(), vec![40, 41]);
    }

    #[test]
    fn collect_extents_caps_a_continuation_at_max_extents() {
        // A continuation holding three real descriptors with a cap of two → the
        // chain stops amassing extents at exactly the cap.
        let mut ads = sad(0, 2048, 50);
        ads.extend_from_slice(&sad(0, 2048, 51));
        ads.extend_from_slice(&sad(0, 2048, 52));
        ads.extend_from_slice(&sad(0, 0, 0)); // terminator
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, 2 * SS, &aed(&ads));
        let mut cursor = std::io::Cursor::new(image);
        let entry = FileEntry::parse(&fe(261, 5, 0, 6144, &sad(3, SS as u32, 2))).expect("fe");
        let locs = vec![PartitionLoc::Physical { start: 0 }];
        let limits = Limits { max_extents: 2, ..Limits::DEFAULT };
        let extents =
            collect_extents(&mut cursor, &locs, &entry, 0, SS as u64, limits).expect("extents");
        assert_eq!(extents.iter().map(|e| e.block).collect::<Vec<_>>(), vec![50, 51]);
    }

    #[test]
    fn collect_extents_clamps_a_hostile_continuation_length_to_one_block() {
        // The NextExtent claims the full 30-bit length (~1 GiB); only one
        // logical block is read for it, so the walk parses the continuation
        // instead of attempting (and failing) a giant read.
        let mut ads = sad(0, 4096, 77);
        ads.extend_from_slice(&sad(0, 0, 0)); // terminator
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, 2 * SS, &aed(&ads));
        let mut cursor = std::io::Cursor::new(image);
        let entry = FileEntry::parse(&fe(261, 5, 0, 4096, &sad(3, 0x3FFF_FFFF, 2))).expect("fe");
        let locs = vec![PartitionLoc::Physical { start: 0 }];
        let extents = collect_extents(&mut cursor, &locs, &entry, 0, SS as u64, Limits::DEFAULT)
            .expect("extents");
        assert_eq!(extents.iter().map(|e| e.block).collect::<Vec<_>>(), vec![77]);
    }

    #[test]
    fn directory_bytes_clamps_each_extent_to_the_remaining_budget() {
        let volume = flat_volume();
        // The directory's one extent claims the full 30-bit length (~1 GiB):
        // the read is clamped to the remaining `max_dir_bytes` budget instead
        // of allocating and reading the whole claim.
        let entry =
            FileEntry::parse(&fe(261, 4, 0, 0x3FFF_FFFF, &sad(0, 0x3FFF_FFFF, 1))).expect("fe");
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, SS, &[0xEE_u8; SS]);
        let mut cursor = std::io::Cursor::new(image);
        let limits = Limits { max_dir_bytes: 100, ..Limits::DEFAULT };
        let bytes = directory_bytes(&mut cursor, &volume, &entry, 0, limits).expect("clamped");
        assert_eq!(bytes, vec![0xEE_u8; 100]);
    }

    #[test]
    fn directory_bytes_clamps_before_resolving_a_metadata_extent() {
        // The extent's claimed length exceeds the metadata mapping, so resolving
        // it UNCLAMPED would fail; clamped to the remaining budget first, the
        // read resolves and stays within budget — the clamp must happen before
        // run resolution, not just before the read.
        let volume = Volume {
            block_size: SS as u64,
            locs: vec![PartitionLoc::Metadata {
                phys_start: 0,
                extents: vec![Extent {
                    partition_ref: None,
                    block: 1,
                    length: SS as u32,
                    kind: ExtentKind::RecordedAllocated,
                }],
                mirror_extents: Vec::new(),
            }],
            label: String::new(),
            root_icb: (0, 0),
        };
        let entry =
            FileEntry::parse(&fe(261, 4, 0, 0x3FFF_FFFF, &sad(0, 0x3FFF_FFFF, 0))).expect("fe");
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, SS, &[0xDD_u8; SS]);
        let mut cursor = std::io::Cursor::new(image);
        let limits = Limits { max_dir_bytes: 64, ..Limits::DEFAULT };
        let bytes = directory_bytes(&mut cursor, &volume, &entry, 0, limits).expect("clamped");
        assert_eq!(bytes, vec![0xDD_u8; 64]);
    }

    #[test]
    fn read_extent_bytes_with_an_unbounded_cap_reads_the_full_extent() {
        use super::read_extent_bytes;
        // A cap wider than `u32` saturates the clamp — the extent's own length wins.
        let locs = [PartitionLoc::Physical { start: 0 }];
        let extent = Extent {
            partition_ref: None,
            block: 1,
            length: 16,
            kind: ExtentKind::RecordedAllocated,
        };
        let mut image = vec![0_u8; 4 * SS];
        put(&mut image, SS, b"0123456789ABCDEF");
        let mut cursor = std::io::Cursor::new(image);
        let bytes = read_extent_bytes(&mut cursor, &locs, 0, &extent, SS as u64, u64::MAX)
            .expect("uncapped read");
        assert_eq!(bytes, b"0123456789ABCDEF");
    }

    #[test]
    fn file_body_caps_the_per_file_run_list() {
        // One extent resolving through a shattered metadata mapping yields two
        // discontiguous runs; with `max_extents: 1` the per-file run list keeps
        // exactly one (bounded result, not an unbounded run explosion).
        let volume = Volume {
            block_size: SS as u64,
            locs: vec![PartitionLoc::Metadata {
                phys_start: 100,
                extents: vec![
                    Extent {
                        partition_ref: None,
                        block: 10,
                        length: SS as u32,
                        kind: ExtentKind::RecordedAllocated,
                    },
                    Extent {
                        partition_ref: None,
                        block: 50,
                        length: SS as u32,
                        kind: ExtentKind::RecordedAllocated,
                    },
                ],
                mirror_extents: Vec::new(),
            }],
            label: String::new(),
            root_icb: (0, 0),
        };
        let entry =
            FileEntry::parse(&fe(261, 5, 0, 2 * SS as u64, &sad(0, 2 * SS as u32, 0))).expect("fe");
        let mut cursor = std::io::Cursor::new(vec![0_u8; SS]);
        let limits = Limits { max_extents: 1, ..Limits::DEFAULT };
        let body = file_body(&mut cursor, &volume, &entry, 0, limits).expect("body");
        assert_eq!(
            body.content,
            Content::Runs(vec![Run { src: Some(110 * SS as u64), len: SS as u64 }])
        );
    }

    #[test]
    fn metadata_runs_coalesces_physically_adjacent_extents() {
        // Two extents mapping consecutive physical blocks → one coalesced run.
        let extents = vec![
            Extent {
                partition_ref: None,
                block: 10,
                length: SS as u32,
                kind: ExtentKind::RecordedAllocated,
            },
            Extent {
                partition_ref: None,
                block: 11,
                length: SS as u32,
                kind: ExtentKind::RecordedAllocated,
            },
        ];
        let runs = metadata_runs(100, &extents, 0, 2 * SS as u64, SS as u64).expect("runs");
        assert_eq!(runs, vec![Run { src: Some(110 * SS as u64), len: 2 * SS as u64 }]);
    }

    #[test]
    fn metadata_runs_skips_extents_past_the_wanted_range() {
        // The wanted range ends exactly where the second extent begins — it
        // contributes nothing (in particular, no zero-length run).
        let extents = vec![
            Extent {
                partition_ref: None,
                block: 10,
                length: 2 * SS as u32,
                kind: ExtentKind::RecordedAllocated,
            },
            Extent {
                partition_ref: None,
                block: 50,
                length: SS as u32,
                kind: ExtentKind::RecordedAllocated,
            },
        ];
        let runs = metadata_runs(100, &extents, 0, 2 * SS as u64, SS as u64).expect("runs");
        assert_eq!(runs, vec![Run { src: Some(110 * SS as u64), len: 2 * SS as u64 }]);
    }

    #[test]
    fn metadata_runs_rejects_a_zero_block_size() {
        let extents = [Extent {
            partition_ref: None,
            block: 1,
            length: 2048,
            kind: ExtentKind::RecordedAllocated,
        }];
        assert_eq!(metadata_runs(0, &extents, 0, 2048, 0), None);
    }

    // ── Metadata-partition namespaces (PartitionNumber ≠ reference index) ────

    /// Like [`metadata_iso`] but the physical partition's `PartitionNumber` is 5
    /// while its partition-REFERENCE index stays 0, and the metadata file's
    /// allocation list runs through a `NextExtent` continuation — the trigger
    /// for the namespace conflation this regression-tests (the continuation
    /// must be read through reference index 0, not `PartitionNumber` 5).
    fn metadata_iso_with_nonzero_partition_number() -> Vec<u8> {
        let mut iso = Iso::new(330);
        iso.write(256, &avdp(257, 2 * SS as u32));
        iso.write(257, &pd(5, 300, 100));
        let mut maps = phys_map(5);
        maps.extend_from_slice(&meta_map(5, 2, 3));
        iso.write(258, &lvd("VOLNS", SS as u32, 1, 1, &maps, 2));
        // Metadata file FE at physical block 2 → sector 302; its first extent
        // maps metadata blocks 0..3 → physical 10..13, then a NextExtent
        // continuation (physical block 4 → sector 304) maps blocks 3..6 →
        // physical 13..16.
        let mut meta_ads = sad(0, 3 * SS as u32, 10);
        meta_ads.extend_from_slice(&sad(3, SS as u32, 4));
        iso.write(302, &fe(261, 250, 0, 6 * SS as u64, &meta_ads));
        let mut cont = sad(0, 3 * SS as u32, 13);
        cont.extend_from_slice(&sad(0, 0, 0)); // terminator
        iso.write(304, &aed(&cont));
        // FSD at metadata block 1 → sector 311; root dir FE at metadata block 2
        // → sector 312, its FID data at metadata block 3 → continuation-mapped
        // physical block 13 → sector 313.
        iso.write(311, &fsd(1, 2));
        let root_fids = dir_data(&[fid(0x0A, "", 2, 1), fid(0x00, "FILE.BIN", 4, 1)]);
        iso.write(312, &fe(261, 4, 0, root_fids.len() as u64, &sad(0, root_fids.len() as u32, 3)));
        iso.write(313, &root_fids);
        // FILE.BIN FE at metadata block 4 → sector 314; its data in the
        // physical partition (reference 0) at block 20 → sector 320.
        iso.write(314, &fe(261, 5, 1, 64, &lad(0, 64, 20, 0)));
        iso.write(320, b"physical-partition-data-bytes-payload-0123456789ABCDEF0123456789");
        iso.into_bytes()
    }

    #[test]
    fn metadata_partition_number_distinct_from_reference_index_resolves() {
        let src = UdfSource::open(MemIso::boxed(metadata_iso_with_nonzero_partition_number()))
            .expect("open namespace iso");
        assert_eq!(src.volume_label(), "VOLNS");
        let root = src.root();
        assert_eq!(file_names(&root), vec!["FILE.BIN".to_owned()]);
        let files = root.get_files().expect("files");
        let file = files.first().expect("FILE.BIN");
        assert_eq!(
            read_all(&**file),
            b"physical-partition-data-bytes-payload-0123456789ABCDEF0123456789"
        );
    }

    #[test]
    fn resolve_partitions_rejects_a_metadata_map_without_a_physical_map() {
        // The metadata map's PartitionNumber has a matching Partition
        // Descriptor but NO type-1 map carries it, so there is no partition
        // reference for the metadata file's short-form descriptors.
        let lvd = Lvd {
            logical_volume_identifier: String::new(),
            logical_block_size: SS as u32,
            file_set_descriptor: LongAd {
                raw_length: 0,
                location: LbAddr { block: 0, partition: 0 },
            },
            partition_maps: vec![PartitionMap::Metadata(MetadataPartitionMap {
                physical_partition: 0,
                metadata_file_location: 2,
                metadata_mirror_file_location: 3,
            })],
        };
        let pds =
            vec![PartitionDescriptor { partition_number: 0, starting_location: 0, length: 100 }];
        let mut cursor = std::io::Cursor::new(vec![0_u8; 8 * SS]);
        assert!(resolve_partitions(&mut cursor, &lvd, &pds, SS as u64, Limits::DEFAULT).is_err());
    }

    // ── Bad-sector fault injection (resilient vs strict file reads) ──────────

    /// An [`IsoReader`] over an in-memory image whose reads fail inside a chosen
    /// absolute byte range — the injectable "bad sector" a real `.iso` file
    /// cannot produce on demand.
    #[derive(Debug, Clone)]
    struct FaultyIso {
        data: Arc<[u8]>,
        /// The absolute byte range whose reads fail.
        bad: std::ops::Range<u64>,
    }

    impl FaultyIso {
        fn boxed(bytes: Vec<u8>, bad: std::ops::Range<u64>) -> Box<dyn IsoReader> {
            Box::new(Self { data: Arc::from(bytes), bad })
        }
    }

    impl IsoReader for FaultyIso {
        fn open(&self) -> std::io::Result<Box<dyn super::ReadSeek>> {
            Ok(Box::new(FaultyReader {
                cursor: std::io::Cursor::new(self.data.to_vec()),
                bad: self.bad.clone(),
            }))
        }
    }

    /// The stream behind [`FaultyIso`]: any read overlapping `bad` errors.
    struct FaultyReader {
        cursor: std::io::Cursor<Vec<u8>>,
        bad: std::ops::Range<u64>,
    }

    impl Read for FaultyReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let pos = self.cursor.position();
            let end = pos.saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
            if pos < self.bad.end && end > self.bad.start {
                return Err(std::io::Error::other("injected bad sector"));
            }
            self.cursor.read(buf)
        }
    }

    impl Seek for FaultyReader {
        fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(from)
        }
    }

    /// `data.bin`'s first extent (block 20 → sector 280) as an absolute byte range
    /// — the injected bad sector for the fault tests.
    fn data_bin_first_extent() -> std::ops::Range<u64> {
        (280 * SS as u64)..(281 * SS as u64)
    }

    #[test]
    fn resilient_open_zero_fills_a_bad_sector_records_it_once_and_continues() {
        let src =
            UdfSource::open_resilient(FaultyIso::boxed(physical_iso(), data_bin_first_extent()))
                .expect("the volume structures are intact, only file data is bad");
        let root = src.root();
        let files = root.get_files().expect("list root");
        let data = files.iter().find(|f| f.name() == "data.bin").expect("data.bin");

        // The unreadable first extent reads as zeros; the readable second extent
        // (and the rest of the disc) still reads — the scan continues.
        let bytes = read_all(&**data);
        assert_eq!(bytes.len(), SS + 100);
        assert!(bytes.iter().take(SS).all(|&b| b == 0), "bad extent zero-filled");
        assert!(bytes.iter().skip(SS).all(|&b| b == 0xCD), "good extent still read");

        // Exactly one failure is recorded for the handle, at its first unreadable
        // byte position, and draining empties the sink.
        let errors = src.take_errors();
        assert_eq!(errors.len(), 1);
        let err = errors.first().expect("one recorded error");
        assert_eq!(err.stage, ScanStage::SectorRead);
        assert!(err.file.starts_with("data.bin @ byte 0"), "got {:?}", err.file);
        assert!(err.reason.to_string().contains("injected bad sector"));
        assert!(src.take_errors().is_empty());

        // An untouched healthy file records nothing.
        let hide = files.iter().find(|f| f.name() == "hide.x").expect("hide.x");
        assert_eq!(read_all(&**hide), b"0123456789");
        assert!(src.take_errors().is_empty());
    }

    #[test]
    fn strict_open_propagates_a_bad_sector_read() {
        // The same damaged image under the strict open: the file read errors (the
        // pre-resilience behavior `--strict` preserves) and nothing is recorded.
        let src = UdfSource::open(FaultyIso::boxed(physical_iso(), data_bin_first_extent()))
            .expect("volume structures intact");
        let root = src.root();
        let files = root.get_files().expect("list root");
        let data = files.iter().find(|f| f.name() == "data.bin").expect("data.bin");
        let mut reader = data.open_read().expect("open");
        let mut sink = Vec::new();
        let err = reader.read_to_end(&mut sink).expect_err("strict read fails");
        assert!(err.to_string().contains("injected bad sector"));
        assert!(src.take_errors().is_empty());
    }

    #[test]
    fn a_damaged_handle_reports_only_its_first_unreadable_position() {
        let src =
            UdfSource::open_resilient(FaultyIso::boxed(physical_iso(), data_bin_first_extent()))
                .expect("open resilient");
        let root = src.root();
        let files = root.get_files().expect("list root");
        let data = files.iter().find(|f| f.name() == "data.bin").expect("data.bin");

        // Two consecutive failing reads on one handle: both zero-fill, but only the
        // first records (the sink cannot be flooded by a long run of bad sectors).
        let mut reader = data.open_read().expect("open");
        let mut buf = [0xFF_u8; 16];
        assert_eq!(reader.read(&mut buf).expect("first failing read"), 16);
        assert_eq!(buf, [0_u8; 16]);
        assert_eq!(reader.read(&mut buf).expect("second failing read"), 16);
        let errors = src.take_errors();
        assert_eq!(errors.len(), 1, "one record per damaged handle");
        assert!(
            errors.first().expect("recorded").file.ends_with("@ byte 0"),
            "the FIRST failing position is the one recorded"
        );

        // A second handle on the same file records its own (fresh) failure.
        let mut again = data.open_read().expect("reopen");
        let mut rest = Vec::new();
        again.read_to_end(&mut rest).expect("resilient read completes");
        assert_eq!(src.take_errors().len(), 1);
    }

    // ── No-panic property over arbitrary images ──────────────────────────────

    proptest! {
        /// `UdfSource::open` returns `Ok`/`Err` on arbitrary bytes — never
        /// panics, hangs, or allocation-amplifies. The always-on mirror of the
        /// `source` fuzz target (fuzz/README.md), which maps its input at the
        /// anchor sector the same way.
        #[test]
        fn open_never_panics_on_arbitrary_bytes(
            data in proptest::collection::vec(any::<u8>(), 0..2048)
        ) {
            // As a raw (short) image…
            drop(UdfSource::open(MemIso::boxed(data.clone())));
            // …and mapped at the anchor sector, where the volume chain reads it.
            let mut image = vec![0_u8; 256 * SS];
            image.extend_from_slice(&data);
            drop(UdfSource::open(MemIso::boxed(image)));
        }

        /// Opening a VALID image after **arbitrary byte corruptions** never
        /// panics. The three images carry the hardened structures — a VDP
        /// continuation, an AED `NextExtent` chain, and a metadata mirror — so
        /// random corruption drives the fallback-anchor, reserve-VDS, VDP-hop,
        /// AED-clamp, and mirror-retry recovery paths with malformed bytes, not
        /// just the well-formed fixtures.
        #[test]
        fn corrupted_valid_images_never_panic(
            flips in proptest::collection::vec((0_usize..360 * SS, any::<u8>()), 0..16),
            which in 0_usize..3,
        ) {
            let mut bytes = match which {
                0 => mirrored_metadata_iso(),
                1 => metadata_iso_with_nonzero_partition_number(),
                _ => vdp_iso(),
            };
            for &(pos, val) in &flips {
                if let Some(slot) = bytes.get_mut(pos) {
                    *slot = val;
                }
            }
            drop(UdfSource::open(MemIso::boxed(bytes.clone())));
            if let Ok(src) = UdfSource::open_resilient(MemIso::boxed(bytes)) {
                let mut sink = Vec::new();
                for file in src.root().get_files().expect("arena listing is infallible") {
                    if let Ok(mut reader) = file.open_read() {
                        drop(reader.read_to_end(&mut sink));
                    }
                }
                drop(src.take_errors());
            }
        }

        /// The resilient open over a valid image with an **arbitrary bad byte
        /// range** never panics: if the fault hits the volume structures the open
        /// fails cleanly; otherwise every file still reads to its full length
        /// (unreadable spans zero-filled) and each damaged read is recorded — the
        /// no-panic amplifier over hostile fault patterns.
        #[test]
        fn resilient_reads_never_panic_under_arbitrary_fault_ranges(
            start in 0_u64..(300 * SS as u64),
            len in 0_u64..(40 * SS as u64),
        ) {
            let bad = start..start.saturating_add(len);
            // Strict: open + read may fail, must not panic.
            if let Ok(src) = UdfSource::open(FaultyIso::boxed(physical_iso(), bad.clone())) {
                let mut sink = Vec::new();
                for file in src.root().get_files().expect("arena listing is infallible") {
                    if let Ok(mut reader) = file.open_read() {
                        drop(reader.read_to_end(&mut sink));
                    }
                }
            }
            // The same arbitrary fault over the mirrored-metadata image drives
            // the mirror-retry paths under hostile fault patterns.
            if let Ok(src) =
                UdfSource::open(FaultyIso::boxed(mirrored_metadata_iso(), bad.clone()))
            {
                let mut sink = Vec::new();
                for file in src.root().get_files().expect("arena listing is infallible") {
                    if let Ok(mut reader) = file.open_read() {
                        drop(reader.read_to_end(&mut sink));
                    }
                }
            }
            // Resilient: a post-open fault zero-fills, so every file reads fully.
            if let Ok(src) = UdfSource::open_resilient(FaultyIso::boxed(physical_iso(), bad)) {
                for file in src.root().get_files().expect("arena listing is infallible") {
                    let bytes = read_all(&*file);
                    prop_assert_eq!(bytes.len() as u64, file.length());
                }
                drop(src.take_errors());
            }
        }
    }
}
