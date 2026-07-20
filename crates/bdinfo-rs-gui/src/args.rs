//! The window binary's unattended-launch contract: the argv surface and the
//! attended-session evidence.
//!
//! Package-manager validation harnesses (winget's sandbox first among them)
//! launch installed executables with no human present and judge the exit
//! code; the near-universal probe for a GUI binary is `--version`. This
//! module holds the two pure decisions the binary's shell makes before (or
//! without) any window machinery: what argv asked for ([`classify`]), and
//! whether a human is plausibly present to dismiss a blocking dialog
//! ([`attended`] / [`attended_unix`]).
//!
//! Deliberately hand-rolled rather than clap: the surface is three cases,
//! and the crate's dependency budget does not extend to an argument parser
//! whose only job is to be recognised by an installer smoke test.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// The `--version` output: `bdinfo-rs-gui <version>`.
///
/// The same `<binary> <version>` shape clap prints for the `bdinfo-rs` CLI —
/// this project's own post-release install smoke
/// (`.github/workflows/install-smoke-test.yml`) asserts on that convention.
pub const VERSION_LINE: &str = concat!("bdinfo-rs-gui ", env!("CARGO_PKG_VERSION"));

/// The `--help` output.
///
/// The binary's whole surface: an optional disc path and the two
/// informational flags — there is nothing else to document.
pub const USAGE: &str = concat!(
    "bdinfo-rs-gui ",
    env!("CARGO_PKG_VERSION"),
    " — native desktop GUI for the bdinfo-rs Blu-ray analyzer

Usage: bdinfo-rs-gui [BD_PATH]

Arguments:
  [BD_PATH]  Blu-ray disc to open at launch: a disc folder (the disc root,
             its BDMV directory, or any directory inside it) or an .iso
             image. Without it the window opens empty.

Options:
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit"
);

/// What the process was asked to do, decided from argv alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Open the window; `Some` opens that disc at boot.
    Window(Option<PathBuf>),
    /// Print [`VERSION_LINE`] to stdout and exit 0 — never opens a window.
    Version,
    /// Print [`USAGE`] to stdout and exit 0 — never opens a window.
    Help,
}

/// Classifies argv (WITHOUT the program name) into an [`Invocation`].
///
/// The flags `--version`/`-V` and `--help`/`-h` — exactly clap's flag set for the
/// `bdinfo-rs` CLI, so the two binaries answer the same probes — are matched
/// only as the first argument and only byte-exactly: every other first
/// argument is a disc path taken verbatim, so a non-UTF-8 path survives
/// intact (the flag literals are ASCII, so a non-UTF-8 argument can never be
/// one). Arguments past the first are ignored, as they always have been.
#[must_use]
pub fn classify<I: IntoIterator<Item = OsString>>(args: I) -> Invocation {
    let Some(first) = args.into_iter().next() else {
        return Invocation::Window(None);
    };
    match first.to_str() {
        Some("--version" | "-V") => Invocation::Version,
        Some("--help" | "-h") => Invocation::Help,
        _ => Invocation::Window(Some(PathBuf::from(first))),
    }
}

/// Whether a human is plausibly present at the desktop, decided from the
/// injected environment values.
///
/// The gate for showing any blocking dialog: a modal in an unattended
/// session hangs the process until some job timeout, strictly worse than
/// the non-zero exit it decorates.
///
/// Two automation signals, either one meaning "nobody can dismiss a modal":
///
/// - `ci`: the `CI` environment variable, the convention every major CI service exports (GitHub
///   Actions, GitLab CI, Travis, Azure Pipelines). Presence is the whole signal; the value is never
///   parsed, so `CI=false` also counts as automation — the conservative reading, since a wrongly
///   suppressed dialog still leaves the stderr line, the log, and the exit code, while a wrongly
///   shown one blocks forever.
/// - `opt_out`: `BDINFO_GUI_NONINTERACTIVE`, the explicit handle for harnesses that export no `CI`
///   (winget's validation sandbox sets none).
///
/// An empty value counts as unset, matching the `VAR= cmd` shell convention
/// for clearing a variable.
#[must_use]
pub fn attended(ci: Option<&OsStr>, opt_out: Option<&OsStr>) -> bool {
    !is_set(ci) && !is_set(opt_out)
}

/// [`attended`], plus the display-server evidence a unix desktop always
/// carries.
///
/// With neither `DISPLAY` (X11) nor `WAYLAND_DISPLAY` (Wayland) set there
/// is no server a dialog could render on — the headless case where the boot
/// itself failed with `WindowCreationFailed` and rfd's desktop-portal call
/// could only error or stall.
#[must_use]
pub fn attended_unix(
    ci: Option<&OsStr>,
    opt_out: Option<&OsStr>,
    display: Option<&OsStr>,
    wayland_display: Option<&OsStr>,
) -> bool {
    attended(ci, opt_out) && (is_set(display) || is_set(wayland_display))
}

/// A variable counts only when set and non-empty (`VAR= cmd` clears it).
fn is_set(value: Option<&OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;

    use super::Invocation;

    fn classify(args: &[&str]) -> Invocation {
        super::classify(args.iter().map(OsString::from))
    }

    #[test]
    fn empty_argv_opens_an_empty_window() {
        assert_eq!(classify(&[]), Invocation::Window(None));
    }

    #[test]
    fn the_version_flags_are_recognised() {
        assert_eq!(classify(&["--version"]), Invocation::Version);
        assert_eq!(classify(&["-V"]), Invocation::Version);
    }

    #[test]
    fn the_help_flags_are_recognised() {
        assert_eq!(classify(&["--help"]), Invocation::Help);
        assert_eq!(classify(&["-h"]), Invocation::Help);
    }

    #[test]
    fn any_other_first_argument_is_a_path() {
        assert_eq!(
            classify(&["D:\\MY_DISC"]),
            Invocation::Window(Some(PathBuf::from("D:\\MY_DISC")))
        );
        // Only the exact literals are flags: case, prefixes, and lookalikes
        // stay paths.
        assert_eq!(classify(&["--Version"]), Invocation::Window(Some(PathBuf::from("--Version"))));
        assert_eq!(classify(&["-v"]), Invocation::Window(Some(PathBuf::from("-v"))));
        assert_eq!(
            classify(&["--version=x"]),
            Invocation::Window(Some(PathBuf::from("--version=x")))
        );
        assert_eq!(classify(&["--h"]), Invocation::Window(Some(PathBuf::from("--h"))));
    }

    #[test]
    fn only_the_first_argument_is_inspected() {
        assert_eq!(
            classify(&["disc", "--version"]),
            Invocation::Window(Some(PathBuf::from("disc")))
        );
    }

    /// An `OsString` that is valid on the platform but not valid UTF-8.
    #[cfg(unix)]
    fn non_utf8() -> OsString {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(vec![b'd', b'i', b's', b'c', 0x80])
    }

    /// An `OsString` holding a lone UTF-16 surrogate — representable in a
    /// Windows path, unrepresentable in UTF-8.
    #[cfg(windows)]
    fn non_utf8() -> OsString {
        use std::os::windows::ffi::OsStringExt as _;
        OsString::from_wide(&[b'd'.into(), 0xD800])
    }

    #[test]
    fn a_non_utf8_argument_survives_as_a_path() {
        let raw = non_utf8();
        assert_eq!(super::classify([raw.clone()]), Invocation::Window(Some(PathBuf::from(raw))));
    }

    /// An env-var value as the probes take it (they all accept an `Option`).
    #[expect(clippy::unnecessary_wraps, reason = "mirrors the Option-taking probe signatures")]
    fn os(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }

    #[test]
    fn attended_means_no_automation_evidence() {
        assert!(super::attended(None, None));
        assert!(!super::attended(os("true"), None));
        assert!(!super::attended(os("false"), None)); // presence, not value
        assert!(!super::attended(None, os("1")));
        assert!(!super::attended(os("true"), os("1")));
        // Empty is unset (`VAR= cmd`).
        assert!(super::attended(os(""), os("")));
    }

    #[test]
    fn attended_unix_also_needs_a_display_server() {
        assert!(super::attended_unix(None, None, os(":0"), None));
        assert!(super::attended_unix(None, None, None, os("wayland-0")));
        assert!(super::attended_unix(None, None, os(":0"), os("wayland-0")));
        assert!(!super::attended_unix(None, None, None, None));
        assert!(!super::attended_unix(None, None, os(""), os("")));
        // Automation evidence wins even with a display present.
        assert!(!super::attended_unix(os("true"), None, os(":0"), None));
        assert!(!super::attended_unix(None, os("1"), None, os("wayland-0")));
    }

    mod prop {
        use std::ffi::OsString;
        use std::path::PathBuf;

        use proptest::prelude::*;

        use super::super::Invocation;

        proptest! {
            /// Totality: any first argument that is not one of the four flag
            /// literals — including anything non-ASCII — is a path, verbatim.
            #[test]
            fn every_non_flag_argument_classifies_as_its_own_path(
                // The flag literals ride along at weight 1-in-10 purely so
                // the filter's rejection arm runs (an arbitrary String is
                // never one of them); the filter then discards them — their
                // classification is pinned by the unit tests above.
                first in prop_oneof![
                    1 => proptest::sample::select(vec!["--version", "-V", "--help", "-h"])
                        .prop_map(str::to_owned),
                    9 => any::<String>(),
                ]
                    .prop_filter("not a flag", |s| {
                        !matches!(s.as_str(), "--version" | "-V" | "--help" | "-h")
                    })
            ) {
                let arg = OsString::from(first.clone());
                prop_assert_eq!(
                    super::super::classify([arg]),
                    Invocation::Window(Some(PathBuf::from(first)))
                );
            }
        }
    }
}
