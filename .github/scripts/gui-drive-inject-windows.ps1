#!/usr/bin/env pwsh
# Strategy (a) of the GUI driving experiment (gui-drive.yml), Windows leg:
# OS-LEVEL INPUT INJECTION — real SendInput cursor/keyboard events through the
# real windowing stack, the truest end-to-end test of the input path. The
# window is launched at its default pinned geometry (880x960 logical, dark
# theme, isolated config) so the click targets sit at fixed LOGICAL client
# coordinates, scaled at run time by the measured client width (client px /
# 880) and offset by ClientToScreen — DPI-proof without pixel hunting.
#
# The walk: select all → sort → activate a row → Settings open/Cancel → start
# the measured scan → completion modal → Ctrl+'+' UI-scale step (the keyboard
# probe with a visible effect). PrintWindow screenshots after every event are
# the evidence; one hard automated assert (the Settings click must change the
# rendered pixels vs the shot before it) proves input actually reached the
# app. The app exits by the BDINFO_GUI_SMOKE_MS deadline — exit 0 means the
# event loop survived the whole injected walk.
#
# Every click re-verifies GetForegroundWindow() is still the app first —
# injected input lands wherever the OS says focus is, never click blind.
param(
    [Parameter(Mandatory)][string]$Exe,
    [Parameter(Mandatory)][string]$Disc,
    [Parameter(Mandatory)][string]$Gallery,
    # Both scans stall this long (ms): holds Scanning up for its shot and
    # leaves a wide window between launch and the first click.
    [int]$ScanDelayMs = 5000,
    # The app self-exits this long (ms) after boot — must outlast the walk.
    [int]$SmokeMs = 150000
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class GuiInput {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("user32.dll", SetLastError = true)] public static extern uint SendInput(uint n, INPUT[] inputs, int size);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk, wScan; public uint dwFlags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Explicit)] public struct UNION {
        [FieldOffset(0)] public MOUSEINPUT mi;
        [FieldOffset(0)] public KEYBDINPUT ki;
    }
    [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public UNION U; }
    public static uint Mouse(uint flags) {
        var inp = new INPUT[1];
        inp[0].type = 0; inp[0].U.mi.dwFlags = flags;
        return SendInput(1, inp, Marshal.SizeOf(typeof(INPUT)));
    }
    public static uint Key(ushort vk, bool up) {
        var inp = new INPUT[1];
        inp[0].type = 1; inp[0].U.ki.wVk = vk; inp[0].U.ki.dwFlags = up ? 2u : 0u;
        return SendInput(1, inp, Marshal.SizeOf(typeof(INPUT)));
    }
}
"@
[void][GuiInput]::SetProcessDPIAware()

$tempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$configDir = Join-Path $tempBase "gui-inject-config-$PID"
Remove-Item -Recurse -Force $configDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $configDir, $Gallery | Out-Null
$env:APPDATA = $configDir
$env:BDINFO_GUI_OPEN = (Resolve-Path $Disc).Path
$env:BDINFO_GUI_THEME = 'dark'
$env:BDINFO_GUI_SCAN_DELAY = "$ScanDelayMs"
$env:BDINFO_GUI_SMOKE_MS = "$SmokeMs"

Write-Host "==> launch $Exe"
$proc = Start-Process -FilePath (Resolve-Path $Exe).Path -PassThru
$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 300; $i++) {
    $proc.Refresh()
    if ($proc.HasExited) { throw "app exited before showing a window ($($proc.ExitCode))" }
    if ($proc.MainWindowHandle -ne [IntPtr]::Zero) { $hwnd = $proc.MainWindowHandle; break }
    Start-Sleep -Milliseconds 100
}
if ($hwnd -eq [IntPtr]::Zero) { throw 'window never appeared' }

# Client geometry -> the logical->screen transform (client px / 880 = scale).
$rect = New-Object GuiInput+RECT
[void][GuiInput]::GetClientRect($hwnd, [ref]$rect)
$origin = New-Object GuiInput+POINT
[void][GuiInput]::ClientToScreen($hwnd, [ref]$origin)
$scale = $rect.Right / 880.0
Write-Host ("==> client {0}x{1} at {2},{3} (scale {4})" -f $rect.Right, $rect.Bottom, $origin.X, $origin.Y, $scale)

function Save-Shot([string]$Name) {
    & powershell.exe -NoProfile -STA -ExecutionPolicy Bypass `
        -File (Join-Path $PSScriptRoot 'gui-drive-capture-win.ps1') -Hwnd $hwnd.ToInt64() -Out (Join-Path $Gallery "$Name.png")
    if ($LASTEXITCODE -ne 0) { Write-Host "!! capture failed for $Name" }
}

function Assert-Foreground {
    if ([GuiInput]::GetForegroundWindow() -eq $hwnd) { return }
    [void][GuiInput]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 400
    if ([GuiInput]::GetForegroundWindow() -ne $hwnd) {
        throw 'the app is not the foreground window — refusing to inject blind clicks'
    }
}

function Send-Click([double]$Lx, [double]$Ly) {
    Assert-Foreground
    $px = [int][math]::Round($origin.X + ($Lx * $scale))
    $py = [int][math]::Round($origin.Y + ($Ly * $scale))
    [void][GuiInput]::SetCursorPos($px, $py)
    Start-Sleep -Milliseconds 150
    [void][GuiInput]::Mouse(0x0002) # LEFTDOWN
    Start-Sleep -Milliseconds 80
    [void][GuiInput]::Mouse(0x0004) # LEFTUP
    Start-Sleep -Milliseconds 900   # let the resulting state render
}

# Logical client coordinates of the click targets at the default 880x960
# layout (dark theme, 100% UI scale) — measured from a pinned-geometry
# capture; the layout is deterministic at a fixed size, so they hold on any
# 100%-DPI runner and rescale with $scale elsewhere.
$SelectAll = @(111, 127)
$LengthHeader = @(484, 170)
$FirstRow = @(110, 206)
$SettingsBtn = @(817, 931)
$DialogCancel = @(538, 645)
$ScanBtn = @(566, 932)

Write-Host '==> waiting for the structural scan (delay + listing)'
Start-Sleep -Milliseconds ($ScanDelayMs + 7000)
Save-Shot '00-listed'

Send-Click @SelectAll; Save-Shot '01-select-all'
Send-Click @LengthHeader; Save-Shot '02-sort-length'
Send-Click @FirstRow; Save-Shot '03-row-activate'
Send-Click @SettingsBtn; Save-Shot '04-settings-open'
Send-Click @DialogCancel; Save-Shot '05-settings-cancel'
Send-Click @ScanBtn
Start-Sleep -Milliseconds 1500
Save-Shot '06-scanning'
Write-Host '==> waiting for the measured scan'
Start-Sleep -Milliseconds ($ScanDelayMs + 25000)
Save-Shot '07-scanned'

# Keyboard probe: Ctrl + numpad-plus steps the UI scale — a whole-window
# visible effect if the key path works (recorded, not asserted).
Assert-Foreground
[void][GuiInput]::Key(0x11, $false) # VK_CONTROL down
Start-Sleep -Milliseconds 60
[void][GuiInput]::Key(0x6B, $false) # VK_ADD down
Start-Sleep -Milliseconds 60
[void][GuiInput]::Key(0x6B, $true)
[void][GuiInput]::Key(0x11, $true)
Start-Sleep -Milliseconds 900
Save-Shot '08-scale-step'

# Hard assert that injection reached the app: the Settings dialog shot must
# render different pixels from the shot before the click. Nothing else on the
# window animates between the two, so identical bytes = the click was lost.
$before = Get-FileHash (Join-Path $Gallery '03-row-activate.png') -ErrorAction SilentlyContinue
$after = Get-FileHash (Join-Path $Gallery '04-settings-open.png') -ErrorAction SilentlyContinue
$landed = $before -and $after -and ($before.Hash -ne $after.Hash)
Write-Host "==> settings-click pixel change: $landed"

Write-Host '==> waiting for the smoke-deadline exit'
$exited = $proc.WaitForExit($SmokeMs)
if (-not $exited) { Stop-Process -Id $proc.Id -Force; throw 'the app never hit its smoke deadline' }
Write-Host ("==> app exit code {0}" -f $proc.ExitCode)
if ($proc.ExitCode -ne 0 -or -not $landed) {
    Write-Host '!! injection walk FAILED (the uploaded gallery holds whatever was captured)'
    exit 1
}
Write-Host '==> injection walk complete'
