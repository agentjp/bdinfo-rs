//! Tier A — the master-detail pane view-models.
//!
//! When a playlist row is the active (highlighted) one, the lower two panes show
//! its **stream files** and its **streams / codecs** — exactly the classic
//! `BDInfo` master-detail flow. These pure constructors turn the active
//! [`PlaylistSummary`]'s clips and streams into pre-formatted display rows, so
//! the iced view is a thin projection: no widgets, no IO. The codec
//! `description` is the same string the locked report prints — built by the core,
//! reused verbatim here.

use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary, StreamSummary};

use crate::model::{group_n0, table_length};

/// One "Stream Files" pane row: a clip of the selected playlist, pre-formatted.
///
/// Columns: Stream File / Index / Length / Size. `size` is the measured
/// packet-derived size (`0` until the measured scan fills it) — the same value
/// the report's `FILES:` section prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFileRow {
    /// The clip's display name (the interleaved `*.ssif`, else the `*.m2ts`).
    pub file: String,
    /// The 1-based clip index, counting the main angle's clips.
    pub index: String,
    /// `hh:mm:ss` clip length.
    pub length: String,
    /// Measured (packet-derived) size, thousands-grouped (`0` until measured).
    pub size: String,
}

/// One "Streams" pane row: a presented stream of the selected playlist.
///
/// Columns: Codec / Language / Bit Rate / Description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecRow {
    /// The long codec name, e.g. `MPEG-4 AVC Video`.
    pub codec: String,
    /// The language name (empty when the stream has none).
    pub language: String,
    /// The bit rate as `N kbps`, or `-` until the measured scan fills it.
    pub bitrate: String,
    /// The codec description (resolution/HDR for video, channels/rate for audio).
    pub description: String,
    /// Whether this stream is hidden — the table marks it with a `*`.
    pub hidden: bool,
}

/// Builds the "Stream Files" rows for `playlist` — one per clip, the index
/// counting the main (angle-0) clips.
#[must_use]
pub fn stream_file_rows(playlist: &PlaylistSummary) -> Vec<StreamFileRow> {
    let mut rows = Vec::new();
    let mut index: usize = 0;
    for clip in &playlist.clips {
        if clip.angle_index == 0 {
            index = index.saturating_add(1);
        }
        rows.push(stream_file_row(clip, index));
    }
    rows
}

/// Formats one clip into its stream-file row.
fn stream_file_row(clip: &ClipSummary, index: usize) -> StreamFileRow {
    StreamFileRow {
        file: clip.display_name.clone(),
        index: index.to_string(),
        length: table_length(clip.length),
        size: group_n0(clip.packet_size()),
    }
}

/// Builds the "Streams" (codec) rows for `playlist` — one per presented stream,
/// in the playlist's own stream order.
#[must_use]
pub fn codec_rows(playlist: &PlaylistSummary) -> Vec<CodecRow> {
    playlist.streams.iter().map(codec_row).collect()
}

/// Formats one stream into its codec row.
fn codec_row(stream: &StreamSummary) -> CodecRow {
    CodecRow {
        codec: stream.codec_name.clone(),
        language: stream.language_name.clone(),
        bitrate: bitrate_cell(stream.bitrate),
        description: stream.description.clone(),
        hidden: stream.is_hidden,
    }
}

/// The bit-rate cell: `N kbps` (thousands-grouped), or `-` while unmeasured
/// (a non-positive bits/s).
fn bitrate_cell(bits_per_second: i64) -> String {
    let kbps = bits_per_second.checked_div(1000).unwrap_or(0);
    if kbps <= 0 {
        "-".to_owned()
    } else {
        format!("{} kbps", group_n0(u64::try_from(kbps).unwrap_or(0)))
    }
}

#[cfg(test)]
mod tests {
    use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary, StreamSummary};
    use bdinfo_rs_core::primitives::Pid;
    use bdinfo_rs_core::stream::TsStreamType;

    use super::{CodecRow, StreamFileRow, bitrate_cell, codec_rows, stream_file_rows};

    /// A clip carrying the fields the stream-file row reads.
    fn clip(name: &str, angle_index: i32, length: f64, packet_count: u64) -> ClipSummary {
        ClipSummary {
            name: name.to_owned(),
            display_name: name.to_owned(),
            angle_index,
            relative_time_in: 0.0,
            length,
            payload_bytes: 0,
            packet_count,
            packet_seconds: 0.0,
            file_seconds: 0.0,
            streams: Vec::new(),
        }
    }

    /// A stream carrying the fields the codec row reads; the rest are defaulted.
    fn stream(codec_name: &str, language: &str, bitrate: i64, description: &str) -> StreamSummary {
        StreamSummary {
            pid: Pid::new(0x1011),
            stream_type: TsStreamType::default(),
            codec_short_name: String::new(),
            codec_name: codec_name.to_owned(),
            codec_alt_name: "",
            bitrate,
            active_bitrate: 0,
            language_name: language.to_owned(),
            language_code: String::new(),
            description: description.to_owned(),
            full_description: description.to_owned(),
            channel_description: String::new(),
            sample_rate: 0,
            bit_depth: 0,
            channel_count: 0,
            height: 0,
            angle_index: 0,
            is_hidden: false,
            ssif_only: false,
        }
    }

    /// A two-clip, two-stream playlist (the fields the panes read).
    fn playlist() -> PlaylistSummary {
        PlaylistSummary {
            name: "00000.MPLS".to_owned(),
            total_length: 130.0,
            file_size: 0,
            interleaved_file_size: 0,
            chapter_count: 0,
            stream_count: 2,
            angle_count: 0,
            has_loops: false,
            streams: vec![
                stream("MPEG-4 AVC Video", "", 0, "1080p / 23.976 fps / 16:9"),
                stream("DTS-HD Master Audio", "English", 3_948_000, "5.1 / 48 kHz / 24-bit"),
            ],
            clips: vec![clip("00000.M2TS", 0, 100.0, 1000), clip("00001.M2TS", 0, 30.0, 0)],
            chapters: Vec::new(),
        }
    }

    #[test]
    fn stream_files_carry_index_length_and_measured_size() {
        let rows = stream_file_rows(&playlist());
        assert_eq!(
            rows,
            [
                StreamFileRow {
                    file: "00000.M2TS".to_owned(),
                    index: "1".to_owned(),
                    length: "00:01:40".to_owned(),
                    // 1000 packets * 192 bytes = 192,000.
                    size: "192,000".to_owned(),
                },
                StreamFileRow {
                    file: "00001.M2TS".to_owned(),
                    index: "2".to_owned(),
                    length: "00:00:30".to_owned(),
                    // Nothing demuxed yet → 0.
                    size: "0".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn an_angle_clip_does_not_bump_the_index() {
        // 00000 (main) = 1, 00001 (angle 1) shares index 1, 00002 (main) = 2.
        let mut p = playlist();
        p.clips = vec![
            clip("00000.M2TS", 0, 100.0, 0),
            clip("00001.M2TS", 1, 100.0, 0),
            clip("00002.M2TS", 0, 50.0, 0),
        ];
        let indices: Vec<_> = stream_file_rows(&p).into_iter().map(|row| row.index).collect();
        assert_eq!(indices, ["1", "1", "2"]);
    }

    #[test]
    fn codec_rows_carry_codec_language_bitrate_description() {
        let rows = codec_rows(&playlist());
        assert_eq!(
            rows,
            [
                CodecRow {
                    codec: "MPEG-4 AVC Video".to_owned(),
                    language: String::new(),
                    bitrate: "-".to_owned(), // unmeasured
                    description: "1080p / 23.976 fps / 16:9".to_owned(),
                    hidden: false,
                },
                CodecRow {
                    codec: "DTS-HD Master Audio".to_owned(),
                    language: "English".to_owned(),
                    bitrate: "3,948 kbps".to_owned(),
                    description: "5.1 / 48 kHz / 24-bit".to_owned(),
                    hidden: false,
                },
            ]
        );
    }

    #[test]
    fn the_bitrate_cell_is_a_dash_until_measured() {
        assert_eq!(bitrate_cell(0), "-");
        assert_eq!(bitrate_cell(-1), "-");
        assert_eq!(bitrate_cell(192_000), "192 kbps");
        assert_eq!(bitrate_cell(70_000_000), "70,000 kbps");
    }
}
