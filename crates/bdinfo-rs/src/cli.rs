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
// The help is one screen: a single short line per argument, with the long-form
// prose living on the man page instead — `build.rs` reattaches it to this same
// `Command` before rendering it, so nothing documented here is lost.
//
// Four settings hold that card in shape. `-h`/`--help` is declared here with
// `HelpShort` (clap's own flag renders the long help for `--help`, which would
// make the two spellings two different pages). `arg_required_else_help` routes a
// bare invocation to the same card. `help_template` drops clap's leading `about`
// line, because the tagline already sits on the header line the binary prints
// above the card. And `bin_name` fixes the program name in the usage line, which
// clap otherwise echoes from the invoked file — `bdinfo-rs.exe` on Windows — so
// the card reads identically on every platform.
//
// NB: no `//!` inner doc comments here — this file is pasted into another module
// by `include!`, where inner attributes are not allowed. Plain `//` only.

use clap::Parser;

/// The parsed `bdinfo-rs` command line.
#[derive(Debug, Parser)]
#[command(
    name = "bdinfo-rs",
    bin_name = "bdinfo-rs",
    version,
    about = "BDInfo-style Blu-ray disc reports",
    arg_required_else_help = true,
    help_template = "{usage-heading} {usage}\n\n{all-args}",
    disable_help_flag = true,
    disable_version_flag = true
)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one bool per independent CLI switch; clap's derive has no other spelling, and the \
              switches are orthogonal — no state machine or two-variant enum fits them"
)]
struct Cli {
    #[arg(value_name = "BD_PATH", help = "BDMV folder or .iso image")]
    bd_path: String,
    #[arg(
        value_name = "REPORT_DEST",
        help = "Report folder (default: BD_PATH; required for .iso)"
    )]
    report_dest: Option<String>,
    #[arg(short = 'l', long, help = "List playlists and exit")]
    list: bool,
    #[arg(
        short = 'm',
        long,
        value_name = "NAME,...",
        value_delimiter = ',',
        help = "Scan only the named playlists"
    )]
    mpls: Vec<String>,
    #[arg(short = 'w', long, help = "Scan every listed playlist")]
    whole: bool,
    #[arg(long, help = "Also list short playlists")]
    show_short_playlists: bool,
    #[arg(long, help = "Also list looping playlists")]
    show_looping_playlists: bool,
    // `hide_default_value` keeps clap from appending its own `[default: 20]`:
    // the help card states the default inline instead, the way REPORT_DEST
    // does, and the appended form would push this line past 80 columns.
    //
    // The `86_400` ceiling (one day; `0` classifies nothing as short)
    // duplicates `bdinfo_rs_core::bdrom::order::MAX_SHORT_PLAYLIST_SECONDS` by
    // necessity: `build.rs` `include!`s this file and has no core
    // build-dependency, so the constant cannot be named here. The test
    // `the_cutoff_ceiling_matches_the_core_constant` in `tests/cli.rs` pins
    // the two equal.
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 20,
        hide_default_value = true,
        value_parser = clap::value_parser!(u32).range(..=86_400),
        help = "Short-playlist cutoff (default: 20)"
    )]
    short_playlist_seconds: u32,
    #[arg(long, help = "Discard partially scanned stream data")]
    drop_partial: bool,
    #[arg(long, help = "Omit the STREAM DIAGNOSTICS sections")]
    no_stream_diagnostics: bool,
    #[arg(long, help = "Omit the QUICK SUMMARY blocks")]
    no_quick_summary: bool,
    #[arg(long, help = "Never print the banner")]
    no_banner: bool,
    #[arg(
        short = 'v',
        long,
        action = clap::ArgAction::Version,
        value_parser = clap::value_parser!(bool),
        help = "Print version"
    )]
    version: Option<bool>,
    #[arg(
        short = 'h',
        long,
        action = clap::ArgAction::HelpShort,
        value_parser = clap::value_parser!(bool),
        help = "Print help"
    )]
    help: Option<bool>,
}
