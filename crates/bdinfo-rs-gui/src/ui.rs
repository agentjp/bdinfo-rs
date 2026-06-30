//! The design tokens and widget-style vocabulary the [`view`](crate) reads.
//!
//! Every spacing, radius, font, type size, and column width the GUI draws is a
//! named constant here, and every styled widget gets its appearance from one of
//! the small style builders below — so the whole look changes from this one file
//! and `view` stays a thin projection. The builders take a [`Palette`] by value
//! (it is `Copy`) and return the `iced` style closure for a widget, reading only
//! palette tokens — never a raw colour.

use iced::font::Weight;
use iced::widget::{button, container, progress_bar};
use iced::{Border, Color, Font, Shadow, Theme, Vector, border};

use crate::theme::Palette;

// ── Typography ──────────────────────────────────────────────────────────────

/// The UI typeface (Inter) — labels, buttons, headers. Bundled, so it renders
/// identically on every OS/arch.
pub const UI: Font = Font::with_name("Inter");
/// Inter at medium weight — column headers and the info strip.
pub const UI_MEDIUM: Font = Font { weight: Weight::Medium, ..Font::with_name("Inter") };
/// Inter at semibold — the brand wordmark, pane titles, the primary action.
pub const UI_SEMIBOLD: Font = Font { weight: Weight::Semibold, ..Font::with_name("Inter") };
/// The report / data typeface (`JetBrains Mono`): the report pane and the table's
/// numeric columns, where fixed-width digits keep figures aligned.
pub const MONO: Font = Font::with_name("JetBrains Mono");

/// Type scale, in pixels.
pub const TEXT_XS: f32 = 12.0;
/// Small body / secondary labels.
pub const TEXT_SM: f32 = 13.0;
/// Default body text.
pub const TEXT_BASE: f32 = 14.0;
/// Emphasised body / a pane title.
pub const TEXT_MD: f32 = 16.0;
/// The empty-state headline.
pub const TEXT_LG: f32 = 22.0;

// ── Spacing & shape ───────────────────────────────────────────────────────

/// 4 px — the base spacing unit.
pub const GAP_1: f32 = 4.0;
/// 8 px.
pub const GAP_2: f32 = 8.0;
/// 12 px.
pub const GAP_3: f32 = 12.0;
/// 16 px — the window's outer padding and major section gap.
pub const GAP_4: f32 = 16.0;
/// 24 px.
pub const GAP_6: f32 = 24.0;

/// The corner radius of panels, fields, and buttons.
pub const RADIUS: f32 = 8.0;
/// A tighter radius for inline chips and the row-selection bar.
pub const RADIUS_SM: f32 = 5.0;

/// A table row's height.
pub const ROW_H: f32 = 34.0;
/// The table header strip's height.
pub const HEADER_H: f32 = 36.0;

// ── Table column widths (the Playlist File column takes the remaining space) ──

/// The leading selection-indicator column.
pub const COL_CHECK: f32 = 44.0;
/// The `#` column.
pub const COL_NUM: f32 = 48.0;
/// The `Group` column.
pub const COL_GROUP: f32 = 72.0;
/// The `Length` column.
pub const COL_LENGTH: f32 = 112.0;
/// The `Estimated Bytes` column.
pub const COL_BYTES: f32 = 152.0;
/// The `Measured Bytes` column.
pub const COL_MEASURED: f32 = 152.0;
/// The stream-files `Index` column.
pub const COL_INDEX: f32 = 72.0;
/// The streams `Codec` column.
pub const COL_CODEC: f32 = 220.0;
/// The streams `Language` column.
pub const COL_LANG: f32 = 120.0;
/// The streams `Bit Rate` column.
pub const COL_BITRATE: f32 = 124.0;
/// The width of the row-selection accent bar.
pub const SEL_BAR: f32 = 3.0;

/// Returns `color` with its alpha replaced — for tints and shadows.
const fn with_alpha(color: Color, a: f32) -> Color {
    Color { a, ..color }
}

// ── Container styles ──────────────────────────────────────────────────────

/// The top toolbar: the most-raised surface.
pub fn toolbar(p: Palette) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(p.panel_high.into()),
        ..container::Style::default()
    }
}

/// The bottom action bar: the most-raised surface, with a hairline above it.
pub fn bottom_bar(p: Palette) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(p.panel_high.into()),
        border: border::color(p.line).width(0.0),
        ..container::Style::default()
    }
}

/// The table header strip: raised, over the body rows.
pub fn table_header(p: Palette) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(p.panel_high.into()),
        ..container::Style::default()
    }
}

/// The table's outer frame — a precise, **sharp-cornered** data grid (a
/// deliberate contrast to the rounded chrome and buttons around it).
pub fn table_frame(p: Palette) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(p.surface.into()),
        border: border::color(p.line).width(1.0),
        ..container::Style::default()
    }
}

/// A 1-pixel horizontal divider in the given line colour.
pub fn divider(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style { background: Some(color.into()), ..container::Style::default() }
}

/// A read-only data row's zebra background (even rows on the surface, odd on the
/// alt) — the non-interactive panes (Stream Files / Streams) use this.
pub fn zebra(p: Palette, even: bool) -> impl Fn(&Theme) -> container::Style {
    let bg = if even { p.surface } else { p.surface_alt };
    move |_| container::Style { background: Some(bg.into()), ..container::Style::default() }
}

/// The dimmed scrim behind a modal — a translucent wash over the whole window.
pub fn scrim() -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(with_alpha(Color::BLACK, 0.45).into()),
        ..container::Style::default()
    }
}

/// The read-only "source path" field — styled like a quiet text input.
pub fn path_field(p: Palette) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(p.surface.into()),
        border: border::rounded(RADIUS_SM).color(p.line_strong).width(1.0),
        ..container::Style::default()
    }
}

/// The slim "detected disc" info strip beneath the table.
pub fn info_strip(p: Palette) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        text_color: Some(p.text_muted),
        background: Some(p.surface_alt.into()),
        border: border::rounded(RADIUS_SM).color(p.line).width(1.0),
        ..container::Style::default()
    }
}

/// The report pane: a slightly inset readout surface for the monospace report.
pub fn report_pane(p: Palette) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(p.surface_alt.into()),
        border: border::rounded(RADIUS).color(p.line).width(1.0),
        ..container::Style::default()
    }
}

/// The centred empty-state / error card — a lifted panel with a soft shadow.
pub fn hero_card(p: Palette) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(p.panel.into()),
        border: border::rounded(RADIUS).color(p.line).width(1.0),
        shadow: Shadow {
            color: with_alpha(Color::BLACK, if p.is_dark { 0.35 } else { 0.10 }),
            offset: Vector::new(0.0, 6.0),
            blur_radius: 24.0,
        },
        ..container::Style::default()
    }
}

/// A non-blocking banner tinted by `kind` (warning or danger): a faint wash and a
/// matching hairline, the message drawn in the normal text colour.
pub fn banner(p: Palette, kind: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        text_color: Some(p.text),
        background: Some(with_alpha(kind, if p.is_dark { 0.14 } else { 0.10 }).into()),
        border: border::rounded(RADIUS_SM).color(with_alpha(kind, 0.45)).width(1.0),
        ..container::Style::default()
    }
}

/// A small solid status dot (the leading mark on a banner) in `color`.
pub fn dot(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(color.into()),
        border: border::rounded(RADIUS_SM),
        ..container::Style::default()
    }
}

/// The row-selection accent bar — solid accent when the row is selected, else
/// invisible (so every row reserves the same width and nothing shifts).
pub fn sel_bar(p: Palette, selected: bool) -> impl Fn(&Theme) -> container::Style {
    let color = if selected { p.accent } else { Color::TRANSPARENT };
    move |_| container::Style { background: Some(color.into()), ..container::Style::default() }
}

/// The selection-indicator box in the leading column: an accent-filled chip when
/// the row is selected, an empty outlined box when not.
pub fn check_box(p: Palette, selected: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        if selected {
            container::Style {
                background: Some(p.accent.into()),
                border: border::rounded(RADIUS_SM),
                ..container::Style::default()
            }
        } else {
            container::Style {
                border: border::rounded(RADIUS_SM).color(p.line_strong).width(1.5),
                ..container::Style::default()
            }
        }
    }
}

// ── Button styles ─────────────────────────────────────────────────────────

/// The primary action (Scan selected, the empty-state Open buttons): a filled
/// amber button with `on_accent` text.
pub fn primary_button(p: Palette) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let bg = match status {
            button::Status::Hovered | button::Status::Pressed => p.accent_hover,
            button::Status::Active => p.accent,
            button::Status::Disabled => with_alpha(p.accent, 0.40),
        };
        let text_alpha = if status == button::Status::Disabled { 0.7 } else { 1.0 };
        button::Style {
            background: Some(bg.into()),
            text_color: with_alpha(p.on_accent, text_alpha),
            border: border::rounded(RADIUS),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// A secondary action (Browse, ISO, Rescan, Save, Copy): an outlined button that
/// fills on hover.
pub fn secondary_button(p: Palette) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let (bg, line, text) = match status {
            button::Status::Hovered | button::Status::Pressed => (p.hover, p.accent, p.text),
            button::Status::Active => (p.surface, p.line_strong, p.text),
            button::Status::Disabled => (p.surface, p.line, p.text_faint),
        };
        button::Style {
            background: Some(bg.into()),
            text_color: text,
            border: border::rounded(RADIUS).color(line).width(1.0),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// A quiet toolbar button (the theme toggle): no chrome until hover.
pub fn toolbar_button(p: Palette) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let (bg, text) = match status {
            button::Status::Hovered | button::Status::Pressed => (Some(p.hover.into()), p.text),
            button::Status::Active => (None, p.text_muted),
            button::Status::Disabled => (None, p.text_faint),
        };
        button::Style {
            background: bg,
            text_color: text,
            border: border::rounded(RADIUS_SM),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// A clickable table-row region, styled as a flush button so it gets
/// hover/press/disabled states for free: the **active** (highlighted) row carries
/// the amber wash, others zebra-stripe by parity, and any row lifts to `hover`
/// under the cursor (a disabled row — during a scan — keeps its resting fill so
/// the table reads frozen, not greyed).
pub fn row_button(
    p: Palette,
    active: bool,
    even: bool,
) -> impl Fn(&Theme, button::Status) -> button::Style {
    let resting = if active {
        p.selected
    } else if even {
        p.surface
    } else {
        p.surface_alt
    };
    move |_, status| {
        let bg = match status {
            button::Status::Hovered | button::Status::Pressed if !active => p.hover,
            _ => resting,
        };
        button::Style {
            background: Some(bg.into()),
            text_color: p.text,
            border: Border::default(),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

// ── Progress bar ────────────────────────────────────────────────────────────

/// The scan progress bar: an amber fill over a quiet track.
pub fn progress(p: Palette) -> impl Fn(&Theme) -> progress_bar::Style {
    move |_| progress_bar::Style {
        background: p.line.into(),
        bar: p.accent.into(),
        border: border::rounded(RADIUS_SM),
    }
}
