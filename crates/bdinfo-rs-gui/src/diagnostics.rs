//! Best-effort diagnostics: the per-launch log file, the panic hook, and
//! the tolerated-error helper.
//!
//! The release binary hides the console on Windows (`windows_subsystem`), so
//! stderr does not exist — without a file, a failed boot, a panic, or a
//! settings write that didn't stick would all be invisible. This module
//! writes one plain-text log (`gui.log`, next to `gui.conf`), fed through the
//! `log` facade so the dependencies that already speak it land in the same
//! file: `iced_wgpu` logs the selected adapter (the renderer identity every
//! graphics bug report needs first) and `rfd` logs the Linux portal errors it
//! otherwise swallows.
//!
//! Each launch starts a fresh file and keeps exactly one previous generation
//! beside it as `gui.log.1` ([`init`]) — two generations bound the size, and
//! the second one exists because the first thing a user does when something
//! goes wrong is run the app again, which would otherwise destroy the record
//! of the launch that showed it. Every line is stamped with seconds since
//! launch; the one wall-clock line [`init`] writes is what places them all in
//! absolute time.
//!
//! Everything here is best-effort by contract: logging never panics, never
//! meaningfully blocks (one short line write under a mutex), and a failure
//! to log costs nothing but the log. The pure pieces — [`log_path_from`],
//! the line format, the level policy — are the Tier-A surface; the sink is
//! the same thin best-effort IO the [`settings`](crate::settings) store
//! wraps around its parser.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

/// The log file's name — a sibling of `gui.conf`.
const FILE_NAME: &str = "gui.log";

/// The log's name when it lands in the shared temp directory instead, where
/// `gui.log` would be an unattributable stray.
const FALLBACK_NAME: &str = "bdinfo-rs-gui.log";

/// The suffix the previous launch's log is renamed to.
const PREVIOUS_SUFFIX: &str = ".1";

/// Where the log lives: `gui.log` beside the config file, or
/// `bdinfo-rs-gui.log` in the OS temp directory when no config location
/// resolved.
///
/// The app runs without persistence in that case, but not without a log: a
/// desktop that resolves no config directory is exactly the environment whose
/// failures need a witness most, and a temp file is one every OS gives.
#[must_use]
pub fn log_path_from(config: Option<&Path>) -> PathBuf {
    config
        .and_then(Path::parent)
        .map_or_else(|| std::env::temp_dir().join(FALLBACK_NAME), |dir| dir.join(FILE_NAME))
}

/// Where the previous launch's log is kept: the same path with
/// [`PREVIOUS_SUFFIX`] appended (`gui.log` → `gui.log.1`).
///
/// Appended to the whole file name rather than swapped into the extension, so
/// the pair sorts together and neither name can collide with a config file.
fn previous_log_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(PREVIOUS_SUFFIX);
    PathBuf::from(name)
}

/// The process-wide sink behind the `log` facade. All state is interior,
/// because the facade hands out `&'static dyn Log`.
struct Sink {
    /// The open log file — `None` until [`init`] opens one (or forever, when
    /// no location resolved or the create failed; every write then no-ops).
    file: Mutex<Option<File>>,
    /// The launch instant — every line's timestamp is seconds since this.
    start: OnceLock<Instant>,
}

/// The one global sink [`init`] installs.
static SINK: Sink = Sink { file: Mutex::new(None), start: OnceLock::new() };

impl Sink {
    /// The file slot, poison-proof: a thread that died mid-write leaves the
    /// lock poisoned, and diagnostics must never panic in sympathy.
    fn file(&self) -> MutexGuard<'_, Option<File>> {
        self.file.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl log::Log for Sink {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        line_allowed(metadata.level(), metadata.target())
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let elapsed = self.start.get().map_or(0.0, |start| start.elapsed().as_secs_f64());
        let line =
            format_line(elapsed, record.level(), record.target(), &record.args().to_string());
        if let Some(file) = self.file().as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }

    fn flush(&self) {}
}

/// One rendered log line: `[   12.345s] LEVEL target: message`.
fn format_line(elapsed_secs: f64, level: log::Level, target: &str, message: &str) -> String {
    format!("[{elapsed_secs:9.3}s] {level:<5} {target}: {message}")
}

/// The level policy — deliberately not configurable (one behaviour to
/// support): warnings and errors from anyone (rfd's swallowed portal
/// errors, wgpu's device warnings), Info only from our own crates and
/// `iced_wgpu` (whose Info lines carry the adapter selection), nothing
/// below Info.
fn line_allowed(level: log::Level, target: &str) -> bool {
    match level {
        log::Level::Error | log::Level::Warn => true,
        log::Level::Info => target.starts_with("bdinfo_rs_gui") || target.starts_with("iced_wgpu"),
        log::Level::Debug | log::Level::Trace => false,
    }
}

/// Rotates the previous log out of the way, opens a fresh file at `path`,
/// installs the global logger, and writes the launch's wall-clock anchor.
///
/// Called once, first thing in `main`. Best-effort at every step: an
/// uncreatable directory, a rename that finds nothing to rename, or a failed
/// open all leave a sink that swallows writes — the app never notices the
/// difference.
pub fn init(path: &Path) {
    let _ = SINK.start.set(Instant::now());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // The previous launch's file becomes `gui.log.1` before this one truncates
    // it away; the rename fails harmlessly on the first launch, when there is
    // nothing there yet.
    let _ = std::fs::rename(path, previous_log_path(path));
    if let Ok(file) = File::create(path) {
        *SINK.file() = Some(file);
    }
    // Err means a logger is already installed (a test re-initializing) — the
    // installed one keeps working; the level cap applies either way.
    let _ = log::set_logger(&SINK);
    log::set_max_level(log::LevelFilter::Info);
    log::info!("launched {} UTC", utc_stamp(epoch_secs()));
}

/// Seconds since the Unix epoch, now. A clock set before the epoch reads as
/// the epoch itself rather than failing the launch line.
fn epoch_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// `secs` (seconds since the Unix epoch) as a UTC `YYYY-MM-DD HH:MM:SS` stamp
/// — the launch line every relative timestamp in the file is measured from.
///
/// UTC rather than local time: safe `std` reads no timezone offset, and an
/// unambiguous instant is what a log arriving from another timezone needs
/// anyway.
///
/// The date is Howard Hinnant's `civil_from_days`
/// (`howardhinnant.github.io/date_algorithms.html`), whose 400-year era
/// begins on 1 March so that leap days fall at the end of a year and need no
/// special case. `wrapping_*` is exact here, never a tolerated overflow:
/// every intermediate is bounded by the input, and even `u64::MAX` seconds —
/// about 2.1e14 days — leaves the largest product below (`era * 146_097`)
/// four orders of magnitude short of `u64::MAX`.
fn utc_stamp(secs: u64) -> String {
    let days = secs / 86_400;
    let time = secs % 86_400;
    let (hour, minute, second) = (time / 3_600, time % 3_600 / 60, time % 60);
    // 719_468 is the day count from the era's 0000-03-01 origin to the epoch.
    let z = days.wrapping_add(719_468);
    let era = z / 146_097;
    let doe = z.wrapping_sub(era.wrapping_mul(146_097));
    let yoe =
        doe.wrapping_sub(doe / 1_460).wrapping_add(doe / 36_524).wrapping_sub(doe / 146_096) / 365;
    let doy = doe.wrapping_sub(yoe.wrapping_mul(365).wrapping_add(yoe / 4).wrapping_sub(yoe / 100));
    let mp = doy.wrapping_mul(5).wrapping_add(2) / 153;
    let day = doy.wrapping_sub(mp.wrapping_mul(153).wrapping_add(2) / 5).wrapping_add(1);
    // The era's months run March (0) to February (11), so 10 and 11 are the
    // January and February that belong to the NEXT calendar year.
    let month = if mp < 10 { mp.wrapping_add(3) } else { mp.wrapping_sub(9) };
    let year = era.wrapping_mul(400).wrapping_add(yoe).wrapping_add(u64::from(month <= 2));
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// Installs a panic hook that writes the payload, location and a backtrace to
/// the log before the default hook runs.
///
/// Release builds keep it too — a hidden Windows console leaves the log as
/// the only witness. The scan worker's `PanicAlarm` still reports the death
/// to the UI; this records **why**.
///
/// The backtrace is forced rather than left to `RUST_BACKTRACE`, which nobody
/// running a desktop app has set. The shipped binary is built with
/// `strip = true`, so its frames may resolve to addresses and module offsets
/// only — still the difference between a locatable crash and a bare message.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map_or(String::new(), ToString::to_string);
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("(non-string panic payload)");
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!(target: "panic", "panicked at {location}: {message}\n{backtrace}");
        previous(info);
    }));
}

/// Tolerated-`Result` logging.
///
/// Where the code deliberately swallows an error (a settings write, an
/// autosave destination, a recall of a vanished path), `log_err` records it
/// with the swallow's own `file:line` before the value flows on unchanged —
/// the tolerance stays, the invisibility goes.
pub trait LogErr {
    /// Logs the `Err` (if any) at Warn, tagged with the caller's `file:line`
    /// and `what` naming the tolerated operation, and returns `self`.
    #[must_use]
    #[track_caller]
    fn log_err(self, what: &str) -> Self;
}

impl<T, E: std::fmt::Display> LogErr for Result<T, E> {
    #[track_caller]
    fn log_err(self, what: &str) -> Self {
        if let Err(error) = &self {
            let caller = std::panic::Location::caller();
            log::warn!("{what} failed at {}:{}: {error}", caller.file(), caller.line());
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    use super::{
        LogErr, Sink, format_line, line_allowed, log_path_from, previous_log_path, utc_stamp,
    };

    #[test]
    fn the_log_lives_next_to_the_config_or_in_the_temp_dir() {
        // Built by joins, not path literals: a Windows-separator literal is one
        // opaque component on Unix, where `parent()` would strip nothing.
        let dir = PathBuf::from("config-home").join("bdinfo-rs");
        let config = dir.join("gui.conf");
        assert_eq!(log_path_from(Some(&config)), dir.join("gui.log"));
        // No config location, and a parentless config path that can have no
        // sibling, both fall back to a named file in the temp directory.
        let fallback = std::env::temp_dir().join("bdinfo-rs-gui.log");
        assert_eq!(log_path_from(None), fallback);
        assert_eq!(log_path_from(Some(Path::new("/"))), fallback);
    }

    #[test]
    fn the_previous_generation_keeps_the_whole_name() {
        // `.1` after the extension, not instead of it: `gui.log.1` sorts beside
        // `gui.log` and can never be mistaken for another kind of file.
        assert_eq!(previous_log_path(Path::new("dir/gui.log")), PathBuf::from("dir/gui.log.1"));
    }

    #[test]
    fn the_wall_clock_stamp_renders_utc_civil_time() {
        // The epoch itself, and the last second of that day.
        assert_eq!(utc_stamp(0), "1970-01-01 00:00:00");
        assert_eq!(utc_stamp(86_399), "1970-01-01 23:59:59");
        // 1970-03-01: the day the era's month numbering starts from, so this
        // is the arm that renders a month without the year correction.
        assert_eq!(utc_stamp(5_097_600), "1970-03-01 00:00:00");
        // A leap day: February belongs to the previous era-year, so its date
        // renders only if the year correction applies to month 2 as well as 1.
        assert_eq!(utc_stamp(1_709_164_800), "2024-02-29 00:00:00");
        // Two arbitrary instants with every field non-zero.
        assert_eq!(utc_stamp(1_000_000_000), "2001-09-09 01:46:40");
        assert_eq!(utc_stamp(1_700_000_000), "2023-11-14 22:13:20");
    }

    #[test]
    fn a_line_formats_with_timestamp_level_and_target() {
        assert_eq!(
            format_line(0.0, log::Level::Info, "bdinfo_rs_gui", "hello"),
            "[    0.000s] INFO  bdinfo_rs_gui: hello"
        );
        assert_eq!(format_line(12.3456, log::Level::Warn, "t", "m"), "[   12.346s] WARN  t: m");
        assert_eq!(
            format_line(2.0, log::Level::Error, "panic", "boom"),
            "[    2.000s] ERROR panic: boom"
        );
    }

    #[test]
    fn the_level_policy_is_warnings_from_anyone_info_from_the_allowlist() {
        // Warn and Error pass from any target (rfd, wgpu, winit…).
        assert!(line_allowed(log::Level::Error, "rfd::backend"));
        assert!(line_allowed(log::Level::Warn, "wgpu_core::device"));
        // Info passes only from our own crates and iced_wgpu.
        assert!(line_allowed(log::Level::Info, "bdinfo_rs_gui"));
        assert!(line_allowed(log::Level::Info, "bdinfo_rs_gui::settings"));
        assert!(line_allowed(log::Level::Info, "iced_wgpu::window::compositor"));
        assert!(!line_allowed(log::Level::Info, "wgpu_core::instance"));
        assert!(!line_allowed(log::Level::Info, "naga::valid"));
        // Nothing below Info, not even from us.
        assert!(!line_allowed(log::Level::Debug, "bdinfo_rs_gui"));
        assert!(!line_allowed(log::Level::Trace, "iced_wgpu"));
    }

    /// A scratch path under the target dir (unique per test).
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/diagnostics-tests");
        dir.join(name)
    }

    /// One `log::Record` for driving the `Log` impl directly.
    fn record<'a>(
        level: log::Level,
        target: &'a str,
        args: std::fmt::Arguments<'a>,
    ) -> log::Record<'a> {
        log::Record::builder().level(level).target(target).args(args).build()
    }

    #[test]
    fn a_sink_writes_allowed_lines_and_drops_the_rest() {
        let path = scratch("local-sink.log");
        let _ = std::fs::create_dir_all(path.parent().expect("a parent"));
        let file = std::fs::File::create(&path).expect("scratch create");
        // A local sink with no launch instant: timestamps render as 0.000.
        let sink = Sink { file: Mutex::new(Some(file)), start: OnceLock::new() };
        log::Log::log(&sink, &record(log::Level::Info, "bdinfo_rs_gui", format_args!("kept")));
        log::Log::log(&sink, &record(log::Level::Info, "wgpu_core", format_args!("dropped")));
        log::Log::log(&sink, &record(log::Level::Debug, "bdinfo_rs_gui", format_args!("below")));
        log::Log::log(&sink, &record(log::Level::Error, "rfd", format_args!("portal")));
        log::Log::flush(&sink);
        let text = std::fs::read_to_string(&path).expect("scratch read");
        assert_eq!(
            text,
            "[    0.000s] INFO  bdinfo_rs_gui: kept\n[    0.000s] ERROR rfd: portal\n"
        );
    }

    #[test]
    fn a_fileless_sink_swallows_writes() {
        let sink = Sink { file: Mutex::new(None), start: OnceLock::new() };
        // Nothing to observe — the contract is simply "does not panic".
        log::Log::log(&sink, &record(log::Level::Error, "panic", format_args!("nowhere")));
    }

    /// The one test that touches process-global state (the installed logger,
    /// the panic hook): everything global runs INSIDE this single function,
    /// sequentially, so no other test races it.
    #[test]
    fn the_global_logger_rotates_hooks_panics_and_tags_tolerated_errors() {
        let path = scratch("global/gui.log");
        let previous = super::previous_log_path(&path);
        let _ = std::fs::create_dir_all(path.parent().expect("a parent"));
        let _ = std::fs::remove_file(&previous);
        std::fs::write(&path, "stale content from a previous launch\n").expect("scratch write");

        super::init(&path);
        let after_init = std::fs::read_to_string(&path).expect("scratch read");
        assert_eq!(
            std::fs::read_to_string(&previous).expect("the previous generation reads"),
            "stale content from a previous launch\n",
            "the launch before this one is kept, not destroyed"
        );

        // The launch line anchors every relative timestamp in the file. Its
        // date is read from the clock, so it can only be this century.
        let anchor = after_init.lines().next().expect("init writes the launch line");
        assert!(anchor.contains("launched 20"), "the anchor carries a real date: {anchor}");
        assert!(anchor.ends_with(" UTC"), "the anchor names its timezone: {anchor}");
        assert_eq!(after_init.lines().count(), 1, "and nothing else survived the launch");

        // The facade now reaches the file, at the Info cap.
        log::info!("MARKER_HEADER");
        log::debug!("MARKER_DEBUG");
        let text = std::fs::read_to_string(&path).expect("scratch read");
        assert!(text.contains("MARKER_HEADER"));
        assert!(!text.contains("MARKER_DEBUG"));

        // log_err: the Err arm logs `what` + this file's name + the error;
        // the Ok arm stays silent.
        let err: Result<(), &str> = Err("MARKER_BOOM");
        assert_eq!(err.log_err("MARKER_OP"), Err("MARKER_BOOM"));
        let ok: Result<(), &str> = Ok(());
        assert_eq!(ok.log_err("MARKER_FINE"), Ok(()));
        let text = std::fs::read_to_string(&path).expect("scratch read");
        assert!(text.contains("MARKER_OP failed at"));
        assert!(text.contains("diagnostics.rs"));
        assert!(text.contains("MARKER_BOOM"));
        assert!(!text.contains("MARKER_FINE"));

        // The panic hook writes payload + location + backtrace before the
        // previous hook runs. A no-op previous hook keeps the test output
        // clean; each payload shape (&str, String, non-string) lands.
        std::panic::set_hook(Box::new(|_| {}));
        super::install_panic_hook();
        assert!(std::panic::catch_unwind(|| std::panic::panic_any("MARKER_STR")).is_err());
        assert!(
            std::panic::catch_unwind(|| std::panic::panic_any(String::from("MARKER_STRING")))
                .is_err()
        );
        assert!(std::panic::catch_unwind(|| std::panic::panic_any(42_u32)).is_err());
        let text = std::fs::read_to_string(&path).expect("scratch read");
        assert!(text.contains("panicked at"));
        assert!(text.contains("diagnostics.rs"));
        assert!(text.contains("MARKER_STR"));
        assert!(text.contains("MARKER_STRING"));
        assert!(text.contains("(non-string panic payload)"));
        // The forced backtrace follows the message: frames are rendered
        // `   0: symbol`, one per line, by `Backtrace`'s own Display.
        assert!(text.contains("\n   0: "), "the panic record carries a backtrace");

        // Re-initialization is tolerated: an unopenable path leaves the open
        // file in place, and the facade still lands.
        super::init(Path::new("/"));
        log::info!("MARKER_STILL_ALIVE");
        let text = std::fs::read_to_string(&path).expect("scratch read");
        assert!(text.contains("MARKER_STILL_ALIVE"));
    }
}
