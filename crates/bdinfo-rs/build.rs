//! Build script: generate the shell completions + man page for the `bdinfo-rs`
//! CLI, derived from the same `clap::Command` the binary parses.
//!
//! WHERE THE ARTIFACTS LAND (the contract `.github/build-setup.yml` — the
//! cargo-dist build-setup step wired in by `dist-workspace.toml`'s
//! `github-build-setup` — and `.github/workflows/core.yml`'s packaging step both
//! rely on; do not change without updating them):
//!
//! Every generated file is written into a single `assets/` directory rooted at
//! this build script's `OUT_DIR`:
//!
//! ```text
//! $OUT_DIR/assets/bdinfo-rs.bash      bash completion
//! $OUT_DIR/assets/_bdinfo-rs          zsh completion   (clap's zsh name)
//! $OUT_DIR/assets/bdinfo-rs.fish      fish completion
//! $OUT_DIR/assets/_bdinfo-rs.ps1      PowerShell completion
//! $OUT_DIR/assets/bdinfo-rs.1         roff man page (section 1)
//! ```
//!
//! `OUT_DIR` is chosen by Cargo and carries a build hash, e.g.
//! `target/<profile>/build/bdinfo-rs-<hash>/out`. Release packaging locates the
//! directory deterministically by globbing
//! `target/<profile>/build/bdinfo-rs-*/out/assets/` (this crate has exactly one
//! such build directory per profile). Writing under `OUT_DIR` — never into the
//! source tree — keeps the build reproducible and the working tree clean.
//!
//! The CLI definition is shared with `src/main.rs` via `include!`: a build
//! script cannot depend on its own crate, so we rebuild the same `clap::Command`
//! from the same `src/cli.rs` the binary parses. That guarantees the completions
//! and the man page can never drift from the shipped flags, help text, or
//! defaults.
//!
//! The man page carries more prose than `--help` does. `src/cli.rs` deliberately
//! holds no `long_about`, `long_help` or `after_long_help` — that is what keeps
//! the help one screen and identical for `-h` and `--help` — so the long-form
//! text lives here instead ([`LONG_ABOUT`], [`EXAMPLES`], [`LONG_HELP`]) and is
//! attached to the shared command by [`man_command`] just before rendering. The
//! completions are generated from the command untouched: they read only the
//! short help.
#![forbid(unsafe_code)]

use std::fs;
use std::io::Error;
use std::path::PathBuf;

use clap::CommandFactory;
use clap_complete::{Shell, generate_to};

// Pull in the exact `Cli` the binary uses (brings `use clap::Parser;` with it).
include!("src/cli.rs");

/// What the tool is, for the man page's DESCRIPTION section.
const LONG_ABOUT: &str = "bdinfo-rs is a memory-safe, cross-platform Blu-ray analyzer — a drop-in \
                          replacement for the classic BDInfo tool. It inspects a Blu-ray (a BDMV \
                          folder or a .iso image) and writes the familiar human-readable disc \
                          report: the playlists and clips, the M2TS stream layout, and the \
                          per-stream video and audio technical specs — codecs, measured bitrates, \
                          resolution, and HDR, Dolby Vision, or HDR10+ metadata.\n\n\
                          It ships as a single statically-linked binary with no runtime, no \
                          shared libraries, and nothing to install: the disc structures, the \
                          codec scanners, and the read-only UDF 2.50 reader for .iso images are \
                          all pure Rust.\n\n\
                          Point BD_PATH at the disc and bdinfo-rs scans its metadata, presents \
                          the playlist selection table, measures the chosen playlists, and writes \
                          BDINFO.<volume label>.txt into REPORT_DEST.";

/// Every exit code the binary returns, for the man page's EXIT STATUS section.
/// `src/main.rs` documents the same list in its crate docs — the copy a reader
/// of the shipped binary never sees, which is why it is restated here.
const EXIT_STATUS: [(&str, &str); 6] = [
    ("0", "Success. A bare invocation prints the help card and lands here too."),
    ("1", "The path holds no readable Blu-ray structure, or no playlist matched --mpls."),
    (
        "2",
        "No such path, an unusable REPORT_DEST, an unwritable report file, or an invalid \
         argument.",
    ),
    (
        "3",
        "The scan completed with errors. Unreadable files were collected into the report's \
         WARNING block and the report was still written.",
    ),
    (
        "4",
        "The disc is AACS-encrypted, so the measured scan was refused before it started — no \
         picker, no scan, no report file — since only ciphertext would be measured. --list still \
         works, its output being entirely database-derived, and exits by its own rules.",
    ),
    (
        "130",
        "The scan was cancelled with Ctrl+C: the Unix 128 + SIGINT spelling, used on every \
         platform. No report is written.",
    ),
];

/// The worked invocations the man page closes with.
const EXAMPLES: &str = "Examples:\n  \
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
    bdinfo-rs /media/MY_MOVIE --whole";

/// The man page's per-argument descriptions, keyed by the derive's argument id
/// — the `Cli` field name. `mut_arg` panics on an id that no longer exists, so
/// a renamed field fails this build rather than silently dropping its prose.
const LONG_HELP: [(&str, &str); 14] = [
    (
        "bd_path",
        "The Blu-ray to analyze. This may be the disc root (the folder containing BDMV), the \
         BDMV folder itself, any folder inside it, or a .iso disc image. Folder input is read \
         through the filesystem; a .iso is read through the in-house pure-Rust UDF 2.50 reader. \
         A folder's disc label is taken from the directory name, while a .iso reports the real \
         UDF volume label, so the same disc can differ on that one line.",
    ),
    (
        "report_dest",
        "The folder the report is written into, as BDINFO.<volume label>.txt. It defaults to \
         BD_PATH, which works for folder input; a .iso image has no folder to fall back on, so a \
         report destination must then be given explicitly. The destination must be an existing \
         directory.",
    ),
    (
        "list",
        "Print the playlist selection table — the standard filtered set of playlists with their \
         length and size — then exit without measuring anything or writing a report.",
    ),
    (
        "mpls",
        "Select playlists by name instead of from the table, as a comma-separated list, for \
         example --mpls 00800,00801. Names are matched case-insensitively and the .MPLS extension may \
         be omitted. The named playlists are scanned in the order given, unfiltered, so this \
         reaches playlists the table would hide. It takes precedence over --whole and the \
         interactive picker.",
    ),
    (
        "whole",
        "Select every playlist shown in the selection table — the standard filtered set — and \
         scan them all, rather than choosing interactively.",
    ),
    (
        "show_short_playlists",
        "Include short playlists — those below the --short-playlist-seconds cutoff — in the \
         selection table. They are hidden by default — a disc's menu, logo and transition \
         playlists are usually short — so this widens what --list prints, what the interactive \
         picker offers, and what --whole scans.",
    ),
    (
        "show_looping_playlists",
        "Include looping playlists — those repeating a play item — in the selection table. They \
         are hidden by default as authoring artefacts, so this widens what --list prints, what \
         the interactive picker offers, and what --whole scans. A disc that disguises its main \
         feature as a loop needs this flag to make that playlist selectable.",
    ),
    (
        "short_playlist_seconds",
        "The length in whole seconds below which a playlist counts as short, 20 by default. A \
         playlist of exactly this length is not short. Valid from 0 to 86400 (one day): 0 \
         classifies nothing as short, and a value past the ceiling is rejected at argument \
         parsing. The cutoff decides what the selection table hides, what the hidden-playlist \
         line names, and therefore what --whole scans; --show-short-playlists lists the short \
         ones anyway. Playlists named with --mpls are never filtered, so the cutoff does not \
         reach them.",
    ),
    (
        "drop_partial",
        "Discard a stream file's partially measured data when a read error interrupts its scan, \
         instead of keeping the measurements — the default, which fills the report's chapter and \
         stream-diagnostics rows for the span read before the failure and leaves the rest zero. \
         The failure is recorded in the report's WARNING block either way. Unreadable sectors of \
         a .iso image are unaffected: they are zero-filled and the scan continues regardless.",
    ),
    (
        "no_stream_diagnostics",
        "Omit each playlist's STREAM DIAGNOSTICS section from the report — the per-clip table of \
         every presented stream with its language, duration, bitrate, byte and packet tallies. It \
         is the widest part of the report, so dropping it leaves something short enough to paste. \
         Nothing else in the report changes.",
    ),
    (
        "no_quick_summary",
        "Omit each playlist's QUICK SUMMARY block from the report — the condensed repeat of the \
         disc, playlist and stream lines that the sections above already carry in full. Nothing \
         else in the report changes.",
    ),
    (
        "no_banner",
        "Never print the bdinfo-rs banner — the wordmark above the help card and above the scan \
         flow. It is only ever printed when stdout is a terminal, so a piped or redirected run \
         needs no flag; this switch silences it on a terminal too.",
    ),
    ("version", "Print the bdinfo-rs version and exit."),
    ("help", "Print the one-screen help card and exit. -h and --help print the same card."),
];

/// The shared command with the long-form prose attached — the man page's input,
/// and the only place that prose exists.
fn man_command() -> clap::Command {
    let mut cmd = Cli::command().long_about(LONG_ABOUT).after_long_help(EXAMPLES);
    for (id, long_help) in LONG_HELP {
        cmd = cmd.mut_arg(id, |arg| arg.long_help(long_help));
    }
    cmd
}

/// [`EXIT_STATUS`] as a roff EXIT STATUS section: one tagged paragraph
/// (`.TP`) per code, the shape `clap_mangen` gives each option.
///
/// Written as roff by hand because `clap_mangen` renders a fixed set of
/// sections from the `clap::Command` and has no seam for another one; the
/// page is therefore assembled section by section in [`main`] with this one
/// spliced in after OPTIONS, where a man page conventionally carries it.
fn exit_status_section() -> Vec<u8> {
    let mut roff = String::from(".SH EXIT STATUS\n");
    for (code, meaning) in EXIT_STATUS {
        roff.push_str(".TP\n\\fB");
        roff.push_str(code);
        roff.push_str("\\fR\n");
        roff.push_str(&roff_escape(meaning));
        roff.push('\n');
    }
    roff.into_bytes()
}

/// `text` as roff running text: the two characters roff reads as markup there
/// are the backslash (an escape sequence) and the hyphen, which renders as a
/// typographic hyphen rather than the minus a flag like `--mpls` needs. No
/// line starts with `.` or `'`, the other two roff-significant positions.
fn roff_escape(text: &str) -> String {
    text.replace('\\', "\\e").replace('-', "\\-")
}

fn main() -> Result<(), Error> {
    // Only regenerate when the CLI surface or this script changes.
    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = std::env::var_os("OUT_DIR").ok_or_else(|| Error::other("OUT_DIR is not set"))?;
    let assets = PathBuf::from(out_dir).join("assets");
    fs::create_dir_all(&assets)?;

    let mut cmd = Cli::command();
    let bin = "bdinfo-rs";

    // Completion scripts for the four shells distro packages ship.
    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
        generate_to(shell, &mut cmd, bin, &assets)?;
    }

    // Man page (section 1, user commands), rendered from the same command with
    // the long-form prose reattached. Assembled section by section rather than
    // through `Man::render`, which renders the same sequence but leaves no room
    // for the hand-written EXIT STATUS section. The command declares a version
    // and no author, so VERSION closes the page and there is no AUTHORS section.
    // Each call writes its own roff document, so the two-line `.ds Aq` preamble
    // clap_mangen prefixes to one now repeats per section — a redefinition of
    // the same string, which renders nothing.
    let man = clap_mangen::Man::new(man_command());
    let mut page = Vec::new();
    man.render_title(&mut page)?;
    man.render_name_section(&mut page)?;
    man.render_synopsis_section(&mut page)?;
    man.render_description_section(&mut page)?;
    man.render_options_section(&mut page)?;
    page.extend_from_slice(&exit_status_section());
    man.render_extra_section(&mut page)?;
    man.render_version_section(&mut page)?;
    fs::write(assets.join("bdinfo-rs.1"), page)?;

    Ok(())
}
