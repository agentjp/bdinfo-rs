#!/usr/bin/env pwsh
# Publish bdinfo-rs-gui to crates.io from a checkout of the gui-v* tag it is
# given — the crates.io leg of gui-publish-crates.yml.
#
# The tag is guarded against the crate's own manifest version first: cargo
# publishes what the manifest carries, not what the tag says.
#
# `cargo publish` runs from the crate's own directory because bdinfo-rs-gui is
# an independent workspace (the root Cargo.toml lists it under `exclude`), so
# `-p bdinfo-rs-gui` from the repository root does not reach it.
#
# An "already uploaded" failure counts as success: crates.io versions are
# immutable, so that error is exactly what re-running an already-published
# version produces, and tolerating it is what lets a failed release be healed
# by re-dispatching this workflow. There is deliberately no registry
# pre-check — it would add a crates.io API call that a transient 503 could
# fail the job on, which is the reasoning publish-crates.yml records for the
# same decision.
#
# bdinfo-rs-gui depends on bdinfo-rs-core at the same version, so this publish
# cannot resolve until the matching vX.Y.Z tag's publish-crates.yml run has put
# that core version on crates.io. Push vX.Y.Z and let it finish before pushing
# gui-vX.Y.Z.
#
# CARGO_REGISTRY_TOKEN comes from the caller's OIDC mint (crates.io Trusted
# Publishing). That trust rule only applies to a crate that already exists on
# crates.io, so a new crate's first publish is done by hand.

[CmdletBinding()]
param(
    # The gui-v* tag being published.
    [Parameter(Mandatory)] [string] $Tag,
    # A checkout of that tag; defaults to the working directory.
    [string] $SrcDir = '.'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

. "$PSScriptRoot/_common.ps1"

$tagVersion = Get-GuiTagVersion $Tag

$crate = Join-Path $SrcDir 'crates/bdinfo-rs-gui'
if (-not (Test-Path -LiteralPath (Join-Path $crate 'Cargo.toml'))) { Stop-Leg "no gui crate under $SrcDir" }

$crateVersion = Get-CrateVersion -Name 'bdinfo-rs-gui' -ManifestPath (Join-Path $crate 'Cargo.toml')
if ($crateVersion -ne $tagVersion) { Stop-Leg "tag $Tag does not match the gui crate version $crateVersion" }

Push-Location -LiteralPath $crate
$code = 1
try {
    $out = cargo publish --locked -p bdinfo-rs-gui 2>&1 | Out-String
    Write-Host $out
    $code = $LASTEXITCODE
    if ($code -ne 0 -and $out -match 'already (uploaded|exists)') {
        Write-Host "bdinfo-rs-gui@$crateVersion is already on crates.io - treating as success"
        $code = 0
    }
}
finally { Pop-Location }
if ($code -ne 0) { Stop-Leg "cargo publish exit $code" }

Write-Host "published bdinfo-rs-gui@$crateVersion to crates.io"
