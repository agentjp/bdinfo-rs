# Installing bdinfo-rs

Every route below delivers the same analyzer. The **desktop app** and the **command-line binary**
are separate downloads; the browser version needs no install at all
([bdinfo.hyperslop.dev](https://bdinfo.hyperslop.dev)).

## At a glance

| Route | Platforms | Updates | Ships man page + completions |
|---|---|---|---|
| [Desktop app](#desktop-app) | Windows, macOS, Linux | automatic via a package manager | n/a |
| [Package manager](#package-managers) | all | automatic | Homebrew, apt, dnf |
| [apt / dnf repository](#apt--dnf) | Linux | automatic | yes |
| [Install script](#install-script) | all | manual | no |
| [Prebuilt archive](#prebuilt-archives) | all | manual | yes, in the archive |
| [Docker](#docker) | Linux containers | pull a new tag | n/a |
| [From source](#from-source) | all | `git pull` | regenerated at build |

---

## Desktop app

The desktop app is `bdinfo-rs-gui`, released on its own `gui-v*` tags and versioned
independently of the command-line binary. The rest of this page covers the command line.

```sh
# Windows — WinGet
winget install agentjp.bdinfo-rs-gui

# macOS / Linux — Homebrew cask
brew install --cask agentjp/tap/bdinfo-rs-gui

# Arch Linux — AUR (with your helper of choice)
yay -S bdinfo-rs-gui-bin

# Debian / Ubuntu and derivatives — Cloudsmith
curl -1sLf 'https://dl.cloudsmith.io/public/bdinfo-rs/bdinfo-rs/setup.deb.sh' | sudo -E bash
sudo apt install bdinfo-rs-gui

# Fedora / RHEL / openSUSE — Cloudsmith
curl -1sLf 'https://dl.cloudsmith.io/public/bdinfo-rs/bdinfo-rs/setup.rpm.sh' | sudo -E bash
sudo dnf install bdinfo-rs-gui

# Rust — build from crates.io
cargo install bdinfo-rs-gui
```

Or download an artifact directly from a
[`gui-v*` release](https://github.com/agentjp/bdinfo-rs/releases?q=%22native+desktop+GUI%22&expanded=true). Every
asset is listed in the release's `SHA256SUMS` and carries a Sigstore build-provenance
attestation (`gh attestation verify <asset> --repo agentjp/bdinfo-rs`).

| Platform | Artifacts |
|---|---|
| Windows x64 | `bdinfo-rs-gui-x86_64-pc-windows-msvc.msi` · `bdinfo-rs-gui-x86_64-pc-windows-msvc.zip` |
| Windows arm64 | `bdinfo-rs-gui-aarch64-pc-windows-msvc.msi` · `bdinfo-rs-gui-aarch64-pc-windows-msvc.zip` |
| macOS Intel | `bdinfo-rs-gui-x86_64-apple-darwin.dmg` |
| macOS Apple Silicon | `bdinfo-rs-gui-aarch64-apple-darwin.dmg` |
| Linux x64 | `bdinfo-rs-gui-x86_64-unknown-linux-gnu.AppImage` · `.deb` · `.rpm` |
| Linux arm64 | `bdinfo-rs-gui-aarch64-unknown-linux-gnu.AppImage` · `.deb` · `.rpm` |

### Windows

The `.msi` installs for the current user only and never asks for administrator rights. It shows
a license page and one optional feature — adding the install directory to your user `PATH`, off
unless you pick it. It creates a Start-menu entry and uninstalls from Apps & features.

The `.zip` is the portable route: extract it anywhere and run `bdinfo-rs-gui.exe`. By default
settings and the log still live in `%APPDATA%\bdinfo-rs`; create an empty file named
`bdinfo-rs-gui.portable` next to the executable and the app keeps `gui.conf` and `gui.log`
beside itself instead, so the whole folder can move between machines.

The binaries are not code-signed, so SmartScreen may warn on a new release ("Windows protected
your PC") until that build has been seen enough times. Choose **More info** → **Run anyway**.

### macOS

The `.dmg` is not signed or notarized — there is no Apple Developer account behind this
project — so Gatekeeper blocks the first launch. Drag the app to Applications, try to open it
once, then allow it under **System Settings → Privacy & Security → "Open Anyway"**. The
command-line equivalent:

```sh
xattr -d com.apple.quarantine "/Applications/bdinfo-rs GUI.app"
```

On macOS 26 (Tahoe) the same block is reported as **"bdinfo-rs GUI" is damaged and can't be
opened. You should move it to the Trash.** The app is not damaged; that is Tahoe's wording for
an unsigned quarantined app, and the two steps above still apply.

### Linux

The `.AppImage` is self-contained — `chmod +x` it and run it. The `.deb` and `.rpm` packages
install a desktop entry and icon; the Cloudsmith repository above keeps them updated.

## Package managers

```sh
# macOS / Linux — Homebrew
brew install agentjp/tap/bdinfo-rs

# Windows — WinGet
winget install agentjp.bdinfo-rs

# Windows — Scoop
scoop bucket add agentjp https://github.com/agentjp/scoop-bucket
scoop install bdinfo-rs

# Arch Linux — AUR (with your helper of choice)
yay -S bdinfo-rs-bin

# Rust — fetch the prebuilt binary, no compile
cargo binstall bdinfo-rs

# Rust — build from crates.io
cargo install bdinfo-rs
```

## apt / dnf

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

Prefer not to add a repository? Individual `.deb` / `.rpm` packages (x64 and arm64) are attached to
every [release](https://github.com/agentjp/bdinfo-rs/releases):

```sh
sudo apt install ./bdinfo-rs_*_amd64.deb     # Debian / Ubuntu
sudo dnf install ./bdinfo-rs-*.x86_64.rpm    # Fedora / RHEL
```

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

## Prebuilt archives

Archives are named by Rust target triple — `bdinfo-rs-x86_64-unknown-linux-musl.tar.gz`,
`bdinfo-rs-aarch64-pc-windows-msvc.zip`, and so on. Extract and run; there is no install step.

Verify a download against the aggregate `sha256.sum` attached to the release, or the per-archive
`.sha256` sidecar. Releases also carry Sigstore provenance.

## Docker

Multi-arch images (`linux/amd64`, `linux/arm64`) on the GitHub Container Registry. The image is the
static binary on `scratch` — no OS, no shell, no libc, about 1 MB.

```sh
docker pull ghcr.io/agentjp/bdinfo-rs:latest      # or pin a release: :2.0.0
```

Mount the disc and pass its in-container path. `-it` gives the interactive picker a terminal; the
report is written back into the mounted folder.

```sh
docker run --rm -it -v /path/to/disc:/mnt/bd ghcr.io/agentjp/bdinfo-rs /mnt/bd
```

An `.iso` has no folder to write into, so mount a second directory for the report:

```sh
docker run --rm -it -v /path/to/movie.iso:/movie.iso:ro -v /path/to/out:/out \
  ghcr.io/agentjp/bdinfo-rs /movie.iso /out
```

Tags track the repo's versions (`2.0.0`, plus rolling `2.0` and `2`). To build it yourself:
`docker build -t bdinfo-rs .`

## From source

No C toolchain, no system libraries, no extra steps — the same on every platform. The pinned Rust
toolchain installs itself via `rust-toolchain.toml`.

```sh
git clone https://github.com/agentjp/bdinfo-rs
cd bdinfo-rs
cargo build --release      # CLI at target/release/bdinfo-rs
cargo test
```

The desktop app is its own workspace:

```sh
cd crates/bdinfo-rs-gui
cargo build --release      # app at target/release/bdinfo-rs-gui
```

## Shell completions and the man page

Generated from the CLI itself, so they always match the binary. Homebrew and the `.deb` / `.rpm`
packages install them for you; from an extracted archive, place them by hand:

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
