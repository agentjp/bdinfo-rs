#!/usr/bin/env pwsh
# The Homebrew channel leg of gui-publish.yml: the cask for the agentjp
# third-party tap, rendered from packaging/homebrew/bdinfo-rs-gui.rb.template.
#
#   -Mode prepare  render the template — version plus the two .dmg hashes
#                  taken from the release's verified SHA256SUMS (-Sums) —
#                  assert no placeholder survived, syntax-check it with
#                  `ruby -c`, run `brew style` advisorily (non-blocking, the
#                  homebrew.yml posture), and stage it in -Payload.
#   -Mode publish  copy the prepared cask (-CaskFile) into the tap checkout
#                  (-TapDir, checked out with a push-capable token) as
#                  Casks/bdinfo-rs-gui.rb and commit + push. Idempotent the
#                  same way homebrew.yml's formula push is: a byte-identical
#                  re-push stages nothing, skips the commit, and the push
#                  no-ops.

[CmdletBinding()]
param(
    # The bare X.Y.Z version being published.
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [ValidateSet('prepare', 'publish')] [string] $Mode,
    # prepare: path to the release's verified SHA256SUMS.
    [string] $Sums,
    # prepare: directory the rendered cask is staged in.
    [string] $Payload,
    # publish: the tap repository checkout to commit into.
    [string] $TapDir,
    # publish: the prepared cask file (the prepare payload artifact).
    [string] $CaskFile
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

. "$PSScriptRoot/_common.ps1"

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..' '..')).Path

if ($Mode -eq 'prepare') {
    foreach ($p in @($Sums, $Payload)) { if (-not $p) { Stop-Leg 'prepare needs -Sums and -Payload' } }

    $shaByName = Get-Sha256Sums -Path $Sums
    $shaArm = $shaByName['bdinfo-rs-gui-aarch64-apple-darwin.dmg']
    $shaIntel = $shaByName['bdinfo-rs-gui-x86_64-apple-darwin.dmg']
    if (-not $shaArm -or -not $shaIntel) { Stop-Leg 'SHA256SUMS carries no entry for one of the two .dmg assets' }

    $cask = Expand-Template -Path (Join-Path $repoRoot 'packaging/homebrew/bdinfo-rs-gui.rb.template') -Values @{
        '__PKGVER__'       = $Version
        '__SHA256_ARM__'   = $shaArm
        '__SHA256_INTEL__' = $shaIntel
    }

    New-Item -ItemType Directory -Force $Payload | Out-Null
    $out = Join-Path $Payload 'bdinfo-rs-gui.rb'
    [System.IO.File]::WriteAllText($out, $cask)
    Write-Host $cask

    ruby -c $out
    if ($LASTEXITCODE -ne 0) { Stop-Leg 'the rendered cask is not valid Ruby' }

    # Style pass, advisory: brew is preinstalled on the runner but its rubocop
    # bootstrap can rot independently of the cask being correct, so a style
    # failure must not block a release (`|| true`, the homebrew.yml posture).
    $env:PATH = "/home/linuxbrew/.linuxbrew/bin:$env:PATH"
    if (Get-Command brew -ErrorAction SilentlyContinue) {
        try { brew style $out } catch { }
        Write-Host '    (brew style is advisory - its result does not gate the leg)'
    }
    else {
        Write-Host '    brew not found - skipping the advisory style pass (ruby -c above is the gate)'
    }
    Write-Host "prepared + validated the cask for bdinfo-rs-gui $Version"
    exit 0
}

# ── publish ──────────────────────────────────────────────────────────────────
foreach ($p in @($TapDir, $CaskFile)) { if (-not $p) { Stop-Leg 'publish needs -TapDir and -CaskFile' } }
if (-not (Test-Path -LiteralPath $CaskFile)) { Stop-Leg "no prepared cask at $CaskFile" }

$casks = Join-Path $TapDir 'Casks'
New-Item -ItemType Directory -Force $casks | Out-Null
Copy-Item -LiteralPath $CaskFile (Join-Path $casks 'bdinfo-rs-gui.rb')

Push-Location -LiteralPath $TapDir
$code = 1
try {
    git config user.name 'bdinfo-rs CI'
    git config user.email 'bdinfo-rs@users.noreply.github.com'
    git add Casks/bdinfo-rs-gui.rb
    git diff --cached --quiet
    if ($LASTEXITCODE -eq 0) {
        Write-Host "cask bdinfo-rs-gui $Version is unchanged in the tap - nothing to commit"
        $code = 0
    }
    else {
        git commit -m "bdinfo-rs-gui $Version"
        if ($LASTEXITCODE -eq 0) { git push }
        $code = $LASTEXITCODE
    }
}
finally { Pop-Location }
if ($code -ne 0) { Stop-Leg "tap commit/push exit $code" }
Write-Host "published the cask: bdinfo-rs-gui $Version"
