//! A deliberately small Markdown-to-ratatui renderer for agent messages.
//!
//! Language models lean on a narrow Markdown subset — fenced code blocks,
//! inline `` `code` ``, `**bold**`, headings, and bullet/numbered lists — so
//! this renders exactly that, mapping each construct onto a [`Palette`] token.
//! It is not a `CommonMark` implementation: unmatched markers render literally
//! rather than raising errors, which keeps a malformed message readable. No
//! external Markdown dependency is pulled in (CLAUDE.md dependency red line).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Palette;

/// Renders `source` Markdown into owned [`Line`]s styled with `palette`.
///
/// # Examples
///
/// ```
/// use zhive_tui::markdown::render;
/// use zhive_tui::theme::Palette;
/// let lines = render("hello `world`", &Palette::default());
/// assert_eq!(lines.len(), 1);
/// ```
#[must_use]
pub fn render(source: &str, palette: &Palette) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_fence = false;

    for raw in source.split('\n') {
        let trimmed_start = raw.trim_start();
        if let Some(rest) = trimmed_start.strip_prefix("```") {
            // Toggle the fenced code block; render the fence as a dim divider.
            in_fence = !in_fence;
            let label = rest.trim();
            let marker = if label.is_empty() {
                "─── code ───".to_owned()
            } else {
                format!("─── {label} ───")
            };
            out.push(Line::styled(marker, Style::new().fg(palette.fg_mute)));
            continue;
        }

        if in_fence {
            out.push(Line::styled(
                format!("  {raw}"),
                Style::new().fg(palette.fg).bg(palette.bg_overlay),
            ));
            continue;
        }

        out.push(render_block_line(raw, palette));
    }

    // A trailing fence open with no content still toggled; nothing to flush.
    out
}

/// Renders one non-fenced source line into a styled [`Line`].
fn render_block_line(raw: &str, palette: &Palette) -> Line<'static> {
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();
    let pad: String = " ".repeat(indent);

    // Headings: one or more leading '#'.
    if let Some(level) = heading_level(trimmed) {
        let text = trimmed[level..].trim_start();
        return Line::styled(
            text.to_owned(),
            Style::new()
                .fg(palette.fg_bright)
                .add_modifier(Modifier::BOLD),
        );
    }

    // Bullet list: "- ", "* ", "+ ".
    if let Some(body) = bullet_body(trimmed) {
        let mut spans = vec![
            Span::raw(pad),
            Span::styled("▸ ".to_owned(), Style::new().fg(palette.accent)),
        ];
        spans.extend(render_inline(body, palette));
        return Line::from(spans);
    }

    // Block quote: "> ".
    if let Some(body) = trimmed.strip_prefix("> ") {
        let mut spans = vec![Span::styled(
            "▏ ".to_owned(),
            Style::new().fg(palette.fg_mute),
        )];
        spans.extend(
            render_inline(body, palette)
                .into_iter()
                .map(|s| Span::styled(s.content.into_owned(), Style::new().fg(palette.fg_dim))),
        );
        return Line::from(spans);
    }

    // Plain paragraph line with inline styling.
    let mut spans = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(pad));
    }
    spans.extend(render_inline(trimmed, palette));
    Line::from(spans)
}

/// Returns the heading level (count of leading '#') if `s` is an ATX heading.
fn heading_level(s: &str) -> Option<usize> {
    let hashes = s.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && s[hashes..].starts_with(' ') {
        Some(hashes)
    } else {
        None
    }
}

/// Returns the text after a bullet marker (`- `, `* `, `+ `) if present.
fn bullet_body(s: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(marker) {
            return Some(rest);
        }
    }
    None
}

/// Splits a line into spans, handling `` `code` `` and `**bold**` markers.
///
/// Backtick code spans take precedence; unmatched markers render literally.
fn render_inline(text: &str, palette: &Palette) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut chars = text.char_indices().peekable();

    // Flushes the plain-text accumulator into a `fg` span.
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(
                std::mem::take(buf),
                Style::new().fg(palette.fg),
            ));
        }
    };

    while let Some((_, c)) = chars.next() {
        match c {
            '`' => {
                // Collect until the next backtick; if none, treat literally.
                let mut code = String::new();
                let mut closed = false;
                for (_, cc) in chars.by_ref() {
                    if cc == '`' {
                        closed = true;
                        break;
                    }
                    code.push(cc);
                }
                if closed {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::styled(code, Style::new().fg(palette.accent)));
                } else {
                    buf.push('`');
                    buf.push_str(&code);
                }
            }
            '*' if chars.peek().map(|&(_, n)| n) == Some('*') => {
                chars.next(); // consume second '*'
                let mut bold = String::new();
                let mut closed = false;
                while let Some((_, cc)) = chars.next() {
                    if cc == '*' && chars.peek().map(|&(_, n)| n) == Some('*') {
                        chars.next();
                        closed = true;
                        break;
                    }
                    bold.push(cc);
                }
                if closed {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::styled(
                        bold,
                        Style::new()
                            .fg(palette.fg_bright)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else {
                    buf.push_str("**");
                    buf.push_str(&bold);
                }
            }
            other => buf.push(other),
        }
    }
    flush(&mut buf, &mut spans);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
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
    fn plain_text_is_one_line() {
        let lines = render("just text", &palette());
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn fenced_code_block_brackets_content() {
        let lines = render("```rust\nlet x = 1;\n```", &palette());
        // open fence + code line + close fence
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn inline_code_splits_into_spans() {
        let spans = render_inline("a `b` c", &palette());
        assert!(spans.len() >= 3, "plain, code, plain");
    }

    #[test]
    fn unmatched_backtick_is_literal() {
        let spans = render_inline("a `b c", &palette());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "a `b c");
    }

    #[test]
    fn bold_marker_is_emphasized() {
        let spans = render_inline("a **b** c", &palette());
        assert!(
            spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn heading_detection() {
        assert_eq!(heading_level("## Title"), Some(2));
        assert_eq!(heading_level("###### deep"), Some(6));
        assert_eq!(heading_level("####### too deep"), None);
        assert_eq!(heading_level("#notspace"), None);
    }
}

// Rust guideline compliant 2026-02-21
