# Differences from BDInfo

bdinfo-rs follows the classic BDInfo report format byte-for-byte, with a handful of
**deliberate** exceptions: where the original BDInfo is provably wrong against the codec
specification / FFmpeg, bdinfo-rs emits the correct value instead. Each divergence is
verified bit-by-bit and stays within BDInfo's existing report vocabulary — bdinfo-rs never
invents a report string BDInfo wouldn't emit.

This page shows the concrete before/after for every divergence and flags which ones you'll
actually see on a normal disc.

> **Reading the diffs.** `-` is what the original BDInfo prints; `+` is what bdinfo-rs
> prints. The `+` lines are pinned verbatim by tests in the codec scanners. The `-` lines
> are reconstructed from the spec and from BDInfo's behavior (there are no captured BDInfo
> fixtures in this repo), so treat them as faithful reconstructions, not byte-captures.
> Surrounding values (language, resolution, fps) are illustrative; only the highlighted
> field changes.

## At a glance

| Divergence | Changes the report? | When you'll see it |
|---|---|---|
| [DTS core bit rate (1509 → 1536 kbps)](#dts-core-bit-rate) | **Yes** — bitrate number | **Common** — most DTS-HD MA cores and full-rate DTS tracks |
| [DTS:X IMAX detection](#dtsx-imax-detection) | **Yes** — codec name | DTS:X IMAX tracks |
| [HDR10+ without a mastering display](#hdr10-without-a-mastering-display) | **Yes** — HDR token | HDR10+ titles whose stream carries no static mastering-display SEI |
| [MaxCLL unit label (`cd / m2` → `cd/m2`)](#maxcll-unit-label) | **Yes** — unit spelling | **Common** — every HDR title with a content-light-level SEI |
| [AVC High 4:4:4 (profile 244)](#avc-high-444-profile-244) | **Yes** — profile token | Rare — Blu-ray video is almost always 4:2:0 |
| [PGS forced-caption counts](#pgs-forced-caption-counts) | **Yes** — caption tally | Discs with multi-object subtitle compositions |
| [E-AC-3 reduced data-rate](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only on non-BD input | Reduced-rate E-AC-3 (24 / 22.05 / 16 kHz) isn't used on Blu-ray |
| [AC-3 low-sample-rate shift](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only on non-BD input | Legacy `bsid` 9/10; conforming Blu-ray AC-3 is always `bsid` 8 |
| [HEVC `profile_idc` recovery](#correctness-fixes-with-no-effect-on-a-normal-disc) | Edge case only | Malformed headers with `general_profile_idc == 0` |
| [VC-1 interlaced-field picture type](#correctness-fixes-with-no-effect-on-a-normal-disc) | **No** — internal only | Never — the picture tag is counted, never printed |
| [PAT `table_id` validation](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only on malformed input | Never — no conforming disc carries a wrong PAT table id |
| [AACS-encrypted discs are refused](#aacs-encrypted-discs-are-refused) | **Yes** — no report at all | Discs whose stream content is still encrypted |
| [Damaged-media read granularity](#damaged-media-read-granularity) | Only on damaged media — partial scans retain more | Discs with unreadable spans |
| [Read errors: folder input vs `.iso` input](#read-errors-folder-input-vs-iso-input) | Only on damaged media — the same disc measures differently through the two inputs | Damaged discs and damaged disc images |

---

## Visible on a normal disc

### DTS core bit rate

DTS core rate code 24 is **1536 kbps** per the spec (ETSI TS 102 114) and FFmpeg; BDInfo
historically printed **1509 kbps**. This is the most frequently seen divergence — the same
number appears standalone for a full-rate DTS track *and* embedded in the `(DTS Core: …)`
block of essentially every DTS-HD Master Audio track.

```diff
- Audio: English / DTS Audio / 5.1 / 48 kHz /  1509 kbps / 16-bit
+ Audio: English / DTS Audio / 5.1 / 48 kHz /  1536 kbps / 16-bit
```

Embedded inside a DTS-HD MA track:

```diff
- Audio: English / DTS-HD Master Audio / 5.1 / 48 kHz / 16-bit (DTS Core: 5.1 / 48 kHz /  1509 kbps / 16-bit)
+ Audio: English / DTS-HD Master Audio / 5.1 / 48 kHz / 16-bit (DTS Core: 5.1 / 48 kHz /  1536 kbps / 16-bit)
```

<sub>Source: `crates/bdinfo-rs-core/src/codec/dts.rs` (bit-rate table).</sub>

### DTS:X IMAX detection

DTS:X IMAX tracks carry the IMAX extension sync word (`0xF14000D0`) instead of the legacy
DTS:X word BDInfo knows. BDInfo therefore labels them as plain DTS-HD Master Audio;
bdinfo-rs recognizes the IMAX word and renders `DTS:X Master Audio`. Only the codec-name
field changes — the channel/rate/bit-depth numbers are identical.

```diff
- Audio: English / DTS-HD Master Audio / 5.1 / 48 kHz / 16-bit (DTS Core: 5.1 / 48 kHz /  1536 kbps / 16-bit)
+ Audio: English / DTS:X Master Audio / 5.1 / 48 kHz / 16-bit (DTS Core: 5.1 / 48 kHz /  1536 kbps / 16-bit)
```

<sub>Short names map the same way: `DTS-HD MA` → `DTS:X MA`, `DTS-HD HR` → `DTS:X HR`.
Source: `crates/bdinfo-rs-core/src/codec/dts_hd.rs`.</sub>

### HDR10+ without a mastering display

The BDInfo lineage emits an HDR label only when a static mastering-display SEI is present.
bdinfo-rs also recognizes HDR10+ from the ST 2094-40 (T.35) dynamic-metadata SEI alone, so
a valid HDR10+ stream that carries no mastering display still gets its `HDR10+` token
instead of dropping the descriptor entirely.

```diff
- Video: MPEG-H HEVC Video / … / 10 bits / Limited Range / …
+ Video: MPEG-H HEVC Video / … / 10 bits / HDR10+ / Limited Range / …
```

<sub>Only changes output on the mastering-display-absent path; with a mastering display
present, both tools already emit a label. Source: `crates/bdinfo-rs-core/src/codec/hevc.rs`
(HDR-label gate).</sub>

### MaxCLL unit label

BDInfo's HEVC scanner prints the Maximum Content Light Level unit as `cd / m2` — spaced
around the slash on that one line only, while the adjacent Maximum Frame-Average Light
Level and both mastering-display luminance values use `cd/m2`. The spaced form is a typo
in the original source (one string literal out of step with its neighbors); bdinfo-rs
emits the `cd/m2` spelling BDInfo itself uses everywhere else.

```diff
- Video: MPEG-H HEVC Video / … / Maximum Content Light Level: 992 cd / m2 / Maximum Frame-Average Light Level: 772 cd/m2
+ Video: MPEG-H HEVC Video / … / Maximum Content Light Level: 992 cd/m2 / Maximum Frame-Average Light Level: 772 cd/m2
```

<sub>Only the unit spacing changes — the value and every other field are identical.
Source: `crates/bdinfo-rs-core/src/codec/hevc.rs` (extended-format info).</sub>

### AVC High 4:4:4 (profile 244)

AVC `profile_idc` 244 is High 4:4:4 Predictive. BDInfo only mapped the equivalent legacy
code (144), so 244 fell through to `Unknown Profile`; bdinfo-rs maps both to the existing
`High 4:4:4 Profile` string. Rare in practice — Blu-ray video is almost always 4:2:0.

```diff
- Video: AVC Video / 1080p / 23.976 fps / 16:9 / Unknown Profile 4.1
+ Video: AVC Video / 1080p / 23.976 fps / 16:9 / High 4:4:4 Profile 4.1
```

<sub>Within the same fix, the CAVLC 4:4:4 code (44) and constraint-flag refinements are
deliberately left at `Unknown Profile` — BDInfo has no string for them, and inventing one
would break the locked report. Source: `crates/bdinfo-rs-core/src/codec/avc.rs`.</sub>

### PGS forced-caption counts

BDInfo reads a composition object's four cropping fields unconditionally; per the HDMV
layout (FFmpeg `pgssubdec`, libbluray `pg_decode`) they are present only when the crop
flag (`0x80`) is set. On a multi-object composition the over-read consumes the next
object's bytes and then reads past the segment end into stale buffer memory, so the
forced flag comes from leftover data — captions get randomly misreported as forced.
bdinfo-rs parses the spec layout and reports the true normal/forced split.

```diff
- Presentation Graphics           English         33.00 kbps      1920x1080 / 1546 Captions (54 Forced Captions)
+ Presentation Graphics           English         33.00 kbps      1920x1080 / 1600 Captions
```

<sub>The total is unaffected — only the split; genuinely forced captions still render with
BDInfo's existing forms. Source: `crates/bdinfo-rs-core/src/codec/pgs.rs` (PCS
composition-object parse).</sub>

---

## Correctness fixes with no effect on a normal disc

These are real spec/FFmpeg-correctness fixes, but they cannot change the report for a
conforming Blu-ray — either the triggering input never occurs on Blu-ray, or the corrected
value is never rendered.

- **E-AC-3 reduced data-rate (`fscod2`).** When the `fscod` field signals a reduced sample
  rate (24 / 22.05 / 16 kHz), bdinfo-rs halves the rate and derives the bit rate from it.
  Reduced-rate E-AC-3 is not used on Blu-ray (E-AC-3 there is 48 kHz), so this never fires
  on a real disc.
- **AC-3 low-sample-rate shift (`bsid` 9/10).** Legacy half/quarter-rate AC-3 right-shifts
  both the sample rate and the table bit rate. Conforming Blu-ray AC-3 is always `bsid` 8
  (shift 0), so the output is unchanged.
- **HEVC `profile_idc` recovery.** When a malformed header carries
  `general_profile_idc == 0`, bdinfo-rs recovers the profile from the compatibility flags
  (FFmpeg parity). This is a parse-robustness fix for non-conforming headers, not a
  documented divergence from a known BDInfo value.
- **VC-1 interlaced-field picture-type handling.** The per-frame picture type
  (`I`/`P`/`B`/`BI`) is classified more correctly (SMPTE 421M field-pair collapse), but the
  tag is only ever used as a frame-present counter — its string is never rendered. The
  report is byte-identical.
- **PAT `table_id` validation.** A PID-0 section is assembled only when its `table_id` is
  0x00 (ISO 13818-1); a wrong id abandons the section, exactly as both tools already do for
  a PMT whose `table_id` is not 0x02. BDInfo assembles the mislabeled section and adopts
  whatever PMT PID it announces. No conforming disc carries a wrong PAT table id, so the
  report is unchanged for every real disc.

---

## Deliberately different behavior

### AACS-encrypted discs are refused

AACS leaves only the first 16 bytes of every 6144-byte Aligned Unit in the clear (AACS Blu-ray
Prerecorded Book §3.10), so a demux of the rest reads ciphertext and every value it measures is
meaningless. BDInfo scans such a disc anyway and prints the resulting statistics — stream layouts,
bitrates and codec tokens describing nothing. bdinfo-rs detects the encryption during the metadata
scan and refuses to measure:

- **Command line** — one notice on stderr, then exit code 4: no picker, no scan, no report file.
  `--list` still works and exits by its own rules, its output being read from the disc's own
  database rather than from the streams.
- **Desktop app** — the disc opens, lists and browses like any other, the info box marks it
  encrypted, and the scan action stays disabled.
- **Browser package** — no policy: the disc model carries `isAacsEncrypted` and the application
  decides. The package is a library and mirrors the library layering below.

Refusing is each front-end's policy, not the analyzer's: `bdinfo-rs-core` scans an encrypted disc
when asked, which is what keeps the decision with the caller.

<sub>Source: `crates/bdinfo-rs-core/src/bdrom/disc.rs` (the encryption probe),
`crates/bdinfo-rs/src/main.rs` (exit code 4).</sub>

### Damaged-media read granularity

BDInfo 0.8 reads stream files in 5 MiB requests in both measurement passes (raised from
0.7.5.6's 16 KiB by a throughput change with no correctness motive). bdinfo-rs reads
256 KiB per request in the full pass and 16 KiB in the quick codec pass; the browser
package keeps 5 MiB for both, since its file reads each cost one synchronous round trip.
Healthy discs produce byte-identical reports either way — the packet state machine carries
its state across chunk boundaries, so the request size cannot change a measured value.

On damaged media the request size is the failure granularity, and there the outputs
deliberately diverge from BDInfo 0.8:

- a failed request discards only its own bytes, so a partial scan retains up to ~5 MiB
  more measured data per damaged file than 0.8 would (toward 0.7.5.6's loss profile) —
  chapter rows and stream tallies fill further before the zero rows begin;
- cancellation, polled once per request, takes effect within 256 KiB of reading instead
  of 5 MiB;
- one unreadable span stalls one small request inside the drive's retry storm rather
  than a 5 MiB one — 16 KiB in the quick codec pass (which runs first in every
  measurement scan and alone in a codec-only browse), 256 KiB in the full pass.

A damaged scan of a bare Windows drive root also keeps a degraded disc label: once
stream read errors are recorded, the raw-device read that recovers the UDF volume label
is skipped — one more sector-by-sector grind of a failing drive for a cosmetic string —
so the report and its file name carry the bare drive letter. Classic BDInfo shows the
OS-reported volume label there (it reads it through the filesystem before scanning and
has no raw-device read to skip).

<sub>Source: `crates/bdinfo-rs-core/src/bdrom/m2ts.rs` (`DATA_SIZE`, `QUICK_DATA_SIZE`),
`crates/bdinfo-rs-core/src/vfs/volume.rs` (the label-read gate).</sub>

### Read errors: folder input vs `.iso` input

A read that fails is handled at a different layer for the two inputs, so the same damaged
disc measures differently depending on which one it is scanned through. Both are
deliberate; neither has a BDInfo counterpart, which reads folders only.

- **Folder input** — the failing read aborts that stream file. Everything demuxed before
  it is kept (unless the retention switch discards it) and everything after it is never
  read, so the file's chapter rows and tallies stop at the failure and stay zero past it.
  The failure is recorded against the file and printed in the report `WARNING:` block.
- **`.iso` input** — the UDF reader sits under the demux and is resilient there: it records
  the unreadable sectors and serves that span **as zeros**, so the demux reads on to the
  end of the file. The damage shows up as depressed rates across the affected span rather
  than as a truncated file, the values after it are measured normally, and the retention
  switch has nothing to decide. The reader's recordings are drained into the scan and
  printed in the same `WARNING:` block. Only file data degrades this way: the volume
  structures stay strict, since a damaged filesystem descriptor has no readable rest to
  degrade to, so an image damaged there does not open at all.

Zero-fill is what lets one bad sector cost its own sector rather than the rest of a 30 GB
stream file, which is why the `.iso` path keeps it. Every surface that takes an `.iso`
drains those recordings — the command line, the desktop app and the browser package alike
— so an unreadable image is never reported as a clean scan.

<sub>Source: `crates/bdinfo-rs-core/src/vfs/udf/source.rs` (`UdfSource::open_resilient`,
`take_errors`), `crates/bdinfo-rs-core/src/scan.rs` (`open_iso`),
`crates/bdinfo-rs-wasm/src/lib.rs` (`scan_iso`, `run_iso_report`).</sub>

---

## Desktop app vs. the BDInfo GUI

The report is byte-identical; the window around it is not a pixel-for-pixel port. The three-pane
Playlist / Stream File / Codec view, the playlist table, sorting, drag-and-drop, the settings
dialog, and the report toggles all behave as in the original.

Not carried over:

- **Bitrate charts.** The original plots six chart types (`FormChart`) from per-second and
  per-frame measurements; bdinfo-rs keeps only the aggregate statistics the report prints, so
  there is nothing to plot. The chart-tied image-prefix setting is absent with them.
- **Custom playlist builder.** Playlists are selected from the disc's own table, not assembled.
- **Taskbar progress and live in-grid numbers.** Progress is shown in the window, not painted into
  the taskbar button or streamed into the playlist grid mid-scan.
- **The `KeepStreamOrder`, `EnableSSIF`, and `ExtendedStreamDiagnostics` toggles.**
- **Selectable report text.** The text widgets in use cannot span-select; *Copy report* and
  *Save report…* cover the same need.

Deliberately different:

- **Unreadable files do not prompt.** The original asks Yes/No per failing file; bdinfo-rs collects
  them, scans the rest, and shows one warning banner — the same `WARNING` block the report carries.
- **Saving is both manual and automatic.** A *Save report…* dialog exists alongside the
  save-on-completion setting.
- **An encrypted disc is never scanned.** The original measures the ciphertext; the app lists the
  disc and leaves the scan action disabled — see
  [AACS-encrypted discs are refused](#aacs-encrypted-discs-are-refused).

## See also

- [`CHANGELOG.md`](CHANGELOG.md#differences-from-bdinfo) — the per-release record of these
  divergences.
- [`README.md`](README.md) — project overview and the locked-report contract.
