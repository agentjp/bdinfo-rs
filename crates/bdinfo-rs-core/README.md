# bdinfo-rs-core

**The Blu-ray disc analyzer behind [bdinfo-rs](https://github.com/agentjp/bdinfo-rs), as a library.**

[![crates.io](https://img.shields.io/crates/v/bdinfo-rs-core)](https://crates.io/crates/bdinfo-rs-core)
[![docs.rs](https://img.shields.io/docsrs/bdinfo-rs-core)](https://docs.rs/bdinfo-rs-core)
[![license](https://img.shields.io/badge/license-LGPL--2.1--or--later-blue)](https://github.com/agentjp/bdinfo-rs/blob/master/LICENSE)

Parses `BDMV` folders and `.iso` images — playlists, clips, M2TS demux — and renders the classic
BDInfo disc report: per-stream video and audio specs, codecs, measured bitrates, resolution, and
HDR / Dolby Vision / HDR10+. It is the entire analyzer; the command-line tool, the desktop app, and
the WebAssembly package are thin shells over it, which is why their output is byte-identical.

```sh
cargo add bdinfo-rs-core
```

## What is in it

| Module area | Responsibility |
|---|---|
| `vfs` | Filesystem and `.iso` access — including a read-only UDF 2.50 reader |
| `bitstream`, `bytes` | Bit-level reads and Exp-Golomb decoding |
| `mpls`, `clpi`, `index` | Playlist, clip-info, and index parsing |
| `bdrom`, `discovery` | Disc scan and structure discovery |
| `bdrom::m2ts` | Transport-stream demux |
| `codec` | 13 codec scanners, video and audio |
| `report` | The byte-locked report renderer |

## Guarantees

- **`#![forbid(unsafe_code)]`** — no `unsafe` anywhere in the crate.
- **Never panics on input.** Malformed data returns `Result<_, BdError>`; an absent field or EOF is
  an `Option`. No `unwrap`, no raw indexing on disc bytes. Held by property tests and a libFuzzer
  target on every untrusted-input entry point.
- **Deterministic across platforms.** Explicit big-endian parsing and ordered collections on every
  output path, so the rendered report is byte-identical everywhere.
- **No C dependencies.** The demuxer, the codec scanners, the bitstream reader, and the UDF reader
  are all in-house Rust. No libbluray, no libudfread, no FFI.
- **SemVer-stable API**, verified against the last release tag by `cargo-semver-checks`.

The report is a frozen byte contract — CRLF, UTF-8 without BOM, invariant number formatting. Where
it deliberately diverges from the original BDInfo, the reason is documented in
[DIFFERENCES.md](https://github.com/agentjp/bdinfo-rs/blob/master/DIFFERENCES.md).

## Documentation

[docs.rs/bdinfo-rs-core](https://docs.rs/bdinfo-rs-core) — every public item is documented, and the
doc build runs with warnings as errors.

## License

[LGPL-2.1-or-later](https://github.com/agentjp/bdinfo-rs/blob/master/LICENSE). Usable from
applications under other licenses; changes to this library must be shared under the same terms.
Derived from BDInfo (© 2010 Cinema Squid).
