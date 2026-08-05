#!/usr/bin/env pwsh
# Tripwire guards for the BROWSER package (crates/bdinfo-rs-wasm) — the two
# rules no clippy lint covers, enforced IDENTICALLY by the local gate
# (scripts/wasm-compliance.ps1) and CI (the `gate` job in wasm.yml). This script
# is the single source of truth for both, exactly like
# .github/scripts/gui-rules.ps1 and source-rules.ps1.
#
#   -Check licenses
#       The npm tarball ships web/ copies of the repo-root LICENSE and NOTICE:
#       under the LGPL the license text and the use-notice must travel with the
#       statically-linked code. They are MANUAL copies, so assert each still
#       matches its root original byte for byte (re-copy on drift). Cheap and
#       toolchain-free, so a caller can run it before installing anything.
#   -Check unsafe
#       Exactly three `unsafe` tokens in src/lib.rs, all of them cfg-gated
#       marker impls: `unsafe impl Send for WebFile` (the folder backend) plus
#       `unsafe impl Send` and `unsafe impl Sync for WebIso` (the .iso backend —
#       IsoReader requires Send + Sync, so that marker needs both impls). Any
#       other count is a new unsafe block or a deleted marker, and either is a
#       deliberate decision that this count records. Unlike the GUI crate, which
#       carries #![forbid(unsafe_code)], these three are the memory-safety
#       exception the browser bindings need, so the guard is a fixed count
#       rather than zero.
#
# Exits 1 naming the offender on any violation, 0 when clean.

[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('licenses', 'unsafe')]
    [string] $Check,

    # Default: crates/bdinfo-rs-wasm relative to this script (.github/scripts/).
    [string] $Crate = [System.IO.Path]::Combine($PSScriptRoot, '..', '..', 'crates', 'bdinfo-rs-wasm')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$crate = (Resolve-Path -LiteralPath $Crate).Path
$repoRoot = (Resolve-Path -LiteralPath (Join-Path $crate '..' '..')).Path

if ($Check -eq 'licenses') {
    foreach ($name in @('LICENSE', 'NOTICE')) {
        $rootHash = (Get-FileHash -LiteralPath (Join-Path $repoRoot $name) -Algorithm SHA256).Hash
        $webHash = (Get-FileHash -LiteralPath (Join-Path $crate 'web' $name) -Algorithm SHA256).Hash
        if ($rootHash -ne $webHash) {
            Write-Host "FAILED: crates/bdinfo-rs-wasm/web/$name does not match the repo-root $name (re-copy it)." -ForegroundColor Red
            exit 1
        }
        Write-Host "    ok: web/$name matches root" -ForegroundColor DarkGray
    }
    exit 0
}

# Strip // comments before counting: the module and SAFETY docs say "unsafe" in
# prose, so a bare match would count those too. `\bunsafe\b` also leaves
# `unsafe_code` alone.
$code = ((Get-Content -LiteralPath (Join-Path $crate 'src/lib.rs')) -replace '//.*$', '') -join "`n"
$found = ([regex]::Matches($code, '\bunsafe\b')).Count
if ($found -ne 3) {
    Write-Host "FAILED: expected exactly 3 unsafe tokens in src/lib.rs (cfg-gated WebFile Send + WebIso Send/Sync), found $found" -ForegroundColor Red
    exit 1
}
Write-Host '    ok: 3 unsafe tokens (cfg-gated WebFile Send + WebIso Send/Sync)' -ForegroundColor DarkGray
exit 0
