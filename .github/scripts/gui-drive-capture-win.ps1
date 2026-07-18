# Captures one window as a PNG via PrintWindow(PW_RENDERFULLCONTENT) — the
# only capture technique that reads back GPU-composited (wgpu) content on a
# hosted Windows runner. Runs under WINDOWS PowerShell 5.1 (powershell.exe),
# which bundles System.Drawing; PowerShell 7 does not ship it. Invoked by the
# gui-drive harnesses (gui-drive-assisted.ps1 / gui-drive-inject-windows.ps1)
# once per screenshot.
param(
    [Parameter(Mandatory)][long]$Hwnd,
    [Parameter(Mandatory)][string]$Out
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Drawing.dll -TypeDefinition @"
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
public static class GuiCap {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    public static void Save(IntPtr h, string path) {
        RECT r; GetWindowRect(h, out r);
        int w = r.Right - r.Left, ht = r.Bottom - r.Top;
        if (w <= 0 || ht <= 0) throw new Exception("window has no size");
        using (var bmp = new Bitmap(w, ht, PixelFormat.Format32bppArgb))
        using (var g = Graphics.FromImage(bmp)) {
            IntPtr dc = g.GetHdc();
            PrintWindow(h, dc, 0x2); // PW_RENDERFULLCONTENT
            g.ReleaseHdc(dc);
            bmp.Save(path, ImageFormat.Png);
        }
    }
}
"@
[void][GuiCap]::SetProcessDPIAware()
[GuiCap]::Save([IntPtr]$Hwnd, $Out)
