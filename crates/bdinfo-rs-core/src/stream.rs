//! Transport-stream data model — the typed shape every BDMV/M2TS parser fills.
//!
//! The model covers the enums, [`TsDescriptor`], and the
//! `TsStream`/`TsVideoStream`/`TsAudioStream`/`TsGraphicsStream`/`TsTextStream`
//! stream family.
//!
//! The enum discriminants are fixed **verbatim** (`#[repr(u8)]`, the on-disc
//! numeric values) because the parsers cast on-disc bytes straight into
//! them — e.g. `HevcVideo = 0x24`, `Ac3Audio = 0x81`. (`TsAspectRatio` and
//! `TsAudioMode` carry tiny values that fit `u8`, so they
//! are stored `#[repr(u8)]` too — the values are preserved exactly and never
//! emitted as a raw integer, so the width is non-observable.)
//!
//! The stream family shares one polymorphic core, modelled by
//! composition: the shared members live in [`TsStreamBase`], and each concrete
//! stream embeds one. Setting the language code has a side effect (recomputing
//! the language name via the ISO-639 table), so it goes through
//! [`TsStreamBase::set_language_code`] rather than a bare field write; the
//! video-format/frame-rate setters' derived fields are likewise methods.
//!
//! Per-codec pieces live with their codecs: the *video/graphics/text*
//! codec-derived display strings (the short/long codec names and the stream description)
//! are computed here from what the codec scanners fill in. The PGS composition
//! state is the
//! [`caption_ids`](TsGraphicsStream::caption_ids)/[`last_frame`](TsGraphicsStream::last_frame)
//! pair driven by [`crate::codec::pgs`]. The audio
//! [`ext_data`](TsAudioStream::ext_data) carries the AAC/MPA `codecname`.
//! The Dolby-audio slice of the codec strings
//! ([`TsAudioStream::codec_short_name`]/[`codec_name`]/[`description`]) works
//! with the recursive [`core_stream`](TsAudioStream::core_stream);
//! the reset-copy used when a stream is re-registered is
//! [`TsAudioStream::ts_clone`] (distinct from `#[derive(Clone)]`, a full
//! structural clone).
//!
//! [`codec_name`]: TsAudioStream::codec_name
//! [`description`]: TsAudioStream::description

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::language_codes;
use crate::primitives::Pid;

/// Elementary-stream type, keyed by the on-disc `stream_coding_type` byte.
///
/// Discriminants are the exact on-disc
/// stream-type codes the demuxer reads, never renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TsStreamType {
    /// Unrecognized / unset (`0x00`).
    #[default]
    Unknown = 0,
    /// MPEG-1 video.
    Mpeg1Video = 0x01,
    /// MPEG-2 video.
    Mpeg2Video = 0x02,
    /// MPEG-4 AVC (H.264) video.
    AvcVideo = 0x1B,
    /// MPEG-4 MVC (3D) video.
    MvcVideo = 0x20,
    /// MPEG-H HEVC (H.265) video.
    HevcVideo = 0x24,
    /// SMPTE VC-1 video.
    Vc1Video = 0xEA,
    /// MPEG-1 audio.
    Mpeg1Audio = 0x03,
    /// MPEG-2 audio.
    Mpeg2Audio = 0x04,
    /// MPEG-2 AAC audio.
    Mpeg2AacAudio = 0x0F,
    /// MPEG-4 AAC audio.
    Mpeg4AacAudio = 0x11,
    /// Linear PCM audio.
    LpcmAudio = 0x80,
    /// Dolby Digital (AC-3) audio.
    Ac3Audio = 0x81,
    /// Dolby Digital Plus (E-AC-3) audio.
    Ac3PlusAudio = 0x84,
    /// Dolby Digital Plus secondary audio.
    Ac3PlusSecondaryAudio = 0xA1,
    /// Dolby `TrueHD` audio.
    Ac3TrueHdAudio = 0x83,
    /// DTS audio.
    DtsAudio = 0x82,
    /// DTS-HD audio.
    DtsHdAudio = 0x85,
    /// DTS-HD secondary audio.
    DtsHdSecondaryAudio = 0xA2,
    /// DTS-HD Master Audio.
    DtsHdMasterAudio = 0x86,
    /// Presentation Graphics (PGS) subtitles.
    PresentationGraphics = 0x90,
    /// Interactive Graphics (menus).
    InteractiveGraphics = 0x91,
    /// Text subtitle (`TextST`).
    Subtitle = 0x92,
}

/// Video resolution + scan code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TsVideoFormat {
    /// Unknown / unset.
    #[default]
    Unknown = 0,
    /// 480-line interlaced.
    Videoformat480i = 1,
    /// 576-line interlaced.
    Videoformat576i = 2,
    /// 480-line progressive.
    Videoformat480p = 3,
    /// 1080-line interlaced.
    Videoformat1080i = 4,
    /// 720-line progressive.
    Videoformat720p = 5,
    /// 1080-line progressive.
    Videoformat1080p = 6,
    /// 576-line progressive.
    Videoformat576p = 7,
    /// 2160-line progressive (4K UHD).
    Videoformat2160p = 8,
}

/// Frame-rate code. Note 5 is an unused/reserved code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TsFrameRate {
    /// Unknown / unset.
    #[default]
    Unknown = 0,
    /// 23.976 fps (24000/1001).
    Framerate23_976 = 1,
    /// 24 fps.
    Framerate24 = 2,
    /// 25 fps.
    Framerate25 = 3,
    /// 29.97 fps (30000/1001).
    Framerate29_97 = 4,
    /// 50 fps.
    Framerate50 = 6,
    /// 59.94 fps (60000/1001).
    Framerate59_94 = 7,
}

/// Channel-layout code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TsChannelLayout {
    /// Unknown / unset.
    #[default]
    Unknown = 0,
    /// Mono.
    ChannellayoutMono = 1,
    /// Stereo.
    ChannellayoutStereo = 3,
    /// Multichannel (e.g. 5.1).
    ChannellayoutMulti = 6,
    /// Combined layout.
    ChannellayoutCombo = 12,
}

/// Sample-rate code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TsSampleRate {
    /// Unknown / unset.
    #[default]
    Unknown = 0,
    /// 48 kHz.
    Samplerate48 = 1,
    /// 96 kHz.
    Samplerate96 = 4,
    /// 192 kHz.
    Samplerate192 = 5,
    /// 48 kHz core / 192 kHz extension.
    Samplerate48_192 = 12,
    /// 48 kHz core / 96 kHz extension.
    Samplerate48_96 = 14,
}

/// Display aspect-ratio code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TsAspectRatio {
    /// Unknown / unset.
    #[default]
    Unknown = 0,
    /// 4:3.
    Aspect4_3 = 2,
    /// 16:9.
    Aspect16_9 = 3,
    /// 2.21:1.
    Aspect2_21 = 4,
}

/// Audio channel mode (values sequential).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TsAudioMode {
    /// Unknown / unset.
    #[default]
    Unknown = 0,
    /// Dual mono.
    DualMono = 1,
    /// Stereo.
    Stereo = 2,
    /// Dolby Surround.
    Surround = 3,
    /// Extended (AC3-EX / DTS-ES).
    Extended = 4,
    /// Joint stereo.
    JointStereo = 5,
    /// Mono.
    Mono = 6,
}

/// A raw stream descriptor: a one-byte tag and its payload.
///
/// `new(name, length)` allocates a `length`-byte zeroed payload that the
/// parser then fills in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsDescriptor {
    /// The descriptor tag.
    pub name: u8,
    /// The descriptor payload bytes.
    pub value: Vec<u8>,
}

impl TsDescriptor {
    /// Creates a descriptor with tag `name` and a zeroed payload of `length`
    /// bytes.
    #[must_use]
    pub fn new(name: u8, length: u8) -> Self {
        Self { name, value: vec![0; usize::from(length)] }
    }
}

/// Members shared by every stream kind.
///
/// The language code is private and paired with `language_name` through
/// [`set_language_code`](Self::set_language_code), which recomputes the name
/// via the ISO-639 table so the pair can never fall out of sync.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TsStreamBase {
    /// Packet identifier (`PID`).
    pub pid: Pid,
    /// Elementary-stream type.
    pub stream_type: TsStreamType,
    /// Raw descriptors, if any; `None` until the demux attaches them.
    pub descriptors: Option<Vec<TsDescriptor>>,
    /// Nominal bit rate, bits/s.
    pub bit_rate: i64,
    /// Active-region bit rate, bits/s.
    pub active_bit_rate: i64,
    /// Whether the stream is variable-bit-rate.
    pub is_vbr: bool,
    /// Whether codec analysis has completed.
    pub is_initialized: bool,
    /// Whether the stream is hidden.
    pub is_hidden: bool,
    /// Total payload bytes seen by the demuxer.
    pub payload_bytes: u64,
    /// Transport packets seen.
    pub packet_count: u64,
    /// Stream duration in seconds.
    pub packet_seconds: f64,
    /// Angle index for multi-angle titles.
    pub angle_index: i32,
    /// 3D base-view eye flag: `None`, or `Some(true)`/`Some(false)`.
    pub base_view: Option<bool>,
    /// Resolved language display name, set with the code.
    pub language_name: Option<String>,
    /// Backing field for [`set_language_code`](Self::set_language_code).
    language_code: Option<String>,
}

impl TsStreamBase {
    /// Sets the ISO-639 language code and recomputes `language_name` from it
    /// via the ISO-639 table, keeping the pair consistent.
    pub fn set_language_code(&mut self, code: &str) {
        self.language_name = Some(language_codes::get_name(code));
        self.language_code = Some(code.to_owned());
    }

    /// Returns the current language code, or `None` if it has not been set.
    #[must_use]
    pub fn language_code(&self) -> Option<&str> {
        self.language_code.as_deref()
    }

    /// Size of all packets in bytes (`packet_count * 192`).
    ///
    /// Uses `wrapping_mul` per the house number-math rule: fixed-width
    /// codec/counter arithmetic wraps rather than panics.
    #[must_use]
    pub const fn packet_size(&self) -> u64 {
        self.packet_count.wrapping_mul(192)
    }

    /// Returns the identity-only reset-copy of the shared members — the PID,
    /// stream type, VBR flag, bit rate, initialized flag, language code, and
    /// descriptors. Every demux counter
    /// (`active_bit_rate`/`payload_bytes`/`packet_count`/`packet_seconds`/…)
    /// starts at its default in the copy.
    #[must_use]
    pub fn reset_copy(&self) -> Self {
        let mut base = Self {
            pid: self.pid,
            stream_type: self.stream_type,
            is_vbr: self.is_vbr,
            bit_rate: self.bit_rate,
            is_initialized: self.is_initialized,
            descriptors: self.descriptors.clone(),
            ..Self::default()
        };
        // Assign through the setter so the language name is recomputed.
        if let Some(code) = self.language_code() {
            base.set_language_code(code);
        }
        base
    }

    /// Whether the stream type is a video type.
    #[must_use]
    pub const fn is_video_stream(&self) -> bool {
        self.stream_type.is_video()
    }

    /// Whether the stream type is an audio type.
    #[must_use]
    pub const fn is_audio_stream(&self) -> bool {
        self.stream_type.is_audio()
    }

    /// Whether the stream type is a graphics type.
    #[must_use]
    pub const fn is_graphics_stream(&self) -> bool {
        self.stream_type.is_graphics()
    }

    /// Whether the stream type is a text-subtitle type.
    #[must_use]
    pub const fn is_text_stream(&self) -> bool {
        self.stream_type.is_text()
    }
}

impl TsStreamType {
    /// Whether this type is a video type.
    #[must_use]
    pub const fn is_video(self) -> bool {
        matches!(
            self,
            Self::Mpeg1Video
                | Self::Mpeg2Video
                | Self::AvcVideo
                | Self::MvcVideo
                | Self::Vc1Video
                | Self::HevcVideo
        )
    }

    /// Whether this type is an audio type.
    #[must_use]
    pub const fn is_audio(self) -> bool {
        matches!(
            self,
            Self::Mpeg1Audio
                | Self::Mpeg2Audio
                | Self::Mpeg2AacAudio
                | Self::Mpeg4AacAudio
                | Self::LpcmAudio
                | Self::Ac3Audio
                | Self::Ac3PlusAudio
                | Self::Ac3PlusSecondaryAudio
                | Self::Ac3TrueHdAudio
                | Self::DtsAudio
                | Self::DtsHdAudio
                | Self::DtsHdSecondaryAudio
                | Self::DtsHdMasterAudio
        )
    }

    /// Whether this type is a graphics (PGS/IGS) type.
    #[must_use]
    pub const fn is_graphics(self) -> bool {
        matches!(self, Self::PresentationGraphics | Self::InteractiveGraphics)
    }

    /// Whether this type is a text-subtitle type.
    #[must_use]
    pub const fn is_text(self) -> bool {
        matches!(self, Self::Subtitle)
    }
}

/// A video elementary stream.
///
/// The video format and frame rate are private; setting either derives
/// dependent fields (`height`/`is_interlaced` and the frame-rate
/// enumerator/denominator), so writes go through the
/// [`set_video_format`](Self::set_video_format)
/// and [`set_frame_rate`](Self::set_frame_rate) methods.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TsVideoStream {
    /// Shared stream members.
    pub base: TsStreamBase,
    /// Pixel width.
    pub width: i32,
    /// Pixel height, derived from the video format.
    pub height: i32,
    /// Whether the scan is interlaced, derived from the video format.
    pub is_interlaced: bool,
    /// Frame-rate enumerator, derived from the frame rate.
    pub frame_rate_enumerator: i32,
    /// Frame-rate denominator, derived from the frame rate.
    pub frame_rate_denominator: i32,
    /// Display aspect ratio.
    pub aspect_ratio: TsAspectRatio,
    /// Codec encoding profile string, if known.
    pub encoding_profile: Option<String>,
    /// HEVC HDR analysis state, filled by [`crate::codec::hevc`];
    /// its extended-format info is appended to [`description`](Self::description)
    /// for an `HEVC_VIDEO` stream. `None` for every other codec.
    pub extended_data: Option<crate::codec::hevc::HevcExtendedData>,
    /// Backing field for [`set_video_format`](Self::set_video_format).
    video_format: TsVideoFormat,
    /// Backing field for [`set_frame_rate`](Self::set_frame_rate).
    frame_rate: TsFrameRate,
}

impl TsVideoStream {
    /// Returns the reset-copy: the identity fields
    /// ([`TsStreamBase::reset_copy`]) plus every video detail field, with the
    /// demux counters back at their defaults — the copy taken when a playlist
    /// presents this stream (each presentation accumulates its own counters).
    #[must_use]
    pub fn ts_clone(&self) -> Self {
        Self {
            base: self.base.reset_copy(),
            width: self.width,
            height: self.height,
            is_interlaced: self.is_interlaced,
            frame_rate_enumerator: self.frame_rate_enumerator,
            frame_rate_denominator: self.frame_rate_denominator,
            aspect_ratio: self.aspect_ratio,
            encoding_profile: self.encoding_profile.clone(),
            extended_data: self.extended_data.clone(),
            video_format: self.video_format,
            frame_rate: self.frame_rate,
        }
    }

    /// Sets the video format, deriving `height`/`is_interlaced` from it.
    /// `Unknown` leaves both unchanged.
    pub const fn set_video_format(&mut self, value: TsVideoFormat) {
        self.video_format = value;
        match value {
            TsVideoFormat::Videoformat480i => {
                self.height = 480;
                self.is_interlaced = true;
            }
            TsVideoFormat::Videoformat480p => {
                self.height = 480;
                self.is_interlaced = false;
            }
            TsVideoFormat::Videoformat576i => {
                self.height = 576;
                self.is_interlaced = true;
            }
            TsVideoFormat::Videoformat576p => {
                self.height = 576;
                self.is_interlaced = false;
            }
            TsVideoFormat::Videoformat720p => {
                self.height = 720;
                self.is_interlaced = false;
            }
            TsVideoFormat::Videoformat1080i => {
                self.height = 1080;
                self.is_interlaced = true;
            }
            TsVideoFormat::Videoformat1080p => {
                self.height = 1080;
                self.is_interlaced = false;
            }
            TsVideoFormat::Videoformat2160p => {
                self.height = 2160;
                self.is_interlaced = false;
            }
            TsVideoFormat::Unknown => {}
        }
    }

    /// Returns the current video format.
    #[must_use]
    pub const fn video_format(&self) -> TsVideoFormat {
        self.video_format
    }

    /// Sets the frame rate, deriving the enumerator/denominator from it.
    /// `Unknown` leaves both unchanged.
    pub const fn set_frame_rate(&mut self, value: TsFrameRate) {
        self.frame_rate = value;
        match value {
            TsFrameRate::Framerate23_976 => {
                self.frame_rate_enumerator = 24_000;
                self.frame_rate_denominator = 1001;
            }
            TsFrameRate::Framerate24 => {
                self.frame_rate_enumerator = 24_000;
                self.frame_rate_denominator = 1000;
            }
            TsFrameRate::Framerate25 => {
                self.frame_rate_enumerator = 25_000;
                self.frame_rate_denominator = 1000;
            }
            TsFrameRate::Framerate29_97 => {
                self.frame_rate_enumerator = 30_000;
                self.frame_rate_denominator = 1001;
            }
            TsFrameRate::Framerate50 => {
                self.frame_rate_enumerator = 50_000;
                self.frame_rate_denominator = 1000;
            }
            TsFrameRate::Framerate59_94 => {
                self.frame_rate_enumerator = 60_000;
                self.frame_rate_denominator = 1001;
            }
            TsFrameRate::Unknown => {}
        }
    }

    /// Returns the current frame rate.
    #[must_use]
    pub const fn frame_rate(&self) -> TsFrameRate {
        self.frame_rate
    }

    /// The short codec label shown as the report's short codec name.
    /// `UNKNOWN` for any non-video type (a
    /// [`TsVideoStream`] only ever carries a video stream type).
    #[must_use]
    pub const fn codec_short_name(&self) -> &'static str {
        match self.base.stream_type {
            TsStreamType::Mpeg1Video => "MPEG-1",
            TsStreamType::Mpeg2Video => "MPEG-2",
            TsStreamType::AvcVideo => "AVC",
            TsStreamType::MvcVideo => "MVC",
            TsStreamType::HevcVideo => "HEVC",
            TsStreamType::Vc1Video => "VC-1",
            _ => "UNKNOWN",
        }
    }

    /// The long codec label shown as the report's long codec name.
    /// `UNKNOWN` for any non-video type.
    #[must_use]
    pub const fn codec_name(&self) -> &'static str {
        match self.base.stream_type {
            TsStreamType::Mpeg1Video => "MPEG-1 Video",
            TsStreamType::Mpeg2Video => "MPEG-2 Video",
            TsStreamType::AvcVideo => "MPEG-4 AVC Video",
            TsStreamType::MvcVideo => "MPEG-4 MVC Video",
            TsStreamType::HevcVideo => "MPEG-H HEVC Video",
            TsStreamType::Vc1Video => "VC-1 Video",
            _ => "UNKNOWN",
        }
    }

    /// The full per-stream description shown as the report's stream description:
    /// an optional 3D base-view eye tag, the height
    /// with its scan-type suffix (`1080p`/`1080i`), the frame rate (integer
    /// `N fps` when it divides evenly, else a three-decimal value), the display
    /// aspect ratio
    /// (`4:3`/`16:9`), and the codec-derived encoding profile — each joined by
    /// ` / ` with the trailing separator trimmed.
    ///
    /// For an `HEVC_VIDEO` stream it then appends the HEVC HDR detail (ST 2086 /
    /// `MaxCLL` / HDR10+ / Dolby Vision) extended-format info, joined by ` / `,
    /// that [`crate::codec::hevc::scan`] filled.
    #[must_use]
    pub fn description(&self) -> String {
        self.compose_description(false)
    }

    /// [`description`](Self::description) with the HEVC ST 2086 luminance
    /// respelled to its exact four-decimal form — the report-table variant.
    #[must_use]
    pub fn full_description(&self) -> String {
        self.compose_description(true)
    }

    /// The shared description composer;
    /// `exact_luminance` selects the exact HEVC luminance spelling.
    fn compose_description(&self, exact_luminance: bool) -> String {
        let mut description = String::new();
        if let Some(base_view) = self.base.base_view {
            description.push_str(if base_view { "Right Eye" } else { "Left Eye" });
            description.push_str(" / ");
        }
        if self.height > 0 {
            let scan = if self.is_interlaced { "i" } else { "p" };
            let _ = write!(description, "{}{scan} / ", self.height);
        }
        let (enumr, denom) = (self.frame_rate_enumerator, self.frame_rate_denominator);
        if enumr > 0 && denom > 0 {
            // The integer-fps test (`enumr % denom == 0`); the
            // `denom > 0` guard makes `checked_*` always `Some`, so the float `_` arm
            // only ever runs for a genuinely non-integer rate (e.g. 24000/1001).
            if let (Some(quotient), Some(0)) = (enumr.checked_div(denom), enumr.checked_rem(denom))
            {
                let _ = write!(description, "{quotient} fps / ");
            } else {
                let fps = f64::from(enumr) / f64::from(denom);
                let _ = write!(description, "{fps:.3} fps / ");
            }
        }
        if self.aspect_ratio == TsAspectRatio::Aspect4_3 {
            description.push_str("4:3 / ");
        } else if self.aspect_ratio == TsAspectRatio::Aspect16_9 {
            description.push_str("16:9 / ");
        }
        if let Some(profile) = &self.encoding_profile {
            let _ = write!(description, "{profile} / ");
        }
        // The HEVC extended-data block (ST 2086 / MaxCLL / HDR10+ / Dolby Vision)
        // appends its format-info entries, joined by ` / `, for an HEVC stream.
        if self.base.stream_type == TsStreamType::HevcVideo
            && let Some(ext) = &self.extended_data
        {
            if exact_luminance {
                description.push_str(&ext.extended_format_info_exact().join(" / "));
            } else {
                description.push_str(&ext.extended_format_info().join(" / "));
            }
        }
        if description.ends_with(" / ") {
            description.truncate(description.len().saturating_sub(3));
        }
        description
    }
}

/// An audio elementary stream.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TsAudioStream {
    /// Shared stream members.
    pub base: TsStreamBase,
    /// Sample rate in Hz.
    pub sample_rate: i32,
    /// Channel count.
    pub channel_count: i32,
    /// Bit depth.
    pub bit_depth: i32,
    /// Low-frequency-effects channel count.
    pub lfe: i32,
    /// Dialogue-normalization value in dB.
    pub dial_norm: i32,
    /// Whether codec extensions (Atmos / DTS:X) are present.
    pub has_extensions: bool,
    /// Channel mode.
    pub audio_mode: TsAudioMode,
    /// Channel layout.
    pub channel_layout: TsChannelLayout,
    /// Codec-supplied descriptor string — for MPEG-1/2 audio and
    /// AAC this `"<id> <profile/layer>"` text *is* the [`codec_name`](Self::codec_name)
    /// (`codecname`) field; `None` until a codec scanner fills it.
    pub ext_data: Option<String>,
    /// The embedded backward-compatible core stream, when this stream
    /// wraps one — e.g. a Dolby `TrueHD` or DD+ stream carrying an AC3 core. Boxed
    /// for the self-reference; `None` until a codec scanner fills it.
    pub core_stream: Option<Box<Self>>,
}

impl TsAudioStream {
    /// Converts a [`TsSampleRate`] code to its rate in Hz;
    /// unknown codes yield `0`.
    #[must_use]
    pub const fn convert_sample_rate(sample_rate: TsSampleRate) -> i32 {
        match sample_rate {
            TsSampleRate::Samplerate48 => 48_000,
            TsSampleRate::Samplerate96 | TsSampleRate::Samplerate48_96 => 96_000,
            TsSampleRate::Samplerate192 | TsSampleRate::Samplerate48_192 => 192_000,
            TsSampleRate::Unknown => 0,
        }
    }

    /// Returns the reset-copy — the partial clone the
    /// AC3 codec takes for a DD+ dependent stream's core: the identity fields
    /// (PID, stream type, VBR flag, bit rate, initialized flag, language code,
    /// descriptors), then the audio fields and `ext_data`, then a recursive
    /// clone of `core_stream`.
    ///
    /// Distinct from `#[derive(Clone)]` (a full structural copy): the reset-copy
    /// deliberately does **not** carry `has_extensions` or the base demux counters
    /// (`active_bit_rate`/`payload_bytes`/`packet_count`/…), so they start at
    /// their defaults in the copy.
    #[must_use]
    pub fn ts_clone(&self) -> Self {
        Self {
            base: self.base.reset_copy(),
            sample_rate: self.sample_rate,
            channel_count: self.channel_count,
            bit_depth: self.bit_depth,
            lfe: self.lfe,
            dial_norm: self.dial_norm,
            // The reset-copy omits `has_extensions` — it starts false in the copy.
            has_extensions: false,
            audio_mode: self.audio_mode,
            channel_layout: self.channel_layout,
            ext_data: self.ext_data.clone(),
            core_stream: self.core_stream.as_ref().map(|c| Box::new(c.ts_clone())),
        }
    }

    /// The short codec label shown as the report's short codec name, covering the
    /// Dolby and DTS audio families plus the simple audio codecs.
    /// `UNKNOWN` for any unhandled type.
    #[must_use]
    pub fn codec_short_name(&self) -> &'static str {
        match self.base.stream_type {
            TsStreamType::Mpeg1Audio => "MP1",
            TsStreamType::Mpeg2Audio => "MP2",
            TsStreamType::Mpeg2AacAudio => "MPEG-2 AAC",
            TsStreamType::Mpeg4AacAudio => "MPEG-4 AAC",
            TsStreamType::LpcmAudio => "LPCM",
            TsStreamType::Ac3Audio => {
                if self.audio_mode == TsAudioMode::Extended {
                    "AC3-EX"
                } else {
                    "AC3"
                }
            }
            TsStreamType::Ac3PlusAudio | TsStreamType::Ac3PlusSecondaryAudio => "AC3+",
            TsStreamType::Ac3TrueHdAudio => {
                if self.has_extensions {
                    "Atmos"
                } else {
                    "TrueHD"
                }
            }
            TsStreamType::DtsAudio => {
                if self.audio_mode == TsAudioMode::Extended {
                    "DTS-ES"
                } else {
                    "DTS"
                }
            }
            TsStreamType::DtsHdAudio => {
                if self.has_extensions {
                    "DTS:X HR"
                } else {
                    "DTS-HD HR"
                }
            }
            TsStreamType::DtsHdSecondaryAudio => "DTS Express",
            TsStreamType::DtsHdMasterAudio => {
                if self.has_extensions {
                    "DTS:X MA"
                } else {
                    "DTS-HD MA"
                }
            }
            _ => "UNKNOWN",
        }
    }

    /// The long codec label shown as the report's long codec name, covering the
    /// Dolby and DTS audio families plus the simple audio codecs.
    /// `UNKNOWN` for any unhandled type.
    #[must_use]
    pub fn codec_name(&self) -> &str {
        match self.base.stream_type {
            // MPEG-1/2 audio and AAC report their codec-supplied `ext_data`
            // string (an unset value → empty).
            TsStreamType::Mpeg1Audio
            | TsStreamType::Mpeg2Audio
            | TsStreamType::Mpeg2AacAudio
            | TsStreamType::Mpeg4AacAudio => self.ext_data.as_deref().unwrap_or(""),
            TsStreamType::LpcmAudio => "LPCM Audio",
            TsStreamType::Ac3Audio => {
                if self.audio_mode == TsAudioMode::Extended {
                    "Dolby Digital EX Audio"
                } else {
                    "Dolby Digital Audio"
                }
            }
            TsStreamType::Ac3PlusAudio => {
                if self.has_extensions {
                    "Dolby Digital Plus/Atmos Audio"
                } else {
                    "Dolby Digital Plus Audio"
                }
            }
            TsStreamType::Ac3PlusSecondaryAudio => "Dolby Digital Plus Audio",
            TsStreamType::Ac3TrueHdAudio => {
                if self.has_extensions {
                    "Dolby TrueHD/Atmos Audio"
                } else {
                    "Dolby TrueHD Audio"
                }
            }
            TsStreamType::DtsAudio => {
                if self.audio_mode == TsAudioMode::Extended {
                    "DTS-ES Audio"
                } else {
                    "DTS Audio"
                }
            }
            TsStreamType::DtsHdAudio => {
                if self.has_extensions {
                    "DTS:X High-Res Audio"
                } else {
                    "DTS-HD High-Res Audio"
                }
            }
            TsStreamType::DtsHdSecondaryAudio => "DTS Express",
            TsStreamType::DtsHdMasterAudio => {
                if self.has_extensions {
                    "DTS:X Master Audio"
                } else {
                    "DTS-HD Master Audio"
                }
            }
            _ => "UNKNOWN",
        }
    }

    /// The channel layout string: `"C.L"` from
    /// the channel + LFE counts (or a channel-layout fallback when no count is
    /// known), with an `-EX`/`-ES` suffix for an `Extended` AC3/DTS mode.
    #[must_use]
    pub fn channel_description(&self) -> String {
        let mut description = String::new();
        if self.channel_count > 0 {
            let _ = write!(description, "{}.{}", self.channel_count, self.lfe);
        } else {
            match self.channel_layout {
                TsChannelLayout::ChannellayoutMono => description.push_str("1.0"),
                TsChannelLayout::ChannellayoutStereo => description.push_str("2.0"),
                TsChannelLayout::ChannellayoutMulti => description.push_str("5.1"),
                _ => {}
            }
        }
        if self.audio_mode == TsAudioMode::Extended {
            if self.base.stream_type == TsStreamType::Ac3Audio {
                description.push_str("-EX");
            }
            if matches!(
                self.base.stream_type,
                TsStreamType::DtsAudio | TsStreamType::DtsHdAudio | TsStreamType::DtsHdMasterAudio
            ) {
                description.push_str("-ES");
            }
        }
        description
    }

    /// The full per-stream description shown as the report's stream description:
    /// channel layout, then ` / N kHz`, ` / N kbps`
    /// (net of an embedded `TrueHD` core's rate), ` / N-bit`, a dial-norm tag, a 2.0
    /// mode tag, and finally the recursive `(Core: …)` embedded-stream block.
    #[must_use]
    pub fn description(&self) -> String {
        let mut description = self.channel_description();
        if self.sample_rate > 0 {
            let _ = write!(description, " / {} kHz", self.sample_rate.wrapping_div(1000));
        }
        if self.base.bit_rate > 0 {
            let core_bit_rate = if self.base.stream_type == TsStreamType::Ac3TrueHdAudio {
                self.core_stream.as_ref().map_or(0, |c| c.base.bit_rate)
            } else {
                0
            };
            let kbps = round_kbps(self.base.bit_rate.wrapping_sub(core_bit_rate));
            let _ = write!(description, " / {kbps:5} kbps");
        }
        if self.bit_depth > 0 {
            let _ = write!(description, " / {}-bit", self.bit_depth);
        }
        if self.dial_norm != 0 {
            let _ = write!(description, " / DN {}dB", self.dial_norm);
        }
        if self.channel_count == 2 {
            match self.audio_mode {
                TsAudioMode::DualMono => description.push_str(" / Dual Mono"),
                TsAudioMode::Surround => description.push_str(" / Dolby Surround"),
                TsAudioMode::JointStereo => description.push_str(" / Joint Stereo"),
                _ => {}
            }
        }
        // (No trailing-separator trim: every appended field ends in its unit,
        // never " / ", so a trim would be a permanently-dead branch.)
        if let Some(core) = &self.core_stream {
            let codec = match core.base.stream_type {
                TsStreamType::Ac3Audio => "AC3 Embedded",
                TsStreamType::DtsAudio => "DTS Core",
                TsStreamType::Ac3PlusAudio => "DD+ Embedded",
                _ => "",
            };
            let _ = write!(description, " ({codec}: {})", core.description());
        }
        description
    }
}

/// Rounds `delta_bits / 1000` to the nearest kbps, half-to-even, for
/// [`TsAudioStream::description`].
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::as_conversions,
    reason = "float round then deliberate u32 narrowing; audio kbps fits u32 exactly (int↔float; TryFrom inapplicable)"
)]
fn round_kbps(delta_bits: i64) -> u32 {
    (delta_bits as f64 / 1000.0).round_ties_even() as u32
}

/// A graphics (PGS/IGS) elementary stream.
///
/// A new graphics stream is variable-bit-rate (the rest default), captured by
/// the [`Default`] impl. The PGS composition state
/// ([`caption_ids`](Self::caption_ids) /
/// [`last_frame`](Self::last_frame)) is tracked across access units by
/// [`crate::codec::pgs::scan`].
#[derive(Debug, Clone, PartialEq)]
pub struct TsGraphicsStream {
    /// Shared stream members.
    pub base: TsStreamBase,
    /// Pixel width.
    pub width: i32,
    /// Pixel height.
    pub height: i32,
    /// Caption count.
    pub captions: i32,
    /// Forced-caption count.
    pub forced_captions: i32,
    /// The most recent composition object's frame, carried between
    /// PGS access units so an object-definition segment can attribute its caption.
    pub last_frame: crate::codec::pgs::Frame,
    /// Composition-number → first frame seen, so each composition is
    /// counted once. The value is never read back; a `BTreeMap`
    /// keeps the (output-irrelevant) iteration order deterministic.
    pub caption_ids: BTreeMap<i32, crate::codec::pgs::Frame>,
}

impl Default for TsGraphicsStream {
    fn default() -> Self {
        Self {
            base: TsStreamBase { is_vbr: true, ..TsStreamBase::default() },
            width: 0,
            height: 0,
            captions: 0,
            forced_captions: 0,
            last_frame: crate::codec::pgs::Frame::default(),
            caption_ids: BTreeMap::new(),
        }
    }
}

impl TsGraphicsStream {
    /// Returns the reset-copy: the identity fields
    /// ([`TsStreamBase::reset_copy`]) plus the caption tallies and resolution,
    /// with the demux counters back at their defaults.
    #[must_use]
    pub fn ts_clone(&self) -> Self {
        Self {
            base: self.base.reset_copy(),
            width: self.width,
            height: self.height,
            captions: self.captions,
            forced_captions: self.forced_captions,
            last_frame: self.last_frame,
            caption_ids: self.caption_ids.clone(),
        }
    }

    /// The short codec label shown as the report's short codec name
    /// (`PGS` / `IGS`). `UNKNOWN` for any non-graphics
    /// type (a [`TsGraphicsStream`] only ever carries a graphics stream type).
    #[must_use]
    pub const fn codec_short_name(&self) -> &'static str {
        match self.base.stream_type {
            TsStreamType::PresentationGraphics => "PGS",
            TsStreamType::InteractiveGraphics => "IGS",
            _ => "UNKNOWN",
        }
    }

    /// The long codec label shown as the report's long codec name
    /// (`Presentation Graphics` / `Interactive Graphics`).
    /// `UNKNOWN` for any non-graphics type.
    #[must_use]
    pub const fn codec_name(&self) -> &'static str {
        match self.base.stream_type {
            TsStreamType::PresentationGraphics => "Presentation Graphics",
            TsStreamType::InteractiveGraphics => "Interactive Graphics",
            _ => "UNKNOWN",
        }
    }

    /// The full per-stream description shown as the report's stream description:
    /// the `WxH` resolution (when either dimension is
    /// known) followed by the caption tally. A non-zero caption count appends
    /// ` / N Caption[s]`; a non-zero forced-caption count appends ` (N Forced
    /// Caption[s])` when regular captions were also seen, else ` / N Forced
    /// Caption[s]` — the `[s]` plural shown only for a count above one. Empty until
    /// [`crate::codec::pgs::scan`] fills the dimensions and caption counts.
    #[must_use]
    pub fn description(&self) -> String {
        let mut description = String::new();
        if self.width > 0 || self.height > 0 {
            let _ = write!(description, "{}x{}", self.width, self.height);
        }
        // No outer `captions > 0 || forced_captions > 0` wrapper: it would be a
        // redundant no-op around the two inner guards (with both counts at zero
        // nothing is appended either way), and its `> 0` checks would be
        // provably-equivalent mutants on the non-negative counts.
        if self.captions > 0 {
            let plural = if self.captions > 1 { "s" } else { "" };
            let _ = write!(description, " / {} Caption{plural}", self.captions);
        }
        if self.forced_captions > 0 {
            let plural = if self.forced_captions > 1 { "s" } else { "" };
            if self.captions > 0 {
                let _ = write!(description, " ({} Forced Caption{plural})", self.forced_captions);
            } else {
                let _ = write!(description, " / {} Forced Caption{plural}", self.forced_captions);
            }
        }
        description
    }
}

/// A text-subtitle elementary stream.
///
/// A new text stream is variable-bit-rate and already initialized (it gets no
/// codec scan), captured by the [`Default`] impl.
#[derive(Debug, Clone, PartialEq)]
pub struct TsTextStream {
    /// Shared stream members.
    pub base: TsStreamBase,
}

impl Default for TsTextStream {
    fn default() -> Self {
        Self {
            base: TsStreamBase { is_vbr: true, is_initialized: true, ..TsStreamBase::default() },
        }
    }
}

impl TsTextStream {
    /// Returns the reset-copy: the identity fields
    /// ([`TsStreamBase::reset_copy`]) with the demux counters back at their
    /// defaults (a text stream carries no further detail fields).
    #[must_use]
    pub fn ts_clone(&self) -> Self {
        Self { base: self.base.reset_copy() }
    }

    /// The short codec label shown as the report's short codec name
    /// (`SUB`). `UNKNOWN` for any non-text type (a
    /// [`TsTextStream`] only ever carries the `SUBTITLE` stream type).
    #[must_use]
    pub const fn codec_short_name(&self) -> &'static str {
        match self.base.stream_type {
            TsStreamType::Subtitle => "SUB",
            _ => "UNKNOWN",
        }
    }

    /// The long codec label shown as the report's long codec name
    /// (`Subtitle`). `UNKNOWN` for any non-text type.
    #[must_use]
    pub const fn codec_name(&self) -> &'static str {
        match self.base.stream_type {
            TsStreamType::Subtitle => "Subtitle",
            _ => "UNKNOWN",
        }
    }

    /// The per-stream description shown as the report's stream description. Always the
    /// empty string — a text subtitle never contributes a `desc`.
    #[must_use]
    pub const fn description(&self) -> String {
        String::new()
    }
}

/// A parsed elementary stream of any kind — a
/// tagged union over the four
/// concrete kinds the BDMV parsers build into their per-PID stream
/// maps.
#[derive(Debug, Clone, PartialEq)]
pub enum TsStream {
    /// A video elementary stream.
    Video(TsVideoStream),
    /// An audio elementary stream.
    Audio(TsAudioStream),
    /// A graphics (PGS/IGS) elementary stream.
    Graphics(TsGraphicsStream),
    /// A text-subtitle elementary stream.
    Text(TsTextStream),
}

impl TsStream {
    /// Borrows the [`TsStreamBase`] members shared by every concrete kind.
    #[must_use]
    pub const fn base(&self) -> &TsStreamBase {
        match self {
            Self::Video(s) => &s.base,
            Self::Audio(s) => &s.base,
            Self::Graphics(s) => &s.base,
            Self::Text(s) => &s.base,
        }
    }

    /// Mutably borrows the shared [`TsStreamBase`] members, so a parser can set
    /// the PID/stream type after constructing the concrete stream.
    pub const fn base_mut(&mut self) -> &mut TsStreamBase {
        match self {
            Self::Video(s) => &mut s.base,
            Self::Audio(s) => &mut s.base,
            Self::Graphics(s) => &mut s.base,
            Self::Text(s) => &mut s.base,
        }
    }

    /// The stream's packet identifier (`PID`).
    #[must_use]
    pub const fn pid(&self) -> Pid {
        self.base().pid
    }

    /// The stream's elementary-stream type.
    #[must_use]
    pub const fn stream_type(&self) -> TsStreamType {
        self.base().stream_type
    }

    /// The short codec label of the concrete kind (the per-kind
    /// `codec_short_name`).
    #[must_use]
    pub fn codec_short_name(&self) -> &'static str {
        match self {
            Self::Video(s) => s.codec_short_name(),
            Self::Audio(s) => s.codec_short_name(),
            Self::Graphics(s) => s.codec_short_name(),
            Self::Text(s) => s.codec_short_name(),
        }
    }

    /// The alternate codec label some report tables print (e.g. the summary
    /// table's `AVC` / `DD AC3` / `DTS-HD Master` cells) — keyed by the stream
    /// type, with the Dolby/DTS extension upgrades (`Dolby Atmos`,
    /// `DTS:X Hi-Res`, `DTS:X Master`) when the audio stream carries them.
    /// `UNKNOWN` for an unrecognized type.
    #[must_use]
    pub const fn codec_alt_name(&self) -> &'static str {
        let has_extensions = match self {
            Self::Audio(audio) => audio.has_extensions,
            _ => false,
        };
        match self.stream_type() {
            TsStreamType::Mpeg1Video => "MPEG-1",
            TsStreamType::Mpeg2Video => "MPEG-2",
            TsStreamType::AvcVideo => "AVC",
            TsStreamType::MvcVideo => "MVC",
            TsStreamType::HevcVideo => "HEVC",
            TsStreamType::Vc1Video => "VC-1",
            TsStreamType::Mpeg1Audio => "MP1",
            TsStreamType::Mpeg2Audio => "MP2",
            TsStreamType::Mpeg2AacAudio => "MPEG-2 AAC",
            TsStreamType::Mpeg4AacAudio => "MPEG-4 AAC",
            TsStreamType::LpcmAudio => "LPCM",
            TsStreamType::Ac3Audio => "DD AC3",
            TsStreamType::Ac3PlusAudio | TsStreamType::Ac3PlusSecondaryAudio => "DD AC3+",
            TsStreamType::Ac3TrueHdAudio => {
                if has_extensions {
                    "Dolby Atmos"
                } else {
                    "Dolby TrueHD"
                }
            }
            TsStreamType::DtsAudio => "DTS",
            TsStreamType::DtsHdAudio => {
                if has_extensions {
                    "DTS:X Hi-Res"
                } else {
                    "DTS-HD Hi-Res"
                }
            }
            TsStreamType::DtsHdSecondaryAudio => "DTS Express",
            TsStreamType::DtsHdMasterAudio => {
                if has_extensions {
                    "DTS:X Master"
                } else {
                    "DTS-HD Master"
                }
            }
            TsStreamType::PresentationGraphics => "PGS",
            TsStreamType::InteractiveGraphics => "IGS",
            TsStreamType::Subtitle => "SUB",
            TsStreamType::Unknown => "UNKNOWN",
        }
    }

    /// Returns the reset-copy of the concrete stream — the identity and detail
    /// fields with every demux counter back at its default
    /// (the per-kind `ts_clone`). Distinct from `#[derive(Clone)]`, the full
    /// structural copy: a playlist presenting a clip's stream takes the
    /// reset-copy so each presentation accumulates its own counters.
    #[must_use]
    pub fn ts_clone(&self) -> Self {
        match self {
            Self::Video(s) => Self::Video(s.ts_clone()),
            Self::Audio(s) => Self::Audio(s.ts_clone()),
            Self::Graphics(s) => Self::Graphics(s.ts_clone()),
            Self::Text(s) => Self::Text(s.ts_clone()),
        }
    }
}

// ── Raw on-disc byte → model enum conversions ──────────────────────────────
// The parsers take a raw byte (or nibble) straight off the disc; these
// `from_u8` helpers map the recognized codes
// to their variant and collapse every unrecognized code to `Unknown` — safe
// for the parse path, where an unknown `stream_coding_type` creates no stream
// and the other fields only ever carry defined codes
// on real discs (the report layer owns formatting of any value).

impl TsStreamType {
    /// Maps a raw `stream_coding_type` byte to its variant, or `Unknown` for any
    /// code this analyzer does not recognize.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x01 => Self::Mpeg1Video,
            0x02 => Self::Mpeg2Video,
            0x1B => Self::AvcVideo,
            0x20 => Self::MvcVideo,
            0x24 => Self::HevcVideo,
            0xEA => Self::Vc1Video,
            0x03 => Self::Mpeg1Audio,
            0x04 => Self::Mpeg2Audio,
            0x0F => Self::Mpeg2AacAudio,
            0x11 => Self::Mpeg4AacAudio,
            0x80 => Self::LpcmAudio,
            0x81 => Self::Ac3Audio,
            0x84 => Self::Ac3PlusAudio,
            0xA1 => Self::Ac3PlusSecondaryAudio,
            0x83 => Self::Ac3TrueHdAudio,
            0x82 => Self::DtsAudio,
            0x85 => Self::DtsHdAudio,
            0xA2 => Self::DtsHdSecondaryAudio,
            0x86 => Self::DtsHdMasterAudio,
            0x90 => Self::PresentationGraphics,
            0x91 => Self::InteractiveGraphics,
            0x92 => Self::Subtitle,
            _ => Self::Unknown,
        }
    }

    /// The on-disc `stream_coding_type` byte for this variant — the inverse of
    /// [`from_u8`](Self::from_u8) (`Unknown` yields `0x00`). The report's
    /// diagnostics table prints it as the `0xNN` type cell.
    #[must_use]
    pub const fn value(self) -> u8 {
        match self {
            Self::Unknown => 0x00,
            Self::Mpeg1Video => 0x01,
            Self::Mpeg2Video => 0x02,
            Self::AvcVideo => 0x1B,
            Self::MvcVideo => 0x20,
            Self::HevcVideo => 0x24,
            Self::Vc1Video => 0xEA,
            Self::Mpeg1Audio => 0x03,
            Self::Mpeg2Audio => 0x04,
            Self::Mpeg2AacAudio => 0x0F,
            Self::Mpeg4AacAudio => 0x11,
            Self::LpcmAudio => 0x80,
            Self::Ac3Audio => 0x81,
            Self::Ac3PlusAudio => 0x84,
            Self::Ac3PlusSecondaryAudio => 0xA1,
            Self::Ac3TrueHdAudio => 0x83,
            Self::DtsAudio => 0x82,
            Self::DtsHdAudio => 0x85,
            Self::DtsHdSecondaryAudio => 0xA2,
            Self::DtsHdMasterAudio => 0x86,
            Self::PresentationGraphics => 0x90,
            Self::InteractiveGraphics => 0x91,
            Self::Subtitle => 0x92,
        }
    }

    /// The stream type's display name — what `stream.NNNNN.type` prints
    /// (e.g. `AVC_VIDEO`, `DTS_HD_MASTER_AUDIO`,
    /// `PRESENTATION_GRAPHICS`). Used verbatim by the report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Mpeg1Video => "MPEG1_VIDEO",
            Self::Mpeg2Video => "MPEG2_VIDEO",
            Self::AvcVideo => "AVC_VIDEO",
            Self::MvcVideo => "MVC_VIDEO",
            Self::HevcVideo => "HEVC_VIDEO",
            Self::Vc1Video => "VC1_VIDEO",
            Self::Mpeg1Audio => "MPEG1_AUDIO",
            Self::Mpeg2Audio => "MPEG2_AUDIO",
            Self::Mpeg2AacAudio => "MPEG2_AAC_AUDIO",
            Self::Mpeg4AacAudio => "MPEG4_AAC_AUDIO",
            Self::LpcmAudio => "LPCM_AUDIO",
            Self::Ac3Audio => "AC3_AUDIO",
            Self::Ac3PlusAudio => "AC3_PLUS_AUDIO",
            Self::Ac3PlusSecondaryAudio => "AC3_PLUS_SECONDARY_AUDIO",
            Self::Ac3TrueHdAudio => "AC3_TRUE_HD_AUDIO",
            Self::DtsAudio => "DTS_AUDIO",
            Self::DtsHdAudio => "DTS_HD_AUDIO",
            Self::DtsHdSecondaryAudio => "DTS_HD_SECONDARY_AUDIO",
            Self::DtsHdMasterAudio => "DTS_HD_MASTER_AUDIO",
            Self::PresentationGraphics => "PRESENTATION_GRAPHICS",
            Self::InteractiveGraphics => "INTERACTIVE_GRAPHICS",
            Self::Subtitle => "SUBTITLE",
        }
    }
}

impl TsVideoFormat {
    /// Maps the 4-bit `video_format` field to its variant, or `Unknown`.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Videoformat480i,
            2 => Self::Videoformat576i,
            3 => Self::Videoformat480p,
            4 => Self::Videoformat1080i,
            5 => Self::Videoformat720p,
            6 => Self::Videoformat1080p,
            7 => Self::Videoformat576p,
            8 => Self::Videoformat2160p,
            _ => Self::Unknown,
        }
    }
}

impl TsFrameRate {
    /// Maps the 4-bit `frame_rate` field to its variant, or `Unknown` (code 5 is
    /// unused/reserved and so maps to `Unknown` as well).
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Framerate23_976,
            2 => Self::Framerate24,
            3 => Self::Framerate25,
            4 => Self::Framerate29_97,
            6 => Self::Framerate50,
            7 => Self::Framerate59_94,
            _ => Self::Unknown,
        }
    }
}

impl TsAspectRatio {
    /// Maps the 4-bit `aspect_ratio` field to its variant, or `Unknown`.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            2 => Self::Aspect4_3,
            3 => Self::Aspect16_9,
            4 => Self::Aspect2_21,
            _ => Self::Unknown,
        }
    }
}

impl TsChannelLayout {
    /// Maps the 4-bit `channel_layout` field to its variant, or `Unknown`.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::ChannellayoutMono,
            3 => Self::ChannellayoutStereo,
            6 => Self::ChannellayoutMulti,
            12 => Self::ChannellayoutCombo,
            _ => Self::Unknown,
        }
    }
}

impl TsSampleRate {
    /// Maps the 4-bit `sample_rate` field to its variant, or `Unknown`.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Samplerate48,
            4 => Self::Samplerate96,
            5 => Self::Samplerate192,
            12 => Self::Samplerate48_192,
            14 => Self::Samplerate48_96,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::as_conversions,
    reason = "tests assert enum discriminants with `Variant as u8`, the idiomatic repr check"
)]
mod tests {
    use proptest::prelude::{any, prop_assert, prop_assert_eq, proptest};

    use super::{
        Pid, TsAspectRatio, TsAudioMode, TsAudioStream, TsChannelLayout, TsDescriptor, TsFrameRate,
        TsGraphicsStream, TsSampleRate, TsStream, TsStreamBase, TsStreamType, TsTextStream,
        TsVideoFormat, TsVideoStream,
    };

    #[test]
    fn codec_short_name_delegates_to_every_concrete_kind() {
        let mut video = TsVideoStream::default();
        video.base.stream_type = TsStreamType::AvcVideo;
        assert_eq!(TsStream::Video(video).codec_short_name(), "AVC");
        let mut audio = TsAudioStream::default();
        audio.base.stream_type = TsStreamType::Ac3Audio;
        assert_eq!(TsStream::Audio(audio).codec_short_name(), "AC3");
        let mut graphics = TsGraphicsStream::default();
        graphics.base.stream_type = TsStreamType::PresentationGraphics;
        assert_eq!(TsStream::Graphics(graphics).codec_short_name(), "PGS");
        let mut text = TsTextStream::default();
        text.base.stream_type = TsStreamType::Subtitle;
        assert_eq!(TsStream::Text(text).codec_short_name(), "SUB");
    }

    #[test]
    fn stream_type_discriminants_match_the_disc_codes() {
        // The on-disc stream-type codes, pinned verbatim.
        assert_eq!(TsStreamType::Unknown as u8, 0x00);
        assert_eq!(TsStreamType::Mpeg1Video as u8, 0x01);
        assert_eq!(TsStreamType::Mpeg2Video as u8, 0x02);
        assert_eq!(TsStreamType::AvcVideo as u8, 0x1B);
        assert_eq!(TsStreamType::MvcVideo as u8, 0x20);
        assert_eq!(TsStreamType::HevcVideo as u8, 0x24);
        assert_eq!(TsStreamType::Vc1Video as u8, 0xEA);
        assert_eq!(TsStreamType::Mpeg1Audio as u8, 0x03);
        assert_eq!(TsStreamType::Mpeg2Audio as u8, 0x04);
        assert_eq!(TsStreamType::Mpeg2AacAudio as u8, 0x0F);
        assert_eq!(TsStreamType::Mpeg4AacAudio as u8, 0x11);
        assert_eq!(TsStreamType::LpcmAudio as u8, 0x80);
        assert_eq!(TsStreamType::Ac3Audio as u8, 0x81);
        assert_eq!(TsStreamType::Ac3PlusAudio as u8, 0x84);
        assert_eq!(TsStreamType::Ac3PlusSecondaryAudio as u8, 0xA1);
        assert_eq!(TsStreamType::Ac3TrueHdAudio as u8, 0x83);
        assert_eq!(TsStreamType::DtsAudio as u8, 0x82);
        assert_eq!(TsStreamType::DtsHdAudio as u8, 0x85);
        assert_eq!(TsStreamType::DtsHdSecondaryAudio as u8, 0xA2);
        assert_eq!(TsStreamType::DtsHdMasterAudio as u8, 0x86);
        assert_eq!(TsStreamType::PresentationGraphics as u8, 0x90);
        assert_eq!(TsStreamType::InteractiveGraphics as u8, 0x91);
        assert_eq!(TsStreamType::Subtitle as u8, 0x92);
    }

    #[test]
    fn other_enum_discriminants_are_pinned() {
        assert_eq!(TsVideoFormat::Unknown as u8, 0);
        assert_eq!(TsVideoFormat::Videoformat480i as u8, 1);
        assert_eq!(TsVideoFormat::Videoformat576i as u8, 2);
        assert_eq!(TsVideoFormat::Videoformat480p as u8, 3);
        assert_eq!(TsVideoFormat::Videoformat1080i as u8, 4);
        assert_eq!(TsVideoFormat::Videoformat720p as u8, 5);
        assert_eq!(TsVideoFormat::Videoformat1080p as u8, 6);
        assert_eq!(TsVideoFormat::Videoformat576p as u8, 7);
        assert_eq!(TsVideoFormat::Videoformat2160p as u8, 8);

        assert_eq!(TsFrameRate::Unknown as u8, 0);
        assert_eq!(TsFrameRate::Framerate23_976 as u8, 1);
        assert_eq!(TsFrameRate::Framerate24 as u8, 2);
        assert_eq!(TsFrameRate::Framerate25 as u8, 3);
        assert_eq!(TsFrameRate::Framerate29_97 as u8, 4);
        assert_eq!(TsFrameRate::Framerate50 as u8, 6);
        assert_eq!(TsFrameRate::Framerate59_94 as u8, 7);

        assert_eq!(TsChannelLayout::Unknown as u8, 0);
        assert_eq!(TsChannelLayout::ChannellayoutMono as u8, 1);
        assert_eq!(TsChannelLayout::ChannellayoutStereo as u8, 3);
        assert_eq!(TsChannelLayout::ChannellayoutMulti as u8, 6);
        assert_eq!(TsChannelLayout::ChannellayoutCombo as u8, 12);

        assert_eq!(TsSampleRate::Unknown as u8, 0);
        assert_eq!(TsSampleRate::Samplerate48 as u8, 1);
        assert_eq!(TsSampleRate::Samplerate96 as u8, 4);
        assert_eq!(TsSampleRate::Samplerate192 as u8, 5);
        assert_eq!(TsSampleRate::Samplerate48_192 as u8, 12);
        assert_eq!(TsSampleRate::Samplerate48_96 as u8, 14);

        assert_eq!(TsAspectRatio::Unknown as u8, 0);
        assert_eq!(TsAspectRatio::Aspect4_3 as u8, 2);
        assert_eq!(TsAspectRatio::Aspect16_9 as u8, 3);
        assert_eq!(TsAspectRatio::Aspect2_21 as u8, 4);

        assert_eq!(TsAudioMode::Unknown as u8, 0);
        assert_eq!(TsAudioMode::DualMono as u8, 1);
        assert_eq!(TsAudioMode::Stereo as u8, 2);
        assert_eq!(TsAudioMode::Surround as u8, 3);
        assert_eq!(TsAudioMode::Extended as u8, 4);
        assert_eq!(TsAudioMode::JointStereo as u8, 5);
        assert_eq!(TsAudioMode::Mono as u8, 6);
    }

    #[test]
    fn enum_defaults_are_unknown_and_debug_renders() {
        // Exercise the derived Default + Debug (the discovery.rs pattern).
        assert_eq!(TsStreamType::default(), TsStreamType::Unknown);
        assert_eq!(TsVideoFormat::default(), TsVideoFormat::Unknown);
        assert_eq!(TsFrameRate::default(), TsFrameRate::Unknown);
        assert_eq!(TsChannelLayout::default(), TsChannelLayout::Unknown);
        assert_eq!(TsSampleRate::default(), TsSampleRate::Unknown);
        assert_eq!(TsAspectRatio::default(), TsAspectRatio::Unknown);
        assert_eq!(TsAudioMode::default(), TsAudioMode::Unknown);
        assert_eq!(format!("{:?}", TsStreamType::HevcVideo), "HevcVideo");
        assert_ne!(TsStreamType::AvcVideo, TsStreamType::HevcVideo);
    }

    #[test]
    fn descriptor_new_allocates_zeroed_payload() {
        let d = TsDescriptor::new(0x05, 3);
        assert_eq!(d.name, 0x05);
        assert_eq!(d.value, vec![0, 0, 0]);
        assert_eq!(d.clone(), d); // Clone round-trips.
        assert_eq!(TsDescriptor::new(0x09, 0).value, Vec::<u8>::new());
    }

    #[test]
    fn set_language_code_resolves_name_and_round_trips() {
        let mut base = TsStreamBase::default();
        assert_eq!(base.language_code(), None);
        base.set_language_code("eng");
        assert_eq!(base.language_code(), Some("eng"));
        assert_eq!(base.language_name.as_deref(), Some("English"));
        // Unknown code falls through to itself, exactly like `get_name`.
        base.set_language_code("zzz");
        assert_eq!(base.language_code(), Some("zzz"));
        assert_eq!(base.language_name.as_deref(), Some("zzz"));
    }

    #[test]
    fn packet_size_is_packet_count_times_192() {
        let mut base = TsStreamBase::default();
        assert_eq!(base.packet_size(), 0);
        base.packet_count = 10;
        assert_eq!(base.packet_size(), 1920);
        base.packet_count = 2;
        assert_eq!(base.packet_size(), 384);
        // The multiply wraps rather than panics at the extreme.
        base.packet_count = u64::MAX;
        assert_eq!(base.packet_size(), u64::MAX.wrapping_mul(192));
    }

    /// Builds a base with a given stream type to drive the classifiers.
    fn base_of(stream_type: TsStreamType) -> TsStreamBase {
        TsStreamBase { stream_type, ..TsStreamBase::default() }
    }

    #[test]
    fn stream_classifiers_partition_the_types() {
        let video = [
            TsStreamType::Mpeg1Video,
            TsStreamType::Mpeg2Video,
            TsStreamType::AvcVideo,
            TsStreamType::MvcVideo,
            TsStreamType::Vc1Video,
            TsStreamType::HevcVideo,
        ];
        let audio = [
            TsStreamType::Mpeg1Audio,
            TsStreamType::Mpeg2Audio,
            TsStreamType::Mpeg2AacAudio,
            TsStreamType::Mpeg4AacAudio,
            TsStreamType::LpcmAudio,
            TsStreamType::Ac3Audio,
            TsStreamType::Ac3PlusAudio,
            TsStreamType::Ac3PlusSecondaryAudio,
            TsStreamType::Ac3TrueHdAudio,
            TsStreamType::DtsAudio,
            TsStreamType::DtsHdAudio,
            TsStreamType::DtsHdSecondaryAudio,
            TsStreamType::DtsHdMasterAudio,
        ];
        let graphics = [TsStreamType::PresentationGraphics, TsStreamType::InteractiveGraphics];
        let text = [TsStreamType::Subtitle];

        for &t in &video {
            let b = base_of(t);
            assert!(b.is_video_stream(), "{t:?} should be video");
            assert!(!b.is_audio_stream());
            assert!(!b.is_graphics_stream());
            assert!(!b.is_text_stream());
        }
        for &t in &audio {
            let b = base_of(t);
            assert!(b.is_audio_stream(), "{t:?} should be audio");
            assert!(!b.is_video_stream());
            assert!(!b.is_graphics_stream());
            assert!(!b.is_text_stream());
        }
        for &t in &graphics {
            let b = base_of(t);
            assert!(b.is_graphics_stream(), "{t:?} should be graphics");
            assert!(!b.is_video_stream());
            assert!(!b.is_audio_stream());
            assert!(!b.is_text_stream());
        }
        for &t in &text {
            let b = base_of(t);
            assert!(b.is_text_stream(), "{t:?} should be text");
            assert!(!b.is_video_stream());
            assert!(!b.is_audio_stream());
            assert!(!b.is_graphics_stream());
        }
        // Unknown is none of the four.
        let u = base_of(TsStreamType::Unknown);
        assert!(!u.is_video_stream());
        assert!(!u.is_audio_stream());
        assert!(!u.is_graphics_stream());
        assert!(!u.is_text_stream());
    }

    #[test]
    fn set_video_format_derives_height_and_scan() {
        // (format, expected height, expected interlaced) — every switch arm.
        let cases = [
            (TsVideoFormat::Videoformat480i, 480, true),
            (TsVideoFormat::Videoformat480p, 480, false),
            (TsVideoFormat::Videoformat576i, 576, true),
            (TsVideoFormat::Videoformat576p, 576, false),
            (TsVideoFormat::Videoformat720p, 720, false),
            (TsVideoFormat::Videoformat1080i, 1080, true),
            (TsVideoFormat::Videoformat1080p, 1080, false),
            (TsVideoFormat::Videoformat2160p, 2160, false),
        ];
        for (fmt, height, interlaced) in cases {
            let mut v = TsVideoStream::default();
            v.set_video_format(fmt);
            assert_eq!(v.video_format(), fmt);
            assert_eq!(v.height, height, "height for {fmt:?}");
            assert_eq!(v.is_interlaced, interlaced, "scan for {fmt:?}");
        }
        // Unknown leaves the derived fields at their current values.
        let mut v = TsVideoStream { height: 99, is_interlaced: true, ..Default::default() };
        v.set_video_format(TsVideoFormat::Unknown);
        assert_eq!(v.video_format(), TsVideoFormat::Unknown);
        assert_eq!(v.height, 99);
        assert!(v.is_interlaced);
    }

    #[test]
    fn set_frame_rate_derives_enumerator_and_denominator() {
        let cases = [
            (TsFrameRate::Framerate23_976, 24_000, 1001),
            (TsFrameRate::Framerate24, 24_000, 1000),
            (TsFrameRate::Framerate25, 25_000, 1000),
            (TsFrameRate::Framerate29_97, 30_000, 1001),
            (TsFrameRate::Framerate50, 50_000, 1000),
            (TsFrameRate::Framerate59_94, 60_000, 1001),
        ];
        for (rate, num, den) in cases {
            let mut v = TsVideoStream::default();
            v.set_frame_rate(rate);
            assert_eq!(v.frame_rate(), rate);
            assert_eq!(v.frame_rate_enumerator, num, "enumerator for {rate:?}");
            assert_eq!(v.frame_rate_denominator, den, "denominator for {rate:?}");
        }
        // Unknown leaves the derived fields untouched.
        let mut v = TsVideoStream {
            frame_rate_enumerator: 7,
            frame_rate_denominator: 11,
            ..Default::default()
        };
        v.set_frame_rate(TsFrameRate::Unknown);
        assert_eq!(v.frame_rate(), TsFrameRate::Unknown);
        assert_eq!(v.frame_rate_enumerator, 7);
        assert_eq!(v.frame_rate_denominator, 11);
    }

    /// A video stream of `stream_type` with everything else default.
    fn video(stream_type: TsStreamType) -> TsVideoStream {
        TsVideoStream {
            base: TsStreamBase { stream_type, ..TsStreamBase::default() },
            ..TsVideoStream::default()
        }
    }

    #[test]
    fn video_codec_short_and_long_names_cover_every_arm() {
        let cases = [
            (TsStreamType::Mpeg1Video, "MPEG-1", "MPEG-1 Video"),
            (TsStreamType::Mpeg2Video, "MPEG-2", "MPEG-2 Video"),
            (TsStreamType::AvcVideo, "AVC", "MPEG-4 AVC Video"),
            (TsStreamType::MvcVideo, "MVC", "MPEG-4 MVC Video"),
            (TsStreamType::HevcVideo, "HEVC", "MPEG-H HEVC Video"),
            (TsStreamType::Vc1Video, "VC-1", "VC-1 Video"),
        ];
        for (ty, short, long) in cases {
            let v = video(ty);
            assert_eq!(v.codec_short_name(), short, "codec for {ty:?}");
            assert_eq!(v.codec_name(), long, "codecname for {ty:?}");
        }
        // The non-video fallback arm (a video stream never carries this type).
        let v = video(TsStreamType::Unknown);
        assert_eq!(v.codec_short_name(), "UNKNOWN");
        assert_eq!(v.codec_name(), "UNKNOWN");
    }

    #[test]
    fn video_description_renders_a_full_avc_line() {
        // 1080p / 23.976 fps / 16:9 + a codec-set encoding profile — every
        // segment populated, joined by ` / `.
        let mut v = video(TsStreamType::AvcVideo);
        v.set_video_format(TsVideoFormat::Videoformat1080p);
        v.set_frame_rate(TsFrameRate::Framerate23_976);
        v.aspect_ratio = TsAspectRatio::Aspect16_9;
        v.encoding_profile = Some("High Profile 4.1".to_owned());
        assert_eq!(v.description(), "1080p / 23.976 fps / 16:9 / High Profile 4.1");
    }

    #[test]
    fn video_description_renders_an_mpeg2_line() {
        // 1080p / 24 fps / 16:9, no encoding profile (the integer fps path).
        let mut v = video(TsStreamType::Mpeg2Video);
        v.set_video_format(TsVideoFormat::Videoformat1080p);
        v.set_frame_rate(TsFrameRate::Framerate24);
        v.aspect_ratio = TsAspectRatio::Aspect16_9;
        assert_eq!(v.description(), "1080p / 24 fps / 16:9");
    }

    #[test]
    fn video_description_eye_aspect_and_scan_variants() {
        // 3D base view (Right/Left Eye), interlaced scan, 4:3 aspect, no frame rate.
        let mut right = video(TsStreamType::MvcVideo);
        right.base.base_view = Some(true);
        right.set_video_format(TsVideoFormat::Videoformat1080i);
        right.aspect_ratio = TsAspectRatio::Aspect4_3;
        assert_eq!(right.description(), "Right Eye / 1080i / 4:3");

        let mut left = video(TsStreamType::MvcVideo);
        left.base.base_view = Some(false);
        assert_eq!(left.description(), "Left Eye");

        // ASPECT_2_21 and Unknown contribute nothing (neither aspect branch fires).
        let mut other = video(TsStreamType::Vc1Video);
        other.set_video_format(TsVideoFormat::Videoformat720p);
        other.aspect_ratio = TsAspectRatio::Aspect2_21;
        assert_eq!(other.description(), "720p");

        // Everything empty → an empty description (the trailing-trim is not taken).
        assert_eq!(video(TsStreamType::AvcVideo).description(), "");
    }

    #[test]
    fn video_description_frame_rate_fractional_and_denominator_guard() {
        // 29.97 (30000/1001) takes the three-decimal path; a zero denominator
        // skips the block.
        let mut v = video(TsStreamType::AvcVideo);
        v.set_frame_rate(TsFrameRate::Framerate29_97);
        assert_eq!(v.description(), "29.970 fps");

        let mut z = video(TsStreamType::AvcVideo);
        z.frame_rate_enumerator = 24_000;
        z.frame_rate_denominator = 0;
        assert_eq!(z.description(), "");

        // A zero enumerator with a positive denominator also skips the block (the
        // `enumr > 0` guard; a mutated `>= 0` would emit "0 fps").
        let mut e = video(TsStreamType::AvcVideo);
        e.frame_rate_enumerator = 0;
        e.frame_rate_denominator = 1000;
        assert_eq!(e.description(), "");

        // A standalone encoding profile with no resolution/rate (still trimmed).
        let mut p = video(TsStreamType::Vc1Video);
        p.encoding_profile = Some("Main Profile 2".to_owned());
        assert_eq!(p.description(), "Main Profile 2");
    }

    /// Builds an audio stream of `stream_type` with the given codec-derived fields,
    /// leaving everything else at its default.
    fn audio(
        stream_type: TsStreamType,
        channel_count: i32,
        lfe: i32,
        sample_rate: i32,
        bit_rate: i64,
    ) -> TsAudioStream {
        TsAudioStream {
            base: TsStreamBase { stream_type, bit_rate, ..TsStreamBase::default() },
            channel_count,
            lfe,
            sample_rate,
            ..TsAudioStream::default()
        }
    }

    #[test]
    fn codec_short_name_covers_the_dolby_and_dts_arms() {
        let mut ac3 = audio(TsStreamType::Ac3Audio, 5, 1, 48_000, 640_000);
        assert_eq!(ac3.codec_short_name(), "AC3");
        ac3.audio_mode = TsAudioMode::Extended;
        assert_eq!(ac3.codec_short_name(), "AC3-EX");
        assert_eq!(audio(TsStreamType::Ac3PlusAudio, 6, 0, 48_000, 0).codec_short_name(), "AC3+");
        assert_eq!(
            audio(TsStreamType::Ac3PlusSecondaryAudio, 2, 0, 48_000, 0).codec_short_name(),
            "AC3+"
        );
        let mut thd = audio(TsStreamType::Ac3TrueHdAudio, 7, 1, 48_000, 0);
        assert_eq!(thd.codec_short_name(), "TrueHD");
        thd.has_extensions = true;
        assert_eq!(thd.codec_short_name(), "Atmos");
        // DTS core: plain vs DTS-ES (Extended).
        let mut dts = audio(TsStreamType::DtsAudio, 6, 1, 48_000, 0);
        assert_eq!(dts.codec_short_name(), "DTS");
        dts.audio_mode = TsAudioMode::Extended;
        assert_eq!(dts.codec_short_name(), "DTS-ES");
        // DTS-HD High-Res vs DTS:X High-Res.
        let mut hr = audio(TsStreamType::DtsHdAudio, 6, 1, 48_000, 0);
        assert_eq!(hr.codec_short_name(), "DTS-HD HR");
        hr.has_extensions = true;
        assert_eq!(hr.codec_short_name(), "DTS:X HR");
        // DTS Express (secondary) is fixed.
        assert_eq!(
            audio(TsStreamType::DtsHdSecondaryAudio, 2, 0, 48_000, 0).codec_short_name(),
            "DTS Express"
        );
        // DTS-HD Master vs DTS:X Master.
        let mut ma = audio(TsStreamType::DtsHdMasterAudio, 6, 1, 48_000, 0);
        assert_eq!(ma.codec_short_name(), "DTS-HD MA");
        ma.has_extensions = true;
        assert_eq!(ma.codec_short_name(), "DTS:X MA");
        // The simple-audio arms: LPCM, MPEG-1/2 audio, MPEG-2/4 AAC.
        assert_eq!(audio(TsStreamType::LpcmAudio, 6, 1, 48_000, 0).codec_short_name(), "LPCM");
        assert_eq!(audio(TsStreamType::Mpeg1Audio, 2, 0, 48_000, 0).codec_short_name(), "MP1");
        assert_eq!(audio(TsStreamType::Mpeg2Audio, 2, 0, 48_000, 0).codec_short_name(), "MP2");
        assert_eq!(
            audio(TsStreamType::Mpeg2AacAudio, 2, 0, 48_000, 0).codec_short_name(),
            "MPEG-2 AAC"
        );
        assert_eq!(
            audio(TsStreamType::Mpeg4AacAudio, 2, 0, 48_000, 0).codec_short_name(),
            "MPEG-4 AAC"
        );
        // An unhandled type falls through to UNKNOWN.
        assert_eq!(audio(TsStreamType::Unknown, 6, 1, 48_000, 0).codec_short_name(), "UNKNOWN");
    }

    #[test]
    fn codec_name_covers_the_dolby_and_dts_arms() {
        let mut ac3 = audio(TsStreamType::Ac3Audio, 5, 1, 48_000, 640_000);
        assert_eq!(ac3.codec_name(), "Dolby Digital Audio");
        ac3.audio_mode = TsAudioMode::Extended;
        assert_eq!(ac3.codec_name(), "Dolby Digital EX Audio");
        let mut plus = audio(TsStreamType::Ac3PlusAudio, 6, 0, 48_000, 0);
        assert_eq!(plus.codec_name(), "Dolby Digital Plus Audio");
        plus.has_extensions = true;
        assert_eq!(plus.codec_name(), "Dolby Digital Plus/Atmos Audio");
        assert_eq!(
            audio(TsStreamType::Ac3PlusSecondaryAudio, 2, 0, 48_000, 0).codec_name(),
            "Dolby Digital Plus Audio"
        );
        let mut thd = audio(TsStreamType::Ac3TrueHdAudio, 7, 1, 48_000, 0);
        assert_eq!(thd.codec_name(), "Dolby TrueHD Audio");
        thd.has_extensions = true;
        assert_eq!(thd.codec_name(), "Dolby TrueHD/Atmos Audio");
        // DTS core: plain vs DTS-ES (Extended).
        let mut dts = audio(TsStreamType::DtsAudio, 6, 1, 48_000, 0);
        assert_eq!(dts.codec_name(), "DTS Audio");
        dts.audio_mode = TsAudioMode::Extended;
        assert_eq!(dts.codec_name(), "DTS-ES Audio");
        // DTS-HD High-Res vs DTS:X High-Res.
        let mut hr = audio(TsStreamType::DtsHdAudio, 6, 1, 48_000, 0);
        assert_eq!(hr.codec_name(), "DTS-HD High-Res Audio");
        hr.has_extensions = true;
        assert_eq!(hr.codec_name(), "DTS:X High-Res Audio");
        // DTS Express (secondary) is fixed.
        assert_eq!(
            audio(TsStreamType::DtsHdSecondaryAudio, 2, 0, 48_000, 0).codec_name(),
            "DTS Express"
        );
        // DTS-HD Master vs DTS:X Master.
        let mut ma = audio(TsStreamType::DtsHdMasterAudio, 6, 1, 48_000, 0);
        assert_eq!(ma.codec_name(), "DTS-HD Master Audio");
        ma.has_extensions = true;
        assert_eq!(ma.codec_name(), "DTS:X Master Audio");
        // LPCM is a fixed label; MPEG-1/2 audio and AAC echo their ext_data
        // (empty when unset).
        assert_eq!(audio(TsStreamType::LpcmAudio, 6, 1, 48_000, 0).codec_name(), "LPCM Audio");
        for ty in [
            TsStreamType::Mpeg1Audio,
            TsStreamType::Mpeg2Audio,
            TsStreamType::Mpeg2AacAudio,
            TsStreamType::Mpeg4AacAudio,
        ] {
            let mut s = audio(ty, 2, 0, 48_000, 0);
            assert_eq!(s.codec_name(), "", "{ty:?} with no ExtendedData");
            s.ext_data = Some("MPEG-4 AAC LC".to_owned());
            assert_eq!(s.codec_name(), "MPEG-4 AAC LC", "{ty:?} echoes ExtendedData");
        }
        // An unhandled type falls through to UNKNOWN.
        assert_eq!(audio(TsStreamType::Unknown, 6, 1, 48_000, 0).codec_name(), "UNKNOWN");
    }

    #[test]
    fn channel_description_counts_layouts_and_modes() {
        // Channel count present → "C.L".
        assert_eq!(audio(TsStreamType::Ac3TrueHdAudio, 7, 1, 0, 0).channel_description(), "7.1");
        assert_eq!(audio(TsStreamType::Ac3Audio, 2, 0, 0, 0).channel_description(), "2.0");
        // No channel count → fall back to the channel-layout map.
        let layout = |l| {
            let mut s = audio(TsStreamType::Ac3Audio, 0, 0, 0, 0);
            s.channel_layout = l;
            s.channel_description()
        };
        assert_eq!(layout(TsChannelLayout::ChannellayoutMono), "1.0");
        assert_eq!(layout(TsChannelLayout::ChannellayoutStereo), "2.0");
        assert_eq!(layout(TsChannelLayout::ChannellayoutMulti), "5.1");
        assert_eq!(layout(TsChannelLayout::ChannellayoutCombo), ""); // unmapped → empty
        assert_eq!(layout(TsChannelLayout::Unknown), "");
        // Extended AC3 → "-EX"; Extended DTS → "-ES".
        let mut ac3ex = audio(TsStreamType::Ac3Audio, 5, 1, 0, 0);
        ac3ex.audio_mode = TsAudioMode::Extended;
        assert_eq!(ac3ex.channel_description(), "5.1-EX");
        let mut dtses = audio(TsStreamType::DtsAudio, 6, 1, 0, 0);
        dtses.audio_mode = TsAudioMode::Extended;
        assert_eq!(dtses.channel_description(), "6.1-ES");
        // Extended on a non-AC3/DTS type adds no suffix.
        let mut thd = audio(TsStreamType::Ac3TrueHdAudio, 8, 0, 0, 0);
        thd.audio_mode = TsAudioMode::Extended;
        assert_eq!(thd.channel_description(), "8.0");
    }

    #[test]
    fn description_renders_a_truehd_over_ac3_line() {
        // A TrueHD stream wrapping an AC3 core, with a dial-norm tag on the core.
        let core = audio(TsStreamType::Ac3Audio, 5, 1, 48_000, 640_000);
        let mut core = core;
        core.dial_norm = -31;
        assert_eq!(core.description(), "5.1 / 48 kHz /   640 kbps / DN -31dB");

        let mut thd = audio(TsStreamType::Ac3TrueHdAudio, 7, 1, 48_000, 0);
        thd.bit_depth = 16;
        thd.core_stream = Some(Box::new(core));
        assert_eq!(
            thd.description(),
            "7.1 / 48 kHz / 16-bit (AC3 Embedded: 5.1 / 48 kHz /   640 kbps / DN -31dB)"
        );
    }

    #[test]
    fn description_truehd_bitrate_nets_out_the_core() {
        // For a TrueHD stream with its own bit rate, the kbps shown is net of the
        // embedded core's rate (exercises the core-subtraction with a core present).
        let mut thd = audio(TsStreamType::Ac3TrueHdAudio, 7, 1, 48_000, 5_000_000);
        let core = audio(TsStreamType::Ac3Audio, 5, 1, 48_000, 640_000);
        thd.core_stream = Some(Box::new(core));
        // (5_000_000 - 640_000) / 1000 = 4360.
        assert_eq!(
            thd.description(),
            "7.1 / 48 kHz /  4360 kbps (AC3 Embedded: 5.1 / 48 kHz /   640 kbps)"
        );
        // The same stream type with no core → the subtraction default is 0.
        let bare = audio(TsStreamType::Ac3TrueHdAudio, 7, 1, 48_000, 5_000_000);
        assert_eq!(bare.description(), "7.1 / 48 kHz /  5000 kbps");
    }

    #[test]
    fn description_covers_each_optional_field_and_mode_tag() {
        // No fields set (all the `if` guards false) → just the channel layout.
        assert_eq!(audio(TsStreamType::Ac3Audio, 2, 0, 0, 0).description(), "2.0");
        // 2.0 stereo mode tags.
        let with_mode = |mode| {
            let mut s = audio(TsStreamType::Ac3Audio, 2, 0, 48_000, 0);
            s.audio_mode = mode;
            s.description()
        };
        assert_eq!(with_mode(TsAudioMode::DualMono), "2.0 / 48 kHz / Dual Mono");
        assert_eq!(with_mode(TsAudioMode::Surround), "2.0 / 48 kHz / Dolby Surround");
        assert_eq!(with_mode(TsAudioMode::JointStereo), "2.0 / 48 kHz / Joint Stereo");
        assert_eq!(with_mode(TsAudioMode::Stereo), "2.0 / 48 kHz"); // no tag for plain Stereo
        // Bit depth + dial norm, and a non-2.0 count skips the mode tags entirely.
        let mut s = audio(TsStreamType::Ac3Audio, 6, 1, 48_000, 448_000);
        s.bit_depth = 24;
        s.dial_norm = -27;
        s.audio_mode = TsAudioMode::DualMono; // ignored: channel_count != 2
        assert_eq!(s.description(), "6.1 / 48 kHz /   448 kbps / 24-bit / DN -27dB");
    }

    #[test]
    fn description_embedded_core_labels_each_type() {
        let embed = |core_type, expect: &str| {
            let core = audio(core_type, 6, 1, 0, 0); // minimal core desc → just "6.1"
            let mut s = audio(TsStreamType::Ac3TrueHdAudio, 8, 0, 0, 0);
            s.core_stream = Some(Box::new(core));
            assert_eq!(s.description(), format!("8.0 ({expect}: 6.1)"));
        };
        embed(TsStreamType::Ac3Audio, "AC3 Embedded");
        embed(TsStreamType::DtsAudio, "DTS Core");
        embed(TsStreamType::Ac3PlusAudio, "DD+ Embedded");
        embed(TsStreamType::LpcmAudio, ""); // an unmapped core type → empty label
    }

    #[test]
    fn ts_clone_copies_the_reset_subset() {
        let mut inner = audio(TsStreamType::Ac3Audio, 5, 1, 48_000, 640_000);
        inner.dial_norm = -31;
        let mut src = audio(TsStreamType::Ac3TrueHdAudio, 7, 1, 48_000, 1234);
        src.bit_depth = 16;
        src.audio_mode = TsAudioMode::Surround;
        src.channel_layout = TsChannelLayout::ChannellayoutMulti;
        src.has_extensions = true; // deliberately NOT carried by ts_clone
        src.base.pid = Pid::new(0x1100);
        src.base.descriptors = Some(vec![TsDescriptor::new(0x05, 3)]);
        src.base.is_vbr = true;
        src.base.is_initialized = true;
        src.base.set_language_code("eng");
        src.base.payload_bytes = 999; // a base demux counter NOT carried by ts_clone
        src.ext_data = Some("MPEG 1 Layer III".to_owned()); // carried by ts_clone
        src.core_stream = Some(Box::new(inner));

        let clone = src.ts_clone();
        // Carried fields (the identity subset).
        assert_eq!(clone.base.pid, Pid::new(0x1100));
        assert_eq!(clone.base.descriptors, Some(vec![TsDescriptor::new(0x05, 3)]));
        assert_eq!(clone.base.stream_type, TsStreamType::Ac3TrueHdAudio);
        assert_eq!(clone.base.bit_rate, 1234);
        assert!(clone.base.is_vbr && clone.base.is_initialized);
        assert_eq!(clone.base.language_code(), Some("eng"));
        assert_eq!(clone.base.language_name.as_deref(), Some("English"));
        assert_eq!(
            (clone.channel_count, clone.lfe, clone.sample_rate, clone.bit_depth),
            (7, 1, 48_000, 16)
        );
        assert_eq!(clone.audio_mode, TsAudioMode::Surround);
        assert_eq!(clone.channel_layout, TsChannelLayout::ChannellayoutMulti);
        assert_eq!(clone.ext_data.as_deref(), Some("MPEG 1 Layer III"));
        // NOT carried: has_extensions resets to false; base counters reset.
        assert!(!clone.has_extensions);
        assert_eq!(clone.base.payload_bytes, 0);
        // The core is deep-cloned (recursive ts_clone).
        let core = clone.core_stream.expect("core cloned");
        assert_eq!((core.channel_count, core.lfe, core.dial_norm), (5, 1, -31));
        assert_eq!(core.base.bit_rate, 640_000);

        // No language code / no core / no ext_data → those branches stay None.
        let bare = audio(TsStreamType::Ac3Audio, 2, 0, 48_000, 0).ts_clone();
        assert_eq!(bare.base.language_code(), None);
        assert!(bare.core_stream.is_none());
        assert_eq!(bare.ext_data, None);
    }

    /// The video inside `s`, or `None` for any other kind.
    fn video_of(s: TsStream) -> Option<TsVideoStream> {
        if let TsStream::Video(v) = s { Some(v) } else { None }
    }

    /// The audio inside `s`, or `None` for any other kind.
    fn audio_of(s: TsStream) -> Option<TsAudioStream> {
        if let TsStream::Audio(a) = s { Some(a) } else { None }
    }

    /// The graphics inside `s`, or `None` for any other kind.
    fn graphics_of(s: TsStream) -> Option<TsGraphicsStream> {
        if let TsStream::Graphics(g) = s { Some(g) } else { None }
    }

    /// The text inside `s`, or `None` for any other kind.
    fn text_of(s: TsStream) -> Option<TsTextStream> {
        if let TsStream::Text(t) = s { Some(t) } else { None }
    }

    #[test]
    fn ts_clone_resets_counters_for_every_stream_kind() {
        // Video: format/rate detail carried, demux counters reset.
        let mut video = TsVideoStream::default();
        video.base.pid = Pid::new(0x1011);
        video.base.stream_type = TsStreamType::AvcVideo;
        video.base.is_vbr = true;
        video.base.bit_rate = 5_000_000;
        video.base.payload_bytes = 7;
        video.base.packet_count = 8;
        video.base.packet_seconds = 9.0;
        video.base.active_bit_rate = 10;
        video.set_video_format(TsVideoFormat::Videoformat1080i);
        video.set_frame_rate(TsFrameRate::Framerate23_976);
        video.aspect_ratio = TsAspectRatio::Aspect16_9;
        video.width = 1920;
        video.encoding_profile = Some("High Profile 4.1".to_owned());
        let clone = video_of(TsStream::Video(video).ts_clone()).expect("video stays video");
        assert_eq!(clone.base.pid, Pid::new(0x1011));
        assert_eq!(clone.base.bit_rate, 5_000_000);
        assert!(clone.base.is_vbr);
        assert_eq!((clone.width, clone.height, clone.is_interlaced), (1920, 1080, true));
        assert_eq!(clone.video_format(), TsVideoFormat::Videoformat1080i);
        assert_eq!(clone.frame_rate(), TsFrameRate::Framerate23_976);
        assert_eq!((clone.frame_rate_enumerator, clone.frame_rate_denominator), (24_000, 1001));
        assert_eq!(clone.aspect_ratio, TsAspectRatio::Aspect16_9);
        assert_eq!(clone.encoding_profile.as_deref(), Some("High Profile 4.1"));
        assert_eq!(clone.extended_data, None);
        assert_eq!(clone.base.payload_bytes, 0);
        assert_eq!(clone.base.packet_count, 0);
        assert_eq!(clone.base.packet_seconds.to_bits(), 0.0_f64.to_bits());
        assert_eq!(clone.base.active_bit_rate, 0);

        // Graphics: caption tallies + resolution carried, counters reset.
        let mut graphics = TsGraphicsStream::default();
        graphics.base.stream_type = TsStreamType::PresentationGraphics;
        graphics.base.set_language_code("fra");
        graphics.base.packet_count = 3;
        graphics.width = 1920;
        graphics.height = 1080;
        graphics.captions = 5;
        graphics.forced_captions = 2;
        graphics.caption_ids.insert(1, crate::codec::pgs::Frame::default());
        let clone =
            graphics_of(TsStream::Graphics(graphics).ts_clone()).expect("graphics stays graphics");
        assert_eq!((clone.width, clone.height), (1920, 1080));
        assert_eq!((clone.captions, clone.forced_captions), (5, 2));
        assert_eq!(clone.caption_ids.len(), 1);
        assert_eq!(clone.base.language_name.as_deref(), Some("French"));
        assert!(clone.base.is_vbr); // the graphics default, carried
        assert_eq!(clone.base.packet_count, 0);

        // Text: only the identity fields exist; counters reset. The identity
        // fields must survive the clone — a default-constructed copy would
        // satisfy the flag/counter checks alone.
        let mut text = TsTextStream::default();
        text.base.stream_type = TsStreamType::Subtitle;
        text.base.pid = Pid::new(0x1A00);
        text.base.set_language_code("ita");
        text.base.payload_bytes = 11;
        let clone = text_of(TsStream::Text(text).ts_clone()).expect("text stays text");
        assert!(clone.base.is_vbr && clone.base.is_initialized);
        assert_eq!(clone.base.stream_type, TsStreamType::Subtitle);
        assert_eq!(clone.base.pid, Pid::new(0x1A00));
        assert_eq!(clone.base.language_name.as_deref(), Some("Italian"));
        assert_eq!(clone.base.payload_bytes, 0);

        // The audio arm of the dispatch (the audio detail is covered above).
        let mut audio_stream = TsAudioStream::default();
        audio_stream.base.stream_type = TsStreamType::Ac3Audio;
        audio_stream.base.packet_count = 4;
        let clone = audio_of(TsStream::Audio(audio_stream).ts_clone()).expect("audio stays audio");
        assert_eq!(clone.base.stream_type, TsStreamType::Ac3Audio);
        assert_eq!(clone.base.packet_count, 0);

        // A clone never changes kind (the accessors' `None` arms).
        assert!(audio_of(TsStream::Video(TsVideoStream::default()).ts_clone()).is_none());
        assert!(video_of(TsStream::Text(TsTextStream::default()).ts_clone()).is_none());
        assert!(graphics_of(TsStream::Audio(TsAudioStream::default()).ts_clone()).is_none());
        assert!(text_of(TsStream::Graphics(TsGraphicsStream::default()).ts_clone()).is_none());
    }

    #[test]
    fn convert_sample_rate_maps_each_code() {
        assert_eq!(TsAudioStream::convert_sample_rate(TsSampleRate::Samplerate48), 48_000);
        assert_eq!(TsAudioStream::convert_sample_rate(TsSampleRate::Samplerate96), 96_000);
        assert_eq!(TsAudioStream::convert_sample_rate(TsSampleRate::Samplerate48_96), 96_000);
        assert_eq!(TsAudioStream::convert_sample_rate(TsSampleRate::Samplerate192), 192_000);
        assert_eq!(TsAudioStream::convert_sample_rate(TsSampleRate::Samplerate48_192), 192_000);
        assert_eq!(TsAudioStream::convert_sample_rate(TsSampleRate::Unknown), 0);
    }

    #[test]
    fn concrete_stream_defaults_set_the_right_flags() {
        // Video/audio streams: all-default.
        let v = TsVideoStream::default();
        assert!(!v.base.is_vbr);
        assert!(!v.base.is_initialized);
        assert_eq!(v.video_format(), TsVideoFormat::Unknown);
        assert_eq!(v.aspect_ratio, TsAspectRatio::Unknown);
        let a = TsAudioStream::default();
        assert!(!a.base.is_vbr);
        assert_eq!(a.audio_mode, TsAudioMode::Unknown);
        assert_eq!(a.channel_layout, TsChannelLayout::Unknown);
        // Graphics streams: VBR on, not yet initialized.
        let g = TsGraphicsStream::default();
        assert!(g.base.is_vbr);
        assert!(!g.base.is_initialized);
        assert_eq!((g.width, g.height, g.captions, g.forced_captions), (0, 0, 0, 0));
        // Text streams: VBR on and already initialized.
        let t = TsTextStream::default();
        assert!(t.base.is_vbr);
        assert!(t.base.is_initialized);
        // Exercise the derived PartialEq/Debug on the structs.
        assert_eq!(t, TsTextStream::default());
        assert_eq!(g, TsGraphicsStream::default());
        assert!(format!("{v:?}").starts_with("TsVideoStream"));
    }

    #[test]
    fn stream_type_from_u8_maps_known_and_unknown() {
        let known: [(u8, TsStreamType); 22] = [
            (0x01, TsStreamType::Mpeg1Video),
            (0x02, TsStreamType::Mpeg2Video),
            (0x1B, TsStreamType::AvcVideo),
            (0x20, TsStreamType::MvcVideo),
            (0x24, TsStreamType::HevcVideo),
            (0xEA, TsStreamType::Vc1Video),
            (0x03, TsStreamType::Mpeg1Audio),
            (0x04, TsStreamType::Mpeg2Audio),
            (0x0F, TsStreamType::Mpeg2AacAudio),
            (0x11, TsStreamType::Mpeg4AacAudio),
            (0x80, TsStreamType::LpcmAudio),
            (0x81, TsStreamType::Ac3Audio),
            (0x84, TsStreamType::Ac3PlusAudio),
            (0xA1, TsStreamType::Ac3PlusSecondaryAudio),
            (0x83, TsStreamType::Ac3TrueHdAudio),
            (0x82, TsStreamType::DtsAudio),
            (0x85, TsStreamType::DtsHdAudio),
            (0xA2, TsStreamType::DtsHdSecondaryAudio),
            (0x86, TsStreamType::DtsHdMasterAudio),
            (0x90, TsStreamType::PresentationGraphics),
            (0x91, TsStreamType::InteractiveGraphics),
            (0x92, TsStreamType::Subtitle),
        ];
        for &(byte, variant) in &known {
            assert_eq!(TsStreamType::from_u8(byte), variant, "byte {byte:#04X}");
        }
        // Every byte not in the table (including 0x00) collapses to Unknown.
        for byte in 0..=u8::MAX {
            if !known.iter().any(|&(b, _)| b == byte) {
                assert_eq!(TsStreamType::from_u8(byte), TsStreamType::Unknown, "byte {byte:#04X}");
            }
        }
    }

    #[test]
    fn stream_type_name_renders_each_display_name() {
        let cases: [(TsStreamType, &str); 23] = [
            (TsStreamType::Unknown, "Unknown"),
            (TsStreamType::Mpeg1Video, "MPEG1_VIDEO"),
            (TsStreamType::Mpeg2Video, "MPEG2_VIDEO"),
            (TsStreamType::AvcVideo, "AVC_VIDEO"),
            (TsStreamType::MvcVideo, "MVC_VIDEO"),
            (TsStreamType::HevcVideo, "HEVC_VIDEO"),
            (TsStreamType::Vc1Video, "VC1_VIDEO"),
            (TsStreamType::Mpeg1Audio, "MPEG1_AUDIO"),
            (TsStreamType::Mpeg2Audio, "MPEG2_AUDIO"),
            (TsStreamType::Mpeg2AacAudio, "MPEG2_AAC_AUDIO"),
            (TsStreamType::Mpeg4AacAudio, "MPEG4_AAC_AUDIO"),
            (TsStreamType::LpcmAudio, "LPCM_AUDIO"),
            (TsStreamType::Ac3Audio, "AC3_AUDIO"),
            (TsStreamType::Ac3PlusAudio, "AC3_PLUS_AUDIO"),
            (TsStreamType::Ac3PlusSecondaryAudio, "AC3_PLUS_SECONDARY_AUDIO"),
            (TsStreamType::Ac3TrueHdAudio, "AC3_TRUE_HD_AUDIO"),
            (TsStreamType::DtsAudio, "DTS_AUDIO"),
            (TsStreamType::DtsHdAudio, "DTS_HD_AUDIO"),
            (TsStreamType::DtsHdSecondaryAudio, "DTS_HD_SECONDARY_AUDIO"),
            (TsStreamType::DtsHdMasterAudio, "DTS_HD_MASTER_AUDIO"),
            (TsStreamType::PresentationGraphics, "PRESENTATION_GRAPHICS"),
            (TsStreamType::InteractiveGraphics, "INTERACTIVE_GRAPHICS"),
            (TsStreamType::Subtitle, "SUBTITLE"),
        ];
        for (variant, name) in cases {
            assert_eq!(variant.name(), name, "{variant:?}");
        }
    }

    #[test]
    fn stream_type_value_round_trips_through_from_u8() {
        // `value` is the exact inverse of `from_u8` on every recognized code,
        // and `Unknown` yields 0x00. Round-tripping every byte pins both maps
        // against each other (a wrong arm in either breaks some byte).
        for byte in 0..=u8::MAX {
            let variant = TsStreamType::from_u8(byte);
            if variant != TsStreamType::Unknown {
                assert_eq!(variant.value(), byte, "byte {byte:#04X}");
            }
        }
        assert_eq!(TsStreamType::Unknown.value(), 0x00);
        // Spot-pin the two report-visible cells so a swapped pair of arms that
        // still round-trips cannot slip through.
        assert_eq!(TsStreamType::AvcVideo.value(), 0x1B);
        assert_eq!(TsStreamType::Ac3Audio.value(), 0x81);
    }

    #[test]
    fn codec_alt_name_covers_every_type_and_extension_upgrade() {
        let plain: [(TsStreamType, &str); 17] = [
            (TsStreamType::Mpeg1Video, "MPEG-1"),
            (TsStreamType::Mpeg2Video, "MPEG-2"),
            (TsStreamType::AvcVideo, "AVC"),
            (TsStreamType::MvcVideo, "MVC"),
            (TsStreamType::HevcVideo, "HEVC"),
            (TsStreamType::Vc1Video, "VC-1"),
            (TsStreamType::Mpeg1Audio, "MP1"),
            (TsStreamType::Mpeg2Audio, "MP2"),
            (TsStreamType::Mpeg2AacAudio, "MPEG-2 AAC"),
            (TsStreamType::Mpeg4AacAudio, "MPEG-4 AAC"),
            (TsStreamType::LpcmAudio, "LPCM"),
            (TsStreamType::Ac3Audio, "DD AC3"),
            (TsStreamType::DtsAudio, "DTS"),
            (TsStreamType::DtsHdSecondaryAudio, "DTS Express"),
            (TsStreamType::PresentationGraphics, "PGS"),
            (TsStreamType::InteractiveGraphics, "IGS"),
            (TsStreamType::Subtitle, "SUB"),
        ];
        for (ty, alt) in plain {
            // The concrete kind is irrelevant to the lookup: a video wrapper
            // suffices for every non-extension arm.
            let stream = TsStream::Video(video(ty));
            assert_eq!(stream.codec_alt_name(), alt, "{ty:?}");
        }
        assert_eq!(TsStream::Video(video(TsStreamType::Unknown)).codec_alt_name(), "UNKNOWN");

        // Both DD+ types share one label.
        for ty in [TsStreamType::Ac3PlusAudio, TsStreamType::Ac3PlusSecondaryAudio] {
            let stream = TsStream::Audio(audio(ty, 6, 0, 48_000, 0));
            assert_eq!(stream.codec_alt_name(), "DD AC3+", "{ty:?}");
        }

        // The Dolby/DTS extension upgrades flip on `has_extensions` — and only
        // an audio wrapper can carry them.
        let upgrades = [
            (TsStreamType::Ac3TrueHdAudio, "Dolby TrueHD", "Dolby Atmos"),
            (TsStreamType::DtsHdAudio, "DTS-HD Hi-Res", "DTS:X Hi-Res"),
            (TsStreamType::DtsHdMasterAudio, "DTS-HD Master", "DTS:X Master"),
        ];
        for (ty, without, with) in upgrades {
            let mut a = audio(ty, 6, 1, 48_000, 0);
            assert_eq!(TsStream::Audio(a.clone()).codec_alt_name(), without, "{ty:?}");
            a.has_extensions = true;
            assert_eq!(TsStream::Audio(a).codec_alt_name(), with, "{ty:?} + extensions");
        }
    }

    #[test]
    fn nibble_enums_from_u8_map_known_and_unknown() {
        let video: [(u8, TsVideoFormat); 8] = [
            (1, TsVideoFormat::Videoformat480i),
            (2, TsVideoFormat::Videoformat576i),
            (3, TsVideoFormat::Videoformat480p),
            (4, TsVideoFormat::Videoformat1080i),
            (5, TsVideoFormat::Videoformat720p),
            (6, TsVideoFormat::Videoformat1080p),
            (7, TsVideoFormat::Videoformat576p),
            (8, TsVideoFormat::Videoformat2160p),
        ];
        for &(byte, variant) in &video {
            assert_eq!(TsVideoFormat::from_u8(byte), variant, "video {byte}");
        }
        for byte in 0..=u8::MAX {
            if !video.iter().any(|&(b, _)| b == byte) {
                assert_eq!(TsVideoFormat::from_u8(byte), TsVideoFormat::Unknown, "video {byte}");
            }
        }

        let rate: [(u8, TsFrameRate); 6] = [
            (1, TsFrameRate::Framerate23_976),
            (2, TsFrameRate::Framerate24),
            (3, TsFrameRate::Framerate25),
            (4, TsFrameRate::Framerate29_97),
            (6, TsFrameRate::Framerate50),
            (7, TsFrameRate::Framerate59_94),
        ];
        for &(byte, variant) in &rate {
            assert_eq!(TsFrameRate::from_u8(byte), variant, "rate {byte}");
        }
        for byte in 0..=u8::MAX {
            if !rate.iter().any(|&(b, _)| b == byte) {
                // Code 5 is unused/reserved → Unknown, exercised here.
                assert_eq!(TsFrameRate::from_u8(byte), TsFrameRate::Unknown, "rate {byte}");
            }
        }

        let aspect: [(u8, TsAspectRatio); 3] = [
            (2, TsAspectRatio::Aspect4_3),
            (3, TsAspectRatio::Aspect16_9),
            (4, TsAspectRatio::Aspect2_21),
        ];
        for &(byte, variant) in &aspect {
            assert_eq!(TsAspectRatio::from_u8(byte), variant, "aspect {byte}");
        }
        for byte in 0..=u8::MAX {
            if !aspect.iter().any(|&(b, _)| b == byte) {
                assert_eq!(TsAspectRatio::from_u8(byte), TsAspectRatio::Unknown, "aspect {byte}");
            }
        }

        let layout: [(u8, TsChannelLayout); 4] = [
            (1, TsChannelLayout::ChannellayoutMono),
            (3, TsChannelLayout::ChannellayoutStereo),
            (6, TsChannelLayout::ChannellayoutMulti),
            (12, TsChannelLayout::ChannellayoutCombo),
        ];
        for &(byte, variant) in &layout {
            assert_eq!(TsChannelLayout::from_u8(byte), variant, "layout {byte}");
        }
        for byte in 0..=u8::MAX {
            if !layout.iter().any(|&(b, _)| b == byte) {
                assert_eq!(
                    TsChannelLayout::from_u8(byte),
                    TsChannelLayout::Unknown,
                    "layout {byte}"
                );
            }
        }

        let sample: [(u8, TsSampleRate); 5] = [
            (1, TsSampleRate::Samplerate48),
            (4, TsSampleRate::Samplerate96),
            (5, TsSampleRate::Samplerate192),
            (12, TsSampleRate::Samplerate48_192),
            (14, TsSampleRate::Samplerate48_96),
        ];
        for &(byte, variant) in &sample {
            assert_eq!(TsSampleRate::from_u8(byte), variant, "sample {byte}");
        }
        for byte in 0..=u8::MAX {
            if !sample.iter().any(|&(b, _)| b == byte) {
                assert_eq!(TsSampleRate::from_u8(byte), TsSampleRate::Unknown, "sample {byte}");
            }
        }
    }

    #[test]
    fn ts_stream_union_exposes_base_for_each_kind() {
        // One of each kind, with a distinct PID/stream type set through the shared
        // mutable base — verifies every match arm of base/base_mut/pid/stream_type.
        let mut v = TsStream::Video(TsVideoStream::default());
        v.base_mut().pid = Pid::new(0x1011);
        v.base_mut().stream_type = TsStreamType::AvcVideo;
        assert_eq!(v.pid(), Pid::new(0x1011));
        assert_eq!(v.stream_type(), TsStreamType::AvcVideo);
        assert_eq!(v.base().pid, Pid::new(0x1011));

        let mut a = TsStream::Audio(TsAudioStream::default());
        a.base_mut().pid = Pid::new(0x1100);
        a.base_mut().stream_type = TsStreamType::DtsHdMasterAudio;
        assert_eq!(a.pid(), Pid::new(0x1100));
        assert_eq!(a.stream_type(), TsStreamType::DtsHdMasterAudio);
        assert_eq!(a.base().stream_type, TsStreamType::DtsHdMasterAudio);

        let mut g = TsStream::Graphics(TsGraphicsStream::default());
        g.base_mut().pid = Pid::new(0x1200);
        assert_eq!(g.pid(), Pid::new(0x1200));
        assert_eq!(g.stream_type(), TsStreamType::Unknown);
        assert!(g.base().is_vbr); // graphics default: VBR on

        let mut t = TsStream::Text(TsTextStream::default());
        t.base_mut().pid = Pid::new(0x1A00);
        t.base_mut().stream_type = TsStreamType::Subtitle;
        assert_eq!(t.pid(), Pid::new(0x1A00));
        assert_eq!(t.stream_type(), TsStreamType::Subtitle);
        assert!(t.base().is_initialized); // text default: already initialized

        // Derived Debug / Clone / PartialEq on the union.
        let clone = t.clone();
        assert_eq!(clone, t);
        assert!(format!("{v:?}").starts_with("Video"));
        assert_ne!(TsStream::Video(TsVideoStream::default()), v);
    }

    /// A graphics stream of `stream_type` with the given codec-derived fields.
    fn graphics(
        stream_type: TsStreamType,
        width: i32,
        height: i32,
        captions: i32,
        forced_captions: i32,
    ) -> TsGraphicsStream {
        TsGraphicsStream {
            base: TsStreamBase { stream_type, ..TsStreamBase::default() },
            width,
            height,
            captions,
            forced_captions,
            ..TsGraphicsStream::default()
        }
    }

    #[test]
    fn graphics_codec_short_and_long_names_cover_every_arm() {
        let pgs = graphics(TsStreamType::PresentationGraphics, 0, 0, 0, 0);
        assert_eq!(pgs.codec_short_name(), "PGS");
        assert_eq!(pgs.codec_name(), "Presentation Graphics");
        let igs = graphics(TsStreamType::InteractiveGraphics, 0, 0, 0, 0);
        assert_eq!(igs.codec_short_name(), "IGS");
        assert_eq!(igs.codec_name(), "Interactive Graphics");
        // The non-graphics fallback arm (a graphics stream never carries this type).
        let other = graphics(TsStreamType::Subtitle, 0, 0, 0, 0);
        assert_eq!(other.codec_short_name(), "UNKNOWN");
        assert_eq!(other.codec_name(), "UNKNOWN");
    }

    #[test]
    fn graphics_description_renders_resolution_and_caption_tally() {
        // A PGS stream whose scan set no dimensions or captions stays empty.
        assert_eq!(graphics(TsStreamType::PresentationGraphics, 0, 0, 0, 0).description(), "");
        // Resolution only (either dimension non-zero triggers the WxH block).
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 1920, 1080, 0, 0).description(),
            "1920x1080"
        );
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 0, 1080, 0, 0).description(),
            "0x1080"
        );
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 1920, 0, 0, 0).description(),
            "1920x0"
        );
        // Captions: singular vs plural.
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 1920, 1080, 1, 0).description(),
            "1920x1080 / 1 Caption"
        );
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 1920, 1080, 5, 0).description(),
            "1920x1080 / 5 Captions"
        );
        // Forced captions without regular captions: the " / N Forced Caption[s]" form.
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 1920, 1080, 0, 1).description(),
            "1920x1080 / 1 Forced Caption"
        );
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 1920, 1080, 0, 3).description(),
            "1920x1080 / 3 Forced Captions"
        );
        // Both: the " (N Forced Caption[s])" parenthesised form.
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 1920, 1080, 2, 1).description(),
            "1920x1080 / 2 Captions (1 Forced Caption)"
        );
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 1920, 1080, 1, 4).description(),
            "1920x1080 / 1 Caption (4 Forced Captions)"
        );
        // Captions with no resolution (the WxH block is skipped, the tally still runs).
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 0, 0, 1, 0).description(),
            " / 1 Caption"
        );
        assert_eq!(
            graphics(TsStreamType::PresentationGraphics, 0, 0, 0, 2).description(),
            " / 2 Forced Captions"
        );
    }

    /// A text-subtitle stream of `stream_type`.
    fn text(stream_type: TsStreamType) -> TsTextStream {
        TsTextStream { base: TsStreamBase { stream_type, ..TsStreamBase::default() } }
    }

    #[test]
    fn text_codec_names_and_empty_description() {
        let sub = text(TsStreamType::Subtitle);
        assert_eq!(sub.codec_short_name(), "SUB");
        assert_eq!(sub.codec_name(), "Subtitle");
        // TextST descriptions are always empty — never a desc.
        assert_eq!(sub.description(), "");
        // The non-text fallback arm (a text stream never carries this type).
        let other = text(TsStreamType::PresentationGraphics);
        assert_eq!(other.codec_short_name(), "UNKNOWN");
        assert_eq!(other.codec_name(), "UNKNOWN");
        assert_eq!(other.description(), "");
    }

    proptest! {
        #[test]
        fn descriptor_payload_is_length_zeros(name in any::<u8>(), length in any::<u8>()) {
            let d = TsDescriptor::new(name, length);
            prop_assert_eq!(d.name, name);
            prop_assert_eq!(d.value.len(), usize::from(length));
            prop_assert!(d.value.iter().all(|&b| b == 0));
        }

        #[test]
        fn set_language_code_matches_get_name(code in "[a-z]{3}") {
            let mut base = TsStreamBase::default();
            base.set_language_code(&code);
            prop_assert_eq!(base.language_code(), Some(code.as_str()));
            prop_assert_eq!(base.language_name, Some(super::language_codes::get_name(&code)));
        }

        #[test]
        fn packet_size_never_panics(count in any::<u64>()) {
            let base = TsStreamBase { packet_count: count, ..TsStreamBase::default() };
            prop_assert_eq!(base.packet_size(), count.wrapping_mul(192));
        }
    }
}
