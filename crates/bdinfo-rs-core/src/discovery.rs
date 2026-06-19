//! Blu-ray folder-structure discovery primitives.
//!
//! The BD spec mandates uppercase directory and file names (`BDMV`, `PLAYLIST`,
//! `00000.MPLS` …), but real-world rips and case-sensitive filesystems
//! (Linux/macOS) vary. Ad-hoc "try `*.mpls`, then `*.MPLS`" double-glob
//! fallbacks are a recurring source of bugs, so here we match
//! **ASCII case-insensitively, once, correctly**.

/// A recognized directory within a Blu-ray disc structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BdmvDir {
    /// `BDMV` — the root of the Blu-ray structure.
    Bdmv,
    /// `BACKUP` — the duplicate copies (`index.bdmv`, `PLAYLIST/`, `CLIPINF/`,
    /// …) the resilient scan falls back to when a primary read fails.
    Backup,
    /// `PLAYLIST` — `*.mpls` playlist files.
    Playlist,
    /// `CLIPINF` — `*.clpi` clip-information files.
    ClipInf,
    /// `STREAM` — `*.m2ts` transport-stream files.
    Stream,
    /// `STREAM/SSIF` — `*.ssif` interleaved files (3D Blu-ray).
    Ssif,
    /// `META` — disc metadata (e.g. `bdmt_*.xml` title).
    Meta,
    /// `BDJO` — BD-Java objects.
    BdJo,
    /// `SNP` — PSP / mobile content (`*.mnv`).
    Snp,
}

impl BdmvDir {
    /// Classifies a directory `name` (ASCII case-insensitive), or `None` if it
    /// isn't a recognized Blu-ray directory.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        const TABLE: [(&str, BdmvDir); 9] = [
            ("BDMV", BdmvDir::Bdmv),
            ("BACKUP", BdmvDir::Backup),
            ("PLAYLIST", BdmvDir::Playlist),
            ("CLIPINF", BdmvDir::ClipInf),
            ("STREAM", BdmvDir::Stream),
            ("SSIF", BdmvDir::Ssif),
            ("META", BdmvDir::Meta),
            ("BDJO", BdmvDir::BdJo),
            ("SNP", BdmvDir::Snp),
        ];
        TABLE
            .iter()
            .find(|(candidate, _)| name.eq_ignore_ascii_case(candidate))
            .map(|&(_, dir)| dir)
    }
}

/// A recognized file kind within a Blu-ray disc structure, by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BdFileKind {
    /// `*.mpls` — a movie playlist.
    Playlist,
    /// A BDAV MPEG-2 transport stream — `*.m2ts`, or the `*.fmts` variant some
    /// 4K/UHD-era authorings reference through an `FMTS` play-item codec id
    /// (same 192-byte transport layout; only the extension differs).
    Stream,
    /// `*.clpi` — clip information for a stream.
    ClipInfo,
    /// `*.ssif` — an interleaved (3D) stream.
    Interleaved,
}

impl BdFileKind {
    /// Classifies a `filename` by its extension (ASCII case-insensitive), or
    /// `None` if the extension isn't recognized (or the name has no extension).
    #[must_use]
    pub fn from_filename(filename: &str) -> Option<Self> {
        const TABLE: [(&str, BdFileKind); 5] = [
            ("mpls", BdFileKind::Playlist),
            ("m2ts", BdFileKind::Stream),
            ("fmts", BdFileKind::Stream),
            ("clpi", BdFileKind::ClipInfo),
            ("ssif", BdFileKind::Interleaved),
        ];
        let (_, ext) = filename.rsplit_once('.')?;
        TABLE
            .iter()
            .find(|(candidate, _)| ext.eq_ignore_ascii_case(candidate))
            .map(|&(_, kind)| kind)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{prop_assert_eq, proptest};

    use super::{BdFileKind, BdmvDir};

    #[test]
    fn dir_classification_is_case_insensitive() {
        assert_eq!(BdmvDir::from_name("BDMV"), Some(BdmvDir::Bdmv));
        assert_eq!(BdmvDir::from_name("bdmv"), Some(BdmvDir::Bdmv));
        assert_eq!(BdmvDir::from_name("BACKUP"), Some(BdmvDir::Backup));
        assert_eq!(BdmvDir::from_name("backup"), Some(BdmvDir::Backup));
        assert_eq!(BdmvDir::from_name("PlayList"), Some(BdmvDir::Playlist));
        assert_eq!(BdmvDir::from_name("clipinf"), Some(BdmvDir::ClipInf));
        assert_eq!(BdmvDir::from_name("STREAM"), Some(BdmvDir::Stream));
        assert_eq!(BdmvDir::from_name("ssif"), Some(BdmvDir::Ssif));
        assert_eq!(BdmvDir::from_name("META"), Some(BdmvDir::Meta));
        assert_eq!(BdmvDir::from_name("bdjo"), Some(BdmvDir::BdJo));
        assert_eq!(BdmvDir::from_name("SNP"), Some(BdmvDir::Snp));
    }

    #[test]
    fn dir_classification_rejects_unknown() {
        assert_eq!(BdmvDir::from_name("CERTIFICATE"), None);
        assert_eq!(BdmvDir::from_name(""), None);
    }

    #[test]
    fn file_classification_is_case_insensitive() {
        assert_eq!(BdFileKind::from_filename("00000.mpls"), Some(BdFileKind::Playlist));
        assert_eq!(BdFileKind::from_filename("00000.MPLS"), Some(BdFileKind::Playlist));
        assert_eq!(BdFileKind::from_filename("00001.m2ts"), Some(BdFileKind::Stream));
        assert_eq!(BdFileKind::from_filename("00001.fmts"), Some(BdFileKind::Stream));
        assert_eq!(BdFileKind::from_filename("00001.FMTS"), Some(BdFileKind::Stream));
        assert_eq!(BdFileKind::from_filename("00001.CLPI"), Some(BdFileKind::ClipInfo));
        assert_eq!(BdFileKind::from_filename("00002.ssif"), Some(BdFileKind::Interleaved));
    }

    #[test]
    fn file_classification_rejects_unknown_and_extensionless() {
        assert_eq!(BdFileKind::from_filename("index.bdmv"), None);
        assert_eq!(BdFileKind::from_filename("README"), None);
        assert_eq!(BdFileKind::from_filename(""), None);
        // Honors the last extension on a multi-dot name.
        assert_eq!(BdFileKind::from_filename("a.b.m2ts"), Some(BdFileKind::Stream));
    }

    #[test]
    fn enums_are_debug_and_eq() {
        // Exercise the derived Debug + PartialEq so coverage sees them.
        assert_eq!(format!("{:?}", BdmvDir::Bdmv), "Bdmv");
        assert_eq!(format!("{:?}", BdFileKind::Stream), "Stream");
        assert_ne!(BdmvDir::Bdmv, BdmvDir::Stream);
    }

    proptest! {
        #[test]
        fn dir_classification_ignores_case(name in "[A-Za-z]{1,10}") {
            prop_assert_eq!(
                BdmvDir::from_name(&name.to_ascii_uppercase()),
                BdmvDir::from_name(&name.to_ascii_lowercase())
            );
        }

        #[test]
        fn file_classification_ignores_case(stem in "[0-9]{5}", ext in "[A-Za-z]{1,4}") {
            let upper = format!("{stem}.{}", ext.to_ascii_uppercase());
            let lower = format!("{stem}.{}", ext.to_ascii_lowercase());
            prop_assert_eq!(BdFileKind::from_filename(&upper), BdFileKind::from_filename(&lower));
        }
    }
}
