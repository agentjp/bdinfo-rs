#!/usr/bin/env pwsh
# Trusted-publishing trigger gate: no workflow may mint a registry token from a
# `workflow_run` trigger.
#
# crates.io answers an OIDC token request made from a `workflow_run` run with
# HTTP 400 — "Trusted Publishing does not support the `workflow_run` event
# trigger due to security concerns. Please use a different trigger such as
# `push`, `release`, or `workflow_dispatch`." npm's trusted publishing rejects
# the same trigger. The failure is invisible until a real release: the token is
# only requested at publish time, and a `cargo publish --dry-run` earlier in the
# same workflow needs no token and passes.
#
# This is not the same thing as `id-token: write` under `workflow_run`, which is
# legitimate and used here — GitHub's own attestation service (docker.yml) has no
# such restriction. So the check matches the two registry mints by name rather
# than the permission.
#
# No per-file linter catches this: it is a relationship between a trigger and a
# step, and both are individually valid.
#
# Usage:
#   pwsh check-trusted-publishing-triggers.ps1 [-Path <workflow dir>]
#   pwsh check-trusted-publishing-triggers.ps1 -SelfTest
# Exit code: 0 = no workflow mints from a workflow_run trigger, 1 = at least one
# does (or a self-test case failed).

[CmdletBinding()]
param(
    [string] $Path,
    [switch] $SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# The registry mints. Each is the exact spelling that requests a trusted-publishing
# token, so a workflow can name the mechanism in prose without matching.
$script:Mints = @(
    @{ Pattern = 'rust-lang/crates-io-auth-action'; What = 'crates.io OIDC mint' }
    @{ Pattern = 'npm\s+(stage\s+)?publish'; What = 'npm publish' }
)

# True when the file's top-level `on:` block declares a workflow_run trigger.
# Anchored to the block so that `github.event.workflow_run.conclusion` in a job
# condition — which every workflow_run consumer uses — is never mistaken for the
# trigger itself.
function Test-WorkflowRunTrigger {
    param([string[]] $Lines)

    $inOn = $false
    foreach ($line in $Lines) {
        if ($line -match '^\s*#') { continue }
        if ($line -match '^["'']?on["'']?\s*:') { $inOn = $true; continue }
        if (-not $inOn) { continue }
        # The block ends at the next top-level key.
        if ($line -match '^\S') { break }
        if ($line -match '^\s{1,4}workflow_run\s*:') { return $true }
    }
    return $false
}

# The mints a file requests, ignoring comment lines so that a header explaining
# this very rule does not trip it.
function Get-MintsRequested {
    param([string[]] $Lines)

    $hits = @()
    foreach ($line in $Lines) {
        if ($line -match '^\s*#') { continue }
        foreach ($mint in $script:Mints) {
            if ($line -match $mint.Pattern -and $hits -notcontains $mint.What) { $hits += $mint.What }
        }
    }
    return $hits
}

function Test-TrustedPublishingTriggers {
    param([string] $Dir)

    $found = @()
    foreach ($file in Get-ChildItem -LiteralPath $Dir -Filter '*.yml' | Sort-Object Name) {
        $lines = Get-Content -LiteralPath $file.FullName
        if (-not (Test-WorkflowRunTrigger $lines)) { continue }
        foreach ($what in Get-MintsRequested $lines) {
            $found += "$($file.Name): $what runs under a workflow_run trigger"
        }
    }
    return $found
}

# --- self-test --------------------------------------------------------------

if ($SelfTest) {
    $root = Join-Path ([System.IO.Path]::GetTempPath()) "tpt-selftest-$([System.Guid]::NewGuid())"
    $cases = @(
        @{ Name = 'workflow_run + crates mint'; Expect = 1; Body = @'
on:
  workflow_run:
    workflows: [build]
    types: [completed]
jobs:
  publish:
    steps:
      - uses: rust-lang/crates-io-auth-action@abc # v1.0.5
'@ }
        @{ Name = 'tag push + crates mint'; Expect = 0; Body = @'
on:
  push:
    tags: ["v*"]
jobs:
  publish:
    steps:
      - uses: rust-lang/crates-io-auth-action@abc # v1.0.5
'@ }
        @{ Name = 'workflow_run, no mint'; Expect = 0; Body = @'
on:
  workflow_run:
    workflows: [build]
    types: [completed]
jobs:
  ship:
    steps:
      - run: echo hello
'@ }
        @{ Name = 'dispatch + mint, workflow_run only in an expression'; Expect = 0; Body = @'
on:
  workflow_dispatch:
jobs:
  publish:
    if: github.event.workflow_run.conclusion == 'success'
    steps:
      - uses: rust-lang/crates-io-auth-action@abc # v1.0.5
'@ }
        @{ Name = 'workflow_run + npm publish'; Expect = 1; Body = @'
on:
  workflow_run:
    workflows: [build]
    types: [completed]
jobs:
  publish:
    steps:
      - run: npm stage publish --provenance
'@ }
        @{ Name = 'workflow_run + mint named only in a comment'; Expect = 0; Body = @'
on:
  workflow_run:
    workflows: [build]
    types: [completed]
jobs:
  ship:
    # rust-lang/crates-io-auth-action cannot be used here; see the header.
    steps:
      - run: echo hello
'@ }
    )

    $failures = @()
    foreach ($case in $cases) {
        $dir = Join-Path $root ($case.Name -replace '[^A-Za-z0-9]', '-')
        New-Item -ItemType Directory -Force $dir | Out-Null
        Set-Content -LiteralPath (Join-Path $dir 'case.yml') -Value $case.Body
        $found = @(Test-TrustedPublishingTriggers $dir)
        if ($found.Count -ne $case.Expect) {
            $failures += "  '$($case.Name)': expected $($case.Expect) failure(s), got $($found.Count)$(if ($found.Count) { " — $($found -join '; ')" })"
        }
    }
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue

    if ($failures.Count) {
        Write-Host 'trusted-publishing trigger self-test FAILED:' -ForegroundColor Red
        $failures | ForEach-Object { Write-Host $_ -ForegroundColor Red }
        exit 1
    }
    Write-Host "trusted-publishing trigger self-test passed ($($cases.Count) cases)." -ForegroundColor Green
    exit 0
}

# --- normal mode ------------------------------------------------------------

if (-not $Path) {
    $Path = Join-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) '.github/workflows'
}

$found = @(Test-TrustedPublishingTriggers $Path)
if ($found.Count) {
    Write-Host 'FAILED: a registry token is minted from a workflow_run trigger:' -ForegroundColor Red
    $found | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    Write-Host '    Move the publish to a workflow triggered by the tag push (or a dispatch).' -ForegroundColor DarkGray
    exit 1
}
Write-Host 'trusted publishing: no registry mint runs under a workflow_run trigger.' -ForegroundColor Green
exit 0
