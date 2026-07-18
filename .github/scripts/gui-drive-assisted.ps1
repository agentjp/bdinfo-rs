#!/usr/bin/env pwsh
# Strategy (b) of the GUI driving experiment (gui-drive.yml): APP-ASSISTED
# driving. The debug-only BDINFO_GUI_DRIVE seam walks the real window through a
# fixed click-everything sequence (sort → splitter → rows → Settings → scan →
# cancel → rescan → report → copy → save), dropping an `NN-name.marker` file
# after each step has rendered; this harness watches the marker directory and
# screenshots the live window on every marker — a per-step gallery of the real
# window on a real display stack, with the OS input layer deliberately out of
# the loop (that is the injection harness's job).
#
# Per-OS capture: PrintWindow(PW_RENDERFULLCONTENT) under Windows PowerShell
# 5.1 on Windows, ImageMagick `import -window root` on Linux (run the whole
# harness under `xvfb-run` so DISPLAY reaches both the app and the capture),
# `screencapture -x` on macOS.
#
# Success = the walk completed (its terminal `done` marker exists, no FAILED
# marker), the app exited 0, and the walk's saved report landed on disk. The
# gallery uploads either way — on failure the partial gallery IS the evidence.
param(
    # The debug GUI binary (the env hooks are compiled out of release builds).
    [Parameter(Mandatory)][string]$Exe,
    # The disc folder to auto-open (the committed fixture in CI).
    [Parameter(Mandatory)][string]$Disc,
    # Where the screenshots + walk evidence land (uploaded as the gallery).
    [Parameter(Mandatory)][string]$Gallery,
    # Per-step capture window, ms. Windows needs headroom for the per-shot
    # powershell.exe + Add-Type spin-up (~3-4 s); import/screencapture are fast.
    [int]$DriveMs = $(if ($IsWindows) { 6000 } else { 2500 }),
    # Artificial scan delay, ms — holds the Scanning states up long enough to
    # capture and gives the walk's Cancel step a window to land in.
    [int]$ScanDelayMs = 6000,
    # Overall deadline for the walk, seconds.
    [int]$TimeoutSec = 900
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$tempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$driveDir = Join-Path $tempBase "gui-drive-markers-$PID"
$configDir = Join-Path $tempBase "gui-drive-config-$PID"
Remove-Item -Recurse -Force $driveDir, $configDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $driveDir, $configDir, $Gallery | Out-Null

# Isolate the persisted config so the window opens at the default pinned
# geometry (880x960 logical) with default settings, never a runner-cached one.
if ($IsWindows) { $env:APPDATA = $configDir }
elseif ($IsMacOS) { $env:HOME = $configDir }
else { $env:XDG_CONFIG_HOME = $configDir }

$env:BDINFO_GUI_OPEN = (Resolve-Path $Disc).Path
$env:BDINFO_GUI_THEME = 'dark'
$env:BDINFO_GUI_SCAN_DELAY = "$ScanDelayMs"
$env:BDINFO_GUI_DRIVE = $driveDir
$env:BDINFO_GUI_DRIVE_MS = "$DriveMs"

Write-Host "==> launch $Exe (drive markers: $driveDir)"
$proc = Start-Process -FilePath (Resolve-Path $Exe).Path -PassThru

# On Windows the capture needs the window handle; elsewhere the virtual /
# runner display is captured whole (the app is the only window on it).
$hwnd = 0
if ($IsWindows) {
    for ($i = 0; $i -lt 300; $i++) {
        $proc.Refresh()
        if ($proc.HasExited) { break }
        if ($proc.MainWindowHandle -ne [IntPtr]::Zero) { $hwnd = $proc.MainWindowHandle.ToInt64(); break }
        Start-Sleep -Milliseconds 100
    }
    if ($hwnd -eq 0 -and -not $proc.HasExited) { Write-Host '!! window handle never appeared' }
}

function Save-Shot([string]$Name) {
    $png = Join-Path $Gallery "$Name.png"
    try {
        if ($IsWindows) {
            & powershell.exe -NoProfile -STA -ExecutionPolicy Bypass `
                -File (Join-Path $PSScriptRoot 'gui-drive-capture-win.ps1') -Hwnd $hwnd -Out $png
            if ($LASTEXITCODE -ne 0) { Write-Host "!! capture failed for $Name ($LASTEXITCODE)" }
        }
        elseif ($IsMacOS) { & screencapture -x $png }
        else { & import -window root $png }
    }
    catch { Write-Host "!! capture failed for ${Name}: $_" }
}

# Watch the marker directory: every new marker is a rendered state guaranteed
# to stay up for the capture window — screenshot it under the marker's name.
$seen = @{}
$failed = $false
$deadline = (Get-Date).AddSeconds($TimeoutSec)
while ($true) {
    foreach ($marker in Get-ChildItem $driveDir -Filter '*.marker' -ErrorAction SilentlyContinue | Sort-Object Name) {
        if ($seen.ContainsKey($marker.Name)) { continue }
        $seen[$marker.Name] = $true
        $stage = Get-Content $marker.FullName -Raw -ErrorAction SilentlyContinue
        Write-Host ("==> {0}  (stage: {1})" -f $marker.BaseName, $stage)
        if ($marker.Name -match 'FAILED') { $failed = $true }
        Save-Shot $marker.BaseName
    }
    $proc.Refresh()
    if ($proc.HasExited) { break }
    if ((Get-Date) -gt $deadline) {
        Write-Host "!! deadline: the walk did not finish in $TimeoutSec s"
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        $failed = $true
        break
    }
    Start-Sleep -Milliseconds 250
}
$proc.WaitForExit()

# Preserve the walk's on-disk evidence alongside the shots.
Get-ChildItem $driveDir -ErrorAction SilentlyContinue |
    Copy-Item -Destination $Gallery -Force -ErrorAction SilentlyContinue

# Verdict: walk complete + clean exit + the saved report actually landed.
$done = Test-Path (Join-Path $driveDir '*-done.marker')
$report = @(Get-ChildItem $driveDir -Filter 'BDINFO.*.txt' -ErrorAction SilentlyContinue)
$shots = @(Get-ChildItem $Gallery -Filter '*.png' -ErrorAction SilentlyContinue)
Write-Host ("==> exit={0} markers={1} shots={2} done={3} failed={4} report={5}" -f `
        $proc.ExitCode, $seen.Count, $shots.Count, $done, $failed, $report.Count)
if ($failed -or -not $done -or $proc.ExitCode -ne 0 -or $report.Count -eq 0) {
    Write-Host '!! assisted walk FAILED (the uploaded gallery holds whatever was captured)'
    exit 1
}
Write-Host '==> assisted walk complete'
