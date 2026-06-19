# bdinfo-rs fuzz tier

Adversarial coverage over the **untrusted-input entry points** (Blu-ray bytes are attacker-
controlled). The always-on local guarantee is the in-tree proptests (`cargo nt` on every host);
these fuzz targets are the amplifier that runs deeper, on a Linux/nightly tier.

> **Platform:** cargo-fuzz / libFuzzer need a **nightly** toolchain and have **no Windows MSVC
> support**, so they run on a Linux toolchain. The tier now runs on **every host**: natively on
> **Linux / WSL / CI** (nightly), and on **Windows via Docker** — `scripts/fuzz-docker.ps1`
> (`scripts/compliance.ps1 -Full` invokes it when Docker is available; else it skips with a printed
> note, and the no-panic / no-hang contract is held by the proptests). Replays carry per-unit
> `-timeout`/`-rss_limit_mb` guards so a non-termination or allocation blow-up on hostile bytes
> fails the gate instead of hanging it.

This is an **independent workspace** (own `[workspace]`, `exclude`d from the root) so its
`unsafe`-using `libfuzzer-sys` harness never touches the main workspace's `forbid(unsafe_code)`
posture or `cargo ck`/`cargo lt`.

## Live targets

| target | entry point | proptest mirror (`cargo nt`) |
|---|---|---|
| `read_be` | every `bdinfo_rs_core::bytes` reader — `read_u8` / `read_u16_be` / `read_u24_be` / `read_u32_be` / `read_u64_be` / `read_uint_be` (incl. past-width requests) / `read_ascii` — over all offsets | `read_*_never_panics` |
| `discovery` | `BdmvDir::from_name` / `BdFileKind::from_filename` (lossy-UTF-8) | `*_classification_ignores_case` |
| `bitstream` | `bdinfo_rs_core::bitstream::TsStreamBuffer` — the whole reader surface (`read_bool`, `read_bits2`/`4`/`8`, Exp-Golomb, `bs_skip_*`, seek) driven by an opcode stream | `reads_never_panic_on_arbitrary_input`, `read_exp_never_panics_on_arbitrary_input`, `read_bits*_matches_bit_by_bit` |
| `clpi` | `bdinfo_rs_core::bdrom::clpi::TsStreamClipFile::scan` — a `*.clpi` clip-info file | `scan_never_panics_on_arbitrary_input` |
| `mpls` | `bdinfo_rs_core::bdrom::mpls::TsPlaylistFile::scan` — a `*.mpls` playlist file | `scan_never_panics_on_arbitrary_input` |
| `m2ts` | `bdinfo_rs_core::bdrom::m2ts::TsStreamFile::scan` — a `*.m2ts`/`*.ssif` transport stream | `scan_never_panics_on_arbitrary_bytes` |
| `codec` | the audio `bdinfo_rs_core::codec::ac3::scan` / `truehd::scan` / `dts::scan` / `dts_hd::scan` / `lpcm::scan` / `aac::scan` / `mpa::scan`, the video `avc::scan` / `mpeg2::scan` / `vc1::scan` / `mvc::scan` / `hevc::scan` and the graphics `pgs::scan` — an access unit (first byte selects the stream type `% 17`; its high bits seed the DTS `bitrate`) | `codec::{ac3,truehd,dts,dts_hd,lpcm,aac,mpa,avc,mpeg2,vc1,mvc,hevc,pgs}::…::scan_never_panics_on_arbitrary_bytes` |
| `udf` | the `vfs::udf` parsers — `Avdp`/`Lvd`/`PartitionDescriptor`/`Fsd::parse`, `FileEntry::parse`, `parse_directory`, CS0 `decode_dstring` — over disc-image sectors | `vfs::udf::…::*_never_panics` |
| `source` | the whole-`.iso` `vfs::udf::source::UdfSource` reader (hostile-input caps included) — `open` over an in-memory image, then a full tree walk + bounded reads of every file. The input maps to byte 512 KiB (the AVDP's fixed sector), so seeds are images with the first 256 sectors stripped — the committed valid seeds mirror `source.rs`'s test fixtures | `vfs::udf::source::open_never_panics_on_arbitrary_bytes` |
| `parse_report` | the **end-to-end** pipeline: the input becomes a synthetic in-memory BDMV tree (`u16`-BE length-prefixed sections → `index.bdmv`, `MovieObject.bdmv`, `00000.mpls`, `00000.clpi`, `00000.m2ts`, `META/DL/bdmt_eng.xml` — the roxmltree input) → `BdRom::open_resilient` with the packet scan on → `report::text::render` | the resilient-open fault proptests + the render fixture (`cargo nt`) |

Every untrusted-input surface now carries a target; the only deliberate exception is
`vfs::fs` (OS-mediated folder IO, exercised by fault-injecting mock-tree tests instead
of byte fuzzing).

## Running (Linux / WSL / CI, nightly)

```sh
cargo install cargo-fuzz                              # once
cargo +nightly fuzz run read_be   -- -runs=0          # PR: replay committed corpus (fast, regression gate)
cargo +nightly fuzz run read_be   -- -max_total_time=300   # nightly: time-boxed (5 min)
cargo +nightly fuzz cmin read_be                      # minimize the corpus
cargo +nightly fuzz list                              # show targets
```

Seed corpora live in `corpus/<target>/` (empty / boundary / valid / garbage inputs) and are
committed so `-runs=0` is a deterministic replay.
