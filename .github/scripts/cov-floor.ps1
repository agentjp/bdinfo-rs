#!/usr/bin/env pwsh
# Coverage-floor evaluator over a cargo-llvm-cov JSON export, enforced
# IDENTICALLY by the local gates (scripts/compliance.ps1,
# scripts/wasm-compliance.ps1, scripts/gui-compliance.ps1) and CI (the coverage
# steps in wasm.yml and gui.yml). This script is the single source of truth for
# all five, exactly like .github/scripts/source-rules.ps1 and gui-rules.ps1.
#
# The line/region/function floor is exact — covered == count, never a rounded
# percentage — so a single uncovered line cannot round its way to 100.00%.
# -Include/-Exclude select the measured denominator: a crate whose coverage
# report also carries a path dependency's files filters by crate directory, and
# a crate that verifies part of its surface by other means (rendered pixels,
# byte-exact parity) excludes those files by name.
#
# The BRANCH floors are percentages rather than covered == count, and they are
# PLATFORM-VARIANT: a library with #[cfg(windows)] arms has a different branch
# count on each host, so the same source clears a floor on one OS and misses it
# on another. Only a caller that knows it runs on one developer's machine may
# pass them; a CI caller enforces the platform-invariant three and leaves
# -BranchFloor / -FileBranchFloor unset.
#
# Prints one line per metric, and on failure the per-file breakdown naming every
# file with a gap — the totals alone never say WHICH file is short. Exits 1 on
# any shortfall (or when the filters select no file at all), 0 when clean.

[CmdletBinding()]
param(
    # The --json --output-path export of `cargo llvm-cov`.
    [Parameter(Mandatory)]
    [string] $JsonPath,

    # Regex a file's path must match to enter the denominator. Empty = every
    # file in the report.
    [string] $Include = '',

    # Regex a file's path must NOT match to enter the denominator. Empty = no
    # exclusions.
    [string] $Exclude = '',

    # Names the measured surface in the pass/fail lines.
    [string] $Label = 'coverage',

    # Minimum branch percentage over the whole denominator. 0 = not enforced.
    [int] $BranchFloor = 0,

    # Minimum branch percentage for each individual file. 0 = not enforced.
    [int] $FileBranchFloor = 0,

    # Files with fewer branches than this are exempt from -FileBranchFloor: a
    # file with two branches reads as 50% the moment one arm is unreachable by
    # construction, which says nothing about how well it is tested.
    [int] $FileBranchMinCount = 5
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$data = (Get-Content -LiteralPath $JsonPath -Raw | ConvertFrom-Json).data[0]

$files = @($data.files)
if ($Include) { $files = @($files | Where-Object { $_.filename -match $Include }) }
if ($Exclude) { $files = @($files | Where-Object { $_.filename -notmatch $Exclude }) }

if (-not $files.Count) {
    Write-Host "FAILED: $Label — the coverage report holds no file matching the measured surface." -ForegroundColor Red
    if ($Include) { Write-Host "    include: $Include" -ForegroundColor DarkGray }
    if ($Exclude) { Write-Host "    exclude: $Exclude" -ForegroundColor DarkGray }
    exit 1
}

# Summed from the selected files rather than read from the report's own totals,
# which describe every file it measured — including the ones the filters drop.
function Get-MetricTotal {
    param([object[]] $File, [string] $Metric)
    $covered = (@($File | ForEach-Object { $_.summary.$Metric.covered }) | Measure-Object -Sum).Sum
    $count = (@($File | ForEach-Object { $_.summary.$Metric.count }) | Measure-Object -Sum).Sum
    return [pscustomobject]@{
        Covered = $covered
        Count   = $count
        Percent = if ($count) { 100.0 * $covered / $count } else { 100.0 }
    }
}

$failures = @()

foreach ($metric in @('lines', 'regions', 'functions')) {
    $t = Get-MetricTotal -File $files -Metric $metric
    Write-Host ("    {0,-10}{1,7:N2}%  ({2}/{3})" -f $metric, $t.Percent, $t.Covered, $t.Count) -ForegroundColor DarkGray
    if ($t.Covered -lt $t.Count) {
        $failures += "$metric $([math]::Round($t.Percent, 2))% < 100% ($($t.Covered)/$($t.Count))"
    }
}

if ($BranchFloor -gt 0) {
    $b = Get-MetricTotal -File $files -Metric 'branches'
    Write-Host ("    {0,-10}{1,7:N2}%  ({2}/{3})  floor {4}%" -f 'branches', $b.Percent, $b.Covered, $b.Count, $BranchFloor) -ForegroundColor DarkGray
    if ($b.Percent -lt $BranchFloor) {
        $failures += "branches $([math]::Round($b.Percent, 2))% < $BranchFloor% floor"
    }
}

if ($FileBranchFloor -gt 0) {
    foreach ($f in $files) {
        $fb = $f.summary.branches
        if ($fb.count -ge $FileBranchMinCount -and $fb.percent -lt $FileBranchFloor) {
            $leaf = Split-Path $f.filename -Leaf
            $failures += "branches[$leaf] $([math]::Round($fb.percent, 2))% < $FileBranchFloor% per-file floor ($($fb.covered)/$($fb.count))"
        }
    }
}

if (-not $failures.Count) { exit 0 }

Write-Host "FAILED: $Label below floor (ratchets only go UP):" -ForegroundColor Red
$failures | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }

Write-Host '    files with an uncovered line/region/function:' -ForegroundColor DarkGray
foreach ($f in $files) {
    $gaps = @()
    foreach ($metric in @('functions', 'lines', 'regions')) {
        $s = $f.summary.$metric
        if ($s.covered -lt $s.count) { $gaps += ("{0} {1}/{2}" -f $metric, $s.covered, $s.count) }
    }
    if ($gaps.Count) { Write-Host "      $(Split-Path $f.filename -Leaf):  $($gaps -join '  ')" -ForegroundColor DarkGray }
}

exit 1
