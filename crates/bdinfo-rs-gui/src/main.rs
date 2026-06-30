//! `bdinfo-rs-gui` — the native desktop window for the bdinfo-rs Blu-ray
//! analyzer.
//!
//! The thin iced (Tier-B) shell over the [`bdinfo_rs_gui`] library: it drives the
//! full two-phase flow — pick a disc FOLDER or `.iso`, fast structural scan →
//! selectable playlist table → measured scan on a worker thread with a live
//! progress bar → the rendered report, with Save / Copy. Every `update` arm calls
//! a Tier-A transition ([`flow::Flow`]) or seam ([`scan`]) and applies the result;
//! `view` is a thin projection of [`flow::Flow`]. The only impure thing the shell
//! owns is the measured-scan worker thread and the channel that streams its
//! progress back as messages.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bdinfo_rs_gui::flow::{Flow, ScanRequest, Stage};
use bdinfo_rs_gui::model::SelectableRow;
use bdinfo_rs_gui::scan::{self, Input, Structural};
use iced::widget::{
    Column, Row, button, checkbox, container, progress_bar, scrollable, table, text,
};
use iced::{Element, Font, Length, Task};

fn main() -> iced::Result {
    iced::application(App::default, App::update, App::view).title(window_title).run()
}

/// The app state — the Tier-A [`Flow`] the shell drives, plus the bits the
/// runtime side owns: the scan generation counter (stamped on the worker's
/// messages so a stale event is ignored), the scan start time (for the elapsed /
/// remaining estimate), and a transient status line (the saved-report / copied
/// note).
#[derive(Default)]
struct App {
    /// The flow state machine — `view` is a projection of it.
    flow: Flow,
    /// Bumped on each "Scan selected"; the in-flight scan's id.
    generation: u64,
    /// When the current measured scan started.
    scan_start: Option<Instant>,
    /// A one-line status (e.g. `Report saved to: …`), shown under the body.
    status: Option<String>,
}

/// Everything the UI can ask the runtime to do.
#[derive(Debug, Clone)]
enum Message {
    /// "Open folder…" pressed.
    OpenFolder,
    /// "Open ISO…" pressed.
    OpenIso,
    /// The folder dialog closed: `Some(path)` picked, `None` cancelled.
    FolderPicked(Option<PathBuf>),
    /// The `.iso` file dialog closed.
    IsoPicked(Option<PathBuf>),
    /// The structural scan for `input` finished (or failed with a message).
    Listed { input: Input, result: Result<Structural, String> },
    /// A table row's `Scan` checkbox toggled (0-based table position).
    RowToggled(usize),
    /// "Select all" pressed.
    SelectAll,
    /// "Select none" pressed.
    SelectNone,
    /// "Scan selected" pressed — start the measured scan.
    ScanSelected,
    /// A streamed scan-progress event from the worker thread.
    Progress { generation: u64, file: String, done: u64, total: u64 },
    /// The worker's measured scan finished.
    Finished { generation: u64, report: String, label: String, errors: Vec<String> },
    /// The worker's measured scan failed fatally.
    ScanFailed { generation: u64, error: String },
    /// "Cancel" pressed during a scan.
    Cancel,
    /// "Save report…" pressed — choose a destination folder.
    SaveReport,
    /// The save-destination dialog closed.
    SaveDest(Option<PathBuf>),
    /// "Copy" pressed — copy the report to the clipboard.
    CopyReport,
}

impl App {
    /// The pure-ish message dispatch: each arm applies a Tier-A transition and
    /// returns the side effect to run (a dialog, the scan worker, a clipboard
    /// write, or nothing).
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenFolder => Task::perform(pick_folder(), Message::FolderPicked),
            Message::OpenIso => Task::perform(pick_iso(), Message::IsoPicked),
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
            Message::Finished { generation, report, label, errors } => {
                self.flow =
                    std::mem::take(&mut self.flow).finished(generation, report, label, errors);
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
            Message::SaveReport => Task::perform(pick_folder(), Message::SaveDest),
            Message::SaveDest(Some(dir)) => {
                self.save_report(&dir);
                Task::none()
            }
            Message::CopyReport => self.copy_report(),
        }
    }

    /// Begins the structural scan of `input` on iced's executor (fast — no demux),
    /// carrying the input back so a superseded pick's result is discarded.
    fn begin_listing(&mut self, input: Input) -> Task<Message> {
        self.status = None;
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

        let (sender, receiver) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            // `ScanProgress.file` is borrowed; own it before sending. A closed
            // receiver (the user moved on) just makes the send fail — harmless.
            let result = scan::scan_measured(&input, &selection, &scan_files, &mut |progress| {
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

    /// Projects the flow state onto widgets: the always-present Open bar above a
    /// body that is the opening prompt, a scanning note, a fatal error, or the
    /// loaded disc view (table + controls + the state-specific lower pane), with
    /// the transient status line beneath.
    fn view(&self) -> Element<'_, Message> {
        let open = Row::new()
            .spacing(8.0)
            .push(button(text("Open folder…")).on_press(Message::OpenFolder))
            .push(button(text("Open ISO…")).on_press(Message::OpenIso));

        let body: Element<'_, Message> = match self.flow.stage() {
            Stage::Idle => text("Open a Blu-ray disc folder or a .iso image to begin.").into(),
            Stage::Listing => {
                text(format!("Scanning {}…", self.flow.input_display().unwrap_or_default())).into()
            }
            Stage::Failed => {
                text(format!("Scan failed: {}", self.flow.error_message().unwrap_or_default()))
                    .into()
            }
            Stage::Listed | Stage::Scanning | Stage::Reported => self.disc_view(),
        };

        let mut content = Column::new().spacing(16.0).push(open).push(body);
        if let Some(status) = &self.status {
            content = content.push(text(status.as_str()));
        }
        container(content).padding(20.0).width(Length::Fill).height(Length::Fill).into()
    }

    /// The loaded-disc view: warning banners, the selectable playlist table, the
    /// selection controls, and the lower pane (scan progress, or the report +
    /// Save/Copy).
    fn disc_view(&self) -> Element<'_, Message> {
        let mut content = Column::new().spacing(12.0);

        // A failed measured scan left a banner over the still-usable table; any
        // structural warnings (a corrupt playlist) sit alongside.
        if let Some(error) = self.flow.error_message() {
            content = content.push(text(format!("⚠ {error}")));
        }
        for warning in self.flow.warnings() {
            content = content.push(text(format!("⚠ {warning}")));
        }

        content = content.push(self.playlist_table());
        if self.flow.show_hidden_note() {
            content = content.push(text(HIDDEN_NOTE));
        }
        content = content.push(self.selection_controls());

        match self.flow.stage() {
            Stage::Scanning => content = content.push(self.scan_progress()),
            Stage::Reported => content = self.report_pane(content),
            _ => {}
        }
        content.into()
    }

    /// The playlist table with a leading `Scan` checkbox column. The checkboxes
    /// (and so the selection) are live only while the table is editable — they
    /// are inert while a scan is in flight.
    fn playlist_table(&self) -> Element<'_, Message> {
        let editable = self.flow.editable();
        let columns = [
            table::column(text("Scan"), move |row: SelectableRow| {
                let toggle = checkbox(row.selected);
                if editable {
                    toggle.on_toggle(move |_| Message::RowToggled(row.index))
                } else {
                    toggle
                }
            }),
            table::column(text("#"), |row: SelectableRow| text(row.cells.number)),
            table::column(text("Group"), |row: SelectableRow| text(row.cells.group)),
            table::column(text("Playlist File"), |row: SelectableRow| text(row.cells.file)),
            table::column(text("Length"), |row: SelectableRow| text(row.cells.length)),
            table::column(text("Estimated Bytes"), |row: SelectableRow| {
                text(row.cells.estimated_bytes)
            }),
        ];
        table::table(columns, self.flow.table()).into()
    }

    /// The "Select all / none" + "Scan selected (N)" control row. The select
    /// toggle and scan action are disabled in states where they do not apply.
    fn selection_controls(&self) -> Element<'_, Message> {
        let editable = self.flow.editable();
        let select = if self.flow.all_selected() {
            button(text("Select none")).on_press_maybe(editable.then_some(Message::SelectNone))
        } else {
            button(text("Select all")).on_press_maybe(editable.then_some(Message::SelectAll))
        };
        let scan = button(text(format!("Scan selected ({})", self.flow.selected_count())))
            .on_press_maybe(self.flow.can_scan().then_some(Message::ScanSelected));
        Row::new().spacing(8.0).push(select).push(scan).into()
    }

    /// The in-flight scan pane: the progress bar + text line, and a Cancel button.
    fn scan_progress(&self) -> Element<'_, Message> {
        let mut pane = Column::new().spacing(8.0);
        if let Some(progress) = self.flow.progress_view() {
            pane = pane.push(progress_bar(0.0..=1.0, progress.fraction()));
            pane = pane.push(text(progress.line()));
        } else {
            pane = pane.push(text("Starting scan…"));
        }
        pane.push(button(text("Cancel")).on_press(Message::Cancel)).into()
    }

    /// The report pane: a resilient "completed with errors" banner (when any
    /// failures were recorded), the Save / Copy actions, and the scrollable
    /// monospace report.
    fn report_pane<'a>(&'a self, content: Column<'a, Message>) -> Column<'a, Message> {
        let mut content = content;
        let errors = self.flow.report_errors();
        if !errors.is_empty() {
            content = content.push(text(format!(
                "⚠ Completed with {} error(s) — the report below was still written.",
                errors.len()
            )));
        }
        content = content.push(
            Row::new()
                .spacing(8.0)
                .push(button(text("Save report…")).on_press(Message::SaveReport))
                .push(button(text("Copy")).on_press(Message::CopyReport)),
        );
        if let Some(report) = self.flow.report() {
            content =
                content.push(scrollable(text(report).font(Font::MONOSPACE)).height(Length::Fill));
        }
        content
    }
}

/// The CLI's hidden-tracks footer note, shown verbatim below the table when any
/// listed playlist hides a stream.
const HIDDEN_NOTE: &str =
    "(*) Some playlists on this disc have hidden tracks. These tracks are marked with an asterisk.";

/// The window title (an iced title function over the app state).
fn window_title(_state: &App) -> String {
    "bdinfo-rs".to_owned()
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
