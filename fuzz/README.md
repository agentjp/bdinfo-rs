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

## Per-target discovery flags

`targets.txt` is the one place the per-target flags live — one row per target, a
dictionary column and a `-max_len` column. It documents its own format; read it from a
shell loop with:

```sh
awk '!/^[[:space:]]*#/ && NF { print $1, $2, $3 }' targets.txt
```

These flags belong to **fresh (discovery) fuzzing only**. The `-runs=0` corpus replay
that gates every pull request takes neither: a dictionary steers mutation and there is
no mutation at zero runs, while `-max_len` would silently drop seeds longer than the cap
and weaken the regression set.

Dictionaries live in `dictionaries/`. Every token is derived from **the code that
consumes it** — the literal a parser compares an input byte range against — and each
file's header comment names the module every token comes from, so an entry can be
checked against the source. To extend one, add a token when that module gains a new
literal comparison over input bytes. To add one for a target that has none, write the
file, point `targets.txt` at it, and measure it as below; a dictionary that does not
move coverage does not belong in the table.

### What a dictionary is worth here — measured 2026-08-03

Six dictionaries were written, one per format with a gate, covering the seven targets
below (`udf` and `source` read the same format), and compared against the tier's existing
flags: same corpus copy, same 300 s budget, same host, repeated independent samples per
arm. `cov` (inline 8-bit counters reached) is the metric.

| target | base `cov` | with a dictionary | wired up |
|---|---|---|---|
| `mpls` | 718, 718 | 717, 718 | no |
| `clpi` | 391, 391 | 391, 391 | no |
| `m2ts` | 1605, 1631 | 1612, 1629 | no |
| `codec` | 2625, 2625 | 2627, 2627 | no |
| `udf` | 589, 586 | 586, 588 | no |
| `source` | 735, 735, 735 | **784, 784, 757, 736** | **yes** |
| `parse_report` | 3683, 3717 | 3649, 3597 | no — it costs 2% |

The zeroes have one cause: **libFuzzer's own comparison tracing already recovers a
format magic without help.** Its end-of-run recommended dictionary from the plain base
runs contains `MPLS0240`, `HDMV0240` and a one-byte-off `*UDF Metadata Partition`, all
found unaided inside the budget. A magic string is exactly the shape tracing solves, so
writing one down for it buys nothing.

`source` is the exception because its gate is **not a literal**. Every `vfs::udf`
descriptor parser starts at `Tag::parse`, which validates the modulo-256 `TagChecksum` —
a value computed *from* the input bytes, so there is no constant for tracing to latch
onto, and random bytes clear it once in 256 tries. Handing the fuzzer eleven
checksum-valid tags skips that gate. The effect is intermittent rather than uniform,
which is what clearing a discrete gate inside a fixed budget looks like: the base arm
sits on exactly 735 in three runs while the dictionary arm reaches 784, 784, 757 and
736.

Only the `source` target feels it. The per-parser `udf` target uses the same file and
gains nothing, because it already feeds every parser at offset 0 and so is not gated the
same way — which is why the table is keyed by target rather than by format.

`parse_report` is the opposite case, and the reason is worth knowing before writing
another dictionary for a target framed like it. Its input is a run of `u16` big-endian
length-prefixed sections, so inserting or overwriting a token shifts the bytes a length
prefix counts and desynchronises the split of all six files. **A dictionary and a
length-prefixed container do not compose.**

### What `-max_len` is worth here — measured 2026-08-03

`-max_len` does not truncate an over-long seed; libFuzzer **drops** it. Shortening the
cap therefore trades seeds for throughput, and only pays when the throughput buys back
more than the discarded seeds carried. Of the ten targets, only `m2ts` and `source` have
a cap above the 4096-byte default at all, because libFuzzer takes it from the largest
seed.

`m2ts` pays. Nine independent 300 s samples per arm, its 98,304-byte corpus-derived cap
against 16,384:

| arm | `cov` samples | mean | exec/s |
|---|---|---|---|
| corpus-derived (98,304) | 1574, 1584, 1589, 1605, 1606, 1610, 1615, 1627, 1631 | 1605 | ~445 |
| `-max_len=16384` | 1596, 1612, 1617, 1626, 1633, 1665, 1685, 1825, 2003 | **1696** | ~720 |

+5.7% mean on 62% more throughput. The medians nearly touch; what separates the arms is
the tail — **four of the nine shorter-unit runs beat the best run the full-length arm
ever produced, and none of the full-length runs did.** A 16 KiB cap keeps 161 of the 415
seeds, so it starts about 46 counters behind at `INITED` (1363 against 1410, averaged
over the same nine runs each) and still finishes ahead. 8 KiB, 32 KiB and 64 KiB were
also measured and none beat 16 KiB.

`source` does not pay, and shows the failure mode. Capping it at 8 KiB costs 24% of its
reach (784 → 557) — deep multi-extent structures stop being expressible — while the
throughput number improves. Its cap is left as the corpus sets it.

Raising a cap was measured too and is not worth it. libFuzzer ramps its per-unit length
with execution count and `parse_report` never reaches its 4096 ceiling in a short run;
forcing it there with `-len_control=0` *loses* coverage (3683 → 3640), and raising the
ceiling to 16 KiB on top loses more (3556). Its units cost more than they discover.

Value profile (`-use_value_profile=1`) was measured on all seven and is wired on none.
It never gained a counter outside `m2ts`, where a shorter `max_len` beats it and it
*subtracts* from that; and it costs 13% throughput on `mpls` and `codec`, 25% on
`source`, 61% on `clpi` and 78% on `udf`. Since the length ramp advances with execution
count, that throughput cost is also a reach cost: `parse_report`'s `lim` after 300 s
falls from 1915 to 615 with it on.

## Running (Linux / WSL / CI, nightly)

```sh
cargo install cargo-fuzz                              # once
cargo +nightly fuzz run read_be   -- -runs=0          # replay the committed corpus (the regression gate)
cargo +nightly fuzz run read_be   -- -max_total_time=300   # fresh-fuzz this target, time-boxed (5 min)
cargo +nightly fuzz run source    -- -max_total_time=300 -dict=dictionaries/udf.dict  # ... with its targets.txt flags
cargo +nightly fuzz run m2ts      -- -max_total_time=300 -max_len=16384               # ... likewise
cargo +nightly fuzz cmin read_be                      # minimize the corpus
cargo +nightly fuzz list                              # show targets
```

Seed corpora live in `corpus/<target>/` (empty / boundary / valid / garbage inputs) and are
committed so `-runs=0` is a deterministic replay.

## What a run costs (measured 2026-08-03, x86_64 Linux, cargo-fuzz 0.13.2)

**Replaying the whole corpus takes about 4 seconds for all ten targets** — 0.13 s
(`discovery`) to 1.03 s (`m2ts`). The `-runs=0` gate costs a build, not a run.

**Fresh-fuzz throughput spans three orders of magnitude**, because a unit means something
different per target: `discovery` classifies a filename at ~10^6 units/s, while an `m2ts`
unit is a full transport-stream scan of thousands of bytes at ~500-800/s. Budget per
target accordingly — an equal time budget is not an equal experiment.

Where a target stops learning — from fresh runs of 120 s on all ten, and 900 s on `m2ts`,
`codec`, `parse_report` and `source`:

| target | new edges after its corpus is loaded |
|---|---|
| `read_be` | none at all |
| `bitstream` | none in 120 s (two new feature combinations, no new edge) |
| `discovery` | stops at ~66 s |
| `udf` | stops at ~13 s |
| `source`, `codec` | stop at ~205 s and ~245 s |
| `m2ts` | slows sharply after ~250 s, still gaining at 843 s |
| `parse_report` | still gaining linearly at 900 s |

`parse_report` is the one target where a longer budget keeps paying: it is the end-to-end
harness, and it reaches only 13% of `report::text`'s regions, so most of the renderer has
never seen a fuzzed disc.
