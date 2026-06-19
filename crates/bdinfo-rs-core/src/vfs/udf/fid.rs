//! File Identifier Descriptors → directory enumeration + OSTA CS0 names.
//!
//! A directory's data (located via its File Entry's extents / embedded bytes,
//! see [`super::icb`]) is a packed sequence of File Identifier Descriptors
//! (ECMA-167 §4/14.4, tag 257). Each names one child: its [`LongAd`] ICB (the
//! child's File Entry), its `FileCharacteristics` (directory / parent / hidden /
//! deleted), and its identifier in OSTA CS0 ([`super::cs0`]).
//! [`parse_directory`] walks the whole sequence; [`Fid::parse`] reads one
//! descriptor and reports its padded length so the walk can advance (each FID is
//! padded to a 4-byte boundary, §4/14.4.9).
//!
//! All numeric fields are little-endian per ECMA-167 (see [`super`]).

use super::{LongAd, TAG_FILE_IDENTIFIER, Tag, cs0, u8_at, u16_le};

/// `FileCharacteristics` bit — the entry is marked hidden (ECMA-167 §4/14.4.3).
const FID_HIDDEN: u8 = 0x01;
/// `FileCharacteristics` bit — the entry is a directory.
const FID_DIRECTORY: u8 = 0x02;
/// `FileCharacteristics` bit — the entry is deleted.
const FID_DELETED: u8 = 0x04;
/// `FileCharacteristics` bit — the entry is the parent (`..`) link.
const FID_PARENT: u8 = 0x08;

/// One File Identifier Descriptor (ECMA-167 §4/14.4) — a directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fid {
    /// `FileCharacteristics` (offset 18) — the directory/parent/hidden/deleted
    /// bit flags.
    pub characteristics: u8,
    /// `ICB` (offset 20) — the `LongAd` locating the child's File Entry.
    pub icb: LongAd,
    /// `FileIdentifier`, decoded from OSTA CS0. Empty for the parent (`..`) entry.
    pub name: String,
}

impl Fid {
    /// Parses one [`Fid`] at `offset` in `buf`, returning the descriptor and its
    /// **total padded length** in bytes (so a caller can advance to the next).
    ///
    /// Returns `None` if the tag is not a FID (id 257) or any field — including
    /// the identifier of `LengthofFileIdentifier` bytes — runs past `buf`.
    #[must_use]
    pub fn parse(buf: &[u8], offset: usize) -> Option<(Self, usize)> {
        let tag = Tag::parse(buf, offset)?;
        if tag.identifier != TAG_FILE_IDENTIFIER {
            return None;
        }
        let characteristics = u8_at(buf, offset.saturating_add(18))?;
        let l_fi = usize::from(u8_at(buf, offset.saturating_add(19))?);
        let icb = LongAd::parse(buf, offset.saturating_add(20))?;
        let l_iu = usize::from(u16_le(buf, offset.saturating_add(36))?);
        // FileIdentifier follows the 38-byte header and the L_IU implementation
        // bytes (offsets advance with saturating_add; an out-of-range start then
        // fails the bounds-checked slice below — the clpi/mpls offset pattern).
        let name_start = offset.saturating_add(38).saturating_add(l_iu);
        let name_end = name_start.saturating_add(l_fi);
        let name_bytes = buf.get(name_start..name_end)?;
        let name = cs0::decode_dchars(name_bytes);
        // Total length = 38 + L_IU + L_FI, padded up to a 4-byte boundary.
        let raw_len = 38_usize.saturating_add(l_iu).saturating_add(l_fi);
        let padded = raw_len.saturating_add(3) & !3_usize;
        Some((Self { characteristics, icb, name }, padded))
    }

    /// Whether this entry is a directory (`FileCharacteristics` bit 1).
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.characteristics & FID_DIRECTORY != 0
    }

    /// Whether this entry is the parent (`..`) link (`FileCharacteristics`
    /// bit 3) — these carry an empty identifier and are skipped when listing.
    #[must_use]
    pub const fn is_parent(&self) -> bool {
        self.characteristics & FID_PARENT != 0
    }

    /// Whether this entry is marked hidden (`FileCharacteristics` bit 0).
    #[must_use]
    pub const fn is_hidden(&self) -> bool {
        self.characteristics & FID_HIDDEN != 0
    }

    /// Whether this entry is marked deleted (`FileCharacteristics` bit 2).
    #[must_use]
    pub const fn is_deleted(&self) -> bool {
        self.characteristics & FID_DELETED != 0
    }
}

/// Enumerates every File Identifier Descriptor packed in a directory's data
/// `buf`, in on-disc order (parent entry first, then children).
///
/// The walk ends when a descriptor no longer parses — at the end of the data, or
/// at the zero padding UDF writes after the last FID (zero bytes form a valid tag
/// whose identifier is 0, not a FID). Every FID is at least 40 bytes once padded,
/// so the cursor always advances.
#[must_use]
pub fn parse_directory(buf: &[u8]) -> Vec<Fid> {
    let mut fids = Vec::new();
    let mut off: usize = 0;
    while let Some((fid, len)) = Fid::parse(buf, off) {
        fids.push(fid);
        off = off.saturating_add(len);
    }
    fids
}

#[cfg(test)]
mod tests {
    use super::{Fid, parse_directory};
    use crate::vfs::udf::LbAddr;

    /// Builds a single padded FID with the given characteristics, OSTA CS0 name
    /// bytes, implementation-use length, and ICB block/partition.
    fn make_fid(
        characteristics: u8,
        name: &[u8],
        l_iu: usize,
        block: u32,
        partition: u16,
    ) -> Vec<u8> {
        let raw = 38_usize.saturating_add(l_iu).saturating_add(name.len());
        let padded = raw.saturating_add(3) & !3_usize;
        let mut buf = vec![0_u8; padded];
        put(&mut buf, 0, &super::TAG_FILE_IDENTIFIER.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
        put(&mut buf, 18, &[characteristics]);
        put(&mut buf, 19, &[u8::try_from(name.len()).unwrap_or(0)]);
        // ICB long_ad at 20: length 0x800, then lb_addr (block + partition).
        put(&mut buf, 20, &0x800_u32.to_le_bytes());
        put(&mut buf, 24, &block.to_le_bytes());
        put(&mut buf, 28, &partition.to_le_bytes());
        put(&mut buf, 36, &u16::try_from(l_iu).unwrap_or(0).to_le_bytes());
        // ImplementationUse (l_iu bytes) is left zero; the name follows it.
        put(&mut buf, 38_usize.saturating_add(l_iu), name);
        fix_tag_checksum(&mut buf);
        buf
    }

    /// Writes `bytes` at `off` (branchless — adds no uncovered region).
    fn put(buf: &mut [u8], off: usize, bytes: &[u8]) {
        for (dst, &src) in buf.iter_mut().skip(off).zip(bytes) {
            *dst = src;
        }
    }

    fn fix_tag_checksum(buf: &mut [u8]) {
        let sum = buf
            .iter()
            .take(16)
            .enumerate()
            .filter(|&(i, _)| i != 4)
            .fold(0_u8, |acc, (_, &b)| acc.wrapping_add(b));
        put(buf, 4, &[sum]);
    }

    #[test]
    fn fid_parses_directory_child_with_latin1_name() {
        // A subdirectory "BDMV" (compId 8), ICB at block 0x12 in partition 0.
        let buf = make_fid(0x02, &[8, b'B', b'D', b'M', b'V'], 0, 0x12, 0);
        let (fid, len) = Fid::parse(&buf, 0).expect("fid");
        assert_eq!(fid.name, "BDMV");
        assert!(fid.is_directory());
        assert!(!fid.is_parent());
        assert!(!fid.is_hidden());
        assert!(!fid.is_deleted());
        assert_eq!(fid.icb.location, LbAddr { block: 0x12, partition: 0 });
        assert_eq!(len, buf.len());
        // 38 header + 5 name = 43 → padded to 44.
        assert_eq!(len, 44);
    }

    #[test]
    fn fid_parses_utf16_name_and_implementation_use() {
        // Name "Aé" in CS0 compId 16 (UTF-16BE), with 4 implementation-use bytes.
        let buf = make_fid(0x00, &[16, 0x00, b'A', 0x00, 0xE9], 4, 0x20, 1);
        let (fid, _len) = Fid::parse(&buf, 0).expect("fid");
        assert_eq!(fid.name, "Aé");
        assert!(!fid.is_directory());
        assert_eq!(fid.icb.location, LbAddr { block: 0x20, partition: 1 });
    }

    #[test]
    fn fid_flags_decode_each_bit() {
        // All four characteristic bits set.
        let all = make_fid(0x0F, &[], 0, 0, 0);
        let (fid, _) = Fid::parse(&all, 0).expect("fid");
        assert!(fid.is_hidden());
        assert!(fid.is_directory());
        assert!(fid.is_deleted());
        assert!(fid.is_parent());
        assert_eq!(fid.name, "");
        // No bits set.
        let none = make_fid(0x00, &[8, b'X'], 0, 0, 0);
        let (fid, _) = Fid::parse(&none, 0).expect("fid");
        assert!(!fid.is_hidden());
        assert!(!fid.is_directory());
        assert!(!fid.is_deleted());
        assert!(!fid.is_parent());
    }

    #[test]
    fn fid_rejects_wrong_tag() {
        let mut buf = make_fid(0x02, &[8, b'X'], 0, 0, 0);
        // Re-stamp the tag identifier as a File Entry (261) and fix the checksum.
        put(&mut buf, 0, &261_u16.to_le_bytes());
        fix_tag_checksum(&mut buf);
        assert_eq!(Fid::parse(&buf, 0), None);
    }

    #[test]
    fn fid_parse_is_none_for_every_truncation() {
        // A full FID (name "BDMV") parses; every truncation before the name end
        // (43 bytes) fails some bounds-checked read — exercising each `?` arm.
        let full = make_fid(0x02, &[8, b'B', b'D', b'M', b'V'], 0, 0x10, 0);
        for len in [0_usize, 8, 16, 18, 19, 20, 30, 36, 38, 42] {
            let truncated = full.get(..len).unwrap_or_default();
            assert_eq!(Fid::parse(truncated, 0), None, "truncation to {len} bytes");
        }
        assert!(Fid::parse(&full, 0).is_some());
        // A huge offset can't parse a tag → None (no panic).
        assert_eq!(Fid::parse(&full, usize::MAX), None);
    }

    #[test]
    fn parse_directory_enumerates_parent_then_children() {
        // A directory: parent ("..") + two children, then zero padding.
        let mut dir = Vec::new();
        dir.extend_from_slice(&make_fid(0x0A, &[], 0, 0x05, 0)); // parent+dir, no name
        dir.extend_from_slice(&make_fid(0x02, &[8, b'B', b'D', b'M', b'V'], 0, 0x12, 0));
        dir.extend_from_slice(&make_fid(0x00, &[8, b'M', b'O', b'V', b'I', b'E'], 0, 0x40, 0));
        dir.extend_from_slice(&[0_u8; 8]); // trailing padding → walk stops

        let fids = parse_directory(&dir);
        assert_eq!(fids.len(), 3);
        assert!(fids.first().is_some_and(Fid::is_parent));
        let names: Vec<&str> = fids.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["", "BDMV", "MOVIE"]);
        // The children resolve to their ICB blocks.
        assert_eq!(fids.get(1).map(|f| f.icb.location.block), Some(0x12));
        assert_eq!(fids.get(2).map(|f| f.icb.location.block), Some(0x40));
    }

    #[test]
    fn parse_directory_empty_input_is_empty() {
        assert!(parse_directory(&[]).is_empty());
    }

    #[test]
    fn parse_directory_stops_after_last_valid_fid() {
        // Exactly one FID, no padding — the walk ends by exhausting the buffer.
        let dir = make_fid(0x02, &[8, b'A'], 0, 1, 0);
        let fids = parse_directory(&dir);
        assert_eq!(fids.len(), 1);
        assert_eq!(fids.first().map(|f| f.name.clone()), Some("A".to_owned()));
    }
}
