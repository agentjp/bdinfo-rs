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
| [DTS core bit rate (1509 → 1536 kbps)](#dts-core-bit-rate) | **Yes** — bitrate number | **Common** — most DTS-HD MA cores, full-rate DTS tracks and DTS-HD High-Res totals |
| [DTS:X IMAX detection](#dtsx-imax-detection) | **Yes** — codec name | DTS:X IMAX tracks |
| [HDR10+ without a mastering display](#hdr10-without-a-mastering-display) | **Yes** — HDR token, mastering and light-level lines | HDR10+ titles whose stream carries no valid static mastering-display SEI, or whose HDR SEIs arrive after the first scanned access unit |
| [HEVC sub-layer skip in `profile_tier_level`](#hevc-sub-layer-skip-in-profile_tier_level) | **Yes** — the whole HEVC profile/colour/HDR detail and chapter frame sizes | UHD discs whose SPS declares more than one temporal sub-layer |
| [HDR10+ flag per stream](#hdr10-flag-per-stream) | **Yes** — HDR token | Discs that mix HDR10+ and HDR10 HEVC streams |
| [HEVC SEI message alignment](#hevc-sei-message-alignment) | **Yes** — MaxCLL / MaxFALL values or the HDR10+ token | Rare — an SEI unit that packs a timing or recovery message before the light-level or HDR10+ message |
| [MaxCLL unit label (`cd / m2` → `cd/m2`)](#maxcll-unit-label) | **Yes** — unit spelling | **Common** — every HDR title with a content-light-level SEI |
| [AVC High 4:4:4 (profile 244)](#avc-high-444-profile-244) | **Yes** — profile token | Rare — Blu-ray video is almost always 4:2:0 |
| [PGS forced-caption counts](#pgs-forced-caption-counts) | **Yes** — caption tally | Discs with multi-object subtitle compositions |
| [Playlist attribute selection on short clips](#playlist-attribute-selection-on-short-clips) | **Yes** — which clip's streams are presented | Rare — multi-clip playlists whose early clips are short |
| [Bitrate under a backwards PTS](#bitrate-under-a-backwards-pts) | **Yes** — every VBR bitrate, by a few kbps on features | **Common** — any stream whose PES headers carry a PTS without a DTS |
| [Blu-ray 3D eye labels on a 50 Hz disc](#blu-ray-3d-eye-labels-on-a-50-hz-disc) | **Yes** — `Left Eye` / `Right Eye` present | 3D discs with any 25 or 50 fps playlist, when BDInfo happened to enumerate that playlist first |
| [Blu-ray 3D eye labels only on AVC and MVC streams](#blu-ray-3d-eye-labels-only-on-avc-and-mvc-streams) | Only against BDInfo 0.8 — no `Left Eye` on a third video codec | Rare — a 3D playlist that also presents an MPEG-2, VC-1 or HEVC video stream |
| [PGS caption counts are summed over every clip](#pgs-caption-counts-are-summed-over-every-clip) | Only against BDInfo 0.7.5.6 and the 0.8 library ports — caption tally | **Common** — multi-clip and multi-angle playlists with subtitles; matches the BDInfo 0.8 app |
| [Disc Size of a 3D `.iso` (BDInfo 0.8.0.1)](#disc-size-of-a-3d-iso) | **Yes** — roughly half the number 0.8.0.1 prints | Every 3D disc opened as an `.iso` with BDInfo 0.8.0.1; folders and BDInfo 0.7.5.6 agree with bdinfo-rs |
| [E-AC-3 reduced data-rate](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only on non-BD input | Reduced-rate E-AC-3 (24 / 22.05 / 16 kHz) isn't used on Blu-ray |
| [AC-3 low-sample-rate shift](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only on non-BD input | Legacy `bsid` 9/10; conforming Blu-ray AC-3 is always `bsid` 8 |
| [Undecoded MPEG-1/2 and AAC audio names](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only when the stream was never decoded | Damaged discs whose MPA/AAC frames the scan could not read |
| [HEVC `profile_idc` recovery](#correctness-fixes-with-no-effect-on-a-normal-disc) | Edge case only | Malformed headers with `general_profile_idc == 0` |
| [VC-1 interlaced-field picture type](#correctness-fixes-with-no-effect-on-a-normal-disc) | **No** — internal only | Never — the picture tag is counted, never printed |
| [PAT `table_id` validation](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only on malformed input | Never — no conforming disc carries a wrong PAT table id |
| [Signed interval on a backwards timestamp](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only against BDInfo 0.7.5.6 — variable bitrates | Discontinuities and damaged streams |
| [Unknown PMT stream type](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only against BDInfo 0.8 — streams after the entry | Rare — a PMT entry of a type neither tool handles |
| [Oversized adaptation field](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only on malformed input | Never — ISO 13818-1 caps the length |
| [Secondary and picture-in-picture stream declarations](#disc-structures-bdinfo-misreads) | **Yes** — the hidden marker on commentary, PiP video and PiP subtitle rows | BonusView discs with commentary tracks or picture-in-picture |
| [Angle copies of a hidden video stream](#disc-structures-bdinfo-misreads) | **Yes** — the hidden marker on angle rows | Rare — a multi-angle playlist that does not declare its video stream |
| [`MPLS0240` / `HDMV0240` files](#disc-structures-bdinfo-misreads) | **Yes** — the playlist's clips and streams | Discs whose playlists or clip infos carry version `0240` |
| [`FMTS` play items](#disc-structures-bdinfo-misreads) | **Yes** — the clip is measured | Rare — play items whose codec id is `FMTS` |
| [Several program sequences in one clip info](#disc-structures-bdinfo-misreads) | **Yes** — the later sequences' streams | Recorder-authored clips; pressed discs carry one sequence |
| [Header type 4 stream entries](#disc-structures-bdinfo-misreads) | **Yes** — the hidden marker | Rare — a type-4 entry outside the Dolby Vision section |
| [Directory names in any case](#disc-structures-bdinfo-misreads) | **Yes** — the disc opens, its features are detected | Folder rips with lower- or mixed-case directory names |
| [Chapter-mark edge cases](#disc-structures-bdinfo-misreads) | Only on malformed input — chapters kept | Marks with bit 31 set or a bad play-item index |
| [A playlist without a video stream](#disc-structures-bdinfo-misreads) | **Yes** — a report where BDInfo prints none | Audio-only or graphics-only playlists |
| [AACS-encrypted discs are refused](#aacs-encrypted-discs-are-refused) | **Yes** — no report at all | Discs whose stream content is still encrypted |
| [Damaged-media read granularity](#damaged-media-read-granularity) | Only on damaged media — partial scans retain more | Discs with unreadable spans |
| [Read errors: folder input vs `.iso` input](#read-errors-folder-input-vs-iso-input) | Only on damaged media — the same disc measures differently through the two inputs | Damaged discs and damaged disc images |
| [No sticky scan exception](#no-sticky-scan-exception) | Only on damaged media — the `WARNING:` lines | Discs where a stream file other than the last one fails |
| [Damaged metadata](#damaged-metadata) | Only on damaged media — playlists recovered from `BDMV/BACKUP` or kept past a missing angle file | Discs with a corrupt or missing `.mpls` or `.clpi` |
| [An unreadable `.ssif` falls back to the base view](#an-unreadable-ssif-falls-back-to-the-base-view) | Only on damaged media — the clip is measured in 2D instead of lost | 3D discs whose `STREAM/SSIF` file will not open |
| [Angle block layout](#angle-block-layout) | **Yes** — the per-angle bitrate unit and line breaks | Multi-angle playlists |
| [Disc label of a non-UDF Windows drive root](#disc-label-of-a-non-udf-windows-drive-root) | **Yes** — `Disc Label:` and the report file name | A disc folder copied to the root of a USB stick, external disk or mapped share on Windows |

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

A DTS-HD High-Res track's total bit rate includes its core, so it moves by the same
27 kbps:

```diff
- Audio: English / DTS-HD High-Res Audio / 5.1 / 48 kHz /  3018 kbps / 24-bit (DTS Core: 5.1 / 48 kHz /  1509 kbps / 24-bit)
+ Audio: English / DTS-HD High-Res Audio / 5.1 / 48 kHz /  3045 kbps / 24-bit (DTS Core: 5.1 / 48 kHz /  1536 kbps / 24-bit)
```

<sub>Source: `crates/bdinfo-rs-core/src/codec/dts.rs` (bit-rate table),
`crates/bdinfo-rs-core/src/codec/dts_hd.rs` (High-Res total).</sub>

### DTS:X IMAX detection

DTS:X IMAX tracks carry the IMAX extension sync word (`0xF14000D0`) instead of the legacy
DTS:X word BDInfo knows. bdinfo-rs compares it with the low bit ignored, as FFmpeg does, so
`0xF14000D1` also counts as IMAX. BDInfo therefore labels them as plain DTS-HD Master Audio;
bdinfo-rs recognizes the IMAX word and renders `DTS:X Master Audio`. Only the codec-name
field changes — the channel/rate/bit-depth numbers are identical.

```diff
- Audio: English / DTS-HD Master Audio / 5.1 / 48 kHz / 16-bit (DTS Core: 5.1 / 48 kHz /  1536 kbps / 16-bit)
+ Audio: English / DTS:X Master Audio / 5.1 / 48 kHz / 16-bit (DTS Core: 5.1 / 48 kHz /  1536 kbps / 16-bit)
```

<sub>Short names map the same way: `DTS-HD MA` → `DTS:X MA`, `DTS-HD HR` → `DTS:X HR`.
Source: `crates/bdinfo-rs-core/src/codec/dts_hd.rs`.</sub>

### HDR10+ without a mastering display

The BDInfo lineage emits an HDR label only when a static mastering-display SEI with valid
primaries is present. bdinfo-rs also recognizes HDR10+ from the ST 2094-40 (T.35)
dynamic-metadata SEI alone, so a valid HDR10+ stream that carries no mastering display (or
one whose primaries hold the invalid `0xFFFF`) still gets its `HDR10+` token instead of
dropping the descriptor entirely. This applies at any PID: `Dolby Vision` still requires a
mastering display, as in BDInfo.

```diff
- Video: MPEG-H HEVC Video / … / 10 bits / Limited Range / …
+ Video: MPEG-H HEVC Video / … / 10 bits / HDR10+ / Limited Range / …
```

bdinfo-rs also collects the mastering-display, content-light-level and T.35 SEIs on every
access unit it scans, where BDInfo stops at the access unit that initialises the stream.
Metadata that first arrives in a later access unit is therefore printed — its `HDR10` /
`HDR10+` token and its mastering and MaxCLL lines — where BDInfo prints none of them (or
`HDR10` instead of `HDR10+`); and when the values change along the stream, bdinfo-rs
prints the last one seen where BDInfo prints the first.

<sub>When the initialising access unit carries every HDR SEI the stream has, with valid
primaries and values that never change, both tools print the same lines. Source: `crates/bdinfo-rs-core/src/codec/hevc.rs` (HDR-label gate, `sei`).</sub>

### HEVC sub-layer skip in `profile_tier_level`

When an SPS declares `sps_max_sub_layers_minus1` = n ≥ 1, `profile_tier_level` carries
(8 − n) × 2 reserved bits before the sub-layer fields (H.265 §7.3.3, FFmpeg
`decode_profile_tier_level`). BDInfo skips 2 bits whatever n is, so for n < 7 it reads the
rest of the SPS from the wrong position. The misread fields typically fall outside the
ranges BDInfo accepts, so it discards the SPS, no picture parameter set can register
against it, and the stream never initialises. BDInfo then prints the video line without any HEVC detail and an
average frame size of 0 bytes on every chapter; bdinfo-rs skips the spec length and prints
the full detail.

```diff
- Video: MPEG-H HEVC Video / … / 2160p / 23.976 fps / 16:9
+ Video: MPEG-H HEVC Video / … / 2160p / 23.976 fps / 16:9 / Main 10 @ Level 5.1 @ High / 4:2:0 / 10 bits / HDR10 / …
```

<sub>Source: `crates/bdinfo-rs-core/src/codec/hevc.rs` (`profile_tier_level`).</sub>

### HDR10+ flag per stream

BDInfo keeps its HDR10+ flag in one process-wide static that is set by the first ST 2094-40
message it sees and never cleared. Every HEVC stream that initialises after that point is
labelled `HDR10+`, whether or not it carries HDR10+ metadata. BDInfo scans a disc's stream
files from smallest to largest, so one small HDR10+ clip relabels every larger HDR10 clip
on the disc, and in the BDInfo GUI the flag carries over to later discs in the same
session. bdinfo-rs keeps the flag per stream.

```diff
- Video: MPEG-H HEVC Video / … / 10 bits / HDR10+ / Limited Range / …
+ Video: MPEG-H HEVC Video / … / 10 bits / HDR10 / Limited Range / …
```

<sub>Dolby Vision streams are unaffected: their PID decides the label first. Source:
`crates/bdinfo-rs-core/src/codec/hevc.rs` (`HevcExtendedData`).</sub>

### HEVC SEI message alignment

One SEI unit can hold several messages. BDInfo parses the buffering-period, picture-timing,
recovery-point, active-parameter-sets and alternative-transfer messages (types 0, 1, 6,
129, 147) bit by bit, then moves to the next message by whole bytes only, keeping the bit
offset the parse left. Only its mastering-display handler resets that offset, so a
light-level or T.35 message that follows one of those five without a mastering display in
between can be read shifted: wrong MaxCLL / MaxFALL values, or a missed `HDR10+`.
bdinfo-rs skips those five message types whole and reads every following message aligned
to its first byte.

<sub>Source: `crates/bdinfo-rs-core/src/codec/hevc.rs` (`sei`).</sub>

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

### Playlist attribute selection on short clips

BDInfo 0.8 guards its reference clip — the clip whose streams the report presents —
against negligibly short clips: a clip with more clip-info streams takes over only if its
`RelativeLength` exceeds 1% of the playlist. The intent is sound — a 3-second logo clip
must not decide a feature's stream layout — but 0.8 computes `RelativeLength` against the
playlist length accumulated *before* the clip is appended (`TSPlaylistFile.Scan`):
the first clip divides by zero into `+Infinity`, the second is measured against the opener
alone, and every clip against a partial total — so early clips pass the guard however
short they are. bdinfo-rs computes each clip's true fraction of the finished playlist.

The outcome differs only when a playlist's clips disagree about their stream counts —
mainly playlists that open with short logo or warning clips whose stream layout differs
from the feature's. On such a playlist the reference clip, and with it the presented
stream rows, can change; on a playlist whose clips agree, the report is byte-identical.

The rest of the reference-clip rule is 0.8's (`TSPlaylistFile.LoadStreamClips`): a clip
whose stream file exists replaces a reference without one, and a longer clip takes over
only when its stream file exists. BDInfo 0.7.5.6 has none of the three conditions — the
clip with more streams wins, else the longer clip, whatever its length or files. The same
1% guard also decides which of two play items' declarations of one PID is kept; that
choice never reaches the report, because a playlist's declarations only decide which
streams are marked hidden.

<sub>Source: `crates/bdinfo-rs-core/src/bdrom/mpls.rs` (the post-parse resolution),
`crates/bdinfo-rs-core/src/bdrom/disc.rs` (`select_reference`).</sub>

### Bitrate under a backwards PTS

BDInfo 0.8 advances a stream's last-seen PTS whenever the value merely *changes*
(`TSStreamFile`'s PES parse compares `PTS != PTSLast`), so a backwards step measures a
negative transfer interval — discarding that access unit's bitrate sample — and drags the
interval base backwards, overstating the next forward unit's interval and understating its
rate. bdinfo-rs advances only on a forward step, against the running maximum, as BDInfo
0.7.5.6 did (`PTS > PTSLast`) and as the PTS+DTS path of both tools already does.

BDInfo 0.8's `!=` dates from a January 2023 change ("Workaround negative frame
timestamps"); the Jellyfin and Go ports copy 0.8 here, so they print what 0.8 prints.

The rule governs any stream whose PES headers carry a PTS without a DTS — not audio alone
— and the same last-seen PTS is what the end-of-file bitrate pass reads for the video
streams, so it sets the measured seconds every variable-bitrate row divides by. Video rows
move by 1–5 kbps on feature-length playlists and by more on short clips, and the CHAPTERS
table's peak windows shift with them.

<sub>Source: `crates/bdinfo-rs-core/src/bdrom/m2ts.rs` (the PTS-only PES arm and
`finish_scan`).</sub>

---

### Blu-ray 3D eye labels on a 50 Hz disc

On a 3D disc, both tools label the video rows of every playlist that presents more than
one video stream: the AVC base view and the MVC dependent view become `Left Eye` and
`Right Eye`, swapped when the playlist's `MVC_Base_view_R` flag is set. BDInfo does this in
one pass over its playlist dictionary after loading, and that pass stops labelling at the
first playlist carrying 25 or 50 fps video — every playlist it reaches after that one is
left unlabelled. The dictionary is unsorted, so "after" means the order the directory
listing returned the `.mpls` files, which differs between an `.iso` and a mounted folder
and between operating systems. The same disc can print the labels or not depending on how
it was opened.

bdinfo-rs labels every 3D playlist, in every input, whatever its frame rate. On a 3D disc
with 50 Hz playlists, BDInfo prints the labels only when its enumeration happened to reach
the 3D feature before a 50 Hz playlist:

```diff
 VIDEO:

 Codec                   Bitrate             Description
 ---------------         -------------       -----------
-MPEG-4 AVC Video        21,911 kbps         720p / 50 fps / 16:9 / High Profile 4.1
-MPEG-4 MVC Video        10,957 kbps
+MPEG-4 AVC Video        21,911 kbps         Left Eye / 720p / 50 fps / 16:9 / High Profile 4.1
+MPEG-4 MVC Video        10,957 kbps         Right Eye
```

Angle clones are unlabelled in both tools: BDInfo clones them before the labelling pass,
and bdinfo-rs keeps that.

### Blu-ray 3D eye labels only on AVC and MVC streams

When a 3D playlist presents a video stream that is neither AVC nor MVC beside its 3D pair —
an MPEG-2, VC-1 or HEVC picture-in-picture stream, say — bdinfo-rs prints no eye label on
that row, as BDInfo 0.7.5.6 does (`BDROM.Scan` sets `BaseView` on AVC and MVC streams
only). BDInfo 0.8's `switch` sets `BaseView = false` in its `default:` arm, so it prints a
second `Left Eye` on that row; the Jellyfin and Go ports print no label.

```diff
 MPEG-4 AVC Video        21,911 kbps         Left Eye / 1080p / 23.976 fps / 16:9 / High Profile 4.1
 MPEG-4 MVC Video        10,957 kbps         Right Eye
-MPEG-2 Video             4,012 kbps         Left Eye / 480i / 29.970 fps / 4:3 / Main Profile Main Level
+MPEG-2 Video             4,012 kbps         480i / 29.970 fps / 4:3 / Main Profile Main Level
```

### PGS caption counts are summed over every clip

The SUBTITLES description `1920x1080 / N Captions (M Forced Captions)` counts the
captions of every clip in the playlist, as the BDInfo 0.8 app does: between the scan and
the report it zeroes each graphics stream's tallies and adds in each clip's measured
tallies (`MainWindowViewModel.UpdateSubtitleChapterCount`). The sum counts every angle's
captions, as the 0.8 app does, so a multi-angle playlist counts the subtitles of each angle
clip on top of the main clip's, and a clip the playlist plays twice counts twice. The
resolution is the first clip's that has one.

BDInfo 0.7.5.6 prints no tally. The 0.8 library, which the Jellyfin and Go ports run
without the app's post-scan step, copies the reference clip's tally alone, so on a
multi-clip feature they print the count of one clip.

<sub>Source: `crates/bdinfo-rs-core/src/bdrom/disc.rs` (`sum_graphics_captions`).</sub>

### Disc Size of a 3D `.iso`

`Disc Size:` is the byte total of the disc tree without the interleaved `.ssif` files: an
`.ssif` shares its sectors with the base and dependent `.m2ts` files it interleaves, so
counting it counts that video twice. Every BDInfo lineage skips `.SSIF` on a mounted folder.
BDInfo 0.8.0.1 fails to skip it on `.iso` input: its one shared directory walk compares the
extension against `.SSIF` with the leading dot, and the extension its `.iso` file layer
returns has no dot, so nothing matches and the interleaved bytes are added. BDInfo 0.7.5.6
keeps a separate `.iso` walk that compares against `SSIF` without the dot and prints the
folder number.

bdinfo-rs prints the folder number for both inputs. On a 3D `.iso`, BDInfo 0.8.0.1 prints
roughly twice the disc:

```diff
 Disc Label:     DISC_LABEL
-Disc Size:      98,194,245,876 bytes
+Disc Size:      49,873,019,124 bytes
 Protection:     BD+
 Extras:         BD-Java, Blu-ray 3D
```

Opened as a mounted folder, 0.8.0.1 prints the `+` line too.

## Correctness fixes with no effect on a normal disc

These are real spec/FFmpeg-correctness fixes, but they cannot change the report for a
conforming Blu-ray — either the triggering input never occurs on Blu-ray, or the corrected
value is never rendered.

- **E-AC-3 reduced data-rate (`fscod2`).** When the `fscod` field signals a reduced sample
  rate (24 / 22.05 / 16 kHz), bdinfo-rs halves the table rate and derives the bit rate
  from the halved rate over the fixed 6 audio blocks, as ETSI TS 102 366 and FFmpeg do.
  BDInfo uses the same 6 blocks but does not halve the rate, so its sample rate and bit
  rate are both twice the true value. Reduced-rate E-AC-3 is not used on Blu-ray (E-AC-3
  there is 48 kHz), so this never fires on a real disc.
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
- **Undecoded MPEG-1/2 and AAC audio names.** BDInfo derives these four codec names from
  the decoded frame header alone and renders an empty name when no frame was ever decoded
  (the unset `ExtendedData` cast to a null string). bdinfo-rs falls back to the
  type-derived constants BDInfo itself emits — `MP1 Audio` / `MP2 Audio` (0.7.5.6's fixed
  codec names) and `MPEG-2 AAC` / `MPEG-4 AAC` (the short-name constants) — so an audio
  row whose stream the scan could not read never renders a hole. A decoded stream prints
  the same header-derived name as BDInfo.
- **Signed interval on a backwards timestamp.** When a video DTS (or a PTS without a DTS)
  steps backwards inside one file, bdinfo-rs and BDInfo 0.8 measure a negative interval.
  BDInfo 0.7.5.6 subtracts in unsigned 64-bit arithmetic, so the interval wraps to about
  2 × 10^14 seconds and the playlist's variable bitrates collapse to 0. Only a
  discontinuity or damage steps a timestamp backwards.
- **Unknown PMT stream type.** bdinfo-rs skips a PMT entry whose stream type it does not
  handle and registers the entries after it, as BDInfo 0.7.5.6 does. BDInfo 0.8 looks up a
  stream it never created for that entry, catches the resulting exception, and abandons the
  rest of that PMT — every later PMT repeat stops at the same entry, so the streams listed
  after it never register. A Blu-ray PMT rarely lists a stream type outside the set both
  tools handle.
- **Oversized adaptation field.** An adaptation-field length longer than the rest of its
  packet is clamped to the packet. BDInfo lets the leftover countdown run on into the next
  packet's bytes. Only malformed input carries such a length (ISO 13818-1 caps it at 183
  bytes).

## Disc structures BDInfo misreads

BDInfo reads these disc structures differently from their on-disc layout (libbluray's
parsers as the reference). bdinfo-rs reads the layout, so each one changes the report only
on the disc shape named.

- **Secondary and picture-in-picture stream declarations.** A playlist's Stream-Number
  table follows each secondary-audio entry with a list of the primary tracks it mixes with,
  and each secondary-video entry with two such lists, each `2 + n` bytes padded to even.
  BDInfo skips a fixed 2 bytes after every secondary-audio entry and 6 after every
  secondary-video entry (`TSPlaylistFile.Scan`), so after the first commentary
  track that names a primary track it parses the following entries out of those lists:
  later commentary tracks and the picture-in-picture video lose their declaration and
  print as hidden (`* `). BDInfo also never reads the picture-in-picture subtitle entries
  (the loop is commented out), so its interactive-graphics loop parses them in their
  place. bdinfo-rs walks every list at its real length and registers the
  picture-in-picture subtitle entries, so those streams print unhidden.
- **Angle copies of a hidden video stream stay hidden.** A multi-angle playlist whose video
  stream the Stream-Number table does not declare prints that stream as hidden on every
  angle row. BDInfo's angle copies lose the marker because `TSStream.CopyTo` does not copy
  `IsHidden`.
- **`MPLS0240` and `HDMV0240` files are read.** Version `0240` is the BD-ROM 2.4 format of
  Blu-ray 3D, which libbluray accepts. BDInfo accepts only `0100`, `0200` and `0300`
  (`TSPlaylistFile.Scan`, `TSStreamClipFile.Scan`), so it presents such a playlist with no
  clips and such a clip with no streams.
- **`FMTS` play items are measured.** A play item whose codec id is `FMTS` names a
  `<clip>.FMTS` stream file; bdinfo-rs finds and measures it. BDInfo names every clip
  `<clip>.M2TS` and lists `*.m2ts` files only, so the clip is never measured.
- **Every program sequence of a clip-info file is read.** Recorder-authored clips can
  declare more than one program sequence; bdinfo-rs reads the streams of all of them, and a
  PID a later sequence declares again replaces the earlier declaration in its place.
  BDInfo reads the first sequence only (`TSStreamClipFile.Scan` takes its stream count at
  a fixed offset).
- **Stream entries of header type 4 read their PID at offset 2.** libbluray reads the PID
  of a type-4 entry where type 3 has it; BDInfo reads it one byte later
  (`TSPlaylistFile.CreatePlaylistStream`), so the stream that entry declares prints as
  hidden.
- **Directory names match in any case.** `BDMV`, `CLIPINF`, `PLAYLIST`, `STREAM`, `SSIF`,
  `BDJO`, `BDSVM` and the other disc directories are found whatever their case. BDInfo
  compares the names exactly (`BDROM.GetDirectory`), so a lower-case folder rip either
  fails to open or loses the feature that directory signals — no BD-Java, no 3D, `AACS`
  instead of `BD+`.
- **Chapter marks: the time is masked to 31 bits, and a bad play-item index skips the
  mark.** Both tools mask play-item times to 31 bits; BDInfo reads the mark time unmasked,
  so a mark with bit 31 set lands about 47,722 s later and is dropped. An out-of-range
  play-item index makes BDInfo throw and lose every chapter of the playlist
  (`TSPlaylistFile.Scan`); bdinfo-rs skips that one mark.
- **A playlist without a video stream still reports.** An audio-only or graphics-only
  playlist prints its report with zero chapter rates. BDInfo's chapter table reads
  `VideoStreams[0]` unconditionally (`FormReport`, `ReportWindowViewModel`), so report
  generation throws and nothing is printed.

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

The packet state machine carries its state across request boundaries, with one exception
that bdinfo-rs reproduces on purpose. When one of BDInfo 0.8's requests ends inside a
packet the demux is skipping — a PID the PMT does not list, a scrambled packet, the SIT —
its resume count is off: the packet is taken as finished one byte early when only its last
byte lies in the next request, and two bytes early otherwise. The misframing heals at the
next `0x47` sync byte unless a time-code byte at the resume point is itself `0x47`; then
one real packet is swallowed and a bogus one counted in its place, which moves that PID's
packet tally and can merge two PES. bdinfo-rs applies that count exactly at absolute file
offsets that are multiples of 5 MiB, where 0.8's requests end, and the correct count at
every other boundary. Every request size above divides 5 MiB, so a healthy disc measures
what 0.8 measures on every surface and in both passes. BDInfo 0.7.5.6, with 16 KiB
requests, hits the miscount at every 16 KiB boundary instead.

On damaged media the request size is the failure granularity, and there the outputs
deliberately diverge from BDInfo 0.8:

- a failed request discards only its own bytes, so a partial scan retains up to ~5 MiB
  more measured data per damaged file than 0.8 would (toward 0.7.5.6's loss profile) —
  chapter rows and stream tallies fill further before the zero rows begin;
- cancellation, polled once per request, takes effect within 256 KiB of reading instead
  of 5 MiB;
- one unreadable span stalls one small request inside the drive's retry storm rather
  than a 5 MiB one — 16 KiB in the quick codec pass (which runs alone in a codec-only
  browse, and ahead of the full pass in a measurement scan that did not already read
  the codec detail — the desktop app's does, and skips it), 256 KiB in the full pass.

<sub>Source: `crates/bdinfo-rs-core/src/bdrom/m2ts.rs` (`DATA_SIZE`, `QUICK_DATA_SIZE`,
`BDINFO_CHUNK_SIZE`).</sub>

### Read errors: folder input vs `.iso` input

A read that fails is handled at a different layer for the two inputs, so the same damaged
disc measures differently depending on which one it is scanned through. Both are
deliberate. BDInfo reads both inputs too (0.7.5.6 and 0.8.0.1 open `.iso` images), and on
either one a failed read throws out of that stream file's scan, which then lists the file in
its report's `WARNING:` block.

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

The split reaches the report text. Folder input names the stream file that a failed read
aborted, upper-cased as BDInfo prints it, but without the exception's stack trace BDInfo
prints on the line after:

```diff
  WARNING: File errors were encountered during scan:

  00011.M2TS	<the read failure>
- <the exception's stack trace>
```

`.iso` input names the open file handle and the byte offset it first found unreadable, a
shape BDInfo never prints:

```diff
  WARNING: File errors were encountered during scan:

- 00011.M2TS	<the read failure>
- <the exception's stack trace>
+ 00011.M2TS @ byte 5242880	<the read failure>
```

The reader then serves that handle zeros and reads on, so further bad sectors under the
same handle add no further lines. Both shapes are per attempt, not per disc: a damaged span
that both measurement passes read as far as prints a line for each of them.

<sub>Source: `crates/bdinfo-rs-core/src/vfs/udf/source.rs` (`UdfSource::open_resilient`,
`take_errors`), `crates/bdinfo-rs-core/src/scan.rs` (`open_iso`),
`crates/bdinfo-rs-wasm/src/lib.rs` (`scan_iso`, `run_iso_report`).</sub>

### No sticky scan exception

Both BDInfo apps keep one scan state for the whole measured scan and never clear its error
(`ScanBDROMThread` in 0.7.5.6's `FormMain` and 0.8's `MainWindowViewModel`). After the first
stream file that fails, every later stream file is listed in the `WARNING:` block too, each
with the earlier file's message. bdinfo-rs lists only the files that failed, each with its
own failure, and prints no stack-trace line under any of them.

<sub>Source: `crates/bdinfo-rs-core/src/report/text.rs` (`warnings`).</sub>

### Damaged metadata

A playlist or clip-information file that fails to parse keeps what was read before the
failure, as BDInfo keeps its half-built object:

- **A playlist** keeps the clips read before the failure — a play item's clip counts as read
  once its in and out times are — with the Stream-Number-table entries and chapter marks read
  before it. A failure among the play items leaves it with no chapters. A playlist that
  fails before its first clip is left out: BDInfo 0.8 keeps it at length zero, which its
  default short-playlist filter hides, and 0.7.5.6's scan fails on it.
- **A play item whose clip-information file is not on the disc** ends the playlist before
  that item's clip, where BDInfo throws.
- **A clip-information file** keeps the streams read before the failure. One that fails
  before its first stream, or cannot be read at all, stays on the disc with no streams, as
  BDInfo lists every clip-information file before parsing any; the playlists that use it
  keep that clip. A stream PID declared twice within one program sequence is such a
  failure: the parse stops at the second declaration, where BDInfo's stream dictionary
  throws.

Every such failure is recorded in the report's `WARNING:` block and makes the scan exit with
code 3. BDInfo shows it in a prompt during the scan instead, and lists only stream files in
the block.

bdinfo-rs goes further than BDInfo in two places:

- **`BDMV/BACKUP` recovery.** When a damaged `.mpls` or `.clpi` yields nothing — no clip or
  no stream — and its `BDMV/BACKUP` copy parses in full, the copy is used. A primary that
  yields part of its content is kept over the backup. The primary's failure is still
  recorded. BDInfo never reads `BDMV/BACKUP`.
- **Angle clips with missing files.** An extra angle whose clip-information file is missing
  is left out, and one whose stream file is missing stays in the clip list unmeasured. The
  playlist keeps its later play items. BDInfo throws at that angle and ends the playlist
  there.

A chapter mark with a bad play-item index is covered under
[Disc structures BDInfo misreads](#disc-structures-bdinfo-misreads).

<sub>Source: `crates/bdinfo-rs-core/src/bdrom/mpls.rs` (`TsPlaylistFile::scan_partial`),
`crates/bdinfo-rs-core/src/bdrom/clpi.rs` (`TsStreamClipFile::scan_partial`),
`crates/bdinfo-rs-core/src/bdrom/disc.rs` (`Sink::absorb_parse`).</sub>

### An unreadable `.ssif` falls back to the base view

A 3D clip stores its base and dependent views interleaved in
`BDMV/STREAM/SSIF/<clip>.ssif`, and both tools demux that file in preference to the plain
`BDMV/STREAM/<clip>.m2ts` — it is the superset, and it is the only place the dependent
(MVC) view exists. It is not, however, the only readable source of the base view.

When the `.ssif` cannot be opened, BDInfo abandons the clip: no streams, no bitrates, no
chapter rows. bdinfo-rs opens the `.m2ts` instead and measures the base view from it, so a
3D disc with an unreadable interleaved file still reports everything a 2D scan of it would
— minus the dependent-view row, which genuinely could not be read. The failure is not
swallowed: it is recorded against the `.ssif` and printed in the report `WARNING:` block,
and the clip is named by the `.m2ts` it was actually read from wherever the report names
its source file. (A 3D clip the scan never measured at all is named by its `.ssif`, as in
BDInfo.) The clip's interleaved file size still counts the `.ssif` — the file is on
the disc either way, and an unreadable one is no smaller. (The disc-level `Disc Size:`
never counts it, in either tool.) A strict (fail-fast) scan still aborts;
degrading is the resilient scan's job.

This applies to the failed *open* only. An `.ssif` that opens and then fails partway is an
ordinary damaged stream file, handled as
[above](#read-errors-folder-input-vs-iso-input): what was demuxed before the failure is
kept, and the base view is not re-read from the `.m2ts`.

<sub>Source: `crates/bdinfo-rs-core/src/bdrom/disc.rs` (`scan_one_stream_file`).</sub>

### Angle block layout

A playlist with extra angles prints a block of per-angle lines after `Total Bitrate:`. bdinfo-rs
prints BDInfo 0.8's layout — two-digit hours, `(h:mm:ss.ms)` on the angle lines, no blank lines —
with two of its slips repaired:

```diff
- Angle 1 Total Bitrate:  2.05 kbps Mbps / 2.05 kbps MbpsAngle 2 Length:         00:00:00.000 (h:mm:ss.ms) / 00:00:30.000 (h:mm:ss.ms)
+ Angle 1 Total Bitrate:  2.05 Mbps / 2.05 Mbps
+ Angle 2 Length:         00:00:00.000 (h:mm:ss.ms) / 00:00:30.000 (h:mm:ss.ms)
```

The value is megabits per second in every BDInfo version (bytes × 8 / seconds / 10⁶, rounded
to two decimals), so the stray `kbps` 0.8 places before `Mbps` is dropped. 0.8 also ends
each `Angle N Total Bitrate:` line without a line break, so with two or more extra angles that
line runs into the next angle's `Length:` line; bdinfo-rs ends every line. Each line bdinfo-rs
prints exists in some BDInfo output: the `Total Bitrate` line equals 0.7.5.6's byte for byte.
BDInfo 0.7.5.6 lays the block out differently again — a blank line before each angle and before
`All Angles`, unpadded hours, and `(h:m:s.ms)` on every line; bdinfo-rs keeps the 0.8 layout the
rest of its report follows.

<sub>Source: `crates/bdinfo-rs-core/src/report/text.rs` (`playlist_report`).</sub>

### Disc label of a non-UDF Windows drive root

A `BDMV` folder at the root of a Windows drive that is not a UDF disc — an NTFS, exFAT or FAT USB
stick, an external disk, a mapped network share — prints the bare drive letter as its
`Disc Label:` (and names the report `BDINFO.<letter>.txt`), where BDInfo prints the filesystem's
volume label. Reading that label needs a Windows API call, which bdinfo-rs's no-`unsafe` rule
rules out. A disc folder inside a named directory is unaffected, and so is a UDF disc in a drive:
its label is read off the raw volume.

<sub>Source: `crates/bdinfo-rs-core/src/vfs/volume.rs` (`resolve_folder_label`).</sub>

---

## Desktop app vs. the BDInfo GUI

The report is byte-identical; the window around it is not a pixel-for-pixel port. The three-pane
Playlist / Stream File / Codec view, the playlist table, sorting, drag-and-drop, the settings
dialog, and the report toggles all behave as in the original. The Stream File pane names a
3D clip by its `.ssif` file before any scan has measured it, as the original does. The info box's
`Detected Features:` line follows the original's order and includes BD+ — Ultra HD, 50Hz Content,
BD+ Copy Protection, BD-Java, Blu-ray 3D, D-BOX Motion Code, PSP Digital Copy — while the report's
`Extras:` line keeps the report's own order, without BD+.

Not carried over:

- **Bitrate charts.** The original plots six chart types (`FormChart`) from per-second and
  per-frame measurements; bdinfo-rs keeps only the aggregate statistics the report prints, so
  there is nothing to plot. The chart-tied image-prefix setting is absent with them.
- **Custom playlist builder.** Playlists are selected from the disc's own table, not assembled.
- **Taskbar progress.** Progress is shown in the window, not painted into the taskbar button.
- **The `KeepStreamOrder`, `EnableSSIF`, and `ExtendedStreamDiagnostics` toggles.** Stream
  rows always take `KeepStreamOrder`'s default: the clip-info declaration order within each
  stream kind.
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
- **A dropped stream file loses its codec detail.** The app browses a disc at codec depth before
  you scan it, so its measured scan skips the quick codec pass rather than reading every stream
  file's head a second time — on damaged media that second read can stall for minutes. The pass it
  skips is also the fallback for a file the measured pass gives up on, so with *Keep partially
  scanned stream files* turned **off** (it is on by default) a stream file that fails partway
  through leaves its rows carrying only what the disc's clip info declares — codec name,
  resolution, channel layout — without the profile, level, and HDR detail the stream itself would
  have supplied. Nothing else changes: the file is still named in the warning banner and the
  report's `WARNING` block, and with the setting left on, or on media that reads cleanly, the rows
  are identical to the command line's.

## See also

- [`CHANGELOG.md`](CHANGELOG.md#differences-from-bdinfo) — the per-release record of these
  divergences.
- [`README.md`](README.md) — project overview and the locked-report contract.
