#!/usr/bin/env pwsh
# Content assertions over the packaged GUI artifacts (gui-package-*.ps1
# output) — the PR-time proof that each artifact SHAPE is right, so the
# tag-only surface stays thin. One script, one -Kind branch per OS family,
# run by gui.yml's packaging job on the native runner that built the
# artifacts. Exits 1 with the failed assertion on any mismatch.

[CmdletBinding()]
param(
    [Parameter(Mandatory)] [ValidateSet('windows', 'macos', 'linux')] [string] $Kind,
    # The directory holding the packaged artifacts.
    [Parameter(Mandatory)] [string] $Dir,
    # The target triple used in the artifact names.
    [Parameter(Mandatory)] [string] $Triple
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$dir = (Resolve-Path -LiteralPath $Dir).Path
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..' '..')).Path
$fail = @()

function Assert([bool] $ok, [string] $what) {
    if ($ok) { Write-Host "  ok: $what" } else { $script:fail += $what }
}

switch ($Kind) {
    'windows' {
        # ── portable zip: flat, exe + license set ────────────────────────────
        $zip = Join-Path $dir "bdinfo-rs-gui-$Triple.zip"
        Assert (Test-Path $zip) 'zip exists'
        $archive = [System.IO.Compression.ZipFile]::OpenRead($zip)
        try {
            $entries = $archive.Entries.FullName
            foreach ($name in 'bdinfo-rs-gui.exe', 'LICENSE', 'NOTICE', 'README.md') {
                Assert ($entries -contains $name) "zip carries $name at the root"
            }
        }
        finally { $archive.Dispose() }

        # ── MSI: admin-extract layout + platform summary ─────────────────────
        $msi = Join-Path $dir "bdinfo-rs-gui-$Triple.msi"
        Assert (Test-Path $msi) 'msi exists'
        $extract = Join-Path ([System.IO.Path]::GetTempPath()) "gui-msi-extract-$PID"
        if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
        $proc = Start-Process msiexec -ArgumentList "/a `"$msi`" /qn TARGETDIR=`"$extract`"" -Wait -PassThru
        Assert ($proc.ExitCode -eq 0) 'msiexec /a (administrative extract) succeeds'
        $extracted = @(Get-ChildItem $extract -Recurse -Filter 'bdinfo-rs-gui.exe' -ErrorAction SilentlyContinue)
        Assert ($extracted.Count -eq 1) 'extracted layout carries the exe once'
        foreach ($name in 'LICENSE', 'NOTICE') {
            $found = @(Get-ChildItem $extract -Recurse -Filter $name -ErrorAction SilentlyContinue)
            Assert ($found.Count -eq 1) "extracted layout carries $name once"
        }
        # Template summary property: "<platform>;<language>". arm64 MSIs are
        # authored on the x64 runner, so the tag must be checked, not assumed.
        # Plain dynamic COM calls — pwsh 7's binder resolves them; the
        # InvokeMember detour Windows PowerShell needed mis-marshals here.
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $database = $installer.OpenDatabase($msi, 0)
        $template = $database.SummaryInformation(0).Property(7)
        $platform = if ($Triple -eq 'aarch64-pc-windows-msvc') { 'Arm64' } else { 'x64' }
        Assert ($template -like "$platform;*") "MSI platform summary is $platform (got '$template')"

        # Everything below is invisible in the extracted layout: it lives in the
        # MSI's own tables and would otherwise surface only as a UAC prompt, a
        # blank Add/Remove-Programs field or a split taskbar entry on a user's
        # machine. ICE validation, which polices some of the same ground, is
        # suppressed at link time (see gui-package-windows.ps1), so these
        # assertions and a real unelevated install are the whole check.
        #
        # Rows come back as arrays of the requested columns' string data. The
        # [void] casts matter: a COM call whose return value is null emits that
        # null into the function's output, which would otherwise prepend two
        # phantom rows to every result.
        function Get-MsiRows([string] $sql, [int] $columns) {
            $view = $database.OpenView($sql)
            [void] $view.Execute()
            $rows = @()
            for ($record = $view.Fetch(); $null -ne $record; $record = $view.Fetch()) {
                $rows += , @(1..$columns | ForEach-Object { $record.StringData($_) })
            }
            [void] $view.Close()
            $rows
        }
        # A table only exists once something authors a row into it, and OpenView
        # against an absent one throws a bare COM error. Removing the last
        # authored row is exactly the regression these assertions exist to
        # catch, so an absent table has to read as no rows and fail the named
        # assertion rather than kill the script.
        $tables = @(Get-MsiRows 'SELECT Name FROM _Tables' 1 | ForEach-Object { $_[0] })
        function Get-MsiTableRows([string] $table, [string] $sql, [int] $columns) {
            if ($tables -contains $table) { Get-MsiRows $sql $columns } else { @() }
        }

        # Summary-information word count: bit 3 (value 8) is "elevated
        # privileges are not required", which is the whole of the no-UAC
        # guarantee; 2 is the compressed/long-filenames baseline. A count of 2
        # means the package went back to per-machine and now prompts.
        $words = $database.SummaryInformation(0).Property(15)
        Assert ($words -eq 10) "MSI word count is 10 — per-user, elevation not required (got '$words')"

        # ARPINSTALLLOCATION is what fills Add/Remove Programs' InstallLocation,
        # which a package manager reads the install path back out of. It has to
        # be a type-51 property assignment sequenced after costing: Property
        # rows are not formatted, so an authored row would register the literal
        # text rather than the resolved directory.
        $setLocation = @(Get-MsiTableRows 'CustomAction' "SELECT Type, Source, Target FROM CustomAction WHERE Action='SetARPINSTALLLOCATION'" 3)
        Assert ($setLocation.Count -eq 1 -and $setLocation[0][0] -eq '51' -and
            $setLocation[0][1] -eq 'ARPINSTALLLOCATION' -and $setLocation[0][2] -eq '[INSTALLDIR]') `
            'ARPINSTALLLOCATION is assigned [INSTALLDIR] by a property-setting action'

        # A silent or winget-driven install runs the execute sequence and
        # nothing else, so an execute-sequence custom action is how one would
        # come to spawn a window or demand elevation. The property assignment
        # above runs no code and is the only one allowed here; the dialog set's
        # own action (WixUIPrintEula) belongs to the UI sequence.
        $actions = @(Get-MsiTableRows 'CustomAction' 'SELECT Action FROM CustomAction' 1 | ForEach-Object { $_[0] })
        $inExecute = @(Get-MsiRows 'SELECT Action FROM InstallExecuteSequence' 1 |
            ForEach-Object { $_[0] } | Where-Object { $actions -contains $_ })
        Assert ($inExecute.Count -eq 1 -and $inExecute[0] -eq 'SetARPINSTALLLOCATION') `
            "the execute sequence runs no custom action but that assignment (got '$($inExecute -join ', ')')"

        $helpRows = @(Get-MsiRows "SELECT Value FROM Property WHERE Property='ARPHELPLINK'" 1)
        $helpLink = if ($helpRows.Count -eq 1) { $helpRows[0][0] } else { '<absent>' }
        Assert ($helpLink -eq 'https://github.com/agentjp/bdinfo-rs/issues') `
            "ARPHELPLINK points at the issue tracker (got '$helpLink')"

        # The explicit application id keys taskbar grouping and pinning to the
        # shortcut; the string is shared with the id the binary hands the
        # windowing system (`APP_ID` in crates/bdinfo-rs-gui/src/main.rs).
        $shortcutProperties = @(Get-MsiTableRows 'MsiShortcutProperty' 'SELECT Shortcut_, PropertyKey, PropVariantValue FROM MsiShortcutProperty' 3)
        Assert ($shortcutProperties.Count -eq 1 -and $shortcutProperties[0][0] -eq 'StartMenuShortcut' -and
            $shortcutProperties[0][1] -eq 'System.AppUserModel.ID' -and
            $shortcutProperties[0][2] -eq 'bdinfo-rs-gui') `
            'the Start-menu shortcut declares System.AppUserModel.ID = bdinfo-rs-gui'

        if (Test-Path $extract) { Remove-Item -Recurse -Force $extract }
    }

    'macos' {
        $dmg = Join-Path $dir "bdinfo-rs-gui-$Triple.dmg"
        Assert (Test-Path $dmg) 'dmg exists'
        $mount = "/Volumes/bdinfo-rs GUI"
        & hdiutil attach $dmg -readonly -nobrowse
        Assert ($LASTEXITCODE -eq 0) 'hdiutil attach succeeds'
        try {
            $app = Join-Path $mount 'bdinfo-rs GUI.app'
            Assert (Test-Path $app) 'dmg carries the .app'
            Assert (Test-Path (Join-Path $mount 'Applications')) 'dmg carries the /Applications symlink'
            Assert (Test-Path (Join-Path $app 'Contents/MacOS/bdinfo-rs-gui')) 'bundle carries the binary'
            Assert (Test-Path (Join-Path $app 'Contents/Resources/bdinfo-rs-gui.icns')) 'bundle carries the icns'
            foreach ($name in 'LICENSE', 'NOTICE') {
                Assert (Test-Path (Join-Path $app "Contents/Resources/$name")) "bundle carries $name"
            }
            & plutil -lint (Join-Path $app 'Contents/Info.plist')
            Assert ($LASTEXITCODE -eq 0) 'Info.plist lints'
            $bundleVersion = & plutil -extract CFBundleShortVersionString raw (Join-Path $app 'Contents/Info.plist')
            Assert ($bundleVersion -match '^\d+\.\d+\.\d+$') "CFBundleShortVersionString is substituted (got '$bundleVersion')"
            & codesign --verify --deep --strict $app
            Assert ($LASTEXITCODE -eq 0) 'ad-hoc signature verifies'
        }
        finally { & hdiutil detach $mount | Out-Null }
    }

    'linux' {
        # ── the shared text assets validate against their specs ──────────────
        $packaging = Join-Path $repo 'crates/bdinfo-rs-gui/packaging'
        # The AppStream component id names the desktop file, the metainfo file
        # and the icons as installed.
        $appId = 'io.github.agentjp.bdinfo-rs'
        & desktop-file-validate (Join-Path $packaging "$appId.desktop")
        Assert ($LASTEXITCODE -eq 0) 'desktop file validates'
        # --nonet: without it the validator fetches every <screenshot> URL. Those
        # point at raw.githubusercontent.com on master, so a screenshot added in
        # a pull request would 404 until the very merge the check is gating —
        # and a green gate must not depend on a live third-party fetch anyway.
        & appstream-util validate-relax --nonet (Join-Path $packaging "$appId.metainfo.xml")
        Assert ($LASTEXITCODE -eq 0) 'AppStream metainfo validates (relax — the Fedora gate)'

        # ── .deb: recommends + payload paths ─────────────────────────────────
        $deb = Join-Path $dir "bdinfo-rs-gui-$Triple.deb"
        Assert (Test-Path $deb) 'deb exists'
        $info = & dpkg-deb --info $deb
        Assert (@($info | Select-String 'Recommends:.*xdg-desktop-portal').Count -gt 0) 'deb recommends the portal'
        Assert (@($info | Select-String 'Recommends:.*libvulkan1').Count -gt 0) 'deb recommends libvulkan1'
        Assert (@($info | Select-String 'Depends:.*libc6').Count -gt 0) 'deb depends on glibc ($auto resolved)'
        $contents = & dpkg-deb --contents $deb
        foreach ($path in
            './usr/bin/bdinfo-rs-gui',
            "./usr/share/applications/$appId.desktop",
            "./usr/share/metainfo/$appId.metainfo.xml",
            "./usr/share/icons/hicolor/512x512/apps/$appId.png",
            './usr/share/doc/bdinfo-rs-gui/copyright') {
            Assert (@($contents | Select-String ([regex]::Escape($path))).Count -gt 0) "deb carries $path"
        }

        # ── .rpm: weak deps + payload paths ──────────────────────────────────
        $rpm = Join-Path $dir "bdinfo-rs-gui-$Triple.rpm"
        Assert (Test-Path $rpm) 'rpm exists'
        $recommends = & rpm -qp --recommends $rpm 2>$null
        Assert (@($recommends | Select-String 'xdg-desktop-portal').Count -gt 0) 'rpm recommends the portal'
        Assert (@($recommends | Select-String 'vulkan-loader').Count -gt 0) 'rpm recommends vulkan-loader'
        $files = & rpm -qpl $rpm 2>$null
        foreach ($path in
            '/usr/bin/bdinfo-rs-gui',
            "/usr/share/applications/$appId.desktop",
            "/usr/share/metainfo/$appId.metainfo.xml",
            "/usr/share/icons/hicolor/512x512/apps/$appId.png") {
            Assert (@($files | Select-String ([regex]::Escape($path))).Count -gt 0) "rpm carries $path"
        }

        # ── AppImage: the four spec-mandated root entries ────────────────────
        $appImage = Join-Path $dir "bdinfo-rs-gui-$Triple.AppImage"
        Assert (Test-Path $appImage) 'AppImage exists'
        $work = Join-Path ([System.IO.Path]::GetTempPath()) "gui-appimage-check-$PID"
        New-Item -ItemType Directory -Force $work | Out-Null
        Push-Location $work
        try {
            & chmod +x $appImage
            & $appImage --appimage-extract | Out-Null
            Assert ($LASTEXITCODE -eq 0) 'AppImage self-extracts (static runtime, no FUSE)'
            $root = Join-Path $work 'squashfs-root'
            foreach ($entry in 'AppRun', "$appId.desktop", "$appId.png", '.DirIcon') {
                Assert (Test-Path (Join-Path $root $entry)) "AppImage root carries $entry"
            }
            Assert (Test-Path (Join-Path $root 'usr/bin/bdinfo-rs-gui')) 'AppImage carries the binary'
            foreach ($name in 'LICENSE', 'NOTICE') {
                Assert (Test-Path (Join-Path $root "usr/share/doc/bdinfo-rs-gui/$name")) "AppImage carries $name"
            }
        }
        finally { Pop-Location }
        Remove-Item -Recurse -Force $work
    }
}

if ($fail.Count) {
    Write-Host 'FAILED packaging assertions:'
    $fail | ForEach-Object { Write-Host "    $_" }
    exit 1
}
Write-Host "packaging checks: PASS ($Kind, $Triple)"
