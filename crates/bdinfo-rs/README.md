# bdinfo-rs

**The classic BDInfo disc report from the command line — one static binary, no runtime.**

[![crates.io](https://img.shields.io/crates/v/bdinfo-rs)](https://crates.io/crates/bdinfo-rs)
[![license](https://img.shields.io/badge/license-LGPL--2.1--or--later-blue)](https://github.com/agentjp/bdinfo-rs/blob/master/LICENSE)

Analyzes `BDMV` folders and `.iso` images and writes the classic
`BDINFO.{volume label}.txt` report: playlists and per-stream video and audio specs — codecs,
measured bitrates, resolution, and HDR / Dolby Vision / HDR10+. A drop-in replacement for the
original tool, with the same console flow. No GUI; for one, see the desktop app in the
[main project](https://github.com/agentjp/bdinfo-rs).

## Install

```sh
cargo install bdinfo-rs      # or, without compiling: cargo binstall bdinfo-rs
```

Also on Homebrew, WinGet, Scoop, the AUR, apt/dnf, and Docker — every route is in the
[main project's install guide](https://github.com/agentjp/bdinfo-rs#command-line).

## Usage

```sh
bdinfo-rs <BD_PATH> [REPORT_DEST]
```

`BD_PATH` takes the disc root, the `BDMV` folder, any directory inside it, or an `.iso`. The report
goes to `REPORT_DEST`, defaulting to the disc folder and required for an `.iso`.

```sh
bdinfo-rs /path/to/disc                       # playlist table, then pick interactively
bdinfo-rs /path/to/movie.iso /path/to/out     # an .iso needs an explicit report folder
bdinfo-rs /path/to/disc --list                # print the playlist table, scan nothing
bdinfo-rs /path/to/disc --mpls 00800,00801    # scan exactly these playlists
bdinfo-rs /path/to/disc --whole               # scan everything the table lists
```

The table hides playlists shorter than 20 seconds and looping ones, and names what it withheld.
`--show-short-playlists` and `--show-looping-playlists` put each category back — into the table,
the picker, and `--whole`. `--short-playlist-seconds` moves that cutoff (0 to 86400 seconds; 0
counts nothing as short).

The report renders every section by default. `--no-stream-diagnostics` drops the per-playlist
`STREAM DIAGNOSTICS` tables and `--no-quick-summary` drops the `QUICK SUMMARY` blocks; each omits
exactly its own section. `--drop-partial` discards what a stream file measured before a read error
interrupted it, which is otherwise kept.

A run on a terminal opens with the bdinfo-rs banner; `--no-banner` drops it. Piped or redirected
output never carries it.

| Exit code | Meaning |
|---|---|
| 0 | Scan completed |
| 1 | Not a Blu-ray structure, or no playlist matched `--mpls` |
| 2 | No such path, an unusable report destination, or an invalid argument |
| 3 | Scan completed with unreadable files, collected into the report's `WARNING` block |
| 4 | AACS-encrypted disc — the scan is refused before it starts; `--list` still works |
| 130 | Cancelled with <kbd>Ctrl</kbd>+<kbd>C</kbd> |

Release archives ship bash, zsh, fish, and PowerShell completions plus a `bdinfo-rs.1` man page,
generated from the CLI itself so they always match the binary.

## Library

The analyzer is [`bdinfo-rs-core`](https://crates.io/crates/bdinfo-rs-core); this crate is a thin
front-end over it.

## License

[LGPL-2.1-or-later](https://github.com/agentjp/bdinfo-rs/blob/master/LICENSE). Derived from BDInfo
(© 2010 Cinema Squid).
