//! MPEG-4 MVC (H.264 Multiview / 3D) video codec scanner.
//!
//! MVC carries the stereo dependent view alongside the AVC base view; the scanner
//! is deliberately a stub that only marks the stream `is_vbr`/`is_initialized` —
//! the displayed resolution / frame rate / aspect ratio and the `Left Eye`/`Right
//! Eye` base-view tag all come from the CLPI/MPLS metadata, not from decoding the
//! bitstream. [`scan`] therefore reads nothing from `buffer` and never sets `tag`;
//! both are present only to match the shared codec-scan signature.

use crate::bitstream::TsStreamBuffer;
use crate::stream::TsVideoStream;

/// Scans one MVC access unit.
///
/// Marks `stream` initialised and variable-bit-rate; `buffer` and `tag` are
/// unused (the stub decodes nothing).
pub const fn scan(
    stream: &mut TsVideoStream,
    buffer: &mut TsStreamBuffer,
    tag: &mut Option<String>,
) {
    // The shared codec-scan signature; MVC reads neither (a `pub fn` is exempt from
    // `needless_pass_by_ref_mut`).
    let _ = buffer;
    let _ = tag;
    stream.base.is_vbr = true;
    stream.base.is_initialized = true;
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, prop_assert, proptest};

    use super::scan;
    use crate::bitstream::TsStreamBuffer;
    use crate::stream::{TsStreamType, TsVideoFormat, TsVideoStream};

    /// A rewound buffer holding `data`.
    fn buf(data: &[u8]) -> TsStreamBuffer {
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        b
    }

    /// A fresh MVC video stream.
    fn stream() -> TsVideoStream {
        let mut s = TsVideoStream::default();
        s.base.stream_type = TsStreamType::MvcVideo;
        s
    }

    #[test]
    fn scan_initializes_the_stream_and_touches_nothing_else() {
        let mut s = stream();
        let mut b = buf(&[0x00, 0x00, 0x01, 0x09, 0xFF]);
        let mut tag = None;
        scan(&mut s, &mut b, &mut tag);
        assert!(s.base.is_vbr);
        assert!(s.base.is_initialized);
        assert_eq!(tag, None); // MVC never sets the tag
        assert!(s.encoding_profile.is_none()); // nor a profile
    }

    #[test]
    fn mvc_codec_strings_and_base_view_description() {
        // MVC rides the 3D titles: the base-view eye tag + resolution come from the
        // metadata, joined ahead of the (empty) codec contribution.
        let mut s = stream();
        s.base.base_view = Some(true);
        s.set_video_format(TsVideoFormat::Videoformat1080p);
        let mut b = buf(&[]);
        scan(&mut s, &mut b, &mut None);
        assert_eq!(s.codec_short_name(), "MVC");
        assert_eq!(s.codec_name(), "MPEG-4 MVC Video");
        assert_eq!(s.description(), "Right Eye / 1080p");
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>()) {
            let mut s = stream();
            let mut b = buf(&data);
            let mut tag = None;
            scan(&mut s, &mut b, &mut tag);
            prop_assert!(s.base.is_initialized);
        }
    }
}
