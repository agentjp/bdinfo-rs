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
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int ht, uint flags);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder sb, int max);
    // The top-level window that would receive a click at (x, y).
    public static IntPtr RootAt(int x, int y) {
        POINT p; p.X = x; p.Y = y;
        return GetAncestor(WindowFromPoint(p), 2); // GA_ROOT
    }
    // Who is that window — for the refusal diagnostics.
    public static string Describe(IntPtr h) {
        var t = new System.Text.StringBuilder(256); GetWindowText(h, t, 256);
        var c = new System.Text.StringBuilder(256); GetClassName(h, c, 256);
        return "hwnd=" + h + " class='" + c + "' title='" + t + "'";
    }
    [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h, int idx);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    public static uint PidOf(IntPtr h) { uint pid; GetWindowThreadProcessId(h, out pid); return pid; }
    [DllImport("user32.dll")] public static extern bool SystemParametersInfo(uint action, uint p, ref RECT r, uint ini);
    // Move the window to the work-area top-left and clamp its height to fit,
    // keeping the width (already 880 logical). Hosted runner displays are
    // small (1024x768 with a 48 px taskbar): the default 960-tall window
    // hangs off-screen there and SetCursorPos clamps, so bottom-bar clicks
    // land on the taskbar instead of the app. The walk's y coordinates
    // derive from the measured client height, so a shorter window is fine
    // (the macOS leg already walks at 681).
    public static bool FitToWork(IntPtr h) {
        RECT wa = new RECT();
        if (!SystemParametersInfo(0x30, 0, ref wa, 0)) return false; // SPI_GETWORKAREA
        RECT w; GetWindowRect(h, out w);
        int width = w.Right - w.Left;
        int height = System.Math.Min(w.Bottom - w.Top, wa.Bottom - wa.Top);
        return SetWindowPos(h, IntPtr.Zero, wa.Left, wa.Top, width, height, 0x0014); // SWP_NOZORDER | SWP_NOACTIVATE
    }
    // The desktop composition — every visible top-level with its rect and
    // topmost flag, so a failing run documents exactly what shares the
    // runner desktop with the app instead of leaving it to guesswork.
    public static string[] DumpDesktop() {
        var list = new System.Collections.Generic.List<string>();
        EnumWindows((h, l) => {
            if (!IsWindowVisible(h)) return true;
            RECT r; GetWindowRect(h, out r);
            bool topmost = (GetWindowLong(h, -20) & 0x8) != 0; // GWL_EXSTYLE, WS_EX_TOPMOST
            list.Add(Describe(h) + " rect=" + r.Left + "," + r.Top + "," + (r.Right - r.Left) + "x" + (r.Bottom - r.Top) + (topmost ? " TOPMOST" : ""));
            return true;
        }, IntPtr.Zero);
        return list.ToArray();
    }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    // Pin the app above everything non-topmost, without moving or activating
    // it (HWND_TOPMOST; SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE).
    public static bool MakeTopmost(IntPtr h) {
        return SetWindowPos(h, new IntPtr(-1), 0, 0, 0, 0, 0x0013);
    }
    // The real app window: owned by the pid, visible, titled "bdinfo-rs…".
    // Process.MainWindowHandle is NOT that — winit also creates hidden
    // zero-sized top-level helpers and MainWindowHandle can pick one.
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
    // Clear the deck: hosted Windows runners keep a "Runner Provisioner"
    // window in the foreground (actions/runner#2189) and each console spawn
    // adds a conhost — minimize every OTHER visible titled top-level so no
    // stranger sits over a click point. Throwaway CI desktop, so this is safe.
    public static int MinimizeOthers(IntPtr keep) {
        int n = 0;
        EnumWindows((h, l) => {
            if (h == keep || !IsWindowVisible(h)) return true;
            var sb = new System.Text.StringBuilder(256);
            GetWindowText(h, sb, 256);
            if (sb.Length == 0) return true;
            ShowWindow(h, 6); // SW_MINIMIZE
            n++;
            return true;
        }, IntPtr.Zero);
        return n;
    }
    // The AttachThreadInput technique: adopt the current foreground thread's
    // input state so the foreground lock treats this process as entitled.
    public static bool ForceForeground(IntPtr h) {
        uint tmp;
        uint fgThread = GetWindowThreadProcessId(GetForegroundWindow(), out tmp);
        uint cur = GetCurrentThreadId();
        AttachThreadInput(cur, fgThread, true);
        ShowWindow(h, 9); // SW_RESTORE
        BringWindowToTop(h);
        SetForegroundWindow(h);
        AttachThreadInput(cur, fgThread, false);
        return GetForegroundWindow() == h;
    }
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
    $hwnd = [GuiInput]::FindMain([uint32]$proc.Id, 'bdinfo-rs')
    if ($hwnd -ne [IntPtr]::Zero) { break }
    Start-Sleep -Milliseconds 100
}
if ($hwnd -eq [IntPtr]::Zero) { throw 'window never appeared' }

Write-Host '==> waiting for the structural scan (delay + listing)'
Start-Sleep -Milliseconds ($ScanDelayMs + 7000)

# Client geometry -> the logical->screen transform (client px / 880 = scale).
# Measured only now, after the listing rendered: the handle exists a beat
# before winit applies the requested size (the first CI run read 0x0 and a
# mid-layout 960x480), so poll until the rect is sane and stable.
$rect = New-Object GuiInput+RECT
$prev = 0
for ($i = 0; $i -lt 60; $i++) {
    [void][GuiInput]::GetClientRect($hwnd, [ref]$rect)
    if ($rect.Right -ge 600 -and $rect.Bottom -ge 400 -and ($rect.Right * 100000) + $rect.Bottom -eq $prev) { break }
    $prev = ($rect.Right * 100000) + $rect.Bottom
    Start-Sleep -Milliseconds 500
}
if ($rect.Right -lt 600 -or $rect.Bottom -lt 400) { throw "client rect never settled ($($rect.Right)x$($rect.Bottom))" }
Write-Host '==> desktop composition before the walk:'
[GuiInput]::DumpDesktop() | ForEach-Object { Write-Host "    $_" }
if (-not [GuiInput]::FitToWork($hwnd)) { Write-Host '!! FitToWork failed' }
Start-Sleep -Milliseconds 800 # let the resize lay out before measuring
$minimized = [GuiInput]::MinimizeOthers($hwnd)
Write-Host "==> minimized $minimized other top-level window(s) (runner provisioner etc.)"
# Belt and braces: some overlays (UWP input hosts) refuse SW_MINIMIZE and
# still hit-test above a normal window — pin the app topmost instead.
if (-not [GuiInput]::MakeTopmost($hwnd)) { Write-Host '!! SetWindowPos(HWND_TOPMOST) failed' }

$origin = New-Object GuiInput+POINT
[void][GuiInput]::ClientToScreen($hwnd, [ref]$origin)
$scale = $rect.Right / 880.0
# The logical HEIGHT actually granted (a small runner display can clamp the
# requested 960): the bottom action bar and the centred dialog track it.
$logicalH = $rect.Bottom / $scale
Write-Host ("==> client {0}x{1} at {2},{3} (scale {4}, logical height {5})" -f $rect.Right, $rect.Bottom, $origin.X, $origin.Y, $scale, [int]$logicalH)

function Save-Shot([string]$Name) {
    # -WindowStyle Hidden: a visible conhost from each capture spawn cascades
    # across the desktop and lands under later click points — the hit-test
    # guard below caught exactly that in run 4.
    $cap = Start-Process powershell.exe -WindowStyle Hidden -Wait -PassThru -ArgumentList @(
        '-NoProfile', '-STA', '-ExecutionPolicy', 'Bypass',
        '-File', (Join-Path $PSScriptRoot 'gui-drive-capture-win.ps1'),
        '-Hwnd', $hwnd.ToInt64(), '-Out', (Join-Path $Gallery "$Name.png"))
    if ($cap.ExitCode -ne 0) { Write-Host "!! capture failed for $Name ($($cap.ExitCode))" }
}

function Request-Foreground {
    # Best-effort only — mouse clicks land by CURSOR POSITION, not focus, and
    # Send-Click verifies the window under the cursor per click. Focus is
    # needed only by the keyboard probe. Runs the ALT-tap release plus the
    # AttachThreadInput takeover; hosted runner sessions can still refuse
    # (run 3: both Windows legs held the lock through both techniques).
    for ($i = 0; $i -lt 5; $i++) {
        if ([GuiInput]::GetForegroundWindow() -eq $hwnd) { return $true }
        [void][GuiInput]::Key(0x12, $false) # VK_MENU down
        [void][GuiInput]::Key(0x12, $true)
        if ([GuiInput]::ForceForeground($hwnd)) { return $true }
        Start-Sleep -Milliseconds 400
    }
    return $false
}

function Send-Click([double]$Lx, [double]$Ly) {
    $px = [int][math]::Round($origin.X + ($Lx * $scale))
    $py = [int][math]::Round($origin.Y + ($Ly * $scale))
    [void][GuiInput]::SetCursorPos($px, $py)
    Start-Sleep -Milliseconds 150
    # The precise no-blind-clicks guard: the top-level window that will
    # receive this click must be the app — stronger than a foreground check,
    # because injected mouse input routes by cursor position, not focus.
    # A blocker gets escalated, not worked around: UWP immersive windows (the
    # runner image ships a "Microsoft account" OOBE CoreWindow) sit in a
    # z-band ABOVE HWND_TOPMOST and ignore SW_MINIMIZE, so ask it to close
    # (WM_CLOSE), then kill its process — a throwaway CI desktop, by design.
    $clear = $false
    for ($i = 0; $i -lt 10; $i++) {
        $root = [GuiInput]::RootAt($px, $py)
        if ($root -eq $hwnd) { $clear = $true; break }
        if ($i -eq 2 -and $root -ne [IntPtr]::Zero) {
            Write-Host "==> closing blocker $([GuiInput]::Describe($root))"
            [void][GuiInput]::PostMessage($root, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) # WM_CLOSE
        }
        if ($i -eq 6 -and $root -ne [IntPtr]::Zero) {
            $bpid = [GuiInput]::PidOf($root)
            if ($bpid -ne 0 -and $bpid -ne $proc.Id) {
                Write-Host "==> killing blocker process $bpid ($([GuiInput]::Describe($root)))"
                Stop-Process -Id $bpid -Force -ErrorAction SilentlyContinue
            }
        }
        Start-Sleep -Milliseconds 400
    }
    if (-not $clear) {
        $who = [GuiInput]::Describe([GuiInput]::RootAt($px, $py))
        throw "another window sits under the cursor at ${px},${py} ($who) — refusing to inject blind clicks"
    }
    [void][GuiInput]::Mouse(0x0002) # LEFTDOWN
    Start-Sleep -Milliseconds 80
    [void][GuiInput]::Mouse(0x0004) # LEFTUP
    Start-Sleep -Milliseconds 900   # let the resulting state render
}

# Logical client coordinates of the click targets at the default 880-wide
# layout (dark theme, 100% UI scale) — measured from a pinned-geometry
# capture. Top-anchored rows are fixed offsets; the bottom action bar and the
# centred dialog buttons track the granted logical height (960 requested, but
# a small runner display can clamp it — the macOS probe got 681).
$SelectAll = @(111, 127)
$LengthHeader = @(484, 170)
$FirstRow = @(110, 206)
$SettingsBtn = @(817, ($logicalH - 29))
$DialogCancel = @(538, (($logicalH / 2) + 165))
$ScanBtn = @(566, ($logicalH - 28))

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
# visible effect if the key path works (recorded, not asserted). Keyboard
# routes by FOCUS, so this one genuinely needs the foreground; when the
# runner session refuses to hand it over, record that instead of failing
# the mouse-path verdict.
if (Request-Foreground) {
    [void][GuiInput]::Key(0x11, $false) # VK_CONTROL down
    Start-Sleep -Milliseconds 60
    [void][GuiInput]::Key(0x6B, $false) # VK_ADD down
    Start-Sleep -Milliseconds 60
    [void][GuiInput]::Key(0x6B, $true)
    [void][GuiInput]::Key(0x11, $true)
    Start-Sleep -Milliseconds 900
    Save-Shot '08-scale-step'
}
else {
    Write-Host '!! keyboard probe SKIPPED: the session never granted the app foreground'
}

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
