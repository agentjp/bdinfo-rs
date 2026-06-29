#!/usr/bin/env pwsh
# Source-rule guard for the bdinfo-rs-core LIBRARY — two house rules that no clippy
# lint covers, enforced IDENTICALLY by the local gate (scripts/compliance.ps1) and
# CI (the `lint` job in ci.yml). This script is the single source of truth for both,
# exactly like .github/scripts/check-banned-words.ps1.
#
#   (1) Big-endian only. Disc structures are big-endian, so from_ne_bytes /
#       from_le_bytes / to_ne_bytes silently misreads on a little-endian host (x86).
#       The vfs::udf reader is little-endian BY SPEC (ECMA-167) and is the sole
#       exemption to this rule (HashMap/HashSet still applies there).
#   (2) No HashMap/HashSet in the deterministic output path. Rust's HashMap is
#       per-instance seed-randomized, so it yields nondeterministic iteration order
#       and therefore nondeterministic report bytes — use BTreeMap/BTreeSet or a
#       sorted Vec.
#
# #[cfg(test)] modules are exempt (test code may use either freely). Exits 1 with a
# list of offenders on any violation in non-test library code, 0 when clean.

[CmdletBinding()]
param(
    # Default: crates/bdinfo-rs-core/src relative to this script (.github/scripts/).
    [string] $SrcDir = [System.IO.Path]::Combine($PSScriptRoot, '..', '..', 'crates', 'bdinfo-rs-core', 'src')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$srcPath = (Resolve-Path -LiteralPath $SrcDir).Path
$rules = @(
    @{ Rx = 'from_ne_bytes|from_le_bytes|to_ne_bytes'; Why = 'host/little-endian byte op (disc bytes are big-endian; use from_be_bytes)'; Exempt = '[\\/]udf([\\/]|\.rs$)' }
    @{ Rx = '\bHashMap\b|\bHashSet\b'; Why = 'nondeterministic collection in an output path (use BTreeMap/BTreeSet or a sorted Vec)' }
)

$hits = @()
foreach ($file in @(Get-ChildItem -LiteralPath $srcPath -Recurse -File -Filter *.rs)) {
    # Track brace depth so a #[cfg(test)] mod can be skipped wholesale. $testAt is the
    # depth at which the active test module opened, or -1 when not inside one.
    $depth = 0; $testAt = -1; $pending = $false; $lineNo = 0
    foreach ($raw in [System.IO.File]::ReadAllLines($file.FullName)) {
        $lineNo++
        # Drop line/doc comments so prose ("never HashMap") and braces in comments
        # can't trip the scan.
        $code = $raw -replace '//.*$', ''
        if ($testAt -lt 0) {
            if ($pending) {
                # We saw #[cfg(test)] and are deciding what it attaches to. Skip blank
                # lines and STACKED attributes (#[allow(...)] between cfg(test) and the
                # item); the first real token decides: a `mod ... {` is a test module
                # (exempt), anything else (a test fn/use/const) is NOT a module — clear
                # pending so a LATER production `mod` is never mis-exempted (the G-06
                # latent false-negative the old single-flag walk allowed).
                if (-not $code.Trim()) { }
                elseif ($code -match '^\s*#!?\[') { }
                elseif ($code -match '\bmod\b.*\{') { $testAt = $depth; $pending = $false }
                else { $pending = $false }
            }
            elseif ($code -match '#\[cfg\(test\)\]') {
                $pending = $true
            }
            else {
                foreach ($r in $rules) {
                    if ($r.ContainsKey('Exempt') -and $file.FullName -match $r.Exempt) { continue }
                    if ($code -match $r.Rx) { $hits += "    $($file.Name):$lineNo  $($r.Why)" }
                }
            }
        }
        $depth += ([regex]::Matches($code, '\{')).Count - ([regex]::Matches($code, '\}')).Count
        if ($testAt -ge 0 -and $depth -le $testAt) { $testAt = -1 }
    }
}

if ($hits.Count) {
    Write-Host 'FAILED: source rules — forbidden constructs in non-test library code:' -ForegroundColor Red
    $hits | ForEach-Object { Write-Host $_ -ForegroundColor DarkGray }
    exit 1
}
Write-Host 'source rules: ok (big-endian only; no HashMap/HashSet outside tests)' -ForegroundColor Green
exit 0
