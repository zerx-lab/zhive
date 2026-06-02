//! Reusable rendering primitives shared across screens.
//!
//! These map the `zap-tui-design` component vocabulary onto ratatui building
//! blocks: a titled bordered [`panel`] (title floats on the top border, accent
//! when focused), the 10-frame braille [`spinner`], a block-character
//! [`progress_bar`], connection [`status_dot`]s, and the bottom-bar
//! [`kbd_hints`] strip. Everything is styled exclusively from a [`Palette`].

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding};

use crate::theme::Palette;

/// The ten braille frames of the busy spinner, cycled at the redraw tick.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Returns the spinner glyph for animation step `tick`.
///
/// # Examples
///
/// ```
/// use zhive_tui::widgets::spinner;
/// assert_eq!(spinner(0), "⠋");
/// assert_eq!(spinner(10), "⠋");
/// ```
#[must_use]
pub fn spinner(tick: usize) -> &'static str {
    SPINNER_FRAMES[tick % SPINNER_FRAMES.len()]
}

/// Builds a titled, bordered panel block in the design's house style.
///
/// The `title` floats on the top-left border; an optional `status` floats on
/// the top-right. A focused panel uses the accent border and accent title.
#[must_use]
pub fn panel(title: &str, status: Option<&str>, focused: bool, p: &Palette) -> Block<'static> {
    let border_color = if focused { p.accent } else { p.border };
    let title_color = if focused { p.accent } else { p.fg_dim };
    let mut block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(border_color))
        .style(Style::new().bg(p.bg).fg(p.fg))
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            format!(" {title} "),
            Style::new().fg(title_color).add_modifier(Modifier::BOLD),
        ));
    if let Some(status) = status {
        block = block
            .title(Line::styled(format!(" {status} "), Style::new().fg(p.fg_dim)).right_aligned());
    }
    block
}

/// A rounded, elevated variant of [`panel`] for nested / overlay panels.
#[must_use]
pub fn panel_rounded(
    title: &str,
    status: Option<&str>,
    focused: bool,
    p: &Palette,
) -> Block<'static> {
    panel(title, status, focused, p).border_type(BorderType::Rounded)
}

/// Renders a `█`/`░` progress bar `width` cells wide, filled to `percent`.
///
/// `percent` is clamped to `0..=100`. Filled cells use the accent color,
/// the remainder uses the muted foreground.
///
/// # Examples
///
/// ```
/// use zhive_tui::widgets::progress_bar;
/// use zhive_tui::theme::Palette;
/// let line = progress_bar(50, 10, &Palette::default());
/// assert_eq!(line.spans.len(), 2);
/// ```
#[must_use]
pub fn progress_bar(percent: u16, width: u16, p: &Palette) -> Line<'static> {
    let w = usize::from(width);
    let pct = usize::from(percent.min(100));
    let filled = w * pct / 100;
    let empty = w.saturating_sub(filled);
    Line::from(vec![
        Span::styled("█".repeat(filled), Style::new().fg(p.accent)),
        Span::styled("░".repeat(empty), Style::new().fg(p.fg_mute)),
    ])
}

/// Connection / liveness state shown as a `●◐○` dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotState {
    /// Connected / ready / healthy (`●`, success color).
    On,
    /// Degraded / busy / loading (`◐`, warn color).
    Degraded,
    /// Off / disabled / offline (`○`, muted color).
    Off,
}

/// Renders a single status dot for `state`.
#[must_use]
pub fn status_dot(state: DotState, p: &Palette) -> Span<'static> {
    let (glyph, color) = match state {
        DotState::On => ("●", p.success),
        DotState::Degraded => ("◐", p.warn),
        DotState::Off => ("○", p.fg_mute),
    };
    Span::styled(glyph, Style::new().fg(color))
}

/// A single bottom-bar key hint: a highlighted key and its description.
#[derive(Debug, Clone, Copy)]
pub struct Hint {
    /// The key glyph(s), e.g. `"↵"` or `"⌃C"`.
    pub key: &'static str,
    /// What the key does, e.g. `"send"`.
    pub label: &'static str,
}

impl Hint {
    /// Convenience constructor for a [`Hint`].
    #[must_use]
    pub const fn new(key: &'static str, label: &'static str) -> Self {
        Self { key, label }
    }
}

/// Renders a sequence of key hints as a single styled line (`[key] label`).
#[must_use]
pub fn kbd_hints(hints: &[Hint], p: &Palette) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, hint) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!(" {} ", hint.key),
            Style::new().fg(p.fg_bright).bg(p.bg_elev),
        ));
        spans.push(Span::styled(
            format!(" {}", hint.label),
            Style::new().fg(p.fg_dim),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bar_fills_proportionally() {
        let p = Palette::default();
        let line = progress_bar(50, 10, &p);
        assert_eq!(line.spans[0].content.chars().count(), 5);
        assert_eq!(line.spans[1].content.chars().count(), 5);
    }

    #[test]
    fn progress_bar_clamps_over_100() {
        let p = Palette::default();
        let line = progress_bar(250, 8, &p);
        assert_eq!(line.spans[0].content.chars().count(), 8);
        assert_eq!(line.spans[1].content.chars().count(), 0);
    }

    #[test]
    fn spinner_wraps() {
        assert_eq!(spinner(0), spinner(SPINNER_FRAMES.len()));
    }
}

// Rust guideline compliant 2026-02-21
