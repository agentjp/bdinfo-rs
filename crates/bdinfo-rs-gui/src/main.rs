//! `bdinfo-rs-gui` — the native desktop window for the bdinfo-rs Blu-ray
//! analyzer.
//!
//! The thin iced shell over the [`bdinfo_rs_gui`] library: it drives the
//! full two-phase flow — pick a disc FOLDER or `.iso`, fast structural scan →
//! selectable playlist table → measured scan on a worker thread with a live
//! progress bar → the rendered report, with Save / Copy. Every `update` arm calls
//! a library transition ([`Flow`]) or seam ([`scan`]) and applies the result; the
//! `view` is a thin projection of [`Flow`] onto widgets dressed in the
//! "Projection Booth" identity ([`theme`] + [`ui`]). The only impure thing the
//! shell owns is the measured-scan worker thread and the channel that streams its
//! progress back as messages, plus the OS-theme and file-drop subscriptions and
//! the best-effort config store ([`settings`]): the window geometry, the last
//! opened source, and every Settings-dialog preference persist across launches.
//! A disc can arrive three ways — the pickers, a drag-and-drop onto the window,
//! or a boot argument (`bdinfo-rs-gui <path>`) — all funnelling into the same
//! open flow. The recalled last source is not a fourth: it only pre-fills the
//! Source field with Rescan live (see `boot`); nothing scans until asked.
//! The only non-window surface is `--version`/`--help` ([`args::classify`]),
//! answered on stdout with exit 0 before any iced machinery loads — the
//! handle unattended packaging validators probe a shipped binary with.
#![forbid(unsafe_code)]
// The window is a leaf binary, not a library: a Windows release build hides the
// console so launching it does not flash a terminal. No effect on other targets.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod ui;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bdinfo_rs_core::bdrom::disc::{HIDDEN_STREAMS_NOTE, PlaylistSummary, ScanProgress};
use bdinfo_rs_core::error::ScanError;
use bdinfo_rs_core::report;
use bdinfo_rs_gui::diagnostics::{self, LogErr as _};
use bdinfo_rs_gui::flow::{self, Flow, ScanRequest, Stage};
use bdinfo_rs_gui::model::{SelectableRow, Sort, SortColumn, ViewSettings, format_file_size};
use bdinfo_rs_gui::panes::{CodecRow, StreamFileRow};
use bdinfo_rs_gui::progress::{self, ProgressModel};
use bdinfo_rs_gui::scan::{self, Input, Structural};
use bdinfo_rs_gui::theme::{Palette, ThemePref};
use bdinfo_rs_gui::{args, clipboard, columns, icon, paths, settings};
use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Column, PaneGrid, Row, Space, Stack, button, container, mouse_area, pane_grid, progress_bar,
    responsive, scrollable, text, text_input, tooltip,
};
use iced::{Element, Length, Task};

/// The window's initial logical size — a **portrait-leaning** footprint (taller
/// than wide), because the three master-detail panes are stacked vertically, so
/// the vertical axis is where the value is. Roughly `BDInfo`'s width but much
/// taller — it fills the work area of a 1080-tall display and overflows a
/// smaller one (see `debug_window_override`, which exists because the
/// 960-tall default hangs off-screen on a 1024x768 CI runner). The layout is
/// fully responsive (`Fill`/`FillPortion` + scrollable panes), so any size works;
/// long codec descriptions clip at the pane edge exactly as they do in `BDInfo`.
const WINDOW_SIZE: (f32, f32) = (880.0, 960.0);
/// The minimum size the window can be resized down to — low enough to fit a
/// small screen, beyond which the panes scroll rather than disappear (so the
/// window never gets stuck larger than a modest display).
const MIN_SIZE: (f32, f32) = (680.0, 540.0);
/// The window-icon resolution (procedurally drawn; winit downscales as needed).
const ICON_SIZE: u32 = 128;
/// The application identity handed to the windowing system — `Settings::id` on
/// every platform, and (via [`window_settings`]) the Wayland/X11
/// `application_id` on Linux. On Wayland the runtime RGBA icon is ignored by
/// protocol: the dock icon, window grouping, and pinning come solely from a
/// `.desktop` file matched against this id — first by basename, then by
/// `StartupWMClass`. The shipped entry is named for the AppStream component id
/// instead, so it is the `StartupWMClass` line that must equal this string. An
/// empty id (iced's Linux default) means a generic gear icon and broken
/// grouping.
const APP_ID: &str = "bdinfo-rs-gui";

fn main() -> ExitCode {
    // The two informational flags are answered before any iced/winit/tokio
    // machinery loads: package-manager validation harnesses launch installed
    // binaries unattended with `--version` and judge the exit code, so this
    // path must not depend on a display, a GPU, or an event loop. The print
    // results are discarded exactly like the `bdinfo-rs` CLI's bare-invocation
    // help print: a closed or failed stdout must not turn an informational
    // print into a non-zero exit. On a Windows release build
    // (`windows_subsystem = "windows"`) no console is attached and the prints
    // go nowhere — the exit code is the contract a validator reads; attaching
    // to the parent console would take `AttachConsole`, an `unsafe` extern
    // call, unavailable under `forbid(unsafe_code)`.
    match args::classify(std::env::args_os().skip(1)) {
        args::Invocation::Version => {
            let _ = writeln!(std::io::stdout(), "{}", args::VERSION_LINE).is_ok();
            ExitCode::SUCCESS
        }
        args::Invocation::Help => {
            let _ = writeln!(std::io::stdout(), "{}", args::USAGE).is_ok();
            ExitCode::SUCCESS
        }
        args::Invocation::Window(path) => run_window(path),
    }
}

/// Boots the window: diagnostics, the persisted state, then the iced
/// application. `open` is the disc path argv carried, if any. A failed boot
/// exits non-zero with the error on stderr; the dialog in
/// [`surface_boot_failure`] is presentation, not the exit path.
fn run_window(open: Option<PathBuf>) -> ExitCode {
    // One boot-time resolution of the config path and load of the persisted
    // state: the geometry feeds the window settings, the rest seeds the app in
    // `boot`. The path travels into the app so every later save writes the
    // same file (and a test-built `App` — path `None` — never writes at all).
    let config = config_path();
    // Diagnostics come up before anything can fail: the per-launch log next
    // to the config file, the panic hook (release too — a hidden Windows
    // console leaves the log as the only witness), and a launch header. The
    // renderer identity lands by itself: iced_wgpu logs its adapter selection
    // through the same facade once the compositor initializes.
    let log_path = diagnostics::log_path_from(config.as_deref());
    diagnostics::init(log_path.as_deref());
    diagnostics::install_panic_hook();
    log::info!(
        "bdinfo-rs-gui {} ({}, {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    match &config {
        Some(path) => log::info!("config: {}", path.display()),
        None => log::info!("config: no base directory resolved; running without persistence"),
    }
    let persisted = config.as_deref().map_or_else(settings::Settings::default, settings::load_from);
    let persisted = debug_window_override(persisted);
    let window = window_settings(&persisted);
    let result = iced::application(
        move || boot(config.clone(), persisted.clone(), open.clone()),
        App::update,
        App::view,
    )
    // Replaces the whole Settings, so it comes before the font
    // builders below (which merge into it).
    .settings(iced::Settings { id: Some(APP_ID.to_owned()), ..iced::Settings::default() })
    .title(App::title)
    .theme(App::theme)
    .style(App::style)
    // The persisted UI scale, multiplied on top of the OS DPI — the
    // X11 fractional-DPI escape hatch, doubling as an accessibility
    // zoom. The open Settings dialog's draft previews its pick live,
    // exactly like the theme chips.
    .scale_factor(App::ui_scale)
    // "Auto" tracks the desktop live; while the initial scan runs, also drive
    // the indeterminate progress-bar animation, and while the measured scan
    // runs, the 1 s liveness tick.
    .subscription(App::subscription)
    .default_font(ui::UI)
    .font(
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Inter-Variable.ttf"))
            .as_slice(),
    )
    .font(
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/JetBrainsMono-Regular.ttf"
        ))
        .as_slice(),
    )
    .window(window)
    .run();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Returning `ExitCode` instead of the error skips `Termination`'s
            // own stderr report, so the error is printed here — before the
            // dialog, which blocks on a human.
            let _ = writeln!(std::io::stderr(), "Error: {error}").is_ok();
            surface_boot_failure(&error, log_path.as_deref());
            ExitCode::FAILURE
        }
    }
}

/// A failed boot (both compositors declining, or the event loop dying) must
/// not end as an invisible exit 1: log it, then — only for a human at a
/// desktop — show it. rfd's synchronous message dialog is plain `MessageBoxW`
/// on Windows — a native modal that needs no event loop, so it renders fine
/// after `run()` has failed (verified live) — and `NSAlert` on macOS. On
/// Linux it rides the desktop portal over zbus, whose tokio backend panics
/// without an ambient runtime; the app's executor died with the boot, so the
/// dialog runs inside a fresh one. Wherever no dialog host exists it fails
/// into the same log (best-effort by construction — rfd routes its own
/// errors to the facade).
///
/// In an unattended session ([`attended_session`]) the dialog is suppressed:
/// a modal nobody can dismiss blocks the process until some job timeout —
/// strictly worse than the non-zero exit and stderr line it decorates, and
/// exactly what a packaging validation sandbox with a desktop but no human
/// would hit.
fn surface_boot_failure(error: &iced::Error, log_path: Option<&std::path::Path>) {
    log::error!("boot failed: {error}");
    if !attended_session() {
        log::info!("unattended session: boot-failure dialog suppressed");
        return;
    }
    let mut description = format!("The window could not be started.\n\n{error}");
    if let Some(path) = log_path {
        use std::fmt::Write as _;
        let _ = write!(description, "\n\nDiagnostic log: {}", path.display());
    }
    let show = move || {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("bdinfo-rs-gui")
            .set_description(description)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    };
    #[cfg(target_os = "linux")]
    match <iced::executor::Default as iced::Executor>::new() {
        // The trait method by path: with the `tokio` feature the executor IS
        // `tokio::runtime::Runtime`, whose inherent zero-arg `enter` shadows it.
        Ok(executor) => iced::Executor::enter(&executor, show),
        // No runtime → no portal call is possible; the log above already
        // carries the boot error.
        Err(spawn_error) => log::error!("boot-failure dialog unavailable: {spawn_error}"),
    }
    #[cfg(not(target_os = "linux"))]
    show();
}

/// Whether a human is plausibly at this desktop — the boot-failure dialog is
/// shown only then. The one env-bound read in the attended story; the
/// decision itself is pure ([`args::attended`] / [`args::attended_unix`],
/// where the signals are documented).
///
/// Environment evidence is the whole mechanism, per platform the narrowest
/// reliable signal available to safe code: on Windows the direct probe (an
/// interactive window station, `GetUserObjectInformationW`) is an `unsafe`
/// extern call, unavailable under `forbid(unsafe_code)` — and the truly
/// window-station-less case needs no gate, since `MessageBoxW` fails
/// immediately there; macOS has no safe process-level probe either. Unix
/// adds the display-server variables: without one no dialog can render at
/// all.
fn attended_session() -> bool {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        args::attended(
            std::env::var_os("CI").as_deref(),
            std::env::var_os("BDINFO_GUI_NONINTERACTIVE").as_deref(),
        )
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        args::attended_unix(
            std::env::var_os("CI").as_deref(),
            std::env::var_os("BDINFO_GUI_NONINTERACTIVE").as_deref(),
            std::env::var_os("DISPLAY").as_deref(),
            std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        )
    }
}

/// Boots the app and fires a one-shot request for the OS theme, so the first
/// frame matches the desktop's light/dark setting. The persisted state seeds
/// the launch: the stored theme preference applies, and the last opened source
/// — when it still exists — shows in the Source field with "Rescan" live
/// (`BDInfo` seeds its source box from `LastPath` the same way), but nothing
/// scans until asked; a vanished path starts clean. A command-line path
/// (`bdinfo-rs-gui <path>`, `BDInfo`'s `args[0]`) opens at boot through the same
/// classify-and-list road a drop takes — `open` arrives from
/// [`args::classify`] over the OS-native argv, so a non-UTF-8 path survives
/// intact. In a **debug** build it also applies the screenshot-harness
/// environment hooks (see the debug `boot_with`).
fn boot(
    config: Option<PathBuf>,
    persisted: settings::Settings,
    open: Option<PathBuf>,
) -> (App, Task<Message>) {
    let mut app = App {
        config,
        theme_pref: ThemePref::from_choice(persisted.theme),
        persisted,
        ..App::default()
    };
    if let Some(input) = app
        .persisted
        .last_path
        .clone()
        .and_then(|path| Input::classify(path).log_err("last-path recall").ok())
    {
        app.flow = Flow::recall(input);
    }
    let mut tasks = vec![iced::system::theme().map(Message::OsTheme)];
    if let Some(path) = open {
        tasks.push(Task::done(Message::OpenPath(path)));
    }
    boot_with(app, Task::batch(tasks))
}

/// Release: boot with just the base tasks (theme probe + any argv open) — no
/// environment hooks.
#[cfg(not(debug_assertions))]
fn boot_with(app: App, base: Task<Message>) -> (App, Task<Message>) {
    (app, base)
}

/// Debug: layer the screenshot-harness hooks over the boot. `BDINFO_GUI_THEME`
/// (`light`/`dark`) pins the palette, `BDINFO_GUI_OPEN` / `BDINFO_GUI_ISO`
/// auto-open a disc, and `BDINFO_GUI_SETTINGS` (any value) opens the Settings
/// dialog, so `scripts/gui-shoot.ps1` can drive the real window to a known
/// state and capture it. `BDINFO_GUI_DRIVE=<dir>` runs the scripted
/// click-everything walk for the CI driving harness ([`DRIVE_STEPS`]).
/// Compiled out of every release build.
#[cfg(debug_assertions)]
fn boot_with(mut app: App, base: Task<Message>) -> (App, Task<Message>) {
    // One log line naming the hooks this boot actually sees — the CI
    // harnesses read it back from the diagnostics log when a hook seems not
    // to have fired (an env var lost between runner shells is otherwise
    // indistinguishable from a broken hook).
    log::info!(
        "debug hooks: {}",
        [
            "BDINFO_GUI_OPEN",
            "BDINFO_GUI_ISO",
            "BDINFO_GUI_THEME",
            "BDINFO_GUI_SCAN",
            "BDINFO_GUI_SCAN_DELAY",
            "BDINFO_GUI_SETTINGS",
            "BDINFO_GUI_SMOKE_MS",
            "BDINFO_GUI_DRIVE",
            "BDINFO_GUI_DRIVE_MS",
            "BDINFO_GUI_WIN"
        ]
        .iter()
        .filter(|name| std::env::var_os(name).is_some())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
    );
    if let Ok(pref) = std::env::var("BDINFO_GUI_THEME") {
        match pref.as_str() {
            "light" => app.theme_pref = ThemePref::Light,
            "dark" => app.theme_pref = ThemePref::Dark,
            _ => {}
        }
    }
    // The auto-open (`BDINFO_GUI_OPEN`/`_ISO`) is NOT a boot task: messages
    // completing during compositor bring-up get dropped by the shell, so it
    // rides an Idle-gated subscription instead (see [`App::subscription`]).
    let settings_dialog =
        std::env::var("BDINFO_GUI_SETTINGS").ok().map(|_| Task::done(Message::OpenSettings));
    let mut tasks = vec![base];
    tasks.extend(settings_dialog);
    // BDINFO_GUI_SMOKE_MS=<ms>: exit cleanly after the delay — the headless
    // launch smoke's seam ("the app opens, renders, exits 0"; see gui.yml).
    // The sleep blocks one executor worker, which is fine for a debug hook.
    if let Some(ms) = std::env::var("BDINFO_GUI_SMOKE_MS").ok().and_then(|v| v.parse::<u64>().ok())
    {
        tasks.push(Task::perform(
            async move { std::thread::sleep(Duration::from_millis(ms)) },
            |()| Message::SmokeExit,
        ));
    }
    // BDINFO_GUI_DRIVE=<dir>: run the scripted walk, markers into <dir> (see
    // [`DRIVE_STEPS`]). The directory is created here so the harness can start
    // watching it before the first marker lands.
    if let Some(dir) = drive_dir() {
        let _ = std::fs::create_dir_all(&dir).log_err("drive dir");
        tasks.push(drive_after(DRIVE_SETTLE_MS, Message::DriveNext { step: 0, polls: 0 }));
    }
    (app, Task::batch(tasks))
}

/// Debug only: whether the harness asked for an automatic measured scan of every
/// playlist once the structural list lands (`BDINFO_GUI_SCAN=all`), so a capture
/// can show the panes with measured sizes and bitrates.
#[cfg(debug_assertions)]
fn debug_scan_all() -> bool {
    std::env::var("BDINFO_GUI_SCAN").is_ok_and(|value| value == "all")
}

/// Debug only: an artificial pause (milliseconds from `BDINFO_GUI_SCAN_DELAY`)
/// inserted into the initial scan AND the measured-scan worker, so the
/// transient "scanning" states stay up long enough for a capture harness to
/// grab them — and long enough for the drive walk's Cancel step to land inside
/// a measured scan of the small CI fixture. No effect without the var.
#[cfg(debug_assertions)]
fn debug_scan_delay() {
    if let Some(ms) = std::env::var("BDINFO_GUI_SCAN_DELAY").ok().and_then(|v| v.parse().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Debug only: `BDINFO_GUI_PANIC` (any value) panics inside the measured-scan
/// worker — the honest trigger for watching the diagnostics panic hook and the
/// worker's [`scan::PanicAlarm`] drop-guard cooperate on a real unwind. No
/// effect without the var; compiled out of every release build.
#[cfg(debug_assertions)]
fn debug_worker_panic() {
    // `assert!` rather than a `panic!` in an `if` — the same real unwind,
    // with the crate-wide clippy::panic deny left intact.
    assert!(
        std::env::var_os("BDINFO_GUI_PANIC").is_none(),
        "BDINFO_GUI_PANIC: forced worker panic"
    );
}

/// Release: the persisted geometry boots untouched — no environment hooks.
#[cfg(not(debug_assertions))]
const fn debug_window_override(persisted: settings::Settings) -> settings::Settings {
    persisted
}

/// Debug only: `BDINFO_GUI_WIN=<W>x<H>[+<X>+<Y>]` (logical pixels) pins the
/// boot geometry through the normal launch path — the same seam the persisted
/// settings ride, so winit itself creates the window at that size. The CI
/// driving harnesses need it: hosted runner displays are small (1024x768 with
/// a taskbar), the default 960-tall window hangs off-screen there, and
/// resizing from OUTSIDE the process proved unreliable on those desktops.
/// Malformed specs are ignored; [`settings::launch_size`] still clamps to the
/// window minimum. Compiled out of every release build.
#[cfg(debug_assertions)]
fn debug_window_override(mut persisted: settings::Settings) -> settings::Settings {
    if let Ok(spec) = std::env::var("BDINFO_GUI_WIN") {
        let (size, pos) = spec
            .split_once('+')
            .map_or((spec.as_str(), None), |(size, rest)| (size, rest.split_once('+')));
        if let Some((w, h)) = size.split_once('x')
            && let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse())
        {
            persisted.window_size = Some((w, h));
            persisted.window_maximized = false;
        }
        if let Some((x, y)) = pos
            && let (Ok(x), Ok(y)) = (x.trim().parse(), y.trim().parse())
        {
            persisted.window_pos = Some((x, y));
        }
    }
    persisted
}

/// Debug only: the marker directory of the scripted walk (`BDINFO_GUI_DRIVE`),
/// `None` when the walk is off. See [`DRIVE_STEPS`].
#[cfg(debug_assertions)]
fn drive_dir() -> Option<PathBuf> {
    std::env::var_os("BDINFO_GUI_DRIVE").map(PathBuf::from)
}

/// Debug only: the per-step capture window of the scripted walk, milliseconds
/// (`BDINFO_GUI_DRIVE_MS`, default 1500) — how long a rendered state is
/// guaranteed to stay on screen after its marker appears.
#[cfg(debug_assertions)]
fn drive_window_ms() -> u64 {
    std::env::var("BDINFO_GUI_DRIVE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500)
}

#[cfg(debug_assertions)]
const DRIVE_SETTLE_MS: u64 = 500;
#[cfg(debug_assertions)]
const DRIVE_POLL_MS: u64 = 200;
/// Poll attempts before the walk gives up on a step (450 × 200 ms = 90 s).
#[cfg(debug_assertions)]
const DRIVE_MAX_POLLS: u32 = 450;

/// One step of the scripted walk: poll `ready` until the app state can take
/// the step, apply `act`, and (a settle later) write the `NN-name.marker`
/// file the capture harness screenshots on.
#[cfg(debug_assertions)]
struct DriveStep {
    /// The marker / screenshot name.
    name: &'static str,
    /// Whether the app state is ready for this step (polled until true).
    ready: fn(&App) -> bool,
    /// The step itself — injects the same messages the widgets emit.
    act: fn(&mut App) -> Task<Message>,
}

/// Debug only: the fixed click-everything walk behind `BDINFO_GUI_DRIVE` —
/// open (via `BDINFO_GUI_OPEN`) → sort → splitter → rows → Settings
/// (toggle / Cancel / OK) → scan → cancel → rescan → report → copy → save.
/// The steps inject the exact messages the widgets emit, so the OS input
/// stack is deliberately NOT under test (that is the injection harness's
/// job); the one dialog road (`SaveReport`) is entered through its result
/// message (`SaveDest`, aimed at the marker directory) so no native chooser
/// can block a headless runner. Pair with `BDINFO_GUI_SCAN_DELAY` so the
/// cancel step lands inside the measured scan of the small CI fixture.
#[cfg(debug_assertions)]
static DRIVE_STEPS: &[DriveStep] = &[
    // The auto-open (BDINFO_GUI_OPEN) has listed the disc — the baseline shot.
    DriveStep { name: "listed", ready: drive_listed, act: |_| Task::none() },
    DriveStep {
        name: "sort-length",
        ready: drive_listed,
        act: |app| app.update(Message::SortBy(SortColumn::Length)),
    },
    DriveStep {
        name: "sort-length-flip",
        ready: drive_listed,
        act: |app| app.update(Message::SortBy(SortColumn::Length)),
    },
    DriveStep {
        name: "sort-file",
        ready: drive_listed,
        act: |app| app.update(Message::SortBy(SortColumn::File)),
    },
    // Drag the outer splitter: the same resize message the pane grid emits.
    DriveStep {
        name: "splitter",
        ready: drive_listed,
        act: |app| {
            drive_first_split(app.panes.layout()).map_or_else(Task::none, |split| {
                app.update(Message::PaneResized(pane_grid::ResizeEvent { split, ratio: 0.72 }))
            })
        },
    },
    DriveStep {
        name: "row-activate",
        ready: drive_listed,
        act: |app| {
            let row = app.flow.row_count().saturating_sub(1).min(1);
            app.update(Message::RowActivated(row))
        },
    },
    DriveStep {
        name: "row-toggle",
        ready: drive_listed,
        act: |app| app.update(Message::RowToggled(0)),
    },
    DriveStep {
        name: "select-none",
        ready: drive_listed,
        act: |app| app.update(Message::SelectNone),
    },
    DriveStep {
        name: "select-all",
        ready: drive_listed,
        act: |app| app.update(Message::SelectAll),
    },
    DriveStep {
        name: "settings-open",
        ready: drive_idle,
        act: |app| app.update(Message::OpenSettings),
    },
    // A checkbox plus a theme chip — the live preview makes the shot obvious.
    DriveStep {
        name: "settings-toggle",
        ready: drive_settings_open,
        act: |app| {
            let toggled = app.update(Message::SettingsToggled(SettingsField::ChapterCount));
            let themed = app.update(Message::SettingsTheme(settings::ThemeChoice::Light));
            Task::batch([toggled, themed])
        },
    },
    // Cancel drops the draft — the theme preview visibly reverts.
    DriveStep {
        name: "settings-cancel",
        ready: drive_settings_open,
        act: |app| app.update(Message::SettingsCancel),
    },
    DriveStep {
        name: "settings-reopen",
        ready: drive_idle,
        act: |app| {
            let opened = app.update(Message::OpenSettings);
            let toggled = app.update(Message::SettingsToggled(SettingsField::HumanSizes));
            Task::batch([opened, toggled])
        },
    },
    // OK applies the draft — the size columns re-render human-readable.
    DriveStep {
        name: "settings-ok",
        ready: drive_settings_open,
        act: |app| app.update(Message::SettingsOk),
    },
    DriveStep { name: "scan", ready: drive_listed, act: |app| app.update(Message::ScanSelected) },
    // Cancel right away, while BDINFO_GUI_SCAN_DELAY still holds the worker —
    // the fixture disc is small, so a later cancel would miss the scan.
    DriveStep { name: "cancel", ready: drive_scanning, act: |app| app.update(Message::Cancel) },
    DriveStep { name: "cancelled", ready: drive_idle, act: |_| Task::none() },
    DriveStep { name: "rescan", ready: drive_listed, act: |app| app.update(Message::ScanSelected) },
    // No-op: captures the in-flight progress UI (the delay holds it up).
    DriveStep { name: "scanning", ready: drive_scanning, act: |_| Task::none() },
    // No-op: captures the measured table + the scan-complete modal.
    DriveStep { name: "scanned", ready: drive_reported, act: |_| Task::none() },
    DriveStep {
        name: "dismiss",
        ready: |app| app.notice.is_some(),
        act: |app| app.update(Message::DismissNotice),
    },
    DriveStep { name: "report", ready: drive_reported, act: |app| app.update(Message::ShowReport) },
    DriveStep { name: "copy", ready: drive_reported, act: |app| app.update(Message::CopyReport) },
    // The dialog's result message, aimed at the marker directory: the saved
    // BDINFO.<label>.txt landing there is the walk's hard on-disk evidence.
    DriveStep {
        name: "save",
        ready: drive_reported,
        act: |app| {
            drive_dir().map_or_else(Task::none, |dir| app.update(Message::SaveDest(Some(dir))))
        },
    },
    // The terminal sentinel: its marker means every step above completed.
    DriveStep { name: "done", ready: |_| true, act: |_| Task::none() },
];

/// The playlist table is up with at least one row.
#[cfg(debug_assertions)]
fn drive_listed(app: &App) -> bool {
    app.flow.stage() == Stage::Listed && app.flow.row_count() > 0
}

/// No scan is in flight (the state the Settings / save roads require).
#[cfg(debug_assertions)]
const fn drive_idle(app: &App) -> bool {
    !app.is_busy()
}

/// The Settings dialog is open (a draft exists).
#[cfg(debug_assertions)]
const fn drive_settings_open(app: &App) -> bool {
    app.settings_draft.is_some()
}

/// The measured scan is in flight.
#[cfg(debug_assertions)]
fn drive_scanning(app: &App) -> bool {
    app.flow.stage() == Stage::Scanning
}

/// A measured scan has finished — the report is the measured render.
#[cfg(debug_assertions)]
fn drive_reported(app: &App) -> bool {
    app.flow.stage() == Stage::Reported
}

/// The first splitter in the pane layout — the outer horizontal one, the walk's
/// drag target.
#[cfg(debug_assertions)]
const fn drive_first_split(node: &pane_grid::Node) -> Option<pane_grid::Split> {
    match node {
        pane_grid::Node::Split { id, .. } => Some(*id),
        pane_grid::Node::Pane(_) => None,
    }
}

/// Delivers `message` after `ms` — the walk's scheduler. The sleep blocks one
/// executor worker, the established debug-hook trade-off (`BDINFO_GUI_SMOKE_MS`
/// sleeps the same way).
#[cfg(debug_assertions)]
fn drive_after(ms: u64, message: Message) -> Task<Message> {
    Task::perform(async move { std::thread::sleep(Duration::from_millis(ms)) }, move |()| message)
}

/// Writes a walk marker file (`NN-name.marker`, the flow stage as content)
/// into the marker directory — the capture harness's screenshot trigger. The
/// index prefix keeps the gallery in walk order. Best-effort like every debug
/// hook: a failed write only logs.
#[cfg(debug_assertions)]
fn drive_marker(index: usize, name: &str, stage: Stage) {
    if let Some(dir) = drive_dir() {
        let _ = std::fs::write(dir.join(format!("{index:02}-{name}.marker")), format!("{stage:?}"))
            .log_err("drive marker");
    }
}

/// The config file location, resolved from the process environment: the
/// portable path beside the executable when its marker file is there,
/// otherwise the per-platform user directory. `None` when no base directory
/// can be resolved (the app then runs without persistence). The one env-bound
/// read in the config story; the path math it dispatches to is pure
/// ([`settings`]).
fn config_path() -> Option<PathBuf> {
    // Portable mode wins where it applies, so a copy carried between machines
    // keeps its settings instead of writing into each host's user profile.
    // `current_exe()` can fail; a launch that cannot locate itself is simply
    // not portable and still boots.
    if let Some(portable) = settings::portable_config_path(std::env::current_exe().ok().as_deref())
    {
        return Some(portable);
    }
    #[cfg(target_os = "windows")]
    {
        settings::windows_config_path(std::env::var_os("APPDATA").as_deref())
    }
    #[cfg(target_os = "macos")]
    {
        settings::macos_config_path(std::env::var_os("HOME").as_deref())
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        settings::unix_config_path(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }
}

/// The window settings: last session's geometry (the stored size clamped to
/// the minimum; the default size and OS placement when nothing sane is
/// stored), the maximized state layered on top (the geometry stays the
/// restore bounds an unmaximize returns to), the minimum size, and the
/// embedded app icon (drawn procedurally, so the binary stays
/// self-contained). Closing is handled manually
/// (`exit_on_close_request: false`) so the geometry can be captured and saved
/// on the way out — see [`Message::CloseRequested`].
fn window_settings(persisted: &settings::Settings) -> iced::window::Settings {
    let (width, height) = settings::launch_size(persisted.window_size, WINDOW_SIZE, MIN_SIZE);
    // The store validated the position's range; Wayland (no global positions)
    // simply never stores one, so the platform default placement applies.
    let position = persisted.window_pos.map_or(iced::window::Position::Default, |(x, y)| {
        iced::window::Position::Specific(iced::Point::new(x, y))
    });
    iced::window::Settings {
        size: iced::Size::new(width, height),
        position,
        maximized: persisted.window_maximized,
        min_size: Some(iced::Size::new(MIN_SIZE.0, MIN_SIZE.1)),
        icon: iced::window::icon::from_rgba(icon::rgba(ICON_SIZE), ICON_SIZE, ICON_SIZE)
            .log_err("window icon")
            .ok(),
        exit_on_close_request: false,
        // Wayland ignores the RGBA icon above by protocol and keys off this
        // id instead — see `APP_ID`. The field only exists on Linux; every
        // other platform (the BSDs included) keeps its own platform defaults.
        #[cfg(target_os = "linux")]
        platform_specific: iced::window::settings::PlatformSpecific {
            application_id: APP_ID.to_owned(),
            ..iced::window::settings::PlatformSpecific::default()
        },
        ..iced::window::Settings::default()
    }
}

/// The app state — the pure [`Flow`] the shell drives, plus the runtime-side
/// bits: the visual preference + last-known OS mode, the scan generation counter
/// (stamped on the worker's messages so a stale event is ignored), the scan start
/// time (for the elapsed / remaining estimate), and a transient status line.
struct App {
    /// The flow state machine — `view` is a projection of it.
    flow: Flow,
    /// Where the persisted state lives — resolved once at boot, `None` when
    /// the platform offers no config base (the app then runs without
    /// persistence). Tests build the `App` directly with `None`, so no test
    /// can ever touch the user's real config file.
    config: Option<PathBuf>,
    /// The persisted state (geometry / last path / theme), as it will next be
    /// written: updated alongside the live state and saved best-effort at the
    /// change points (theme toggle, successful listing) and on close.
    persisted: settings::Settings,
    /// Follow-the-OS / pinned light / pinned dark.
    theme_pref: ThemePref,
    /// The last light/dark setting reported by the OS.
    os_mode: iced::theme::Mode,
    /// Bumped at each listing and each "Scan selected"; the in-flight
    /// worker's id, stamped on its messages so a stale worker's are ignored.
    generation: u64,
    /// The in-flight worker's cooperative cancel flag — listing and measured
    /// scan alike, a fresh one per worker (so cancelling an old worker can
    /// never touch a newer one), shared with its thread, set by the Cancel
    /// button. The read loop polls it per chunk and aborts, so Cancel really
    /// frees the machine.
    scan_cancel: Option<Arc<AtomicBool>>,
    /// When the current worker (listing or measured scan) started.
    scan_start: Option<Instant>,
    /// When the in-flight worker's last progress event arrived (the start
    /// until the first one) — what the 1 s scanning tick measures the stall
    /// hint against ([`Flow::tick`]).
    last_progress: Option<Instant>,
    /// A one-line status (e.g. `Report saved to: …`), shown in the bottom bar.
    status: Option<String>,
    /// Whether the report view (over the panes) is showing.
    showing_report: bool,
    /// The "scan completed" confirmation, shown as a modal until dismissed.
    notice: Option<String>,
    /// The Settings dialog's working copy — `Some` while the dialog is open;
    /// OK applies it to `persisted`, Cancel just drops it.
    settings_draft: Option<settings::Draft>,
    /// The row the cursor is currently over — one `(region, index)` across all
    /// three panes, so the whole row (not a sub-widget) lifts on hover.
    hovered: Option<(Region, usize)>,
    /// The sortable playlist header cell the cursor is over — its hover wash
    /// is the sort-click affordance.
    hovered_header: Option<SortColumn>,
    /// The clicked (selected) row in a lower pane — the anchor a later
    /// right-click "copy line" acts on. `None` until a Stream File / Codec row is
    /// clicked; reset when the active playlist changes.
    pane_selection: Option<(Region, usize)>,
    /// The animation phase of the indeterminate "scanning" bar shown during
    /// the initial disc scan; wraps at [`progress::PULSE_PERIOD`] and is
    /// advanced by [`Message::Tick`].
    pulse: u16,
    /// The three master-detail panes and the two draggable splitters between
    /// them (Playlist / Stream File on top, Codec below). Holds the split ratios,
    /// which the user drags and which resize proportionally with the window.
    panes: pane_grid::State<Region>,
    /// The Playlist pane column weights (Name / Group / Length / Estimated /
    /// Measured) — the user drags header dividers to re-weight them.
    playlist_w: [f32; 5],
    /// The Stream File pane column weights (File / Index / Length / Estimated /
    /// Measured) — its own set, so it resizes independently of the playlist's,
    /// as in `BDInfo`.
    stream_w: [f32; 5],
    /// The Codec pane column weights (Codec / Language / Bit Rate / Description).
    codec_w: [f32; 4],
    /// The in-flight column drag: which grid + boundary, the columns' pixel span
    /// (for delta→weight), and the last cursor x (`None` until the first move) so
    /// successive moves apply a delta.
    col_drag: Option<(ColGrid, usize, f32, Option<f32>)>,
    /// Whether a drag-and-drop payload is held over the window — set by the
    /// first `FileHovered`, cleared by the drop or the hover leaving. Doubles
    /// as the drop-batch guard: only the drop that takes the flag opens (the
    /// FIRST item of a multi-file drop, `BDInfo`'s behaviour — winit delivers
    /// one `FileDropped` per item, always after their hover events).
    drop_hover: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            flow: Flow::default(),
            config: None,
            persisted: settings::Settings::default(),
            theme_pref: ThemePref::default(),
            os_mode: iced::theme::Mode::default(),
            generation: 0,
            scan_cancel: None,
            scan_start: None,
            last_progress: None,
            status: None,
            showing_report: false,
            notice: None,
            settings_draft: None,
            hovered: None,
            hovered_header: None,
            pane_selection: None,
            pulse: 0,
            panes: default_panes(),
            // BDInfo's playlist ratios (both upper grids open with the same
            // proportions); the Codec grid is its own 22/10/10/56.
            playlist_w: [30.0, 7.0, 19.0, 21.0, 21.0],
            stream_w: [30.0, 7.0, 19.0, 21.0, 21.0],
            codec_w: [22.0, 10.0, 10.0, 56.0],
            col_drag: None,
            drop_hover: false,
        }
    }
}

/// The initial pane layout — the same nesting + ratios `BDInfo` opens with:
/// Playlist and Stream File share the top region, Codec takes the bottom. The
/// outer split gives the top region ~0.56 (`BDInfo`'s 228/406); the inner split
/// gives the playlist ~0.48 (`BDInfo`'s 109/228).
fn default_panes() -> pane_grid::State<Region> {
    pane_grid::State::with_configuration(pane_grid::Configuration::Split {
        axis: pane_grid::Axis::Horizontal,
        ratio: 0.56,
        a: Box::new(pane_grid::Configuration::Split {
            axis: pane_grid::Axis::Horizontal,
            ratio: 0.48,
            a: Box::new(pane_grid::Configuration::Pane(Region::Playlist)),
            b: Box::new(pane_grid::Configuration::Pane(Region::StreamFile)),
        }),
        b: Box::new(pane_grid::Configuration::Pane(Region::Codec)),
    })
}

/// A column weight as an `iced` `FillPortion` — the pure scaling
/// ([`columns::portion`]) wrapped in the layout type.
fn fill(weight: f32) -> Length {
    Length::FillPortion(columns::portion(weight))
}

/// Which of the three master-detail panes a row belongs to — the hover / click
/// messages carry it so one `hovered` slot serves all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    /// The top playlist table.
    Playlist,
    /// The middle Stream File pane.
    StreamFile,
    /// The bottom Codec pane.
    Codec,
}

/// Which set of resizable column weights a drag targets — one per pane, so
/// each table's columns resize independently, as in `BDInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColGrid {
    /// The Playlist pane's column weights.
    Playlist,
    /// The Stream File pane's column weights.
    StreamFile,
    /// The Codec pane's column weights.
    Codec,
}

/// The width of the grab strip on a header column's right edge — the zone that
/// shows the resize cursor and drags the boundary (wide enough to hit easily).
const GRAB_W: f32 = 14.0;

/// A top-level command — the menu/toolbar seam a later native macOS menu maps
/// onto. `view` never hard-codes an action's label, message, or enablement: it
/// reads them from here (and [`App::command_enabled`]), so wiring a native menu
/// in a later phase is plumbing, not a rewrite. The intended native grouping is
/// **File**: Open folder / Open ISO / Rescan / Save report (and the window's own
/// Quit); the theme lives inside the Settings dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// Open a Blu-ray disc folder.
    OpenFolder,
    /// Open a `.iso` disc image.
    OpenIso,
    /// Re-list the current disc's playlists.
    Rescan,
    /// Choose a destination and save the report.
    SaveReport,
    /// Copy the report to the clipboard.
    CopyReport,
    /// Open the Settings dialog.
    Settings,
}

impl Command {
    const fn label(self) -> &'static str {
        match self {
            Self::OpenFolder => "Open folder…",
            Self::OpenIso => "Open ISO…",
            Self::Rescan => "Rescan",
            Self::SaveReport => "Save report…",
            Self::CopyReport => "Copy",
            Self::Settings => "Settings...",
        }
    }

    const fn message(self) -> Message {
        match self {
            Self::OpenFolder => Message::OpenFolder,
            Self::OpenIso => Message::OpenIso,
            Self::Rescan => Message::Rescan,
            Self::SaveReport => Message::SaveReport,
            Self::CopyReport => Message::CopyReport,
            Self::Settings => Message::OpenSettings,
        }
    }
}

/// One Settings checkbox — the field a [`Message::SettingsToggled`] flips on
/// the open dialog's draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    /// "Filter short playlists".
    FilterShort,
    /// "Filter playlists that contain loops".
    FilterLoops,
    /// "Stream sizes in human readable format".
    HumanSizes,
    /// "Display chapter count in Playlist view".
    ChapterCount,
    /// "Auto-save report on scan completion".
    Autosave,
    /// "Include stream diagnostics in report".
    StreamDiagnostics,
    /// "Include quick text summary in report".
    QuickSummary,
    /// "Keep partial data from failed stream reads".
    KeepPartial,
}

/// Everything the UI can ask the runtime to do.
#[derive(Debug, Clone)]
enum Message {
    /// "Open folder…" pressed.
    OpenFolder,
    /// "Open ISO…" pressed.
    OpenIso,
    /// "Rescan" pressed — re-list the current disc.
    Rescan,
    /// The folder dialog closed: `Some(path)` picked, `None` cancelled.
    FolderPicked(Option<PathBuf>),
    /// The `.iso` file dialog closed.
    IsoPicked(Option<PathBuf>),
    /// A path arrived from outside the pickers (the boot argument) — classify
    /// it and open like a pick, or land in the friendly failure state.
    OpenPath(PathBuf),
    /// A drag-and-drop payload entered the window (one event per held file).
    FileHovered,
    /// A hovered file was dropped onto the window — open it like a pick.
    FileDropped(PathBuf),
    /// The hovered file(s) left the window without dropping.
    FilesHoveredLeft,
    /// A streamed progress event from the listing worker's quick codec pass.
    ListProgress { generation: u64, file: String, done: u64, total: u64 },
    /// The structural scan for `input` finished (or failed with a message).
    /// `generation` is the stamp [`App::begin_listing`] issued the worker —
    /// the flow's input match alone cannot drop a cancelled worker's late
    /// result once the same input is picked again.
    Listed { generation: u64, input: Input, result: Result<Structural, String> },
    /// A table row's scan checkbox toggled (0-based table position).
    RowToggled(usize),
    /// A table row clicked — make it the active (highlighted) playlist that the
    /// master-detail panes follow.
    RowActivated(usize),
    /// The cursor entered a row in `region` (0-based index within that pane).
    RowHovered(Region, usize),
    /// The cursor left a row in `region` — clears the hover if it still owns it.
    RowUnhovered(Region, usize),
    /// A lower-pane (Stream File / Codec) row was clicked — select it (the
    /// anchor the Ctrl+C copy acts on).
    PaneRowPressed(Region, usize),
    /// A playlist-table column header was clicked — sort by that column
    /// (ascending first — descending when the rows already read ascending —
    /// and a repeat click flips).
    SortBy(SortColumn),
    /// The cursor entered a sortable playlist header cell.
    HeaderHovered(SortColumn),
    /// The cursor left a sortable playlist header cell — clears the hover if
    /// that column still owns it.
    HeaderUnhovered(SortColumn),
    /// Ctrl+C (Cmd+C on macOS) — copy the highlighted row's file path.
    CopyPath,
    /// The hidden-count line's "Show" / "Hide" pressed — flip the transient
    /// reveal of the playlists the settings' filters withhold.
    ToggleReveal,
    /// "Select all" pressed.
    SelectAll,
    /// "Select none" pressed.
    SelectNone,
    /// "Scan selected" pressed — start the measured scan.
    ScanSelected,
    /// A streamed scan-progress event from the worker thread.
    Progress { generation: u64, file: String, done: u64, total: u64 },
    /// The worker's measured scan finished. The errors are typed (shared —
    /// `ScanError` is not `Clone`) so the flow can re-render the report when
    /// a report-section toggle changes.
    Finished {
        generation: u64,
        report: String,
        errors: Arc<Vec<ScanError>>,
        playlists: Vec<PlaylistSummary>,
    },
    /// The worker's measured scan failed fatally.
    ScanFailed { generation: u64, error: String },
    /// "Cancel" pressed during a scan.
    Cancel,
    /// "View Report" pressed — show the report over the panes.
    ShowReport,
    /// "Back" pressed in the report view — return to the panes.
    HideReport,
    /// "Save report…" pressed — choose a destination folder.
    SaveReport,
    /// The save-destination dialog closed.
    SaveDest(Option<PathBuf>),
    /// "Copy" pressed — copy the report to the clipboard.
    CopyReport,
    /// The "scan completed" confirmation modal was dismissed.
    DismissNotice,
    /// "Settings..." pressed — open the dialog over a working copy.
    OpenSettings,
    /// A Settings checkbox toggled on the open dialog.
    SettingsToggled(SettingsField),
    /// The short-playlist seconds field was edited (sanitized to digits).
    SettingsSeconds(String),
    /// A theme chip picked on the open dialog — previewed live via the draft.
    SettingsTheme(settings::ThemeChoice),
    /// A UI-scale chip picked on the open dialog — previewed live via the
    /// draft, like the theme.
    SettingsScale(u16),
    /// Ctrl+'+' / Ctrl+'-' (`true` = up) — step the UI scale through the
    /// presets: the open dialog's draft when there is one, else the applied
    /// settings directly.
    ScaleStepped(bool),
    /// The Settings dialog's OK — apply + persist the draft.
    SettingsOk,
    /// The Settings dialog's Cancel (or a scrim click) — discard the draft.
    SettingsCancel,
    /// The OS reported (or changed) its light/dark setting.
    OsTheme(iced::theme::Mode),
    /// A display tick with no payload: per-frame while listing (advances the
    /// indeterminate bar), ~1 s while scanning (re-reads the wall clock for
    /// the elapsed readout and the stall hint).
    Tick,
    /// A pane splitter was dragged — resize the two panes it divides.
    PaneResized(pane_grid::ResizeEvent),
    /// A column-divider grab was pressed — begin resizing that boundary. Carries
    /// the pixel span the columns share (from the responsive header) for the
    /// delta→weight conversion.
    ColDragStart(ColGrid, usize, f32),
    /// The cursor moved while a column divider is held (window x). Delivered by a
    /// global mouse subscription, so it keeps tracking even off the grab zone.
    ColDragMove(f32),
    /// A column drag ended (the mouse button was released).
    ColDragEnd,
    /// The user asked to close the window (`exit_on_close_request` is off) —
    /// read the live geometry, then save and close.
    CloseRequested(iced::window::Id),
    /// The debug smoke hook (`BDINFO_GUI_SMOKE_MS`) fired — end the event
    /// loop cleanly so the launch smoke's process exits 0.
    #[cfg(debug_assertions)]
    SmokeExit,
    /// The debug auto-open tick (`BDINFO_GUI_OPEN`/`_ISO`): an Idle-gated
    /// subscription re-fires it until the open takes, because a message
    /// delivered during compositor bring-up can be dropped.
    #[cfg(debug_assertions)]
    DebugAutoOpen,
    /// The debug drive walk (`BDINFO_GUI_DRIVE`) wants step `step` applied —
    /// polled (`polls` so far) until the step's precondition holds.
    #[cfg(debug_assertions)]
    DriveNext {
        /// The [`DRIVE_STEPS`] index to apply.
        step: usize,
        /// Precondition polls spent on it so far (gives up at the cap).
        polls: u32,
    },
    /// Step `step` of the drive walk was applied and has had a settle to
    /// render — write its marker, then schedule the next step.
    #[cfg(debug_assertions)]
    DriveMark(usize),
    /// The geometry readout for the closing window arrived — persist
    /// everything and actually close (`BDInfo` saves at `FormClosing` too).
    SaveAndClose {
        /// The closing window.
        id: iced::window::Id,
        /// Its outer position — `None` on Wayland (no global positions).
        position: Option<iced::Point>,
        /// Its logical size.
        size: iced::Size,
        /// Whether it is maximized (the position/size are then the maximized
        /// rect, not the restore bounds — see [`App::save_and_close`]).
        maximized: bool,
    },
}

impl App {
    /// The current palette, resolved from the preference and the OS mode.
    /// While the Settings dialog is open its draft's theme pick wins — the
    /// live preview; Cancel drops the draft and the preview reverts with it.
    fn palette(&self) -> Palette {
        self.settings_draft
            .as_ref()
            .map_or(self.theme_pref, |draft| ThemePref::from_choice(draft.theme))
            .resolve(self.os_mode)
    }

    /// The applied UI scale as the multiplier iced expects (1.0 = native),
    /// from the persisted percent. While the Settings dialog is open its
    /// draft's pick wins — the live preview; Cancel drops the draft and the
    /// scale reverts with it.
    fn ui_scale(&self) -> f32 {
        let percent = self
            .settings_draft
            .as_ref()
            .map_or(self.persisted.ui_scale_percent, |draft| draft.ui_scale_percent);
        f32::from(percent) / 100.0
    }

    fn title(&self) -> String {
        self.flow
            .label()
            .map_or_else(|| "bdinfo-rs".to_owned(), |label| format!("bdinfo-rs — {label}"))
    }

    /// The `iced::Theme` for unstyled widgets (text, caret, scrollbars).
    fn theme(&self) -> iced::Theme {
        self.palette().iced_theme()
    }

    /// The app-level base style: the window background + default text colour.
    fn style(&self, _theme: &iced::Theme) -> iced::theme::Style {
        self.palette().base_style()
    }

    /// Whether a [`Command`] is currently actionable (drives its enabled state).
    fn command_enabled(&self, command: Command) -> bool {
        match command {
            // Browsing / re-scanning / reconfiguring is disabled while a scan
            // is in flight (`BDInfo` disables its Settings button then too).
            Command::OpenFolder | Command::OpenIso | Command::Settings => !self.is_busy(),
            Command::Rescan => !self.is_busy() && self.flow.current_input().is_some(),
            Command::SaveReport | Command::CopyReport => self.flow.report_available(),
        }
    }

    /// The pure-ish message dispatch: each arm applies a library transition and
    /// returns the side effect to run (a dialog, the scan worker, a clipboard
    /// write, an exit, or nothing).
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenFolder => Task::perform(pick_folder(), Message::FolderPicked),
            Message::OpenIso => Task::perform(pick_iso(), Message::IsoPicked),
            Message::Rescan => self
                .flow
                .current_input()
                .cloned()
                .map_or_else(Task::none, |input| self.begin_listing(input)),
            // A cancelled folder / `.iso` / save dialog all do nothing.
            Message::FolderPicked(None) | Message::IsoPicked(None) | Message::SaveDest(None) => {
                Task::none()
            }
            Message::FolderPicked(Some(path)) => self.picked(Input::Folder(path)),
            Message::IsoPicked(Some(path)) => self.picked(Input::Iso(path)),
            Message::OpenPath(path) => self.open_path(path),
            Message::FileHovered => {
                self.drop_hover = true;
                Task::none()
            }
            Message::FilesHoveredLeft => {
                self.drop_hover = false;
                Task::none()
            }
            Message::FileDropped(path) => {
                // Only the drop that takes the hover flag opens — the first item
                // of a multi-file drop; its companions arrive with the flag gone.
                if std::mem::take(&mut self.drop_hover) {
                    self.open_path(path)
                } else {
                    Task::none()
                }
            }
            Message::ListProgress { generation, file, done, total } => {
                self.on_list_progress(generation, file, done, total)
            }
            Message::Listed { generation, input, result } => {
                self.on_listed(generation, &input, result)
            }
            Message::RowToggled(index) => {
                self.flow.toggle(index);
                Task::none()
            }
            Message::RowActivated(index) => self.activate_row(index),
            Message::RowHovered(region, index) => self.on_row_hovered(region, index),
            Message::RowUnhovered(region, index) => self.on_row_unhovered(region, index),
            Message::PaneRowPressed(region, index) => {
                self.pane_selection = Some((region, index));
                Task::none()
            }
            Message::SortBy(column) => {
                self.flow.sort_by(column);
                Task::none()
            }
            Message::HeaderHovered(column) => self.on_header_hovered(column),
            Message::HeaderUnhovered(column) => self.on_header_unhovered(column),
            Message::CopyPath => self.copy_path(),
            message @ (Message::SelectAll | Message::SelectNone | Message::ToggleReveal) => {
                self.on_table_command(&message);
                Task::none()
            }
            Message::ScanSelected => self.start_scan(),
            Message::Progress { generation, file, done, total } => {
                let elapsed = self.scan_start.map_or(Duration::ZERO, |start| start.elapsed());
                self.last_progress = Some(Instant::now());
                self.flow.progress(generation, file, done, total, elapsed);
                Task::none()
            }
            Message::Finished { generation, report, errors, playlists } => {
                self.on_finished(generation, report, errors, playlists)
            }
            Message::ScanFailed { generation, error } => {
                // Logged whether or not the flow applies it: a late failure
                // from a cancelled worker ("scan cancelled") is the log's
                // only record that the blocked thread finally returned.
                log::info!("scan failed: {error}");
                self.flow = std::mem::take(&mut self.flow).scan_failed(generation, error);
                Task::none()
            }
            Message::Cancel => self.cancel_scan(),
            Message::ShowReport => {
                self.showing_report = self.flow.report_available();
                Task::none()
            }
            Message::HideReport => {
                self.showing_report = false;
                Task::none()
            }
            Message::SaveReport => Task::perform(pick_folder(), Message::SaveDest),
            Message::SaveDest(Some(dir)) => self.save_dest(&dir),
            Message::CopyReport => self.copy_report(),
            Message::DismissNotice => {
                self.notice = None;
                Task::none()
            }
            message @ (Message::OpenSettings
            | Message::SettingsToggled(_)
            | Message::SettingsSeconds(_)
            | Message::SettingsTheme(_)
            | Message::SettingsScale(_)
            | Message::ScaleStepped(_)
            | Message::SettingsOk
            | Message::SettingsCancel) => {
                self.on_settings(message);
                Task::none()
            }
            Message::OsTheme(mode) => {
                self.os_mode = mode;
                Task::none()
            }
            Message::Tick => {
                self.on_tick();
                Task::none()
            }
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => {
                self.panes.resize(split, ratio);
                Task::none()
            }
            message @ (Message::ColDragStart(..)
            | Message::ColDragMove(_)
            | Message::ColDragEnd) => {
                self.on_col_drag(&message);
                Task::none()
            }
            #[cfg(debug_assertions)]
            Message::SmokeExit => iced::exit(),
            #[cfg(debug_assertions)]
            Message::DebugAutoOpen => self.debug_auto_open(),
            #[cfg(debug_assertions)]
            Message::DriveNext { step, polls } => self.drive_next(step, polls),
            #[cfg(debug_assertions)]
            Message::DriveMark(step) => self.drive_mark(step),
            Message::CloseRequested(id) => close_with_geometry(id),
            Message::SaveAndClose { id, position, size, maximized } => {
                self.save_and_close(id, position, size, maximized)
            }
        }
    }

    /// Handles the table-wide commands, each a pure flow transition with no
    /// side effect to run: check every row, uncheck every row, and the
    /// hidden-count line's Show / Hide reveal.
    fn on_table_command(&mut self, message: &Message) {
        match *message {
            Message::SelectAll => self.flow.select_all(),
            Message::SelectNone => self.flow.select_none(),
            Message::ToggleReveal => self.flow.toggle_reveal(),
            // Not a table command — update() never routes one here.
            _ => {}
        }
    }

    /// Handles the column-divider drag messages: a grab press starts the drag,
    /// the global mouse feed moves the held boundary, release ends it.
    fn on_col_drag(&mut self, message: &Message) {
        match *message {
            Message::ColDragStart(grid, boundary, cols_width) => {
                self.col_drag = Some((grid, boundary, cols_width, None));
            }
            Message::ColDragMove(x) => self.column_drag(x),
            Message::ColDragEnd => self.col_drag = None,
            // Not a drag message — update() never routes one here.
            _ => {}
        }
    }

    /// Handles the settings messages: open a fresh dialog draft, edit it
    /// (checkboxes / the digits-only seconds field / the theme and UI-scale
    /// chips), OK (apply + persist), Cancel (discard), or the UI-scale
    /// keyboard step (which also works with no dialog open). A stray edit
    /// with no open dialog changes nothing.
    fn on_settings(&mut self, message: Message) {
        match message {
            Message::OpenSettings => {
                self.settings_draft = Some(settings::Draft::from_settings(&self.persisted));
            }
            Message::SettingsToggled(field) => self.toggle_setting(field),
            Message::SettingsSeconds(input) => {
                if let Some(draft) = &mut self.settings_draft {
                    draft.seconds_text = settings::sanitize_seconds(&input);
                }
            }
            Message::SettingsTheme(choice) => {
                // The draft carries the pick; `palette()` previews it live
                // while the dialog stays open, so Cancel reverts for free.
                if let Some(draft) = &mut self.settings_draft {
                    draft.theme = choice;
                }
            }
            Message::SettingsScale(percent) => {
                // Like the theme: `ui_scale()` previews the draft's pick live.
                if let Some(draft) = &mut self.settings_draft {
                    draft.ui_scale_percent = percent;
                }
            }
            Message::ScaleStepped(up) => self.step_scale(up),
            Message::SettingsOk => self.apply_settings(),
            Message::SettingsCancel => self.settings_draft = None,
            // Not a Settings message — update() never routes one here.
            _ => {}
        }
    }

    /// Flips one checkbox on the open Settings dialog's draft (a no-op when
    /// the dialog is closed — a stray message changes nothing).
    const fn toggle_setting(&mut self, field: SettingsField) {
        if let Some(draft) = &mut self.settings_draft {
            let slot = match field {
                SettingsField::FilterShort => &mut draft.filter_short_playlists,
                SettingsField::FilterLoops => &mut draft.filter_looping_playlists,
                SettingsField::HumanSizes => &mut draft.human_readable_sizes,
                SettingsField::ChapterCount => &mut draft.display_chapter_count,
                SettingsField::Autosave => &mut draft.autosave_report,
                SettingsField::StreamDiagnostics => &mut draft.report_stream_diagnostics,
                SettingsField::QuickSummary => &mut draft.report_quick_summary,
                SettingsField::KeepPartial => &mut draft.keep_partial,
            };
            *slot = !*slot;
        }
    }

    /// Steps the UI scale one preset up or down — the Ctrl+'+' / Ctrl+'-'
    /// shortcut. With the Settings dialog open the step edits its draft (the
    /// same live-preview, Cancel-reverts road as the chips); otherwise it
    /// applies to the settings directly and persists.
    fn step_scale(&mut self, up: bool) {
        if let Some(draft) = &mut self.settings_draft {
            draft.ui_scale_percent = settings::scale_step(draft.ui_scale_percent, up);
        } else {
            let stepped = settings::scale_step(self.persisted.ui_scale_percent, up);
            if stepped != self.persisted.ui_scale_percent {
                self.persisted.ui_scale_percent = stepped;
                self.persist();
            }
        }
    }

    /// Writes the persisted state to the boot-resolved config path,
    /// best-effort — a no-op when no path resolved (and in every test-built
    /// `App`, whose path is `None`).
    fn persist(&self) {
        if let Some(path) = &self.config {
            settings::save_to(path, &self.persisted);
        }
    }

    /// The Settings dialog's OK: persist the draft and hand the new view
    /// settings to the flow — a filter change re-derives the table from the
    /// retained disc (no rescan), a display change re-renders the cells, and
    /// the theme pick becomes the applied preference.
    fn apply_settings(&mut self) {
        if let Some(draft) = self.settings_draft.take() {
            draft.apply_to(&mut self.persisted);
            self.theme_pref = ThemePref::from_choice(self.persisted.theme);
            self.persist();
            self.flow.set_view(ViewSettings::from_settings(&self.persisted));
        }
    }

    /// Persists the closing window's geometry (`BDInfo` saves at `FormClosing`
    /// too) and really closes it. The position stays untouched when the
    /// platform reports none (Wayland), so a size-only restore still works.
    /// A maximized close records only the flag: the live readout is then the
    /// maximized rect, and iced exposes no restore bounds, so the stored
    /// geometry keeps the last *normal* close's values — `BDInfo` persists its
    /// `RestoreBounds` at that moment; this is the nearest reachable stand-in.
    fn save_and_close(
        &mut self,
        id: iced::window::Id,
        position: Option<iced::Point>,
        size: iced::Size,
        maximized: bool,
    ) -> Task<Message> {
        self.persisted.window_maximized = maximized;
        if !maximized {
            // Geometry round-trips VERIFIED symmetric at a non-100% UI scale
            // (iced_winit 0.14: `conversion::window_attributes` multiplies
            // `Settings.size` by the application scale factor at creation,
            // and `window::size` divides the physical size by DPI × that
            // same factor — so the stored size is app-logical on both
            // roads; positions are DPI-logical on both). No adjustment, or
            // the size would drift by the scale factor every launch.
            self.persisted.window_size = Some((size.width, size.height));
            if let Some(point) = position {
                self.persisted.window_pos = Some((point.x, point.y));
            }
        }
        self.persist();
        iced::window::close(id)
    }

    /// Applies a column-divider drag: shifts weight across the held boundary by
    /// the cursor's movement since the last event, scaled by the stored column
    /// span so it tracks the cursor at any window size. Only the two columns the
    /// divider separates change; the rest hold, so far columns never shift.
    fn column_drag(&mut self, x: f32) {
        let Some((grid, boundary, cols_width, last_x)) = self.col_drag else { return };
        if let Some(prev_x) = last_x {
            let total: f32 = self.column_weights(grid).iter().sum();
            let delta = columns::drag_delta(x, prev_x, cols_width, total);
            columns::adjust(self.column_weights_mut(grid), boundary, delta);
        }
        self.col_drag = Some((grid, boundary, cols_width, Some(x)));
    }

    /// The live column weights for `grid`.
    const fn column_weights(&self, grid: ColGrid) -> &[f32] {
        match grid {
            ColGrid::Playlist => self.playlist_w.as_slice(),
            ColGrid::StreamFile => self.stream_w.as_slice(),
            ColGrid::Codec => self.codec_w.as_slice(),
        }
    }

    /// The live column weights for `grid`, mutable.
    const fn column_weights_mut(&mut self, grid: ColGrid) -> &mut [f32] {
        match grid {
            ColGrid::Playlist => self.playlist_w.as_mut_slice(),
            ColGrid::StreamFile => self.stream_w.as_mut_slice(),
            ColGrid::Codec => self.codec_w.as_mut_slice(),
        }
    }

    /// The live subscriptions: the OS light/dark change feed, plus — only while
    /// the initial scan is in flight — a per-frame tick that animates the
    /// indeterminate progress bar, and — only while the measured scan is — a
    /// ~1 s wall-clock tick that keeps the elapsed readout and the stall hint
    /// moving. The ticks are off in every other state, so the window is not
    /// redrawing continuously when idle.
    fn subscription(&self) -> iced::Subscription<Message> {
        let mut subs = vec![iced::system::theme_changes().map(Message::OsTheme)];
        // Debug auto-open (`BDINFO_GUI_OPEN`/`_ISO`) rides a SUBSCRIPTION,
        // not a boot task: boot-batch messages that complete while the
        // compositor is still initializing are dropped by the shell (proven
        // on the CI runners and reproduced locally), and any retry chained
        // from such a message dies with it. A subscription is always alive;
        // this one only exists while the flow sits at Idle, so it vanishes
        // the moment the open takes.
        #[cfg(debug_assertions)]
        if self.flow.stage() == Stage::Idle
            && (std::env::var_os("BDINFO_GUI_OPEN").is_some()
                || std::env::var_os("BDINFO_GUI_ISO").is_some())
        {
            subs.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::DebugAutoOpen));
        }
        // The close button / Alt+F4: closing is manual (so the geometry saves
        // on the way out) — this feed is what makes the window closable at all.
        subs.push(iced::window::close_requests().map(Message::CloseRequested));
        // Drag-and-drop, surfaced by winit as window events on every desktop
        // platform (Wayland excepted — winit does not implement file drops
        // there). Always on: the busy rule is applied where the drop lands.
        subs.push(iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Window(iced::window::Event::FileHovered(_)) => Some(Message::FileHovered),
            iced::Event::Window(iced::window::Event::FileDropped(path)) => {
                Some(Message::FileDropped(path))
            }
            iced::Event::Window(iced::window::Event::FilesHoveredLeft) => {
                Some(Message::FilesHoveredLeft)
            }
            _ => None,
        }));
        // The command-key shortcuts (Ctrl on Windows/Linux, Cmd on macOS —
        // iced's `command` modifier): Ctrl+C copies the highlighted row's
        // file path; Ctrl+'+' / Ctrl+'-' step the UI scale ("=" is the
        // unshifted '+' key on common layouts — the usual zoom-in binding).
        // Only events no widget consumed are taken; the guard rules (busy /
        // report view / what is highlighted) live in the handlers, so the
        // subscription is a plain feed.
        subs.push(iced::event::listen_with(|event, status, _window| match event {
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. })
                if status == iced::event::Status::Ignored && modifiers.command() =>
            {
                match key.as_ref() {
                    iced::keyboard::Key::Character("c") => Some(Message::CopyPath),
                    iced::keyboard::Key::Character("+" | "=") => Some(Message::ScaleStepped(true)),
                    iced::keyboard::Key::Character("-") => Some(Message::ScaleStepped(false)),
                    _ => None,
                }
            }
            _ => None,
        }));
        if self.flow.stage() == Stage::Listing {
            subs.push(iced::window::frames().map(|_| Message::Tick));
        }
        // The measured scan can run for hours, so its liveness tick is a 1 Hz
        // wall-clock subscription — NOT the per-frame ticker above, whose ~60
        // redraws a second are only acceptable for the brief listing stage.
        // Each tick re-reads the clock in the Tick arm, so elapsed climbs and
        // the stall hint appears even when a read stuck in drive-firmware
        // retries emits no progress event for minutes.
        if self.flow.stage() == Stage::Scanning {
            subs.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick));
        }
        // While a column divider is held, follow the mouse globally — even off the
        // grab zone — so the drag tracks the cursor and ends on button release.
        if self.col_drag.is_some() {
            subs.push(iced::event::listen_with(|event, _status, _window| match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Message::ColDragMove(position.x))
                }
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )) => Some(Message::ColDragEnd),
                _ => None,
            }));
        }
        iced::Subscription::batch(subs)
    }

    /// Whether a scan (initial or measured) is in flight — the source controls are
    /// disabled while one runs, like `BDInfo`.
    const fn is_busy(&self) -> bool {
        matches!(self.flow.stage(), Stage::Listing | Stage::Scanning)
    }

    /// Debug auto-open: send the `BDINFO_GUI_OPEN`/`_ISO` pick. The Idle-gated
    /// subscription that delivers this tick stops the moment the flow leaves
    /// Idle, so a taken open is never re-sent.
    #[cfg(debug_assertions)]
    fn debug_auto_open(&mut self) -> Task<Message> {
        if self.flow.stage() != Stage::Idle {
            return Task::none();
        }
        if let Some(path) = std::env::var_os("BDINFO_GUI_OPEN") {
            self.update(Message::FolderPicked(Some(PathBuf::from(path))))
        } else if let Some(path) = std::env::var_os("BDINFO_GUI_ISO") {
            self.update(Message::IsoPicked(Some(PathBuf::from(path))))
        } else {
            Task::none()
        }
    }

    /// Debug drive walk: try to apply step `step` — poll while its
    /// precondition is false, leave the FAILED marker and end the walk when
    /// the poll cap is spent, and schedule the step's marker once applied.
    #[cfg(debug_assertions)]
    fn drive_next(&mut self, step: usize, polls: u32) -> Task<Message> {
        match DRIVE_STEPS.get(step) {
            Some(spec) if (spec.ready)(self) => {
                let applied = (spec.act)(self);
                Task::batch([applied, drive_after(DRIVE_SETTLE_MS, Message::DriveMark(step))])
            }
            // The precondition never came true: the FAILED marker is the
            // harness's red signal.
            Some(spec) if polls >= DRIVE_MAX_POLLS => {
                drive_marker(step, &format!("FAILED-{}", spec.name), self.flow.stage());
                iced::exit()
            }
            Some(_) => drive_after(
                DRIVE_POLL_MS,
                Message::DriveNext { step, polls: polls.wrapping_add(1) },
            ),
            None => iced::exit(),
        }
    }

    /// Debug drive walk: step `step` has rendered — write its marker, then
    /// schedule the next step (or the clean exit) a capture window later.
    #[cfg(debug_assertions)]
    fn drive_mark(&self, step: usize) -> Task<Message> {
        DRIVE_STEPS.get(step).map_or_else(Task::none, |spec| {
            drive_marker(step, spec.name, self.flow.stage());
            let next = step.wrapping_add(1);
            let message = if next < DRIVE_STEPS.len() {
                Message::DriveNext { step: next, polls: 0 }
            } else {
                Message::SmokeExit
            };
            drive_after(drive_window_ms(), message)
        })
    }

    /// Applies a listing worker's progress event — only the current worker's
    /// (`generation` is the stamp `begin_listing` issued it; the flow holds no
    /// listing generation of its own, so a cancelled worker's stragglers are
    /// dropped here, before they could touch a re-pick of the same input).
    fn on_list_progress(
        &mut self,
        generation: u64,
        file: String,
        done: u64,
        total: u64,
    ) -> Task<Message> {
        if generation == self.generation {
            let elapsed = self.scan_start.map_or(Duration::ZERO, |start| start.elapsed());
            self.last_progress = Some(Instant::now());
            self.flow.list_progress(file, done, total, elapsed);
        }
        Task::none()
    }

    /// Applies a finished structural scan. A successful listing becomes the
    /// remembered last path (saved now — `BDInfo` keeps `LastPath` the same
    /// way). In a debug build it also honours the capture harness's auto-scan
    /// hook (`BDINFO_GUI_SCAN=all`), selecting and scanning every playlist so
    /// a screenshot can show measured data.
    fn on_listed(
        &mut self,
        generation: u64,
        input: &Input,
        result: Result<Structural, String>,
    ) -> Task<Message> {
        // The listing outcome, next to begin_listing's start line. A stale
        // worker's outcome is logged too, then dropped: after a cancel it is
        // the record that the blocked thread finally returned.
        match &result {
            Ok(_) => log::info!("listed {}", input.display()),
            Err(error) => log::info!("listing failed: {error}"),
        }
        if generation != self.generation {
            log::info!("stale listing result dropped");
            return Task::none();
        }
        // The worker has returned; its cancel flag can no longer do anything.
        self.scan_cancel = None;
        let view = ViewSettings::from_settings(&self.persisted);
        self.flow = std::mem::take(&mut self.flow).listed(input, result, view);
        // Only when THIS input just became the listing (not a superseded or
        // failed result) does it become the remembered source.
        if self.flow.stage() == Stage::Listed && self.flow.current_input() == Some(input) {
            self.persisted.last_path = Some(input.path().to_path_buf());
            self.persist();
        }
        #[cfg(debug_assertions)]
        if self.flow.stage() == Stage::Listed && debug_scan_all() {
            self.flow.select_all();
            return self.start_scan();
        }
        Task::none()
    }

    /// Applies a display tick: advances the indeterminate bar's phase, and —
    /// while the measured scan is in flight — re-reads the wall clock so the
    /// elapsed readout and the stall hint move even when no progress event
    /// arrives ([`Flow::tick`]).
    fn on_tick(&mut self) {
        self.pulse = progress::pulse_advance(self.pulse);
        if self.flow.stage() == Stage::Scanning {
            let elapsed = self.scan_start.map_or(Duration::ZERO, |start| start.elapsed());
            let since_progress = self.last_progress.map_or(Duration::ZERO, |at| at.elapsed());
            self.flow.tick(elapsed, since_progress);
        }
    }

    /// Cancels the in-flight scan for real — the measured scan back to the
    /// table, the listing back to the source prompt: trips the worker's
    /// cooperative stop flag (the read loop exits at its next chunk, freeing
    /// CPU and IO), then moves the UI on immediately — never waiting on the
    /// worker, which a stuck read can hold for minutes with no further
    /// message. The worker's late result is dropped by the generation guards
    /// (the flow is no longer Scanning / Listing), so nothing else changes.
    fn cancel_scan(&mut self) -> Task<Message> {
        if let Some(cancel) = self.scan_cancel.take() {
            if self.flow.stage() == Stage::Listing {
                log::info!("listing cancelled");
            } else {
                log::info!("scan cancelled");
            }
            cancel.store(true, Ordering::Relaxed);
        }
        self.flow = std::mem::take(&mut self.flow).cancel();
        Task::none()
    }

    /// Applies a finished measured scan, announcing a clean finish only when this
    /// event actually completed the in-flight scan (a stale finish leaves the
    /// stage unchanged, so no modal fires).
    fn on_finished(
        &mut self,
        generation: u64,
        report: String,
        errors: Arc<Vec<ScanError>>,
        playlists: Vec<PlaylistSummary>,
    ) -> Task<Message> {
        let was_scanning = self.flow.stage() == Stage::Scanning;
        self.flow = std::mem::take(&mut self.flow).finished(generation, report, errors, playlists);
        if was_scanning && self.flow.stage() == Stage::Reported {
            log::info!("scan finished: {} error(s) recorded", self.flow.report_errors().len());
            for error in self.flow.report_errors() {
                log::info!("scan error: {error}");
            }
            self.notice = Some(flow::scan_notice(self.flow.report_errors().len()));
            if self.persisted.autosave_report {
                self.autosave_report();
            }
        } else {
            // A completion the generation guard dropped — the user cancelled
            // or restarted; the log still records the old worker returning.
            log::info!("stale scan completion dropped");
        }
        Task::none()
    }

    /// Writes the finished report without asking — `BDInfo`'s `AutosaveReport`
    /// on `ScanBDROMCompleted`. The destination is implied by the input (the
    /// disc folder, or next to the `.iso`); the outcome lands in the status
    /// line either way, a failure being the same non-blocking message a manual
    /// save shows.
    fn autosave_report(&mut self) {
        match self.flow.current_input().and_then(paths::autosave_dir) {
            Some(dir) => self.save_report(&dir),
            None => self.status = Some("Save failed: no autosave destination".to_owned()),
        }
    }

    /// Applies a folder / `.iso` picker completion. rfd's dialogs are
    /// parentless (non-modal), so one can sit open across a state change and
    /// resolve into a stage it was never opened in — ignored while a scan is
    /// in flight, the busy rule every other input road obeys (a late pick must
    /// never replace a live scan and strand its worker's cancel flag).
    fn picked(&mut self, input: Input) -> Task<Message> {
        if self.is_busy() {
            return Task::none();
        }
        self.begin_listing(input)
    }

    /// Applies a save-destination dialog completion. The dialog is non-modal
    /// like the pickers, so a destination can resolve mid-scan — when
    /// `flow.report()` is the structural (zero-bitrate) render, which would
    /// silently overwrite the measured report on disk. Any settled stage may
    /// save ([`Self::save_report`] itself requires an available report, and
    /// the pre-scan structural save is a deliberate feature); a busy one never.
    fn save_dest(&mut self, dir: &std::path::Path) -> Task<Message> {
        if !self.is_busy() {
            self.save_report(dir);
        }
        Task::none()
    }

    /// Opens `path` — a dropped item or the boot argument — through the same
    /// classify-and-list road the pickers take ([`Input::classify`], the one
    /// classifier both roads share). Ignored while a scan is in flight,
    /// the same rule that disables the Source buttons; a path that is neither a
    /// directory nor a `.iso` file lands in the same friendly failure state as
    /// a bad pick, with the loaded-disc chrome cleared like any new open.
    fn open_path(&mut self, path: PathBuf) -> Task<Message> {
        if self.is_busy() {
            return Task::none();
        }
        match Input::classify(path) {
            Ok(input) => self.begin_listing(input),
            Err(message) => {
                self.status = None;
                self.showing_report = false;
                self.notice = None;
                self.hovered = None;
                self.hovered_header = None;
                self.pane_selection = None;
                self.flow = std::mem::take(&mut self.flow).open_failed(message);
                Task::none()
            }
        }
    }

    /// Begins the initial scan of `input` — the bounded `Codecs` pass, which
    /// reads each stream file's head to fill the codec detail — on a worker
    /// thread, streaming its progress + result back as messages, exactly like
    /// the measured scan below: a progress stream needs more than the one
    /// message a one-shot task can deliver, and on damaged media the listing
    /// can block inside a single read for minutes, which must not park one of
    /// iced's executor threads. The generation stamp on the messages is what
    /// discards a superseded or cancelled worker's stragglers.
    fn begin_listing(&mut self, input: Input) -> Task<Message> {
        self.status = None;
        self.showing_report = false;
        self.notice = None;
        self.hovered = None;
        self.hovered_header = None;
        self.pane_selection = None;
        self.pulse = 0;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.scan_start = Some(Instant::now());
        self.last_progress = self.scan_start;
        // The one log line per open: pins down whether a scan ever started
        // when a report or listing seems to be missing in the field.
        log::info!("listing {}", input.display());
        self.flow = Flow::start_listing(input.clone());
        // A fresh flag per listing — see the `scan_cancel` field.
        let cancel = Arc::new(AtomicBool::new(false));
        self.scan_cancel = Some(Arc::clone(&cancel));

        let (sender, receiver) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            #[cfg(debug_assertions)]
            debug_scan_delay();
            // The same ~100 ms coalescing as the measured worker below — the
            // codec pass fires its callback around every read too.
            let mut last_sent: Option<Instant> = None;
            let mut last_file = String::new();
            let progress_sender = sender.clone();
            // The core is fuzz-held no-panic, but the exposure is exactly
            // this untrusted-disc path — an escaped panic would lose the
            // Listed message and stick the non-dismissable "scanning"
            // modal, so it is caught onto the same friendly failure road
            // a bad disc takes.
            let result = scan::catch_scan_panic(|| {
                scan::scan_structural(
                    &input,
                    &mut |progress: ScanProgress<'_>| {
                        let file_changed = progress.file != last_file;
                        let since_last = last_sent.map(|at| at.elapsed());
                        if !progress::emit_due(
                            file_changed,
                            progress.done,
                            progress.total,
                            since_last,
                        ) {
                            return;
                        }
                        last_sent = Some(Instant::now());
                        if file_changed {
                            // One log line per stream file — after a pre-scan
                            // freeze, the last of these names the stuck file.
                            log::info!("listing {}", progress.file);
                            progress.file.clone_into(&mut last_file);
                        }
                        let _ = progress_sender.unbounded_send(Message::ListProgress {
                            generation,
                            file: progress.file.to_owned(),
                            done: progress.done,
                            total: progress.total,
                        });
                    },
                    &cancel,
                )
            });
            let _ = sender.unbounded_send(Message::Listed { generation, input, result });
        });
        // The receiver stream ends when the worker drops its sender, completing
        // this task; each item arrives as a `Message`.
        Task::stream(receiver)
    }

    /// Spawns the measured scan on a worker thread (so native demux keeps its
    /// `thread::scope` parallelism) and streams its progress + result back as
    /// messages. The UI thread stays free — only the channel is polled here.
    fn start_scan(&mut self) -> Task<Message> {
        let Some(ScanRequest { input, selection, scan_files, options, scan_options }) =
            self.flow.scan_request()
        else {
            return Task::none();
        };
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.scan_start = Some(Instant::now());
        self.last_progress = self.scan_start;
        // The scan's opening log line: with the per-file lines and the
        // terminal line below, gui.log can now place a field report's hang
        // inside the scan (and name the file) rather than only before it.
        log::info!(
            "scan started: {} playlist(s), {} stream file(s), input {}",
            selection.len(),
            scan_files.len(),
            input.display()
        );
        self.status = None;
        self.showing_report = false;
        self.notice = None;
        self.pane_selection = None;
        // A fresh flag per scan — see the `scan_cancel` field.
        let cancel = Arc::new(AtomicBool::new(false));
        self.scan_cancel = Some(Arc::clone(&cancel));

        let (sender, receiver) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            // A panic anywhere below would drop the sender and simply end the
            // message stream — the UI stuck at Scanning with no hint. The
            // armed alarm's Drop runs during that unwind and reports the
            // death as an ordinary scan failure; the generation tag keeps a
            // stale alarm harmless, and the success path defuses it after
            // delivering its own terminal message.
            let alarm_sender = sender.clone();
            let alarm = scan::PanicAlarm::new(move || {
                let _ = alarm_sender.unbounded_send(Message::ScanFailed {
                    generation,
                    error: "scan worker terminated unexpectedly".to_owned(),
                });
            });
            #[cfg(debug_assertions)]
            debug_worker_panic();
            #[cfg(debug_assertions)]
            debug_scan_delay();
            // The scan fires its progress callback before and after every read —
            // several per 256 KiB chunk, hundreds of thousands of times on a
            // feature disc. Coalesce to one message per ~100 ms (plus every file
            // boundary) so the UI re-renders a few times a second, not thousands;
            // the CLI throttles its terminal redraw the same way.
            let mut last_sent: Option<Instant> = None;
            let mut last_file = String::new();
            let mut on_progress = |progress: ScanProgress<'_>| {
                let file_changed = progress.file != last_file;
                let since_last = last_sent.map(|at| at.elapsed());
                if !progress::emit_due(file_changed, progress.done, progress.total, since_last) {
                    return;
                }
                last_sent = Some(Instant::now());
                if file_changed {
                    // One log line per stream file, from the worker thread —
                    // after a freeze, the last of these names the stuck file.
                    log::info!("scanning {}", progress.file);
                    progress.file.clone_into(&mut last_file);
                }
                let _ = sender.unbounded_send(Message::Progress {
                    generation,
                    file: progress.file.to_owned(),
                    done: progress.done,
                    total: progress.total,
                });
            };
            let result = scan::scan_measured(
                &input,
                &selection,
                &scan_files,
                options,
                scan_options,
                &mut on_progress,
                &cancel,
            );
            let outcome = match result {
                Ok(measured) => Message::Finished {
                    generation,
                    report: measured.report,
                    errors: measured.errors,
                    playlists: measured.playlists,
                },
                Err(error) => Message::ScanFailed { generation, error },
            };
            let _ = sender.unbounded_send(outcome);
            alarm.defuse();
        });

        self.flow = std::mem::take(&mut self.flow).start_scanning(generation);
        // The receiver stream ends when the worker drops its sender, completing
        // this task; each item arrives as a `Message`.
        Task::stream(receiver)
    }

    /// Writes the rendered report into `dir` as `BDINFO.{label}.txt`, bytes
    /// verbatim (the locked CRLF / UTF-8 no-BOM contract — never re-encode).
    /// The label is disc-controlled, so the filename goes through the core's
    /// sanitizer ([`report::file_name`]) — the same name the CLI writes.
    fn save_report(&mut self, dir: &std::path::Path) {
        let (Some(rendered), Some(label)) = (self.flow.report(), self.flow.label()) else {
            return;
        };
        let target = dir.join(report::file_name(label));
        self.status = match std::fs::write(&target, rendered.as_bytes()) {
            Ok(()) => Some(format!("Report saved to: {}", target.display())),
            Err(err) => {
                // The status line is transient; autosave in particular runs
                // with nobody watching, so the failure goes to the log too.
                log::warn!("report save failed ({}): {err}", target.display());
                Some(format!("Save failed: {err}"))
            }
        };
    }

    /// Makes `index` the active (highlighted) row. The lower panes now show a
    /// different playlist, so any pane-row selection that pointed into the old
    /// contents is dropped.
    fn activate_row(&mut self, index: usize) -> Task<Message> {
        self.flow.set_active(index);
        self.pane_selection = None;
        Task::none()
    }

    /// Records the row under the cursor (its hover lift).
    fn on_row_hovered(&mut self, region: Region, index: usize) -> Task<Message> {
        self.hovered = Some((region, index));
        Task::none()
    }

    /// Clears the row hover if `(region, index)` still owns it.
    fn on_row_unhovered(&mut self, region: Region, index: usize) -> Task<Message> {
        if self.hovered == Some((region, index)) {
            self.hovered = None;
        }
        Task::none()
    }

    /// Records the sortable header cell under the cursor (its hover wash).
    fn on_header_hovered(&mut self, column: SortColumn) -> Task<Message> {
        self.hovered_header = Some(column);
        Task::none()
    }

    /// Clears the header hover if `column` still owns it.
    fn on_header_unhovered(&mut self, column: SortColumn) -> Task<Message> {
        if self.hovered_header == Some(column) {
            self.hovered_header = None;
        }
        Task::none()
    }

    /// Copies the highlighted row's underlying file path — `BDInfo`'s Ctrl+C.
    /// The last-clicked Stream File row wins (that clip's path); otherwise the
    /// active playlist row copies its `.MPLS` path. A selected Codec row copies
    /// nothing, matching `BDInfo`'s scope (its stream list doesn't respond).
    /// Ignored while a scan is in flight or the report view covers the panes.
    fn copy_path(&mut self) -> Task<Message> {
        if self.is_busy() || self.showing_report {
            return Task::none();
        }
        let Some(input) = self.flow.current_input() else { return Task::none() };
        let target = match self.pane_selection {
            Some((Region::StreamFile, index)) => {
                self.flow.active_clip_name(index).map(|clip| paths::stream_path(input, &clip))
            }
            Some((Region::Codec, _)) => None,
            _ => self.flow.active_playlist_name().map(|name| paths::playlist_path(input, &name)),
        };
        match target {
            Some(path) => {
                self.status = Some(format!("Copied: {path}"));
                iced::clipboard::write(clipboard::sanitize(path))
            }
            None => Task::none(),
        }
    }

    /// Copies the report to the clipboard via iced's clipboard task. The text
    /// is NUL-sanitized on the way ([`clipboard::sanitize`]) — a degenerate
    /// disc's report carries literal NULs, and Windows' NUL-terminated
    /// clipboard would truncate every paste there.
    fn copy_report(&mut self) -> Task<Message> {
        match self.flow.report() {
            Some(report) => {
                self.status = Some("Report copied to the clipboard.".to_owned());
                // One owned copy for the clipboard task — a one-shot click
                // cost; the flow keeps the stored string.
                iced::clipboard::write(clipboard::sanitize(report.to_owned()))
            }
            None => Task::none(),
        }
    }

    // ── view ────────────────────────────────────────────────────────────────

    /// Projects the flow onto the `BDInfo`-style layout: a brand toolbar, the
    /// source bar, a stage-specific body (empty prompt / structural-scan note /
    /// fatal error / the three master-detail panes, or the report view), and the
    /// bottom action+progress bar — with the scan-complete modal over the top.
    fn view(&self) -> Element<'_, Message> {
        let p = self.palette();
        let body: Element<'_, Message> = if self.showing_report && self.flow.report_available() {
            self.report_view(p)
        } else {
            match self.flow.stage() {
                Stage::Idle => self.empty_state(p),
                Stage::Failed => self.failed_state(p),
                // The initial scan (`Listing`) shows the empty pane skeleton with a
                // "please wait" overlay on top, like BDInfo.
                Stage::Listing | Stage::Listed | Stage::Scanning | Stage::Reported => {
                    self.disc_view(p)
                }
            }
        };

        let content = Column::new()
            .width(Length::Fill)
            .height(Length::Fill)
            .push(toolbar(p))
            .push(self.source_bar(p))
            .push(container(body).width(Length::Fill).height(Length::Fill).padding(ui::GAP_4))
            .push(self.bottom_bar(p));

        let base = container(content).width(Length::Fill).height(Length::Fill).into();
        if self.flow.stage() == Stage::Listing {
            return scanning_modal(base, scanning_card(p, self.pulse, self.flow.progress_view()));
        }
        if let Some(draft) = &self.settings_draft {
            return modal(base, settings_card(p, draft), Message::SettingsCancel);
        }
        match &self.notice {
            Some(notice) => modal(base, notice_card(p, notice), Message::DismissNotice),
            None => base,
        }
    }

    /// The source bar: a label, the read-only path field, and Browse / ISO /
    /// Rescan — the `File: Open` seam, contextual at the top like `BDInfo`.
    fn source_bar(&self, p: Palette) -> Element<'_, Message> {
        let path = self.flow.input_display();
        let field_text = path.as_deref().unwrap_or("No disc selected");
        let field_color = if path.is_some() { p.text } else { p.text_faint };
        let field = container(
            text(field_text.to_owned())
                .font(ui::MONO)
                .size(ui::TEXT_SM)
                .color(field_color)
                // A long path clips at the field's edge (like BDInfo's textbox)
                // rather than wrapping to a second line when the window narrows.
                .wrapping(text::Wrapping::None)
                // Disc-derived text (here: the disc's path) can legitimately be
                // Japanese/Chinese; only Advanced shaping engages cosmic-text's
                // per-script system-font fallback — Basic renders CJK as tofu
                // even when matching system fonts exist. Chrome text (fixed
                // ASCII labels, the table cells — ASCII by spec) stays Basic,
                // which is cheaper to shape.
                .shaping(text::Shaping::Advanced),
        )
        .width(Length::Fill)
        .clip(true)
        .padding([ui::GAP_2, ui::GAP_3])
        .style(ui::path_field(p));

        let bar = Row::new()
            .width(Length::Fill)
            .align_y(Vertical::Center)
            .spacing(ui::GAP_2)
            .push(text("Source:").size(ui::TEXT_SM).font(ui::UI_MEDIUM).color(p.text_muted))
            .push(field)
            .push(self.labeled_command_button(p, "Browse...", Command::OpenFolder))
            .push(self.labeled_command_button(p, "ISO", Command::OpenIso))
            .push(self.labeled_command_button(p, "Rescan", Command::Rescan));
        container(bar).width(Length::Fill).padding([ui::GAP_3, ui::GAP_4]).into()
    }

    /// A secondary-styled button for a [`Command`], disabled when not actionable,
    /// labelled by the command's own [`Command::label`].
    fn command_button(&self, p: Palette, command: Command) -> Element<'_, Message> {
        self.labeled_command_button(p, command.label(), command)
    }

    /// A secondary-styled button for a [`Command`] with an explicit `label` (so a
    /// toolbar control can match `BDInfo`'s wording — e.g. "Browse..." — while the
    /// same command keeps a friendlier label elsewhere).
    fn labeled_command_button<'a>(
        &self,
        p: Palette,
        label: &'a str,
        command: Command,
    ) -> Element<'a, Message> {
        let enabled = self.command_enabled(command);
        button(text(label).size(ui::TEXT_SM).font(ui::UI_MEDIUM))
            .padding([ui::GAP_2, ui::GAP_3])
            .style(ui::secondary_button(p))
            .on_press_maybe(enabled.then(|| command.message()))
            .into()
    }

    /// The centred empty state: the brand mark, a call to action, and the two
    /// open buttons.
    fn empty_state(&self, p: Palette) -> Element<'_, Message> {
        let card = Column::new()
            .align_x(Horizontal::Center)
            .spacing(ui::GAP_4)
            .push(brand_mark_large(p))
            .push(text("Analyze a Blu-ray disc").size(ui::TEXT_LG).font(ui::UI_SEMIBOLD).color(p.text))
            .push(
                text("Open a disc folder or an .iso image to read its playlists,\nstreams, and per-stream technical specs.")
                    .size(ui::TEXT_BASE)
                    .color(p.text_muted)
                    .align_x(Horizontal::Center),
            )
            .push(
                Row::new()
                    .spacing(ui::GAP_2)
                    .push(
                        button(text(Command::OpenFolder.label()).size(ui::TEXT_BASE).font(ui::UI_MEDIUM))
                            .padding([ui::GAP_2, ui::GAP_6])
                            .style(ui::primary_button(p))
                            .on_press(Command::OpenFolder.message()),
                    )
                    .push(self.command_button(p, Command::OpenIso)),
            );
        let framed = container(card).padding(ui::GAP_6).max_width(560.0).style(ui::hero_card(p));
        container(framed).center(Length::Fill).into()
    }

    /// The fatal-pick error state: a danger card with the message and a retry hint.
    fn failed_state(&self, p: Palette) -> Element<'_, Message> {
        let message = self.flow.error_message().unwrap_or("Unknown error").to_owned();
        let card = Column::new()
            .align_x(Horizontal::Center)
            .spacing(ui::GAP_3)
            .push(
                Row::new()
                    .align_y(Vertical::Center)
                    .spacing(ui::GAP_2)
                    .push(status_dot(p.danger))
                    .push(
                        text("Couldn't open the disc")
                            .size(ui::TEXT_MD)
                            .font(ui::UI_SEMIBOLD)
                            .color(p.text),
                    ),
            )
            .push(
                text(message)
                    .size(ui::TEXT_SM)
                    .font(ui::MONO)
                    .color(p.text_muted)
                    .align_x(Horizontal::Center)
                    // The failure message embeds the offending path (CJK-able).
                    .shaping(text::Shaping::Advanced),
            )
            .push(
                text("Choose a Blu-ray disc folder or a .iso image and try again.")
                    .size(ui::TEXT_SM)
                    .color(p.text_faint),
            )
            .push(
                Row::new()
                    .spacing(ui::GAP_2)
                    .push(self.command_button(p, Command::OpenFolder))
                    .push(self.command_button(p, Command::OpenIso)),
            );
        let framed = container(card).padding(ui::GAP_6).max_width(620.0).style(ui::hero_card(p));
        container(framed).center(Length::Fill).into()
    }

    /// The loaded-disc body: any banners, then the three resizable master-detail
    /// panes (Playlist / Stream File / Codec, with draggable splitters between
    /// them) and the detected-disc info box — the classic `BDInfo` layout.
    fn disc_view(&self, p: Palette) -> Element<'_, Message> {
        let mut content = Column::new().width(Length::Fill).height(Length::Fill).spacing(ui::GAP_2);

        // A failed measured scan leaves a banner over the still-usable table; any
        // structural warnings sit alongside.
        if let Some(error) = self.flow.error_message() {
            content = content.push(banner(p, p.danger, error));
        }
        for warning in self.flow.warnings() {
            content = content.push(banner(p, p.warning, warning.as_str()));
        }

        content =
            content.push(container(self.disc_panes(p)).width(Length::Fill).height(Length::Fill));
        content = content.push(self.info_box(p));
        content.into()
    }

    /// The three panes as a resizable [`PaneGrid`]: draggable splitters between
    /// them (their ratios live in [`App::panes`]) that also scale with the window.
    /// The Playlist pane's title bar carries the section's selection controls; the
    /// other two carry a plain title.
    fn disc_panes(&self, p: Palette) -> Element<'_, Message> {
        PaneGrid::new(&self.panes, |_pane, region, _maximized| match *region {
            Region::Playlist => pane_grid::Content::new(self.playlist_table(p)).title_bar(
                // The bottom padding keeps the select toggle clear of the
                // table frame below it (a bare title bar sits flush on it).
                pane_grid::TitleBar::new(self.select_bar(p))
                    .padding(iced::padding::bottom(ui::GAP_2)),
            ),
            Region::StreamFile => pane_grid::Content::new(data_table(
                p,
                STREAM_FILE_COLS,
                &self.stream_w,
                ColGrid::StreamFile,
                self.stream_file_table_rows(),
                Region::StreamFile,
                self.pane_hovered(Region::StreamFile),
                self.pane_selected(Region::StreamFile),
            ))
            .title_bar(pane_grid::TitleBar::new(pane_title(p, "Stream File"))),
            Region::Codec => pane_grid::Content::new(data_table(
                p,
                CODEC_COLS,
                &self.codec_w,
                ColGrid::Codec,
                self.codec_table_rows(),
                Region::Codec,
                self.pane_hovered(Region::Codec),
                self.pane_selected(Region::Codec),
            ))
            .title_bar(pane_grid::TitleBar::new(pane_title(p, "Codec"))),
        })
        .spacing(ui::GAP_2)
        .on_resize(8.0, Message::PaneResized)
        .into()
    }

    /// The active playlist's stream-file rows, flattened to display cells
    /// (Stream File / Index / Length / Estimated Size / Measured Size).
    fn stream_file_table_rows(&self) -> Vec<Vec<String>> {
        self.flow
            .stream_file_rows()
            .into_iter()
            .map(|row: StreamFileRow| {
                vec![row.file, row.index, row.length, row.estimated, row.measured]
            })
            .collect()
    }

    /// The active playlist's codec rows, flattened to display cells (a hidden
    /// stream is marked with a leading `*` on the codec name).
    fn codec_table_rows(&self) -> Vec<Vec<String>> {
        self.flow
            .codec_rows()
            .into_iter()
            .map(|row: CodecRow| {
                let codec = if row.hidden { format!("* {}", row.codec) } else { row.codec };
                vec![codec, row.language, row.bitrate, row.description]
            })
            .collect()
    }

    /// The hovered row index within `region`, if the cursor is over one of its
    /// rows (else `None`) — drives that pane's per-row hover band.
    fn pane_hovered(&self, region: Region) -> Option<usize> {
        match self.hovered {
            Some((r, index)) if r == region => Some(index),
            _ => None,
        }
    }

    /// The clicked (selected) row index within `region`, if any — drives that
    /// pane's selection band.
    fn pane_selected(&self, region: Region) -> Option<usize> {
        match self.pane_selection {
            Some((r, index)) if r == region => Some(index),
            _ => None,
        }
    }

    /// The "Select Playlist(s)" control row: the select-all/none toggle and the
    /// count of what is selected.
    fn select_bar(&self, p: Palette) -> Element<'_, Message> {
        let editable = self.flow.editable();
        let toggle = if self.flow.all_selected() {
            button(text("Unselect All").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                .on_press_maybe(editable.then_some(Message::SelectNone))
        } else {
            button(text("Select All").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                .on_press_maybe(editable.then_some(Message::SelectAll))
        };
        let count = format!("{} of {} selected", self.flow.selected_count(), self.flow.row_count());
        Row::new()
            .width(Length::Fill)
            .align_y(Vertical::Center)
            .spacing(ui::GAP_3)
            .push(text("Playlist").size(ui::TEXT_SM).font(ui::UI_MEDIUM).color(p.text_muted))
            .push(toggle.padding([ui::GAP_2, ui::GAP_3]).style(ui::secondary_button(p)))
            .push(Space::new().width(Length::Fill))
            .push(text(count).size(ui::TEXT_SM).color(p.text_muted))
            .into()
    }

    /// The hand-rolled playlist table: a sharp data grid with a quiet header over
    /// scrollable selectable rows.
    fn playlist_table(&self, p: Palette) -> Element<'_, Message> {
        let header = container(resizable_header(
            p,
            PLAYLIST_COLS,
            ColGrid::Playlist,
            self.playlist_w.to_vec(),
            Some((PLAYLIST_SORT_COLS, self.flow.sort(), self.hovered_header)),
        ))
        .width(Length::Fill)
        .height(Length::Fixed(ui::HEADER_H))
        .style(ui::table_header(p));
        let rule = container(Space::new())
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(ui::divider(p.line_strong));

        let editable = self.flow.editable();
        let mut rows = Column::new().width(Length::Fill);
        for row_data in self.flow.table() {
            let hovered = self.hovered == Some((Region::Playlist, row_data.index));
            rows = rows.push(table_row(p, &row_data, editable, hovered, &self.playlist_w));
        }
        let body = scrollable(rows).width(Length::Fill).height(Length::Fill);

        let frame = container(
            Column::new()
                .width(Length::Fill)
                .height(Length::Fill)
                .push(header)
                .push(rule)
                .push(body),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(ui::table_frame(p));

        // With nothing withheld the pane is the bare frame — the note's own
        // absence, not an empty row, so the table keeps the full pane height.
        match self.hidden_line(p) {
            None => frame.into(),
            Some(line) => Column::new()
                .width(Length::Fill)
                .height(Length::Fill)
                .spacing(ui::GAP_2)
                .push(frame)
                .push(line)
                .into(),
        }
    }

    /// The hidden-count line under the playlist table: what the settings'
    /// filters withheld — their names in a tooltip on it — and the Show / Hide
    /// button that flips the transient reveal. `None` when they withheld
    /// nothing.
    ///
    /// The button is live only while the table is editable, like the selection
    /// controls: revealing rows under an in-flight scan would change the set it
    /// is measuring.
    fn hidden_line(&self, p: Palette) -> Option<Element<'_, Message>> {
        let hidden = self.flow.hidden_playlists()?;
        let revealed = self.flow.revealing();
        let note = text(hidden.line(revealed)).size(ui::TEXT_XS).color(p.text_muted);
        let names = container(
            // Playlist file names come off the disc, so they are CJK-able.
            text(hidden.names().join(", "))
                .size(ui::TEXT_XS)
                .color(p.text)
                .shaping(text::Shaping::Advanced),
        )
        .max_width(360.0)
        .padding([ui::GAP_1, ui::GAP_2])
        .style(ui::tooltip(p));
        let toggle = button(
            text(if revealed { "Hide" } else { "Show" }).size(ui::TEXT_XS).font(ui::UI_MEDIUM),
        )
        .padding([ui::GAP_1, ui::GAP_3])
        .style(ui::secondary_button(p))
        .on_press_maybe(self.flow.editable().then_some(Message::ToggleReveal));
        Some(
            Row::new()
                .width(Length::Fill)
                .align_y(Vertical::Center)
                .spacing(ui::GAP_2)
                .push(tooltip(note, names, tooltip::Position::Top))
                .push(Space::new().width(Length::Fill))
                .push(toggle)
                .into(),
        )
    }

    /// The "detected disc" info box beneath the panes — the footer block `BDInfo`
    /// prints: the disc title (when present), the detected folder, the detected
    /// features (when any), the disc size in bytes and human-readable form, and the
    /// hidden-tracks note (when any playlist hides a stream).
    fn info_box(&self, p: Palette) -> Element<'_, Message> {
        let mut lines = Column::new().width(Length::Fill).spacing(ui::GAP_1);

        if let Some(title) = self.flow.disc_title() {
            lines = lines.push(info_line(p, "Disc Title:", title.to_owned(), false));
        }

        let path = self.flow.current_input().map(Input::display).unwrap_or_default();
        lines = lines.push(info_line(p, "Detected BDMV Folder:", path, true));

        let features = self.flow.disc_features();
        if !features.is_empty() {
            lines = lines.push(info_line(p, "Detected Features:", features.join(", "), false));
        }

        if let Some(size) = self.flow.disc_size() {
            let value = format!(
                "{} bytes ({})",
                report::text::group(u128::from(size)),
                format_file_size(size)
            );
            lines = lines.push(info_line(p, "Disc Size:", value, true));
        }

        if self.flow.show_hidden_note() {
            lines = lines.push(text(HIDDEN_STREAMS_NOTE).size(ui::TEXT_XS).color(p.text_muted));
        }

        // The indicator takes the slack the strip already has to the right of
        // its lines, so it costs no layout: the lines keep the full width they
        // draw in (each is `Shrink`-wide and never wraps, so a narrower column
        // does not move them), and the badge is one `TEXT_XS` line tall — the
        // height of the line it sits beside — so the strip does not grow. A
        // disc with the badge and the same disc without it are pixel-identical
        // everywhere but the badge itself.
        let mut strip = Row::new().width(Length::Fill).align_y(Vertical::Top).spacing(ui::GAP_3);
        strip = strip.push(lines);
        if self.flow.disc_encrypted() {
            strip = strip.push(encrypted_badge(p));
        }

        container(strip)
            .width(Length::Fill)
            .padding([ui::GAP_2, ui::GAP_3])
            .style(ui::info_strip(p))
            .into()
    }

    /// The full report view (over the panes): a header with Back / Save / Copy,
    /// a "completed with errors" banner when any failures were recorded, and the
    /// scrollable monospace report in its readout panel.
    fn report_view(&self, p: Palette) -> Element<'_, Message> {
        let label = self.flow.label().unwrap_or("");
        let header = Row::new()
            .width(Length::Fill)
            .align_y(Vertical::Center)
            .spacing(ui::GAP_2)
            .push(
                button(text("← Back").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                    .padding([ui::GAP_2, ui::GAP_3])
                    .style(ui::secondary_button(p))
                    .on_press(Message::HideReport),
            )
            .push(
                text(format!("Report — {label}"))
                    .size(ui::TEXT_MD)
                    .font(ui::UI_SEMIBOLD)
                    .color(p.text)
                    // The volume label is disc-controlled (CJK-able).
                    .shaping(text::Shaping::Advanced),
            )
            .push(Space::new().width(Length::Fill))
            .push(self.command_button(p, Command::SaveReport))
            .push(self.command_button(p, Command::CopyReport));

        let mut column = Column::new().width(Length::Fill).height(Length::Fill).spacing(ui::GAP_2);
        column = column.push(header);
        let errors = self.flow.report_errors();
        if !errors.is_empty() {
            column = column.push(banner(
                p,
                p.warning,
                &format!(
                    "Completed with {} error(s) — the report below was still written.",
                    errors.len()
                ),
            ));
        }
        if let Some(report) = self.flow.report() {
            // Monospace, no wrapping — long report lines extend past the pane and
            // scroll horizontally, exactly like BDInfo's read-only text box.
            // Advanced shaping because the report embeds the disc label and
            // title (CJK-able); measured against Basic on a feature-disc-sized
            // (270 KB) report under the debug software renderer, it adds ~20 ms
            // to a relayout (~6% of a full render pass), paid once per
            // open/resize — iced caches the shaped paragraph between frames.
            let body = scrollable(
                container(
                    text(report)
                        .font(ui::MONO)
                        .size(ui::TEXT_SM)
                        .color(p.text)
                        .wrapping(text::Wrapping::None)
                        .shaping(text::Shaping::Advanced),
                )
                .padding(ui::GAP_3),
            )
            .direction(scrollable::Direction::Both {
                vertical: scrollable::Scrollbar::new(),
                horizontal: scrollable::Scrollbar::new(),
            })
            .width(Length::Fill)
            .height(Length::Fill);
            column = column.push(
                container(body).width(Length::Fill).height(Length::Fill).style(ui::report_pane(p)),
            );
        }
        column.into()
    }

    /// The bottom bar: the live progress + elapsed/remaining readout, and the
    /// stage's actions (Scan selected / Cancel, plus Save / Copy when reported).
    fn bottom_bar(&self, p: Palette) -> Element<'_, Message> {
        let scanning = self.flow.stage() == Stage::Scanning;
        let progress = self.flow.progress_view();
        let fraction = match self.flow.stage() {
            Stage::Scanning => progress.map_or(0.0, ProgressModel::fraction),
            Stage::Reported => 1.0,
            // The listing has no measurable % until the codec pass's first
            // event, so the bar breathes, then tracks the pass.
            Stage::Listing => progress
                .map_or_else(|| progress::pulse_fraction(self.pulse), ProgressModel::fraction),
            _ => 0.0,
        };

        // The lead line: the drop affordance while a file is held over the
        // window (and a drop would be accepted), else a transient status, else
        // the current scan file, else a hint.
        let lead = if self.drop_hover && !self.is_busy() {
            "Drop to open the disc folder or .iso image.".to_owned()
        } else {
            self.status.clone().unwrap_or_else(|| match self.flow.stage() {
                // A stalled scan (no progress event past the threshold —
                // [`Flow::scan_stalled`]) says the drive is struggling with
                // the named file; classic BDInfo's bar just freezes silently.
                Stage::Scanning if self.flow.scan_stalled() => progress.map_or_else(
                    || "Starting scan…".to_owned(),
                    |pr| format!("Still reading {}…", pr.file()),
                ),
                Stage::Scanning => progress.map_or_else(
                    || "Starting scan…".to_owned(),
                    |pr| format!("Scanning {}…", pr.file()),
                ),
                Stage::Listing => progress.map_or_else(
                    || "Scanning disc…".to_owned(),
                    |pr| format!("Reading {}…", pr.file()),
                ),
                Stage::Reported => "Report ready.".to_owned(),
                Stage::Listed if self.flow.selected_count() > 0 => "Ready to scan.".to_owned(),
                // Nothing checked still scans — the whole disc, like BDInfo.
                Stage::Listed => "Ready to scan the whole disc.".to_owned(),
                _ => String::new(),
            })
        };

        let times = progress.map_or_else(String::new, |pr| {
            format!("Elapsed {}   ·   Remaining {}", pr.elapsed_hms(), pr.remaining_hms())
        });

        let top = Row::new()
            .width(Length::Fill)
            .spacing(ui::GAP_3)
            .push(
                // The lead line embeds disc-derived paths ("Report saved
                // to: …", "Copied: …" — CJK-able).
                text(lead)
                    .size(ui::TEXT_SM)
                    .color(p.text_muted)
                    .width(Length::Fill)
                    .shaping(text::Shaping::Advanced),
            )
            .push(text(times).size(ui::TEXT_SM).font(ui::MONO).color(p.text_muted));

        let bar = progress_bar(0.0..=1.0, fraction)
            .length(Length::Fill)
            .girth(Length::Fixed(8.0))
            .style(ui::progress(p));

        // `BDInfo`'s bottom row, right-aligned: Scan Bitrates + View Report...
        // (its order), then Settings... set apart on the far right. While a
        // scan runs the pair swaps to Cancel in place; Settings... stays put,
        // disabled ([`Self::command_enabled`]) — the row never jumps between
        // states, and a not-yet-available report shows a disabled button, not
        // a hole (`BDInfo` greys these too).
        let scan_actions: Element<'_, Message> = if scanning {
            button(text("Cancel").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                .padding([ui::GAP_2, ui::GAP_4])
                .style(ui::secondary_button(p))
                .on_press(Message::Cancel)
                .into()
        } else {
            let scan = button(text("Scan Bitrates").size(ui::TEXT_SM).font(ui::UI_SEMIBOLD))
                .padding([ui::GAP_2, ui::GAP_4])
                .style(ui::primary_button(p))
                .on_press_maybe(self.flow.can_scan().then_some(Message::ScanSelected));
            // An encrypted disc leaves the button in place and dead — the same
            // greyed control every other unavailable action shows — and says
            // why on hover rather than in a dialog the user has to dismiss.
            let scan: Element<'_, Message> = if self.flow.disc_encrypted() {
                tooltip(scan, encrypted_bubble(p), tooltip::Position::Top).into()
            } else {
                scan.into()
            };
            Row::new()
                .spacing(ui::GAP_2)
                .push(scan)
                .push(
                    button(text("View Report...").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                        .padding([ui::GAP_2, ui::GAP_4])
                        .style(ui::secondary_button(p))
                        .on_press_maybe(
                            self.flow.report_available().then_some(Message::ShowReport),
                        ),
                )
                .into()
        };
        let actions = Row::new()
            .width(Length::Fill)
            .push(Space::new().width(Length::Fill))
            .push(scan_actions)
            .push(Space::new().width(Length::Fixed(ui::GAP_6)))
            .push(
                button(text(Command::Settings.label()).size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                    .padding([ui::GAP_2, ui::GAP_4])
                    .style(ui::secondary_button(p))
                    .on_press_maybe(
                        self.command_enabled(Command::Settings)
                            .then(|| Command::Settings.message()),
                    ),
            );

        let content =
            Column::new().width(Length::Fill).spacing(ui::GAP_2).push(top).push(bar).push(actions);
        container(content)
            .width(Length::Fill)
            .padding([ui::GAP_3, ui::GAP_4])
            .style(ui::bottom_bar(p))
            .into()
    }
}

/// The encrypted-disc indicator in the info box: a warning sign and the
/// condition, spelled out rather than left to the glyph alone — the glyph is
/// what catches the eye, the words are what make it findable.
///
/// U+26A0 is in the bundled Inter face, so it draws under the default Basic
/// shaping like the rest of the fixed chrome.
const ENCRYPTED_BADGE: &str = "⚠ AACS-encrypted";

/// Why the encrypted-disc indicator is up and the measured scan is dead — the
/// hover text on both of them, so the two read as one statement wherever the
/// user meets it first.
const ENCRYPTED_REASON: &str = "This disc is AACS-encrypted — stream data cannot be analyzed";

// ── stateless widget builders ────────────────────────────────────────────────

/// The top toolbar: the brand mark + wordmark on the left — the quiet brand
/// band (the theme control lives in the Settings dialog).
fn toolbar<'a>(p: Palette) -> Element<'a, Message> {
    let bar = Row::new()
        .width(Length::Fill)
        .align_y(Vertical::Center)
        .spacing(ui::GAP_2)
        .push(brand_mark(p))
        .push(wordmark(p));
    container(bar).width(Length::Fill).padding([ui::GAP_2, ui::GAP_4]).style(ui::toolbar(p)).into()
}

/// The toolbar brand "aperture" mark — a ring with a centre dot, mirroring the
/// app icon, composed from styled widgets so it renders identically on every
/// backend.
fn brand_mark<'a>(p: Palette) -> Element<'a, Message> {
    aperture(p, 22.0)
}

/// The larger empty-state brand mark.
fn brand_mark_large<'a>(p: Palette) -> Element<'a, Message> {
    aperture(p, 56.0)
}

/// Builds the ring-and-dot aperture at `size` — the in-app echo of the window
/// icon, its stroke and dot derived from the icon's ratio constants so every
/// rendering of the brand shares one geometry (the widget border draws inward
/// from the box edge, so the ring occupies the icon's outer-to-inner band).
fn aperture<'a>(p: Palette, size: f32) -> Element<'a, Message> {
    let ring = (icon::RING_OUTER - icon::RING_INNER) * size;
    let dot = 2.0 * icon::DOT_R * size;
    let center = container(Space::new())
        .width(Length::Fixed(dot))
        .height(Length::Fixed(dot))
        .style(move |_| iced::widget::container::Style {
            background: Some(p.accent.into()),
            border: iced::border::rounded(dot / 2.0),
            ..iced::widget::container::Style::default()
        });
    container(center)
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .center_x(Length::Fixed(size))
        .center_y(Length::Fixed(size))
        .style(move |_| iced::widget::container::Style {
            border: iced::border::rounded(size / 2.0).color(p.accent).width(ring),
            ..iced::widget::container::Style::default()
        })
        .into()
}

/// The "bdinfo-rs" wordmark — "bdinfo" in the text colour, "-rs" in the accent.
fn wordmark<'a>(p: Palette) -> Element<'a, Message> {
    Row::new()
        .push(text("bdinfo").size(ui::TEXT_MD).font(ui::UI_SEMIBOLD).color(p.text))
        .push(text("-rs").size(ui::TEXT_MD).font(ui::UI_SEMIBOLD).color(p.accent))
        .into()
}

/// A small solid status dot in `color` (the leading mark on a banner / error).
fn status_dot<'a>(color: iced::Color) -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fixed(10.0))
        .height(Length::Fixed(10.0))
        .style(ui::dot(color))
        .into()
}

/// A non-blocking banner tinted by `kind`, leading with a status dot.
fn banner<'a>(p: Palette, kind: iced::Color, message: &str) -> Element<'a, Message> {
    let line = Row::new()
        .align_y(Vertical::Center)
        .spacing(ui::GAP_2)
        .push(status_dot(kind))
        // A banner's message can embed disc-derived names/paths (CJK-able).
        .push(
            text(message.to_owned())
                .size(ui::TEXT_SM)
                .color(p.text)
                .shaping(text::Shaping::Advanced),
        );
    container(line)
        .width(Length::Fill)
        .padding([ui::GAP_2, ui::GAP_3])
        .style(ui::banner(p, kind))
        .into()
}

/// One header cell: a muted label in a fixed-width, aligned box. A sortable
/// cell passes `hovered` while the cursor is over it, lifting it with the same
/// wash the body rows use (the sort-click affordance).
fn header_cell<'a>(
    p: Palette,
    label: String,
    width: Length,
    align: Horizontal,
    hovered: bool,
) -> Element<'a, Message> {
    container(
        text(label)
            .size(ui::TEXT_XS)
            .font(ui::UI_MEDIUM)
            .color(p.text_muted)
            // Clip a long label (e.g. "Estimated Size") at the column edge, like
            // every other cell — never wrap it to a second line.
            .wrapping(text::Wrapping::None),
    )
    .width(width)
    // So every column's label sits on the same baseline whether or not it
    // carries a resize grab.
    .height(Length::Fill)
    .align_x(align)
    .align_y(Vertical::Center)
    .clip(true)
    .padding([0.0, ui::GAP_2])
    .style(ui::header_surface(p, hovered))
    .into()
}

/// A header cell (weight `width`) with an optional resize grab on its right edge
/// and an optional sort action. The grab is a `Stack` overlay wrapped in a
/// `container(width)` — the
/// `FillPortion` lives on the container, exactly like a body cell, so it stays aligned
/// with the rows. It shows a visible 1px separator inside a wide hit zone and a
/// horizontal-resize cursor. Pressing it only *starts* the drag (carrying the
/// column span `cols_width`); a global mouse subscription then tracks the cursor
/// even as it leaves the zone, so the drag does not stop after a few pixels.
///
/// When `sort` names this column's [`SortColumn`], the label area becomes a
/// click target dispatching that sort (the grab strip still wins on the right
/// edge), the active sort column carries an ↑/↓ marker after its label, and
/// the cell lifts with the row-hover wash while the cursor is over it.
fn header_col<'a>(
    p: Palette,
    label: &str,
    width: Length,
    align: Horizontal,
    grab: Option<(ColGrid, usize, f32)>,
    sort: Option<(SortColumn, Option<Sort>, bool)>,
) -> Element<'a, Message> {
    let title = match sort {
        Some((column, Some(active), _)) if active.column == column => {
            format!("{label} {}", if active.ascending { "↑" } else { "↓" })
        }
        _ => label.to_owned(),
    };
    // The (possibly clickable) label cell at `w` — built twice below, once per
    // layout (plain vs grab-overlaid).
    let cell = |w: Length| -> Element<'a, Message> {
        match sort {
            Some((column, _, hovered)) => {
                mouse_area(header_cell(p, title.clone(), w, align, hovered))
                    .interaction(iced::mouse::Interaction::Pointer)
                    .on_press(Message::SortBy(column))
                    .on_enter(Message::HeaderHovered(column))
                    .on_exit(Message::HeaderUnhovered(column))
                    .into()
            }
            None => header_cell(p, title.clone(), w, align, false),
        }
    };
    let Some((grid, boundary, cols_width)) = grab else {
        return cell(width);
    };
    // A visible 1px line at the right edge (the boundary), inside a wide grab zone.
    let separator = Row::new()
        .width(Length::Fixed(GRAB_W))
        .height(Length::Fill)
        .push(Space::new().width(Length::Fill))
        .push(
            container(Space::new())
                .width(Length::Fixed(1.0))
                .height(Length::Fill)
                .style(ui::divider(p.line_strong)),
        );
    let handle = mouse_area(separator)
        .interaction(iced::mouse::Interaction::ResizingHorizontally)
        .on_press(Message::ColDragStart(grid, boundary, cols_width));
    let overlay = Row::new()
        .width(Length::Fill)
        .height(Length::Fill)
        .push(Space::new().width(Length::Fill))
        .push(handle);
    container(
        Stack::new()
            .width(Length::Fill)
            .height(Length::Fill)
            .push(cell(Length::Fill))
            .push(overlay),
    )
    .width(width)
    .height(Length::Fill)
    .into()
}

/// Builds a resizable table header: a leading indent, one [`header_col`] per
/// column (each with a right-edge drag grab, except the last), and the trailing
/// scrollbar gutter — wrapped in `responsive` so the grabs know the column span.
/// `weights` are the live column weights for `grid`. `sort_cols`, when given,
/// makes every column a sort click target: the per-column [`SortColumn`] list
/// (parallel to `cols`), the active sort for the direction marker, and the
/// hovered header column for the hover wash.
fn resizable_header<'a>(
    p: Palette,
    cols: &'static [Col],
    grid: ColGrid,
    weights: Vec<f32>,
    sort_cols: Option<(&'static [SortColumn], Option<Sort>, Option<SortColumn>)>,
) -> Element<'a, Message> {
    responsive(move |size| {
        let cols_width = (size.width - ui::ROW_INDENT - ui::SCROLLBAR_GUTTER).max(1.0);
        let last = cols.len().saturating_sub(1);
        let mut row = Row::new()
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Vertical::Center)
            .push(Space::new().width(Length::Fixed(ui::ROW_INDENT)));
        for (i, col) in cols.iter().enumerate() {
            let weight = weights.get(i).copied().unwrap_or(1.0);
            let grab = (i != last).then_some((grid, i, cols_width));
            let sort = sort_cols.and_then(|(columns, active, hovered)| {
                columns.get(i).map(|&column| (column, active, hovered == Some(column)))
            });
            row = row.push(header_col(p, col.label, fill(weight), col.align, grab, sort));
        }
        row.push(Space::new().width(Length::Fixed(ui::SCROLLBAR_GUTTER))).into()
    })
    .into()
}

/// One playlist row: a leading accent bar (when active), then **two sibling
/// buttons** — the scan checkbox (toggles the row's selection) and the cells
/// (click to make the row active, driving the master-detail panes). The click
/// targets are background-free ([`ui::flat_button`]); the whole row sits on one
/// [`ui::row_surface`] container wrapped in a `mouse_area`, so hover lifts the
/// **entire** row as a single band (no checkbox-vs-cells seam) and the **active**
/// row carries the amber wash. `hovered` is whether the cursor is over this row.
fn table_row<'a>(
    p: Palette,
    row_data: &SelectableRow,
    editable: bool,
    hovered: bool,
    weights: &[f32],
) -> Element<'a, Message> {
    let w = |i: usize| fill(weights.get(i).copied().unwrap_or(1.0));
    let even = row_data.index.checked_rem(2) == Some(0);
    let index = row_data.index;
    let active = row_data.active;

    let checkbox = button(
        container(check_chip(p, row_data.selected)).center_x(Length::Fill).center_y(Length::Fill),
    )
    .width(Length::Fixed(ui::COL_CHECK))
    .height(Length::Fill)
    .padding(0.0)
    .style(ui::flat_button(p))
    .on_press_maybe(editable.then_some(Message::RowToggled(index)));

    let mut file = Row::new().push(
        text(row_data.cells.file.clone())
            .size(ui::TEXT_SM)
            .font(ui::MONO)
            .color(p.text)
            // Clip a long name (e.g. `00002.MPLS [12 Chapters]`) at the column
            // edge when the window narrows, rather than wrapping to a new line.
            .wrapping(text::Wrapping::None),
    );
    if row_data.has_hidden {
        file = file.push(text(" *").size(ui::TEXT_SM).font(ui::MONO).color(p.accent));
    }
    let cells = Row::new()
        .width(Length::Fill)
        .align_y(Vertical::Center)
        .push(
            container(file)
                .width(w(0))
                .clip(true)
                .align_x(Horizontal::Left)
                .padding([0.0, ui::GAP_2]),
        )
        .push(num_cell(p, row_data.cells.group.clone(), w(1), Horizontal::Center))
        .push(num_cell(p, row_data.cells.length.clone(), w(2), Horizontal::Right))
        .push(num_cell(p, row_data.cells.estimated_bytes.clone(), w(3), Horizontal::Right))
        .push(num_cell(p, row_data.cells.measured_bytes.clone(), w(4), Horizontal::Right))
        .push(Space::new().width(Length::Fixed(ui::SCROLLBAR_GUTTER)));
    // Centre the cells vertically in the fixed-height row (the button does not
    // centre its content on its own, so the text would otherwise sit at the top).
    let cells_button =
        button(container(cells).width(Length::Fill).height(Length::Fill).align_y(Vertical::Center))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(0.0)
            .style(ui::flat_button(p))
            .on_press_maybe(editable.then_some(Message::RowActivated(index)));

    let row = Row::new()
        .width(Length::Fill)
        .height(Length::Fixed(ui::ROW_H))
        .align_y(Vertical::Center)
        .push(
            container(Space::new())
                .width(Length::Fixed(ui::SEL_BAR))
                .height(Length::Fill)
                .style(ui::sel_bar(p, active)),
        )
        .push(checkbox)
        .push(cells_button);
    let framed = container(row)
        .width(Length::Fill)
        .height(Length::Fixed(ui::ROW_H))
        .style(ui::row_surface(p, active, hovered, even));
    mouse_area(framed)
        .on_enter(Message::RowHovered(Region::Playlist, index))
        .on_exit(Message::RowUnhovered(Region::Playlist, index))
        .into()
}

/// The selection chip: an amber square with a check when checked, an outlined box
/// when not.
fn check_chip<'a>(p: Palette, checked: bool) -> Element<'a, Message> {
    let inner: Element<'a, Message> = if checked {
        text("✓").size(11.0).font(ui::UI_SEMIBOLD).color(p.on_accent).into()
    } else {
        Space::new().into()
    };
    container(inner)
        .width(Length::Fixed(16.0))
        .height(Length::Fixed(16.0))
        .center_x(Length::Fixed(16.0))
        .center_y(Length::Fixed(16.0))
        .style(ui::check_box(p, checked))
        .into()
}

/// A pane's title-bar label — a small muted section heading (Stream File /
/// Codec) with a little breathing room, matching the playlist select bar's line.
fn pane_title<'a>(p: Palette, title: &str) -> Element<'a, Message> {
    container(text(title.to_owned()).size(ui::TEXT_SM).font(ui::UI_MEDIUM).color(p.text_muted))
        .padding([ui::GAP_1, 0.0])
        .into()
}

/// One read-only data column — its header label, alignment, and whether its
/// cells render in the tabular monospace. The **width** is not fixed here: it
/// comes from the live, user-resizable weights (see [`App::playlist_w`] /
/// [`App::stream_w`] / [`App::codec_w`]).
struct Col {
    /// The header label.
    label: &'static str,
    /// The horizontal alignment of the label + cells.
    align: Horizontal,
    /// Whether to render the cells in the monospace face.
    mono: bool,
}

/// The Playlist columns. The checkbox is a separate leading column, so these
/// are the five data columns only.
const PLAYLIST_COLS: &[Col] = &[
    Col { label: "Playlist File", align: Horizontal::Left, mono: true },
    Col { label: "Group", align: Horizontal::Center, mono: true },
    Col { label: "Length", align: Horizontal::Right, mono: true },
    Col { label: "Estimated Size", align: Horizontal::Right, mono: true },
    Col { label: "Measured Size", align: Horizontal::Right, mono: true },
];

/// The sort key behind each Playlist column, parallel to [`PLAYLIST_COLS`] —
/// what a click on that header sorts by.
const PLAYLIST_SORT_COLS: &[SortColumn] = &[
    SortColumn::File,
    SortColumn::Group,
    SortColumn::Length,
    SortColumn::Estimated,
    SortColumn::Measured,
];

/// The "Stream File" pane columns — its own grid, resized independently of the
/// playlist's (they only *open* on the same proportions).
const STREAM_FILE_COLS: &[Col] = &[
    Col { label: "Stream File", align: Horizontal::Left, mono: true },
    Col { label: "Index", align: Horizontal::Center, mono: true },
    Col { label: "Length", align: Horizontal::Right, mono: true },
    Col { label: "Estimated Size", align: Horizontal::Right, mono: true },
    Col { label: "Measured Size", align: Horizontal::Right, mono: true },
];

/// The "Codec" pane columns — its own grid.
const CODEC_COLS: &[Col] = &[
    Col { label: "Codec", align: Horizontal::Left, mono: false },
    Col { label: "Language", align: Horizontal::Left, mono: false },
    Col { label: "Bit Rate", align: Horizontal::Right, mono: true },
    Col { label: "Description", align: Horizontal::Left, mono: false },
];

/// A data table: a quiet header over rows that hover and select as one band.
/// Each `rows` entry supplies one cell string per column (extra cells are
/// ignored); an empty table shows a single dash. Every row is wrapped in a
/// `mouse_area`, so it lifts under the cursor (`hovered`), takes the selection
/// wash when clicked (`selected`), and reports hover / click for `region`.
///
/// `weights` are the live, user-resizable column weights for `grid`; the header
/// carries the drag grabs. Every column starts on the shared grid line (a fixed
/// [`ui::ROW_INDENT`] leading, where the playlist's checkbox sits).
fn data_table<'a>(
    p: Palette,
    cols: &'static [Col],
    weights: &[f32],
    grid: ColGrid,
    rows: Vec<Vec<String>>,
    region: Region,
    hovered: Option<usize>,
    selected: Option<usize>,
) -> Element<'a, Message> {
    // No sort columns: like `BDInfo`, only the playlist list sorts on a header
    // click — its Stream File list has no ColumnClick handler.
    let header = container(resizable_header(p, cols, grid, weights.to_vec(), None))
        .width(Length::Fill)
        .height(Length::Fixed(ui::HEADER_H))
        .style(ui::table_header(p));
    let rule = container(Space::new())
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(ui::divider(p.line_strong));

    let mut body_col = Column::new().width(Length::Fill);
    if rows.is_empty() {
        body_col = body_col
            .push(container(text("—").size(ui::TEXT_SM).color(p.text_faint)).padding(ui::GAP_3));
    }
    for (i, row) in rows.into_iter().enumerate() {
        let even = i.checked_rem(2) == Some(0);
        let mut cells = Row::new()
            .width(Length::Fill)
            .align_y(Vertical::Center)
            .push(Space::new().width(Length::Fixed(ui::ROW_INDENT)));
        for (col_index, (col, value)) in cols.iter().zip(row).enumerate() {
            let font = if col.mono { ui::MONO } else { ui::UI };
            cells = cells.push(
                container(
                    text(value)
                        .size(ui::TEXT_SM)
                        .font(font)
                        .color(p.text)
                        .wrapping(text::Wrapping::None),
                )
                .width(fill(weights.get(col_index).copied().unwrap_or(1.0)))
                .clip(true)
                .align_x(col.align)
                .padding([0.0, ui::GAP_2]),
            );
        }
        // The trailing gutter matches the header, keeping the last value clear of
        // the scrollbar.
        cells = cells.push(Space::new().width(Length::Fixed(ui::SCROLLBAR_GUTTER)));
        let framed = container(cells)
            .width(Length::Fill)
            .padding([ui::GAP_2, 0.0])
            .style(ui::row_surface(p, selected == Some(i), hovered == Some(i), even));
        body_col = body_col.push(
            mouse_area(framed)
                .on_enter(Message::RowHovered(region, i))
                .on_exit(Message::RowUnhovered(region, i))
                .on_press(Message::PaneRowPressed(region, i)),
        );
    }
    let body = scrollable(body_col).width(Length::Fill).height(Length::Fill);
    container(
        Column::new().width(Length::Fill).height(Length::Fill).push(header).push(rule).push(body),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(ui::table_frame(p))
    .into()
}

/// Overlays `card` (centred over a dim scrim) on `base` — a dismissable modal
/// (the scan-complete notice, the Settings dialog). Clicking the scrim
/// dispatches `on_scrim` — dismiss for the notice, Cancel for Settings; the
/// card itself is `opaque`, so a click inside it never counts as a scrim
/// click (a stray click must not discard half-edited settings).
fn modal<'a>(
    base: Element<'a, Message>,
    card: Element<'a, Message>,
    on_scrim: Message,
) -> Element<'a, Message> {
    let scrim =
        mouse_area(container(iced::widget::opaque(card)).center(Length::Fill).style(ui::scrim()))
            .on_press(on_scrim);
    Stack::new().push(base).push(scrim).into()
}

/// Overlays `card` on `base` over a dim scrim, **without** a dismiss — the
/// "please wait" overlay shown while the initial scan runs (it clears itself when
/// the scan completes).
fn scanning_modal<'a>(
    base: Element<'a, Message>,
    card: Element<'a, Message>,
) -> Element<'a, Message> {
    let scrim = container(card).center(Length::Fill).style(ui::scrim());
    Stack::new().push(base).push(scrim).into()
}

/// The "please wait" scan overlay card: the brand mark over the same message
/// `BDInfo` shows while it reads a disc — plus what `BDInfo`'s modal lacks
/// and a damaged disc needs: the codec pass's live progress (the bar breathes
/// indeterminately until its first event) and a Cancel that works, because
/// this pre-scan pass reads every stream file's head and can stall on a bad
/// sector exactly like the measured scan.
fn scanning_card<'a>(
    p: Palette,
    pulse: u16,
    progress: Option<&ProgressModel>,
) -> Element<'a, Message> {
    let fraction =
        progress.map_or_else(|| progress::pulse_fraction(pulse), ProgressModel::fraction);
    let mut card = Column::new()
        .align_x(Horizontal::Center)
        .spacing(ui::GAP_4)
        .push(brand_mark_large(p))
        .push(
            text("Please wait while we scan the disc...")
                .size(ui::TEXT_MD)
                .color(p.text)
                .align_x(Horizontal::Center),
        )
        .push(
            progress_bar(0.0..=1.0, fraction)
                .length(Length::Fixed(320.0))
                .girth(Length::Fixed(8.0))
                .style(ui::progress(p)),
        );
    if let Some(pr) = progress {
        card = card.push(
            text(format!("Reading {}…", pr.file()))
                .size(ui::TEXT_SM)
                .color(p.text_muted)
                .align_x(Horizontal::Center),
        );
    }
    card = card.push(
        button(text("Cancel").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
            .padding([ui::GAP_2, ui::GAP_4])
            .style(ui::secondary_button(p))
            .on_press(Message::Cancel),
    );
    container(card).padding(ui::GAP_6).max_width(440.0).style(ui::hero_card(p)).into()
}

/// The Settings dialog card — `BDInfo`'s `FormSettings`, narrowed to what is
/// safely in the GUI's own hands: the two playlist filters (+ the seconds
/// threshold), the two display toggles, the two report-section toggles,
/// partial-scan retention, and autosave, with OK / Cancel (Cancel discards
/// the draft). The wording mirrors the original's checkboxes, except the
/// retention line, which `BDInfo` has no setting for.
fn settings_card<'a>(p: Palette, draft: &settings::Draft) -> Element<'a, Message> {
    let seconds_field = text_input("20", &draft.seconds_text)
        .on_input(Message::SettingsSeconds)
        .width(Length::Fixed(72.0))
        .padding([ui::GAP_1, ui::GAP_2])
        .size(ui::TEXT_SM)
        .font(ui::MONO)
        .style(ui::text_field(p));
    let short_row = Row::new()
        .align_y(Vertical::Center)
        .spacing(ui::GAP_2)
        .push(setting_check(
            p,
            "Filter short playlists under",
            draft.filter_short_playlists,
            SettingsField::FilterShort,
        ))
        .push(seconds_field)
        .push(text("seconds").size(ui::TEXT_SM).color(p.text_muted));

    // The appearance group, set off from BDInfo's scan settings above by a
    // hairline: the Auto / Light / Dark picker, previewed live off the draft.
    let rule = container(Space::new())
        .width(Length::Fill)
        .height(Length::Fixed(1.0))
        .style(ui::divider(p.line));
    let theme_row = Row::new()
        .align_y(Vertical::Center)
        .spacing(ui::GAP_1)
        .push(text("Theme").size(ui::TEXT_SM).color(p.text))
        .push(Space::new().width(Length::Fill))
        .push(theme_chip(p, settings::ThemeChoice::System, draft.theme))
        .push(theme_chip(p, settings::ThemeChoice::Light, draft.theme))
        .push(theme_chip(p, settings::ThemeChoice::Dark, draft.theme));

    // The UI-scale picker, chips like the theme's (previewed live off the
    // draft the same way). Slimmer padding — six chips share the row.
    let scale_row = settings::UI_SCALE_PRESETS.iter().fold(
        Row::new()
            .align_y(Vertical::Center)
            .spacing(ui::GAP_1)
            .push(text("UI scale").size(ui::TEXT_SM).color(p.text))
            .push(Space::new().width(Length::Fill)),
        |row, &preset| row.push(scale_chip(p, preset, draft.ui_scale_percent)),
    );

    let actions = Row::new()
        .width(Length::Fill)
        .spacing(ui::GAP_2)
        .push(Space::new().width(Length::Fill))
        .push(
            button(text("Cancel").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                .padding([ui::GAP_2, ui::GAP_4])
                .style(ui::secondary_button(p))
                .on_press(Message::SettingsCancel),
        )
        .push(
            button(text("OK").size(ui::TEXT_SM).font(ui::UI_SEMIBOLD))
                .padding([ui::GAP_2, ui::GAP_6])
                .style(ui::primary_button(p))
                .on_press(Message::SettingsOk),
        );

    let card = Column::new()
        .width(Length::Fixed(420.0))
        .spacing(ui::GAP_3)
        .push(text("Settings").size(ui::TEXT_MD).font(ui::UI_SEMIBOLD).color(p.text))
        .push(short_row)
        .push(setting_check(
            p,
            "Filter playlists that contain loops",
            draft.filter_looping_playlists,
            SettingsField::FilterLoops,
        ))
        .push(setting_check(
            p,
            "Stream sizes in human readable format",
            draft.human_readable_sizes,
            SettingsField::HumanSizes,
        ))
        .push(setting_check(
            p,
            "Display chapter count in Playlist view",
            draft.display_chapter_count,
            SettingsField::ChapterCount,
        ))
        .push(setting_check(
            p,
            "Include stream diagnostics in report",
            draft.report_stream_diagnostics,
            SettingsField::StreamDiagnostics,
        ))
        .push(setting_check(
            p,
            "Include quick text summary in report",
            draft.report_quick_summary,
            SettingsField::QuickSummary,
        ))
        .push(setting_check(
            p,
            "Keep partial data from failed stream reads",
            draft.keep_partial,
            SettingsField::KeepPartial,
        ))
        .push(setting_check(
            p,
            "Auto-save report on scan completion",
            draft.autosave_report,
            SettingsField::Autosave,
        ))
        .push(rule)
        .push(theme_row)
        .push(scale_row)
        .push(actions);
    container(card).padding(ui::GAP_6).style(ui::hero_card(p)).into()
}

/// One chip of the Settings dialog's Auto / Light / Dark theme picker — the
/// picked choice takes the filled-accent (selection) look, the others stay
/// quiet outlines. A click lands on the draft, so the pick previews live.
fn theme_chip<'a>(
    p: Palette,
    choice: settings::ThemeChoice,
    picked: settings::ThemeChoice,
) -> Element<'a, Message> {
    let label = ThemePref::from_choice(choice).label();
    let chip =
        button(text(label).size(ui::TEXT_SM).font(ui::UI_MEDIUM)).padding([ui::GAP_1, ui::GAP_3]);
    let chip = if choice == picked {
        chip.style(ui::primary_button(p))
    } else {
        chip.style(ui::secondary_button(p))
    };
    chip.on_press(Message::SettingsTheme(choice)).into()
}

/// One chip of the Settings dialog's UI-scale picker — the picked percent
/// takes the filled-accent look, the rest stay quiet outlines, exactly like
/// the theme chips above it. A click lands on the draft, so the scale
/// previews live (the whole window re-scales) and Cancel reverts it.
fn scale_chip<'a>(p: Palette, preset: u16, picked: u16) -> Element<'a, Message> {
    let chip = button(text(format!("{preset}%")).size(ui::TEXT_SM).font(ui::UI_MEDIUM))
        .padding([ui::GAP_1, ui::GAP_2]);
    let chip = if preset == picked {
        chip.style(ui::primary_button(p))
    } else {
        chip.style(ui::secondary_button(p))
    };
    chip.on_press(Message::SettingsScale(preset)).into()
}

/// One Settings checkbox line: the app's check chip + label, the whole line
/// one click target (like the table rows, no chip-vs-label seam).
fn setting_check<'a>(
    p: Palette,
    label: &str,
    checked: bool,
    field: SettingsField,
) -> Element<'a, Message> {
    let line = Row::new()
        .align_y(Vertical::Center)
        .spacing(ui::GAP_2)
        .push(check_chip(p, checked))
        .push(text(label.to_owned()).size(ui::TEXT_SM).color(p.text));
    mouse_area(line)
        .interaction(iced::mouse::Interaction::Pointer)
        .on_press(Message::SettingsToggled(field))
        .into()
}

/// The scan-complete card: the brand mark, the outcome text, and an OK button.
fn notice_card<'a>(p: Palette, notice: &str) -> Element<'a, Message> {
    let card = Column::new()
        .align_x(Horizontal::Center)
        .spacing(ui::GAP_4)
        .push(brand_mark_large(p))
        .push(text(notice.to_owned()).size(ui::TEXT_MD).color(p.text).align_x(Horizontal::Center))
        .push(
            button(text("OK").size(ui::TEXT_BASE).font(ui::UI_MEDIUM))
                .padding([ui::GAP_2, ui::GAP_6])
                .style(ui::primary_button(p))
                .on_press(Message::DismissNotice),
        );
    container(card).padding(ui::GAP_6).max_width(440.0).style(ui::hero_card(p)).into()
}

/// One "label: value" line in the disc-info box — a muted label followed by the
/// value (monospace when `mono`, e.g. the byte size; the UI face otherwise).
fn info_line<'a>(p: Palette, label: &str, value: String, mono: bool) -> Element<'a, Message> {
    let font = if mono { ui::MONO } else { ui::UI };
    Row::new()
        .width(Length::Fill)
        .align_y(Vertical::Center)
        .spacing(ui::GAP_2)
        .push(text(label.to_owned()).size(ui::TEXT_XS).font(ui::UI_MEDIUM).color(p.text_muted))
        .push(
            // The values are disc-derived (title, folder path — CJK-able);
            // the fixed labels stay on the default Basic shaping.
            text(value)
                .size(ui::TEXT_XS)
                .font(font)
                .color(p.text)
                .wrapping(text::Wrapping::None)
                .shaping(text::Shaping::Advanced),
        )
        .into()
}

/// The encrypted-disc indicator: [`ENCRYPTED_BADGE`] in the palette's warning
/// tone, explaining itself on hover.
///
/// The tone is a token, so it reads correctly in both palettes; the bubble
/// opens to the left because the badge sits at the right edge of the info
/// strip.
fn encrypted_badge<'a>(p: Palette) -> Element<'a, Message> {
    tooltip(
        text(ENCRYPTED_BADGE).size(ui::TEXT_XS).font(ui::UI_MEDIUM).color(p.warning),
        encrypted_bubble(p),
        tooltip::Position::Left,
    )
    .into()
}

/// The hover bubble both encrypted-disc controls carry — the indicator and the
/// dead "Scan Bitrates" button.
fn encrypted_bubble<'a>(p: Palette) -> Element<'a, Message> {
    container(text(ENCRYPTED_REASON).size(ui::TEXT_XS).color(p.text))
        .padding([ui::GAP_1, ui::GAP_2])
        .style(ui::tooltip(p))
        .into()
}

/// One right/centre-aligned numeric cell in the table's tabular monospace.
fn num_cell<'a>(
    p: Palette,
    value: String,
    width: Length,
    align: Horizontal,
) -> Element<'a, Message> {
    container(
        text(value).size(ui::TEXT_SM).font(ui::MONO).color(p.text).wrapping(text::Wrapping::None),
    )
    .width(width)
    .clip(true)
    .align_x(align)
    .padding([0.0, ui::GAP_2])
    .into()
}

/// Reads the closing window's live geometry (the outer position + logical
/// size, the same coordinates [`window_settings`] restores) plus its
/// maximized state, and forwards them to [`Message::SaveAndClose`]. The
/// window stays open while the readouts round-trip
/// (`exit_on_close_request` is off) — `SaveAndClose` then really closes it.
fn close_with_geometry(id: iced::window::Id) -> Task<Message> {
    iced::window::position(id).then(move |position| {
        iced::window::size(id).then(move |size| {
            iced::window::is_maximized(id).map(move |maximized| Message::SaveAndClose {
                id,
                position,
                size,
                maximized,
            })
        })
    })
}

/// Opens the native folder picker (rfd, pure-Rust xdg-portal backend on Linux)
/// and yields the chosen path, or `None` when cancelled.
async fn pick_folder() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Select a Blu-ray disc folder")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf())
}

/// Opens the native file picker filtered to `.iso` images and yields the chosen
/// path, or `None` when cancelled.
async fn pick_iso() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Select a Blu-ray .iso image")
        .add_filter("Disc image", &["iso"])
        .pick_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}

/// Test-only helpers shared by the `interaction` and `snapshots` suites: a
/// synthetic listed disc, an `iced_test` Simulator over the app's live
/// `view()` (bundled fonts loaded, so layout and pixels match the shipped
/// window), a click-and-dispatch driver, and the snapshot-hash assert.
#[cfg(test)]
mod harness {
    use std::path::PathBuf;

    use bdinfo_rs_core::bdrom::disc::{BdRom, PlaylistSummary, fixtures};
    use bdinfo_rs_gui::flow::Flow;
    use bdinfo_rs_gui::model::ViewSettings;
    use bdinfo_rs_gui::scan::{Input, Structural};
    use iced_test::Simulator;

    use super::{App, Message};

    /// A `PlaylistSummary` with a name, length, estimated size, and clips
    /// carrying a name only (what the panes read here).
    fn playlist(name: &str, length: f64, file_size: u64, clips: &[&str]) -> PlaylistSummary {
        PlaylistSummary {
            file_size,
            ..fixtures::playlist(name, length, fixtures::clips(clips, 0.0))
        }
    }

    /// The two-playlist synthetic disc the flow tests use: 00000 (100 s,
    /// est 1000 B) and 00001 (70 s, est 500 B), distinct clips.
    pub fn structural() -> Structural {
        Structural {
            bdrom: BdRom {
                volume_label: "DISC".to_owned(),
                disc_title: Some("A Movie".to_owned()),
                size: 78_000_000_000,
                interleaved_size: 0,
                is_3d: false,
                is_50hz: false,
                is_uhd: false,
                is_aacs_encrypted: false,
                is_bd_plus: false,
                is_bd_java: false,
                is_dbox: false,
                is_psp: false,
                playlists: vec![
                    playlist("00000.MPLS", 100.0, 1000, &["A.M2TS"]),
                    playlist("00001.MPLS", 70.0, 500, &["B.M2TS"]),
                ],
            },
            features: vec!["Ultra HD".to_owned()],
            warnings: Vec::new(),
        }
    }

    /// An app with the synthetic disc listed — the state most flows start
    /// from. Its config path is `None` (the `App::default`), so nothing any
    /// test does can ever write the user's real config.
    pub fn listed_app() -> App {
        let input = Input::Folder("disc".into());
        let flow = Flow::start_listing(input.clone()).listed(
            &input,
            Ok(structural()),
            ViewSettings::default(),
        );
        App { flow, ..App::default() }
    }

    /// An app listed from the same synthetic disc as [`listed_app`], flagged
    /// AACS-encrypted. The two differ in exactly that one flag, so anything
    /// that differs between their views is the encrypted-disc treatment.
    pub fn encrypted_app() -> App {
        let mut structural = structural();
        structural.bdrom.is_aacs_encrypted = true;
        let input = Input::Folder("disc".into());
        let flow = Flow::start_listing(input.clone()).listed(
            &input,
            Ok(structural),
            ViewSettings::default(),
        );
        App { flow, ..App::default() }
    }

    /// An app listed from a disc the default filters withhold two playlists
    /// from — one per rule: 00002 (80 s) loops and 00003 (5 s) is short,
    /// alongside the listed 00000 (100 s) and 00001 (70 s). The state the
    /// hidden-count line under the table renders in.
    pub fn filtered_app() -> App {
        let mut looping = playlist("00002.MPLS", 80.0, 800, &["C.M2TS"]);
        looping.has_loops = true;
        let mut structural = structural();
        structural.bdrom.playlists.push(looping);
        structural.bdrom.playlists.push(playlist("00003.MPLS", 5.0, 100, &["D.M2TS"]));
        let input = Input::Folder("disc".into());
        let flow = Flow::start_listing(input.clone()).listed(
            &input,
            Ok(structural),
            ViewSettings::default(),
        );
        App { flow, ..App::default() }
    }

    /// The simulator settings: the bundled fonts, loaded exactly as `main`
    /// loads them, and the same default face.
    fn settings() -> iced::Settings {
        iced::Settings {
            fonts: vec![
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/assets/fonts/Inter-Variable.ttf"
                ))
                .as_slice()
                .into(),
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/assets/fonts/JetBrainsMono-Regular.ttf"
                ))
                .as_slice()
                .into(),
            ],
            default_font: crate::ui::UI,
            ..iced::Settings::default()
        }
    }

    /// A Simulator over the app's CURRENT `view()` (one frame; re-build it
    /// after every state change, as iced would re-view), at the app's real
    /// launch size — the Simulator's own default is a shorter landscape
    /// viewport that clips the portrait layout's lower rows.
    pub fn ui(app: &App) -> Simulator<'_, Message> {
        let (width, height) = crate::WINDOW_SIZE;
        Simulator::with_size(settings(), iced::Size::new(width, height), app.view())
    }

    /// Clicks the widget showing exactly `label` (the `&str` selector matches
    /// a text widget's whole content) and feeds every produced message
    /// through `update` — one real user interaction, end to end. Returns the
    /// messages for shape asserts.
    pub fn click(app: &mut App, label: &str) -> Vec<Message> {
        let messages: Vec<Message> = {
            let mut ui = ui(app);
            ui.click(label).expect("the click target is on screen");
            ui.into_messages().collect()
        };
        for message in messages.clone() {
            let _ = app.update(message);
        }
        messages
    }

    /// Whether the app's current view shows a text widget reading exactly
    /// `label`.
    pub fn sees(app: &App, label: &str) -> bool {
        ui(app).find(label).is_ok()
    }

    /// The laid-out rectangle of the text widget reading exactly `label` — the
    /// position and size the renderer would draw it at, so two views can be
    /// compared for layout rather than for content.
    pub fn bounds(app: &App, label: &str) -> iced::Rectangle {
        ui(app).find(label).expect("the label is on screen").bounds()
    }

    /// Renders the app's view offscreen and ties it to the committed SHA-256
    /// under `snapshots/<family>/` (created on first run — commit it). On a
    /// mismatch the rendered PNG is dumped under `target/snapshot-failures/`
    /// for eyes. Requires the deterministic software backend: without
    /// `ICED_TEST_BACKEND=tiny-skia` (the gate and CI set it) the tie is
    /// skipped with a note rather than pinning GPU-varying pixels.
    ///
    /// Two pin families, keyed by OS family: the first 6-arch CI run proved
    /// the render byte-identical across arches WITHIN a family (Windows
    /// x86-64 = arm64; Linux x86-64 = arm64 = macOS Intel = arm), but the
    /// bold text runs (brand, buttons, headings) rasterize differently
    /// between Windows and the unix family — cosmic-text's font handling is
    /// platform-conditional even with bundled fonts. Regular-weight text
    /// (the tables, the whole report body) is identical everywhere.
    pub fn assert_snapshot(app: &App, name: &str) {
        if std::env::var("ICED_TEST_BACKEND").as_deref() != Ok("tiny-skia") {
            eprintln!(
                "snapshot '{name}' skipped: set ICED_TEST_BACKEND=tiny-skia \
                 (scripts/gui-compliance.ps1 and gui.yml do)"
            );
            return;
        }
        let mut ui = ui(app);
        let snapshot = ui.snapshot(&app.theme()).expect("the snapshot renders");
        // With BDINFO_GUI_SNAPSHOT_DUMP set to a directory, every tie also
        // writes its rendered PNG there — how a pin is inspected by eye
        // (per OS/arch) without having to break it first.
        if let Some(dir) = std::env::var_os("BDINFO_GUI_SNAPSHOT_DUMP") {
            let dir = PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            let _ = snapshot.matches_image(dir.join(name));
        }
        let family = if cfg!(windows) { "windows" } else { "unix" };
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("snapshots").join(family);
        let matches = snapshot.matches_hash(dir.join(name)).expect("the hash file is readable");
        if !matches {
            let dump = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/snapshot-failures");
            let _ = std::fs::create_dir_all(&dump);
            let _ = snapshot.matches_image(dump.join(name));
        }
        assert!(
            matches,
            "snapshot '{name}' drifted from the committed hash; the rendered PNG is \
             under target/snapshot-failures/ — re-pin deliberately if the change is intended"
        );
    }
}

/// Interaction tests: `iced_test`'s Simulator renders the real
/// `view()` offscreen, clicks it by visible text, and the produced messages
/// run through the real `update` — so the widget wiring (what a click
/// dispatches, what a state renders) is verified against the library state it
/// must land in. No window, no Tasks, no subscriptions: worker completions
/// are injected as the messages they would arrive as.
#[cfg(test)]
mod interaction {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bdinfo_rs_gui::flow::Stage;
    use bdinfo_rs_gui::model::{Sort, SortColumn};
    use bdinfo_rs_gui::theme::ThemePref;

    use super::harness::{
        bounds, click, encrypted_app, filtered_app, listed_app, sees, structural,
    };
    use super::{App, ENCRYPTED_BADGE, Message};

    #[test]
    fn the_scan_flow_reaches_the_report_through_clicks() {
        let mut app = listed_app();
        // Check every row, then start the measured scan from the button.
        click(&mut app, "Select All");
        assert_eq!(app.flow.selected_count(), 2);
        click(&mut app, "Scan Bitrates");
        assert_eq!(app.flow.stage(), Stage::Scanning);
        assert!(sees(&app, "Cancel"), "the scanning state offers Cancel");
        // The worker's completion arrives (injected — the Simulator runs no
        // Tasks; the real worker's stale failure is dropped by the guard).
        let _ = app.update(Message::Finished {
            generation: app.generation,
            report: "THE REPORT BODY".to_owned(),
            errors: Arc::new(Vec::new()),
            playlists: structural().bdrom.playlists,
        });
        assert_eq!(app.flow.stage(), Stage::Reported);
        assert_eq!(app.notice.as_deref(), Some("Scan completed successfully."));
        // Dismiss the completion modal, open the report view, and come back.
        click(&mut app, "OK");
        assert!(app.notice.is_none());
        click(&mut app, "View Report...");
        assert!(app.showing_report);
        assert!(sees(&app, "THE REPORT BODY"), "the report text is on screen");
        click(&mut app, "← Back");
        assert!(!app.showing_report);
    }

    #[test]
    fn an_encrypted_disc_shows_the_indicator_and_the_scan_stays_dead() {
        let mut app = encrypted_app();
        assert!(sees(&app, ENCRYPTED_BADGE), "the info box flags the disc");
        assert!(!sees(&listed_app(), ENCRYPTED_BADGE), "an ordinary disc does not");

        // The control is still there and still says what it does — it just
        // does nothing. Clicking it dispatches no message…
        assert!(sees(&app, "Scan Bitrates"));
        assert!(click(&mut app, "Scan Bitrates").is_empty(), "the button is dead");
        assert_eq!(app.flow.stage(), Stage::Listed);
        // …and neither does the message it would have dispatched, so a road
        // that skips the button cannot start the scan either.
        let _ = app.update(Message::ScanSelected);
        assert_eq!(app.flow.stage(), Stage::Listed);
        assert!(app.scan_cancel.is_none(), "no worker was ever handed a cancel flag");

        // Everything structural stays browsable: the playlists list, the panes
        // fill from the active row, and the pre-scan report is still offered.
        assert_eq!(app.flow.row_count(), 2);
        assert!(sees(&app, "00000.MPLS"));
        click(&mut app, "View Report...");
        assert!(app.showing_report);
    }

    #[test]
    fn the_encrypted_indicator_costs_no_layout() {
        // The indicator draws into the slack the info strip already has, so
        // every other widget is laid out at the same rectangle as on the same
        // disc without it — the info-box lines it sits beside, the table above
        // and the actions below. A shift shows up here as a moved rectangle.
        let plain = listed_app();
        let flagged = encrypted_app();
        for label in [
            "Disc Title:",
            "Detected BDMV Folder:",
            "Detected Features:",
            "Disc Size:",
            "00000.MPLS",
            "0 of 2 selected",
            "Scan Bitrates",
            "View Report...",
            "Settings...",
        ] {
            assert_eq!(bounds(&plain, label), bounds(&flagged, label), "{label} moved");
        }
    }

    #[test]
    fn cancel_returns_the_scan_to_the_table() {
        let mut app = listed_app();
        click(&mut app, "Scan Bitrates"); // nothing checked — scan-all
        assert_eq!(app.flow.stage(), Stage::Scanning);
        click(&mut app, "Cancel");
        assert_eq!(app.flow.stage(), Stage::Listed);
        // The cooperative stop flag was taken and tripped.
        assert!(app.scan_cancel.is_none());
    }

    #[test]
    fn cancel_returns_the_listing_to_the_source_prompt() {
        let mut app = App::default();
        let _ = app.update(Message::FolderPicked(Some(PathBuf::from("disc"))));
        assert_eq!(app.flow.stage(), Stage::Listing);
        assert!(app.scan_cancel.is_some(), "the listing worker owns a cancel flag");
        let generation = app.generation;
        // The "please wait" modal offers Cancel; clicking it returns to the
        // source prompt without waiting on the (possibly stuck) worker.
        click(&mut app, "Cancel");
        assert_eq!(app.flow.stage(), Stage::Idle);
        assert!(app.scan_cancel.is_none(), "the flag was taken and tripped");
        assert!(sees(&app, "disc"), "the source field keeps the input for a one-click Rescan");
        // The cancelled worker's late return applies nothing — and must not
        // turn the idle prompt into the fatal failure screen.
        let _ = app.update(Message::Listed {
            generation,
            input: bdinfo_rs_gui::scan::Input::Folder("disc".into()),
            result: Err("scan cancelled".to_owned()),
        });
        assert_eq!(app.flow.stage(), Stage::Idle);
    }

    #[test]
    fn a_cancelled_workers_result_never_resolves_a_relisting_of_the_same_input() {
        let mut app = App::default();
        let _ = app.update(Message::FolderPicked(Some(PathBuf::from("disc"))));
        let stale = app.generation;
        click(&mut app, "Cancel");
        // Rescan the SAME input: the flow's input match alone could not tell
        // the old worker's result from the new one's — the generation stamp
        // is what drops it.
        let _ = app.update(Message::Rescan);
        assert_eq!(app.flow.stage(), Stage::Listing);
        assert_ne!(app.generation, stale);
        let _ = app.update(Message::Listed {
            generation: stale,
            input: bdinfo_rs_gui::scan::Input::Folder("disc".into()),
            result: Ok(structural()),
        });
        assert_eq!(app.flow.stage(), Stage::Listing, "the old worker's table must not surface");
        // The new worker's result lands normally.
        let _ = app.update(Message::Listed {
            generation: app.generation,
            input: bdinfo_rs_gui::scan::Input::Folder("disc".into()),
            result: Ok(structural()),
        });
        assert_eq!(app.flow.stage(), Stage::Listed);
    }

    #[test]
    fn a_listing_progress_event_reaches_the_modal_and_the_bar() {
        let mut app = App::default();
        let _ = app.update(Message::FolderPicked(Some(PathBuf::from("disc"))));
        assert!(sees(&app, "Please wait while we scan the disc..."));
        let _ = app.update(Message::ListProgress {
            generation: app.generation,
            file: "A.M2TS".to_owned(),
            done: 1,
            total: 2,
        });
        let fraction =
            app.flow.progress_view().map(bdinfo_rs_gui::progress::ProgressModel::fraction);
        assert_eq!(fraction, Some(0.5));
        assert!(sees(&app, "Reading A.M2TS…"), "the modal names the file being read");
        // An event from a superseded worker is dropped before the flow.
        let _ = app.update(Message::ListProgress {
            generation: app.generation.wrapping_add(1),
            file: "B.M2TS".to_owned(),
            done: 2,
            total: 2,
        });
        assert!(sees(&app, "Reading A.M2TS…"), "the stale event must not repaint");
        assert!(!sees(&app, "Reading B.M2TS…"));
    }

    #[test]
    fn a_progress_message_reaches_the_bottom_bar() {
        let mut app = listed_app();
        click(&mut app, "Scan Bitrates");
        let _ = app.update(Message::Progress {
            generation: app.generation,
            file: "A.M2TS".to_owned(),
            done: 1,
            total: 2,
        });
        let fraction =
            app.flow.progress_view().map(bdinfo_rs_gui::progress::ProgressModel::fraction);
        assert_eq!(fraction, Some(0.5));
        assert!(sees(&app, "Scanning A.M2TS…"), "the lead line names the demuxing file");
    }

    #[test]
    fn a_tick_keeps_elapsed_climbing_and_flags_a_stalled_read() {
        let mut app = listed_app();
        click(&mut app, "Scan Bitrates");
        let _ = app.update(Message::Progress {
            generation: app.generation,
            file: "A.M2TS".to_owned(),
            done: 1,
            total: 2,
        });
        assert!(sees(&app, "Scanning A.M2TS…"));
        // Re-date the scan start and the last progress event into the past —
        // the wall time a stuck ReadFile would let pass with no event at all.
        app.scan_start = Instant::now().checked_sub(Duration::from_secs(90));
        app.last_progress = Instant::now().checked_sub(Duration::from_secs(30));
        let _ = app.update(Message::Tick);
        // One tick re-reads the clock: elapsed climbs with no progress event,
        // and the quiet 30 s turn the lead line into the stall hint.
        assert_eq!(
            app.flow.progress_view().map(bdinfo_rs_gui::progress::ProgressModel::elapsed_hms),
            Some("00:01:30".to_owned())
        );
        assert!(app.flow.scan_stalled());
        assert!(sees(&app, "Still reading A.M2TS…"), "the stall hint names the stuck file");
    }

    #[test]
    fn a_failed_scan_banners_over_the_still_usable_table() {
        let mut app = listed_app();
        click(&mut app, "Scan Bitrates");
        let _ = app.update(Message::ScanFailed {
            generation: app.generation,
            error: "disc vanished".to_owned(),
        });
        assert_eq!(app.flow.stage(), Stage::Listed);
        assert!(sees(&app, "disc vanished"), "the failure banner renders");
        assert!(app.flow.editable(), "the table stays usable for a retry");
    }

    #[test]
    fn a_checkbox_toggle_message_checks_its_row() {
        // The checkbox has no text for the selector to find; the message it
        // dispatches is pinned here through the real update road.
        let mut app = listed_app();
        let _ = app.update(Message::RowToggled(0));
        assert_eq!(app.flow.selected_count(), 1);
        let _ = app.update(Message::RowToggled(0));
        assert_eq!(app.flow.selected_count(), 0);
    }

    #[test]
    fn the_hidden_count_line_reveals_and_re_hides_the_withheld_playlists() {
        let mut app = filtered_app();
        // The table is short of two playlists, and says so under itself.
        assert_eq!(app.flow.row_count(), 2);
        assert!(sees(&app, "2 playlists hidden by filters (short, looping)"));
        assert!(!sees(&app, "00002.MPLS"), "the looping playlist is not a row");
        // Show puts them in the table as ordinary rows.
        click(&mut app, "Show");
        assert_eq!(app.flow.row_count(), 4);
        assert!(sees(&app, "Showing 2 filtered playlists (short, looping)"));
        assert!(sees(&app, "00002.MPLS"));
        // Hide takes them back out.
        click(&mut app, "Hide");
        assert_eq!(app.flow.row_count(), 2);
        assert!(sees(&app, "2 playlists hidden by filters (short, looping)"));
        // A disc the filters withhold nothing from draws no line at all.
        assert!(!sees(&listed_app(), "Show"));
    }

    #[test]
    fn applying_the_settings_dialog_drops_the_transient_reveal() {
        // The reveal is session-only: the dialog's OK — even one that changed
        // nothing — restores the persisted settings' own view of the table.
        let mut app = filtered_app();
        click(&mut app, "Show");
        assert!(app.flow.revealing());
        let _ = app.update(Message::OpenSettings);
        let _ = app.update(Message::SettingsOk);
        assert!(!app.flow.revealing());
        assert_eq!(app.flow.row_count(), 2);
        assert!(app.persisted.filter_looping_playlists, "the reveal never touched the settings");
    }

    #[test]
    fn a_recalled_source_shows_with_rescan_live() {
        // Last session's disc, remembered but not scanned — the Source field
        // shows it and Rescan is clickable (it starts a listing of that path).
        let recalled = bdinfo_rs_gui::flow::Flow::recall(bdinfo_rs_gui::scan::Input::Folder(
            "remembered-disc".into(),
        ));
        let mut app = App { flow: recalled, ..App::default() };
        assert!(sees(&app, "remembered-disc"));
        click(&mut app, "Rescan");
        assert_eq!(app.flow.stage(), Stage::Listing);
    }

    #[test]
    fn ctrl_c_copies_the_selected_stream_file_path() {
        let mut app = listed_app();
        // Click the Stream File pane's row (the active playlist's clip), then
        // the copy shortcut's message; the status line confirms the target.
        click(&mut app, "A.M2TS");
        assert_eq!(app.pane_selection, Some((super::Region::StreamFile, 0)));
        let _ = app.update(Message::CopyPath);
        assert_eq!(
            app.status.as_deref(),
            Some(
                format!("Copied: disc{0}BDMV{0}STREAM{0}A.M2TS", std::path::MAIN_SEPARATOR)
                    .as_str()
            )
        );
    }

    #[test]
    fn select_all_flips_to_unselect_all() {
        let mut app = listed_app();
        assert!(sees(&app, "Select All"));
        click(&mut app, "Select All");
        assert!(app.flow.all_selected());
        // The toggle re-labels; clicking it clears the selection.
        click(&mut app, "Unselect All");
        assert_eq!(app.flow.selected_count(), 0);
    }

    #[test]
    fn a_row_click_activates_its_playlist() {
        let mut app = listed_app();
        let messages = click(&mut app, "00001.MPLS");
        assert!(
            messages.iter().any(|m| matches!(m, Message::RowActivated(1))),
            "the row's cells dispatch its activation: {messages:?}"
        );
        assert_eq!(app.flow.active_index(), Some(1));
        assert_eq!(app.flow.active_playlist_name().as_deref(), Some("00001.MPLS"));
    }

    #[test]
    fn a_header_click_sorts_the_table() {
        let mut app = listed_app();
        // The Group column reads ascending (1, 2) — BDInfo's rule makes the
        // first click descend so it visibly re-orders.
        click(&mut app, "Group");
        assert_eq!(app.flow.sort(), Some(Sort { column: SortColumn::Group, ascending: false }));
        let first = app.flow.table().into_iter().next().expect("a first row");
        assert_eq!(first.cells.group, "2");
    }

    #[test]
    fn the_settings_dialog_applies_on_ok_and_discards_on_cancel() {
        let mut app = listed_app();
        click(&mut app, "Settings...");
        assert!(app.settings_draft.is_some());
        click(&mut app, "Stream sizes in human readable format");
        click(&mut app, "OK");
        assert!(app.settings_draft.is_none());
        assert!(app.persisted.human_readable_sizes);
        // The retained disc re-rendered under the toggle — no rescan.
        let first = app.flow.table().into_iter().next().expect("a first row");
        assert_eq!(first.cells.estimated_bytes, "1,000.00 B");
        // Cancel discards: flip a filter checkbox, then back out.
        click(&mut app, "Settings...");
        click(&mut app, "Filter playlists that contain loops");
        click(&mut app, "Cancel");
        assert!(app.settings_draft.is_none());
        assert!(app.persisted.filter_looping_playlists, "the cancelled edit never landed");
    }

    #[test]
    fn the_scale_chips_preview_apply_and_revert() {
        let mut app = listed_app();
        assert_eq!(app.ui_scale().to_bits(), 1.0f32.to_bits());
        // Picking a chip previews live off the draft — nothing applied yet.
        click(&mut app, "Settings...");
        click(&mut app, "150%");
        assert_eq!(app.ui_scale().to_bits(), 1.5f32.to_bits(), "the open dialog previews");
        assert_eq!(app.persisted.ui_scale_percent, 100);
        // OK applies and persists the pick.
        click(&mut app, "OK");
        assert_eq!(app.persisted.ui_scale_percent, 150);
        assert_eq!(app.ui_scale().to_bits(), 1.5f32.to_bits());
        // Cancel drops a previewed pick — the applied percent stands.
        click(&mut app, "Settings...");
        click(&mut app, "75%");
        assert_eq!(app.ui_scale().to_bits(), 0.75f32.to_bits(), "the preview switched");
        click(&mut app, "Cancel");
        assert_eq!(app.persisted.ui_scale_percent, 150);
        assert_eq!(app.ui_scale().to_bits(), 1.5f32.to_bits(), "the cancelled preview reverted");
    }

    #[test]
    fn the_scale_shortcut_steps_the_settings_or_the_open_draft() {
        let mut app = listed_app();
        // No dialog: the step lands on (and would persist) the settings.
        let _ = app.update(Message::ScaleStepped(true));
        assert_eq!(app.persisted.ui_scale_percent, 125);
        let _ = app.update(Message::ScaleStepped(false));
        let _ = app.update(Message::ScaleStepped(false));
        assert_eq!(app.persisted.ui_scale_percent, 75);
        // Dialog open: the step edits the draft only — OK applies it.
        click(&mut app, "Settings...");
        let _ = app.update(Message::ScaleStepped(true));
        let draft = app.settings_draft.as_ref().expect("the dialog is open");
        assert_eq!(draft.ui_scale_percent, 100);
        assert_eq!(app.persisted.ui_scale_percent, 75, "the step previewed, not applied");
        click(&mut app, "OK");
        assert_eq!(app.persisted.ui_scale_percent, 100);
    }

    #[test]
    fn a_bad_path_lands_in_the_friendly_failure_screen() {
        // A REAL file that is not an `.iso` (this crate's own manifest), so
        // the classifier's extension road runs before rejecting it.
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let mut app = App::default();
        let _ = app.update(Message::OpenPath(manifest.clone()));
        assert_eq!(app.flow.stage(), Stage::Failed);
        assert!(sees(&app, "Couldn't open the disc"));
        // The classifier's whole message renders (the selector is exact-text).
        assert!(sees(
            &app,
            &format!("Not a Blu-ray disc folder or .iso image: {}", manifest.display())
        ));
    }

    #[test]
    fn only_the_first_dropped_item_opens() {
        // Two files dropped together: winit hovers both, then drops both. The
        // first drop takes the hover flag and opens (here: fails classify);
        // the second must be ignored, not clobber the first.
        let mut app = App::default();
        let _ = app.update(Message::FileHovered);
        let _ = app.update(Message::FileHovered);
        let _ = app.update(Message::FileDropped(PathBuf::from("first.xyz")));
        let _ = app.update(Message::FileDropped(PathBuf::from("second.xyz")));
        assert_eq!(app.flow.stage(), Stage::Failed);
        assert!(sees(&app, "The path does not exist: first.xyz"), "the FIRST item won the drop");
        assert!(!sees(&app, "The path does not exist: second.xyz"));
    }

    /// A scratch directory under the crate's target dir (unique per test) for
    /// the save-destination tests.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/interaction-tests").join(name);
        std::fs::create_dir_all(&dir).expect("the scratch dir creates");
        dir
    }

    #[test]
    fn a_picker_completion_never_interrupts_a_running_scan() {
        // rfd dialogs are non-modal: a Browse / ISO dialog left open across a
        // scan start resolves into Scanning. Both completions must be dropped
        // — replacing the flow would strand the worker's cancel flag.
        let mut app = listed_app();
        click(&mut app, "Scan Bitrates");
        assert_eq!(app.flow.stage(), Stage::Scanning);
        let _ = app.update(Message::FolderPicked(Some(PathBuf::from("other-disc"))));
        assert_eq!(
            app.flow.stage(),
            Stage::Scanning,
            "a late folder pick must not replace the scan"
        );
        let _ = app.update(Message::IsoPicked(Some(PathBuf::from("other.iso"))));
        assert_eq!(app.flow.stage(), Stage::Scanning, "a late .iso pick must not replace the scan");
        assert!(app.scan_cancel.is_some(), "the worker's cancel flag is still owned");
        // The loaded disc is still the one being scanned, not the late pick.
        assert_eq!(
            app.flow.current_input(),
            Some(&bdinfo_rs_gui::scan::Input::Folder("disc".into()))
        );
    }

    #[test]
    fn a_save_destination_resolving_mid_scan_writes_nothing() {
        // "Save report…" open → a new scan starts → the destination resolves:
        // mid-scan `flow.report()` is the structural (zero-bitrate) render,
        // which would silently overwrite the measured report on disk.
        let dir = scratch("save-mid-scan");
        let target = dir.join("BDINFO.DISC.txt");
        let _ = std::fs::remove_file(&target);
        let mut app = listed_app();
        click(&mut app, "Scan Bitrates");
        assert_eq!(app.flow.stage(), Stage::Scanning);
        let _ = app.update(Message::SaveDest(Some(dir)));
        assert!(app.status.is_none(), "no save outcome is reported mid-scan");
        assert!(!target.exists(), "nothing may be written while a scan is in flight");
    }

    #[test]
    fn a_save_destination_in_a_settled_stage_writes_the_report() {
        // The positive control: once the scan has settled into Reported, the
        // same completion writes the measured report bytes verbatim.
        let dir = scratch("save-settled");
        let target = dir.join("BDINFO.DISC.txt");
        let _ = std::fs::remove_file(&target);
        let mut app = listed_app();
        click(&mut app, "Scan Bitrates");
        let _ = app.update(Message::Finished {
            generation: app.generation,
            report: "THE MEASURED REPORT\r\n".to_owned(),
            errors: Arc::new(Vec::new()),
            playlists: structural().bdrom.playlists,
        });
        assert_eq!(app.flow.stage(), Stage::Reported);
        let _ = app.update(Message::SaveDest(Some(dir)));
        assert_eq!(
            std::fs::read(&target).expect("the report file was written"),
            b"THE MEASURED REPORT\r\n"
        );
        assert!(
            app.status.as_deref().is_some_and(|status| status.starts_with("Report saved to:")),
            "the status line confirms the save: {:?}",
            app.status
        );
    }

    #[test]
    fn a_hostile_volume_label_saves_as_a_sanitized_flat_filename() {
        // A disc controls its label bytes and autosave writes with no user in
        // the loop: a separator-carrying label must land as ONE flat file
        // inside the chosen directory — the same name the CLI writes.
        let dir = scratch("save-hostile-label");
        let mut hostile = structural();
        hostile.bdrom.volume_label = r"..\..\EVIL".to_owned();
        let input = bdinfo_rs_gui::scan::Input::Folder("disc".into());
        let flow = bdinfo_rs_gui::flow::Flow::start_listing(input.clone()).listed(
            &input,
            Ok(hostile),
            bdinfo_rs_gui::model::ViewSettings::default(),
        );
        let mut app = App { flow, ..App::default() };
        let _ = app.update(Message::SaveDest(Some(dir.clone())));
        let target = dir.join("BDINFO..._.._EVIL.txt");
        assert!(target.exists(), "the sanitized name landed inside the chosen directory");
        assert!(
            app.status.as_deref().is_some_and(|status| status.starts_with("Report saved to:")),
            "the status line confirms the save: {:?}",
            app.status
        );
    }

    #[test]
    fn the_settings_theme_picker_previews_applies_and_reverts() {
        let mut app = listed_app();
        assert_eq!(app.theme_pref, ThemePref::System);
        // Picking a chip previews live off the draft — nothing applied yet.
        click(&mut app, "Settings...");
        click(&mut app, "Dark");
        assert!(app.palette().is_dark, "the open dialog previews the pick");
        assert_eq!(app.theme_pref, ThemePref::System);
        // OK applies and persists the pick.
        click(&mut app, "OK");
        assert_eq!(app.theme_pref, ThemePref::Dark);
        assert_eq!(app.persisted.theme, bdinfo_rs_gui::settings::ThemeChoice::Dark);
        // Cancel drops a previewed pick — the applied choice stands.
        click(&mut app, "Settings...");
        click(&mut app, "Light");
        assert!(!app.palette().is_dark, "the preview switched to light");
        click(&mut app, "Cancel");
        assert_eq!(app.theme_pref, ThemePref::Dark);
        assert!(app.palette().is_dark, "the cancelled preview reverted");
    }
}

/// Pixel ties: key states rendered offscreen on the deterministic
/// tiny-skia backend with the bundled fonts, pinned as committed SHA-256
/// hashes per OS family (`snapshots/{windows,unix}/*.sha256` — see
/// [`harness::assert_snapshot`] for the family evidence). Run them via the
/// gate (it sets `ICED_TEST_BACKEND=tiny-skia`); kept in their own module so
/// CI can run them as its own blocking per-arch step
/// (`-E 'test(/^snapshots::/)'`).
#[cfg(test)]
mod snapshots {
    use std::sync::Arc;

    use bdinfo_rs_gui::settings;
    use bdinfo_rs_gui::theme::ThemePref;

    use super::harness::{assert_snapshot, filtered_app, listed_app, structural};
    use super::{App, Message};

    #[test]
    fn idle_dark() {
        let app = App { theme_pref: ThemePref::Dark, ..App::default() };
        assert_snapshot(&app, "idle-dark");
    }

    #[test]
    fn listed_light() {
        let mut app = listed_app();
        app.theme_pref = ThemePref::Light;
        assert_snapshot(&app, "listed-light");
    }

    #[test]
    fn reported_dark() {
        let mut app = listed_app();
        app.theme_pref = ThemePref::Dark;
        let _ = app.update(Message::ScanSelected);
        let _ = app.update(Message::Finished {
            generation: app.generation,
            report: "Disc Label: DISC\r\nA stable little report body.\r\n".to_owned(),
            errors: Arc::new(Vec::new()),
            playlists: structural().bdrom.playlists,
        });
        let _ = app.update(Message::DismissNotice);
        let _ = app.update(Message::ShowReport);
        assert_snapshot(&app, "report-dark");
    }

    #[test]
    fn settings_dialog_dark() {
        let mut app = listed_app();
        app.theme_pref = ThemePref::Dark;
        app.settings_draft = Some(settings::Draft::from_settings(&app.persisted));
        assert_snapshot(&app, "settings-dark");
    }

    /// The hidden-count line as drawn: the note, its Show button, and the two
    /// withheld playlists absent from the table above it. The other pinned
    /// states use a disc the filters withhold nothing from, so this is the only
    /// tie that renders the line at all.
    #[test]
    fn filtered_hidden_dark() {
        let mut app = filtered_app();
        app.theme_pref = ThemePref::Dark;
        assert_snapshot(&app, "filtered-hidden-dark");
    }

    /// The same disc after Show: the withheld playlists sit in the table as
    /// ordinary rows and the line reads "Showing …" over a Hide button.
    #[test]
    fn filtered_shown_dark() {
        let mut app = filtered_app();
        app.theme_pref = ThemePref::Dark;
        let _ = app.update(Message::ToggleReveal);
        assert_snapshot(&app, "filtered-shown-dark");
    }
}
