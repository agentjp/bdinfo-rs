//! Tier A — best-effort diagnostics: the per-launch log file, the panic
//! hook, and the tolerated-error helper.
//!
//! The release binary hides the console on Windows (`windows_subsystem`), so
//! stderr does not exist — without a file, a failed boot, a panic, or a
//! settings write that didn't stick would all be invisible. This module
//! writes one plain-text log (`gui.log`, next to `gui.conf`), truncated at
//! each launch (per-launch truncation bounds its size — no rotation), fed
//! through the `log` facade so the dependencies that already speak it land
//! in the same file: `iced_wgpu` logs the selected adapter (the renderer
//! identity every graphics bug report needs first) and `rfd` logs the Linux
//! portal errors it otherwise swallows.
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

/// Where the log lives: `gui.log` beside the config file. `None` when no
/// config path resolved — the app then runs without a log, exactly as it
/// runs without persistence.
#[must_use]
pub fn log_path_from(config: Option<&Path>) -> Option<PathBuf> {
    Some(config?.parent()?.join(FILE_NAME))
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

/// Opens (truncating) the log file at `path` and installs the global logger.
///
/// Called once, first thing in `main`. Best-effort at every step: a `None`
/// path, an uncreatable directory, or a failed open all leave a sink that
/// swallows writes — the app never notices the difference.
pub fn init(path: Option<&Path>) {
    let _ = SINK.start.set(Instant::now());
    if let Some(path) = path {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(file) = File::create(path) {
            *SINK.file() = Some(file);
        }
    }
    // Err means a logger is already installed (a test re-initializing) — the
    // installed one keeps working; the level cap applies either way.
    let _ = log::set_logger(&SINK);
    log::set_max_level(log::LevelFilter::Info);
}

/// Installs a panic hook that writes the payload and location to the log
/// before the default hook runs.
///
/// Release builds keep it too — a hidden Windows console leaves the log as
/// the only witness. The scan worker's `PanicAlarm` still reports the death
/// to the UI; this records **why**.
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
        log::error!(target: "panic", "panicked at {location}: {message}");
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

    use super::{LogErr, Sink, format_line, line_allowed, log_path_from};

    #[test]
    fn the_log_lives_next_to_the_config() {
        // Built by joins, not path literals: a Windows-separator literal is one
        // opaque component on Unix, where `parent()` would strip nothing.
        let dir = PathBuf::from("config-home").join("bdinfo-rs");
        let config = dir.join("gui.conf");
        assert_eq!(log_path_from(Some(&config)), Some(dir.join("gui.log")));
        // No config location → no log location (run without either).
        assert_eq!(log_path_from(None), None);
        // A parentless config path can't have a sibling.
        assert_eq!(log_path_from(Some(Path::new("/"))), None);
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
    fn the_global_logger_truncates_hooks_panics_and_tags_tolerated_errors() {
        let path = scratch("global/gui.log");
        let _ = std::fs::create_dir_all(path.parent().expect("a parent"));
        std::fs::write(&path, "stale content from a previous launch\n").expect("scratch write");

        super::init(Some(&path));
        let after_init = std::fs::read_to_string(&path).expect("scratch read");
        assert_eq!(after_init, "", "init truncates the previous launch's log");

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

        // The panic hook writes payload + location before the previous hook
        // runs. A no-op previous hook keeps the test output clean; each
        // payload shape (&str, String, non-string) lands.
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

        // Re-initialization is tolerated: a pathless init keeps the open
        // file, an unopenable path keeps it too, and the facade still lands.
        super::init(None);
        super::init(Some(Path::new("/")));
        log::info!("MARKER_STILL_ALIVE");
        let text = std::fs::read_to_string(&path).expect("scratch read");
        assert!(text.contains("MARKER_STILL_ALIVE"));
    }
}
