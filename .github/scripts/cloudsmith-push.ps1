#!/usr/bin/env pwsh
#Requires -Version 7.0

<#
.SYNOPSIS
    Push already-built .deb/.rpm packages to the Cloudsmith repository, idempotently.

.DESCRIPTION
    The one push implementation for every lane that ships a Linux package: the
    CLI's packages.yml and the GUI's publish-gui-cloudsmith.ps1 both call it, so
    both get the same idempotency.

    Idempotent by PREFLIGHT rather than by unconditional `--republish`. A
    dispatch re-run or a backfill must not re-upload a version that is already
    there: a plain re-push of an existing version uploads and then fails
    mid-sync ("...already exists...This package should be deleted"), leaving a
    broken entry, while `--republish` avoids that but needlessly re-uploads and
    is discouraged by Cloudsmith. So query the public API by exact filename and
    SKIP when the version is present; push cleanly when it is absent.
    `--republish` survives solely as the safe fallback for the two cases where a
    plain push would otherwise strand a broken entry: the preflight could not
    determine the state, or the push raced another run into "already exists".

    The query matches the version BASE. Cloudsmith stores the
    packaging-revisioned version (1.2.0-1), so a bare `version:X.Y.Z` query
    matches nothing.

    Every file is attempted before the script fails, so one channel's bad
    package does not hide the state of the others.

    Env: CLOUDSMITH_API_KEY (the push credential), CLOUDSMITH_CLI_VERSION (the
    pipx pin; version-freshness.yml asserts every lane pins the same value).

.PARAMETER Version
    The bare X.Y.Z version the packages are expected to carry.

.PARAMETER Path
    The package files to push. The format is taken from each file's extension.

.PARAMETER Repo
    The Cloudsmith `owner/repo` to push into.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [string[]] $Path,
    [string] $Repo = 'bdinfo-rs/bdinfo-rs'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

. "$PSScriptRoot/_common.ps1"

if (-not $env:CLOUDSMITH_API_KEY) { Stop-Leg 'CLOUDSMITH_API_KEY is not set' }
if (-not $env:CLOUDSMITH_CLI_VERSION) { Stop-Leg 'CLOUDSMITH_CLI_VERSION is not set (the workflow pins it)' }
pipx install "cloudsmith-cli==$env:CLOUDSMITH_CLI_VERSION" | Out-Null
if ($LASTEXITCODE -ne 0) { Stop-Leg 'pipx install cloudsmith-cli failed' }

# One `any-distro/any-version` upload serves every distro in the family, so
# users can install by name from a single entry.
$distro = "$Repo/any-distro/any-version"

$failed = @()
foreach ($file in $Path) {
    if (-not (Test-Path -LiteralPath $file)) { Stop-Leg "no package at $file" }
    $name = Split-Path -Leaf $file
    $format = if ($name.EndsWith('.deb')) { 'deb' } elseif ($name.EndsWith('.rpm')) { 'rpm' } else { $null }
    if (-not $format) { Stop-Leg "$name is neither a .deb nor a .rpm" }

    # An empty $count means the state could not be determined -> --republish.
    $count = ''
    try {
        $query = [uri]::EscapeDataString("filename:$name")
        $json = Invoke-RestMethod -Uri "https://api.cloudsmith.io/v1/packages/$Repo/?query=$query&page_size=100"
        $count = @($json | Where-Object { ($_.version -replace '-\d+$', '') -eq $Version }).Count
    }
    catch { $count = '' }

    if ($count -is [int] -and $count -gt 0) {
        Write-Host "$name $Version is already in Cloudsmith ($count) - skipping"
        continue
    }
    if ($count -is [int]) {
        cloudsmith push $format $distro $file
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'plain push failed - retrying idempotently with --republish'
            cloudsmith push $format --republish $distro $file
        }
    }
    else {
        Write-Host 'could not query the Cloudsmith state - pushing with --republish (safe)'
        cloudsmith push $format --republish $distro $file
    }
    if ($LASTEXITCODE -ne 0) { $failed += $name }
}
if ($failed.Count) { Stop-Leg "Cloudsmith push failed for: $($failed -join ', ')" }
Write-Host "pushed $($Path.Count) package(s) to Cloudsmith ($Version)"
