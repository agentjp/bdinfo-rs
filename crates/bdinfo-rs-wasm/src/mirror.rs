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
//! ## Building one
//!
//! [`Disc::from_scan`] mirrors a whole scanned disc and [`Disc::from_listing`] mirrors one as a
//! filtered playlist listing; every other type here is reached through a [`From`] impl those two
//! walk.

use bdinfo_rs_core::bdrom::chapters::ChapterSummary;
use bdinfo_rs_core::bdrom::disc::{
    BdRom, ClipStreamTally, ClipSummary, PlaylistSummary, StreamSummary,
};
use bdinfo_rs_core::bdrom::order::PlaylistFilter;
use bdinfo_rs_core::error;
use serde::{Deserialize, Serialize};
use tsify::Tsify;

/// A scanned Blu-ray disc: the disc-level properties and every playlist on it.
// `into_wasm_abi` is what lets an export return a `Disc` by value: it implements
// wasm-bindgen's `IntoWasmAbi` over `serde_wasm_bindgen`, so the value crosses as a real
// JavaScript object. Only the type an export names needs it; the types below are reached
// through this one.
#[derive(Tsify, Serialize, Deserialize, Debug, Clone, PartialEq)]
#[tsify(into_wasm_abi)]
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
    /// The playlists, sorted by file name.
    ///
    /// A scan puts every playlist on the disc here, the short and looping ones
    /// a selection table would withhold included. A listing narrows it to the
    /// playlists its filter keeps, which is what the two options on the
    /// structural calls select; the disc-level values above and `errors` below
    /// always describe the whole disc.
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
    pub angle_count: usize,
    /// Whether the playlist loops: two main-angle clips replay the same clip
    /// file from the same in-time. A selection table withholds a looping
    /// playlist by default.
    pub has_loops: bool,
    /// The presented streams, sorted by PID.
    pub streams: Vec<Stream>,
    /// The sequenced clips in playlist order, angle clips included.
    pub clips: Vec<Clip>,
    /// One entry per chapter mark, carrying the measured video statistics of
    /// that chapter. Present without the packet scan too, but then only the
    /// times are filled in.
    pub chapters: Vec<Chapter>,
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
    /// open. This is the one member a rebuilt disc cannot render back exactly,
    /// so a kind that starts occurring in earnest wants a member of its own.
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
    #[must_use]
    pub fn from_scan(bdrom: &BdRom, errors: &[error::ScanError], measured: bool) -> Self {
        Self::from_scan_filtered(bdrom, errors, measured, &PlaylistFilter::everything())
    }

    /// Mirrors a metadata-only scan as a playlist listing: as
    /// [`from_scan`](Self::from_scan), except that `playlists` holds only the
    /// playlists `filter` keeps and `measured` is false.
    #[must_use]
    pub fn from_listing(
        bdrom: &BdRom,
        errors: &[error::ScanError],
        filter: &PlaylistFilter,
    ) -> Self {
        Self::from_scan_filtered(bdrom, errors, false, filter)
    }

    /// The body both constructors share; `from_scan` passes the filter that
    /// keeps every playlist.
    fn from_scan_filtered(
        bdrom: &BdRom,
        errors: &[error::ScanError],
        measured: bool,
        filter: &PlaylistFilter,
    ) -> Self {
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
                .filter(|playlist| filter.keeps(playlist))
                .map(Playlist::from)
                .collect(),
            measured,
            errors: errors.iter().map(ScanError::from).collect(),
        }
    }
}

impl From<&PlaylistSummary> for Playlist {
    fn from(playlist: &PlaylistSummary) -> Self {
        Self {
            name: playlist.name.clone(),
            total_length_seconds: playlist.total_length,
            file_size_bytes: playlist.file_size,
            interleaved_file_size_bytes: playlist.interleaved_file_size,
            chapter_count: playlist.chapter_count,
            stream_count: playlist.stream_count,
            angle_count: playlist.angle_count,
            has_loops: playlist.has_loops,
            streams: playlist.streams.iter().map(Stream::from).collect(),
            clips: playlist.clips.iter().map(Clip::from).collect(),
            chapters: playlist.chapters.iter().map(Chapter::from).collect(),
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
    use bdinfo_rs_core::stream::TsStreamType;
    use serde_json::{Value, json};

    use super::{
        Chapter, Clip, ClipStream, Disc, Playlist, ScanError, ScanErrorReason, ScanStage, Stream,
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

    /// A playlist with every field set to a distinct value.
    fn a_playlist() -> Playlist {
        Playlist {
            name: "00000.MPLS".to_owned(),
            total_length_seconds: 3.5,
            file_size_bytes: 4,
            interleaved_file_size_bytes: 5,
            chapter_count: 6,
            stream_count: 7,
            angle_count: 8,
            has_loops: true,
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
            "fileSizeBytes": 4,
            "interleavedFileSizeBytes": 5,
            "chapterCount": 6,
            "streamCount": 7,
            "angleCount": 8,
            "hasLoops": true,
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

    /// The fixture disc scanned through the in-memory tree, mirrored.
    fn a_fixture_disc(mode: ScanMode) -> Disc {
        let tree = crate::build_tree(&crate::tests::fixture_blob());
        let report = BdRom::open_resilient(&tree, mode).expect("the fixture disc opens");
        Disc::from_scan(&report.bdrom, &report.errors, mode == ScanMode::Full)
    }

    #[test]
    fn every_field_of_a_scanned_disc_maps_into_its_mirror() {
        assert_eq!(Disc::from_scan(&a_core_bdrom(), &[a_core_scan_error()], true), a_disc());
    }

    #[test]
    fn the_scan_mode_alone_decides_the_measured_flag() {
        // Same disc, same values: only the flag differs, and it says whether a
        // zero below was measured or never looked at.
        let unmeasured = Disc::from_scan(&a_core_bdrom(), &[], false);
        assert!(!unmeasured.measured);
        assert!(unmeasured.errors.is_empty(), "a scan with no failures mirrors none");
        assert_eq!(Disc { measured: true, ..unmeasured }, Disc { errors: Vec::new(), ..a_disc() });
    }

    #[test]
    fn from_scan_keeps_the_playlists_a_listing_would_withhold() {
        let disc = Disc::from_scan(&a_mixed_core_bdrom(), &[], true);
        assert_eq!(names(&disc), ["00000.MPLS", "00001.MPLS", "00002.MPLS"]);
    }

    #[test]
    fn from_listing_holds_only_the_playlists_the_filter_keeps() {
        let bdrom = a_mixed_core_bdrom();
        let listing = |filter: &PlaylistFilter| {
            let disc = Disc::from_listing(&bdrom, &[a_core_scan_error()], filter);
            assert!(!disc.measured, "a listing scan never ran the packet scan");
            assert_eq!(disc.errors, vec![a_scan_error()], "the failures are not filtered");
            names(&disc).into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        // The standard filter withholds the short and the looping playlist.
        assert_eq!(listing(&PlaylistFilter::default()), ["00000.MPLS"]);
        // A threshold the short playlist meets exactly keeps it: the boundary
        // is the same one `PlaylistFilter::keeps` draws.
        let lowered = PlaylistFilter { short_playlist_seconds: 5.0, ..PlaylistFilter::default() };
        assert_eq!(listing(&lowered), ["00000.MPLS", "00001.MPLS"]);
        assert_eq!(
            listing(&PlaylistFilter::everything()),
            ["00000.MPLS", "00001.MPLS", "00002.MPLS"]
        );
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
}
