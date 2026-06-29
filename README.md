<div align="center">

# bdinfo-rs

**A memory-safe, cross-platform Blu-ray disc analyzer — the classic [BDInfo](https://github.com/UniqProject/BDInfo) report, reimplemented in Rust as a single static binary.**

[![CI](https://github.com/agentjp/bdinfo-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/agentjp/bdinfo-rs/actions/workflows/ci.yml)
[![fuzz](https://github.com/agentjp/bdinfo-rs/actions/workflows/fuzz.yml/badge.svg)](https://github.com/agentjp/bdinfo-rs/actions/workflows/fuzz.yml)
[![audit](https://github.com/agentjp/bdinfo-rs/actions/workflows/audit.yml/badge.svg)](https://github.com/agentjp/bdinfo-rs/actions/workflows/audit.yml)
[![codeql](https://github.com/agentjp/bdinfo-rs/actions/workflows/codeql.yml/badge.svg)](https://github.com/agentjp/bdinfo-rs/actions/workflows/codeql.yml)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/agentjp/bdinfo-rs/badge)](https://scorecard.dev/viewer/?uri=github.com/agentjp/bdinfo-rs)
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/13304/badge)](https://www.bestpractices.dev/projects/13304)
<br>
[![release](https://img.shields.io/github/v/release/agentjp/bdinfo-rs?include_prereleases&sort=semver)](https://github.com/agentjp/bdinfo-rs/releases)
[![MSRV](https://img.shields.io/badge/MSRV-1.96-blue)](rust-toolchain.toml)
[![license](https://img.shields.io/badge/license-LGPL--2.1--or--later-blue)](LICENSE)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success)](Cargo.toml)

[Features](#-features) · [Install](#-installation) · [Usage](#-usage) · [Performance](#-performance) · [Footprint](#-footprint) · [Library](#-library) · [Security](#-quality--security)

</div>

bdinfo-rs scans `BDMV` folders and `.iso` images — playlists, clips, M2TS demux — and
produces the same human-readable disc report as the classic BDInfo tool: per-stream
video/audio technical specs, including codecs, measured bitrates, resolution, and
HDR / Dolby Vision / HDR10+. It ships as **one small statically-linked binary**: no
runtime, no DLLs, no install — drop the file anywhere and it runs.

<details>
<summary><b>Table of contents</b></summary>

- [✨ Features](#-features)
- [🧪 Disclaimer](#-disclaimer)
- [📀 Owned discs only](#-owned-discs-only)
- [📦 Installation](#-installation)
  - [Package managers](#package-managers)
  - [Install script](#install-script)
  - [Prebuilt binaries](#prebuilt-binaries)
  - [Docker](#docker)
  - [Build from source](#build-from-source)
- [🚀 Usage](#-usage)
  - [Shell completions & man page](#shell-completions--man-page)
- [⚡ Performance](#-performance)
- [🪶 Footprint](#-footprint)
- [📚 Library](#-library)
- [🔒 Quality & security](#-quality--security)
- [🔀 Differences from BDInfo](#-differences-from-bdinfo)
- [🧬 Lineage](#-lineage)
- [📄 License](#-license)

</details>

## ✨ Features

- **Drop-in BDInfo replacement.** The same console flow and the same human-readable
  disc report, byte-for-byte deterministic across platforms.
- **One tiny static binary.** No runtime, no DLLs, no install step — a single ~1 MB
  file per platform, [tens of times smaller](#-footprint) than the .NET BDInfo tools.
- **Memory-safe by construction.** `unsafe` is `forbid`-den across the workspace.
  Every read is bounds-checked; malformed input returns an error rather than panicking.
- **Zero C dependencies.** The M2TS demuxer, all 13 codec scanners, the bitstream
  reader, and the read-only UDF 2.50 `.iso` reader are pure Rust — no libbluray, no
  libudfread, no FFI.
- **Fast.** A pipelined demuxer keeps the scan close to NVMe read speed — roughly
  [1.7–13× faster](#-performance) than the existing .NET BDInfo forks.
- **Cross-platform.** Windows, Linux, and macOS, on both x64 and arm64.

## 🧪 Disclaimer

BDInfo is mostly dead code: Blu-ray discs are a thing of the past, and the spec is very unlikely to ever change again.

This made BDInfo the perfect target for a little experiment to try and port it from .NET to Rust as a single fast binary using LLMs, namely Claude Opus 4.8 and Fable 5 (while it was around).
The chatter about porting Bun from Zig to Rust sparked my curiosity, and I wanted to test the current limits of partially-supervised closed-loop LLM-driven development and, most importantly, maintenance.
The goal here is to have a well-maintained open source project, all through LLMs, for maximum user comfort (I'm happy to burn tokens on this).

I like to think the process is the product in this case; the code is just what falls out the other end, and I gotta say I am surprised by the results so far: I invite you to try it and share your results.

One honest caveat: all of this is best-effort. I've thrown a lot at it, proptests, fuzzing, mutation testing, and the real rips on my shelf, but Blu-ray is a sprawling, quirky format, and no test suite covers the long tail of mastering oddities out there. The real proof can only come from the wild: real users running it against the multitude of discs that actually exist. If `bdinfo-rs` stumbles on one of yours, that's the most useful thing you can send me — open an issue with the details and I'll feed it straight back into the loop.

## 📀 Owned discs only

bdinfo-rs is for analyzing Blu-ray discs **you legally own**. It reads disc *structure
and metadata* — playlists, clip info, and per-stream codec specs — from a `BDMV` folder
or `.iso` that already exists on your filesystem. It contains **no decryption and no
copy-protection circumvention** of any kind; it neither rips nor copies the feature
content, and this project does not endorse or assist piracy.

Because it never decrypts, it works only on **already-decrypted** discs — a decrypted
`BDMV` folder or `.iso`; aimed at an encrypted retail disc the playlist list may still
appear, but the per-stream analysis will be meaningless.

When you file an issue — especially an [output difference](https://github.com/agentjp/bdinfo-rs/issues/new?template=output_difference.yml)
— please report only from discs you own, and paste **text reports** (codecs, bitrates,
stream layout — technical metadata), never copyrighted video or audio. A blu-ray.com link
to the exact release is welcome and helps verify the disc; a download link to one is not.

## 📦 Installation

Prebuilt static binaries for Windows, Linux, and macOS (x64 and arm64) ship on every
[release](https://github.com/agentjp/bdinfo-rs/releases), and bdinfo-rs is on the major
package managers. Pick whichever route fits — they're ordered roughly easiest first, and
all deliver the same single static binary.

### Package managers

The quickest route if you already have one — a single command, kept up to date for you:

```sh
# macOS / Linux — Homebrew
brew install agentjp/tap/bdinfo-rs

# Windows — WinGet
winget install agentjp.bdinfo-rs

# Windows — Scoop
scoop bucket add agentjp https://github.com/agentjp/scoop-bucket
scoop install bdinfo-rs

# Rust — fetch the prebuilt binary (no compile)…
cargo binstall bdinfo-rs
# …or build it from crates.io
cargo install bdinfo-rs
```

On Debian/Ubuntu and Fedora/RHEL/openSUSE, install **by name with updates** from the hosted
package repository — add it once (like any third-party repo), then use your package manager
normally. Like Homebrew, these ship the man page and shell completions too:

```sh
# Debian / Ubuntu (and derivatives)
curl -1sLf 'https://dl.cloudsmith.io/public/bdinfo-rs/bdinfo-rs/setup.deb.sh' | sudo -E bash
sudo apt install bdinfo-rs

# Fedora / RHEL / openSUSE
curl -1sLf 'https://dl.cloudsmith.io/public/bdinfo-rs/bdinfo-rs/setup.rpm.sh' | sudo -E bash
sudo dnf install bdinfo-rs
```

Prefer not to add a repository? The individual `.deb`/`.rpm` packages (x64 + arm64) are attached
to every [release](https://github.com/agentjp/bdinfo-rs/releases) — download and install one directly:

```sh
sudo apt install ./bdinfo-rs_*_amd64.deb     # Debian/Ubuntu
sudo dnf install ./bdinfo-rs-*.x86_64.rpm    # Fedora/RHEL
```

The `apt`/`dnf` repository is graciously hosted by [Cloudsmith](https://cloudsmith.com) ♥ OSS.

### Install script

Downloads the right binary for your platform and puts it on `PATH`:

```sh
# Linux / macOS
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/agentjp/bdinfo-rs/releases/latest/download/bdinfo-rs-installer.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/agentjp/bdinfo-rs/releases/latest/download/bdinfo-rs-installer.ps1 | iex"
```

### Prebuilt binaries

Grab the archive for your platform (named by Rust target triple, e.g.
`bdinfo-rs-x86_64-unknown-linux-musl.tar.gz` or
`bdinfo-rs-aarch64-pc-windows-msvc.zip`), extract it, and run the binary — no install
step. Verify a download against the attached aggregate `sha256.sum`, or the per-archive
`.sha256` sidecar.

### Docker

Multi-arch images (`linux/amd64` + `linux/arm64`) are published to the GitHub Container
Registry on each release — the image is just the static binary on `scratch`: no OS, no
shell, no libc, nothing to patch, about 1 MB. The tags match the repo's versions
(`1.0.0`, plus rolling `1.0` and `1`):

```sh
docker pull ghcr.io/agentjp/bdinfo-rs:latest      # or pin a release: :1.0.0
```

Mount the disc and pass its in-container path. `-it` gives the interactive playlist
picker a terminal; the report is written back into the mounted folder:

```sh
docker run --rm -it -v /path/to/disc:/mnt/bd ghcr.io/agentjp/bdinfo-rs /mnt/bd
```

Add a second mount for a separate report folder — required for an `.iso`, which has no
folder to write into — and use the usual flags for non-interactive runs:

```sh
docker run --rm -it -v /path/to/movie.iso:/movie.iso:ro -v /path/to/out:/out \
  ghcr.io/agentjp/bdinfo-rs /movie.iso /out
docker run --rm -v /path/to/disc:/mnt/bd ghcr.io/agentjp/bdinfo-rs /mnt/bd --list
```

To build the image yourself instead of pulling it: `docker build -t bdinfo-rs .`

### Build from source

Same on every platform — no C toolchain, no system libraries, no extra steps:

```sh
git clone https://github.com/agentjp/bdinfo-rs
cd bdinfo-rs
cargo build --release      # binary at target/release/bdinfo-rs
cargo test                 # run the test suite
```

The pinned Rust toolchain (1.96) installs itself automatically via `rust-toolchain.toml`;
Cargo fetches the few pure-Rust dependencies. That's the whole setup on Windows, macOS,
and Linux alike.

## 🚀 Usage

```text
bdinfo-rs <BD_PATH> [REPORT_DEST]
```

`BD_PATH` accepts the disc root, the `BDMV` folder itself, any directory inside it, or a
`.iso` image. The report is written to `REPORT_DEST` (default: the disc folder; required
for `.iso` input) as `BDINFO.{volume label}.txt`.

```sh
bdinfo-rs D:\Rips\MY_MOVIE                # playlist table + interactive selection
bdinfo-rs my_movie.iso C:\Reports         # an .iso needs an explicit report folder
bdinfo-rs D:\Rips\MY_MOVIE --list         # print the playlist table, scan nothing
bdinfo-rs D:\Rips\MY_MOVIE --mpls 00800,00801   # scan exactly these playlists
bdinfo-rs D:\Rips\MY_MOVIE --whole        # scan everything the table lists
```

The console flow is the classic one: a metadata scan, the playlist selection table, the
live scan progress bar, and the report. Unreadable files on a damaged disc are collected
into a `WARNING` block and the rest is scanned (exit code 3).

### Shell completions & man page

Every release archive ships ready-to-install shell completion scripts — for **bash**,
**zsh**, **fish**, and **PowerShell** — and a **`bdinfo-rs.1`** man page, all generated
from the CLI itself so they always match the binary's flags and help text. Package
managers that ship them (Homebrew, the `.deb`/`.rpm` packages, and the AUR package) drop
them into the standard locations automatically; to install one by hand from an extracted
archive:

```sh
# bash  — system-wide, or source it from your ~/.bashrc
install -Dm644 bdinfo-rs.bash /usr/share/bash-completion/completions/bdinfo-rs
# zsh   — put it on your $fpath (e.g. site-functions)
install -Dm644 _bdinfo-rs     /usr/share/zsh/site-functions/_bdinfo-rs
# fish
install -Dm644 bdinfo-rs.fish ~/.config/fish/completions/bdinfo-rs.fish
# man page
install -Dm644 bdinfo-rs.1    /usr/share/man/man1/bdinfo-rs.1
```

```powershell
# PowerShell — dot-source it from your $PROFILE
. .\_bdinfo-rs.ps1
```

Building from source regenerates the same files under
`target/<profile>/build/bdinfo-rs-*/out/assets/`.

## ⚡ Performance

bdinfo-rs scans a disc's streams substantially faster than the existing .NET BDInfo
forks. Its demuxer is pipelined — it reads the next chunk while parsing the current — so
it stays close to NVMe read speed instead of serializing read-then-parse. The table
times three tools on the **same work**: each scans the identical main feature playlist
(the movie) with `-m`, reading the same streams and producing structurally identical
reports, so the only variable is speed.

| Feature playlist scanned | bdinfo-rs | uniqproject | tetrahydroc |
|---|--:|--:|--:|
| MPEG‑2 · 1080i · 3 GB | **0.6 s** | 1.9 s *(2.9×)* | 2.2 s *(3.4×)* |
| AVC · 1080p · DTS:X · 14 GB | **2.7 s** | 6.8 s *(2.5×)* | 35.4 s *(13.2×)* |
| VC‑1 · 1080p · 28 GB | **6.6 s** | 11.9 s *(1.8×)* | 70.6 s *(10.7×)* |
| AVC · 1080p · TrueHD/Atmos · 30 GB | **7.5 s** | 13.7 s *(1.8×)* | 75.0 s *(10.0×)* |
| HEVC · 2160p · HDR10/DV · 50 GB | **15.8 s** | 26.2 s *(1.7×)* | 128.8 s *(8.2×)* |

Across every Blu‑ray video codec and up to 2160p, bdinfo-rs is **≈1.7–2.9× faster than
uniqproject and ≈3–13× faster than tetrahydroc** on byte‑for‑byte equal work. Its lead over
uniqproject is widest on cache‑resident discs and narrows on the largest ones, where both
become bound by the same NVMe read; tetrahydroc trails by roughly an order of magnitude on any
full‑length feature, its scanner CPU‑bound far below disk speed.

<sub>Median of 3 runs (slow references timed once), warm page cache, tools interleaved and
start‑order rotated per disc, wall‑clock end to end — Intel Core Ultra 9 285K · NVMe SSD ·
47 GB RAM · Windows 11. Reference builds: uniqproject =
[UniqProject BDInfo](https://github.com/UniqProject/BDInfo) 0.8.0.1b; tetrahydroc =
[BDInfoCLI](https://github.com/tetrahydroc/BDInfoCLI) (.NET 8), which ships with a
2 GB GC heap cap that aborts on large UHD streams — it was given unrestricted heap so it
could complete the scan and be timed.</sub>

## 🪶 Footprint

bdinfo-rs isn't just faster — it's a rounding error on the others' size. It's a single
self-contained binary of about **1 MB** with nothing else to install. The .NET BDInfo
tools bundle (or require) the .NET runtime, so a working install runs to **tens of
megabytes**:

| Tool | Installed on disk | Runtime |
|---|--:|:--|
| **bdinfo-rs** | **≈ 1 MB** | none |
| BDInfoCLI (tetrahydroc) | 33.7 MB | .NET 8, bundled |
| BDInfo (uniqproject) | 54.1 MB | .NET + Skia, bundled |

That's roughly **34–54× smaller** — one file you can drop on a USB stick, commit to a
repo, or bake into a `FROM scratch` container, with no runtime to carry along.

<sub>On-disk size of a complete install, measured on Windows x64: the size-optimized
`bdinfo-rs` release binary vs. the self-contained publishes of
[UniqProject BDInfo](https://github.com/UniqProject/BDInfo) 0.8.0.1b and
[BDInfoCLI](https://github.com/tetrahydroc/BDInfoCLI) (.NET 8). The Linux musl and
macOS binaries are in the same ≈1 MB ballpark.</sub>

## 📚 Library

The parser core is a separate crate, `bdinfo-rs-core`: disc discovery, MPLS/CLPI/index
parsing, M2TS demux, the codec scanners, the UDF 2.50 reader, and the report renderer,
all reusable behind a documented API. The CLI is a thin front-end over it.

## 🔒 Quality & security

Every push and pull request runs the full gate in CI:

- **Build + test** on Linux, Windows, and macOS — and since `unsafe` is `forbid`-den
  workspace-wide, a green build is itself the memory-safety guarantee.
- **100% line / region / function coverage**, enforced (not aspirational).
- **Max-strictness lints** — clippy `pedantic` + `nursery` + `cargo` plus a restriction
  set (no indexing panics, no silent integer wrap, no `unwrap` on input) as hard errors;
  nightly `rustfmt`; spell-check; docs-as-errors.
- **Supply-chain auditing** — `cargo deny` (advisories, license allowlist, a
  no-C-dependencies ban) and `cargo vet` (every dependency covered by a trusted audit),
  plus CodeQL SAST and dependency review.
- **Mutation testing — zero surviving mutants.** Every pull request mutation-tests its
  diff (`cargo mutants --in-diff`), with a full sweep before each release; a mutant must
  either make a test *fail* or be documented as provably equivalent. Coverage that can't
  kill a mutant doesn't count.
- **Fuzzing — no panics, no hangs.** Every untrusted-input parser (bitstream, M2TS, every
  codec scanner, the UDF `.iso` reader, and the end-to-end pipeline) carries a libFuzzer
  target enforcing a no-panic / no-hang contract on hostile input, with property tests as
  the always-on backstop. Pull requests replay the committed seed corpus as a regression
  gate; every release runs an extended fresh-fuzz pass. No open findings.

Every check above ships in-tree — the lint gate, mutation policy, and fuzz corpus all live
in the repo, so anyone can clone and reproduce these results.

## 🔀 Differences from BDInfo

bdinfo-rs follows the classic BDInfo report format, but its output may differ where
bdinfo-rs fixes a bug in the original tool. In those cases bdinfo-rs emits the value that
is correct against the codec specification / FFmpeg, and the corrected value is the
intended behavior — not a compatibility regression.

👉 **See [DIFFERENCES.md](DIFFERENCES.md) for the exact before/after examples** — the few
fields where bdinfo-rs and BDInfo disagree, and which ones you'll actually see on a normal
disc. (Also summarized in the [changelog](CHANGELOG.md#differences-from-bdinfo).)

bdinfo-rs is derived from BDInfo — a Rust port of it — and is not affiliated with or
endorsed by BDInfo ([UniqProject](https://github.com/UniqProject/BDInfo)). See
[NOTICE](NOTICE) for upstream attribution.

## 🧬 Lineage

bdinfo-rs began as a port and was checked against the reference implementation and spec at
each layer:

- **Report & analysis** — ported from [UniqProject BDInfo](https://github.com/UniqProject/BDInfo),
  the classic .NET tool, as the baseline.
- **Console flow** — the CLI follows [BDInfoCLI](https://github.com/tetrahydroc/BDInfoCLI)
  (tetrahydroc), the command-line BDInfo.
- **Codec correctness** — edge cases cross-checked against
  [libbluray](https://code.videolan.org/videolan/libbluray) and [FFmpeg](https://ffmpeg.org/)
  to fix bugs in the original (see [Differences from BDInfo](#-differences-from-bdinfo)).
- **UDF reader** — validated against [libudfread](https://code.videolan.org/videolan/libudfread)
  and the OSTA UDF 2.60 specification.

## 📄 License

[LGPL-2.1-or-later](LICENSE). The library can be used from applications under other licenses;
changes to bdinfo-rs itself must be shared under the same terms. bdinfo-rs is a derivative
work of BDInfo (© 2010 Cinema Squid), also LGPL-2.1-or-later — see [NOTICE](NOTICE) for
upstream attribution.
