//! `bdinfo-rs-gui` — the native desktop window for the bdinfo-rs Blu-ray
//! analyzer.
//!
//! The Phase-1 spike: pick a Blu-ray disc FOLDER, run the fast structural scan,
//! list its playlists in a table, and show the rendered text report. This is the
//! thin iced (Tier-B) shell — every message arm calls a seam / Tier-A function in
//! `bdinfo_rs_gui` and applies the result; it holds no business logic of its own.
//! Row selection, the measured scan, progress, `.iso`, and theming are Phases
//! 2–3.
#![forbid(unsafe_code)]

use std::path::PathBuf;

use bdinfo_rs_gui::model::TableRow;
use bdinfo_rs_gui::scan::{self, ScanData};
use iced::widget::{Column, button, container, scrollable, table, text};
use iced::{Element, Font, Length, Task};

fn main() -> iced::Result {
    iced::application(App::default, App::update, App::view).title(window_title).run()
}

/// The app's display state — a small machine the seam drives; `view` is a thin
/// projection of it.
#[derive(Debug, Default)]
struct App {
    /// The current screen.
    state: ViewState,
}

/// The screens the window moves through, one per message outcome.
#[derive(Debug, Default, Clone)]
enum ViewState {
    /// Nothing picked yet — the opening prompt.
    #[default]
    Idle,
    /// A scan is running for this folder.
    Scanning(PathBuf),
    /// A finished scan: the playlist table + rendered report.
    Loaded(ScanData),
    /// The pick or scan failed — a short message.
    Failed(String),
}

/// Everything the UI can ask the runtime to do.
#[derive(Debug, Clone)]
enum Message {
    /// The "Pick folder…" button was pressed.
    PickFolder,
    /// The folder dialog closed: `Some(path)` picked, `None` cancelled.
    FolderPicked(Option<PathBuf>),
    /// The structural scan finished (or failed with a message).
    Scanned(Result<ScanData, String>),
}

impl App {
    /// The pure-ish message dispatch: each arm sets the next state and returns
    /// the side effect to run (a dialog, a scan, or nothing).
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PickFolder => Task::perform(pick_folder_dialog(), Message::FolderPicked),
            Message::FolderPicked(None) => Task::none(),
            Message::FolderPicked(Some(path)) => {
                self.state = ViewState::Scanning(path.clone());
                // The structural scan is fast, so Phase 1 runs it on iced's
                // executor; the long measured scan moves to a worker in Phase 2.
                Task::perform(async move { scan::scan_folder(&path) }, Message::Scanned)
            }
            Message::Scanned(Ok(data)) => {
                self.state = ViewState::Loaded(data);
                Task::none()
            }
            Message::Scanned(Err(error)) => {
                self.state = ViewState::Failed(error);
                Task::none()
            }
        }
    }

    /// Projects the state onto widgets: the pick button above a body that is the
    /// opening prompt, a scanning note, an error, or the loaded table + report.
    fn view(&self) -> Element<'_, Message> {
        let pick = button(text("Pick folder…")).on_press(Message::PickFolder);
        let body: Element<'_, Message> = match &self.state {
            ViewState::Idle => {
                text("Pick a Blu-ray disc folder (the disc root or its BDMV folder).").into()
            }
            ViewState::Scanning(path) => text(format!("Scanning {}…", path.display())).into(),
            ViewState::Failed(error) => text(format!("Scan failed: {error}")).into(),
            ViewState::Loaded(data) => loaded_view(data),
        };
        let content = Column::new().spacing(16.0).push(pick).push(body);
        container(content).padding(20.0).width(Length::Fill).height(Length::Fill).into()
    }
}

/// The window title (an iced title function over the app state).
fn window_title(_state: &App) -> String {
    "bdinfo-rs".to_owned()
}

/// The CLI's hidden-tracks footer note, shown verbatim below the table when any
/// listed playlist hides a stream.
const HIDDEN_NOTE: &str =
    "(*) Some playlists on this disc have hidden tracks. These tracks are marked with an asterisk.";

/// The loaded screen: the playlist table, the optional hidden-tracks footer, and
/// the scrollable monospace report.
fn loaded_view(data: &ScanData) -> Element<'_, Message> {
    let columns = [
        table::column(text("#"), |row: TableRow| text(row.number)),
        table::column(text("Group"), |row: TableRow| text(row.group)),
        table::column(text("Playlist File"), |row: TableRow| text(row.file)),
        table::column(text("Length"), |row: TableRow| text(row.length)),
        table::column(text("Estimated Bytes"), |row: TableRow| text(row.estimated_bytes)),
    ];
    let playlists = table::table(columns, data.table_rows.clone());

    let mut content = Column::new().spacing(12.0).push(playlists);
    if data.show_hidden_note {
        content = content.push(text(HIDDEN_NOTE));
    }
    let report = scrollable(text(data.report.as_str()).font(Font::MONOSPACE)).height(Length::Fill);
    content.push(report).into()
}

/// Opens the native folder picker (rfd, pure-Rust xdg-portal backend on Linux)
/// and yields the chosen path, or `None` when cancelled.
async fn pick_folder_dialog() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Select a Blu-ray disc folder")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf())
}
