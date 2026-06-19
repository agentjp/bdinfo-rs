//! File Entry / Extended File Entry (ICB) + allocation descriptors → extents.
//!
//! A file or directory is described by an Information Control Block: a File Entry
//! (ECMA-167 §4/14.9, tag 261) or — on UDF 2.50 media — an Extended File Entry
//! (§4/14.17, tag 266). Both carry an [`IcbTag`] (§4/14.6) whose `FileType`
//! distinguishes a directory (4) from a file (5), and whose flags select how the
//! data is addressed:
//!
//! - `short_ad` / `long_ad` / `ext_ad` allocation descriptors — the data lives in one or more
//!   [`Extent`]s (runs of logical blocks), parsed here into byte ranges.
//! - **Embedded** — the data is stored inline in the allocation-descriptor area itself (UDF uses
//!   this for small directories); exposed via [`FileEntry::embedded_data`].
//!
//! Resolving an extent's partition-relative block to an absolute sector needs the
//! logical-volume partition map ([`super::source`]) — this layer only decodes the
//! byte ranges. All fields are little-endian per ECMA-167 (see [`super`]).

use super::{
    EXTENT_LENGTH_MASK, Extent, ExtentKind, LbAddr, LongAd, ShortAd, TAG_EXTENDED_FILE_ENTRY,
    TAG_FILE_ENTRY, Tag, as_offset, u8_at, u16_le, u32_le, u64_le,
};

/// `FileType` value for a directory (ECMA-167 §4/14.6.6).
const FILE_TYPE_DIRECTORY: u8 = 4;
/// The ICB `StrategyType` whose direct entries are authoritative (ECMA-167
/// §4/14.6.2 strategy 4 — the flat tree every UDF disc records; UDF 2.50
/// §2.3.11). The only strategy [`FileEntry::parse`] accepts.
const ICB_STRATEGY_DIRECT: u16 = 4;
/// The fixed size in bytes of an `ext_ad` (ECMA-167 §4/14.14.3).
const EXT_AD_LEN: usize = 20;

/// How a File Entry's data is addressed — the low 3 bits of the ICB tag flags
/// (ECMA-167 §4/14.6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationType {
    /// `0` — `short_ad` allocation descriptors (partition implicit).
    Short,
    /// `1` — `long_ad` allocation descriptors (each names its partition).
    Long,
    /// `2` — `ext_ad` (extended) allocation descriptors.
    Extended,
    /// `3` — the data is embedded directly in the allocation-descriptor area.
    Embedded,
    /// `4..=7` — reserved by ECMA-167; treated as having no extents.
    Reserved,
}

impl AllocationType {
    /// Extracts the allocation type from the ICB tag flags (bits 0–2).
    const fn from_flags(flags: u16) -> Self {
        match flags & 0x7 {
            0 => Self::Short,
            1 => Self::Long,
            2 => Self::Extended,
            3 => Self::Embedded,
            _ => Self::Reserved,
        }
    }
}

/// An ICB tag (ECMA-167 §4/14.6) — the 20-byte block prefacing a File Entry's
/// body. Only the fields this reader needs are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IcbTag {
    /// `StrategyType` (offset 4) — how the ICB hierarchy is recorded (ECMA-167
    /// §4/14.6.2); UDF permits 4 and 4096 (UDF 2.50 §2.3.11).
    pub strategy_type: u16,
    /// `FileType` (offset 11) — 4 = directory, 5 = file (ECMA-167 §4/14.6.6).
    pub file_type: u8,
    /// The allocation-descriptor addressing mode (from the flags at offset 18).
    pub allocation_type: AllocationType,
}

impl IcbTag {
    /// Parses an [`IcbTag`] at `offset` in `buf`, or `None` if its
    /// `StrategyType` / `FileType` / flags are out of range.
    #[must_use]
    pub fn parse(buf: &[u8], offset: usize) -> Option<Self> {
        let strategy_type = u16_le(buf, offset.saturating_add(4))?;
        let file_type = u8_at(buf, offset.saturating_add(11))?;
        let flags = u16_le(buf, offset.saturating_add(18))?;
        Some(Self { strategy_type, file_type, allocation_type: AllocationType::from_flags(flags) })
    }
}

/// A File Entry's data — either a list of allocation [`Extent`]s or the inline
/// bytes of an embedded entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileData {
    /// Allocation extents (runs of logical blocks within a partition).
    Extents(Vec<Extent>),
    /// The data stored inline in the allocation-descriptor area (embedded entry).
    Embedded(Vec<u8>),
}

/// A File Entry (ECMA-167 §4/14.9) or Extended File Entry (§4/14.17) — a file's
/// size, type, and the location of its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// `InformationLength` — the data size in bytes.
    pub information_length: u64,
    /// The ICB tag (file type + allocation mode).
    pub icb_tag: IcbTag,
    /// The file's data (extents or embedded bytes).
    pub data: FileData,
}

impl FileEntry {
    /// Parses a [`FileEntry`] from its descriptor buffer, handling both the File
    /// Entry (tag 261) and Extended File Entry (tag 266) layouts.
    ///
    /// Returns `None` if the tag is neither, if the ICB strategy is not 4, or if
    /// the allocation-descriptor area (`176 + L_EA .. + L_AD` for FE,
    /// `216 + L_EA .. + L_AD` for EFE) runs past the buffer.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        let tag = Tag::parse(buf, 0)?;
        // (L_EA offset, L_AD offset, AD-area base) differ between FE and EFE; the
        // ICB tag (offset 16) and InformationLength (offset 56) are common.
        let (l_ea_off, l_ad_off, ad_base) = match tag.identifier {
            TAG_FILE_ENTRY => (168_usize, 172_usize, 176_usize),
            TAG_EXTENDED_FILE_ENTRY => (208_usize, 212_usize, 216_usize),
            _ => return None,
        };
        let icb_tag = IcbTag::parse(buf, 16)?;
        // Only strategy 4 — the flat direct-entry tree every UDF disc
        // records — is supported. Under any other strategy (4096's direct entry
        // heads an indirect-entry chain) this entry's data fields are not
        // authoritative, and reading them as final would return garbage where
        // libudfread fails loudly; rejecting matches libudfread exactly. The
        // hostile-input posture survives the hard reject: the directory walk
        // degrades a rejected entry to a skipped child / childless directory
        // rather than failing the scan.
        if icb_tag.strategy_type != ICB_STRATEGY_DIRECT {
            return None;
        }
        let information_length = u64_le(buf, 56)?;
        let l_ea = as_offset(u32_le(buf, l_ea_off)?);
        let l_ad = as_offset(u32_le(buf, l_ad_off)?);
        // Saturating offsets (the clpi/mpls house pattern): an out-of-range area
        // then fails the one bounds-checked slice below, the reachable EOF path.
        let ad_start = ad_base.saturating_add(l_ea);
        let ad_end = ad_start.saturating_add(l_ad);
        let ad_area = buf.get(ad_start..ad_end)?;
        let data = match icb_tag.allocation_type {
            AllocationType::Embedded => FileData::Embedded(ad_area.to_vec()),
            other => FileData::Extents(parse_allocation_area(ad_area, other)),
        };
        Some(Self { information_length, icb_tag, data })
    }

    /// Whether this entry is a directory (`FileType` 4).
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.icb_tag.file_type == FILE_TYPE_DIRECTORY
    }

    /// The file's allocation extents, or an empty slice for an embedded entry.
    #[must_use]
    pub fn extents(&self) -> &[Extent] {
        match &self.data {
            FileData::Extents(extents) => extents,
            FileData::Embedded(_) => &[],
        }
    }

    /// The inline data of an embedded entry, or `None` if the data is in extents.
    #[must_use]
    pub fn embedded_data(&self) -> Option<&[u8]> {
        match &self.data {
            FileData::Embedded(bytes) => Some(bytes),
            FileData::Extents(_) => None,
        }
    }
}

/// Reads a `short_ad` at `off` into an [`Extent`] (partition implicit).
fn short_extent(area: &[u8], off: usize) -> Option<Extent> {
    let ad = ShortAd::parse(area, off)?;
    Some(Extent {
        partition_ref: None,
        block: ad.position,
        length: ad.length_bytes(),
        kind: ad.kind(),
    })
}

/// Reads a `long_ad` at `off` into an [`Extent`] (carries its partition).
fn long_extent(area: &[u8], off: usize) -> Option<Extent> {
    let ad = LongAd::parse(area, off)?;
    Some(Extent {
        partition_ref: Some(ad.location.partition),
        block: ad.location.block,
        length: ad.length_bytes(),
        kind: ad.kind(),
    })
}

/// Reads an `ext_ad` at `off` into an [`Extent`] (ECMA-167 §4/14.14.3): the
/// `ExtentLength` is at offset 0 and the `lb_addr` location at offset 12.
fn ext_extent(area: &[u8], off: usize) -> Option<Extent> {
    let raw_length = u32_le(area, off)?;
    let location = LbAddr::parse(area, off.saturating_add(12))?;
    Some(Extent {
        partition_ref: Some(location.partition),
        block: location.block,
        length: raw_length & EXTENT_LENGTH_MASK,
        kind: ExtentKind::from_raw(raw_length),
    })
}

/// Parses a raw allocation-descriptor area into [`Extent`]s, picking the
/// descriptor form from `allocation_type`.
///
/// This is the dispatch [`FileEntry::parse`] uses for the File Entry's own AD area,
/// exposed so the integration layer ([`super`]) can also parse a **`NextExtent`
/// continuation** block (the area a
/// [`ExtentKind::NextExtent`] descriptor points at,
/// which extends the extent list, ECMA-167 §4/12.1). An [`Embedded`] / [`Reserved`]
/// type holds no extents.
///
/// [`Embedded`]: AllocationType::Embedded
/// [`Reserved`]: AllocationType::Reserved
#[must_use]
pub fn parse_allocation_area(area: &[u8], allocation_type: AllocationType) -> Vec<Extent> {
    match allocation_type {
        AllocationType::Short => parse_ads(area, ShortAd::LEN, short_extent),
        AllocationType::Long => parse_ads(area, LongAd::LEN, long_extent),
        AllocationType::Extended => parse_ads(area, EXT_AD_LEN, ext_extent),
        AllocationType::Embedded | AllocationType::Reserved => Vec::new(),
    }
}

/// Walks an allocation-descriptor area: reads fixed-size descriptors with `read`,
/// stopping at the end of `area` or a zero-length descriptor (the list
/// terminator, ECMA-167 §4/12.1). `step` saturates, so a (never-reached for real
/// extents) cursor overflow simply ends the walk via the next short read.
fn parse_ads(
    area: &[u8],
    step: usize,
    read: impl Fn(&[u8], usize) -> Option<Extent>,
) -> Vec<Extent> {
    let mut extents = Vec::new();
    let mut off: usize = 0;
    while let Some(extent) = read(area, off) {
        if extent.length == 0 {
            break;
        }
        extents.push(extent);
        off = off.saturating_add(step);
    }
    extents
}

#[cfg(test)]
mod tests {
    use super::{AllocationType, FileData, FileEntry, IcbTag};
    use crate::vfs::udf::{Extent, ExtentKind};

    /// Writes `bytes` at `off` (branchless — adds no uncovered region).
    fn put(buf: &mut [u8], off: usize, bytes: &[u8]) {
        for (dst, &src) in buf.iter_mut().skip(off).zip(bytes) {
            *dst = src;
        }
    }

    /// Builds a `size`-byte File Entry buffer of tag `identifier` with the given
    /// ICB `file_type` and allocation-type `flags` (low 3 bits).
    fn file_entry(size: usize, identifier: u16, file_type: u8, flags: u16) -> Vec<u8> {
        let mut buf = vec![0_u8; size];
        put(&mut buf, 0, &identifier.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        // ICB tag is at offset 16: StrategyType at 16+4=20, FileType at
        // 16+11=27, flags at 16+18=34.
        put(&mut buf, 20, &4_u16.to_le_bytes());
        put(&mut buf, 27, &[file_type]);
        put(&mut buf, 34, &flags.to_le_bytes());
        // Tag checksum over the 16 tag bytes excluding byte 4.
        let sum = buf
            .iter()
            .take(16)
            .enumerate()
            .filter(|&(i, _)| i != 4)
            .fold(0_u8, |acc, (_, &b)| acc.wrapping_add(b));
        put(&mut buf, 4, &[sum]);
        buf
    }

    #[test]
    fn icb_tag_decodes_each_allocation_type() {
        for (flags, expected) in [
            (0_u16, AllocationType::Short),
            (1, AllocationType::Long),
            (2, AllocationType::Extended),
            (3, AllocationType::Embedded),
            (4, AllocationType::Reserved),
            // High bits above the low 3 are ignored.
            (0x0108, AllocationType::Short),
        ] {
            let buf = file_entry(64, super::TAG_FILE_ENTRY, 5, flags);
            let icb = IcbTag::parse(&buf, 16).expect("icb tag");
            assert_eq!(icb.allocation_type, expected);
            assert_eq!(icb.file_type, 5);
            assert_eq!(icb.strategy_type, 4);
        }
    }

    #[test]
    fn icb_tag_reads_any_strategy_type() {
        // The tag parser itself stays a pure reader — the strategy gate lives
        // in FileEntry::parse.
        let mut buf = file_entry(64, super::TAG_FILE_ENTRY, 5, 0);
        put(&mut buf, 20, &4096_u16.to_le_bytes());
        assert_eq!(IcbTag::parse(&buf, 16).expect("icb tag").strategy_type, 4096);
    }

    #[test]
    fn file_entry_rejects_a_non_direct_strategy() {
        // Strategy 4096 (an indirect-entry chain) and every other non-4 value
        // make the direct entry non-authoritative → rejected, exactly as
        // libudfread does. The same bytes with strategy 4 parse.
        let good = file_entry(256, super::TAG_FILE_ENTRY, 5, 0);
        assert!(FileEntry::parse(&good).is_some());
        for strategy in [0_u16, 1, 4096, u16::MAX] {
            let mut buf = good.clone();
            put(&mut buf, 20, &strategy.to_le_bytes());
            assert_eq!(FileEntry::parse(&buf), None, "strategy {strategy}");
        }
    }

    #[test]
    fn icb_tag_short_buffer_is_none() {
        let buf = file_entry(20, super::TAG_FILE_ENTRY, 4, 0);
        // Offsets 27/34 are past a 20-byte buffer.
        assert_eq!(IcbTag::parse(&buf, 16), None);
    }

    #[test]
    fn file_entry_short_ad_extents_with_terminator() {
        // FE, directory, short_ad. L_EA=0, L_AD=24 (three 8-byte short_ads:
        // two real extents then a zero-length terminator).
        let mut buf = file_entry(256, super::TAG_FILE_ENTRY, 4, 0);
        put(&mut buf, 56, &4096_u64.to_le_bytes()); // InformationLength
        put(&mut buf, 168, &0_u32.to_le_bytes()); // L_EA
        put(&mut buf, 172, &24_u32.to_le_bytes()); // L_AD
        // AD area at 176: extent1 (len 2048, block 0x10), extent2 (len 2048,
        // block 0x11), terminator (len 0).
        put(&mut buf, 176, &0x800_u32.to_le_bytes());
        put(&mut buf, 180, &0x10_u32.to_le_bytes());
        put(&mut buf, 184, &0x800_u32.to_le_bytes());
        put(&mut buf, 188, &0x11_u32.to_le_bytes());
        // 192..200 stays zero → zero-length terminator.

        let fe = FileEntry::parse(&buf).expect("file entry");
        assert!(fe.is_directory());
        assert_eq!(fe.information_length, 4096);
        assert_eq!(fe.embedded_data(), None);
        assert_eq!(
            fe.extents(),
            &[
                Extent {
                    partition_ref: None,
                    block: 0x10,
                    length: 0x800,
                    kind: ExtentKind::RecordedAllocated
                },
                Extent {
                    partition_ref: None,
                    block: 0x11,
                    length: 0x800,
                    kind: ExtentKind::RecordedAllocated
                },
            ]
        );
    }

    #[test]
    fn file_entry_short_ad_stops_at_area_end_without_terminator() {
        // L_AD spans exactly one short_ad — the walk ends by exhausting the area.
        let mut buf = file_entry(256, super::TAG_FILE_ENTRY, 5, 0);
        put(&mut buf, 172, &8_u32.to_le_bytes());
        put(&mut buf, 176, &0x1000_u32.to_le_bytes());
        put(&mut buf, 180, &0x20_u32.to_le_bytes());
        let fe = FileEntry::parse(&buf).expect("file entry");
        assert!(!fe.is_directory());
        assert_eq!(
            fe.extents(),
            &[Extent {
                partition_ref: None,
                block: 0x20,
                length: 0x1000,
                kind: ExtentKind::RecordedAllocated
            }]
        );
    }

    #[test]
    fn file_entry_long_ad_extents_carry_partition() {
        // FE with long_ad (flags=1): a single extent in partition 2.
        let mut buf = file_entry(256, super::TAG_FILE_ENTRY, 5, 1);
        put(&mut buf, 172, &16_u32.to_le_bytes()); // L_AD = one long_ad
        put(&mut buf, 176, &0x800_u32.to_le_bytes()); // length
        put(&mut buf, 180, &0x30_u32.to_le_bytes()); // block
        put(&mut buf, 184, &2_u16.to_le_bytes()); // partition ref 2
        let fe = FileEntry::parse(&buf).expect("file entry");
        assert_eq!(
            fe.extents(),
            &[Extent {
                partition_ref: Some(2),
                block: 0x30,
                length: 0x800,
                kind: ExtentKind::RecordedAllocated
            }]
        );
    }

    #[test]
    fn file_entry_ext_ad_extents() {
        // FE with ext_ad (flags=2): a single 20-byte extended descriptor.
        let mut buf = file_entry(256, super::TAG_FILE_ENTRY, 5, 2);
        put(&mut buf, 172, &20_u32.to_le_bytes()); // L_AD = one ext_ad
        put(&mut buf, 176, &0x800_u32.to_le_bytes()); // ExtentLength
        // RecordedLength (180), InformationLength (184) ignored.
        put(&mut buf, 188, &0x44_u32.to_le_bytes()); // lb_addr.block (offset 12)
        put(&mut buf, 192, &1_u16.to_le_bytes()); // lb_addr.partition
        let fe = FileEntry::parse(&buf).expect("file entry");
        assert_eq!(
            fe.extents(),
            &[Extent {
                partition_ref: Some(1),
                block: 0x44,
                length: 0x800,
                kind: ExtentKind::RecordedAllocated
            }]
        );
    }

    #[test]
    fn file_entry_embedded_exposes_inline_bytes() {
        // FE, directory, embedded (flags=3): L_AD bytes are the directory data.
        let mut buf = file_entry(256, super::TAG_FILE_ENTRY, 4, 3);
        put(&mut buf, 172, &5_u32.to_le_bytes()); // L_AD = 5 inline bytes
        put(&mut buf, 176, &[0xDE, 0xAD, 0xBE, 0xEF, 0x42]);
        let fe = FileEntry::parse(&buf).expect("file entry");
        assert!(fe.is_directory());
        assert_eq!(fe.embedded_data(), Some([0xDE_u8, 0xAD, 0xBE, 0xEF, 0x42].as_slice()));
        assert_eq!(fe.extents(), &[]);
    }

    #[test]
    fn file_entry_reserved_allocation_type_has_no_extents() {
        let mut buf = file_entry(256, super::TAG_FILE_ENTRY, 5, 4);
        put(&mut buf, 172, &8_u32.to_le_bytes());
        let fe = FileEntry::parse(&buf).expect("file entry");
        assert_eq!(fe.extents(), &[]);
        assert_eq!(fe.embedded_data(), None);
    }

    #[test]
    fn extended_file_entry_uses_its_own_offsets() {
        // EFE (tag 266): InformationLength at 56, L_EA at 208, L_AD at 212, AD
        // area at 216. A single short_ad with an L_EA gap before it.
        let mut buf = file_entry(512, super::TAG_EXTENDED_FILE_ENTRY, 5, 0);
        put(&mut buf, 56, &9000_u64.to_le_bytes());
        put(&mut buf, 208, &4_u32.to_le_bytes()); // L_EA = 4 (skipped)
        put(&mut buf, 212, &8_u32.to_le_bytes()); // L_AD = one short_ad
        put(&mut buf, 220, &0x900_u32.to_le_bytes()); // AD at 216+4=220
        put(&mut buf, 224, &0x55_u32.to_le_bytes());
        let fe = FileEntry::parse(&buf).expect("efe");
        assert_eq!(fe.information_length, 9000);
        assert_eq!(
            fe.extents(),
            &[Extent {
                partition_ref: None,
                block: 0x55,
                length: 0x900,
                kind: ExtentKind::RecordedAllocated
            }]
        );
    }

    #[test]
    fn next_extent_descriptor_is_kept_with_its_kind() {
        // A short_ad whose top 2 length bits mark it as a NextExtent continuation
        // is retained (non-zero length) so the integration layer can follow it.
        let mut buf = file_entry(256, super::TAG_FILE_ENTRY, 4, 0);
        put(&mut buf, 172, &8_u32.to_le_bytes());
        put(&mut buf, 176, &0xC000_0800_u32.to_le_bytes()); // kind=3, len 0x800
        put(&mut buf, 180, &0x70_u32.to_le_bytes());
        let fe = FileEntry::parse(&buf).expect("file entry");
        assert_eq!(
            fe.extents(),
            &[Extent {
                partition_ref: None,
                block: 0x70,
                length: 0x800,
                kind: ExtentKind::NextExtent
            }]
        );
    }

    #[test]
    fn file_entry_ext_ad_truncated_descriptor_yields_no_extents() {
        // An ext_ad needs 20 bytes; an L_AD of 14 leaves its lb_addr (at offset
        // 12 of the descriptor) out of range, so it is dropped — no extents.
        let mut buf = file_entry(256, super::TAG_FILE_ENTRY, 5, 2);
        put(&mut buf, 172, &14_u32.to_le_bytes());
        let fe = FileEntry::parse(&buf).expect("file entry");
        assert_eq!(fe.extents(), &[]);
    }

    #[test]
    fn file_entry_none_for_all_truncations() {
        // A minimal FE (no extended attributes, no allocation descriptors) parses
        // at 176 bytes; every shorter buffer fails one field read, exercising each
        // `?` arm (tag, ICB tag, InformationLength, L_EA, L_AD).
        let full = file_entry(176, super::TAG_FILE_ENTRY, 4, 0);
        assert!(FileEntry::parse(&full).is_some());
        for len in 0..176 {
            assert_eq!(FileEntry::parse(full.get(..len).unwrap_or_default()), None, "fe {len}");
        }
    }

    #[test]
    fn file_entry_rejects_wrong_tag() {
        let buf = file_entry(256, crate::vfs::udf::TAG_FILE_IDENTIFIER, 5, 0);
        assert_eq!(FileEntry::parse(&buf), None);
    }

    #[test]
    fn file_entry_rejects_ad_area_past_buffer() {
        // L_AD claims more bytes than the buffer holds → None.
        let mut buf = file_entry(180, super::TAG_FILE_ENTRY, 5, 0);
        put(&mut buf, 172, &1000_u32.to_le_bytes());
        assert_eq!(FileEntry::parse(&buf), None);
    }

    #[test]
    fn file_entry_short_buffer_before_icb_is_none() {
        let buf = file_entry(20, super::TAG_FILE_ENTRY, 5, 0);
        assert_eq!(FileEntry::parse(&buf), None);
    }

    #[test]
    fn parse_allocation_area_handles_each_form() {
        use super::parse_allocation_area;
        use crate::vfs::udf::ExtentKind;

        // A short_ad continuation: one real extent then a zero-length terminator.
        let mut short = Vec::new();
        short.extend_from_slice(&0x800_u32.to_le_bytes()); // len 2048
        short.extend_from_slice(&0x90_u32.to_le_bytes()); // block 0x90
        short.extend_from_slice(&[0_u8; 8]); // terminator
        assert_eq!(
            parse_allocation_area(&short, AllocationType::Short),
            vec![Extent {
                partition_ref: None,
                block: 0x90,
                length: 0x800,
                kind: ExtentKind::RecordedAllocated
            }]
        );

        // A long_ad continuation carries its partition reference.
        let mut long = Vec::new();
        long.extend_from_slice(&0x800_u32.to_le_bytes());
        long.extend_from_slice(&0x10_u32.to_le_bytes()); // block
        long.extend_from_slice(&3_u16.to_le_bytes()); // partition ref 3
        long.extend_from_slice(&[0_u8; 6]); // ImplementationUse
        assert_eq!(
            parse_allocation_area(&long, AllocationType::Long).first().map(|e| e.partition_ref),
            Some(Some(3))
        );

        // An ext_ad continuation (20-byte descriptors).
        let mut ext = vec![0_u8; 20];
        put(&mut ext, 0, &0x800_u32.to_le_bytes());
        put(&mut ext, 12, &0x44_u32.to_le_bytes()); // lb_addr.block
        put(&mut ext, 16, &1_u16.to_le_bytes()); // lb_addr.partition
        assert_eq!(parse_allocation_area(&ext, AllocationType::Extended).len(), 1);

        // Embedded / Reserved areas yield no extents.
        assert!(parse_allocation_area(&short, AllocationType::Embedded).is_empty());
        assert!(parse_allocation_area(&short, AllocationType::Reserved).is_empty());
    }

    #[test]
    fn file_data_variants_are_equatable() {
        // Exercise the derived PartialEq on both FileData arms.
        assert_eq!(FileData::Embedded(vec![1, 2]), FileData::Embedded(vec![1, 2]));
        assert_ne!(FileData::Embedded(vec![1]), FileData::Extents(vec![]));
    }
}
