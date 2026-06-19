//! The shared library error type for malformed Blu-ray metadata.
//!
//! The BDMV parsers (`bdrom::clpi`, `bdrom::mpls`, …) consume attacker-controlled
//! disc bytes, so every failure is surfaced as a [`BdError`] rather than a panic
//! (the house rule: malformed data → `Result<_, BdError>`; absent field / EOF →
//! `Option`). Every parser failure routes through this one enum, so callers
//! handle malformed metadata in a single place.
//!
//! `BdError` is `#[non_exhaustive]` so new variants can be added without breaking
//! downstream `match`es, and its `Display`/`Error`/`From` impls are derived with
//! [`thiserror`] (messages lowercase, no trailing punctuation). The [`Io`] variant
//! *wraps* the underlying [`std::io::Error`] (`#[from]`, exposed via
//! [`Error::source`](std::error::Error::source)) rather than stringifying it, so the
//! original IO cause is preserved for callers that inspect the error chain. The
//! library returns `BdError` everywhere; `anyhow` is confined to the CLI.
//!
//! [`Io`]: BdError::Io

/// An error parsing a Blu-ray metadata structure, or reading the disc it lives on.
///
/// `#[non_exhaustive]`: match with a wildcard arm — the demux/codec layers may add
/// variants as the parser grows.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BdError {
    /// A metadata file began with a type magic this parser does not accept —
    /// e.g. a `*.clpi` whose first eight bytes are not `HDMV0100`/`0200`/`0300`,
    /// or a `*.mpls` not starting `MPLS0100`/`0200`/`0300`. Carries the offending
    /// magic.
    #[error("unknown file type: {0}")]
    UnknownFileType(String),
    /// A field read ran past the end of the buffer — a truncated or otherwise
    /// malformed file. Returned wherever a short metadata file would otherwise
    /// force an out-of-bounds read.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// The disc folder does not contain a recognizable BD structure — no `BDMV`
    /// directory, or `BDMV` without both `CLIPINF` and `PLAYLIST`.
    #[error("unable to locate BD structure")]
    StructureNotFound,
    /// A playlist referenced a clip-information (`*.clpi`) file that is not present
    /// on the disc. Carries the missing file's name.
    #[error("referenced missing clip file: {0}")]
    MissingClipFile(String),
    /// An IO error reading the disc (enumerating a directory, opening or reading a
    /// file). Wraps the underlying [`std::io::Error`] as its source; the disc-scan
    /// and parser layers surface filesystem failures rather than panicking on
    /// them.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Where in a resilient disc scan a per-file failure occurred — the discriminant
/// of a [`ScanError`].
///
/// The resilient scan isolates failures per file class — the clip-info, playlist,
/// and stream-file scans each record their own errors, as do the discovery-phase
/// reads and the `.iso` sector layer — so one bad file never aborts the whole
/// scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanStage {
    /// Enumerating directories or reading disc-level metadata (the disc-open
    /// phase: directory sizes, flags, `index.bdmv`, `bdmt_eng.xml`).
    Discovery,
    /// Parsing a clip-information (`*.clpi`) file.
    ClipInfo,
    /// Parsing a playlist (`*.mpls`) file or resolving its clips.
    Playlist,
    /// Packet-scanning a stream (`*.m2ts`) file.
    StreamFile,
    /// Reading a sector-backed byte range out of a UDF `.iso` (a bad/unreadable
    /// sector under a file's data — recorded and zero-filled in resilient mode).
    SectorRead,
}

impl ScanStage {
    /// The lowercase stage label the CLI's stderr error summary prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::ClipInfo => "clipinfo",
            Self::Playlist => "playlist",
            Self::StreamFile => "stream",
            Self::SectorRead => "sector",
        }
    }
}

/// One recorded per-file failure from a resilient disc scan.
///
/// Records which file failed, at which [`ScanStage`], and the underlying
/// [`BdError`]. Collected by `BdRom::open_resilient` (and the resilient `.iso`
/// reader) instead of aborting the whole scan — the readable rest is still
/// emitted, and the failures are surfaced, never silently dropped.
#[derive(Debug)]
pub struct ScanError {
    /// The file (or directory) the failure occurred on, as named on the disc.
    pub file: String,
    /// Where in the scan the failure occurred.
    pub stage: ScanStage,
    /// The underlying failure.
    pub reason: BdError,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}: {}", self.stage.label(), self.file, self.reason)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::io;

    use super::{BdError, ScanError, ScanStage};

    #[test]
    fn display_renders_each_variant() {
        assert_eq!(
            BdError::UnknownFileType("MPLSXXXX".to_owned()).to_string(),
            "unknown file type: MPLSXXXX"
        );
        assert_eq!(BdError::UnexpectedEof.to_string(), "unexpected end of input");
        assert_eq!(BdError::StructureNotFound.to_string(), "unable to locate BD structure");
        assert_eq!(
            BdError::MissingClipFile("00000.CLPI".to_owned()).to_string(),
            "referenced missing clip file: 00000.CLPI"
        );
        let io = BdError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        assert_eq!(io.to_string(), "io error: denied");
    }

    #[test]
    fn io_errors_convert_into_bd_error() {
        let io = io::Error::new(io::ErrorKind::NotFound, "nope");
        let err: BdError = io.into();
        // The `#[from]` produced an `Io` variant wrapping the original error.
        assert!(matches!(err, BdError::Io(ref inner) if inner.kind() == io::ErrorKind::NotFound));
        assert_eq!(err.to_string(), "io error: nope");
    }

    #[test]
    fn only_the_io_variant_exposes_an_underlying_cause() {
        // The wrapped IO error is reachable as the error's `source` (the chain the
        // stringifying predecessor discarded); every other variant has no source.
        // Exercising `source` on all five covers each arm thiserror generates.
        let io = BdError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "short"));
        assert_eq!(io.source().expect("Io carries a source").to_string(), "short");
        for sourceless in [
            BdError::UnknownFileType("HDMV9999".to_owned()),
            BdError::UnexpectedEof,
            BdError::StructureNotFound,
            BdError::MissingClipFile("00000.CLPI".to_owned()),
        ] {
            assert!(sourceless.source().is_none());
        }
    }

    #[test]
    fn scan_stage_labels_every_variant() {
        assert_eq!(ScanStage::Discovery.label(), "discovery");
        assert_eq!(ScanStage::ClipInfo.label(), "clipinfo");
        assert_eq!(ScanStage::Playlist.label(), "playlist");
        assert_eq!(ScanStage::StreamFile.label(), "stream");
        assert_eq!(ScanStage::SectorRead.label(), "sector");
        // Exercise the derived Debug/Copy/Eq.
        assert_eq!(format!("{:?}", ScanStage::ClipInfo), "ClipInfo");
        let copied = ScanStage::Playlist;
        assert_eq!(copied, ScanStage::Playlist);
        assert_ne!(ScanStage::Discovery, ScanStage::SectorRead);
    }

    #[test]
    fn scan_error_displays_stage_file_and_reason() {
        let err = ScanError {
            file: "00001.CLPI".to_owned(),
            stage: ScanStage::ClipInfo,
            reason: BdError::UnexpectedEof,
        };
        assert_eq!(err.to_string(), "clipinfo 00001.CLPI: unexpected end of input");
        assert!(format!("{err:?}").contains("ClipInfo")); // derived Debug
    }

    #[test]
    fn is_a_std_error_and_debug() {
        // Usable as a boxed std::error::Error (the derived `impl Error` is exercised).
        let boxed: Box<dyn std::error::Error> =
            Box::new(BdError::UnknownFileType("HDMVZZZZ".to_owned()));
        assert_eq!(boxed.to_string(), "unknown file type: HDMVZZZZ");
        // Derived Debug.
        assert_eq!(format!("{:?}", BdError::UnexpectedEof), "UnexpectedEof");
    }
}
