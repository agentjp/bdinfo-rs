//! Read-only UDF 2.50 / ECMA-167 on-disc structure parsing — the `.iso` backend.
//!
//! A Blu-ray `.iso` is a UDF (Universal Disk Format) image; reading the BDMV
//! files out of it means parsing the UDF filesystem. This module is a
//! from-scratch, `#![forbid(unsafe_code)]`, **no-C-deps** implementation of the
//! subset a Blu-ray needs:
//!
//! - [`Tag`] — the 16-byte descriptor tag prefixing every structure (ECMA-167 §3/7.2), with
//!   checksum validation.
//! - [`ExtentAd`], [`LbAddr`], [`LongAd`], [`ShortAd`] — the address/extent primitives (ECMA-167
//!   §3/7.1, §4/7.1, §4/14.14.1–2).
//! - [`descriptor`] — AVDP, Logical Volume Descriptor, Partition Descriptor(s), the UDF 2.50
//!   Metadata partition map, and the File Set Descriptor.
//! - [`icb`] — File Entry / Extended File Entry + allocation descriptors → byte ranges
//!   ([`Extent`]).
//! - [`fid`] — File Identifier Descriptors → directory enumeration + OSTA CS0 name decoding
//!   ([`cs0`]).
//!
//! Reference: ECMA-167 (3rd edition) + the OSTA UDF 2.50/2.60 specification.
//! Spec section numbers cite ECMA-167 unless noted as UDF.
//!
//! ## Endianness — the deliberate exception
//!
//! Every Blu-ray / M2TS structure this crate reads is **big-endian** (the
//! `compliance.ps1` source-rule guard enforces `from_be_bytes` only). UDF is the
//! **sole exception**: ECMA-167 numeric fields are stored **little-endian**, so
//! this module reads them with [`u16::from_le_bytes`] etc. That is intentional
//! and spec-mandated, and the endianness guard carves out the `udf` path. Each
//! field below is annotated with its spec-defined width; the few fields that are
//! themselves byte arrays (the OSTA identifiers, the checksum) have no
//! endianness.
//!
//! ## Untrusted input
//!
//! `.iso` bytes are untrusted, so — like every parser in this crate — these
//! functions never `panic`, `unwrap`, or raw-index disc bytes: a short read or a
//! structurally invalid descriptor yields `None`, and the caller decides whether
//! that is benign or corruption.

pub mod cs0;
pub mod descriptor;
pub mod fid;
pub mod icb;
pub mod source;

// ---------------------------------------------------------------------------
// Little-endian readers — the ECMA-167 exception (see the module-level
// "Endianness" note). These mirror the big-endian
// `crate::bytes` readers but assemble via `from_le_bytes`; they are
// module-private (visible to the `udf` submodules as descendants) so the
// little-endian surface stays sealed inside the UDF reader.
// ---------------------------------------------------------------------------

/// Reads the byte at `off`, or `None` past the end of `buf`.
fn u8_at(buf: &[u8], off: usize) -> Option<u8> {
    buf.get(off).copied()
}

/// Reads a little-endian `u16` at `off`, or `None` if the two bytes don't fit.
fn u16_le(buf: &[u8], off: usize) -> Option<u16> {
    let chunk = buf.get(off..)?.first_chunk::<2>()?;
    Some(u16::from_le_bytes(*chunk))
}

/// Reads a little-endian `u32` at `off`, or `None` if the four bytes don't fit.
fn u32_le(buf: &[u8], off: usize) -> Option<u32> {
    let chunk = buf.get(off..)?.first_chunk::<4>()?;
    Some(u32::from_le_bytes(*chunk))
}

/// Reads a little-endian `u64` at `off`, or `None` if the eight bytes don't fit.
fn u64_le(buf: &[u8], off: usize) -> Option<u64> {
    let chunk = buf.get(off..)?.first_chunk::<8>()?;
    Some(u64::from_le_bytes(*chunk))
}

/// Converts a UDF `u32` block/sector number into a `usize` byte-buffer offset,
/// saturating to [`usize::MAX`] on a `<32`-bit target (where the next
/// bounds-checked read then fails) — mirroring `crate::bdrom::u32_off`.
fn as_offset(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

// ---------------------------------------------------------------------------
// Descriptor tag (ECMA-167 §3/7.2)
// ---------------------------------------------------------------------------

/// The fixed size in bytes of a [`Tag`] (ECMA-167 §3/7.2).
const TAG_LEN: usize = 16;

/// Tag identifier — Primary Volume Descriptor (ECMA-167 §3/7.2.1, PVD).
pub const TAG_PRIMARY_VOLUME: u16 = 1;
/// Tag identifier — Anchor Volume Descriptor Pointer (ECMA-167 §3/7.2.1, AVDP).
pub const TAG_ANCHOR_VOLUME_POINTER: u16 = 2;
/// Tag identifier — Volume Descriptor Pointer (ECMA-167 §3/7.2.1, VDP);
/// continues a volume descriptor sequence in another extent.
pub const TAG_VOLUME_DESCRIPTOR_POINTER: u16 = 3;
/// Tag identifier — Partition Descriptor (ECMA-167 §3/7.2.1).
pub const TAG_PARTITION: u16 = 5;
/// Tag identifier — Logical Volume Descriptor (ECMA-167 §3/7.2.1).
pub const TAG_LOGICAL_VOLUME: u16 = 6;
/// Tag identifier — Terminating Descriptor (ECMA-167 §3/7.2.1); ends a volume
/// descriptor sequence.
pub const TAG_TERMINATING: u16 = 8;
/// Tag identifier — File Set Descriptor (ECMA-167 §4/7.2.1, FSD).
pub const TAG_FILE_SET: u16 = 256;
/// Tag identifier — File Identifier Descriptor (ECMA-167 §4/7.2.1, FID).
pub const TAG_FILE_IDENTIFIER: u16 = 257;
/// Tag identifier — Allocation Extent Descriptor (ECMA-167 §4/7.2.1, AED); the
/// 24-byte header a `NextExtent` continuation block must begin with (§4/14.5).
pub const TAG_ALLOCATION_EXTENT: u16 = 258;
/// Tag identifier — File Entry (ECMA-167 §4/7.2.1, FE).
pub const TAG_FILE_ENTRY: u16 = 261;
/// Tag identifier — Extended File Entry (ECMA-167 §4/7.2.1, EFE; UDF 2.50).
pub const TAG_EXTENDED_FILE_ENTRY: u16 = 266;

/// A descriptor tag — the 16-byte header prefixing every UDF descriptor
/// (ECMA-167 §3/7.2).
///
/// Only the [`identifier`](Tag::identifier) is retained: descriptor parsers
/// dispatch on it. The 8-bit `TagChecksum` (the sum of the other 15 tag bytes,
/// modulo 256) is **validated** during [`parse`](Tag::parse) — a mismatch yields
/// `None`, rejecting bytes that do not begin a real descriptor. (The
/// `DescriptorCRC` over the body is not checked: it adds a CRC-CCITT table for no
/// gain in a read-only parser whose every downstream field is itself
/// bounds-checked.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tag {
    /// The `TagIdentifier` (offset 0, `Uint16`) — e.g. [`TAG_FILE_ENTRY`].
    pub identifier: u16,
}

impl Tag {
    /// Parses a [`Tag`] at `offset` in `buf`, validating the tag checksum.
    ///
    /// Returns `None` if 16 bytes are not available, or if the stored
    /// `TagChecksum` (byte 4) does not equal the modulo-256 sum of the other
    /// fifteen tag bytes (ECMA-167 §3/7.2.3) — i.e. the bytes are not a tag.
    #[must_use]
    pub fn parse(buf: &[u8], offset: usize) -> Option<Self> {
        // Read the identifier first (a 0/1-byte buffer fails here), then take the
        // full 16-byte tag for the checksum (a 2..=15-byte buffer fails there);
        // ordering the two reachable short-reads this way leaves no dead path.
        let identifier = u16_le(buf, offset)?;
        let bytes = buf.get(offset..offset.saturating_add(TAG_LEN))?;
        // Sum every tag byte except the checksum field itself (byte 4), modulo
        // 256 — wrapping_add gives the spec's modular byte accumulation.
        let mut sum: u8 = 0;
        let mut stored: u8 = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if i == 4 {
                stored = b;
            } else {
                sum = sum.wrapping_add(b);
            }
        }
        if sum != stored {
            return None;
        }
        Some(Self { identifier })
    }
}

// ---------------------------------------------------------------------------
// Extent / address primitives
// ---------------------------------------------------------------------------

/// An `extent_ad` — a length + an **absolute logical-sector** location
/// (ECMA-167 §3/7.1). Used by volume-level descriptors (e.g. the AVDP's pointers
/// to the volume descriptor sequence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentAd {
    /// `ExtentLength` (offset 0, `Uint32`) — length in **bytes**.
    pub length: u32,
    /// `ExtentLocation` (offset 4, `Uint32`) — logical sector number.
    pub location: u32,
}

impl ExtentAd {
    /// The fixed size in bytes of an `extent_ad` (ECMA-167 §3/7.1).
    pub const LEN: usize = 8;

    /// Parses an [`ExtentAd`] at `offset` in `buf`, or `None` if 8 bytes don't
    /// fit.
    #[must_use]
    pub fn parse(buf: &[u8], offset: usize) -> Option<Self> {
        let length = u32_le(buf, offset)?;
        let location = u32_le(buf, offset.saturating_add(4))?;
        Some(Self { length, location })
    }
}

/// An `lb_addr` — a logical block number within a partition (ECMA-167 §4/7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LbAddr {
    /// `LogicalBlockNumber` (offset 0, `Uint32`) — block within the partition.
    pub block: u32,
    /// `PartitionReferenceNumber` (offset 4, `Uint16`) — index into the logical
    /// volume's partition map table.
    pub partition: u16,
}

impl LbAddr {
    /// The fixed size in bytes of an `lb_addr` (ECMA-167 §4/7.1).
    pub const LEN: usize = 6;

    /// Parses an [`LbAddr`] at `offset` in `buf`, or `None` if 6 bytes don't fit.
    #[must_use]
    pub fn parse(buf: &[u8], offset: usize) -> Option<Self> {
        let block = u32_le(buf, offset)?;
        let partition = u16_le(buf, offset.saturating_add(4))?;
        Some(Self { block, partition })
    }
}

/// The mask selecting the length-in-bytes (low 30 bits) of an allocation
/// descriptor's `ExtentLength` (ECMA-167 §4/14.14.1.1).
const EXTENT_LENGTH_MASK: u32 = 0x3FFF_FFFF;
/// The shift selecting the 2-bit extent type from an `ExtentLength`.
const EXTENT_TYPE_SHIFT: u32 = 30;

/// The type of an allocation extent, from the top 2 bits of an `ExtentLength`
/// (ECMA-167 §4/14.14.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtentKind {
    /// `0` — the extent is recorded and allocated (holds real data).
    RecordedAllocated,
    /// `1` — allocated but not recorded (sparse; reads as zero).
    NotRecordedAllocated,
    /// `2` — neither recorded nor allocated (a hole).
    NotRecordedNotAllocated,
    /// `3` — the extent is the next extent of allocation descriptors (a
    /// continuation of the allocation-descriptor list, not file data).
    NextExtent,
}

impl ExtentKind {
    /// Extracts the extent type from a raw `ExtentLength` (its top 2 bits).
    #[must_use]
    pub const fn from_raw(raw_length: u32) -> Self {
        match raw_length.wrapping_shr(EXTENT_TYPE_SHIFT) {
            0 => Self::RecordedAllocated,
            1 => Self::NotRecordedAllocated,
            2 => Self::NotRecordedNotAllocated,
            // `u32 >> 30` is in `0..=3`, so the only remaining value is 3.
            _ => Self::NextExtent,
        }
    }
}

/// A `long_ad` — a length + an [`LbAddr`] location (ECMA-167 §4/14.14.2).
///
/// Used where a descriptor must name a block in a possibly-different partition
/// (the FSD root-directory ICB, FID ICBs, and long-form file allocation
/// descriptors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongAd {
    /// The raw `ExtentLength` (offset 0, `Uint32`): the top 2 bits are the
    /// [`ExtentKind`], the low 30 bits the length in bytes. Use
    /// [`length_bytes`](LongAd::length_bytes) / [`kind`](LongAd::kind).
    pub raw_length: u32,
    /// `ExtentLocation` (offset 4, `lb_addr`) — block + partition reference.
    pub location: LbAddr,
}

impl LongAd {
    /// The fixed size in bytes of a `long_ad` (ECMA-167 §4/14.14.2): 4-byte
    /// length + 6-byte `lb_addr` + 6 bytes of `ImplementationUse`.
    pub const LEN: usize = 16;

    /// Parses a [`LongAd`] at `offset` in `buf`, or `None` if the length +
    /// `lb_addr` don't fit. (The trailing 6-byte `ImplementationUse` is ignored.)
    #[must_use]
    pub fn parse(buf: &[u8], offset: usize) -> Option<Self> {
        let raw_length = u32_le(buf, offset)?;
        let location = LbAddr::parse(buf, offset.saturating_add(4))?;
        Some(Self { raw_length, location })
    }

    /// The extent length in bytes (the low 30 bits of `ExtentLength`).
    #[must_use]
    pub const fn length_bytes(&self) -> u32 {
        self.raw_length & EXTENT_LENGTH_MASK
    }

    /// The extent type (the top 2 bits of `ExtentLength`).
    #[must_use]
    pub const fn kind(&self) -> ExtentKind {
        ExtentKind::from_raw(self.raw_length)
    }
}

/// A `short_ad` — a length + a partition-relative block position (ECMA-167
/// §4/14.14.1). Used for short-form file allocation descriptors, where the
/// partition is implicitly the file entry's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortAd {
    /// The raw `ExtentLength` (offset 0, `Uint32`): top 2 bits [`ExtentKind`],
    /// low 30 bits length in bytes.
    pub raw_length: u32,
    /// `ExtentPosition` (offset 4, `Uint32`) — logical block within the partition.
    pub position: u32,
}

impl ShortAd {
    /// The fixed size in bytes of a `short_ad` (ECMA-167 §4/14.14.1).
    pub const LEN: usize = 8;

    /// Parses a [`ShortAd`] at `offset` in `buf`, or `None` if 8 bytes don't fit.
    #[must_use]
    pub fn parse(buf: &[u8], offset: usize) -> Option<Self> {
        let raw_length = u32_le(buf, offset)?;
        let position = u32_le(buf, offset.saturating_add(4))?;
        Some(Self { raw_length, position })
    }

    /// The extent length in bytes (the low 30 bits of `ExtentLength`).
    #[must_use]
    pub const fn length_bytes(&self) -> u32 {
        self.raw_length & EXTENT_LENGTH_MASK
    }

    /// The extent type (the top 2 bits of `ExtentLength`).
    #[must_use]
    pub const fn kind(&self) -> ExtentKind {
        ExtentKind::from_raw(self.raw_length)
    }
}

/// One resolved file allocation extent — a contiguous run of a file's data.
/// Produced by [`icb::FileEntry::extents`].
///
/// `partition_ref` is `Some` for a `long_ad` (which names its own partition) and
/// `None` for a `short_ad` (whose partition is implicitly the file entry's). The
/// byte range within that partition is `block * block_size .. + length`, but the
/// resolution to an absolute sector — which needs the logical-volume partition
/// map — is the integration layer's job ([`source`]), not this parse layer's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// The partition reference this extent's `block` is relative to, or `None`
    /// for a `short_ad` (the file entry's own partition).
    pub partition_ref: Option<u16>,
    /// The starting logical block of the extent within its partition.
    pub block: u32,
    /// The extent length in **bytes** (the low 30 bits of `ExtentLength`).
    pub length: u32,
    /// The extent type (ECMA-167 §4/14.14.1.1).
    pub kind: ExtentKind,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest};

    use super::{
        EXTENT_LENGTH_MASK, ExtentAd, ExtentKind, LbAddr, LongAd, ShortAd,
        TAG_ANCHOR_VOLUME_POINTER, Tag, as_offset, u8_at, u16_le, u32_le, u64_le,
    };

    /// Sets byte 4 of a 16-byte tag to the checksum its other bytes require, so
    /// the array parses as a valid [`Tag`].
    fn fix_tag_checksum(mut tag: [u8; 16]) -> [u8; 16] {
        let mut sum: u8 = 0;
        for (i, &b) in tag.iter().enumerate() {
            if i != 4 {
                sum = sum.wrapping_add(b);
            }
        }
        tag[4] = sum;
        tag
    }

    #[test]
    fn le_readers_assemble_little_endian() {
        let buf = [0x11_u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        assert_eq!(u8_at(&buf, 0), Some(0x11));
        assert_eq!(u16_le(&buf, 0), Some(0x2211));
        assert_eq!(u32_le(&buf, 0), Some(0x4433_2211));
        assert_eq!(u64_le(&buf, 0), Some(0x8877_6655_4433_2211));
    }

    #[test]
    fn le_readers_out_of_bounds_are_none() {
        let buf = [0x11_u8, 0x22, 0x33];
        assert_eq!(u8_at(&buf, 3), None);
        assert_eq!(u16_le(&buf, 2), None);
        assert_eq!(u32_le(&buf, 0), None);
        assert_eq!(u64_le(&buf, 0), None);
        assert_eq!(u16_le(&buf, usize::MAX), None);
    }

    #[test]
    fn as_offset_passes_through_and_saturates() {
        assert_eq!(as_offset(0), 0);
        assert_eq!(as_offset(0x1234_5678), 0x1234_5678_usize);
        // On 64-bit (the test host) a u32 always fits, so this is the value.
        assert_eq!(as_offset(u32::MAX), usize::try_from(u32::MAX).unwrap_or(usize::MAX));
    }

    #[test]
    fn tag_parses_with_valid_checksum() {
        // identifier = 2 (AVDP) little-endian, version 3.
        let tag = fix_tag_checksum([2, 0, 3, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 1, 0, 0]);
        let parsed = Tag::parse(&tag, 0).expect("valid tag");
        assert_eq!(parsed.identifier, TAG_ANCHOR_VOLUME_POINTER);
    }

    #[test]
    fn tag_rejects_bad_checksum() {
        let mut tag = fix_tag_checksum([6, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // Corrupt a non-checksum byte so the stored checksum no longer matches.
        tag[0] = tag[0].wrapping_add(1);
        assert_eq!(Tag::parse(&tag, 0), None);
    }

    #[test]
    fn tag_parses_at_offset() {
        let tag = fix_tag_checksum([6, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 9, 0, 0, 0]);
        let mut buf = vec![0xAA_u8; 5];
        buf.extend_from_slice(&tag);
        let parsed = Tag::parse(&buf, 5).expect("valid tag at offset 5");
        assert_eq!(parsed.identifier, super::TAG_LOGICAL_VOLUME);
    }

    #[test]
    fn tag_short_buffer_is_none() {
        let tag = fix_tag_checksum([2, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(Tag::parse(tag.get(..15).unwrap_or_default(), 0), None);
        assert_eq!(Tag::parse(&tag, usize::MAX), None);
    }

    #[test]
    fn extent_ad_parses_length_and_location() {
        let buf = [0x00_u8, 0x08, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00];
        let ad = ExtentAd::parse(&buf, 0).expect("extent_ad");
        assert_eq!(ad.length, 0x0800);
        assert_eq!(ad.location, 0x20);
        assert_eq!(ExtentAd::parse(&buf, 1), None);
    }

    #[test]
    fn lb_addr_parses_block_and_partition() {
        let buf = [0x10_u8, 0x00, 0x00, 0x00, 0x02, 0x00];
        let addr = LbAddr::parse(&buf, 0).expect("lb_addr");
        assert_eq!(addr.block, 0x10);
        assert_eq!(addr.partition, 2);
        assert_eq!(LbAddr::parse(&buf, 1), None);
    }

    #[test]
    fn long_ad_splits_length_and_type() {
        // raw_length = 0x4000_0800 → type bits = 0b01 (NotRecordedAllocated),
        // length = 0x0800; block 0x30 in partition 0.
        let buf = [0x00_u8, 0x08, 0x00, 0x40, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00];
        let ad = LongAd::parse(&buf, 0).expect("long_ad");
        assert_eq!(ad.length_bytes(), 0x0800);
        assert_eq!(ad.kind(), ExtentKind::NotRecordedAllocated);
        assert_eq!(ad.location, LbAddr { block: 0x30, partition: 0 });
        assert_eq!(LongAd::parse(&buf, 5), None);
    }

    #[test]
    fn short_ad_splits_length_and_type() {
        // raw_length = 0xC000_1000 → type bits = 0b11 (NextExtent), len 0x1000.
        let buf = [0x00_u8, 0x10, 0x00, 0xC0, 0x40, 0x00, 0x00, 0x00];
        let ad = ShortAd::parse(&buf, 0).expect("short_ad");
        assert_eq!(ad.length_bytes(), 0x1000);
        assert_eq!(ad.kind(), ExtentKind::NextExtent);
        assert_eq!(ad.position, 0x40);
        assert_eq!(ShortAd::parse(&buf, 1), None);
    }

    #[test]
    fn small_parsers_are_none_until_fully_present() {
        // Every truncation below a parser's needed length fails one bounds-checked
        // field read; the full 16-byte buffer parses. (LongAd needs 10 of its 16
        // bytes — the trailing ImplementationUse is unread.)
        let buf = [0x11_u8; 16];
        for len in 0..8 {
            assert_eq!(
                ExtentAd::parse(buf.get(..len).unwrap_or_default(), 0),
                None,
                "extent_ad {len}"
            );
            assert_eq!(
                ShortAd::parse(buf.get(..len).unwrap_or_default(), 0),
                None,
                "short_ad {len}"
            );
        }
        for len in 0..6 {
            assert_eq!(LbAddr::parse(buf.get(..len).unwrap_or_default(), 0), None, "lb_addr {len}");
        }
        for len in 0..10 {
            assert_eq!(LongAd::parse(buf.get(..len).unwrap_or_default(), 0), None, "long_ad {len}");
        }
        assert!(ExtentAd::parse(&buf, 0).is_some());
        assert!(LbAddr::parse(&buf, 0).is_some());
        assert!(LongAd::parse(&buf, 0).is_some());
        assert!(ShortAd::parse(&buf, 0).is_some());
        // A huge offset saturates and the reads return None — never a panic.
        assert_eq!(ExtentAd::parse(&buf, usize::MAX), None);
        assert_eq!(LbAddr::parse(&buf, usize::MAX), None);
        assert_eq!(LongAd::parse(&buf, usize::MAX), None);
        assert_eq!(ShortAd::parse(&buf, usize::MAX), None);
    }

    #[test]
    fn extent_kind_covers_all_four_types() {
        assert_eq!(ExtentKind::from_raw(0x0000_0000), ExtentKind::RecordedAllocated);
        assert_eq!(ExtentKind::from_raw(0x4000_0000), ExtentKind::NotRecordedAllocated);
        assert_eq!(ExtentKind::from_raw(0x8000_0000), ExtentKind::NotRecordedNotAllocated);
        assert_eq!(ExtentKind::from_raw(0xC000_0000), ExtentKind::NextExtent);
        // The length bits never leak into the type.
        assert_eq!(ExtentKind::from_raw(EXTENT_LENGTH_MASK), ExtentKind::RecordedAllocated);
    }

    proptest! {
        #[test]
        fn le_readers_never_panic(buf in any::<Vec<u8>>(), off in any::<usize>()) {
            prop_assert_eq!(u8_at(&buf, off).is_some(), off < buf.len());
            prop_assert_eq!(u16_le(&buf, off).is_some(), off.checked_add(2).is_some_and(|e| e <= buf.len()));
            prop_assert_eq!(u32_le(&buf, off).is_some(), off.checked_add(4).is_some_and(|e| e <= buf.len()));
            prop_assert_eq!(u64_le(&buf, off).is_some(), off.checked_add(8).is_some_and(|e| e <= buf.len()));
        }

        #[test]
        fn u32_le_matches_from_le_bytes(prefix in any::<Vec<u8>>(), chunk in any::<[u8; 4]>()) {
            let off = prefix.len();
            let mut buf = prefix;
            buf.extend_from_slice(&chunk);
            prop_assert_eq!(u32_le(&buf, off), Some(u32::from_le_bytes(chunk)));
        }

        #[test]
        fn tag_parse_never_panics(buf in any::<Vec<u8>>(), off in any::<usize>()) {
            // Whatever the bytes, parsing only ever yields Some/None — never panics.
            let parsed = Tag::parse(&buf, off);
            prop_assert!(parsed.is_some() || parsed.is_none());
        }

        #[test]
        fn long_short_length_is_low_30_bits(raw in any::<u32>(), pos in any::<u32>()) {
            let mut buf = raw.to_le_bytes().to_vec();
            buf.extend_from_slice(&pos.to_le_bytes());
            buf.extend_from_slice(&[0_u8; 8]);
            let short = ShortAd::parse(&buf, 0).expect("short_ad");
            let long = LongAd::parse(&buf, 0).expect("long_ad");
            prop_assert_eq!(short.length_bytes(), raw & EXTENT_LENGTH_MASK);
            prop_assert_eq!(long.length_bytes(), raw & EXTENT_LENGTH_MASK);
            prop_assert!(short.length_bytes() <= EXTENT_LENGTH_MASK);
        }
    }
}
