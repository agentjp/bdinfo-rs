#!/usr/bin/env pwsh
# Publishes the GUI AppImages' .zsync update control files to the branch the
# AppImages embed as their update information
# (https://raw.githubusercontent.com/<repo>/<branch>/<name>.AppImage.zsync —
# .github/scripts/gui-package-linux.ps1 has the reason for that address).
#
# Every released AppImage keeps asking that address for as long as it is used,
# so what is served there has to be right. In order: each control file's
# header is checked (an absolute URL on this repository's gui-v* release —
# the exact tag when -Tag is given — and the name it will be served under),
# the branch is created when it does not exist, each file is committed through
# the contents API unless the branch already holds the same bytes, and each
# one is then read back, first from the API and then from the public raw URL,
# and compared byte for byte.
#
# Re-running with the same input changes nothing and still verifies.
#
# Env: GH_TOKEN (contents: write on the repository), GITHUB_REPOSITORY.

[CmdletBinding()]
param(
    # A directory holding the *.AppImage.zsync files to publish (searched
    # recursively).
    [Parameter(Mandatory)] [string] $Dir,
    # The branch to publish to.
    [Parameter(Mandatory)] [string] $Branch,
    # The gui-v* release the files belong to. Given, every control file must
    # name that release in its URL; omitted (a rehearsal from a dry run), any
    # gui-v* release of this repository is accepted.
    [string] $Tag
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

. "$PSScriptRoot/_common.ps1"

$repo = $env:GITHUB_REPOSITORY
if (-not $repo) { Stop-Leg 'GITHUB_REPOSITORY is not set' }
if (-not $env:GH_TOKEN) { Stop-Leg 'GH_TOKEN is not set' }
if ($Branch -notmatch '^[A-Za-z0-9][A-Za-z0-9._/-]*$') { Stop-Leg "'$Branch' is not a usable branch name" }
$tagPattern = if ($Tag) { [regex]::Escape("gui-v$(Get-GuiTagVersion $Tag)") } else { 'gui-v\d+\.\d+\.\d+' }

$files = @(Get-ChildItem -LiteralPath $Dir -Recurse -File -Filter '*.AppImage.zsync' | Sort-Object Name)
if (-not $files.Count) { Stop-Leg "no *.AppImage.zsync file under $Dir" }
$duplicates = @($files | Group-Object Name | Where-Object Count -gt 1)
if ($duplicates.Count) { Stop-Leg "more than one control file named $($duplicates[0].Name) under $Dir" }

# ── the header of each control file ──────────────────────────────────────────
foreach ($file in $files) {
    $appImage = $file.Name -replace '\.zsync$', ''
    # Text up to the first blank line; the block checksums follow.
    $text = [System.Text.Encoding]::Latin1.GetString([System.IO.File]::ReadAllBytes($file.FullName))
    $end = $text.IndexOf("`n`n")
    if ($end -lt 0) { Stop-Leg "$($file.Name) has no header" }
    $header = @{}
    foreach ($line in ($text.Substring(0, $end) -split "`n")) {
        $key, $value = $line -split ': ', 2
        $header[$key] = $value
    }
    foreach ($key in 'Filename', 'URL', 'Length', 'SHA-1') {
        if (-not $header.ContainsKey($key)) { Stop-Leg "$($file.Name) has no $key header" }
    }
    if ($header['Filename'] -ne $appImage) { Stop-Leg "$($file.Name) describes '$($header['Filename'])', not $appImage" }
    $want = "^https://github\.com/$([regex]::Escape($repo))/releases/download/$tagPattern/$([regex]::Escape($appImage))$"
    if ($header['URL'] -notmatch $want) { Stop-Leg "$($file.Name) points at '$($header['URL'])', not at the release asset $appImage" }
    Write-Host "    header ok  $($file.Name) -> $($header['URL'])"
}

# The git blob id of a file's bytes: what the contents API reports as `sha`.
function Get-BlobId([byte[]] $Bytes) {
    $prefix = [System.Text.Encoding]::ASCII.GetBytes("blob $($Bytes.Length)`0")
    $sha1 = [System.Security.Cryptography.SHA1]::Create()
    try { return [System.Convert]::ToHexString($sha1.ComputeHash([byte[]] ($prefix + $Bytes))).ToLowerInvariant() }
    finally { $sha1.Dispose() }
}

# POST/PUT a JSON body, returning the parsed answer.
function Send-Api([string] $Method, [string] $Path, [hashtable] $Body) {
    $tmp = New-TemporaryFile
    try {
        [System.IO.File]::WriteAllText($tmp.FullName, ($Body | ConvertTo-Json -Depth 8 -Compress))
        $out = gh api -X $Method $Path --input $tmp.FullName 2>&1 | Out-String
        if ($LASTEXITCODE -ne 0) { Stop-Leg "$Method $Path failed: $out" }
        return $out | ConvertFrom-Json
    }
    finally { Remove-Item -Force -LiteralPath $tmp.FullName }
}

# ── the branch ───────────────────────────────────────────────────────────────
gh api "repos/$repo/git/ref/heads/$Branch" *> $null
if ($LASTEXITCODE -ne 0) {
    # A root commit holding one README, so the branch shares no history with
    # the source branches and carries no workflow that a push to it could run.
    $readme = @(
        '# AppImage update control files'
        ''
        'The bdinfo-rs GUI AppImages embed the raw URL of the `.zsync` files on this branch as their'
        'update information. Each file describes the newest released AppImage of its architecture.'
        'CI writes them at every GUI release; do not edit, rename or delete this branch.'
        ''
    ) -join "`n"
    $tree = Send-Api POST "repos/$repo/git/trees" @{ tree = @(@{ path = 'README.md'; mode = '100644'; type = 'blob'; content = $readme }) }
    $commit = Send-Api POST "repos/$repo/git/commits" @{ message = 'Create the AppImage update control branch'; tree = $tree.sha; parents = @() }
    [void] (Send-Api POST "repos/$repo/git/refs" @{ ref = "refs/heads/$Branch"; sha = $commit.sha })
    Write-Host "    created branch $Branch"
}

# ── commit what changed ──────────────────────────────────────────────────────
foreach ($file in $files) {
    $bytes = [System.IO.File]::ReadAllBytes($file.FullName)
    $existing = gh api "repos/$repo/contents/$($file.Name)?ref=$Branch" --jq '.sha' 2>$null
    if ($LASTEXITCODE -ne 0) { $existing = $null }
    if ($existing -eq (Get-BlobId $bytes)) {
        Write-Host "    unchanged  $($file.Name)"
        continue
    }
    $body = @{
        message = "Update $($file.Name)$(if ($Tag) { " for $Tag" })"
        content = [System.Convert]::ToBase64String($bytes)
        branch  = $Branch
    }
    if ($existing) { $body['sha'] = $existing }
    [void] (Send-Api PUT "repos/$repo/contents/$($file.Name)" $body)
    Write-Host "    committed  $($file.Name)"
}

# ── read back what is served ─────────────────────────────────────────────────
foreach ($file in $files) {
    $want = Get-BlobId ([System.IO.File]::ReadAllBytes($file.FullName))
    $got = gh api "repos/$repo/contents/$($file.Name)?ref=$Branch" --jq '.sha'
    if ($LASTEXITCODE -ne 0 -or $got -ne $want) { Stop-Leg "branch $Branch does not hold the published $($file.Name)" }

    # The address an update tool asks. raw.githubusercontent.com caches a path
    # for about five minutes, so a changed file takes that long to show (304 s
    # measured on 2026-11-11); the 12 minutes of attempts leave room past it.
    $url = "https://raw.githubusercontent.com/$repo/$Branch/$($file.Name)"
    $ok = $false
    foreach ($attempt in 1..24) {
        $tmp = New-TemporaryFile
        try {
            Invoke-WebRequest -Uri $url -OutFile $tmp.FullName
            if ((Get-BlobId ([System.IO.File]::ReadAllBytes($tmp.FullName))) -eq $want) { $ok = $true }
        }
        catch { Write-Host "    attempt ${attempt}: $($_.Exception.Message)" }
        finally { Remove-Item -Force -LiteralPath $tmp.FullName }
        if ($ok) { break }
        Start-Sleep -Seconds 30
    }
    if (-not $ok) { Stop-Leg "$url does not serve the published control file" }
    Write-Host "    served ok  $url"
}
Write-Host "published $($files.Count) control file(s) to $Branch"
