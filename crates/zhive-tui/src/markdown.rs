//! Markdown-to-ratatui rendering for agent messages.
//!
//! Parsing is delegated to [`tui_markdown`] (a `pulldown-cmark` driver that
//! emits `ratatui` [`Text`](ratatui::text::Text)); this module only re-themes
//! the output onto a [`Palette`] and replaces fenced code blocks with real
//! syntax highlighting via `two-face` + `syntect` (pure-Rust `fancy-regex`
//! backend).  The public [`render`] signature is unchanged so existing call
//! sites keep collecting owned `Vec<Line<'static>>`.
//!
//! Theming seam: a [`PaletteSheet`] implements [`tui_markdown::StyleSheet`] so
//! headings / inline code / links / blockquotes carry palette colors.  Code
//! blocks are post-processed: `tui-markdown` (built with `highlight-code`
//! disabled) emits fence sentinel lines (```` ```lang ```` / ```` ``` ````) and
//! plain code body lines, which this module detects and re-highlights.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use tui_markdown::{Options, StyleSheet, from_str_with_options};
use two_face::re_exports::syntect::easy::HighlightLines;
use two_face::re_exports::syntect::highlighting::FontStyle;
use two_face::re_exports::syntect::parsing::SyntaxSet;
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

use crate::theme::Palette;

/// Renders `source` Markdown into owned [`Line`]s styled with `palette`.
///
/// Parsing is delegated to [`tui_markdown`]; fenced code blocks receive
/// `syntect` syntax highlighting.  Unrecognized constructs degrade gracefully —
/// a malformed message renders as readable text rather than erroring.
///
/// # Examples
///
/// ```
/// use zhive_tui::markdown::render;
/// use zhive_tui::theme::Palette;
/// let lines = render("hello `world`", &Palette::default());
/// assert!(!lines.is_empty());
/// ```
#[must_use]
pub fn render(source: &str, palette: &Palette) -> Vec<Line<'static>> {
    // Approximate LaTeX math with Unicode before Markdown parsing (tui-markdown
    // does not parse `$…$`; a terminal cannot render real LaTeX anyway).
    let mathed = crate::math::render_math(source);
    // GFM tables (which tui-markdown does not render) are split out and drawn as
    // aligned grids; every other block goes through tui-markdown.
    let mut out: Vec<Line<'static>> = Vec::new();
    for segment in crate::table::split_segments(mathed.as_ref()) {
        match segment {
            crate::table::Segment::Markdown(text) => out.extend(render_block(&text, palette)),
            crate::table::Segment::Table(rows) => {
                out.extend(crate::table::render_table(&rows, palette));
            }
        }
    }
    out
}

/// Renders one non-table Markdown block via tui-markdown + syntect highlighting.
fn render_block(source: &str, palette: &Palette) -> Vec<Line<'static>> {
    let sheet = PaletteSheet::from_palette(palette);
    let text = from_str_with_options(source, &Options::new(sheet));

    let mut out: Vec<Line<'static>> = Vec::with_capacity(text.lines.len());
    let mut in_fence = false;
    // Highlighter state is rebuilt at each opening fence; `'static` because the
    // syntax + theme references come from the process-global `Highlighter`.
    let mut highlighter: Option<HighlightLines<'static>> = None;

    for line in text.lines {
        let raw = line_raw_text(&line);
        if raw.trim_start().starts_with("```") {
            in_fence = !in_fence;
            if in_fence {
                let lang = fence_lang(&raw);
                highlighter = Some(new_highlighter(&lang));
                let marker = if lang.is_empty() {
                    "─── code ───".to_owned()
                } else {
                    format!("─── {lang} ───")
                };
                out.push(Line::styled(marker, Style::new().fg(palette.fg_mute)));
            } else {
                out.push(Line::styled(
                    "─────────────",
                    Style::new().fg(palette.fg_mute),
                ));
                highlighter = None;
            }
            continue;
        }

        if in_fence {
            out.push(Line::from(highlight_code_line(
                &raw,
                highlighter.as_mut(),
                palette,
            )));
            continue;
        }

        out.push(inject_palette_line(line, palette));
    }

    out
}

/// Eagerly initializes the process-global syntax highlighter.
///
/// Called once at startup so the first streamed code block does not pay the
/// one-time `two-face` asset deserialization on the render path.
pub(crate) fn prewarm() {
    let _ = get_highlighter();
}

/// Concatenates the text content of a line's spans (markup-free).
fn line_raw_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Extracts the language token from a ```` ```lang ```` fence header line.
fn fence_lang(raw: &str) -> String {
    raw.trim_start()
        .strip_prefix("```")
        .map(|rest| rest.trim().to_owned())
        .unwrap_or_default()
}

// ============================================================
// Palette theming seam
// ============================================================

/// A [`tui_markdown::StyleSheet`] backed by copied [`Palette`] colors.
///
/// The trait requires `Clone + Send + Sync + 'static`, so concrete `Color`
/// values are copied out rather than holding a `&Palette` reference.
#[derive(Clone)]
struct PaletteSheet {
    accent: Color,
    fg_bright: Color,
    fg_dim: Color,
    fg_mute: Color,
    info: Color,
    warn: Color,
}

impl PaletteSheet {
    fn from_palette(p: &Palette) -> Self {
        Self {
            accent: p.accent,
            fg_bright: p.fg_bright,
            fg_dim: p.fg_dim,
            fg_mute: p.fg_mute,
            info: p.info,
            warn: p.warn,
        }
    }
}

impl StyleSheet for PaletteSheet {
    fn heading(&self, level: u8) -> Style {
        let base = Style::new().fg(self.fg_bright).add_modifier(Modifier::BOLD);
        match level {
            1 => base.add_modifier(Modifier::UNDERLINED),
            2 => base,
            _ => base.add_modifier(Modifier::ITALIC),
        }
    }
    fn code(&self) -> Style {
        Style::new().fg(self.accent)
    }
    fn link(&self) -> Style {
        Style::new()
            .fg(self.info)
            .add_modifier(Modifier::UNDERLINED)
    }
    fn blockquote(&self) -> Style {
        Style::new().fg(self.fg_dim)
    }
    fn heading_meta(&self) -> Style {
        Style::new().fg(self.fg_mute)
    }
    fn metadata_block(&self) -> Style {
        Style::new().fg(self.warn)
    }
}

/// Converts a parsed line into an owned, palette-injected [`Line`].
///
/// `tui-markdown` colors headings / blockquotes at the *line* level (`fg=Some`)
/// and inline code / links at the *span* level; plain text carries `fg=None`.
/// Injecting `palette.fg` as a line base only when the line has no color lets
/// span-level colors still override (ratatui composes span-over-line), while the
/// downstream `Paragraph` (which sets no base fg) never falls back to the
/// terminal default.  The literal heading `#` marker span that `tui-markdown`
/// keeps is stripped to match the previous renderer's bare-heading look.
fn inject_palette_line(line: Line<'_>, palette: &Palette) -> Line<'static> {
    let line_style = line.style;
    let mut spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| {
            let style = s.style;
            Span::styled(s.content.into_owned(), style)
        })
        .collect();

    // Strip tui-markdown's literal "## " heading marker (line-colored heading).
    if line_style.fg.is_some()
        && spans
            .first()
            .is_some_and(|s| is_heading_marker(s.content.as_ref()))
    {
        spans.remove(0);
    }

    let mut owned = Line::from(spans);
    owned.style = line_style;
    if owned.style.fg.is_none() {
        owned.style = owned.style.fg(palette.fg);
    }
    owned
}

/// `true` when `s` is exactly a heading marker token (`#`..=`######` + space).
fn is_heading_marker(s: &str) -> bool {
    let hashes = s.strip_suffix(' ').unwrap_or(s);
    !hashes.is_empty() && hashes.len() <= 6 && hashes.bytes().all(|b| b == b'#')
}

// ============================================================
// Syntax highlighting (two-face + syntect, fancy-regex)
// ============================================================

/// Process-global syntax + theme sets, lazily initialized once.
struct Highlighter {
    syntax_set: SyntaxSet,
    theme_set: EmbeddedLazyThemeSet,
}

/// Returns the process-global [`Highlighter`], initializing it on first use.
fn get_highlighter() -> &'static Highlighter {
    static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(|| Highlighter {
        // `extra_no_newlines` matches our per-line feed (lines carry no '\n').
        syntax_set: two_face::syntax::extra_no_newlines(),
        theme_set: two_face::theme::extra(),
    })
}

/// Builds a [`HighlightLines`] for `lang`, falling back to plain text.
fn new_highlighter(lang: &str) -> HighlightLines<'static> {
    let hl = get_highlighter();
    let syntax = hl
        .syntax_set
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| hl.syntax_set.find_syntax_plain_text());
    let theme = hl.theme_set.get(EmbeddedThemeName::TwoDark);
    HighlightLines::new(syntax, theme)
}

/// Highlights one code-body line into owned spans over the overlay background.
///
/// syntect token colors map to [`Color::Rgb`]; the theme background is ignored
/// in favor of `palette.bg_overlay` for a consistent block backdrop across
/// palettes.  A highlight failure or absent highlighter degrades to a single
/// plain span.
fn highlight_code_line(
    raw: &str,
    highlighter: Option<&mut HighlightLines<'static>>,
    palette: &Palette,
) -> Vec<Span<'static>> {
    let plain = || {
        vec![Span::styled(
            raw.to_owned(),
            Style::new().fg(palette.fg).bg(palette.bg_overlay),
        )]
    };

    let Some(hl) = highlighter else {
        return plain();
    };
    let syntax_set = &get_highlighter().syntax_set;
    let Ok(ranges) = hl.highlight_line(raw, syntax_set) else {
        return plain();
    };

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(ranges.len());
    for (style, text) in ranges {
        if text.is_empty() {
            continue;
        }
        let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
        let mut modifier = Modifier::empty();
        if style.font_style.contains(FontStyle::BOLD) {
            modifier |= Modifier::BOLD;
        }
        if style.font_style.contains(FontStyle::ITALIC) {
            modifier |= Modifier::ITALIC;
        }
        spans.push(Span::styled(
            text.to_owned(),
            Style::new()
                .fg(fg)
                .bg(palette.bg_overlay)
                .add_modifier(modifier),
        ));
    }
    if spans.is_empty() {
        return plain();
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::default()
    }

    #[test]
    fn plain_text_renders_nonempty() {
        let lines = render("just text", &palette());
        assert!(!lines.is_empty());
    }

    #[test]
    fn inline_code_does_not_panic() {
        let lines = render("a `b` c", &palette());
        assert!(!lines.is_empty());
    }

    #[test]
    fn bold_and_italic_do_not_panic() {
        let lines = render("**bold** and _italic_ and ~~strike~~", &palette());
        assert!(!lines.is_empty());
    }

    #[test]
    fn heading_strips_literal_hash_marker() {
        let lines = render("## Title", &palette());
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("Title"), "heading text must survive");
        assert!(
            !joined.contains("## "),
            "literal heading marker must be stripped, got {joined:?}"
        );
    }

    #[test]
    fn fenced_code_block_has_divider_and_body() {
        let lines = render("```rust\nfn main() {}\n```", &palette());
        // open divider + code body + close divider (at least 3 lines).
        assert!(lines.len() >= 3, "got {} lines", lines.len());
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("fn"), "code body must appear");
    }

    #[test]
    fn unknown_lang_renders_without_panic() {
        let lines = render("```cobol\nIDENTIFICATION DIVISION.\n```", &palette());
        assert!(lines.len() >= 3);
    }

    #[test]
    fn heading_marker_detector() {
        assert!(is_heading_marker("# "));
        assert!(is_heading_marker("###### "));
        assert!(is_heading_marker("##"));
        assert!(!is_heading_marker("####### "));
        assert!(!is_heading_marker("#x "));
        assert!(!is_heading_marker(""));
    }
}

// Rust guideline compliant 2026-06-05
