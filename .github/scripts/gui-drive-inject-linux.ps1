#!/usr/bin/env pwsh
# Strategy (a) of the GUI drive gate (gui.yml, the drive job's "Inject the
# walk" steps), Linux leg:
# OS-LEVEL INPUT INJECTION with xdotool under xvfb (run this whole script via
# `xvfb-run -a pwsh ...` so DISPLAY reaches the app, xdotool, and the capture
# alike). Same walk, coordinates and pixel-change assert as the Windows leg
# (see gui-drive-inject-windows.ps1); the targets come from the shared
# _gui-walk.ps1, which carries why one table serves every leg. Under bare
# Xvfb there is no
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
# Boot geometry through the app's own seam — the shared 880x960 size across
# every driving leg (the xvfb screen is 1920x1080, so it fits with room).
$env:BDINFO_GUI_WIN = '880x960+0+0'

Write-Host "==> launch $Exe (DISPLAY=$env:DISPLAY OPEN=$env:BDINFO_GUI_OPEN WIN=$env:BDINFO_GUI_WIN)"
$proc = Start-Process -FilePath (Resolve-Path $Exe).Path -PassThru

# Find the X11 window (xdotool --sync blocks until it exists), then focus it
# so keyboard events have a target.
$wid = & xdotool search --sync --onlyvisible --name '^bdinfo-rs' | Select-Object -First 1
if (-not $wid) { throw 'xdotool never found the window' }
& xdotool windowfocus --sync $wid

Write-Host '==> waiting for the structural scan (delay + listing)'
Start-Sleep -Milliseconds ($ScanDelayMs + 7000)

# Geometry only now, after the listing rendered (winit applies the requested
# size a beat after the window exists). Under bare Xvfb the undecorated
# window IS the client area.
$geo = & xdotool getwindowgeometry --shell $wid
$gx = [int](($geo | Select-String '^X=(\-?\d+)').Matches.Groups[1].Value)
$gy = [int](($geo | Select-String '^Y=(\-?\d+)').Matches.Groups[1].Value)
$gw = [int](($geo | Select-String '^WIDTH=(\d+)').Matches.Groups[1].Value)
$gh = [int](($geo | Select-String '^HEIGHT=(\d+)').Matches.Groups[1].Value)
$scale = $gw / 880.0
# The logical height actually granted — the bottom bar and dialog track it.
$logicalH = $gh / $scale
Write-Host "==> window $wid at $gx,$gy size ${gw}x${gh} (scale $scale, logical height $([int]$logicalH))"

function Save-Shot([string]$Name) {
    # Crop to the app window (not the whole 1920x1080 root) so the gallery
    # carries no desktop margin and matches the other OSes' framing.
    & import -window $wid (Join-Path $Gallery "$Name.png")
    if ($LASTEXITCODE -ne 0) { Write-Host "!! capture failed for $Name" }
}

function Send-Click([double[]]$Target) {
    $px = [int][math]::Round($gx + ($Target[0] * $scale))
    $py = [int][math]::Round($gy + ($Target[1] * $scale))
    & xdotool mousemove --sync $px $py click 1
    Start-Sleep -Milliseconds 900
}

# The click targets, from the table all three injected legs share.
. (Join-Path $PSScriptRoot '_gui-walk.ps1')
$walk = Get-GuiWalkTargets -LogicalHeight $logicalH

Save-Shot '00-listed'

Send-Click $walk.SelectAll; Save-Shot '01-select-all'
Send-Click $walk.LengthHeader; Save-Shot '02-sort-length'
Send-Click $walk.FirstRow; Save-Shot '03-row-activate'
Send-Click $walk.SettingsBtn; Save-Shot '04-settings-open'
Send-Click $walk.DialogCancel; Save-Shot '05-settings-cancel'
Send-Click $walk.ScanBtn
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
# The app's per-launch diagnostics log rides along in the gallery.
Get-ChildItem $configDir -Recurse -Filter '*.log' -ErrorAction SilentlyContinue |
    Copy-Item -Destination $Gallery -Force -ErrorAction SilentlyContinue
if ($proc.ExitCode -ne 0 -or -not $landed) {
    Write-Host '!! injection walk FAILED (the uploaded gallery holds whatever was captured)'
    exit 1
}
Write-Host '==> injection walk complete'
