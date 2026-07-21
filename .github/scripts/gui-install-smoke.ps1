#!/usr/bin/env pwsh
# Install-and-launch smoke over the packaged GUI artifacts (gui-package-*.ps1
# output): install the OS package for real, probe the INSTALLED binary with
# `--version`, uninstall again. gui-package-check.ps1 proves artifact shape;
# this proves the package installs unattended and the installed binary
# executes on the target OS. Shared by gui.yml's packaging smoke and the
# gui-release.yml lane, like the packaging scripts themselves.
#
# The probe is Start-Process + WaitForExit, the version-probe idiom gui.yml's
# build legs established: a windows_subsystem=windows binary detaches from
# the console, so a plain invocation would neither wait nor see the exit
# code, and on Windows the exit code is the whole contract (with no console
# the print goes nowhere — crates/bdinfo-rs-gui/src/main.rs). On the unix
# families stdout works, so the probe also asserts the printed line is
# exactly `bdinfo-rs-gui <-Version>`: the installed binary must be the one
# just packaged, not a stale install.
#
# Per -Kind:
#   windows      the portable zip's exe, extracted and probed; then the MSI:
#                msiexec /i /qn (per-user by design, so the unattended
#                install itself asserts that no elevation prompt appears),
#                probe under %LOCALAPPDATA%\Programs, assert the Add/Remove
#                Programs entry and that the user PATH stays untouched,
#                msiexec /x, gone-checks; then a second install pass with
#                ADDLOCAL selecting the opt-in PATH feature, asserting the
#                PATH entry appears and its uninstall removes it.
#   windows-zip  the zip probe alone — the release lane's windows-11-arm leg
#                has no MSI (authored on the x64 runner, where an Arm64
#                package cannot be installed).
#   linux        dpkg -i of the .deb, probe /usr/bin, dpkg -r; then the
#                AppImage via --appimage-extract-and-run (no FUSE on
#                runners). The .rpm is deliberately not installed: rpm -i
#                onto a dpkg system is not a real install — its payload
#                assertions live in gui-package-check.ps1.
#   macos        mount the dmg and probe the bundle binary in place — the
#                .app installs by copy, so executing it from the image is
#                the install test.

[CmdletBinding()]
param(
    [Parameter(Mandatory)] [ValidateSet('windows', 'windows-zip', 'macos', 'linux')] [string] $Kind,
    # The directory holding the packaged artifacts.
    [Parameter(Mandatory)] [string] $Dir,
    # The target triple used in the artifact names.
    [Parameter(Mandatory)] [string] $Triple,
    # The crate version the probes must report.
    [Parameter(Mandatory)] [string] $Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$dir = (Resolve-Path -LiteralPath $Dir).Path
$fail = @()

function Assert([bool] $ok, [string] $what) {
    if ($ok) { Write-Host "  ok: $what" } else { $script:fail += $what }
}

# Runs `<exe> --version`, asserting exit 0 within 30 s; where stdout is
# readable (everywhere but a Windows-subsystem binary launched without a
# console) also asserts the printed version line.
function Invoke-VersionProbe([string] $exe, [string] $what) {
    $outFile = Join-Path ([System.IO.Path]::GetTempPath()) "gui-smoke-stdout-$PID.txt"
    $p = Start-Process -FilePath $exe -ArgumentList '--version' -PassThru `
        -RedirectStandardOutput $outFile
    if (-not $p.WaitForExit(30000)) {
        $p.Kill()
        Assert $false "$what --version exits within 30 s"
        return
    }
    Assert ($p.ExitCode -eq 0) "$what --version exits 0 (got $($p.ExitCode))"
    if (-not $IsWindows) {
        $line = ((Get-Content -Raw $outFile -ErrorAction SilentlyContinue) ?? '').Trim()
        Assert ($line -eq "bdinfo-rs-gui $Version") "$what reports 'bdinfo-rs-gui $Version' (got '$line')"
    }
    Remove-Item -Force $outFile -ErrorAction SilentlyContinue
}

# The Add/Remove Programs entries whose DisplayName is exactly the product
# name, searched in both the per-user and the machine hive. A per-user MSI's
# ARP entry lands under HKLM, not HKCU: Windows Installer's ProductRegister
# opcode takes no hive argument, so the hive is not authorable (measured
# 2026-07-19, Windows 11 10.0.26200, WiX 3.14.1). Searching both hives keeps
# the assertion on the contract — exactly one entry, correct strings —
# rather than on that quirk.
function Get-ArpEntry {
    foreach ($hive in 'HKCU:', 'HKLM:') {
        Get-ChildItem "$hive\Software\Microsoft\Windows\CurrentVersion\Uninstall" -ErrorAction SilentlyContinue |
            Where-Object { [string]$_.GetValue('DisplayName') -eq 'bdinfo-rs GUI' }
    }
}

# The stored user PATH (HKCU\Environment), split into entries. Read from the
# registry, unexpanded, rather than from $env:PATH: the installer edits the
# stored value, and a running process's environment never sees that edit.
function Get-UserPathEntry {
    (Get-Item HKCU:\Environment).GetValue('Path', '',
        [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) -split ';' |
        Where-Object { $_ }
}

switch ($Kind) {
    { $_ -in 'windows', 'windows-zip' } {
        # ── portable zip: the exe runs from a plain unpack ───────────────────
        $zip = Join-Path $dir "bdinfo-rs-gui-$Triple.zip"
        $unpack = Join-Path ([System.IO.Path]::GetTempPath()) "gui-smoke-zip-$PID"
        if (Test-Path $unpack) { Remove-Item -Recurse -Force $unpack }
        Expand-Archive -Path $zip -DestinationPath $unpack
        Invoke-VersionProbe (Join-Path $unpack 'bdinfo-rs-gui.exe') 'zip exe'
        Remove-Item -Recurse -Force $unpack

        if ($Kind -eq 'windows') {
            # ── MSI: unattended per-user install, probe, uninstall ───────────
            $msi = Join-Path $dir "bdinfo-rs-gui-$Triple.msi"
            $log = Join-Path ([System.IO.Path]::GetTempPath()) "gui-smoke-msi-$PID.log"
            $proc = Start-Process msiexec -ArgumentList "/i `"$msi`" /qn /l*v `"$log`"" -Wait -PassThru
            Assert ($proc.ExitCode -eq 0) "msiexec /i /qn succeeds (got $($proc.ExitCode))"
            if ($proc.ExitCode -ne 0 -and (Test-Path $log)) {
                Write-Host '--- msiexec install log (tail) ---'
                Get-Content $log -Tail 40 | ForEach-Object { Write-Host "    $_" }
            }
            $installDir = Join-Path $env:LOCALAPPDATA 'Programs\bdinfo-rs GUI'
            $installed = Join-Path $installDir 'bdinfo-rs-gui.exe'
            Assert (Test-Path $installed) "install lands at $installed"
            if (Test-Path $installed) { Invoke-VersionProbe $installed 'installed exe' }

            # winget correlates the installed app to its manifest through
            # these exact strings (AppsAndFeaturesEntries); a drifted
            # Name/Manufacturer/Version in setup.wxs would break upgrade and
            # uninstall correlation for every winget user. Exactly one entry:
            # a second match would mean a stale install leaked in.
            $arp = @(Get-ArpEntry)
            Assert ($arp.Count -eq 1) "exactly one ARP entry named 'bdinfo-rs GUI' (got $($arp.Count))"
            if ($arp.Count -eq 1) {
                $v = [string]$arp[0].GetValue('DisplayVersion')
                $pub = [string]$arp[0].GetValue('Publisher')
                Assert ($v -eq $Version) "ARP DisplayVersion is $Version (got '$v')"
                Assert ($pub -eq 'bdinfo-rs contributors') "ARP Publisher is 'bdinfo-rs contributors' (got '$pub')"
            }
            # The PathEnvironment feature (setup.wxs) sits above the default
            # INSTALLLEVEL, so the silent road winget and unattended installs
            # take must leave the user PATH alone.
            Assert (@(Get-UserPathEntry | Where-Object { $_.TrimEnd('\') -eq $installDir }).Count -eq 0) `
                'default /qn install leaves the user PATH untouched'

            $proc = Start-Process msiexec -ArgumentList "/x `"$msi`" /qn" -Wait -PassThru
            Assert ($proc.ExitCode -eq 0) "msiexec /x /qn succeeds (got $($proc.ExitCode))"
            Assert (-not (Test-Path $installed)) 'uninstall removes the exe'
            Assert (@(Get-ArpEntry).Count -eq 0) 'uninstall removes the ARP entry'

            # ── MSI second pass: the opt-in PATH feature ─────────────────────
            # Feature ids from setup.wxs: Main is the app, PathEnvironment
            # holds the Environment element that appends [INSTALLDIR] to the
            # HKCU PATH (Part="last", Permanent="no" — uninstall must take
            # the entry back out).
            $proc = Start-Process msiexec -ArgumentList "/i `"$msi`" /qn ADDLOCAL=Main,PathEnvironment" -Wait -PassThru
            Assert ($proc.ExitCode -eq 0) "msiexec /i /qn ADDLOCAL=Main,PathEnvironment succeeds (got $($proc.ExitCode))"
            Assert (@(Get-UserPathEntry | Where-Object { $_.TrimEnd('\') -eq $installDir }).Count -eq 1) `
                'ADDLOCAL install appends the install dir to the user PATH'
            $proc = Start-Process msiexec -ArgumentList "/x `"$msi`" /qn" -Wait -PassThru
            Assert ($proc.ExitCode -eq 0) "msiexec /x /qn (PATH pass) succeeds (got $($proc.ExitCode))"
            Assert (@(Get-UserPathEntry | Where-Object { $_.TrimEnd('\') -eq $installDir }).Count -eq 0) `
                'uninstall removes the PATH entry'
            Remove-Item -Force $log -ErrorAction SilentlyContinue
        }
    }

    'linux' {
        # ── .deb: real dpkg install, probe from the installed path ───────────
        $deb = Join-Path $dir "bdinfo-rs-gui-$Triple.deb"
        & sudo dpkg -i $deb
        Assert ($LASTEXITCODE -eq 0) 'dpkg -i succeeds'
        Assert (Test-Path '/usr/bin/bdinfo-rs-gui') 'install lands at /usr/bin/bdinfo-rs-gui'
        if (Test-Path '/usr/bin/bdinfo-rs-gui') { Invoke-VersionProbe '/usr/bin/bdinfo-rs-gui' 'installed binary' }
        & sudo dpkg -r bdinfo-rs-gui
        Assert ($LASTEXITCODE -eq 0) 'dpkg -r succeeds'
        Assert (-not (Test-Path '/usr/bin/bdinfo-rs-gui')) 'removal deletes the binary'

        # ── AppImage: the runtime boots the binary without extraction ────────
        $appImage = Join-Path $dir "bdinfo-rs-gui-$Triple.AppImage"
        & chmod +x $appImage
        $outFile = Join-Path ([System.IO.Path]::GetTempPath()) "gui-smoke-appimage-$PID.txt"
        $p = Start-Process -FilePath $appImage -ArgumentList '--appimage-extract-and-run', '--version' `
            -PassThru -RedirectStandardOutput $outFile
        if ($p.WaitForExit(30000)) {
            Assert ($p.ExitCode -eq 0) "AppImage --version exits 0 (got $($p.ExitCode))"
            $line = ((Get-Content -Raw $outFile -ErrorAction SilentlyContinue) ?? '').Trim()
            Assert ($line -eq "bdinfo-rs-gui $Version") "AppImage reports 'bdinfo-rs-gui $Version' (got '$line')"
        }
        else {
            $p.Kill()
            Assert $false 'AppImage --version exits within 30 s'
        }
        Remove-Item -Force $outFile -ErrorAction SilentlyContinue
    }

    'macos' {
        $dmg = Join-Path $dir "bdinfo-rs-gui-$Triple.dmg"
        $mount = '/Volumes/bdinfo-rs GUI'
        & hdiutil attach $dmg -readonly -nobrowse
        Assert ($LASTEXITCODE -eq 0) 'hdiutil attach succeeds'
        try {
            Invoke-VersionProbe (Join-Path $mount 'bdinfo-rs GUI.app/Contents/MacOS/bdinfo-rs-gui') 'bundle binary'
        }
        finally { & hdiutil detach $mount | Out-Null }
    }
}

if ($fail.Count) {
    Write-Host 'FAILED install smoke:'
    $fail | ForEach-Object { Write-Host "    $_" }
    exit 1
}
Write-Host "install smoke: PASS ($Kind, $Triple)"
