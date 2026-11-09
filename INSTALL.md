# Installing bdinfo-rs

Two applications ship from this repository: the **command-line tool** (`bdinfo-rs`, released on
`v*` tags) and the **desktop app** (`bdinfo-rs-gui`, released independently on `gui-v*` tags).
The browser version needs no install at all ([bdinfo.hyperslop.dev](https://bdinfo.hyperslop.dev)).

Every channel below states three things: where the files go, how the install updates, and how to
uninstall it.

## Recommended routes

These update automatically through a package manager and uninstall cleanly:

| OS | Command line | Desktop app |
|---|---|---|
| Windows | `winget install agentjp.bdinfo-rs` | `winget install agentjp.bdinfo-rs-gui` |
| macOS | `brew install agentjp/tap/bdinfo-rs` | `brew install --cask agentjp/tap/bdinfo-rs-gui` |
| Debian / Ubuntu | [apt repository](#apt--dnf-repository-linux) | same repository, `bdinfo-rs-gui` |
| Fedora / RHEL / openSUSE | [dnf repository](#apt--dnf-repository-linux) | same repository, `bdinfo-rs-gui` |
| Arch Linux | AUR `bdinfo-rs-bin` | AUR `bdinfo-rs-gui-bin` |
| Other Linux | [install script](#install-script) or Homebrew | [AppImage](#appimage-linux) |

## All channels

| Channel | Platforms | Updates | Uninstall |
|---|---|---|---|
| [WinGet](#winget-windows) | Windows | `winget upgrade` | `winget uninstall` |
| [Scoop](#scoop-windows) | Windows | `scoop update` | `scoop uninstall` |
| [Homebrew](#homebrew-macos--linux) | macOS, Linux | `brew upgrade` | `brew uninstall` |
| [apt / dnf repository](#apt--dnf-repository-linux) | Linux | `apt` / `dnf` upgrade | `apt` / `dnf` remove |
| [AUR](#aur-arch-linux) | Arch Linux | AUR helper | `pacman -R` |
| [cargo](#cargo-cratesio) | all | re-run install | `cargo uninstall` |
| [Install script](#install-script) | all | re-run the script | manual (documented below) |
| [Prebuilt archive](#prebuilt-archives) | all | replace the files | delete the files |
| [Docker](#docker) | Linux containers | `docker pull` | `docker rmi` |
| [From source](#from-source) | all | `git pull` + rebuild | `cargo uninstall` / delete |

The [desktop app](#desktop-app) has its own section further down, channel by channel.

---

# Command-line tool

## WinGet (Windows)

```powershell
winget install agentjp.bdinfo-rs
```

- **Where it goes** — a portable install, no administrator rights: WinGet extracts the release
  `.zip` under `%LOCALAPPDATA%\Microsoft\WinGet\Packages\agentjp.bdinfo-rs__DefaultSource\` and
  exposes the `bdinfo-rs` command through a link in `%LOCALAPPDATA%\Microsoft\WinGet\Links`,
  a directory WinGet keeps on the user `PATH`.
- **Updates** — `winget upgrade agentjp.bdinfo-rs` (or `winget upgrade --all`). New releases are
  submitted to the WinGet community repository automatically.
- **Uninstall** — `winget uninstall agentjp.bdinfo-rs` removes the package directory and the
  `PATH` link.

## Scoop (Windows)

```powershell
scoop bucket add agentjp https://github.com/agentjp/scoop-bucket
scoop install bdinfo-rs
```

- **Where it goes** — `%USERPROFILE%\scoop\apps\bdinfo-rs\<version>\`, with a `current` junction
  and a `bdinfo-rs` shim in `%USERPROFILE%\scoop\shims` (on the user `PATH`).
- **Updates** — `scoop update bdinfo-rs`. The bucket tracks releases automatically.
- **Uninstall** — `scoop uninstall bdinfo-rs`; drop the bucket with `scoop bucket rm agentjp`.

## Homebrew (macOS / Linux)

```sh
brew install agentjp/tap/bdinfo-rs
```

- **Where it goes** — the versioned keg `$(brew --prefix)/Cellar/bdinfo-rs/<version>/`, symlinked
  as `$(brew --prefix)/bin/bdinfo-rs`. The prefix is `/opt/homebrew` on Apple Silicon,
  `/usr/local` on Intel Macs, `/home/linuxbrew/.linuxbrew` on Linux. The formula also installs
  the man page and the bash / zsh / fish completions into Homebrew's standard share paths.
- **Updates** — `brew upgrade bdinfo-rs` (after a `brew update`).
- **Uninstall** — `brew uninstall bdinfo-rs`; drop the tap with `brew untap agentjp/tap`.

## apt / dnf repository (Linux)

Add the repository once, then use your package manager normally. Package repository hosting is
graciously provided by [Cloudsmith](https://cloudsmith.com) ♥ OSS.

```sh
# Debian / Ubuntu and derivatives
curl -1sLf 'https://dl.cloudsmith.io/public/bdinfo-rs/bdinfo-rs/setup.deb.sh' | sudo -E bash
sudo apt install bdinfo-rs

# Fedora / RHEL / openSUSE
curl -1sLf 'https://dl.cloudsmith.io/public/bdinfo-rs/bdinfo-rs/setup.rpm.sh' | sudo -E bash
sudo dnf install bdinfo-rs
```

- **Where it goes** — `/usr/bin/bdinfo-rs`, the man page at `/usr/share/man/man1/bdinfo-rs.1`,
  completions under `/usr/share/bash-completion/completions/`, `/usr/share/zsh/site-functions/`
  and `/usr/share/fish/vendor_completions.d/`, and the license at
  `/usr/share/doc/bdinfo-rs/copyright`. The setup script writes the repository definition to
  `/etc/apt/sources.list.d/bdinfo-rs-bdinfo-rs.list` plus a signing key under
  `/usr/share/keyrings/` (apt), or `/etc/yum.repos.d/bdinfo-rs-bdinfo-rs.repo` (dnf).
- **Updates** — your normal `sudo apt upgrade` / `sudo dnf upgrade` picks up new releases.
- **Uninstall** — `sudo apt remove bdinfo-rs` / `sudo dnf remove bdinfo-rs`. To drop the
  repository too, delete the repository definition and key files listed above.

Prefer not to add a repository? Individual `.deb` / `.rpm` packages (x64 and arm64) are attached
to every [release](https://github.com/agentjp/bdinfo-rs/releases) — same file layout, but
updates become manual (download and install the next release's package; the package manager
still uninstalls it cleanly):

```sh
sudo apt install ./bdinfo-rs_*_amd64.deb     # Debian / Ubuntu
sudo dnf install ./bdinfo-rs-*.x86_64.rpm    # Fedora / RHEL
```

## AUR (Arch Linux)

```sh
yay -S bdinfo-rs-bin        # or any AUR helper
```

- **Where it goes** — `/usr/bin/bdinfo-rs`, the man page, all three shell completions, and the
  license under `/usr/share/licenses/bdinfo-rs-bin/` — the same layout as the `.deb` / `.rpm`.
- **Updates** — your AUR helper's normal upgrade (`yay -Syu`); the package tracks releases
  automatically.
- **Uninstall** — `sudo pacman -R bdinfo-rs-bin`.

## cargo (crates.io)

```sh
cargo binstall bdinfo-rs    # fetch the prebuilt release binary, no compile
cargo install bdinfo-rs     # or build from the crates.io source
```

- **Where it goes** — `$CARGO_HOME/bin/bdinfo-rs` (default `~/.cargo/bin`), which a Rust
  installation already has on `PATH`. This route installs the bare binary only — no man page or
  completions (see [the last section](#shell-completions-and-the-man-page) to add them).
- **Updates** — re-run the same command; `cargo install` rebuilds when a newer version exists.
- **Uninstall** — `cargo uninstall bdinfo-rs`.

## Install script

Downloads the right binary for your platform and puts it on `PATH`.

```sh
# Linux / macOS
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/agentjp/bdinfo-rs/releases/latest/download/bdinfo-rs-installer.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/agentjp/bdinfo-rs/releases/latest/download/bdinfo-rs-installer.ps1 | iex"
```

- **Where it goes** — `$CARGO_HOME/bin`, falling back to `~/.cargo/bin` when `CARGO_HOME` is not
  set (on Windows: `%USERPROFILE%\.cargo\bin`) — even without Rust installed; the script just
  creates the directory. It then puts that directory on `PATH`: on Linux / macOS by adding a
  source line for `~/.cargo/env` to your shell profiles, on Windows by editing the user
  `Environment\Path` registry value. It also writes an install receipt,
  `bdinfo-rs-receipt.json`, to `~/.config/bdinfo-rs/` (Linux / macOS, honoring
  `XDG_CONFIG_HOME`) or `%LOCALAPPDATA%\bdinfo-rs\` (Windows), recording the version and whether
  `PATH` was modified.
- **Overrides** — set `BDINFO_RS_INSTALL_DIR` to force a custom directory;
  `BDINFO_RS_NO_MODIFY_PATH=1` skips the `PATH` edit.
- **Updates** — re-run the same one-liner. It always fetches the latest release and overwrites
  the binary in place; that is the update mechanism. There is no self-update command.
- **Uninstall** — manual, three pieces:
  1. Delete the `bdinfo-rs` binary from `~/.cargo/bin` (or wherever `BDINFO_RS_INSTALL_DIR`
     pointed).
  2. Delete the receipt directory (`~/.config/bdinfo-rs` or `%LOCALAPPDATA%\bdinfo-rs`).
  3. Optionally undo the `PATH` edit — but only if nothing else uses that directory: a Rust
     toolchain lives in `~/.cargo/bin` too, and rustup shares the same `~/.cargo/env` mechanism.

## Prebuilt archives

Archives are named by Rust target triple — `bdinfo-rs-x86_64-unknown-linux-musl.tar.gz`,
`bdinfo-rs-aarch64-pc-windows-msvc.zip`, and so on — and attached to every
[release](https://github.com/agentjp/bdinfo-rs/releases).

- **Where it goes** — wherever you extract it; there is no install step and nothing else is
  written. The archive carries the binary, LICENSE, README, CHANGELOG, the man page, and all
  four shell completions.
- **Updates** — download the next release's archive and replace the files.
- **Uninstall** — delete the extracted files.

See [Verifying downloads](#verifying-downloads) for checksums and provenance.

## Docker

Multi-arch images (`linux/amd64`, `linux/arm64`) on the GitHub Container Registry. The image is
the static binary on `scratch` — no OS, no shell, no libc, about 1 MB.

```sh
docker pull ghcr.io/agentjp/bdinfo-rs:latest      # or pin a release: :4.0.0
```

- **Updates** — `docker pull` a newer tag. Tags track the repo's versions (`4.0.0`, plus rolling
  `4.0` and `4`).
- **Uninstall** — `docker rmi ghcr.io/agentjp/bdinfo-rs`.

Mount the disc and pass its in-container path. `-it` gives the interactive picker a terminal;
the report is written back into the mounted folder.

```sh
docker run --rm -it -v /path/to/disc:/mnt/bd ghcr.io/agentjp/bdinfo-rs /mnt/bd
```

An `.iso` has no folder to write into, so mount a second directory for the report:

```sh
docker run --rm -it -v /path/to/movie.iso:/movie.iso:ro -v /path/to/out:/out \
  ghcr.io/agentjp/bdinfo-rs /movie.iso /out
```

To build the image yourself: `docker build -t bdinfo-rs .`

## From source

No C toolchain, no system libraries, no extra steps — the same on every platform. The pinned
Rust toolchain installs itself via `rust-toolchain.toml`.

```sh
git clone https://github.com/agentjp/bdinfo-rs
cd bdinfo-rs
cargo build --release      # CLI at target/release/bdinfo-rs
cargo test
```

- **Where it goes** — `target/release/bdinfo-rs`; run it from there, or
  `cargo install --path crates/bdinfo-rs` to place it in `~/.cargo/bin`. The build also
  regenerates the man page and completions under `target/release/build/bdinfo-rs-*/out/assets/`.
- **Updates** — `git pull` and rebuild.
- **Uninstall** — delete the clone (and `cargo uninstall bdinfo-rs` if you installed it).

---

# Desktop app

The desktop app is `bdinfo-rs-gui`, released on its own
[`gui-v*` tags](https://github.com/agentjp/bdinfo-rs/releases?q=%22native+desktop+GUI%22&expanded=true)
and versioned in lock-step with the command-line binary.

**Settings and log.** On every platform the app keeps its settings (`gui.conf`) and log
(`gui.log`) in one per-user folder: `%APPDATA%\bdinfo-rs` on Windows,
`~/Library/Application Support/bdinfo-rs` on macOS, `$XDG_CONFIG_HOME/bdinfo-rs` (default
`~/.config/bdinfo-rs`) on Linux. This is runtime data the app creates — **no uninstaller
removes it** (so settings survive a reinstall); delete the folder by hand for a complete
removal. The one exception is Homebrew's `--zap`, noted below. The portable zip can override
this, also below.

Release artifacts, per platform:

| Platform | Artifacts |
|---|---|
| Windows x64 | `bdinfo-rs-gui-x86_64-pc-windows-msvc.msi` · `bdinfo-rs-gui-x86_64-pc-windows-msvc.zip` |
| Windows arm64 | `bdinfo-rs-gui-aarch64-pc-windows-msvc.msi` · `bdinfo-rs-gui-aarch64-pc-windows-msvc.zip` |
| macOS Intel | `bdinfo-rs-gui-x86_64-apple-darwin.dmg` |
| macOS Apple Silicon | `bdinfo-rs-gui-aarch64-apple-darwin.dmg` |
| Linux x64 | `bdinfo-rs-gui-x86_64-unknown-linux-gnu.AppImage` · `.deb` · `.rpm` |
| Linux arm64 | `bdinfo-rs-gui-aarch64-unknown-linux-gnu.AppImage` · `.deb` · `.rpm` |

## WinGet (Windows)

```powershell
winget install agentjp.bdinfo-rs-gui
```

- **Where it goes** — WinGet runs the MSI below silently, so everything in the next section
  applies: `%LOCALAPPDATA%\Programs\bdinfo-rs GUI\`, per-user, no administrator rights. A silent
  install never shows the license page and does not add the install directory to `PATH`.
- **Updates** — `winget upgrade agentjp.bdinfo-rs-gui` (or `winget upgrade --all`). New releases
  are submitted to the WinGet community repository automatically.
- **Uninstall** — `winget uninstall agentjp.bdinfo-rs-gui`, or Apps & features.

## MSI installer (Windows)

- **Where it goes** — `%LOCALAPPDATA%\Programs\bdinfo-rs GUI\` (`bdinfo-rs-gui.exe`, LICENSE,
  NOTICE), a Start-menu shortcut, and an Apps & features entry — all per-user; the installer
  never asks for administrator rights and touches nothing machine-wide. It shows a license page
  and one optional feature, off unless you pick it: adding the install directory to your user
  `PATH` (for unattended installs: `msiexec /i <msi> /qn ADDLOCAL=Main,PathEnvironment`).
- **Updates** — run a newer release's MSI; it replaces the installed version in one step.
  Downgrades are refused.
- **Uninstall** — Apps & features (or `msiexec /x <msi>`). Removes the program folder, shortcut,
  and the `PATH` entry if that feature was installed; leaves `%APPDATA%\bdinfo-rs` (settings and
  log) in place.

## Portable zip (Windows)

- **Where it goes** — wherever you extract it; run `bdinfo-rs-gui.exe` from there. By default
  settings and the log still live in `%APPDATA%\bdinfo-rs`; create an empty file named
  `bdinfo-rs-gui.portable` next to the executable and the app keeps `gui.conf` and `gui.log`
  beside itself instead, so the whole folder can move between machines.
- **Updates** — extract the next release's zip over the old files (the portable marker and
  `gui.conf` are yours; the zip never contains them).
- **Uninstall** — delete the folder, plus `%APPDATA%\bdinfo-rs` if the portable marker was not
  in use.

The Windows binaries are not code-signed, so SmartScreen may warn on a new release ("Windows
protected your PC") until that build has been seen enough times. Choose **More info** → **Run
anyway**.

## Homebrew cask (macOS)

```sh
brew install --cask agentjp/tap/bdinfo-rs-gui
```

- **Where it goes** — `/Applications/bdinfo-rs GUI.app`.
- **Updates** — `brew upgrade --cask bdinfo-rs-gui`.
- **Uninstall** — `brew uninstall --cask bdinfo-rs-gui` removes the app;
  `brew uninstall --zap --cask bdinfo-rs-gui` also trashes the settings folder, caches, and
  saved window state.

The cask prints the Gatekeeper first-launch steps (next section) after install.

## DMG (macOS)

- **Where it goes** — drag the app to `/Applications` (or anywhere).
- **Updates** — download the next release's DMG and replace the app.
- **Uninstall** — move the app to the Trash; settings stay in
  `~/Library/Application Support/bdinfo-rs` until you delete them.

The app is not signed or notarized — there is no Apple Developer account behind this project —
so Gatekeeper blocks the first launch. Open it once, then allow it under **System Settings →
Privacy & Security → "Open Anyway"**. The command-line equivalent:

```sh
xattr -d com.apple.quarantine "/Applications/bdinfo-rs GUI.app"
```

On macOS 26 (Tahoe) the same block is reported as **"bdinfo-rs GUI" is damaged and can't be
opened. You should move it to the Trash.** The app is not damaged; that is Tahoe's wording for
an unsigned quarantined app, and the two steps above still apply.

## apt / dnf repository (Linux)

The same Cloudsmith repository as the command-line tool (setup commands
[above](#apt--dnf-repository-linux)):

```sh
sudo apt install bdinfo-rs-gui      # Debian / Ubuntu
sudo dnf install bdinfo-rs-gui      # Fedora / RHEL / openSUSE
```

- **Where it goes** — `/usr/bin/bdinfo-rs-gui`, a desktop entry and AppStream metadata under
  `/usr/share/applications/` and `/usr/share/metainfo/`, and icons under
  `/usr/share/icons/hicolor/`. The package recommends `xdg-desktop-portal` (+ a backend) for
  the file dialogs and `libvulkan1` for GPU rendering — without the latter the app falls back
  to software rendering.
- **Updates** — your normal `sudo apt upgrade` / `sudo dnf upgrade`.
- **Uninstall** — `sudo apt remove bdinfo-rs-gui` / `sudo dnf remove bdinfo-rs-gui`; settings
  stay in `~/.config/bdinfo-rs` until you delete them.

Standalone `.deb` / `.rpm` packages are also attached to every `gui-v*` release for
repository-free installs (updates become manual).

## AUR (Arch Linux)

```sh
yay -S bdinfo-rs-gui-bin    # or any AUR helper
```

- **Where it goes** — the same layout as the `.deb`: `/usr/bin/bdinfo-rs-gui`, desktop entry,
  AppStream metadata, icons.
- **Updates** — your AUR helper's normal upgrade (`yay -Syu`).
- **Uninstall** — `sudo pacman -R bdinfo-rs-gui-bin`.

## AppImage (Linux)

- **Where it goes** — a single self-contained file; put it anywhere, `chmod +x` it, run it.
  Nothing else is installed — no desktop entry, no icon.
- **Updates** — download the next release's AppImage and replace the file.
- **Uninstall** — delete the file (and `~/.config/bdinfo-rs` for the settings).

## From crates.io / source

```sh
cargo install bdinfo-rs-gui             # build from crates.io

# or from a clone (the GUI is its own workspace):
cd crates/bdinfo-rs-gui
cargo build --release                   # app at target/release/bdinfo-rs-gui
```

- **Where it goes** — `~/.cargo/bin/bdinfo-rs-gui` (`cargo install`) or
  `target/release/bdinfo-rs-gui` (clone build). The bare binary only — no desktop entry, icons,
  or Start-menu shortcut.
- **Updates** — re-run `cargo install bdinfo-rs-gui`, or `git pull` and rebuild.
- **Uninstall** — `cargo uninstall bdinfo-rs-gui`, or delete the clone.

---

# Verifying downloads

Every release asset — both lanes, `v*` and `gui-v*` — is covered by a checksum manifest attached
to its release (`sha256.sum` on CLI releases, `SHA256SUMS` on GUI releases; CLI archives also
carry a per-archive `.sha256` sidecar) and a GitHub Artifact Attestation, verifiable with the
[GitHub CLI](https://cli.github.com/manual/gh_attestation_verify):

```sh
gh attestation verify <file-path of downloaded artifact> --repo agentjp/bdinfo-rs
```

You can also download the attestation from
[GitHub](https://github.com/agentjp/bdinfo-rs/attestations) and verify against that directly:

```sh
gh attestation verify <file-path of downloaded artifact> --bundle <file-path of downloaded attestation>
```

# Shell completions and the man page

Generated from the CLI itself, so they always match the binary. Homebrew, the `.deb` / `.rpm`
packages, and the AUR package install them for you; from an extracted archive, place them by
hand:

```sh
install -Dm644 bdinfo-rs.bash /usr/share/bash-completion/completions/bdinfo-rs
install -Dm644 _bdinfo-rs     /usr/share/zsh/site-functions/_bdinfo-rs
install -Dm644 bdinfo-rs.fish ~/.config/fish/completions/bdinfo-rs.fish
install -Dm644 bdinfo-rs.1    /usr/share/man/man1/bdinfo-rs.1
```

```powershell
# PowerShell — dot-source from your $PROFILE
. .\_bdinfo-rs.ps1
```

A source build writes the same files to `target/<profile>/build/bdinfo-rs-*/out/assets/`.
