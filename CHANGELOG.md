# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- From the next release onward, each new `## [X.Y.Z]` section is GENERATED from the
     Conventional Commits since the previous tag (`convco changelog v<prev>..HEAD`) and
     inserted above the preserved history below; the curated pre-adoption entries are
     never regenerated or overwritten. convco emits the same bracketed Keep-a-Changelog
     heading shape used below, which cargo-dist parses for the GitHub Release notes. See
     CONTRIBUTING.md § "Cutting a release". -->

## [v3.0.0](https://github.com/agentjp/bdinfo-rs/compare/v2.0.0...v3.0.0) (2026-08-08)

### ⚠ BREAKING CHANGES

* **wasm:** the npm package's string-and-rows API is replaced by a structured disc model:
  `inspect` and `scan` resolve to a `Disc` — a lossless, typed mirror of the analyzer's
  output (disc metadata, playlists, streams, chapters, read errors) — and the new
  `renderReport` re-renders the classic report from it on demand, byte-identical to the
  CLI's. The report-string and row-tuple exports are gone. Only the npm surface breaks
  here; the CLI and report contracts are untouched. (#215, #217)
* **wasm:** every entry point takes one `ScanOptions` object — `inspect` previously took
  a bare positional threshold — and an out-of-domain `shortPlaylistSeconds` (negative,
  non-finite, or above 86400) now rejects the call instead of silently scanning with the
  20-second default. (#286)
* **core:** AACS-encrypted discs are detected up front, and the CLI refuses to scan them
  with the new exit code 4; the library exposes the detection and leaves the decision to
  the caller. (#246)

### Added

* **core:** stream scans survive read errors: statistics accumulated before a failing
  read are kept and reported, with the failure still listed in the `WARNING` block.
  `--drop-partial` (CLI), the keep-partial-scans setting (GUI), and `keepPartial` (npm)
  opt back into discarding. (#285)
* **core:** one short-playlist threshold contract on every surface: the valid domain is
  0 to 86400 seconds, and 0 switches the short rule off. Programmatic surfaces (CLI,
  npm) reject out-of-domain values; interactive ones (GUI dialog, demo page) clamp.
  (#286)
* **wasm:** a codecs-depth inspect: `codecs: true` reads just each stream file's head,
  so every stream carries its full codec description — profile, level, HDR metadata —
  without the whole-file demux a measured scan costs. (#286)
* **wasm:** `reportFileName` exposes the sanitized `BDINFO.<label>.txt` filename the CLI
  and GUI derive, so browser apps save reports under the identical name. (#286)
* **wasm:** scans list every playlist, each carrying `hiddenBy` classification metadata
  instead of being withheld by a filter — filtering is the client's choice — and
  `selection` measures an explicit playlist set unfiltered. (#214)
* **wasm:** the demo gains playlist-filter, report-section, and size-format settings.
  (#214, #223)
* **wasm:** the structured model carries the encrypted-disc flag (`aacsEncrypted`).
  (#247)
* **cli:** playlist visibility switches — `--show-short-playlists` and
  `--show-looping-playlists` with a hidden-playlist hint (#202) — plus
  `--short-playlist-seconds` for the cutoff itself and the `--no-stream-diagnostics` /
  `--no-quick-summary` report-section switches (#219), on a compacted single-screen
  help card (#203).
* **gui:** encrypted discs are flagged and the scan disabled (#248); playlists hidden by
  the filters can be shown with a transient toggle (#204).
* **core:** the playlist projection, classification, formatting, and selection layer is
  part of the public library API, shared by all three front-ends. (#216, #261, #263)
* **core:** drive-root volume-label recovery moved into the vfs, covering every
  front-end. (#262)

### Fixes

* Hardening against hostile disc images from the continuous fuzz campaign: the backup
  index is used when the primary is unparsable (#245), unknown PMT stream types are
  skipped instead of aborting the section (#249), UDF CS0 dstring used lengths are
  clamped (#250), chapter-mark timestamps are masked (#251), `read_bits4` zero-fills its
  past-window tail (#253), and a playlist summary's angle totals are bounded before
  allocation (#244). Metadata reads are capped, cutting peak scan memory
  ([827918c](https://github.com/agentjp/bdinfo-rs/commit/827918c0560e3943b6871eff61d716fcdc667a27)).
* **wasm:** a `shortPlaylistSeconds` of zero disables the short-playlist filter instead
  of reading as the 20-second default. (#252)
* **packaging:** the installer metadata names agentjp as the publisher. (#195)
* **ci:** the 2.0.0 release lanes are repaired — the gui-publish channel matrix, the AUR
  and Homebrew checksum legs, gui crate packaging, the AUR deploy's source tree, and the
  gui crate's post-release gaps. (#142, #143, #144, #145, #147, #148)

## [v2.0.0](https://github.com/agentjp/bdinfo-rs/compare/v1.2.0...v2.0.0) (2026-07-20)

### ⚠ BREAKING CHANGES

* **core:** `BdRom::open`, `open_with`, `open_resilient`, and `open_resilient_with` take a
  `ScanMode` — `Metadata`, `Codecs`, or `Full` — where they previously took a `bool` selecting the
  measured pass. `open_with` and `open_resilient_with` also take a `&AtomicBool` cancel flag:
  set it from any thread and the scan aborts at its next read chunk with
  `BdError::ScanCancelled`. (#114)
* **report:** `report::text::render_with` takes a `RenderOptions` argument, gating each
  playlist's `STREAM DIAGNOSTICS` and `QUICK SUMMARY` sections. `render` is unchanged and
  still emits the locked default report with every section on. (#114)

### Added

* **Desktop app:** `bdinfo-rs-gui`, a native GUI over the same analyzer — open a `BDMV` folder
  or `.iso`, pick playlists from the familiar three-pane view with draggable splitters and
  resizable sortable columns, scan with live progress and cancel, then view, save, or copy the
  report. Pure Rust on `iced`, GPU-accelerated through wgpu with a software fallback; no
  webview and no bundled runtime. The report it renders is byte-identical to the CLI's.
  Settings, window geometry, theme, and UI scale persist; a per-launch `gui.log` records the
  selected graphics adapter, panics, and otherwise-swallowed errors. (#114)
* **Desktop app distribution:** `gui-v*` tags publish their own GitHub Release with twelve
  artifacts — per-user `.msi` and portable `.zip` for Windows x64/arm64, `.dmg` for Intel and
  Apple Silicon, and `.AppImage` / `.deb` / `.rpm` for Linux x64/arm64 — each covered by
  `SHA256SUMS` and a Sigstore build-provenance attestation. The app is also published to
  WinGet (`agentjp.bdinfo-rs-gui`), a Homebrew cask, the AUR (`bdinfo-rs-gui-bin`), the
  Cloudsmith apt/dnf repository, and crates.io. Installation and the first-launch prompts on
  the unsigned Windows and macOS builds are documented in
  [INSTALL.md](INSTALL.md#desktop-app). (#132)
* **CLI:** `Ctrl+C` during a scan cancels it gracefully and exits with code 130, instead of
  killing the process mid-write. (#114)

### Fixes

* **codec:** print the Maximum Content Light Level unit as `cd/m2`, matching the MaxFALL and
  mastering-display luminance lines; the BDInfo lineage spells this one line `cd / m2`. The
  divergence is recorded in [DIFFERENCES.md](DIFFERENCES.md). (#114)
* **ci:** resolve appimagetool freshness against its highest semver tag (#136), stage the GUI
  release downloads outside the tracked assets directory (#134), and compile the Linux
  packagers from source on the GUI release legs (#133).

## [v1.2.0](https://github.com/agentjp/bdinfo-rs/compare/v1.1.0...v1.2.0) (2026-07-11)

### Fixes

* **cli:** recover the real disc label when scanning a Windows drive root
(#77)
([1f6ee48](https://github.com/agentjp/bdinfo-rs/commit/1f6ee485a43a4a105fb063e6355530e1ef3e82a2)),
closes [#77](https://github.com/agentjp/bdinfo-rs/issues/77)
[#76](https://github.com/agentjp/bdinfo-rs/issues/76)

## [v1.1.0](https://github.com/agentjp/bdinfo-rs/compare/v1.0.1...v1.1.0) (2026-06-29)

### Features

* **wasm:** publish an in-browser build of the analyzer as the npm package
  `@bdinfo-rs/wasm`. It runs the FULL measured scan (M2TS demux + per-stream /
  per-chapter statistics) entirely in the browser — off the main thread in a Web
  Worker, reading a `webkitdirectory`-picked `BDMV` folder, or a single `.iso`
  image, synchronously at byte offsets via `FileReaderSync`, so a multi-GB stream
  never has to fit in memory. The rendered report is byte-for-byte the classic disc
  report, pinned to its own golden rendered from the same Big Buck Bunny fixture the
  native end-to-end test scans (held byte-identical across native, Node, and
  headless Chrome and Firefox). No bytes leave the page. (#41)

### Bug Fixes

* **wasm:** spawn the scan Web Worker through a statically analyzable
  `new Worker(new URL('./worker.js', import.meta.url), { type: 'module' })`, so
  bundlers (Vite, webpack) detect and emit the worker chunk instead of breaking at
  runtime. (#45)
* **packaging:** ship a complete machine-readable DEP-5 `copyright` in the `.deb`
  (every bundled license enumerated), satisfying Debian policy. (#31)

### Changed

* **License:** relicensed to `LGPL-2.1-or-later` (previously declared as the single
  LGPL 2.1 version only), matching the upstream BDInfo per-file source headers
  ("either version 2.1 of the License, or (at your option) any later version"). This
  is a documentation/metadata correction: no code changes, and downstream terms are
  unchanged except that the "or later" option is now explicitly granted. (#40)

### Added

* **Attribution:** added a root `NOTICE` and clarified the README to record that
  bdinfo-rs is a Rust port of, and derivative work based on, BDInfo (© 2010 Cinema
  Squid, LGPL-2.1-or-later) — with the report/analysis baseline ported from
  [UniqProject/BDInfo](https://github.com/UniqProject/BDInfo) and the console flow
  following [tetrahydroc/BDInfoCLI](https://github.com/tetrahydroc/BDInfoCLI). (#40)

## [v1.0.1](https://github.com/agentjp/bdinfo-rs/compare/v1.0.0...218dab463b7973086ae318e8d38da945787dd458) (2026-06-22)

### Features

* **packaging:** install by name on Linux — `apt install bdinfo-rs` (Debian/Ubuntu)
  and `dnf install bdinfo-rs` (Fedora/RHEL/openSUSE) from a hosted package
  repository, in addition to the standalone `.deb`/`.rpm` release downloads.

### Bug Fixes

* **cli:** running `bdinfo-rs` with no arguments now prints the help and exits 0
  instead of clap's missing-argument usage error (exit 2); an actual but invalid
  argument still reports a usage error. Friendlier for a double-clicked binary and
  for package-manager install validators that smoke-run the executable.
* **packaging:** `cargo binstall bdinfo-rs` now resolves the prebuilt release
  archives via explicit binstall metadata, including the flat Windows `.zip` layout
  (1.0.0 shipped no binstall configuration).
* **packaging:** the `.deb` and `.rpm` release packages now publish automatically
  on every release (1.0.0's had to be attached by hand).

## [1.0.0] - 2026-06-19

First public release — a memory-safe, single-static-binary drop-in for the
classic BDInfo disc report.

### Added

- Analyze Blu-ray discs from a `BDMV` folder or a `.iso` image: playlist (MPLS),
  clip (CLPI), and index parsing, M2TS demux, and measured per-stream /
  per-chapter statistics.
- Pure-Rust, read-only **UDF 2.50 reader** for `.iso` input — no libbluray, no
  libudfread, no FFI — hardened against hostile images with block-size, extent,
  directory-depth, and run caps.
- **13 codec scanners** covering the Blu-ray codec set, including HEVC HDR10,
  Dolby Vision, and HDR10+ detection.
- The classic human-readable **BDInfo disc report** as a locked byte contract
  (CRLF, UTF-8 without BOM, invariant number spellings), plus the classic
  interactive console flow (`--list`, `--mpls`, `--whole`, interactive picker).
- **Resilient damaged-disc scan** (`open_resilient`): unreadable files are
  collected into a `WARNING` block and the readable rest is still analyzed
  (exit code 3).
- `unsafe`-free (`forbid`-den workspace-wide) parser with a no-panic / no-hang
  contract on malformed input, held by property tests and continuous fuzzing.
- Reusable parser library crate **`bdinfo-rs-core`** behind a documented API,
  with the **`bdinfo-rs`** CLI as a thin front-end over it.
- Prebuilt static binaries for Windows, Linux, and macOS (x64 and arm64), plus a
  multi-arch (`linux/amd64` + `linux/arm64`) `scratch`-based Docker image. Each
  release archive bundles the binary, LICENSE, README, the four shell completions,
  and the man page; one-line install scripts (`curl … | sh`, `irm … | iex`),
  per-archive `.sha256` sidecars, an aggregate `sha256.sum`, and Sigstore
  build-provenance attestation accompany every release.

### Differences from BDInfo

Where the original BDInfo is provably wrong against the codec specification /
FFmpeg, bdinfo-rs emits the correct value and deliberately diverges (each verified
bit-by-bit, and staying within the existing report vocabulary):

See [DIFFERENCES.md](DIFFERENCES.md) for concrete before/after report examples and
which of these are visible on a normal disc.

- DTS:X IMAX detection, rendered as `DTS:X Master Audio`.
- E-AC-3 reduced data-rate handling.
- HDR10+ recognized from the ST 2094-40 SEI alone, decoupled from a mastering
  display being present.
- AVC High 4:4:4 Predictive profile (profile 244).
- HEVC `profile_idc` / mastering-display / HDR10+ gating.
- VC-1 interlaced-field picture-type handling.
- AC-3 low-sample-rate frame-size shift.
- DTS core 1536 kbps bitrate.

[1.0.0]: https://github.com/agentjp/bdinfo-rs/releases/tag/v1.0.0
