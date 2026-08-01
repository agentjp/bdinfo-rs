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
// The help is one screen: a single short line per argument, and no `long_about`,
// `long_help` or `after_long_help` anywhere. That absence is load-bearing —
// clap renders the short card for `--help` only while no long help exists at
// all (`Command::long_help_exists`), so removing one of those fields silently
// splits `-h` and `--help` into two different pages. The long-form prose lives
// on the man page instead: `build.rs` reattaches it to this same `Command`
// before rendering it, so nothing documented here is lost.
//
// `arg_required_else_help` routes a bare invocation to that same card, and
// `help_template` drops clap's leading `about` line because the tagline already
// sits on the header line the binary prints above the card.
//
// NB: no `//!` inner doc comments here — this file is pasted into another module
// by `include!`, where inner attributes are not allowed. Plain `//` only.

use clap::Parser;

/// The parsed `bdinfo-rs` command line.
#[derive(Debug, Parser)]
#[command(
    name = "bdinfo-rs",
    version,
    about = "BDInfo-style Blu-ray disc reports",
    arg_required_else_help = true,
    help_template = "{usage-heading} {usage}\n\n{all-args}",
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
        help = "Scan only the named playlists (00800,00801)"
    )]
    mpls: Vec<String>,
    #[arg(short = 'w', long, help = "Scan every listed playlist")]
    whole: bool,
    #[arg(long, help = "Also list playlists shorter than 20s")]
    show_short_playlists: bool,
    #[arg(long, help = "Also list looping playlists")]
    show_looping_playlists: bool,
    #[arg(
        short = 'v',
        long,
        action = clap::ArgAction::Version,
        value_parser = clap::value_parser!(bool),
        help = "Print version"
    )]
    version: Option<bool>,
}
