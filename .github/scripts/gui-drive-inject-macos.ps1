#!/usr/bin/env pwsh
# Strategy (a) of the GUI drive gate (gui.yml, drive-inject), macOS leg —
# SOFT/EXPERIMENTAL by design. Synthetic input on macOS needs the
# Accessibility permission (both the System Events geometry lookup and
# cliclick's CGEvent posts), which hosted runners are not expected to grant to
# an unattended shell; the point of this leg is to RECORD exactly where that
# wall is, per the experiment's brief. Every failure path logs the precise
# error and still uploads whatever screencapture produced.
param(
    [Parameter(Mandatory)][string]$Exe,
    [Parameter(Mandatory)][string]$Disc,
    [Parameter(Mandatory)][string]$Gallery,
    [int]$ScanDelayMs = 5000,
    [int]$SmokeMs = 120000
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$tempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$configDir = Join-Path $tempBase "gui-inject-config-$PID"
Remove-Item -Recurse -Force $configDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $configDir, $Gallery | Out-Null
$env:HOME = $configDir
$env:BDINFO_GUI_OPEN = (Resolve-Path $Disc).Path
$env:BDINFO_GUI_THEME = 'dark'
$env:BDINFO_GUI_SCAN_DELAY = "$ScanDelayMs"
$env:BDINFO_GUI_SMOKE_MS = "$SmokeMs"
# Boot geometry through the app's own seam — the shared 880x960 size. The mac
# runner's paravirtual display may clamp a tall window if the desktop is
# small, so raise the resolution first (best-effort; the System Events rect
# read later rules the click math and the crop regardless).
$env:BDINFO_GUI_WIN = '880x960'
$disp = (& displayplacer list 2>$null | Select-String 'Persistent screen id:\s*(\S+)' | Select-Object -First 1).Matches.Groups[1].Value
if ($disp) {
    & displayplacer "id:$disp res:1920x1080" 2>$null
    Write-Host "==> macOS display id $disp -> 1920x1080 (exit $LASTEXITCODE; best-effort)"
}

# Grant/dismiss the Sequoia screen-capture consent on the EMPTY desktop before
# the app launches, so pressing the dialog's default button can never reach the
# app. A warm-up capture raises it; click Allow via AX, press Return (default
# button, for the WindowServer-drawn variant AX can't see), and cliclick the
# button's usual spot — belt, braces, and a spare. Allow grants for the session.
& screencapture -x (Join-Path ([IO.Path]::GetTempPath()) 'gui-drive-prime.png') 2>$null
Start-Sleep -Milliseconds 2500
& osascript -e 'tell application "System Events"
    repeat with p in every process
        try
            if exists (button "Allow" of window 1 of p) then click button "Allow" of window 1 of p
        end try
    end repeat
    key code 36
end tell' 2>$null
Start-Sleep -Milliseconds 1200
Write-Host '==> macOS screen-capture consent primed (Allow + Return on empty desktop)'

Write-Host "==> launch $Exe (OPEN=$env:BDINFO_GUI_OPEN WIN=$env:BDINFO_GUI_WIN)"
$proc = Start-Process -FilePath (Resolve-Path $Exe).Path -PassThru
Start-Sleep -Milliseconds ($ScanDelayMs + 10000)

# Window geometry via System Events — the click math and the crop rect.
$failures = @()
$geom = & osascript -e 'tell application "System Events" to get {position, size} of window 1 of (first process whose name is "bdinfo-rs-gui")' 2>&1
$geomOk = $LASTEXITCODE -eq 0 -and $geom

# Crop every shot to the window rect (comparable to the other OSes), falling
# back to the whole screen if geometry is unknown.
$macRect = $null
function Save-Shot([string]$Name) {
    $png = Join-Path $Gallery "$Name.png"
    if ($script:macRect) { & screencapture -x -R $script:macRect $png } else { & screencapture -x $png }
    if ($LASTEXITCODE -ne 0) { Write-Host "!! screencapture failed for $Name ($LASTEXITCODE)" }
}

if (-not $geomOk) {
    $failures += "System Events geometry lookup: $geom"
    Write-Host "!! $($failures[-1])"
}
else {
    Write-Host "==> window geometry: $geom"
    # {position, size} = x, y, w, h of the whole window, TITLE BAR INCLUDED;
    # the content area starts ~28 pt below. The runner's paravirtual display
    # is small enough to clamp a tall window, so the bottom bar and the centred
    # dialog track the granted height.
    $parts = @("$geom" -split ',\s*')
    $gx = [double]$parts[0]; $gy = [double]$parts[1]; $gw = [double]$parts[2]; $gh = [double]$parts[3]
    $macRect = "$([int]$gx),$([int]$gy),$([int]$gw),$([int]$gh)"
    $title = 28.0
    $scale = $gw / 880.0
    $logicalH = ($gh - $title) / $scale
    function Send-Click([double]$Lx, [double]$Ly) {
        $px = [int][math]::Round($gx + ($Lx * $scale))
        $py = [int][math]::Round($gy + $title + ($Ly * $scale))
        $out = & cliclick "c:$px,$py" 2>&1
        if ($LASTEXITCODE -ne 0) { $script:failures += "cliclick c:${px},${py}: $out"; Write-Host "!! $out" }
        Start-Sleep -Milliseconds 900
    }
    Save-Shot '00-listed'
    # The shared logical coordinate table — see gui-drive-inject-windows.ps1.
    Send-Click 111 127; Save-Shot '01-select-all'
    Send-Click 484 170; Save-Shot '02-sort-length'
    Send-Click 817 ($logicalH - 29); Save-Shot '04-settings-open'
    Send-Click 538 (($logicalH / 2) + 165); Save-Shot '05-settings-cancel'
    # Same hard assert as the other legs: the Settings click must change the
    # window pixels vs the shot before it. Now robust — the shots are cropped
    # to the window rect, so the menu-bar clock is out of frame.
    $before = Get-FileHash (Join-Path $Gallery '02-sort-length.png') -ErrorAction SilentlyContinue
    $after = Get-FileHash (Join-Path $Gallery '04-settings-open.png') -ErrorAction SilentlyContinue
    $landed = $before -and $after -and ($before.Hash -ne $after.Hash)
    Write-Host "==> settings-click pixel change: $landed"
    if (-not $landed) { $failures += 'the Settings click changed no pixels (missed or blocked)' }
}

Write-Host '==> waiting for the smoke-deadline exit'
$exited = $proc.WaitForExit($SmokeMs)
if (-not $exited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
$code = if ($exited) { $proc.ExitCode } else { 'killed' }
Write-Host "==> app exit code $code"
# The app's per-launch diagnostics log rides along in the gallery.
Get-ChildItem $configDir -Recurse -Filter '*.log' -ErrorAction SilentlyContinue |
    Copy-Item -Destination $Gallery -Force -ErrorAction SilentlyContinue
if ($failures.Count -gt 0) {
    Write-Host '!! macOS injection blocked — recorded failures:'
    $failures | ForEach-Object { Write-Host "   $_" }
    exit 1
}
if (-not $exited -or $proc.ExitCode -ne 0) { exit 1 }
Write-Host '==> injection walk complete'
