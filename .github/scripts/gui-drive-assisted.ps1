#!/usr/bin/env pwsh
# Strategy (b) of the GUI driving experiment (gui-drive.yml): APP-ASSISTED
# driving. The debug-only BDINFO_GUI_DRIVE seam walks the real window through a
# fixed click-everything sequence (sort → splitter → rows → Settings → scan →
# cancel → rescan → report → copy → save), dropping an `NN-name.marker` file
# after each step has rendered; this harness watches the marker directory and
# screenshots the live window on every marker — a per-step gallery of the real
# window on a real display stack, with the OS input layer deliberately out of
# the loop (that is the injection harness's job).
#
# Per-OS capture: PrintWindow(PW_RENDERFULLCONTENT) under Windows PowerShell
# 5.1 on Windows, ImageMagick `import -window root` on Linux (run the whole
# harness under `xvfb-run` so DISPLAY reaches both the app and the capture),
# `screencapture -x` on macOS.
#
# Success = the walk completed (its terminal `done` marker exists, no FAILED
# marker), the app exited 0, and the walk's saved report landed on disk. The
# gallery uploads either way — on failure the partial gallery IS the evidence.
param(
    # The debug GUI binary (the env hooks are compiled out of release builds).
    [Parameter(Mandatory)][string]$Exe,
    # The disc folder to auto-open (the committed fixture in CI).
    [Parameter(Mandatory)][string]$Disc,
    # Where the screenshots + walk evidence land (uploaded as the gallery).
    [Parameter(Mandatory)][string]$Gallery,
    # Per-step capture window, ms. Windows needs headroom for the per-shot
    # powershell.exe + Add-Type spin-up (~3-4 s); import/screencapture are fast.
    [int]$DriveMs = $(if ($IsWindows) { 6000 } else { 2500 }),
    # Artificial scan delay, ms — holds the Scanning states up long enough to
    # capture and gives the walk's Cancel step a window to land in. MUST
    # comfortably exceed one capture window: the Cancel step arrives one
    # settle + one window after scan-start, and a delay equal to the window
    # loses that race (the first Windows CI run proved it). 0 = derive.
    [int]$ScanDelayMs = 0,
    # Overall deadline for the walk, seconds.
    [int]$TimeoutSec = 900
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($ScanDelayMs -le 0) { $ScanDelayMs = [int][math]::Max(12000, (2 * $DriveMs) + 4000) }

$tempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$driveDir = Join-Path $tempBase "gui-drive-markers-$PID"
$configDir = Join-Path $tempBase "gui-drive-config-$PID"
Remove-Item -Recurse -Force $driveDir, $configDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $driveDir, $configDir, $Gallery | Out-Null

# Isolate the persisted config so the window opens at a known geometry with
# default settings, never a runner-cached one.
if ($IsWindows) { $env:APPDATA = $configDir }
elseif ($IsMacOS) { $env:HOME = $configDir }
else { $env:XDG_CONFIG_HOME = $configDir }

$env:BDINFO_GUI_OPEN = (Resolve-Path $Disc).Path
$env:BDINFO_GUI_THEME = 'dark'
$env:BDINFO_GUI_SCAN_DELAY = "$ScanDelayMs"
$env:BDINFO_GUI_DRIVE = $driveDir
$env:BDINFO_GUI_DRIVE_MS = "$DriveMs"
# The SHARED window geometry across every driving leg (assisted + inject, all
# six arches): the app's natural 880x960 design size at the desktop origin, so
# the galleries are directly comparable. Every desktop is raised to 1920x1080
# (xvfb in gui-drive.yml; the Windows/mac harness blocks below) so this window
# always fits, and every shot is CROPPED to the window — no desktop margin,
# identical framing on every OS.
$env:BDINFO_GUI_WIN = '880x960+0+0'

# Raise the desktop to 1920x1080 BEFORE launch so the window places on the full
# canvas (Linux xvfb is already sized by gui-drive.yml). Best-effort: a display
# that rejects the mode just stays as-is and the window/crop still work.
if ($IsWindows) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class GuiRes {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
    public struct DEVMODE {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
        public short dmSpecVersion, dmDriverVersion, dmSize, dmDriverExtra;
        public int dmFields, dmPositionX, dmPositionY, dmDisplayOrientation, dmDisplayFixedOutput;
        public short dmColor, dmDuplex, dmYResolution, dmTTOption, dmCollate;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
        public short dmLogPixels;
        public int dmBitsPerPel, dmPelsWidth, dmPelsHeight, dmDisplayFlags, dmDisplayFrequency;
        public int dmICMMethod, dmICMIntent, dmMediaType, dmDitherType, dmReserved1, dmReserved2, dmPanningWidth, dmPanningHeight;
    }
    [DllImport("user32.dll")] public static extern int ChangeDisplaySettings(ref DEVMODE dm, int flags);
    // Try each mode until one is accepted (DISP_CHANGE_SUCCESSFUL = 0); the
    // hosted virtual display may not offer 1080p. Returns the mode that took.
    public static string Fit(int minW, int minH) {
        int[][] modes = { new[]{1920,1080}, new[]{1600,1200}, new[]{1440,1080}, new[]{1280,1024} };
        foreach (var m in modes) {
            if (m[0] < minW || m[1] < minH) continue;
            DEVMODE dm = new DEVMODE();
            dm.dmSize = (short)Marshal.SizeOf(typeof(DEVMODE));
            dm.dmPelsWidth = m[0]; dm.dmPelsHeight = m[1]; dm.dmBitsPerPel = 32;
            dm.dmFields = 0x40000 | 0x80000 | 0x100000; // BITSPERPEL|PELSWIDTH|PELSHEIGHT
            if (ChangeDisplaySettings(ref dm, 0) == 0) return m[0] + "x" + m[1];
        }
        return "unchanged";
    }
}
"@
    Write-Host "==> Windows display -> $([GuiRes]::Fit(1280, 1040))"
}
elseif ($IsMacOS) {
    $disp = (& displayplacer list 2>$null | Select-String 'Persistent screen id:\s*(\S+)' | Select-Object -First 1).Matches.Groups[1].Value
    if ($disp) {
        & displayplacer "id:$disp res:1920x1080" 2>$null
        Write-Host "==> macOS display id $disp -> 1920x1080 (exit $LASTEXITCODE; best-effort)"
    }
    # Grant/dismiss the Sequoia screen-capture consent NOW, on the empty
    # desktop before the app exists — so pressing the dialog's default button
    # (Return) can never reach the app. A warm-up capture raises it; we click
    # Allow via AX and also press Return, since the dialog may be
    # WindowServer-drawn (not AX-reachable). Clicking Allow grants the
    # permission for the session, so no dialog pollutes the gallery.
    & screencapture -x (Join-Path ([IO.Path]::GetTempPath()) 'gui-drive-prime.png') 2>$null
    Start-Sleep -Milliseconds 2500
    & osascript -e 'tell application "System Events"
        repeat with p in every process
            try
                if exists (button "Allow" of window 1 of p) then click button "Allow" of window 1 of p
            end try
        end repeat
        key code 36
    end tell' 2>$null
    Start-Sleep -Milliseconds 1200
    Write-Host '==> macOS screen-capture consent primed (Allow + Return on empty desktop)'
}

Write-Host "==> launch $Exe (drive markers: $driveDir)"
$proc = Start-Process -FilePath (Resolve-Path $Exe).Path -PassThru

# On Windows the capture needs the window handle; elsewhere the virtual /
# runner display is captured whole (the app is the only window on it).
# Found by pid + visibility + title: Process.MainWindowHandle can pick one of
# winit's hidden zero-sized helper top-levels instead of the real window.
$hwnd = 0
if ($IsWindows) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class GuiFind {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder sb, int max);
    public static IntPtr FindMain(uint pid, string prefix) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((h, l) => {
            uint wpid; GetWindowThreadProcessId(h, out wpid);
            if (wpid != pid || !IsWindowVisible(h)) return true;
            var sb = new System.Text.StringBuilder(256);
            GetWindowText(h, sb, 256);
            if (!sb.ToString().StartsWith(prefix)) return true;
            found = h; return false;
        }, IntPtr.Zero);
        return found;
    }
}
"@
    for ($i = 0; $i -lt 300; $i++) {
        $proc.Refresh()
        if ($proc.HasExited) { break }
        $found = [GuiFind]::FindMain([uint32]$proc.Id, 'bdinfo-rs')
        if ($found -ne [IntPtr]::Zero) { $hwnd = $found.ToInt64(); break }
        Start-Sleep -Milliseconds 100
    }
    if ($hwnd -eq 0 -and -not $proc.HasExited) { Write-Host '!! window handle never appeared' }
}

# Linux: the X11 window id, so shots crop to the app (not the whole 1920x1080
# root). --sync blocks until the window exists.
$xwid = $null
if ($IsLinux) {
    $xwid = & xdotool search --sync --onlyvisible --name '^bdinfo-rs' 2>$null | Select-Object -First 1
    if ($xwid) { Write-Host "==> Linux window id: $xwid" } else { Write-Host '!! xdotool never found the window' }
}

# macOS: read the window rect once so every shot crops to the app (the consent
# dialog was already granted+dismissed pre-launch above).
$macRect = $null
if ($IsMacOS) {
    $geom = & osascript -e 'tell application "System Events" to get {position, size} of window 1 of (first process whose name is "bdinfo-rs-gui")' 2>$null
    if ($LASTEXITCODE -eq 0 -and $geom) {
        $p = @("$geom" -split ',\s*')
        $macRect = "$([int]$p[0]),$([int]$p[1]),$([int]$p[2]),$([int]$p[3])"
        Write-Host "==> macOS window rect: $macRect"
    }
}

function Save-Shot([string]$Name) {
    $png = Join-Path $Gallery "$Name.png"
    try {
        if ($IsWindows) {
            & powershell.exe -NoProfile -STA -ExecutionPolicy Bypass `
                -File (Join-Path $PSScriptRoot 'gui-drive-capture-win.ps1') -Hwnd $hwnd -Out $png
            if ($LASTEXITCODE -ne 0) { Write-Host "!! capture failed for $Name ($LASTEXITCODE)" }
        }
        elseif ($IsMacOS) {
            # Crop to the window rect (comparable to the other OSes' window
            # captures); fall back to the full screen if the rect is unknown.
            if ($macRect) { & screencapture -x -R $macRect $png } else { & screencapture -x $png }
        }
        elseif ($xwid) { & import -window $xwid $png }
        else { & import -window root $png }
    }
    catch { Write-Host "!! capture failed for ${Name}: $_" }
}

# Watch the marker directory: every new marker is a rendered state guaranteed
# to stay up for the capture window — screenshot it under the marker's name.
$seen = @{}
$failed = $false
$deadline = (Get-Date).AddSeconds($TimeoutSec)
while ($true) {
    foreach ($marker in Get-ChildItem $driveDir -Filter '*.marker' -ErrorAction SilentlyContinue | Sort-Object Name) {
        if ($seen.ContainsKey($marker.Name)) { continue }
        $seen[$marker.Name] = $true
        $stage = Get-Content $marker.FullName -Raw -ErrorAction SilentlyContinue
        Write-Host ("==> {0}  (stage: {1})" -f $marker.BaseName, $stage)
        if ($marker.Name -match 'FAILED') { $failed = $true }
        Save-Shot $marker.BaseName
    }
    $proc.Refresh()
    if ($proc.HasExited) { break }
    if ((Get-Date) -gt $deadline) {
        Write-Host "!! deadline: the walk did not finish in $TimeoutSec s"
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        $failed = $true
        break
    }
    Start-Sleep -Milliseconds 250
}
$proc.WaitForExit()

# Preserve the walk's on-disk evidence alongside the shots, plus the app's
# per-launch diagnostics log (it names the debug hooks the boot saw).
Get-ChildItem $driveDir -ErrorAction SilentlyContinue |
    Copy-Item -Destination $Gallery -Force -ErrorAction SilentlyContinue
Get-ChildItem $configDir -Recurse -Filter '*.log' -ErrorAction SilentlyContinue |
    Copy-Item -Destination $Gallery -Force -ErrorAction SilentlyContinue

# Verdict: walk complete + clean exit + the saved report actually landed.
$done = Test-Path (Join-Path $driveDir '*-done.marker')
$report = @(Get-ChildItem $driveDir -Filter 'BDINFO.*.txt' -ErrorAction SilentlyContinue)
$shots = @(Get-ChildItem $Gallery -Filter '*.png' -ErrorAction SilentlyContinue)
Write-Host ("==> exit={0} markers={1} shots={2} done={3} failed={4} report={5}" -f `
        $proc.ExitCode, $seen.Count, $shots.Count, $done, $failed, $report.Count)
if ($failed -or -not $done -or $proc.ExitCode -ne 0 -or $report.Count -eq 0) {
    Write-Host '!! assisted walk FAILED (the uploaded gallery holds whatever was captured)'
    exit 1
}
Write-Host '==> assisted walk complete'
