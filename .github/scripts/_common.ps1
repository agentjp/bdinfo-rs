#!/usr/bin/env pwsh
#Requires -Version 7.0

<#
.SYNOPSIS
    Helpers shared by the publish lanes.

.DESCRIPTION
    Dot-source this file; every function below then runs in the caller's scope:

        . "$PSScriptRoot/_common.ps1"                        # from a script
        . ./.github/scripts/_common.ps1                      # from a workflow step

    Callers keep their own three-line preamble

        Set-StrictMode -Version Latest
        $ErrorActionPreference = 'Stop'
        $PSNativeCommandUseErrorActionPreference = $false

    ahead of the dot-source: it has to be in force before this file is read, and
    the third line is what makes the `if ($LASTEXITCODE -ne 0)` checks these
    scripts are built on reachable at all — with it left at $true a non-zero exit
    from a native command throws instead.

    Consumers: the publish-gui-*.ps1 channel legs, cloudsmith-push.ps1, and the
    inline pwsh steps of publish-crates.yml and publish-npm.yml.
#>

# Fail a publishing leg. Never a `throw`: these run as the whole body of a
# workflow step, where the exit code is the verdict and a bare error record
# buries the reason in a stack trace.
function Stop-Leg {
    param([Parameter(Mandatory)] [string] $Why)

    Write-Host "FAILED: $Why" -ForegroundColor Red
    exit 1
}

# Parse a SHA256SUMS file into a filename -> lower-case hex hash map, accepting
# both the plain and the binary-marker (`*name`) coreutils spellings.
#
# -Strict rejects a line that matches neither, for the caller that is proving
# the file itself well-formed rather than looking a known name up in it.
#
# The parameter is -Path, not -Sums, and the local is $shaByName: PowerShell
# variable names are case-insensitive, so a `$sums` local inside a function
# taking `$Sums` overwrites the path before the loop reads it.
function Get-Sha256Sums {
    param(
        [Parameter(Mandatory)] [string] $Path,
        [switch] $Strict
    )

    $shaByName = @{}
    foreach ($line in Get-Content -LiteralPath $Path) {
        if ($line -match '^([0-9a-f]{64})\s+\*?(.+)$') { $shaByName[$Matches[2].Trim()] = $Matches[1] }
        elseif ($Strict) { Stop-Leg "unparseable SHA256SUMS line: $line" }
    }
    return $shaByName
}

# The X.Y.Z of a stable `gui-v*` tag. Stable only — a prerelease suffix never
# reaches a package manager (the packages.yml gate's rule).
function Get-GuiTagVersion {
    param([Parameter(Mandatory)] [string] $Tag)

    if ($Tag -notmatch '^gui-v(\d+\.\d+\.\d+)$') { Stop-Leg "tag '$Tag' is not a stable gui-vX.Y.Z tag" }
    return $Matches[1]
}

# The X.Y.Z base of a `v*` release tag, with any prerelease suffix stripped.
# The CLI lanes publish prereleases (npm under the `next` dist-tag, crates.io as
# a prerelease version), so unlike the GUI tag above the suffix is accepted, not
# rejected; what the callers compare against a manifest is the base.
function Get-ReleaseTagBase {
    param([Parameter(Mandatory)] [string] $Tag)

    if ($Tag -notmatch '^v(?<base>\d+\.\d+\.\d+)(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?$') {
        Stop-Leg "tag '$Tag' is not vMAJOR.MINOR.PATCH[-prerelease]"
    }
    return $Matches['base']
}

# The version cargo would publish for a package, read from cargo itself rather
# than from the manifest text: a crate inheriting `version.workspace = true`
# carries no version line of its own, and a hand-rolled line parser has to know
# which table it is in to avoid reading a dependency's version instead.
#
# -ManifestPath selects a workspace other than the one the working directory
# resolves to; the sibling crates are excluded workspaces and need it.
function Get-CrateVersion {
    param(
        [Parameter(Mandatory)] [string] $Name,
        [string] $ManifestPath
    )

    $cargoArgs = @('metadata', '--no-deps', '--format-version', '1')
    if ($ManifestPath) { $cargoArgs += @('--manifest-path', $ManifestPath) }
    $json = & cargo @cargoArgs | Out-String
    if ($LASTEXITCODE -ne 0) { Stop-Leg "cargo metadata failed while reading the version of $Name" }

    $found = @(($json | ConvertFrom-Json).packages | Where-Object { $_.name -eq $Name })
    if ($found.Count -ne 1) { Stop-Leg "cargo metadata names $($found.Count) packages called $Name, expected exactly one" }
    return $found[0].version
}

# Render a packaging template by literal placeholder substitution and prove the
# render complete. The assertion is the point: a template gaining a placeholder
# that its caller does not fill would otherwise ship the literal `__NAME__` to a
# package manager.
function Expand-Template {
    param(
        [Parameter(Mandatory)] [string] $Path,
        [Parameter(Mandatory)] [System.Collections.IDictionary] $Values
    )

    $text = Get-Content -LiteralPath $Path -Raw
    foreach ($placeholder in $Values.Keys) { $text = $text.Replace($placeholder, $Values[$placeholder]) }
    if ($text -match '__[A-Z0-9_]+__') { Stop-Leg "an unreplaced placeholder survived the render: $($Matches[0])" }
    return $text
}
