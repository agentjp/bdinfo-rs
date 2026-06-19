#![no_main]
//! Fuzz target: the codec scanners — the audio `codec::ac3::scan`,
//! `codec::truehd::scan`, `codec::dts::scan`, `codec::dts_hd::scan`,
//! `codec::lpcm::scan`, `codec::aac::scan`, `codec::mpa::scan`, the video
//! `codec::avc::scan`, `codec::mpeg2::scan`, `codec::vc1::scan`, `codec::mvc::scan`,
//! `codec::hevc::scan` and the graphics `codec::pgs::scan` — over the untrusted bytes
//! of an assembled access unit. Amplifies the no-panic / no-out-of-bounds contract
//! that each codec's `scan_never_panics_on_arbitrary_bytes` proptest holds on
//! Windows; here it runs adversarially on nightly/Linux (see fuzz/README.md).
//!
//! The first byte selects the stream type (`% 17`) — so one corpus drives every
//! `ScanStream` dispatch arm (AC-3, DD+, DD+ secondary, TrueHD whose CORE path
//! re-enters the AC-3 scanner, the DTS core, DTS-HD Master/High-Res/Secondary whose
//! CORE path re-enters the DTS scanner, the simple-audio LPCM/AAC/MPEG-audio
//! scanners, the AVC / MPEG-2 / VC-1 / MVC / HEVC video start-code state machines,
//! and the PGS segment dispatcher) — and its high bits seed the measured `bitrate`
//! the DTS scanners take; the rest is the access-unit payload fed to the bit reader.
//! Selectors `0..=15` keep their pre-PGS mapping (the existing seeds use those
//! bytes), and `16` reaches PGS.

use bdinfo_rs_core::bitstream::TsStreamBuffer;
use bdinfo_rs_core::codec::{aac, ac3, avc, dts, dts_hd, hevc, lpcm, mpa, mpeg2, mvc, pgs, truehd, vc1};
use bdinfo_rs_core::stream::{TsAudioStream, TsGraphicsStream, TsStreamType, TsVideoStream};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let selector = data.first().copied().unwrap_or(0);
    let payload = data.get(1..).unwrap_or(&[]);
    // The selector's high bits seed a measured bitrate (0 and positive both occur).
    let bitrate = i64::from(selector / 8).wrapping_mul(100_000);

    let mut buffer = TsStreamBuffer::new();
    buffer.add(payload, 0, payload.len());
    buffer.begin_read();

    let mut tag = None;
    let kind = selector % 17;
    // The graphics scanner takes a graphics stream (selector 16).
    if kind == 16 {
        let mut stream = TsGraphicsStream::default();
        stream.base.stream_type = TsStreamType::PresentationGraphics;
        pgs::scan(&mut stream, &mut buffer, &mut tag);
        return;
    }
    // The video scanners take a video stream; the audio scanners an audio one.
    if kind >= 11 {
        let mut stream = TsVideoStream::default();
        match kind {
            11 => {
                stream.base.stream_type = TsStreamType::AvcVideo;
                avc::scan(&mut stream, &mut buffer, &mut tag);
            }
            12 => {
                stream.base.stream_type = TsStreamType::Mpeg2Video;
                mpeg2::scan(&mut stream, &mut buffer, &mut tag);
            }
            13 => {
                stream.base.stream_type = TsStreamType::Vc1Video;
                vc1::scan(&mut stream, &mut buffer, &mut tag);
            }
            14 => {
                stream.base.stream_type = TsStreamType::MvcVideo;
                mvc::scan(&mut stream, &mut buffer, &mut tag);
            }
            _ => {
                stream.base.stream_type = TsStreamType::HevcVideo;
                hevc::scan(&mut stream, &mut buffer, &mut tag);
            }
        }
        return;
    }

    let mut stream = TsAudioStream::default();
    match kind {
        0 => {
            stream.base.stream_type = TsStreamType::Ac3Audio;
            ac3::scan(&mut stream, &mut buffer, &mut tag);
        }
        1 => {
            stream.base.stream_type = TsStreamType::Ac3PlusAudio;
            ac3::scan(&mut stream, &mut buffer, &mut tag);
        }
        2 => {
            stream.base.stream_type = TsStreamType::Ac3PlusSecondaryAudio;
            ac3::scan(&mut stream, &mut buffer, &mut tag);
        }
        3 => {
            stream.base.stream_type = TsStreamType::Ac3TrueHdAudio;
            truehd::scan(&mut stream, &mut buffer, &mut tag);
        }
        4 => {
            stream.base.stream_type = TsStreamType::DtsAudio;
            dts::scan(&mut stream, &mut buffer, bitrate, &mut tag);
        }
        5 => {
            stream.base.stream_type = TsStreamType::DtsHdMasterAudio;
            dts_hd::scan(&mut stream, &mut buffer, bitrate, &mut tag);
        }
        6 => {
            stream.base.stream_type = TsStreamType::DtsHdAudio;
            dts_hd::scan(&mut stream, &mut buffer, bitrate, &mut tag);
        }
        7 => {
            stream.base.stream_type = TsStreamType::DtsHdSecondaryAudio;
            dts_hd::scan(&mut stream, &mut buffer, bitrate, &mut tag);
        }
        8 => {
            stream.base.stream_type = TsStreamType::LpcmAudio;
            lpcm::scan(&mut stream, &mut buffer, &mut tag);
        }
        9 => {
            stream.base.stream_type = TsStreamType::Mpeg4AacAudio;
            aac::scan(&mut stream, &mut buffer, &mut tag);
        }
        _ => {
            stream.base.stream_type = TsStreamType::Mpeg1Audio;
            mpa::scan(&mut stream, &mut buffer, &mut tag);
        }
    }
});
