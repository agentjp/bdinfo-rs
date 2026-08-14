//! The measured tallies of a scan still in flight.
//!
//! The measurement pass accumulates its per-clip and per-stream numbers into
//! the playlists as it demuxes, but the demux holds those playlists borrowed
//! for the length of a stream file, so nothing outside it can read the numbers
//! while they move. A caller that installs a measured observer
//! ([`ScanObservers::with_measured`](super::disc::ScanObservers::with_measured))
//! is handed an owned [`MeasuredSnapshot`] instead — a copy of the tallies as
//! they stood at one point of the pass, which the caller can keep, send to
//! another thread, or drop.
//!
//! Two properties make the snapshots composable into a live display:
//!
//! * Each covers only the playlists that play the stream file it was taken over, since only those
//!   can have moved — a surface keeps its last known values for the rest.
//! * Within one measurement pass the tallies only grow, so a cell a snapshot raises is never walked
//!   back by a later one.
//!
//! Same numbers, earlier: every value here is one the finished scan reports
//! too, and each field names the summary field it is the live form of. A
//! display that ticks these cells during the scan and takes the summaries at
//! the end therefore never jumps at the handover.

use super::mpls::TsPlaylistFile;
use crate::primitives::Pid;
use crate::stream::TsStream;

/// One owned snapshot of what a measurement pass has tallied so far.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredSnapshot {
    /// The stream file the pass was demuxing, upper-cased (`00011.M2TS`).
    pub file: String,
    /// The playlists that sequence that file, sorted by name (ordinal) like
    /// [`BdRom::playlists`](super::disc::BdRom::playlists). Empty when no
    /// playlist plays the file — a `*.m2ts` no playlist references is still
    /// scanned, and moves nothing.
    pub playlists: Vec<MeasuredPlaylist>,
}

/// One playlist's tallies within a [`MeasuredSnapshot`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredPlaylist {
    /// The upper-cased playlist file name, e.g. `00000.MPLS`.
    pub name: String,
    /// Packet-derived bytes over every clip the playlist sequences, angle
    /// clips included — the live form of
    /// [`PlaylistSummary::total_angle_packet_size`](super::disc::PlaylistSummary::total_angle_packet_size).
    pub measured_bytes: u64,
    /// The sequenced clips' own tallies, in playlist order and angle clips
    /// included — one entry per row of
    /// [`PlaylistSummary::clips`](super::disc::PlaylistSummary::clips), in
    /// that same order.
    pub clips: Vec<MeasuredClip>,
    /// The measured rates of the playlist's presented streams, one entry per
    /// PID and camera angle. Empty until the scan resolves the playlist's
    /// presented streams, which a playlist with no reference clip never gets.
    pub streams: Vec<MeasuredStream>,
}

/// One sequenced clip's tallies within a [`MeasuredSnapshot`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredClip {
    /// The clip's stream-file name, e.g. `00011.M2TS`. Only the clips carrying
    /// the snapshot's [`file`](MeasuredSnapshot::file) can have moved since the
    /// previous snapshot; a playlist that plays one file twice has one entry
    /// per play item.
    pub name: String,
    /// The camera angle this clip plays; `0` is the main angle.
    pub angle_index: i32,
    /// Packet-derived bytes measured for this clip — the live form of
    /// [`ClipSummary::packet_size`](super::disc::ClipSummary::packet_size).
    pub measured_bytes: u64,
}

/// One presented stream's measured rates within a [`MeasuredSnapshot`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredStream {
    /// The stream's packet identifier.
    pub pid: Pid,
    /// The camera angle whose copy of the stream this is: `0` for the main
    /// row, `1..` for the per-angle clones a video stream gets. Pairs with
    /// [`StreamSummary::angle_index`](super::disc::StreamSummary::angle_index),
    /// so `(pid, angle_index)` names exactly one report row.
    pub angle_index: usize,
    /// Bits per second over the pass's demuxed seconds — the live form of
    /// [`StreamSummary::bitrate`](super::disc::StreamSummary::bitrate). A
    /// constant-bitrate stream keeps its declared rate here from the start; a
    /// variable-bitrate one is recomputed as the pass proceeds.
    pub bitrate: i64,
    /// Bits per second over the stream's own presentation windows — the live
    /// form of
    /// [`StreamSummary::active_bitrate`](super::disc::StreamSummary::active_bitrate).
    pub active_bitrate: i64,
}

/// The measured size in bytes of `packets` source packets.
///
/// A BDAV source packet is 192 bytes — a 188-byte transport packet behind its
/// 4-byte `TP_extra_header` — so a packet count converts to a byte count by
/// this one multiplication, applied here to the live tallies and by
/// [`ClipSummary::packet_size`](super::disc::ClipSummary::packet_size) to a
/// finished scan's summaries. Sharing it is what makes a cell that ticks during
/// the scan land exactly on the number the report prints.
pub(crate) const fn packet_size(packets: u64) -> u64 {
    packets.wrapping_mul(192)
}

/// The snapshot of `playlists` taken over the stream file `file`.
///
/// `file` is compared to the clip names exactly, so it must be the upper-cased
/// stream-file name the demux attributes its windows under — the same match
/// the demux itself makes when it pre-filters the playlists a clip can touch.
pub(crate) fn snapshot(file: &str, playlists: &[TsPlaylistFile]) -> MeasuredSnapshot {
    MeasuredSnapshot {
        file: file.to_owned(),
        playlists: playlists
            .iter()
            .filter_map(|playlist| playlist_tallies(file, playlist))
            .collect(),
    }
}

/// One playlist's entry in a [`snapshot`] over `file`, or `None` when the
/// playlist does not sequence that file: the demux attributes windows only to
/// the playlists that play the clip, so the others cannot have moved and a
/// surface holding their last values stays right.
fn playlist_tallies(file: &str, playlist: &TsPlaylistFile) -> Option<MeasuredPlaylist> {
    if !playlist.stream_clips.iter().any(|clip| clip.name == file) {
        return None;
    }
    let clips: Vec<MeasuredClip> = playlist
        .stream_clips
        .iter()
        .map(|clip| MeasuredClip {
            name: clip.name.clone(),
            angle_index: clip.angle_index,
            measured_bytes: packet_size(clip.packet_count),
        })
        .collect();
    let measured_bytes =
        clips.iter().fold(0_u64, |sum, clip| sum.saturating_add(clip.measured_bytes));
    let mut streams: Vec<MeasuredStream> =
        playlist.streams.iter().map(|(pid, stream)| measured_stream(*pid, 0, stream)).collect();
    for (index, angle) in playlist.angle_streams.iter().enumerate() {
        streams.extend(
            angle
                .iter()
                .map(|(pid, stream)| measured_stream(*pid, index.saturating_add(1), stream)),
        );
    }
    Some(MeasuredPlaylist { name: playlist.name.clone(), measured_bytes, clips, streams })
}

/// One resolved `stream`'s rates, tagged with its PID and camera angle.
const fn measured_stream(pid: u16, angle_index: usize, stream: &TsStream) -> MeasuredStream {
    MeasuredStream {
        pid: Pid::new(pid),
        angle_index,
        bitrate: stream.base().bit_rate,
        active_bitrate: stream.base().active_bit_rate,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{MeasuredClip, MeasuredStream, packet_size, snapshot};
    use crate::bdrom::clpi::TsStreamClip;
    use crate::bdrom::mpls::TsPlaylistFile;
    use crate::primitives::Pid;
    use crate::stream::{TsStream, TsStreamType, TsVideoStream};

    /// A playlist named `name` sequencing `clips` as `(file, angle_index,
    /// packet_count)` triples, with no resolved streams.
    fn playlist(name: &str, clips: &[(&str, i32, u64)]) -> TsPlaylistFile {
        let mut playlist = TsPlaylistFile {
            file_type: "MPLS0300".to_owned(),
            name: name.to_owned(),
            mvc_base_view_r: false,
            chapters: Vec::new(),
            playlist_streams: BTreeMap::new(),
            streams: BTreeMap::new(),
            angle_streams: Vec::new(),
            stream_clips: Vec::new(),
            angle_count: 0,
        };
        for &(file, angle_index, packet_count) in clips {
            playlist.stream_clips.push(TsStreamClip {
                name: file.to_owned(),
                angle_index,
                packet_count,
                ..TsStreamClip::default()
            });
        }
        playlist
    }

    /// A video stream carrying `bit_rate` and `active_bit_rate`.
    fn video(bit_rate: i64, active_bit_rate: i64) -> TsStream {
        let mut stream = TsVideoStream::default();
        stream.base.stream_type = TsStreamType::AvcVideo;
        stream.base.bit_rate = bit_rate;
        stream.base.active_bit_rate = active_bit_rate;
        TsStream::Video(stream)
    }

    #[test]
    fn a_snapshot_covers_only_the_playlists_that_play_the_file() {
        let playlists = vec![
            playlist("00000.MPLS", &[("00011.M2TS", 0, 10)]),
            playlist("00001.MPLS", &[("00022.M2TS", 0, 20)]),
            playlist("00002.MPLS", &[("00033.M2TS", 0, 30), ("00011.M2TS", 0, 40)]),
        ];
        let taken = snapshot("00011.M2TS", &playlists);
        assert_eq!(taken.file, "00011.M2TS");
        let named: Vec<&str> = taken.playlists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(named, ["00000.MPLS", "00002.MPLS"], "the playlist between them plays no 00011");
        // A file no playlist sequences yields a snapshot with nothing in it.
        assert!(snapshot("00099.M2TS", &playlists).playlists.is_empty());
    }

    #[test]
    fn a_playlists_bytes_are_its_clips_bytes_over_every_angle() {
        // Two play items of the main angle plus one angle clip: the playlist
        // total counts all three, and each row carries its own count.
        let playlists =
            vec![playlist("00000.MPLS", &[("00011.M2TS", 0, 10), ("00011.M2TS", 1, 3)])];
        let taken = snapshot("00011.M2TS", &playlists);
        let first = taken.playlists.first().expect("the playlist plays the file");
        assert_eq!(first.measured_bytes, packet_size(13));
        assert_eq!(
            first.clips,
            [
                MeasuredClip {
                    name: "00011.M2TS".to_owned(),
                    angle_index: 0,
                    measured_bytes: packet_size(10),
                },
                MeasuredClip {
                    name: "00011.M2TS".to_owned(),
                    angle_index: 1,
                    measured_bytes: packet_size(3),
                },
            ]
        );
    }

    #[test]
    fn the_clip_rows_cover_the_whole_playlist_not_just_the_scanned_file() {
        // The pass measures one file at a time, so the entries for the clips
        // it is not on carry whatever earlier files left them at.
        let playlists =
            vec![playlist("00000.MPLS", &[("00033.M2TS", 0, 5), ("00011.M2TS", 0, 10)])];
        let taken = snapshot("00011.M2TS", &playlists);
        let first = taken.playlists.first().expect("the playlist plays the file");
        let rows: Vec<(&str, u64)> =
            first.clips.iter().map(|clip| (clip.name.as_str(), clip.measured_bytes)).collect();
        assert_eq!(rows, [("00033.M2TS", packet_size(5)), ("00011.M2TS", packet_size(10))]);
        assert_eq!(first.measured_bytes, packet_size(15));
    }

    #[test]
    fn every_presented_stream_reports_its_rates_under_its_own_angle() {
        let mut playlists = vec![playlist("00000.MPLS", &[("00011.M2TS", 0, 10)])];
        let first = playlists.first_mut().expect("one playlist");
        first.streams = BTreeMap::from([(0x1011, video(7, 8)), (0x1100, video(5, 6))]);
        first.angle_streams = vec![BTreeMap::from([(0x1011, video(3, 4))])];
        let taken = snapshot("00011.M2TS", &playlists);
        let measured = taken.playlists.first().expect("the playlist plays the file");
        assert_eq!(
            measured.streams,
            [
                MeasuredStream {
                    pid: Pid::new(0x1011),
                    angle_index: 0,
                    bitrate: 7,
                    active_bitrate: 8,
                },
                MeasuredStream {
                    pid: Pid::new(0x1100),
                    angle_index: 0,
                    bitrate: 5,
                    active_bitrate: 6,
                },
                MeasuredStream {
                    pid: Pid::new(0x1011),
                    angle_index: 1,
                    bitrate: 3,
                    active_bitrate: 4,
                },
            ]
        );
    }

    #[test]
    fn an_unresolved_playlist_reports_no_streams() {
        // A playlist the scan found no reference clip for keeps empty stream
        // maps, so its snapshot carries clips and no rates.
        let playlists = vec![playlist("00000.MPLS", &[("00011.M2TS", 0, 10)])];
        let taken = snapshot("00011.M2TS", &playlists);
        let first = taken.playlists.first().expect("the playlist plays the file");
        assert!(first.streams.is_empty());
        assert_eq!(first.clips.len(), 1);
    }

    #[test]
    fn a_packet_count_converts_at_192_bytes_a_packet() {
        assert_eq!(packet_size(0), 0);
        assert_eq!(packet_size(1), 192);
        assert_eq!(packet_size(1000), 192_000);
    }
}
