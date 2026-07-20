#!/usr/bin/env pwsh
# Phase-1 gate of gui-publish.yml: prove the gui-v* release is complete and
# untampered BEFORE any channel work starts, and resolve the channel matrix.
# In order: the tag is a stable gui-vX.Y.Z, the release exists and is not a
# draft, its asset set is EXACTLY the 12 packages gui-release.yml publishes
# plus SHA256SUMS, every downloaded byte matches SHA256SUMS, and every asset
# (SHA256SUMS included) carries a verifiable Sigstore build-provenance
# attestation. Downloads land in -OutDir so the workflow can publish
# SHA256SUMS as the artifact the prepare legs re-check their own downloads
# against.
#
# Release immutability is a hard gate: a mutable release's assets can be
# swapped after this verification, which would void everything the checks
# below prove, so no channel may publish from one.
#
# Writes `version` and `channels` (a JSON array feeding both phase matrices)
# to $GITHUB_OUTPUT. Channel selection: `all` expands to every channel; a
# paused AUR (repo variable AUR_ENABLED != true) is dropped from `all` with a
# warning annotation, but an EXPLICIT aur dispatch fails instead — a silently
# skipped channel would read as published.

[CmdletBinding()]
param(
    # The gui-v* release tag to publish from.
    [Parameter(Mandatory)] [string] $Tag,
    # The dispatch's channel selection: `all` or one channel name.
    [Parameter(Mandatory)]
    [ValidateSet('all', 'winget', 'homebrew', 'aur', 'cloudsmith', 'crates')]
    [string] $Channels,
    # Where the release assets are downloaded for verification.
    [Parameter(Mandatory)] [string] $OutDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

function Stop-Verify([string] $why) {
    Write-Host "FAILED: $why" -ForegroundColor Red
    exit 1
}

$repo = $env:GITHUB_REPOSITORY
if (-not $repo) { Stop-Verify 'GITHUB_REPOSITORY is not set' }

# Stable releases only — a prerelease suffix never reaches a package manager
# (the packages.yml gate's rule).
if ($Tag -notmatch '^gui-v(\d+\.\d+\.\d+)$') {
    Stop-Verify "tag '$Tag' is not a stable gui-vX.Y.Z tag"
}
$version = $Matches[1]

$releaseJson = gh api "repos/$repo/releases/tags/$Tag" 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) { Stop-Verify "no release found for tag $Tag`: $releaseJson" }
$release = $releaseJson | ConvertFrom-Json
if ($release.draft) { Stop-Verify "the release for $Tag is a draft" }
if (-not $release.immutable) {
    Stop-Verify "release $Tag is not immutable - its assets could change after this verification, so no channel may publish from it"
}

# The exact asset contract gui-release.yml publishes: 12 packages +
# SHA256SUMS, nothing else. Names must match both ways.
$packages = @(
    "bdinfo-rs-gui-x86_64-unknown-linux-gnu.AppImage"
    "bdinfo-rs-gui-x86_64-unknown-linux-gnu.deb"
    "bdinfo-rs-gui-x86_64-unknown-linux-gnu.rpm"
    "bdinfo-rs-gui-aarch64-unknown-linux-gnu.AppImage"
    "bdinfo-rs-gui-aarch64-unknown-linux-gnu.deb"
    "bdinfo-rs-gui-aarch64-unknown-linux-gnu.rpm"
    "bdinfo-rs-gui-x86_64-pc-windows-msvc.zip"
    "bdinfo-rs-gui-x86_64-pc-windows-msvc.msi"
    "bdinfo-rs-gui-aarch64-pc-windows-msvc.zip"
    "bdinfo-rs-gui-aarch64-pc-windows-msvc.msi"
    "bdinfo-rs-gui-x86_64-apple-darwin.dmg"
    "bdinfo-rs-gui-aarch64-apple-darwin.dmg"
)
$expected = @($packages + 'SHA256SUMS')
$actual = @($release.assets.name)
$diff = @(Compare-Object -ReferenceObject $expected -DifferenceObject $actual)
if ($diff.Count) {
    $diff | ForEach-Object { Write-Host "    $($_.SideIndicator) $($_.InputObject)" }
    Stop-Verify "release $Tag does not carry exactly the expected asset set (<= missing, => unexpected)"
}

New-Item -ItemType Directory -Force $OutDir | Out-Null
gh release download $Tag --repo $repo --dir $OutDir
if ($LASTEXITCODE -ne 0) { Stop-Verify "gh release download $Tag failed" }

# Every SHA256SUMS line must name one of the 12 packages, cover all 12, and
# match the downloaded bytes.
$sums = @{}
foreach ($line in Get-Content -LiteralPath (Join-Path $OutDir 'SHA256SUMS')) {
    if ($line -notmatch '^([0-9a-f]{64})\s+\*?(.+)$') { Stop-Verify "unparseable SHA256SUMS line: $line" }
    $sums[$Matches[2].Trim()] = $Matches[1]
}
$sumDiff = @(Compare-Object -ReferenceObject $packages -DifferenceObject @($sums.Keys))
if ($sumDiff.Count) { Stop-Verify 'SHA256SUMS does not cover exactly the 12 packages' }
foreach ($name in $packages) {
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $OutDir $name)).Hash.ToLowerInvariant()
    if ($hash -ne $sums[$name]) { Stop-Verify "checksum mismatch for $name (SHA256SUMS $($sums[$name]), downloaded $hash)" }
    Write-Host "    sha256 ok  $name"
}

# Sigstore build-provenance over every asset, SHA256SUMS included (the
# release job attests the full set).
foreach ($name in $expected) {
    gh attestation verify (Join-Path $OutDir $name) --repo $repo
    if ($LASTEXITCODE -ne 0) { Stop-Verify "attestation verification failed for $name" }
}

# Channel matrix. AUR is armed by the AUR_ENABLED repo variable, like the
# packages.yml aur job.
$selected = if ($Channels -eq 'all') { @('winget', 'homebrew', 'aur', 'cloudsmith', 'crates') } else { @($Channels) }
if ($env:AUR_ENABLED -ne 'true' -and $selected -contains 'aur') {
    if ($Channels -eq 'aur') {
        Stop-Verify 'AUR publishing is paused (repo variable AUR_ENABLED is not true); set it before an explicit aur dispatch'
    }
    Write-Host '::warning::AUR publishing is paused (repo variable AUR_ENABLED is not true) - dropping the aur channel from this run.'
    $selected = @($selected | Where-Object { $_ -ne 'aur' })
}

Add-Content -LiteralPath $env:GITHUB_OUTPUT -Value "version=$version"
Add-Content -LiteralPath $env:GITHUB_OUTPUT -Value "channels=$(ConvertTo-Json $selected -Compress -AsArray)"
Write-Host "verified release $Tag (version $version): 12 packages + SHA256SUMS, checksums + attestations ok; channels: $($selected -join ', ')"
