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

## [Unreleased]

### Changed

- Running `bdinfo-rs` with no arguments now prints the help and exits 0, instead
  of clap's missing-argument usage error (exit 2). Invoking it with an actual but
  invalid argument still reports a usage error. This makes a bare run friendlier
  (such as a double-clicked binary) and lets package-manager install validators
  that smoke-run the executable see a clean exit.

### Added

- **Install by name on Linux**: `apt install bdinfo-rs` (Debian/Ubuntu) and
  `dnf install bdinfo-rs` (Fedora/RHEL/openSUSE) from a hosted package repository —
  add the repo once, then use your package manager normally, updates included — in
  addition to the standalone `.deb`/`.rpm` release downloads.
- README: an installed-footprint size comparison and an "owned discs only" usage
  policy; a one-paragraph code of conduct; and a structured output-difference
  issue template.

### Fixed

- `cargo binstall bdinfo-rs` now resolves the prebuilt release archives via
  explicit binstall metadata, including the flat Windows `.zip` layout. (1.0.0
  shipped no binstall configuration, so installs fell back to fragile
  auto-detection.)
- The `.deb` and `.rpm` release packages now publish automatically on every release
  (1.0.0's had to be attached by hand).

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

[Unreleased]: https://github.com/agentjp/bdinfo-rs/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/agentjp/bdinfo-rs/releases/tag/v1.0.0
