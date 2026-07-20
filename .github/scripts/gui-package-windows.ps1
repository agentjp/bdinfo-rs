#!/usr/bin/env pwsh
# Packages the built GUI executable for Windows: the portable .zip and the
# WiX v3 .msi (packaging/setup.wxs), named by target triple like the CLI's
# release assets. Shared by the gui.yml packaging smoke and the gui-release.yml
# lane so the tag path exercises exactly the PR-tested code.
#
# WiX 3.14 is preinstalled on the windows-2025 runner image (3.14 added arm64
# MSI support); the windows-11-arm image does NOT carry it, so the release lane
# builds BOTH MSIs on the x64 runner — MSI authoring is not arch-bound, only
# `candle -arch` and the packaged .exe differ.

[CmdletBinding()]
param(
    # The built bdinfo-rs-gui.exe to package.
    [Parameter(Mandatory)] [string] $Exe,
    # The crate version (the MSI ProductVersion), e.g. 2.0.0.
    [Parameter(Mandatory)] [string] $Version,
    # The target triple the exe was built for — names the artifacts and picks
    # the MSI architecture.
    [Parameter(Mandatory)]
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string] $Triple,
    # Output directory for the finished artifacts.
    [Parameter(Mandatory)] [string] $OutDir,
    # Which artifacts to build. The release lane's windows-11-arm leg zips
    # natively but has no WiX, so its MSI is authored on the x64 runner from
    # the uploaded exe: 'zip' there, 'msi' in the follow-up job.
    [ValidateSet('all', 'zip', 'msi')] [string] $Artifacts = 'all'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..' '..')).Path
$packaging = Join-Path $repo 'crates/bdinfo-rs-gui/packaging'
$exe = (Resolve-Path -LiteralPath $Exe).Path
New-Item -ItemType Directory -Force $OutDir | Out-Null
$out = (Resolve-Path -LiteralPath $OutDir).Path

# ── portable zip (flat, like the CLI's Windows archives) ─────────────────────
if ($Artifacts -in 'all', 'zip') {
    $stage = Join-Path ([System.IO.Path]::GetTempPath()) "gui-zip-$PID"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory $stage | Out-Null
    Copy-Item $exe (Join-Path $stage 'bdinfo-rs-gui.exe')
    Copy-Item (Join-Path $repo 'LICENSE') $stage
    Copy-Item (Join-Path $repo 'NOTICE') $stage
    Copy-Item (Join-Path $repo 'crates/bdinfo-rs-gui/README.md') $stage
    $zip = Join-Path $out "bdinfo-rs-gui-$Triple.zip"
    if (Test-Path $zip) { Remove-Item -Force $zip }
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip
    Remove-Item -Recurse -Force $stage
    Write-Host "packaged $zip"
}
if ($Artifacts -eq 'zip') { exit 0 }

# ── MSI (WiX v3: candle + light) ─────────────────────────────────────────────
# Resolve the toolset: PATH first, then the WIX env var the installer sets,
# then the image's standard install dir.
$candle = (Get-Command candle.exe -ErrorAction SilentlyContinue)?.Source
if (-not $candle -and $env:WIX) { $candle = Join-Path $env:WIX 'bin/candle.exe' }
if (-not $candle -or -not (Test-Path $candle)) {
    $candle = 'C:\Program Files (x86)\WiX Toolset v3.14\bin\candle.exe'
}
if (-not (Test-Path $candle)) { Write-Host 'FAILED: WiX v3 (candle.exe) not found'; exit 1 }
$light = Join-Path (Split-Path $candle) 'light.exe'

$arch = if ($Triple -eq 'aarch64-pc-windows-msvc') { 'arm64' } else { 'x64' }
$work = Join-Path ([System.IO.Path]::GetTempPath()) "gui-msi-$PID"
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory $work | Out-Null

$icon = Join-Path $packaging 'bdinfo-rs-gui.ico'
$wixobj = Join-Path $work 'setup.wixobj'
# -ext WixUIExtension: setup.wxs references the WixUI_FeatureTree dialog set,
# which lives in that extension's wixlib. WiX documents it on light alone;
# passing it to candle too is harmless and keeps the two lines symmetric.
& $candle -nologo -arch $arch -ext WixUIExtension `
    "-dVersion=$Version" "-dExePath=$exe" "-dIconPath=$icon" `
    -out $wixobj (Join-Path $packaging 'setup.wxs')
if ($LASTEXITCODE -ne 0) { Write-Host "FAILED: candle exit $LASTEXITCODE"; exit 1 }

$msi = Join-Path $out "bdinfo-rs-gui-$Triple.msi"
# -spdb: no .wixpdb sidecar (nothing consumes it).
#
# -sval: skip ICE validation entirely. The ICE suite executes as VBScript, and
# the engine's CLSID {B54F3741-5B07-11cf-A4B0-00AA004A55E8} is unregistered on
# stock Windows 11 — VBScript is a deprecated on-demand feature — so every ICE
# action aborts with LGHT0217/2738 and the link fails. This is a blanket
# suppression, not a targeted one: it also silences ICE38 and ICE43, which post
# errors rather than warnings and are precisely the rules governing the
# per-user components setup.wxs authors (an HKCU key path on every component
# installed under the profile; the same on the component carrying the
# non-advertised shortcut). That file is written so neither can fire; the
# standing check on the claim is gui-package-check.ps1 plus a real unelevated
# install, not the validator.
& $light -nologo -spdb -sval -ext WixUIExtension -out $msi $wixobj
if ($LASTEXITCODE -ne 0) { Write-Host "FAILED: light exit $LASTEXITCODE"; exit 1 }
Remove-Item -Recurse -Force $work
Write-Host "packaged $msi"
