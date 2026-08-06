#!/usr/bin/env pwsh
# The AUR prepare leg of gui-publish.yml (bdinfo-rs-gui-bin): render
# packaging/aur/PKGBUILD-gui.template and prove it valid before anything
# touches the AUR. The version and both per-arch sha256 values come from the
# release's verified SHA256SUMS (-Sums), so the PKGBUILD pins the exact
# attested bytes rather than trusting a later download. Validation runs in an
# Arch container (the only place makepkg exists):
#
#   makepkg --printsrcinfo   parses the PKGBUILD and regenerates .SRCINFO,
#                            which ships in the payload for the publish leg;
#   makepkg --verifysource   downloads the host-arch .deb from the release
#                            and checks it against the pinned hash.
#
# The publish half has no script: KSXGitHub/github-actions-deploy-aur takes
# the prepared PKGBUILD, regenerates .SRCINFO itself, re-derives the
# checksums (updpkgsums), test-builds in its own container, and only then
# pushes — so a bad payload still cannot reach the AUR.

[CmdletBinding()]
param(
    # The bare X.Y.Z version being published.
    [Parameter(Mandatory)] [string] $Version,
    # Path to the release's verified SHA256SUMS.
    [Parameter(Mandatory)] [string] $Sums,
    # Directory the rendered PKGBUILD + .SRCINFO are staged in.
    [Parameter(Mandatory)] [string] $Payload
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

. "$PSScriptRoot/_common.ps1"

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..' '..')).Path

$shaByName = Get-Sha256Sums -Path $Sums
$shaX64 = $shaByName['bdinfo-rs-gui-x86_64-unknown-linux-gnu.deb']
$shaArm64 = $shaByName['bdinfo-rs-gui-aarch64-unknown-linux-gnu.deb']
if (-not $shaX64 -or -not $shaArm64) { Stop-Leg 'SHA256SUMS carries no entry for one of the two .deb assets' }

$pkgbuild = Expand-Template -Path (Join-Path $repoRoot 'packaging/aur/PKGBUILD-gui.template') -Values @{
    '__PKGVER__'          = $Version
    '__SHA256_X86_64__'   = $shaX64
    '__SHA256_AARCH64__'  = $shaArm64
}

New-Item -ItemType Directory -Force $Payload | Out-Null
$payloadDir = (Resolve-Path -LiteralPath $Payload).Path
# LF + no BOM: the file is executed by bash inside the containers.
[System.IO.File]::WriteAllText((Join-Path $payloadDir 'PKGBUILD'), $pkgbuild.Replace("`r`n", "`n"))
Write-Host $pkgbuild

# makepkg refuses to run as root, hence the builder user inside the container.
$script = @'
set -eu
useradd -m builder
cp /work/PKGBUILD /home/builder/PKGBUILD
cd /home/builder
su builder -c 'makepkg --printsrcinfo > .SRCINFO'
su builder -c 'makepkg --verifysource'
cp .SRCINFO /work/.SRCINFO
'@
$scriptPath = Join-Path $payloadDir 'container.sh'
[System.IO.File]::WriteAllText($scriptPath, $script.Replace("`r`n", "`n"))
docker run --rm -v "${payloadDir}:/work" archlinux:base-devel bash /work/container.sh
$dockerCode = $LASTEXITCODE
Remove-Item -LiteralPath $scriptPath
if ($dockerCode -ne 0) { Stop-Leg "PKGBUILD validation in the Arch container failed (exit $dockerCode)" }

$srcinfo = Get-Content -LiteralPath (Join-Path $payloadDir '.SRCINFO') -Raw
Write-Host $srcinfo
if ($srcinfo -notmatch "(?m)^\s*pkgver = $([regex]::Escape($Version))\s*$") { Stop-Leg '.SRCINFO does not carry the expected pkgver' }
foreach ($sha in $shaX64, $shaArm64) {
    if ($srcinfo -notmatch [regex]::Escape($sha)) { Stop-Leg '.SRCINFO does not carry one of the pinned .deb hashes' }
}
Write-Host "prepared + validated the AUR payload for bdinfo-rs-gui-bin $Version"
