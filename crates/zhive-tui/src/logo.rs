//! Large `ZAP` wordmark for the welcome screen, with a click ripple.
//!
//! An "ANSI Shadow" block-letter wordmark: full blocks (`█`) form the letter
//! faces, and box-drawing edges (`═║╔╗╚╝`) form a 3D extrude rendered darker.
//! A blue→violet gradient runs across the columns. At rest the wordmark is
//! static, so an untouched welcome screen costs zero CPU; a left click sends a
//! white ripple expanding radially from the click point, which fades as it
//! crosses.
//!
//! Colors are fixed here (a dedicated brand palette) rather than themed —
//! theming lands later. All gradient and ripple math is integer-only so the
//! render is deterministic and unit-testable.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Column count of every [`ART`] row (the wordmark's rendered width).
pub(crate) const WIDTH: u16 = 24;

/// Row count of the wordmark (its rendered height).
pub(crate) const HEIGHT: u16 = 6;

/// The `ZAP` wordmark in the "ANSI Shadow" block font.
///
/// `█` is a letter-face cell; `═║╔╗╚╝` are the 3D extrude (drawn darker); space
/// is blank. Every row is exactly [`WIDTH`] columns — an invariant the tests
/// pin so the gradient can treat the grid as rectangular.
const ART: [&str; HEIGHT as usize] = [
    "███████╗ █████╗ ██████╗ ",
    "╚══███╔╝██╔══██╗██╔══██╗",
    "  ███╔╝ ███████║██████╔╝",
    " ███╔╝  ██╔══██║██╔═══╝ ",
    "███████╗██║  ██║██║     ",
    "╚══════╝╚═╝  ╚═╝╚═╝     ",
];

/// Gradient start (left edge): azure — opencode's `secondary` blue.
const GRAD_START: (u8, u8, u8) = (0x5c, 0x9c, 0xf5);

/// Gradient end (right edge): violet — opencode's `accent` purple.
const GRAD_END: (u8, u8, u8) = (0x9d, 0x7c, 0xd8);

/// Brightness the 3D extrude keeps, as a percent of the face color.
///
/// The box-drawing edges are the letters' shadow; dimming them to roughly half
/// gives the faces depth without a second hardcoded color.
const SHADOW_PCT: u16 = 45;

/// Ripple crest level: the number of brightening steps toward white.
///
/// [`glow_level`] returns `0..=GLOW_STEPS`; 0 is the resting gradient color and
/// `GLOW_STEPS` is pure white on the wavefront crest.
const GLOW_STEPS: u16 = 4;

/// Frames a click ripple lasts before the wordmark settles back to rest.
///
/// Also the initial countdown of `App::logo_pulse`. The wavefront radius grows
/// one cell per frame, so this is large enough for the ripple to fully cross
/// the wordmark from any click point; at the ~90ms tick it reads as a ~2.5s
/// flourish.
pub(crate) const SWEEP_FRAMES: u16 = 28;

/// Renders the wordmark as colored lines.
///
/// `pulse` is the remaining ripple frames (0 = at rest); `origin_col`/
/// `origin_row` are the click cell within the wordmark the ripple expands from.
/// Returns exactly [`HEIGHT`] lines, each [`WIDTH`] columns wide.
pub(crate) fn render(pulse: u16, origin_col: u16, origin_row: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(ART.len());
    for (row, art) in ART.iter().enumerate() {
        let row = u16::try_from(row).unwrap_or(0);
        let mut spans = Vec::with_capacity(usize::from(WIDTH));
        for (col, ch) in art.chars().enumerate() {
            let glyph = glyph_str(ch);
            if glyph == " " {
                spans.push(Span::raw(" "));
                continue;
            }
            let col = u16::try_from(col).unwrap_or(0);
            let level = glow_level(pulse, origin_col, origin_row, col, row);
            let color = cell_color(ch, col, level);
            spans.push(Span::styled(glyph, Style::new().fg(color)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Maps an [`ART`] character to a borrowed glyph (or `" "` for blanks).
fn glyph_str(ch: char) -> &'static str {
    match ch {
        '█' => "█",
        '═' => "═",
        '║' => "║",
        '╔' => "╔",
        '╗' => "╗",
        '╚' => "╚",
        '╝' => "╝",
        _ => " ",
    }
}

/// Resolved color of a lit cell: gradient base, dimmed if it is shadow, then
/// brightened toward white by the ripple `level`.
fn cell_color(ch: char, col: u16, level: u16) -> Color {
    let base = gradient(col);
    let base = if ch == '█' {
        base
    } else {
        // Box-drawing edges are the 3D extrude: a dimmer shade of the face.
        (
            lerp(0, base.0, SHADOW_PCT, 100),
            lerp(0, base.1, SHADOW_PCT, 100),
            lerp(0, base.2, SHADOW_PCT, 100),
        )
    };
    let lit = (
        lerp(base.0, 0xff, level, GLOW_STEPS),
        lerp(base.1, 0xff, level, GLOW_STEPS),
        lerp(base.2, 0xff, level, GLOW_STEPS),
    );
    Color::Rgb(lit.0, lit.1, lit.2)
}

/// Blue→violet gradient color at column `col`.
fn gradient(col: u16) -> (u8, u8, u8) {
    let span = WIDTH - 1;
    let n = col.min(span);
    (
        lerp(GRAD_START.0, GRAD_END.0, n, span),
        lerp(GRAD_START.1, GRAD_END.1, n, span),
        lerp(GRAD_START.2, GRAD_END.2, n, span),
    )
}

/// Integer channel lerp: `a + (b - a) * num / den`, clamped to a byte.
fn lerp(a: u8, b: u8, num: u16, den: u16) -> u8 {
    let a = i32::from(a);
    let b = i32::from(b);
    let v = a + (b - a) * i32::from(num) / i32::from(den.max(1));
    u8::try_from(v.clamp(0, 255)).unwrap_or(0)
}

/// Ripple brightness level of cell `(col, row)`, in `0..=GLOW_STEPS`.
///
/// 0 at rest (or once the wavefront has passed). While a click ripple plays, a
/// wavefront expands one cell per frame from the click origin; cells on the
/// crest read white and the few cells just behind fade back to base. Rows are
/// weighted ×2 so the ripple is round on a terminal's tall cells.
fn glow_level(pulse: u16, origin_col: u16, origin_row: u16, col: u16, row: u16) -> u16 {
    if pulse == 0 {
        return 0;
    }
    // Frames elapsed since the click; the wavefront radius equals this.
    let radius = u32::from(SWEEP_FRAMES.saturating_sub(pulse));
    let dx = i32::from(col) - i32::from(origin_col);
    let dy = (i32::from(row) - i32::from(origin_row)) * 2;
    let dist = u32::try_from(dx * dx + dy * dy).unwrap_or(0).isqrt();
    if dist > radius {
        return 0; // the wavefront has not reached this cell yet
    }
    // Brightest on the crest, fading over the next few cells behind it.
    match radius - dist {
        0 => 4,
        1 => 3,
        2 => 2,
        3 => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_row_is_full_width() {
        for row in ART {
            assert_eq!(
                row.chars().count(),
                usize::from(WIDTH),
                "glyph row width must match WIDTH"
            );
        }
    }

    #[test]
    fn render_emits_height_lines() {
        assert_eq!(render(0, 0, 0).len(), usize::from(HEIGHT));
    }

    #[test]
    fn at_rest_every_cell_is_unbrightened() {
        for col in 0u16..WIDTH {
            for row in 0u16..HEIGHT {
                assert_eq!(glow_level(0, 0, 0, col, row), 0);
            }
        }
    }

    #[test]
    fn glow_level_never_exceeds_the_crest() {
        for pulse in 0u16..=SWEEP_FRAMES {
            for col in 0u16..WIDTH {
                for row in 0u16..HEIGHT {
                    assert!(glow_level(pulse, 4, 3, col, row) <= GLOW_STEPS);
                }
            }
        }
    }

    #[test]
    fn gradient_runs_start_to_end() {
        assert_eq!(gradient(0), GRAD_START);
        assert_eq!(gradient(WIDTH - 1), GRAD_END);
    }

    #[test]
    fn ripple_crest_is_white_at_the_origin_on_click() {
        // The instant a click lands (full pulse), the origin cell is the crest.
        assert_eq!(glow_level(SWEEP_FRAMES, 6, 3, 6, 3), GLOW_STEPS);
        assert_eq!(cell_color('█', 6, GLOW_STEPS), Color::Rgb(0xff, 0xff, 0xff));
    }

    #[test]
    fn ripple_expands_outward_over_frames() {
        // A cell four columns from the origin lights only once the wavefront
        // has had time to travel there.
        let (oc, or) = (6u16, 3u16);
        let (fc, fr) = (10u16, 3u16);
        assert_eq!(
            glow_level(SWEEP_FRAMES, oc, or, fc, fr),
            0,
            "wavefront should not have reached the cell yet"
        );
        assert!(
            glow_level(SWEEP_FRAMES - 4, oc, or, fc, fr) > 0,
            "wavefront should reach the cell after a few frames"
        );
    }
}

// Rust guideline compliant 2026-02-21
