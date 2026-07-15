#!/usr/bin/env pwsh
# Tripwire guard for the NATIVE GUI crate (crates/bdinfo-rs-gui) — the two
# product-identity rules no clippy lint covers, enforced IDENTICALLY by the local
# gate (scripts/gui-compliance.ps1) and CI (the `checks` job in gui.yml). This
# script is the single source of truth for both, exactly like
# .github/scripts/source-rules.ps1 and check-banned-words.ps1.
#
#   (1) The no-C tripwire. The GUI is pure Rust: the renderer/windowing stack
#       reaches the GPU/display by runtime `dlopen` (NOT an FFI C link), and NO
#       crate compiles or links external C. A name-based `cargo tree` scan over
#       every SHIPPED target enforces it: no GTK/C-GUI crate, no C build crate,
#       every `*-sys`/`*-dl` allowlisted as a known pure-Rust binding,
#       cc/pkg-config present only as verified-inert dlopen build-deps, and
#       rfd's pure-Rust xdg-portal backend (ashpd) POSITIVELY present on the
#       Linux targets (the classic trap is rfd's C-linking `gtk3` backend).
#   (2) The unsafe tripwire. Unlike the wasm crate (which counts a fixed number
#       of unsafe tokens), the GUI stays fully memory-safe: the
#       `#![forbid(unsafe_code)]` attribute must be in both crate roots and
#       there must be ZERO `unsafe` tokens anywhere in src/.
#
# Exits 1 with a list of offenders on any violation, 0 when clean.

[CmdletBinding()]
param(
    # Default: crates/bdinfo-rs-gui relative to this script (.github/scripts/).
    [string] $Crate = [System.IO.Path]::Combine($PSScriptRoot, '..', '..', 'crates', 'bdinfo-rs-gui')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$crate = (Resolve-Path -LiteralPath $Crate).Path
$manifest = Join-Path $crate 'Cargo.toml'

# ── (1) no-C tripwire ────────────────────────────────────────────────────────

$shippedTargets = @(
    'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu',
    'x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc',
    'x86_64-apple-darwin', 'aarch64-apple-darwin'
)

# Pure-Rust OS-ABI bindings + dlopen loaders: a `*-sys`/`*-dl` name is NOT a C
# build. Anything matching `*-sys`/`*-dl` outside this set fails (it might wrap C).
$allowSys = @(
    'windows-sys', 'linux-raw-sys', # OS-ABI bindings (pure Rust)
    'wayland-sys', 'x11-dl', 'xkbcommon-dl', # dlopen loaders (pure Rust)
    'core-foundation-sys', 'objc-sys', # macOS system-lib bindings (always present)
    'renderdoc-sys', 'ndk-sys', 'jni-sys' # header-only / Android-ABI (no C link)
)
# GTK / C-GUI families that mean a real C link — must never appear.
$gtkPattern = 'gtk|glib|gdk|gobject|^gio$|cairo|pango|atk|webkit'
# Build crates that compile/find external C — banned outright (never present).
$cBuild = @('cmake', 'bindgen', 'vcpkg')
# cc + pkg-config DO appear on Linux as INERT build-deps of the dlopen stack
# (verified: wayland-backend builds C only under its OFF `log` feature; wayland-sys
# /khronos-egl skip pkg-config under dlopen/dynamic; tiny-xlib's probe is non-fatal).
# Allowed ONLY when every direct dependent is one of these — a new source fails.
$inertParents = @{
    'cc'         = @('wayland-backend', 'android-activity')
    'pkg-config' = @('wayland-sys', 'khronos-egl', 'tiny-xlib', 'x11-dl')
}

# Flat, deduped crate names (normal + build deps) for a target.
function Get-CrateNames([string] $triple) {
    $lines = & cargo tree --manifest-path $manifest --target $triple --edges normal,build --prefix none 2>$null
    @($lines | ForEach-Object { (($_ -replace '^[├└│─\s]+', '') -split '\s+')[0] } |
        Where-Object { $_ } | Sort-Object -Unique)
}

# The crates that directly depend on $name (the inverted tree at depth 1, minus
# $name itself) for a target.
function Get-DirectDependents([string] $name, [string] $triple) {
    $lines = & cargo tree --manifest-path $manifest -i $name --target $triple `
        --edges normal,build --depth 1 --prefix none 2>$null
    @($lines | ForEach-Object { (($_ -replace '^[├└│─\s]+', '') -split '\s+')[0] } |
        Where-Object { $_ -and $_ -ne $name } | Sort-Object -Unique)
}

$ncFail = @()
$ncSeen = [System.Collections.Generic.HashSet[string]]::new()
foreach ($triple in $shippedTargets) {
    $names = Get-CrateNames $triple
    if (-not $names.Count) {
        $ncFail += "[$triple] cargo tree produced no crates"
        continue
    }
    # The POSITIVE backend proof: on Linux the folder picker must be rfd's
    # pure-Rust xdg-portal road (ashpd over D-Bus) — if ashpd vanishes, rfd's
    # backend changed and the gtk trap is one feature-flag away.
    if ($triple -like '*linux*' -and $names -notcontains 'ashpd') {
        $ncFail += "[$triple] rfd's xdg-portal backend (ashpd) is missing — the picker backend changed"
    }
    foreach ($name in $names) {
        if ($cBuild -contains $name) {
            $ncFail += "[$triple] banned C-build crate '$name'"
        }
        elseif ($name -match $gtkPattern) {
            $ncFail += "[$triple] GTK / C-GUI crate '$name' (rfd must stay on xdg-portal)"
        }
        elseif ($inertParents.ContainsKey($name)) {
            $parents = Get-DirectDependents $name $triple
            $unexpected = @($parents | Where-Object { $inertParents[$name] -notcontains $_ })
            if ($unexpected.Count) {
                $ncFail += "[$triple] '$name' pulled by an unexpected dependent: $($unexpected -join ', ')"
            }
            elseif ($ncSeen.Add($name)) {
                Write-Host "    note: '$name' present via $($parents -join ', ') — inert (dlopen build-dep, no C compiled)"
            }
        }
        elseif ($name -match '(-sys|-dl)$' -and $allowSys -notcontains $name) {
            $ncFail += "[$triple] non-allowlisted '*-sys'/'*-dl' crate '$name'"
        }
    }
}
if ($ncFail.Count) {
    Write-Host 'FAILED: the no-C tripwire found C / GTK / unknown -sys crates:'
    $ncFail | Sort-Object -Unique | ForEach-Object { Write-Host "    $_" }
    exit 1
}
Write-Host 'ok: no GTK/C-build crate; every -sys/-dl allowlisted; cc/pkg-config inert; ashpd present on Linux'

# ── (2) forbid(unsafe_code) tripwire ─────────────────────────────────────────
# Strip // comments first (prose may say "unsafe"), then count the keyword;
# `\bunsafe\b` also won't match `unsafe_code`.

foreach ($root in @('src/lib.rs', 'src/main.rs')) {
    $text = Get-Content -LiteralPath (Join-Path $crate $root) -Raw
    if ($text -notmatch '#!\[forbid\(unsafe_code\)\]') {
        Write-Host "FAILED: $root is missing #![forbid(unsafe_code)]"
        exit 1
    }
}
$unsafeTotal = 0
foreach ($file in @(Get-ChildItem -LiteralPath (Join-Path $crate 'src') -Recurse -Filter '*.rs')) {
    $code = ((Get-Content -LiteralPath $file.FullName) -replace '//.*$', '') -join "`n"
    $unsafeTotal += ([regex]::Matches($code, '\bunsafe\b')).Count
}
if ($unsafeTotal -ne 0) {
    Write-Host "FAILED: found $unsafeTotal 'unsafe' token(s) in src/ (must be 0)"
    exit 1
}
Write-Host 'ok: forbid(unsafe_code) in both crate roots; 0 unsafe tokens in src/'
exit 0
