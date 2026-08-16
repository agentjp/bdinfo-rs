# bdinfo-rs-gui

**The native desktop app for [bdinfo-rs](../../README.md) — the classic BDInfo disc report, on
Windows, macOS, and Linux.**

Open a `BDMV` folder or `.iso`, pick playlists from the familiar three-pane view (Playlist / Stream
File / Codec), run the measured scan with live progress, then read, save, or copy the report. It is
**byte-identical** to the one the `bdinfo-rs` CLI produces.

Pure Rust — no webview, no bundled runtime, no C libraries — on x64 and arm64. Rendering is
GPU-accelerated through wgpu, falling back to software (tiny-skia) automatically when no usable GPU
is found.

## Installing

Every [`gui-v*` release](https://github.com/agentjp/bdinfo-rs/releases?q=%22native+desktop+GUI%22&expanded=true)
ships, for x64 and arm64:

- **Windows** — an `.msi` installer (Start-menu entry, clean uninstall) and a portable `.zip`.
  SmartScreen may warn on the portable exe's first run: "More info" → "Run anyway".
- **macOS** — a `.dmg`; drag the app to Applications. The app is unsigned: allow the first launch
  under System Settings → Privacy & Security → **Open Anyway**, or run
  `xattr -d com.apple.quarantine "/Applications/bdinfo-rs GUI.app"`.
- **Linux** — an AppImage (download, `chmod +x`, run) plus `.deb` and `.rpm` packages.

Each release carries `SHA256SUMS` and Sigstore build-provenance attestations
(`gh attestation verify <asset> --repo agentjp/bdinfo-rs`).

## Opening a disc

- **Double-click** the app. There is no installation or setup step; settings are created on first
  use.
- **Drag and drop** a disc folder or `.iso` onto the window.
- **From a terminal:** `bdinfo-rs-gui <path>` opens that disc at startup — the disc root, the `BDMV`
  folder, any directory inside it, or an `.iso`. That is the entire command-line surface; for
  scripting and reports without a window, use the `bdinfo-rs` CLI.

Reports are saved as `BDINFO.{volume label}.txt`, either through *Save report…* or automatically
beside the disc when *Auto-save report on scan completion* is enabled.

## Files it writes

One per-user directory, created on first use:

| Platform | Directory |
|---|---|
| Windows | `%APPDATA%\bdinfo-rs\` |
| macOS | `~/Library/Application Support/bdinfo-rs/` |
| Linux | `$XDG_CONFIG_HOME/bdinfo-rs/` (default `~/.config/bdinfo-rs/`) |

- **`gui.conf`** — settings and window geometry, saved as they change. **Deleting it is the full
  reset**: window size and position, theme, and every setting return to defaults. This is also the
  fix when the window restores off-screen, for example onto a monitor you have since unplugged.
- **`gui.log`** — a plain-text diagnostic log, one file per launch. It opens with the launch time
  and records which renderer was selected, the disc each scan opened, stalled reads, scan
  durations, any panics, and dialog or portal errors. Each launch renames the previous log to
  **`gui.log.1`** before starting a new one, so the run before the one you are in is still there.
  **Attach it when filing a bug** — it answers the first questions a graphics issue raises, and
  after a crash the interesting file is usually `gui.log.1`.

## Troubleshooting

Rendering problems are almost always the GPU driver stack, and two environment variables cover
nearly all of them.

| Symptom | Fix |
|---|---|
| Black icons, banding, flashing, or a crash at startup on an old GPU | `ICED_BACKEND=tiny-skia` — force software rendering |
| One graphics API is broken, but the GPU is fine | `WGPU_BACKEND=vulkan` (or `gl`, `dx12`, `metal`) |
| A hybrid-GPU laptop spins up the discrete GPU | `WGPU_POWER_PREF=low` — a disc analyzer does not need it |
| Blank window, missing decorations, compositor quirks (Wayland) | Unset `WAYLAND_DISPLAY` for the launch to run under X11/XWayland |
| Crash when the window goes wider than 2048 px (Raspberry Pi, Asahi) | `ICED_BACKEND=tiny-skia` — those drivers cap texture size at 2048 px |
| White window on Windows with the "Beta: Unicode UTF-8 worldwide language support" locale option | `ICED_BACKEND=tiny-skia`, or turn the option off (Control Panel → Region → Administrative) |
| White or blank window after a driver update, sleep/resume, or a monitor change | Restart the app — recovering a lost GPU device is an upstream renderer limitation |
| Everything is too small or too large (typically X11 fractional scaling) | Set UI scale in Settings (50–200%), or <kbd>Ctrl</kbd>+<kbd>+</kbd> / <kbd>Ctrl</kbd>+<kbd>-</kbd> |
| File-picker buttons do nothing on a minimal Linux setup | Install `xdg-desktop-portal` and a backend such as `xdg-desktop-portal-gtk`, or `zenity` |
| Non-Latin disc titles render as boxes | Install a font covering the script, such as a Noto CJK package |

Two notes that are behaviour, not bugs:

- **File dialogs** go through the XDG desktop portal with a zenity fallback. With neither installed
  they cannot appear — but drag-and-drop and the command-line path argument always work. Portal
  failures are recorded in `gui.log`.
- **macOS:** quitting with <kbd>Cmd</kbd>+<kbd>Q</kbd> skips the window-close path, so that
  session's window size and position are not remembered. Settings are unaffected. Close the window
  itself to keep the geometry.

## Differences from the original BDInfo GUI

| | bdinfo-rs-gui | Original |
|---|---|---|
| Damaged discs | Scans through; unreadable files go to the report's `WARNING` block and a single banner | Stops with a message box per failed file |
| AACS-encrypted discs | Lists and browses the disc, marks it encrypted, and disables the scan | Scans the ciphertext and reports the statistics it yields |
| Saving | *Save report…* dialog with a destination picker, plus optional autosave (off by default) | Autosave beside the disc |
| Bitrate charts | Not implemented | Present |
| Custom playlist builder | Not implemented | Present |
| Taskbar progress | Not implemented | Present |

The report itself is byte-compatible. The deliberate content-level divergences — places where the
original is provably wrong against the codec specifications — are in
[DIFFERENCES.md](../../DIFFERENCES.md).

## Building from source

The app is its own workspace inside the repository. Pure Rust: no C toolchain and no system
development libraries on any platform. The pinned toolchain installs itself via the repository's
`rust-toolchain.toml`.

```sh
git clone https://github.com/agentjp/bdinfo-rs
cd bdinfo-rs/crates/bdinfo-rs-gui
cargo build --release      # binary at target/release/bdinfo-rs-gui
```

## Packaging notes

- The app identifies itself to the windowing system as `bdinfo-rs-gui`. On Linux, and Wayland
  especially, the desktop entry must match for the icon and window grouping to work. The shipped
  entry is named `io.github.agentjp.bdinfo-rs.desktop`, after the AppStream component id, so the
  match runs through its `StartupWMClass=bdinfo-rs-gui` line — the two strings move together.
- Linux packages should recommend `xdg-desktop-portal` for file dialogs.
- A macOS `.app` bundle must set `CFBundleIdentifier` in `Info.plist` — the native open panel
  requires it in bundled apps.
- There is nothing to install: configuration is created at runtime in the per-user directory above.
  An uninstall purge should remove that directory.

## License

[LGPL-2.1-or-later](../../LICENSE), like the rest of bdinfo-rs — a derivative work of BDInfo
([UniqProject](https://github.com/UniqProject/BDInfo)); see [NOTICE](../../NOTICE) for attribution.
