//! The `Disc` mirror — the scanned disc model as plain data JavaScript can hold.
//!
//! One type here per model type in [`bdinfo_rs_core`], field for field:
//!
//! | Mirror | Core |
//! |---|---|
//! | [`Disc`] | [`BdRom`] plus the scan mode and the recorded failures |
//! | [`Playlist`] | [`PlaylistSummary`] |
//! | [`Clip`] | [`ClipSummary`] |
//! | [`Stream`] | [`StreamSummary`] |
//! | [`ClipStream`] | [`ClipStreamTally`] |
//! | [`Chapter`] | [`ChapterSummary`] |
//! | [`ScanError`] | [`ScanError`](bdinfo_rs_core::error::ScanError) |
//! | [`ScanStage`] | [`ScanStage`](bdinfo_rs_core::error::ScanStage) |
//! | [`ScanErrorReason`] | [`BdError`](bdinfo_rs_core::error::BdError) |
//! | [`HiddenRule`] | [`HiddenRule`](bdinfo_rs_core::bdrom::order::HiddenRule) |
//!
//! ## Conventions
//!
//! * **Complete.** Every field of every core type appears here. That completeness is the point: it
//!   is what lets a disc be rebuilt from the mirror and re-rendered as the identical report, and a
//!   field left out would be invisible until that render came back wrong.
//! * **Numbers stay numbers.** Nothing is pre-formatted for display, so a consumer can sort,
//!   compare and chart the values. Only strings the scan itself composed, and which a consumer
//!   could therefore not reconstruct, cross as strings: codec and language names, the stream
//!   descriptions.
//! * **Units live in the field name** wherever a bare number would be ambiguous — `size_bytes`,
//!   `bitrate_bps`, `length_seconds`, `sample_rate_hz`. The name is the unit of record; the core
//!   field it mirrors is frequently named without one.
//! * **Wire names are camelCase**, from `#[serde(rename_all = "camelCase")]` on every type.
//!
//! ## Doc comments here are published documentation
//!
//! `tsify` copies the doc comment of each type and field below into the generated TypeScript
//! declaration as `JSDoc`, so those lines are what a consumer reads in an editor. Write them for
//! that reader, and note three things `tsify` 0.5.6 does to them on the way out:
//!
//! * An apostrophe is emitted escaped, as `\'` — so avoid it.
//! * A rustdoc intra-doc link is emitted verbatim, brackets and path and all, which resolves to
//!   nothing in TypeScript. Name a sibling field in plain backticks instead, under its published
//!   camelCase name.
//! * The doc comment of an **enum variant** is dropped. What a consumer must know to use
//!   [`ScanStage`] or [`ScanErrorReason`] therefore belongs on the enum itself.
//!
//! ## Building one, and taking it apart again
//!
//! [`Disc::from_scan`] mirrors a whole scanned disc; every other type here is reached through a
//! [`From`] impl it walks. [`Disc::into_scan`] goes back the other way, rebuilding the [`BdRom`]
//! and the recorded failures so the report can be rendered from a mirror alone.
//!
//! [`ScanResult`] is the one type here with no counterpart in the model: it pairs a rendered
//! report with the [`Disc`] it was rendered beside, for a scan that returns both.

use std::io;

use bdinfo_rs_core::bdrom::chapters::{ChapterSummary, seconds_to_ticks};
use bdinfo_rs_core::bdrom::disc::{
    BdRom, ClipStreamTally, ClipSummary, PlaylistSummary, StreamSummary,
};
use bdinfo_rs_core::bdrom::order::{self, PlaylistFilter, table_rows};
use bdinfo_rs_core::error;
use bdinfo_rs_core::primitives::Pid;
use bdinfo_rs_core::stream::TsStreamType;
use serde::{Deserialize, Serialize};
use tsify::Tsify;

/// What one measured scan produced: the classic report, and the same scan as
/// structured data.
///
/// Both come from a single pass over the media, so a consumer that wants the
/// report and the model never has to read the disc twice. Neither is derived
/// from the other: the report is rendered straight from the scan, and the disc
/// mirrors that same scan.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[tsify(into_wasm_abi)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    /// The classic disc report, rendered with the sections the scan was asked
    /// for.
    pub report: String,
    /// The scanned disc, with `measured` true.
    pub disc: Disc,
}

/// A scanned Blu-ray disc: the disc-level properties and every playlist on it.
// `into_wasm_abi` is what lets an export return a `Disc` by value: it implements
// wasm-bindgen's `IntoWasmAbi` over `serde_wasm_bindgen`, so the value crosses as a real
// JavaScript object. `from_wasm_abi` is its inverse, letting an export take one back as a
// parameter. Only the type an export names needs them; the types below are reached
// through this one.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[tsify(into_wasm_abi, from_wasm_abi)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "seven independent disc properties (3D/50Hz/UHD/BD+/BD-Java/D-BOX/PSP) plus the scan-mode flag, not a state machine"
)]
pub struct Disc {
    /// The disc volume label. For a disc image this is the volume identifier
    /// recorded in the filesystem; for a picked folder it is the name of the
    /// folder holding `BDMV`, which carries no real volume label.
    pub volume_label: String,
    /// The disc title declared in `META/bdmt_eng.xml`, absent when the disc
    /// declares none or declares only the placeholder `blu-ray`.
    pub disc_title: Option<String>,
    /// Total size of the disc in bytes, excluding the interleaved `*.ssif`
    /// files counted by `interleavedSizeBytes`.
    pub size_bytes: u64,
    /// Total size of the interleaved `*.ssif` files in bytes — the bytes
    /// `sizeBytes` leaves out. Zero on a disc that is not 3D.
    pub interleaved_size_bytes: u64,
    /// Whether the disc carries 3D video: a non-empty `STREAM/SSIF` directory.
    pub is_3d: bool,
    /// Whether the disc carries 50 Hz content: any playlist video stream at 25
    /// or 50 frames per second.
    pub is_50hz: bool,
    /// Whether the disc is 4K UHD: the `INDX0300` version magic in
    /// `index.bdmv`.
    pub is_uhd: bool,
    /// Whether the disc carries BD+ copy protection: a `BDSVM`, `SLYVM` or
    /// `ANYVM` directory.
    pub is_bd_plus: bool,
    /// Whether the disc carries BD-Java: a non-empty `BDJO` directory.
    pub is_bd_java: bool,
    /// Whether the disc carries D-BOX motion code: a `FilmIndex.xml` file in
    /// the disc root.
    pub is_dbox: bool,
    /// Whether the disc carries PSP or mobile content: a `*.mnv` file under
    /// `SNP`.
    pub is_psp: bool,
    /// Every playlist on the disc, sorted by file name.
    ///
    /// Nothing is ever filtered out, the short and looping playlists a
    /// selection table withholds included: each one carries the rules that
    /// withhold it in `hiddenBy`, so a consumer narrows the list itself
    /// without scanning again. Sorting by file name rather than by table
    /// position is what makes `position` a field.
    pub playlists: Vec<Playlist>,
    /// Whether the packet scan ran. When false the scan read only the disc
    /// metadata, and every measured value below — bitrates, packet counts,
    /// chapter rates — is zero because it was never measured rather than
    /// because it is genuinely zero. This flag is the only way to tell those
    /// two apart.
    pub measured: bool,
    /// Per-file failures the scan recorded and continued past. Empty for a
    /// scan of healthy media.
    pub errors: Vec<ScanError>,
}

/// One playlist (`*.MPLS`) with its streams, clips and chapters.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    /// The playlist file name in upper case, for example `00000.MPLS`.
    pub name: String,
    /// Total presentation length in seconds.
    pub total_length_seconds: f64,
    /// The same length in 100 ns ticks, truncated toward zero — the unit the
    /// report and the selection table format their `hh:mm:ss` cells from.
    /// Integer-dividing this by 10,000,000 reproduces those cells exactly,
    /// where formatting `totalLengthSeconds` can land a tick either side.
    pub total_length_ticks: i64,
    /// Summed size of the clip `*.m2ts` files in bytes.
    pub file_size_bytes: u64,
    /// Summed size of the clip interleaved `*.ssif` files in bytes.
    pub interleaved_file_size_bytes: u64,
    /// Number of chapter marks.
    pub chapter_count: usize,
    /// Number of presented streams. Counts the same rows the report presents,
    /// so it excludes the rows flagged `ssifOnly` and is not always the length
    /// of `streams`.
    pub stream_count: usize,
    /// Number of extra camera angles, zero for a playlist with a single angle.
    ///
    /// A playlist can declare at most 254, so that is also as many angle blocks
    /// as a rendered report will carry: a disc handed back with a larger count
    /// here renders the first 254 rather than one block per angle.
    pub angle_count: usize,
    /// Whether the playlist loops: two main-angle clips replay the same clip
    /// file from the same in-time. A selection table withholds a looping
    /// playlist by default.
    pub has_loops: bool,
    /// The shared-clip group this playlist belongs to, numbered from 1 — the
    /// Group column of the selection table. Playlists sharing any clip file
    /// land in one group, and the groups are numbered in table order, so group
    /// 1 holds the longest playlist on the disc.
    ///
    /// Computed over the whole disc, so it does not move when a consumer
    /// narrows the list.
    pub group: usize,
    /// Where this playlist sits in the selection table, numbered from 1 —
    /// group order, and longest first inside each group.
    ///
    /// Computed over the whole disc, like `group`. Sorting `playlists` by it
    /// reproduces the table the classic report prints, which is why it is a
    /// field rather than the array index: the array stays in the file-name
    /// order the disc records.
    pub position: usize,
    /// The filter rules that classify this playlist as withheld: `short` for a
    /// playlist under the length threshold in force, `looping` for one that
    /// replays a clip, both, or none.
    ///
    /// A selection table shows the playlists whose `hiddenBy` is empty, so
    /// filtering on this field client-side is the whole of what the two
    /// playlist-filter switches of the classic report do — no rescan needed.
    /// The threshold behind `short` defaults to 20 seconds, and
    /// `shortPlaylistSeconds` on the scanning calls is what moves it.
    pub hidden_by: Vec<HiddenRule>,
    /// The presented streams, sorted by PID.
    pub streams: Vec<Stream>,
    /// The sequenced clips in playlist order, angle clips included.
    pub clips: Vec<Clip>,
    /// One entry per chapter mark, carrying the measured video statistics of
    /// that chapter. Present without the packet scan too, but then only the
    /// times are filled in.
    pub chapters: Vec<Chapter>,
}

/// One reason a selection table withholds a playlist, and one member of a
/// `hiddenBy`.
///
/// `short` means the playlist is strictly shorter than the length threshold in
/// force — 20 seconds unless `shortPlaylistSeconds` moved it — and a playlist
/// of exactly the threshold length is not short. `looping` means it replays a
/// clip. The two rules are judged independently, so a playlist can name both.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HiddenRule {
    Short,
    Looping,
}

/// One clip (`*.M2TS`) as one playlist sequences it.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    /// The clip `*.m2ts` file name in upper case, for example `00001.M2TS`.
    pub name: String,
    /// The name to display: the interleaved `*.ssif` file name when the clip
    /// has one, otherwise `name`.
    pub display_name: String,
    /// Size of the clip `*.m2ts` file in bytes, zero when the file is missing.
    /// This is the estimated size, the one available without a packet scan.
    pub file_size_bytes: u64,
    /// Size of the clip interleaved `*.ssif` file in bytes, zero when the clip
    /// has none.
    pub interleaved_file_size_bytes: u64,
    /// Which camera angle this clip belongs to; zero is the main angle.
    pub angle_index: i32,
    /// Where the clip starts on the playlist timeline, in seconds.
    pub relative_time_in_seconds: f64,
    /// How long the clip runs, in seconds.
    pub length_seconds: f64,
    /// Payload bytes the packet scan attributed to this clip.
    pub payload_bytes: u64,
    /// Transport packets the packet scan attributed to this clip. The
    /// packet-derived size in bytes is this count times 192.
    pub packet_count: u64,
    /// How many seconds of content the packet scan demuxed for this clip.
    pub packet_seconds: f64,
    /// Presentation length of the whole clip file in seconds, zero when the
    /// file is missing. Differs from `lengthSeconds` when the playlist
    /// presents only part of the file.
    pub file_seconds: f64,
    /// Whole-file tallies for each stream in this clip file, in the order the
    /// file registers them, limited to the streams the playlist presents.
    pub streams: Vec<ClipStream>,
}

/// One stream measured across one whole clip file.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClipStream {
    /// The packet identifier. The report prints it zero-padded to five digits.
    pub pid: u16,
    /// The elementary-stream type, as the `stream_coding_type` byte recorded
    /// on the disc. This is the type the clip file itself registers, which can
    /// differ from the type the playlist declares for the same packet
    /// identifier — the one a `Stream` carries.
    pub stream_type: u8,
    /// The short codec label of the stream as the clip file registers it, for
    /// example `AVC`.
    pub codec_short_name: String,
    /// Payload bytes demuxed for this packet identifier across the whole clip
    /// file.
    pub payload_bytes: u64,
    /// Transport packets seen on this packet identifier across the whole clip
    /// file.
    pub packet_count: u64,
}

/// One stream as one playlist presents it.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Stream {
    /// The packet identifier. The report prints it zero-padded to five digits.
    pub pid: u16,
    /// The elementary-stream type, as the `stream_coding_type` byte recorded
    /// on the disc — for example 27 (`0x1B`) for MPEG-4 AVC video, 134
    /// (`0x86`) for DTS-HD Master Audio. The report never prints it: it selects
    /// which table the row lands in (video, audio, subtitles or text), so the
    /// number is the only faithful mirror of it.
    pub stream_type: u8,
    /// The short codec name, for example `AVC` or `DTS-HD MA`.
    pub codec_short_name: String,
    /// The long codec name, for example `MPEG-4 AVC Video`.
    pub codec_name: String,
    /// The alternate codec name some report tables print instead of
    /// `codecName`, for example `DD AC3`.
    pub codec_alt_name: String,
    /// The stream bitrate in bits per second: measured by the packet scan, or
    /// for a constant-rate codec taken from the coded nominal rate. On the
    /// per-angle copy of a video row it is the rate measured for that angle.
    /// Zero without the packet scan.
    pub bitrate_bps: i64,
    /// The bitrate in bits per second over the region where the stream is
    /// active, measured by the packet scan. Filled in for video and Dolby
    /// `TrueHD` only; zero otherwise, and zero without the packet scan.
    pub active_bitrate_bps: i64,
    /// The language name, empty when the stream declares no language.
    pub language_name: String,
    /// The ISO 639 language code, for example `eng`. Empty when the stream
    /// declares no language.
    pub language_code: String,
    /// The stream description as the metadata alone gives it: resolution,
    /// frame rate and HDR for video; channels, sample rate and bit depth for
    /// audio. Empty for graphics and text streams on a scan without the packet
    /// scan.
    pub description: String,
    /// The description including what only the packet scan can know: the
    /// measured rate of a variable-rate audio stream, net of the rate of an
    /// embedded `TrueHD` core, and the exact four-decimal HEVC luminance
    /// spelling. Equal to `description` otherwise. This is the text the report
    /// prints.
    pub full_description: String,
    /// The audio channel layout, for example `5.1`. Empty for a stream that is
    /// not audio.
    pub channel_description: String,
    /// The audio sample rate in hertz. Zero for a stream that is not audio, or
    /// when the rate is unknown.
    pub sample_rate_hz: i32,
    /// The audio bit depth. Zero for a stream that is not audio, or when the
    /// depth is unknown.
    pub bit_depth: i32,
    /// The audio channel count, not counting the LFE channel. Zero for a
    /// stream that is not audio, or when the count is unknown.
    pub channel_count: i32,
    /// The video picture height in pixels, for example 1080. Zero for a stream
    /// that is not video, or when the height is unknown.
    pub height_pixels: i32,
    /// Which camera angle this row presents: zero for the main presentation
    /// row, and 1 upwards for the per-angle copies of a video stream, each
    /// carrying the rates measured for its own angle.
    pub angle_index: usize,
    /// Whether the playlist hides the stream: the clip carries it, but the
    /// playlist stream-number table does not declare it.
    pub is_hidden: bool,
    /// Whether the stream is reached only through the interleaved `*.ssif`
    /// dependent-view scan — the 3D MVC video the clip information omits.
    /// These rows are excluded from the playlist `streamCount`.
    pub ssif_only: bool,
}

/// The measured video statistics of one chapter.
///
/// Times are seconds from the start of the playlist, rates are bits per second,
/// sizes are bytes. Every value except the times is zero without the packet
/// scan.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Chapter {
    /// When the chapter starts, in seconds.
    pub time_in_seconds: f64,
    /// How long the chapter runs, in seconds: up to the next chapter mark, or
    /// for the last chapter up to the total length of the playlist.
    pub length_seconds: f64,
    /// Average video rate over the chapter in bits per second. Zero for a
    /// chapter of zero length.
    pub avg_rate_bps: f64,
    /// The highest video rate over any one-second window, in bits per second.
    pub max_1sec_rate_bps: f64,
    /// When the highest one-second window starts, in seconds.
    pub max_1sec_time_seconds: f64,
    /// The highest video rate over any five-second window, in bits per second.
    pub max_5sec_rate_bps: f64,
    /// When the highest five-second window starts, in seconds.
    pub max_5sec_time_seconds: f64,
    /// The highest video rate over any ten-second window, in bits per second.
    pub max_10sec_rate_bps: f64,
    /// When the highest ten-second window starts, in seconds.
    pub max_10sec_time_seconds: f64,
    /// Average video frame size in bytes over the frames the codec tagged.
    /// Zero when it tagged none.
    pub avg_frame_size_bytes: f64,
    /// The largest single video frame in bytes.
    pub max_frame_size_bytes: f64,
    /// When the largest video frame occurs, in seconds.
    pub max_frame_time_seconds: f64,
}

/// One file the scan could not read or parse, and continued past.
///
/// A scan never stops on a bad file: it records the failure here and scans the
/// readable rest. The report lists these under its `WARNING:` heading, printing
/// the `file` and a message describing the `reason`.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScanError {
    /// The file or directory that failed, named as it is on the disc.
    pub file: String,
    /// Which part of the scan hit the failure.
    pub stage: ScanStage,
    /// What went wrong.
    pub reason: ScanErrorReason,
}

/// Which part of a scan a failure occurred in.
///
/// A scan isolates failures per file class, so one unreadable clip never costs
/// more than that clip.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScanStage {
    /// Listing directories or reading the disc-level metadata: directory
    /// sizes, the disc flags, `index.bdmv`, `bdmt_eng.xml`.
    Discovery,
    /// Parsing a clip-information (`*.clpi`) file.
    ClipInfo,
    /// Parsing a playlist (`*.mpls`) file, or resolving the clips it names.
    Playlist,
    /// Packet-scanning a stream (`*.m2ts`) file.
    #[serde(rename = "stream")]
    StreamFile,
    /// Reading a range of sectors out of a disc image — a bad sector under the
    /// data of a file, which the scan records and reads back as zeros.
    #[serde(rename = "sector")]
    SectorRead,
}

/// What went wrong on one file a scan could not read or parse.
///
/// The `kind` property discriminates the union; the other properties of each
/// member carry what that kind of failure knows.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ScanErrorReason {
    /// A metadata file began with a type magic the parser does not accept, for
    /// example a `*.mpls` not starting with `MPLS0100`, `MPLS0200` or
    /// `MPLS0300`.
    #[serde(rename_all = "camelCase")]
    UnknownFileType {
        /// The magic the file actually began with.
        magic: String,
    },
    /// A field ran past the end of the file: it is truncated or otherwise
    /// malformed.
    UnexpectedEof,
    /// The folder holds no recognizable Blu-ray structure: no `BDMV`
    /// directory, or a `BDMV` without both `CLIPINF` and `PLAYLIST`.
    StructureNotFound,
    /// A playlist named a clip-information (`*.clpi`) file that is not on the
    /// disc.
    #[serde(rename_all = "camelCase")]
    MissingClipFile {
        /// The name of the missing file.
        file: String,
    },
    /// Reading the disc failed: listing a directory, or opening or reading a
    /// file.
    #[serde(rename_all = "camelCase")]
    Io {
        /// What the read failure reported.
        message: String,
    },
    /// A metadata file is larger than the parser will buffer whole. Authored
    /// metadata is kilobytes, so this fires only on malformed or hostile
    /// input.
    #[serde(rename_all = "camelCase")]
    MetadataTooLarge {
        /// The file, named as it is on the disc.
        file: String,
        /// The cap the file exceeded, in bytes.
        limit_bytes: u64,
    },
    /// A failure this mirror does not name, carrying the message it reported.
    ///
    /// Two things land here. A scan the caller cancelled — which is a
    /// whole-scan abort rather than a per-file failure, so it reaches a
    /// recorded failure only if one is fabricated. And any failure kind added
    /// to the scanner after this mirror was written, since the set of kinds is
    /// open. This is the one member whose message a rebuilt disc does not carry
    /// back: it rebuilds as the cancelled-scan failure, the only kind that
    /// reaches it today, so a kind that starts occurring in earnest wants a
    /// member of its own.
    #[serde(rename_all = "camelCase")]
    Other {
        /// What the failure reported.
        message: String,
    },
}

// ── the forward mapping: a scanned disc into the mirror ─────────────────────
//
// One `From` impl per mirrored type, nesting the way the model does. The
// elementary-stream type and the packet identifier are the only fields that
// change representation on the way across: both are newtypes over the byte and
// the 16-bit word the disc records, and both cross as that raw number.

impl Disc {
    /// Mirrors a scanned disc: `bdrom` with the per-file failures `errors` the
    /// scan recorded, and every playlist the scan produced.
    ///
    /// `measured` states whether the packet scan ran. Derive it from the scan
    /// mode rather than from the values: a measured scan can legitimately
    /// report a zero, and only the scan mode tells that apart from a value
    /// nothing ever measured.
    ///
    /// `filter` supplies the short-playlist threshold each playlist is
    /// classified against for its `hiddenBy`; its two switches are not read,
    /// because no playlist is ever dropped here.
    #[must_use]
    pub fn from_scan(
        bdrom: &BdRom,
        errors: &[error::ScanError],
        measured: bool,
        filter: &PlaylistFilter,
    ) -> Self {
        // The table numbering describes the disc, not a view of it, so it is
        // taken over every playlist: a group or a position that shifted when a
        // consumer changed a display filter would be describing the view.
        let rows = table_rows(&bdrom.playlists, &PlaylistFilter::everything());
        Self {
            volume_label: bdrom.volume_label.clone(),
            disc_title: bdrom.disc_title.clone(),
            size_bytes: bdrom.size,
            interleaved_size_bytes: bdrom.interleaved_size,
            is_3d: bdrom.is_3d,
            is_50hz: bdrom.is_50hz,
            is_uhd: bdrom.is_uhd,
            is_bd_plus: bdrom.is_bd_plus,
            is_bd_java: bdrom.is_bd_java,
            is_dbox: bdrom.is_dbox,
            is_psp: bdrom.is_psp,
            playlists: bdrom
                .playlists
                .iter()
                .enumerate()
                .map(|(index, playlist)| {
                    // A keep-everything filter places every playlist in the
                    // table exactly once, so the search always finds this one
                    // and the (0, 0) default is unreachable.
                    let (group, position) = rows
                        .iter()
                        .enumerate()
                        .find(|(_, (_, at))| *at == index)
                        .map_or((0, 0), |(row, (group, _))| (*group, row.saturating_add(1)));
                    Playlist::mirror(
                        playlist,
                        group,
                        position,
                        filter.classify(playlist).into_iter().map(HiddenRule::from).collect(),
                    )
                })
                .collect(),
            measured,
            errors: errors.iter().map(ScanError::from).collect(),
        }
    }
}

impl Playlist {
    /// Mirrors one playlist, with the table numbering and the classification
    /// its own fields cannot supply — those describe where the playlist sits
    /// among the others, which only the whole disc knows.
    fn mirror(
        playlist: &PlaylistSummary,
        group: usize,
        position: usize,
        hidden_by: Vec<HiddenRule>,
    ) -> Self {
        Self {
            name: playlist.name.clone(),
            total_length_seconds: playlist.total_length,
            total_length_ticks: seconds_to_ticks(playlist.total_length),
            file_size_bytes: playlist.file_size,
            interleaved_file_size_bytes: playlist.interleaved_file_size,
            chapter_count: playlist.chapter_count,
            stream_count: playlist.stream_count,
            angle_count: playlist.angle_count,
            has_loops: playlist.has_loops,
            group,
            position,
            hidden_by,
            streams: playlist.streams.iter().map(Stream::from).collect(),
            clips: playlist.clips.iter().map(Clip::from).collect(),
            chapters: playlist.chapters.iter().map(Chapter::from).collect(),
        }
    }
}

impl From<order::HiddenRule> for HiddenRule {
    fn from(rule: order::HiddenRule) -> Self {
        match rule {
            order::HiddenRule::Short => Self::Short,
            order::HiddenRule::Looping => Self::Looping,
        }
    }
}

impl From<&ClipSummary> for Clip {
    fn from(clip: &ClipSummary) -> Self {
        Self {
            name: clip.name.clone(),
            display_name: clip.display_name.clone(),
            file_size_bytes: clip.file_size,
            interleaved_file_size_bytes: clip.interleaved_file_size,
            angle_index: clip.angle_index,
            relative_time_in_seconds: clip.relative_time_in,
            length_seconds: clip.length,
            payload_bytes: clip.payload_bytes,
            packet_count: clip.packet_count,
            packet_seconds: clip.packet_seconds,
            file_seconds: clip.file_seconds,
            streams: clip.streams.iter().map(ClipStream::from).collect(),
        }
    }
}

impl From<&ClipStreamTally> for ClipStream {
    fn from(tally: &ClipStreamTally) -> Self {
        Self {
            pid: tally.pid.get(),
            stream_type: tally.stream_type as u8,
            codec_short_name: tally.codec_short_name.clone(),
            payload_bytes: tally.payload_bytes,
            packet_count: tally.packet_count,
        }
    }
}

impl From<&StreamSummary> for Stream {
    fn from(stream: &StreamSummary) -> Self {
        Self {
            pid: stream.pid.get(),
            // `TsStreamType` is `#[repr(u8)]` over the on-disc
            // `stream_coding_type` codes, so the discriminant IS the byte the
            // disc records.
            stream_type: stream.stream_type as u8,
            codec_short_name: stream.codec_short_name.clone(),
            codec_name: stream.codec_name.clone(),
            codec_alt_name: stream.codec_alt_name.to_owned(),
            bitrate_bps: stream.bitrate,
            active_bitrate_bps: stream.active_bitrate,
            language_name: stream.language_name.clone(),
            language_code: stream.language_code.clone(),
            description: stream.description.clone(),
            full_description: stream.full_description.clone(),
            channel_description: stream.channel_description.clone(),
            sample_rate_hz: stream.sample_rate,
            bit_depth: stream.bit_depth,
            channel_count: stream.channel_count,
            height_pixels: stream.height,
            angle_index: stream.angle_index,
            is_hidden: stream.is_hidden,
            ssif_only: stream.ssif_only,
        }
    }
}

impl From<&ChapterSummary> for Chapter {
    fn from(chapter: &ChapterSummary) -> Self {
        Self {
            time_in_seconds: chapter.time_in,
            length_seconds: chapter.length,
            avg_rate_bps: chapter.avg_rate,
            max_1sec_rate_bps: chapter.max_1sec_rate,
            max_1sec_time_seconds: chapter.max_1sec_time,
            max_5sec_rate_bps: chapter.max_5sec_rate,
            max_5sec_time_seconds: chapter.max_5sec_time,
            max_10sec_rate_bps: chapter.max_10sec_rate,
            max_10sec_time_seconds: chapter.max_10sec_time,
            avg_frame_size_bytes: chapter.avg_frame_size,
            max_frame_size_bytes: chapter.max_frame_size,
            max_frame_time_seconds: chapter.max_frame_time,
        }
    }
}

impl From<&error::ScanError> for ScanError {
    fn from(failure: &error::ScanError) -> Self {
        Self {
            file: failure.file.clone(),
            stage: failure.stage.into(),
            reason: (&failure.reason).into(),
        }
    }
}

impl From<error::ScanStage> for ScanStage {
    fn from(stage: error::ScanStage) -> Self {
        match stage {
            error::ScanStage::ClipInfo => Self::ClipInfo,
            error::ScanStage::Playlist => Self::Playlist,
            error::ScanStage::StreamFile => Self::StreamFile,
            error::ScanStage::SectorRead => Self::SectorRead,
            // `Discovery`, and — `ScanStage` being `#[non_exhaustive]` — any
            // stage added to it later. Reading an unnamed stage as the
            // disc-level one is safe to a consumer: a failure is presented by
            // its file and its reason, and the report never prints the stage.
            _ => Self::Discovery,
        }
    }
}

impl From<&error::BdError> for ScanErrorReason {
    fn from(reason: &error::BdError) -> Self {
        match reason {
            error::BdError::UnknownFileType(magic) => {
                Self::UnknownFileType { magic: magic.clone() }
            }
            error::BdError::UnexpectedEof => Self::UnexpectedEof,
            error::BdError::StructureNotFound => Self::StructureNotFound,
            error::BdError::MissingClipFile(file) => Self::MissingClipFile { file: file.clone() },
            // The wrapped `io::Error`, not the `BdError` around it: the mirror
            // supplies the `io error: ` framing itself when it names the kind.
            error::BdError::Io(err) => Self::Io { message: err.to_string() },
            error::BdError::MetadataTooLarge { file, limit } => {
                Self::MetadataTooLarge { file: file.clone(), limit_bytes: *limit }
            }
            // A cancelled scan, which aborts the whole open rather than being
            // recorded against a file, and — `BdError` being
            // `#[non_exhaustive]` — any kind added to it later. Both cross as
            // the message they reported.
            unmirrored => Self::Other { message: unmirrored.to_string() },
        }
    }
}

// ── the reverse mapping: the mirror back into a scanned disc ────────────────
//
// The inverse of the forward mapping above, one `From` impl per type, so a disc
// that crossed to JavaScript as data can be handed back to the report renderer.
// Every core summary type is a struct of public fields with no
// `#[non_exhaustive]`, so all but three kinds of field invert by plain
// assignment. The exceptions are the packet identifier and the
// elementary-stream type, both rebuilt from the raw number the disc records,
// and the alternate codec name, matched back to the borrowed label the scan
// hands out (see `borrowed_codec_alt_name`).
//
// The impls consume the mirror rather than borrowing it: every string moves
// straight into the summary it belongs to, so rebuilding a disc copies none of
// its text.

/// Every alternate codec label a scan hands out, so an owned copy of one can be
/// matched back to the borrowed original. Kept in the order
/// [`codec_alt_name`](bdinfo_rs_core::stream::TsStream::codec_alt_name) spells
/// them: by stream type, with the three Dolby/DTS labels that upgrade when the
/// audio stream carries its extensions listed beside the plain form.
const CODEC_ALT_NAMES: &[&str] = &[
    "MPEG-1",
    "MPEG-2",
    "AVC",
    "MVC",
    "HEVC",
    "VC-1",
    "MP1",
    "MP2",
    "MPEG-2 AAC",
    "MPEG-4 AAC",
    "LPCM",
    "DD AC3",
    "DD AC3+",
    "Dolby TrueHD",
    "Dolby Atmos",
    "DTS",
    "DTS-HD Hi-Res",
    "DTS:X Hi-Res",
    "DTS Express",
    "DTS-HD Master",
    "DTS:X Master",
    "PGS",
    "IGS",
    "SUB",
    "UNKNOWN",
];

/// The borrowed alternate codec label equal to `name`, or the empty label when
/// no scan hands that name out.
///
/// [`Stream`] mirrors the label as an owned string and a [`StreamSummary`]
/// needs the borrowed form back, which no runtime string can become. The labels
/// are a closed set, so the way back is to match `name` against it. A name
/// outside the set can only reach here from a mirror a consumer edited, and
/// rebuilds as the empty label — which the report prints as an empty cell,
/// rather than as some name a scan never emits.
fn borrowed_codec_alt_name(name: &str) -> &'static str {
    CODEC_ALT_NAMES.iter().copied().find(|known| *known == name).unwrap_or("")
}

impl Disc {
    /// Rebuilds the disc this mirror describes: the [`BdRom`] and the per-file
    /// failures, the two halves a report is rendered from.
    ///
    /// The inverse of [`from_scan`](Self::from_scan) but for `measured`, which
    /// describes how the values were obtained rather than being one of them —
    /// a disc carries no such flag, so it is dropped here.
    #[must_use]
    pub fn into_scan(self) -> (BdRom, Vec<error::ScanError>) {
        let bdrom = BdRom {
            volume_label: self.volume_label,
            disc_title: self.disc_title,
            size: self.size_bytes,
            interleaved_size: self.interleaved_size_bytes,
            is_3d: self.is_3d,
            is_50hz: self.is_50hz,
            is_uhd: self.is_uhd,
            is_bd_plus: self.is_bd_plus,
            is_bd_java: self.is_bd_java,
            is_dbox: self.is_dbox,
            is_psp: self.is_psp,
            playlists: self.playlists.into_iter().map(PlaylistSummary::from).collect(),
        };
        (bdrom, self.errors.into_iter().map(error::ScanError::from).collect())
    }
}

impl From<Playlist> for PlaylistSummary {
    // Four mirror fields have no counterpart to rebuild and are dropped:
    // `totalLengthTicks` is `totalLengthSeconds` in another unit, and `group`,
    // `position` and `hiddenBy` are projections of the whole disc that the
    // presentation order recomputes from the rebuilt playlists.
    fn from(playlist: Playlist) -> Self {
        Self {
            name: playlist.name,
            total_length: playlist.total_length_seconds,
            file_size: playlist.file_size_bytes,
            interleaved_file_size: playlist.interleaved_file_size_bytes,
            chapter_count: playlist.chapter_count,
            stream_count: playlist.stream_count,
            angle_count: playlist.angle_count,
            has_loops: playlist.has_loops,
            streams: playlist.streams.into_iter().map(StreamSummary::from).collect(),
            clips: playlist.clips.into_iter().map(ClipSummary::from).collect(),
            chapters: playlist.chapters.into_iter().map(ChapterSummary::from).collect(),
        }
    }
}

impl From<Clip> for ClipSummary {
    fn from(clip: Clip) -> Self {
        Self {
            name: clip.name,
            display_name: clip.display_name,
            file_size: clip.file_size_bytes,
            interleaved_file_size: clip.interleaved_file_size_bytes,
            angle_index: clip.angle_index,
            relative_time_in: clip.relative_time_in_seconds,
            length: clip.length_seconds,
            payload_bytes: clip.payload_bytes,
            packet_count: clip.packet_count,
            packet_seconds: clip.packet_seconds,
            file_seconds: clip.file_seconds,
            streams: clip.streams.into_iter().map(ClipStreamTally::from).collect(),
        }
    }
}

impl From<ClipStream> for ClipStreamTally {
    fn from(tally: ClipStream) -> Self {
        Self {
            pid: Pid::new(tally.pid),
            stream_type: TsStreamType::from_u8(tally.stream_type),
            codec_short_name: tally.codec_short_name,
            payload_bytes: tally.payload_bytes,
            packet_count: tally.packet_count,
        }
    }
}

impl From<Stream> for StreamSummary {
    fn from(stream: Stream) -> Self {
        Self {
            pid: Pid::new(stream.pid),
            // The inverse of the discriminant cast the forward mapping makes:
            // `from_u8` reads the same on-disc `stream_coding_type` byte back,
            // and a byte no stream type claims reads as `Unknown`.
            stream_type: TsStreamType::from_u8(stream.stream_type),
            codec_short_name: stream.codec_short_name,
            codec_name: stream.codec_name,
            codec_alt_name: borrowed_codec_alt_name(&stream.codec_alt_name),
            bitrate: stream.bitrate_bps,
            active_bitrate: stream.active_bitrate_bps,
            language_name: stream.language_name,
            language_code: stream.language_code,
            description: stream.description,
            full_description: stream.full_description,
            channel_description: stream.channel_description,
            sample_rate: stream.sample_rate_hz,
            bit_depth: stream.bit_depth,
            channel_count: stream.channel_count,
            height: stream.height_pixels,
            angle_index: stream.angle_index,
            is_hidden: stream.is_hidden,
            ssif_only: stream.ssif_only,
        }
    }
}

impl From<Chapter> for ChapterSummary {
    fn from(chapter: Chapter) -> Self {
        Self {
            time_in: chapter.time_in_seconds,
            length: chapter.length_seconds,
            avg_rate: chapter.avg_rate_bps,
            max_1sec_rate: chapter.max_1sec_rate_bps,
            max_1sec_time: chapter.max_1sec_time_seconds,
            max_5sec_rate: chapter.max_5sec_rate_bps,
            max_5sec_time: chapter.max_5sec_time_seconds,
            max_10sec_rate: chapter.max_10sec_rate_bps,
            max_10sec_time: chapter.max_10sec_time_seconds,
            avg_frame_size: chapter.avg_frame_size_bytes,
            max_frame_size: chapter.max_frame_size_bytes,
            max_frame_time: chapter.max_frame_time_seconds,
        }
    }
}

impl From<ScanError> for error::ScanError {
    fn from(failure: ScanError) -> Self {
        Self { file: failure.file, stage: failure.stage.into(), reason: failure.reason.into() }
    }
}

impl From<ScanStage> for error::ScanStage {
    fn from(stage: ScanStage) -> Self {
        match stage {
            ScanStage::Discovery => Self::Discovery,
            ScanStage::ClipInfo => Self::ClipInfo,
            ScanStage::Playlist => Self::Playlist,
            ScanStage::StreamFile => Self::StreamFile,
            ScanStage::SectorRead => Self::SectorRead,
        }
    }
}

impl From<ScanErrorReason> for error::BdError {
    fn from(reason: ScanErrorReason) -> Self {
        match reason {
            ScanErrorReason::UnknownFileType { magic } => Self::UnknownFileType(magic),
            ScanErrorReason::UnexpectedEof => Self::UnexpectedEof,
            ScanErrorReason::StructureNotFound => Self::StructureNotFound,
            ScanErrorReason::MissingClipFile { file } => Self::MissingClipFile(file),
            // The message is the wrapped `io::Error`'s, so it goes back inside a
            // fresh one: the `io error: ` framing the report prints comes from
            // the `BdError` around it, and the original `io::ErrorKind` is not
            // mirrored because nothing prints it.
            ScanErrorReason::Io { message } => Self::Io(io::Error::other(message)),
            ScanErrorReason::MetadataTooLarge { file, limit_bytes } => {
                Self::MetadataTooLarge { file, limit: limit_bytes }
            }
            // The catch-all mirrors a cancelled scan and nothing else today, and
            // that is the failure it rebuilds as — carrying its own message back
            // exactly. Another kind mirrored here would arrive with the
            // cancelled-scan message instead, which is why one that starts
            // occurring wants a member of its own.
            ScanErrorReason::Other { .. } => Self::ScanCancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use bdinfo_rs_core::bdrom::chapters::ChapterSummary;
    use bdinfo_rs_core::bdrom::disc::{
        BdRom, ClipStreamTally, ClipSummary, PlaylistSummary, ScanMode, StreamSummary,
    };
    use bdinfo_rs_core::bdrom::order::PlaylistFilter;
    use bdinfo_rs_core::error;
    use bdinfo_rs_core::primitives::Pid;
    use bdinfo_rs_core::report::text::{self, RenderOptions};
    use bdinfo_rs_core::stream::{TsAudioStream, TsStream, TsStreamType};
    use serde_json::{Value, json};

    use super::{
        Chapter, Clip, ClipStream, Disc, HiddenRule, Playlist, ScanError, ScanErrorReason,
        ScanStage, Stream, borrowed_codec_alt_name, order,
    };

    // One fixture value and one hand-written wire form per mirror type, composed
    // upwards into a whole disc. The wire forms spell the published property
    // names out rather than deriving them, so a rename in the Rust field names
    // surfaces here rather than in a consumer. They are split per type because
    // `serde_json::json!` hits the macro recursion limit on the whole disc in
    // one literal.

    /// A stream with every field set to a distinct value. The numbers are
    /// positional markers, not a plausible stream.
    fn a_stream() -> Stream {
        Stream {
            pid: 4113,
            stream_type: 0x1B,
            codec_short_name: "AVC".to_owned(),
            codec_name: "MPEG-4 AVC Video".to_owned(),
            codec_alt_name: "AVC".to_owned(),
            bitrate_bps: 9,
            active_bitrate_bps: 10,
            language_name: "English".to_owned(),
            language_code: "eng".to_owned(),
            description: "1080p / 23.976 fps".to_owned(),
            full_description: "1080p / 23.976 fps / 16:9".to_owned(),
            channel_description: "5.1".to_owned(),
            sample_rate_hz: 48_000,
            bit_depth: 24,
            channel_count: 5,
            height_pixels: 1080,
            angle_index: 11,
            is_hidden: true,
            ssif_only: false,
        }
    }

    /// The wire form of [`a_stream`].
    fn a_stream_json() -> Value {
        json!({
            "pid": 4113,
            "streamType": 27,
            "codecShortName": "AVC",
            "codecName": "MPEG-4 AVC Video",
            "codecAltName": "AVC",
            "bitrateBps": 9,
            "activeBitrateBps": 10,
            "languageName": "English",
            "languageCode": "eng",
            "description": "1080p / 23.976 fps",
            "fullDescription": "1080p / 23.976 fps / 16:9",
            "channelDescription": "5.1",
            "sampleRateHz": 48_000,
            "bitDepth": 24,
            "channelCount": 5,
            "heightPixels": 1080,
            "angleIndex": 11,
            "isHidden": true,
            "ssifOnly": false,
        })
    }

    /// A whole-file stream tally with every field set to a distinct value.
    fn a_clip_stream() -> ClipStream {
        ClipStream {
            pid: 4352,
            stream_type: 0x86,
            codec_short_name: "DTS-HD MA".to_owned(),
            payload_bytes: 21,
            packet_count: 22,
        }
    }

    /// The wire form of [`a_clip_stream`].
    fn a_clip_stream_json() -> Value {
        json!({
            "pid": 4352,
            "streamType": 134,
            "codecShortName": "DTS-HD MA",
            "payloadBytes": 21,
            "packetCount": 22,
        })
    }

    /// A clip with every field set to a distinct value.
    fn a_clip() -> Clip {
        Clip {
            name: "00001.M2TS".to_owned(),
            display_name: "00001.SSIF".to_owned(),
            file_size_bytes: 12,
            interleaved_file_size_bytes: 13,
            angle_index: 14,
            relative_time_in_seconds: 15.5,
            length_seconds: 16.25,
            payload_bytes: 17,
            packet_count: 18,
            packet_seconds: 19.5,
            file_seconds: 20.5,
            streams: vec![a_clip_stream()],
        }
    }

    /// The wire form of [`a_clip`].
    fn a_clip_json() -> Value {
        json!({
            "name": "00001.M2TS",
            "displayName": "00001.SSIF",
            "fileSizeBytes": 12,
            "interleavedFileSizeBytes": 13,
            "angleIndex": 14,
            "relativeTimeInSeconds": 15.5,
            "lengthSeconds": 16.25,
            "payloadBytes": 17,
            "packetCount": 18,
            "packetSeconds": 19.5,
            "fileSeconds": 20.5,
            "streams": [a_clip_stream_json()],
        })
    }

    /// A chapter with every field set to a distinct value.
    fn a_chapter() -> Chapter {
        Chapter {
            time_in_seconds: 23.5,
            length_seconds: 24.5,
            avg_rate_bps: 25.5,
            max_1sec_rate_bps: 26.5,
            max_1sec_time_seconds: 27.5,
            max_5sec_rate_bps: 28.5,
            max_5sec_time_seconds: 29.5,
            max_10sec_rate_bps: 30.5,
            max_10sec_time_seconds: 31.5,
            avg_frame_size_bytes: 32.5,
            max_frame_size_bytes: 33.5,
            max_frame_time_seconds: 34.5,
        }
    }

    /// The wire form of [`a_chapter`].
    fn a_chapter_json() -> Value {
        json!({
            "timeInSeconds": 23.5,
            "lengthSeconds": 24.5,
            "avgRateBps": 25.5,
            "max1secRateBps": 26.5,
            "max1secTimeSeconds": 27.5,
            "max5secRateBps": 28.5,
            "max5secTimeSeconds": 29.5,
            "max10secRateBps": 30.5,
            "max10secTimeSeconds": 31.5,
            "avgFrameSizeBytes": 32.5,
            "maxFrameSizeBytes": 33.5,
            "maxFrameTimeSeconds": 34.5,
        })
    }

    /// A playlist with every field set to a distinct value. It is 3.5 s long
    /// and loops, so the standard classification names both rules.
    fn a_playlist() -> Playlist {
        Playlist {
            name: "00000.MPLS".to_owned(),
            total_length_seconds: 3.5,
            total_length_ticks: 35_000_000,
            file_size_bytes: 4,
            interleaved_file_size_bytes: 5,
            chapter_count: 6,
            stream_count: 7,
            angle_count: 8,
            has_loops: true,
            group: 1,
            position: 1,
            hidden_by: vec![HiddenRule::Short, HiddenRule::Looping],
            streams: vec![a_stream()],
            clips: vec![a_clip()],
            chapters: vec![a_chapter()],
        }
    }

    /// The wire form of [`a_playlist`].
    fn a_playlist_json() -> Value {
        json!({
            "name": "00000.MPLS",
            "totalLengthSeconds": 3.5,
            "totalLengthTicks": 35_000_000,
            "fileSizeBytes": 4,
            "interleavedFileSizeBytes": 5,
            "chapterCount": 6,
            "streamCount": 7,
            "angleCount": 8,
            "hasLoops": true,
            "group": 1,
            "position": 1,
            "hiddenBy": ["short", "looping"],
            "streams": [a_stream_json()],
            "clips": [a_clip_json()],
            "chapters": [a_chapter_json()],
        })
    }

    /// A recorded failure with every field set to a distinct value.
    fn a_scan_error() -> ScanError {
        ScanError {
            file: "00002.CLPI".to_owned(),
            stage: ScanStage::ClipInfo,
            reason: ScanErrorReason::UnknownFileType { magic: "HDMV9999".to_owned() },
        }
    }

    /// The wire form of [`a_scan_error`].
    fn a_scan_error_json() -> Value {
        json!({
            "file": "00002.CLPI",
            "stage": "clipinfo",
            "reason": { "kind": "unknownFileType", "magic": "HDMV9999" },
        })
    }

    /// A disc with every field of every mirror type set to a distinct value.
    fn a_disc() -> Disc {
        Disc {
            volume_label: "MIRROR_DISC".to_owned(),
            disc_title: Some("Mirror Title".to_owned()),
            size_bytes: 1,
            interleaved_size_bytes: 2,
            is_3d: true,
            is_50hz: false,
            is_uhd: true,
            is_bd_plus: false,
            is_bd_java: true,
            is_dbox: false,
            is_psp: true,
            playlists: vec![a_playlist()],
            measured: true,
            errors: vec![a_scan_error()],
        }
    }

    /// The wire form of [`a_disc`].
    fn a_disc_json() -> Value {
        json!({
            "volumeLabel": "MIRROR_DISC",
            "discTitle": "Mirror Title",
            "sizeBytes": 1,
            "interleavedSizeBytes": 2,
            "is3d": true,
            "is50hz": false,
            "isUhd": true,
            "isBdPlus": false,
            "isBdJava": true,
            "isDbox": false,
            "isPsp": true,
            "playlists": [a_playlist_json()],
            "measured": true,
            "errors": [a_scan_error_json()],
        })
    }

    #[test]
    fn every_field_serializes_under_its_published_name() {
        let wire = serde_json::to_value(a_disc()).expect("serialize the mirror");
        assert_eq!(wire, a_disc_json());
    }

    #[test]
    fn every_field_survives_a_round_trip_through_the_wire_form() {
        let back: Disc = serde_json::from_value(a_disc_json()).expect("deserialize the mirror");
        assert_eq!(back, a_disc());
    }

    #[test]
    fn an_absent_disc_title_round_trips() {
        let disc = Disc { disc_title: None, ..a_disc() };
        let wire = serde_json::to_value(&disc).expect("serialize a disc with no title");
        assert_eq!(wire.get("discTitle"), Some(&Value::Null));
        let back: Disc = serde_json::from_value(wire).expect("deserialize a disc with no title");
        assert_eq!(back, disc);
    }

    #[test]
    fn every_scan_stage_serializes_under_the_label_the_scan_reports() {
        for (stage, label) in [
            (ScanStage::Discovery, "discovery"),
            (ScanStage::ClipInfo, "clipinfo"),
            (ScanStage::Playlist, "playlist"),
            (ScanStage::StreamFile, "stream"),
            (ScanStage::SectorRead, "sector"),
        ] {
            let wire = serde_json::to_value(stage).expect("serialize a scan stage");
            assert_eq!(wire, json!(label));
            let back: ScanStage = serde_json::from_value(wire).expect("deserialize a scan stage");
            assert_eq!(back, stage);
        }
    }

    #[test]
    fn every_failure_reason_serializes_as_a_discriminated_union_member() {
        for (reason, wire_form) in [
            (
                ScanErrorReason::UnknownFileType { magic: "MPLSXXXX".to_owned() },
                json!({ "kind": "unknownFileType", "magic": "MPLSXXXX" }),
            ),
            (ScanErrorReason::UnexpectedEof, json!({ "kind": "unexpectedEof" })),
            (ScanErrorReason::StructureNotFound, json!({ "kind": "structureNotFound" })),
            (
                ScanErrorReason::MissingClipFile { file: "00003.CLPI".to_owned() },
                json!({ "kind": "missingClipFile", "file": "00003.CLPI" }),
            ),
            (
                ScanErrorReason::Io { message: "denied".to_owned() },
                json!({ "kind": "io", "message": "denied" }),
            ),
            (
                ScanErrorReason::MetadataTooLarge {
                    file: "00000.MPLS".to_owned(),
                    limit_bytes: 67_108_864,
                },
                json!({
                    "kind": "metadataTooLarge",
                    "file": "00000.MPLS",
                    "limitBytes": 67_108_864,
                }),
            ),
            (
                ScanErrorReason::Other { message: "unmirrored".to_owned() },
                json!({ "kind": "other", "message": "unmirrored" }),
            ),
        ] {
            let wire = serde_json::to_value(&reason).expect("serialize a failure reason");
            assert_eq!(wire, wire_form);
            let back: ScanErrorReason =
                serde_json::from_value(wire).expect("deserialize a failure reason");
            assert_eq!(back, reason);
        }
    }

    // ── the forward mapping ─────────────────────────────────────────────────
    //
    // One core-model value per mirror fixture above, built to mirror it exactly.
    // Pairing them means the mapping is asserted against values written for a
    // different purpose: a field wired to the wrong source shows up as a
    // mismatch on the one field, named.

    /// The [`StreamSummary`] whose mirror is [`a_stream`].
    fn a_core_stream() -> StreamSummary {
        StreamSummary {
            pid: Pid::new(4113),
            stream_type: TsStreamType::AvcVideo,
            codec_short_name: "AVC".to_owned(),
            codec_name: "MPEG-4 AVC Video".to_owned(),
            codec_alt_name: "AVC",
            bitrate: 9,
            active_bitrate: 10,
            language_name: "English".to_owned(),
            language_code: "eng".to_owned(),
            description: "1080p / 23.976 fps".to_owned(),
            full_description: "1080p / 23.976 fps / 16:9".to_owned(),
            channel_description: "5.1".to_owned(),
            sample_rate: 48_000,
            bit_depth: 24,
            channel_count: 5,
            height: 1080,
            angle_index: 11,
            is_hidden: true,
            ssif_only: false,
        }
    }

    /// The [`ClipStreamTally`] whose mirror is [`a_clip_stream`].
    fn a_core_clip_stream() -> ClipStreamTally {
        ClipStreamTally {
            pid: Pid::new(4352),
            stream_type: TsStreamType::DtsHdMasterAudio,
            codec_short_name: "DTS-HD MA".to_owned(),
            payload_bytes: 21,
            packet_count: 22,
        }
    }

    /// The [`ClipSummary`] whose mirror is [`a_clip`].
    fn a_core_clip() -> ClipSummary {
        ClipSummary {
            name: "00001.M2TS".to_owned(),
            display_name: "00001.SSIF".to_owned(),
            file_size: 12,
            interleaved_file_size: 13,
            angle_index: 14,
            relative_time_in: 15.5,
            length: 16.25,
            payload_bytes: 17,
            packet_count: 18,
            packet_seconds: 19.5,
            file_seconds: 20.5,
            streams: vec![a_core_clip_stream()],
        }
    }

    /// The [`ChapterSummary`] whose mirror is [`a_chapter`].
    fn a_core_chapter() -> ChapterSummary {
        ChapterSummary {
            time_in: 23.5,
            length: 24.5,
            avg_rate: 25.5,
            max_1sec_rate: 26.5,
            max_1sec_time: 27.5,
            max_5sec_rate: 28.5,
            max_5sec_time: 29.5,
            max_10sec_rate: 30.5,
            max_10sec_time: 31.5,
            avg_frame_size: 32.5,
            max_frame_size: 33.5,
            max_frame_time: 34.5,
        }
    }

    /// The [`PlaylistSummary`] whose mirror is [`a_playlist`].
    fn a_core_playlist() -> PlaylistSummary {
        PlaylistSummary {
            name: "00000.MPLS".to_owned(),
            total_length: 3.5,
            file_size: 4,
            interleaved_file_size: 5,
            chapter_count: 6,
            stream_count: 7,
            angle_count: 8,
            has_loops: true,
            streams: vec![a_core_stream()],
            clips: vec![a_core_clip()],
            chapters: vec![a_core_chapter()],
        }
    }

    /// The recorded failure whose mirror is [`a_scan_error`].
    fn a_core_scan_error() -> error::ScanError {
        error::ScanError {
            file: "00002.CLPI".to_owned(),
            stage: error::ScanStage::ClipInfo,
            reason: error::BdError::UnknownFileType("HDMV9999".to_owned()),
        }
    }

    /// The [`BdRom`] whose mirror is [`a_disc`] — bar `measured` and `errors`,
    /// which the scan supplies rather than the disc.
    fn a_core_bdrom() -> BdRom {
        BdRom {
            volume_label: "MIRROR_DISC".to_owned(),
            disc_title: Some("Mirror Title".to_owned()),
            size: 1,
            interleaved_size: 2,
            is_3d: true,
            is_50hz: false,
            is_uhd: true,
            is_bd_plus: false,
            is_bd_java: true,
            is_dbox: false,
            is_psp: true,
            playlists: vec![a_core_playlist()],
        }
    }

    /// A playlist carrying what the presentation filter reads — its length and
    /// whether it loops — over the whole-fields fixture.
    fn filtered_playlist(name: &str, total_length: f64, has_loops: bool) -> PlaylistSummary {
        PlaylistSummary { name: name.to_owned(), total_length, has_loops, ..a_core_playlist() }
    }

    /// A disc holding one playlist per filter classification: 00000 (100 s) is
    /// listed by every filter, 00001 (5 s) is short, 00002 (200 s) loops.
    fn a_mixed_core_bdrom() -> BdRom {
        BdRom {
            playlists: vec![
                filtered_playlist("00000.MPLS", 100.0, false),
                filtered_playlist("00001.MPLS", 5.0, false),
                filtered_playlist("00002.MPLS", 200.0, true),
            ],
            ..a_core_bdrom()
        }
    }

    /// The mirrored playlists' names, in the order the mirror holds them.
    fn names(disc: &Disc) -> Vec<&str> {
        disc.playlists.iter().map(|playlist| playlist.name.as_str()).collect()
    }

    /// The fixture disc scanned through the in-memory tree, mirrored under the
    /// standard 20 s classification.
    fn a_fixture_disc(mode: ScanMode) -> Disc {
        let tree = crate::build_tree(&crate::tests::fixture_blob());
        let report = BdRom::open_resilient(&tree, mode).expect("the fixture disc opens");
        Disc::from_scan(
            &report.bdrom,
            &report.errors,
            mode == ScanMode::Full,
            &PlaylistFilter::default(),
        )
    }

    #[test]
    fn every_field_of_a_scanned_disc_maps_into_its_mirror() {
        assert_eq!(
            Disc::from_scan(
                &a_core_bdrom(),
                &[a_core_scan_error()],
                true,
                &PlaylistFilter::default()
            ),
            a_disc()
        );
    }

    #[test]
    fn the_scan_mode_alone_decides_the_measured_flag() {
        // Same disc, same values: only the flag differs, and it says whether a
        // zero below was measured or never looked at.
        let unmeasured = Disc::from_scan(&a_core_bdrom(), &[], false, &PlaylistFilter::default());
        assert!(!unmeasured.measured);
        assert!(unmeasured.errors.is_empty(), "a scan with no failures mirrors none");
        assert_eq!(Disc { measured: true, ..unmeasured }, Disc { errors: Vec::new(), ..a_disc() });
    }

    #[test]
    fn a_mirrored_disc_holds_every_playlist_whatever_the_classification_withholds() {
        // A short playlist and a looping one are both mirrored, under the
        // standard filter that withholds them and under one that keeps them —
        // the filter only ever decides what `hiddenBy` names.
        let bdrom = a_mixed_core_bdrom();
        for filter in [PlaylistFilter::default(), PlaylistFilter::everything()] {
            let disc = Disc::from_scan(&bdrom, &[a_core_scan_error()], true, &filter);
            assert_eq!(names(&disc), ["00000.MPLS", "00001.MPLS", "00002.MPLS"], "{filter:?}");
            assert_eq!(disc.errors, vec![a_scan_error()], "the failures are not filtered");
        }
    }

    #[test]
    fn every_playlist_carries_the_rules_that_withhold_it() {
        let bdrom = a_mixed_core_bdrom();
        let rules = |filter: &PlaylistFilter| {
            Disc::from_scan(&bdrom, &[], false, filter)
                .playlists
                .into_iter()
                .map(|playlist| playlist.hidden_by)
                .collect::<Vec<_>>()
        };
        // 00001 is 5 s and 00002 loops, so the standard 20 s threshold names
        // one rule for each and none for the 100 s feature.
        assert_eq!(
            rules(&PlaylistFilter::default()),
            [vec![], vec![HiddenRule::Short], vec![HiddenRule::Looping]]
        );
        // A threshold the short playlist meets exactly clears its rule — the
        // same boundary `PlaylistFilter::keeps` draws — and leaves the looping
        // rule, which does not read the threshold, alone.
        let lowered = PlaylistFilter { short_playlist_seconds: 5.0, ..PlaylistFilter::default() };
        assert_eq!(rules(&lowered), [vec![], vec![], vec![HiddenRule::Looping]]);
        // A playlist that is both short and looping names both, Short first.
        let both =
            BdRom { playlists: vec![filtered_playlist("00003.MPLS", 5.0, true)], ..a_core_bdrom() };
        let disc = Disc::from_scan(&both, &[], false, &PlaylistFilter::default());
        let playlist = disc.playlists.first().expect("the one playlist");
        assert_eq!(playlist.hidden_by, [HiddenRule::Short, HiddenRule::Looping]);
    }

    #[test]
    fn every_hidden_rule_crosses_under_the_name_the_scan_reports() {
        // The wire strings are the ones core prints in its hidden-playlist
        // lines, so a relabelling there fails here rather than reaching a
        // consumer as a silently different union member.
        for rule in [order::HiddenRule::Short, order::HiddenRule::Looping] {
            let wire = serde_json::to_value(HiddenRule::from(rule)).expect("serialize the rule");
            assert_eq!(wire, Value::String(rule.label().to_owned()), "{rule:?}");
        }
    }

    #[test]
    fn the_table_numbering_describes_the_whole_disc_in_file_name_order() {
        // 00002 (200 s, clip B) heads group 1; 00000 (100 s) and 00003 (50 s)
        // share clip A, so they are group 2 in that order; 00001 (5 s, clip C)
        // is group 3. The array itself stays in the file-name order the disc
        // records, which is why the table position has to be a field.
        let over = |name: &str, total_length: f64, clip: &str| PlaylistSummary {
            clips: vec![ClipSummary { name: clip.to_owned(), ..a_core_clip() }],
            ..filtered_playlist(name, total_length, false)
        };
        let bdrom = BdRom {
            playlists: vec![
                over("00000.MPLS", 100.0, "A.M2TS"),
                over("00001.MPLS", 5.0, "C.M2TS"),
                over("00002.MPLS", 200.0, "B.M2TS"),
                over("00003.MPLS", 50.0, "A.M2TS"),
            ],
            ..a_core_bdrom()
        };
        // The standard filter would withhold the 5 s playlist; the numbering
        // must not notice, since it describes the disc rather than a view.
        let numbering = |filter: &PlaylistFilter| {
            Disc::from_scan(&bdrom, &[], true, filter)
                .playlists
                .into_iter()
                .map(|playlist| (playlist.name, playlist.group, playlist.position))
                .collect::<Vec<_>>()
        };
        let want = vec![
            ("00000.MPLS".to_owned(), 2, 2),
            ("00001.MPLS".to_owned(), 3, 4),
            ("00002.MPLS".to_owned(), 1, 1),
            ("00003.MPLS".to_owned(), 2, 3),
        ];
        assert_eq!(numbering(&PlaylistFilter::default()), want);
        assert_eq!(numbering(&PlaylistFilter::everything()), want);
    }

    #[test]
    fn every_scan_stage_crosses_as_its_own_mirror_member() {
        for (stage, mirrored) in [
            (error::ScanStage::Discovery, ScanStage::Discovery),
            (error::ScanStage::ClipInfo, ScanStage::ClipInfo),
            (error::ScanStage::Playlist, ScanStage::Playlist),
            (error::ScanStage::StreamFile, ScanStage::StreamFile),
            (error::ScanStage::SectorRead, ScanStage::SectorRead),
        ] {
            assert_eq!(ScanStage::from(stage), mirrored, "{stage:?}");
        }
    }

    #[test]
    fn every_failure_kind_crosses_as_its_own_mirror_member() {
        for (reason, mirrored) in [
            (
                error::BdError::UnknownFileType("MPLSXXXX".to_owned()),
                ScanErrorReason::UnknownFileType { magic: "MPLSXXXX".to_owned() },
            ),
            (error::BdError::UnexpectedEof, ScanErrorReason::UnexpectedEof),
            (error::BdError::StructureNotFound, ScanErrorReason::StructureNotFound),
            (
                error::BdError::MissingClipFile("00003.CLPI".to_owned()),
                ScanErrorReason::MissingClipFile { file: "00003.CLPI".to_owned() },
            ),
            (
                error::BdError::Io(io::Error::other("denied")),
                ScanErrorReason::Io { message: "denied".to_owned() },
            ),
            (
                error::BdError::MetadataTooLarge {
                    file: "00000.MPLS".to_owned(),
                    limit: 67_108_864,
                },
                ScanErrorReason::MetadataTooLarge {
                    file: "00000.MPLS".to_owned(),
                    limit_bytes: 67_108_864,
                },
            ),
            // The mirror names no cancelled-scan kind: cancelling aborts the
            // whole open, so it is never recorded against a file. It crosses
            // as the catch-all, which is also where a kind added to the
            // scanner later would land.
            (
                error::BdError::ScanCancelled,
                ScanErrorReason::Other { message: "scan cancelled".to_owned() },
            ),
        ] {
            assert_eq!(ScanErrorReason::from(&reason), mirrored, "{reason}");
        }
    }

    #[test]
    fn a_measured_scan_of_the_fixture_disc_fills_the_mirror_in() {
        let disc = a_fixture_disc(ScanMode::Full);
        assert!(disc.measured);
        assert_eq!(disc.volume_label, "WASMDISC");
        assert_eq!(disc.size_bytes, 11_146_324);
        assert!(disc.errors.is_empty(), "the fixture is healthy media");

        let playlist = disc.playlists.first().expect("the fixture has one playlist");
        assert_eq!(names(&disc), ["00000.MPLS"]);
        assert_eq!(playlist.stream_count, 2);
        // The clip file on disk. The report prints the packet-derived size
        // (11,064,384 bytes) on its `Size:` line instead, which is why the two
        // numbers differ.
        assert_eq!(playlist.file_size_bytes, 11_145_216);

        let clip = playlist.clips.first().expect("the playlist has one clip");
        assert_eq!(clip.name, "00000.M2TS");
        assert!(clip.packet_count > 0, "the packet scan tallied the clip");
        assert_eq!(clip.streams.len(), 2, "both streams are tallied over the whole file");

        let video = playlist.streams.first().expect("the playlist has a video stream");
        assert_eq!(video.codec_name, "MPEG-4 AVC Video");
        assert!(video.bitrate_bps > 0, "the packet scan measured the video rate");

        let chapter = playlist.chapters.first().expect("the playlist has a chapter");
        assert!(chapter.max_1sec_rate_bps > 0.0, "the packet scan measured the chapter rates");
    }

    #[test]
    fn a_metadata_scan_of_the_fixture_disc_mirrors_the_structure_unmeasured() {
        let disc = a_fixture_disc(ScanMode::Metadata);
        assert!(!disc.measured);
        // The structure is all there — the same playlist, clip and streams…
        let measured = a_fixture_disc(ScanMode::Full);
        assert_eq!(names(&disc), names(&measured));
        let playlist = disc.playlists.first().expect("the fixture has one playlist");
        assert_eq!(playlist.clips.len(), 1);
        assert_eq!(playlist.stream_count, 2);
        // …and every measured value is the zero `measured: false` explains.
        assert_eq!(playlist.clips.iter().map(|clip| clip.packet_count).sum::<u64>(), 0);
        assert!(playlist.streams.iter().all(|stream| stream.bitrate_bps == 0));
        assert!(playlist.chapters.iter().all(|chapter| chapter.max_1sec_rate_bps == 0.0));
    }

    // ── the reverse mapping ─────────────────────────────────────────────────
    //
    // Asserted against the same paired fixtures, in the other direction: a
    // mirror rebuilds into the core value it was written to mirror. The two
    // failure types core uses carry no `PartialEq`, so those are asserted on
    // the text the report prints from them, which is all a report reads.

    #[test]
    fn every_field_of_a_mirror_rebuilds_into_the_disc_it_mirrors() {
        assert_eq!(a_disc().into_scan().0, a_core_bdrom());
    }

    #[test]
    fn a_rebuilt_failure_names_the_same_file_stage_and_reason() {
        let (_, errors) = a_disc().into_scan();
        assert_eq!(errors.len(), 1);
        let failure = errors.first().expect("the mirrored failure");
        assert_eq!(failure.file, "00002.CLPI");
        assert_eq!(failure.stage, error::ScanStage::ClipInfo);
        assert_eq!(failure.reason.to_string(), "unknown file type: HDMV9999");
    }

    #[test]
    fn every_scan_stage_survives_the_round_trip() {
        for stage in [
            error::ScanStage::Discovery,
            error::ScanStage::ClipInfo,
            error::ScanStage::Playlist,
            error::ScanStage::StreamFile,
            error::ScanStage::SectorRead,
        ] {
            assert_eq!(error::ScanStage::from(ScanStage::from(stage)), stage, "{stage:?}");
        }
    }

    #[test]
    fn every_failure_kind_round_trips_to_the_message_the_report_prints() {
        // The report prints a failure as its file and the text of its reason,
        // so carrying that text across and back unchanged is what losslessness
        // means here. The cancelled scan is the kind the mirror has no member
        // for: it crosses as the catch-all and comes back as itself.
        for reason in [
            error::BdError::UnknownFileType("MPLSXXXX".to_owned()),
            error::BdError::UnexpectedEof,
            error::BdError::StructureNotFound,
            error::BdError::MissingClipFile("00003.CLPI".to_owned()),
            error::BdError::Io(io::Error::other("denied")),
            error::BdError::MetadataTooLarge { file: "00000.MPLS".to_owned(), limit: 67_108_864 },
            error::BdError::ScanCancelled,
        ] {
            let printed = reason.to_string();
            let rebuilt = error::BdError::from(ScanErrorReason::from(&reason));
            assert_eq!(rebuilt.to_string(), printed, "{reason}");
        }
    }

    #[test]
    fn every_alternate_codec_name_a_scan_hands_out_is_matched_back() {
        // Every stream type a disc can record, each with and without the codec
        // extensions that upgrade three of the labels — the whole set
        // `TsStream::codec_alt_name` returns. The stream is built as audio
        // because only audio carries extensions; the label is keyed by the
        // stream type either way. A label `bdinfo_rs_core` adds later fails
        // here, rather than silently rebuilding as the empty cell.
        for byte in 0..=u8::MAX {
            for has_extensions in [false, true] {
                // The stream type is assigned rather than named in the literal:
                // `TsStreamBase` keeps one private field, so it cannot be built
                // by struct literal from outside `bdinfo_rs_core`.
                let mut audio = TsAudioStream { has_extensions, ..TsAudioStream::default() };
                audio.base.stream_type = TsStreamType::from_u8(byte);
                let name = TsStream::Audio(audio).codec_alt_name();
                assert_eq!(borrowed_codec_alt_name(name), name, "byte {byte:#04X}");
            }
        }
        // A name no scan hands out — only a hand-edited mirror can carry one —
        // rebuilds as the empty label rather than as something plausible.
        assert_eq!(borrowed_codec_alt_name("Nonesuch"), "");
    }

    // ── the round trip ──────────────────────────────────────────────────────
    //
    // The mirror claims to carry every fact the report prints. These prove it:
    // the fixture disc is scanned, mirrored, rebuilt from the mirror alone, and
    // rendered — and the bytes must be the pinned report. A field the mirror
    // stops carrying shows up here as a byte mismatch on the day it lands.

    /// The pinned report for the fixture disc — the same bytes the whole-disc
    /// scan renders, kept verbatim by the `-text` `.gitattributes` rule.
    const GOLDEN: &[u8] = include_bytes!("../tests/golden_report.txt");

    /// The whole line byte `offset` falls on, as lossy text — the context for a
    /// byte-level difference.
    fn line_at(bytes: &[u8], offset: usize) -> String {
        let offset = offset.min(bytes.len());
        let start = bytes
            .iter()
            .take(offset)
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index.saturating_add(1));
        let end = bytes
            .iter()
            .skip(offset)
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| offset.saturating_add(index));
        String::from_utf8_lossy(bytes.get(start..end).unwrap_or_default()).into_owned()
    }

    /// Where `actual` first differs from `expected` — the byte offset, the line
    /// that byte falls on, and the whole of that line from each side. The empty
    /// string when the two are identical.
    ///
    /// A report is scores of lines of fixed-width columns; told only that two
    /// walls of text differ, the reader has to diff them by eye. Told the
    /// offset and the one line from each side, they can see which value went
    /// missing.
    fn difference(actual: &[u8], expected: &[u8]) -> String {
        if actual == expected {
            return String::new();
        }
        // Equal as far as both run means the difference is the length: the
        // first byte only one of them has.
        let offset = actual
            .iter()
            .zip(expected)
            .position(|(left, right)| left != right)
            .unwrap_or_else(|| actual.len().min(expected.len()));
        let line =
            expected.iter().take(offset).filter(|byte| **byte == b'\n').count().saturating_add(1);
        format!(
            "first difference at byte {offset}, line {line}\n  actual:   {:?}\n  expected: {:?}",
            line_at(actual, offset),
            line_at(expected, offset)
        )
    }

    /// The lines of `whole` that `reduced` leaves out, or `None` when `reduced`
    /// is not a subsequence of it — that is, when a line was changed or added
    /// rather than only dropped. Every line `reduced` keeps is therefore
    /// byte-identical to the one it matched, and in the same order.
    fn dropped_lines<'a>(whole: &'a str, reduced: &str) -> Option<Vec<&'a str>> {
        let mut dropped = Vec::new();
        let mut kept = reduced.lines();
        let mut next = kept.next();
        for line in whole.lines() {
            if next == Some(line) {
                next = kept.next();
            } else {
                dropped.push(line);
            }
        }
        next.is_none().then_some(dropped)
    }

    #[test]
    fn difference_locates_the_first_differing_byte_and_its_line() {
        assert!(difference(b"same", b"same").is_empty());
        // A differing byte on the second line names that byte and both lines.
        let report = difference(b"one\ntwo\n", b"one\nTWO\n");
        assert!(report.starts_with("first difference at byte 4, line 2\n"), "{report}");
        assert!(report.contains("\"two\""), "{report}");
        assert!(report.contains("\"TWO\""), "{report}");
        // Identical as far as the shorter runs: the difference is the first
        // byte only the longer one has.
        assert!(difference(b"ab", b"abc").starts_with("first difference at byte 2, line 1\n"));
    }

    #[test]
    fn line_at_takes_the_whole_line_around_an_offset() {
        assert_eq!(line_at(b"only", 2), "only"); // no newline either side
        assert_eq!(line_at(b"one\ntwo\nthree", 5), "two"); // a newline both sides
        assert_eq!(line_at(b"one\ntwo", 7), "two"); // an offset at the very end
        assert_eq!(line_at(b"one", 99), "one"); // ...or past it
    }

    #[test]
    fn dropped_lines_reports_removals_and_rejects_a_changed_line() {
        assert_eq!(dropped_lines("a\nb\nc", "a\nc"), Some(vec!["b"]));
        assert_eq!(dropped_lines("a\nb", "a\nb"), Some(Vec::new()));
        // A line that changed, or one that was never there, is not a removal.
        assert_eq!(dropped_lines("a\nb", "a\nB"), None);
        assert_eq!(dropped_lines("a", "a\nb"), None);
    }

    /// The fixture disc scanned, mirrored, and rebuilt from that mirror alone —
    /// the two halves a report is rendered from.
    fn a_rebuilt_fixture_disc() -> (BdRom, Vec<error::ScanError>) {
        a_fixture_disc(ScanMode::Full).into_scan()
    }

    #[test]
    fn a_disc_rebuilt_from_its_mirror_renders_the_pinned_report() {
        let (bdrom, errors) = a_rebuilt_fixture_disc();
        let order = bdrom.presentation_order(&PlaylistFilter::default());
        let rendered = text::render_with(&bdrom, &order, &errors, RenderOptions::default());
        let mismatch = difference(rendered.as_bytes(), GOLDEN);
        assert!(
            mismatch.is_empty(),
            "the mirror lost something the report prints — a field it stopped \
             carrying, or one it rebuilds wrongly:\n{mismatch}"
        );
    }

    #[test]
    fn switching_off_a_report_section_removes_that_section_and_nothing_else() {
        let (bdrom, errors) = a_rebuilt_fixture_disc();
        let order = bdrom.presentation_order(&PlaylistFilter::default());
        let render = |options| text::render_with(&bdrom, &order, &errors, options);

        let whole = render(RenderOptions::default());
        for (options, dropped_section, kept_section) in [
            (
                RenderOptions { stream_diagnostics: false, quick_summary: true },
                "STREAM DIAGNOSTICS:",
                "QUICK SUMMARY:",
            ),
            (
                RenderOptions { stream_diagnostics: true, quick_summary: false },
                "QUICK SUMMARY:",
                "STREAM DIAGNOSTICS:",
            ),
        ] {
            let reduced = render(options);
            // A subsequence at all means every surviving line is byte-identical
            // and in order: nothing was rewritten, only removed.
            let dropped = dropped_lines(&whole, &reduced)
                .expect("switching a section off must only remove lines");
            assert!(
                dropped.iter().any(|line| line.contains(dropped_section)),
                "{dropped_section} must be among the removed lines"
            );
            assert!(!reduced.contains(dropped_section), "{dropped_section} must be gone");
            assert!(reduced.contains(kept_section), "{kept_section} must survive");
        }

        // Both off removes both sections and still nothing else.
        let neither = render(RenderOptions { stream_diagnostics: false, quick_summary: false });
        assert!(dropped_lines(&whole, &neither).is_some(), "both off must only remove lines");
        assert!(!neither.contains("STREAM DIAGNOSTICS:"));
        assert!(!neither.contains("QUICK SUMMARY:"));
    }
}
