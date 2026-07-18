#!/usr/bin/env pwsh
# Strategy (a) of the GUI driving experiment (gui-drive.yml), Linux leg:
# OS-LEVEL INPUT INJECTION with xdotool under xvfb (run this whole script via
# `xvfb-run -a pwsh ...` so DISPLAY reaches the app, xdotool, and the capture
# alike). Same walk, coordinates, and pixel-change assert as the Windows leg
# (see gui-drive-inject-windows.ps1) — the logical layout is identical across
# platforms (bundled fonts, pinned 880x960 geometry, dark theme, isolated
# config), so one coordinate table serves both. Under bare Xvfb there is no
# window manager: the window sits undecorated at its X11 position, and
# `xdotool getwindowgeometry` gives the client origin directly.
param(
    [Parameter(Mandatory)][string]$Exe,
    [Parameter(Mandatory)][string]$Disc,
    [Parameter(Mandatory)][string]$Gallery,
    [int]$ScanDelayMs = 5000,
    [int]$SmokeMs = 150000
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$tempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$configDir = Join-Path $tempBase "gui-inject-config-$PID"
Remove-Item -Recurse -Force $configDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $configDir, $Gallery | Out-Null
$env:XDG_CONFIG_HOME = $configDir
$env:BDINFO_GUI_OPEN = (Resolve-Path $Disc).Path
$env:BDINFO_GUI_THEME = 'dark'
$env:BDINFO_GUI_SCAN_DELAY = "$ScanDelayMs"
$env:BDINFO_GUI_SMOKE_MS = "$SmokeMs"

Write-Host "==> launch $Exe (DISPLAY=$env:DISPLAY)"
$proc = Start-Process -FilePath (Resolve-Path $Exe).Path -PassThru

# Find the X11 window (xdotool --sync blocks until it exists), then focus it
# so keyboard events have a target.
$wid = & xdotool search --sync --onlyvisible --name '^bdinfo-rs' | Select-Object -First 1
if (-not $wid) { throw 'xdotool never found the window' }
& xdotool windowfocus --sync $wid
$geo = & xdotool getwindowgeometry --shell $wid
$gx = [int](($geo | Select-String '^X=(\-?\d+)').Matches.Groups[1].Value)
$gy = [int](($geo | Select-String '^Y=(\-?\d+)').Matches.Groups[1].Value)
$gw = [int](($geo | Select-String '^WIDTH=(\d+)').Matches.Groups[1].Value)
$scale = $gw / 880.0
Write-Host "==> window $wid at $gx,$gy width $gw (scale $scale)"

function Save-Shot([string]$Name) {
    & import -window root (Join-Path $Gallery "$Name.png")
    if ($LASTEXITCODE -ne 0) { Write-Host "!! capture failed for $Name" }
}

function Send-Click([double]$Lx, [double]$Ly) {
    $px = [int][math]::Round($gx + ($Lx * $scale))
    $py = [int][math]::Round($gy + ($Ly * $scale))
    & xdotool mousemove --sync $px $py click 1
    Start-Sleep -Milliseconds 900
}

# The shared logical coordinate table — see gui-drive-inject-windows.ps1.
$SelectAll = @(111, 127)
$LengthHeader = @(484, 170)
$FirstRow = @(110, 206)
$SettingsBtn = @(817, 931)
$DialogCancel = @(538, 645)
$ScanBtn = @(566, 932)

Write-Host '==> waiting for the structural scan (delay + listing)'
Start-Sleep -Milliseconds ($ScanDelayMs + 7000)
Save-Shot '00-listed'

Send-Click @SelectAll; Save-Shot '01-select-all'
Send-Click @LengthHeader; Save-Shot '02-sort-length'
Send-Click @FirstRow; Save-Shot '03-row-activate'
Send-Click @SettingsBtn; Save-Shot '04-settings-open'
Send-Click @DialogCancel; Save-Shot '05-settings-cancel'
Send-Click @ScanBtn
Start-Sleep -Milliseconds 1500
Save-Shot '06-scanning'
Write-Host '==> waiting for the measured scan'
Start-Sleep -Milliseconds ($ScanDelayMs + 25000)
Save-Shot '07-scanned'

# Keyboard probe (recorded, not asserted): Ctrl+plus steps the UI scale.
& xdotool key ctrl+plus
Start-Sleep -Milliseconds 900
Save-Shot '08-scale-step'

# Same hard assert as the Windows leg: the Settings click must change pixels.
$before = Get-FileHash (Join-Path $Gallery '03-row-activate.png') -ErrorAction SilentlyContinue
$after = Get-FileHash (Join-Path $Gallery '04-settings-open.png') -ErrorAction SilentlyContinue
$landed = $before -and $after -and ($before.Hash -ne $after.Hash)
Write-Host "==> settings-click pixel change: $landed"

Write-Host '==> waiting for the smoke-deadline exit'
$exited = $proc.WaitForExit($SmokeMs)
if (-not $exited) { Stop-Process -Id $proc.Id -Force; throw 'the app never hit its smoke deadline' }
Write-Host ("==> app exit code {0}" -f $proc.ExitCode)
if ($proc.ExitCode -ne 0 -or -not $landed) {
    Write-Host '!! injection walk FAILED (the uploaded gallery holds whatever was captured)'
    exit 1
}
Write-Host '==> injection walk complete'
