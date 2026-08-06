#!/usr/bin/env pwsh
#Requires -Version 7.0

<#
.SYNOPSIS
    Map changed paths to the CI areas that gate the workflow's jobs.

.DESCRIPTION
    One rule governs every entry in $Rules below: a job runs if and only if the
    diff could change that job's verdict. A rule therefore asks what KIND of
    input a file is (Rust source, manifest, lockfile, YAML, TOML, disc bytes,
    icon, prose) before it asks which directory holds it — classifying by
    directory alone treats a README beside a source file as if it were source.

    Every rule carries a Why: the mechanism by which the file reaches the jobs
    its areas gate. A rule whose Why cannot be stated as a mechanism does not
    belong here.

    Areas and the jobs they gate:

      core       build + test, lint, coverage, mutants, install + scan, musl scan
      gui        the gui reusable workflow and its mutation job
      wasm       the wasm reusable workflow and its mutation job
      fuzz       corpus replay
      deps       cargo deny, cargo vet
      yaml       action-validator + ryl
      workflows  zizmor
      canary     build + test on one leg per shell family
      dist       dist generate --check
      pkg        .deb/.rpm build + install
      toml       taplo
      links      lychee
      typos      typos

    A rule with an empty Areas list is a deliberate declaration that no job on
    the pull-request path reads the file. -SelfTest requires every tracked path
    to match at least one Structural rule, so a file added without a rule fails
    the gate rather than silently gating nothing.

.PARAMETER Path
    Paths to classify, relative to the repository root, forward-slash
    separated. Read from standard input when omitted.

.PARAMETER All
    Emit every area. The caller's fail-safe for a diff it could not compute.

.PARAMETER SelfTest
    Assert the golden cases in $GoldenCases and that every tracked path matches
    a Structural rule, then exit. Classifies nothing.

.EXAMPLE
    git diff --name-only HEAD^1 HEAD | ./.github/scripts/classify-diff.ps1
#>

[CmdletBinding()]
param(
    [Parameter(ValueFromPipeline)]
    [string[]] $Path,

    [switch] $All,

    [switch] $SelfTest
)

begin {
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'

    $script:Inputs = [System.Collections.Generic.List[string]]::new()

    # Every area this script can emit. The workflow reads each as a job output,
    # so adding one here without adding the matching `if:` gates nothing.
    $script:AllAreas = @(
        'core', 'gui', 'wasm', 'fuzz', 'deps', 'yaml', 'workflows',
        'canary', 'dist', 'pkg', 'toml', 'links', 'typos'
    )

    # The four crates that link bdinfo-rs-core by path (bdinfo-rs, the two
    # sibling workspaces, and the fuzz targets) rebuild when its sources change.
    # bdinfo-rs itself is a leaf: nothing depends on the CLI crate, so its
    # sources reach `core` alone.
    $script:CoreDependents = @('core', 'gui', 'wasm', 'fuzz')

    function Test-Match {
        param([string] $File, [string[]] $Patterns)

        foreach ($p in $Patterns) {
            if ($File -like $p) { return $true }
        }
        return $false
    }

    # Prose carries no build input: no source file includes a Markdown file,
    # rustdoc does not read one, and `cargo package` requires only that the
    # readme named in a manifest exists. Prose under a crate directory is
    # therefore prose, not that crate's source.
    function Test-Prose {
        param([string] $File)

        return (Test-Match $File @('*.md', 'LICENSE', '*/LICENSE', 'NOTICE'))
    }

    # Opaque bytes: no URL for lychee to resolve and no word for typos to spell.
    # Mirrors the `binary` attributes in .gitattributes.
    function Test-Binary {
        param([string] $File)

        return (Test-Match $File @(
                '*.iso', '*.m2ts', '*.clpi', '*.mpls', '*.bdmv',
                '*.png', '*.ico', '*.icns', '*.jpg', '*.jpeg', '*.gif', '*.webp',
                '*.exe', '*.pdb',
                'fuzz/corpus/*'
            ))
    }

    # ------------------------------------------------------------------ rules
    # Ordered only for readability; every matching rule contributes its areas.
    # Structural = $false marks the two whole-tree scanners, whose own configs
    # decide what they read — they can match any file and so cannot serve as
    # evidence that a path was deliberately classified.
    $script:Rules = @(
        @{
            Name       = 'core and CLI sources'
            Areas      = $script:CoreDependents
            Structural = $true
            Why        = 'bdinfo-rs-core is a path dependency of bdinfo-rs, both sibling workspaces and the fuzz targets, so its sources rebuild all four.'
            Match      = { param($f) (Test-Match $f @('crates/bdinfo-rs-core/*')) -and -not (Test-Prose $f) }
        },
        @{
            Name       = 'CLI sources and fixtures'
            Areas      = @('core')
            Structural = $true
            Why        = 'The bdinfo-rs binary crate is a leaf. Its sources, the committed disc fixtures and the golden reports feed build + test, coverage and the install + scan jobs; no other crate links it.'
            Match      = { param($f) (Test-Match $f @('crates/bdinfo-rs/*')) -and -not (Test-Prose $f) }
        },
        @{
            Name       = 'debian packaging inputs'
            Areas      = @('pkg')
            Structural = $true
            Why        = 'build-linux-packages.sh reads this tree to assemble the .deb.'
            Match      = { param($f) Test-Match $f @('crates/bdinfo-rs/debian/*') }
        },
        @{
            Name       = 'gui crate'
            Areas      = @('gui')
            Structural = $true
            Why        = 'Sources, the generated icon set the packaging step consumes, and the crate manifest all feed the gui workflow.'
            Match      = { param($f) (Test-Match $f @('crates/bdinfo-rs-gui/*')) -and -not (Test-Prose $f) }
        },
        @{
            Name       = 'wasm crate'
            Areas      = @('wasm')
            Structural = $true
            Why        = 'Sources, the web assets and the parity golden all feed the wasm workflow.'
            Match      = { param($f) (Test-Match $f @('crates/bdinfo-rs-wasm/*')) -and -not (Test-Prose $f) }
        },
        @{
            Name       = 'fuzz workspace'
            Areas      = @('fuzz')
            Structural = $true
            Why        = 'Targets, the seed corpus and fuzz/Cargo.lock are exactly what the replay job builds and feeds.'
            Match      = { param($f) (Test-Match $f @('fuzz/*')) -and -not (Test-Prose $f) }
        },
        @{
            Name       = 'root workspace manifest'
            Areas      = $script:CoreDependents + @('deps', 'dist', 'toml')
            Structural = $true
            Why        = 'bdinfo-rs-core inherits version, edition, rust-version, license and authors from [workspace.package], so the manifest reaches every crate that links it; dist derives the release workflow from it.'
            Match      = { param($f) $f -eq 'Cargo.toml' }
        },
        @{
            Name       = 'root lockfile'
            Areas      = @('core', 'deps', 'pkg')
            Structural = $true
            Why        = 'Resolves the root workspace only: crates/bdinfo-rs-gui, crates/bdinfo-rs-wasm and fuzz each carry their own Cargo.lock and never read this one.'
            Match      = { param($f) $f -eq 'Cargo.lock' }
        },
        @{
            Name       = 'toolchain and formatting configuration'
            Areas      = $script:CoreDependents + @('toml')
            Structural = $true
            Why        = 'rust-toolchain.toml pins the compiler for every workspace in the tree; rustfmt reads the nearest rustfmt.toml walking up from each file, so the root file governs the sibling crates too.'
            Match      = { param($f) Test-Match $f @('rust-toolchain.toml', 'rustfmt.toml') }
        },
        @{
            Name       = 'root clippy configuration'
            Areas      = $script:CoreDependents + @('toml')
            Structural = $true
            Why        = 'crates/bdinfo-rs-gui carries its own clippy.toml but crates/bdinfo-rs-wasm does not; rather than depend on clippy resolving a configuration across workspace roots, this fires every lint that could read it.'
            Match      = { param($f) $f -eq 'clippy.toml' }
        },
        @{
            Name       = 'cargo configuration'
            Areas      = $script:CoreDependents + @('toml')
            Structural = $true
            Why        = 'Cargo merges configuration from every ancestor directory of the invocation, so the root aliases and build settings apply inside the sibling workspaces as well.'
            Match      = { param($f) $f -eq '.cargo/config.toml' }
        },
        @{
            Name       = 'root mutation and test-runner configuration'
            Areas      = @('core', 'toml')
            Structural = $true
            Why        = 'cargo-mutants and nextest each read the configuration under the workspace root they are invoked in; the sibling workspaces carry their own .cargo/mutants.toml and neither reads these.'
            Match      = { param($f) Test-Match $f @('.cargo/mutants.toml', '.config/nextest.toml') }
        },
        @{
            Name       = 'spell-check configuration'
            Areas      = @('typos', 'toml')
            Structural = $true
            Why        = 'The allow-list and exclusions decide the typos verdict and nothing else.'
            Match      = { param($f) $f -eq 'typos.toml' }
        },
        @{
            Name       = 'dependency ban policy'
            Areas      = @('deps', 'toml')
            Structural = $true
            Why        = 'The cargo-deny policy for bans, licences and sources.'
            Match      = { param($f) Test-Match $f @('deny.toml', 'crates/*/deny.toml') }
        },
        @{
            Name       = 'vet audit store'
            Areas      = @('deps')
            Structural = $true
            Why        = 'The cargo-vet audits, imports and configuration. Written by cargo-vet and excluded from taplo, so a change here reaches no formatter.'
            Match      = { param($f) Test-Match $f @('supply-chain/*') }
        },
        @{
            Name       = 'release layout'
            Areas      = @('dist', 'pkg', 'toml')
            Structural = $true
            Why        = 'dist generate --check regenerates the release workflow from dist-workspace.toml, and the Linux package build reads its target list.'
            Match      = { param($f) $f -eq 'dist-workspace.toml' }
        },
        @{
            Name       = 'orchestrator workflow'
            Areas      = @('yaml', 'workflows', 'canary')
            Structural = $true
            Why        = 'Editing the file that routes every gate job must exercise that routing end to end; one build and test leg per shell family runs the plan, a caller and an inner job.'
            Match      = { param($f) $f -eq '.github/workflows/ci.yml' }
        },
        @{
            Name       = 'root-workspace reusable workflow'
            Areas      = @('yaml', 'workflows', 'core', 'deps', 'dist', 'fuzz', 'pkg')
            Structural = $true
            Why        = 'The file is the root-workspace gate: an edit to it, including a pinned action digest, changes what its build, lint, coverage, install, packaging, audit, drift-guard and corpus-replay jobs do. Each of those keeps its own area gate inside the file, so every area they consume has to fire for the edit to reach them.'
            Match      = { param($f) $f -eq '.github/workflows/core.yml' }
        },
        @{
            Name       = 'whole-tree scanner reusable workflow'
            Areas      = @('yaml', 'workflows', 'toml', 'typos', 'links')
            Structural = $true
            Why        = 'The file is the gate for the scanners that read the whole repository; see the root-workspace reusable workflow.'
            Match      = { param($f) $f -eq '.github/workflows/repo.yml' }
        },
        @{
            Name       = 'gui reusable workflow'
            Areas      = @('yaml', 'workflows', 'gui')
            Structural = $true
            Why        = 'The file is the gui gate: an edit to it, including a pinned action digest, changes what those jobs do.'
            Match      = { param($f) $f -eq '.github/workflows/gui.yml' }
        },
        @{
            Name       = 'wasm reusable workflow'
            Areas      = @('yaml', 'workflows', 'wasm')
            Structural = $true
            Why        = 'The file is the wasm gate; see the gui reusable workflow.'
            Match      = { param($f) $f -eq '.github/workflows/wasm.yml' }
        },
        @{
            Name       = 'generated release workflow'
            Areas      = @('yaml', 'workflows', 'dist')
            Structural = $true
            Why        = 'cargo-dist generates this file; an edit that dist would not regenerate byte-for-byte fails dist generate --check.'
            Match      = { param($f) $f -eq '.github/workflows/v-release.yml' }
        },
        @{
            Name       = 'dist build setup'
            Areas      = @('yaml', 'dist')
            Structural = $true
            Why        = 'cargo-dist splices this file into the generated release workflow.'
            Match      = { param($f) $f -eq '.github/build-setup.yml' }
        },
        @{
            Name       = 'remaining workflows'
            Areas      = @('yaml', 'workflows')
            Structural = $true
            Why        = 'zizmor audits every file under .github/workflows and action-validator schema-checks it. Workflows the orchestrator never calls are verified by their own dispatch or tag run, not by re-running an unrelated gate.'
            Match      = { param($f) Test-Match $f @('.github/workflows/*') }
        },
        @{
            Name       = 'zizmor configuration'
            Areas      = @('yaml', 'workflows')
            Structural = $true
            Why        = 'The suppression list decides the zizmor verdict.'
            Match      = { param($f) $f -eq '.github/zizmor.yml' }
        },
        @{
            Name       = 'mutation composite action'
            Areas      = @('core', 'gui', 'wasm', 'yaml', 'workflows')
            Structural = $true
            Why        = 'The mutate-crate composite is the body of the three advisory PR-diff mutation jobs, which the core, gui and wasm areas gate; action-validator and zizmor read the action definitions under .github/actions as well as the workflows.'
            Match      = { param($f) Test-Match $f @('.github/actions/mutate-crate/*') }
        },
        @{
            Name       = 'source rule script'
            Areas      = @('core')
            Structural = $true
            Why        = 'The lint job runs it over the root workspace sources.'
            Match      = { param($f) $f -eq '.github/scripts/source-rules.ps1' }
        },
        @{
            Name       = 'gui gate scripts'
            Areas      = @('gui')
            Structural = $true
            Why        = 'The gui workflow runs these directly. The publishing helpers are named publish-gui-* so that this pattern cannot reach them.'
            Match      = { param($f) (Test-Match $f @('.github/scripts/gui-*.ps1')) }
        },
        @{
            Name       = 'wasm gate script'
            Areas      = @('wasm')
            Structural = $true
            Why        = 'The wasm workflow runs it directly, twice: the LICENSE/NOTICE parity check and the unsafe-token tripwire.'
            Match      = { param($f) (Test-Match $f @('.github/scripts/wasm-*.ps1')) }
        },
        @{
            Name       = 'coverage floor script'
            Areas      = @('gui', 'wasm')
            Structural = $true
            Why        = 'Both sibling-crate workflows hand it their llvm-cov export, so it decides both coverage verdicts. The root workspace enforces its floors through cargo-llvm-cov''s own --fail-under flags and never reads this script.'
            Match      = { param($f) $f -eq '.github/scripts/cov-floor.ps1' }
        },
        @{
            Name       = 'linux package script'
            Areas      = @('pkg')
            Structural = $true
            Why        = 'The .deb/.rpm job runs it.'
            Match      = { param($f) $f -eq '.github/build-linux-packages.sh' }
        },
        @{
            Name       = 'checkout byte fidelity'
            Areas      = $script:CoreDependents
            Structural = $true
            Why        = 'The attributes decide the bytes every job checks out: eol=lf keeps rustfmt --check green, `binary` keeps the fuzz corpus and disc fixtures from line-ending rewriting, and -text preserves the CRLF the golden reports are compared against.'
            Match      = { param($f) $f -eq '.gitattributes' }
        },
        @{
            Name       = 'link-check exclusions'
            Areas      = @('links')
            Structural = $true
            Why        = 'The ignore list decides the lychee verdict.'
            Match      = { param($f) $f -eq '.lycheeignore' }
        },
        @{
            Name       = 'remaining YAML'
            Areas      = @('yaml')
            Structural = $true
            Why        = 'ryl lints every tracked YAML file in the repository, not only the workflows.'
            Match      = { param($f) Test-Match $f @('*.yml', '*.yaml') }
        },
        @{
            Name       = 'remaining TOML'
            Areas      = @('toml')
            Structural = $true
            Why        = 'taplo formats and lints every *.toml outside the trees its own exclude list names.'
            Match      = {
                param($f)
                if (-not (Test-Match $f @('*.toml'))) { return $false }
                return -not (Test-Match $f @('supply-chain/*', 'target/*', 'scratch/*'))
            }
        },
        @{
            Name       = 'crate manifests'
            Areas      = @('deps')
            Structural = $true
            Why        = 'cargo-deny resolves bans, licences and sources from the manifests.'
            Match      = { param($f) Test-Match $f @('*/Cargo.toml') }
        },
        @{
            Name       = 'prose'
            Areas      = @()
            Structural = $true
            Why        = 'Reaches lychee and typos through the whole-tree rules below and no other job: nothing includes, compiles or parses it.'
            Match      = { param($f) Test-Prose $f }
        },
        @{
            Name       = 'publishing helpers'
            Areas      = @()
            Structural = $true
            Why        = 'Run only by the tag and dispatch publishing lanes, which the orchestrator never calls; their dry runs verify them. _common.ps1 carries what those legs share and cloudsmith-push.ps1 the package push both the CLI and the GUI lane call, so the two are named here as well as the publish-* legs.'
            Match      = { param($f) Test-Match $f @('.github/scripts/publish-*.ps1', '.github/scripts/_common.ps1', '.github/scripts/cloudsmith-push.ps1') }
        },
        @{
            Name       = 'banned-word and classifier scripts'
            Areas      = @()
            Structural = $true
            Why        = 'Both run unconditionally on every pull request — the banned-word check from a workflow this classifier does not gate, and this script from the job that calls it — so neither needs an area to be verified.'
            Match      = { param($f) Test-Match $f @('.github/scripts/check-banned-words.ps1', '.github/scripts/classify-diff.ps1') }
        },
        @{
            Name       = 'packaging templates'
            Areas      = @()
            Structural = $true
            Why        = 'Rendered by the tag and dispatch publishing lanes only; no pull-request job reads them.'
            Match      = { param($f) Test-Match $f @('packaging/*') }
        },
        @{
            Name       = 'container build'
            Areas      = @()
            Structural = $true
            Why        = 'The image workflow runs on workflow_run after a completed Release and on dispatch, never on a pull request.'
            Match      = { param($f) Test-Match $f @('Dockerfile', '.dockerignore') }
        },
        @{
            Name       = 'repository metadata'
            Areas      = @()
            Structural = $true
            Why        = 'Read by git, editors, the citation widget and the commit-message linter — none of which is a job this classifier gates.'
            Match      = { param($f) Test-Match $f @('.gitignore', '*/.gitignore', '.editorconfig', '.convco', 'CITATION.cff') }
        },

        # --- whole-tree scanners -------------------------------------------
        # Both read the working tree rather than a declared file set, so their
        # exclusions are mirrored from the tools' own configuration.
        @{
            Name       = 'link surface'
            Areas      = @('links')
            Structural = $false
            Why        = 'lychee walks the whole tree, so any file it can read as text can carry a URL that changes its verdict.'
            Match      = {
                param($f)
                if (Test-Binary $f) { return $false }
                return -not (Test-Match $f @(
                        'Cargo.lock',
                        'fuzz/corpus/*',
                        'crates/bdinfo-rs/tests/fixtures/golden/*',
                        'crates/bdinfo-rs-wasm/tests/golden_report.txt'
                    ))
            }
        },
        @{
            Name       = 'spell-check surface'
            Areas      = @('typos')
            Structural = $false
            Why        = 'typos walks the whole tree; the exclusions mirror files.extend-exclude in typos.toml.'
            Match      = {
                param($f)
                if (Test-Binary $f) { return $false }
                return -not (Test-Match $f @(
                        'target/*', 'fuzz/target/*', 'fuzz/corpus/*',
                        'crates/bdinfo-rs/tests/fixtures/*',
                        'crates/bdinfo-rs-wasm/tests/golden_report.txt',
                        'crates/bdinfo-rs-core/src/language_codes.rs',
                        'supply-chain/*', 'Cargo.lock', '*.exe'
                    ))
            }
        }
    )

    function Get-PathAreas {
        param([string] $File)

        $areas = [System.Collections.Generic.SortedSet[string]]::new()
        $structural = $false

        foreach ($rule in $script:Rules) {
            if (-not (& $rule.Match $File)) { continue }
            if ($rule.Structural) { $structural = $true }
            foreach ($a in $rule.Areas) { [void] $areas.Add($a) }
        }

        return [pscustomobject]@{
            Areas      = @($areas)
            Structural = $structural
        }
    }
}

process {
    if ($Path) { $script:Inputs.AddRange([string[]] $Path) }
}

end {
    # ------------------------------------------------------------- self-test
    if ($SelfTest) {
        # Each case pins one rule's outcome. A case is the assertion that the
        # rule's Why holds, so a case that starts failing is a claim to re-check,
        # not a number to update.
        $GoldenCases = @(
            @{ Path = 'crates/bdinfo-rs/tests/fixtures/README.md'; Areas = 'links' }
            @{ Path = 'crates/bdinfo-rs-gui/README.md'; Areas = 'links typos' }
            @{ Path = 'README.md'; Areas = 'links typos' }
            @{ Path = 'crates/bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV/index.bdmv'; Areas = 'core' }
            @{ Path = 'crates/bdinfo-rs/tests/fixtures/golden/folder.txt'; Areas = 'core' }
            @{ Path = 'crates/bdinfo-rs/src/main.rs'; Areas = 'core links typos' }
            @{ Path = 'crates/bdinfo-rs-core/src/bdrom/m2ts.rs'; Areas = 'core fuzz gui links typos wasm' }
            @{ Path = 'crates/bdinfo-rs-gui/packaging/icons/hicolor/32x32/apps/bdinfo-rs-gui.png'; Areas = 'gui' }
            @{ Path = 'crates/bdinfo-rs-gui/src/main.rs'; Areas = 'gui links typos' }
            @{ Path = '.cargo/config.toml'; Areas = 'core fuzz gui links toml typos wasm' }
            @{ Path = '.cargo/mutants.toml'; Areas = 'core links toml typos' }
            @{ Path = '.config/nextest.toml'; Areas = 'core links toml typos' }
            @{ Path = 'Cargo.lock'; Areas = 'core deps pkg' }
            @{ Path = 'Cargo.toml'; Areas = 'core deps dist fuzz gui links toml typos wasm' }
            @{ Path = '.gitattributes'; Areas = 'core fuzz gui links typos wasm' }
            @{ Path = '.lycheeignore'; Areas = 'links typos' }
            @{ Path = 'scorecard.yml'; Areas = 'links typos yaml' }
            @{ Path = '.github/dependabot.yml'; Areas = 'links typos yaml' }
            @{ Path = '.github/workflows/ci.yml'; Areas = 'canary links typos workflows yaml' }
            @{ Path = '.github/workflows/core.yml'; Areas = 'core deps dist fuzz links pkg typos workflows yaml' }
            @{ Path = '.github/workflows/repo.yml'; Areas = 'links toml typos workflows yaml' }
            @{ Path = '.github/workflows/gui.yml'; Areas = 'gui links typos workflows yaml' }
            @{ Path = '.github/workflows/sweep-mutants.yml'; Areas = 'links typos workflows yaml' }
            @{ Path = '.github/workflows/docker.yml'; Areas = 'links typos workflows yaml' }
            @{ Path = '.github/actions/mutate-crate/action.yml'; Areas = 'core gui links typos wasm workflows yaml' }
            @{ Path = '.github/scripts/gui-package-windows.ps1'; Areas = 'gui links typos' }
            @{ Path = '.github/scripts/wasm-rules.ps1'; Areas = 'links typos wasm' }
            @{ Path = '.github/scripts/cov-floor.ps1'; Areas = 'gui links typos wasm' }
            @{ Path = '.github/scripts/publish-gui-aur.ps1'; Areas = 'links typos' }
            @{ Path = '.github/scripts/_common.ps1'; Areas = 'links typos' }
            @{ Path = '.github/scripts/cloudsmith-push.ps1'; Areas = 'links typos' }
            @{ Path = 'packaging/aur/PKGBUILD.template'; Areas = 'links typos' }
            @{ Path = 'Dockerfile'; Areas = 'links typos' }
            @{ Path = 'fuzz/fuzz_targets/mpls.rs'; Areas = 'fuzz links typos' }
            @{ Path = 'fuzz/corpus/mpls/seed'; Areas = 'fuzz' }
            @{ Path = 'supply-chain/audits.toml'; Areas = 'deps links' }
            @{ Path = 'deny.toml'; Areas = 'deps links toml typos' }
        )

        $failures = [System.Collections.Generic.List[string]]::new()

        foreach ($case in $GoldenCases) {
            $got = (Get-PathAreas $case.Path).Areas -join ' '
            if ($got -ne $case.Areas) {
                $failures.Add("  $($case.Path)`n      expected: $($case.Areas)`n      got:      $got")
            }
        }

        # Exhaustiveness. A tracked path that matches no Structural rule gates
        # nothing and would be verified only by a scheduled full run, so the
        # gate refuses it until a rule either claims it or declares, with a
        # Why, that no pull-request job reads it.
        $tracked = @(git ls-files)
        if ($LASTEXITCODE -ne 0) { throw 'git ls-files failed' }

        $unclassified = @($tracked | Where-Object { -not (Get-PathAreas $_).Structural })
        if ($unclassified) {
            $failures.Add("  no rule claims these tracked paths:`n" +
                (($unclassified | ForEach-Object { "      $_" }) -join "`n"))
        }

        if ($failures.Count -gt 0) {
            Write-Host "classify-diff self-test FAILED`n"
            $failures | ForEach-Object { Write-Host $_ }
            exit 1
        }

        Write-Host "classify-diff self-test passed: $($GoldenCases.Count) cases, $($tracked.Count) tracked paths classified."
        exit 0
    }

    # ------------------------------------------------------------- classify
    $fired = [System.Collections.Generic.SortedSet[string]]::new()

    if ($All) {
        foreach ($a in $script:AllAreas) { [void] $fired.Add($a) }
    }
    else {
        foreach ($f in $script:Inputs) {
            $f = $f.Trim()
            if (-not $f) { continue }
            foreach ($a in (Get-PathAreas $f).Areas) { [void] $fired.Add($a) }
        }
    }

    # build + test runs one leg per released target when the root workspace is
    # in play, and one leg per shell family when only the orchestrator changed.
    $testOs = '[]'
    if ($fired.Contains('core')) {
        $testOs = '["ubuntu-latest","ubuntu-24.04-arm","windows-2025","windows-11-arm","macos-15-intel","macos-15"]'
    }
    elseif ($fired.Contains('canary')) {
        $testOs = '["ubuntu-latest","windows-2025"]'
    }

    foreach ($a in $script:AllAreas) {
        "$a=$($fired.Contains($a).ToString().ToLowerInvariant())"
    }
    "test=$(($testOs -ne '[]').ToString().ToLowerInvariant())"
    "test-os=$testOs"
}
