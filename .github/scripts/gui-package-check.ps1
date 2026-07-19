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
        # Template summary property: "<platform>;<language>". arm64 MSIs are
        # authored on the x64 runner, so the tag must be checked, not assumed.
        # Plain dynamic COM calls — pwsh 7's binder resolves them; the
        # InvokeMember detour Windows PowerShell needed mis-marshals here.
        $installer = New-Object -ComObject WindowsInstaller.Installer
        $database = $installer.OpenDatabase($msi, 0)
        $template = $database.SummaryInformation(0).Property(7)
        $platform = if ($Triple -eq 'aarch64-pc-windows-msvc') { 'Arm64' } else { 'x64' }
        Assert ($template -like "$platform;*") "MSI platform summary is $platform (got '$template')"
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
        & desktop-file-validate (Join-Path $packaging 'bdinfo-rs-gui.desktop')
        Assert ($LASTEXITCODE -eq 0) 'desktop file validates'
        & appstream-util validate-relax (Join-Path $packaging 'io.github.agentjp.bdinfo_rs_gui.metainfo.xml')
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
            './usr/share/applications/bdinfo-rs-gui.desktop',
            './usr/share/metainfo/io.github.agentjp.bdinfo_rs_gui.metainfo.xml',
            './usr/share/icons/hicolor/512x512/apps/bdinfo-rs-gui.png',
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
            '/usr/share/applications/bdinfo-rs-gui.desktop',
            '/usr/share/metainfo/io.github.agentjp.bdinfo_rs_gui.metainfo.xml',
            '/usr/share/icons/hicolor/512x512/apps/bdinfo-rs-gui.png') {
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
            foreach ($entry in 'AppRun', 'bdinfo-rs-gui.desktop', 'bdinfo-rs-gui.png', '.DirIcon') {
                Assert (Test-Path (Join-Path $root $entry)) "AppImage root carries $entry"
            }
            Assert (Test-Path (Join-Path $root 'usr/bin/bdinfo-rs-gui')) 'AppImage carries the binary'
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
