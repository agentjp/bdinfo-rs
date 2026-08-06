#!/usr/bin/env pwsh
# The crates.io channel leg of gui-publish.yml: publish bdinfo-rs-gui from
# the tagged source (-SrcDir is a checkout of the gui-v* tag — the release's
# bytes were built from that same commit, and the attested release is what
# admitted the tag into this workflow).
#
#   -Mode prepare  guard tag == crate version, then `cargo package --locked`:
#                  packages the crate and verify-builds it against the
#                  registry, which is also where the sequencing rule
#                  bites — bdinfo-rs-gui depends on bdinfo-rs-core at the
#                  same version, so this fails cleanly until the CLI
#                  release's publish-crates.yml run has put that core version
#                  on crates.io. The built .crate lands in -Payload.
#                  `package`, not `publish --dry-run`: the latter stages the
#                  .crate under target/package/tmp-crate/ and only a real
#                  publish promotes it (observed with cargo 1.96), while
#                  `package` finalizes target/package/<name>-<ver>.crate.
#   -Mode publish  the same guard, then `cargo publish --locked`, tolerating
#                  "already uploaded" as success — crates.io versions are
#                  immutable, so that error IS the idempotent re-dispatch
#                  path, and a registry pre-check would only add an API call
#                  a transient failure could kill the job on (the
#                  publish-crates.yml idiom). CARGO_REGISTRY_TOKEN arrives
#                  from the OIDC mint (Trusted Publishing); the crate must
#                  already exist on crates.io for that trust rule to apply,
#                  so the FIRST publish is manual, like the CLI crates'.

[CmdletBinding()]
param(
    # The gui-v* tag being published.
    [Parameter(Mandatory)] [string] $Tag,
    # A checkout of that tag.
    [Parameter(Mandatory)] [string] $SrcDir,
    [Parameter(Mandatory)] [ValidateSet('prepare', 'publish')] [string] $Mode,
    # prepare: directory the packaged .crate is staged in.
    [string] $Payload
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

. "$PSScriptRoot/_common.ps1"

$tagVersion = Get-GuiTagVersion $Tag
if ($Mode -eq 'prepare' -and -not $Payload) { Stop-Leg 'prepare needs -Payload' }

$crate = Join-Path $SrcDir 'crates/bdinfo-rs-gui'
if (-not (Test-Path -LiteralPath (Join-Path $crate 'Cargo.toml'))) { Stop-Leg "no gui crate under $SrcDir" }

# cargo publishes whatever version the manifest carries, not the tag — fail
# fast on a mismatch (the publish-crates.yml guard, on the gui crate).
$crateVersion = Get-CrateVersion -Name 'bdinfo-rs-gui' -ManifestPath (Join-Path $crate 'Cargo.toml')
if ($crateVersion -ne $tagVersion) { Stop-Leg "tag $Tag does not match the gui crate version $crateVersion" }

Push-Location -LiteralPath $crate
$code = 1
try {
    if ($Mode -eq 'prepare') {
        cargo package --locked -p bdinfo-rs-gui
        $code = $LASTEXITCODE
    }
    else {
        $out = cargo publish --locked -p bdinfo-rs-gui 2>&1 | Out-String
        Write-Host $out
        $code = $LASTEXITCODE
        if ($code -ne 0 -and $out -match 'already (uploaded|exists)') {
            Write-Host "bdinfo-rs-gui@$crateVersion is already on crates.io - treating as success"
            $code = 0
        }
    }
}
finally { Pop-Location }
if ($code -ne 0) { Stop-Leg "cargo publish ($Mode) exit $code" }

if ($Mode -eq 'prepare') {
    New-Item -ItemType Directory -Force $Payload | Out-Null
    $built = @(Get-ChildItem (Join-Path $crate 'target/package') -Filter '*.crate')
    if ($built.Count -ne 1) { Stop-Leg "expected exactly one packaged .crate, found $($built.Count)" }
    Copy-Item $built[0].FullName $Payload
    Write-Host "prepared + verify-built $($built[0].Name)"
}
else {
    Write-Host "published bdinfo-rs-gui@$crateVersion to crates.io"
}
