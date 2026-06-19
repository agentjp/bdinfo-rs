//! `bdinfo-rs` CLI — drives the `bdinfo-rs-core` library. No GUI, ever.
//!
//! Usage: `bdinfo-rs <BD_PATH> [REPORT_DEST]`.
//!
//! `BD_PATH` is the disc: a directory containing a `BDMV` folder (the disc
//! root, the `BDMV` directory itself, or any directory inside it — the scan
//! walks up to the disc root) or a `.iso` image. A `.iso` path is read through
//! the in-house UDF 2.50 reader ([`bdinfo_rs_core::vfs::udf::source`]); a directory
//! through `std::fs` — the same parsers run over either (the vfs seam). For
//! folder input the disc label is the directory name; `.iso` input reads the
//! real UDF volume label.
//!
//! `REPORT_DEST` is the folder the disc report is written to, as
//! `BDINFO.{volume label}.txt`. It defaults to `BD_PATH` and is required when
//! `BD_PATH` is a `.iso` image (there is no folder to default to).
//!
//! The classic console flow: after the metadata scan, every mode but
//! `-m/--mpls` prints the playlist selection table over the standard filtered
//! set (short and looping playlists dropped). `-l/--list` exits after the
//! table; `-w/--whole` selects everything the table lists; `-m/--mpls A,B`
//! selects the named playlists (unfiltered, in the given order) without a
//! table; and the default is the **interactive picker** — table indices typed
//! one per line, `q` (or the input ending) finishes, no selection exits. The
//! selected playlists' stream files are then scanned and the report written.
//! The scan is always **resilient**: a damaged file or sector is recorded,
//! the readable rest is still reported (the failures land in the report's
//! WARNING block and are summarized on stderr), and the live scan progress
//! draws on stderr; the flow narration (table, picker, analysis preamble,
//! classic epilogue, saved-report message) prints on stdout.
//!
//! Exit codes: `0` success, `1` malformed/not a BD structure or no matching
//! playlist, `2` no such path / unusable `REPORT_DEST` / unwritable report
//! file, `3` completed with errors (a partial report was written).
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
use std::time::{Duration, Instant};

use bdinfo_rs_core::bdrom::chapters::seconds_to_ticks;
use bdinfo_rs_core::bdrom::disc::{BdRom, PlaylistSummary, ScanProgress};
use bdinfo_rs_core::bdrom::order::{PlaylistFilter, presentation_groups};
use bdinfo_rs_core::error::{BdError, ScanError};
use bdinfo_rs_core::report::text;
use bdinfo_rs_core::vfs::fs::FsDir;
use bdinfo_rs_core::vfs::udf::source::{PathIso, UdfSource};
use crossterm::style::Stylize as _;
use crossterm::{Command as _, cursor, terminal};

// The clap `Cli` definition lives in its own file so `build.rs` can `include!`
// the same source to generate the shell completions + man page (a build script
// cannot depend on its own crate). Included here at crate root, `Cli` and its
// private fields are in scope exactly as if declared inline. `cli.rs` brings
// `use clap::Parser;` along with it, so `Cli::parse()` below resolves.
include!("cli.rs");

fn main() -> ExitCode {
    ExitCode::from(run(&Cli::parse()))
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
/// feeding the given progress observer.
type ScanFn<'a> = dyn FnMut(&mut dyn FnMut(ScanProgress<'_>)) -> Result<ScanOutcome, BdError> + 'a;

/// Scans the Blu-ray `.iso` at `path` through the UDF reader, returning the same
/// `BdRom` the folder path would (only the IO backend differs). The resilient
/// scan merges its per-file failures with the reader's bad-sector recordings.
/// `scan_files` narrows the packet scan and `progress` observes it (the
/// library's `open_with` extras).
fn scan_iso(
    path: &str,
    run_packet_scan: bool,
    scan_files: Option<&BTreeSet<String>>,
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<ScanOutcome, BdError> {
    let source = UdfSource::open_resilient(Box::new(PathIso::new(path)))?;
    let report = BdRom::open_resilient_with(&source.root(), run_packet_scan, scan_files, progress)?;
    let mut errors = report.errors;
    errors.extend(source.take_errors());
    Ok((report.bdrom, errors))
}

/// Scans the Blu-ray folder at `location` through `std::fs`. The resilient
/// scan merges its per-file failures with the enumeration's recorded
/// per-entry failures.
fn scan_folder(
    location: &Path,
    run_packet_scan: bool,
    scan_files: Option<&BTreeSet<String>>,
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<ScanOutcome, BdError> {
    let root = FsDir::new(location);
    let report = BdRom::open_resilient_with(&root, run_packet_scan, scan_files, progress)?;
    let mut errors = report.errors;
    errors.extend(root.take_errors());
    Ok((report.bdrom, errors))
}

/// The no-op progress observer for the metadata scan — it never runs the
/// packet scan, so no display fires.
const fn no_progress(_: ScanProgress<'_>) {}

/// Dispatches `path` to the `.iso` or folder scan.
fn scan_disc(
    path: &str,
    run_packet_scan: bool,
    scan_files: Option<&BTreeSet<String>>,
    progress: &mut dyn FnMut(ScanProgress<'_>),
) -> Result<ScanOutcome, BdError> {
    let location = Path::new(path);
    if is_iso(location) {
        scan_iso(path, run_packet_scan, scan_files, progress)
    } else {
        scan_folder(location, run_packet_scan, scan_files, progress)
    }
}

/// Prints the "scan completed with errors" stderr summary — one line per recorded
/// failure (the always-report discipline; stdout stays clean).
fn report_errors(errors: &[ScanError]) {
    eprintln!("warning: scan completed with {} error(s):", errors.len());
    for err in errors {
        eprintln!("warning:   {err}");
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

    println!("Please wait while we scan the disc...");
    let (bdrom, errors) = match scan_disc(&cli.bd_path, false, None, &mut no_progress) {
        Ok(scanned) => scanned,
        Err(err) => {
            eprintln!("error: {err}");
            return 1;
        }
    };

    let selection: Vec<String> = if cli.mpls.is_empty() {
        // Every mode but `--mpls` starts from the playlist table.
        let rows = table_rows(&bdrom.playlists);
        print!("{}", selection_table(&bdrom.playlists, &rows));
        if rows
            .iter()
            .any(|&(_, i)| bdrom.playlists.get(i).is_some_and(PlaylistSummary::has_hidden_streams))
        {
            println!(
                "(*) Some playlists on this disc have hidden tracks. These tracks are marked \
                 with an asterisk."
            );
        }
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
        &mut |progress| scan_disc(&cli.bd_path, true, Some(&scan_files), progress),
        &dest,
        &selection,
    )
}

/// The post-selection phase of [`run`]: packet-scans through `scan` with the
/// live progress display, writes the report into `dest` in selection order,
/// and returns the exit code. `scan` is injectable so the
/// fatal-scan-failure path is testable (a structure that survived the
/// metadata scan rarely fails the packet scan, but the disc can vanish
/// between the two).
fn scan_and_report(scan: &mut ScanFn<'_>, dest: &Path, selection: &[String]) -> u8 {
    let mut line = ProgressDisplay::new();
    let scanned = scan(&mut |p: ScanProgress<'_>| line.observe(&p));
    match scanned {
        Ok((bdrom, errors)) => {
            line.finish(errors.is_empty());
            println!("Please wait while we generate the report...");
            let order = selection_order(&bdrom.playlists, selection);
            let rendered = text::render_with(&bdrom, &order, &errors);
            if let Err(code) = save_report(dest, &bdrom, &rendered) {
                return code;
            }
            println!("Report saved to: {}", dest.display());
            finish_early(&errors)
        }
        Err(err) => {
            line.clear();
            eprintln!("error: {err}");
            1
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

/// Normalizes a `--mpls` value to the playlist-file spelling the model uses:
/// upper-cased, with `.MPLS` appended when no extension was given.
fn normalize_playlist_name(name: &str) -> String {
    let upper = name.to_ascii_uppercase();
    if upper.contains('.') { upper } else { format!("{upper}.MPLS") }
}

/// The `--mpls` selection: each requested name normalized and matched against
/// the disc, in the given order, first occurrence wins; an unknown name is
/// skipped, the classic selection behaviour.
fn named_selection(playlists: &[PlaylistSummary], requested: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for raw in requested {
        let name = normalize_playlist_name(raw);
        if playlists.iter().any(|p| p.name == name) && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The playlist table rows as `(group number, playlist index)` pairs in table
/// order: the shared-clip groups of the standard filtered set (short and
/// looping playlists dropped), each group longest-first.
fn table_rows(playlists: &[PlaylistSummary]) -> Vec<(usize, usize)> {
    presentation_groups(playlists, &PlaylistFilter::default())
        .into_iter()
        .enumerate()
        .flat_map(|(group, members)| {
            members.into_iter().map(move |index| (group.saturating_add(1), index))
        })
        .collect()
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
/// line) and one row per listed playlist. Estimated bytes prefer the
/// interleaved `*.ssif` size, fall back to the `*.m2ts` size, and read `-`
/// when neither is known; measured bytes read `-` until a packet scan has
/// measured the playlist.
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
        let estimated = if playlist.interleaved_file_size > 0 {
            group_n0(playlist.interleaved_file_size)
        } else if playlist.file_size > 0 {
            group_n0(playlist.file_size)
        } else {
            "-".to_owned()
        };
        let measured = if playlist.total_packet_size() > 0 {
            group_n0(playlist.total_packet_size())
        } else {
            "-".to_owned()
        };
        let _ = writeln!(
            out,
            "{:<4}{:<7}{:<15}{:<10}{:<16}{:<16}",
            position.saturating_add(1),
            group,
            playlist.name,
            table_length(playlist.total_length),
            estimated,
            measured
        );
    }
    out
}

/// `hh:mm:ss` from playlist seconds, truncated to the tick like the classic
/// table (hours wrap at 24; the day component is not shown).
fn table_length(seconds: f64) -> String {
    let total = seconds_to_ticks(seconds).max(0).checked_div(10_000_000).unwrap_or(0);
    let h = total.checked_div(3600).and_then(|h| h.checked_rem(24)).unwrap_or(0);
    let m = total.checked_div(60).and_then(|m| m.checked_rem(60)).unwrap_or(0);
    let s = total.checked_rem(60).unwrap_or(0);
    format!("{h:02}:{m:02}:{s:02}")
}

/// `N0` thousands grouping for a byte count (`1234567` → `1,234,567`).
fn group_n0(value: u64) -> String {
    let mut grouped = Vec::new();
    for (position, digit) in value.to_string().chars().rev().enumerate() {
        if position > 0 && position.checked_rem(3) == Some(0) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped.into_iter().rev().collect()
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

/// The stream files the packet scan reads: every clip of every selected
/// playlist.
fn selection_stream_files(playlists: &[PlaylistSummary], selection: &[String]) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    for name in selection {
        if let Some(playlist) = playlists.iter().find(|p| &p.name == name) {
            files.extend(playlist.clips.iter().map(|clip| clip.name.clone()));
        }
    }
    files
}

/// The report's playlist order: the selection order, mapped to indices into
/// the scanned disc's playlists (a duplicated pick renders its playlist
/// again, like the classic picker).
fn selection_order(playlists: &[PlaylistSummary], selection: &[String]) -> Vec<usize> {
    selection.iter().filter_map(|name| playlists.iter().position(|p| &p.name == name)).collect()
}

/// Writes `rendered` into `dest` as `BDINFO.{volume label}.txt` (CRLF, UTF-8,
/// no BOM). Returns the exit code on a failed write (`2`).
fn save_report(dest: &Path, bdrom: &BdRom, rendered: &str) -> Result<(), u8> {
    let target = dest.join(format!("BDINFO.{}.txt", bdrom.volume_label));
    std::fs::write(&target, rendered.as_bytes()).map_err(|err| {
        eprintln!("error: cannot write {}: {err}", target.display());
        2
    })
}

/// The widest the styled display's progress bar grows, in cells.
const BAR_MAX_CELLS: usize = 24;

/// The narrowest bar still worth drawing; less room than this falls back to
/// the plain line.
const BAR_MIN_CELLS: usize = 8;

/// The live scan progress display on stderr, redrawn in place at most every
/// 100 ms (and always at completion), then erased and replaced by the classic
/// epilogue. A terminal that renders ANSI sequences gets a styled line with a
/// progress bar; a piped or redirected stderr gets the plain `\r`-rewritten
/// line — same information either way. stdout never sees any of it.
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
}

impl ProgressDisplay {
    fn new() -> Self {
        Self::with_style(stderr_styles())
    }

    /// A display with the styled/plain choice forced (the testable seam;
    /// [`new`](Self::new) detects it from stderr).
    fn with_style(styled: bool) -> Self {
        Self { styled, start: Instant::now(), drawn: None, width: 0 }
    }

    /// Observes one scan-progress event, redrawing the display when due.
    fn observe(&mut self, progress: &ScanProgress<'_>) {
        let due = progress.done == progress.total
            || self.drawn.is_none_or(|at| at.elapsed() >= Duration::from_millis(100));
        if !due {
            return;
        }
        self.drawn = Some(Instant::now());
        if self.styled {
            let columns = terminal_columns();
            write_stderr(&redraw_sequence(progress, self.start.elapsed(), columns));
        } else {
            let line = compose_progress(progress, self.start.elapsed());
            self.width = self.width.max(line.chars().count());
            eprint!("\r{line:<width$}", width = self.width);
        }
    }

    /// Erases the progress display, if any was drawn.
    fn clear(&mut self) {
        if self.drawn.take().is_none() {
            return;
        }
        if self.styled {
            write_stderr(&erase_sequence());
        } else {
            eprint!("\r{:width$}\r", "", width = self.width);
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
/// the rest of the line, then the styled progress line.
fn redraw_sequence(progress: &ScanProgress<'_>, elapsed: Duration, columns: usize) -> String {
    let mut sequence = String::new();
    let _ = cursor::Hide.write_ansi(&mut sequence).is_ok();
    let _ = cursor::MoveToColumn(0).write_ansi(&mut sequence).is_ok();
    let _ = terminal::Clear(terminal::ClearType::UntilNewLine).write_ansi(&mut sequence).is_ok();
    sequence.push_str(&compose_styled_progress(progress, elapsed, columns));
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
/// truncated to the width.
fn compose_styled_progress(
    progress: &ScanProgress<'_>,
    elapsed: Duration,
    columns: usize,
) -> String {
    let (percent, elapsed_seconds, remaining_seconds) = progress_stats(progress, elapsed);
    let tail = format!(
        "{percent:>3}% - {} | Elapsed: {} | Remaining: {}",
        progress.file,
        hms(elapsed_seconds),
        hms(remaining_seconds)
    );
    // The room the bar gets: the width less one spare cell, the `Scanning
    // [] ` frame, and the tail — all measured in display cells.
    let frame = "Scanning [] ".chars().count();
    let cells = columns
        .saturating_sub(1)
        .saturating_sub(frame)
        .saturating_sub(tail.chars().count())
        .min(BAR_MAX_CELLS);
    if cells < BAR_MIN_CELLS {
        let plain = compose_progress(progress, elapsed);
        return plain.chars().take(columns.saturating_sub(1)).collect();
    }
    let filled = usize::try_from(percent)
        .unwrap_or(cells)
        .saturating_mul(cells)
        .checked_div(100)
        .unwrap_or(0)
        .min(cells);
    format!(
        "Scanning [{}{}] {tail}",
        "█".repeat(filled).cyan(),
        "░".repeat(cells.saturating_sub(filled)).dark_grey()
    )
}

/// Composes the plain progress line: percent, current file, elapsed, and the
/// remaining-time estimate.
fn compose_progress(progress: &ScanProgress<'_>, elapsed: Duration) -> String {
    let (percent, elapsed_seconds, remaining_seconds) = progress_stats(progress, elapsed);
    format!(
        "Scanning {percent:>3}% - {} | Elapsed: {} | Remaining: {}",
        progress.file,
        hms(elapsed_seconds),
        hms(remaining_seconds)
    )
}

/// The percent / elapsed-seconds / remaining-seconds triple both progress
/// spellings draw: the percent from the byte counts (an empty scan reads
/// 100%), the remaining estimate the elapsed time scaled by the bytes still
/// to read. The estimate works in milliseconds — whole-second math would
/// read zero for the entire first second, exactly when the first plausible
/// estimate should already show.
fn progress_stats(progress: &ScanProgress<'_>, elapsed: Duration) -> (u64, u64, u64) {
    let percent = if progress.total == 0 {
        100
    } else {
        progress.done.saturating_mul(100).checked_div(progress.total).unwrap_or(100)
    };
    let elapsed_seconds = elapsed.as_secs();
    let remaining = progress.total.saturating_sub(progress.done);
    let remaining_seconds = elapsed
        .as_millis()
        .saturating_mul(u128::from(remaining))
        .checked_div(u128::from(progress.done).saturating_mul(1000))
        .map_or(0, |seconds| u64::try_from(seconds).unwrap_or(u64::MAX));
    (percent, elapsed_seconds, remaining_seconds)
}

/// `hh:mm:ss` from whole seconds.
fn hms(seconds: u64) -> String {
    format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds % 3600) / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use bdinfo_rs_core::bdrom::disc::{ClipSummary, PlaylistSummary, ScanProgress};
    use bdinfo_rs_core::error::{BdError, ScanError, ScanStage};
    use clap::Parser;
    use clap::error::ErrorKind;

    use super::{
        BAR_MAX_CELLS, Cli, ProgressDisplay, analyze_preamble, compose_progress,
        compose_styled_progress, erase_sequence, finish_early, group_n0, hms, named_selection,
        normalize_playlist_name, pick_playlists, redraw_sequence, row_names, run, selection_order,
        selection_stream_files, selection_table, table_length, table_rows,
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
        let mut body = Vec::new();
        body.extend_from_slice(b"00000"); // clip name
        body.extend_from_slice(b"M2TS");
        body.extend_from_slice(&[0_u8; 3]); // codec id pad + flags
        body.extend_from_slice(&0_u32.to_be_bytes()); // in time (45 kHz)
        body.extend_from_slice(&2_700_000_u32.to_be_bytes()); // out time: 60 s
        body.extend_from_slice(&[0_u8; 12]);
        body.extend_from_slice(&[0_u8; 4]); // STN table length + reserved
        body.extend_from_slice(&[0_u8; 12]); // the empty stream counts + reserved

        let playlist_offset: usize = 0x3C;
        let mut playlist = Vec::new();
        playlist.extend_from_slice(&[0_u8; 4]); // PlayList length
        playlist.extend_from_slice(&[0_u8; 2]); // reserved
        playlist.extend_from_slice(&1_u16.to_be_bytes()); // item count
        playlist.extend_from_slice(&[0_u8; 2]); // sub-item count
        playlist.extend_from_slice(&u16::try_from(body.len()).expect("item length").to_be_bytes());
        playlist.extend_from_slice(&body);
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
    fn playlist_names_normalize_to_the_model_spelling() {
        assert_eq!(normalize_playlist_name("00800"), "00800.MPLS");
        assert_eq!(normalize_playlist_name("00800.mpls"), "00800.MPLS");
        assert_eq!(normalize_playlist_name("00800.MPLS"), "00800.MPLS");
        // The shared no-op observer observes nothing (it backs the scan paths
        // that never run the packet scan).
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

    #[test]
    fn progress_composes_percent_elapsed_and_remaining() {
        // 50/200 bytes after 10 s → 25%, 30 s remaining.
        let line = compose_progress(
            &ScanProgress { file: "00000.M2TS", done: 50, total: 200 },
            Duration::from_secs(10),
        );
        assert_eq!(line, "Scanning  25% - 00000.M2TS | Elapsed: 00:00:10 | Remaining: 00:00:30");
        // Half a second in, the estimate already shows (millisecond math):
        // 50/200 bytes after 0.5 s → 1.5 s left, truncated.
        let line = compose_progress(
            &ScanProgress { file: "00000.M2TS", done: 50, total: 200 },
            Duration::from_millis(500),
        );
        assert_eq!(line, "Scanning  25% - 00000.M2TS | Elapsed: 00:00:00 | Remaining: 00:00:01");
        // Nothing read yet → no estimate; an empty scan reads 100%.
        let line = compose_progress(
            &ScanProgress { file: "00000.M2TS", done: 0, total: 200 },
            Duration::from_secs(1),
        );
        assert_eq!(line, "Scanning   0% - 00000.M2TS | Elapsed: 00:00:01 | Remaining: 00:00:00");
        let line = compose_progress(
            &ScanProgress { file: "00000.M2TS", done: 0, total: 0 },
            Duration::ZERO,
        );
        assert!(line.starts_with("Scanning 100% - 00000.M2TS"));
        assert_eq!(hms(3661), "01:01:01");
        assert_eq!(hms(0), "00:00:00");
    }

    /// A playlist summary carrying only what the selection flow reads.
    fn summary(name: &str, total_length: f64, clips: &[&str]) -> PlaylistSummary {
        PlaylistSummary {
            name: name.to_owned(),
            total_length,
            file_size: 0,
            interleaved_file_size: 0,
            chapter_count: 0,
            stream_count: 0,
            angle_count: 0,
            has_loops: false,
            streams: Vec::new(),
            clips: clips
                .iter()
                .map(|clip| ClipSummary {
                    name: (*clip).to_owned(),
                    display_name: (*clip).to_owned(),
                    angle_index: 0,
                    relative_time_in: 0.0,
                    length: total_length,
                    payload_bytes: 0,
                    packet_count: 0,
                    packet_seconds: 0.0,
                    file_seconds: 0.0,
                    streams: Vec::new(),
                })
                .collect(),
            chapters: Vec::new(),
        }
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
        let rows = table_rows(&playlists);
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
    fn table_cells_group_digits_and_wrap_hours() {
        assert_eq!(group_n0(0), "0");
        assert_eq!(group_n0(123), "123");
        assert_eq!(group_n0(1_000), "1,000");
        assert_eq!(group_n0(1_234_567), "1,234,567");
        assert_eq!(table_length(0.0), "00:00:00");
        assert_eq!(table_length(100.0), "00:01:40");
        // 26 h wraps at 24, like the classic table's TimeSpan hours.
        assert_eq!(table_length(93_600.0 + 65.0), "02:01:05");
        assert_eq!(table_length(-5.0), "00:00:00");
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
    fn named_selection_keeps_the_given_order_and_dedups() {
        let playlists =
            [summary("00001.MPLS", 60.0, &["A.M2TS"]), summary("00002.MPLS", 60.0, &["B.M2TS"])];
        let requested =
            ["00002".to_owned(), "00001.mpls".to_owned(), "00002".to_owned(), "X".to_owned()];
        assert_eq!(named_selection(&playlists, &requested), ["00002.MPLS", "00001.MPLS"]);
        assert!(named_selection(&playlists, &["X".to_owned()]).is_empty());
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
        // The scan set is the union of the selected playlists' clips…
        let files = selection_stream_files(&playlists, &selection);
        assert_eq!(files.into_iter().collect::<Vec<_>>(), ["X.M2TS", "Y.M2TS", "Z.M2TS"]);
        // …and the report renders in selection order (a repeated selection
        // repeats its playlist; a gone name is skipped).
        let again = ["B.MPLS".to_owned(), "A.MPLS".to_owned(), "B.MPLS".to_owned()];
        assert_eq!(selection_order(&playlists, &again), [1, 0, 1]);
        assert_eq!(selection_order(&playlists, &["GONE.MPLS".to_owned()]), [0_usize; 0]);
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

    #[test]
    fn a_packet_scan_failure_after_selection_exits_1() {
        // The disc vanished between the metadata scan and the packet scan —
        // the injectable seam makes the fatal arm deterministic.
        let dest = TempDest::new();
        let code =
            super::scan_and_report(&mut |_| Err(BdError::StructureNotFound), &dest.root, &[]);
        assert_eq!(code, 1);
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
        let line = compose_styled_progress(&progress, Duration::from_secs(10), 120);
        assert_eq!(line.matches('█').count(), 6);
        assert_eq!(line.matches('░').count(), 18);
        assert!(line.contains("\u{1b}["), "the bar is styled: {line}");
        assert!(line.contains(" 25% - 00000.M2TS | Elapsed: 00:00:10 | Remaining: 00:00:30"));
        // A complete scan fills the whole bar.
        let done = ScanProgress { file: "00000.M2TS", done: 200, total: 200 };
        let line = compose_styled_progress(&done, Duration::from_secs(10), 120);
        assert_eq!(line.matches('█').count(), BAR_MAX_CELLS);
        // A standard 80-column terminal shrinks the bar to the room the tail
        // leaves (8 cells here) rather than dropping it.
        let line = compose_styled_progress(&progress, Duration::from_secs(10), 80);
        assert_eq!(line.matches('█').count(), 2);
        assert_eq!(line.matches('░').count(), 6);
    }

    #[test]
    fn a_narrow_terminal_falls_back_to_the_truncated_plain_line() {
        let progress = ScanProgress { file: "00000.M2TS", done: 50, total: 200 };
        let line = compose_styled_progress(&progress, Duration::from_secs(10), 20);
        assert_eq!(line, "Scanning  25% - 000");
        assert!(!line.contains('\u{1b}'), "the narrow fallback is unstyled");
        // A zero-width terminal draws nothing rather than panicking.
        assert_eq!(compose_styled_progress(&progress, Duration::ZERO, 0), "");
    }

    #[test]
    fn redraw_and_erase_sequences_manage_the_cursor_and_line() {
        let progress = ScanProgress { file: "00000.M2TS", done: 50, total: 200 };
        let sequence = redraw_sequence(&progress, Duration::from_secs(10), 120);
        // Hide the cursor, return to column 0, clear to the line end, draw.
        assert!(sequence.starts_with("\u{1b}[?25l\u{1b}[1G\u{1b}[K"), "{sequence:?}");
        assert!(sequence.contains("Scanning ["));
        let erase = erase_sequence();
        assert_eq!(erase, "\u{1b}[1G\u{1b}[K\u{1b}[?25h");
    }
}
