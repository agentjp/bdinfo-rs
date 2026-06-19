#![no_main]
//! Fuzz target: the read-only UDF (`.iso`) reader's parsers over the untrusted
//! bytes of a disc image's sectors — descriptor tags, the volume/partition/file-
//! set descriptors, File Entries + allocation descriptors, File Identifier
//! Descriptors / directory enumeration, and OSTA CS0 string decoding.
//!
//! Amplifies the no-panic / no-out-of-bounds contract the `*_never_panics` /
//! truncation proptests hold on Windows; here it runs adversarially on
//! nightly/Linux (see fuzz/README.md). Each parser takes a `&[u8]` of sector
//! bytes, so the raw fuzz input is fed at offset 0 to every entry point.

use libfuzzer_sys::fuzz_target;

use bdinfo_rs_core::vfs::udf::cs0::{decode_dchars, decode_dstring};
use bdinfo_rs_core::vfs::udf::descriptor::{Avdp, Fsd, Lvd, PartitionDescriptor};
use bdinfo_rs_core::vfs::udf::fid::{Fid, parse_directory};
use bdinfo_rs_core::vfs::udf::icb::FileEntry;
use bdinfo_rs_core::vfs::udf::{ExtentAd, LbAddr, LongAd, ShortAd, Tag};

fuzz_target!(|data: &[u8]| {
    // Address / extent primitives.
    let _ = Tag::parse(data, 0);
    let _ = ExtentAd::parse(data, 0);
    let _ = LbAddr::parse(data, 0);
    let _ = LongAd::parse(data, 0);
    let _ = ShortAd::parse(data, 0);
    // Volume-level descriptors.
    let _ = Avdp::parse(data);
    let _ = Lvd::parse(data);
    let _ = PartitionDescriptor::parse(data);
    let _ = Fsd::parse(data);
    // File Entry / Extended File Entry + allocation extents.
    let _ = FileEntry::parse(data);
    // File Identifier Descriptors + directory enumeration.
    let _ = Fid::parse(data, 0);
    let _ = parse_directory(data);
    // OSTA CS0 name decoding.
    let _ = decode_dchars(data);
    let _ = decode_dstring(data);
});
