#!/usr/bin/env pwsh
# Strategy (a) of the GUI driving experiment (gui-drive.yml), macOS leg —
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

Write-Host "==> launch $Exe"
$proc = Start-Process -FilePath (Resolve-Path $Exe).Path -PassThru
Start-Sleep -Milliseconds ($ScanDelayMs + 10000)

function Save-Shot([string]$Name) {
    & screencapture -x (Join-Path $Gallery "$Name.png")
    if ($LASTEXITCODE -ne 0) { Write-Host "!! screencapture failed for $Name ($LASTEXITCODE)" }
}
Save-Shot '00-listed'

# Window geometry via System Events — the first Accessibility-gated call.
$failures = @()
$geom = & osascript -e 'tell application "System Events" to get {position, size} of window 1 of (first process whose name is "bdinfo-rs-gui")' 2>&1
if ($LASTEXITCODE -ne 0) {
    $failures += "System Events geometry lookup: $geom"
    Write-Host "!! $($failures[-1])"
}
else {
    Write-Host "==> window geometry: $geom"
    $parts = @("$geom" -split ',\s*')
    $gx = [double]$parts[0]; $gy = [double]$parts[1]; $gw = [double]$parts[2]
    $scale = $gw / 880.0
    function Send-Click([double]$Lx, [double]$Ly) {
        $px = [int][math]::Round($gx + ($Lx * $scale))
        $py = [int][math]::Round($gy + ($Ly * $scale))
        $out = & cliclick "c:$px,$py" 2>&1
        if ($LASTEXITCODE -ne 0) { $script:failures += "cliclick c:${px},${py}: $out"; Write-Host "!! $out" }
        Start-Sleep -Milliseconds 900
    }
    # The shared logical coordinate table — see gui-drive-inject-windows.ps1.
    Send-Click 111 127; Save-Shot '01-select-all'
    Send-Click 484 170; Save-Shot '02-sort-length'
    Send-Click 817 931; Save-Shot '04-settings-open'
    Send-Click 538 645; Save-Shot '05-settings-cancel'
}

Write-Host '==> waiting for the smoke-deadline exit'
$exited = $proc.WaitForExit($SmokeMs)
if (-not $exited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
$code = if ($exited) { $proc.ExitCode } else { 'killed' }
Write-Host "==> app exit code $code"
if ($failures.Count -gt 0) {
    Write-Host '!! macOS injection blocked — recorded failures:'
    $failures | ForEach-Object { Write-Host "   $_" }
    exit 1
}
if (-not $exited -or $proc.ExitCode -ne 0) { exit 1 }
Write-Host '==> injection walk complete'
