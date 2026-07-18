//! Tier A — recovering the real disc label when a folder scan lands on a
//! nameless Windows drive root.
//!
//! A folder scan labels the disc after its root directory's name. That works
//! everywhere a disc is mounted at a *named* path — an extracted folder, or the
//! label-named mount points macOS (`/Volumes/MY_DISC`) and Linux
//! (`/media/<user>/MY_DISC`) use — but a physical disc or mounted image at a
//! bare Windows drive root (`J:\`) has no such name: the disc root resolves to
//! the drive root, whose path carries no final component, so the label degrades
//! to the drive-root string itself (`"J:\"`). That is wrong in the report's
//! `Disc Label:` line, the info box, and the `BDINFO.<label>.txt` save name
//! (whose *characters* `paths::report_file_name` already sanitizes — this
//! module fixes the label's SOURCE).
//!
//! [`resolve_folder_label`] replaces a nameless-drive-root label with the real
//! UDF volume label — read off the raw volume device (`\\.\J:`) through the
//! same `UdfSource` core the `.iso` path uses (no doc link: the import is
//! Windows-only, so the name only resolves there), the identical string Windows
//! Explorer and classic `BDInfo` show — falling back to the drive letter when
//! that read is unavailable. The scan seam applies it to every folder scan
//! (`scan::open`), so the structural listing and the measured report agree.
//!
//! Replicates the CLI's `volume` module (`crates/bdinfo-rs/src/volume.rs`),
//! which must resolve the same label for the same disc — the crates share no
//! code, so keep the two in lock-step.

#[cfg(any(windows, test))]
use std::io::{self, Read, Seek, SeekFrom};

#[cfg(any(windows, test))]
use bdinfo_rs_core::vfs::ReadSeek;
#[cfg(windows)]
use bdinfo_rs_core::vfs::udf::source::{IsoReader, UdfSource};

/// The uppercase drive letter of a bare Windows drive-root label — `"J:\"`,
/// `"k:/"`, or `"k:"` all yield `'J'`/`'K'` — or `None` for any real name.
///
/// A folder scan's label is its root directory's name; a disc mounted at a
/// nameless Windows drive root degrades to exactly one of these forms (the root
/// path has no final component), which is the signal to recover the genuine
/// label. Pure string matching, so it is identical on every platform (`std` only
/// parses a path `Prefix` on Windows).
fn drive_root_letter(label: &str) -> Option<char> {
    let core = label.strip_suffix(['/', '\\']).unwrap_or(label);
    let mut chars = core.chars();
    let letter = chars.next()?;
    (letter.is_ascii_alphabetic() && chars.next() == Some(':') && chars.next().is_none())
        .then(|| letter.to_ascii_uppercase())
}

/// The report-worthy disc label for a folder scan.
///
/// `scanned` unchanged for a real name, or — when `scanned` is a bare Windows
/// drive root — the genuine UDF volume label read off the raw volume device,
/// falling back to the drive letter when that read is unavailable (no disc,
/// unreadable, or a non-Windows build). This is what recovers `MY_DISC` from
/// a `J:\` scan.
#[must_use]
pub fn resolve_folder_label(scanned: &str) -> String {
    resolve_with(scanned, real_volume_label)
}

/// The pure core of [`resolve_folder_label`], with the raw-device read injected
/// as `read` so the decision logic is exercised without a real device.
fn resolve_with(scanned: &str, read: impl FnOnce(char) -> Option<String>) -> String {
    drive_root_letter(scanned).map_or_else(
        || scanned.to_owned(),
        |letter| read(letter).unwrap_or_else(|| letter.to_string()),
    )
}

/// How many times to attempt the raw-device read — an optical drive can fail the
/// first read right after spin-up, so a couple of quick reattempts recover the
/// genuine label instead of silently falling back to the drive letter.
#[cfg(windows)]
const RAW_READ_ATTEMPTS: usize = 3;

/// The pause before each raw-read retry, giving the drive a moment to settle.
#[cfg(windows)]
const RAW_READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// Reads the genuine UDF volume label off the raw volume device for drive
/// `letter` (Windows `\\.\J:`), or `None` when it cannot — the device is absent,
/// unreadable, holds no UDF volume, or reports an empty label.
///
/// Environment-bound: opening a real volume device cannot run in a deterministic
/// test, so this is excluded from coverage and mutation. The machinery it drives
/// — [`AlignedReader`], [`UdfSource::read_label`], and the decision in
/// [`resolve_with`] — is unit-tested directly, and the live read is validated
/// end-to-end against a physical disc.
#[cfg(windows)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn real_volume_label(letter: char) -> Option<String> {
    let iso = RawVolumeIso { device: format!(r"\\.\{letter}:") };
    for attempt in 0..RAW_READ_ATTEMPTS {
        // Pause before each retry (never the first) so a spun-down drive gets a
        // moment to settle between reads.
        if attempt != 0 {
            std::thread::sleep(RAW_READ_RETRY_DELAY);
        }
        // A successful read is definitive: use the label, or fall back to the
        // drive letter (via `None`) for a genuinely empty one.
        if let Ok(label) = UdfSource::read_label(&iso) {
            return (!label.is_empty()).then_some(label);
        }
    }
    None
}

/// Off Windows a mounted disc already carries its label in the mount-point
/// directory name (`/Volumes/LABEL`, `/media/<user>/LABEL`), so a folder scan
/// never reaches a nameless root and there is no raw volume device to read. A
/// platform constant, excluded from coverage like its Windows sibling.
#[cfg(not(windows))]
#[cfg_attr(coverage_nightly, coverage(off))]
const fn real_volume_label(_letter: char) -> Option<String> {
    None
}

/// The raw optical/UDF logical sector size. Reads on a raw volume device must
/// start and end on a multiple of it.
#[cfg(any(windows, test))]
const SECTOR: u64 = 2048;

/// [`SECTOR`] as a `usize`, for sizing the one-sector cache buffer.
#[cfg(any(windows, test))]
const SECTOR_USIZE: usize = 2048;

/// A raw Windows volume device (`\\.\J:`) presented as an [`IsoReader`]. Reads
/// are served through an [`AlignedReader`] so the UDF parser's byte-granular
/// reads land as the sector-aligned reads the device requires.
#[cfg(windows)]
#[derive(Debug)]
struct RawVolumeIso {
    /// The device path, e.g. `\\.\J:`.
    device: String,
}

#[cfg(windows)]
impl IsoReader for RawVolumeIso {
    // Env-bound: opens a real volume device. See `real_volume_label`.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(AlignedReader::new(Box::new(std::fs::File::open(&self.device)?))))
    }
}

/// Presents a byte-granular `Read + Seek` over an inner reader that only permits
/// sector-aligned reads, by always reading whole sectors into a one-sector cache
/// and serving the requested sub-range. A raw Windows volume handle rejects an
/// unaligned read (`ERROR_INVALID_PARAMETER`); this adapter is what lets the
/// UDF core read it unchanged.
///
/// The inner reader is boxed (not a generic parameter) so the whole adapter is
/// one non-generic type: a single instantiation the tests cover in full, driving
/// both the real device and the test cursors through identical code.
#[cfg(any(windows, test))]
struct AlignedReader {
    /// The sector-aligned inner reader (a raw volume device in use; a cursor in
    /// tests).
    inner: Box<dyn ReadSeek>,
    /// The logical byte position presented to the caller.
    pos: u64,
    /// The one-sector cache — always `SECTOR` bytes; only `valid` of them hold
    /// the current sector's data.
    buf: Vec<u8>,
    /// How many leading bytes of `buf` are the cached sector (below `SECTOR`
    /// only for a short final sector past the device end).
    valid: usize,
    /// Which sector index `buf` currently holds, or `None` before the first
    /// fill.
    cached: Option<u64>,
}

#[cfg(any(windows, test))]
impl AlignedReader {
    /// Wraps `inner`, positioned at its start with an empty cache.
    fn new(inner: Box<dyn ReadSeek>) -> Self {
        Self { inner, pos: 0, buf: vec![0; SECTOR_USIZE], valid: 0, cached: None }
    }

    /// Ensures `buf` holds sector `sector`, reading it aligned from `inner`. A
    /// no-op when it is already cached.
    fn fill(&mut self, sector: u64) -> io::Result<()> {
        if self.cached == Some(sector) {
            return Ok(());
        }
        // `sector` is a floor of `pos / SECTOR`, so `sector * SECTOR <= pos`
        // (<= u64::MAX) — the aligned offset can never overflow.
        let offset = sector.wrapping_mul(SECTOR);
        self.inner.seek(SeekFrom::Start(offset))?;
        // ONE read of the whole fixed-size buffer: every device read is then
        // sector-aligned in both offset and length, the raw volume handle's
        // requirement. A raw device (and the test cursor) returns the full
        // sector, or fewer bytes only at the device end — never a mid-sector
        // partial that would need a length-unaligned follow-up read.
        self.valid = self.inner.read(self.buf.as_mut_slice())?;
        self.cached = Some(sector);
        Ok(())
    }
}

#[cfg(any(windows, test))]
impl Read for AlignedReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let sector = self.pos.wrapping_div(SECTOR);
        let within = self.pos.wrapping_rem(SECTOR);
        self.fill(sector)?;
        // `within < SECTOR (2048)`, so it is an exact `usize` on every target.
        #[expect(
            clippy::as_conversions,
            clippy::cast_possible_truncation,
            reason = "within < SECTOR (2048) is an exact usize"
        )]
        let within = within as usize;
        let Some(src) = self.buf.get(within..self.valid) else {
            // `within` is past the bytes read for this sector — the device end.
            return Ok(0);
        };
        let n = src.len().min(out.len());
        for (dst, &byte) in out.iter_mut().zip(src) {
            *dst = byte;
        }
        // `n <= SECTOR (2048)`, so advancing the position cannot overflow a u64.
        #[expect(clippy::as_conversions, reason = "n <= SECTOR (2048) is an exact u64")]
        let advance = n as u64;
        self.pos = self.pos.wrapping_add(advance);
        Ok(n)
    }
}

#[cfg(any(windows, test))]
impl Seek for AlignedReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(delta) => add_signed(self.pos, delta)?,
            SeekFrom::End(delta) => add_signed(self.inner.seek(SeekFrom::End(0))?, delta)?,
        };
        self.pos = target;
        Ok(target)
    }
}

/// Adds a signed `delta` to an unsigned `base`, erroring on under/overflow — the
/// seek-target arithmetic. Both directions and both bounds are reachable, so
/// every branch is testable.
#[cfg(any(windows, test))]
fn add_signed(base: u64, delta: i64) -> io::Result<u64> {
    let target = if delta < 0 {
        base.checked_sub(delta.unsigned_abs())
    } else {
        base.checked_add(delta.unsigned_abs())
    };
    target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek out of range"))
}

#[cfg(test)]
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    reason = "controlled, in-range sector casts and slicing in test fixtures"
)]
mod tests {
    use std::io::{Cursor, Read as _, Seek as _, SeekFrom};

    use super::{
        AlignedReader, SECTOR, add_signed, drive_root_letter, resolve_folder_label, resolve_with,
    };

    #[test]
    fn drive_root_letter_matches_every_bare_drive_root_spelling() {
        assert_eq!(drive_root_letter(r"J:\"), Some('J'));
        assert_eq!(drive_root_letter("k:/"), Some('K'));
        assert_eq!(drive_root_letter("k:"), Some('K'));
        assert_eq!(drive_root_letter("Z:/"), Some('Z'));
    }

    #[test]
    fn drive_root_letter_rejects_real_names() {
        assert_eq!(drive_root_letter("BigBuckBunny"), None);
        assert_eq!(drive_root_letter(""), None);
        assert_eq!(drive_root_letter("C:temp"), None); // a drive-relative path, not a root
        assert_eq!(drive_root_letter("1:/"), None); // not a letter
        assert_eq!(drive_root_letter(r"J:\sub"), None);
    }

    #[test]
    fn resolve_with_consults_the_reader_only_for_a_drive_root() {
        // One reader for both shapes: consulted for a drive root, ignored for a
        // real name — so its label wins the first but never the second.
        let read = |letter: char| Some(format!("VOL{letter}"));
        assert_eq!(resolve_with(r"J:\", read), "VOLJ");
        assert_eq!(resolve_with("BigBuckBunny", read), "BigBuckBunny");
    }

    #[test]
    fn resolve_with_falls_back_to_the_drive_letter_when_the_read_fails() {
        assert_eq!(resolve_with(r"J:\", |_| None), "J");
        assert_eq!(resolve_with("k:/", |_| None), "K");
    }

    #[test]
    fn resolve_folder_label_leaves_a_real_name_untouched() {
        // A real folder name never touches the device — exercises the public
        // wrapper (the drive-root path is validated end-to-end on real hardware).
        assert_eq!(resolve_folder_label("BigBuckBunny"), "BigBuckBunny");
    }

    /// An inner reader that fails any read whose current position or length is
    /// not sector-aligned — proving [`AlignedReader`] only ever issues aligned
    /// reads to the device.
    struct AlignmentGuard {
        cursor: Cursor<Vec<u8>>,
    }

    impl std::io::Read for AlignmentGuard {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let pos = self.cursor.position();
            let len = buf.len() as u64;
            assert_eq!(pos % SECTOR, 0, "unaligned read offset {pos}");
            assert_eq!(len % SECTOR, 0, "unaligned read length {len}");
            self.cursor.read(buf)
        }
    }

    impl std::io::Seek for AlignmentGuard {
        fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(from)
        }
    }

    /// Distinct byte per offset so a wrongly served range is caught.
    fn ramp(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn aligned(bytes: Vec<u8>) -> AlignedReader {
        AlignedReader::new(Box::new(AlignmentGuard { cursor: Cursor::new(bytes) }))
    }

    #[test]
    fn reads_a_sub_sector_range_only_issuing_aligned_device_reads() {
        let data = ramp(3 * SECTOR as usize);
        let mut reader = aligned(data.clone());
        reader.seek(SeekFrom::Start(100)).expect("seek");
        let mut got = [0_u8; 50];
        reader.read_exact(&mut got).expect("read");
        assert_eq!(got.as_slice(), &data[100..150]);
    }

    #[test]
    fn reads_spanning_a_sector_boundary_reassemble_correctly() {
        let data = ramp(3 * SECTOR as usize);
        let mut reader = aligned(data.clone());
        // Start just before a boundary and read across it.
        let start = SECTOR as usize - 20;
        reader.seek(SeekFrom::Start(start as u64)).expect("seek");
        let mut got = vec![0_u8; 100];
        reader.read_exact(&mut got).expect("read");
        assert_eq!(got, data[start..start + 100]);
    }

    #[test]
    fn a_cached_sector_is_reused_across_reads() {
        let data = ramp(2 * SECTOR as usize);
        let mut reader = aligned(data.clone());
        let mut a = [0_u8; 10];
        let mut b = [0_u8; 10];
        reader.seek(SeekFrom::Start(0)).expect("seek");
        reader.read_exact(&mut a).expect("read a");
        reader.read_exact(&mut b).expect("read b"); // same sector — served from cache
        assert_eq!(a, data[0..10]);
        assert_eq!(b, data[10..20]);
    }

    #[test]
    fn reading_at_or_past_the_end_yields_zero() {
        let data = ramp(SECTOR as usize + 100); // 1 full sector + a short tail
        let mut reader = aligned(data.clone());
        reader.seek(SeekFrom::Start(data.len() as u64)).expect("seek to end");
        let mut got = [0_u8; 16];
        // At the exact end of the short final sector: `within == valid`, an empty
        // in-range slice.
        assert_eq!(reader.read(&mut got).expect("read at end"), 0);
        // Past the valid tail within that sector: `within > valid`, an out-of-range
        // slice (the `None` branch).
        reader.seek(SeekFrom::Start(SECTOR + 150)).expect("seek past tail");
        assert_eq!(reader.read(&mut got).expect("read past tail"), 0);
    }

    #[test]
    fn an_empty_output_buffer_reads_nothing() {
        let mut reader = aligned(ramp(SECTOR as usize));
        assert_eq!(reader.read(&mut []).expect("empty read"), 0);
    }

    #[test]
    fn seek_current_and_end_land_on_the_right_bytes() {
        let data = ramp(3 * SECTOR as usize);
        let mut reader = aligned(data.clone());
        reader.seek(SeekFrom::Start(200)).expect("seek start");
        reader.seek(SeekFrom::Current(50)).expect("seek current +");
        let mut got = [0_u8; 4];
        reader.read_exact(&mut got).expect("read");
        assert_eq!(got.as_slice(), &data[250..254]);
        let end = reader.seek(SeekFrom::End(-10)).expect("seek end -10");
        assert_eq!(end, data.len() as u64 - 10);
        reader.seek(SeekFrom::Current(-4)).expect("seek current -");
        let mut tail = [0_u8; 4];
        reader.read_exact(&mut tail).expect("read tail");
        assert_eq!(tail.as_slice(), &data[data.len() - 14..data.len() - 10]);
    }

    #[test]
    fn add_signed_handles_both_directions_and_reports_out_of_range() {
        assert_eq!(add_signed(100, 20).expect("add"), 120);
        assert_eq!(add_signed(100, -20).expect("sub"), 80);
        assert!(add_signed(10, -20).is_err()); // underflow below 0
        assert!(add_signed(u64::MAX, 1).is_err()); // overflow past u64::MAX
    }

    #[test]
    fn a_seek_before_the_start_errors() {
        let mut reader = aligned(ramp(SECTOR as usize));
        assert!(reader.seek(SeekFrom::Current(-1)).is_err());
    }

    #[test]
    fn an_end_relative_seek_before_the_start_errors() {
        // The inner End seek succeeds; the signed add underflows below zero.
        let mut reader = aligned(ramp(2 * SECTOR as usize));
        assert!(reader.seek(SeekFrom::End(i64::MIN)).is_err());
    }

    /// An inner reader whose seek and/or read fail on demand, driving the
    /// adapter's IO-error propagation.
    struct FailingInner {
        fail_seek: bool,
        fail_read: bool,
    }

    impl std::io::Read for FailingInner {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            if self.fail_read { Err(std::io::Error::other("read boom")) } else { Ok(0) }
        }
    }

    impl std::io::Seek for FailingInner {
        fn seek(&mut self, _from: SeekFrom) -> std::io::Result<u64> {
            if self.fail_seek { Err(std::io::Error::other("seek boom")) } else { Ok(0) }
        }
    }

    fn failing(fail_seek: bool, fail_read: bool) -> AlignedReader {
        AlignedReader::new(Box::new(FailingInner { fail_seek, fail_read }))
    }

    #[test]
    fn a_failing_inner_seek_propagates_through_read() {
        // fill's seek fails → the read propagates it.
        assert!(failing(true, false).read(&mut [0_u8; 4]).is_err());
    }

    #[test]
    fn a_failing_inner_read_propagates_through_read() {
        // fill's seek succeeds, its read fails → the read propagates it.
        assert!(failing(false, true).read(&mut [0_u8; 4]).is_err());
    }

    #[test]
    fn a_failing_inner_end_seek_propagates() {
        assert!(failing(true, false).seek(SeekFrom::End(0)).is_err());
    }

    #[test]
    fn a_non_failing_inner_serves_an_empty_device() {
        // Seek and read both succeed but yield zero bytes → a read of 0.
        assert_eq!(failing(false, false).read(&mut [0_u8; 4]).expect("read"), 0);
    }

    mod prop {
        use proptest::prelude::*;

        proptest! {
            // The resolution contract over arbitrary labels: a drive-root
            // label always resolves to the injected read's answer (or the
            // bare letter), and any other label passes through byte-exact —
            // resolution can never corrupt a real name.
            #[test]
            fn resolution_is_identity_off_the_drive_root(label in any::<String>()) {
                let resolved = super::super::resolve_with(&label, |_| None);
                if let Some(letter) = super::super::drive_root_letter(&label) {
                    prop_assert_eq!(resolved, letter.to_string());
                } else {
                    prop_assert_eq!(resolved, label);
                }
            }
        }
    }
}
