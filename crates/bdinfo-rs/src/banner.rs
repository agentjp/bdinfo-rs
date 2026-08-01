//! The header printed above the CLI's help card, in the widest spelling the
//! terminal can render: a framed wordmark, a one-line colour chip, or a plain
//! ASCII line.
//!
//! Two flows print it: the help paths, and the opening of an interactive
//! picker session on a terminal. Every flag-driven mode (`--list`, `--whole`,
//! `--mpls`, `--version`) and every piped or redirected run emit the same bytes
//! with or without this module.

// In a binary, `pub` items are unexported (`unreachable_pub`), so the reachable
// visibility for these helpers is `pub(crate)` — which the nursery
// `redundant_pub_crate` then flags. `pub(crate)` is the correct choice here.
#![expect(clippy::redundant_pub_crate, reason = "pub is unreachable_pub in a binary")]

use std::fmt::Write as _;
use std::io::IsTerminal as _;

/// The one-line description every header tier carries. It repeats the clap
/// command's `about` — the two are pinned together by a test — because the
/// help card's body no longer prints it.
pub(crate) const TAGLINE: &str = "BDInfo-style Blu-ray disc reports";

// The colours below are written as SGR escapes rather than built through
// crossterm's `Stylize`: crossterm drops every colour whenever `NO_COLOR` holds
// a non-empty value (`Colored::ansi_color_disabled`, crossterm 0.29), emitting
// an empty `ESC [ m` in its place. That would silently override the tier
// [`Capabilities`] chose and make the painted bytes depend on the environment
// the code runs in.

/// The wordmark's colour, `rgb(242, 166, 90)`: the desktop app's dark-theme
/// accent, restated here rather than shared, because the CLI does not depend on
/// the GUI crate.
const ACCENT_FG: &str = "\u{1b}[38;2;242;166;90m";

/// The same accent as a fill, for the chip's background.
const ACCENT_BG: &str = "\u{1b}[48;2;242;166;90m";

/// The text colour on an accent-filled cell, `rgb(20, 17, 12)` — the chip's
/// label.
const ON_ACCENT_FG: &str = "\u{1b}[38;2;20;17;12m";

/// Closes a painted run, returning every attribute to the terminal's default.
const RESET: &str = "\u{1b}[0m";

/// The `bdinfo-rs` wordmark in the Coder Mini figlet font, one string per row,
/// each right-padded to [`WORDMARK_CELLS`] so the frame closes square.
const WORDMARK: [&str; 5] = [
    "▄▄       ▄▄             ▄▄                        ",
    "██       ██ ▀▀         ██                         ",
    "████▄ ▄████ ██  ████▄ ▀██▀ ▄███▄       ████▄ ▄█▀▀▀",
    "██ ██ ██ ██ ██  ██ ██  ██  ██ ██ ▀▀▀▀▀ ██ ▀▀ ▀███▄",
    "████▀ ▀████ ██▄ ██ ██  ██  ▀███▀       ██    ▄▄▄█▀",
];

/// The display width of one [`WORDMARK`] row, in terminal cells.
const WORDMARK_CELLS: usize = 50;

/// The display width of a framed banner line: a [`WORDMARK`] row, one space
/// either side, and the two block side rails. Also the narrowest terminal the
/// banner is offered to — below it the header falls back to the chip.
const BANNER_CELLS: usize = WORDMARK_CELLS.saturating_add(4);

/// What the terminal receiving the help can render.
///
/// Probed from stdout once per run and stated outright in tests, which is why
/// [`header`] takes it as an argument instead of reading the environment
/// itself.
#[derive(Debug)]
pub(crate) struct Capabilities {
    /// Whether stdout is a terminal rather than a pipe or a file.
    pub(crate) tty: bool,
    /// Whether that terminal renders ANSI escape sequences.
    pub(crate) ansi: bool,
    /// Whether `NO_COLOR` is set to anything at all (<https://no-color.org>).
    pub(crate) no_color: bool,
    /// The terminal's width in cells.
    pub(crate) columns: usize,
}

impl Capabilities {
    /// Probes the running process's stdout.
    pub(crate) fn probe() -> Self {
        Self {
            tty: std::io::stdout().is_terminal(),
            ansi: crate::ansi_supported(),
            no_color: std::env::var_os("NO_COLOR").is_some(),
            columns: crate::terminal_columns(),
        }
    }

    /// Whether colour may be emitted at all: a terminal that renders ANSI
    /// sequences, with `NO_COLOR` unset.
    pub(crate) const fn colors(&self) -> bool {
        self.tty && self.ansi && !self.no_color
    }
}

/// The header for `caps`, always ending in a newline: the framed wordmark when
/// there is colour and room for it, the chip when there is colour but no room,
/// and the plain line otherwise.
pub(crate) fn header(caps: &Capabilities, version: &str) -> String {
    if !caps.colors() {
        plain(version)
    } else if caps.columns < BANNER_CELLS {
        chip(version)
    } else {
        banner(version)
    }
}

/// The header a scan session opens with, followed by a blank line — or nothing
/// at all unless `interactive` and stdout is a terminal.
///
/// Only the interactive picker has a human at the keyboard to greet. A
/// flag-driven mode, or any run whose stdout is piped or redirected, gets the
/// empty string, so its output is byte-for-byte what it would be without a
/// header at all.
pub(crate) fn session_header(caps: &Capabilities, version: &str, interactive: bool) -> String {
    if interactive && caps.tty { format!("{}\n", header(caps, version)) } else { String::new() }
}

/// The framed wordmark: the accent-painted rows between uncoloured block rails,
/// closed top and bottom, then the [`plain`] line.
fn banner(version: &str) -> String {
    let inner = BANNER_CELLS.saturating_sub(2);
    let rows = WORDMARK.iter().fold(String::new(), |mut rows, row| {
        let _ = writeln!(rows, "█ {ACCENT_FG}{row}{RESET} █").is_ok();
        rows
    });
    format!("█{}█\n{rows}█{}█\n{}", "▀".repeat(inner), "▄".repeat(inner), plain(version))
}

/// The narrow-terminal header: the name and version in an accent-filled chip,
/// then the tagline.
fn chip(version: &str) -> String {
    format!("{ACCENT_BG}{ON_ACCENT_FG} bdinfo-rs {version} {RESET}  {TAGLINE}\n")
}

/// The colourless header — one ASCII line, no escape sequences at all, for a
/// pipe, a redirect, a terminal without ANSI, or `NO_COLOR`.
fn plain(version: &str) -> String {
    format!("bdinfo-rs {version} - {TAGLINE}\n")
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::{
        BANNER_CELLS, Capabilities, TAGLINE, WORDMARK, WORDMARK_CELLS, header, session_header,
    };

    /// A wide terminal with colour — the banner tier.
    fn wide() -> Capabilities {
        Capabilities { tty: true, ansi: true, no_color: false, columns: 120 }
    }

    /// `text` with every ANSI escape sequence dropped, so a painted line can be
    /// measured in cells.
    fn unstyled(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                // Every sequence the header emits is an SGR one, ending in `m`.
                while chars.next().is_some_and(|next| next != 'm') {}
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn every_wordmark_row_and_frame_line_is_one_width() {
        for row in WORDMARK {
            assert_eq!(row.chars().count(), WORDMARK_CELLS, "row {row:?}");
        }
        // The art is 50 cells wide, framed to 54 — the width the banner tier
        // needs before it is offered at all.
        assert_eq!((WORDMARK_CELLS, BANNER_CELLS), (50, 54));
    }

    #[test]
    fn the_banner_frames_the_painted_wordmark_and_ends_with_the_plain_line() {
        let out = header(&wide(), "9.9.9");
        let lines: Vec<&str> = out.lines().collect();
        let rail = |fill: &str| format!("█{}█", fill.repeat(BANNER_CELLS.saturating_sub(2)));
        let (top, bottom) = (rail("▀"), rail("▄"));
        let tail = format!("bdinfo-rs 9.9.9 - {TAGLINE}");

        // Five wordmark rows between two borders, then the plain line.
        assert_eq!(lines.len(), 8, "{out}");
        assert_eq!(lines.first().copied(), Some(top.as_str()));
        assert_eq!(lines.get(6).copied(), Some(bottom.as_str()));
        assert_eq!(lines.get(7).copied(), Some(tail.as_str()));
        // Only the wordmark cells are painted — the rails stay uncoloured.
        for (line, row) in lines.iter().skip(1).zip(WORDMARK) {
            assert_eq!(*line, format!("█ \u{1b}[38;2;242;166;90m{row}\u{1b}[0m █"));
        }
        for line in lines.iter().take(7) {
            assert_eq!(unstyled(line).chars().count(), BANNER_CELLS, "line {line:?}");
        }
    }

    #[test]
    fn a_terminal_narrower_than_the_banner_falls_back_to_the_chip() {
        let narrow = Capabilities {
            tty: true,
            ansi: true,
            no_color: false,
            columns: BANNER_CELLS.saturating_sub(1),
        };
        let out = header(&narrow, "9.9.9");
        assert_eq!(unstyled(&out), format!(" bdinfo-rs 9.9.9   {TAGLINE}\n"));
        assert!(out.contains("48;2;242;166;90"), "the chip fills with the accent: {out:?}");
        assert!(out.contains("38;2;20;17;12"), "its label reads on the accent: {out:?}");
        // Exactly at the banner's width the banner still fits.
        let exact = Capabilities { tty: true, ansi: true, no_color: false, columns: BANNER_CELLS };
        assert!(header(&exact, "9.9.9").starts_with('█'));
    }

    #[test]
    fn no_terminal_no_ansi_or_no_color_all_give_the_plain_line() {
        let expected = format!("bdinfo-rs 9.9.9 - {TAGLINE}\n");
        for caps in [
            Capabilities { tty: false, ansi: true, no_color: false, columns: 120 },
            Capabilities { tty: true, ansi: false, no_color: false, columns: 120 },
            Capabilities { tty: true, ansi: true, no_color: true, columns: 120 },
        ] {
            assert!(!caps.colors(), "{caps:?}");
            let out = header(&caps, "9.9.9");
            assert_eq!(out, expected, "{caps:?}");
            assert!(!out.contains('\u{1b}'), "no escape byte: {out:?}");
        }
        assert!(wide().colors());
    }

    #[test]
    fn only_an_interactive_run_on_a_terminal_opens_with_a_header() {
        let piped = Capabilities { tty: false, ansi: false, no_color: false, columns: 120 };
        // The header, then a blank line before the flow narration.
        assert_eq!(
            session_header(&wide(), "9.9.9", true),
            format!("{}\n", header(&wide(), "9.9.9"))
        );
        // A flag-driven mode, a pipe, or both: not a byte.
        assert_eq!(session_header(&wide(), "9.9.9", false), "");
        assert_eq!(session_header(&piped, "9.9.9", true), "");
        assert_eq!(session_header(&piped, "9.9.9", false), "");
    }

    #[test]
    fn the_probe_always_answers_with_a_usable_width() {
        // Environment-bound — under a test harness stdout is captured, so the
        // tiers themselves are exercised through stated capabilities above.
        assert!(Capabilities::probe().columns > 0, "the width probe falls back, never to 0");
    }

    #[test]
    fn the_tagline_is_the_commands_about() {
        let about = crate::Cli::command().get_about().map(ToString::to_string);
        assert_eq!(about.as_deref(), Some(TAGLINE));
    }
}
