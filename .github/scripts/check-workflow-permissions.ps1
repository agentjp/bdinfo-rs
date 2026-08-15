#!/usr/bin/env pwsh
# Permission-compatibility gate for local reusable-workflow edges (`uses:
# ./.github/workflows/*.yml`).
#
# A called workflow's jobs run under the CALLING job's token, and GitHub
# rejects a run outright when a called job requests a permission its caller
# does not hold. The rejection happens before any job exists: the run's
# conclusion is `startup_failure` with zero jobs and zero check runs, so
# nothing inside the run can observe or report it, and a fan-in or
# issue-filing job in the calling workflow never runs either.
#
# No per-file linter catches this. action-validator schema-checks one file at
# a time, ryl lints its style, and zizmor audits over-permissioning within a
# file — none of the three follows a `uses: ./…` edge and compares the two
# ends. This script does exactly that and nothing else.
#
# Usage:
#   pwsh check-workflow-permissions.ps1 [-Path <workflow dir>]
#   pwsh check-workflow-permissions.ps1 -SelfTest
# Exit code: 0 = every edge is satisfiable, 1 = at least one is not (or a
# self-test case failed).

[CmdletBinding()]
param(
    [string] $Path,
    [switch] $SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# GitHub's three permission levels, ordered. A caller satisfies a callee's
# request when it grants at least the requested level for that scope; a scope
# the caller omits is `none`, which satisfies nothing above it.
$script:Levels = @{ none = 0; read = 1; write = 2 }

# The two shorthand forms that stand in for a full block. `write-all` covers
# every scope, so it satisfies any request; `read-all` covers every scope at
# read. Both are recorded under a wildcard key the comparison understands.
$script:Wildcard = '*'

# --- parsing ----------------------------------------------------------------
# A hand-rolled, indentation-driven reader rather than a YAML library: no YAML
# parser ships with PowerShell, and the shapes this needs — the top-level
# `permissions:` mapping, each job's `permissions:` mapping, and each job's
# `uses:` scalar — are all fixed-indent keys under `jobs:`. Block scalars are
# tracked so that a `run: |` body can never be mistaken for a key.

# Parses "scope: level" pairs out of a mapping block, or the inline shorthands.
# Returns $null when the key is absent, so "declared nothing" stays
# distinguishable from "declared an empty block" (`permissions: {}`, which
# grants nothing and is a real, stricter statement).
function Read-PermissionBlock {
    param(
        [string[]] $Lines,
        [int] $StartIndex,   # index of the `permissions:` line itself
        [int] $KeyIndent     # indent of that line; entries sit two deeper
    )

    $line = $Lines[$StartIndex]
    $inline = ($line -replace '^\s*permissions:\s*', '') -replace '\s*#.*$', ''
    $inline = $inline.Trim()
    if ($inline -eq 'write-all') { return @{ $script:Wildcard = 'write' } }
    if ($inline -eq 'read-all') { return @{ $script:Wildcard = 'read' } }
    if ($inline -eq '{}') { return @{} }

    $block = @{}
    for ($i = $StartIndex + 1; $i -lt $Lines.Count; $i++) {
        $l = $Lines[$i]
        if ($l -match '^\s*$' -or $l -match '^\s*#') { continue }
        $indent = ($l -replace '\S.*$', '').Length
        if ($indent -le $KeyIndent) { break }
        # Quotes are optional on both halves: cargo-dist generates
        # `"contents": "read"` where a hand-written workflow says
        # `contents: read`, and both are the same mapping.
        if ($l -match '^\s*"?([a-z][a-z-]*)"?:\s*"?([a-z-]+)"?') {
            $block[$Matches[1]] = $Matches[2]
        }
    }
    return $block
}

# Reads one workflow file into the shape the edge check needs: its
# workflow-level permissions, and one entry per job carrying that job's own
# permissions and the local workflow it calls, if any.
function Read-Workflow([string] $File) {
    $lines = [System.IO.File]::ReadAllLines($File)
    $result = @{
        File        = $File
        Permissions = $null
        Jobs        = [System.Collections.Generic.List[object]]::new()
    }

    $inJobs = $false
    $job = $null
    # A block scalar (`run: |`) swallows every following line indented deeper
    # than its key, none of which is a key of its own.
    $blockIndent = -1

    for ($i = 0; $i -lt $lines.Count; $i++) {
        $l = $lines[$i]
        if ($l -match '^\s*$') { continue }
        $indent = ($l -replace '\S.*$', '').Length
        if ($blockIndent -ge 0) {
            if ($indent -gt $blockIndent) { continue }
            $blockIndent = -1
        }
        if ($l -match '^\s*#') { continue }
        if ($l -match ':\s*[|>][+-]?\s*(#.*)?$') { $blockIndent = $indent; continue }

        if ($indent -eq 0) {
            $inJobs = $l -match '^jobs:'
            $job = $null
            if ($l -match '^permissions:') {
                $result.Permissions = Read-PermissionBlock $lines $i 0
            }
            continue
        }

        if (-not $inJobs) { continue }

        if ($indent -eq 2 -and $l -match '^\s{2}([A-Za-z0-9_-]+):\s*(#.*)?$') {
            $job = @{ Name = $Matches[1]; Line = $i + 1; Permissions = $null; Uses = $null; UsesLine = 0 }
            $result.Jobs.Add($job)
            continue
        }

        if ($indent -eq 4 -and $null -ne $job) {
            if ($l -match '^\s{4}permissions:') {
                $job.Permissions = Read-PermissionBlock $lines $i 4
            }
            elseif ($l -match '^\s{4}uses:\s*(\S+)') {
                $target = $Matches[1]
                if ($target -like './.github/workflows/*') {
                    $job.Uses = $target
                    $job.UsesLine = $i + 1
                }
            }
        }
    }

    return $result
}

# --- the check --------------------------------------------------------------

# Everything a workflow can request of whoever calls it: every permission
# block it declares, plus the requirements of the workflows its own jobs call.
# Memoized, and cycle-safe — a workflow already on the resolution stack
# contributes nothing further rather than recursing forever.
function Get-Requirement {
    param([string] $Name, [hashtable] $All, [hashtable] $Memo, [System.Collections.Generic.HashSet[string]] $Stack)

    if ($Memo.ContainsKey($Name)) { return $Memo[$Name] }
    if (-not $All.ContainsKey($Name)) { return @{} }
    if (-not $Stack.Add($Name)) { return @{} }

    $wf = $All[$Name]
    $need = @{}
    $merge = {
        param($block)
        if ($null -eq $block) { return }
        foreach ($scope in $block.Keys) {
            $level = $block[$scope]
            if (-not $script:Levels.ContainsKey($level)) { continue }
            if (-not $need.ContainsKey($scope) -or $script:Levels[$level] -gt $script:Levels[$need[$scope]]) {
                $need[$scope] = $level
            }
        }
    }

    & $merge $wf.Permissions
    foreach ($j in $wf.Jobs) {
        & $merge $j.Permissions
        if ($j.Uses) {
            & $merge (Get-Requirement -Name (Split-Path $j.Uses -Leaf) -All $All -Memo $Memo -Stack $Stack)
        }
    }

    [void] $Stack.Remove($Name)
    $Memo[$Name] = $need
    return $need
}

# The level a grant actually confers for one scope, honouring the `read-all` /
# `write-all` wildcard.
function Get-GrantedLevel([hashtable] $Grant, [string] $Scope) {
    if ($Grant.ContainsKey($Scope)) { return $Grant[$Scope] }
    if ($Grant.ContainsKey($script:Wildcard)) { return $Grant[$script:Wildcard] }
    return 'none'
}

# Walks every local `uses:` edge and returns one string per unsatisfiable one.
function Test-WorkflowPermissions([string] $Dir) {
    $all = @{}
    foreach ($f in (Get-ChildItem -LiteralPath $Dir -Filter *.yml -File | Sort-Object Name)) {
        $all[$f.Name] = Read-Workflow $f.FullName
    }

    $memo = @{}
    $failures = [System.Collections.Generic.List[string]]::new()

    foreach ($name in ($all.Keys | Sort-Object)) {
        $wf = $all[$name]
        foreach ($job in $wf.Jobs) {
            if (-not $job.Uses) { continue }
            $callee = Split-Path $job.Uses -Leaf
            if (-not $all.ContainsKey($callee)) {
                $failures.Add("${name}:$($job.UsesLine): job '$($job.Name)' calls '$($job.Uses)', which does not exist")
                continue
            }

            $need = Get-Requirement -Name $callee -All $all -Memo $memo -Stack ([System.Collections.Generic.HashSet[string]]::new())
            if ($need.Count -eq 0) { continue }

            # A calling job's own block replaces the workflow-level one; with
            # neither, the token is whatever the repository default grants,
            # which this file cannot see and must not assume.
            $grant = if ($null -ne $job.Permissions) { $job.Permissions } else { $wf.Permissions }
            if ($null -eq $grant) {
                $failures.Add("${name}:$($job.Line): job '$($job.Name)' calls $callee, which requests [$(($need.Keys | Sort-Object | ForEach-Object { "${_}: $($need[$_])" }) -join ', ')], but neither the job nor the workflow declares permissions")
                continue
            }

            foreach ($scope in ($need.Keys | Sort-Object)) {
                $wanted = $need[$scope]
                $held = Get-GrantedLevel $grant $scope
                if ($script:Levels[$held] -lt $script:Levels[$wanted]) {
                    $failures.Add("${name}:$($job.Line): job '$($job.Name)' grants '${scope}: $held' but $callee requests '${scope}: $wanted' — this run fails at startup with no jobs and no check runs")
                }
            }
        }
    }

    return $failures
}

# --- self-test --------------------------------------------------------------

if ($SelfTest) {
    $root = Join-Path ([System.IO.Path]::GetTempPath()) "wfperm-$([guid]::NewGuid().ToString('n'))"
    $cases = @(
        @{
            Name    = 'a caller narrower than its callee'
            Expect  = 1
            # The shape that took the daily sweep down: the calling job leaves
            # the workflow-level `contents: read` in place while the called
            # workflow's job asks for two more scopes.
            Files   = @{
                'caller.yml' = "name: caller`non:`n  schedule:`n    - cron: `"0 0 * * *`"`npermissions:`n  contents: read`njobs:`n  call:`n    uses: ./.github/workflows/callee.yml`n"
                'callee.yml' = "name: callee`non:`n  workflow_call:`npermissions:`n  contents: read`njobs:`n  plan:`n    runs-on: ubuntu-latest`n    permissions:`n      actions: read`n      contents: read`n    steps:`n      - run: echo hi`n"
            }
        },
        @{
            Name    = 'a caller that grants exactly what its callee asks'
            Expect  = 0
            Files   = @{
                'caller.yml' = "name: caller`non:`n  schedule:`n    - cron: `"0 0 * * *`"`npermissions:`n  contents: read`njobs:`n  call:`n    permissions:`n      actions: read`n      contents: read`n    uses: ./.github/workflows/callee.yml`n"
                'callee.yml' = "name: callee`non:`n  workflow_call:`npermissions:`n  contents: read`njobs:`n  plan:`n    runs-on: ubuntu-latest`n    permissions:`n      actions: read`n      contents: read`n    steps:`n      - run: echo hi`n"
            }
        },
        @{
            Name    = 'read where write is asked for'
            Expect  = 1
            Files   = @{
                'caller.yml' = "name: caller`non:`n  push:`npermissions:`n  contents: read`njobs:`n  call:`n    permissions:`n      issues: read`n    uses: ./.github/workflows/callee.yml`n"
                'callee.yml' = "name: callee`non:`n  workflow_call:`npermissions:`n  issues: write`njobs:`n  file:`n    runs-on: ubuntu-latest`n    steps:`n      - run: echo hi`n"
            }
        },
        @{
            Name    = 'write-all satisfies every scope'
            Expect  = 0
            Files   = @{
                'caller.yml' = "name: caller`non:`n  push:`npermissions:`n  contents: read`njobs:`n  call:`n    permissions: write-all`n    uses: ./.github/workflows/callee.yml`n"
                'callee.yml' = "name: callee`non:`n  workflow_call:`npermissions:`n  issues: write`njobs:`n  file:`n    runs-on: ubuntu-latest`n    steps:`n      - run: echo hi`n"
            }
        },
        @{
            Name    = 'a requirement inherited through a second hop'
            Expect  = 1
            # The middle workflow declares nothing beyond `contents: read`, so
            # only following the edge past it exposes the deep request.
            Files   = @{
                'caller.yml' = "name: caller`non:`n  push:`npermissions:`n  contents: read`njobs:`n  call:`n    uses: ./.github/workflows/middle.yml`n"
                'middle.yml' = "name: middle`non:`n  workflow_call:`npermissions:`n  contents: read`njobs:`n  call:`n    permissions:`n      actions: read`n      contents: read`n    uses: ./.github/workflows/deep.yml`n"
                'deep.yml'   = "name: deep`non:`n  workflow_call:`npermissions:`n  contents: read`njobs:`n  work:`n    runs-on: ubuntu-latest`n    permissions:`n      actions: read`n      contents: read`n    steps:`n      - run: echo hi`n"
            }
        },
        @{
            Name    = 'quoted scope and level'
            Expect  = 0
            # cargo-dist emits every mapping key and scalar quoted, so the
            # generated release workflow's grants only count if quotes parse.
            Files   = @{
                'caller.yml' = "name: caller`non:`n  push:`npermissions:`n  contents: read`njobs:`n  call:`n    uses: ./.github/workflows/callee.yml`n    permissions:`n      `"contents`": `"read`"`n"
                'callee.yml' = "name: callee`non:`n  workflow_call:`npermissions:`n  contents: read`njobs:`n  work:`n    runs-on: ubuntu-latest`n    steps:`n      - run: echo hi`n"
            }
        },
        @{
            Name    = 'permissions text inside a run block is not a declaration'
            Expect  = 0
            # `permissions:` written into a script body must not be read as a
            # key, or a job would appear to grant what it merely prints.
            Files   = @{
                'caller.yml' = "name: caller`non:`n  push:`npermissions:`n  contents: read`njobs:`n  call:`n    uses: ./.github/workflows/callee.yml`n"
                'callee.yml' = "name: callee`non:`n  workflow_call:`npermissions:`n  contents: read`njobs:`n  work:`n    runs-on: ubuntu-latest`n    steps:`n      - run: |`n          echo 'permissions:'`n          echo '  actions: write'`n"
            }
        }
    )

    $failures = @()
    foreach ($case in $cases) {
        $dir = Join-Path $root ($case.Name -replace '\W', '-')
        [void] (New-Item -ItemType Directory -Path $dir -Force)
        foreach ($f in $case.Files.Keys) {
            [System.IO.File]::WriteAllText((Join-Path $dir $f), $case.Files[$f])
        }
        $found = @(Test-WorkflowPermissions $dir)
        $verdict = [math]::Min($found.Count, 1)
        if ($verdict -ne $case.Expect) {
            $failures += "  '$($case.Name)': expected $($case.Expect) failure(s), got $($found.Count)$(if ($found.Count) { " — $($found -join '; ')" })"
        }
    }
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue

    if ($failures.Count) {
        Write-Host 'workflow-permission self-test FAILED:' -ForegroundColor Red
        $failures | ForEach-Object { Write-Host $_ -ForegroundColor Red }
        exit 1
    }
    Write-Host "workflow-permission self-test passed ($($cases.Count) cases)." -ForegroundColor Green
    exit 0
}

# --- normal mode ------------------------------------------------------------

if (-not $Path) {
    $Path = Join-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) '.github/workflows'
}

$found = @(Test-WorkflowPermissions $Path)
if ($found.Count) {
    Write-Host 'FAILED: a called workflow requests more than its caller grants:' -ForegroundColor Red
    $found | ForEach-Object { Write-Host "  $_" -ForegroundColor Red }
    Write-Host '    Widen the CALLING job''s permissions block to cover the request.' -ForegroundColor DarkGray
    exit 1
}
Write-Host "workflow permissions: every local reusable-workflow edge is satisfiable." -ForegroundColor Green
exit 0
