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
# usr/ tree, and in usr/lib the windowing libraries the binary dlopens that a
# host is not guaranteed to have (see the bundling section below). The .deb and
# .rpm bundle nothing: a package manager resolves those libraries on the host.

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

# ── AppImage: the libraries the binary dlopens ───────────────────────────────
# The binary's only DT_NEEDED entries are glibc's. winit, wgpu and softbuffer
# reach the windowing and graphics stack through dlopen (x11-dl, x11rb,
# xkbcommon-dl, wayland-sys, khronos-egl, ash), so the library names exist only as strings
# inside the binary, and the binary panics at startup when one is missing
# (xkbcommon-dl's loader does not return an error). An AppImage has to carry
# every library its host is not guaranteed to have; the AppImage project's
# excludelist names the ones a host is guaranteed to have. The roots below are
# read from the binary itself, so a winit upgrade that dlopens a new library
# gets it bundled, or fails this script, rather than shipping without it.
#
# $hostLibraries: the excludelist at AppImageCommunity/pkg2appimage@15a64c2
# (2024-11-03), plus two additions. ld-linux-aarch64.so.1 is the aarch64
# counterpart of the list's x86_64 loader entry. libvulkan.so.1 is the Vulkan
# loader, which finds the GPU drivers the host installs; without it wgpu uses
# EGL, and iced falls back to its tiny-skia software renderer.
$hostLibraries = [System.Collections.Generic.HashSet[string]]::new([string[]] @(
        'ld-linux.so.2', 'ld-linux-x86-64.so.2', 'ld-linux-aarch64.so.1', 'libanl.so.1',
        'libBrokenLocale.so.1', 'libcidn.so.1', 'libc.so.6', 'libdl.so.2', 'libm.so.6',
        'libmvec.so.1', 'libnss_compat.so.2', 'libnss_dns.so.2', 'libnss_files.so.2',
        'libnss_hesiod.so.2', 'libnss_nisplus.so.2', 'libnss_nis.so.2', 'libpthread.so.0',
        'libresolv.so.2', 'librt.so.1', 'libthread_db.so.1', 'libutil.so.1', 'libstdc++.so.6',
        'libGL.so.1', 'libEGL.so.1', 'libGLdispatch.so.0', 'libGLX.so.0', 'libOpenGL.so.0',
        'libdrm.so.2', 'libglapi.so.0', 'libgbm.so.1', 'libxcb.so.1', 'libX11.so.6',
        'libX11-xcb.so.1', 'libwayland-client.so.0', 'libasound.so.2', 'libfontconfig.so.1',
        'libfreetype.so.6', 'libharfbuzz.so.0', 'libcom_err.so.2', 'libexpat.so.1',
        'libgcc_s.so.1', 'libgpg-error.so.0', 'libICE.so.6', 'libSM.so.6', 'libusb-1.0.so.0',
        'libuuid.so.1', 'libz.so.1', 'libjack.so.0', 'libpipewire-0.3.so.0',
        'libxcb-dri3.so.0', 'libxcb-dri2.so.0', 'libfribidi.so.0', 'libgmp.so.10',
        'libvulkan.so.1'), [System.StringComparer]::Ordinal)

# soname -> path, for this runner's architecture only (ldconfig lists every
# multiarch copy it knows).
$ldconfigArch = if ($arch -eq 'aarch64') { 'AArch64' } else { 'x86-64' }
$libraryPath = @{}
foreach ($line in & /sbin/ldconfig -p) {
    if ($line -match '^\s+(\S+) \(([^)]*)\) => (\S+)$' -and $Matches[2] -like "*$ldconfigArch*" -and
        -not $libraryPath.ContainsKey($Matches[1])) {
        $libraryPath[$Matches[1]] = $Matches[3]
    }
}

# Versioned names only: each loader also tries the unversioned development
# symlink (`libxkbcommon.so`) as a fallback, which no runtime package ships.
$latin1 = [System.Text.Encoding]::Latin1.GetString([System.IO.File]::ReadAllBytes($binary))
$roots = [regex]::Matches($latin1, 'lib[A-Za-z0-9_+-]+\.so\.[0-9]+') | ForEach-Object Value | Sort-Object -Unique -CaseSensitive

$libDir = Join-Path $appDir 'usr/lib'
New-Item -ItemType Directory $libDir -Force | Out-Null
$bundled = [System.Collections.Generic.SortedSet[string]]::new([System.StringComparer]::Ordinal)
$pending = [System.Collections.Generic.Queue[string]]::new([string[]] $roots)
while ($pending.Count) {
    $name = $pending.Dequeue()
    if ($hostLibraries.Contains($name) -or $bundled.Contains($name)) { continue }
    if (-not $libraryPath.ContainsKey($name)) {
        Write-Host "FAILED: the AppImage must bundle $name, but ldconfig on this runner does not know it (install its package in the gui-package action)"
        exit 1
    }
    $real = & readlink -f $libraryPath[$name]
    Copy-Item -LiteralPath $real (Join-Path $libDir $name)
    [void] $bundled.Add($name)
    foreach ($line in & objdump -p $real) {
        if ($line -match '^\s+NEEDED\s+(\S+)$') { $pending.Enqueue($Matches[1]) }
    }
    if ($LASTEXITCODE -ne 0) { Write-Host "FAILED: objdump -p $real"; exit 1 }

    # The copyright file of the Debian package that ships the library, under
    # that package's own name, next to the app's LGPL-2.1 text.
    $owners = @(& dpkg -S "*/$(Split-Path -Leaf $real)" 2>$null | ForEach-Object { ($_ -split ':')[0] } | Sort-Object -Unique)
    if ($owners.Count -ne 1) { Write-Host "FAILED: expected one package owning $real (got '$($owners -join ', ')')"; exit 1 }
    $copyright = "/usr/share/doc/$($owners[0])/copyright"
    if (-not (Test-Path -LiteralPath $copyright)) { Write-Host "FAILED: no $copyright for $name"; exit 1 }
    $licenseDir = Join-Path $appDir "usr/share/doc/$($owners[0])"
    New-Item -ItemType Directory $licenseDir -Force | Out-Null
    Copy-Item -LiteralPath (& readlink -f $copyright) (Join-Path $licenseDir 'copyright')
}
if ($bundled.Count -eq 0) { Write-Host 'FAILED: bundled no library; the binary reads none of the expected dlopen names'; exit 1 }
Write-Host "AppImage bundles: $($bundled -join ', ')"

# A launcher rather than a symlink to the binary: the bundled libraries are
# found through LD_LIBRARY_PATH, ahead of the host's copies. bdinfo-rs-gui
# starts no child process, so the variable reaches no other program.
$appRun = @(
    '#!/bin/sh'
    'HERE="$(dirname "$(readlink -f "$0")")"'
    'export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"'
    'exec "$HERE/usr/bin/bdinfo-rs-gui" "$@"'
) -join "`n"
[System.IO.File]::WriteAllText((Join-Path $appDir 'AppRun'), "$appRun`n")
& chmod +x (Join-Path $appDir 'AppRun')
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: chmod AppRun'; exit 1 }

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
