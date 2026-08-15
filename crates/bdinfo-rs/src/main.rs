//! `bdinfo-rs` CLI — drives the `bdinfo-rs-core` library. Terminal only: the
//! desktop window is a separate binary (`bdinfo-rs-gui`).
//!
//! Usage: `bdinfo-rs <BD_PATH> [REPORT_DEST]`.
//!
//! `BD_PATH` is the disc: a directory containing a `BDMV` folder (the disc
//! root, the `BDMV` directory itself, or any directory inside it — the scan
//! walks up to the disc root) or a `.iso` image. A `.iso` path is read through
//! the in-house UDF 2.50 reader ([`bdinfo_rs_core::vfs::udf::source`]); a directory
//! through `std::fs` — the same parsers run over either (the vfs seam). For
//! folder input the disc label is the directory name; `.iso` input reads the
//! real UDF volume label. The one exception is a folder scan whose disc root is
//! a nameless Windows drive root (`J:\`): the directory name is unusable there,
//! so the label is recovered from the raw volume device's UDF descriptor — the
//! same real label — falling back to the drive letter (see
//! [`bdinfo_rs_core::vfs::volume`]).
//!
//! `REPORT_DEST` is the folder the disc report is written to, as
//! `BDINFO.{volume label}.txt`. It defaults to `BD_PATH` and is required when
//! `BD_PATH` is a `.iso` image (there is no folder to default to).
//!
//! The classic console flow: after the metadata scan, every mode but
//! `-m/--mpls` prints the playlist selection table over the standard filtered
//! set (short and looping playlists dropped), followed by a hint line naming
//! what each active filter rule withheld. `--show-short-playlists` and
//! `--show-looping-playlists` turn the matching rule off, widening the table
//! and with it every mode that reads it. `-l/--list` exits after the
//! table; `-w/--whole` selects everything the table lists; `-m/--mpls A,B`
//! selects the named playlists (unfiltered, in the given order) without a
//! table; and the default is the **interactive picker** — table indices typed
//! one per line, `q` (or the input ending) finishes, no selection exits. The
//! selected playlists' stream files are then scanned and the report written.
//! The scan is always **resilient**: a damaged file or sector is recorded,
//! the readable rest is still reported (the failures land in the report's
//! WARNING block and are summarized on stderr), and the live scan progress
//! draws on stderr. On an ANSI terminal that line repaints about once a second
//! even while the scan reports nothing, so the elapsed and remaining readouts
//! keep moving through a read that has not returned; after five seconds of
//! silence its lead-in reads `Still reading` instead of `Scanning`. The flow
//! narration (table, picker, analysis preamble,
//! classic epilogue, saved-report message) prints on stdout. A stream file
//! measured shorter than the disc declares (a truncated file reads to a clean
//! end of file, so it records no error) gets a stderr notice after the report
//! is saved; the report bytes and the exit code are unchanged by it.
//!
//! `-h`/`--help` and a bare invocation all print the same one-screen help
//! card, headed by a banner, a colour chip or a plain line depending on what
//! the terminal can render (see [`banner`]); the same header opens every scan
//! run whose stdout is a terminal. `--no-banner` drops it everywhere, and a
//! piped or redirected run never prints it at all. The long-form documentation
//! is the man page `build.rs` generates from the same command.
//!
//! Exit codes: `0` success (a bare invocation prints the help and lands here
//! too), `1` malformed/not a BD structure or no matching playlist, `2` no such
//! path / unusable `REPORT_DEST` / unwritable report file / an invalid
//! argument, `3` completed with errors (a partial report was written), `4`
//! AACS-encrypted disc — the scan is refused up front (no picker, no scan, no
//! report file), since only ciphertext would be measured; `--list` still
//! works, its output being entirely database-derived, and exits by its own
//! rules with the same stderr notice, `130` scan cancelled by Ctrl+C (the
//! Unix `128 + SIGINT` spelling, used on every platform; no report is
//! written).
//!
//! Ctrl+C during the packet scan, on the styled (ANSI-terminal) progress
//! path, cancels cooperatively: the terminal is put in raw mode for the scan
//! so the press arrives as a key event, the first press aborts the scan
//! cleanly (`Cancelling...` notice, cursor restored, exit `130`), and a
//! second press while the cancellation is in flight exits at once. The plain
//! path (piped stderr, CI) and every other phase of the flow keep the
//! OS-default Ctrl+C. Only the interactive key press is intercepted —
//! `kill`/SIGTERM stay OS-default: the scan is read-only and the report is
//! only written at the end, so a hard kill corrupts nothing.
#![forbid(unsafe_code)]
// Under `cargo llvm-cov` on nightly (the `cov` gate step), enable the
// `#[coverage(off)]` attribute so the platform-constant `ansi_supported` stub
// can be excluded. Inert on stable (the cfg is set only by cargo-llvm-cov), and
// the cfg itself is registered in the workspace `check-cfg` lint.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal as _, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bdinfo_rs_core::bdrom::chapters::seconds_to_ticks;
use bdinfo_rs_core::bdrom::disc::{
    BdRom, HIDDEN_STREAMS_NOTE, PlaylistSummary, ScanMode, ScanOptions, ScanProgress,
};
use bdinfo_rs_core::bdrom::order::{
    HiddenRule, PlaylistFilter, hidden_by, named_selection, selection_order,
    selection_stream_files, table_rows,
};
use bdinfo_rs_core::bdrom::progress::{hms, progress_stats};
use bdinfo_rs_core::bdrom::shortfall::ShortStreamFile;
use bdinfo_rs_core::error::{BdError, ScanError};
use bdinfo_rs_core::report::text::{self, RenderOptions};
use bdinfo_rs_core::{report, scan};
use clap::error::ErrorKind;
use crossterm::style::Stylize as _;
use crossterm::{Command as _, cursor, terminal};

// The clap `Cli` definition lives in its own file so `build.rs` can `include!`
// the same source to generate the shell completions + man page (a build script
// cannot depend on its own crate). Included here at crate root, `Cli` and its
// private fields are in scope exactly as if declared inline. `cli.rs` brings
// `use clap::Parser;` along with it, so `Cli::parse()` below resolves.
include!("cli.rs");

mod banner;

fn main() -> ExitCode {
    match Cli::try_parse() {
        Ok(cli) => ExitCode::from(run(&cli)),
        Err(err) => ExitCode::from(report_parse_failure(&err)),
    }
}

/// Prints what clap could not parse, and returns the process exit code.
///
/// The two help kinds — `-h`/`--help`, and the bare invocation
/// `arg_required_else_help` answers with that same card — print the
/// capability-chosen header above clap's card on stdout and exit `0`. Every
/// other kind, `--version` included, is clap's own output on clap's own stream,
/// with clap's own exit code.
fn report_parse_failure(err: &clap::Error) -> u8 {
    if matches!(
        err.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        print!("{}", help_page(&banner::Capabilities::probe(), err, !banner_declined()));
        // A bare run is a help request, not a usage error, so it exits 0 where
        // clap would exit 2: package-manager install validators smoke-run the
        // executable with no arguments, and a non-zero exit there reads as a
        // broken install (CHANGELOG.md, v1.0.1). An actual but invalid argument
        // still takes the usage-error path below.
        return 0;
    }
    let _ = err.print().is_ok();
    u8::try_from(err.exit_code()).unwrap_or(2)
}

/// Whether `--no-banner` is on the command line, read from the raw arguments:
/// the help paths answer before clap has produced a `Cli` to ask, and the
/// switch is a plain flag, so its presence is the whole question.
fn banner_declined() -> bool {
    std::env::args_os().any(|arg| arg == "--no-banner")
}

/// The help page as printed: the header for `caps`, a blank line, then clap's
/// rendered card — or the card alone when `header` is false. The card is
/// ANSI-styled only where `caps` allows colour, so a piped or `NO_COLOR` run
/// carries no escape byte at all, header and card alike.
fn help_page(caps: &banner::Capabilities, err: &clap::Error, header: bool) -> String {
    let card = err.render();
    let card = if caps.colors() { card.ansi().to_string() } else { card.to_string() };
    if !header {
        return card;
    }
    format!("{}\n{card}", banner::header(caps, env!("CARGO_PKG_VERSION")))
}

/// Whether `path` is an `.iso` image (a file with the `.iso` extension,
/// case-insensitive) — such a path dispatches to the UDF reader. A directory is
/// read through `std::fs`.
fn is_iso(path: &Path) -> bool {
    path.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("iso"))
}

/// A completed scan: the (possibly partial) disc plus every recorded failure —
/// empty on healthy media.
type ScanOutcome = (BdRom, Vec<ScanError>);

/// The injectable packet-scan seam of [`scan_and_report`]: runs the scan,
/// feeding the given progress observer and honouring the given cooperative
/// cancel flag.
type ScanFn<'a> =
    dyn FnMut(&mut dyn FnMut(ScanProgress<'_>), &AtomicBool) -> Result<ScanOutcome, BdError> + 'a;

/// The CLI's scan depth: it only ever wants the metadata list or the full
/// measured report, so `run_packet_scan` maps to [`ScanMode::Full`] or
/// [`ScanMode::Metadata`] (the bounded [`ScanMode::Codecs`] is a GUI affordance).
const fn scan_mode(run_packet_scan: bool) -> ScanMode {
    if run_packet_scan { ScanMode::Full } else { ScanMode::Metadata }
}

/// The no-op progress observer for the metadata scan — it never runs the
/// packet scan, so no display fires.
const fn no_progress(_: ScanProgress<'_>) {}

/// The library's [`ScanOptions`] for the parsed command line: `--drop-partial`
/// switches off the default retention of what a damaged stream file's scan
/// measured before its failing read.
fn scan_options(cli: &Cli) -> ScanOptions {
    let mut options = ScanOptions::default();
    options.keep_partial = !cli.drop_partial;
    options
}

/// Dispatches `path` to the `.iso` or folder scan. `options` carries the
/// behavior switches ([`scan_options`]), `scan_files` narrows the packet scan,
/// `progress` observes it, and `cancel` aborts it (the library's `open_with`
/// extras).
fn scan_disc(
    path: &str,
    run_packet_scan: bool,
    options: ScanOptions,
    scan_files: Option<&BTreeSet<String>>,
    progress: &mut dyn FnMut(ScanProgress<'_>),
    cancel: &AtomicBool,
) -> Result<ScanOutcome, BdError> {
    let location = Path::new(path);
    let mode = scan_mode(run_packet_scan);
    let report = if is_iso(location) {
        scan::open_iso(location, mode, options, scan_files, progress, cancel)
    } else {
        scan::open_folder(location, mode, options, scan_files, progress, cancel)
    }?;
    Ok((report.bdrom, report.errors))
}

/// Prints the "scan completed with errors" stderr summary — one line per recorded
/// failure (the always-report discipline; stdout stays clean).
fn report_errors(errors: &[ScanError]) {
    eprintln!("warning: scan completed with {} error(s):", errors.len());
    for err in errors {
        eprintln!("warning:   {err}");
    }
}

/// The stderr line for one stream file the demux measured shorter than the disc
/// declares: the CLI's `warning: ` prefix over [`ShortStreamFile::notice`],
/// which is where that sentence is worded for every surface.
///
/// Raised beside the report, because the locked report format has no line for
/// it ([`bdinfo_rs_core::bdrom::shortfall`]).
fn short_stream_line(short: &ShortStreamFile) -> String {
    format!("warning: {}", short.notice())
}

/// Prints the short-stream notices on stderr, one line per affected file —
/// nothing for a disc with none.
///
/// Notices are advisory: unlike [`report_errors`] they carry no exit code,
/// because the scan itself completed cleanly (a truncated file reads to a
/// clean end of file).
fn report_short_streams(bdrom: &BdRom) {
    for short in bdrom.short_stream_files() {
        eprintln!("{}", short_stream_line(&short));
    }
}

/// Executes the parsed CLI, returning the process exit code.
///
/// The classic console flow: validate `BD_PATH`, resolve and validate
/// `REPORT_DEST`, metadata-scan the disc, select playlists (`--mpls` by name;
/// the playlist table plus everything-listed for `--whole`/`--list`, or the
/// interactive picker), then packet-scan the selection and write the report.
fn run(cli: &Cli) -> u8 {
    let bd_path = Path::new(&cli.bd_path);
    if !bd_path.exists() {
        eprintln!("error: {} does not exist", cli.bd_path);
        return 2;
    }
    let dest = match resolve_dest(cli, bd_path) {
        Ok(dest) => dest,
        Err(code) => return code,
    };

    // After the argument checks, so a bad path stays a bare error message.
    print!(
        "{}",
        banner::session_header(
            &banner::Capabilities::probe(),
            env!("CARGO_PKG_VERSION"),
            !cli.no_banner,
        )
    );
    println!("Please wait while we scan the disc...");
    let options = scan_options(cli);
    let (bdrom, errors) = match scan_disc(
        &cli.bd_path,
        false,
        options,
        None,
        &mut no_progress,
        &AtomicBool::new(false),
    ) {
        Ok(scanned) => scanned,
        Err(err) => {
            eprintln!("error: {err}");
            return 1;
        }
    };
    if bdrom.is_aacs_encrypted {
        // One dry notice on stderr either way; only `--list` continues — its
        // output is entirely database-derived, so it is correct on an
        // encrypted disc, while a measured scan would demux ciphertext.
        eprintln!(
            "Disc is AACS-encrypted: stream data cannot be analyzed. Playlists can still be \
             listed with --list."
        );
        if !cli.list {
            return EXIT_ENCRYPTED;
        }
    }

    let selection: Vec<String> = if cli.mpls.is_empty() {
        // Every mode but `--mpls` starts from the playlist table, over the
        // filtered set the `--show-*-playlists` switches widen.
        let filter = PlaylistFilter {
            filter_short_playlists: !cli.show_short_playlists,
            short_playlist_seconds: f64::from(cli.short_playlist_seconds),
            filter_looping_playlists: !cli.show_looping_playlists,
        };
        let rows = table_rows(&bdrom.playlists, &filter);
        print!("{}", selection_table(&bdrom.playlists, &rows));
        if rows
            .iter()
            .any(|&(_, i)| bdrom.playlists.get(i).is_some_and(PlaylistSummary::has_hidden_streams))
        {
            println!("{HIDDEN_STREAMS_NOTE}");
        }
        print!("{}", hidden_hint(&bdrom.playlists, &filter));
        if cli.whole || cli.list {
            row_names(&bdrom.playlists, &rows, &(1..=rows.len()).collect::<Vec<_>>())
        } else {
            let picks =
                pick_playlists(rows.len(), &mut std::io::stdin().lock(), &mut std::io::stdout());
            if picks.is_empty() {
                println!("No playlists selected. Exiting.");
                return finish_early(&errors);
            }
            row_names(&bdrom.playlists, &rows, &picks)
        }
    } else {
        // `--mpls`: echo the requested list, then select by name in the given
        // order — no table, no filtering (any parseable playlist is
        // addressable by name).
        println!("{}", cli.mpls.join(","));
        let names = named_selection(&bdrom.playlists, &cli.mpls);
        if names.is_empty() {
            eprintln!("error: No matching playlists found on BD");
            return 1;
        }
        names
    };
    if cli.list {
        return finish_early(&errors);
    }

    print!("{}", analyze_preamble(&bdrom.playlists, &selection));
    let scan_files = selection_stream_files(&bdrom.playlists, &selection);
    scan_and_report(
        &mut |progress, cancel| {
            scan_disc(&cli.bd_path, true, options, Some(&scan_files), progress, cancel)
        },
        &dest,
        &selection,
        RenderOptions {
            stream_diagnostics: !cli.no_stream_diagnostics,
            quick_summary: !cli.no_quick_summary,
        },
    )
}

/// The post-selection phase of [`run`]: packet-scans through `scan` with the
/// live progress display, renders under `options`, writes the report into
/// `dest` in selection order, and returns the exit code. `scan` is injectable
/// so the fatal-scan-failure path is testable (a structure that survived the
/// metadata scan rarely fails the packet scan, but the disc can vanish
/// between the two).
fn scan_and_report(
    scan: &mut ScanFn<'_>,
    dest: &Path,
    selection: &[String],
    options: RenderOptions,
) -> u8 {
    let display = Arc::new(Mutex::new(ProgressDisplay::new()));
    let styled = locked(&display).styled;
    // The scan's Ctrl+C affordance, styled path only: raw mode makes the press
    // arrive as a key event instead of killing the process, the per-tick poll
    // turns the first press into the cooperative cancel and a second into the
    // immediate exit. The plain path (piped stderr, CI, dumb terminals) keeps
    // the OS-default Ctrl+C — there is no hidden cursor to leak there.
    let cancel = AtomicBool::new(false);
    let mut watch = CancelWatch::new();
    let raw = RawModeScope::enter(styled);
    let ticker = StallTicker::start(&display);
    let scanned = scan(
        &mut |p: ScanProgress<'_>| {
            observe_cancellable(&mut locked(&display), &mut watch, &cancel, raw.active, &p);
        },
        &cancel,
    );
    ticker.stop();
    // Out of raw mode before anything prints a plain `\n` (Unix raw mode
    // disables output post-processing, so a bare newline would not return
    // the carriage).
    drop(raw);
    // The display is locked per call, not for the rest of the run: the report
    // is rendered and written with nothing holding the terminal line.
    match scanned {
        Ok((bdrom, errors)) => {
            locked(&display).finish(errors.is_empty());
            println!("Please wait while we generate the report...");
            let order = selection_order(&bdrom.playlists, selection);
            let rendered = text::render_with(&bdrom, &order, &errors, options);
            if let Err(code) = save_report(dest, &bdrom, &rendered) {
                return code;
            }
            println!("Report saved to: {}", dest.display());
            report_short_streams(&bdrom);
            finish_early(&errors)
        }
        Err(BdError::ScanCancelled) => {
            locked(&display).clear();
            eprintln!("Scan cancelled.");
            EXIT_CANCELLED
        }
        Err(err) => {
            locked(&display).clear();
            eprintln!("error: {err}");
            1
        }
    }
}

/// The cancelled-scan exit code: the Unix `128 + SIGINT` spelling, distinct
/// from `3` (scan errors) and recognizable everywhere — it carries no special
/// meaning on Windows but is documented in the crate docs' exit-code list.
const EXIT_CANCELLED: u8 = 130;

/// The AACS-refusal exit code: the disc opened fine but its stream content is
/// encrypted ([`BdRom::is_aacs_encrypted`]), so the measured scan was refused
/// before any selection. Distinct from `1` (the disc did not open) and `3`
/// (the disc was scanned, with damage).
const EXIT_ENCRYPTED: u8 = 4;

/// The Ctrl+C interception decision for one observed press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelStep {
    /// The first press: signal the cooperative cancel and show the notice.
    Cancel,
    /// A repeat press while the cancellation is in flight: exit immediately.
    Exit,
}

/// The once/twice Ctrl+C state machine of the styled scan path — pure, so
/// the decision logic is testable apart from the terminal event source.
struct CancelWatch {
    /// Whether the first press has already signalled the cancel.
    cancelling: bool,
}

impl CancelWatch {
    const fn new() -> Self {
        Self { cancelling: false }
    }

    /// Whether a cancellation is in flight (the display then holds the
    /// `Cancelling...` notice instead of the bar).
    const fn cancelling(&self) -> bool {
        self.cancelling
    }

    /// Records one Ctrl+C press, returning the action it triggers.
    const fn press(&mut self) -> CancelStep {
        if self.cancelling {
            CancelStep::Exit
        } else {
            self.cancelling = true;
            CancelStep::Cancel
        }
    }
}

/// One tick of the cancellable scan observer: a Ctrl+C press signals the
/// cooperative cancel (first press) or requests the immediate exit (second
/// press, returning `true`); otherwise the progress display redraws — unless
/// a cancellation is already in flight, whose notice must not be overdrawn.
fn scan_tick(
    line: &mut ProgressDisplay,
    watch: &mut CancelWatch,
    cancel: &AtomicBool,
    pressed: bool,
    progress: &ScanProgress<'_>,
) -> bool {
    if pressed {
        match watch.press() {
            CancelStep::Cancel => {
                cancel.store(true, Ordering::Relaxed);
                line.notice("Cancelling...");
            }
            CancelStep::Exit => {
                line.clear();
                return true;
            }
        }
    } else if !watch.cancelling() {
        line.observe(progress);
    }
    false
}

/// The cancellable scan's progress observer: one [`scan_tick`] per event,
/// with the environment-bound edges wired in — the live Ctrl+C poll on the
/// way in, the process-fatal immediate exit on the way out. The decision
/// logic is [`scan_tick`] (unit-tested); this composition is excluded from
/// coverage with its edges, which no test process can drive.
#[cfg_attr(coverage_nightly, coverage(off))]
fn observe_cancellable(
    line: &mut ProgressDisplay,
    watch: &mut CancelWatch,
    cancel: &AtomicBool,
    intercepting: bool,
    progress: &ScanProgress<'_>,
) {
    if scan_tick(line, watch, cancel, ctrl_c_pressed(intercepting), progress) {
        exit_cancelled_now();
    }
}

/// Drains every pending terminal event (a zero-timeout poll per pending
/// event), reporting whether any was a Ctrl+C key press. Only asked on an
/// active raw-mode scan, where Ctrl+C arrives as a key event on both console
/// families: Windows reports it as keyboard input once `ENABLE_PROCESSED_INPUT`
/// is off, Unix delivers the raw `0x03` byte once `ISIG` is off — crossterm's
/// raw mode clears both, and its parser yields the same `KeyEvent` either way.
/// The Windows console also delivers key-release records; only presses count.
/// Environment-bound (a live console delivering key events), so excluded from
/// coverage like [`terminal_columns`].
#[cfg_attr(coverage_nightly, coverage(off))]
fn ctrl_c_pressed(intercepting: bool) -> bool {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    if !intercepting {
        return false;
    }
    let mut pressed = false;
    while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
        // A read error ends the drain — nothing more can arrive through it.
        let Ok(event) = crossterm::event::read() else {
            break;
        };
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            pressed = true;
        }
    }
    pressed
}

/// Restores the terminal and exits with the cancellation code at once — the
/// second Ctrl+C must not wait for the in-flight scan to unwind. The caller's
/// [`ProgressDisplay::clear`] has already re-shown the cursor; this drops raw
/// mode (the RAII scope up the stack never unwinds through `exit`).
/// Environment-bound and process-fatal, so excluded from coverage.
#[cfg_attr(coverage_nightly, coverage(off))]
fn exit_cancelled_now() -> ! {
    let _ = terminal::disable_raw_mode().is_ok();
    #[expect(
        clippy::exit,
        reason = "the immediate second-Ctrl+C exit cannot return through the in-flight scan"
    )]
    std::process::exit(i32::from(EXIT_CANCELLED))
}

/// Scoped terminal raw mode for the styled scan: entering (when `wanted`)
/// makes Ctrl+C arrive as a key event instead of killing the process, and
/// dropping guarantees the terminal is restored on every exit path, success
/// or failure. A failed enable (no usable console) leaves `active` false —
/// the scan then runs with the OS-default Ctrl+C, exactly like the plain
/// path. Raw mode also disables echo and input line-processing; the scan
/// reads no input, and the pending-event drain discards anything typed.
struct RawModeScope {
    /// Whether raw mode was actually enabled (and must be undone on drop).
    active: bool,
}

impl RawModeScope {
    /// Enters raw mode when `wanted` (the styled path). Environment-bound —
    /// a test process must never toggle its console — so excluded from
    /// coverage like [`terminal_columns`].
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn enter(wanted: bool) -> Self {
        Self { active: wanted && terminal::enable_raw_mode().is_ok() }
    }
}

impl Drop for RawModeScope {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn drop(&mut self) {
        if self.active {
            let _ = terminal::disable_raw_mode().is_ok();
        }
    }
}

/// The exit code of a flow that wrote no report or just finished one: clean
/// is `0`; recorded scan failures print the stderr summary and exit `3`.
fn finish_early(errors: &[ScanError]) -> u8 {
    if errors.is_empty() {
        0
    } else {
        report_errors(errors);
        3
    }
}

/// Resolves and validates the report destination folder: `REPORT_DEST` when
/// given, else `BD_PATH` itself — which only works for folder input, so a
/// `.iso` `BD_PATH` requires an explicit `REPORT_DEST`. Either way the
/// destination must be an existing directory. Returns the exit code (`2`) on
/// a violation.
fn resolve_dest(cli: &Cli, bd_path: &Path) -> Result<PathBuf, u8> {
    let dest = if let Some(dir) = &cli.report_dest {
        PathBuf::from(dir)
    } else {
        if !bd_path.is_dir() {
            eprintln!("error: REPORT_DEST must be given if BD_PATH is an ISO.");
            return Err(2);
        }
        bd_path.to_path_buf()
    };
    if !dest.is_dir() {
        eprintln!("error: {} does not exist or is not a directory", dest.display());
        return Err(2);
    }
    Ok(dest)
}

/// How many playlist names a hint line spells out before it counts the rest —
/// enough to recognize the disc's numbering, short enough to stay on one line
/// on an 80-column console.
const HINT_NAMES: usize = 3;

/// The hint block under the selection table: one line per filter rule that is
/// on *and* withheld at least one playlist, looping first, then short. Empty
/// (so the flow's bytes are unchanged) when every rule is off or hid nothing.
///
/// Each rule is judged on its own, not against what the table dropped, so a
/// playlist that is both short and looping is named on both lines and is only
/// revealed when both switches are passed.
///
/// Console narration only: it is never written into the report, and `--mpls`
/// mode — which prints no table — never reaches it.
fn hidden_hint(playlists: &[PlaylistSummary], filter: &PlaylistFilter) -> String {
    let mut out = String::new();
    if filter.filter_looping_playlists {
        out.push_str(&hint_line(
            playlists,
            filter,
            HiddenRule::Looping,
            "--show-looping-playlists",
        ));
    }
    if filter.filter_short_playlists {
        out.push_str(&hint_line(playlists, filter, HiddenRule::Short, "--show-short-playlists"));
    }
    out
}

/// One `Hidden by filters (KIND): NAMES - rerun with FLAG` line for the
/// playlists [`hidden_by`] reports under `rule`, empty when it withheld none.
/// The names keep that function's table order (length descending, then name
/// ascending), the first [`HINT_NAMES`] spelled out and the rest counted as
/// ` and N more`.
///
/// ASCII only — the console flow must survive a legacy OEM codepage, so a
/// plain hyphen stands where prose would take a dash.
fn hint_line(
    playlists: &[PlaylistSummary],
    filter: &PlaylistFilter,
    rule: HiddenRule,
    flag: &str,
) -> String {
    let hidden: Vec<&str> = hidden_by(playlists, filter)
        .into_iter()
        .filter(|(_, rules)| rules.contains(&rule))
        .map(|(playlist, _)| playlist.name.as_str())
        .collect();
    if hidden.is_empty() {
        return String::new();
    }
    let named: Vec<&str> = hidden.iter().take(HINT_NAMES).copied().collect();
    let rest = hidden.len().saturating_sub(HINT_NAMES);
    let more = if rest == 0 { String::new() } else { format!(" and {rest} more") };
    format!(
        "Hidden by filters ({}): {}{more} - rerun with {flag}\n",
        rule.label(),
        named.join(", ")
    )
}

/// Maps 1-based table indices (`picks`) back to playlist names, in pick
/// order; an out-of-range pick maps to nothing (the picker never produces
/// one).
fn row_names(
    playlists: &[PlaylistSummary],
    rows: &[(usize, usize)],
    picks: &[usize],
) -> Vec<String> {
    picks
        .iter()
        .filter_map(|&pick| rows.get(pick.wrapping_sub(1)))
        .filter_map(|&(_, index)| playlists.get(index).map(|p| p.name.clone()))
        .collect()
}

/// Composes the playlist selection table: the `#`/Group/Playlist
/// File/Length/Estimated Bytes/Measured Bytes header (followed by a blank
/// line) and one row per listed playlist. Both byte columns read `-` when the
/// size is not known — estimated per
/// [`PlaylistSummary::estimated_bytes`](bdinfo_rs_core::bdrom::disc::PlaylistSummary::estimated_bytes),
/// measured until a packet scan has measured the playlist.
fn selection_table(playlists: &[PlaylistSummary], rows: &[(usize, usize)]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<4}{:<7}{:<15}{:<10}{:<16}{:<16}\n",
        "#", "Group", "Playlist File", "Length", "Estimated Bytes", "Measured Bytes"
    );
    for (position, &(group, index)) in rows.iter().enumerate() {
        let Some(playlist) = playlists.get(index) else {
            continue;
        };
        let cell = |bytes: u64| text::group(u128::from(bytes));
        let estimated = playlist.estimated_bytes().map_or_else(|| "-".to_owned(), cell);
        let measured = match playlist.total_packet_size() {
            0 => "-".to_owned(),
            bytes => cell(bytes),
        };
        let _ = writeln!(
            out,
            "{:<4}{:<7}{:<15}{:<10}{:<16}{:<16}",
            position.saturating_add(1),
            group,
            playlist.name,
            text::time_hh_short(seconds_to_ticks(playlist.total_length)),
            estimated,
            measured
        );
    }
    out
}

/// The interactive playlist picker: prompts `Select (q when finished): `
/// until `q` (or the input ending, which counts as `q`), collecting 1-based
/// table indices in pick order — duplicates allowed, like the classic picker.
/// A non-number answers `Invalid Input!`; an out-of-range number `Invalid
/// Selection!`; a valid pick a blank line and `Added N`.
fn pick_playlists(max: usize, input: &mut dyn BufRead, output: &mut dyn Write) -> Vec<usize> {
    let mut picks = Vec::new();
    loop {
        let _ = write!(output, "Select (q when finished): ").is_ok();
        let _ = output.flush().is_ok();
        let mut line = String::new();
        let Ok(read) = input.read_line(&mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        let answer = line.trim();
        if answer == "q" {
            break;
        }
        let Ok(number) = answer.parse::<usize>() else {
            let _ = writeln!(output, "Invalid Input!").is_ok();
            continue;
        };
        if number == 0 || number > max {
            let _ = writeln!(output, "Invalid Selection!").is_ok();
            continue;
        }
        let _ = writeln!(output).is_ok();
        let _ = writeln!(output, "Added {number}").is_ok();
        picks.push(number);
    }
    picks
}

/// The analysis preamble: `Preparing to analyze the following:` plus one
/// `NAME --> file + file` line per selected playlist, each stream file
/// claimed by the first playlist that uses it (later playlists don't repeat
/// it).
fn analyze_preamble(playlists: &[PlaylistSummary], selection: &[String]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "Preparing to analyze the following:");
    let mut claimed = BTreeSet::new();
    for name in selection {
        let Some(playlist) = playlists.iter().find(|p| &p.name == name) else {
            continue;
        };
        let mut files = Vec::new();
        for clip in &playlist.clips {
            if claimed.insert(clip.name.clone()) {
                files.push(clip.name.clone());
            }
        }
        let _ = writeln!(out, "{name} --> {}", files.join(" + "));
    }
    out
}

/// Writes `rendered` into `dest` as `BDINFO.{volume label}.txt` (CRLF, UTF-8,
/// no BOM). Returns the exit code on a failed write (`2`).
fn save_report(dest: &Path, bdrom: &BdRom, rendered: &str) -> Result<(), u8> {
    let target = dest.join(report::file_name(&bdrom.volume_label));
    std::fs::write(&target, rendered.as_bytes()).map_err(|err| {
        eprintln!("error: cannot write {}: {err}", target.display());
        2
    })
}

/// The display, locked.
///
/// A panic while painting would poison the mutex; a progress line is worth
/// neither losing the scan to nor a second panic, so the recovered display is
/// used as it stands.
fn locked(display: &Mutex<ProgressDisplay>) -> MutexGuard<'_, ProgressDisplay> {
    display.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A second thread repainting the display about once a second
/// ([`ProgressDisplay::tick`]) for as long as a scan is running.
///
/// The scan drives the display from its own thread, one event per read — so a
/// read syscall that does not return freezes the line exactly when the reader
/// most wants to see it moving (damaged optical media can hold a read inside
/// drive-firmware retries for minutes). The ticker holds the wall clock
/// instead: it owns no scan state and only repaints what the last event left.
struct StallTicker {
    /// Set by [`stop`](Self::stop) to end the thread.
    stop: Arc<AtomicBool>,
    /// The thread, or `None` for a plain display: a plain line never ticks
    /// (see [`ProgressDisplay::tick`]), so a piped or redirected run spawns
    /// nothing at all.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StallTicker {
    /// Starts ticking `display`.
    fn start(display: &Arc<Mutex<ProgressDisplay>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        if !locked(display).styled {
            return Self { stop, thread: None };
        }
        let thread = {
            let display = Arc::clone(display);
            let stop = Arc::clone(&stop);
            // The wait is parked rather than slept, so the unpark in `stop`
            // ends the thread at once instead of after the rest of a poll
            // interval — the CLI runs a scan per invocation, and that wait
            // would be paid on every one of them.
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::park_timeout(TICK_POLL);
                    locked(&display).tick();
                }
            })
        };
        Self { stop, thread: Some(thread) }
    }

    /// Ends the ticking and waits for the thread to finish, so nothing repaints
    /// the line the epilogue is about to print over.
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(thread) = self.thread else {
            return;
        };
        thread.thread().unpark();
        // A panicked ticker leaves the display as the scan left it; the
        // epilogue still prints.
        let _ = thread.join().is_ok();
    }
}

/// The widest the styled display's progress bar grows, in cells.
const BAR_MAX_CELLS: usize = 24;

/// The narrowest bar still worth drawing; less room than this falls back to
/// the plain line.
const BAR_MIN_CELLS: usize = 8;

/// The redraw throttle: the fastest the display repaints on progress events.
const DRAW_EVERY: Duration = Duration::from_millis(100);

/// How often the styled display repaints itself while no progress event
/// arrives, so the elapsed and remaining readouts keep moving through a read
/// that has not returned. One second is the rate classic `BDInfo` refreshes its
/// own progress at.
const TICK_EVERY: Duration = Duration::from_secs(1);

/// How often the ticker thread wakes to ask whether a repaint is due — short
/// enough that a finished scan is not held up waiting for it to sleep out.
const TICK_POLL: Duration = Duration::from_millis(100);

/// How long the scan may go without a progress event before the display calls
/// the current read stalled. Progress arrives many times a second (a heartbeat
/// before and a count after every read), so a multi-second silence means one
/// read syscall has not returned — damaged optical media held in drive-firmware
/// retries, which can block for minutes with no error.
const STALL_AFTER: Duration = Duration::from_secs(5);

/// The progress line's lead-in while reads are arriving.
const LEAD_SCANNING: &str = "Scanning";

/// The lead-in once the reads have gone quiet ([`STALL_AFTER`]) — the same
/// "still reading" wording the desktop shell shows for a stuck read.
const LEAD_STALLED: &str = "Still reading";

/// The lead-in for a quiet or a moving scan.
const fn lead_in(stalled: bool) -> &'static str {
    if stalled { LEAD_STALLED } else { LEAD_SCANNING }
}

/// Whether reads have gone quiet long enough to call the current one stalled:
/// [`STALL_AFTER`] since the last progress event, inclusive.
fn stalled(since_progress: Duration) -> bool {
    since_progress >= STALL_AFTER
}

/// Whether a wall-clock repaint is due: a whole [`TICK_EVERY`] since the last
/// paint, inclusive.
fn tick_due(since_paint: Duration) -> bool {
    since_paint >= TICK_EVERY
}

/// Where the display's painted bytes go: stderr in a real run
/// ([`write_stderr`]), a recorder under test.
type Ink = Box<dyn FnMut(&str) + Send>;

/// The last progress event, retained so a wall-clock tick can repaint the line
/// without one.
#[derive(Clone)]
struct LastProgress {
    /// The stream file that event named.
    file: String,
    /// The bytes read as of that event.
    done: u64,
    /// The bytes the pass will read, as of that event.
    total: u64,
}

/// The live scan progress display on stderr, redrawn in place at most every
/// 100 ms (and always at completion), then erased and replaced by the classic
/// epilogue. A terminal that renders ANSI sequences gets a styled line with a
/// progress bar; a piped or redirected stderr gets the plain `\r`-rewritten
/// line — same information either way. stdout never sees any of it.
///
/// The styled line is additionally repainted on a wall clock
/// ([`tick`](Self::tick), driven by [`with_stall_ticker`]), because progress
/// events stop entirely while a read syscall is stuck.
struct ProgressDisplay {
    /// Whether stderr is a terminal that renders ANSI sequences.
    styled: bool,
    /// When the scan started — drives the elapsed/remaining estimates.
    start: Instant,
    /// The last redraw, for throttling.
    drawn: Option<Instant>,
    /// The widest plain line drawn so far, so a shorter redraw still
    /// overwrites. Unused in styled mode — the erase sequence wipes the line.
    width: usize,
    /// The last progress event, or `None` before the first one and after the
    /// display is erased — a tick has nothing to repaint in either case.
    last: Option<LastProgress>,
    /// When that event arrived, for the stall verdict ([`stalled`]).
    evented: Option<Instant>,
    /// Whether a [`notice`](Self::notice) owns the line: a tick must not
    /// overdraw the `Cancelling...` message with the bar it replaced.
    holding: bool,
    /// The terminal width the styled line is composed for, asked per paint so a
    /// resized window is followed. [`terminal_columns`] in a real run; a test
    /// pins a width instead of depending on the console it runs under.
    columns: fn() -> usize,
    /// Where the painted bytes go.
    ink: Ink,
}

impl ProgressDisplay {
    fn new() -> Self {
        Self::with_style(stderr_styles())
    }

    /// A display with the styled/plain choice forced (the testable seam;
    /// [`new`](Self::new) detects it from stderr).
    fn with_style(styled: bool) -> Self {
        Self::with_ink(styled, Box::new(write_stderr))
    }

    /// A display painting into `ink` instead of stderr — the seam a test reads
    /// the painted lines back through.
    fn with_ink(styled: bool, ink: Ink) -> Self {
        Self {
            styled,
            start: Instant::now(),
            drawn: None,
            width: 0,
            last: None,
            evented: None,
            holding: false,
            columns: terminal_columns,
            ink,
        }
    }

    /// Observes one scan-progress event, redrawing the display when due.
    fn observe(&mut self, progress: &ScanProgress<'_>) {
        self.evented = Some(Instant::now());
        self.last = Some(LastProgress {
            file: progress.file.to_owned(),
            done: progress.done,
            total: progress.total,
        });
        let due = progress.done == progress.total
            || self.drawn.is_none_or(|at| at.elapsed() >= DRAW_EVERY);
        if due {
            self.paint(progress, false);
        }
    }

    /// Repaints the styled line from the last progress event when a whole
    /// [`TICK_EVERY`] has passed without one, so the elapsed clock climbs and
    /// the remaining estimate grows through a read that has not returned; past
    /// [`STALL_AFTER`] of silence the lead-in says the file is still being
    /// read rather than implying normal progress.
    ///
    /// Silent before the first event (there is no file to name yet — the
    /// pre-scan wait line stands), while a notice holds the line, and in plain
    /// mode, which rewrites one `\r` line into a pipe or a log where a repeated
    /// line is noise rather than an animation.
    fn tick(&mut self) {
        if !self.styled || self.holding {
            return;
        }
        if !self.drawn.is_some_and(|at| tick_due(at.elapsed())) {
            return;
        }
        let Some(last) = self.last.clone() else {
            return;
        };
        let quiet = self.evented.map_or(Duration::ZERO, |at| at.elapsed());
        let progress = ScanProgress { file: &last.file, done: last.done, total: last.total };
        self.paint(&progress, stalled(quiet));
    }

    /// Paints the progress line for `progress` at the current elapsed time, in
    /// whichever mode the display is in. `stalled` swaps the styled line's
    /// lead-in; the plain line never carries it.
    fn paint(&mut self, progress: &ScanProgress<'_>, stalled: bool) {
        self.drawn = Some(Instant::now());
        if self.styled {
            let columns = (self.columns)();
            let sequence = redraw_sequence(progress, self.start.elapsed(), columns, stalled);
            (self.ink)(&sequence);
        } else {
            let line = compose_progress(progress, self.start.elapsed(), LEAD_SCANNING);
            self.width = self.width.max(line.chars().count());
            (self.ink)(&format!("\r{line:<width$}", width = self.width));
        }
    }

    /// Repaints the display line to `text` in place (stderr, like the bar) —
    /// the `Cancelling...` notice that replaces the bar once a cancellation
    /// is in flight. The cursor stays as the redraws left it;
    /// [`clear`](Self::clear) restores it.
    fn notice(&mut self, text: &str) {
        self.drawn = Some(Instant::now());
        self.holding = true;
        if self.styled {
            let mut sequence = String::new();
            let _ = cursor::MoveToColumn(0).write_ansi(&mut sequence).is_ok();
            let _ = terminal::Clear(terminal::ClearType::UntilNewLine)
                .write_ansi(&mut sequence)
                .is_ok();
            sequence.push_str(text);
            (self.ink)(&sequence);
        } else {
            self.width = self.width.max(text.chars().count());
            (self.ink)(&format!("\r{text:<width$}", width = self.width));
        }
    }

    /// Erases the progress display, if any was drawn. The retained event goes
    /// with it, so a tick racing the scan's end cannot repaint the line the
    /// epilogue is about to print over.
    fn clear(&mut self) {
        self.last = None;
        self.holding = false;
        if self.drawn.take().is_none() {
            return;
        }
        if self.styled {
            (self.ink)(&erase_sequence());
        } else {
            (self.ink)(&format!("\r{:width$}\r", "", width = self.width));
        }
    }

    /// Erases the progress display and prints the classic epilogue (on
    /// stdout, part of the flow narration) for a clean or error-recording
    /// scan.
    fn finish(&mut self, clean: bool) {
        self.clear();
        if clean {
            println!("Scan completed successfully.");
        } else {
            println!("Scan completed with errors (see report).");
        }
    }
}

/// Whether stderr gets the styled display: it must be a terminal, and the
/// terminal must render ANSI escape sequences.
fn stderr_styles() -> bool {
    styles_when(std::io::stderr().is_terminal())
}

/// [`stderr_styles`] for a known terminal state: a terminal gets styles when
/// it renders ANSI sequences; a piped or redirected stderr never does.
#[cfg_attr(
    not(windows),
    expect(
        clippy::missing_const_for_fn,
        reason = "kept non-const to match the Windows build, where ansi_supported is a live probe"
    )
)]
fn styles_when(tty: bool) -> bool {
    tty && ansi_supported()
}

/// Whether the console renders ANSI sequences. On Windows this asks for (and,
/// the first time, enables) the console's virtual-terminal mode; everywhere
/// else ANSI is a given.
#[cfg(windows)]
fn ansi_supported() -> bool {
    crossterm::ansi_support::supports_ansi()
}

/// Whether the console renders ANSI sequences — outside Windows, always.
#[cfg(not(windows))]
#[cfg_attr(coverage_nightly, coverage(off))]
const fn ansi_supported() -> bool {
    // A platform constant: ANSI is always rendered off Windows. There is no
    // logic to exercise, and a `const fn` returning a literal folds at every
    // call site — so the nightly coverage build never records its body as run.
    // Excluded from coverage with `#[coverage(off)]` rather than contorted into
    // a fake runtime call (a bare `true` is the honest implementation).
    true
}

/// The terminal width in columns, falling back to 80 when it can't be probed.
/// `terminal::size` succeeds or fails by the runtime environment — a real
/// console answers, a headless or piped CI does not — so neither arm can be hit
/// deterministically in a test; the environment-bound probe is excluded from
/// coverage rather than faked, while the width-driven rendering it feeds
/// ([`compose_styled_progress`]) is tested directly with explicit columns.
#[cfg_attr(coverage_nightly, coverage(off))]
fn terminal_columns() -> usize {
    terminal::size().map_or(80, |(w, _)| usize::from(w))
}

/// Writes `sequence` to stderr and flushes it — a live display draws now, not
/// at the next buffer flush. A failed stderr write is dropped, like
/// a piped stderr closing early.
fn write_stderr(sequence: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(sequence.as_bytes()).is_ok();
    let _ = err.flush().is_ok();
}

/// The in-place redraw sequence: hide the cursor, return to column 0, clear
/// the rest of the line, then the styled progress line — with the stalled
/// lead-in when `stalled`.
fn redraw_sequence(
    progress: &ScanProgress<'_>,
    elapsed: Duration,
    columns: usize,
    stalled: bool,
) -> String {
    let mut sequence = String::new();
    let _ = cursor::Hide.write_ansi(&mut sequence).is_ok();
    let _ = cursor::MoveToColumn(0).write_ansi(&mut sequence).is_ok();
    let _ = terminal::Clear(terminal::ClearType::UntilNewLine).write_ansi(&mut sequence).is_ok();
    sequence.push_str(&compose_styled_progress(progress, elapsed, columns, stalled));
    sequence
}

/// The display teardown sequence: return to column 0, clear the line, show
/// the cursor again.
fn erase_sequence() -> String {
    let mut sequence = String::new();
    let _ = cursor::MoveToColumn(0).write_ansi(&mut sequence).is_ok();
    let _ = terminal::Clear(terminal::ClearType::UntilNewLine).write_ansi(&mut sequence).is_ok();
    let _ = cursor::Show.write_ansi(&mut sequence).is_ok();
    sequence
}

/// Composes the styled progress line: a percent-filled bar ahead of the plain
/// line's percent/file/elapsed/remaining tail. The bar takes whatever room
/// the terminal leaves the tail, up to [`BAR_MAX_CELLS`]; under
/// [`BAR_MIN_CELLS`] of room the line falls back to the plain spelling,
/// truncated to the width. `stalled` picks the lead-in ([`lead_in`]), which
/// the fallback spelling carries too.
fn compose_styled_progress(
    progress: &ScanProgress<'_>,
    elapsed: Duration,
    columns: usize,
    stalled: bool,
) -> String {
    let (percent, elapsed_seconds, remaining_seconds) =
        progress_stats(progress.done, progress.total, elapsed);
    let lead = lead_in(stalled);
    let tail = format!(
        "{percent:>3}% - {} | Elapsed: {} | Remaining: {}",
        progress.file,
        hms(elapsed_seconds),
        hms(remaining_seconds)
    );
    // The room the bar gets: the width less one spare cell, the lead-in and its
    // ` [] ` bar frame, and the tail — all measured in display cells.
    let frame = lead.chars().count().saturating_add(" [] ".chars().count());
    let cells = columns
        .saturating_sub(1)
        .saturating_sub(frame)
        .saturating_sub(tail.chars().count())
        .min(BAR_MAX_CELLS);
    if cells < BAR_MIN_CELLS {
        let plain = compose_progress(progress, elapsed, lead);
        return plain.chars().take(columns.saturating_sub(1)).collect();
    }
    let filled = usize::try_from(percent)
        .unwrap_or(cells)
        .saturating_mul(cells)
        .checked_div(100)
        .unwrap_or(0)
        .min(cells);
    format!(
        "{lead} [{}{}] {tail}",
        "█".repeat(filled).cyan(),
        "░".repeat(cells.saturating_sub(filled)).dark_grey()
    )
}

/// Composes the plain progress line: the `lead`-in, percent, current file,
/// elapsed, and the remaining-time estimate.
fn compose_progress(progress: &ScanProgress<'_>, elapsed: Duration, lead: &str) -> String {
    let (percent, elapsed_seconds, remaining_seconds) =
        progress_stats(progress.done, progress.total, elapsed);
    format!(
        "{lead} {percent:>3}% - {} | Elapsed: {} | Remaining: {}",
        progress.file,
        hms(elapsed_seconds),
        hms(remaining_seconds)
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use bdinfo_rs_core::bdrom::disc::{
        BdRom, ClipStreamTally, PlaylistSummary, ScanProgress, fixtures,
    };
    use bdinfo_rs_core::bdrom::order::{PlaylistFilter, table_rows};
    use bdinfo_rs_core::error::{BdError, ScanError, ScanStage};
    use bdinfo_rs_core::primitives::Pid;
    use bdinfo_rs_core::report::text::RenderOptions;
    use bdinfo_rs_core::stream::TsStreamType;
    use clap::Parser;
    use clap::error::ErrorKind;

    use super::{
        BAR_MAX_CELLS, Cli, ProgressDisplay, analyze_preamble, banner, compose_progress,
        compose_styled_progress, erase_sequence, finish_early, help_page, hidden_hint,
        pick_playlists, redraw_sequence, report_parse_failure, row_names, run, scan_options,
        selection_table,
    };

    /// A throwaway minimal BD folder (`BDMV/PLAYLIST` + `BDMV/CLIPINF`, both empty)
    /// under the system temp dir, removed on drop. Enough for `BDROM` to scan it
    /// successfully with zero playlists — the library's `disc` tests cover the full
    /// metadata path.
    struct TempBd {
        root: PathBuf,
    }

    impl TempBd {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut root = std::env::temp_dir();
            root.push(format!("bdinfo-rs-{}-{unique}", std::process::id()));
            let bdmv = root.join("BDMV");
            std::fs::create_dir_all(bdmv.join("PLAYLIST")).expect("create PLAYLIST");
            std::fs::create_dir_all(bdmv.join("CLIPINF")).expect("create CLIPINF");
            Self { root }
        }

        /// `new()` plus a corrupt playlist (a `*.mpls` that fails its magic check).
        fn with_corrupt_playlist() -> Self {
            let bd = Self::new();
            std::fs::write(bd.root.join("BDMV").join("PLAYLIST").join("00000.mpls"), b"XXXXjunk")
                .expect("write corrupt mpls");
            bd
        }

        /// `new()` plus a 60-second single-item playlist over clip
        /// `00000.M2TS`, its clip info, and a small stream file — enough for
        /// `--mpls` to resolve a real clip selection.
        fn with_item_playlist() -> Self {
            let bd = Self::new();
            let bdmv = bd.root.join("BDMV");
            std::fs::write(bdmv.join("PLAYLIST").join("00000.mpls"), one_item_mpls())
                .expect("write mpls");
            std::fs::write(bdmv.join("CLIPINF").join("00000.clpi"), avc_clpi())
                .expect("write clpi");
            std::fs::create_dir_all(bdmv.join("STREAM")).expect("create STREAM");
            std::fs::write(bdmv.join("STREAM").join("00000.m2ts"), vec![0_u8; 1024])
                .expect("write m2ts");
            bd
        }

        /// `new()` plus three playlists over one shared clip: `00000.MPLS`
        /// (60 s, one item) survives the default filter, `00001.MPLS` (two
        /// items, 120 s) loops, and `00002.MPLS` (10 s, one item) is short —
        /// so the default table lists only the first, and each
        /// `--show-*-playlists` switch admits exactly one of the other two.
        fn with_filtered_playlists() -> Self {
            let bd = Self::new();
            let bdmv = bd.root.join("BDMV");
            for (name, items, seconds) in [("00000", 1, 60), ("00001", 2, 60), ("00002", 1, 10)] {
                std::fs::write(
                    bdmv.join("PLAYLIST").join(format!("{name}.mpls")),
                    repeated_item_mpls(items, seconds),
                )
                .expect("write mpls");
            }
            std::fs::write(bdmv.join("CLIPINF").join("00000.clpi"), avc_clpi())
                .expect("write clpi");
            std::fs::create_dir_all(bdmv.join("STREAM")).expect("create STREAM");
            std::fs::write(bdmv.join("STREAM").join("00000.m2ts"), vec![0_u8; 1024])
                .expect("write m2ts");
            bd
        }

        /// [`with_item_playlist`](Self::with_item_playlist) turned into an
        /// AACS-encrypted disc: the `AACS/Unit_Key_RO.inf` key file plus the
        /// stream file rewritten as two encrypted Aligned Units.
        fn with_encrypted_stream() -> Self {
            let bd = Self::with_item_playlist();
            std::fs::create_dir_all(bd.root.join("AACS")).expect("create AACS");
            std::fs::write(bd.root.join("AACS").join("Unit_Key_RO.inf"), [])
                .expect("write key file");
            std::fs::write(
                bd.root.join("BDMV").join("STREAM").join("00000.m2ts"),
                encrypted_m2ts(),
            )
            .expect("write m2ts");
            bd
        }

        /// The folder's expected report-file path (the default `REPORT_DEST`
        /// is the disc folder; the label is the directory name).
        fn report_file(&self) -> PathBuf {
            let label = self.root.file_name().expect("dir name").to_string_lossy().into_owned();
            self.root.join(format!("BDINFO.{label}.txt"))
        }
    }

    /// A valid single-item `*.mpls`: `MPLS0300`, one 60-second `PlayItem`
    /// over clip `00000.M2TS`, no angles, no Stream-Number table, no marks.
    fn one_item_mpls() -> Vec<u8> {
        repeated_item_mpls(1, 60)
    }

    /// [`one_item_mpls`] with the play item repeated `items` times, each
    /// running `seconds` from in-time 0. Every repeat replays one clip file
    /// from one in-time, which is what the core's loop detection keys on, and
    /// the playlist's total length is `items * seconds`.
    fn repeated_item_mpls(items: u16, seconds: u32) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"00000"); // clip name
        body.extend_from_slice(b"M2TS");
        body.extend_from_slice(&[0_u8; 3]); // codec id pad + flags
        body.extend_from_slice(&0_u32.to_be_bytes()); // in time (45 kHz)
        body.extend_from_slice(&seconds.wrapping_mul(45_000).to_be_bytes()); // out time
        body.extend_from_slice(&[0_u8; 12]);
        body.extend_from_slice(&[0_u8; 4]); // STN table length + reserved
        body.extend_from_slice(&[0_u8; 12]); // the empty stream counts + reserved

        let playlist_offset: usize = 0x3C;
        let mut playlist = Vec::new();
        playlist.extend_from_slice(&[0_u8; 4]); // PlayList length
        playlist.extend_from_slice(&[0_u8; 2]); // reserved
        playlist.extend_from_slice(&items.to_be_bytes()); // item count
        playlist.extend_from_slice(&[0_u8; 2]); // sub-item count
        for _ in 0..items {
            playlist
                .extend_from_slice(&u16::try_from(body.len()).expect("item length").to_be_bytes());
            playlist.extend_from_slice(&body);
        }
        let chapters_offset = playlist_offset.wrapping_add(playlist.len());

        let mut buf = b"MPLS0300".to_vec();
        buf.extend_from_slice(&u32::try_from(playlist_offset).expect("offset").to_be_bytes());
        buf.extend_from_slice(&u32::try_from(chapters_offset).expect("offset").to_be_bytes());
        buf.extend_from_slice(&[0_u8; 4]); // extensions offset
        buf.resize(playlist_offset, 0);
        buf.extend_from_slice(&playlist);
        buf.extend_from_slice(&[0_u8; 4]); // PlayListMark length
        buf.extend_from_slice(&0_u16.to_be_bytes()); // zero marks
        buf
    }

    /// Two AACS-encrypted 6144-byte Aligned Units: in each, only packet 0's
    /// `0x47` sync byte (offset 4, inside the unit's 16 cleartext bytes)
    /// survives; the other 31 packets' sync positions read ciphertext (`0xAA`).
    fn encrypted_m2ts() -> Vec<u8> {
        let mut unit = vec![0xAA_u8; 4];
        unit.push(0x47);
        unit.resize(6144, 0xAA);
        [unit.clone(), unit].concat()
    }

    /// A valid `*.clpi` declaring one AVC 1080p video stream at PID 0x1011.
    fn avc_clpi() -> Vec<u8> {
        let mut clip_data = vec![0_u8, 1]; // reserved + num_prog = 1
        clip_data.extend_from_slice(&[0_u8; 6]); // spn start + program_map_pid
        clip_data.push(1); // stream count
        clip_data.push(0); // num_groups
        clip_data.extend_from_slice(&0x1011_u16.to_be_bytes());
        clip_data.push(5); // coding-info length
        clip_data.push(0x1B); // AVC video
        clip_data.extend_from_slice(&[0x62, 0x30, 0, 0]); // 1080p / 24 fps
        let mut buf = b"HDMV0300".to_vec();
        buf.extend_from_slice(&[0_u8; 4]);
        buf.extend_from_slice(&16_u32.to_be_bytes()); // ProgramInfo address
        buf.extend_from_slice(&u32::try_from(clip_data.len()).expect("length").to_be_bytes());
        buf.extend_from_slice(&clip_data);
        buf
    }

    impl Drop for TempBd {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root).is_ok();
        }
    }

    /// A throwaway `.iso` file holding `bytes`, removed on drop.
    struct TempIso {
        path: PathBuf,
    }

    impl TempIso {
        fn new(bytes: &[u8]) -> Self {
            use std::io::Write as _;
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("bdinfo-rs-{}-{unique}.iso", std::process::id()));
            std::fs::File::create(&path).expect("create iso").write_all(bytes).expect("write iso");
            Self { path }
        }

        fn arg(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempIso {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path).is_ok();
        }
    }

    /// A throwaway empty destination directory, removed on drop.
    struct TempDest {
        root: PathBuf,
    }

    impl TempDest {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("bdinfo-rs-dest-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&root).expect("create dest");
            Self { root }
        }

        fn arg(&self) -> String {
            self.root.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDest {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root).is_ok();
        }
    }

    /// Builds a minimal valid UDF `.iso`. With `bdmv` `false` the root directory is
    /// empty (no `BDMV`), so the volume parses but the BD scan reports no
    /// structure; with `true` the root carries empty `BDMV/CLIPINF` +
    /// `BDMV/PLAYLIST` directories, so the BD scan succeeds with zero playlists.
    /// Exercises the `.iso` success-to-`BdRom` paths without a full disc; the
    /// library's `vfs::udf::source` tests cover the reader exhaustively.
    #[expect(
        clippy::arithmetic_side_effects,
        clippy::cast_possible_truncation,
        clippy::as_conversions,
        reason = "controlled, in-range .iso byte-layout offsets"
    )]
    fn build_udf_iso(bdmv: bool) -> Vec<u8> {
        const SS: usize = 2048;
        fn put(buf: &mut [u8], off: usize, data: &[u8]) {
            for (dst, &src) in buf.iter_mut().skip(off).zip(data) {
                *dst = src;
            }
        }
        fn fix_tag(buf: &mut [u8], off: usize) {
            let mut sum: u8 = 0;
            for (i, b) in buf.iter().skip(off).take(16).enumerate() {
                if i != 4 {
                    sum = sum.wrapping_add(*b);
                }
            }
            put(buf, off + 4, &[sum]);
        }
        /// A padded directory FID naming `name` at partition-0 block `block`.
        fn fid(name: &str, block: u32) -> Vec<u8> {
            let cs0_len = 1 + name.len();
            let padded = (38 + cs0_len + 3) & !3_usize;
            let mut buf = vec![0_u8; padded];
            put(&mut buf, 0, &257_u16.to_le_bytes());
            put(&mut buf, 18, &[0x02]); // FileCharacteristics: directory
            put(&mut buf, 19, &[cs0_len as u8]); // L_FI
            put(&mut buf, 20, &0x800_u32.to_le_bytes()); // ICB extent length
            put(&mut buf, 24, &block.to_le_bytes());
            put(&mut buf, 28, &0_u16.to_le_bytes()); // partition ref 0
            put(&mut buf, 38, &[8]); // OSTA CS0 compression id 8 (Latin-1)
            put(&mut buf, 39, name.as_bytes());
            fix_tag(&mut buf, 0);
            buf
        }
        /// An embedded directory File Entry at `sector` whose data is `fids`.
        fn dir_fe(img: &mut [u8], sector: usize, fids: &[u8]) {
            let off = sector * SS;
            put(img, off, &261_u16.to_le_bytes());
            put(img, off + 20, &4_u16.to_le_bytes()); // ICB StrategyType = 4
            put(img, off + 27, &[4]); // ICB FileType = directory
            put(img, off + 34, &3_u16.to_le_bytes()); // embedded allocation
            put(img, off + 56, &(fids.len() as u64).to_le_bytes()); // InformationLength
            put(img, off + 172, &(fids.len() as u32).to_le_bytes()); // L_AD
            put(img, off + 176, fids);
            fix_tag(img, off);
        }
        let mut img = vec![0_u8; 268 * SS];
        // AVDP @ sector 256 → Main VDS at sector 257 (2 sectors).
        let avdp = 256 * SS;
        put(&mut img, avdp, &2_u16.to_le_bytes());
        put(&mut img, avdp + 16, &(2 * SS as u32).to_le_bytes());
        put(&mut img, avdp + 20, &257_u32.to_le_bytes());
        fix_tag(&mut img, avdp);
        // Partition Descriptor #0 @ sector 257: start 260, length 10.
        let pd = 257 * SS;
        put(&mut img, pd, &5_u16.to_le_bytes());
        put(&mut img, pd + 22, &0_u16.to_le_bytes());
        put(&mut img, pd + 188, &260_u32.to_le_bytes());
        put(&mut img, pd + 192, &10_u32.to_le_bytes());
        fix_tag(&mut img, pd);
        // LVD @ sector 258: block size 2048, FSD long_ad → part 0 block 1, one map.
        let lvd = 258 * SS;
        put(&mut img, lvd, &6_u16.to_le_bytes());
        put(&mut img, lvd + 84, &[8, b'I', b'S', b'O']);
        put(&mut img, lvd + 211, &[4]); // dstring used length
        put(&mut img, lvd + 212, &(SS as u32).to_le_bytes());
        put(&mut img, lvd + 248, &(SS as u32).to_le_bytes()); // fsd long_ad length
        put(&mut img, lvd + 252, &1_u32.to_le_bytes()); // fsd block
        put(&mut img, lvd + 256, &0_u16.to_le_bytes()); // fsd partition ref
        put(&mut img, lvd + 264, &6_u32.to_le_bytes()); // MapTableLength
        put(&mut img, lvd + 268, &1_u32.to_le_bytes()); // NumberOfPartitionMaps
        put(&mut img, lvd + 440, &[1, 6, 0, 0, 0, 0]); // type-1 map → partition 0
        fix_tag(&mut img, lvd);
        // FSD @ sector 261 (part 0 start 260 + block 1): root ICB → part 0 block 2.
        let fsd = 261 * SS;
        put(&mut img, fsd, &256_u16.to_le_bytes());
        put(&mut img, fsd + 400, &(SS as u32).to_le_bytes());
        put(&mut img, fsd + 404, &2_u32.to_le_bytes());
        put(&mut img, fsd + 408, &0_u16.to_le_bytes());
        fix_tag(&mut img, fsd);
        // Root directory File Entry @ sector 262 (block 2): embedded — empty, or
        // carrying BDMV (block 3) → CLIPINF (block 4) + PLAYLIST (block 5).
        if bdmv {
            dir_fe(&mut img, 262, &fid("BDMV", 3));
            let mut children = fid("CLIPINF", 4);
            children.extend_from_slice(&fid("PLAYLIST", 5));
            dir_fe(&mut img, 263, &children);
            dir_fe(&mut img, 264, &[]); // CLIPINF (empty)
            dir_fe(&mut img, 265, &[]); // PLAYLIST (empty)
        } else {
            dir_fe(&mut img, 262, &[]);
        }
        img
    }

    /// A valid UDF `.iso` whose root directory is empty (no `BDMV`).
    fn minimal_udf_iso() -> Vec<u8> {
        build_udf_iso(false)
    }

    /// Parses an argument list (without the leading program name) and runs it.
    fn run_args(args: &[&str]) -> u8 {
        let mut full = vec!["bdinfo-rs"];
        full.extend_from_slice(args);
        run(&Cli::try_parse_from(full).expect("parse args"))
    }

    #[test]
    fn missing_bd_path_exits_2() {
        assert_eq!(run_args(&["no/such/disc/xyzzy-42"]), 2);
    }

    #[test]
    fn an_iso_without_report_dest_exits_2() {
        // A healthy `.iso` is rejected before any scan: there is no folder to
        // default the report destination to.
        let iso = TempIso::new(&build_udf_iso(true));
        assert_eq!(run_args(&[&iso.arg()]), 2);
        // `--list` validates the destination the same way (the classic flow
        // validates paths before looking at the mode flags).
        assert_eq!(run_args(&[&iso.arg(), "--list"]), 2);
    }

    #[test]
    fn an_iso_with_a_report_dest_scans_and_saves() {
        let iso = TempIso::new(&build_udf_iso(true));
        let dest = TempDest::new();
        assert_eq!(run_args(&[&iso.arg(), &dest.arg(), "--whole"]), 0);
        // The report lands in the destination under the UDF volume label.
        let report = dest.root.join("BDINFO.ISO.txt");
        let bytes = std::fs::read(&report).expect("read the saved report");
        assert!(bytes.starts_with(b"Disc Label:"), "report: {bytes:?}");
    }

    #[test]
    fn a_missing_or_non_directory_report_dest_exits_2() {
        let bd = TempBd::new();
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, "no/such/dest/xyzzy-42"]), 2);
        // A file is not a directory either.
        let file = TempIso::new(b"not a directory");
        assert_eq!(run_args(&[&path, &file.arg()]), 2);
    }

    #[test]
    fn an_invalid_iso_exits_1() {
        // A `.iso` too short to even read the anchor sector → UDF open fails.
        let iso = TempIso::new(b"not a real udf image");
        let dest = TempDest::new();
        assert_eq!(run_args(&[&iso.arg(), &dest.arg()]), 1);
    }

    #[test]
    fn a_valid_iso_without_bdmv_exits_1() {
        // A parseable UDF volume whose root has no BDMV → the scan reports no
        // structure (exercises the `.iso` open → BdRom::open path).
        let iso = TempIso::new(&minimal_udf_iso());
        let dest = TempDest::new();
        assert_eq!(run_args(&[&iso.arg(), &dest.arg()]), 1);
    }

    #[test]
    fn whole_saves_the_report_into_the_disc_folder_by_default() {
        let bd = TempBd::new();
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, "--whole"]), 0);
        let bytes = std::fs::read(bd.report_file()).expect("read the saved report");
        assert!(bytes.starts_with(b"Disc Label:"), "report: {bytes:?}");
    }

    #[test]
    fn the_default_picker_without_input_selects_nothing_and_writes_no_report() {
        // No mode flag → the interactive picker; the test runner's stdin
        // yields nothing, which counts as `q` — no selection, no report.
        let bd = TempBd::with_item_playlist();
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path]), 0);
        assert!(!bd.report_file().exists(), "an empty selection writes no report");
    }

    #[test]
    fn an_explicit_report_dest_receives_the_report() {
        let bd = TempBd::new();
        let dest = TempDest::new();
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, &dest.arg(), "--whole"]), 0);
        let label = bd.root.file_name().expect("dir name").to_string_lossy().into_owned();
        let saved = dest.root.join(format!("BDINFO.{label}.txt"));
        assert!(saved.is_file(), "the report lands in REPORT_DEST");
        assert!(!bd.report_file().exists(), "nothing is written into BD_PATH");
    }

    #[test]
    fn an_unwritable_report_file_exits_2() {
        // The destination directory exists, but the report file's name is
        // taken by a directory — the write itself fails.
        let bd = TempBd::new();
        let path = bd.root.to_string_lossy().into_owned();
        std::fs::create_dir_all(bd.report_file()).expect("occupy the report path");
        assert_eq!(run_args(&[&path, "--whole"]), 2);
    }

    #[test]
    fn a_corrupt_playlist_reports_resiliently_and_exits_3() {
        let bd = TempBd::with_corrupt_playlist();
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, "--whole"]), 3);
        // The (partial) report is still written.
        assert!(bd.report_file().is_file());
    }

    #[test]
    fn an_encrypted_disc_refuses_every_scanning_mode_with_exit_4() {
        let bd = TempBd::with_encrypted_stream();
        let path = bd.root.to_string_lossy().into_owned();
        // The default (picker), `--whole`, and `--mpls` modes all refuse
        // before any table, picker, or scan.
        assert_eq!(run_args(&[&path]), 4);
        assert_eq!(run_args(&[&path, "--whole"]), 4);
        assert_eq!(run_args(&[&path, "--mpls", "00000"]), 4);
        assert!(!bd.report_file().exists(), "no report is written");
    }

    #[test]
    fn list_still_works_on_an_encrypted_disc() {
        // `--list` output is entirely database-derived, so it stays correct
        // and keeps its own exit rules on an encrypted disc.
        let bd = TempBd::with_encrypted_stream();
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, "--list"]), 0);
        assert!(!bd.report_file().exists());
    }

    #[test]
    fn a_leftover_aacs_folder_with_clear_streams_refuses_nothing() {
        // A decrypted rip that kept the key file: the content probe resolves
        // to "not encrypted" and the scan runs to a report.
        let bd = TempBd::with_item_playlist();
        std::fs::create_dir_all(bd.root.join("AACS")).expect("create AACS");
        std::fs::write(bd.root.join("AACS").join("Unit_Key_RO.inf"), []).expect("write key file");
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, "--whole"]), 0);
        assert!(bd.report_file().is_file());
    }

    #[test]
    fn pointing_at_the_bdmv_folder_or_a_descendant_scans() {
        // The common mistake — pointing at `BDMV` (or inside it) instead of
        // the disc root — resolves to the same scan; the report lands in the
        // given directory.
        let bd = TempBd::new();
        for rel in ["BDMV", "BDMV/PLAYLIST"] {
            let path = bd.root.join(rel).to_string_lossy().into_owned();
            assert_eq!(run_args(&[&path, "--whole"]), 0, "input {rel}");
        }
    }

    #[test]
    fn a_directory_named_like_an_iso_is_scanned_as_a_folder() {
        // `is_iso` requires BOTH "is a file" AND the `.iso` extension: a
        // *directory* whose name ends in `.iso` is still folder input, and a valid
        // BD folder under it scans clean (the UDF reader would reject it).
        let root = std::env::temp_dir().join(format!(
            "bdinfo-rs-dir-{}-{}.iso",
            std::process::id(),
            line!()
        ));
        let bdmv = root.join("BDMV");
        std::fs::create_dir_all(bdmv.join("PLAYLIST")).expect("create PLAYLIST");
        std::fs::create_dir_all(bdmv.join("CLIPINF")).expect("create CLIPINF");
        let code = run_args(&[&root.to_string_lossy(), "--whole"]);
        let _ = std::fs::remove_dir_all(&root).is_ok();
        assert_eq!(code, 0);
    }

    #[test]
    fn an_existing_non_bd_path_exits_1() {
        // The CLI crate's manifest dir exists but has no BDMV → "unable to locate".
        assert_eq!(run_args(&[env!("CARGO_MANIFEST_DIR")]), 1);
    }

    #[test]
    fn list_prints_the_table_and_exits_clean() {
        let bd = TempBd::new();
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, "--list"]), 0);
        assert!(!bd.report_file().exists(), "--list writes no report");
        // `--list` on a corrupt playlist still lists the readable rest
        // resiliently (exit 3); an unscannable path is fatal (exit 1).
        let bad = TempBd::with_corrupt_playlist();
        let path = bad.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, "--list"]), 3);
        assert_eq!(run_args(&[env!("CARGO_MANIFEST_DIR"), "--list"]), 1);
    }

    #[test]
    fn mpls_selects_an_existing_playlist() {
        // A real one-item playlist resolves its clip selection and reports.
        let bd = TempBd::with_item_playlist();
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, "--mpls", "00000"]), 0);
        assert!(bd.report_file().is_file());
        // An unknown name among known ones is skipped, and a repeated name
        // selects once — the classic selection behaviour.
        assert_eq!(run_args(&[&path, "--mpls", "00000,99999,00000"]), 0);
        // `--whole` selects the same playlist from the table.
        assert_eq!(run_args(&[&path, "--whole"]), 0);
        // `--mpls` wins over `--whole`/`--list` when combined, like the
        // classic flow (`--list` then exits without a report).
        assert_eq!(run_args(&[&path, "--mpls", "00000", "--whole"]), 0);
        let _ = std::fs::remove_file(bd.report_file()).is_ok();
        assert_eq!(run_args(&[&path, "--mpls", "00000", "--list"]), 0);
        assert!(!bd.report_file().exists(), "--list exits before the scan");
    }

    #[test]
    fn mpls_with_no_matching_playlist_exits_1() {
        let bd = TempBd::new();
        let path = bd.root.to_string_lossy().into_owned();
        assert_eq!(run_args(&[&path, "--mpls", "99999"]), 1);
        // An unscannable disc fails the selection scan the same way.
        assert_eq!(run_args(&[env!("CARGO_MANIFEST_DIR"), "--mpls", "00000"]), 1);
    }

    #[test]
    fn the_no_op_progress_observer_observes_nothing() {
        // It backs the scan paths that never run the packet scan.
        super::no_progress(ScanProgress { file: "00000.M2TS", done: 0, total: 0 });
    }

    #[test]
    fn mode_flags_combine_like_the_classic_cli() {
        // The classic option set declares no conflicts — combinations parse
        // and the flow gives `--mpls` priority, then `--list`/`--whole`.
        for args in [
            ["bdinfo-rs", "disc", "--list", "--whole"].as_slice(),
            &["bdinfo-rs", "disc", "--list", "--mpls", "00000"],
            &["bdinfo-rs", "disc", "--whole", "--mpls", "00000"],
        ] {
            assert!(Cli::try_parse_from(args.iter()).is_ok(), "{args:?} should parse");
        }
    }

    #[test]
    fn missing_bd_path_is_a_parse_error() {
        assert!(Cli::try_parse_from(["bdinfo-rs"]).is_err());
    }

    #[test]
    fn version_flag_uses_the_short_v() {
        for flag in ["-v", "--version"] {
            let err = Cli::try_parse_from(["bdinfo-rs", flag]).expect_err("version exits parsing");
            assert_eq!(err.kind(), ErrorKind::DisplayVersion, "{flag}");
        }
        // Debug-format the parsed CLI once (covers the derived Debug).
        let bd = TempBd::new();
        let cli = Cli::try_parse_from(["bdinfo-rs", &bd.root.to_string_lossy()]).expect("parse");
        assert!(!format!("{cli:?}").is_empty());
    }

    /// The compact help card, byte for byte: the body every help path prints,
    /// under whichever header the terminal earns.
    const CARD: &str = "\
Usage: bdinfo-rs [OPTIONS] <BD_PATH> [REPORT_DEST]

Arguments:
  <BD_PATH>      BDMV folder or .iso image
  [REPORT_DEST]  Report folder (default: BD_PATH; required for .iso)

Options:
  -l, --list                              List playlists and exit
  -m, --mpls <NAME,...>                   Scan only the named playlists
  -w, --whole                             Scan every listed playlist
      --show-short-playlists              Also list short playlists
      --show-looping-playlists            Also list looping playlists
      --short-playlist-seconds <SECONDS>  Short-playlist cutoff (default: 20)
      --drop-partial                      Discard partially scanned stream data
      --no-stream-diagnostics             Omit the STREAM DIAGNOSTICS sections
      --no-quick-summary                  Omit the QUICK SUMMARY blocks
      --no-banner                         Never print the banner
  -v, --version                           Print version
  -h, --help                              Print help
";

    /// A piped, colourless stdout — the header tier that pins a stable page.
    fn piped_caps() -> banner::Capabilities {
        banner::Capabilities { tty: false, ansi: false, no_color: false, columns: 80 }
    }

    /// The whole page a piped help path prints: the plain header, a blank line,
    /// then [`CARD`].
    fn plain_page() -> String {
        format!("bdinfo-rs {} - {}\n\n{CARD}", env!("CARGO_PKG_VERSION"), banner::TAGLINE)
    }

    #[test]
    fn every_help_path_prints_the_same_one_screen_card() {
        let bare = Cli::try_parse_from(["bdinfo-rs"]).expect_err("a bare run asks for the help");
        assert_eq!(bare.kind(), ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand);
        assert_eq!(help_page(&piped_caps(), &bare, true), plain_page());
        // `--help` matches `-h` only while no long help exists anywhere in the
        // command; adding one back would split them into two pages.
        for flag in ["-h", "--help"] {
            let err = Cli::try_parse_from(["bdinfo-rs", flag]).expect_err("help exits parsing");
            assert_eq!(err.kind(), ErrorKind::DisplayHelp, "{flag}");
            assert_eq!(help_page(&piped_caps(), &err, true), plain_page(), "{flag}");
        }
        // One screen: nothing wraps at the classic 80 columns.
        for line in plain_page().lines() {
            assert!(line.chars().count() <= 80, "{line:?}");
        }
    }

    #[test]
    fn a_colour_terminal_heads_the_card_with_the_banner() {
        let err = Cli::try_parse_from(["bdinfo-rs", "-h"]).expect_err("help exits parsing");
        let caps = banner::Capabilities { tty: true, ansi: true, no_color: false, columns: 120 };
        let page = help_page(&caps, &err, true);
        assert!(page.starts_with('█'), "the banner heads the page: {page}");
        assert!(page.contains("\u{1b}[38;2;242;166;90m"), "the wordmark is painted: {page}");
        assert!(page.contains("--show-looping-playlists"), "the card follows: {page}");
        // The same card is styled here and bare when piped.
        assert!(!plain_page().contains('\u{1b}'), "a piped page carries no escape byte");
    }

    #[test]
    fn the_help_paths_exit_zero_and_bad_arguments_exit_two() {
        for (args, code) in [
            // A bare run is a help request, not a usage error.
            (["bdinfo-rs"].as_slice(), 0),
            (&["bdinfo-rs", "-h"], 0),
            (&["bdinfo-rs", "--help"], 0),
            (&["bdinfo-rs", "-v"], 0),
            // An actual argument that cannot parse still is one.
            (&["bdinfo-rs", "--nope"], 2),
            (&["bdinfo-rs", "--list"], 2),
        ] {
            let err = Cli::try_parse_from(args.iter()).expect_err("none of these parse");
            assert_eq!(report_parse_failure(&err), code, "{args:?}");
        }
    }

    #[test]
    fn no_banner_drops_the_header_and_leaves_the_card() {
        let err = Cli::try_parse_from(["bdinfo-rs", "-h"]).expect_err("help exits parsing");
        assert_eq!(help_page(&piped_caps(), &err, false), CARD);
        // The switch is read from the real command line, which under a test
        // harness is the harness's own — so the help path keeps its header.
        assert!(!super::banner_declined());
        let cli = Cli::try_parse_from(["bdinfo-rs", "disc", "--no-banner"]).expect("parse");
        assert!(cli.no_banner);
    }

    #[test]
    fn the_generated_man_page_carries_the_long_form_prose() {
        // The card drops the long-form prose; `build.rs` reattaches it to the
        // same command before rendering the man page. roff escapes every
        // hyphen, so these probes carry none.
        let man = include_str!(concat!(env!("OUT_DIR"), "/assets/bdinfo-rs.1"));
        for prose in [
            "It ships as a single statically",
            "Scan a ripped disc folder",
            "the .MPLS extension may be omitted",
            "authoring artefacts",
            "the span read before the failure",
        ] {
            assert!(man.contains(prose), "the man page is missing {prose:?}");
        }
    }

    #[test]
    fn progress_composes_percent_elapsed_and_remaining() {
        // 50/200 bytes after 10 s → 25%, 30 s remaining.
        let line = compose_progress(
            &ScanProgress { file: "00000.M2TS", done: 50, total: 200 },
            Duration::from_secs(10),
            super::LEAD_SCANNING,
        );
        assert_eq!(line, "Scanning  25% - 00000.M2TS | Elapsed: 00:00:10 | Remaining: 00:00:30");
        // Half a second in, the estimate already shows (millisecond math):
        // 50/200 bytes after 0.5 s → 1.5 s left, truncated.
        let line = compose_progress(
            &ScanProgress { file: "00000.M2TS", done: 50, total: 200 },
            Duration::from_millis(500),
            super::LEAD_SCANNING,
        );
        assert_eq!(line, "Scanning  25% - 00000.M2TS | Elapsed: 00:00:00 | Remaining: 00:00:01");
        // Nothing read yet → no estimate; an empty scan reads 100%.
        let line = compose_progress(
            &ScanProgress { file: "00000.M2TS", done: 0, total: 200 },
            Duration::from_secs(1),
            super::LEAD_SCANNING,
        );
        assert_eq!(line, "Scanning   0% - 00000.M2TS | Elapsed: 00:00:01 | Remaining: 00:00:00");
        let line = compose_progress(
            &ScanProgress { file: "00000.M2TS", done: 0, total: 0 },
            Duration::ZERO,
            super::LEAD_SCANNING,
        );
        assert!(line.starts_with("Scanning 100% - 00000.M2TS"));
        // The stalled lead-in replaces it, and nothing else about the line
        // moves.
        let line = compose_progress(
            &ScanProgress { file: "00000.M2TS", done: 50, total: 200 },
            Duration::from_secs(10),
            super::LEAD_STALLED,
        );
        assert_eq!(
            line,
            "Still reading  25% - 00000.M2TS | Elapsed: 00:00:10 | Remaining: 00:00:30"
        );
    }

    /// A playlist summary carrying only what the selection flow reads: name,
    /// total length, and its clip names (each clip as long as the playlist).
    fn summary(name: &str, total_length: f64, clips: &[&str]) -> PlaylistSummary {
        fixtures::playlist(name, total_length, fixtures::clips(clips, total_length))
    }

    #[test]
    fn the_selection_table_spells_the_classic_columns() {
        // A (interleaved size wins, 1 h 1 min 5 s), B (plain file size,
        // measured packets), C (no sizes at all). A+B share a clip (group 1);
        // C is group 2.
        let mut a = summary("00001.MPLS", 3665.0, &["X.M2TS", "Y.M2TS"]);
        a.interleaved_file_size = 1_234_567;
        a.file_size = 5;
        let mut b = summary("00002.MPLS", 65.0, &["Y.M2TS"]);
        b.file_size = 1_000;
        for clip in &mut b.clips {
            clip.packet_count = 10;
        }
        let c = summary("00003.MPLS", 30.0, &["Z.M2TS"]);
        let playlists = [a, b, c];
        let rows = table_rows(&playlists, &PlaylistFilter::default());
        assert_eq!(rows, [(1, 0), (1, 1), (2, 2)]);
        assert_eq!(
            selection_table(&playlists, &rows),
            "#   Group  Playlist File  Length    Estimated Bytes Measured Bytes  \n\
             \n\
             1   1      00001.MPLS     01:01:05  1,234,567       -               \n\
             2   1      00002.MPLS     00:01:05  1,000           1,920           \n\
             3   2      00003.MPLS     00:00:30  -               -               \n"
        );
        // An out-of-range row index renders nothing.
        assert_eq!(
            selection_table(&playlists, &[(1, 9)]),
            "#   Group  Playlist File  Length    Estimated Bytes Measured Bytes  \n\n"
        );
        // Pick mapping: in pick order, out-of-range picks map to nothing.
        assert_eq!(row_names(&playlists, &rows, &[3, 1, 9]), ["00003.MPLS", "00001.MPLS"]);
    }

    #[test]
    fn the_hidden_hint_names_what_each_rule_withheld_looping_first() {
        let mut long_loop = summary("00001.MPLS", 5765.0, &["A.M2TS"]);
        long_loop.has_loops = true;
        let mut tiny_loop = summary("00090.MPLS", 5.0, &["C.M2TS"]);
        tiny_loop.has_loops = true;
        let playlists = [
            long_loop,
            summary("00013.MPLS", 3600.0, &["B.M2TS"]),
            tiny_loop,
            summary("00091.MPLS", 10.0, &["D.M2TS"]),
        ];
        let looping = "Hidden by filters (looping): 00001.MPLS, 00090.MPLS - rerun with \
                       --show-looping-playlists\n";
        // 00091 (10 s) precedes 00090 (5 s): the names come in table order,
        // length descending — and 00090, short *and* looping, is on both lines.
        let short = "Hidden by filters (short): 00091.MPLS, 00090.MPLS - rerun with \
                     --show-short-playlists\n";
        assert_eq!(
            hidden_hint(&playlists, &PlaylistFilter::default()),
            format!("{looping}{short}")
        );
        // Each switch suppresses exactly its own line.
        let keep_loops =
            PlaylistFilter { filter_looping_playlists: false, ..PlaylistFilter::default() };
        assert_eq!(hidden_hint(&playlists, &keep_loops), short);
        let keep_short =
            PlaylistFilter { filter_short_playlists: false, ..PlaylistFilter::default() };
        assert_eq!(hidden_hint(&playlists, &keep_short), looping);
        // Both switches — the threshold still 20 s, both rules off — print
        // nothing, and so does a disc with nothing to hide.
        let keep_all = PlaylistFilter {
            filter_short_playlists: false,
            filter_looping_playlists: false,
            ..PlaylistFilter::default()
        };
        assert!(hidden_hint(&playlists, &keep_all).is_empty());
        let plain = [summary("00013.MPLS", 3600.0, &["B.M2TS"])];
        assert!(hidden_hint(&plain, &PlaylistFilter::default()).is_empty());
        // A playlist of exactly the threshold length is not short (the filter
        // drops strictly shorter ones), so it is not named either.
        let edge = [summary("00014.MPLS", 20.0, &["E.M2TS"])];
        assert!(hidden_hint(&edge, &PlaylistFilter::default()).is_empty());
    }

    #[test]
    fn the_hidden_hint_spells_three_names_then_counts_the_rest() {
        // Equal-length playlists tie to ordinal name order, so the first three
        // are the lowest-numbered ones.
        let playlists: Vec<PlaylistSummary> =
            (90..306).map(|n| summary(&format!("{n:05}.MPLS"), 1.0, &["A.M2TS"])).collect();
        assert_eq!(
            hidden_hint(&playlists, &PlaylistFilter::default()),
            "Hidden by filters (short): 00090.MPLS, 00091.MPLS, 00092.MPLS and 213 more - rerun \
             with --show-short-playlists\n"
        );
        // Exactly three fit with no remainder; a fourth starts the count.
        assert_eq!(
            hidden_hint(playlists.get(..3).expect("three rows"), &PlaylistFilter::default()),
            "Hidden by filters (short): 00090.MPLS, 00091.MPLS, 00092.MPLS - rerun with \
             --show-short-playlists\n"
        );
        assert!(
            hidden_hint(playlists.get(..4).expect("four rows"), &PlaylistFilter::default())
                .contains("00092.MPLS and 1 more - rerun")
        );
    }

    #[test]
    fn the_show_switches_are_long_only_and_default_off() {
        let cli = Cli::try_parse_from(["bdinfo-rs", "disc"]).expect("parse");
        assert!(!cli.show_short_playlists);
        assert!(!cli.show_looping_playlists);
        let cli = Cli::try_parse_from([
            "bdinfo-rs",
            "disc",
            "--show-short-playlists",
            "--show-looping-playlists",
        ])
        .expect("parse");
        assert!(cli.show_short_playlists);
        assert!(cli.show_looping_playlists);
        // No short letters were taken for them.
        for flag in ["-s", "-S"] {
            assert!(Cli::try_parse_from(["bdinfo-rs", "disc", flag]).is_err(), "{flag}");
        }
    }

    #[test]
    fn drop_partial_turns_the_scan_options_retention_off() {
        let cli = Cli::try_parse_from(["bdinfo-rs", "disc"]).expect("parse");
        assert!(scan_options(&cli).keep_partial, "the default keeps partial stream scans");
        let cli = Cli::try_parse_from(["bdinfo-rs", "disc", "--drop-partial"]).expect("parse");
        assert!(!scan_options(&cli).keep_partial, "--drop-partial discards them");
        // Long-only, like the other behavior switches.
        assert!(Cli::try_parse_from(["bdinfo-rs", "disc", "-d"]).is_err());
    }

    #[test]
    fn the_show_switches_widen_what_whole_scans() {
        // The table feeds `--whole`, so which playlists reach the report is
        // the observable proof that each switch reached the filter.
        let bd = TempBd::with_filtered_playlists();
        let path = bd.root.to_string_lossy().into_owned();
        let scanned = |args: &[&str]| -> String {
            let mut full = vec![path.as_str(), "--whole"];
            full.extend_from_slice(args);
            assert_eq!(run_args(&full), 0, "{args:?}");
            std::fs::read_to_string(bd.report_file()).expect("read the saved report")
        };
        let default = scanned(&[]);
        assert!(default.contains("PLAYLIST: 00000.MPLS"), "{default}");
        assert!(!default.contains("PLAYLIST: 00001.MPLS"), "the looping playlist stays hidden");
        assert!(!default.contains("PLAYLIST: 00002.MPLS"), "the short playlist stays hidden");
        let loops = scanned(&["--show-looping-playlists"]);
        assert!(loops.contains("PLAYLIST: 00001.MPLS"), "{loops}");
        assert!(!loops.contains("PLAYLIST: 00002.MPLS"), "the short playlist stays hidden");
        let shorts = scanned(&["--show-short-playlists"]);
        assert!(shorts.contains("PLAYLIST: 00002.MPLS"), "{shorts}");
        assert!(!shorts.contains("PLAYLIST: 00001.MPLS"), "the looping playlist stays hidden");
        let both = scanned(&["--show-short-playlists", "--show-looping-playlists"]);
        for name in ["00000.MPLS", "00001.MPLS", "00002.MPLS"] {
            assert!(both.contains(&format!("PLAYLIST: {name}")), "{name}: {both}");
        }
        // The hint is console narration; no report ever carries it.
        for report in [&default, &loops, &shorts, &both] {
            assert!(!report.contains("Hidden by filters"), "the hint leaked into the report");
        }
    }

    #[test]
    fn the_short_playlist_cutoff_reaches_the_filter() {
        // Same observable seam as the show switches: the table feeds `--whole`,
        // so the cutoff's effect shows up as which playlists the report carries.
        let bd = TempBd::with_filtered_playlists();
        let path = bd.root.to_string_lossy().into_owned();
        let scanned = |seconds: &str| -> String {
            assert_eq!(
                run_args(&[path.as_str(), "--whole", "--short-playlist-seconds", seconds]),
                0
            );
            std::fs::read_to_string(bd.report_file()).expect("read the saved report")
        };
        // Below the 10 s playlist: it is no longer short, so it reports.
        let lowered = scanned("5");
        assert!(lowered.contains("PLAYLIST: 00002.MPLS"), "{lowered}");
        // Above the 60 s playlist: the disc's one long playlist is short too,
        // so nothing survives the filter and no playlist section is written.
        let raised = scanned("61");
        assert!(!raised.contains("PLAYLIST: "), "{raised}");
    }

    #[test]
    fn each_report_switch_omits_only_its_own_section() {
        let bd = TempBd::with_filtered_playlists();
        let path = bd.root.to_string_lossy().into_owned();
        let scanned = |args: &[&str]| -> String {
            let mut full = vec![path.as_str(), "--whole"];
            full.extend_from_slice(args);
            assert_eq!(run_args(&full), 0, "{args:?}");
            std::fs::read_to_string(bd.report_file()).expect("read the saved report")
        };
        let full = scanned(&[]);
        assert!(full.contains("STREAM DIAGNOSTICS:"), "{full}");
        assert!(full.contains("QUICK SUMMARY:"), "{full}");
        let no_diagnostics = scanned(&["--no-stream-diagnostics"]);
        assert!(!no_diagnostics.contains("STREAM DIAGNOSTICS:"), "{no_diagnostics}");
        assert!(no_diagnostics.contains("QUICK SUMMARY:"), "{no_diagnostics}");
        let no_summary = scanned(&["--no-quick-summary"]);
        assert!(no_summary.contains("STREAM DIAGNOSTICS:"), "{no_summary}");
        assert!(!no_summary.contains("QUICK SUMMARY:"), "{no_summary}");
        let neither = scanned(&["--no-stream-diagnostics", "--no-quick-summary"]);
        assert!(!neither.contains("STREAM DIAGNOSTICS:"), "{neither}");
        assert!(!neither.contains("QUICK SUMMARY:"), "{neither}");
    }

    #[test]
    fn the_picker_collects_indices_until_q() {
        // An invalid word, an out-of-range number, two picks (one repeated),
        // then `q` — picks keep their order and the duplicate.
        let mut input = Cursor::new(b"x\n9\n2\n1\n2\nq\n".to_vec());
        let mut output = Vec::new();
        assert_eq!(pick_playlists(2, &mut input, &mut output), [2, 1, 2]);
        let transcript = String::from_utf8_lossy(&output);
        assert_eq!(transcript.matches("Select (q when finished): ").count(), 6);
        assert!(transcript.contains("Invalid Input!"));
        assert!(transcript.contains("Invalid Selection!"));
        assert!(transcript.contains("\nAdded 2\n"));
        assert!(transcript.contains("\nAdded 1\n"));
        // Zero is out of range too (the table is 1-based).
        let mut input = Cursor::new(b"0\nq\n".to_vec());
        let mut output = Vec::new();
        assert!(pick_playlists(2, &mut input, &mut output).is_empty());
        assert!(String::from_utf8_lossy(&output).contains("Invalid Selection!"));
        // The input ending counts as `q`.
        let mut input = Cursor::new(Vec::new());
        assert!(pick_playlists(2, &mut input, &mut Vec::new()).is_empty());
        // A read error ends the picker the same way (and the failing reader
        // fails through both of its entry points).
        assert!(pick_playlists(2, &mut FailingInput, &mut Vec::new()).is_empty());
        assert!(std::io::Read::read(&mut FailingInput, &mut [0_u8; 1]).is_err());
        std::io::BufRead::consume(&mut FailingInput, 0);
    }

    /// A reader whose every read fails — the picker treats it like `q`.
    struct FailingInput;

    impl std::io::Read for FailingInput {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("broken input"))
        }
    }

    impl std::io::BufRead for FailingInput {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Err(std::io::Error::other("broken input"))
        }

        fn consume(&mut self, _: usize) {}
    }

    #[test]
    fn the_preamble_claims_each_stream_file_once() {
        // B shares clip Y with A: A claims it, B lists only its own Z; a
        // selected name the disc does not carry renders no line.
        let playlists = [
            summary("A.MPLS", 60.0, &["X.M2TS", "Y.M2TS"]),
            summary("B.MPLS", 60.0, &["Y.M2TS", "Z.M2TS"]),
        ];
        let selection = ["A.MPLS".to_owned(), "B.MPLS".to_owned(), "GONE.MPLS".to_owned()];
        assert_eq!(
            analyze_preamble(&playlists, &selection),
            "Preparing to analyze the following:\n\
             A.MPLS --> X.M2TS + Y.M2TS\n\
             B.MPLS --> Z.M2TS\n"
        );
    }

    #[test]
    fn finish_early_reports_recorded_errors_with_exit_3() {
        assert_eq!(finish_early(&[]), 0);
        let errors = [ScanError {
            file: "00001.M2TS".to_owned(),
            stage: ScanStage::StreamFile,
            reason: BdError::StructureNotFound,
        }];
        assert_eq!(finish_early(&errors), 3);
    }

    /// A scanned disc whose one stream file was demuxed to 500 of its declared
    /// 1640 seconds — past both shortfall thresholds — with the per-stream
    /// tally that marks the file as measured.
    fn short_disc() -> BdRom {
        let clip = bdinfo_rs_core::bdrom::disc::ClipSummary {
            packet_seconds: 500.0,
            streams: vec![ClipStreamTally {
                pid: Pid::new(0x1011),
                stream_type: TsStreamType::AvcVideo,
                codec_short_name: "AVC".to_owned(),
                payload_bytes: 1024,
                packet_count: 8,
            }],
            ..fixtures::clip("00011.M2TS", 1640.0)
        };
        BdRom {
            volume_label: "DISC".to_owned(),
            disc_title: None,
            size: 0,
            interleaved_size: 0,
            is_3d: false,
            is_50hz: false,
            is_uhd: false,
            is_aacs_encrypted: false,
            is_bd_plus: false,
            is_bd_java: false,
            is_dbox: false,
            is_psp: false,
            playlists: vec![fixtures::playlist("00000.MPLS", 1640.0, vec![clip])],
        }
    }

    #[test]
    fn the_short_stream_line_prefixes_cores_notice_sentence() {
        // Delegation, not a second pin: the CLI adds `warning: ` and nothing
        // else — the sentence itself is worded and pinned in core, once, so it
        // cannot fork per surface.
        let shorts = short_disc().short_stream_files();
        let short = shorts.first().expect("the truncated file is reported");
        let notice = short.notice();
        assert_eq!(super::short_stream_line(short), format!("warning: {notice}"));
        assert!(
            notice.starts_with("00011.M2TS is shorter"),
            "the notice names the truncated file: {notice}"
        );
    }

    #[test]
    fn a_clean_scan_with_a_short_stream_file_still_exits_0() {
        // The notice is advisory: the scan recorded no error, so the report is
        // written and the exit code stays the clean 0.
        let dest = TempDest::new();
        let code = super::scan_and_report(
            &mut |_, _| Ok((short_disc(), Vec::new())),
            &dest.root,
            &["00000.MPLS".to_owned()],
            RenderOptions::default(),
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn a_packet_scan_failure_after_selection_exits_1() {
        // The disc vanished between the metadata scan and the packet scan —
        // the injectable seam makes the fatal arm deterministic.
        let dest = TempDest::new();
        let code = super::scan_and_report(
            &mut |_, _| Err(BdError::StructureNotFound),
            &dest.root,
            &[],
            RenderOptions::default(),
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn a_cancelled_packet_scan_exits_130_and_writes_no_report() {
        // The injectable seam plays the core's part: the observer runs like a
        // real scan tick (a test process has no console, so no press is ever
        // seen and the flag stays clear), then the scan aborts the way a
        // signalled cancel flag makes `open_resilient_with` abort.
        let dest = TempDest::new();
        let code = super::scan_and_report(
            &mut |progress, cancel| {
                progress(ScanProgress { file: "00000.M2TS", done: 0, total: 100 });
                assert!(!cancel.load(Ordering::Relaxed), "nothing pressed Ctrl+C");
                Err(BdError::ScanCancelled)
            },
            &dest.root,
            &[],
            RenderOptions::default(),
        );
        assert_eq!(code, super::EXIT_CANCELLED);
        assert_eq!(code, 130, "the documented cancellation exit code");
        let saved = std::fs::read_dir(&dest.root).expect("dest listing").next();
        assert!(saved.is_none(), "a cancelled scan writes no report");
    }

    #[test]
    fn the_ctrl_c_watch_cancels_once_then_exits() {
        let mut watch = super::CancelWatch::new();
        assert!(!watch.cancelling());
        assert_eq!(watch.press(), super::CancelStep::Cancel);
        assert!(watch.cancelling());
        assert_eq!(watch.press(), super::CancelStep::Exit);
        assert!(watch.cancelling(), "the exit decision keeps the state");
    }

    #[test]
    fn scan_ticks_draw_then_cancel_then_freeze_then_exit() {
        let progress = ScanProgress { file: "00000.M2TS", done: 10, total: 100 };
        let mut line = ProgressDisplay::with_style(false);
        let mut watch = super::CancelWatch::new();
        let cancel = AtomicBool::new(false);
        // An unpressed tick draws the progress line, nothing more.
        assert!(!super::scan_tick(&mut line, &mut watch, &cancel, false, &progress));
        assert!(line.drawn.is_some());
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(!watch.cancelling());
        // The first press signals the cooperative cancel and repaints the
        // line to the notice.
        assert!(!super::scan_tick(&mut line, &mut watch, &cancel, true, &progress));
        assert!(cancel.load(Ordering::Relaxed));
        assert!(watch.cancelling());
        // An unpressed tick during the in-flight cancellation must NOT
        // overdraw the notice: with the throttle disarmed, an `observe` here
        // would redraw (drawn would be set again) — the tick skips it.
        line.drawn = None;
        assert!(!super::scan_tick(&mut line, &mut watch, &cancel, false, &progress));
        assert!(line.drawn.is_none(), "the notice stays; the bar does not return");
        // The second press erases the display (the erase sequence re-shows
        // the cursor) and requests the immediate exit.
        line.drawn = Some(Instant::now());
        assert!(super::scan_tick(&mut line, &mut watch, &cancel, true, &progress));
        assert!(line.drawn.is_none(), "the display is cleared before the exit");
    }

    #[test]
    fn the_notice_repaints_both_display_modes_in_place() {
        let mut styled = ProgressDisplay::with_style(true);
        styled.notice("Cancelling...");
        assert!(styled.drawn.is_some());
        assert_eq!(styled.width, 0, "styled mode never pads; the sequence wipes");
        let mut plain = ProgressDisplay::with_style(false);
        plain.notice("Cancelling...");
        assert!(plain.drawn.is_some());
        assert_eq!(plain.width, "Cancelling...".chars().count(), "plain mode pads over the bar");
    }

    #[test]
    fn raw_mode_is_never_entered_when_unwanted() {
        // The plain path never wants raw mode; a test process must never
        // toggle its console, so only the unwanted arm is exercised here (the
        // live arm is held by the manual acceptance run).
        let scope = super::RawModeScope::enter(false);
        assert!(!scope.active);
        drop(scope);
        // The Ctrl+C poll never reports a press when not intercepting.
        assert!(!super::ctrl_c_pressed(false));
    }

    #[test]
    fn progress_display_draws_throttles_and_finishes() {
        // `new` detects the style from stderr (captured here, so plain).
        let mut line = ProgressDisplay::new();
        // The first observation draws; an immediate second one is throttled.
        line.observe(&ScanProgress { file: "00000.M2TS", done: 10, total: 100 });
        let first_draw = line.drawn;
        assert!(first_draw.is_some());
        assert!(line.width > 0);
        line.observe(&ScanProgress { file: "00000.M2TS", done: 11, total: 100 });
        assert_eq!(line.drawn, first_draw, "an immediate redraw is throttled");
        // Completion always draws, and finishing erases the line.
        line.observe(&ScanProgress { file: "00000.M2TS", done: 100, total: 100 });
        assert_ne!(line.drawn, first_draw);
        line.finish(true);
        assert!(line.drawn.is_none());
        // The error epilogue (and a finish with nothing drawn) also work.
        line.finish(false);
        // `clear` with nothing drawn is a no-op.
        line.clear();
        // The style decision: a piped stderr never styles; a terminal asks
        // the ANSI-support probe (whose answer depends on the console).
        assert!(!super::styles_when(false));
        assert_eq!(super::styles_when(true), super::ansi_supported());
    }

    #[cfg(windows)]
    #[test]
    fn ansi_probe_matches_the_crossterm_oracle() {
        // Independent oracle: the wrapper must return exactly what the live
        // crossterm probe says — a hardcoded bool can only ever match one
        // console state.
        assert_eq!(super::ansi_supported(), crossterm::ansi_support::supports_ansi());
    }

    #[cfg(not(windows))]
    #[test]
    fn ansi_is_a_given_off_windows() {
        // Outside Windows the probe is always true (see `ansi_supported`, whose
        // `black_box` keeps the body from folding away under coverage).
        assert!(super::ansi_supported());
    }

    #[test]
    fn styled_progress_display_draws_and_erases_in_place() {
        // The styled mode forced (stderr is captured, so `new` won't pick
        // it): observing draws the redraw sequence, clearing the erase one.
        let mut line = ProgressDisplay::with_style(true);
        line.observe(&ScanProgress { file: "00000.M2TS", done: 10, total: 100 });
        assert!(line.drawn.is_some());
        assert_eq!(line.width, 0, "styled mode never pads; the sequence wipes");
        line.finish(true);
        assert!(line.drawn.is_none());
    }

    #[test]
    fn styled_progress_composes_a_bar_ahead_of_the_plain_tail() {
        // A wide terminal gets the full 24-cell bar: 25% → 6 filled cells,
        // the rest empty; the tail repeats the plain line's information.
        let progress = ScanProgress { file: "00000.M2TS", done: 50, total: 200 };
        let line = compose_styled_progress(&progress, Duration::from_secs(10), 120, false);
        assert_eq!(line.matches('█').count(), 6);
        assert_eq!(line.matches('░').count(), 18);
        assert!(line.contains("\u{1b}["), "the bar is styled: {line}");
        assert!(line.starts_with("Scanning ["));
        assert!(line.contains(" 25% - 00000.M2TS | Elapsed: 00:00:10 | Remaining: 00:00:30"));
        // A complete scan fills the whole bar.
        let done = ScanProgress { file: "00000.M2TS", done: 200, total: 200 };
        let line = compose_styled_progress(&done, Duration::from_secs(10), 120, false);
        assert_eq!(line.matches('█').count(), BAR_MAX_CELLS);
        // A standard 80-column terminal shrinks the bar to the room the tail
        // leaves (8 cells here) rather than dropping it.
        let line = compose_styled_progress(&progress, Duration::from_secs(10), 80, false);
        assert_eq!(line.matches('█').count(), 2);
        assert_eq!(line.matches('░').count(), 6);
    }

    #[test]
    fn a_stalled_line_swaps_the_lead_in_and_keeps_the_bar() {
        // The stalled spelling is the same line under a different lead-in: the
        // bar and the tail are untouched, and the five extra lead cells come
        // out of the bar's room, never out of the tail.
        let progress = ScanProgress { file: "00000.M2TS", done: 50, total: 200 };
        let line = compose_styled_progress(&progress, Duration::from_secs(10), 120, true);
        assert!(line.starts_with("Still reading ["), "{line}");
        assert!(line.contains(" 25% - 00000.M2TS | Elapsed: 00:00:10 | Remaining: 00:00:30"));
        assert_eq!(line.matches('█').count(), 6);
        assert_eq!(line.matches('░').count(), 18);
        // The lead-in decision itself.
        assert_eq!(super::lead_in(false), "Scanning");
        assert_eq!(super::lead_in(true), "Still reading");
        // Its trigger: five seconds of silence, inclusive.
        assert!(!super::stalled(Duration::from_millis(4_999)));
        assert!(super::stalled(Duration::from_secs(5)));
        assert!(super::stalled(Duration::from_secs(90)));
        // And the repaint interval, on the same inclusive boundary — a whole
        // second since the last paint, which no wall clock lands on exactly.
        assert!(!super::tick_due(Duration::from_millis(999)));
        assert!(super::tick_due(Duration::from_secs(1)));
        assert!(super::tick_due(Duration::from_secs(40)));
    }

    #[test]
    fn a_narrow_terminal_falls_back_to_the_truncated_plain_line() {
        let progress = ScanProgress { file: "00000.M2TS", done: 50, total: 200 };
        let line = compose_styled_progress(&progress, Duration::from_secs(10), 20, false);
        assert_eq!(line, "Scanning  25% - 000");
        assert!(!line.contains('\u{1b}'), "the narrow fallback is unstyled");
        // The fallback carries the stall wording too — a narrow terminal loses
        // the bar, not the news that the read is stuck.
        let line = compose_styled_progress(&progress, Duration::from_secs(10), 20, true);
        assert_eq!(line, "Still reading  25% ");
        // A zero-width terminal draws nothing rather than panicking.
        assert_eq!(compose_styled_progress(&progress, Duration::ZERO, 0, false), "");
    }

    #[test]
    fn redraw_and_erase_sequences_manage_the_cursor_and_line() {
        let progress = ScanProgress { file: "00000.M2TS", done: 50, total: 200 };
        let sequence = redraw_sequence(&progress, Duration::from_secs(10), 120, false);
        // Hide the cursor, return to column 0, clear to the line end, draw.
        assert!(sequence.starts_with("\u{1b}[?25l\u{1b}[1G\u{1b}[K"), "{sequence:?}");
        assert!(sequence.contains("Scanning ["));
        let erase = erase_sequence();
        assert_eq!(erase, "\u{1b}[1G\u{1b}[K\u{1b}[?25h");
    }

    /// A styled display painting into a recorder instead of stderr, with the
    /// painted strings the test reads back.
    fn recording_display() -> (ProgressDisplay, Arc<Mutex<Vec<String>>>) {
        let painted = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&painted);
        let ink = Box::new(move |text: &str| {
            sink.lock().expect("the recorder").push(text.to_owned());
        });
        let mut display = ProgressDisplay::with_ink(true, ink);
        // A fixed width, so the assertions read the same under a console of
        // any size and under none at all.
        display.columns = || 120;
        (display, painted)
    }

    /// The recorded paints, oldest first.
    fn paints(painted: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        painted.lock().expect("the recorder").clone()
    }

    /// Backdates `display`'s clocks by `seconds`: the scan start, the last
    /// paint and the last progress event, which is exactly the state a read
    /// that has not returned for that long leaves behind.
    fn quiet_for(display: &mut ProgressDisplay, seconds: u64) {
        let past = Instant::now().checked_sub(Duration::from_secs(seconds));
        display.start = past.unwrap_or_else(Instant::now);
        display.drawn = past;
        display.evented = past;
    }

    #[test]
    fn a_tick_repaints_the_stalled_line_with_a_climbing_clock() {
        let (mut line, painted) = recording_display();
        line.observe(&ScanProgress { file: "00011.M2TS", done: 50, total: 200 });
        assert_eq!(paints(&painted).len(), 1, "the first event draws");
        // Inside the tick interval nothing is repainted…
        line.tick();
        assert_eq!(paints(&painted).len(), 1, "a tick inside the interval is throttled");
        // …a second of silence repaints the retained event, still under the
        // plain lead-in (one second is not a stall)…
        quiet_for(&mut line, 1);
        line.tick();
        let after_one = paints(&painted);
        assert_eq!(after_one.len(), 2);
        let second = after_one.get(1).expect("the tick's paint");
        assert!(second.contains("Scanning ["), "{second}");
        assert!(second.contains("00011.M2TS | Elapsed: 00:00:01"), "{second}");
        // …and past the stall threshold the lead-in changes while the elapsed
        // readout keeps climbing off the same retained counts.
        quiet_for(&mut line, 40);
        line.tick();
        let after_forty = paints(&painted);
        assert_eq!(after_forty.len(), 3);
        let third = after_forty.get(2).expect("the stalled paint");
        assert!(third.contains("Still reading ["), "{third}");
        assert!(third.contains("00011.M2TS | Elapsed: 00:00:40"), "{third}");
        assert!(third.contains(" 25% - "), "the percent cannot move without an event: {third}");
    }

    #[test]
    fn a_tick_paints_nothing_without_a_line_of_its_own() {
        // Before the first progress event there is no file to name, so a tick
        // leaves the pre-scan wait text alone.
        let (mut line, painted) = recording_display();
        quiet_for(&mut line, 40);
        line.drawn = None;
        line.tick();
        assert!(paints(&painted).is_empty(), "nothing is retained to repaint");
        // A notice owns the line while a cancellation is in flight; the bar
        // must not come back over it.
        line.observe(&ScanProgress { file: "00011.M2TS", done: 50, total: 200 });
        line.notice("Cancelling...");
        quiet_for(&mut line, 40);
        line.tick();
        assert_eq!(paints(&painted).len(), 2, "the notice stays; the bar does not return");
        // Erasing the display drops the retained event with it, so a tick
        // racing the scan's end cannot repaint over the epilogue.
        line.clear();
        quiet_for(&mut line, 40);
        line.tick();
        assert_eq!(paints(&painted).len(), 3, "only the erase, no repaint after it");
        // The plain path never ticks: a pipe or a log gets one line per event.
        let mut plain = ProgressDisplay::with_style(false);
        plain.observe(&ScanProgress { file: "00011.M2TS", done: 50, total: 200 });
        quiet_for(&mut plain, 40);
        plain.tick();
        assert_eq!(
            plain.drawn.map(|at| at.elapsed().as_secs()),
            Some(40),
            "the plain line was not repainted"
        );
    }

    #[test]
    fn the_ticker_keeps_the_line_moving_while_a_read_hangs() {
        // The end-to-end shape, with a reader that blocks instead of
        // reporting: the ticker thread is the only thing that can paint, and
        // the line it paints names the stuck file with a climbing clock.
        let (mut line, painted) = recording_display();
        line.observe(&ScanProgress { file: "00011.M2TS", done: 50, total: 200 });
        quiet_for(&mut line, 40);
        let display = Arc::new(Mutex::new(line));
        let ticker = super::StallTicker::start(&display);
        // The scan thread inside a read that does not return: it reports
        // nothing at all for the whole wait.
        std::thread::sleep(Duration::from_millis(1_500));
        ticker.stop();
        let repaints = paints(&painted);
        assert!(repaints.len() >= 2, "the ticker repainted at least once: {repaints:?}");
        let last = repaints.last().expect("a painted line");
        assert!(last.contains("Still reading ["), "{last}");
        assert!(last.contains("00011.M2TS | Elapsed: 00:00:4"), "the clock climbed: {last}");

        // Stopping really stops it: with the clocks pushed back into the past
        // again, a running ticker would repaint inside its poll interval.
        quiet_for(&mut super::locked(&display), 80);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(paints(&painted), repaints, "no paint arrives after the ticker is stopped");
    }
}
