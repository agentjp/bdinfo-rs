//! The persistent GUI configuration.
//!
//! The app's small memory between launches: the window geometry, the last
//! opened source path, and the theme preference, stored as a flat
//! `key = value` text file (hand-rolled parser + writer, pure std — this
//! project hand-rolled a UDF reader; a key-value file is squarely in its
//! idiom). The store is a **convenience, never a dependency**: unknown keys
//! are ignored (a newer version's file loads fine), malformed lines and a
//! missing / corrupt / unreadable file all degrade to defaults silently, and
//! a failed save is swallowed — persistence must never crash or block the
//! app. Config content is local state, not disc data, but the same
//! hostile-input discipline applies to parsing it: no panics, no raw
//! indexing, values validated before use.
//!
//! The pure pieces — [`Settings::parse`] / [`Settings::render`] (a proptested
//! round-trip) and the per-platform path helpers — are the Tier-A surface;
//! [`load_from`] / [`save_to`] are the thin best-effort IO wrappers over them,
//! taking an explicit path so they stay testable. Resolving the *real* config
//! path from the process environment is the shell's job (`main.rs`): the one
//! env-bound read stays out of this module, so everything here is exercisable
//! against scratch paths.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The config file's key for the short-playlist filter switch.
const KEY_FILTER_SHORT: &str = "filter-short-playlists";
/// The config file's key for the short-playlist threshold (whole seconds).
const KEY_SHORT_SECONDS: &str = "short-playlist-seconds";
/// The config file's key for the looping-playlist filter switch.
const KEY_FILTER_LOOPS: &str = "filter-looping-playlists";
/// The config file's key for the human-readable size toggle.
const KEY_HUMAN_SIZES: &str = "human-readable-sizes";
/// The config file's key for the chapter-count display toggle.
const KEY_CHAPTER_COUNT: &str = "display-chapter-count";
/// The config file's key for the autosave-report switch.
const KEY_AUTOSAVE: &str = "autosave-report";
/// The config file's key for the report's stream-diagnostics sections.
const KEY_REPORT_DIAGNOSTICS: &str = "report-stream-diagnostics";
/// The config file's key for the report's quick-summary blocks.
const KEY_REPORT_SUMMARY: &str = "report-quick-summary";
/// The config file's key for the last opened source path.
const KEY_LAST_PATH: &str = "last-path";
/// The config file's key for the theme preference.
const KEY_THEME: &str = "theme";
/// The config file's key for the UI scale, in percent.
const KEY_UI_SCALE: &str = "ui-scale-percent";
/// The config file's keys for the window's logical size.
const KEY_WINDOW_WIDTH: &str = "window-width";
/// See [`KEY_WINDOW_WIDTH`].
const KEY_WINDOW_HEIGHT: &str = "window-height";
/// The config file's keys for the window's logical position.
const KEY_WINDOW_X: &str = "window-x";
/// See [`KEY_WINDOW_X`].
const KEY_WINDOW_Y: &str = "window-y";
/// The config file's key for the window's maximized state.
const KEY_WINDOW_MAXIMIZED: &str = "window-maximized";

/// The largest believable window axis, in logical pixels — a stored size or
/// position beyond this (a corrupt file, a minimized window's fake Win32
/// `-32000` position) is dropped rather than restored.
const MAX_AXIS: f32 = 16_384.0;

/// The largest accepted short-playlist threshold, in seconds (a full day) —
/// a stored or typed value beyond it clamps here rather than filtering every
/// playlist off a disc with a nonsense number.
pub const MAX_SHORT_SECONDS: u32 = 86_400;

/// The smallest accepted UI scale, in percent of the platform-native scale.
///
/// The 50–200 range is deliberately tighter than the reference apps'
/// (Sniffnet allows 30–300%, Halloy 10–300%): 50% fully compensates the
/// worst real fractional-DPI wrongness (X11 reporting 2× for a 1× display)
/// — smaller only shrinks the 680-px minimum window under ~340 physical
/// pixels — and 200% covers both the inverse compensation (1× reported for
/// a `HiDPI` panel) and the accessibility zoom Windows itself tops out at
/// on standard displays; past 2× the portrait layout no longer fits even a
/// 2160-px-tall screen.
pub const MIN_UI_SCALE_PERCENT: u16 = 50;
/// The largest accepted UI scale, in percent — see [`MIN_UI_SCALE_PERCENT`].
pub const MAX_UI_SCALE_PERCENT: u16 = 200;

/// The UI-scale ladder, in percent.
///
/// The Settings dialog offers exactly these as chips and the keyboard
/// shortcut (Ctrl+'+' / Ctrl+'-', via [`scale_step`]) walks them. A
/// hand-edited config may hold any percent within the bounds; the chips
/// then simply show no pick until one is chosen.
pub const UI_SCALE_PRESETS: &[u16] = &[50, 75, 100, 125, 150, 200];

/// The persisted theme preference — the storable form of the shell's
/// System / Light / Dark cycle.
///
/// The shell maps this onto its own `ThemePref`, which carries the
/// iced-facing resolve logic. Defaults to following the OS, exactly like a
/// fresh install.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    /// Follow the operating system's light/dark setting.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

impl ThemeChoice {
    /// The stored token for this choice.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Parses a stored token — `None` for anything unrecognised (the caller
    /// keeps the default; a corrupt value never fails the load).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

/// Everything the GUI persists between launches — plain data, one field per
/// remembered fact.
///
/// `Default` reproduces today's fresh-launch behaviour exactly (no geometry,
/// no path, follow-the-OS theme, the standard filtered table with grouped
/// bytes and the chapter suffix, no autosave), so a missing or corrupt
/// config file changes nothing.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is a distinct persisted on/off setting, mirroring BDInfo's"
)]
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The window's logical size, `(width, height)`, in the app's own
    /// logical units — what layout sees, independent of both the OS DPI and
    /// [`ui_scale_percent`](Self::ui_scale_percent): iced multiplies both
    /// back in at restore exactly as it divided them out at the close-time
    /// readout (verified symmetric; see `save_and_close` in the shell).
    pub window_size: Option<(f32, f32)>,
    /// The window's logical outer position, `(x, y)` — absent on Wayland,
    /// which has no global window positions.
    pub window_pos: Option<(f32, f32)>,
    /// The window was maximized at close (`BDInfo`'s `WindowState`). The
    /// geometry pair above stays the *normal* (restore) bounds — a maximized
    /// close never overwrites it — so unmaximizing after a restore returns
    /// to the remembered size, like `BDInfo`'s `RestoreBounds`.
    pub window_maximized: bool,
    /// The last successfully opened source (disc folder or `.iso`).
    pub last_path: Option<PathBuf>,
    /// The theme preference.
    pub theme: ThemeChoice,
    /// The UI scale in percent (100 = the platform-native scale), applied
    /// through the application scale factor ON TOP of the OS DPI — the
    /// user-facing answer to X11/fractional-DPI wrongness, doubling as an
    /// accessibility zoom. Clamped to
    /// [`MIN_UI_SCALE_PERCENT`]..=[`MAX_UI_SCALE_PERCENT`] on load.
    pub ui_scale_percent: u16,
    /// Drop playlists shorter than [`short_playlist_seconds`](Self::short_playlist_seconds)
    /// from the table (`BDInfo`'s `FilterShortPlaylists`).
    pub filter_short_playlists: bool,
    /// The short-playlist threshold, in whole seconds
    /// (`BDInfo`'s `FilterShortPlaylistsValue`).
    pub short_playlist_seconds: u32,
    /// Drop looping playlists from the table (`BDInfo`'s
    /// `FilterLoopingPlaylists`).
    pub filter_looping_playlists: bool,
    /// Render the table's size cells human-readable (`72.64 GB`) instead of
    /// thousands-grouped bytes (`BDInfo`'s `SizeFormatHR`).
    pub human_readable_sizes: bool,
    /// Append the ` [NN Chapters]` suffix in the Playlist File column
    /// (`BDInfo`'s `DisplayChapterCount`).
    pub display_chapter_count: bool,
    /// Write the report unprompted when a measured scan completes
    /// (`BDInfo`'s `AutosaveReport`).
    pub autosave_report: bool,
    /// Render the report's `STREAM DIAGNOSTICS` sections (`BDInfo`'s
    /// `GenerateStreamDiagnostics`). Off trims the report for pasting.
    pub report_stream_diagnostics: bool,
    /// Render the report's `QUICK SUMMARY` blocks (`BDInfo`'s
    /// `GenerateTextSummary`).
    pub report_quick_summary: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            window_size: None,
            window_pos: None,
            window_maximized: false,
            last_path: None,
            theme: ThemeChoice::default(),
            // Native scale — a fresh install renders exactly as before the
            // setting existed.
            ui_scale_percent: 100,
            // The filter defaults are core's (on at 20 s) — today's table.
            filter_short_playlists: true,
            short_playlist_seconds: 20,
            filter_looping_playlists: true,
            // Two deliberate divergences from BDInfo's own defaults, chosen so
            // a fresh config renders this GUI's table exactly as before the
            // Settings dialog existed: BDInfo defaults SizeFormatHR ON (we
            // ship grouped bytes) and DisplayChapterCount OFF (we show the
            // chapter suffix).
            human_readable_sizes: false,
            display_chapter_count: true,
            autosave_report: false,
            // Both report sections on — BDInfo's defaults, and the locked
            // default report bytes.
            report_stream_diagnostics: true,
            report_quick_summary: true,
        }
    }
}

impl Settings {
    /// Parses the config file's text. Tolerant by design: unknown keys are
    /// ignored (forward compatibility), malformed lines are skipped, and an
    /// out-of-range or non-finite number is dropped — whatever the bytes,
    /// this returns a usable value and never panics.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        Self::parse_reporting(text).0
    }

    /// [`parse`](Self::parse), also returning the unknown keys it ignored
    /// (first-seen order, deduplicated) — the loader warns about them so a
    /// `theem = dark` typo is diagnosable, while the parse itself stays
    /// exactly as tolerant as before: reporting, not validation.
    #[must_use]
    pub fn parse_reporting(text: &str) -> (Self, Vec<String>) {
        let mut settings = Self::default();
        let mut unknown: Vec<String> = Vec::new();
        let (mut width, mut height, mut x, mut y) = (None, None, None, None);
        for line in text.lines() {
            let Some((raw_key, raw_value)) = line.split_once('=') else {
                continue; // not a `key = value` line — skipped
            };
            let key = raw_key.trim();
            // The writer puts exactly one space after `=`; strip exactly that
            // one, so a value's own leading/trailing spaces round-trip.
            let value = raw_value.strip_prefix(' ').unwrap_or(raw_value);
            match key {
                // A path with a stray carriage return could never be
                // re-rendered onto one line, so it is treated as corrupt.
                // (The check lives INSIDE the arm — a guard would fall
                // through and misreport this known key as unknown.)
                KEY_LAST_PATH => {
                    if !value.is_empty() && !value.contains('\r') {
                        settings.last_path = Some(PathBuf::from(value));
                    }
                }
                KEY_THEME => {
                    if let Some(theme) = ThemeChoice::parse(value.trim()) {
                        settings.theme = theme;
                    }
                }
                KEY_UI_SCALE => {
                    if let Some(percent) = parse_ui_scale(value) {
                        settings.ui_scale_percent = percent;
                    }
                }
                KEY_WINDOW_WIDTH => width = parse_axis(value),
                KEY_WINDOW_HEIGHT => height = parse_axis(value),
                KEY_WINDOW_X => x = parse_axis(value),
                KEY_WINDOW_Y => y = parse_axis(value),
                KEY_WINDOW_MAXIMIZED => set_bool(&mut settings.window_maximized, value),
                KEY_FILTER_SHORT => set_bool(&mut settings.filter_short_playlists, value),
                KEY_SHORT_SECONDS => {
                    if let Some(seconds) = parse_seconds(value) {
                        settings.short_playlist_seconds = seconds;
                    }
                }
                KEY_FILTER_LOOPS => set_bool(&mut settings.filter_looping_playlists, value),
                KEY_HUMAN_SIZES => set_bool(&mut settings.human_readable_sizes, value),
                KEY_CHAPTER_COUNT => set_bool(&mut settings.display_chapter_count, value),
                KEY_AUTOSAVE => set_bool(&mut settings.autosave_report, value),
                KEY_REPORT_DIAGNOSTICS => {
                    set_bool(&mut settings.report_stream_diagnostics, value);
                }
                KEY_REPORT_SUMMARY => set_bool(&mut settings.report_quick_summary, value),
                // Unknown key — a newer version's setting (or a typo);
                // ignored as ever, but reported (once per key) so the caller
                // can say so. An EMPTY key is malformed junk, not a nameable
                // setting; a repeat falls through to the silent arm.
                key if !key.is_empty() && !unknown.iter().any(|seen| seen == key) => {
                    unknown.push(key.to_owned());
                }
                _ => {}
            }
        }
        // Geometry is restored only whole: a lone width (or a width whose
        // height was corrupt) restores nothing rather than something skewed.
        settings.window_size = width.zip(height).filter(|&(w, h)| w >= 1.0 && h >= 1.0);
        settings.window_pos = x.zip(y);
        (settings, unknown)
    }

    /// Renders the config file's text: one `key = value` line per stored
    /// fact, sorted by key (deterministic output), `\n` endings. Absent
    /// fields write no line, and a value that cannot live on one line (a
    /// non-UTF-8 or control-character path) is simply not persisted.
    #[must_use]
    pub fn render(&self) -> String {
        let mut entries: Vec<(&str, String)> = vec![
            (KEY_THEME, self.theme.as_str().to_owned()),
            (KEY_UI_SCALE, self.ui_scale_percent.to_string()),
            (KEY_FILTER_SHORT, self.filter_short_playlists.to_string()),
            (KEY_SHORT_SECONDS, self.short_playlist_seconds.to_string()),
            (KEY_FILTER_LOOPS, self.filter_looping_playlists.to_string()),
            (KEY_HUMAN_SIZES, self.human_readable_sizes.to_string()),
            (KEY_CHAPTER_COUNT, self.display_chapter_count.to_string()),
            (KEY_AUTOSAVE, self.autosave_report.to_string()),
            (KEY_REPORT_DIAGNOSTICS, self.report_stream_diagnostics.to_string()),
            (KEY_REPORT_SUMMARY, self.report_quick_summary.to_string()),
            (KEY_WINDOW_MAXIMIZED, self.window_maximized.to_string()),
        ];
        if let Some(path) = self.last_path.as_deref().and_then(Path::to_str)
            && !path.is_empty()
            && !path.contains(['\n', '\r'])
        {
            entries.push((KEY_LAST_PATH, path.to_owned()));
        }
        if let Some((width, height)) = self.window_size {
            entries.push((KEY_WINDOW_WIDTH, width.to_string()));
            entries.push((KEY_WINDOW_HEIGHT, height.to_string()));
        }
        if let Some((x, y)) = self.window_pos {
            entries.push((KEY_WINDOW_X, x.to_string()));
            entries.push((KEY_WINDOW_Y, y.to_string()));
        }
        entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut out = String::new();
        for (key, value) in entries {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(&value);
            out.push('\n');
        }
        out
    }
}

/// Parses one geometry number: finite and within the believable range, else
/// `None` (dropped, never clamped into pretend-validity).
fn parse_axis(value: &str) -> Option<f32> {
    value.trim().parse::<f32>().ok().filter(|v| v.is_finite() && v.abs() <= MAX_AXIS)
}

/// Parses a short-playlist threshold: a whole number of seconds, clamped to
/// [`MAX_SHORT_SECONDS`]; `None` for anything non-numeric (the caller keeps
/// its current value).
fn parse_seconds(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok().map(|v| v.min(MAX_SHORT_SECONDS))
}

/// Parses a UI-scale percent: a whole number, clamped into the
/// [`MIN_UI_SCALE_PERCENT`]..=[`MAX_UI_SCALE_PERCENT`] bounds; `None` for
/// anything non-numeric — including a number past `u16` — so a corrupt value
/// keeps the 100% default.
fn parse_ui_scale(value: &str) -> Option<u16> {
    value.trim().parse::<u16>().ok().map(|v| v.clamp(MIN_UI_SCALE_PERCENT, MAX_UI_SCALE_PERCENT))
}

/// Steps the UI scale one [`UI_SCALE_PRESETS`] entry up or down — the
/// Ctrl+'+' / Ctrl+'-' move.
///
/// Holds at the bound preset when there is nothing further; an off-ladder
/// (hand-edited) value snaps to the nearest preset in the direction of
/// travel.
#[must_use]
pub fn scale_step(current: u16, up: bool) -> u16 {
    if up {
        UI_SCALE_PRESETS
            .iter()
            .copied()
            .find(|&preset| preset > current)
            .unwrap_or(MAX_UI_SCALE_PERCENT)
    } else {
        UI_SCALE_PRESETS
            .iter()
            .copied()
            .rev()
            .find(|&preset| preset < current)
            .unwrap_or(MIN_UI_SCALE_PERCENT)
    }
}

/// Applies one stored boolean: the exact `true` / `false` tokens set the
/// slot; any other value keeps the default (a corrupt value never fails the
/// load).
fn set_bool(slot: &mut bool, value: &str) {
    match value.trim() {
        "true" => *slot = true,
        "false" => *slot = false,
        _ => {}
    }
}

/// The window size to launch with.
///
/// The stored size, each axis clamped up to `floor` (the window's minimum
/// size — a remembered size below it must not open an unusably small window),
/// or `fallback` when nothing is stored. [`Settings::parse`] already
/// validated the stored range; this is the UI's own floor, applied where the
/// window is built.
#[must_use]
pub fn launch_size(
    stored: Option<(f32, f32)>,
    fallback: (f32, f32),
    floor: (f32, f32),
) -> (f32, f32) {
    stored.map_or(fallback, |(width, height)| (width.max(floor.0), height.max(floor.1)))
}

// ── the Settings dialog's working copy ───────────────────────────────────────

/// The Settings dialog's draft — the checkbox states plus the short-playlist
/// threshold **as typed**.
///
/// The seconds field can sit empty or half-edited without touching the real
/// settings. OK applies the draft ([`Draft::apply_to`]); Cancel just drops
/// the value — the OK/Cancel semantics of `BDInfo`'s `FormSettings`.
#[expect(
    clippy::struct_excessive_bools,
    reason = "one bool per dialog checkbox, mirroring BDInfo's FormSettings"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// The Theme picker (Auto / Light / Dark). The shell previews the pick
    /// live while the dialog is open; Cancel drops the draft and the preview
    /// with it.
    pub theme: ThemeChoice,
    /// The UI-scale picker (percent chips), previewed live like the theme.
    pub ui_scale_percent: u16,
    /// The "Filter short playlists" checkbox.
    pub filter_short_playlists: bool,
    /// The seconds field's text, kept digits-only by [`sanitize_seconds`].
    pub seconds_text: String,
    /// The "Filter playlists that contain loops" checkbox.
    pub filter_looping_playlists: bool,
    /// The "Stream sizes in human readable format" checkbox.
    pub human_readable_sizes: bool,
    /// The "Display chapter count in Playlist view" checkbox.
    pub display_chapter_count: bool,
    /// The "Auto-save report on scan completion" checkbox.
    pub autosave_report: bool,
    /// The "Include stream diagnostics in report" checkbox.
    pub report_stream_diagnostics: bool,
    /// The "Include quick text summary in report" checkbox.
    pub report_quick_summary: bool,
}

impl Draft {
    /// A fresh draft mirroring the current settings — what opening the
    /// dialog edits.
    #[must_use]
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            theme: settings.theme,
            ui_scale_percent: settings.ui_scale_percent,
            filter_short_playlists: settings.filter_short_playlists,
            seconds_text: settings.short_playlist_seconds.to_string(),
            filter_looping_playlists: settings.filter_looping_playlists,
            human_readable_sizes: settings.human_readable_sizes,
            display_chapter_count: settings.display_chapter_count,
            autosave_report: settings.autosave_report,
            report_stream_diagnostics: settings.report_stream_diagnostics,
            report_quick_summary: settings.report_quick_summary,
        }
    }

    /// Applies the draft to `settings` — the dialog's OK. The seconds text is
    /// parsed defensively: a valid number lands (clamped to
    /// [`MAX_SHORT_SECONDS`]); an empty or unparsable field keeps the prior
    /// threshold, exactly like `BDInfo`'s `int.TryParse` guard.
    pub fn apply_to(&self, settings: &mut Settings) {
        settings.theme = self.theme;
        settings.ui_scale_percent = self.ui_scale_percent;
        settings.filter_short_playlists = self.filter_short_playlists;
        settings.filter_looping_playlists = self.filter_looping_playlists;
        settings.human_readable_sizes = self.human_readable_sizes;
        settings.display_chapter_count = self.display_chapter_count;
        settings.autosave_report = self.autosave_report;
        settings.report_stream_diagnostics = self.report_stream_diagnostics;
        settings.report_quick_summary = self.report_quick_summary;
        if let Some(seconds) = parse_seconds(&self.seconds_text) {
            settings.short_playlist_seconds = seconds;
        }
    }
}

/// Sanitizes the seconds field on every edit: digits only, capped at five
/// characters.
///
/// Typed junk is dropped at the edge, so the field never holds anything
/// [`Draft::apply_to`] could not parse (except emptiness, which keeps the
/// prior value).
#[must_use]
pub fn sanitize_seconds(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).take(5).collect()
}

// ── where the file lives ─────────────────────────────────────────────────────

/// The directory + file name under the platform's config base.
const APP_DIR: &str = "bdinfo-rs";
/// See [`APP_DIR`].
const FILE_NAME: &str = "gui.conf";

/// Windows: `%APPDATA%\bdinfo-rs\gui.conf`. `None` without a usable
/// `APPDATA` (run without persistence).
#[must_use]
pub fn windows_config_path(appdata: Option<&OsStr>) -> Option<PathBuf> {
    let base = appdata.filter(|v| !v.is_empty())?;
    Some(Path::new(base).join(APP_DIR).join(FILE_NAME))
}

/// macOS: `~/Library/Application Support/bdinfo-rs/gui.conf`. `None` without
/// a usable `HOME`.
#[must_use]
pub fn macos_config_path(home: Option<&OsStr>) -> Option<PathBuf> {
    let base = home.filter(|v| !v.is_empty())?;
    Some(Path::new(base).join("Library").join("Application Support").join(APP_DIR).join(FILE_NAME))
}

/// Linux (and other unix): `$XDG_CONFIG_HOME/bdinfo-rs/gui.conf`, falling
/// back to `~/.config/bdinfo-rs/gui.conf`.
///
/// Per the XDG base-directory spec a relative `XDG_CONFIG_HOME` is invalid
/// and ignored (checked with [`Path::has_root`], which equals `is_absolute`
/// on unix and keeps this helper testable on any host). `None` without a
/// usable base.
#[must_use]
pub fn unix_config_path(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(base) =
        xdg_config_home.filter(|v| !v.is_empty()).map(Path::new).filter(|p| p.has_root())
    {
        return Some(base.join(APP_DIR).join(FILE_NAME));
    }
    let base = home.filter(|v| !v.is_empty())?;
    Some(Path::new(base).join(".config").join(APP_DIR).join(FILE_NAME))
}

// ── best-effort IO ───────────────────────────────────────────────────────────

/// Loads the settings from `path`.
///
/// Any failure — missing file, permission error, garbage bytes — degrades to
/// defaults; invalid UTF-8 is read lossily so the intact lines still count.
/// Unknown keys still load fine, but each earns one log line so a typo is
/// diagnosable.
#[must_use]
pub fn load_from(path: &Path) -> Settings {
    std::fs::read(path).map_or_else(
        |_| Settings::default(),
        |bytes| {
            let (settings, unknown) = Settings::parse_reporting(&String::from_utf8_lossy(&bytes));
            for key in unknown {
                log::warn!("config: ignoring unknown setting key `{key}`");
            }
            settings
        },
    )
}

/// Saves the settings to `path`, creating the parent directory on first save.
///
/// Best-effort: every IO failure is swallowed — but logged with its swallow
/// site, so a settings write that didn't stick leaves a trace.
pub fn save_to(path: &Path, settings: &Settings) {
    use crate::diagnostics::LogErr as _;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir).log_err("config-directory create");
    }
    let _ = std::fs::write(path, settings.render()).log_err("settings save");
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use super::{Settings, ThemeChoice};

    #[test]
    fn defaults_are_a_fresh_launch() {
        let settings = Settings::default();
        assert_eq!(settings.window_size, None);
        assert_eq!(settings.window_pos, None);
        assert!(!settings.window_maximized);
        assert_eq!(settings.last_path, None);
        assert_eq!(settings.theme, ThemeChoice::System);
        assert_eq!(settings.ui_scale_percent, 100);
        // Today's exact table: the standard filter (on at 20 s), grouped
        // bytes, the chapter suffix shown, no autosave.
        assert!(settings.filter_short_playlists);
        assert_eq!(settings.short_playlist_seconds, 20);
        assert!(settings.filter_looping_playlists);
        assert!(!settings.human_readable_sizes);
        assert!(settings.display_chapter_count);
        assert!(!settings.autosave_report);
        // Both report sections on — the locked default report bytes.
        assert!(settings.report_stream_diagnostics);
        assert!(settings.report_quick_summary);
    }

    #[test]
    fn a_full_file_round_trips() {
        let settings = Settings {
            window_size: Some((880.0, 960.5)),
            window_pos: Some((-120.25, 64.0)),
            window_maximized: true,
            last_path: Some(PathBuf::from(r"D:\rips\MY_DISC")),
            theme: ThemeChoice::Dark,
            ui_scale_percent: 150,
            filter_short_playlists: false,
            short_playlist_seconds: 0,
            filter_looping_playlists: false,
            human_readable_sizes: true,
            display_chapter_count: false,
            autosave_report: true,
            report_stream_diagnostics: false,
            report_quick_summary: false,
        };
        let text = settings.render();
        assert_eq!(Settings::parse(&text), settings);
        // Keys come out sorted — deterministic output.
        let keys: Vec<&str> =
            text.lines().filter_map(|line| line.split_once(" = ").map(|(k, _)| k)).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let text = "future-key = 42\ntheme = light\nchart-style = bars\n";
        let settings = Settings::parse(text);
        assert_eq!(settings.theme, ThemeChoice::Light);
        assert_eq!(settings.window_size, None);
    }

    #[test]
    fn unknown_keys_are_reported_once_each_in_file_order() {
        // The ignored keys come back for the loader to warn about —
        // first-seen order, duplicates collapsed, known keys and malformed
        // lines (no `=`, empty key) never reported.
        let text = "future-key = 42\ntheme = light\nchart-style = bars\n\
                    future-key = 43\nnot a key value line\n= empty key\n\
                    last-path = \n";
        let (settings, unknown) = Settings::parse_reporting(text);
        assert_eq!(settings.theme, ThemeChoice::Light);
        // `last-path` is a KNOWN key whose empty value was dropped — dropped
        // values are not unknown keys.
        assert_eq!(unknown, vec!["future-key".to_owned(), "chart-style".to_owned()]);
        // A fully-known file reports nothing.
        assert_eq!(Settings::parse_reporting("theme = dark\n").1, Vec::<String>::new());
    }

    #[test]
    fn malformed_lines_degrade_to_defaults() {
        // No panic and no partial nonsense from any of these shapes.
        let text = "no separator here\n= empty key\nwindow-width = not a number\n\
                    window-height = \ntheme = purple\nlast-path = \n\
                    filter-short-playlists = maybe\nshort-playlist-seconds = -3\n\
                    filter-looping-playlists = 1\nhuman-readable-sizes = TRUE\n\
                    display-chapter-count = \nautosave-report = yes\n\
                    window-maximized = maybe\n";
        assert_eq!(Settings::parse(text), Settings::default());
        assert_eq!(Settings::parse(""), Settings::default());
        assert_eq!(Settings::parse("\u{0}\u{FFFD}garbage\u{7F}"), Settings::default());
    }

    #[test]
    fn the_report_settings_parse_their_exact_tokens() {
        let text = "filter-short-playlists = false\nshort-playlist-seconds = 45\n\
                    filter-looping-playlists = false\nhuman-readable-sizes = true\n\
                    display-chapter-count = false\nautosave-report = true\n\
                    report-stream-diagnostics = false\nreport-quick-summary = false\n";
        let settings = Settings::parse(text);
        assert!(!settings.filter_short_playlists);
        assert_eq!(settings.short_playlist_seconds, 45);
        assert!(!settings.filter_looping_playlists);
        assert!(settings.human_readable_sizes);
        assert!(!settings.display_chapter_count);
        assert!(settings.autosave_report);
        assert!(!settings.report_stream_diagnostics);
        assert!(!settings.report_quick_summary);
    }

    #[test]
    fn a_stored_ui_scale_parses_clamps_and_rejects_junk() {
        // A percent inside the bounds lands as-is — presets and hand-edited
        // off-ladder values alike.
        assert_eq!(Settings::parse("ui-scale-percent = 125\n").ui_scale_percent, 125);
        assert_eq!(Settings::parse("ui-scale-percent = 137\n").ui_scale_percent, 137);
        // Numeric but absurd clamps into the bounds (never an unusable UI).
        assert_eq!(
            Settings::parse("ui-scale-percent = 10\n").ui_scale_percent,
            super::MIN_UI_SCALE_PERCENT
        );
        assert_eq!(
            Settings::parse("ui-scale-percent = 999\n").ui_scale_percent,
            super::MAX_UI_SCALE_PERCENT
        );
        // Non-numeric (a fraction, junk, past u16) keeps the 100% default.
        assert_eq!(Settings::parse("ui-scale-percent = 1.25\n").ui_scale_percent, 100);
        assert_eq!(Settings::parse("ui-scale-percent = large\n").ui_scale_percent, 100);
        assert_eq!(Settings::parse("ui-scale-percent = 70000\n").ui_scale_percent, 100);
        assert_eq!(Settings::parse("ui-scale-percent = -50\n").ui_scale_percent, 100);
    }

    #[test]
    fn scale_step_walks_the_presets_and_holds_at_the_bounds() {
        // From a preset: one step in each direction.
        assert_eq!(super::scale_step(100, true), 125);
        assert_eq!(super::scale_step(100, false), 75);
        // The bounds hold — no step past the ladder's ends.
        assert_eq!(super::scale_step(super::MAX_UI_SCALE_PERCENT, true), 200);
        assert_eq!(super::scale_step(super::MIN_UI_SCALE_PERCENT, false), 50);
        // An off-ladder (hand-edited) value snaps to the nearest preset in
        // the direction of travel.
        assert_eq!(super::scale_step(137, true), 150);
        assert_eq!(super::scale_step(137, false), 125);
        // Total even outside the clamped range (defence in depth).
        assert_eq!(super::scale_step(10, true), 50);
        assert_eq!(super::scale_step(10, false), 50);
        assert_eq!(super::scale_step(400, true), 200);
        assert_eq!(super::scale_step(400, false), 200);
    }

    #[test]
    fn a_stored_threshold_clamps_to_the_sane_bound() {
        // Numeric but absurd clamps (never filters a disc to nothing by
        // accident); non-numeric keeps the default.
        let clamped = Settings::parse("short-playlist-seconds = 4000000000\n");
        assert_eq!(clamped.short_playlist_seconds, super::MAX_SHORT_SECONDS);
        assert_eq!(Settings::parse("short-playlist-seconds = 20.5\n").short_playlist_seconds, 20);
        assert_eq!(Settings::parse("short-playlist-seconds = 0\n").short_playlist_seconds, 0);
    }

    #[test]
    fn geometry_is_restored_only_whole_and_sane() {
        // A lone width restores nothing.
        assert_eq!(Settings::parse("window-width = 800\n").window_size, None);
        // Non-finite and out-of-range axes are dropped, taking the pair with them.
        assert_eq!(Settings::parse("window-width = NaN\nwindow-height = 600\n").window_size, None);
        assert_eq!(Settings::parse("window-width = inf\nwindow-height = 600\n").window_size, None);
        assert_eq!(
            Settings::parse("window-width = 99999\nwindow-height = 600\n").window_size,
            None
        );
        // A sub-pixel size is nonsense; Win32's minimized -32000 position is too.
        assert_eq!(Settings::parse("window-width = 0\nwindow-height = 600\n").window_size, None);
        assert_eq!(Settings::parse("window-x = -32000\nwindow-y = -32000\n").window_pos, None);
        // A negative position (a monitor left of the primary) is legitimate.
        assert_eq!(
            Settings::parse("window-x = -1920\nwindow-y = 32\n").window_pos,
            Some((-1920.0, 32.0))
        );
    }

    #[test]
    fn the_maximized_flag_parses_its_exact_tokens() {
        assert!(Settings::parse("window-maximized = true\n").window_maximized);
        assert!(!Settings::parse("window-maximized = false\n").window_maximized);
        // Anything else keeps the default — a corrupt value never fails the load.
        assert!(!Settings::parse("window-maximized = TRUE\n").window_maximized);
        assert!(!Settings::parse("window-maximized = 1\n").window_maximized);
        // The flag is orthogonal to the geometry: it restores alone.
        let alone = Settings::parse("window-maximized = true\n");
        assert_eq!(alone.window_size, None);
        assert_eq!(alone.window_pos, None);
    }

    #[test]
    fn launch_size_clamps_to_the_floor_and_falls_back() {
        let floor = (680.0, 540.0);
        let fallback = (880.0, 960.0);
        // Nothing stored: the built-in default size.
        assert_eq!(super::launch_size(None, fallback, floor), fallback);
        // A sane stored size restores as-is.
        assert_eq!(super::launch_size(Some((1000.0, 800.0)), fallback, floor), (1000.0, 800.0));
        // Each axis clamps up to the window minimum independently.
        assert_eq!(super::launch_size(Some((100.0, 800.0)), fallback, floor), (680.0, 800.0));
        assert_eq!(super::launch_size(Some((1000.0, 100.0)), fallback, floor), (1000.0, 540.0));
    }

    #[test]
    fn the_theme_tokens_round_trip_and_reject_junk() {
        for theme in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
            assert_eq!(ThemeChoice::parse(theme.as_str()), Some(theme));
        }
        assert_eq!(ThemeChoice::parse("Dark"), None); // tokens are exact
        assert_eq!(ThemeChoice::parse(""), None);
    }

    #[test]
    fn a_path_value_keeps_its_spaces_and_equals_signs() {
        // Split on the FIRST `=`: a path containing `=` survives, and so do
        // leading/trailing spaces (one separator space belongs to the writer).
        let settings = Settings {
            last_path: Some(PathBuf::from(" /discs/name = weird/BDMV ")),
            ..Settings::default()
        };
        assert_eq!(Settings::parse(&settings.render()), settings);
    }

    #[test]
    fn an_unrepresentable_path_is_not_persisted() {
        // A newline could never live on one `key = value` line; the writer
        // drops the entry rather than corrupting the file.
        let settings = Settings { last_path: Some(PathBuf::from("a\nb")), ..Settings::default() };
        assert_eq!(Settings::parse(&settings.render()).last_path, None);
    }

    // ── the dialog draft ─────────────────────────────────────────────────────

    #[test]
    fn a_draft_mirrors_and_applies_the_settings() {
        let mut settings = Settings {
            short_playlist_seconds: 30,
            theme: ThemeChoice::Light,
            ui_scale_percent: 125,
            ..Settings::default()
        };
        let mut draft = super::Draft::from_settings(&settings);
        assert_eq!(draft.seconds_text, "30");
        assert_eq!(draft.theme, ThemeChoice::Light);
        assert_eq!(draft.ui_scale_percent, 125);
        assert!(draft.filter_short_playlists);
        assert!(draft.report_stream_diagnostics);
        assert!(draft.report_quick_summary);
        // Edit every field and apply: all of it lands.
        draft.theme = ThemeChoice::Dark;
        draft.ui_scale_percent = 150;
        draft.filter_short_playlists = false;
        draft.seconds_text = "0".to_owned();
        draft.filter_looping_playlists = false;
        draft.human_readable_sizes = true;
        draft.display_chapter_count = false;
        draft.autosave_report = true;
        draft.report_stream_diagnostics = false;
        draft.report_quick_summary = false;
        draft.apply_to(&mut settings);
        assert_eq!(settings.theme, ThemeChoice::Dark);
        assert_eq!(settings.ui_scale_percent, 150);
        assert!(!settings.filter_short_playlists);
        assert_eq!(settings.short_playlist_seconds, 0);
        assert!(!settings.filter_looping_playlists);
        assert!(settings.human_readable_sizes);
        assert!(!settings.display_chapter_count);
        assert!(settings.autosave_report);
        assert!(!settings.report_stream_diagnostics);
        assert!(!settings.report_quick_summary);
    }

    #[test]
    fn an_unparsable_seconds_field_keeps_the_prior_threshold() {
        let mut settings = Settings { short_playlist_seconds: 30, ..Settings::default() };
        let mut draft = super::Draft::from_settings(&settings);
        // An emptied field is not a zero — BDInfo's TryParse guard.
        draft.seconds_text = String::new();
        draft.apply_to(&mut settings);
        assert_eq!(settings.short_playlist_seconds, 30);
        // A typed-over-the-top value clamps to the bound.
        draft.seconds_text = "99999".to_owned();
        draft.apply_to(&mut settings);
        assert_eq!(settings.short_playlist_seconds, super::MAX_SHORT_SECONDS);
    }

    #[test]
    fn sanitize_seconds_keeps_digits_only_and_caps_the_length() {
        assert_eq!(super::sanitize_seconds("20"), "20");
        assert_eq!(super::sanitize_seconds("2a0!x"), "20");
        assert_eq!(super::sanitize_seconds("-15"), "15"); // no sign, no negatives
        assert_eq!(super::sanitize_seconds(""), "");
        assert_eq!(super::sanitize_seconds("1234567890"), "12345"); // capped
        assert_eq!(super::sanitize_seconds("١٢٣"), ""); // ASCII digits only
    }

    // ── the per-platform path helpers (pure, host-independent) ──────────────

    /// An env-var value as the helpers take it (they all accept an `Option`).
    #[expect(clippy::unnecessary_wraps, reason = "mirrors the Option-taking helper signatures")]
    fn os(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }

    #[test]
    fn windows_path_hangs_off_appdata() {
        let path =
            super::windows_config_path(os(r"C:\Users\a\AppData\Roaming")).expect("a base resolves");
        assert_eq!(path, Path::new(r"C:\Users\a\AppData\Roaming").join("bdinfo-rs/gui.conf"));
        assert_eq!(super::windows_config_path(None), None);
        assert_eq!(super::windows_config_path(os("")), None);
    }

    #[test]
    fn macos_path_hangs_off_home() {
        let path = super::macos_config_path(os("/Users/a")).expect("a base resolves");
        assert_eq!(
            path,
            Path::new("/Users/a").join("Library/Application Support/bdinfo-rs/gui.conf")
        );
        assert_eq!(super::macos_config_path(None), None);
        assert_eq!(super::macos_config_path(os("")), None);
    }

    #[test]
    fn unix_path_prefers_a_rooted_xdg_config_home() {
        assert_eq!(
            super::unix_config_path(os("/xdg"), os("/home/a")),
            Some(Path::new("/xdg").join("bdinfo-rs/gui.conf"))
        );
        // A relative XDG_CONFIG_HOME is invalid per the spec — fall through.
        assert_eq!(
            super::unix_config_path(os("rel/config"), os("/home/a")),
            Some(Path::new("/home/a").join(".config/bdinfo-rs/gui.conf"))
        );
        assert_eq!(
            super::unix_config_path(None, os("/home/a")),
            Some(Path::new("/home/a").join(".config/bdinfo-rs/gui.conf"))
        );
        assert_eq!(super::unix_config_path(os(""), None), None);
    }

    // ── the best-effort IO wrappers ──────────────────────────────────────────

    /// A scratch path under the target dir (unique per test).
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/settings-tests");
        dir.join(name)
    }

    #[test]
    fn save_then_load_round_trips_and_creates_the_directory() {
        let path = scratch("round-trip/gui.conf");
        let _ = std::fs::remove_dir_all(path.parent().expect("a parent"));
        let settings = Settings {
            window_size: Some((700.0, 800.0)),
            theme: ThemeChoice::Light,
            ..Settings::default()
        };
        super::save_to(&path, &settings);
        assert_eq!(super::load_from(&path), settings);
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_as_defaults() {
        assert_eq!(super::load_from(Path::new("definitely/not/here.conf")), Settings::default());
        let path = scratch("corrupt.conf");
        super::save_to(&path, &Settings::default()); // ensure the dir exists
        std::fs::write(&path, [0xFF, 0xFE, 0x00, b'\n', 0x80, 0x81]).expect("scratch write");
        assert_eq!(super::load_from(&path), Settings::default());
        // Invalid UTF-8 around an intact line still yields that line.
        std::fs::write(&path, b"\xFF\xFE\ntheme = dark\n\x80").expect("scratch write");
        assert_eq!(super::load_from(&path).theme, ThemeChoice::Dark);
    }

    #[test]
    fn a_file_with_unknown_keys_still_loads_the_known_ones() {
        // The loader warns per unknown key (observability), but the load
        // itself stays exactly as tolerant as ever.
        let path = scratch("unknown-keys.conf");
        super::save_to(&path, &Settings::default()); // ensure the dir exists
        std::fs::write(&path, "future-key = 1\ntheme = dark\n").expect("scratch write");
        assert_eq!(super::load_from(&path).theme, ThemeChoice::Dark);
    }

    #[test]
    fn an_unwritable_destination_is_swallowed() {
        // The parent is a FILE, so neither create_dir_all nor write can
        // succeed — save_to must still return without error.
        let blocker = scratch("blocker");
        super::save_to(&blocker, &Settings::default()); // creates dir + file
        super::save_to(&blocker.join("child.conf"), &Settings::default());
        // A parentless path (the filesystem root) skips the directory step and
        // swallows the doomed write the same way.
        super::save_to(Path::new("/"), &Settings::default());
    }

    mod prop {
        use std::path::PathBuf;

        use proptest::prelude::*;

        use super::super::{MAX_AXIS, Settings, ThemeChoice};

        /// A believable size axis (whatever a real resize can produce).
        fn size_axis() -> impl Strategy<Value = f32> {
            1.0f32..=MAX_AXIS
        }

        /// A believable position axis (negative = a monitor left of primary).
        fn pos_axis() -> impl Strategy<Value = f32> {
            -MAX_AXIS..=MAX_AXIS
        }

        /// A representable stored path: non-empty valid Unicode without line
        /// breaks (a real path; `\r`/`\n` are the file format's one limit).
        fn path_string() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9 ./\\\\:=_-]{1,60}"
        }

        fn theme() -> impl Strategy<Value = ThemeChoice> {
            prop_oneof![
                Just(ThemeChoice::System),
                Just(ThemeChoice::Light),
                Just(ThemeChoice::Dark)
            ]
        }

        fn settings() -> impl Strategy<Value = Settings> {
            (
                proptest::option::of((size_axis(), size_axis())),
                proptest::option::of((pos_axis(), pos_axis())),
                proptest::option::of(path_string().prop_map(PathBuf::from)),
                theme(),
                super::super::MIN_UI_SCALE_PERCENT..=super::super::MAX_UI_SCALE_PERCENT,
                (any::<bool>(), 0..=super::super::MAX_SHORT_SECONDS, any::<bool>()),
                (any::<bool>(), any::<bool>(), any::<bool>()),
                (any::<bool>(), any::<bool>(), any::<bool>()),
            )
                .prop_map(
                    |(
                        window_size,
                        window_pos,
                        last_path,
                        theme,
                        ui_scale_percent,
                        (filter_short_playlists, short_playlist_seconds, filter_looping_playlists),
                        (human_readable_sizes, display_chapter_count, autosave_report),
                        (report_stream_diagnostics, report_quick_summary, window_maximized),
                    )| {
                        Settings {
                            window_size,
                            window_pos,
                            window_maximized,
                            last_path,
                            theme,
                            ui_scale_percent,
                            filter_short_playlists,
                            short_playlist_seconds,
                            filter_looping_playlists,
                            human_readable_sizes,
                            display_chapter_count,
                            autosave_report,
                            report_stream_diagnostics,
                            report_quick_summary,
                        }
                    },
                )
        }

        proptest! {
            // The store's core contract: everything written comes back
            // exactly (floats included — Rust renders the shortest string
            // that re-parses to the same value), and nothing the writer
            // emits is ever reported as an unknown key.
            #[test]
            fn every_settings_value_round_trips(settings in settings()) {
                let (parsed, unknown) = Settings::parse_reporting(&settings.render());
                prop_assert_eq!(parsed, settings);
                prop_assert_eq!(unknown, Vec::<String>::new());
            }

            // Hostile-input discipline: ANY text parses without panicking,
            // and what it parses to is stable through a render cycle (so a
            // partially-salvaged file re-saves losslessly).
            #[test]
            fn any_text_parses_and_restabilizes(text in any::<String>()) {
                let parsed = Settings::parse(&text);
                prop_assert_eq!(Settings::parse(&parsed.render()), parsed);
            }
        }
    }
}
