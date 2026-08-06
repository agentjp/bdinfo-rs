#!/usr/bin/env pwsh
# Packages the built GUI binary for Linux: the AppImage, the .deb, and the .rpm
# (the Sniffnet posture — no tarball), all named by target triple. Runs on a
# native runner per arch with the release binary already at
# crates/bdinfo-rs-gui/target/release/bdinfo-rs-gui (the deb/rpm metadata's
# asset paths and their $auto/auto-req scans assume it). Shared by the gui.yml
# packaging smoke and the gui-release.yml lane.
#
# cargo-deb, cargo-generate-rpm and appimagetool are read from
# .github/pins.env, the one home for every hand-maintained tool version here, so
# this lane and the CLI's .github/build-linux-packages.sh package with the same
# tools by construction. The
# AppDir carries exactly what the spec mandates in its root — AppRun, ONE
# .desktop, the icon named by its Icon= key, and .DirIcon — plus the usual
# usr/ tree; nothing graphics/driver-adjacent is bundled (the binary has no C
# dependencies at all, so there is nothing to exclude).

[CmdletBinding()]
param(
    # The crate version (informational; the deb/rpm read Cargo.toml).
    [Parameter(Mandatory)] [string] $Version,
    # The target triple the binary was built for — names the artifacts and
    # picks the appimagetool arch.
    [Parameter(Mandatory)]
    [ValidateSet('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')]
    [string] $Triple,
    # Output directory for the finished artifacts.
    [Parameter(Mandatory)] [string] $OutDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

# Pinned because a broken or schema-changing upstream release must not abort a
# release run at tag time.
. "$PSScriptRoot/_common.ps1"
$cargoDebVersion = Get-Pin CARGO_DEB
$cargoGenerateRpmVersion = Get-Pin CARGO_GENERATE_RPM
$appimagetoolVersion = Get-Pin APPIMAGETOOL

$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..' '..')).Path
$crate = Join-Path $repo 'crates/bdinfo-rs-gui'
$packaging = Join-Path $crate 'packaging'
# The AppStream component id, which also names the desktop file, the metainfo
# file and the icons as installed (see the metainfo's own header for the shape).
$appId = 'io.github.agentjp.bdinfo-rs'
$binary = Join-Path $crate 'target/release/bdinfo-rs-gui'
if (-not (Test-Path $binary)) { Write-Host "FAILED: no release binary at $binary"; exit 1 }
New-Item -ItemType Directory -Force $OutDir | Out-Null
$out = (Resolve-Path -LiteralPath $OutDir).Path

# ── .deb + .rpm (cargo metadata drives everything) ───────────────────────────
$haveDeb = Get-Command cargo-deb -ErrorAction SilentlyContinue
$haveRpm = Get-Command cargo-generate-rpm -ErrorAction SilentlyContinue
if (-not ($haveDeb -and $haveRpm)) {
    cargo install --locked "cargo-deb@$cargoDebVersion" "cargo-generate-rpm@$cargoGenerateRpmVersion"
    if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: install packagers'; exit 1 }
}

Push-Location -LiteralPath $crate
$code = 1
try {
    # --no-build: package the release binary the caller built; --no-strip: the
    # release profile already strips.
    cargo deb --no-build --no-strip -p bdinfo-rs-gui
    $code = $LASTEXITCODE
    if ($code -eq 0) {
        cargo generate-rpm -p .
        $code = $LASTEXITCODE
    }
}
finally { Pop-Location }
if ($code -ne 0) { Write-Host "FAILED: deb/rpm packaging exit $code"; exit 1 }

$deb = @(Get-ChildItem (Join-Path $crate 'target/debian') -Filter '*.deb')
$rpm = @(Get-ChildItem (Join-Path $crate 'target/generate-rpm') -Filter '*.rpm')
if ($deb.Count -ne 1 -or $rpm.Count -ne 1) {
    Write-Host "FAILED: expected exactly one .deb and one .rpm (got $($deb.Count)/$($rpm.Count))"
    exit 1
}
Copy-Item $deb[0].FullName (Join-Path $out "bdinfo-rs-gui-$Triple.deb")
Copy-Item $rpm[0].FullName (Join-Path $out "bdinfo-rs-gui-$Triple.rpm")
Write-Host "packaged bdinfo-rs-gui-$Triple.deb + .rpm"

# ── AppImage ─────────────────────────────────────────────────────────────────
$arch = if ($Triple -eq 'aarch64-unknown-linux-gnu') { 'aarch64' } else { 'x86_64' }
$appDir = Join-Path ([System.IO.Path]::GetTempPath()) "gui-appdir-$PID/bdinfo-rs-gui.AppDir"
if (Test-Path $appDir) { Remove-Item -Recurse -Force $appDir }
New-Item -ItemType Directory (Join-Path $appDir 'usr/bin') -Force | Out-Null
New-Item -ItemType Directory (Join-Path $appDir 'usr/share/applications') -Force | Out-Null
New-Item -ItemType Directory (Join-Path $appDir 'usr/share/metainfo') -Force | Out-Null

Copy-Item $binary (Join-Path $appDir 'usr/bin/bdinfo-rs-gui')
& chmod +x (Join-Path $appDir 'usr/bin/bdinfo-rs-gui')
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: chmod'; exit 1 }
Copy-Item (Join-Path $packaging "$appId.desktop") (Join-Path $appDir 'usr/share/applications')
Copy-Item (Join-Path $packaging "$appId.metainfo.xml") (Join-Path $appDir 'usr/share/metainfo')
# The LGPL-2.1 text and the attribution notice ride inside the AppImage — a
# portable format carries no package-manager license metadata, so the texts
# themselves are the only license information a recipient gets.
$doc = Join-Path $appDir 'usr/share/doc/bdinfo-rs-gui'
New-Item -ItemType Directory $doc -Force | Out-Null
Copy-Item (Join-Path $repo 'LICENSE') $doc
Copy-Item (Join-Path $repo 'NOTICE') $doc
# The icons install under the AppStream id, which is what the desktop file's
# Icon= key names; their source name is the crate's.
foreach ($size in 16, 24, 32, 48, 64, 128, 256, 512) {
    $dir = Join-Path $appDir "usr/share/icons/hicolor/${size}x${size}/apps"
    New-Item -ItemType Directory $dir -Force | Out-Null
    Copy-Item (Join-Path $packaging "icons/hicolor/${size}x${size}/apps/bdinfo-rs-gui.png") (Join-Path $dir "$appId.png")
}
# The four spec-mandated root entries.
Copy-Item (Join-Path $packaging "$appId.desktop") $appDir
Copy-Item (Join-Path $packaging 'icons/hicolor/256x256/apps/bdinfo-rs-gui.png') (Join-Path $appDir "$appId.png")
Copy-Item (Join-Path $packaging 'icons/hicolor/256x256/apps/bdinfo-rs-gui.png') (Join-Path $appDir '.DirIcon')
& ln -s usr/bin/bdinfo-rs-gui (Join-Path $appDir 'AppRun')
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: ln -s AppRun'; exit 1 }

$tool = Join-Path ([System.IO.Path]::GetTempPath()) "appimagetool-$arch.AppImage"
if (-not (Test-Path $tool)) {
    curl -fsSL -o $tool "https://github.com/AppImage/appimagetool/releases/download/$appimagetoolVersion/appimagetool-$arch.AppImage"
    if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: fetch appimagetool'; exit 1 }
    & chmod +x $tool
    if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: chmod appimagetool'; exit 1 }
}
$appImage = Join-Path $out "bdinfo-rs-gui-$Triple.AppImage"
# --appimage-extract-and-run: the tool itself needs no FUSE on the runner. It
# downloads the static type2 runtime to embed (AppImage/type2-runtime's
# `continuous` release — upstream publishes no versioned runtime tags).
$env:ARCH = $arch
& $tool --appimage-extract-and-run $appDir $appImage
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: appimagetool'; exit 1 }
Remove-Item -Recurse -Force (Split-Path $appDir)
Write-Host "packaged $appImage"
