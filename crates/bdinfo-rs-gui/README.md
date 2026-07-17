# bdinfo-rs-gui

**Native desktop GUI for [bdinfo-rs](../../README.md)** — the classic BDInfo disc
report as a cross-platform desktop app. Open a `BDMV` folder or `.iso` image, pick
playlists from the familiar three-pane view (Playlist / Stream File / Codec), run the
measured scan with live progress, and read, save, or copy the classic text report.
The report is **byte-identical** to the one the `bdinfo-rs` CLI produces.

Like the rest of the project it is pure Rust — no webview, no bundled runtime, no C
libraries — and runs on Windows, macOS, and Linux, on x64 and arm64. Rendering is
GPU-accelerated (wgpu) with an automatic software fallback (tiny-skia) when no usable
GPU is found.

## Launching

- **Double-click** the binary. No installation or setup step; settings are created on
  first use.
- **Drag and drop** a disc folder or `.iso` file onto the window.
- **Command line:** `bdinfo-rs-gui <path>` opens the disc at `<path>` on startup — the
  disc root, the `BDMV` folder itself, any directory inside it, or an `.iso` image.
  This is the whole CLI surface; for scripting and reports-without-a-window use the
  `bdinfo-rs` CLI instead.

## Files it writes

All persistent state lives in one per-user directory, created on first use:

| Platform | Directory |
|---|---|
| Windows | `%APPDATA%\bdinfo-rs\` |
| macOS | `~/Library/Application Support/bdinfo-rs/` |
| Linux | `$XDG_CONFIG_HOME/bdinfo-rs/` (default `~/.config/bdinfo-rs/`) |

- **`gui.conf`** — settings and window geometry, saved as they change. **Deleting it
  is the full reset**: window size/position, theme, and every setting return to
  defaults. This is also the fix when the window restores off-screen (e.g. a saved
  position on a since-unplugged monitor).
- **`gui.log`** — a plain-text diagnostic log, overwritten at each launch. It records
  which GPU/renderer was actually selected, any panics, and dialog/portal errors.
  **When filing a bug, attach `gui.log`** from a launch that shows the problem — it
  answers the first questions a graphics issue needs answered.

Reports are saved as `BDINFO.{volume label}.txt` — via the *Save report…* dialog, or
automatically next to the disc when *Auto-save report on scan completion* is enabled
in Settings.

## Troubleshooting

Rendering problems are almost always the GPU driver stack, and two environment
variables cover nearly all of them:

- **Glitchy or wrong rendering — black icons, gradient banding, flashing, or a crash
  right at startup on an old GPU:** set `ICED_BACKEND=tiny-skia` to force software
  rendering. When GPU initialization fails outright the app already falls back to
  software rendering on its own; the variable is for the cases where the GPU path
  "works" but renders wrongly.
- **Picking a different GPU API:** `WGPU_BACKEND=vulkan` (or `gl`, `dx12`, `metal`)
  skips a broken backend while staying GPU-accelerated.
- **Hybrid-GPU laptop spins up the discrete GPU:** set `WGPU_POWER_PREF=low`. The
  renderer defaults to the high-performance adapter; a disc analyzer does not need
  one.
- **Wayland trouble** (blank window, no decorations, compositor quirks): unset
  `WAYLAND_DISPLAY` for the launch to run via X11/XWayland. (The old
  `WINIT_UNIX_BACKEND` variable no longer exists; the standard display variables are
  the selection mechanism.)
- **Raspberry Pi, Asahi Linux, and similar (2048-px GPU texture limit):** resizing
  the window wider than 2048 physical pixels can crash the GPU renderer on devices
  whose driver reports a 2048-px maximum texture size. Use
  `ICED_BACKEND=tiny-skia` there.
- **White or blank window after a GPU driver update, sleep/resume, or monitor
  change:** restart the app — recovering a lost GPU device is an upstream renderer
  limitation. Settings are saved as they change, and with
  *Auto-save report on scan completion* enabled a finished report is already on disk.
- **White window on Windows with the "Beta: Use Unicode UTF-8 for worldwide language
  support" locale option enabled:** known driver-dependent incompatibility with GPU
  apps. `ICED_BACKEND=tiny-skia` avoids it, or disable the option (Control Panel →
  Region → Administrative → Change system locale).
- **File-picker buttons do nothing on a minimal Linux setup** (bare window manager,
  container): the open/save dialogs go through the XDG desktop portal, with a zenity
  fallback — with neither installed, the dialogs cannot appear (install
  `xdg-desktop-portal` plus a backend such as `xdg-desktop-portal-gtk`, or `zenity`).
  Two roads always work without any portal: drag-and-drop onto the window, and the
  command-line path argument. Portal failures are recorded in `gui.log`.
- **Everything is too small or too large** (typically X11 fractional scaling): set
  the UI scale in Settings (50–200%), or step it with <kbd>Ctrl</kbd>+<kbd>+</kbd> /
  <kbd>Ctrl</kbd>+<kbd>-</kbd>.
- **macOS:** quitting with <kbd>Cmd</kbd>+<kbd>Q</kbd> skips the window-close path,
  so that session's window size and position are not remembered (settings are
  unaffected). Close the window itself to keep the geometry.
- **Non-Latin disc titles show as boxes on a minimal Linux install:** text falls back
  to system fonts, so the system needs a font covering the script (e.g. a Noto CJK
  package for Japanese/Chinese/Korean titles).

## Differences from the BDInfo GUI

Deliberate divergences from the original Windows-only BDInfo application:

- **Damaged discs scan through.** Unreadable files are collected into the report's
  `WARNING` block and shown as a single banner; the scan continues. The original
  stops with a message box per failed file.
- **Saving:** a *Save report…* dialog with a destination picker, in addition to the
  original's autosave-next-to-the-disc setting (off by default here).
- **Not implemented:** the bitrate chart windows; the custom playlist builder; the
  taskbar progress indicator.
- **The report is byte-compatible** with the original tool's. The few deliberate
  content-level divergences — places where the original is provably wrong against the
  codec specifications — are documented in [DIFFERENCES.md](../../DIFFERENCES.md).

## Packaging notes

- The application identifies itself to the windowing system as `bdinfo-rs-gui`. On
  Linux (Wayland especially) the desktop entry must match for the icon and window
  grouping to work: the file must be named `bdinfo-rs-gui.desktop`, and any
  `StartupWMClass` entry must say `bdinfo-rs-gui`.
- Linux packages should recommend `xdg-desktop-portal` (file dialogs; see
  troubleshooting above).
- A macOS `.app` bundle must set `CFBundleIdentifier` in `Info.plist` — the native
  open panel requires it in bundled apps.
- Configuration is created at runtime in the per-user directory listed above; there
  is nothing to install. An uninstall purge should remove that directory.

## Building from source

The GUI is its own workspace inside the repository:

```sh
git clone https://github.com/agentjp/bdinfo-rs
cd bdinfo-rs/crates/bdinfo-rs-gui
cargo build --release      # binary at target/release/bdinfo-rs-gui
```

Pure Rust: no C toolchain and no system development libraries are required on any
platform. The pinned toolchain installs itself via the repository's
`rust-toolchain.toml`.

## License

[LGPL-2.1-or-later](../../LICENSE), like the rest of bdinfo-rs — a derivative work of
BDInfo ([UniqProject](https://github.com/UniqProject/BDInfo)); see
[NOTICE](../../NOTICE) for upstream attribution.
