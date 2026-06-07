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

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
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

/// State for the fenced code block currently being collected.
struct OpenFence {
    /// Language token from the opening fence (empty for a bare ```` ``` ````).
    lang: String,
    /// Raw body lines gathered until the closing fence (or buffer EOF).
    body: Vec<String>,
}

/// Renders one non-table Markdown block via tui-markdown + syntect highlighting.
///
/// A fenced code block's body is buffered and highlighted as a unit when its
/// closing fence arrives, so the block is memoized by content (see
/// [`render_code_body`]): a code block that sits above the in-flight tail of a
/// streaming reply is highlighted once instead of re-highlighted on every
/// frame. (`pulldown-cmark` synthesizes a closing fence at EOF, so a still-open
/// block is closed here too; its body changes each token and simply misses the
/// cache until it settles.)
fn render_block(source: &str, palette: &Palette) -> Vec<Line<'static>> {
    let sheet = PaletteSheet::from_palette(palette);
    let text = from_str_with_options(source, &Options::new(sheet));

    let mut out: Vec<Line<'static>> = Vec::with_capacity(text.lines.len());
    let mut fence: Option<OpenFence> = None;

    for line in text.lines {
        let raw = line_raw_text(&line);
        if raw.trim_start().starts_with("```") {
            if let Some(open) = fence.take() {
                // Closing fence: the block is complete, so render it through the
                // content cache, then draw the bottom divider.
                out.extend(render_code_body(&open.lang, &open.body, palette));
                out.push(Line::styled(
                    "─────────────",
                    Style::new().fg(palette.fg_mute),
                ));
            } else {
                // Opening fence: emit the header marker and start collecting.
                let lang = fence_lang(&raw);
                let marker = if lang.is_empty() {
                    "─── code ───".to_owned()
                } else {
                    format!("─── {lang} ───")
                };
                out.push(Line::styled(marker, Style::new().fg(palette.fg_mute)));
                fence = Some(OpenFence {
                    lang,
                    body: Vec::new(),
                });
            }
            continue;
        }

        if let Some(open) = fence.as_mut() {
            open.body.push(raw);
            continue;
        }

        out.push(inject_palette_line(line, palette));
    }

    // Fallback: a fence left unclosed in tui-markdown's output (no synthesized
    // close) flushes its body without a bottom divider, preserving prior
    // behavior for any such edge case.
    if let Some(open) = fence {
        out.extend(render_code_body(&open.lang, &open.body, palette));
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

/// Hard cap on the closed-code-block highlight cache (see [`render_code_body`]).
///
/// Highlighted blocks are small `Vec<Line>`s and a session accrues few of them;
/// the cap is a runaway guard, cleared wholesale on overflow rather than evicted
/// per entry (a palette change invalidates every entry anyway, so precision here
/// buys nothing).
const CODE_BLOCK_CACHE_CAP: usize = 512;

thread_local! {
    /// Per-thread memo of highlighted *closed* code blocks, keyed by language +
    /// body + the palette colors that affect the output. The TUI renders on one
    /// thread, so a `thread_local` avoids locking; tests on other threads each
    /// get their own (correct, independent) map.
    static CODE_BLOCK_CACHE: RefCell<HashMap<u64, Vec<Line<'static>>>> =
        RefCell::new(HashMap::new());
}

/// Highlights a fenced code block's `body`, memoizing the result by content.
///
/// The highlight (the costly `syntect` pass) is computed once per distinct
/// block content and cloned on later frames — the key to smooth streaming when
/// completed code blocks sit above the in-flight tail. The tail block changes
/// each token, so it simply misses the cache until its content settles; those
/// transient entries are bounded by [`CODE_BLOCK_CACHE_CAP`].
fn render_code_body(lang: &str, body: &[String], palette: &Palette) -> Vec<Line<'static>> {
    let key = code_block_key(lang, body, palette);
    if let Some(hit) = CODE_BLOCK_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let lines = highlight_body(lang, body, palette);
    CODE_BLOCK_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() >= CODE_BLOCK_CACHE_CAP {
            map.clear();
        }
        map.insert(key, lines.clone());
    });
    lines
}

/// Highlights `body` line-by-line with a single fence-scoped highlighter.
fn highlight_body(lang: &str, body: &[String], palette: &Palette) -> Vec<Line<'static>> {
    // Highlighter state is rebuilt per block; `'static` because the syntax +
    // theme references come from the process-global `Highlighter`.
    let mut highlighter: HighlightLines<'static> = new_highlighter(lang);
    body.iter()
        .map(|raw| Line::from(highlight_code_line(raw, Some(&mut highlighter), palette)))
        .collect()
}

/// Content hash keying the closed-block cache: language, body, and the palette
/// colors [`highlight_code_line`] reads (so a `/theme` switch can't serve a
/// stale-colored block).
fn code_block_key(lang: &str, body: &[String], palette: &Palette) -> u64 {
    let mut h = DefaultHasher::new();
    lang.hash(&mut h);
    for line in body {
        line.hash(&mut h);
        // Separator so ["ab"] and ["a","b"] cannot collide.
        0xffu8.hash(&mut h);
    }
    palette.bg_overlay.hash(&mut h);
    palette.fg.hash(&mut h);
    h.finish()
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
    fn closed_code_block_cache_hit_matches_cold_render() {
        // Second render of the same closed block must come from the cache and be
        // byte-for-byte identical to the first (cold) render.
        let src = "intro\n\n```rust\nfn main() { let x = 1; }\n```\n\ntail";
        let p = palette();
        let first = render(src, &p);
        let second = render(src, &p);
        assert_eq!(first, second, "cached closed-block render must match cold");
    }

    #[test]
    fn unclosed_fence_renders_body_without_panic() {
        // An in-flight (unclosed) fence — pulldown-cmark synthesizes a close at
        // EOF — must still render its body. Two renders match (cache-consistent).
        let src = "```rust\nfn main() {";
        let p = palette();
        let a = render(src, &p);
        let b = render(src, &p);
        assert_eq!(a, b, "repeated render of an in-flight block must be stable");
        let joined: String = a
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("fn main"), "in-flight body must render");
    }

    #[test]
    fn distinct_closed_blocks_do_not_cross_contaminate() {
        // Two different blocks must cache and render independently — a content
        // key collision would leak one block's highlight into the other.
        let p = palette();
        let body_of = |ls: &[Line<'_>]| -> String {
            ls.iter()
                .flat_map(|l| l.spans.iter())
                .map(|s| s.content.as_ref().to_owned())
                .collect()
        };
        let a = render("```rust\nlet a = 1;\n```", &p);
        let b = render("```rust\nlet b = 2;\n```", &p);
        assert!(body_of(&a).contains("let a = 1;"));
        assert!(body_of(&b).contains("let b = 2;"));
        assert!(!body_of(&b).contains("let a = 1;"));
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

    /// Perf gate: while a reply streams, a completed code block above the
    /// in-flight tail must be served from the content cache, not re-highlighted
    /// every frame — keeping each streaming repaint well under the reveal tick.
    #[test]
    #[ignore = "perf gate; run manually with --ignored --nocapture"]
    fn closed_block_above_streaming_tail_is_cheap() {
        use std::fmt::Write as _;
        use std::time::Instant;

        let mut prefix = String::from("Here is the implementation:\n\n```rust\n");
        for i in 0..200 {
            let _ = writeln!(prefix, "fn function_{i}(x: u64) -> u64 {{ x * 2 + {i} }}");
        }
        prefix.push_str("```\n\n");
        let p = palette();
        let _ = render(&prefix, &p); // warm the highlighter + cache the closed block

        let t0 = Instant::now();
        for frame in 0..30 {
            let mut buf = prefix.clone();
            let _ = write!(buf, "Now streaming an explanation, token {frame} ...");
            let _ = render(&buf, &p);
        }
        let per_frame = t0.elapsed() / 30;
        println!("[stream-perf] per-frame with cached closed block = {per_frame:?}");
        assert!(
            per_frame.as_millis() < 9,
            "streaming frame with a cached closed block must be <9ms, got {per_frame:?}"
        );
    }
}

// Rust guideline compliant 2026-06-05
