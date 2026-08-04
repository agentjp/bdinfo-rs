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
>
> **Two pointer widths.** Every leg runs twice — on `x86_64-unknown-linux-gnu` and on
> `i686-unknown-linux-gnu`, where `usize` is **32 bits**. That second width is the one the npm
> package ships: it compiles `bdinfo-rs-core` to `wasm32-unknown-unknown`, so every `checked_*`
> offset guard and length computation in the parser has a different overflow boundary in a browser
> than on a 64-bit host. wasm32 cannot host libFuzzer; i686 is a supported libFuzzer target at the
> same width, so it is the proxy. Both widths share **one corpus** — a libFuzzer corpus is
> arch-agnostic byte files, so a unit found at one width is a valid seed at the other.

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
| `source` | the whole-`.iso` `vfs::udf::source::UdfSource` reader (hostile-input caps included) — `open` over an in-memory image, then a full tree walk + bounded reads of every file. The input *describes* a sparse image: a `u16`-BE sector count, then (`u16`-BE sector, `u16`-BE length, content) records the harness places — the same shape `source.rs`'s own test fixtures build images with | `vfs::udf::source::open_never_panics_on_arbitrary_bytes` |
| `parse_report` | the **end-to-end** pipeline: the input becomes a synthetic in-memory BDMV tree (`u32`-BE length-prefixed sections → `index.bdmv`, `MovieObject.bdmv`, `00000.mpls`, `00000.clpi`, `00000.m2ts`, `META/DL/bdmt_eng.xml` — the roxmltree input — and an optional seventh section for `STREAM/SSIF/00000.ssif`, which makes the disc 3D) → `BdRom::open_resilient` with the packet scan on → `report::text::render` | the resilient-open fault proptests + the render fixture (`cargo nt`) |
| `wasm_report` | `bdinfo_rs_wasm::run_report` — the in-memory entry point behind the browser package's `scanReport` export. Same six-section framing as `parse_report`, but through that crate's own `BdDir`/`BdFile` tree, glob matcher and render wrapper — the half of the pipeline shipped over npm that no other target links | the crate's `measured_scan_matches_golden` parity test |
| `wasm_iso` | `bdinfo_rs_wasm::run_iso_report` — the `.iso` entry point behind `scanIso`: `UdfSource::open_resilient` (the recovering open, not `source`'s strict one) over the sparse image the `source` framing describes, then disc scan and render | the crate's `iso_scan_matches_golden` and `run_iso_report_renders_the_iso_golden_and_empties_a_bad_image` |
| `wasm_disc` | the **reverse** mapping of the browser disc model: JSON bytes → `serde_json` → `mirror::Disc` → `Disc::into_scan` → `report::text::render`. This is the one boundary where the value a report is rendered from arrives from outside the scanner — `renderReport` takes a caller-supplied disc, so in a browser it comes from arbitrary JavaScript | the crate's `every_scan_stage_survives_the_round_trip` and `every_field_survives_a_round_trip_through_the_wire_form` |
| `gui_settings` | `bdinfo_rs_gui::settings::Settings::parse_reporting` — the desktop app's hand-rolled `key = value` configuration reader over lossy UTF-8, and the round trip back through its writer | `any_text_parses_and_restabilizes` |

Every untrusted-input surface now carries a target; the only deliberate exception is
`vfs::fs` (OS-mediated folder IO, exercised by fault-injecting mock-tree tests instead
of byte fuzzing).

**The two `.iso` targets share one input framing** (`fuzz_targets/shared/sparse_iso.rs`), and
`wasm_report` reads the same section framing `parse_report` writes, so a seed means the same
thing on both sides of each pair and units promoted from one corpus are valid in the other.
`parse_report` extends its framing with a seventh section the browser export does not read;
sections past the sixth are ignored there, so seeds stay portable in both directions.

## Per-target discovery flags

`targets.txt` is the one place the per-target flags live — one row per target, a
dictionary column and a `-max_len` column. It is also the one place the **target list**
lives: the replay gate, the discovery workflow and `scripts/fuzz-docker.sh` all read it,
so adding a target means adding its `[[bin]]` in `Cargo.toml` and a row here. It
documents its own format; read it from a shell loop with:

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

**Re-measured after `source`'s input framing changed** (2026-08-03), because a dictionary is
a claim about one harness, not about a format: 18 independent 300 s samples per arm on the
re-derived corpus.

| arm | mean `cov` | samples |
|---|---|---|
| base | 780.3 | 750 751 752 753 764 765 767 768 774 781 782 792 795 795 797 815 818 827 |
| `-dict=dictionaries/udf.dict` | **808.4** | 767 770 777 780 798 799 803 804 805 808 816 825 825 832 833 834 837 838 |

+3.6%, Mann-Whitney U 262.5 of 324 (z = 3.2). **It still earns its place, and the sample
count is the lesson.** The first nine pairs alone gave U = 62 against a 5% critical value of
64 — a "not separable" verdict that nine more pairs overturned. Under the old whole-image
framing the base arm landed on exactly 735 counters in three consecutive runs, so a handful
of samples settled it; the sparse framing puts every mutation into live descriptor bytes,
and the base arm now spans 750–827. **Re-framing a target widens its run-to-run variance,
so a flag measured under the old framing has to be re-measured under the new one with more
samples, not fewer.**

`parse_report` is the opposite case, and the reason is worth knowing before writing
another dictionary for a target framed like it. Its input is a run of big-endian
length-prefixed sections, so inserting a token shifts the bytes a length prefix counts
and desynchronises the split of all six files. **A dictionary and a length-prefixed
container do not compose.**

### `wasm_disc` — measured 2026-08-03, neither dictionary wired

Its input is JSON, so the obvious lever is a dictionary; two were written and measured
separately, because they are different bets. `dictionaries/json.dict` holds the structural
tokens `serde_json` matches (the three value keywords and the punctuation runs that open and
close an object, an array and a member); `dictionaries/disc-fields.dict` holds every field
and variant name the `mirror` types' `Deserialize` derives compare a key against. Four
independent 300 s samples per arm, same corpus copy, same host, all three arms running
concurrently so contention is shared:

| arm | `cov` samples | mean |
|---|---|---|
| base | 4836, 4854, 4878 | 4856 |
| `json.dict` | 4853, 4864, 4879, 4942 | 4885 |
| `disc-fields.dict` | 4780, 4838, 4846, 4855 | 4830 |

+0.6% and −0.5%, neither separable. The field names land where prompt-02's rule predicts —
they are literals a parser compares against, exactly the shape libFuzzer's comparison
tracing already recovers. The structural tokens were the open question, and they do not pay
either. Both files are kept for the record, wired to nothing.

What the target's reach actually depends on is **how often a unit parses at all**. Measured
over the same runs: **about 1 execution in 4,500** gets past `serde_json` and into
`Disc::into_scan` (0.018–0.028% across seven runs). That sounds hopeless and is not: at
~28,000 exec/s a 300 s run still renders roughly 2,000 caller-supplied discs. And the rate
**compounds with the corpus** — a second run started from the first run's grown corpus
reached **0.051%**, with `cov` 4,856 → 5,282. Under the nightly cache that is the regime the
target actually runs in.

That is also why the mirror types carry no `cfg(fuzzing)` `Arbitrary` derives, which would
reach `into_scan` on every execution. Going through JSON is what makes the target's findings
true of the shipped export: a `Disc` this target constructs is one a `renderReport` caller
can construct, because both cross the same `Deserialize` derive.

### What `-max_len` is worth here — measured 2026-08-03

`-max_len` does not truncate an over-long seed; libFuzzer **drops** it. Shortening the
cap therefore trades seeds for throughput, and only pays when the throughput buys back
more than the discarded seeds carried. Of the ten targets measured then, only `m2ts` and `source` have
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

## How the tier runs in CI

Three legs, two of them adversarial and one of them a gate:

| leg | when | what it does |
|---|---|---|
| corpus replay (`core.yml`) | every pull request and push touching the `fuzz` area, and daily through the sweep — one job per pointer width | `-runs=0` over the committed seeds — a deterministic regression check, and a **gating status check** |
| nightly discovery (`fuzz.yml`) | 21:11 UTC daily, one job per target per width, 300 s each on four concurrent libFuzzer processes | fresh fuzzing that **starts where last night stopped** |
| release-tag pass (`fuzz.yml`) | every `v*` tag, 600 s per target per width | a release checkpoint; runs alongside the publish workflows and blocks none of them |

**Discovery compounds through a per-target Actions cache.** A nightly leg restores
everything earlier runs found, fuzzes from the seeds plus that, minimises the union, and
saves back only the units the committed seeds do not already cover. Before the cache
existed, every run began from the same seeds and threw away what it learned — measured
2026-08-03, one 120 s pass over all ten targets grew the corpus from 6,254 to 14,178
units and discarded every one of them. The tag pass restores the same cache read-only:
it starts from accumulated knowledge but never writes back, because a release-adjacent
run must not promote anything into a shared scope.

**Corpus growth is human-gated.** A leg that ends with more units than it restored
uploads them as the artifact `fuzz-corpus-<target>` and the run refreshes one rolling
issue (label `fuzz-corpus`) naming the counts. Nothing is committed by CI. To grow the
regression set, download the artifacts, drop each target's units into
`corpus/<target>/`, and open a pull request; the issue closes itself the next night,
when those units stop counting as new.

**A crash files an issue, one per distinct crash signature** (label `fuzz-crash`) — the
panic's source location and message with digit runs folded, so one out-of-bounds bug does
not file an issue per index; a guard kill with no panic (`-timeout`, `-rss_limit_mb`)
keys on the target and the guard that fired. The reproducer is minimised with `tmin`,
uploaded as an artifact, and inlined base64 in the issue so the evidence outlives the
90-day artifact retention. **A human closes a crash issue with the fix**: a later green
run is not evidence, because the fuzzer need not have replayed that input.

### What the runner's other three cores are worth — measured 2026-08-04

A standard hosted runner is **4 vCPU / 16 GB** on a public repository (2 vCPU / 8 GB on a
private one — that is the figure that is easy to get wrong), and a discovery leg used one
core. libFuzzer's own answer is several processes over one shared corpus directory, with
`-reload` (on by default) carrying each process's finds to the others. **Both flags are
needed**: given `-jobs` alone libFuzzer sets `-workers` to `min(cores / 2, jobs)`, which is
2 here. `-rss_limit_mb` is per process, so four workers bound a leg at 8 GiB of the
runner's 16 GB.

Two independent pairs of 300 s runs, single process (`workers=0`, which never enters
libFuzzer's multi-process path) against four. All four runs restored the **identical**
accumulated corpus on every target, so the arms differ only in process count.

| target | `cov` 1 process | `cov` 4 processes | mean delta | executions |
|---|---|---|---|---|
| `m2ts` | 2189, 2175 | 2571, 2316 | **+12.0%** | x1.40 |
| `source` | 807, 837 | 847, 848 | +3.1% | x2.71 |
| `wasm_report` | 5330, 5440 | 5457, 5553 | +2.2% | x1.68 |
| `wasm_iso` | 818, 802 | 827, 824 | +1.9% | x2.51 |
| `parse_report` | 5562, 5634 | 5657, 5657 | +1.1% | x1.47 |
| `gui_settings` | 446, 446 | 450, 446 | +0.4% | x2.18 |
| `codec` | 2624, 2624 | 2627, 2625 | +0.1% | x2.15 |
| `wasm_disc` | 5141, 5024 | 5100, 5061 | −0.0% | x1.26 |
| `bitstream` | 309, 309 | 309, 309 | 0.0% | x2.14 |
| `clpi` | 392, 392 | 392, 392 | 0.0% | x2.44 |
| `discovery` | 227, 227 | 227, 227 | 0.0% | x2.21 |
| `mpls` | 718, 718 | 718, 718 | 0.0% | x2.20 |
| `read_be` | 74, 74 | 74, 74 | 0.0% | x2.00 |
| `udf` | 589, 589 | 589, 589 | 0.0% | x2.26 |

**Four processes on four vCPUs buy x2.13 the executions, not x4** (446.5 M against 949.2 M
over the whole tier). The heaviest targets scale worst — `wasm_disc` x1.26, `m2ts` x1.40,
`parse_report` x1.47 against x2.0–2.7 for the light ones — so the ceiling is not CPU alone.
Budget throughput from the measured multiplier, never from the core count.

**No target lost coverage, and the gains land exactly where a longer budget would have
put them.** The six targets pinned to an identical counter across all four runs are the
ones measured as finished inside 120 s; the ones that gained are the ones still learning at
300 s. Parallelism is therefore the same lever as a longer run, bought without lengthening
the night — which is what the cadence finding above asks for.

**Sample the noisy targets before quoting a per-target figure.** The first pair alone read
`m2ts` at +17.5%; the second read +6.5%. That is the same tail behaviour the `-max_len`
measurement found (1574–2003 across nine 300 s runs), and one sample would have overstated
the gain by nearly 2x. The six deterministic targets are what make the *shape* of this
table trustworthy on two pairs: where the measurement can move, it does; where the target
is saturated, all four runs agree to the counter.

**Sizing the matrix — for whoever widens it next.** The tier is **14 targets over 2 pointer
widths**, one leg each, against the GitHub Free plan's **20 concurrent jobs**. So the 28 legs
do not all start at once: measured 2026-08-04, 20 ran and 8 queued. That costs wall time, not
runner minutes — unmetered on a public repository — and the pass fires at 21:11 UTC with
nothing else scheduled. Throughput per leg is the lever that does not spend the concurrency
budget, and it is already spent to x2.13. A third dimension would have to buy its legs back
from somewhere.

### The 32-bit leg — measured 2026-08-04

`i686-unknown-linux-gnu` is the tier's proxy for the pointer width the npm package ships:
`wasm32-unknown-unknown` has a **32-bit `usize`**, and wasm32 cannot host libFuzzer. What the
leg takes, in the order it bites:

- **A 32-bit C++ toolchain first, and it does not look like one.** `libfuzzer-sys`'s build
  script compiles libFuzzer's own C++ sources through the `cc` crate, which for an i686 target
  drives the host compiler with `-m32`. Without `gcc-multilib`/`g++-multilib` the build dies
  in that build script before reaching any Rust of ours, and the error names `libfuzzer-sys` —
  it reads like a dependency problem. Installing them costs 21 s on a runner.
- **No AddressSanitizer, and rustc says otherwise.** `rustc --print target-spec-json` reports
  `"supported-sanitizers":["address"]` for this target, but the distributed `rust-std` ships no
  `librustc-*_rt.asan.a`, so `-Zsanitizer=address` compiles the whole tree and then fails at
  **link**. The leg runs `--sanitizer none`. Lost with ASan: `-malloc_limit_mb`, which hooks the
  sanitizer's allocator and aborts on a single huge allocation. Kept: `-timeout` and
  `-rss_limit_mb`, both libFuzzer's own — so the non-termination and allocation-amplification
  guards still fire, and the leg's purpose is pointer width rather than memory safety, which
  `forbid(unsafe)` already answers.
- **It is the cheaper leg.** On the per-PR replay gate, both legs cold: **i686 5 m 11 s against
  x86_64 6 m 28 s.** Building without ASan instrumentation more than pays for the multilib
  install.
- **On the heavy harnesses it is also the faster fuzzer**, for the same reason — which means it
  contributes proportionally more to the shared corpus:

  | target | exec/s i686 | exec/s x86_64 | units grown i686 | x86_64 |
  |---|---|---|---|---|
  | `wasm_iso` | 6,080 | 1,719 | 997 | 577 |
  | `wasm_report` | 5,384 | 1,731 | 11,397 | 5,443 |
  | `parse_report` | 5,397 | 1,846 | 8,815 | 3,810 |
  | `source` | 7,519 | 2,248 | 1,545 | 1,059 |
  | `m2ts` | 3,312 | 1,364 | 9,655 | 4,003 |

  The direction reverses on the light targets (`bitstream` 44,536 against 71,773; `codec` 2,356
  against 3,498), where ASan costs little and 32-bit register pressure costs more.
- **`cov` does not compare across the two legs — and the cause is the sanitizer, not the
  width.** The i686 leg reads 0.43–0.71 of the x86_64 leg on all fourteen targets, which
  invites the reading that 32-bit coverage is worse. It is not. Three arms over the identical
  corpus at `-runs=0`, which is deterministic, separate the two variables:

  | target | x86_64 + ASan | x86_64, no ASan | i686, no ASan | ASan | width |
  |---|---|---|---|---|---|
  | `read_be` | 74 | 41 | 35 | −45% | −15% |
  | `discovery` | 225 | 104 | 102 | −54% | −2% |
  | `bitstream` | 309 | 190 | 179 | −39% | −6% |
  | `clpi` | 391 | 274 | 270 | −30% | −1% |
  | `mpls` | 718 | 486 | 465 | −32% | −4% |
  | `m2ts` | 2610 | 1636 | 1665 | −37% | **+2%** |
  | `codec` | 2633 | 1507 | 1571 | −43% | **+4%** |
  | `udf` | 586 | 255 | 255 | −56% | **0** |
  | `source` | 872 | 464 | 467 | −47% | **+1%** |
  | `parse_report` | 6038 | 4201 | 4061 | −30% | −3% |
  | `wasm_report` | 6271 | 4333 | 4190 | −31% | −3% |
  | `wasm_iso` | 906 | 477 | 480 | −47% | **+1%** |
  | `wasm_disc` | 5101 | 3852 | 3702 | −24% | −4% |
  | `gui_settings` | 442 | 309 | 291 | −30% | −6% |

  Removing the sanitizer costs 24–56% of the counters on **every** target, because ASan
  instruments code of its own. Changing only the width moves the figure by −15% to +4% and
  **not always downward** — `udf` is identical to the counter, and four targets read higher at
  32 bits. Compare a target against itself over time, per arch; never one arch against the
  other.

  Measuring this has its own trap: alternating `--sanitizer` or `--target` between arms
  invalidates cargo's fingerprint, so a script that loops targets outer and arms inner rebuilds
  the whole workspace on every switch. Loop **arms outer, targets inner**.

**One corpus, proven rather than asserted.** Both widths restore the same accumulated units and
each saves back only its own arch's key. In the 2026-08-04 verification pass `restored` was
*identical* across the arches on 12 of 14 targets (`m2ts` 1124/1124, `parse_report` 1299/1299,
`wasm_disc` 5044/5044). The exceptions show the sharing working live rather than breaking it:
`read_be` restored 0 at i686 and **2** at x86_64, because the i686 leg finished first and the
x86_64 leg picked up the two units it had just found.

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

### What one coherent seed is worth — measured 2026-08-03

`parse_report` reached **13.20%** of `report/text.rs` on 1,228 accumulated seeds: it parsed
playlists well (`bdrom/mpls.rs` 96.92%) and then almost never assembled a disc whole enough
to render. Two seeds framed from the committed Big Buck Bunny fixture — `valid-disc` and
`valid-disc-3d`, 7 KB and 13 KB, the same six files the wasm parity test frames plus an
`.ssif` — changed that:

| module | 1,228 accumulated seeds | + the two fixture seeds |
|---|---|---|
| `report/text.rs` | 13.20% | **81.84%** |
| `bdrom/chapters.rs` | 22.47% | 89.89% |
| `bdrom/clpi.rs` | 20.00% | 79.44% |
| `bdrom/m2ts.rs` | 20.82% | 74.78% |
| `bdrom/disc.rs` | 49.34% | 75.33% |
| `bdrom/interleaved.rs` | **0.00%** | 61.11% |
| `stream.rs` | 18.41% | 51.80% |

Accumulated fuzzer output explores *around* what the seeds already reach; it does not
discover a format from nothing. Where a module is dark, check first whether any seed
reaches it at all — a fixture-derived seed is cheaper than a target, and here it closed the
three darkest modules in the tier at once. The full-length variants (99 KB and 198 KB of
`*.m2ts`) were measured too and add `report/text.rs` 81.84% → 83.66%, which is not worth
14× the bytes; the deeper demux coverage they also buy is `m2ts`'s job, and that target
already holds `bdrom/m2ts.rs` at 89%.

## What a run costs (measured 2026-08-03, x86_64 Linux, cargo-fuzz 0.13.2)

**Replaying the whole corpus takes a few seconds for the whole tier** — 0.13 s
(`discovery`) to 1.03 s (`m2ts`). The `-runs=0` gate costs a build, not a run.

The build is where the tier's cost sits, and one dependency dominates it: `gui_settings`
pulls `bdinfo-rs-gui` in, and cargo builds every dependency of this package for **every**
target in it, so the iced/wgpu tree is compiled on every fuzz build. Measured 2026-08-03 on
a warm registry, 24 cores: **14 s for the whole tier without it, 65 s with it.** That is the
price of fuzzing the desktop app's configuration reader from the shared workspace D7 chose,
and it is paid once per build rather than once per target.

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
harness, and it is the target the fixture-derived seeds above lifted from 13% of
`report::text`'s regions to 82%.
