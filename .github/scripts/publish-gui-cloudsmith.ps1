#!/usr/bin/env pwsh
# The Cloudsmith channel leg of gui-publish.yml: the four GUI Linux packages
# (.deb + .rpm, x86_64 + aarch64) into the same bdinfo-rs/bdinfo-rs repo the
# CLI packages live in, so `apt install bdinfo-rs-gui` / `dnf install
# bdinfo-rs-gui` work beside the CLI's.
#
#   -Mode prepare  download the four packages from the release, check each
#                  against the verified SHA256SUMS (-Sums), assert the
#                  package metadata (name, version, architecture) via
#                  dpkg-deb / rpm, and stage the bytes in -Payload — the
#                  exact files the publish leg pushes.
#   -Mode publish  for each file: query the public Cloudsmith API by filename
#                  and SKIP if the version is already there (Cloudsmith
#                  stores the packaging-revisioned version, e.g. 2.0.0-1, so
#                  the match strips the trailing revision); push cleanly when
#                  absent; fall back to --republish only when the state could
#                  not be determined or a plain push raced another run — the
#                  packages.yml deb/rpm idiom, unchanged.
#
# Env: GH_TOKEN (prepare: gh release download), CLOUDSMITH_API_KEY (publish),
# CLOUDSMITH_CLI_VERSION (the pipx pin; watched by version-freshness.yml).

[CmdletBinding()]
param(
    # The bare X.Y.Z version being published.
    [Parameter(Mandatory)] [string] $Version,
    # The gui-v* release tag the packages are downloaded from.
    [Parameter(Mandatory)] [string] $Tag,
    [Parameter(Mandatory)] [ValidateSet('prepare', 'publish')] [string] $Mode,
    # Directory holding the four packages (written by prepare, read by publish).
    [Parameter(Mandatory)] [string] $Payload,
    # prepare: path to the release's verified SHA256SUMS.
    [string] $Sums
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$cloudsmithRepo = 'bdinfo-rs/bdinfo-rs'
$files = @(
    'bdinfo-rs-gui-x86_64-unknown-linux-gnu.deb'
    'bdinfo-rs-gui-aarch64-unknown-linux-gnu.deb'
    'bdinfo-rs-gui-x86_64-unknown-linux-gnu.rpm'
    'bdinfo-rs-gui-aarch64-unknown-linux-gnu.rpm'
)

function Stop-Leg([string] $why) {
    Write-Host "FAILED: $why" -ForegroundColor Red
    exit 1
}

if ($Mode -eq 'prepare') {
    if (-not $Sums) { Stop-Leg 'prepare needs -Sums' }
    $sums = @{}
    foreach ($line in Get-Content -LiteralPath $Sums) {
        if ($line -match '^([0-9a-f]{64})\s+\*?(.+)$') { $sums[$Matches[2].Trim()] = $Matches[1] }
    }

    New-Item -ItemType Directory -Force $Payload | Out-Null
    foreach ($name in $files) {
        gh release download $Tag --repo $env:GITHUB_REPOSITORY --pattern $name --dir $Payload
        if ($LASTEXITCODE -ne 0) { Stop-Leg "gh release download $name failed" }
        $path = Join-Path $Payload $name
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
        if ($hash -ne $sums[$name]) { Stop-Leg "checksum mismatch for $name (SHA256SUMS $($sums[$name]), downloaded $hash)" }
        Write-Host "    sha256 ok  $name"
    }

    # rpm the tool is not on the Ubuntu runner image; dpkg-deb is.
    if (-not (Get-Command rpm -ErrorAction SilentlyContinue)) {
        sudo apt-get update -qq
        if ($LASTEXITCODE -ne 0) { Stop-Leg 'apt-get update failed' }
        sudo apt-get install -y -qq rpm
        if ($LASTEXITCODE -ne 0) { Stop-Leg 'apt-get install rpm failed' }
    }

    foreach ($name in $files) {
        $path = Join-Path $Payload $name
        if ($name.EndsWith('.deb')) {
            $pkg = (dpkg-deb --field $path Package).Trim()
            $ver = (dpkg-deb --field $path Version).Trim()
            $arch = (dpkg-deb --field $path Architecture).Trim()
            $wantArch = if ($name.Contains('aarch64')) { 'arm64' } else { 'amd64' }
        }
        else {
            $pkg = (rpm -qp --qf '%{NAME}' $path 2>$null).Trim()
            $ver = (rpm -qp --qf '%{VERSION}' $path 2>$null).Trim()
            $arch = (rpm -qp --qf '%{ARCH}' $path 2>$null).Trim()
            $wantArch = if ($name.Contains('aarch64')) { 'aarch64' } else { 'x86_64' }
        }
        if ($LASTEXITCODE -ne 0) { Stop-Leg "could not read the package metadata of $name" }
        # The deb Version carries the packaging revision (2.0.0-1); the rpm
        # VERSION tag does not.
        if ($pkg -ne 'bdinfo-rs-gui') { Stop-Leg "$name names package '$pkg', not bdinfo-rs-gui" }
        if (($ver -replace '-\d+$', '') -ne $Version) { Stop-Leg "$name carries version '$ver', not $Version" }
        if ($arch -ne $wantArch) { Stop-Leg "$name carries architecture '$arch', not $wantArch" }
        Write-Host "    metadata ok  $name ($pkg $ver $arch)"
    }
    Write-Host "prepared + validated the 4 Cloudsmith packages for $Version"
    exit 0
}

# ── publish ──────────────────────────────────────────────────────────────────
if (-not $env:CLOUDSMITH_API_KEY) { Stop-Leg 'CLOUDSMITH_API_KEY is not set' }
if (-not $env:CLOUDSMITH_CLI_VERSION) { Stop-Leg 'CLOUDSMITH_CLI_VERSION is not set (the workflow pins it)' }
pipx install "cloudsmith-cli==$env:CLOUDSMITH_CLI_VERSION" | Out-Null
if ($LASTEXITCODE -ne 0) { Stop-Leg 'pipx install cloudsmith-cli failed' }

$failed = @()
foreach ($name in $files) {
    $path = Join-Path $Payload $name
    if (-not (Test-Path -LiteralPath $path)) { Stop-Leg "no prepared package at $path" }
    $format = if ($name.EndsWith('.deb')) { 'deb' } else { 'rpm' }

    # Preflight by exact filename via the anonymous public API, matching the
    # version BASE (Cloudsmith stores the packaging-revisioned version, so a
    # bare version:X.Y.Z query matches nothing). An empty count means the
    # state could not be determined -> --republish, the safe fallback.
    $count = ''
    try {
        $q = [uri]::EscapeDataString("filename:$name")
        $json = Invoke-RestMethod -Uri "https://api.cloudsmith.io/v1/packages/$cloudsmithRepo/?query=$q&page_size=100"
        $count = @($json | Where-Object { ($_.version -replace '-\d+$', '') -eq $Version }).Count
    }
    catch { $count = '' }

    if ($count -is [int] -and $count -gt 0) {
        Write-Host "$name $Version is already in Cloudsmith ($count) - skipping"
        continue
    }
    if ($count -is [int]) {
        cloudsmith push $format "$cloudsmithRepo/any-distro/any-version" $path
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'plain push failed - retrying idempotently with --republish'
            cloudsmith push $format --republish "$cloudsmithRepo/any-distro/any-version" $path
        }
    }
    else {
        Write-Host 'could not query the Cloudsmith state - pushing with --republish (safe)'
        cloudsmith push $format --republish "$cloudsmithRepo/any-distro/any-version" $path
    }
    if ($LASTEXITCODE -ne 0) { $failed += $name }
}
if ($failed.Count) { Stop-Leg "Cloudsmith push failed for: $($failed -join ', ')" }
Write-Host "published the 4 packages to Cloudsmith ($Version)"
