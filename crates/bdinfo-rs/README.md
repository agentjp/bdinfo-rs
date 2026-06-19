# bdinfo-rs (CLI)

`bdinfo-rs` is a memory-safe, cross-platform command-line analyzer for Blu-ray disc
structures (`BDMV` folders and `.iso` images). It reports playlists and per-stream
video/audio specs (codecs, bitrates, resolution, HDR / Dolby Vision / HDR10+). No GUI.

It is a drop-in replacement for the classic BDInfo tool: the same console flow and the
same `BDINFO.{volume label}.txt` disc report, reimplemented in Rust. It ships as a
single statically-linked binary — no runtime, no DLLs, no install.

## Install

```sh
cargo install bdinfo-rs
```

OS package managers (winget / Homebrew / apt / pacman) are planned once tagged
releases ship prebuilt binaries.

## Usage

```sh
bdinfo-rs /path/to/bluray-folder            # playlist table + interactive selection,
                                          # then BDINFO.{label}.txt beside the disc
bdinfo-rs /path/to/disc.iso /path/to/out    # an .iso needs an explicit report folder
bdinfo-rs /path/to/bluray-folder --list     # print the playlist table, scan nothing
bdinfo-rs /path/to/bluray-folder --mpls 00800,00801
bdinfo-rs /path/to/bluray-folder --whole    # scan everything the table lists
```

## License

LGPL-2.1-only.
