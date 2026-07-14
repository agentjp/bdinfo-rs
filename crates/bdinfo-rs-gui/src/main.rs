//! `bdinfo-rs-gui` — the native desktop window for the bdinfo-rs Blu-ray
//! analyzer.
//!
//! The thin iced (Tier-B) shell over the [`bdinfo_rs_gui`] library: it drives the
//! full two-phase flow — pick a disc FOLDER or `.iso`, fast structural scan →
//! selectable playlist table → measured scan on a worker thread with a live
//! progress bar → the rendered report, with Save / Copy. Every `update` arm calls
//! a Tier-A transition ([`Flow`]) or seam ([`scan`]) and applies the result; the
//! `view` is a thin projection of [`Flow`] onto widgets dressed in the
//! "Projection Booth" identity ([`theme`] + [`ui`]). The only impure thing the
//! shell owns is the measured-scan worker thread and the channel that streams its
//! progress back as messages, plus the OS-theme and file-drop subscriptions and
//! the best-effort config store ([`settings`]): window geometry, the last opened
//! source, and the theme preference persist across launches.
//! A disc can arrive three ways — the pickers, a drag-and-drop onto the window,
//! or a boot argument (`bdinfo-rs-gui <path>`) — all funnelling into the same
//! open flow.
#![forbid(unsafe_code)]
// The window is a leaf binary, not a library: a Windows release build hides the
// console so launching it does not flash a terminal. No effect on other targets.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod icon;
mod theme;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bdinfo_rs_core::bdrom::disc::{PlaylistSummary, ScanProgress};
use bdinfo_rs_core::error::ScanError;
use bdinfo_rs_gui::flow::{Flow, ScanRequest, Stage};
use bdinfo_rs_gui::model::{
    SelectableRow, Sort, SortColumn, ViewSettings, format_file_size, group_n0,
};
use bdinfo_rs_gui::panes::{CodecRow, StreamFileRow};
use bdinfo_rs_gui::progress::ProgressModel;
use bdinfo_rs_gui::scan::{self, Input, Structural};
use bdinfo_rs_gui::{paths, settings};
use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Column, PaneGrid, Row, Space, Stack, button, container, mouse_area, pane_grid, progress_bar,
    responsive, scrollable, text, text_input,
};
use iced::{Element, Length, Task};

use crate::theme::{Palette, ThemePref};

/// The window's initial logical size — a **portrait-leaning** footprint (taller
/// than wide), because the three master-detail panes are stacked vertically, so
/// the vertical axis is where the value is. Roughly `BDInfo`'s width but much
/// taller, sized to nearly fill a standard ~900-tall work area. The layout is
/// fully responsive (`Fill`/`FillPortion` + scrollable panes), so any size works;
/// long codec descriptions clip at the pane edge exactly as they do in `BDInfo`.
const WINDOW_SIZE: (f32, f32) = (880.0, 960.0);
/// The minimum size the window can be resized down to — low enough to fit a
/// small screen, beyond which the panes scroll rather than disappear (so the
/// window never gets stuck larger than a modest display).
const MIN_SIZE: (f32, f32) = (680.0, 540.0);
/// The window-icon resolution (procedurally drawn; winit downscales as needed).
const ICON_SIZE: u32 = 128;

fn main() -> iced::Result {
    // One boot-time load of the persisted state: the geometry feeds the window
    // settings, the rest seeds the app in `boot`.
    let persisted = settings::load();
    let window = window_settings(&persisted);
    iced::application(move || boot(persisted.clone()), App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .style(App::style)
        // "Auto" tracks the desktop live; while the initial scan runs, also drive
        // the indeterminate progress-bar animation.
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
        .run()
}

/// Boots the app and fires a one-shot request for the OS theme, so the first
/// frame matches the desktop's light/dark setting. The persisted state seeds
/// the launch: the stored theme preference applies, and the last opened source
/// — when it still exists — shows in the Source field with "Rescan" live
/// (`BDInfo` seeds its source box from `LastPath` the same way), but nothing
/// scans until asked; a vanished path starts clean. A command-line path
/// (`bdinfo-rs-gui <path>`, `BDInfo`'s `args[0]`) opens at boot through the same
/// classify-and-list road a drop takes — read from the OS-native argv, so a
/// non-UTF-8 path survives intact. In a **debug** build it also applies the
/// screenshot-harness environment hooks (see [`debug_boot`]).
fn boot(persisted: settings::Settings) -> (App, Task<Message>) {
    let mut app =
        App { theme_pref: ThemePref::from_choice(persisted.theme), persisted, ..App::default() };
    if let Some(input) = app.persisted.last_path.clone().and_then(|path| Input::classify(path).ok())
    {
        app.flow = Flow::recall(input);
    }
    let mut tasks = vec![iced::system::theme().map(Message::OsTheme)];
    if let Some(path) = std::env::args_os().nth(1) {
        tasks.push(Task::done(Message::OpenPath(PathBuf::from(path))));
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
/// (`light`/`dark`) pins the palette and `BDINFO_GUI_OPEN` / `BDINFO_GUI_ISO`
/// auto-open a disc, so `scripts/gui-shoot.ps1` can drive the real window to a
/// known state and capture it. Compiled out of every release build.
#[cfg(debug_assertions)]
fn boot_with(mut app: App, base: Task<Message>) -> (App, Task<Message>) {
    if let Ok(pref) = std::env::var("BDINFO_GUI_THEME") {
        match pref.as_str() {
            "light" => app.theme_pref = ThemePref::Light,
            "dark" => app.theme_pref = ThemePref::Dark,
            _ => {}
        }
    }
    let folder = std::env::var("BDINFO_GUI_OPEN")
        .ok()
        .map(|path| Task::done(Message::FolderPicked(Some(PathBuf::from(path)))));
    let iso = std::env::var("BDINFO_GUI_ISO")
        .ok()
        .map(|path| Task::done(Message::IsoPicked(Some(PathBuf::from(path)))));
    let mut tasks = vec![base];
    tasks.extend(folder);
    tasks.extend(iso);
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
/// inserted into the initial scan, so the transient "scanning" overlay stays up
/// long enough for the capture harness to grab it. No effect without the var.
#[cfg(debug_assertions)]
fn debug_scan_delay() {
    if let Some(ms) = std::env::var("BDINFO_GUI_SCAN_DELAY").ok().and_then(|v| v.parse().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// The window settings: last session's geometry (the stored size clamped to
/// the minimum; the default size and OS placement when nothing sane is
/// stored), the minimum size, and the embedded app icon (drawn procedurally,
/// so the binary stays self-contained). Closing is handled manually
/// (`exit_on_close_request: false`) so the geometry can be captured and saved
/// on the way out — see [`Message::CloseRequested`].
fn window_settings(persisted: &settings::Settings) -> iced::window::Settings {
    let (width, height) =
        persisted.window_size.map_or(WINDOW_SIZE, |(w, h)| (w.max(MIN_SIZE.0), h.max(MIN_SIZE.1)));
    // The store validated the position's range; Wayland (no global positions)
    // simply never stores one, so the platform default placement applies.
    let position = persisted.window_pos.map_or(iced::window::Position::Default, |(x, y)| {
        iced::window::Position::Specific(iced::Point::new(x, y))
    });
    iced::window::Settings {
        size: iced::Size::new(width, height),
        position,
        min_size: Some(iced::Size::new(MIN_SIZE.0, MIN_SIZE.1)),
        icon: iced::window::icon::from_rgba(icon::rgba(ICON_SIZE), ICON_SIZE, ICON_SIZE).ok(),
        exit_on_close_request: false,
        ..iced::window::Settings::default()
    }
}

/// The app state — the Tier-A [`Flow`] the shell drives, plus the runtime-side
/// bits: the visual preference + last-known OS mode, the scan generation counter
/// (stamped on the worker's messages so a stale event is ignored), the scan start
/// time (for the elapsed / remaining estimate), and a transient status line.
struct App {
    /// The flow state machine — `view` is a projection of it.
    flow: Flow,
    /// The persisted state (geometry / last path / theme), as it will next be
    /// written: updated alongside the live state and saved best-effort at the
    /// change points (theme toggle, successful listing) and on close.
    persisted: settings::Settings,
    /// Follow-the-OS / pinned light / pinned dark.
    theme_pref: ThemePref,
    /// The last light/dark setting reported by the OS.
    os_mode: iced::theme::Mode,
    /// Bumped on each "Scan selected"; the in-flight scan's id.
    generation: u64,
    /// The in-flight measured scan's cooperative cancel flag — a fresh one per
    /// scan (so cancelling an old scan can never touch a newer one), shared
    /// with the worker thread, set by the Cancel button. The demux polls it
    /// per read chunk and aborts, so Cancel really frees the machine.
    scan_cancel: Option<Arc<AtomicBool>>,
    /// When the current measured scan started.
    scan_start: Option<Instant>,
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
    /// The animation phase (`0..PULSE_PERIOD`) of the indeterminate "scanning"
    /// bar shown during the initial disc scan; advanced by [`Message::Tick`].
    pulse: u16,
    /// The three master-detail panes and the two draggable splitters between
    /// them (Playlist / Stream File on top, Codec below). Holds the split ratios,
    /// which the user drags and which resize proportionally with the window.
    panes: pane_grid::State<Region>,
    /// The Playlist + Stream File shared column weights (Name / Int / Length /
    /// Estimated / Measured) — the user drags header dividers to re-weight them.
    grid_w: [f32; 5],
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
            persisted: settings::Settings::default(),
            theme_pref: ThemePref::default(),
            os_mode: iced::theme::Mode::default(),
            generation: 0,
            scan_cancel: None,
            scan_start: None,
            status: None,
            showing_report: false,
            notice: None,
            settings_draft: None,
            hovered: None,
            hovered_header: None,
            pane_selection: None,
            pulse: 0,
            panes: default_panes(),
            // BDInfo's playlist ratios; the Codec grid is its own 22/10/10/56.
            grid_w: [30.0, 7.0, 19.0, 21.0, 21.0],
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

/// Shifts `delta` weight across the boundary between column `boundary` and
/// `boundary + 1`, clamped so neither drops below [`MIN_COL_WEIGHT`]. Their sum is
/// preserved, so only these two columns change width — the rest hold still.
fn adjust_weight(weights: &mut [f32], boundary: usize, delta: f32) {
    let (Some(&a), Some(&b)) = (weights.get(boundary), weights.get(boundary.wrapping_add(1)))
    else {
        return;
    };
    let pair = a + b;
    let new_a = (a + delta).clamp(MIN_COL_WEIGHT, (pair - MIN_COL_WEIGHT).max(MIN_COL_WEIGHT));
    if let Some(slot) = weights.get_mut(boundary) {
        *slot = new_a;
    }
    if let Some(slot) = weights.get_mut(boundary.wrapping_add(1)) {
        *slot = pair - new_a;
    }
}

/// A column weight as an `iced` `FillPortion` (scaled up so fractional weights
/// keep resolution).
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "weight is clamped to a small positive range before the cast"
)]
fn fill(weight: f32) -> Length {
    Length::FillPortion((weight * 10.0).round().clamp(1.0, 60000.0) as u16)
}

/// The indeterminate-bar animation period (one full 0→1→0 breath).
const PULSE_PERIOD: u16 = 1000;
/// Half the period — the peak of the triangle wave.
const PULSE_HALF: u16 = 500;
/// The phase advance per animation frame (larger = faster breathing).
const PULSE_STEP: u16 = 16;

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

/// Which set of resizable column weights a drag targets. The Playlist and Stream
/// File panes share ONE weight set (`Shared`) so their columns stay locked
/// together; the Codec pane has its own (`Codec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColGrid {
    /// The Playlist + Stream File shared column weights.
    Shared,
    /// The Codec pane's column weights.
    Codec,
}

/// The minimum weight a column can be dragged down to (keeps every column
/// visible). Weights are `BDInfo`-ratio units (they sum to ~98).
const MIN_COL_WEIGHT: f32 = 4.0;
/// The width of the grab strip on a header column's right edge — the zone that
/// shows the resize cursor and drags the boundary (wide enough to hit easily).
const GRAB_W: f32 = 14.0;

/// A top-level command — the menu/toolbar seam a later native macOS menu maps
/// onto. `view` never hard-codes an action's label, message, or enablement: it
/// reads them from here (and [`App::command_enabled`]), so wiring a native menu
/// in a later phase is plumbing, not a rewrite. The intended native grouping is
/// **File**: Open folder / Open ISO / Rescan / Save report (and the window's own
/// Quit); **View**: the theme toggle.
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
    /// Cycle the theme: Auto → Light → Dark.
    ToggleTheme,
}

impl Command {
    /// The label shown on this command's control.
    const fn label(self) -> &'static str {
        match self {
            Self::OpenFolder => "Open folder…",
            Self::OpenIso => "Open ISO…",
            Self::Rescan => "Rescan",
            Self::SaveReport => "Save report…",
            Self::CopyReport => "Copy",
            Self::Settings => "Settings...",
            Self::ToggleTheme => "Theme",
        }
    }

    /// The message this command dispatches.
    const fn message(self) -> Message {
        match self {
            Self::OpenFolder => Message::OpenFolder,
            Self::OpenIso => Message::OpenIso,
            Self::Rescan => Message::Rescan,
            Self::SaveReport => Message::SaveReport,
            Self::CopyReport => Message::CopyReport,
            Self::Settings => Message::OpenSettings,
            Self::ToggleTheme => Message::ToggleTheme,
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
    /// The structural scan for `input` finished (or failed with a message).
    Listed { input: Input, result: Result<Structural, String> },
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
    /// The Settings dialog's OK — apply + persist the draft.
    SettingsOk,
    /// The Settings dialog's Cancel (or a scrim click) — discard the draft.
    SettingsCancel,
    /// The theme toggle pressed — cycle Auto → Light → Dark.
    ToggleTheme,
    /// The OS reported (or changed) its light/dark setting.
    OsTheme(iced::theme::Mode),
    /// An animation frame — advances the indeterminate "scanning" bar.
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
    /// The geometry readout for the closing window arrived — persist
    /// everything and actually close (`BDInfo` saves at `FormClosing` too).
    SaveAndClose {
        /// The closing window.
        id: iced::window::Id,
        /// Its outer position — `None` on Wayland (no global positions).
        position: Option<iced::Point>,
        /// Its logical size.
        size: iced::Size,
    },
}

impl App {
    /// The current palette, resolved from the preference and the OS mode.
    const fn palette(&self) -> Palette {
        self.theme_pref.resolve(self.os_mode)
    }

    /// The window title (an iced title function over the app state).
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
    const fn style(&self, _theme: &iced::Theme) -> iced::theme::Style {
        self.palette().base_style()
    }

    /// Whether a [`Command`] is currently actionable (drives its enabled state).
    fn command_enabled(&self, command: Command) -> bool {
        match command {
            Command::ToggleTheme => true,
            // Browsing / re-scanning / reconfiguring is disabled while a scan
            // is in flight (`BDInfo` disables its Settings button then too).
            Command::OpenFolder | Command::OpenIso | Command::Settings => !self.is_busy(),
            Command::Rescan => !self.is_busy() && self.flow.current_input().is_some(),
            Command::SaveReport | Command::CopyReport => self.flow.report_available(),
        }
    }

    /// The pure-ish message dispatch: each arm applies a Tier-A transition and
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
            Message::FolderPicked(Some(path)) => self.begin_listing(Input::Folder(path)),
            Message::IsoPicked(Some(path)) => self.begin_listing(Input::Iso(path)),
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
            Message::Listed { input, result } => self.on_listed(&input, result),
            Message::RowToggled(index) => {
                self.flow.toggle(index);
                Task::none()
            }
            Message::RowActivated(index) => self.activate_row(index),
            Message::RowHovered(region, index) => {
                self.hovered = Some((region, index));
                Task::none()
            }
            Message::RowUnhovered(region, index) => {
                if self.hovered == Some((region, index)) {
                    self.hovered = None;
                }
                Task::none()
            }
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
            Message::SelectAll => {
                self.flow.select_all();
                Task::none()
            }
            Message::SelectNone => {
                self.flow.select_none();
                Task::none()
            }
            Message::ScanSelected => self.start_scan(),
            Message::Progress { generation, file, done, total } => {
                let elapsed = self.scan_start.map_or(Duration::ZERO, |start| start.elapsed());
                self.flow.progress(generation, file, done, total, elapsed);
                Task::none()
            }
            Message::Finished { generation, report, errors, playlists } => {
                self.on_finished(generation, report, errors, playlists)
            }
            Message::ScanFailed { generation, error } => {
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
            Message::SaveDest(Some(dir)) => {
                self.save_report(&dir);
                Task::none()
            }
            Message::CopyReport => self.copy_report(),
            Message::DismissNotice => {
                self.notice = None;
                Task::none()
            }
            message @ (Message::OpenSettings
            | Message::SettingsToggled(_)
            | Message::SettingsSeconds(_)
            | Message::SettingsOk
            | Message::SettingsCancel) => {
                self.on_settings(message);
                Task::none()
            }
            Message::ToggleTheme => self.toggle_theme(),
            Message::OsTheme(mode) => {
                self.os_mode = mode;
                Task::none()
            }
            Message::Tick => {
                self.pulse =
                    self.pulse.wrapping_add(PULSE_STEP).checked_rem(PULSE_PERIOD).unwrap_or(0);
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
            Message::CloseRequested(id) => close_with_geometry(id),
            Message::SaveAndClose { id, position, size } => self.save_and_close(id, position, size),
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

    /// Handles the Settings dialog's messages: open a fresh draft, edit it
    /// (checkboxes / the digits-only seconds field), OK (apply + persist), or
    /// Cancel (discard). A stray edit with no open dialog changes nothing.
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
            };
            *slot = !*slot;
        }
    }

    /// The Settings dialog's OK: persist the draft and hand the new view
    /// settings to the flow — a filter change re-derives the table from the
    /// retained disc (no rescan), a display change re-renders the cells.
    fn apply_settings(&mut self) {
        if let Some(draft) = self.settings_draft.take() {
            draft.apply_to(&mut self.persisted);
            settings::save(&self.persisted);
            self.flow.set_view(ViewSettings::from_settings(&self.persisted));
        }
    }

    /// Cycles the theme preference (Auto → Light → Dark) and persists the
    /// choice, so it sticks across launches.
    fn toggle_theme(&mut self) -> Task<Message> {
        self.theme_pref = self.theme_pref.next();
        self.persisted.theme = self.theme_pref.choice();
        settings::save(&self.persisted);
        Task::none()
    }

    /// Persists the closing window's geometry (`BDInfo` saves at `FormClosing`
    /// too) and really closes it. The position stays untouched when the
    /// platform reports none (Wayland), so a size-only restore still works.
    fn save_and_close(
        &mut self,
        id: iced::window::Id,
        position: Option<iced::Point>,
        size: iced::Size,
    ) -> Task<Message> {
        self.persisted.window_size = Some((size.width, size.height));
        if let Some(point) = position {
            self.persisted.window_pos = Some((point.x, point.y));
        }
        settings::save(&self.persisted);
        iced::window::close(id)
    }

    /// Applies a column-divider drag: shifts weight across the held boundary by
    /// the cursor's movement since the last event, scaled by the stored column
    /// span so it tracks the cursor at any window size. Only the two columns the
    /// divider separates change; the rest hold, so far columns never shift.
    fn column_drag(&mut self, x: f32) {
        let Some((grid, boundary, cols_width, last_x)) = self.col_drag else { return };
        if let Some(prev_x) = last_x
            && cols_width > 0.0
        {
            let total: f32 = self.column_weights(grid).iter().sum();
            let delta = (x - prev_x) / cols_width * total;
            adjust_weight(self.column_weights_mut(grid), boundary, delta);
        }
        self.col_drag = Some((grid, boundary, cols_width, Some(x)));
    }

    /// The live column weights for `grid`.
    const fn column_weights(&self, grid: ColGrid) -> &[f32] {
        match grid {
            ColGrid::Shared => self.grid_w.as_slice(),
            ColGrid::Codec => self.codec_w.as_slice(),
        }
    }

    /// The live column weights for `grid`, mutable.
    const fn column_weights_mut(&mut self, grid: ColGrid) -> &mut [f32] {
        match grid {
            ColGrid::Shared => self.grid_w.as_mut_slice(),
            ColGrid::Codec => self.codec_w.as_mut_slice(),
        }
    }

    /// The live subscriptions: the OS light/dark change feed, plus — only while
    /// the initial scan is in flight — a per-frame tick that animates the
    /// indeterminate progress bar. The tick is off in every other state, so the
    /// window is not redrawing continuously when idle.
    fn subscription(&self) -> iced::Subscription<Message> {
        let mut subs = vec![iced::system::theme_changes().map(Message::OsTheme)];
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
        // Ctrl+C (Cmd+C on macOS, iced's `command` modifier) copies the
        // highlighted row's file path. Only events no widget consumed are
        // taken; the guard rules (busy / report view / what is highlighted)
        // live in the handler, so the subscription is a plain feed.
        subs.push(iced::event::listen_with(|event, status, _window| match event {
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. })
                if status == iced::event::Status::Ignored
                    && modifiers.command()
                    && matches!(key.as_ref(), iced::keyboard::Key::Character("c")) =>
            {
                Some(Message::CopyPath)
            }
            _ => None,
        }));
        if self.flow.stage() == Stage::Listing {
            subs.push(iced::window::frames().map(|_| Message::Tick));
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

    /// The indeterminate progress-bar fill while scanning — a 0→1→0 triangle over
    /// the animation phase, so the bar "breathes" rather than implying a measured
    /// percentage.
    fn pulse_fraction(&self) -> f32 {
        let phase = if self.pulse <= PULSE_HALF {
            self.pulse
        } else {
            PULSE_PERIOD.saturating_sub(self.pulse)
        };
        f32::from(phase) / f32::from(PULSE_HALF)
    }

    /// Whether a scan (initial or measured) is in flight — the source controls are
    /// disabled while one runs, like `BDInfo`.
    const fn is_busy(&self) -> bool {
        matches!(self.flow.stage(), Stage::Listing | Stage::Scanning)
    }

    /// Applies a finished structural scan. A successful listing becomes the
    /// remembered last path (saved now — `BDInfo` keeps `LastPath` the same
    /// way). In a debug build it also honours the capture harness's auto-scan
    /// hook (`BDINFO_GUI_SCAN=all`), selecting and scanning every playlist so
    /// a screenshot can show measured data.
    fn on_listed(&mut self, input: &Input, result: Result<Structural, String>) -> Task<Message> {
        let view = ViewSettings::from_settings(&self.persisted);
        self.flow = std::mem::take(&mut self.flow).listed(input, result, view);
        // Only when THIS input just became the listing (not a superseded or
        // failed result) does it become the remembered source.
        if self.flow.stage() == Stage::Listed && self.flow.current_input() == Some(input) {
            self.persisted.last_path = Some(input.path().to_path_buf());
            settings::save(&self.persisted);
        }
        #[cfg(debug_assertions)]
        if self.flow.stage() == Stage::Listed && debug_scan_all() {
            self.flow.select_all();
            return self.start_scan();
        }
        Task::none()
    }

    /// Cancels the in-flight measured scan for real: trips the worker's
    /// cooperative stop flag — the demux exits at its next read chunk, freeing
    /// CPU and IO — then returns the UI to the table. The worker's late "scan
    /// failed (cancelled)" message is dropped by the generation guard (the
    /// flow is no longer Scanning), so nothing else changes.
    fn cancel_scan(&mut self) -> Task<Message> {
        if let Some(cancel) = self.scan_cancel.take() {
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
            self.notice = Some(self.scan_outcome());
            if self.persisted.autosave_report {
                self.autosave_report();
            }
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

    /// Opens `path` — a dropped item or the boot argument — through the same
    /// classify-and-list road the pickers take ([`Input::classify`], the one
    /// Tier-A classifier both roads share). Ignored while a scan is in flight,
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

    /// Begins the initial scan of `input` on iced's executor — the bounded
    /// `Codecs` pass, which reads each stream file's head to fill the codec
    /// detail. It runs off the UI thread, so the "scanning" overlay animates while
    /// it works; the input is carried back so a superseded pick's result is
    /// discarded.
    fn begin_listing(&mut self, input: Input) -> Task<Message> {
        self.status = None;
        self.showing_report = false;
        self.notice = None;
        self.hovered = None;
        self.hovered_header = None;
        self.pane_selection = None;
        self.pulse = 0;
        self.flow = Flow::start_listing(input.clone());
        Task::perform(
            async move {
                #[cfg(debug_assertions)]
                debug_scan_delay();
                let result = scan::scan_structural(&input);
                (input, result)
            },
            |(input, result)| Message::Listed { input, result },
        )
    }

    /// Spawns the measured scan on a worker thread (so native demux keeps its
    /// `thread::scope` parallelism) and streams its progress + result back as
    /// messages. The UI thread stays free — only the channel is polled here.
    fn start_scan(&mut self) -> Task<Message> {
        let Some(ScanRequest { input, selection, scan_files, options }) = self.flow.scan_request()
        else {
            return Task::none();
        };
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.scan_start = Some(Instant::now());
        self.status = None;
        self.showing_report = false;
        self.notice = None;
        self.pane_selection = None;
        // A fresh cancel flag per scan: the Cancel button sets it, the demux
        // polls it per read chunk. Cancelling a superseded scan can never
        // touch this one — it holds its own flag.
        let cancel = Arc::new(AtomicBool::new(false));
        self.scan_cancel = Some(Arc::clone(&cancel));

        let (sender, receiver) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            // The scan fires its progress callback every few MB — tens of thousands
            // of times on a feature disc. Coalesce to one message per ~100 ms (plus
            // every file boundary) so the UI re-renders a few times a second, not
            // thousands; the CLI throttles its terminal redraw the same way.
            let mut last_sent: Option<Instant> = None;
            let mut last_file = String::new();
            let mut on_progress = |progress: ScanProgress<'_>| {
                let file_changed = progress.file != last_file;
                let due = file_changed
                    || progress.done == progress.total
                    || last_sent.is_none_or(|at| at.elapsed() >= Duration::from_millis(100));
                if !due {
                    return;
                }
                last_sent = Some(Instant::now());
                if file_changed {
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
        });

        self.flow = std::mem::take(&mut self.flow).start_scanning(generation);
        // The receiver stream ends when the worker drops its sender, completing
        // this task; each item arrives as a `Message`.
        Task::stream(receiver)
    }

    /// Writes the rendered report into `dir` as `BDINFO.{label}.txt`, bytes
    /// verbatim (the locked CRLF / UTF-8 no-BOM contract — never re-encode).
    fn save_report(&mut self, dir: &std::path::Path) {
        let (Some(report), Some(label)) = (self.flow.report(), self.flow.label()) else {
            return;
        };
        let target = dir.join(format!("BDINFO.{label}.txt"));
        self.status = match std::fs::write(&target, report.as_bytes()) {
            Ok(()) => Some(format!("Report saved to: {}", target.display())),
            Err(err) => Some(format!("Save failed: {err}")),
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
                iced::clipboard::write(path)
            }
            None => Task::none(),
        }
    }

    /// Copies the report to the clipboard via iced's clipboard task.
    fn copy_report(&mut self) -> Task<Message> {
        match self.flow.report() {
            Some(report) => {
                self.status = Some("Report copied to the clipboard.".to_owned());
                iced::clipboard::write(report)
            }
            None => Task::none(),
        }
    }

    /// The scan-complete confirmation text — a clean success, or a note that the
    /// resilient scan recorded some errors but still wrote the report.
    fn scan_outcome(&self) -> String {
        let errors = self.flow.report_errors().len();
        if errors == 0 {
            "Scan completed successfully.".to_owned()
        } else {
            format!("Scan completed with {errors} error(s).\nThe report below was still written.")
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
            .push(self.toolbar(p))
            .push(self.source_bar(p))
            .push(container(body).width(Length::Fill).height(Length::Fill).padding(ui::GAP_4))
            .push(self.bottom_bar(p));

        let base = container(content).width(Length::Fill).height(Length::Fill).into();
        if self.flow.stage() == Stage::Listing {
            return scanning_modal(base, scanning_card(p));
        }
        if let Some(draft) = &self.settings_draft {
            return modal(base, settings_card(p, draft), Message::SettingsCancel);
        }
        match &self.notice {
            Some(notice) => modal(base, notice_card(p, notice), Message::DismissNotice),
            None => base,
        }
    }

    /// The top toolbar: the brand mark + wordmark on the left, the theme toggle
    /// (the `View` menu seam) on the right.
    fn toolbar(&self, p: Palette) -> Element<'_, Message> {
        let bar = Row::new()
            .width(Length::Fill)
            .align_y(Vertical::Center)
            .spacing(ui::GAP_2)
            .push(brand_mark(p))
            .push(wordmark(p))
            .push(Space::new().width(Length::Fill))
            .push(self.theme_toggle(p));
        container(bar)
            .width(Length::Fill)
            .padding([ui::GAP_2, ui::GAP_4])
            .style(ui::toolbar(p))
            .into()
    }

    /// The theme toggle button, naming the current preference (Auto/Light/Dark).
    fn theme_toggle(&self, p: Palette) -> Element<'_, Message> {
        let label = format!("Theme: {}", self.theme_pref.label());
        button(text(label).size(ui::TEXT_SM).font(ui::UI_MEDIUM))
            .padding([ui::GAP_1, ui::GAP_2])
            .style(ui::toolbar_button(p))
            .on_press(Command::ToggleTheme.message())
            .into()
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
                .wrapping(text::Wrapping::None),
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
            .push(self.labeled_command_button(p, "Rescan", Command::Rescan))
            .push(self.command_button(p, Command::Settings));
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
                    .align_x(Horizontal::Center),
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
            Region::Playlist => pane_grid::Content::new(self.playlist_table(p))
                .title_bar(pane_grid::TitleBar::new(self.select_bar(p))),
            Region::StreamFile => pane_grid::Content::new(data_table(
                p,
                STREAM_FILE_COLS,
                &self.grid_w,
                ColGrid::Shared,
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
            ColGrid::Shared,
            self.grid_w.to_vec(),
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
            rows = rows.push(table_row(p, &row_data, editable, hovered, &self.grid_w));
        }
        let body = scrollable(rows).width(Length::Fill).height(Length::Fill);

        container(
            Column::new()
                .width(Length::Fill)
                .height(Length::Fill)
                .push(header)
                .push(rule)
                .push(body),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(ui::table_frame(p))
        .into()
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
            let value = format!("{} bytes ({})", group_n0(size), format_file_size(size));
            lines = lines.push(info_line(p, "Disc Size:", value, true));
        }

        if self.flow.show_hidden_note() {
            lines = lines.push(text(HIDDEN_NOTE).size(ui::TEXT_XS).color(p.text_muted));
        }

        container(lines)
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
                    .color(p.text),
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
            let body = scrollable(
                container(
                    text(report)
                        .font(ui::MONO)
                        .size(ui::TEXT_SM)
                        .color(p.text)
                        .wrapping(text::Wrapping::None),
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
            // The initial scan has no measurable %, so the bar breathes.
            Stage::Listing => self.pulse_fraction(),
            _ => 0.0,
        };

        // The lead line: the drop affordance while a file is held over the
        // window (and a drop would be accepted), else a transient status, else
        // the current scan file, else a hint.
        let lead = if self.drop_hover && !self.is_busy() {
            "Drop to open the disc folder or .iso image.".to_owned()
        } else {
            self.status.clone().unwrap_or_else(|| match self.flow.stage() {
                Stage::Scanning => progress.map_or_else(
                    || "Starting scan…".to_owned(),
                    |pr| format!("Scanning {}…", pr.file()),
                ),
                Stage::Listing => "Scanning disc…".to_owned(),
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
            .push(text(lead).size(ui::TEXT_SM).color(p.text_muted).width(Length::Fill))
            .push(text(times).size(ui::TEXT_SM).font(ui::MONO).color(p.text_muted));

        let bar = progress_bar(0.0..=1.0, fraction)
            .length(Length::Fill)
            .girth(Length::Fixed(8.0))
            .style(ui::progress(p));

        let actions = if scanning {
            Row::new().width(Length::Fill).push(Space::new().width(Length::Fill)).push(
                button(text("Cancel").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                    .padding([ui::GAP_2, ui::GAP_4])
                    .style(ui::secondary_button(p))
                    .on_press(Message::Cancel),
            )
        } else {
            let mut right = Row::new().spacing(ui::GAP_2);
            if self.flow.report_available() {
                right = right.push(
                    button(text("View Report...").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                        .padding([ui::GAP_2, ui::GAP_4])
                        .style(ui::secondary_button(p))
                        .on_press(Message::ShowReport),
                );
            }
            right = right.push(
                button(text("Scan Bitrates").size(ui::TEXT_SM).font(ui::UI_SEMIBOLD))
                    .padding([ui::GAP_2, ui::GAP_4])
                    .style(ui::primary_button(p))
                    .on_press_maybe(self.flow.can_scan().then_some(Message::ScanSelected)),
            );
            Row::new().width(Length::Fill).push(Space::new().width(Length::Fill)).push(right)
        };

        let content =
            Column::new().width(Length::Fill).spacing(ui::GAP_2).push(top).push(bar).push(actions);
        container(content)
            .width(Length::Fill)
            .padding([ui::GAP_3, ui::GAP_4])
            .style(ui::bottom_bar(p))
            .into()
    }
}

/// The CLI's hidden-tracks footer note, shown verbatim below the table when any
/// listed playlist hides a stream.
const HIDDEN_NOTE: &str =
    "(*) Some playlists on this disc have hidden tracks. These tracks are marked with an asterisk.";

// ── stateless widget builders ────────────────────────────────────────────────

/// The 20 px brand "aperture" mark — a ring with a centre dot, mirroring the app
/// icon, composed from styled widgets so it renders identically on every backend.
fn brand_mark<'a>(p: Palette) -> Element<'a, Message> {
    aperture(p, 22.0, 2.0, 8.0)
}

/// The larger empty-state brand mark.
fn brand_mark_large<'a>(p: Palette) -> Element<'a, Message> {
    aperture(p, 56.0, 5.0, 18.0)
}

/// Builds the ring-and-dot aperture at `size`, with `ring` stroke width and a
/// `dot` diameter — the in-app echo of the window icon.
fn aperture<'a>(p: Palette, size: f32, ring: f32, dot: f32) -> Element<'a, Message> {
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
        .push(text(message.to_owned()).size(ui::TEXT_SM).color(p.text));
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
    // Fill the header band and centre vertically, so every column's label sits on
    // the same baseline whether or not it carries a resize grab.
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
/// comes from the live, user-resizable weights (see [`App::grid_w`] /
/// [`App::codec_w`]).
struct Col {
    /// The header label.
    label: &'static str,
    /// The horizontal alignment of the label + cells.
    align: Horizontal,
    /// Whether to render the cells in the monospace face.
    mono: bool,
}

/// The Playlist columns (the shared grid). The checkbox is a separate leading
/// column, so these are the five data columns only.
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

/// The "Stream File" pane columns — the SHARED grid, so they line up under the
/// playlist's (Length / Estimated / Measured on the same grid lines).
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
/// `BDInfo` shows while it reads a disc.
fn scanning_card<'a>(p: Palette) -> Element<'a, Message> {
    let card = Column::new()
        .align_x(Horizontal::Center)
        .spacing(ui::GAP_4)
        .push(brand_mark_large(p))
        .push(
            text("Please wait while we scan the disc...")
                .size(ui::TEXT_MD)
                .color(p.text)
                .align_x(Horizontal::Center),
        );
    container(card).padding(ui::GAP_6).max_width(440.0).style(ui::hero_card(p)).into()
}

/// The Settings dialog card — `BDInfo`'s `FormSettings`, narrowed to what is
/// safely in the GUI's own hands: the two playlist filters (+ the seconds
/// threshold), the two display toggles, the two report-section toggles, and
/// autosave, with OK / Cancel (Cancel discards the draft). The wording
/// mirrors the original's checkboxes.
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
            "Auto-save report on scan completion",
            draft.autosave_report,
            SettingsField::Autosave,
        ))
        .push(actions);
    container(card).padding(ui::GAP_6).style(ui::hero_card(p)).into()
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
        .push(text(value).size(ui::TEXT_XS).font(font).color(p.text).wrapping(text::Wrapping::None))
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
/// size, the same coordinates [`window_settings`] restores) and forwards it
/// to [`Message::SaveAndClose`]. The window stays open while the readout
/// round-trips (`exit_on_close_request` is off) — `SaveAndClose` then really
/// closes it.
fn close_with_geometry(id: iced::window::Id) -> Task<Message> {
    iced::window::position(id).then(move |position| {
        iced::window::size(id).map(move |size| Message::SaveAndClose { id, position, size })
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
