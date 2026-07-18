# Captures one window as a PNG via PrintWindow(PW_RENDERFULLCONTENT) — the
# only capture technique that reads back GPU-composited (wgpu) content on a
# hosted Windows runner. Runs under WINDOWS PowerShell 5.1 (powershell.exe),
# which bundles System.Drawing; PowerShell 7 does not ship it. Invoked by the
# gui-drive harnesses (gui-drive-assisted.ps1 / gui-drive-inject-windows.ps1)
# once per screenshot.
#
# Windows 11 includes an INVISIBLE resize border in GetWindowRect that wgpu
# never paints, so a full-window PrintWindow renders it as a black frame on
# the left/right/bottom edges. The fix crops using the CLIENT geometry (exact
# at any DPI): keep the title bar (from the window top) but trim the sides and
# bottom to the client width/height — precisely the region the app paints — so
# no black frame survives.
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
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    public static void Save(IntPtr h, string path) {
        RECT wr; GetWindowRect(h, out wr);
        int w = wr.Right - wr.Left, ht = wr.Bottom - wr.Top;
        if (w <= 0 || ht <= 0) throw new Exception("window has no size");
        // Client geometry -> the exact painted region within the window.
        RECT cr; GetClientRect(h, out cr);
        POINT co; co.X = 0; co.Y = 0; ClientToScreen(h, ref co);
        int cx = co.X - wr.Left;          // left border width
        int cw = cr.Right, chh = cr.Bottom; // client width / height
        int cropW = cw;                    // sides trimmed to the client width
        int cropH = (co.Y - wr.Top) + chh; // title bar kept, bottom border cut
        bool ok = cw > 0 && chh > 0 && cx >= 0 && cx + cropW <= w && cropH > 0 && cropH <= ht;
        using (var bmp = new Bitmap(w, ht, PixelFormat.Format32bppArgb))
        using (var g = Graphics.FromImage(bmp)) {
            IntPtr dc = g.GetHdc();
            PrintWindow(h, dc, 0x2); // PW_RENDERFULLCONTENT
            g.ReleaseHdc(dc);
            if (ok) {
                using (var crop = new Bitmap(cropW, cropH, PixelFormat.Format32bppArgb))
                using (var gc = Graphics.FromImage(crop)) {
                    gc.DrawImage(bmp, new Rectangle(0, 0, cropW, cropH), new Rectangle(cx, 0, cropW, cropH), GraphicsUnit.Pixel);
                    crop.Save(path, ImageFormat.Png);
                }
            }
            else { bmp.Save(path, ImageFormat.Png); }
        }
    }
}
"@
[void][GuiCap]::SetProcessDPIAware()
[GuiCap]::Save([IntPtr]$Hwnd, $Out)
