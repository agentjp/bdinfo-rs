//! BDMV metadata parsers — the small structural files under `BDMV/`.
//!
//! One module per BDMV file kind: [`clpi`] parses clip-information (`*.clpi`)
//! files (plus the per-clip data model) and [`mpls`] parses playlist (`*.mpls`)
//! files. Both consume a `&[u8]` already read from the VFS and decode it into
//! the [`crate::stream`] data model, returning [`crate::error::BdError`] on
//! malformed input.
//!
//! Both file kinds describe a stream entry with the same coding-info layout at
//! their own offset, so both decode it through the one `build_coded_stream`
//! here, beside the shared bounds-checked byte reads.
//!
//! The disc-level orchestration that wires these together — the [`disc`] scan
//! plus the cross-clip resolution that feeds the [`m2ts`] demux — builds on
//! these standalone parsers. Four presentation modules sit beside them, over
//! the scanned model rather than the bytes: [`order`] (which playlists a
//! surface lists, and in what order), [`progress`] (how far along a running
//! scan is), [`measured`] (what a scan has tallied while it still runs) and
//! [`shortfall`] (which stream files the scan read less of than the disc
//! declares).

use crate::bytes;
use crate::error::BdError;
use crate::stream::{
    StreamKind, TsAspectRatio, TsAudioStream, TsChannelLayout, TsFrameRate, TsGraphicsStream,
    TsSampleRate, TsStream, TsStreamType, TsTextStream, TsVideoFormat, TsVideoStream,
};

pub mod chapters;
pub mod clpi;
pub mod disc;
pub mod interleaved;
pub mod m2ts;
pub mod measured;
pub mod mpls;
pub mod order;
pub mod progress;
pub mod shortfall;

// Shared fallible reads: every byte access in the parsers goes through these
// bounds-checked helpers, mapping an out-of-bounds read (a short/truncated
// file) to [`BdError::UnexpectedEof`].

/// Reads the byte at `off`, or [`BdError::UnexpectedEof`] past the end.
pub(crate) fn byte(buf: &[u8], off: usize) -> Result<u8, BdError> {
    bytes::read_u8(buf, off).ok_or(BdError::UnexpectedEof)
}

/// Reads a big-endian `u16` at `off`, or [`BdError::UnexpectedEof`].
pub(crate) fn u16_be(buf: &[u8], off: usize) -> Result<u16, BdError> {
    bytes::read_u16_be(buf, off).ok_or(BdError::UnexpectedEof)
}

/// Reads a big-endian `u32` at `off`, or [`BdError::UnexpectedEof`].
pub(crate) fn u32_be(buf: &[u8], off: usize) -> Result<u32, BdError> {
    bytes::read_u32_be(buf, off).ok_or(BdError::UnexpectedEof)
}

/// Reads a big-endian `u32` at `off` as a `usize` offset, or
/// [`BdError::UnexpectedEof`].
///
/// The full unsigned 32-bit value is taken as an offset; one larger than
/// `usize` (only reachable on a <32-bit target) saturates to `usize::MAX`,
/// which then fails the next bounds check as EOF — the same "malformed" outcome
/// a truncated file produces.
pub(crate) fn u32_off(buf: &[u8], off: usize) -> Result<usize, BdError> {
    let value = bytes::read_u32_be(buf, off).ok_or(BdError::UnexpectedEof)?;
    Ok(usize::try_from(value).unwrap_or(usize::MAX))
}

/// Reads `count` ASCII bytes at `off` (non-ASCII bytes decode to `'?'`), or
/// [`BdError::UnexpectedEof`].
pub(crate) fn ascii(buf: &[u8], off: usize, count: usize) -> Result<String, BdError> {
    bytes::read_ascii(buf, off, count).ok_or(BdError::UnexpectedEof)
}

/// Builds the [`TsStream`] a `stream_coding_type` of `stream_type` describes,
/// reading its coding info from `data` starting at `coding`, or `None` for a
/// [`StreamKind::Unknown`] type.
///
/// The clip-information (`*.clpi`) and playlist (`*.mpls`) stream entries carry
/// the same coding-info layout at their own offset — a format byte for video and
/// audio, then a 3-byte ISO-639 language code for audio (at `coding + 1`),
/// graphics (at `coding`) and text (at `coding + 1`, past a 1-byte unused code
/// field). `aspect` names the byte holding the display aspect-ratio nibble of a
/// video stream, which only the clip information carries; `None` leaves
/// [`TsVideoStream::aspect_ratio`] at its default.
///
/// `MvcVideo` is *not* refused here — it is [`StreamKind::Video`] like every
/// other video coding type, and both callers return `None` for it before
/// dispatching. [`TsStreamType::default_stream`] documents why.
pub(crate) fn build_coded_stream(
    data: &[u8],
    coding: usize,
    stream_type: TsStreamType,
    aspect: Option<usize>,
) -> Result<Option<TsStream>, BdError> {
    Ok(match stream_type.kind() {
        StreamKind::Video => {
            // The setters (deriving height/scan + the frame-rate ratio) run
            // first, so the later `aspect_ratio` field write isn't a
            // default-reassign.
            let format = byte(data, coding)?;
            let mut stream = TsVideoStream::default();
            stream.set_video_format(TsVideoFormat::from_u8(format.wrapping_shr(4)));
            stream.set_frame_rate(TsFrameRate::from_u8(format & 0x0F));
            if let Some(aspect) = aspect {
                stream.aspect_ratio = TsAspectRatio::from_u8(byte(data, aspect)?.wrapping_shr(4));
            }
            Some(TsStream::Video(stream))
        }
        StreamKind::Audio => {
            let format = byte(data, coding)?;
            let language = ascii(data, coding.saturating_add(1), 3)?;
            let mut stream = TsAudioStream {
                channel_layout: TsChannelLayout::from_u8(format.wrapping_shr(4)),
                sample_rate: TsAudioStream::convert_sample_rate(TsSampleRate::from_u8(
                    format & 0x0F,
                )),
                ..TsAudioStream::default()
            };
            stream.base.set_language_code(&language);
            Some(TsStream::Audio(stream))
        }
        StreamKind::Graphics => {
            let language = ascii(data, coding, 3)?;
            let mut stream = TsGraphicsStream::default();
            stream.base.set_language_code(&language);
            Some(TsStream::Graphics(stream))
        }
        StreamKind::Text => {
            let language = ascii(data, coding.saturating_add(1), 3)?;
            let mut stream = TsTextStream::default();
            stream.base.set_language_code(&language);
            Some(TsStream::Text(stream))
        }
        StreamKind::Unknown => None,
    })
}

#[cfg(test)]
mod tests {
    use super::{ascii, byte, u16_be, u32_be, u32_off};

    #[test]
    fn helpers_read_in_bounds_values() {
        let buf = [0x12_u8, 0x34, 0x56, 0x78, b'e', b'n', b'g'];
        // `BdError` is not `PartialEq` (its `Io` wraps `io::Error`), so the result tests
        // compare the `Ok`/`Err` shape via `.ok()`/`matches!`.
        assert_eq!(byte(&buf, 0).ok(), Some(0x12));
        assert_eq!(u16_be(&buf, 0).ok(), Some(0x1234));
        assert_eq!(u32_be(&buf, 0).ok(), Some(0x1234_5678));
        assert_eq!(u32_off(&buf, 0).ok(), Some(0x1234_5678));
        assert_eq!(ascii(&buf, 4, 3).ok().as_deref(), Some("eng"));
    }

    #[test]
    fn helpers_map_short_reads_to_eof() {
        let buf = [0x12_u8, 0x34];
        // Each short read fails with `UnexpectedEof`; assert on its `Display` (a
        // region-clean check that also pins the error message).
        let eof = "unexpected end of input";
        assert_eq!(byte(&buf, 2).unwrap_err().to_string(), eof);
        assert_eq!(u16_be(&buf, 1).unwrap_err().to_string(), eof);
        assert_eq!(u32_be(&buf, 0).unwrap_err().to_string(), eof);
        assert_eq!(u32_off(&buf, 0).unwrap_err().to_string(), eof);
        assert_eq!(ascii(&buf, 1, 3).unwrap_err().to_string(), eof);
    }
}
