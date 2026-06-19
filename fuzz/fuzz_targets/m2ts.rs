#![no_main]
//! Fuzz target: the M2TS demuxer — `TsStreamFile::scan` — over the untrusted bytes
//! of a `*.m2ts` **or `*.ssif`** transport stream. Amplifies the no-panic /
//! no-out-of-bounds contract that the `scan_never_panics_on_arbitrary_bytes`
//! proptest holds on Windows; here it runs adversarially on nightly/Linux (see
//! fuzz/README.md).
//!
//! `scan` is also the SSIF (3D) de-interleaving engine: a `*.ssif` is just
//! packet-aligned base/dependent extents demuxed by this very state machine, so
//! this target covers the interleaved framing too (the `ssif_*` seed is a
//! multi-extent packet stream). `TsStreamFile::scan_source` merely *selects* the
//! `.ssif` over the `.m2ts` — it parses no bytes of its own — so the only untrusted
//! parsing surface it reaches is exactly this `scan`.
//!
//! The fuzz bytes are the packet stream; a single dummy playlist (with one clip
//! named to match) lets the demux run its full state machine and the bitrate
//! accumulation. The file name is fixed (irrelevant to the parse).
//!
//! After the scan, every demuxed diagnostics list is run through the chapter
//! walker (`walk_chapters`) — the downstream consumer of the per-frame data —
//! so its no-panic/termination contract is amplified over adversarial
//! scan-derived markers, intervals, and byte counts too.

use std::collections::BTreeMap;
use std::io::Cursor;

use bdinfo_rs_core::bdrom::chapters::{ChapterClip, walk_chapters};
use bdinfo_rs_core::bdrom::clpi::TsStreamClip;
use bdinfo_rs_core::bdrom::m2ts::TsStreamFile;
use bdinfo_rs_core::bdrom::mpls::TsPlaylistFile;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut playlists = [TsPlaylistFile {
        file_type: "MPLS0300".to_owned(),
        name: "00000.MPLS".to_owned(),
        mvc_base_view_r: false,
        chapters: Vec::new(),
        playlist_streams: BTreeMap::new(),
        streams: BTreeMap::new(),
        angle_streams: Vec::new(),
        stream_clips: vec![TsStreamClip {
            name: "00000.M2TS".to_owned(),
            time_in: 0.0,
            time_out: 1.0e9,
            ..TsStreamClip::default()
        }],
        angle_count: 0,
    }];
    let mut file = TsStreamFile::new("00000.m2ts");
    let mut cursor = Cursor::new(data);
    let _ = file.scan(&mut cursor, &mut playlists, true);

    // Chapter-walk whatever the scan demuxed: hostile per-frame markers and
    // intervals must neither panic nor hang the walker.
    for diags in file.stream_diagnostics.values() {
        let total = diags.last().map_or(1.0, |d| d.marker);
        let clips = [ChapterClip {
            angle_index: 0,
            time_in: 0.0,
            relative_time_in: 0.0,
            diagnostics: Some(diags.as_slice()),
        }];
        let _ = walk_chapters(&[0.0, total / 2.0], total, &clips);
    }
});
