#!/usr/bin/env pwsh
# The winget channel leg (agentjp.bdinfo-rs-gui) of gui-publish.yml: komac
# over the two release MSIs (x64 + arm64), New-Package-queue-aware and
# idempotent.
#
# State probe first, FAIL CLOSED (the packages.yml winget job's discipline: a
# blocked submit is safe and recoverable; a duplicate PR against
# microsoft/winget-pkgs that a moderator must close is not). The winget-pkgs
# contents API separates the states by HTTP status: 200 on the package
# directory means the package exists (its version subdirectories then answer
# "is this version published"), 404 means it does not exist yet (legitimate
# before the first submission), anything else after retries means "cannot
# determine" and refuses. An open submission PR also counts as pending — a
# re-dispatch while winget-pkgs' human review queue holds the previous
# submission must not spam a second PR.
#
# The FIRST submission cannot come from this leg: komac's `update` requires
# the package to already exist in winget-pkgs (it bases the new manifests on
# the latest published version), and its `new` command prompts interactively
# for manifest fields that have no CLI flag, which no headless runner can
# answer. So the bootstrap is one manual, interactive `komac new
# agentjp.bdinfo-rs-gui` by a maintainer; once that PR merges, this leg
# auto-submits every version.
#
# The submitted manifest must OMIT `Scope`: the per-user MSI registers its
# ARP entry in HKLM, winget derives installed scope from the hive alone, and
# a declared `user` scope would fail every upgrade with "installer scope does
# not match" (winget-cli #3011). komac 2.16.0 derives `Scope: user` from that
# same MSI install context and `update` has no flag to omit it, so both modes
# render locally with `--dry-run`, strip the Scope line, and hold the result
# to the assertions (no Scope / InstallerType wix / ProductCode present /
# both architectures); publishing submits the stripped tree via
# `komac submit`, never `update --submit` (which would re-render and
# re-inject the field).
#
#   -Mode prepare  validate without submitting: the rendered + stripped +
#                  asserted manifests land in -Payload. Package not in
#                  winget-pkgs yet -> record the bootstrap-pending state and
#                  pass (the other channels' rehearsal must not die on a
#                  known human queue), after asserting both MSI URLs resolve.
#   -Mode publish  the same probe, render and assertions, then
#                  `komac submit` over the stripped manifests. Version
#                  already published or a submission PR open -> skip, green
#                  (idempotent re-dispatch). Package missing -> FAIL: the
#                  manual bootstrap has not happened, and nothing this runner
#                  does can replace it.
#
# Env: GITHUB_TOKEN (komac + the probe; the read-only default token in
# prepare, WINGET_TOKEN — the bot PAT komac forks winget-pkgs under — in
# publish), KOMAC_VERSION (the pinned release to fetch; watched by
# version-freshness.yml). Linux runner assumed (the komac binary fetched is
# x86_64-unknown-linux-gnu, as in packages.yml).

[CmdletBinding()]
param(
    # The bare X.Y.Z version being published.
    [Parameter(Mandatory)] [string] $Version,
    [Parameter(Mandatory)] [ValidateSet('prepare', 'publish')] [string] $Mode,
    # Directory the prepare payload (manifests or the bootstrap-pending note)
    # is written to.
    [Parameter(Mandatory)] [string] $Payload
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

. "$PSScriptRoot/_common.ps1"

$packageId = 'agentjp.bdinfo-rs-gui'
# Dots split publisher/package into directory levels: manifests/<first
# letter>/<publisher>/<package>/<version>/ is winget-pkgs' tree shape.
$pkgTree = 'repos/microsoft/winget-pkgs/contents/manifests/a/agentjp/bdinfo-rs-gui'
$base = "https://github.com/agentjp/bdinfo-rs/releases/download/gui-v$Version"
$msiUrls = @("$base/bdinfo-rs-gui-x86_64-pc-windows-msvc.msi", "$base/bdinfo-rs-gui-aarch64-pc-windows-msvc.msi")

if (-not $env:GITHUB_TOKEN) { Stop-Leg 'GITHUB_TOKEN is not set (komac and the state probe both need it)' }
if (-not $env:KOMAC_VERSION) { Stop-Leg 'KOMAC_VERSION is not set (the workflow pins it)' }

# GET an api.github.com path, returning status + parsed body; 0 = network
# error. Retries transient failures (5xx / network) three times.
function Get-Api([string] $path) {
    $headers = @{
        Accept        = 'application/vnd.github+json'
        Authorization = "Bearer $env:GITHUB_TOKEN"
    }
    foreach ($attempt in 1..3) {
        $status = 0
        $body = $null
        try {
            $resp = Invoke-WebRequest -Uri "https://api.github.com/$path" -Headers $headers -SkipHttpErrorCheck
            $status = [int]$resp.StatusCode
            $body = $resp.Content
        }
        catch { $status = 0 }
        if ($status -eq 200 -or $status -eq 404) {
            return @{ Status = $status; Body = $body }
        }
        Write-Host "    api.github.com/$path returned $status (attempt $attempt/3) - retrying..."
        Start-Sleep -Seconds 5
    }
    return @{ Status = $status; Body = $null }
}

# ── the state probe (shared by both modes) ───────────────────────────────────
$tree = Get-Api $pkgTree
$packageExists = $false
$versionPublished = $false
switch ($tree.Status) {
    200 {
        $packageExists = $true
        $versionPublished = @(($tree.Body | ConvertFrom-Json).name) -contains $Version
    }
    404 { $packageExists = $false }
    default {
        Stop-Leg "cannot determine the winget-pkgs state for $packageId (HTTP $($tree.Status)) - refusing to continue rather than risk a duplicate submission"
    }
}

# Open submission PRs (komac titles carry "<identifier> version <version>").
$prPending = $false
$q = [uri]::EscapeDataString("repo:microsoft/winget-pkgs is:pr is:open in:title `"$packageId`"")
$search = Get-Api "search/issues?q=$q&per_page=100"
if ($search.Status -eq 200) {
    $items = @(($search.Body | ConvertFrom-Json).items)
    $prPending = [bool]@($items | Where-Object { $_.title -match [regex]::Escape($Version) }).Count
}
elseif ($Mode -eq 'publish') {
    Stop-Leg "cannot determine whether a winget-pkgs submission PR is already open (HTTP $($search.Status)) - refusing to submit"
}
else {
    Write-Host "::warning::could not query open winget-pkgs PRs (HTTP $($search.Status)) - prepare continues; publish will re-probe."
}
Write-Host "    winget-pkgs state: package exists=$packageExists, $Version published=$versionPublished, submission PR open=$prPending"

New-Item -ItemType Directory -Force $Payload | Out-Null

# ── komac, fetched at the pin (a run: step, no action involved — the same
# posture as packages.yml's winget job) ──────────────────────────────────────
function Install-Komac {
    $dir = Join-Path ([System.IO.Path]::GetTempPath()) "komac-$PID"
    New-Item -ItemType Directory -Force $dir | Out-Null
    $tarball = Join-Path $dir 'komac.tar.gz'
    $v = $env:KOMAC_VERSION
    curl -fsSL "https://github.com/russellbanks/Komac/releases/download/v$v/komac-$v-x86_64-unknown-linux-gnu.tar.gz" -o $tarball
    if ($LASTEXITCODE -ne 0) { Stop-Leg 'could not download komac' }
    tar -xzf $tarball -C $dir komac
    if ($LASTEXITCODE -ne 0) { Stop-Leg 'could not extract komac' }
    $komac = Join-Path $dir 'komac'
    # Write-Host, not bare invocation: the version banner would join the
    # function's output stream and corrupt the returned path into an array.
    Write-Host (& $komac --version)
    if ($LASTEXITCODE -ne 0) { Stop-Leg 'komac does not run' }
    return $komac
}

# Renders the manifests with `komac update --dry-run`, strips the Scope line
# komac injects (see the header), and holds the result to the assertions.
# Returns the output tree root and the directory holding the manifest files;
# everything komac and the review print is routed through Write-Host so the
# returned object stays the function's only output.
function Build-Manifests([string] $komac) {
    $outDir = Join-Path ([System.IO.Path]::GetTempPath()) "komac-manifests-$PID"
    & $komac update $packageId --version $Version --dry-run --output $outDir --urls @msiUrls | Write-Host
    if ($LASTEXITCODE -ne 0) { Stop-Leg 'komac update --dry-run failed' }

    $installer = @(Get-ChildItem -Recurse $outDir -Filter '*.installer.yaml')
    if ($installer.Count -ne 1) { Stop-Leg "expected exactly one installer manifest, found $($installer.Count)" }
    $manifest = (Get-Content -LiteralPath $installer[0].FullName -Raw) -replace '(?m)^\s*Scope:[^\r\n]*\r?\n?', ''
    Set-Content -LiteralPath $installer[0].FullName -Value $manifest -NoNewline
    Write-Host $manifest
    if ($manifest -notmatch '(?m)^\s*InstallerType:\s*wix\s*$') { Stop-Leg 'the manifest does not declare InstallerType: wix' }
    if ($manifest -match '(?m)^\s*Scope:') { Stop-Leg 'the manifest declares Scope - it must be omitted (per-user MSI ARP lands in HKLM; a declared scope breaks every upgrade, winget-cli #3011)' }
    if ($manifest -notmatch '(?m)^\s*ProductCode:') { Stop-Leg 'the manifest carries no ProductCode' }
    foreach ($arch in 'x64', 'arm64') {
        if ($manifest -notmatch "(?m)^\s*-?\s*Architecture:\s*$arch\s*$") { Stop-Leg "the manifest carries no $arch installer" }
    }
    return @{ Root = $outDir; Leaf = $installer[0].DirectoryName }
}

if ($Mode -eq 'prepare') {
    if (-not $packageExists) {
        # Nothing komac can render yet; assert the payload URLs resolve so a
        # naming regression still fails here, and record the state.
        foreach ($url in $msiUrls) {
            $ok = $false
            try { $ok = [int](Invoke-WebRequest -Uri $url -Method Head -SkipHttpErrorCheck).StatusCode -eq 200 }
            catch { $ok = $false }
            if (-not $ok) { Stop-Leg "MSI URL does not resolve: $url" }
            Write-Host "    url ok  $url"
        }
        @(
            "# winget: bootstrap pending"
            ""
            "$packageId is not in microsoft/winget-pkgs yet. The first submission is a"
            "one-time manual, interactive run by a maintainer (komac ``update`` needs an"
            "existing package and ``komac new`` cannot run headlessly):"
            ""
            "    komac new $packageId --version $Version --urls ``"
            "        $($msiUrls[0]) ``"
            "        $($msiUrls[1])"
            ""
            "The New-Package PR waits on winget-pkgs' human review queue. Until it"
            "merges, this channel's publish leg fails by design. Both MSI URLs above"
            "verified reachable by this prepare run."
        ) | Set-Content -LiteralPath (Join-Path $Payload 'winget-bootstrap-pending.md')
        Write-Host "::notice::$packageId is not in winget-pkgs yet - the first submission is the manual komac-new bootstrap (see the payload note). Prepare passes; publish will fail until the bootstrap merges."
        exit 0
    }

    $komac = Install-Komac
    $manifests = Build-Manifests $komac
    Copy-Item -Recurse (Join-Path $manifests.Root '*') $Payload
    Write-Host "prepared + validated the winget manifests for $packageId $Version"
    exit 0
}

# ── publish ──────────────────────────────────────────────────────────────────
if ($versionPublished) {
    Write-Host "$packageId $Version is already in winget-pkgs - nothing to submit"
    exit 0
}
if ($prPending) {
    Write-Host "::notice::a winget-pkgs submission PR for $packageId $Version is already open - waiting on its queue, nothing to submit."
    exit 0
}
if (-not $packageExists) {
    Stop-Leg "$packageId is not in winget-pkgs and the first submission cannot be automated - run the one-time interactive bootstrap (komac new $packageId --version $Version --urls <the two MSI URLs>), wait for the New-Package PR to merge, then re-dispatch this channel"
}

$komac = Install-Komac
$manifests = Build-Manifests $komac
& $komac submit $manifests.Leaf --yes | Write-Host
if ($LASTEXITCODE -ne 0) { Stop-Leg 'komac submit failed' }
Write-Host "submitted $packageId $Version to winget-pkgs"
