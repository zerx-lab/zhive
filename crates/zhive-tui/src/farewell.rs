//! The exit banner printed to the normal terminal after the TUI tears down.
//!
//! On a deliberate quit (Ctrl+D / `/quit`) [`crate::run`] leaves the alternate
//! screen and then writes this compact `ZHIVE` banner to stdout, so it lands in
//! the terminal's scrollback the way opencode's farewell does. When the session
//! held a real conversation, the banner names its thread id and how to resume it
//! — re-launch `zhive` and find it in the `/session` picker (which now matches
//! on the thread id, so the printed id is a working search key) — since the CLI
//! has no resume flag. A never-used session (no turns, so nothing was persisted)
//! shows only the wordmark, never a phantom id that `/session` could not find.
//!
//! Colors come from the live [`Palette`] so the banner matches the theme the
//! user was running, rendered in the theme's grayscale neutrals (no accent, no
//! animation). The banner is written through a plain [`std::io::Write`], so it
//! is unit-testable against an in-memory buffer without a terminal.

use std::io::Write;

use crossterm::queue;
use crossterm::style::{
    Attribute, Color as CtColor, Print, ResetColor, SetAttribute, SetForegroundColor,
};
use ratatui::style::Color;

use crate::theme::Palette;

/// Column the value text starts at, past the widest label (`Continue` + gap).
///
/// `Continue` is 8 cells and `Session` is 7; padding both to this width aligns
/// the values into one column with a two-space gutter after the longer label.
const LABEL_WIDTH: usize = 10;

/// Maps a ratatui [`Color`] to the crossterm color used for stdout styling.
///
/// Every [`Palette`] field is an RGB triple, so the non-`Rgb` arm never fires in
/// practice; it falls back to the terminal default rather than guessing a value.
fn to_crossterm(color: Color) -> CtColor {
    match color {
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
        _ => CtColor::Reset,
    }
}

/// Writes the compact `ZHIVE` exit banner to `out`, styled from `palette`.
///
/// `session` is the live thread id when the conversation held content (and was
/// therefore persisted); `None` for a never-used session. With an id the banner
/// is a `▌ ZHIVE` wordmark over two aligned rows — the thread id and a hint to
/// resume it via `/session` — otherwise just the wordmark. Each line ends with
/// `\n` (the terminal is back in cooked mode when this runs), and styling is
/// reset before returning so it cannot bleed into the shell prompt that follows.
/// Kept generic over [`Write`] so the unit tests can render it into a buffer.
///
/// # Errors
///
/// Propagates any [`std::io::Error`] from writing to or flushing `out`.
pub(crate) fn write_banner(
    out: &mut impl Write,
    palette: &Palette,
    session: Option<&str>,
) -> std::io::Result<()> {
    let dim = to_crossterm(palette.fg_dim);
    let fg = to_crossterm(palette.fg);
    let bright = to_crossterm(palette.fg_bright);

    queue!(
        out,
        // A blank line separates the banner from the prior screen output.
        Print("\n"),
        // Wordmark: a muted bar then a bright, bold `ZHIVE`.
        SetForegroundColor(dim),
        Print("▌ "),
        SetForegroundColor(bright),
        SetAttribute(Attribute::Bold),
        Print("ZHIVE"),
        SetAttribute(Attribute::NormalIntensity),
        ResetColor,
        Print("\n"),
    )?;
    if let Some(thread_id) = session {
        queue!(
            out,
            Print("\n"),
            // Session row: dim label, bright-neutral value.
            SetForegroundColor(dim),
            Print(format!("{:<LABEL_WIDTH$}", "Session")),
            SetForegroundColor(fg),
            Print(thread_id),
            Print("\n"),
            // Continue row: how to resume, with the `/session` verb highlighted.
            SetForegroundColor(dim),
            Print(format!("{:<LABEL_WIDTH$}", "Continue")),
            SetForegroundColor(fg),
            Print("reopen zhive, then "),
            SetForegroundColor(bright),
            Print("/session"),
            ResetColor,
            Print("\n"),
        )?;
    }
    out.flush()
}

/// Prints the exit banner to stdout, ignoring any write error.
///
/// Best-effort, mirroring the mouse-capture teardown in [`crate::run`]: a
/// terminal that has already gone away just yields nothing rather than failing
/// the clean exit the user asked for. `session` is the resumable thread id, or
/// `None` for a never-used session (wordmark only).
pub(crate) fn print(palette: &Palette, session: Option<&str>) {
    let _ = write_banner(&mut std::io::stdout(), palette, session);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders the banner to a byte buffer and returns it as a `String`.
    fn render(session: Option<&str>) -> String {
        let mut buf = Vec::new();
        write_banner(&mut buf, &Palette::default(), session).expect("vec write never fails");
        String::from_utf8(buf).expect("banner is valid utf-8")
    }

    #[test]
    fn banner_carries_brand_session_and_resume_hint() {
        let text = render(Some("thread:native/1749106321000-0"));
        assert!(text.contains("ZHIVE"), "wordmark is present");
        assert!(
            text.contains("thread:native/1749106321000-0"),
            "the session thread id is shown verbatim"
        );
        assert!(text.contains("Session"), "the session label is present");
        assert!(text.contains("Continue"), "the continue label is present");
        assert!(
            text.contains("/session"),
            "the resume hint names the picker command"
        );
    }

    #[test]
    fn empty_session_shows_only_the_wordmark() {
        // A never-used session was never persisted, so it must not print a
        // thread id `/session` could never find — just the brand.
        let text = render(None);
        assert!(text.contains("ZHIVE"), "wordmark is still present");
        assert!(!text.contains("Session"), "no session label without an id");
        assert!(!text.contains("Continue"), "no continue hint without an id");
    }

    #[test]
    fn labels_are_padded_to_one_column() {
        let text = render(Some("t"));
        // Both labels pad to LABEL_WIDTH, so each value starts at the same column.
        assert!(text.contains("Session   "), "Session padded to the gutter");
        assert!(text.contains("Continue  "), "Continue padded to the gutter");
    }

    #[test]
    fn styling_does_not_bleed_past_the_banner() {
        // After the last foreground color is set, a reset must follow so the
        // banner cannot tint the shell prompt that prints after it. Accept
        // either reset spelling so the test does not pin a crossterm detail.
        let text = render(Some("t"));
        let last_fg = text.rfind("\u{1b}[38").expect("a foreground color is set");
        let tail = &text[last_fg..];
        assert!(
            tail.contains("\u{1b}[0m") || tail.contains("\u{1b}[39m"),
            "a color reset follows the last foreground color"
        );
    }
}

// Rust guideline compliant 2026-02-21
