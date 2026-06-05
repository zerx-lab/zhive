//! Color tokens and theme resolution for the zhive TUI.
//!
//! Mirrors the `zap-tui-design` design language: a dark-first, low-saturation,
//! terminal-native palette where the accent appears only on interactive focus
//! and brand points. Colors are exposed as semantic tokens on [`Palette`] —
//! render code references `palette.fg_dim`, never a raw hex literal, so the
//! whole UI re-themes by swapping one [`Palette`].
//!
//! Three themes ([`Theme`]) set the background/foreground/border families;
//! four accents ([`Accent`]) recolor only the accent tokens. The `mono` theme
//! additionally desaturates the accent regardless of the chosen [`Accent`].

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

/// Base theme selecting the background, foreground, and border families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Dark, low-saturation, terminal-native (the default).
    #[default]
    Dark,
    /// Warm off-white background for bright terminals.
    Light,
    /// Colder and darker than `dark`, with the accent desaturated.
    Mono,
}

/// Accent palette recoloring focus rings, the brand mark, and the agent role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    /// Cyan `#00d9ff` (the default).
    #[default]
    Cyan,
    /// Amber `#fbbf24`.
    Amber,
    /// Lime `#a3e635`.
    Lime,
    /// Magenta `#f0abfc`.
    Magenta,
}

/// Padding density applied to panels and the outer shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    /// Tightest padding.
    Lean,
    /// Balanced padding (the default).
    #[default]
    Default,
    /// Generous padding.
    Airy,
}

impl Density {
    /// Horizontal / vertical inner padding (in cells) for a standard panel.
    #[must_use]
    pub const fn panel_padding(self) -> (u16, u16) {
        match self {
            // Lean and Default collapse to the same cell padding at terminal
            // granularity; Airy adds a column and a row of breathing room.
            Self::Lean | Self::Default => (1, 0),
            Self::Airy => (2, 1),
        }
    }
}

/// A fully resolved set of semantic color tokens ready for rendering.
///
/// Every field is a concrete [`Color`]; there is no alpha at the terminal, so
/// the design's translucent `accent-tint` is pre-blended into [`Palette::sel_bg`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Base background.
    pub bg: Color,
    /// Elevated surface (tool headers, tab strips).
    pub bg_elev: Color,
    /// Overlay / diff body / tool body surface.
    pub bg_overlay: Color,
    /// Default border.
    pub border: Color,
    /// Subtle separator border.
    pub border_dim: Color,
    /// Primary accent (focus rings, spinner, brand).
    pub accent: Color,
    /// Muted accent for pill borders.
    pub accent_soft: Color,
    /// Selected-row background (pre-blended `accent` over `bg`).
    pub sel_bg: Color,
    /// Normal foreground text.
    pub fg: Color,
    /// Bright / emphasized text.
    pub fg_bright: Color,
    /// Secondary / dim text.
    pub fg_dim: Color,
    /// Decoration / line numbers / muted separators.
    pub fg_mute: Color,
    /// User message role label.
    pub role_you: Color,
    /// Assistant message role label (tracks `accent`).
    pub role_zap: Color,
    /// System / notice role label.
    pub role_system: Color,
    /// Success / connected status.
    pub success: Color,
    /// Warning / degraded / busy status.
    pub warn: Color,
    /// Error / danger / rejected status.
    pub error: Color,
    /// Informational status.
    pub info: Color,
    /// Added diff line background.
    pub diff_add_bg: Color,
    /// Added diff line text / sign.
    pub diff_add_fg: Color,
    /// Deleted diff line background.
    pub diff_del_bg: Color,
    /// Deleted diff line text / sign.
    pub diff_del_fg: Color,
    /// Diff context line text.
    pub diff_ctx_fg: Color,
}

/// Blends `top` over `bottom` with `alpha` in 256ths, channel-wise.
///
/// Used to pre-compute the translucent selection tint as a solid color, and by
/// the diff renderer to brighten intra-line emphasis backgrounds.
pub(crate) fn blend(top: Color, bottom: Color, alpha: u16) -> Color {
    let (Color::Rgb(tr, tg, tb), Color::Rgb(br, bg, bb)) = (top, bottom) else {
        return top;
    };
    let mix = |t: u8, b: u8| -> u8 {
        let v = (u16::from(t) * alpha + u16::from(b) * (256 - alpha)) / 256;
        // `v` is a weighted average of two `u8`s, so `v <= 255` always holds;
        // the fallback simply satisfies the type without a panic path.
        u8::try_from(v).unwrap_or(u8::MAX)
    };
    Color::Rgb(mix(tr, br), mix(tg, bg), mix(tb, bb))
}

/// Alpha (in 256ths) used to blend the accent into the selection background.
/// 12% ≈ the design's `accent-tint` opacity, kept subtle on dark backgrounds.
const SEL_TINT_ALPHA: u16 = 31;

impl Palette {
    /// Resolves a concrete palette from a [`Theme`] and an [`Accent`].
    ///
    /// The theme sets the neutral families; the accent recolors `accent`,
    /// `accent_soft`, and `role_zap`; the `mono` theme overrides the accent
    /// with a desaturated tone regardless of `accent`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_tui::theme::{Accent, Palette, Theme};
    /// let p = Palette::resolve(Theme::Dark, Accent::Cyan);
    /// assert_eq!(p.role_zap, p.accent);
    /// ```
    #[must_use]
    pub fn resolve(theme: Theme, accent: Accent) -> Self {
        let (accent_c, accent_soft) = match theme {
            // mono desaturates the accent into a cool grey-blue.
            Theme::Mono => (Color::Rgb(0x8b, 0x9b, 0xb0), Color::Rgb(0x5b, 0x68, 0x78)),
            _ => match accent {
                Accent::Cyan => (Color::Rgb(0x00, 0xd9, 0xff), Color::Rgb(0x08, 0x91, 0xb2)),
                Accent::Amber => (Color::Rgb(0xfb, 0xbf, 0x24), Color::Rgb(0xb4, 0x53, 0x09)),
                Accent::Lime => (Color::Rgb(0xa3, 0xe6, 0x35), Color::Rgb(0x65, 0xa3, 0x0d)),
                Accent::Magenta => (Color::Rgb(0xf0, 0xab, 0xfc), Color::Rgb(0xa2, 0x1c, 0xaf)),
            },
        };

        let mut p = match theme {
            Theme::Dark => Self::dark_base(),
            Theme::Light => Self::light_base(),
            Theme::Mono => Self::mono_base(),
        };
        p.accent = accent_c;
        p.accent_soft = accent_soft;
        p.role_zap = accent_c;
        p.sel_bg = blend(accent_c, p.bg, SEL_TINT_ALPHA);
        p
    }

    /// The dark theme neutral + status families (before accent application).
    fn dark_base() -> Self {
        Self {
            bg: Color::Rgb(0x0b, 0x0f, 0x14),
            bg_elev: Color::Rgb(0x10, 0x16, 0x1f),
            bg_overlay: Color::Rgb(0x0d, 0x12, 0x19),
            border: Color::Rgb(0x1f, 0x2a, 0x37),
            border_dim: Color::Rgb(0x14, 0x1c, 0x26),
            accent: Color::Rgb(0x00, 0xd9, 0xff),
            accent_soft: Color::Rgb(0x08, 0x91, 0xb2),
            sel_bg: Color::Rgb(0x12, 0x1d, 0x26),
            fg: Color::Rgb(0xcd, 0xd6, 0xe3),
            fg_bright: Color::Rgb(0xff, 0xff, 0xff),
            fg_dim: Color::Rgb(0x5b, 0x68, 0x78),
            fg_mute: Color::Rgb(0x38, 0x42, 0x4f),
            role_you: Color::Rgb(0xb7, 0x94, 0xf6),
            role_zap: Color::Rgb(0x00, 0xd9, 0xff),
            role_system: Color::Rgb(0x5b, 0x68, 0x78),
            success: Color::Rgb(0x5e, 0xea, 0xd4),
            warn: Color::Rgb(0xfb, 0xbf, 0x24),
            error: Color::Rgb(0xf8, 0x71, 0x71),
            info: Color::Rgb(0x93, 0xc5, 0xfd),
            diff_add_bg: Color::Rgb(0x0f, 0x3a, 0x30),
            diff_add_fg: Color::Rgb(0x5e, 0xea, 0xd4),
            diff_del_bg: Color::Rgb(0x3f, 0x12, 0x16),
            diff_del_fg: Color::Rgb(0xfd, 0xa4, 0xaf),
            diff_ctx_fg: Color::Rgb(0x6b, 0x77, 0x87),
        }
    }

    /// The light theme neutral + status families.
    fn light_base() -> Self {
        Self {
            bg: Color::Rgb(0xf5, 0xf3, 0xee),
            bg_elev: Color::Rgb(0xeb, 0xe7, 0xdf),
            bg_overlay: Color::Rgb(0xf0, 0xec, 0xe4),
            border: Color::Rgb(0xc8, 0xc1, 0xb1),
            border_dim: Color::Rgb(0xdd, 0xd6, 0xc6),
            accent: Color::Rgb(0x08, 0x91, 0xb2),
            accent_soft: Color::Rgb(0x08, 0x91, 0xb2),
            sel_bg: Color::Rgb(0xe6, 0xe1, 0xd6),
            fg: Color::Rgb(0x2a, 0x26, 0x20),
            fg_bright: Color::Rgb(0x00, 0x00, 0x00),
            fg_dim: Color::Rgb(0x6b, 0x63, 0x54),
            fg_mute: Color::Rgb(0x9b, 0x93, 0x84),
            role_you: Color::Rgb(0x6d, 0x28, 0xd9),
            role_zap: Color::Rgb(0x08, 0x91, 0xb2),
            role_system: Color::Rgb(0x6b, 0x63, 0x54),
            success: Color::Rgb(0x0f, 0x76, 0x6e),
            warn: Color::Rgb(0xb4, 0x53, 0x09),
            error: Color::Rgb(0xb9, 0x1c, 0x1c),
            info: Color::Rgb(0x1d, 0x4e, 0xd8),
            diff_add_bg: Color::Rgb(0xd4, 0xf3, 0xdf),
            diff_add_fg: Color::Rgb(0x16, 0x65, 0x34),
            diff_del_bg: Color::Rgb(0xfa, 0xdb, 0xdd),
            diff_del_fg: Color::Rgb(0x99, 0x1b, 0x1b),
            diff_ctx_fg: Color::Rgb(0x6b, 0x63, 0x54),
        }
    }

    /// The mono theme neutral + status families (accent applied separately).
    fn mono_base() -> Self {
        Self {
            bg: Color::Rgb(0x07, 0x09, 0x0c),
            bg_elev: Color::Rgb(0x0c, 0x10, 0x15),
            bg_overlay: Color::Rgb(0x0a, 0x0d, 0x12),
            border: Color::Rgb(0x2a, 0x31, 0x3a),
            border_dim: Color::Rgb(0x1a, 0x1f, 0x26),
            accent: Color::Rgb(0x8b, 0x9b, 0xb0),
            accent_soft: Color::Rgb(0x5b, 0x68, 0x78),
            sel_bg: Color::Rgb(0x12, 0x16, 0x1c),
            fg: Color::Rgb(0xd1, 0xd5, 0xdb),
            fg_bright: Color::Rgb(0xff, 0xff, 0xff),
            fg_dim: Color::Rgb(0x6b, 0x72, 0x80),
            fg_mute: Color::Rgb(0x37, 0x41, 0x51),
            role_you: Color::Rgb(0xe5, 0xe7, 0xeb),
            role_zap: Color::Rgb(0x8b, 0x9b, 0xb0),
            role_system: Color::Rgb(0x6b, 0x72, 0x80),
            success: Color::Rgb(0x5e, 0xea, 0xd4),
            warn: Color::Rgb(0xfb, 0xbf, 0x24),
            error: Color::Rgb(0xf8, 0x71, 0x71),
            info: Color::Rgb(0x93, 0xc5, 0xfd),
            diff_add_bg: Color::Rgb(0x10, 0x20, 0x1c),
            diff_add_fg: Color::Rgb(0x5e, 0xea, 0xd4),
            diff_del_bg: Color::Rgb(0x24, 0x12, 0x14),
            diff_del_fg: Color::Rgb(0xfd, 0xa4, 0xaf),
            diff_ctx_fg: Color::Rgb(0x4b, 0x55, 0x63),
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::resolve(Theme::default(), Accent::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_recolors_zap_and_accent_tokens() {
        let cyan = Palette::resolve(Theme::Dark, Accent::Cyan);
        let amber = Palette::resolve(Theme::Dark, Accent::Amber);
        assert_eq!(cyan.role_zap, cyan.accent);
        assert_ne!(cyan.accent, amber.accent);
        // Neutral tokens stay put across accents.
        assert_eq!(cyan.bg, amber.bg);
        assert_eq!(cyan.fg, amber.fg);
    }

    #[test]
    fn mono_desaturates_accent_regardless_of_choice() {
        let mono_cyan = Palette::resolve(Theme::Mono, Accent::Cyan);
        let mono_magenta = Palette::resolve(Theme::Mono, Accent::Magenta);
        assert_eq!(mono_cyan.accent, mono_magenta.accent);
    }

    #[test]
    fn blend_is_a_weighted_average() {
        let black = Color::Rgb(0, 0, 0);
        let white = Color::Rgb(255, 255, 255);
        // 50% blend lands near mid-grey.
        let Color::Rgb(r, _, _) = blend(white, black, 128) else {
            panic!("rgb expected");
        };
        assert!((126..=128).contains(&r));
    }
}

// Rust guideline compliant 2026-02-21
