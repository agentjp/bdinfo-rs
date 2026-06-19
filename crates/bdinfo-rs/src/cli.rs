// The `bdinfo-rs` command-line surface — the clap `Cli` definition.
//
// This is the single source of truth for the parsed command line, shared
// verbatim by two compilations:
//
//   * `src/main.rs` `include!`s it at crate root, so `Cli` and its fields are in
//     scope for the run logic exactly as if declared there (private struct,
//     private fields — no visibility change).
//   * `build.rs` `include!`s it (`include!("src/cli.rs")`) to rebuild the same
//     `clap::Command` at build time and emit the shell completions + man page
//     from it. A build script cannot depend on its own crate, so the definition
//     is shared by inclusion rather than by `use`.
//
// Keeping it in one file means the completions and the man page can never drift
// from the binary's actual flags, defaults, or help text.
//
// NB: no `//!` inner doc comments here — this file is pasted into another module
// by `include!`, where inner attributes are not allowed. Plain `//` only.

use clap::Parser;

/// Memory-safe Blu-ray disc, folder, and image analyzer.
#[derive(Debug, Parser)]
#[command(
    name = "bdinfo-rs",
    version,
    about = "Memory-safe Blu-ray disc, folder, and image analyzer.",
    long_about = "bdinfo-rs is a memory-safe, cross-platform Blu-ray analyzer — a drop-in \
                  replacement for the classic BDInfo tool. It inspects a Blu-ray (a BDMV \
                  folder or a .iso image) and writes the familiar human-readable disc \
                  report: the playlists and clips, the M2TS stream layout, and the \
                  per-stream video and audio technical specs — codecs, measured bitrates, \
                  resolution, and HDR, Dolby Vision, or HDR10+ metadata.\n\n\
                  It ships as a single statically-linked binary with no runtime, no shared \
                  libraries, and nothing to install: the disc structures, the codec \
                  scanners, and the read-only UDF 2.50 reader for .iso images are all pure \
                  Rust.\n\n\
                  Point BD_PATH at the disc and bdinfo-rs scans its metadata, presents the \
                  playlist selection table, measures the chosen playlists, and writes \
                  BDINFO.<volume label>.txt into REPORT_DEST.",
    after_long_help = "Examples:\n  \
        # Scan a ripped disc folder; pick playlists interactively, writing the\n  \
        # report back into the same folder.\n  \
        bdinfo-rs /media/MY_MOVIE\n\n  \
        # Scan a disc image. A .iso has no folder to write into, so name a\n  \
        # report destination explicitly.\n  \
        bdinfo-rs MY_MOVIE.iso /tmp/reports\n\n  \
        # Print the playlist selection table and stop, scanning nothing.\n  \
        bdinfo-rs /media/MY_MOVIE --list\n\n  \
        # Scan exactly the named playlists, in the given order.\n  \
        bdinfo-rs /media/MY_MOVIE --mpls 00800,00801\n\n  \
        # Scan every playlist the table lists.\n  \
        bdinfo-rs /media/MY_MOVIE --whole",
    disable_version_flag = true
)]
struct Cli {
    #[arg(
        value_name = "BD_PATH",
        help = "The disc to analyze: a BDMV folder or a .iso image",
        long_help = "The Blu-ray to analyze. This may be the disc root (the folder \
                     containing BDMV), the BDMV folder itself, any folder inside it, or a \
                     .iso disc image. Folder input is read through the filesystem; a .iso \
                     is read through the in-house pure-Rust UDF 2.50 reader. A folder's \
                     disc label is taken from the directory name, while a .iso reports the \
                     real UDF volume label, so the same disc can differ on that one line."
    )]
    bd_path: String,
    #[arg(
        value_name = "REPORT_DEST",
        help = "Folder to write BDINFO.<volume label>.txt into [default: the disc folder]",
        long_help = "The folder the report is written into, as BDINFO.<volume label>.txt. \
                     It defaults to BD_PATH, which works for folder input; a .iso image has \
                     no folder to fall back on, so a report destination must then be given \
                     explicitly. The destination must be an existing directory."
    )]
    report_dest: Option<String>,
    #[arg(
        short = 'l',
        long,
        help = "Print the playlist table and exit without scanning",
        long_help = "Print the playlist selection table — the standard filtered set of \
                     playlists with their length and size — then exit without measuring \
                     anything or writing a report."
    )]
    list: bool,
    #[arg(
        short = 'm',
        long,
        value_name = "NAME,...",
        value_delimiter = ',',
        help = "Scan exactly these playlists, by name, in the given order",
        long_help = "Select playlists by name instead of from the table, as a \
                     comma-separated list (for example 00800,00801). Names are matched \
                     case-insensitively and the .MPLS extension may be omitted. The named \
                     playlists are scanned in the order given, unfiltered, so this reaches \
                     playlists the table would hide. It takes precedence over --whole and \
                     the interactive picker."
    )]
    mpls: Vec<String>,
    #[arg(
        short = 'w',
        long,
        help = "Scan every playlist the table lists",
        long_help = "Select every playlist shown in the selection table — the standard \
                     filtered set — and scan them all, rather than choosing interactively."
    )]
    whole: bool,
    #[arg(
        short = 'v',
        long,
        action = clap::ArgAction::Version,
        value_parser = clap::value_parser!(bool),
        help = "Print version",
        long_help = "Print the bdinfo-rs version and exit."
    )]
    version: Option<bool>,
}
