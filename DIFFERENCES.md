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
| [AVC High 4:4:4 (profile 244)](#avc-high-444-profile-244) | **Yes** — profile token | Rare — Blu-ray video is almost always 4:2:0 |
| [E-AC-3 reduced data-rate](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only on non-BD input | Reduced-rate E-AC-3 (24 / 22.05 / 16 kHz) isn't used on Blu-ray |
| [AC-3 low-sample-rate shift](#correctness-fixes-with-no-effect-on-a-normal-disc) | Only on non-BD input | Legacy `bsid` 9/10; conforming Blu-ray AC-3 is always `bsid` 8 |
| [HEVC `profile_idc` recovery](#correctness-fixes-with-no-effect-on-a-normal-disc) | Edge case only | Malformed headers with `general_profile_idc == 0` |
| [VC-1 interlaced-field picture type](#correctness-fixes-with-no-effect-on-a-normal-disc) | **No** — internal only | Never — the picture tag is counted, never printed |

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

---

## See also

- [`CHANGELOG.md`](CHANGELOG.md#differences-from-bdinfo) — the per-release record of these
  divergences.
- [`README.md`](README.md) — project overview and the locked-report contract.
