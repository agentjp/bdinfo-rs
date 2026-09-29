#!/usr/bin/env pwsh
# The open-issue lookup every rolling-issue workflow files through: fuzz.yml
# (growth and crash issues), fuzz-compact.yml, sweep.yml, sweep-watchdog.yml
# and version-freshness.yml. Dot-source it, then call
#
#   Find-OpenIssue -Label <label> [-Marker <text>]
#
# for the number of the OLDEST open issue carrying <label> whose body contains
# <text>, or $null when there is none.
#
# An empty answer is what makes a caller file a NEW issue, so it is read twice
# before it is believed. On 2026-09-15 fuzz.yml's run 34980609631 got an empty
# list from `gh issue list` with no error while #316 was open and labelled,
# filed #476, and every later pass edited #476 and left #316 behind. The second
# read goes through the REST issues endpoint rather than repeating the same
# GraphQL query. Whatever still gets past both is repaired on the next call:
# when more than one open issue matches, every one but the oldest is closed as
# its duplicate. A gh call that exits non-zero throws rather than answering
# "none".
#
# Usage:
#   . ./.github/scripts/find-open-issue.ps1
#   pwsh find-open-issue.ps1 -SelfTest
# Exit code (self-test): 0 = every case passed, 1 = at least one failed.
#
# Nothing at the top level changes the caller's session, because a dot-source
# runs in the caller's scope: strict mode is set inside the function and the
# self-test only.

[CmdletBinding()]
param(
    [switch] $SelfTest
)

function Find-OpenIssue {
    param(
        [Parameter(Mandatory)] [string] $Label,
        [string] $Marker = ''
    )
    Set-StrictMode -Version Latest

    # `gh issue list` never returns pull requests; the REST endpoint does, and
    # a pull request carries a `pull_request` key.
    $pick = {
        param([string[]] $Json)
        $text = $Json -join "`n"
        if ([string]::IsNullOrWhiteSpace($text)) { return }
        $text | ConvertFrom-Json |
            Where-Object { $null -ne $_ -and -not $_.PSObject.Properties['pull_request'] -and ([string]$_.body).Contains($Marker) } |
            ForEach-Object { [int]$_.number } | Sort-Object
    }

    $json = gh issue list --state open --label $Label --limit 200 --json 'number,body'
    if ($LASTEXITCODE -ne 0) { throw "gh issue list --label $Label exited $LASTEXITCODE" }
    $found = @(& $pick $json)
    if ($found.Count -eq 0) {
        $json = gh api "repos/{owner}/{repo}/issues?labels=$Label&state=open&per_page=100"
        if ($LASTEXITCODE -ne 0) { throw "gh api issues?labels=$Label exited $LASTEXITCODE" }
        $found = @(& $pick $json)
    }
    if ($found.Count -eq 0) { return $null }

    $run = "$env:GITHUB_SERVER_URL/$env:GITHUB_REPOSITORY/actions/runs/$env:GITHUB_RUN_ID"
    foreach ($dup in @($found | Select-Object -Skip 1)) {
        gh issue close $dup --reason 'not planned' --comment "Duplicate of #$($found[0]), the issue this workflow refreshes: $run" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "gh issue close $dup exited $LASTEXITCODE" }
        Write-Host "Closed #$dup as a duplicate of #$($found[0])"
    }
    $found[0]
}

if ($SelfTest) {
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'

    # A stand-in for gh: a function outranks the executable of the same name.
    # Each case scripts the two reads and records every call.
    function gh {
        $script:Calls.Add($args -join ' ')
        $global:LASTEXITCODE = 0
        if ($args[0] -eq 'issue' -and $args[1] -eq 'list') {
            $global:LASTEXITCODE = $script:Case.ListRc
            return $script:Case.List
        }
        if ($args[0] -eq 'api') {
            $global:LASTEXITCODE = $script:Case.ApiRc
            return $script:Case.Api
        }
    }

    $open = { param($n, $body = 'b') @{ number = $n; body = $body } }
    $pr = @{ number = 9; body = 'b'; pull_request = @{ url = 'u' } }
    $cases = @(
        @{ Name = 'the first read finds it'; List = @((& $open 476)); Want = 476; NoApi = $true }
        @{ Name = 'an empty first read is re-read through REST'; List = @(); Api = @((& $open 476)); Want = 476 }
        @{ Name = 'two empty reads answer none'; List = @(); Api = @(); Want = $null }
        @{ Name = 'a pull request on the REST read is not an issue'; List = @(); Api = @($pr); Want = $null }
        @{ Name = 'the oldest of two is kept and the newer closed'; List = @((& $open 476), (& $open 316)); Want = 316; Close = 476 }
        @{ Name = 'a marker selects by body'; Marker = '<!-- sig: a -->'; List = @((& $open 5 'x <!-- sig: b -->'), (& $open 7 '<!-- sig: a -->')); Want = 7 }
        @{ Name = 'a marker no body carries answers none'; Marker = '<!-- sig: c -->'; List = @((& $open 5 'x')); Api = @((& $open 5 'x')); Want = $null }
        @{ Name = 'a failing first read throws'; List = @(); ListRc = 1; Throws = $true }
        @{ Name = 'a failing second read throws'; List = @(); ApiRc = 1; Throws = $true }
    )

    $failed = 0
    foreach ($c in $cases) {
        $script:Case = @{
            List   = ($c.List | ConvertTo-Json -AsArray -Depth 5)
            Api    = (@($c['Api']) | Where-Object { $_ } | ConvertTo-Json -AsArray -Depth 5)
            ListRc = [int]$c['ListRc']
            ApiRc  = [int]$c['ApiRc']
        }
        $script:Calls = [System.Collections.Generic.List[string]]::new()
        $problem = $null
        try {
            $got = Find-OpenIssue -Label l -Marker ([string]$c['Marker']) 6> $null
            if ($c['Throws']) { $problem = 'did not throw' }
            elseif ($got -ne $c.Want) { $problem = "got '$got', want '$($c.Want)'" }
        }
        catch {
            if (-not $c['Throws']) { $problem = "threw: $_" }
        }
        $closes = @($script:Calls | Where-Object { $_ -like 'issue close *' })
        $apis = @($script:Calls | Where-Object { $_ -like 'api *' })
        if (-not $problem -and $c['Close'] -and ($closes.Count -ne 1 -or $closes[0] -notlike "issue close $($c.Close) *")) {
            $problem = "closed [$($closes -join '; ')], want only #$($c.Close)"
        }
        if (-not $problem -and -not $c['Close'] -and $closes.Count -gt 0) { $problem = "closed [$($closes -join '; ')]" }
        if (-not $problem -and $c['NoApi'] -and $apis.Count -gt 0) { $problem = 're-read a non-empty answer' }
        if ($problem) { $failed++; Write-Host "FAIL  $($c.Name): $problem" }
        else { Write-Host "ok    $($c.Name)" }
    }
    if ($failed -gt 0) {
        Write-Host "find-open-issue self-test FAILED ($failed of $($cases.Count))"
        exit 1
    }
    Write-Host "find-open-issue self-test passed ($($cases.Count) cases)."
    exit 0
}
