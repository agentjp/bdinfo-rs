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
//! progress back as messages, plus the OS-theme subscription.
#![forbid(unsafe_code)]
// The window is a leaf binary, not a library: a Windows release build hides the
// console so launching it does not flash a terminal. No effect on other targets.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod icon;
mod theme;
mod ui;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bdinfo_rs_core::bdrom::disc::PlaylistSummary;
use bdinfo_rs_gui::flow::{Flow, ScanRequest, Stage};
use bdinfo_rs_gui::model::SelectableRow;
use bdinfo_rs_gui::panes::{CodecRow, StreamFileRow};
use bdinfo_rs_gui::progress::ProgressModel;
use bdinfo_rs_gui::scan::{self, Input, Structural};
use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Column, Row, Space, Stack, button, container, mouse_area, progress_bar, scrollable, text,
};
use iced::{Element, Length, Task};

use crate::theme::{Palette, ThemePref};

/// The window's initial logical size — anchored on the classic `BDInfo`
/// footprint (854×661 client) but taller and wider, so the three stacked
/// master-detail panes get room while still fitting a standard ~900-tall work
/// area. The layout is fully responsive (`Fill`/`FillPortion` + scrollable
/// panes), so any size works.
const WINDOW_SIZE: (f32, f32) = (1080.0, 860.0);
/// The minimum size the window can be resized down to — low enough to fit a
/// small screen, beyond which the panes scroll rather than disappear (so the
/// window never gets stuck larger than a modest display).
const MIN_SIZE: (f32, f32) = (720.0, 520.0);
/// The window-icon resolution (procedurally drawn; winit downscales as needed).
const ICON_SIZE: u32 = 128;

fn main() -> iced::Result {
    iced::application(boot, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .style(App::style)
        // "Auto" tracks the desktop live: re-resolve the palette on every OS change.
        .subscription(|_app| iced::system::theme_changes().map(Message::OsTheme))
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
        .window(window_settings())
        .run()
}

/// Boots the app and fires a one-shot request for the OS theme, so the first
/// frame matches the desktop's light/dark setting.
fn boot() -> (App, Task<Message>) {
    (App::default(), iced::system::theme().map(Message::OsTheme))
}

/// The window settings: a sensible default + minimum size and the embedded app
/// icon (drawn procedurally, so the binary stays self-contained).
fn window_settings() -> iced::window::Settings {
    iced::window::Settings {
        size: iced::Size::new(WINDOW_SIZE.0, WINDOW_SIZE.1),
        min_size: Some(iced::Size::new(MIN_SIZE.0, MIN_SIZE.1)),
        icon: iced::window::icon::from_rgba(icon::rgba(ICON_SIZE), ICON_SIZE, ICON_SIZE).ok(),
        ..iced::window::Settings::default()
    }
}

/// The app state — the Tier-A [`Flow`] the shell drives, plus the runtime-side
/// bits: the visual preference + last-known OS mode, the scan generation counter
/// (stamped on the worker's messages so a stale event is ignored), the scan start
/// time (for the elapsed / remaining estimate), and a transient status line.
#[derive(Default)]
struct App {
    /// The flow state machine — `view` is a projection of it.
    flow: Flow,
    /// Follow-the-OS / pinned light / pinned dark.
    theme_pref: ThemePref,
    /// The last light/dark setting reported by the OS.
    os_mode: iced::theme::Mode,
    /// Bumped on each "Scan selected"; the in-flight scan's id.
    generation: u64,
    /// When the current measured scan started.
    scan_start: Option<Instant>,
    /// A one-line status (e.g. `Report saved to: …`), shown in the bottom bar.
    status: Option<String>,
    /// Whether the report view (over the panes) is showing.
    showing_report: bool,
    /// The "scan completed" confirmation, shown as a modal until dismissed.
    notice: Option<String>,
}

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
            Self::ToggleTheme => Message::ToggleTheme,
        }
    }
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
    /// The structural scan for `input` finished (or failed with a message).
    Listed { input: Input, result: Result<Structural, String> },
    /// A table row's scan checkbox toggled (0-based table position).
    RowToggled(usize),
    /// A table row clicked — make it the active (highlighted) playlist that the
    /// master-detail panes follow.
    RowActivated(usize),
    /// "Select all" pressed.
    SelectAll,
    /// "Select none" pressed.
    SelectNone,
    /// "Scan selected" pressed — start the measured scan.
    ScanSelected,
    /// A streamed scan-progress event from the worker thread.
    Progress { generation: u64, file: String, done: u64, total: u64 },
    /// The worker's measured scan finished.
    Finished {
        generation: u64,
        report: String,
        label: String,
        errors: Vec<String>,
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
    /// The theme toggle pressed — cycle Auto → Light → Dark.
    ToggleTheme,
    /// The OS reported (or changed) its light/dark setting.
    OsTheme(iced::theme::Mode),
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
            Command::OpenFolder | Command::OpenIso | Command::ToggleTheme => true,
            Command::Rescan => self.flow.current_input().is_some(),
            Command::SaveReport | Command::CopyReport => self.flow.report().is_some(),
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
            Message::Listed { input, result } => {
                self.flow = std::mem::take(&mut self.flow).listed(&input, result);
                Task::none()
            }
            Message::RowToggled(index) => {
                self.flow.toggle(index);
                Task::none()
            }
            Message::RowActivated(index) => {
                self.flow.set_active(index);
                Task::none()
            }
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
            Message::Finished { generation, report, label, errors, playlists } => {
                let was_scanning = self.flow.stage() == Stage::Scanning;
                self.flow = std::mem::take(&mut self.flow)
                    .finished(generation, report, label, errors, playlists);
                // Announce a clean finish only if this event actually completed the
                // in-flight scan (a stale finish leaves the stage unchanged).
                if was_scanning && self.flow.stage() == Stage::Reported {
                    self.notice = Some(self.scan_outcome());
                }
                Task::none()
            }
            Message::ScanFailed { generation, error } => {
                self.flow = std::mem::take(&mut self.flow).scan_failed(generation, error);
                Task::none()
            }
            Message::Cancel => {
                self.flow = std::mem::take(&mut self.flow).cancel();
                Task::none()
            }
            Message::ShowReport => {
                self.showing_report = self.flow.report().is_some();
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
            Message::ToggleTheme => {
                self.theme_pref = self.theme_pref.next();
                Task::none()
            }
            Message::OsTheme(mode) => {
                self.os_mode = mode;
                Task::none()
            }
        }
    }

    /// Begins the structural scan of `input` on iced's executor (fast — no demux),
    /// carrying the input back so a superseded pick's result is discarded.
    fn begin_listing(&mut self, input: Input) -> Task<Message> {
        self.status = None;
        self.showing_report = false;
        self.notice = None;
        self.flow = Flow::start_listing(input.clone());
        Task::perform(
            async move {
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
        let Some(ScanRequest { input, selection, scan_files }) = self.flow.scan_request() else {
            return Task::none();
        };
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.scan_start = Some(Instant::now());
        self.status = None;
        self.showing_report = false;
        self.notice = None;

        let (sender, receiver) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            // The scan fires its progress callback every few MB — tens of thousands
            // of times on a feature disc. Coalesce to one message per ~100 ms (plus
            // every file boundary) so the UI re-renders a few times a second, not
            // thousands; the CLI throttles its terminal redraw the same way.
            let mut last_sent: Option<Instant> = None;
            let mut last_file = String::new();
            let result = scan::scan_measured(&input, &selection, &scan_files, &mut |progress| {
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
            });
            let outcome = match result {
                Ok(measured) => Message::Finished {
                    generation,
                    report: measured.report,
                    label: measured.label,
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
        let (Some(report), Some(label)) = (self.flow.report(), self.flow.report_label()) else {
            return;
        };
        let target = dir.join(format!("BDINFO.{label}.txt"));
        self.status = match std::fs::write(&target, report.as_bytes()) {
            Ok(()) => Some(format!("Report saved to: {}", target.display())),
            Err(err) => Some(format!("Save failed: {err}")),
        };
    }

    /// Copies the report to the clipboard via iced's clipboard task.
    fn copy_report(&mut self) -> Task<Message> {
        match self.flow.report() {
            Some(report) => {
                let contents = report.to_owned();
                self.status = Some("Report copied to the clipboard.".to_owned());
                iced::clipboard::write(contents)
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
        let body: Element<'_, Message> = if self.showing_report && self.flow.report().is_some() {
            self.report_view(p)
        } else {
            match self.flow.stage() {
                Stage::Idle => self.empty_state(p),
                Stage::Listing => self.listing_state(p),
                Stage::Failed => self.failed_state(p),
                Stage::Listed | Stage::Scanning | Stage::Reported => self.disc_view(p),
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
        match &self.notice {
            Some(notice) => modal(base, notice_card(p, notice)),
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
            text(field_text.to_owned()).font(ui::MONO).size(ui::TEXT_SM).color(field_color),
        )
        .width(Length::Fill)
        .padding([ui::GAP_2, ui::GAP_3])
        .style(ui::path_field(p));

        let bar = Row::new()
            .width(Length::Fill)
            .align_y(Vertical::Center)
            .spacing(ui::GAP_2)
            .push(text("Source").size(ui::TEXT_SM).font(ui::UI_MEDIUM).color(p.text_muted))
            .push(field)
            .push(self.command_button(p, Command::OpenFolder))
            .push(self.command_button(p, Command::OpenIso))
            .push(self.command_button(p, Command::Rescan));
        container(bar).width(Length::Fill).padding([ui::GAP_3, ui::GAP_4]).into()
    }

    /// A secondary-styled button for a [`Command`], disabled when not actionable.
    fn command_button(&self, p: Palette, command: Command) -> Element<'_, Message> {
        let enabled = self.command_enabled(command);
        button(text(command.label()).size(ui::TEXT_SM).font(ui::UI_MEDIUM))
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

    /// The structural-scan-in-flight state: a quiet centred note.
    fn listing_state(&self, p: Palette) -> Element<'_, Message> {
        let path = self.flow.input_display().unwrap_or_default();
        let card = Column::new()
            .align_x(Horizontal::Center)
            .spacing(ui::GAP_2)
            .push(
                text("Reading disc structure…")
                    .size(ui::TEXT_MD)
                    .font(ui::UI_SEMIBOLD)
                    .color(p.text),
            )
            .push(text(path).size(ui::TEXT_SM).font(ui::MONO).color(p.text_muted));
        let framed = container(card).padding(ui::GAP_6).style(ui::hero_card(p));
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

    /// The loaded-disc body: the select bar, any banners, then the three
    /// master-detail panes (Playlists / Stream Files / Streams) and the
    /// detected-disc strip — the classic `BDInfo` layout.
    fn disc_view(&self, p: Palette) -> Element<'_, Message> {
        let mut content = Column::new().width(Length::Fill).height(Length::Fill).spacing(ui::GAP_2);
        content = content.push(self.select_bar(p));

        // A failed measured scan leaves a banner over the still-usable table; any
        // structural warnings sit alongside.
        if let Some(error) = self.flow.error_message() {
            content = content.push(banner(p, p.danger, error));
        }
        for warning in self.flow.warnings() {
            content = content.push(banner(p, p.warning, warning.as_str()));
        }

        content =
            content.push(pane(p, "Playlists", self.playlist_table(p), Length::FillPortion(5)));
        if self.flow.show_hidden_note() {
            content = content.push(text(HIDDEN_NOTE).size(ui::TEXT_XS).color(p.text_muted));
        }

        let files_title = self
            .flow
            .active_playlist_name()
            .map_or_else(|| "Stream Files".to_owned(), |name| format!("Stream Files — {name}"));
        content = content.push(pane(
            p,
            &files_title,
            data_table(p, STREAM_FILE_COLS, self.stream_file_table_rows()),
            Length::FillPortion(3),
        ));
        content = content.push(pane(
            p,
            "Streams",
            data_table(p, CODEC_COLS, self.codec_table_rows()),
            Length::FillPortion(4),
        ));

        content = content.push(self.info_strip(p));
        content.into()
    }

    /// The active playlist's stream-file rows, flattened to display cells.
    fn stream_file_table_rows(&self) -> Vec<Vec<String>> {
        self.flow
            .stream_file_rows()
            .into_iter()
            .map(|row: StreamFileRow| vec![row.file, row.index, row.length, row.size])
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

    /// The "Select Playlist(s)" control row: the select-all/none toggle and the
    /// count of what is selected.
    fn select_bar(&self, p: Palette) -> Element<'_, Message> {
        let editable = self.flow.editable();
        let toggle = if self.flow.all_selected() {
            button(text("Select none").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                .on_press_maybe(editable.then_some(Message::SelectNone))
        } else {
            button(text("Select all").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                .on_press_maybe(editable.then_some(Message::SelectAll))
        };
        let count = format!("{} of {} selected", self.flow.selected_count(), self.flow.row_count());
        Row::new()
            .width(Length::Fill)
            .align_y(Vertical::Center)
            .spacing(ui::GAP_3)
            .push(text("Playlists").size(ui::TEXT_SM).font(ui::UI_MEDIUM).color(p.text_muted))
            .push(toggle.padding([ui::GAP_2, ui::GAP_3]).style(ui::secondary_button(p)))
            .push(Space::new().width(Length::Fill))
            .push(text(count).size(ui::TEXT_SM).color(p.text_muted))
            .into()
    }

    /// The hand-rolled playlist table: a sharp data grid with a quiet header over
    /// scrollable selectable rows.
    fn playlist_table(&self, p: Palette) -> Element<'_, Message> {
        let header = container(table_header_row(p))
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
            rows = rows.push(table_row(p, &row_data, editable));
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

    /// The slim "detected disc" strip beneath the table — what `BDInfo` prints in
    /// its footer box (the source kind, the path, and the disc label).
    fn info_strip(&self, p: Palette) -> Element<'_, Message> {
        let (kind, path) =
            self.flow.current_input().map_or(("Disc", String::new()), |input| match input {
                Input::Folder(_) => ("Folder", input.display()),
                Input::Iso(_) => ("ISO", input.display()),
            });
        let label = self.flow.label().unwrap_or("—");
        let line = Row::new()
            .width(Length::Fill)
            .align_y(Vertical::Center)
            .spacing(ui::GAP_2)
            .push(text(kind).size(ui::TEXT_XS).font(ui::UI_MEDIUM).color(p.text_muted))
            .push(
                text(path)
                    .size(ui::TEXT_XS)
                    .font(ui::MONO)
                    .color(p.text_muted)
                    .wrapping(text::Wrapping::None),
            )
            .push(Space::new().width(Length::Fill))
            .push(text("Disc Label:").size(ui::TEXT_XS).font(ui::UI_MEDIUM).color(p.text_muted))
            .push(text(label.to_owned()).size(ui::TEXT_XS).font(ui::MONO).color(p.text));
        container(line)
            .width(Length::Fill)
            .padding([ui::GAP_2, ui::GAP_3])
            .style(ui::info_strip(p))
            .into()
    }

    /// The full report view (over the panes): a header with Back / Save / Copy,
    /// a "completed with errors" banner when any failures were recorded, and the
    /// scrollable monospace report in its readout panel.
    fn report_view(&self, p: Palette) -> Element<'_, Message> {
        let label = self.flow.report_label().unwrap_or("");
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
            column = column.push(
                container(scrollable(
                    container(text(report).font(ui::MONO).size(ui::TEXT_SM).color(p.text))
                        .padding(ui::GAP_3),
                ))
                .width(Length::Fill)
                .height(Length::Fill)
                .style(ui::report_pane(p)),
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
            _ => 0.0,
        };

        // The lead line: a transient status, else the current scan file, else a hint.
        let lead = self.status.clone().unwrap_or_else(|| match self.flow.stage() {
            Stage::Scanning => progress.map_or_else(
                || "Starting scan…".to_owned(),
                |pr| format!("Scanning {}…", pr.file()),
            ),
            Stage::Reported => "Report ready.".to_owned(),
            Stage::Listed if self.flow.selected_count() > 0 => "Ready to scan.".to_owned(),
            Stage::Listed => "Choose playlists to scan.".to_owned(),
            _ => String::new(),
        });

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
            if self.flow.report().is_some() {
                right = right.push(
                    button(text("View Report…").size(ui::TEXT_SM).font(ui::UI_MEDIUM))
                        .padding([ui::GAP_2, ui::GAP_4])
                        .style(ui::secondary_button(p))
                        .on_press(Message::ShowReport),
                );
            }
            right = right.push(
                button(
                    text(format!("Scan selected ({})", self.flow.selected_count()))
                        .size(ui::TEXT_SM)
                        .font(ui::UI_SEMIBOLD),
                )
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

/// The playlist-table header row, laid out on the same column grid as the body
/// rows (the leading accent-bar spacer, then the columns).
fn table_header_row<'a>(p: Palette) -> Element<'a, Message> {
    let cells = Row::new()
        .width(Length::Fill)
        .align_y(Vertical::Center)
        .push(header_cell(p, "", Length::Fixed(ui::COL_CHECK), Horizontal::Center))
        .push(header_cell(p, "#", Length::Fixed(ui::COL_NUM), Horizontal::Right))
        .push(header_cell(p, "Group", Length::Fixed(ui::COL_GROUP), Horizontal::Center))
        .push(header_cell(p, "Playlist File", Length::Fill, Horizontal::Left))
        .push(header_cell(p, "Length", Length::Fixed(ui::COL_LENGTH), Horizontal::Right))
        .push(header_cell(p, "Estimated Bytes", Length::Fixed(ui::COL_BYTES), Horizontal::Right))
        .push(header_cell(p, "Measured Bytes", Length::Fixed(ui::COL_MEASURED), Horizontal::Right));
    Row::new()
        .width(Length::Fill)
        .height(Length::Fill)
        .align_y(Vertical::Center)
        .push(Space::new().width(Length::Fixed(ui::SEL_BAR)))
        .push(cells)
        .into()
}

/// One header cell: a muted label in a fixed-width, aligned box.
fn header_cell(p: Palette, label: &str, width: Length, align: Horizontal) -> Element<'_, Message> {
    container(text(label).size(ui::TEXT_XS).font(ui::UI_MEDIUM).color(p.text_muted))
        .width(width)
        .align_x(align)
        .padding([0.0, ui::GAP_2])
        .into()
}

/// One playlist row: a leading accent bar (when active), then **two sibling
/// buttons** — the scan checkbox (toggles the row's selection) and the cells
/// (click to make the row active, driving the master-detail panes). They are
/// siblings, not nested, so each click hits exactly one. The **active** row
/// carries the amber wash; the checkbox shows the **checked** state.
fn table_row<'a>(p: Palette, row_data: &SelectableRow, editable: bool) -> Element<'a, Message> {
    let even = row_data.index.checked_rem(2) == Some(0);
    let index = row_data.index;
    let active = row_data.active;

    let checkbox = button(
        container(check_chip(p, row_data.selected)).center_x(Length::Fill).center_y(Length::Fill),
    )
    .width(Length::Fixed(ui::COL_CHECK))
    .height(Length::Fill)
    .padding(0.0)
    .style(ui::row_button(p, active, even))
    .on_press_maybe(editable.then_some(Message::RowToggled(index)));

    let mut file = Row::new()
        .push(text(row_data.cells.file.clone()).size(ui::TEXT_SM).font(ui::MONO).color(p.text));
    if row_data.has_hidden {
        file = file.push(text(" *").size(ui::TEXT_SM).font(ui::MONO).color(p.accent));
    }
    let cells = Row::new()
        .width(Length::Fill)
        .align_y(Vertical::Center)
        .push(num_cell(p, row_data.cells.number.clone(), ui::COL_NUM, Horizontal::Right))
        .push(num_cell(p, row_data.cells.group.clone(), ui::COL_GROUP, Horizontal::Center))
        .push(
            container(file).width(Length::Fill).align_x(Horizontal::Left).padding([0.0, ui::GAP_2]),
        )
        .push(num_cell(p, row_data.cells.length.clone(), ui::COL_LENGTH, Horizontal::Right))
        .push(num_cell(p, row_data.cells.estimated_bytes.clone(), ui::COL_BYTES, Horizontal::Right))
        .push(num_cell(
            p,
            row_data.cells.measured_bytes.clone(),
            ui::COL_MEASURED,
            Horizontal::Right,
        ));
    let cells_button = button(cells)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(0.0)
        .style(ui::row_button(p, active, even))
        .on_press_maybe(editable.then_some(Message::RowActivated(index)));

    Row::new()
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
        .push(cells_button)
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

/// A titled pane: a small muted header label over its (already-framed) table,
/// taking `height` of the vertical space.
fn pane<'a>(
    p: Palette,
    title: &str,
    table: Element<'a, Message>,
    height: Length,
) -> Element<'a, Message> {
    Column::new()
        .width(Length::Fill)
        .height(height)
        .spacing(ui::GAP_1)
        .push(text(title.to_owned()).size(ui::TEXT_SM).font(ui::UI_MEDIUM).color(p.text_muted))
        .push(table)
        .into()
}

/// One read-only data column: a fixed label, width, alignment, and whether its
/// cells render in the tabular monospace.
struct Col {
    /// The header label.
    label: &'static str,
    /// The column width.
    width: Length,
    /// The horizontal alignment of the label + cells.
    align: Horizontal,
    /// Whether to render the cells in the monospace face.
    mono: bool,
}

/// The "Stream Files" pane columns.
const STREAM_FILE_COLS: &[Col] = &[
    Col { label: "Stream File", width: Length::Fill, align: Horizontal::Left, mono: true },
    Col {
        label: "Index",
        width: Length::Fixed(ui::COL_INDEX),
        align: Horizontal::Right,
        mono: true,
    },
    Col {
        label: "Length",
        width: Length::Fixed(ui::COL_LENGTH),
        align: Horizontal::Right,
        mono: true,
    },
    Col {
        label: "Size",
        width: Length::Fixed(ui::COL_BYTES),
        align: Horizontal::Right,
        mono: true,
    },
];

/// The "Streams" (codec) pane columns.
const CODEC_COLS: &[Col] = &[
    Col {
        label: "Codec",
        width: Length::Fixed(ui::COL_CODEC),
        align: Horizontal::Left,
        mono: false,
    },
    Col {
        label: "Language",
        width: Length::Fixed(ui::COL_LANG),
        align: Horizontal::Left,
        mono: false,
    },
    Col {
        label: "Bit Rate",
        width: Length::Fixed(ui::COL_BITRATE),
        align: Horizontal::Right,
        mono: true,
    },
    Col { label: "Description", width: Length::Fill, align: Horizontal::Left, mono: false },
];

/// A read-only data table: a quiet header over zebra-striped rows. Each `rows`
/// entry supplies one cell string per column (extra cells are ignored); an empty
/// table shows a single dash.
fn data_table<'a>(
    p: Palette,
    cols: &'static [Col],
    rows: Vec<Vec<String>>,
) -> Element<'a, Message> {
    let mut header_cells = Row::new().width(Length::Fill).align_y(Vertical::Center);
    for col in cols {
        header_cells = header_cells.push(header_cell(p, col.label, col.width, col.align));
    }
    let header = container(header_cells)
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
        let mut cells = Row::new().width(Length::Fill).align_y(Vertical::Center);
        for (col, value) in cols.iter().zip(row) {
            let font = if col.mono { ui::MONO } else { ui::UI };
            cells = cells.push(
                container(
                    text(value)
                        .size(ui::TEXT_SM)
                        .font(font)
                        .color(p.text)
                        .wrapping(text::Wrapping::None),
                )
                .width(col.width)
                .align_x(col.align)
                .padding([0.0, ui::GAP_2]),
            );
        }
        body_col = body_col.push(
            container(cells)
                .width(Length::Fill)
                .padding([ui::GAP_2, 0.0])
                .style(ui::zebra(p, even)),
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

/// Overlays `card` (centred over a dim scrim) on `base` — the scan-complete
/// modal. Clicking the scrim dismisses it.
fn modal<'a>(base: Element<'a, Message>, card: Element<'a, Message>) -> Element<'a, Message> {
    let scrim = mouse_area(container(card).center(Length::Fill).style(ui::scrim()))
        .on_press(Message::DismissNotice);
    Stack::new().push(base).push(scrim).into()
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

/// One right/centre-aligned numeric cell in the table's tabular monospace.
fn num_cell<'a>(p: Palette, value: String, width: f32, align: Horizontal) -> Element<'a, Message> {
    container(text(value).size(ui::TEXT_SM).font(ui::MONO).color(p.text))
        .width(Length::Fixed(width))
        .align_x(align)
        .padding([0.0, ui::GAP_2])
        .into()
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
