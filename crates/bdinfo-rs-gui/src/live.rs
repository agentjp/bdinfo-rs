//! The measured cells a scan in flight ticks into the grids.
//!
//! A measured scan installs an observer
//! ([`ScanObservers::with_measured`](bdinfo_rs_core::bdrom::disc::ScanObservers::with_measured))
//! and is handed an owned [`MeasuredSnapshot`] as each demuxed chunk lands. The
//! worker thread turns each one into the per-playlist [`updates`] below and
//! sends them to the shell, which merges them into the state the playlist table
//! and the two panes read while `Stage::Scanning`
//! ([`crate::flow`]). Only the cells: nothing here changes which rows exist,
//! their order, or the rendered report — the finished scan's summaries replace
//! these values wholesale.
//!
//! A snapshot covers only the playlists that play the stream file it was taken
//! over, so the merge is per playlist and every other playlist keeps its last
//! known values. The tallies only grow within one pass, so a cell never walks
//! backwards.

use bdinfo_rs_core::bdrom::measured::MeasuredSnapshot;
use bdinfo_rs_core::primitives::Pid;

/// One playlist's live cells, as one snapshot left them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LivePlaylist {
    /// Packet-derived bytes over every clip the playlist sequences, angle clips
    /// included — the playlist table's `Measured Bytes` cell.
    pub measured_bytes: u64,
    /// The same count per sequenced clip, in the playlist's own clip order
    /// (angle clips included, one entry per play item) — the Stream Files
    /// pane's `Measured Size` column, whose rows are in that same order.
    pub clips: Vec<u64>,
    /// The presented streams' measured rates — the Streams pane's `Bit Rate`
    /// column. The pane has no active-rate column, so the snapshot's
    /// active-bitrate half is dropped here rather than carried unread.
    pub streams: Vec<LiveStream>,
}

/// One presented stream's live rate, tagged with what names its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveStream {
    /// The stream's packet identifier.
    pub pid: Pid,
    /// The camera angle whose copy of the stream this is: `0` for the main
    /// presentation row, `1..` for a video stream's per-angle copies. With
    /// `pid` it names exactly one row of the Streams pane.
    pub angle_index: usize,
    /// Bits per second over the pass's demuxed seconds so far.
    pub bitrate: i64,
}

impl LivePlaylist {
    /// The live measured bytes of the clip the Stream Files pane shows at
    /// `index`, or `None` when this playlist's snapshot carries no such row
    /// (nothing has been measured for it yet).
    #[must_use]
    pub fn clip_bytes(&self, index: usize) -> Option<u64> {
        self.clips.get(index).copied()
    }

    /// The live rate of the stream `pid` presents on camera angle
    /// `angle_index`, or `None` when the pass has not resolved that row yet.
    ///
    /// Looked up by the pair, never by position: the snapshot orders its
    /// streams main-presentation-first and then per angle, which is not the
    /// order the pane's rows are in.
    #[must_use]
    pub fn bitrate(&self, pid: Pid, angle_index: usize) -> Option<i64> {
        self.streams
            .iter()
            .find(|stream| stream.pid == pid && stream.angle_index == angle_index)
            .map(|stream| stream.bitrate)
    }
}

/// The per-playlist cells `snapshot` carries, each under its playlist name.
///
/// The shell's worker thread runs this, so what crosses the channel is the
/// handful of numbers the grids draw rather than the whole snapshot.
#[must_use]
pub fn updates(snapshot: MeasuredSnapshot) -> Vec<(String, LivePlaylist)> {
    snapshot
        .playlists
        .into_iter()
        .map(|playlist| {
            let live = LivePlaylist {
                measured_bytes: playlist.measured_bytes,
                clips: playlist.clips.into_iter().map(|clip| clip.measured_bytes).collect(),
                streams: playlist
                    .streams
                    .into_iter()
                    .map(|stream| LiveStream {
                        pid: stream.pid,
                        angle_index: stream.angle_index,
                        bitrate: stream.bitrate,
                    })
                    .collect(),
            };
            (playlist.name, live)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use bdinfo_rs_core::primitives::Pid;

    use super::{LivePlaylist, LiveStream};

    /// A playlist carrying one clip row and two stream rows of the same PID on
    /// different angles.
    fn live() -> LivePlaylist {
        LivePlaylist {
            measured_bytes: 960,
            clips: vec![768, 192],
            streams: vec![
                LiveStream { pid: Pid::new(0x1011), angle_index: 0, bitrate: 19_000 },
                LiveStream { pid: Pid::new(0x1011), angle_index: 1, bitrate: 7_000 },
                LiveStream { pid: Pid::new(0x1100), angle_index: 0, bitrate: 640_000 },
            ],
        }
    }

    #[test]
    fn clip_bytes_reads_by_row_position() {
        let live = live();
        assert_eq!(live.clip_bytes(0), Some(768));
        assert_eq!(live.clip_bytes(1), Some(192));
        assert_eq!(live.clip_bytes(2), None, "a row the snapshot does not carry");
    }

    #[test]
    fn a_rate_is_found_by_pid_and_angle_together() {
        // Same PID, different angles: keying on the PID alone would hand the
        // main row an extra angle's rate.
        let live = live();
        assert_eq!(live.bitrate(Pid::new(0x1011), 0), Some(19_000));
        assert_eq!(live.bitrate(Pid::new(0x1011), 1), Some(7_000));
        assert_eq!(live.bitrate(Pid::new(0x1100), 0), Some(640_000));
        assert_eq!(live.bitrate(Pid::new(0x1100), 1), None, "that PID has no angle copy");
        assert_eq!(live.bitrate(Pid::new(0x1234), 0), None, "an unresolved stream");
    }

    #[test]
    fn an_empty_playlist_answers_nothing() {
        let empty = LivePlaylist::default();
        assert_eq!(empty.measured_bytes, 0);
        assert_eq!(empty.clip_bytes(0), None);
        assert_eq!(empty.bitrate(Pid::new(0x1011), 0), None);
    }
}
