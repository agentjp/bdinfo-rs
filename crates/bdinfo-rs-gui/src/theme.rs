//! The "Projection Booth" visual identity — the one place every colour the GUI
//! draws is named and defined.
//!
//! A deep, cool **projection-booth** surface set with a single warm
//! **projector-lamp amber** accent reserved for selection, progress, and focus —
//! a deliberate identity for a precise Blu-ray analysis tool, not the default
//! immediate-mode-debug look. Both a first-class light and dark [`Palette`] are
//! defined here as named tokens; the rest of the GUI reads them and never spells
//! a raw hex value of its own.
//!
//! Tier A: the tokens and the resolve/cycle logic are pure data + decisions
//! (iced's `Color`/`Theme` are plain values, native-testable), held to the
//! core bar like the rest of the library; only the widgets that *consume*
//! these tokens (the binary's `ui` style builders) are render glue.

use iced::{Color, Theme, theme};

use crate::settings::ThemeChoice;

/// Creates an opaque [`Color`] from 8-bit channels (the form every token below
/// is written in). `const` so the palettes are compile-time constants.
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}

/// A complete set of named colour tokens — one per role the UI draws.
///
/// Both [`Palette::DARK`] and [`Palette::LIGHT`] fill every field, so a widget
/// style reads `palette.accent` (etc.) and is correct in either mode. `Copy`,
/// so a `view` style closure can capture it by value and ignore the
/// `iced::Theme` arg.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Whether this is the dark palette — drives the OS-derived `iced::Theme`
    /// tone and a few light/dark-only nudges.
    pub is_dark: bool,
    /// The window background — the room behind every panel.
    pub bg: Color,
    /// A panel's base fill / an even table row.
    pub surface: Color,
    /// The zebra-stripe fill for an odd table row.
    pub surface_alt: Color,
    /// A raised panel (the table frame, the report pane).
    pub panel: Color,
    /// The most-raised surface: the toolbar, the table header, the bottom bar.
    pub panel_high: Color,
    /// A row's hover fill (no accent — just lifted off the surface).
    pub hover: Color,
    /// A hairline divider between cells / sections.
    pub line: Color,
    /// A stronger rule: a header underline or a resting input border.
    pub line_strong: Color,
    /// Primary text.
    pub text: Color,
    /// Secondary text: labels, column headers, the info strip.
    pub text_muted: Color,
    /// Tertiary text: placeholders and captions.
    pub text_faint: Color,
    /// The warm projector-lamp accent — selection, progress fill, focus.
    pub accent: Color,
    /// The accent under hover/press (brighter in dark, deeper in light).
    pub accent_hover: Color,
    /// Text/icons drawn on top of a filled-accent surface (a primary button).
    pub on_accent: Color,
    /// A selected table row's background — an amber wash over the surface.
    pub selected: Color,
    /// A non-blocking warning (a corrupt-playlist banner, the hidden-tracks note).
    pub warning: Color,
    /// A failure (a fatal pick error, a partial-scan banner).
    pub danger: Color,
}

impl Palette {
    /// The dark identity — the primary mood: a deep slate booth lit by one warm
    /// amber lamp.
    pub const DARK: Self = Self {
        is_dark: true,
        bg: rgb(15, 20, 25),
        surface: rgb(22, 29, 36),
        surface_alt: rgb(18, 24, 30),
        panel: rgb(27, 35, 43),
        panel_high: rgb(33, 43, 52),
        hover: rgb(31, 41, 50),
        line: rgb(40, 50, 60),
        line_strong: rgb(54, 67, 79),
        text: rgb(230, 234, 238),
        text_muted: rgb(154, 167, 178),
        text_faint: rgb(107, 120, 132),
        accent: rgb(242, 166, 90),
        accent_hover: rgb(255, 188, 120),
        on_accent: rgb(20, 17, 12),
        selected: rgb(44, 42, 28),
        warning: rgb(224, 179, 65),
        danger: rgb(229, 115, 107),
    };
    /// The light identity — the same booth in daylight: warm paper, cool panels,
    /// and a deeper amber so the accent stays legible on white.
    pub const LIGHT: Self = Self {
        is_dark: false,
        bg: rgb(246, 247, 245),
        surface: rgb(255, 255, 255),
        surface_alt: rgb(241, 243, 241),
        panel: rgb(255, 255, 255),
        panel_high: rgb(236, 238, 236),
        hover: rgb(240, 242, 240),
        line: rgb(226, 229, 226),
        line_strong: rgb(203, 208, 204),
        text: rgb(27, 34, 40),
        text_muted: rgb(92, 102, 112),
        text_faint: rgb(138, 148, 157),
        accent: rgb(196, 106, 26),
        accent_hover: rgb(168, 89, 18),
        on_accent: rgb(255, 246, 236),
        selected: rgb(251, 239, 221),
        warning: rgb(181, 134, 11),
        danger: rgb(192, 71, 62),
    };

    /// The matching `iced::Theme`, so the few widgets the GUI does not style by
    /// hand (text, scrollbars, the caret) inherit colours from the same palette.
    /// The hand-styled widgets capture this [`Palette`] directly and ignore it.
    #[must_use]
    pub fn iced_theme(self) -> Theme {
        let name = if self.is_dark { "Projection Booth Dark" } else { "Projection Booth Light" };
        Theme::custom(
            name.to_owned(),
            theme::Palette {
                background: self.bg,
                text: self.text,
                primary: self.accent,
                success: self.warning,
                warning: self.warning,
                danger: self.danger,
            },
        )
    }

    /// The app-level base style: the window background and default text colour.
    #[must_use]
    pub const fn base_style(self) -> theme::Style {
        theme::Style { background_color: self.bg, text_color: self.text }
    }
}

/// The user's theme preference: follow the OS, or pin light/dark. Defaults to
/// [`ThemePref::System`] so a fresh launch matches the desktop, and a click
/// cycles System → Light → Dark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThemePref {
    /// Follow the operating system's light/dark setting.
    #[default]
    System,
    /// Always light, regardless of the OS.
    Light,
    /// Always dark, regardless of the OS.
    Dark,
}

impl ThemePref {
    /// The next preference in the System → Light → Dark → System cycle (the
    /// toolbar toggle).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }

    /// Resolves the concrete [`Palette`] from this preference and the last-known
    /// OS mode. `System` follows the OS (defaulting to light when the OS gives no
    /// preference, `theme::Mode::None`); `Light`/`Dark` ignore the OS.
    #[must_use]
    pub const fn resolve(self, os: theme::Mode) -> Palette {
        match self {
            Self::Light => Palette::LIGHT,
            Self::Dark => Palette::DARK,
            Self::System => match os {
                theme::Mode::Dark => Palette::DARK,
                theme::Mode::Light | theme::Mode::None => Palette::LIGHT,
            },
        }
    }

    /// The persisted form of this preference (the config store's
    /// [`ThemeChoice`] — the store stays iced-free, so the mapping lives here).
    #[must_use]
    pub const fn choice(self) -> ThemeChoice {
        match self {
            Self::System => ThemeChoice::System,
            Self::Light => ThemeChoice::Light,
            Self::Dark => ThemeChoice::Dark,
        }
    }

    /// Restores a preference from its persisted form.
    #[must_use]
    pub const fn from_choice(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::System => Self::System,
            ThemeChoice::Light => Self::Light,
            ThemeChoice::Dark => Self::Dark,
        }
    }

    /// A short label for the toolbar toggle, naming the *current* preference.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "Auto",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

#[cfg(test)]
mod tests {
    use iced::theme::Mode;

    use super::{Palette, ThemePref};

    #[test]
    fn system_follows_the_os_and_defaults_to_light() {
        assert_eq!(ThemePref::System.resolve(Mode::Dark), Palette::DARK);
        assert_eq!(ThemePref::System.resolve(Mode::Light), Palette::LIGHT);
        // No OS preference falls back to light.
        assert_eq!(ThemePref::System.resolve(Mode::None), Palette::LIGHT);
    }

    #[test]
    fn pinned_prefs_ignore_the_os() {
        for os in [Mode::Dark, Mode::Light, Mode::None] {
            assert_eq!(ThemePref::Light.resolve(os), Palette::LIGHT);
            assert_eq!(ThemePref::Dark.resolve(os), Palette::DARK);
        }
    }

    #[test]
    fn the_toggle_cycles_system_light_dark() {
        assert_eq!(ThemePref::System.next(), ThemePref::Light);
        assert_eq!(ThemePref::Light.next(), ThemePref::Dark);
        assert_eq!(ThemePref::Dark.next(), ThemePref::System);
        // Three clicks return home.
        assert_eq!(ThemePref::default().next().next().next(), ThemePref::default());
    }

    #[test]
    fn the_two_palettes_differ_and_are_internally_tagged() {
        // These are invariants of the const palettes, so assert them at compile
        // time (a runtime `assert!` on a constant trips `assertions_on_constants`).
        const {
            assert!(Palette::DARK.is_dark);
            assert!(!Palette::LIGHT.is_dark);
            // The two modes use different backgrounds.
            assert!(Palette::DARK.bg.r != Palette::LIGHT.bg.r);
            // The accent is warm (red channel dominant) in both modes.
            assert!(Palette::DARK.accent.r > Palette::DARK.accent.b);
            assert!(Palette::LIGHT.accent.r > Palette::LIGHT.accent.b);
        }
    }

    #[test]
    fn the_persisted_form_round_trips() {
        for pref in [ThemePref::System, ThemePref::Light, ThemePref::Dark] {
            assert_eq!(ThemePref::from_choice(pref.choice()), pref);
        }
    }

    #[test]
    fn labels_name_the_preference() {
        assert_eq!(ThemePref::System.label(), "Auto");
        assert_eq!(ThemePref::Light.label(), "Light");
        assert_eq!(ThemePref::Dark.label(), "Dark");
    }

    #[test]
    fn rgb_maps_each_channel_onto_the_unit_range() {
        // 51/255 = 0.2, 102/255 = 0.4, 204/255 = 0.8 — exact in f32, so the
        // channel order and the /255 scaling are both pinned.
        let color = super::rgb(51, 102, 204);
        assert_eq!(color, iced::Color { r: 0.2, g: 0.4, b: 0.8, a: 1.0 });
        assert_eq!(super::rgb(255, 0, 0), iced::Color { r: 1.0, g: 0.0, b: 0.0, a: 1.0 });
    }

    #[test]
    fn the_iced_theme_carries_the_palette_and_the_mode_name() {
        // The custom theme's display name tags the mode…
        assert_eq!(Palette::DARK.iced_theme().to_string(), "Projection Booth Dark");
        assert_eq!(Palette::LIGHT.iced_theme().to_string(), "Projection Booth Light");
        // …and every mapped slot reads back the palette token it was built from.
        for p in [Palette::DARK, Palette::LIGHT] {
            let theme = p.iced_theme().palette();
            assert_eq!(theme.background, p.bg);
            assert_eq!(theme.text, p.text);
            assert_eq!(theme.primary, p.accent);
            assert_eq!(theme.success, p.warning);
            assert_eq!(theme.warning, p.warning);
            assert_eq!(theme.danger, p.danger);
        }
    }

    #[test]
    fn the_base_style_is_the_window_background_and_text() {
        for p in [Palette::DARK, Palette::LIGHT] {
            let style = p.base_style();
            assert_eq!(style.background_color, p.bg);
            assert_eq!(style.text_color, p.text);
        }
    }
}
