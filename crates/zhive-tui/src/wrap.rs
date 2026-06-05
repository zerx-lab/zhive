//! Width-aware, style-preserving word wrapping for styled [`Line`]s.
//!
//! The conversation lays messages out with an 8-cell role gutter, so wrapped
//! continuation rows must be re-indented under that gutter — something
//! ratatui's built-in `Wrap` (which wraps to column 0) cannot do. This module
//! pre-wraps a [`Line`] to a target width, preserving each span's [`Style`] and
//! measuring width with [`unicode_width`] so CJK glyphs count as two cells.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// A run of same-style text that is either all-whitespace or all-non-space.
struct Token {
    text: String,
    style: Style,
    is_space: bool,
    width: usize,
}

/// Splits a line's spans into whitespace / non-whitespace tokens.
fn tokenize(line: &Line) -> Vec<Token> {
    let mut tokens = Vec::new();
    for span in &line.spans {
        // Fold the line-level base style into each span: ratatui's Paragraph does
        // not propagate Line::style to cells for a pre-wrapped line, so the span
        // must carry the effective color (span style wins over the line base).
        let style = line.style.patch(span.style);
        let mut current = String::new();
        let mut current_space: Option<bool> = None;
        for c in span.content.chars() {
            let is_space = c == ' ' || c == '\t';
            if current_space != Some(is_space) && !current.is_empty() {
                let text = std::mem::take(&mut current);
                let width = UnicodeWidthStr::width(text.as_str());
                tokens.push(Token {
                    text,
                    style,
                    is_space: current_space.unwrap_or(false),
                    width,
                });
            }
            current.push(c);
            current_space = Some(is_space);
        }
        if !current.is_empty() {
            let width = UnicodeWidthStr::width(current.as_str());
            tokens.push(Token {
                text: current,
                style,
                is_space: current_space.unwrap_or(false),
                width,
            });
        }
    }
    tokens
}

/// Hard-splits a too-wide word into chunks no wider than `max` cells.
///
/// Returns `(chunk, display_width)` pairs; the caller re-applies the span style.
fn hard_split(text: &str, max: usize) -> Vec<(String, usize)> {
    let mut chunks = Vec::new();
    let mut buf = String::new();
    let mut buf_w = 0usize;
    for c in text.chars() {
        let cw = UnicodeWidthStr::width(c.to_string().as_str());
        if buf_w + cw > max && !buf.is_empty() {
            chunks.push((std::mem::take(&mut buf), buf_w));
            buf_w = 0;
        }
        buf.push(c);
        buf_w += cw;
    }
    if !buf.is_empty() {
        chunks.push((buf, buf_w));
    }
    chunks
}

/// Wraps `line` to `max` cells, returning one or more styled [`Line`]s.
///
/// Whitespace at a wrap boundary is dropped; words wider than `max` are
/// hard-split. A `max` of zero returns the line unchanged to avoid a stall.
///
/// # Examples
///
/// ```
/// use ratatui::text::Line;
/// use zhive_tui::wrap::wrap_line;
/// let wrapped = wrap_line(&Line::from("aaa bbb ccc"), 4);
/// assert!(wrapped.len() >= 2);
/// ```
#[must_use]
pub fn wrap_line(line: &Line, max: u16) -> Vec<Line<'static>> {
    let max = usize::from(max);
    if max == 0 {
        return vec![clone_line(line)];
    }
    let tokens = tokenize(line);
    // Preserve the input line's base style on every wrapped row: ratatui renders
    // line.style as the base under each span, so plain text keeps palette.fg,
    // headings keep fg_bright+BOLD, and `Line::styled` dividers/headers keep
    // their color after wrapping.
    let line_style = line.style;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    let flush = |cur: &mut Vec<Span<'static>>, cur_w: &mut usize, out: &mut Vec<Line<'static>>| {
        // Drop trailing whitespace so wrapped rows have no dangling spaces.
        while cur
            .last()
            .is_some_and(|s| s.content.chars().all(char::is_whitespace))
        {
            cur.pop();
        }
        let mut wrapped = Line::from(std::mem::take(cur));
        wrapped.style = line_style;
        out.push(wrapped);
        *cur_w = 0;
    };

    for token in tokens {
        if token.is_space {
            if cur_w == 0 {
                continue; // no leading whitespace on a fresh line
            }
            if cur_w + token.width <= max {
                cur.push(Span::styled(token.text, token.style));
                cur_w += token.width;
            } else {
                flush(&mut cur, &mut cur_w, &mut out);
            }
            continue;
        }
        // A word.
        if cur_w + token.width <= max {
            cur.push(Span::styled(token.text, token.style));
            cur_w += token.width;
        } else if token.width > max {
            // Hard-split the oversized word across lines.
            if cur_w > 0 {
                flush(&mut cur, &mut cur_w, &mut out);
            }
            for (chunk, w) in hard_split(&token.text, max) {
                if cur_w + w > max && cur_w > 0 {
                    flush(&mut cur, &mut cur_w, &mut out);
                }
                cur.push(Span::styled(chunk, token.style));
                cur_w += w;
            }
        } else {
            flush(&mut cur, &mut cur_w, &mut out);
            cur.push(Span::styled(token.text, token.style));
            cur_w += token.width;
        }
    }
    if !cur.is_empty() {
        flush(&mut cur, &mut cur_w, &mut out);
    }
    if out.is_empty() {
        let mut empty = Line::from(Vec::new());
        empty.style = line_style;
        out.push(empty);
    }
    out
}

/// Clones a borrowed [`Line`] into an owned (`'static`) one.
fn clone_line(line: &Line) -> Line<'static> {
    let mut cloned = Line::from(
        line.spans
            .iter()
            .map(|s| Span::styled(s.content.clone().into_owned(), s.style))
            .collect::<Vec<_>>(),
    );
    cloned.style = line.style;
    cloned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_line_level_style() {
        use ratatui::style::{Color, Style};
        let line = Line::from("some long text that wraps").style(Style::new().fg(Color::Red));
        let wrapped = wrap_line(&line, 8);
        assert!(wrapped.len() >= 2, "should wrap into multiple rows");
        assert!(
            wrapped.iter().all(|l| l.style.fg == Some(Color::Red)),
            "every wrapped row must keep the input line's base style"
        );
    }

    #[test]
    fn wraps_on_word_boundaries() {
        let wrapped = wrap_line(&Line::from("alpha beta gamma"), 11);
        assert_eq!(wrapped.len(), 2);
        let first: String = wrapped[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(first, "alpha beta");
    }

    #[test]
    fn hard_splits_oversized_word() {
        let wrapped = wrap_line(&Line::from("abcdefghij"), 4);
        assert_eq!(wrapped.len(), 3);
    }

    #[test]
    fn cjk_counts_two_cells() {
        // Four wide chars = 8 cells; at width 4 that is two per line.
        let wrapped = wrap_line(&Line::from("你好世界"), 4);
        assert_eq!(wrapped.len(), 2);
    }

    #[test]
    fn zero_width_returns_unchanged() {
        let wrapped = wrap_line(&Line::from("anything"), 0);
        assert_eq!(wrapped.len(), 1);
    }
}

// Rust guideline compliant 2026-02-21
