# bdinfo-rs-core

A memory-safe, cross-platform Rust library for analyzing Blu-ray disc structures
(`BDMV` folders and `.iso` images): playlists, clips, and the per-stream video/audio
technical specs (codecs, bitrates, resolution, HDR / Dolby Vision / HDR10+). It is the
parser core behind the `bdinfo-rs` CLI, a Rust reimplementation of the classic BDInfo
report tool.

Built with `#![forbid(unsafe_code)]`, bounds-checked reads, and explicit big-endian
parsing, so behavior is identical on every platform. Malformed input returns errors —
it never panics. The read-only UDF 2.50 reader for `.iso` images is pure Rust; there
are no C dependencies anywhere in the tree.

## Install

```sh
cargo add bdinfo-rs-core
```

## License

LGPL-2.1-or-later.
