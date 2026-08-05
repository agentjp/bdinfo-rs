//! The master-detail pane view-models.
//!
//! When a playlist row is the active (highlighted) one, the lower two panes show
//! its **stream files** and its **streams / codecs** — exactly the classic
//! `BDInfo` master-detail flow. These pure constructors turn the active
//! [`PlaylistSummary`]'s clips and streams into pre-formatted display rows, so
//! the iced view is a thin projection: no widgets, no IO. The codec
//! `description` is the same string the locked report prints — built by the core,
//! reused verbatim here.

use bdinfo_rs_core::bdrom::chapters::seconds_to_ticks;
use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary, StreamSummary};
use bdinfo_rs_core::report::text;

use crate::model::byte_cell;

/// One "Stream File" pane row: a clip of the selected playlist, pre-formatted.
///
/// Columns: Stream File / Index / Length / Estimated Size / Measured Size — the
/// same five `BDInfo` shows. `estimated` is the clip's on-disk size
/// ([`ClipSummary::estimated_bytes`]); `measured` is the packet-derived size the
/// demux attributed to the clip. Either is `-` until it is known (no file on
/// disk / nothing demuxed yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFileRow {
    /// The clip's display name (the interleaved `*.ssif`, else the `*.m2ts`),
    /// with a ` (N)` suffix for an extra-angle clip.
    pub file: String,
    /// The 1-based clip index, counting the main angle's clips.
    pub index: String,
    /// `hh:mm:ss` clip length.
    pub length: String,
    /// The clip's on-disk (estimated) size, thousands-grouped, or `-` when the
    /// file is absent.
    pub estimated: String,
    /// The measured (packet-derived) size, thousands-grouped, or `-` until the
    /// measured scan fills it.
    pub measured: String,
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
///
/// `human_sizes` switches the size cells to the human-readable form —
/// `BDInfo` applies its `SizeFormatHR` setting to this grid exactly as to the
/// playlist table.
#[must_use]
pub fn stream_file_rows(playlist: &PlaylistSummary, human_sizes: bool) -> Vec<StreamFileRow> {
    let mut rows = Vec::new();
    let mut index: usize = 0;
    for clip in &playlist.clips {
        if clip.angle_index == 0 {
            index = index.saturating_add(1);
        }
        rows.push(stream_file_row(clip, index, human_sizes));
    }
    rows
}

/// Formats one clip into its stream-file row, mirroring `BDInfo`: an extra-angle
/// clip gets a ` (N)` suffix, and a size that is not yet known (no file /
/// nothing demuxed) shows as `-`.
fn stream_file_row(clip: &ClipSummary, index: usize, human_sizes: bool) -> StreamFileRow {
    let file = if clip.angle_index > 0 {
        format!("{} ({})", clip.display_name, clip.angle_index)
    } else {
        clip.display_name.clone()
    };
    StreamFileRow {
        file,
        index: index.to_string(),
        length: text::time_hh_short(seconds_to_ticks(clip.length)),
        estimated: size_cell(clip.estimated_bytes().unwrap_or(0), human_sizes),
        measured: size_cell(clip.packet_size(), human_sizes),
    }
}

/// A byte-size cell — grouped or human-readable per the toggle — or `-` when
/// the size is not yet known.
fn size_cell(bytes: u64, human: bool) -> String {
    if bytes > 0 { byte_cell(bytes, human) } else { "-".to_owned() }
}

/// Builds the "Streams" (codec) rows for `playlist` — one per presented stream,
/// in the playlist's own stream order.
#[must_use]
pub fn codec_rows(playlist: &PlaylistSummary) -> Vec<CodecRow> {
    playlist.streams.iter().map(codec_row).collect()
}

/// Formats one stream into its codec row. Uses `full_description` — the same
/// string the locked report prints (video profile/level, audio kbps / bit depth
/// / embedded core) — so the pane matches the report and the original `BDInfo`.
fn codec_row(stream: &StreamSummary) -> CodecRow {
    CodecRow {
        codec: stream.codec_name.clone(),
        language: stream.language_name.clone(),
        bitrate: bitrate_cell(stream.bitrate),
        description: stream.full_description.clone(),
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
        format!("{} kbps", text::group(u128::try_from(kbps).unwrap_or(0)))
    }
}

#[cfg(test)]
mod tests {
    use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary, StreamSummary, fixtures};
    use bdinfo_rs_core::primitives::Pid;
    use bdinfo_rs_core::stream::TsStreamType;

    use super::{CodecRow, StreamFileRow, bitrate_cell, codec_rows, stream_file_rows};

    /// A clip carrying the fields the stream-file row reads.
    fn clip(name: &str, angle_index: i32, length: f64, packet_count: u64) -> ClipSummary {
        sized_clip(name, angle_index, length, packet_count, 0, 0)
    }

    /// A clip that also carries on-disk sizes (`*.m2ts` and interleaved `*.ssif`).
    fn sized_clip(
        name: &str,
        angle_index: i32,
        length: f64,
        packet_count: u64,
        file_size: u64,
        interleaved_file_size: u64,
    ) -> ClipSummary {
        ClipSummary {
            file_size,
            interleaved_file_size,
            angle_index,
            packet_count,
            ..fixtures::clip(name, length)
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
            stream_count: 2,
            streams: vec![
                stream("MPEG-4 AVC Video", "", 0, "1080p / 23.976 fps / 16:9"),
                stream("DTS-HD Master Audio", "English", 3_948_000, "5.1 / 48 kHz / 24-bit"),
            ],
            ..fixtures::playlist(
                "00000.MPLS",
                130.0,
                vec![
                    // A plain clip: a 500 KB `*.m2ts`, 1000 packets demuxed.
                    sized_clip("00000.M2TS", 0, 100.0, 1000, 500_000, 0),
                    // No file on disk, nothing demuxed yet: both sizes unknown.
                    clip("00001.M2TS", 0, 30.0, 0),
                ],
            )
        }
    }

    #[test]
    fn stream_files_carry_index_length_and_both_sizes() {
        let rows = stream_file_rows(&playlist(), false);
        assert_eq!(
            rows,
            [
                StreamFileRow {
                    file: "00000.M2TS".to_owned(),
                    index: "1".to_owned(),
                    length: "00:01:40".to_owned(),
                    // The 500 KB `*.m2ts` on disk (no `*.ssif`).
                    estimated: "500,000".to_owned(),
                    // 1000 packets * 192 bytes = 192,000.
                    measured: "192,000".to_owned(),
                },
                StreamFileRow {
                    file: "00001.M2TS".to_owned(),
                    index: "2".to_owned(),
                    length: "00:00:30".to_owned(),
                    // No file on disk / nothing demuxed → both unknown.
                    estimated: "-".to_owned(),
                    measured: "-".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn the_estimated_size_prefers_the_interleaved_ssif_and_suffixes_the_angle() {
        let mut p = playlist();
        // An extra-angle clip with both a plain and an interleaved size: the row
        // shows the interleaved size and a ` (2)` angle suffix.
        p.clips = vec![sized_clip("00007.M2TS", 2, 40.0, 0, 700_000, 900_000)];
        let rows = stream_file_rows(&p, false);
        assert_eq!(
            rows,
            [StreamFileRow {
                file: "00007.M2TS (2)".to_owned(),
                index: "0".to_owned(),
                length: "00:00:40".to_owned(),
                estimated: "900,000".to_owned(),
                measured: "-".to_owned(),
            }]
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
        let indices: Vec<_> =
            stream_file_rows(&p, false).into_iter().map(|row| row.index).collect();
        assert_eq!(indices, ["1", "1", "2"]);
    }

    #[test]
    fn the_human_readable_toggle_reaches_the_size_cells() {
        // The same grid BDInfo formats with SizeFormatHR: known sizes switch
        // to the short form, the unknown `-` stays a dash.
        let rows = stream_file_rows(&playlist(), true);
        let cells: Vec<_> = rows.into_iter().map(|row| (row.estimated, row.measured)).collect();
        assert_eq!(
            cells,
            [("488.28 KB".to_owned(), "187.50 KB".to_owned()), ("-".to_owned(), "-".to_owned()),]
        );
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
