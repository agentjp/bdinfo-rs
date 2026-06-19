//! UDF volume-level descriptors.
//!
//! The chain a reader follows to reach the file tree (ECMA-167 §3/8.4 + the OSTA
//! UDF 2.50 layout):
//!
//! 1. [`Avdp`] — Anchor Volume Descriptor Pointer at sector 256 (ECMA-167 §3/10.2): points to the
//!    Main Volume Descriptor Sequence.
//! 2. [`Lvd`] — Logical Volume Descriptor (§3/10.6): the logical block size, the [`LongAd`] to the
//!    File Set Descriptor, and the partition-map table.
//! 3. [`PartitionDescriptor`] — (§3/10.5): a partition number → physical starting sector + length.
//! 4. [`Fsd`] — File Set Descriptor (§4/14.1): the [`LongAd`] to the root directory's File Entry.
//!
//! Authored Blu-rays use a **Metadata partition** (UDF 2.50 §2.2.10): a type-2
//! [`PartitionMap`] naming a physical partition plus the location of the metadata
//! *file*, whose own allocation extents ([`super::icb`]) describe where the
//! metadata partition's blocks physically live. This module parses that map;
//! resolving a metadata logical block to a sector (which needs the metadata
//! file's extents) is the integration layer's job ([`super::source`]).
//!
//! Every numeric field is little-endian per ECMA-167 — see the [`super`]
//! module's endianness note.

use super::{
    ExtentAd, LongAd, TAG_ANCHOR_VOLUME_POINTER, TAG_FILE_SET, TAG_LOGICAL_VOLUME, TAG_PARTITION,
    TAG_PRIMARY_VOLUME, TAG_VOLUME_DESCRIPTOR_POINTER, Tag, as_offset, cs0, u8_at, u16_le, u32_le,
};

/// The OSTA identifier marking a type-2 partition map as a **Metadata** partition
/// (UDF 2.50 §2.2.10) — the 23-byte `EntityID` `Identifier` field.
const METADATA_PARTITION_ID: &[u8; 23] = b"*UDF Metadata Partition";

/// Anchor Volume Descriptor Pointer (ECMA-167 §3/10.2).
///
/// Found at logical sector 256 (and mirrored near the end of the disc), it
/// locates the volume descriptor sequence that holds the LVD and partition
/// descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Avdp {
    /// `MainVolumeDescriptorSequenceExtent` — the primary descriptor sequence
    /// (an absolute logical-sector extent).
    pub main_vds: ExtentAd,
    /// `ReserveVolumeDescriptorSequenceExtent` — the backup sequence.
    pub reserve_vds: ExtentAd,
}

impl Avdp {
    /// Parses an [`Avdp`] from a sector buffer, or `None` if the tag is not an
    /// AVDP (id 2) or the bytes are too short.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        let tag = Tag::parse(buf, 0)?;
        if tag.identifier != TAG_ANCHOR_VOLUME_POINTER {
            return None;
        }
        let main_vds = ExtentAd::parse(buf, 16)?;
        let reserve_vds = ExtentAd::parse(buf, 24)?;
        Some(Self { main_vds, reserve_vds })
    }
}

/// Primary Volume Descriptor (ECMA-167 §3/10.1) — carries the volume's own
/// (32-byte) identifier.
///
/// Parsed only as the volume-label **fallback**: the reported label
/// stays the LVD's `LogicalVolumeIdentifier` (the string Windows shows for a
/// mounted UDF volume — verified by mounting a label-patched image: `udfs.sys`
/// displays the FSD's `LogicalVolumeIdentifier` copy, which UDF 2.50 §2.3.2
/// requires to equal the LVD's), with this identifier used when that one is
/// empty. libudfread reports this PVD field instead and never reads the LVD's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pvd {
    /// `VolumeIdentifier` (offset 24, `dstring[32]`) — decoded from OSTA CS0.
    pub volume_identifier: String,
}

impl Pvd {
    /// Parses a [`Pvd`], or `None` if the tag is not a Primary Volume
    /// Descriptor (id 1) or the identifier field is out of range.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        let tag = Tag::parse(buf, 0)?;
        if tag.identifier != TAG_PRIMARY_VOLUME {
            return None;
        }
        let volume_identifier = cs0::decode_dstring(buf.get(24..56)?);
        Some(Self { volume_identifier })
    }
}

/// Volume Descriptor Pointer (ECMA-167 §3/10.3) — continues a volume descriptor
/// sequence in another extent (legal per §3/8.4.2; followed bounded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vdp {
    /// `NextVolumeDescriptorSequenceExtent` (offset 20) — where the sequence
    /// continues.
    pub next: ExtentAd,
}

impl Vdp {
    /// Parses a [`Vdp`] from a sector buffer, or `None` if the tag is not a
    /// Volume Descriptor Pointer (id 3) or the extent is out of range.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        let tag = Tag::parse(buf, 0)?;
        if tag.identifier != TAG_VOLUME_DESCRIPTOR_POINTER {
            return None;
        }
        let next = ExtentAd::parse(buf, 20)?;
        Some(Self { next })
    }
}

/// One entry of a Logical Volume Descriptor's partition-map table (ECMA-167
/// §3/10.7). The entry's index in the table is the *partition reference number*
/// that [`LongAd`]/`lb_addr` fields cite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionMap {
    /// A type-1 map (ECMA-167 §3/10.7.2): names a physical partition directly by
    /// its `PartitionNumber`.
    Physical {
        /// The `PartitionNumber` this reference resolves to — matched against a
        /// [`PartitionDescriptor::partition_number`].
        partition_number: u16,
    },
    /// A type-2 **Metadata** partition map (UDF 2.50 §2.2.10).
    Metadata(MetadataPartitionMap),
    /// A partition map this reader does not resolve — a type-2 map other than
    /// Metadata (Virtual / Sparable), or an unrecognized type. Carries the raw
    /// `PartitionMapType`.
    Other {
        /// The raw `PartitionMapType` byte.
        map_type: u8,
    },
}

/// A UDF 2.50 Metadata partition map (UDF 2.50 §2.2.10) — a type-2 partition map.
///
/// The metadata partition's blocks live inside the physical partition
/// `physical_partition`; the metadata **file** (a File Entry at logical block
/// `metadata_file_location` of that physical partition) holds the allocation
/// extents that map metadata logical blocks to physical ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataPartitionMap {
    /// `PartitionNumber` — the physical partition that backs this metadata
    /// partition.
    pub physical_partition: u16,
    /// `MetadataFileLocation` — logical block (within the physical partition) of
    /// the metadata file's File Entry.
    pub metadata_file_location: u32,
    /// `MetadataMirrorFileLocation` — logical block of the mirror metadata file
    /// (a redundant copy, used if the primary is unreadable).
    pub metadata_mirror_file_location: u32,
}

impl MetadataPartitionMap {
    /// Parses the metadata-specific fields from a type-2 map slice, or `None` if
    /// it is too short.
    #[must_use]
    pub fn parse(map: &[u8]) -> Option<Self> {
        let physical_partition = u16_le(map, 38)?;
        let metadata_file_location = u32_le(map, 40)?;
        let metadata_mirror_file_location = u32_le(map, 44)?;
        Some(Self { physical_partition, metadata_file_location, metadata_mirror_file_location })
    }
}

/// Parses one partition map of the given `map_type` from its `slice` (which
/// spans exactly the map's declared length).
fn parse_partition_map(map_type: u8, slice: &[u8]) -> PartitionMap {
    match map_type {
        // Type 1 — physical: PartitionNumber at offset 4 (after type+len+vol-seq).
        1 => u16_le(slice, 4).map_or(PartitionMap::Other { map_type }, |partition_number| {
            PartitionMap::Physical { partition_number }
        }),
        // Type 2 — distinguished by the EntityID Identifier at offset 5 (the
        // EntityID begins at offset 4; byte 4 is its Flags).
        2 if slice.get(5..28) == Some(METADATA_PARTITION_ID.as_slice()) => {
            MetadataPartitionMap::parse(slice)
                .map_or(PartitionMap::Other { map_type }, PartitionMap::Metadata)
        }
        _ => PartitionMap::Other { map_type },
    }
}

/// Walks `count` partition maps out of `bytes` (the LVD's map table), advancing by
/// each map's self-declared length. Stops early on a truncated table or a
/// zero-length map (which would not advance).
fn parse_partition_maps(bytes: &[u8], count: u32) -> Vec<PartitionMap> {
    let mut maps = Vec::new();
    let mut off: usize = 0;
    for _ in 0..count {
        let Some(map_type) = u8_at(bytes, off) else { break };
        let Some(map_len) = u8_at(bytes, off.saturating_add(1)) else { break };
        let map_len = usize::from(map_len);
        if map_len == 0 {
            break;
        }
        let end = off.saturating_add(map_len);
        let Some(slice) = bytes.get(off..end) else { break };
        maps.push(parse_partition_map(map_type, slice));
        off = end;
    }
    maps
}

/// Logical Volume Descriptor (ECMA-167 §3/10.6) — the logical block size, the
/// pointer to the File Set Descriptor, and the partition-map table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lvd {
    /// `LogicalVolumeIdentifier` — the volume label, decoded from OSTA CS0.
    pub logical_volume_identifier: String,
    /// `LogicalBlockSize` — the logical block (sector) size in bytes (2048 for a
    /// Blu-ray).
    pub logical_block_size: u32,
    /// The `LongAd` from `LogicalVolumeContentsUse` locating the File Set
    /// Descriptor.
    pub file_set_descriptor: LongAd,
    /// The partition-map table; an entry's index is its partition reference.
    pub partition_maps: Vec<PartitionMap>,
}

impl Lvd {
    /// Parses an [`Lvd`], or `None` if the tag is not an LVD (id 6) or a field is
    /// out of range.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        let tag = Tag::parse(buf, 0)?;
        if tag.identifier != TAG_LOGICAL_VOLUME {
            return None;
        }
        let logical_volume_identifier = cs0::decode_dstring(buf.get(84..212)?);
        let logical_block_size = u32_le(buf, 212)?;
        // LogicalVolumeContentsUse (offset 248) is a long_ad to the FSD.
        let file_set_descriptor = LongAd::parse(buf, 248)?;
        let map_table_length = u32_le(buf, 264)?;
        let num_partition_maps = u32_le(buf, 268)?;
        // Partition maps follow the fixed header at offset 440, for
        // MapTableLength bytes (bounded to the buffer).
        let maps_all = buf.get(440..)?;
        let limit = as_offset(map_table_length).min(maps_all.len());
        let maps_bytes = maps_all.get(..limit).unwrap_or(maps_all);
        let partition_maps = parse_partition_maps(maps_bytes, num_partition_maps);
        Some(Self {
            logical_volume_identifier,
            logical_block_size,
            file_set_descriptor,
            partition_maps,
        })
    }
}

/// Partition Descriptor (ECMA-167 §3/10.5) — maps a partition number to its
/// physical starting sector and length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionDescriptor {
    /// `PartitionNumber` — matched against a [`PartitionMap::Physical`]
    /// `partition_number` / a metadata map's `physical_partition`.
    pub partition_number: u16,
    /// `PartitionStartingLocation` — the partition's first physical sector.
    pub starting_location: u32,
    /// `PartitionLength` — the partition length in sectors.
    pub length: u32,
}

impl PartitionDescriptor {
    /// Parses a [`PartitionDescriptor`], or `None` if the tag is not a Partition
    /// Descriptor (id 5) or a field is out of range.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        let tag = Tag::parse(buf, 0)?;
        if tag.identifier != TAG_PARTITION {
            return None;
        }
        let partition_number = u16_le(buf, 22)?;
        let starting_location = u32_le(buf, 188)?;
        let length = u32_le(buf, 192)?;
        Some(Self { partition_number, starting_location, length })
    }
}

/// File Set Descriptor (ECMA-167 §4/14.1) — names the root directory's File
/// Entry. Located via the LVD's [`file_set_descriptor`](Lvd::file_set_descriptor)
/// `LongAd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fsd {
    /// `RootDirectoryICB` (offset 400) — the `LongAd` to the root directory's
    /// File Entry.
    pub root_directory_icb: LongAd,
}

impl Fsd {
    /// Parses an [`Fsd`], or `None` if the tag is not a File Set Descriptor
    /// (id 256) or the root-directory ICB is out of range.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        let tag = Tag::parse(buf, 0)?;
        if tag.identifier != TAG_FILE_SET {
            return None;
        }
        let root_directory_icb = LongAd::parse(buf, 400)?;
        Some(Self { root_directory_icb })
    }
}

/// Resolves a partition reference number to the physical starting sector of the
/// partition that backs it, using the LVD's partition maps and the partition
/// descriptors (a pure cross-lookup, no IO).
///
/// For a [`PartitionMap::Physical`] the reference resolves to that partition; for
/// a [`PartitionMap::Metadata`] it resolves to the *physical* partition the
/// metadata partition lives in (the caller then applies the metadata file's
/// extents to map a specific block). Returns `None` for an out-of-range
/// reference, a [`PartitionMap::Other`], or a partition number with no matching
/// descriptor.
#[must_use]
pub fn physical_partition_start(
    maps: &[PartitionMap],
    descriptors: &[PartitionDescriptor],
    partition_ref: u16,
) -> Option<u32> {
    let partition_number = match maps.get(usize::from(partition_ref))? {
        PartitionMap::Physical { partition_number } => *partition_number,
        PartitionMap::Metadata(meta) => meta.physical_partition,
        PartitionMap::Other { .. } => return None,
    };
    descriptors.iter().find(|d| d.partition_number == partition_number).map(|d| d.starting_location)
}

#[cfg(test)]
mod tests {
    use super::{
        Avdp, Fsd, Lvd, METADATA_PARTITION_ID, MetadataPartitionMap, PartitionDescriptor,
        PartitionMap, Pvd, Vdp, parse_partition_maps, physical_partition_start,
    };
    use crate::vfs::udf::{ExtentAd, LbAddr, LongAd};

    /// Writes `bytes` at `off` in `buf` (test scaffolding; branchless — an
    /// out-of-range tail is simply not written — so it adds no uncovered region).
    fn put(buf: &mut [u8], off: usize, bytes: &[u8]) {
        for (dst, &src) in buf.iter_mut().skip(off).zip(bytes) {
            *dst = src;
        }
    }

    /// Builds a `size`-byte descriptor buffer with a valid tag of `identifier`.
    fn descriptor(size: usize, identifier: u16) -> Vec<u8> {
        let mut buf = vec![0_u8; size];
        put(&mut buf, 0, &identifier.to_le_bytes());
        put(&mut buf, 2, &3_u16.to_le_bytes());
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
    fn metadata_partition_id_is_23_bytes() {
        assert_eq!(METADATA_PARTITION_ID.len(), 23);
    }

    #[test]
    fn avdp_parses_volume_sequence_extents() {
        let mut buf = descriptor(512, super::TAG_ANCHOR_VOLUME_POINTER);
        put(&mut buf, 16, &32_u32.to_le_bytes()); // main length 32 bytes
        put(&mut buf, 20, &0x20_u32.to_le_bytes()); // main location sector 32
        put(&mut buf, 24, &16_u32.to_le_bytes());
        put(&mut buf, 28, &0x40_u32.to_le_bytes());
        let avdp = Avdp::parse(&buf).expect("avdp");
        assert_eq!(avdp.main_vds, ExtentAd { length: 32, location: 0x20 });
        assert_eq!(avdp.reserve_vds, ExtentAd { length: 16, location: 0x40 });
    }

    #[test]
    fn avdp_rejects_wrong_tag_and_truncations() {
        // Wrong tag id.
        assert_eq!(Avdp::parse(&descriptor(512, super::TAG_LOGICAL_VOLUME)), None);
        // A valid AVDP needs 32 bytes (16 tag + two 8-byte extents); the full
        // buffer parses and every shorter one fails a bounds-checked read.
        let full = descriptor(32, super::TAG_ANCHOR_VOLUME_POINTER);
        assert!(Avdp::parse(&full).is_some());
        for len in 0..32 {
            assert_eq!(Avdp::parse(full.get(..len).unwrap_or_default()), None, "avdp {len}");
        }
    }

    #[test]
    fn pvd_parses_the_volume_identifier() {
        let mut buf = descriptor(512, super::TAG_PRIMARY_VOLUME);
        // VolumeIdentifier dstring[32] at 24: compId 8 + "DISC".
        put(&mut buf, 24, &[8, b'D', b'I', b'S', b'C']);
        put(&mut buf, 55, &[5]); // used length: compId + 4 chars
        let pvd = Pvd::parse(&buf).expect("pvd");
        assert_eq!(pvd.volume_identifier, "DISC");
    }

    #[test]
    fn pvd_rejects_wrong_tag_and_truncations() {
        assert_eq!(Pvd::parse(&descriptor(512, super::TAG_LOGICAL_VOLUME)), None);
        // The identifier dstring ends at offset 56.
        let full = descriptor(56, super::TAG_PRIMARY_VOLUME);
        assert!(Pvd::parse(&full).is_some());
        for len in 0..56 {
            assert_eq!(Pvd::parse(full.get(..len).unwrap_or_default()), None, "pvd {len}");
        }
    }

    #[test]
    fn vdp_parses_the_continuation_extent() {
        let mut buf = descriptor(512, super::TAG_VOLUME_DESCRIPTOR_POINTER);
        put(&mut buf, 20, &(2048_u32).to_le_bytes()); // next length
        put(&mut buf, 24, &0x90_u32.to_le_bytes()); // next location
        let vdp = Vdp::parse(&buf).expect("vdp");
        assert_eq!(vdp.next, ExtentAd { length: 2048, location: 0x90 });
    }

    #[test]
    fn vdp_rejects_wrong_tag_and_truncations() {
        assert_eq!(Vdp::parse(&descriptor(512, super::TAG_PARTITION)), None);
        // A valid VDP needs 28 bytes (16 tag + VDSN + an 8-byte extent_ad).
        let full = descriptor(28, super::TAG_VOLUME_DESCRIPTOR_POINTER);
        assert!(Vdp::parse(&full).is_some());
        for len in 0..28 {
            assert_eq!(Vdp::parse(full.get(..len).unwrap_or_default()), None, "vdp {len}");
        }
    }

    #[test]
    fn lvd_parses_block_size_fsd_and_physical_map() {
        let mut buf = descriptor(2048, super::TAG_LOGICAL_VOLUME);
        // LogicalVolumeIdentifier dstring[128] at 84: compId 8 + "MyDisc".
        put(&mut buf, 84, &[8, b'M', b'y', b'D', b'i', b's', b'c']);
        put(&mut buf, 211, &[7]); // dstring used-length (offset 84+127): compId + 6 chars
        put(&mut buf, 212, &2048_u32.to_le_bytes()); // LogicalBlockSize
        // LogicalVolumeContentsUse (248): a long_ad → FSD at block 0x100, part 0.
        put(&mut buf, 248, &0x800_u32.to_le_bytes()); // length 2048 bytes
        put(&mut buf, 252, &0x100_u32.to_le_bytes()); // block
        put(&mut buf, 256, &0_u16.to_le_bytes()); // partition
        // One physical partition map (type 1, len 6) → partition number 0.
        put(&mut buf, 264, &6_u32.to_le_bytes()); // MapTableLength
        put(&mut buf, 268, &1_u32.to_le_bytes()); // NumberOfPartitionMaps
        put(&mut buf, 440, &[1, 6, 0, 0, 0, 0]); // type 1, len 6, part# 0

        let lvd = Lvd::parse(&buf).expect("lvd");
        assert_eq!(lvd.logical_volume_identifier, "MyDisc");
        assert_eq!(lvd.logical_block_size, 2048);
        assert_eq!(lvd.file_set_descriptor.location, LbAddr { block: 0x100, partition: 0 });
        assert_eq!(lvd.file_set_descriptor.length_bytes(), 0x800);
        assert_eq!(lvd.partition_maps, vec![PartitionMap::Physical { partition_number: 0 }]);
    }

    #[test]
    fn lvd_parses_metadata_partition_map() {
        let mut buf = descriptor(2048, super::TAG_LOGICAL_VOLUME);
        put(&mut buf, 212, &2048_u32.to_le_bytes());
        put(&mut buf, 248, &0x800_u32.to_le_bytes());
        put(&mut buf, 264, &70_u32.to_le_bytes()); // 6 (physical) + 64 (metadata)
        put(&mut buf, 268, &2_u32.to_le_bytes());
        // Map 0: physical partition number 0.
        put(&mut buf, 440, &[1, 6, 0, 0, 0, 0]);
        // Map 1: type-2 metadata map (len 64). EntityID identifier at slice+5.
        let meta_off = 446;
        put(&mut buf, meta_off, &[2, 64]);
        put(&mut buf, meta_off + 5, METADATA_PARTITION_ID.as_slice());
        put(&mut buf, meta_off + 38, &0_u16.to_le_bytes()); // physical partition 0
        put(&mut buf, meta_off + 40, &0x50_u32.to_le_bytes()); // metadata file block
        put(&mut buf, meta_off + 44, &0x51_u32.to_le_bytes()); // mirror block

        let lvd = Lvd::parse(&buf).expect("lvd");
        assert_eq!(lvd.partition_maps.len(), 2);
        assert_eq!(
            lvd.partition_maps.first(),
            Some(&PartitionMap::Physical { partition_number: 0 })
        );
        assert_eq!(
            lvd.partition_maps.get(1),
            Some(&PartitionMap::Metadata(MetadataPartitionMap {
                physical_partition: 0,
                metadata_file_location: 0x50,
                metadata_mirror_file_location: 0x51,
            }))
        );
    }

    #[test]
    fn lvd_rejects_wrong_tag_and_truncations() {
        assert_eq!(Lvd::parse(&descriptor(2048, super::TAG_PARTITION)), None);
        // The fixed LVD header ends at offset 440 (the partition-map table); a
        // valid LVD parses there with an empty table, and every shorter buffer
        // fails one of the header field reads.
        let full = descriptor(2048, super::TAG_LOGICAL_VOLUME);
        assert!(Lvd::parse(full.get(..440).unwrap_or_default()).is_some());
        for len in 0..440 {
            assert_eq!(Lvd::parse(full.get(..len).unwrap_or_default()), None, "lvd {len}");
        }
    }

    #[test]
    fn partition_map_loop_handles_type2_other_unknown_and_truncation() {
        // type-2 non-metadata (Virtual) + type-9 unknown, then a truncated 3rd.
        let mut bytes = vec![0_u8; 64 + 6 + 1];
        put(&mut bytes, 0, &[2, 64]); // type 2, not metadata id → Other
        put(&mut bytes, 64, &[9, 6]); // unknown type 9 → Other
        // 3rd map: only one byte present (type, no length) → loop stops.
        put(&mut bytes, 70, &[1]);
        let maps = parse_partition_maps(&bytes, 3);
        assert_eq!(
            maps,
            vec![PartitionMap::Other { map_type: 2 }, PartitionMap::Other { map_type: 9 }]
        );
    }

    #[test]
    fn partition_map_loop_stops_on_zero_length() {
        // A zero-length map cannot advance the cursor — the loop must break.
        let bytes = [1_u8, 0, 0, 0];
        let maps = parse_partition_maps(&bytes, 5);
        assert!(maps.is_empty());
    }

    #[test]
    fn partition_map_loop_stops_when_empty() {
        assert!(parse_partition_maps(&[], 4).is_empty());
    }

    #[test]
    fn partition_map_loop_stops_when_map_runs_past_buffer() {
        // A type-1 map declaring length 6 with only 4 bytes present → the slice
        // fetch fails and the walk stops with nothing parsed.
        let maps = parse_partition_maps(&[1, 6, 0, 0], 1);
        assert!(maps.is_empty());
    }

    #[test]
    fn metadata_partition_map_parses_and_rejects_truncations() {
        let mut map = vec![0_u8; 64];
        put(&mut map, 38, &0_u16.to_le_bytes());
        put(&mut map, 40, &0x50_u32.to_le_bytes());
        put(&mut map, 44, &0x51_u32.to_le_bytes());
        assert_eq!(
            MetadataPartitionMap::parse(&map),
            Some(MetadataPartitionMap {
                physical_partition: 0,
                metadata_file_location: 0x50,
                metadata_mirror_file_location: 0x51,
            })
        );
        // Truncating before each field exercises that field's bounds check.
        assert_eq!(MetadataPartitionMap::parse(map.get(..39).unwrap_or_default()), None);
        assert_eq!(MetadataPartitionMap::parse(map.get(..42).unwrap_or_default()), None);
        assert_eq!(MetadataPartitionMap::parse(map.get(..46).unwrap_or_default()), None);
    }

    #[test]
    fn metadata_map_short_slice_is_other() {
        // A type-2 map flagged metadata whose declared length (40) fits the
        // buffer but is too short for the metadata fields at offsets 38..48 →
        // MetadataPartitionMap::parse fails → Other.
        let mut bytes = vec![0_u8; 40];
        put(&mut bytes, 0, &[2, 40]);
        put(&mut bytes, 5, METADATA_PARTITION_ID.as_slice());
        let maps = parse_partition_maps(&bytes, 1);
        assert_eq!(maps, vec![PartitionMap::Other { map_type: 2 }]);
    }

    #[test]
    fn physical_map_short_slice_is_other() {
        // A type-1 map shorter than 6 bytes can't yield a partition number.
        let bytes = [1_u8, 3, 0]; // declared len 3, no PartitionNumber field
        let maps = parse_partition_maps(&bytes, 1);
        assert_eq!(maps, vec![PartitionMap::Other { map_type: 1 }]);
    }

    #[test]
    fn partition_descriptor_parses_number_start_length() {
        let mut buf = descriptor(512, super::TAG_PARTITION);
        put(&mut buf, 22, &0_u16.to_le_bytes());
        put(&mut buf, 188, &0x120_u32.to_le_bytes());
        put(&mut buf, 192, &0x4000_u32.to_le_bytes());
        let pd = PartitionDescriptor::parse(&buf).expect("pd");
        assert_eq!(
            pd,
            PartitionDescriptor { partition_number: 0, starting_location: 0x120, length: 0x4000 }
        );
    }

    #[test]
    fn partition_descriptor_rejects_wrong_tag_and_truncations() {
        assert_eq!(PartitionDescriptor::parse(&descriptor(512, super::TAG_FILE_SET)), None);
        // The last field (PartitionLength) ends at offset 196.
        let full = descriptor(196, super::TAG_PARTITION);
        assert!(PartitionDescriptor::parse(&full).is_some());
        for len in 0..196 {
            assert_eq!(
                PartitionDescriptor::parse(full.get(..len).unwrap_or_default()),
                None,
                "pd {len}"
            );
        }
    }

    #[test]
    fn fsd_parses_root_directory_icb() {
        let mut buf = descriptor(512, super::TAG_FILE_SET);
        put(&mut buf, 400, &0x800_u32.to_le_bytes()); // length
        put(&mut buf, 404, &0x10_u32.to_le_bytes()); // block
        put(&mut buf, 408, &1_u16.to_le_bytes()); // partition ref 1
        let fsd = Fsd::parse(&buf).expect("fsd");
        assert_eq!(fsd.root_directory_icb.location, LbAddr { block: 0x10, partition: 1 });
        assert_eq!(fsd.root_directory_icb.length_bytes(), 0x800);
    }

    #[test]
    fn fsd_rejects_wrong_tag_and_truncations() {
        assert_eq!(Fsd::parse(&descriptor(512, super::TAG_LOGICAL_VOLUME)), None);
        // The root-directory ICB long_ad at offset 400 needs 10 bytes (ends 410).
        let full = descriptor(416, super::TAG_FILE_SET);
        assert!(Fsd::parse(full.get(..410).unwrap_or_default()).is_some());
        for len in 0..410 {
            assert_eq!(Fsd::parse(full.get(..len).unwrap_or_default()), None, "fsd {len}");
        }
    }

    #[test]
    fn physical_partition_start_resolves_physical_and_metadata_refs() {
        let maps = [
            PartitionMap::Physical { partition_number: 0 },
            PartitionMap::Metadata(MetadataPartitionMap {
                physical_partition: 0,
                metadata_file_location: 0x50,
                metadata_mirror_file_location: 0x51,
            }),
            PartitionMap::Other { map_type: 2 },
        ];
        let descriptors =
            [PartitionDescriptor { partition_number: 0, starting_location: 0x120, length: 0x4000 }];
        // Physical ref 0 and metadata ref 1 both back onto physical partition 0.
        assert_eq!(physical_partition_start(&maps, &descriptors, 0), Some(0x120));
        assert_eq!(physical_partition_start(&maps, &descriptors, 1), Some(0x120));
        // Other map → None; out-of-range ref → None.
        assert_eq!(physical_partition_start(&maps, &descriptors, 2), None);
        assert_eq!(physical_partition_start(&maps, &descriptors, 9), None);
    }

    #[test]
    fn physical_partition_start_none_without_matching_descriptor() {
        let maps = [PartitionMap::Physical { partition_number: 7 }];
        let descriptors =
            [PartitionDescriptor { partition_number: 0, starting_location: 0x120, length: 1 }];
        assert_eq!(physical_partition_start(&maps, &descriptors, 0), None);
    }

    #[test]
    fn long_ad_helpers_used_by_fsd_round_trip() {
        // Guards that the LongAd re-exported here behaves as the FSD expects.
        let buf = [0x00_u8, 0x08, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00];
        let ad = LongAd::parse(&buf, 0).expect("long_ad");
        assert_eq!(ad.length_bytes(), 0x800);
    }
}
