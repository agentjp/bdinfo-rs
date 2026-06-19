//! Presentation Graphics (`PGS`) image-subtitle codec scanner.
//!
//! [`scan`] walks one assembled `PGS`
//! access-unit buffer one segment at a time, tracking presentation compositions and
//! object definitions to fill the [`TsGraphicsStream`] caption tally
//! (`captions` / `forced_captions`) and the subtitle resolution (`width` / `height`)
//! — the values the report joins into the graphics `desc` field. There is almost no
//! bitstream math: the body is segment-type dispatch plus fixed-width field skips.
//!
//! Only three `PGS` segment types are observed. The **presentation composition
//! segment** (`PCS`, `0x16`) records the screen resolution once and seeds a [`Frame`]
//! (including the force-display flag) for each composition object. The **object
//! definition segment** (`ODS`, `0x15`) attributes one caption — forced or normal —
//! to the most recent unfinished frame and returns its `F`/`N` tag. The
//! **end-of-display-set marker** (`0x80`) closes the open frame. Interactive
//! graphics (`IGS`) and `TextST` text subtitles have no scanner — they carry only
//! their type-mapped `codec`/`codecname` (see [`crate::stream`]).
//!
//! **Deliberate divergence from classic `BDInfo`.** Each composition
//! object's four 16-bit cropping fields are read **only** when its crop flag
//! (`0x80`) is set, per the HDMV spec and libbluray (`pg_decode.c`). Classic
//! `BDInfo` reads them unconditionally, over-advancing 8 bytes on a multi-object
//! composition whose objects are uncropped and so misreading every later object's
//! forced flag (and its caption / forced-caption tally). bdinfo-rs chooses spec
//! correctness here: a multi-object uncropped PG stream is counted correctly,
//! while single-object and fully-cropped compositions stay byte-identical to
//! `BDInfo`.
//!
//! Like every codec scanner this is panic-free over arbitrary bytes (the bit reader
//! bounds-checks; the caption counters use `wrapping_add` so a hostile caption flood
//! cannot overflow them) — the shared `codec` fuzz target amplifies that
//! adversarially.

use crate::bitstream::TsStreamBuffer;
use crate::stream::TsGraphicsStream;

/// One presentation-composition frame: whether a composition
/// object started it, whether that object is force-displayed, and whether its
/// display set has finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Frame {
    /// Set when a composition object seeded this frame.
    pub started: bool,
    /// Set when the object's cropped flag carried the force bit `0x40`.
    pub forced: bool,
    /// Set when the end-of-display-set marker closed the frame.
    pub finished: bool,
}

/// Scans one `PGS` access unit from `buffer` into `stream`.
///
/// Dispatches on the leading segment-type byte: an `ODS` (`0x15`) counts a caption
/// and sets `tag` to its `F`/`N` marker; a `PCS` (`0x16`) updates the resolution and
/// frame state; the end marker (`0x80`) finishes the open frame; any other segment is
/// ignored. The stream is always flagged variable-bit-rate.
pub fn scan(stream: &mut TsGraphicsStream, buffer: &mut TsStreamBuffer, tag: &mut Option<String>) {
    let segment_type = buffer.read_byte(false);
    match segment_type {
        0x15 => *tag = read_ods(stream, buffer), // Object Definition Segment
        0x16 => read_pcs(stream, buffer),        // Presentation Composition Segment
        0x80 => {
            // Finishing an already-finished frame is idempotent — no guard needed.
            stream.last_frame.finished = true;
        }
        _ => {}
    }
    stream.base.is_vbr = true;
}

/// Reads an object-definition segment. Skips the segment
/// size and object id, then, while the last frame is still open, tallies the caption
/// (forced or normal) and returns its `F`/`N` tag. Returns `None` once the frame
/// has finished.
fn read_ods(stream: &mut TsGraphicsStream, buffer: &mut TsStreamBuffer) -> Option<String> {
    let _ = buffer.read_bits2(16, false); // Segment Size
    let _ = buffer.read_bits2(16, false); // Object ID

    if stream.last_frame.finished {
        return None;
    }
    if stream.last_frame.forced {
        stream.forced_captions = stream.forced_captions.wrapping_add(1);
        Some("F".to_owned())
    } else {
        stream.captions = stream.captions.wrapping_add(1);
        Some("N".to_owned())
    }
}

/// Reads a presentation-composition segment. Records the
/// screen resolution on the first (uninitialised) segment, then re-seeds
/// [`TsGraphicsStream::last_frame`] from each composition object's force flag and
/// remembers the composition number once.
fn read_pcs(stream: &mut TsGraphicsStream, buffer: &mut TsStreamBuffer) {
    let _ = buffer.read_bits2(16, false); // Segment Size
    if stream.base.is_initialized {
        let _ = buffer.read_bits2(16, false);
        let _ = buffer.read_bits2(16, false);
    } else {
        stream.width = i32::from(buffer.read_bits2(16, false));
        stream.height = i32::from(buffer.read_bits2(16, false));
        stream.base.is_initialized = true;
    }

    let _ = buffer.read_byte(false);
    let composition_number = i32::from(buffer.read_bits2(16, false));
    let _ = buffer.read_byte(false); // Composition State
    let _ = buffer.read_bits2(16, false);
    let num_composition_objects = buffer.read_byte(false);

    for _ in 0..num_composition_objects {
        let _ = buffer.read_bits2(16, false); // Object ID
        let _ = buffer.read_byte(false); // Window ID
        let flags = buffer.read_byte(false); // Object flags: crop 0x80, force 0x40
        let _ = buffer.read_bits2(16, false); // Object Horizontal Position
        let _ = buffer.read_bits2(16, false); // Object Vertical Position
        // The four 16-bit cropping fields are present only when the crop flag
        // (0x80) is set — the spec-conformant variable layout. Classic BDInfo
        // reads them unconditionally, which over-advances 8 bytes on an uncropped
        // multi-object composition and misparses every later object's forced
        // flag; bdinfo-rs reads spec-true.
        if (flags & 0x80) == 0x80 {
            let _ = buffer.read_bits2(16, false); // Object Cropping Horizontal Position
            let _ = buffer.read_bits2(16, false); // Object Cropping Vertical Position
            let _ = buffer.read_bits2(16, false); // Object Cropping Width
            let _ = buffer.read_bits2(16, false); // Object Cropping Height
        }

        let frame = Frame { started: true, forced: (flags & 0x40) == 0x40, finished: false };
        stream.last_frame = frame;
        stream.caption_ids.entry(composition_number).or_insert(frame);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::{any, prop_assert, proptest};

    use super::{Frame, read_ods, read_pcs, scan};
    use crate::bitstream::TsStreamBuffer;
    use crate::stream::{TsGraphicsStream, TsStreamType};

    /// A rewound buffer holding `data`.
    fn buf(data: &[u8]) -> TsStreamBuffer {
        let mut b = TsStreamBuffer::new();
        b.add(data, 0, data.len());
        b.begin_read();
        b
    }

    /// Runs [`scan`] over `data` against `stream`, returning the resulting tag.
    fn run(stream: &mut TsGraphicsStream, data: &[u8]) -> Option<String> {
        let mut b = buf(data);
        let mut tag = None;
        scan(stream, &mut b, &mut tag);
        tag
    }

    /// A presentation-composition segment (leading `0x16`) with `width`/`height`, a
    /// composition number, and one entry per object's flag byte (crop bit `0x80`,
    /// force bit `0x40`). Each object emits its four 16-bit cropping fields **only**
    /// when the crop bit is set — the spec-conformant variable-length layout
    /// (8 bytes uncropped, 16 bytes cropped) the scanner follows.
    fn pcs(width: u16, height: u16, comp_number: u16, objects: &[u8]) -> Vec<u8> {
        let mut v = vec![0x16]; // segment type
        v.extend_from_slice(&[0, 0]); // segment size
        v.extend_from_slice(&width.to_be_bytes());
        v.extend_from_slice(&height.to_be_bytes());
        v.push(0); // frame-rate byte
        v.extend_from_slice(&comp_number.to_be_bytes());
        v.push(0); // composition state
        v.extend_from_slice(&[0, 0]); // palette flag / id
        v.push(u8::try_from(objects.len()).unwrap()); // number of composition objects
        for &flags in objects {
            v.extend_from_slice(&[0, 0]); // object id
            v.push(0); // window id
            v.push(flags); // flags: crop 0x80, force 0x40
            v.extend_from_slice(&[0, 0]); // object horizontal position
            v.extend_from_slice(&[0, 0]); // object vertical position
            if (flags & 0x80) == 0x80 {
                v.extend_from_slice(&[0_u8; 8]); // four 16-bit cropping fields
            }
        }
        v
    }

    /// An object-definition segment (leading `0x15`) — type + segment size + object id.
    fn ods() -> Vec<u8> {
        vec![0x15, 0, 0, 0, 0]
    }

    #[test]
    fn pcs_records_resolution_once_and_seeds_the_frame() {
        let mut s = TsGraphicsStream::default();
        // First PCS: not initialised → width/height recorded, frame seeded.
        run(&mut s, &pcs(1920, 1080, 7, &[0x00]));
        assert_eq!((s.width, s.height), (1920, 1080));
        assert!(s.base.is_initialized);
        assert!(s.base.is_vbr);
        assert_eq!(s.last_frame, Frame { started: true, forced: false, finished: false });
        assert_eq!(s.caption_ids.len(), 1);
        assert_eq!(s.caption_ids.get(&7).copied(), Some(s.last_frame));

        // Second PCS: already initialised → width/height NOT overwritten, but a new
        // composition number is remembered and the frame re-seeded (forced this time).
        run(&mut s, &pcs(1280, 720, 9, &[0x40]));
        assert_eq!((s.width, s.height), (1920, 1080)); // unchanged
        assert_eq!(s.last_frame, Frame { started: true, forced: true, finished: false });
        assert_eq!(s.caption_ids.len(), 2);
        assert!(s.caption_ids.contains_key(&9));
    }

    #[test]
    fn pcs_with_no_objects_leaves_frame_and_captions_untouched() {
        let mut s = TsGraphicsStream {
            last_frame: Frame { started: true, forced: true, finished: true },
            ..TsGraphicsStream::default()
        };
        run(&mut s, &pcs(1920, 1080, 1, &[]));
        // Resolution still recorded, but the (empty) object loop touched nothing.
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(s.last_frame, Frame { started: true, forced: true, finished: true });
        assert!(s.caption_ids.is_empty());
    }

    #[test]
    fn pcs_dedups_same_composition_number_and_keeps_the_first_frame() {
        let mut s = TsGraphicsStream::default();
        // Two objects share the one composition number: the first (forced) is the one
        // kept in caption_ids; last_frame tracks the last (non-forced) object.
        run(&mut s, &pcs(1920, 1080, 5, &[0x40, 0x00]));
        assert_eq!(s.caption_ids.len(), 1);
        assert_eq!(s.caption_ids.get(&5).map(|f| f.forced), Some(true)); // first kept
        assert!(!s.last_frame.forced); // last object wins last_frame
        assert!(s.last_frame.started);
    }

    #[test]
    fn multi_object_uncropped_pcs_reads_each_forced_flag_at_the_right_offset() {
        // Two uncropped composition objects (crop bit clear → 8 bytes each): the
        // first not forced, the second forced. The scanner must advance exactly
        // 8 bytes per uncropped object so the second object's force flag lands at
        // the right offset. An unconditional 16-byte stride over-reads the
        // first object and misparses the second's forced flag (often as 0 past
        // the segment), miscounting the caption. `last_frame` tracks the last
        // object → forced.
        let mut s = TsGraphicsStream::default();
        run(&mut s, &pcs(1920, 1080, 1, &[0x00, 0x40]));
        assert!(s.last_frame.forced);
        let tag = run(&mut s, &ods());
        assert_eq!(tag.as_deref(), Some("F"));
        assert_eq!((s.captions, s.forced_captions), (0, 1));
    }

    #[test]
    fn cropped_object_consumes_its_crop_fields_before_the_next_object() {
        // A cropped first object (crop bit 0x80 → 16 bytes) followed by an
        // uncropped forced object (0x40 → 8 bytes). The scanner must consume the
        // first object's four 16-bit crop fields so the second object aligns and
        // reads its force flag as set; skipping the crop reads slides the second
        // object onto the crop bytes (all zero here) and loses the forced flag.
        let mut s = TsGraphicsStream::default();
        run(&mut s, &pcs(1920, 1080, 1, &[0x80, 0x40]));
        assert!(s.last_frame.forced); // last (uncropped, forced) object
        assert_eq!(s.caption_ids.get(&1).map(|f| f.forced), Some(false)); // first object not forced
    }

    #[test]
    fn ods_counts_a_normal_caption_and_tags_n() {
        let mut s = TsGraphicsStream::default();
        run(&mut s, &pcs(1920, 1080, 1, &[0x00])); // seed a non-forced open frame
        let tag = run(&mut s, &ods());
        assert_eq!(tag.as_deref(), Some("N"));
        assert_eq!((s.captions, s.forced_captions), (1, 0));
        // A second ODS on the still-open frame counts again.
        let tag = run(&mut s, &ods());
        assert_eq!(tag.as_deref(), Some("N"));
        assert_eq!((s.captions, s.forced_captions), (2, 0));
    }

    #[test]
    fn ods_counts_a_forced_caption_and_tags_f() {
        let mut s = TsGraphicsStream::default();
        run(&mut s, &pcs(1920, 1080, 1, &[0x40])); // seed a forced open frame
        let tag = run(&mut s, &ods());
        assert_eq!(tag.as_deref(), Some("F"));
        assert_eq!((s.captions, s.forced_captions), (0, 1));
    }

    #[test]
    fn end_marker_finishes_the_frame_and_later_ods_is_not_counted() {
        let mut s = TsGraphicsStream::default();
        run(&mut s, &pcs(1920, 1080, 1, &[0x00]));
        // End-of-display-set marker closes the frame.
        run(&mut s, &[0x80]);
        assert!(s.last_frame.finished);
        // An ODS after the frame finished returns no tag and counts nothing.
        let tag = run(&mut s, &ods());
        assert_eq!(tag, None);
        assert_eq!((s.captions, s.forced_captions), (0, 0));
    }

    #[test]
    fn unknown_segment_only_sets_vbr() {
        let mut s = TsGraphicsStream::default();
        let before = s.clone();
        let tag = run(&mut s, &[0x00, 0xFF, 0xFF]);
        assert_eq!(tag, None);
        // Nothing changed except is_vbr (which the default already had true).
        assert_eq!(s.width, before.width);
        assert_eq!(s.captions, before.captions);
        assert_eq!(s.last_frame, before.last_frame);
        assert!(s.caption_ids.is_empty());
        assert!(s.base.is_vbr);
    }

    #[test]
    fn force_flag_masks_only_bit_0x40() {
        // The cropped-flag byte is forced iff bit 0x40 is set: 0x40 → forced, any byte
        // without it → not. 0xBF (every bit but 0x40) stays non-forced; 0xFF forced.
        let object = |flag: u8| {
            let mut v = vec![0x16]; // segment type
            v.extend_from_slice(&[0_u8; 12]); // segment size .. palette (12 header bytes)
            v.push(1); // one composition object
            v.extend_from_slice(&[0, 0, 0]); // object id + window id
            v.push(flag); // cropped flag
            v.extend_from_slice(&[0_u8; 12]); // position/crop fields
            let mut s = TsGraphicsStream::default();
            run(&mut s, &v);
            s.last_frame.forced
        };
        assert!(object(0x40));
        assert!(object(0xFF));
        assert!(!object(0x00));
        assert!(!object(0xBF));
    }

    #[test]
    fn read_ods_and_read_pcs_are_reachable_directly() {
        // Exercise the private helpers directly (no leading segment-type byte), so a
        // truncated buffer drives the bounds-checked reads without panicking.
        let mut s = TsGraphicsStream::default();
        let mut b = buf(&[0, 0, 0, 0]); // size + object id, frame open by default
        let tag = read_ods(&mut s, &mut b);
        assert_eq!(tag.as_deref(), Some("N"));
        assert_eq!(s.captions, 1);

        let mut s = TsGraphicsStream::default();
        let mut b = buf(&[]); // fully truncated PCS → all reads return defaults
        read_pcs(&mut s, &mut b);
        assert_eq!((s.width, s.height), (0, 0));
        assert!(s.base.is_initialized);
    }

    #[test]
    fn description_after_a_scanned_caption_run() {
        // A small end-to-end run: resolution + one forced + two normal captions, then
        // the graphics description the report would emit.
        let mut s = TsGraphicsStream::default();
        s.base.stream_type = TsStreamType::PresentationGraphics;
        run(&mut s, &pcs(1920, 1080, 1, &[0x40]));
        run(&mut s, &ods()); // forced
        run(&mut s, &[0x80]);
        run(&mut s, &pcs(0, 0, 2, &[0x00]));
        run(&mut s, &ods()); // normal
        run(&mut s, &pcs(0, 0, 3, &[0x00]));
        run(&mut s, &ods()); // normal
        assert_eq!((s.captions, s.forced_captions), (2, 1));
        assert_eq!(s.description(), "1920x1080 / 2 Captions (1 Forced Caption)");
    }

    proptest! {
        #[test]
        fn scan_never_panics_on_arbitrary_bytes(data in any::<Vec<u8>>()) {
            let mut s = TsGraphicsStream::default();
            s.base.stream_type = TsStreamType::PresentationGraphics;
            let mut b = buf(&data);
            let mut tag = None;
            scan(&mut s, &mut b, &mut tag);
            // The scan always flags VBR and never panics, whatever the bytes.
            prop_assert!(s.base.is_vbr);
        }
    }
}
