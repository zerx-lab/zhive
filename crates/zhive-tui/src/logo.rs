//! Large `ZHIVE` wordmark for the welcome screen, with click ripples.
//!
//! An "ANSI Shadow" block-letter wordmark: full blocks (`█`) form the letter
//! faces and box-drawing edges (`═║╔╗╚╝`) form a 3D extrude rendered as a fixed
//! dark shadow. Every face takes a single muted gray. At rest the art is
//! static, so an untouched welcome screen costs zero CPU.
//!
//! A left click spawns a white ripple that expands radially from the click
//! point and fades as it crosses. Ripples are independent: a fresh click never
//! cancels one already playing, so rapid clicking layers overlapping crests
//! (combined by the brightest contribution) into a continuous shimmer. All
//! shading and ripple math is integer-only, so the render is deterministic and
//! unit-testable.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Column count of every [`ART`] row (the wordmark's rendered width).
pub(crate) const WIDTH: u16 = 40;

/// Row count of the wordmark (its rendered height).
pub(crate) const HEIGHT: u16 = 6;

/// The `ZHIVE` wordmark in the "ANSI Shadow" block font.
///
/// `█` is a letter-face cell; `═║╔╗╚╝` are the 3D extrude (drawn as a fixed
/// shadow); space is blank. A single blank column separates adjacent letters so
/// the narrow `I` reads cleanly. Every row is exactly [`WIDTH`] columns — an
/// invariant the tests pin so the shading can treat the grid as rectangular.
const ART: [&str; HEIGHT as usize] = [
    "███████╗ ██╗  ██╗ ██╗ ██╗   ██╗ ███████╗",
    "╚══███╔╝ ██║  ██║ ██║ ██║   ██║ ██╔════╝",
    "  ███╔╝  ███████║ ██║ ██║   ██║ █████╗  ",
    " ███╔╝   ██╔══██║ ██║ ╚██╗ ██╔╝ ██╔══╝  ",
    "███████╗ ██║  ██║ ██║  ╚████╔╝  ███████╗",
    "╚══════╝ ╚═╝  ╚═╝ ╚═╝   ╚═══╝   ╚══════╝",
];

/// The face shade for the whole wordmark — a single muted gray.
///
/// The faint cool lean keeps it from reading dead-flat on the dark background
/// while still scanning as neutral gray.
const FACE: (u8, u8, u8) = (0x5e, 0x62, 0x69);

/// The 3D extrude's fixed shadow shade.
///
/// A single dark tint for every box-drawing edge (rather than a percentage of
/// each column's face) keeps the dim `AP` letters' depth from sinking into the
/// background while the bright `Z` keeps crisp depth — mirroring opencode's
/// fixed wordmark shadow. Chosen to sit clearly above the welcome background yet
/// still read as shadow.
const SHADOW: (u8, u8, u8) = (0x22, 0x26, 0x2d);

/// Ripple crest brightness: brightening steps toward white on the wavefront.
///
/// Also the width (in cells) of a ripple's lit band: the crest reads white and
/// the `GLOW_STEPS - 1` cells just behind it fade back to the base color.
const GLOW_STEPS: u16 = 6;

/// Frames a single ripple lives before it has crossed the wordmark and settled.
///
/// At the ~90ms render tick this is a ~1.6s flourish. The wavefront eases out
/// over these frames (see [`radius_at`]); large enough that the crest fully
/// crosses the wordmark from any click point before the ripple is pruned.
const LIFE_FRAMES: u16 = 18;

/// A ripple's maximum wavefront radius, in cells.
///
/// Large enough to cover the wordmark's far corner from any click point (a
/// corner-to-corner reach with rows weighted ×2 is ~41 cells) plus the trailing
/// fade band, so every ripple settles fully before [`LIFE_FRAMES`] elapses.
const SPAN: u16 = 48;

/// Most ripples kept alive at once — a rapid-click spam guard.
///
/// A human click storm can land a handful of clicks within one ripple's life;
/// this cap keeps the working set bounded without ever cancelling a ripple a
/// user can still see.
const MAX_RIPPLES: usize = 12;

/// One expanding click ripple: its origin cell and frames since the click.
#[derive(Debug, Clone, Copy)]
struct Ripple {
    /// Wordmark column the ripple expands from.
    col: u16,
    /// Wordmark row the ripple expands from.
    row: u16,
    /// Frames elapsed since the click; the wavefront radius grows with it.
    age: u16,
}

/// The welcome wordmark's live click ripples.
///
/// Empty at rest, so an untouched welcome screen animates nothing. Each left
/// click [`Self::spawn`]s a ripple that ages one frame per [`Self::tick`] and is
/// pruned once it has crossed; ripples never cancel one another, so rapid clicks
/// layer into a continuous shimmer.
///
/// # Examples
///
/// ```ignore
/// let mut ripples = Ripples::default();
/// assert!(!ripples.is_active());
/// ripples.spawn(6, 3);
/// assert!(ripples.is_active());
/// assert_eq!(ripples.render().len(), usize::from(HEIGHT));
/// ```
#[derive(Debug, Clone, Default)]
pub(crate) struct Ripples {
    active: Vec<Ripple>,
}

impl Ripples {
    /// Adds a ripple expanding from wordmark cell `(col, row)`.
    ///
    /// Ripples already playing keep going; once `MAX_RIPPLES` are live the
    /// oldest is dropped so a click storm cannot grow the set without bound.
    pub(crate) fn spawn(&mut self, col: u16, row: u16) {
        if self.active.len() >= MAX_RIPPLES {
            self.active.remove(0);
        }
        self.active.push(Ripple { col, row, age: 0 });
    }

    /// Ages every ripple one frame and prunes those that have settled.
    pub(crate) fn tick(&mut self) {
        for ripple in &mut self.active {
            ripple.age = ripple.age.saturating_add(1);
        }
        self.active.retain(|ripple| ripple.age < LIFE_FRAMES);
    }

    /// Returns `true` while any ripple is still playing.
    #[must_use]
    pub(crate) fn is_active(&self) -> bool {
        !self.active.is_empty()
    }

    /// Renders the wordmark as colored lines for the current ripple state.
    ///
    /// Returns exactly [`HEIGHT`] lines, each [`WIDTH`] columns wide.
    #[must_use]
    pub(crate) fn render(&self) -> Vec<Line<'static>> {
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
                let level = glow_level(&self.active, col, row);
                let color = cell_color(ch, level);
                spans.push(Span::styled(glyph, Style::new().fg(color)));
            }
            lines.push(Line::from(spans));
        }
        lines
    }
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

/// Resolved color of a lit cell: the muted face (or fixed shadow), then
/// brightened toward white by the ripple `level`.
fn cell_color(ch: char, level: u16) -> Color {
    // Box-drawing edges are the 3D extrude; faces take the single muted shade.
    let base = if ch == '█' { FACE } else { SHADOW };
    let lit = (
        lerp(base.0, 0xff, level, GLOW_STEPS),
        lerp(base.1, 0xff, level, GLOW_STEPS),
        lerp(base.2, 0xff, level, GLOW_STEPS),
    );
    Color::Rgb(lit.0, lit.1, lit.2)
}

/// Integer channel lerp: `a + (b - a) * num / den`, clamped to a byte.
fn lerp(a: u8, b: u8, num: u16, den: u16) -> u8 {
    let a = i32::from(a);
    let b = i32::from(b);
    let v = a + (b - a) * i32::from(num) / i32::from(den.max(1));
    u8::try_from(v.clamp(0, 255)).unwrap_or(0)
}

/// Combined ripple brightness of cell `(col, row)`, in `0..=GLOW_STEPS`.
///
/// 0 at rest. Each live ripple contributes a crest that brightens cells on its
/// wavefront and fades over the cells just behind; overlapping ripples combine
/// by the brightest contribution, so layered clicks never dim one another.
fn glow_level(ripples: &[Ripple], col: u16, row: u16) -> u16 {
    let mut level = 0;
    for ripple in ripples {
        level = level.max(ripple_glow(*ripple, col, row));
    }
    level
}

/// One ripple's brightness at cell `(col, row)`, in `0..=GLOW_STEPS`.
///
/// 0 until the wavefront reaches the cell, then [`GLOW_STEPS`] on the crest
/// fading to 0 over the next `GLOW_STEPS` cells behind it. Rows are weighted ×2
/// so the wavefront reads round on a terminal's tall cells.
fn ripple_glow(ripple: Ripple, col: u16, row: u16) -> u16 {
    let radius = radius_at(ripple.age);
    let dx = i32::from(col) - i32::from(ripple.col);
    let dy = (i32::from(row) - i32::from(ripple.row)) * 2;
    let dist = u32::try_from(dx * dx + dy * dy).unwrap_or(0).isqrt();
    if dist > radius {
        return 0; // the wavefront has not reached this cell yet
    }
    // Brightest on the crest, fading over the cells just behind it.
    let behind = u16::try_from(radius - dist).unwrap_or(GLOW_STEPS);
    GLOW_STEPS.saturating_sub(behind)
}

/// A ripple's wavefront radius (in cells) at frame `age`, easing outward.
///
/// Quadratic ease-out — fast at the click, decelerating as it crosses: the
/// `SPAN * (1 - ((LIFE - age) / LIFE)^2)` curve, kept in integer arithmetic so
/// the result is deterministic and testable.
fn radius_at(age: u16) -> u32 {
    let life = u32::from(LIFE_FRAMES);
    let age = u32::from(age).min(life);
    let remain = life - age;
    u32::from(SPAN) * (life * life - remain * remain) / (life * life)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawns a single ripple at the given origin and age (for crest math).
    fn one(col: u16, row: u16, age: u16) -> Vec<Ripple> {
        vec![Ripple { col, row, age }]
    }

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
        assert_eq!(Ripples::default().render().len(), usize::from(HEIGHT));
    }

    #[test]
    fn at_rest_every_cell_is_unbrightened() {
        for col in 0u16..WIDTH {
            for row in 0u16..HEIGHT {
                assert_eq!(glow_level(&[], col, row), 0);
            }
        }
    }

    #[test]
    fn glow_level_never_exceeds_the_crest() {
        for age in 0u16..=LIFE_FRAMES {
            for col in 0u16..WIDTH {
                for row in 0u16..HEIGHT {
                    assert!(glow_level(&one(4, 3, age), col, row) <= GLOW_STEPS);
                }
            }
        }
    }

    #[test]
    fn face_is_a_single_muted_shade() {
        // At rest a face cell is the single muted FACE everywhere — no two-tone
        // split — while a box-drawing edge stays the darker SHADOW.
        assert_eq!(cell_color('█', 0), Color::Rgb(FACE.0, FACE.1, FACE.2));
        assert_eq!(cell_color('║', 0), Color::Rgb(SHADOW.0, SHADOW.1, SHADOW.2));
    }

    #[test]
    fn ripple_crest_is_white_at_the_origin_on_click() {
        // The instant a click lands (a freshly spawned, age-0 ripple), the
        // origin cell is the crest and reads pure white.
        assert_eq!(glow_level(&one(6, 3, 0), 6, 3), GLOW_STEPS);
        assert_eq!(cell_color('█', GLOW_STEPS), Color::Rgb(0xff, 0xff, 0xff));
    }

    #[test]
    fn ripple_expands_outward_over_frames() {
        // A cell four columns from the origin lights only once the wavefront
        // has had time to travel there.
        let (oc, or) = (6u16, 3u16);
        let (fc, fr) = (10u16, 3u16);
        assert_eq!(
            glow_level(&one(oc, or, 0), fc, fr),
            0,
            "wavefront should not have reached the cell yet"
        );
        assert!(
            glow_level(&one(oc, or, 1), fc, fr) > 0,
            "wavefront should reach the cell after a few frames"
        );
    }

    #[test]
    fn overlapping_ripples_combine_by_brightest() {
        // Two simultaneous clicks: each origin stays a white crest, proving a
        // second ripple never dims the first.
        let pair = vec![
            Ripple {
                col: 3,
                row: 2,
                age: 0,
            },
            Ripple {
                col: 18,
                row: 4,
                age: 0,
            },
        ];
        assert_eq!(glow_level(&pair, 3, 2), GLOW_STEPS);
        assert_eq!(glow_level(&pair, 18, 4), GLOW_STEPS);
    }

    #[test]
    fn rapid_clicks_accumulate_then_cap() {
        let mut ripples = Ripples::default();
        for _ in 0..(MAX_RIPPLES + 5) {
            ripples.spawn(4, 3);
        }
        assert_eq!(
            ripples.active.len(),
            MAX_RIPPLES,
            "the live set is bounded by the spam cap"
        );
    }

    #[test]
    fn a_ripple_ages_out_and_prunes() {
        let mut ripples = Ripples::default();
        ripples.spawn(4, 3);
        for _ in 0..LIFE_FRAMES {
            assert!(ripples.is_active());
            ripples.tick();
        }
        assert!(!ripples.is_active(), "a crossed ripple is pruned");
    }
}

// Rust guideline compliant 2026-02-21
