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
#   -Mode publish  hand the four staged files to cloudsmith-push.ps1, the same
#                  idempotent preflight-and-push the CLI's packages.yml uses.
#
# Env: GH_TOKEN (prepare: gh release download); publish additionally needs what
# cloudsmith-push.ps1 documents.

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

. "$PSScriptRoot/_common.ps1"

$files = @(
    'bdinfo-rs-gui-x86_64-unknown-linux-gnu.deb'
    'bdinfo-rs-gui-aarch64-unknown-linux-gnu.deb'
    'bdinfo-rs-gui-x86_64-unknown-linux-gnu.rpm'
    'bdinfo-rs-gui-aarch64-unknown-linux-gnu.rpm'
)

if ($Mode -eq 'prepare') {
    if (-not $Sums) { Stop-Leg 'prepare needs -Sums' }
    $shaByName = Get-Sha256Sums -Path $Sums

    New-Item -ItemType Directory -Force $Payload | Out-Null
    foreach ($name in $files) {
        gh release download $Tag --repo $env:GITHUB_REPOSITORY --pattern $name --dir $Payload
        if ($LASTEXITCODE -ne 0) { Stop-Leg "gh release download $name failed" }
        $path = Join-Path $Payload $name
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
        if ($hash -ne $shaByName[$name]) { Stop-Leg "checksum mismatch for $name (SHA256SUMS $($shaByName[$name]), downloaded $hash)" }
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
& "$PSScriptRoot/cloudsmith-push.ps1" -Version $Version -Path @($files | ForEach-Object { Join-Path $Payload $_ })
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "published the 4 packages to Cloudsmith ($Version)"
