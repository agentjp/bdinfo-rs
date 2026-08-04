#![no_main]
//! Differential fuzz target: the two M2TS scan strategies over one input —
//! `bdinfo_rs_core::bdrom::m2ts::TsStreamFile::scan_threaded` and
//! `TsStreamFile::scan_sequential`, run over the same bytes and asserted to
//! produce the same output.
//!
//! `TsStreamFile::scan` dispatches to the threaded strategy wherever threads
//! exist and to the sequential one on `wasm32-unknown-unknown`, so the
//! command-line tool runs the first and the browser package the second.
//! `scan_sequential`'s doc states their equivalence as a contract — same
//! chunks, same order, same finish condition, therefore the same demux output.
//! This target is what holds it: a divergence means the two report different
//! stream statistics for one disc, on numbers otherwise pinned only by fixture
//! tests that run the threaded strategy alone.
//!
//! Input framing: the bytes are the transport stream, whole — the framing the
//! `m2ts` target reads, which is where this target's seeds come from. The
//! input's first byte doubles as the harness's control byte (bit 0 the full-scan flag,
//! bits 1-2 the read-chunk size, bits 3-7 an injected read failure), which
//! costs the stream nothing: the packet state machine opens with four
//! `TP_extra_header` time-code bytes to discard and consumes them without
//! reading their values, so byte 0 never reaches a parse decision. Every
//! `m2ts` corpus unit is a valid unit here, byte for byte.
//!
//! # What "equal" means here
//!
//! Everything a caller can observe of a finished scan: the `Result`, the
//! demuxed clip (`name`, `size`, `length`, `streams`, `stream_order`,
//! `stream_diagnostics`) and the playlist slice the demux writes per-stream
//! bitrates and per-clip counters into. Compared two ways at once, because
//! neither alone is enough:
//!
//! - the `{:?}` rendering of each, which is **total** — a field added to any of
//!   these public types joins the comparison with no edit here;
//! - the raw bits of every `f64` those renderings reach, which is **exact** —
//!   Rust renders a float as the shortest decimal that round-trips, so a
//!   rendering separates every pair of distinct floats except NaNs, which it
//!   prints as `NaN` whatever their sign and payload.
//!
//! Neither comparison is the derived `PartialEq`: that calls two NaNs unequal
//! and `-0.0` equal to `0.0`, wrong in both directions for a byte-identity
//! claim.
//!
//! Two parts of the state are excluded, and each is a hole in the proof:
//!
//! - `TsStreamFile::interleaved_file` — neither strategy reads or writes it. It
//!   selects the source *before* a scan (`TsStreamFile::scan_source`) and is
//!   `None` for a clip this harness builds.
//! - `TsStreamFile`'s private per-PID demux scratch — unreachable from outside
//!   the crate, not output (the crate releases it once a scan's outputs are
//!   captured), and holding a PES reassembly window of up to 5 MiB per PID, so
//!   rendering it per unit would cost more than the two scans do. The in-tree
//!   `sequential_demux_matches_the_threaded_demux` proptest, this target's
//!   mirror, compares its packet total, byte total and peak transfer length
//!   across chunk sizes on a fixed stream.
//!
//! The `cancel` flag stays unset, so `BdError::ScanCancelled` is out of reach
//! here. A cancel raised while a scan runs cannot be compared at all: the
//! threaded strategy's reader runs ahead of its parser, so the chunk a
//! wall-clock cancel lands on differs between the strategies by design, and
//! that difference is not a divergence. Cancellation and read failure leave the
//! shared chunk reader by the same road out of both strategies — the one that
//! abandons the scan without the bitrate tail — and the injected read failure
//! drives it.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::sync::atomic::AtomicBool;

use bdinfo_rs_core::bdrom::clpi::TsStreamClip;
use bdinfo_rs_core::bdrom::m2ts::TsStreamFile;
use bdinfo_rs_core::bdrom::mpls::TsPlaylistFile;
use bdinfo_rs_core::error::BdError;
use libfuzzer_sys::fuzz_target;

/// The read-chunk sizes bits 1-2 of the control byte select between. Every
/// chunk boundary is a point the two strategies must agree at, and the demux
/// carries its packet state across them: 192 is one BDAV packet, 448 always
/// lands mid-packet, 4096 is a page, and 32,768 is larger than most units so
/// the whole stream arrives as a single chunk. The production size (`scan`
/// passes 5 MiB) is deliberately absent: it would make every unit
/// single-chunk — which 32,768 already does — at the cost of zeroing 5 MiB per
/// scan.
const CHUNK_SIZES: [usize; 4] = [192, 448, 4096, 32_768];

/// The stream reader, optionally failing part-way through.
///
/// The failure point is a byte offset rather than a read count precisely
/// because the threaded strategy issues more reads than the sequential one: it
/// reads ahead of its parser, and after an early finish it can read past the
/// point the sequential strategy stopped at. Both consume the same bytes in the
/// same order, so a byte-keyed failure falls at the same place in the stream
/// for both — and where only the reading-ahead strategy reaches it, the
/// contract is that it discards the error, which is then exactly what this
/// target checks.
struct BudgetedReader<'a> {
    data: &'a [u8],
    pos: usize,
    /// Byte offset from which every read fails, or `None` to serve the whole
    /// stream.
    fail_at: Option<usize>,
}

impl Read for BudgetedReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.fail_at.is_some_and(|at| self.pos >= at) {
            return Err(io::Error::other("injected read failure"));
        }
        let end = self.fail_at.map_or(self.data.len(), |at| at.min(self.data.len()));
        let n = end.saturating_sub(self.pos).min(out.len());
        out[..n].copy_from_slice(&self.data[self.pos..self.pos.saturating_add(n)]);
        self.pos = self.pos.saturating_add(n);
        Ok(n)
    }
}

/// The single dummy playlist both scans run against — one clip named to match
/// the demuxed file, which is what makes the per-window bitrate attribution
/// (and so the playlist writes this target compares) run at all. The same
/// playlist the `m2ts` target builds.
fn playlist() -> TsPlaylistFile {
    TsPlaylistFile {
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
    }
}

/// The whole observable outcome of one scan, rendered for comparison: its
/// result, the demuxed clip's public state, and the playlists it wrote into.
fn rendered(
    result: &Result<(), BdError>,
    file: &TsStreamFile,
    playlists: &[TsPlaylistFile],
) -> String {
    format!(
        "result {result:?}\nname {:?}\nsize {}\nlength {}\norder {:?}\n\
         streams {:?}\ndiagnostics {:?}\nplaylists {playlists:?}",
        file.name,
        file.size,
        file.length,
        file.stream_order,
        file.streams,
        file.stream_diagnostics,
    )
}

/// Every `f64` reachable in the compared state, as raw bits — the exactness the
/// rendering above cannot give, since all NaNs render alike. Total over the
/// public types as they stand: `TsStreamFile` carries one, `TsStreamBase` one
/// per registered stream, `TsStreamDiagnostics` two per sample,
/// `TsPlaylistFile` its chapter times plus its three stream collections, and
/// `TsStreamClip` seven plus its own chapter times.
fn float_bits(file: &TsStreamFile, playlists: &[TsPlaylistFile]) -> Vec<u64> {
    let mut bits = vec![file.length.to_bits()];
    for stream in file.streams.values() {
        bits.push(stream.base().packet_seconds.to_bits());
    }
    for diagnostics in file.stream_diagnostics.values() {
        for sample in diagnostics {
            bits.push(sample.marker.to_bits());
            bits.push(sample.interval.to_bits());
        }
    }
    for playlist in playlists {
        bits.extend(playlist.chapters.iter().map(|c| c.to_bits()));
        let maps = [&playlist.playlist_streams, &playlist.streams]
            .into_iter()
            .chain(playlist.angle_streams.iter());
        for streams in maps {
            bits.extend(streams.values().map(|s| s.base().packet_seconds.to_bits()));
        }
        for clip in &playlist.stream_clips {
            bits.extend([
                clip.time_in.to_bits(),
                clip.time_out.to_bits(),
                clip.relative_time_in.to_bits(),
                clip.relative_time_out.to_bits(),
                clip.length.to_bits(),
                clip.relative_length.to_bits(),
                clip.packet_seconds.to_bits(),
            ]);
            bits.extend(clip.chapters.iter().map(|c| c.to_bits()));
        }
    }
    bits
}

fuzz_target!(|data: &[u8]| {
    let Some(&control) = data.first() else { return };
    let is_full_scan = control & 0x01 != 0;
    let chunk_size = CHUNK_SIZES[usize::from(control >> 1) & 0x03];
    // Bits 3-7 place the injected read failure as a thirty-second of the
    // stream: 0 serves the whole of it, and 1..=31 fail strictly inside it.
    let fail_step = usize::from(control >> 3);
    let fail_at = (fail_step > 0).then(|| data.len().saturating_mul(fail_step) / 32);

    let clean = playlist();
    let mut threaded_playlists = [clean.clone()];
    let mut threaded = TsStreamFile::new("00000.m2ts");
    let threaded_result = threaded.scan_threaded(
        &mut BudgetedReader { data, pos: 0, fail_at },
        &mut threaded_playlists,
        is_full_scan,
        chunk_size,
        &AtomicBool::new(false),
        &mut |_, _| {},
    );

    let mut sequential_playlists = [clean];
    let mut sequential = TsStreamFile::new("00000.m2ts");
    let sequential_result = sequential.scan_sequential(
        &mut BudgetedReader { data, pos: 0, fail_at },
        &mut sequential_playlists,
        is_full_scan,
        chunk_size,
        &AtomicBool::new(false),
        &mut |_, _| {},
    );

    assert_eq!(
        rendered(&threaded_result, &threaded, &threaded_playlists),
        rendered(&sequential_result, &sequential, &sequential_playlists),
        "the threaded and sequential scan strategies disagree \
         (chunk {chunk_size}, full scan {is_full_scan}, read failure at {fail_at:?})",
    );
    assert_eq!(
        float_bits(&threaded, &threaded_playlists),
        float_bits(&sequential, &sequential_playlists),
        "the strategies' floats differ in bits while rendering alike \
         (chunk {chunk_size}, full scan {is_full_scan}, read failure at {fail_at:?})",
    );
});
