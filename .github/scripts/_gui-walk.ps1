#!/usr/bin/env pwsh
# The GUI drive walk's click targets — ONE table, dot-sourced by all three
# injected legs (gui-drive-inject-{windows,linux,macos}.ps1). The coordinates
# are LOGICAL client coordinates of the app's default 880x960 layout (dark
# theme, 100% UI scale), measured from a pinned-geometry capture; each leg
# scales them by its own measured client width / 880 and offsets them by its
# own client origin.
#
# They live here and not per-leg because a click target is a fact about the
# APP, not about an operating system: every leg boots the same geometry through
# BDINFO_GUI_WIN and renders with the same bundled fonts, so the logical layout
# is identical across platforms. Before this file the table existed three
# times, and a widget move silently broke the two copies nobody edited.
#
# A deliberate layout change must update this table in the same pull request —
# snapshot-test semantics. The failure it guards against is a click landing on
# nothing, which the injected legs catch as their pixel-change assert.
#
# Dot-sourced, never run: defining the function is all this file does.

function Get-GuiWalkTargets {
    <#
    .SYNOPSIS
    The injected walk's click targets, as @(x, y) logical client coordinates.
    #>
    [CmdletBinding()]
    param(
        # The logical height the window was actually GRANTED (client height /
        # scale). 960 is what every leg requests, but a small runner display can
        # clamp it — the macOS probe got 681 — so the bottom action bar and the
        # centred dialog are measured from it rather than from 960. Top-anchored
        # targets are fixed offsets and ignore it.
        [Parameter(Mandatory)] [double] $LogicalHeight
    )

    [ordered]@{
        SelectAll    = @(111, 127)
        LengthHeader = @(484, 170)
        FirstRow     = @(110, 206)
        # The parentheses are load-bearing: `,` binds tighter than `-`, so
        # `@(817, $LogicalHeight - 29)` would subtract 29 from the whole array.
        SettingsBtn  = @(817, ($LogicalHeight - 29))
        DialogCancel = @(538, (($LogicalHeight / 2) + 179))
        ScanBtn      = @(566, ($LogicalHeight - 28))
    }
}
