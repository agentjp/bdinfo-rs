//! The classic human-readable disc report — the text the CLI saves.
//!
//! This is bdinfo-rs's locked output format: the
//! forum-pastable disc report long established in the Blu-ray ecosystem
//! (the emitted `BDInfo: 0.8.0.1` version line and Notes URLs are part of the
//! format users and tools key on). The byte contract:
//!
//! * **CRLF** line endings, UTF-8, no BOM.
//! * Locale-independent ("invariant") number spellings: `,` thousands groups (`1,784,034,496`), `.`
//!   decimal points, fixed two/three-decimal values.
//! * Column cells are left-aligned space padding that never truncates — an overlong value pushes
//!   the rest of the line right, and trailing padding survives at line ends.
//! * Every formatted time is **truncated** to the tick (100 ns), never rounded; fixed-point values
//!   round the exact binary value to nearest with ties to even — including the chapter frame-size
//!   byte counts, which round ties to even like every other numeric cell.
//!
//! The section layout per playlist (in [presentation
//! order](crate::bdrom::disc::BdRom::presentation_order)): banner, the
//! forums-paste summary table, `DISC INFO`, `PLAYLIST REPORT` (with per-angle
//! lines when the playlist is angled), the hidden-streams note,
//! `VIDEO`/`AUDIO`/`SUBTITLES`/`TEXT` tables, `FILES`, `CHAPTERS`,
//! `STREAM DIAGNOSTICS`, and the `QUICK SUMMARY`. The library composes the
//! report as a `String`; printing is the caller's job.

use std::fmt::Write as _;

use crate::bdrom::chapters::{ChapterSummary, seconds_to_ticks};
use crate::bdrom::disc::{BdRom, ClipSummary, PlaylistSummary, StreamSummary};
use crate::bdrom::m2ts::{bytes_to_f64, round_long};
use crate::bdrom::order::PlaylistFilter;
use crate::error::ScanError;
use crate::stream::TsStreamType;

/// The version the report format declares — emitted verbatim in the header
/// and `DISC INFO` blocks. Part of the locked format.
const VERSION: &str = "0.8.0.1";

/// The fixed Notes block: the format's home/lineage URLs and the forums-paste
/// pointer. Part of the locked format.
const NOTES: &str = "\r\n\
    Notes:          \r\n\
    \r\n\
    BDINFO HOME:\r\n\
    \x20 Cinema Squid (old)\r\n\
    \x20   http://www.cinemasquid.com/blu-ray/tools/bdinfo\r\n\
    \x20 UniqProject GitHub (new)\r\n\
    \x20   https://github.com/UniqProject/BDInfo\r\n\
    \r\n\
    INCLUDES FORUMS REPORT FOR:\r\n\
    \x20 AVS Forum Blu-ray Audio and Video Specifications Thread\r\n\
    \x20   http://www.avsforum.com/avs-vb/showthread.php?t=1155731\r\n\
    \r\n";

/// Renders the full disc report for `bdrom`.
///
/// The default playlist filtering drops short and looping playlists, and any
/// resilient-scan `errors` fold into the `WARNING:` block after the Notes.
#[must_use]
pub fn render(bdrom: &BdRom, errors: &[ScanError]) -> String {
    render_with(bdrom, &bdrom.presentation_order(&PlaylistFilter::default()), errors)
}

/// [`render`] over an explicit playlist `order` (indices into
/// `bdrom.playlists`; an out-of-range index renders nothing).
#[must_use]
pub fn render_with(bdrom: &BdRom, order: &[usize], errors: &[ScanError]) -> String {
    let protection = protection_label(bdrom);
    let extras = bdrom.extra_features();
    let mut out = String::new();

    out.push_str(&disc_info(bdrom, protection, &extras, false));
    out.push_str(NOTES);
    out.push_str(&warnings(errors));
    for index in order {
        if let Some(playlist) = bdrom.playlists.get(*index) {
            out.push_str(&playlist_block(bdrom, playlist, protection, &extras));
        }
    }
    out
}

/// The disc's protection label: `BD+` when the BD+ directories are present,
/// else `AACS2` for a UHD disc, else `AACS`.
const fn protection_label(bdrom: &BdRom) -> &'static str {
    if bdrom.is_bd_plus {
        "BD+"
    } else if bdrom.is_uhd {
        "AACS2"
    } else {
        "AACS"
    }
}

/// The disc-identity lines shared by the header, `DISC INFO`, and the quick
/// summary: optional `Disc Title:`, then `Disc Label:`, `Disc Size:`, and
/// `Protection:` (labels padded to 16).
fn disc_core(bdrom: &BdRom, protection: &str) -> String {
    let mut out = String::new();
    if let Some(title) = bdrom.disc_title.as_deref()
        && !title.is_empty()
    {
        let _ = write!(out, "{:<16}{title}\r\n", "Disc Title:");
    }
    let _ = write!(out, "{:<16}{}\r\n", "Disc Label:", bdrom.volume_label);
    let _ = write!(out, "{:<16}{} bytes\r\n", "Disc Size:", group(u128::from(bdrom.full_size())));
    let _ = write!(out, "{:<16}{protection}\r\n", "Protection:");
    out
}

/// The full disc-info block: the core identity lines plus the optional
/// comma-joined `Extras:` line and the `BDInfo:` version line. The forums-paste
/// copy of this block spells the version with the `b` beta suffix (`{VERSION}b`),
/// mirroring the original tool; the top-of-report header omits it.
fn disc_info(bdrom: &BdRom, protection: &str, extras: &[&str], forums: bool) -> String {
    let mut out = disc_core(bdrom, protection);
    if !extras.is_empty() {
        let _ = write!(out, "{:<16}{}\r\n", "Extras:", extras.join(", "));
    }
    let suffix = if forums { "b" } else { "" };
    let _ = write!(out, "{:<16}{VERSION}{suffix}\r\n", "BDInfo:");
    out
}

/// The conditional scan-warnings block: one `WARNING:` heading plus a
/// tab-separated `file⇥reason` line per recorded failure. Empty on a clean
/// scan.
fn warnings(errors: &[ScanError]) -> String {
    let mut out = String::new();
    if !errors.is_empty() {
        out.push_str("WARNING: File errors were encountered during scan:\r\n");
        for error in errors {
            let _ = write!(out, "\r\n{}\t{}\r\n", error.file, error.reason);
        }
    }
    out
}

/// One playlist's full report block, banner through quick summary.
fn playlist_block(
    bdrom: &BdRom,
    playlist: &PlaylistSummary,
    protection: &str,
    extras: &[&str],
) -> String {
    // The stream rows in render order — the order the playlist presents them.
    let rows: Vec<&StreamSummary> = playlist.streams.iter().collect();
    let mut out = String::new();
    let _ = write!(
        out,
        "\r\n\
         ********************\r\n\
         PLAYLIST: {}\r\n\
         ********************\r\n\
         \r\n\
         <--- BEGIN FORUMS PASTE --->\r\n\
         [code]\r\n",
        playlist.name
    );
    out.push_str(&forums_table(bdrom, playlist, &rows));
    out.push_str("[/code]\r\n\r\n[code]\r\nDISC INFO:\r\n");
    out.push_str(&disc_info(bdrom, protection, extras, true));
    out.push_str(&playlist_report(playlist));
    if playlist.has_hidden_streams() {
        out.push_str("\r\n(*) Indicates included stream hidden by this playlist.\r\n");
    }
    let (tables, stream_summary_lines) = stream_tables(&rows);
    out.push_str(&tables);
    out.push_str(&files_section(playlist));
    out.push_str(&chapters_section(playlist));
    out.push_str(&diagnostics_section(playlist));
    out.push_str("\r\n[/code]\r\n<---- END FORUMS PASTE ---->\r\n\r\n");
    out.push_str("QUICK SUMMARY:\r\n\r\n");
    out.push_str(&quick_summary(bdrom, playlist, protection, &stream_summary_lines));
    out.push_str("\r\n");
    out
}

/// The forums-paste summary table: the three fixed header rows plus the one
/// data row (title, codec, length, sizes, bitrates, main/secondary audio).
/// `rows` is the playlist's streams in render order — its first video and
/// first audio fill the codec/bitrate/audio cells.
fn forums_table(bdrom: &BdRom, playlist: &PlaylistSummary, rows: &[&StreamSummary]) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{:<64}{:<8}{:<10}{:<18}{:<18}{:<13}{:<13}{:<42}{:<25}\r\n",
        " ", " ", " ", " ", " ", "Total", "Video", " ", " "
    );
    let _ = write!(
        out,
        "{:<64}{:<8}{:<10}{:<18}{:<18}{:<13}{:<13}{:<42}{:<25}\r\n",
        "Title",
        "Codec",
        "Length",
        "Movie Size",
        "Disc Size",
        "Bitrate",
        "Bitrate",
        "Main Audio Track",
        "Secondary Audio Track"
    );
    let _ = write!(
        out,
        "{:<64}{:<8}{:<10}{:<18}{:<18}{:<13}{:<13}{:<42}{:<25}\r\n",
        "-----",
        "------",
        "-------",
        "--------------",
        "----------------",
        "-----------",
        "-----------",
        "------------------",
        "---------------------"
    );

    let first_video = rows.iter().copied().find(|s| s.stream_type.is_video());
    let video_codec = first_video.map_or("", |s| s.codec_alt_name);
    let video_bitrate = first_video
        .map_or_else(String::new, |s| cents(round_long(int_to_f64(s.bitrate) / 10_000.0)));

    let audio_streams: Vec<&StreamSummary> =
        rows.iter().copied().filter(|s| s.stream_type.is_audio()).collect();
    let audio1 = audio_streams.first().map_or_else(String::new, |s| forums_audio_cell(s));
    let audio2 = secondary_audio(&audio_streams).map_or_else(String::new, forums_audio_cell);

    let _ = write!(
        out,
        "{:<64}{:<8}{:<10}{:<18}{:<18}{:<13}{:<13}{:<42}{:<25}\r\n",
        playlist.name,
        video_codec,
        time_hh_short(seconds_to_ticks(playlist.total_length)),
        group(u128::from(playlist.total_packet_size())),
        group(u128::from(bdrom.full_size())),
        format!("{} Mbps", cents(round_long(u64_to_f64(playlist.total_bit_rate()) / 10_000.0))),
        format!("{video_bitrate} Mbps"),
        audio1,
        audio2
    );
    out
}

/// The forums-paste audio cell: alternate codec name, channel layout, then
/// the rounded kbps and `(NkHz/N-bit)` suffixes when those values are known.
fn forums_audio_cell(stream: &StreamSummary) -> String {
    let mut cell = format!("{} {}", stream.codec_alt_name, stream.channel_description);
    if stream.bitrate > 0 {
        let _ = write!(cell, " {} kbps", kbps(stream.bitrate));
    }
    if stream.sample_rate > 0 && stream.bit_depth > 0 {
        let _ = write!(
            cell,
            " ({}kHz/{}-bit)",
            round_long(int_to_f64(i64::from(stream.sample_rate)) / 1000.0),
            stream.bit_depth
        );
    }
    cell
}

/// Picks the forums-paste secondary audio track: the first later audio stream
/// sharing the first track's language that is not a secondary-audio type and
/// not 2.0 Dolby Digital.
fn secondary_audio<'a>(audio_streams: &[&'a StreamSummary]) -> Option<&'a StreamSummary> {
    let (first, rest) = audio_streams.split_first()?;
    rest.iter()
        .find(|s| {
            s.language_code == first.language_code
                && s.stream_type != TsStreamType::Ac3PlusSecondaryAudio
                && s.stream_type != TsStreamType::DtsHdSecondaryAudio
                && !(s.stream_type == TsStreamType::Ac3Audio && s.channel_count == 2)
        })
        .copied()
}

/// The `PLAYLIST REPORT:` block: name, length, size, and total bitrate, plus
/// the per-angle and `All Angles` lines when the playlist carries extra
/// camera angles.
fn playlist_report(playlist: &PlaylistSummary) -> String {
    let total_length = time_hh(seconds_to_ticks(playlist.total_length));
    let mut out = String::new();
    let _ = write!(
        out,
        "\r\nPLAYLIST REPORT:\r\n\
         \r\n\
         {:<16}{}\r\n",
        "Name:", playlist.name
    );
    let _ = write!(out, "{:<16}{total_length} (h:m:s.ms)\r\n", "Length:");
    let _ =
        write!(out, "{:<16}{} bytes\r\n", "Size:", group(u128::from(playlist.total_packet_size())));
    let _ = write!(
        out,
        "{:<16}{} Mbps\r\n",
        "Total Bitrate:",
        cents(round_long(u64_to_f64(playlist.total_bit_rate()) / 10_000.0))
    );
    if playlist.angle_count > 0 {
        for (index, angle) in playlist.angle_totals().iter().enumerate() {
            let number = index.saturating_add(1);
            let _ = write!(
                out,
                "{:<24}{} (h:mm:ss.ms) / {total_length} (h:mm:ss.ms)\r\n",
                format!("Angle {number} Length:"),
                time_hh(seconds_to_ticks(angle.length))
            );
            let _ = write!(
                out,
                "{:<24}{} bytes / {} bytes\r\n",
                format!("Angle {number} Size:"),
                group(u128::from(angle.packet_size)),
                group(u128::from(angle.timeline_packet_size))
            );
            // The angle's own rate over its own length, and its whole-timeline
            // rate over the playlist length — each rounded once from the raw
            // ratio.
            let own = if angle.length > 0.0 {
                round_long(bytes_to_f64(angle.packet_size) * 8.0 / angle.length / 10_000.0)
            } else {
                0
            };
            let timeline = if playlist.total_length > 0.0 {
                round_long(
                    bytes_to_f64(angle.timeline_packet_size) * 8.0
                        / playlist.total_length
                        / 10_000.0,
                )
            } else {
                0
            };
            let _ = write!(
                out,
                "{:<24}{} Mbps / {} Mbps\r\n",
                format!("Angle {number} Total Bitrate:"),
                cents(own),
                cents(timeline)
            );
        }
        let _ = write!(
            out,
            "{:<24}{} (h:m:s.ms)\r\n",
            "All Angles Length:",
            time_hh(seconds_to_ticks(playlist.total_angle_length()))
        );
        let _ = write!(
            out,
            "{:<24}{} bytes\r\n",
            "All Angles Size:",
            group(u128::from(playlist.total_angle_packet_size()))
        );
        let _ = write!(
            out,
            "{:<24}{} Mbps\r\n",
            "All Angles Bitrate:",
            cents(round_long(u64_to_f64(playlist.total_angle_bit_rate()) / 10_000.0))
        );
    }
    out
}

/// The `VIDEO`/`AUDIO`/`SUBTITLES`/`TEXT` stream tables (each emitted only
/// when the playlist presents that kind) over the stream `rows` in render
/// order, returning the tables plus the per-stream
/// `Video:`/`Audio:`/`Subtitle:` lines the quick summary repeats.
fn stream_tables(rows: &[&StreamSummary]) -> (String, String) {
    let mut out = String::new();
    let mut summary = String::new();

    if rows.iter().any(|s| s.stream_type.is_video()) {
        out.push_str("\r\nVIDEO:\r\n\r\n");
        let _ = write!(out, "{:<24}{:<20}{:<16}\r\n", "Codec", "Bitrate", "Description");
        let _ = write!(
            out,
            "{:<24}{:<20}{:<16}\r\n",
            "---------------", "-------------", "-----------"
        );
        for stream in rows.iter().copied().filter(|s| s.stream_type.is_video()) {
            let mut name = stream.codec_name.clone();
            if stream.angle_index > 0 {
                let _ = write!(name, " ({})", stream.angle_index);
            }
            let mut bitrate = group_signed(kbps(stream.bitrate));
            if stream.angle_index > 0 {
                let _ = write!(bitrate, " ({})", kbps(stream.active_bitrate));
            }
            bitrate.push_str(" kbps");
            let _ = write!(
                out,
                "{:<24}{:<20}{:<16}\r\n",
                format!("{}{name}", hidden_prefix(stream)),
                bitrate,
                stream.full_description
            );
            let _ = write!(
                summary,
                "{:<16}{name} / {bitrate} / {}\r\n",
                format!("{}Video:", hidden_prefix(stream)),
                stream.full_description
            );
        }
    }

    if rows.iter().any(|s| s.stream_type.is_audio()) {
        out.push_str("\r\nAUDIO:\r\n\r\n");
        out.push_str(&kind_table_header());
        for stream in rows.iter().copied().filter(|s| s.stream_type.is_audio()) {
            let bitrate = format!("{:>5} kbps", kbps(stream.bitrate));
            let _ = write!(
                out,
                "{:<32}{:<16}{:<16}{:<16}\r\n",
                format!("{}{}", hidden_prefix(stream), stream.codec_name),
                stream.language_name,
                bitrate,
                stream.full_description
            );
            let _ = write!(
                summary,
                "{:<16}{} / {} / {}\r\n",
                format!("{}Audio:", hidden_prefix(stream)),
                stream.language_name,
                stream.codec_name,
                stream.full_description
            );
        }
    }

    if rows.iter().any(|s| s.stream_type.is_graphics()) {
        out.push_str("\r\nSUBTITLES:\r\n\r\n");
        out.push_str(&kind_table_header());
        for stream in rows.iter().copied().filter(|s| s.stream_type.is_graphics()) {
            let bitrate = format!("{:>5} kbps", fixed_even(int_to_f64(stream.bitrate) / 1000.0, 2));
            let _ = write!(
                out,
                "{:<32}{:<16}{:<16}{:<16}\r\n",
                format!("{}{}", hidden_prefix(stream), stream.codec_name),
                stream.language_name,
                bitrate,
                stream.full_description
            );
            let _ = write!(
                summary,
                "{:<16}{} / {}\r\n",
                format!("{}Subtitle:", hidden_prefix(stream)),
                stream.language_name,
                bitrate.trim()
            );
        }
    }

    if rows.iter().any(|s| s.stream_type.is_text()) {
        out.push_str("\r\nTEXT:\r\n\r\n");
        out.push_str(&kind_table_header());
        for stream in rows.iter().copied().filter(|s| s.stream_type.is_text()) {
            let bitrate = format!("{:>5} kbps", fixed_even(int_to_f64(stream.bitrate) / 1000.0, 2));
            let _ = write!(
                out,
                "{:<32}{:<16}{:<16}{:<16}\r\n",
                format!("{}{}", hidden_prefix(stream), stream.codec_name),
                stream.language_name,
                bitrate,
                stream.full_description
            );
        }
    }

    (out, summary)
}

/// The shared `Codec/Language/Bitrate/Description` header pair of the audio,
/// subtitle, and text tables.
fn kind_table_header() -> String {
    let mut out = String::new();
    let _ =
        write!(out, "{:<32}{:<16}{:<16}{:<16}\r\n", "Codec", "Language", "Bitrate", "Description");
    let _ = write!(
        out,
        "{:<32}{:<16}{:<16}{:<16}\r\n",
        "---------------", "-------------", "-------------", "-----------"
    );
    out
}

/// The `* ` marker prefixed to a hidden stream's name and summary label.
const fn hidden_prefix(stream: &StreamSummary) -> &'static str {
    if stream.is_hidden { "* " } else { "" }
}

/// The clip's display name with its ` (N)` angle suffix when it belongs to an
/// extra camera angle. A 3D clip is named by its interleaved `*.ssif` file.
fn clip_display_name(clip: &ClipSummary) -> String {
    let mut name = clip.display_name.clone();
    if clip.angle_index > 0 {
        let _ = write!(name, " ({})", clip.angle_index);
    }
    name
}

/// The `FILES:` section: one row per sequenced clip (angle clips included)
/// with its in-time, length, packet size, and packet bitrate.
fn files_section(playlist: &PlaylistSummary) -> String {
    let mut out = String::new();
    out.push_str("\r\nFILES:\r\n\r\n");
    let _ = write!(
        out,
        "{:<16}{:<16}{:<16}{:<16}{:<16}\r\n",
        "Name", "Time In", "Length", "Size", "Total Bitrate"
    );
    let _ = write!(
        out,
        "{:<16}{:<16}{:<16}{:<16}{:<16}\r\n",
        "---------------", "-------------", "-------------", "-------------", "-------------"
    );
    for clip in &playlist.clips {
        let bitrate = format!(
            "{:>6} kbps",
            group_signed(round_long(u64_to_f64(clip.packet_bit_rate()) / 1000.0))
        );
        let _ = write!(
            out,
            "{:<16}{:<16}{:<16}{:<16}{:<16}\r\n",
            clip_display_name(clip),
            time_h(seconds_to_ticks(clip.relative_time_in)),
            time_h(seconds_to_ticks(clip.length)),
            group(u128::from(clip.packet_size())),
            bitrate
        );
    }
    out
}

/// The `CHAPTERS:` section: the 13-column header plus one row per chapter
/// mark, fed by the model's measured per-chapter video statistics.
fn chapters_section(playlist: &PlaylistSummary) -> String {
    let mut out = String::new();
    out.push_str("\r\nCHAPTERS:\r\n\r\n");
    let _ = write!(
        out,
        "{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}\r\n",
        "Number",
        "Time In",
        "Length",
        "Avg Video Rate",
        "Max 1-Sec Rate",
        "Max 1-Sec Time",
        "Max 5-Sec Rate",
        "Max 5-Sec Time",
        "Max 10Sec Rate",
        "Max 10Sec Time",
        "Avg Frame Size",
        "Max Frame Size",
        "Max Frame Time"
    );
    let _ = write!(
        out,
        "{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}\r\n",
        "------",
        "-------------",
        "-------------",
        "--------------",
        "--------------",
        "--------------",
        "--------------",
        "--------------",
        "--------------",
        "--------------",
        "--------------",
        "--------------",
        "--------------"
    );
    for (index, chapter) in playlist.chapters.iter().enumerate() {
        out.push_str(&chapter_row(index.saturating_add(1), chapter));
    }
    out
}

/// One chapter row: times truncated to the tick, window rates rounded
/// half-to-even to kbps, frame sizes rounded half-to-even to bytes.
fn chapter_row(number: usize, chapter: &ChapterSummary) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}{:<16}\r\n",
        number,
        time_h(seconds_to_ticks(chapter.time_in)),
        time_h(seconds_to_ticks(chapter.length)),
        rate_cell(chapter.avg_rate),
        rate_cell(chapter.max_1sec_rate),
        time_hh(seconds_to_ticks(chapter.max_1sec_time)),
        rate_cell(chapter.max_5sec_rate),
        time_hh(seconds_to_ticks(chapter.max_5sec_time)),
        rate_cell(chapter.max_10sec_rate),
        time_hh(seconds_to_ticks(chapter.max_10sec_time)),
        bytes_cell(chapter.avg_frame_size),
        bytes_cell(chapter.max_frame_size),
        time_hh(seconds_to_ticks(chapter.max_frame_time))
    );
    out
}

/// A chapter-table rate cell: `{kbps,6:N0} kbps`, the rate rounded
/// half-to-even from bits per second.
fn rate_cell(rate: f64) -> String {
    format!("{:>6} kbps", group_signed(round_long(rate / 1000.0)))
}

/// A chapter-table frame-size cell: `{bytes,7:N0} bytes`, the raw byte value
/// rounded half-to-even (.NET `N0` is ties-to-even, like every other cell).
fn bytes_cell(bytes: f64) -> String {
    format!("{:>7} bytes", group_signed(round_long(bytes)))
}

/// The `STREAM DIAGNOSTICS:` section: per measured clip (deduplicated by clip
/// file, first occurrence wins) one row per presented stream with its
/// whole-file payload/packet tallies.
fn diagnostics_section(playlist: &PlaylistSummary) -> String {
    let mut out = String::new();
    out.push_str("\r\nSTREAM DIAGNOSTICS:\r\n\r\n");
    let _ = write!(
        out,
        "{:<16}{:<16}{:<16}{:<16}{:<24}{:<24}{:<24}{:<16}{:<16}\r\n",
        "File",
        "PID",
        "Type",
        "Codec",
        "Language",
        "Seconds",
        format!("{:>11}", "Bitrate"),
        format!("{:>10}", "Bytes"),
        format!("{:>9}", "Packets")
    );
    let _ = write!(
        out,
        "{:<16}{:<16}{:<16}{:<16}{:<24}{:<24}{:<24}{:<16}{:<16}\r\n",
        "----------",
        "-------------",
        "-----",
        "----------",
        "-------------",
        "--------------",
        "---------------",
        "--------------",
        "-----------"
    );
    let mut reported = std::collections::BTreeSet::new();
    for clip in &playlist.clips {
        if clip.streams.is_empty() || !reported.insert(clip.name.as_str()) {
            continue;
        }
        let clip_name = clip_display_name(clip);
        for tally in &clip.streams {
            // The row is typed by the clip file's own registration (the
            // `tally`); the playlist stream supplies only the language.
            let Some(stream) =
                playlist.streams.iter().find(|s| s.pid == tally.pid && s.angle_index == 0)
            else {
                continue;
            };
            let (seconds, bitrate) = if clip.file_seconds > 0.0 {
                (
                    fixed_even(clip.file_seconds, 3),
                    round_long(
                        bytes_to_f64(tally.payload_bytes) * 8.0 / clip.file_seconds / 1000.0,
                    ),
                )
            } else {
                ("0".to_owned(), 0)
            };
            let language = if stream.language_code.is_empty() {
                String::new()
            } else {
                format!("{} ({})", stream.language_code, stream.language_name)
            };
            let _ = write!(
                out,
                "{:<16}{:<16}{:<16}{:<16}{:<24}{:<24}{:<24}{:<16}{:<16}\r\n",
                clip_name,
                format!("{} (0x{:X})", tally.pid, tally.pid.get()),
                format!("0x{:02X}", tally.stream_type.value()),
                tally.codec_short_name,
                language,
                seconds,
                format!("{:>7} kbps", group_signed(bitrate)),
                format!("{:>14}", group(u128::from(tally.payload_bytes))),
                format!("{:>11}", group(u128::from(tally.packet_count)))
            );
        }
    }
    out
}

/// The `QUICK SUMMARY:` body: the disc-identity lines, the playlist totals,
/// then the per-stream lines collected from the stream tables.
fn quick_summary(
    bdrom: &BdRom,
    playlist: &PlaylistSummary,
    protection: &str,
    stream_lines: &str,
) -> String {
    let mut out = disc_core(bdrom, protection);
    let _ = write!(out, "{:<16}{}\r\n", "Playlist:", playlist.name);
    let _ =
        write!(out, "{:<16}{} bytes\r\n", "Size:", group(u128::from(playlist.total_packet_size())));
    let _ =
        write!(out, "{:<16}{}\r\n", "Length:", time_hh(seconds_to_ticks(playlist.total_length)));
    let _ = write!(
        out,
        "{:<16}{} Mbps\r\n",
        "Total Bitrate:",
        cents(round_long(u64_to_f64(playlist.total_bit_rate()) / 10_000.0))
    );
    out.push_str(stream_lines);
    out
}

// ── Invariant number/time formatting ────────────────────────────────────────

/// `bits/s → kbps` for the stream tables: the value divided by 1000 and
/// rounded half-to-even.
fn kbps(bit_rate: i64) -> i64 {
    round_long(int_to_f64(bit_rate) / 1000.0)
}

/// Renders a hundredths count as a fixed two-decimal value — the `Mbps`
/// spelling for the already-rounded `value / 100` bit rates. Negative input
/// (unreachable from the model's non-negative rates) clamps to zero.
fn cents(hundredths: i64) -> String {
    let value = hundredths.max(0);
    format!("{}.{:02}", value.wrapping_div(100), value.wrapping_rem(100))
}

/// Groups a non-negative integer with `,` thousands separators (the `N0`
/// spelling).
fn group(value: u128) -> String {
    let digits = value.to_string();
    let mut parts: Vec<&[u8]> = digits.as_bytes().rchunks(3).collect();
    parts.reverse();
    let joined = parts.join(&b","[..]);
    String::from_utf8(joined).unwrap_or(digits)
}

/// Groups a signed integer with `,` thousands separators.
fn group_signed(value: i64) -> String {
    let grouped = group(u128::from(value.unsigned_abs()));
    if value < 0 { format!("-{grouped}") } else { grouped }
}

/// Widens an `i64` (a model bit rate) to `f64` for the rate divisions —
/// exact for every value the demux can produce (well under 2^53).
#[expect(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "bit rates fit f64 exactly (int→float; TryFrom inapplicable)"
)]
const fn int_to_f64(value: i64) -> f64 {
    value as f64
}

/// Widens a `u64` (a model byte/bit-rate count) to `f64` — exact for every
/// value the demux can produce.
const fn u64_to_f64(value: u64) -> f64 {
    bytes_to_f64(value)
}

/// Formats `value` with exactly `decimals` fractional digits, rounding the
/// **exact** binary value to nearest with ties to even — the report's
/// fixed-point rule (an exact `19.125` renders `19.12`), which is Rust's
/// native `{:.N}` semantics. Non-finite or negative input (unreachable from
/// the model) renders as zero.
fn fixed_even(value: f64, decimals: usize) -> String {
    if !value.is_finite() || value <= 0.0 {
        return format!("{:.decimals$}", 0.0);
    }
    format!("{value:.decimals$}")
}

/// Splits non-negative ticks into the `(hours, minutes, seconds, millis)`
/// time components. The hours component wraps at 24 (days are dropped), like
/// the report's time spellings; negative ticks clamp to zero.
fn time_parts(ticks: i64) -> (i64, i64, i64, i64) {
    let ticks = ticks.max(0);
    let total_seconds = ticks.wrapping_div(10_000_000);
    let millis = ticks.wrapping_rem(10_000_000).wrapping_div(10_000);
    let hours = total_seconds.wrapping_div(3600).wrapping_rem(24);
    let minutes = total_seconds.wrapping_rem(3600).wrapping_div(60);
    let seconds = total_seconds.wrapping_rem(60);
    (hours, minutes, seconds, millis)
}

/// `h:mm:ss.fff` — unpadded hours, truncated milliseconds.
fn time_h(ticks: i64) -> String {
    let (h, m, s, ms) = time_parts(ticks);
    format!("{h}:{m:02}:{s:02}.{ms:03}")
}

/// `hh:mm:ss.fff` — two-digit hours, truncated milliseconds.
fn time_hh(ticks: i64) -> String {
    let (h, m, s, ms) = time_parts(ticks);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// `hh:mm:ss` — two-digit hours, the seconds-resolution spelling.
fn time_hh_short(ticks: i64) -> String {
    let (h, m, s, _) = time_parts(ticks);
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::{
        bytes_cell, cents, fixed_even, group, group_signed, kbps, render, render_with,
        secondary_audio, time_h, time_hh, time_hh_short, time_parts,
    };
    use crate::bdrom::chapters::ChapterSummary;
    use crate::bdrom::disc::{BdRom, ClipStreamTally, ClipSummary, PlaylistSummary, StreamSummary};
    use crate::error::{BdError, ScanError, ScanStage};
    use crate::primitives::Pid;
    use crate::stream::TsStreamType;

    /// A stream row with every field defaulted to its empty/zero value.
    fn stream(pid: u16, stream_type: TsStreamType) -> StreamSummary {
        StreamSummary {
            pid: Pid::new(pid),
            stream_type,
            codec_short_name: String::new(),
            codec_name: String::new(),
            codec_alt_name: "",
            bitrate: 0,
            active_bitrate: 0,
            language_name: String::new(),
            language_code: String::new(),
            description: String::new(),
            full_description: String::new(),
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

    #[test]
    fn the_forums_audio_cell_needs_both_rate_and_depth() {
        // The `(NkHz/N-bit)` suffix needs BOTH values known — a one-sided row
        // (rate without depth, or depth without rate) renders no suffix.
        let mut s = stream(0x1100, TsStreamType::DtsHdMasterAudio);
        s.codec_alt_name = "DTS-HD MA";
        s.channel_description = "5.1".to_owned();
        s.bitrate = 768_000;
        s.sample_rate = 48_000;
        assert_eq!(super::forums_audio_cell(&s), "DTS-HD MA 5.1 768 kbps");
        s.bit_depth = 24;
        assert_eq!(super::forums_audio_cell(&s), "DTS-HD MA 5.1 768 kbps (48kHz/24-bit)");
        s.sample_rate = 0;
        assert_eq!(super::forums_audio_cell(&s), "DTS-HD MA 5.1 768 kbps");
    }

    /// A measured clip row.
    fn clip(name: &str, length: f64, packet_count: u64) -> ClipSummary {
        ClipSummary {
            name: name.to_owned(),
            display_name: name.to_owned(),
            angle_index: 0,
            relative_time_in: 0.0,
            length,
            payload_bytes: 0,
            packet_count,
            packet_seconds: length,
            file_seconds: length,
            streams: Vec::new(),
        }
    }

    /// A playlist with no streams, clips, or chapters.
    fn playlist(name: &str, total_length: f64) -> PlaylistSummary {
        PlaylistSummary {
            name: name.to_owned(),
            total_length,
            file_size: 0,
            interleaved_file_size: 0,
            chapter_count: 0,
            stream_count: 0,
            angle_count: 0,
            has_loops: false,
            streams: Vec::new(),
            clips: Vec::new(),
            chapters: Vec::new(),
        }
    }

    /// A bare disc carrying `playlists` and no feature flags.
    fn disc(playlists: Vec<PlaylistSummary>) -> BdRom {
        BdRom {
            volume_label: "FIXTURE".to_owned(),
            disc_title: None,
            size: 1_000_000_000,
            interleaved_size: 0,
            is_3d: false,
            is_50hz: false,
            is_uhd: false,
            is_bd_plus: false,
            is_bd_java: false,
            is_dbox: false,
            is_psp: false,
            playlists,
        }
    }

    /// The full single-playlist fixture: a UHD disc with one playlist carrying
    /// a video/audio/graphics/text stream each, one measured clip, and two
    /// chapters — the whole-report byte pin.
    fn full_disc() -> BdRom {
        let mut video = stream(4113, TsStreamType::HevcVideo);
        video.codec_short_name = "HEVC".to_owned();
        video.codec_name = "MPEG-H HEVC Video".to_owned();
        video.codec_alt_name = "HEVC";
        video.bitrate = 5_975_000;
        video.description = "2160p / 23.976 fps".to_owned();
        video.full_description = "2160p / 23.976 fps / HDR10".to_owned();

        let mut audio = stream(4352, TsStreamType::DtsHdMasterAudio);
        audio.codec_short_name = "DTS-HD MA".to_owned();
        audio.codec_name = "DTS-HD Master Audio".to_owned();
        audio.codec_alt_name = "DTS-HD Master";
        audio.bitrate = 2_046_000;
        audio.language_name = "Japanese".to_owned();
        audio.language_code = "jpn".to_owned();
        audio.description = "5.1 / 48 kHz / 24-bit".to_owned();
        audio.full_description = "5.1 / 48 kHz /  2046 kbps / 24-bit".to_owned();
        audio.channel_description = "5.1".to_owned();
        audio.sample_rate = 48_000;
        audio.bit_depth = 24;
        audio.channel_count = 5;
        audio.is_hidden = true;

        let mut graphics = stream(4608, TsStreamType::PresentationGraphics);
        graphics.codec_short_name = "PGS".to_owned();
        graphics.codec_name = "Presentation Graphics".to_owned();
        graphics.bitrate = 19_125;
        graphics.language_name = "English".to_owned();
        graphics.language_code = "eng".to_owned();

        let mut text = stream(6144, TsStreamType::Subtitle);
        text.codec_short_name = "TextST".to_owned();
        text.codec_name = "Text Subtitles".to_owned();
        text.bitrate = 1_500;
        text.language_name = "English".to_owned();
        text.language_code = "eng".to_owned();

        let mut main_clip = clip("00001.M2TS", 120.5, 500_000);
        main_clip.streams = vec![
            ClipStreamTally {
                pid: Pid::new(4113),
                stream_type: TsStreamType::HevcVideo,
                codec_short_name: "HEVC".to_owned(),
                payload_bytes: 90_000_000,
                packet_count: 480_000,
            },
            ClipStreamTally {
                pid: Pid::new(4352),
                stream_type: TsStreamType::DtsHdMasterAudio,
                codec_short_name: "DTS-HD MA".to_owned(),
                payload_bytes: 5_000_000,
                packet_count: 30_000,
            },
        ];

        let mut list = playlist("00001.MPLS", 120.5);
        list.chapter_count = 2;
        list.stream_count = 4;
        list.streams = vec![video, audio, graphics, text];
        list.clips = vec![main_clip];
        list.chapters = vec![
            ChapterSummary {
                time_in: 0.0,
                length: 60.25,
                avg_rate: 6_000_000.0,
                max_1sec_rate: 8_000_000.0,
                max_1sec_time: 10.0,
                max_5sec_rate: 7_500_000.0,
                max_5sec_time: 12.0,
                max_10sec_rate: 7_000_000.0,
                max_10sec_time: 15.0,
                avg_frame_size: 31_000.4,
                max_frame_size: 95_000.6,
                max_frame_time: 20.0,
            },
            ChapterSummary {
                time_in: 60.25,
                length: 60.25,
                avg_rate: 0.0,
                max_1sec_rate: 0.0,
                max_1sec_time: 0.0,
                max_5sec_rate: 0.0,
                max_5sec_time: 0.0,
                max_10sec_rate: 0.0,
                max_10sec_time: 0.0,
                avg_frame_size: 0.0,
                max_frame_size: 0.0,
                max_frame_time: 0.0,
            },
        ];

        let mut bdrom = disc(vec![list]);
        bdrom.disc_title = Some("FIXTURE: THE MOVIE".to_owned());
        bdrom.is_uhd = true;
        bdrom.is_bd_java = true;
        bdrom
    }

    /// The whole expected report for [`full_disc`], spelled with `\n` endings
    /// (the assert respells them `\r\n`) — the locked-format byte pin.
    const FULL_REPORT: &str = r"Disc Title:     FIXTURE: THE MOVIE
Disc Label:     FIXTURE
Disc Size:      1,000,000,000 bytes
Protection:     AACS2
Extras:         Ultra HD, BD-Java
BDInfo:         0.8.0.1

Notes:          

BDINFO HOME:
  Cinema Squid (old)
    http://www.cinemasquid.com/blu-ray/tools/bdinfo
  UniqProject GitHub (new)
    https://github.com/UniqProject/BDInfo

INCLUDES FORUMS REPORT FOR:
  AVS Forum Blu-ray Audio and Video Specifications Thread
    http://www.avsforum.com/avs-vb/showthread.php?t=1155731


********************
PLAYLIST: 00001.MPLS
********************

<--- BEGIN FORUMS PASTE --->
[code]
                                                                                                                      Total        Video                                                                           
Title                                                           Codec   Length    Movie Size        Disc Size         Bitrate      Bitrate      Main Audio Track                          Secondary Audio Track    
-----                                                           ------  -------   --------------    ----------------  -----------  -----------  ------------------                        ---------------------    
00001.MPLS                                                      HEVC    00:02:00  96,000,000        1,000,000,000     6.37 Mbps    5.98 Mbps    DTS-HD Master 5.1 2046 kbps (48kHz/24-bit)                         
[/code]

[code]
DISC INFO:
Disc Title:     FIXTURE: THE MOVIE
Disc Label:     FIXTURE
Disc Size:      1,000,000,000 bytes
Protection:     AACS2
Extras:         Ultra HD, BD-Java
BDInfo:         0.8.0.1b

PLAYLIST REPORT:

Name:           00001.MPLS
Length:         00:02:00.500 (h:m:s.ms)
Size:           96,000,000 bytes
Total Bitrate:  6.37 Mbps

(*) Indicates included stream hidden by this playlist.

VIDEO:

Codec                   Bitrate             Description     
---------------         -------------       -----------     
MPEG-H HEVC Video       5,975 kbps          2160p / 23.976 fps / HDR10

AUDIO:

Codec                           Language        Bitrate         Description     
---------------                 -------------   -------------   -----------     
* DTS-HD Master Audio           Japanese         2046 kbps      5.1 / 48 kHz /  2046 kbps / 24-bit

SUBTITLES:

Codec                           Language        Bitrate         Description     
---------------                 -------------   -------------   -----------     
Presentation Graphics           English         19.12 kbps                      

TEXT:

Codec                           Language        Bitrate         Description     
---------------                 -------------   -------------   -----------     
Text Subtitles                  English          1.50 kbps                      

FILES:

Name            Time In         Length          Size            Total Bitrate   
--------------- -------------   -------------   -------------   -------------   
00001.M2TS      0:00:00.000     0:02:00.500     96,000,000       6,373 kbps     

CHAPTERS:

Number          Time In         Length          Avg Video Rate  Max 1-Sec Rate  Max 1-Sec Time  Max 5-Sec Rate  Max 5-Sec Time  Max 10Sec Rate  Max 10Sec Time  Avg Frame Size  Max Frame Size  Max Frame Time  
------          -------------   -------------   --------------  --------------  --------------  --------------  --------------  --------------  --------------  --------------  --------------  --------------  
1               0:00:00.000     0:01:00.250      6,000 kbps      8,000 kbps     00:00:10.000     7,500 kbps     00:00:12.000     7,000 kbps     00:00:15.000     31,000 bytes    95,001 bytes   00:00:20.000    
2               0:01:00.250     0:01:00.250          0 kbps          0 kbps     00:00:00.000         0 kbps     00:00:00.000         0 kbps     00:00:00.000          0 bytes         0 bytes   00:00:00.000    

STREAM DIAGNOSTICS:

File            PID             Type            Codec           Language                Seconds                     Bitrate                  Bytes        Packets       
----------      -------------   -----           ----------      -------------           --------------          ---------------         --------------  -----------     
00001.M2TS      4113 (0x1011)   0x24            HEVC                                    120.500                   5,975 kbps                90,000,000      480,000     
00001.M2TS      4352 (0x1100)   0x86            DTS-HD MA       jpn (Japanese)          120.500                     332 kbps                 5,000,000       30,000     

[/code]
<---- END FORUMS PASTE ---->

QUICK SUMMARY:

Disc Title:     FIXTURE: THE MOVIE
Disc Label:     FIXTURE
Disc Size:      1,000,000,000 bytes
Protection:     AACS2
Playlist:       00001.MPLS
Size:           96,000,000 bytes
Length:         00:02:00.500
Total Bitrate:  6.37 Mbps
Video:          MPEG-H HEVC Video / 5,975 kbps / 2160p / 23.976 fps / HDR10
* Audio:        Japanese / DTS-HD Master Audio / 5.1 / 48 kHz /  2046 kbps / 24-bit
Subtitle:       English / 19.12 kbps

";

    #[test]
    fn render_pins_the_full_fixture_report_bytes() {
        assert_eq!(render(&full_disc(), &[]), FULL_REPORT.replace('\n', "\r\n"));
    }

    #[test]
    fn render_with_skips_an_out_of_range_index() {
        let bdrom = full_disc();
        let header_only = render_with(&bdrom, &[7], &[]);
        assert!(header_only.contains("INCLUDES FORUMS REPORT FOR:"));
        assert!(!header_only.contains("PLAYLIST:"));
    }

    #[test]
    fn scan_errors_render_the_warning_block() {
        let errors = [ScanError {
            file: "00001.M2TS".to_owned(),
            stage: ScanStage::StreamFile,
            reason: BdError::StructureNotFound,
        }];
        let rendered = render_with(&disc(Vec::new()), &[], &errors);
        assert!(rendered.contains("WARNING: File errors were encountered during scan:\r\n"));
        assert!(
            rendered
                .contains(format!("\r\n00001.M2TS\t{}\r\n", BdError::StructureNotFound).as_str())
        );
    }

    #[test]
    fn protection_prefers_bd_plus_over_uhd_and_defaults_to_aacs() {
        let mut bdrom = disc(Vec::new());
        assert!(render(&bdrom, &[]).contains("Protection:     AACS\r\n"));
        bdrom.is_uhd = true;
        bdrom.is_bd_plus = true;
        assert!(render(&bdrom, &[]).contains("Protection:     BD+\r\n"));
    }

    #[test]
    fn an_empty_disc_title_is_skipped_like_a_missing_one() {
        let mut bdrom = disc(Vec::new());
        bdrom.disc_title = Some(String::new());
        let rendered = render(&bdrom, &[]);
        assert!(!rendered.contains("Disc Title:"));
        assert!(!rendered.contains("Extras:"));
    }

    /// A playlist long enough to survive the default filter with one clip
    /// (`60 s`, `100_000` packets) and the given streams.
    fn presented(streams: Vec<StreamSummary>) -> PlaylistSummary {
        let mut list = playlist("00002.MPLS", 60.0);
        list.clips = vec![clip("00002.M2TS", 60.0, 100_000)];
        list.streams = streams;
        list
    }

    #[test]
    fn a_playlist_without_video_or_audio_renders_empty_forums_cells() {
        let bdrom = disc(vec![presented(Vec::new())]);
        let rendered = render(&bdrom, &[]);
        // Codec and video-bitrate cells are blank; ` Mbps` still spells the
        // (empty) video bitrate cell.
        let prefix = format!("{:<64}{:<8}{:<10}", "00002.MPLS", "", "00:01:00");
        assert!(rendered.contains(&prefix));
        assert!(rendered.contains("19,200,000        1,000,000,000     2.56 Mbps     Mbps"));
        assert!(!rendered.contains("VIDEO:"));
        assert!(!rendered.contains("AUDIO:"));
        assert!(!rendered.contains("SUBTITLES:"));
        assert!(!rendered.contains("TEXT:"));
    }

    #[test]
    fn a_rateless_audio_track_renders_without_kbps_or_khz_suffixes() {
        let mut audio = stream(4352, TsStreamType::LpcmAudio);
        audio.codec_alt_name = "LPCM";
        audio.channel_description = "2.0".to_owned();
        let bdrom = disc(vec![presented(vec![audio])]);
        let rendered = render(&bdrom, &[]);
        assert!(rendered.contains("LPCM 2.0                                  "));
        assert!(rendered.contains("    0 kbps"));
    }

    #[test]
    fn the_secondary_audio_cell_takes_the_first_same_language_main_track() {
        let mut first = stream(4352, TsStreamType::DtsHdMasterAudio);
        first.language_code = "eng".to_owned();
        let mut wrong_language = stream(4353, TsStreamType::Ac3Audio);
        wrong_language.language_code = "fra".to_owned();
        let mut secondary_type = stream(4354, TsStreamType::Ac3PlusSecondaryAudio);
        secondary_type.language_code = "eng".to_owned();
        let mut express = stream(4355, TsStreamType::DtsHdSecondaryAudio);
        express.language_code = "eng".to_owned();
        let mut stereo_ac3 = stream(4356, TsStreamType::Ac3Audio);
        stereo_ac3.language_code = "eng".to_owned();
        stereo_ac3.channel_count = 2;
        let mut pick = stream(4357, TsStreamType::Ac3Audio);
        pick.language_code = "eng".to_owned();
        pick.codec_alt_name = "DD AC3";
        pick.channel_description = "5.1".to_owned();

        let streams = [first.clone(), wrong_language, secondary_type, express, stereo_ac3, pick];
        let all: Vec<&StreamSummary> = streams.iter().collect();
        assert_eq!(secondary_audio(&all).map(|s| s.pid.get()), Some(4357));
        assert_eq!(secondary_audio(&[]), None);
        assert_eq!(secondary_audio(&[&first]), None);
    }

    /// An angled playlist: two extra angles, the first carrying its own clip,
    /// the second empty (its measured length is zero).
    fn angled_disc() -> BdRom {
        let mut video = stream(4113, TsStreamType::AvcVideo);
        video.codec_name = "MPEG-4 AVC Video".to_owned();
        video.codec_alt_name = "AVC";
        video.bitrate = 24_000_000;
        let mut angle_one = video.clone();
        angle_one.angle_index = 1;
        angle_one.bitrate = 23_000_000;
        angle_one.active_bitrate = 23_500_000;
        let mut angle_two = video.clone();
        angle_two.angle_index = 2;

        let main_clip = clip("00010.M2TS", 30.0, 50_000);
        let mut angle_clip = clip("00011.M2TS", 30.0, 40_000);
        angle_clip.angle_index = 1;

        let mut list = playlist("00010.MPLS", 30.0);
        list.angle_count = 2;
        list.streams = vec![video, angle_one, angle_two];
        list.clips = vec![main_clip, angle_clip];
        disc(vec![list])
    }

    #[test]
    fn an_angled_playlist_reports_per_angle_lines_and_suffixes() {
        let rendered = render(&angled_disc(), &[]);
        // Angle 1 owns one clip; its whole-timeline size replaces the main
        // clip at the shared start time (last write wins).
        assert!(rendered.contains(
            "Angle 1 Length:         00:00:30.000 (h:mm:ss.ms) / 00:00:30.000 (h:mm:ss.ms)\r\n"
        ));
        assert!(rendered.contains("Angle 1 Size:           7,680,000 bytes / 7,680,000 bytes\r\n"));
        assert!(rendered.contains("Angle 1 Total Bitrate:  2.05 Mbps / 2.05 Mbps\r\n"));
        // Angle 2 has no clips of its own: zero length, the main timeline.
        assert!(rendered.contains(
            "Angle 2 Length:         00:00:00.000 (h:mm:ss.ms) / 00:00:30.000 (h:mm:ss.ms)\r\n"
        ));
        assert!(rendered.contains("Angle 2 Size:           0 bytes / 9,600,000 bytes\r\n"));
        assert!(rendered.contains("Angle 2 Total Bitrate:  0.00 Mbps / 2.56 Mbps\r\n"));
        assert!(rendered.contains("All Angles Length:      00:01:00.000 (h:m:s.ms)\r\n"));
        assert!(rendered.contains("All Angles Size:        17,280,000 bytes\r\n"));
        assert!(rendered.contains("All Angles Bitrate:     2.30 Mbps\r\n"));
        // The angle video rows and the angle clip's FILES row carry ` (N)`.
        assert!(rendered.contains("MPEG-4 AVC Video (1)    23,000 (23500) kbps"));
        assert!(rendered.contains("MPEG-4 AVC Video (2)    24,000 (0) kbps"));
        assert!(rendered.contains("00011.M2TS (1)  "));
    }

    #[test]
    fn a_zero_length_angle_with_bytes_renders_a_zero_rate() {
        // An angle whose only clip measured zero seconds but real packets:
        // the own-rate is pinned to 0.00 — not a saturated divide-by-zero
        // (the all-zero angle can't tell, NaN also rounds to 0).
        let mut bdrom = angled_disc();
        let mut broken_clip = clip("00012.M2TS", 0.0, 10_000);
        broken_clip.angle_index = 2;
        for list in &mut bdrom.playlists {
            list.clips.push(broken_clip.clone());
        }
        let rendered = render_with(&bdrom, &[0], &[]);
        assert!(rendered.contains("Angle 2 Total Bitrate:  0.00 Mbps"), "{rendered}");

        // A zero TOTAL length with real timeline bytes pins the timeline-rate
        // half the same way (the own-rate half stays measured).
        for list in &mut bdrom.playlists {
            list.total_length = 0.0;
        }
        let rendered = render_with(&bdrom, &[0], &[]);
        assert!(rendered.contains("Angle 1 Total Bitrate:  2.05 Mbps / 0.00 Mbps"), "{rendered}");
    }

    #[test]
    fn a_zero_length_playlist_renders_zero_angle_timeline_rates() {
        let mut bdrom = angled_disc();
        for list in &mut bdrom.playlists {
            list.total_length = 0.0;
            list.clips.clear();
            list.chapters.clear();
        }
        let rendered = render_with(&bdrom, &[0], &[]);
        assert!(rendered.contains("Angle 1 Total Bitrate:  0.00 Mbps / 0.00 Mbps\r\n"));
    }

    #[test]
    fn diagnostics_deduplicate_clips_and_skip_unpresented_tallies() {
        let mut video = stream(4113, TsStreamType::AvcVideo);
        video.codec_name = "MPEG-4 AVC Video".to_owned();
        let tally = ClipStreamTally {
            pid: Pid::new(4113),
            stream_type: TsStreamType::AvcVideo,
            codec_short_name: "AVC".to_owned(),
            payload_bytes: 1_000_000,
            packet_count: 6_000,
        };
        let unpresented = ClipStreamTally {
            pid: Pid::new(4400),
            stream_type: TsStreamType::Ac3Audio,
            codec_short_name: "AC3".to_owned(),
            payload_bytes: 1,
            packet_count: 1,
        };
        let mut first = clip("00020.M2TS", 10.0, 10_000);
        first.streams = vec![tally.clone(), unpresented];
        let mut replay = clip("00020.M2TS", 10.0, 10_000);
        replay.streams = vec![tally.clone()];
        let mut empty = clip("00021.M2TS", 10.0, 10_000);
        empty.streams = Vec::new();
        let mut unmeasured = clip("00022.M2TS", 0.0, 0);
        unmeasured.file_seconds = 0.0;
        unmeasured.streams = vec![tally];

        let mut list = playlist("00020.MPLS", 30.0);
        list.streams = vec![video];
        list.clips = vec![first, replay, empty, unmeasured];
        let rendered = render_with(&disc(vec![list]), &[0], &[]);

        // One row per deduplicated measured clip; the unpresented PID and the
        // empty clip render nothing; the unmeasured clip reads `0` seconds.
        assert_eq!(rendered.matches("00020.M2TS      4113 (0x1011)").count(), 1);
        assert!(!rendered.contains("4400"));
        assert!(!rendered.contains("00021.M2TS      4113"));
        assert!(rendered.contains("00022.M2TS      4113 (0x1011)   0x1B            AVC"));
        let zero_seconds = format!("{:<24}{:<24}", "", "0");
        assert!(rendered.contains(&zero_seconds));
        assert!(rendered.contains("      0 kbps"));
    }

    #[test]
    fn a_3d_clip_is_named_by_its_ssif_file() {
        // A 3D clip: the model carries both names; the report names it by the
        // interleaved `*.ssif` in FILES and STREAM DIAGNOSTICS.
        let mut list = presented(vec![stream(4113, TsStreamType::AvcVideo)]);
        let mut interleaved = clip("00002.M2TS", 60.0, 100_000);
        interleaved.display_name = "00002.SSIF".to_owned();
        interleaved.streams = vec![ClipStreamTally {
            pid: Pid::new(4113),
            stream_type: TsStreamType::AvcVideo,
            codec_short_name: "AVC".to_owned(),
            payload_bytes: 1_000_000,
            packet_count: 6_000,
        }];
        list.clips = vec![interleaved];
        let bdrom = disc(vec![list]);

        let rendered = render(&bdrom, &[]);
        assert!(rendered.contains("00002.SSIF      0:00:00.000"));
        assert!(rendered.contains("00002.SSIF      4113 (0x1011)"));
    }

    #[test]
    fn formatting_helpers_pin_their_edge_rules() {
        // `N0` grouping, signed and unsigned.
        assert_eq!(group(0), "0");
        assert_eq!(group(1_234_567), "1,234,567");
        assert_eq!(group_signed(-1_234), "-1,234");
        // The two-decimal hundredths spelling clamps negatives to zero.
        assert_eq!(cents(637), "6.37");
        assert_eq!(cents(-1), "0.00");
        // Exact ties round to even; guards spell zero.
        assert_eq!(fixed_even(19.125, 2), "19.12");
        assert_eq!(fixed_even(19.135, 2), "19.14");
        assert_eq!(fixed_even(0.0, 2), "0.00");
        assert_eq!(fixed_even(-1.0, 3), "0.000");
        assert_eq!(fixed_even(f64::NAN, 3), "0.000");
        // Frame-size cells round half-to-even, like .NET `N0`: the exact 93296.5
        // tie lands on the even 93,296, not 93,297.
        assert_eq!(bytes_cell(93_296.5), " 93,296 bytes");
        assert_eq!(bytes_cell(93_297.5), " 93,298 bytes");
        assert_eq!(bytes_cell(31_000.4), " 31,000 bytes");
        assert_eq!(bytes_cell(95_000.6), " 95,001 bytes");
        // Stream-table kbps rounds half-to-even.
        assert_eq!(kbps(2_500), 2);
        assert_eq!(kbps(3_500), 4);
    }

    #[test]
    fn time_spellings_truncate_wrap_and_clamp() {
        // 26 h 3 min 4.5678 s: hours wrap at 24, millis truncate.
        let ticks = 938_045_678_999_i64;
        assert_eq!(time_h(ticks), "2:03:24.567");
        assert_eq!(time_hh(ticks), "02:03:24.567");
        assert_eq!(time_hh_short(ticks), "02:03:24");
        assert_eq!(time_parts(-1), (0, 0, 0, 0));
    }
}
