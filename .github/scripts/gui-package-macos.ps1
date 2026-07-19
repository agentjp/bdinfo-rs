#!/usr/bin/env pwsh
# Packages the built GUI binary for macOS: assembles the "bdinfo-rs GUI.app"
# bundle (plain directory layout + the committed .icns + Info.plist.in with the
# version substituted), ad-hoc re-signs it, and wraps it in a drag-to-
# Applications .dmg via hdiutil — macOS built-ins only, the Alacritty recipe.
# Shared by the gui.yml packaging smoke and the gui-release.yml lane.
#
# The ad-hoc re-sign (--remove-signature, then --force --deep --sign -)
# replaces the linker's automatic ad-hoc signature with one sealing the whole
# assembled bundle; Apple Silicon requires signed code, and unsigned/
# un-notarized distribution is the open-source norm — users go through
# System Settings > Privacy & Security > "Open Anyway" once.

[CmdletBinding()]
param(
    # The built bdinfo-rs-gui binary to bundle.
    [Parameter(Mandatory)] [string] $Binary,
    # The crate version (fills CFBundleShortVersionString/CFBundleVersion).
    [Parameter(Mandatory)] [string] $Version,
    # The target triple the binary was built for — names the .dmg.
    [Parameter(Mandatory)]
    [ValidateSet('x86_64-apple-darwin', 'aarch64-apple-darwin')]
    [string] $Triple,
    # Output directory for the finished .dmg.
    [Parameter(Mandatory)] [string] $OutDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..' '..')).Path
$packaging = Join-Path $repo 'crates/bdinfo-rs-gui/packaging'
$binary = (Resolve-Path -LiteralPath $Binary).Path
New-Item -ItemType Directory -Force $OutDir | Out-Null
$out = (Resolve-Path -LiteralPath $OutDir).Path

# ── the .app bundle ──────────────────────────────────────────────────────────
$stage = Join-Path ([System.IO.Path]::GetTempPath()) "gui-dmg-$PID"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
$app = Join-Path $stage 'bdinfo-rs GUI.app'
New-Item -ItemType Directory (Join-Path $app 'Contents/MacOS') -Force | Out-Null
New-Item -ItemType Directory (Join-Path $app 'Contents/Resources') -Force | Out-Null

Copy-Item $binary (Join-Path $app 'Contents/MacOS/bdinfo-rs-gui')
& chmod +x (Join-Path $app 'Contents/MacOS/bdinfo-rs-gui')
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: chmod'; exit 1 }
Copy-Item (Join-Path $packaging 'bdinfo-rs-gui.icns') (Join-Path $app 'Contents/Resources')
(Get-Content -Raw (Join-Path $packaging 'Info.plist.in')).Replace('@VERSION@', $Version) |
    Set-Content -NoNewline (Join-Path $app 'Contents/Info.plist')

& plutil -lint (Join-Path $app 'Contents/Info.plist')
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: plutil lint'; exit 1 }

# ── ad-hoc re-sign, sealing the assembled bundle ─────────────────────────────
& codesign --remove-signature $app
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: codesign --remove-signature'; exit 1 }
& codesign --force --deep --sign - $app
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: codesign --sign -'; exit 1 }
& codesign --verify --deep --strict $app
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: codesign --verify'; exit 1 }

# ── the .dmg (classic drag-to-Applications layout) ───────────────────────────
& ln -s /Applications (Join-Path $stage 'Applications')
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: ln -s /Applications'; exit 1 }
$dmg = Join-Path $out "bdinfo-rs-gui-$Triple.dmg"
if (Test-Path $dmg) { Remove-Item -Force $dmg }
& hdiutil create -volname 'bdinfo-rs GUI' -srcfolder $stage -format UDZO -ov $dmg
if ($LASTEXITCODE -ne 0) { Write-Host 'FAILED: hdiutil create'; exit 1 }
Remove-Item -Recurse -Force $stage
Write-Host "packaged $dmg"
