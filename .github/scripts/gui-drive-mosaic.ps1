#!/usr/bin/env pwsh
# Composes the per-run drive mosaic (gui.yml, drive-mosaic): every leg's
# 00-listed.png at its NATIVE resolution in a 2-row (assisted / inject) x
# 5-column (arch) grid, each cell labelled with its job name — one image that
# shows all ten rendered windows at a glance instead of ten gallery artifacts.
# No downscaling: the result is ~5*880 px wide so the screenshots stay real
# and legible. System.Drawing (GDI+), so this runs on a WINDOWS runner — which
# also provides the true Segoe UI / Consolas faces the layout uses. A leg
# with no shot (a red or partial soft leg) renders as a labelled dark tile;
# only zero shots overall is an error — by then the legs themselves have
# already reddened the gate.
param(
    [Parameter(Mandatory)][string]$Root,   # dir holding the gui-drive-* gallery subdirs
    [Parameter(Mandatory)][string]$Out
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# Keep the column axis in lockstep with the drive job's matrix in gui.yml, and
# the row axis with the halves of its two gallery artifact names.
$arches = 'ubuntu-latest', 'ubuntu-24.04-arm', 'windows-2025', 'windows-11-arm', 'macos-15'
$rows = 'assisted', 'inject'

# Measure all shots first to size the uniform cell (max native w/h).
$maxW = 0; $maxH = 0
$paths = @{}
foreach ($r in $rows) {
    foreach ($a in $arches) {
        $p = Join-Path $Root "gui-drive-$r-$a" '00-listed.png'
        $paths["$r-$a"] = $p
        if (Test-Path $p) {
            $im = [System.Drawing.Image]::FromFile($p)
            if ($im.Width -gt $maxW) { $maxW = $im.Width }
            if ($im.Height -gt $maxH) { $maxH = $im.Height }
            $im.Dispose()
        }
    }
}
if ($maxW -eq 0) { throw 'no 00-listed.png found under any gallery' }

$hdr = 40; $pad = 16; $titleStrip = 64
$cellW = $maxW; $cellH = $hdr + $maxH
$cols = $arches.Count; $rc = $rows.Count
$W = $pad + ($cellW + $pad) * $cols
$H = $titleStrip + $pad + ($cellH + $pad) * $rc

$bg = [System.Drawing.Color]::FromArgb(255, 12, 16, 22)
$hdrBg = [System.Drawing.Color]::FromArgb(255, 232, 122, 46)
$canvas = New-Object System.Drawing.Bitmap($W, $H)
$g = [System.Drawing.Graphics]::FromImage($canvas)
$g.SmoothingMode = 'AntiAlias'
$g.Clear($bg)

$titleFont = New-Object System.Drawing.Font('Segoe UI', 26, [System.Drawing.FontStyle]::Bold)
$hdrFont = New-Object System.Drawing.Font('Consolas', 15, [System.Drawing.FontStyle]::Bold)
$white = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::White)
$dark = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 20, 12, 4))
$hdrBrush = New-Object System.Drawing.SolidBrush($hdrBg)
$missBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 40, 20, 20))

$g.DrawString('bdinfo-rs GUI driven on 10 CI runners - real windows, real input, byte-clean captures',
    $titleFont, $white, 20, 14)

for ($r = 0; $r -lt $rc; $r++) {
    for ($c = 0; $c -lt $cols; $c++) {
        $name = "gui-drive-$($rows[$r])-$($arches[$c])"
        $x = $pad + ($cellW + $pad) * $c
        $y = $titleStrip + $pad + ($cellH + $pad) * $r
        $png = $paths["$($rows[$r])-$($arches[$c])"]
        if (Test-Path $png) {
            $img = [System.Drawing.Image]::FromFile($png)
            # native size, centered horizontally, top-aligned under the header
            $dx = $x + [int](($cellW - $img.Width) / 2)
            $g.DrawImage($img, $dx, ($y + $hdr), $img.Width, $img.Height)
            $img.Dispose()
        }
        else {
            $g.FillRectangle($missBrush, $x, ($y + $hdr), $cellW, $maxH)
            $g.DrawString('(missing)', $hdrFont, $white, ($x + 12), ($y + $hdr + 12))
        }
        $g.FillRectangle($hdrBrush, $x, $y, $cellW, $hdr)
        $sf = New-Object System.Drawing.StringFormat
        $sf.LineAlignment = 'Center'; $sf.Alignment = 'Center'
        $g.DrawString($name, $hdrFont, $dark, (New-Object System.Drawing.RectangleF($x, $y, $cellW, $hdr)), $sf)
    }
}

$canvas.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $canvas.Dispose()
Write-Host "saved $Out ($W x $H)"
