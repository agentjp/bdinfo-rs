//! The CLI switch cross-product, driven end to end against the committed
//! `BigBuckBunny` fixture disc.
//!
//! `tests/cli.rs` proves every switch **alone**. This file runs them in
//! **combination**, which is where a wiring interaction lives: one flag's
//! plumbing clobbering another's is invisible to any number of single-switch
//! tests.
//!
//! The sweep pins no goldens of its own. A golden per combination would be an
//! oracle nobody can eye-verify — it freezes bugs in place and rots on every
//! deliberate format change — so every cell is judged against invariants
//! derived from the one golden `tests/cli.rs` already pins:
//!
//! 1. **Anchor** — the unflagged run reproduces `fixtures/golden/folder.txt`.
//! 2. **Section subtraction** — a report switch removes exactly its own blocks from the same cell's
//!    untrimmed report, computed by [`without_section`] and byte-compared; the two switches
//!    commute; neither touches the console.
//! 3. **Selection equivalence** — `--whole` and `--mpls` over the same set of playlists write the
//!    same bytes; the listing options never reach `--mpls`.
//! 4. **Argument-order independence** — permuting the command line changes nothing.
//! 5. **Cutoff monotonicity** — raising `--short-playlist-seconds` only ever moves table rows from
//!    listed to hidden, in place, and the hint names the `short` rule exactly when it withheld
//!    something.
//! 6. **Exit codes and artifacts** — every cell exits 0 on this healthy disc, `--list` writes no
//!    report file, a scan writes exactly one.
//!
//! An invariant that fails is a finding about the binary, never a number to
//! recalibrate.
//!
//! `unused_crate_dependencies` is a known false positive for integration tests:
//! cargo makes the binary's deps (`clap`, `bdinfo-rs-core`) visible to this test
//! crate, but a black-box CLI test legitimately uses neither — it only spawns the
//! compiled binary.
#![expect(
    unused_crate_dependencies,
    reason = "black-box CLI test spawns the built binary; it links the bin's \
              deps (clap, bdinfo-rs-core) but uses neither directly"
)]

pub mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::{bdinfo_rs, real_fixture};

/// The committed fixture disc the whole sweep runs against: a BDMV folder whose
/// single ~30 s playlist survives the default 20 s cutoff.
const DISC: &str = "BigBuckBunny";

/// The report file a scan of [`DISC`] writes — `BDINFO.{disc label}.txt`, and a
/// folder's label is its directory name.
const REPORT: &str = "BDINFO.BigBuckBunny.txt";

/// The full report the unflagged run must reproduce, byte for byte. Included
/// from the file `tests/cli.rs` pins, not copied, so a deliberate format change
/// re-pins both at once.
const GOLDEN: &str = include_str!("fixtures/golden/folder.txt");

/// Heading of the block `--no-stream-diagnostics` removes.
const DIAGNOSTICS: &str = "STREAM DIAGNOSTICS:";

/// Heading of the block `--no-quick-summary` removes.
const SUMMARY: &str = "QUICK SUMMARY:";

/// The three selection modes the sweep covers. The interactive picker stays
/// out: `tests/cli.rs` drives it, and it adds a stdin dimension without adding
/// any wiring these switches touch.
const MODES: [Mode; 3] = [Mode::List, Mode::Named, Mode::Whole];

/// Every boolean switch, absent and present.
const SWITCHES: [bool; 2] = [false, true];

/// The `--short-playlist-seconds` values swept, `None` being the unflagged run.
/// `0` classifies nothing as short; `86400` is a full day, longer than any
/// playlist a disc can hold, so it classifies everything as short; `5` sits
/// below the fixture playlist's ~30 s.
const THRESHOLDS: [Option<u32>; 4] = [None, Some(0), Some(5), Some(86_400)];

/// [`THRESHOLDS`] in ascending cutoff order — the ladder the monotonicity check
/// walks. `None` is the 20-second default, so it sits between `5` and `86400`.
/// The sweep asserts this is a permutation of [`THRESHOLDS`], so adding a value
/// there without placing it here fails loudly rather than skipping it.
const LADDER: [Option<u32>; 4] = [Some(0), Some(5), None, Some(86_400)];

/// The argument layouts a sampled cell is re-run in, beside the canonical one.
const PERMUTED_ORDERS: [Order; 2] = [Order::Reversed, Order::SwitchesFirst];

/// One cell in every `PERMUTATION_STRIDE` is also run in each of
/// [`PERMUTED_ORDERS`], so the sample walks the whole space rather than
/// clustering.
///
/// Permuting every cell triples the sweep's process count, and the wall clock
/// is what constrains this test: `.config/nextest.toml` force-terminates a test
/// at 2 × 30 s, and the run is slowest under coverage instrumentation, which
/// instruments the spawned binary too. A stride of 8 keeps the instrumented
/// sweep at roughly a third of that ceiling on six platform legs. The property
/// being sampled — clap losing an argument in some position — is not
/// cell-specific, so a sample is the right shape for it.
const PERMUTATION_STRIDE: usize = 8;

/// Which selection mode a cell drives the binary in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Mode {
    /// `--list`: print the table and exit without writing a report.
    List,
    /// `--mpls`, naming every playlist an unfiltered listing shows.
    Named,
    /// `--whole`: scan whatever this cell's own filtered table lists.
    Whole,
}

/// One cell of the sweep: a selection mode plus the five option knobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one field per independent CLI switch; the switches are orthogonal, which is what \
              the sweep exists to prove"
)]
struct Cell {
    mode: Mode,
    show_short: bool,
    show_looping: bool,
    threshold: Option<u32>,
    no_diagnostics: bool,
    no_summary: bool,
}

impl Cell {
    /// The mode's own switch and its value.
    fn mode_args(self, names: &str) -> Vec<String> {
        match self.mode {
            Mode::List => vec!["--list".to_owned()],
            Mode::Named => vec!["--mpls".to_owned(), names.to_owned()],
            Mode::Whole => vec!["--whole".to_owned()],
        }
    }

    /// The option switches, one group per switch so a permutation can reorder
    /// them without separating `--short-playlist-seconds` from its value.
    fn option_groups(self) -> Vec<Vec<String>> {
        let mut groups = Vec::new();
        if self.show_short {
            groups.push(vec!["--show-short-playlists".to_owned()]);
        }
        if self.show_looping {
            groups.push(vec!["--show-looping-playlists".to_owned()]);
        }
        if let Some(seconds) = self.threshold {
            groups.push(vec!["--short-playlist-seconds".to_owned(), seconds.to_string()]);
        }
        if self.no_diagnostics {
            groups.push(vec!["--no-stream-diagnostics".to_owned()]);
        }
        if self.no_summary {
            groups.push(vec!["--no-quick-summary".to_owned()]);
        }
        groups
    }

    /// This cell in another selection mode, every option unchanged.
    const fn with_mode(self, mode: Mode) -> Self {
        Self { mode, ..self }
    }

    /// This cell with both report switches absent — the untrimmed report the
    /// subtraction invariant measures against.
    const fn with_full_report(self) -> Self {
        Self { no_diagnostics: false, no_summary: false, ..self }
    }

    /// This cell with all three listing options at their defaults.
    const fn with_default_listing(self) -> Self {
        Self { show_short: false, show_looping: false, threshold: None, ..self }
    }
}

/// How a cell's arguments are laid out. Every order names the same switches
/// with the same values, so the binary must answer all three identically.
#[derive(Clone, Copy, Debug)]
enum Order {
    /// Positionals, the mode switch, then the options as declared.
    Canonical,
    /// Positionals, the options reversed, then the mode switch.
    Reversed,
    /// Every switch first, positionals last.
    SwitchesFirst,
}

/// Everything a cell is judged on: how the process ended, what it narrated, and
/// what it left in the destination folder.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    code: Option<i32>,
    stdout: String,
    /// The destination folder's contents after the run, by file name.
    reports: BTreeMap<String, String>,
}

/// The one destination folder every run writes into.
///
/// A shared path is what makes stdout comparable across cells: the saved-report
/// line names the destination, so a per-run folder would make every scan's
/// narration differ for a reason that has nothing to do with the switches. The
/// sweep is sequential and [`run`] empties the folder before each invocation.
fn dest_dir() -> PathBuf {
    std::env::temp_dir().join(format!("bdinfo-rs-matrix-{}", std::process::id()))
}

/// The full command line for `cell`, with the option switches laid out in
/// `order`.
fn command_line(cell: Cell, names: &str, order: Order) -> Vec<String> {
    let positionals = [
        real_fixture(DISC).to_string_lossy().into_owned(),
        dest_dir().to_string_lossy().into_owned(),
    ];
    let mode = cell.mode_args(names);
    let mut groups = cell.option_groups();
    let mut args = Vec::new();
    match order {
        Order::Canonical => {
            args.extend(positionals);
            args.extend(mode);
            args.extend(groups.concat());
        }
        Order::Reversed => {
            args.extend(positionals);
            groups.reverse();
            args.extend(groups.concat());
            args.extend(mode);
        }
        Order::SwitchesFirst => {
            args.extend(mode);
            args.extend(groups.concat());
            args.extend(positionals);
        }
    }
    args
}

/// Runs the binary with `args` against a freshly emptied destination folder.
#[expect(
    clippy::expect_used,
    reason = "end-to-end test driver; a failed spawn / read / decode should abort the test loudly"
)]
fn run(args: &[String]) -> Run {
    let dest = dest_dir();
    let _ = std::fs::remove_dir_all(&dest).is_ok();
    std::fs::create_dir_all(&dest).expect("create the destination folder");
    let output = bdinfo_rs().args(args).output().expect("spawn bdinfo-rs");
    let mut reports = BTreeMap::new();
    for entry in std::fs::read_dir(&dest).expect("read the destination folder") {
        let entry = entry.expect("read a destination entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        reports.insert(name, std::fs::read_to_string(entry.path()).expect("read a written report"));
    }
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        reports,
    }
}

/// `report` with every `heading` block removed, or `None` when the report does
/// not carry that heading at all.
///
/// A report section is its heading, a blank line, the body, and a closing blank
/// line, so a block runs from the heading through the second blank line below
/// it. The verdict is returned rather than asserted here so both outcomes are
/// checkable from a passing test: a report that carries the section answers
/// `Some`, one a switch already trimmed answers `None`.
fn without_section(report: &str, heading: &str) -> Option<String> {
    let mut kept = String::new();
    let mut found = false;
    // Blank lines still owed by the block being dropped; 0 means "keeping".
    let mut blanks_left = 0_u8;
    for line in report.split_inclusive('\n') {
        if blanks_left > 0 {
            if line.trim().is_empty() {
                blanks_left = blanks_left.wrapping_sub(1);
            }
        } else if line.trim_end() == heading {
            found = true;
            blanks_left = 2;
        } else {
            kept.push_str(line);
        }
    }
    found.then_some(kept)
}

/// `report` minus each heading in `headings`. A heading the report does not
/// carry subtracts nothing: a run whose table is empty selects no playlist and
/// writes a report with no playlist sections at all.
fn subtract(report: &str, headings: &[&str]) -> String {
    let mut kept = report.to_owned();
    for &heading in headings {
        if let Some(stripped) = without_section(&kept, heading) {
            kept = stripped;
        }
    }
    kept
}

/// The playlist names the selection table lists, in table order. A data row
/// opens with its row number; the header, the hint lines and the asterisk note
/// never do.
fn listed_playlists(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| line.starts_with(|c: char| c.is_ascii_digit()))
        .filter_map(|line| line.split_whitespace().find(|word| word.contains(".MPLS")))
        .map(str::to_owned)
        .collect()
}

/// Every cell of the space, in a fixed order.
fn all_cells() -> Vec<Cell> {
    let mut cells = Vec::new();
    for mode in MODES {
        for show_short in SWITCHES {
            for show_looping in SWITCHES {
                for threshold in THRESHOLDS {
                    for no_diagnostics in SWITCHES {
                        for no_summary in SWITCHES {
                            cells.push(Cell {
                                mode,
                                show_short,
                                show_looping,
                                threshold,
                                no_diagnostics,
                                no_summary,
                            });
                        }
                    }
                }
            }
        }
    }
    cells
}

/// The size of the space, from the dimension lists themselves — so a cell count
/// that disagrees means the loop lost cells, not that this number is stale.
#[expect(clippy::expect_used, reason = "a space that overflows a usize is a broken test")]
fn space_size() -> usize {
    [MODES.len(), SWITCHES.len(), SWITCHES.len(), THRESHOLDS.len(), SWITCHES.len(), SWITCHES.len()]
        .into_iter()
        .try_fold(1_usize, usize::checked_mul)
        .expect("the space size fits a usize")
}

/// The recorded run for `cell`.
#[expect(clippy::expect_used, reason = "every cell of the space is swept before any lookup")]
fn run_of(results: &BTreeMap<Cell, Run>, cell: Cell) -> &Run {
    results.get(&cell).expect("the cell was swept")
}

/// The report `cell` wrote.
#[expect(clippy::expect_used, reason = "only scan cells are looked up, and every scan reports")]
fn report_of(results: &BTreeMap<Cell, Run>, cell: Cell) -> &str {
    run_of(results, cell).reports.get(REPORT).expect("the scan wrote its report")
}

/// Invariant 1: the unflagged run reproduces the golden through both selection
/// modes.
fn assert_anchor(results: &BTreeMap<Cell, Run>) {
    for mode in [Mode::Named, Mode::Whole] {
        let cell = Cell {
            mode,
            show_short: false,
            show_looping: false,
            threshold: None,
            no_diagnostics: false,
            no_summary: false,
        };
        assert_eq!(report_of(results, cell), GOLDEN, "{cell:?} drifted from the golden");
    }
}

/// Invariant 2: each report switch subtracts exactly its own blocks, the two
/// commute, and neither reaches the console.
fn assert_section_subtraction(results: &BTreeMap<Cell, Run>, cells: &[Cell]) {
    for &cell in cells {
        assert_eq!(
            run_of(results, cell).stdout,
            run_of(results, cell.with_full_report()).stdout,
            "{cell:?}: a report switch changed the console narration"
        );
        if cell.mode == Mode::List {
            continue;
        }
        let full = report_of(results, cell.with_full_report());
        let mut headings = Vec::new();
        if cell.no_diagnostics {
            headings.push(DIAGNOSTICS);
        }
        if cell.no_summary {
            headings.push(SUMMARY);
        }
        let expected = subtract(full, &headings);
        assert_eq!(report_of(results, cell), expected, "{cell:?}: the switches must subtract");
        headings.reverse();
        assert_eq!(
            subtract(full, &headings),
            expected,
            "{cell:?}: the two report switches must commute"
        );
    }
}

/// Invariant 3: `--whole` and `--mpls` over the same playlists agree, and the
/// listing options never reach `--mpls`.
fn assert_selection_equivalence(
    results: &BTreeMap<Cell, Run>,
    cells: &[Cell],
    all_names: &[String],
) {
    for &cell in cells {
        match cell.mode {
            Mode::List => {}
            Mode::Named => assert_eq!(
                run_of(results, cell),
                run_of(results, cell.with_default_listing()),
                "{cell:?}: a listing option reached --mpls"
            ),
            Mode::Whole => {
                let listed = listed_playlists(&run_of(results, cell.with_mode(Mode::List)).stdout);
                if listed == all_names {
                    assert_eq!(
                        report_of(results, cell),
                        report_of(results, cell.with_mode(Mode::Named)),
                        "{cell:?}: --whole and -m over the same set must write the same bytes"
                    );
                } else {
                    // This disc holds one playlist, so a table that is not the
                    // whole disc is empty and `--whole` selects nothing. A
                    // fixture with a partial table would need an `-m` run over
                    // exactly the listed names; this assertion is where that
                    // surfaces instead of being skipped silently.
                    assert!(listed.is_empty(), "{cell:?}: the table listed a subset {listed:?}");
                    assert!(
                        !report_of(results, cell).contains("PLAYLIST: "),
                        "{cell:?}: an empty table must select nothing"
                    );
                }
            }
        }
    }
}

/// Invariant 5: raising the cutoff only hides rows, in place, and the hint
/// names the `short` rule exactly when it withheld something.
#[expect(clippy::expect_used, reason = "a two-element window always has a first and a last")]
fn assert_cutoff_monotonicity(results: &BTreeMap<Cell, Run>) {
    let mut declared = THRESHOLDS;
    declared.sort_unstable();
    let mut ladder = LADDER;
    ladder.sort_unstable();
    assert_eq!(ladder, declared, "the ladder must cover every cutoff the space sweeps");

    for show_short in SWITCHES {
        for show_looping in SWITCHES {
            let listings: Vec<Vec<String>> = LADDER
                .into_iter()
                .map(|threshold| {
                    let cell = Cell {
                        mode: Mode::List,
                        show_short,
                        show_looping,
                        threshold,
                        no_diagnostics: false,
                        no_summary: false,
                    };
                    let stdout = &run_of(results, cell).stdout;
                    let listed = listed_playlists(stdout);
                    // The short hint appears exactly when the cutoff withheld
                    // something, judged against the cutoff that hides nothing.
                    let widest = LADDER.first().copied().expect("the ladder is not empty");
                    let visible = listed_playlists(
                        &run_of(results, Cell { threshold: widest, ..cell }).stdout,
                    );
                    assert_eq!(
                        stdout.contains("Hidden by filters (short):"),
                        visible.len() != listed.len(),
                        "{cell:?}: the short hint must name exactly what the cutoff withheld"
                    );
                    listed
                })
                .collect();
            // The ladder has to bite, or monotonicity holds vacuously and a
            // cutoff that never reaches the classifier would sweep green.
            let widest = listings.first().expect("the ladder is not empty");
            let narrowest = listings.last().expect("the ladder is not empty");
            assert!(!widest.is_empty(), "a cutoff of 0 must list the whole disc");
            if show_short {
                // The switch decides whether the classification withholds
                // anything, so every rung shows the same rows.
                assert!(
                    listings.iter().all(|listed| listed == widest),
                    "--show-short-playlists must override every cutoff, got {listings:?}"
                );
            } else {
                // A day-long cutoff is longer than any playlist, so the rule
                // withholds every one of them.
                assert!(narrowest.is_empty(), "a day-long cutoff must hide the whole disc");
            }
            for pair in listings.windows(2) {
                let lower = pair.first().expect("a window of two has a first");
                let higher = pair.last().expect("a window of two has a last");
                let survivors: Vec<&String> =
                    lower.iter().filter(|name| higher.contains(name)).collect();
                let kept: Vec<&String> = higher.iter().collect();
                assert_eq!(
                    kept, survivors,
                    "raising the cutoff must only hide rows, keeping the rest in order"
                );
            }
        }
    }
}

#[test]
fn the_flag_cross_product_holds_every_invariant_the_golden_implies() {
    // `--mpls` names every playlist a listing can ever show, so the sweep asks
    // for one that hides nothing rather than hard-coding the fixture's names.
    let probe = Cell {
        mode: Mode::List,
        show_short: true,
        show_looping: true,
        threshold: Some(0),
        no_diagnostics: false,
        no_summary: false,
    };
    let all_names = listed_playlists(&run(&command_line(probe, "", Order::Canonical)).stdout);
    assert!(!all_names.is_empty(), "the fixture disc lists no playlists");
    let names = all_names.join(",");

    let cells = all_cells();
    assert_eq!(cells.len(), space_size(), "the sweep lost cells while building the space");
    let expected_permutations =
        cells.len().div_ceil(PERMUTATION_STRIDE).wrapping_mul(PERMUTED_ORDERS.len());

    let mut results: BTreeMap<Cell, Run> = BTreeMap::new();
    let mut permutations = 0_usize;
    for (index, &cell) in cells.iter().enumerate() {
        let canonical = run(&command_line(cell, &names, Order::Canonical));

        // Invariant 6, per cell: a healthy disc always exits 0, `--list` writes
        // no report file, and a scan writes exactly one.
        assert_eq!(canonical.code, Some(0), "{cell:?} did not exit 0");
        let written: Vec<&str> = canonical.reports.keys().map(String::as_str).collect();
        let expected: &[&str] = if cell.mode == Mode::List { &[] } else { &[REPORT] };
        assert_eq!(written, expected, "{cell:?}: report artifacts");

        // Invariant 4, sampled: the same switches in another order, byte for
        // byte the same answer.
        if index.is_multiple_of(PERMUTATION_STRIDE) {
            for order in PERMUTED_ORDERS {
                assert_eq!(
                    run(&command_line(cell, &names, order)),
                    canonical,
                    "{cell:?} answers {order:?} differently"
                );
                permutations = permutations.wrapping_add(1);
            }
        }
        results.insert(cell, canonical);
    }

    // A cell that silently dropped out — a duplicate key, a skipped iteration —
    // is the classic failure of a matrix test, so the counts are asserted
    // rather than trusted.
    assert_eq!(results.len(), space_size(), "the sweep ran fewer cells than the space holds");
    assert_eq!(permutations, expected_permutations, "the permutation sample shrank");

    assert_anchor(&results);
    assert_section_subtraction(&results, &cells);
    assert_selection_equivalence(&results, &cells, &all_names);
    assert_cutoff_monotonicity(&results);

    assert!(
        !real_fixture(DISC).join(REPORT).exists(),
        "the sweep wrote a report into the committed fixture disc"
    );
    let _ = std::fs::remove_dir_all(dest_dir()).is_ok();
}

#[test]
fn section_stripping_reports_whether_the_heading_was_there() {
    let trimmed = without_section(GOLDEN, DIAGNOSTICS).expect("the golden carries the section");
    assert!(!trimmed.contains(DIAGNOSTICS), "the heading is gone");
    assert!(trimmed.contains(SUMMARY), "the other section stays");
    assert_eq!(without_section(&trimmed, DIAGNOSTICS), None, "nothing left to remove");
    assert_eq!(subtract(&trimmed, &[DIAGNOSTICS]), trimmed, "an absent heading subtracts nothing");
}
