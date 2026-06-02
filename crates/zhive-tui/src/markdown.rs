//! A deliberately small Markdown-to-ratatui renderer for agent messages.
//!
//! Language models lean on a narrow Markdown subset — fenced code blocks,
//! inline `` `code` ``, `**bold**`, `_italic_`, `~~strike~~`, `[text](url)`,
//! headings, and bullet/numbered lists — so this renders exactly that, mapping
//! each construct onto a [`Palette`] token.  It is not a `CommonMark`
//! implementation: unmatched markers render literally rather than raising errors,
//! which keeps a malformed message readable.  No external Markdown dependency is
//! pulled in (CLAUDE.md dependency red line).
//!
//! Fenced code blocks (triple-backtick with a language tag) additionally receive
//! lightweight keyword highlighting for Rust and a handful of common languages.
//! The keyword tables are inlined here — no parser crate is added.

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
    let mut fence_lang = String::new();

    for raw in source.split('\n') {
        let trimmed_start = raw.trim_start();
        if let Some(rest) = trimmed_start.strip_prefix("```") {
            // Toggle the fenced code block; render the fence as a dim divider.
            in_fence = !in_fence;
            if in_fence {
                rest.trim().clone_into(&mut fence_lang);
                let marker = if fence_lang.is_empty() {
                    "─── code ───".to_owned()
                } else {
                    format!("─── {fence_lang} ───")
                };
                out.push(Line::styled(marker, Style::new().fg(palette.fg_mute)));
            } else {
                out.push(Line::styled(
                    "─────────────",
                    Style::new().fg(palette.fg_mute),
                ));
                fence_lang.clear();
            }
            continue;
        }

        if in_fence {
            let spans = highlight_code_line(raw, &fence_lang, palette);
            out.push(Line::from(spans));
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

/// Splits a line into spans, handling inline markup.
///
/// Precedence (highest first): backtick code spans, `**bold**`, `_italic_`,
/// `*italic*`, `~~strikethrough~~`, `[text](url)` links.  Unmatched markers
/// render literally rather than raising errors.
#[expect(
    clippy::too_many_lines,
    reason = "exhaustive inline-markup dispatch; each arm is tightly scoped and splitting into helper functions would scatter related pattern pairs"
)]
pub(crate) fn render_inline(text: &str, palette: &Palette) -> Vec<Span<'static>> {
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
            // ── inline code ──────────────────────────────────────────────────
            '`' => {
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
            // ── bold **…** ───────────────────────────────────────────────────
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
            // ── italic *…* (single star, not double) ─────────────────────────
            '*' => {
                let mut inner = String::new();
                let mut closed = false;
                for (_, cc) in chars.by_ref() {
                    if cc == '*' {
                        closed = true;
                        break;
                    }
                    inner.push(cc);
                }
                if closed && !inner.is_empty() {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::styled(
                        inner,
                        Style::new().fg(palette.fg).add_modifier(Modifier::ITALIC),
                    ));
                } else {
                    buf.push('*');
                    buf.push_str(&inner);
                }
            }
            // ── italic _…_ ───────────────────────────────────────────────────
            '_' => {
                let mut inner = String::new();
                let mut closed = false;
                for (_, cc) in chars.by_ref() {
                    if cc == '_' {
                        closed = true;
                        break;
                    }
                    inner.push(cc);
                }
                if closed && !inner.is_empty() {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::styled(
                        inner,
                        Style::new().fg(palette.fg).add_modifier(Modifier::ITALIC),
                    ));
                } else {
                    buf.push('_');
                    buf.push_str(&inner);
                }
            }
            // ── strikethrough ~~…~~ ──────────────────────────────────────────
            '~' if chars.peek().map(|&(_, n)| n) == Some('~') => {
                chars.next(); // consume second '~'
                let mut inner = String::new();
                let mut closed = false;
                while let Some((_, cc)) = chars.next() {
                    if cc == '~' && chars.peek().map(|&(_, n)| n) == Some('~') {
                        chars.next();
                        closed = true;
                        break;
                    }
                    inner.push(cc);
                }
                if closed && !inner.is_empty() {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::styled(
                        inner,
                        Style::new()
                            .fg(palette.fg_dim)
                            .add_modifier(Modifier::CROSSED_OUT),
                    ));
                } else {
                    buf.push_str("~~");
                    buf.push_str(&inner);
                }
            }
            // ── link [text](url) ─────────────────────────────────────────────
            '[' => {
                // Try to parse [label](url).
                let mut label = String::new();
                let mut found_bracket = false;
                for (_, cc) in chars.by_ref() {
                    if cc == ']' {
                        found_bracket = true;
                        break;
                    }
                    label.push(cc);
                }
                if found_bracket && chars.peek().map(|&(_, n)| n) == Some('(') {
                    chars.next(); // consume '('
                    let mut url = String::new();
                    let mut found_paren = false;
                    for (_, cc) in chars.by_ref() {
                        if cc == ')' {
                            found_paren = true;
                            break;
                        }
                        url.push(cc);
                    }
                    if found_paren {
                        flush(&mut buf, &mut spans);
                        // Render: label in normal fg, URL in muted color.
                        spans.push(Span::styled(label, Style::new().fg(palette.fg_bright)));
                        if !url.is_empty() {
                            spans.push(Span::styled(
                                format!(" ({url})"),
                                Style::new().fg(palette.fg_mute),
                            ));
                        }
                    } else {
                        // Malformed: render literally.
                        buf.push('[');
                        buf.push_str(&label);
                        buf.push(']');
                        buf.push('(');
                        buf.push_str(&url);
                    }
                } else {
                    // No '(' after ']': render literally.
                    buf.push('[');
                    buf.push_str(&label);
                    if found_bracket {
                        buf.push(']');
                    }
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

// ============================================================
// Lightweight syntax highlighting for fenced code blocks
// ============================================================

/// Rust keywords for lightweight highlighting.
///
/// Source: <https://doc.rust-lang.org/reference/keywords.html> (stable keywords).
/// Only statement/declaration keywords are listed; type names and macros are
/// handled separately so they render with a distinct color.
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "union",
    "unsafe", "use", "where", "while",
];

/// Python keywords for lightweight highlighting.
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// JavaScript/TypeScript keywords for lightweight highlighting.
const JS_KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// Shell/bash keywords for lightweight highlighting.
const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "while", "do", "done", "in",
    "function", "return", "exit", "export", "local", "readonly", "unset", "echo", "source", "true",
    "false",
];

/// Maps a fence language tag to its keyword list, if supported.
///
/// Returns `None` for unrecognized languages — those are rendered verbatim.
fn keywords_for_lang(lang: &str) -> Option<&'static [&'static str]> {
    match lang.to_lowercase().as_str() {
        "rust" | "rs" => Some(RUST_KEYWORDS),
        "python" | "py" => Some(PYTHON_KEYWORDS),
        "javascript" | "js" | "typescript" | "ts" | "jsx" | "tsx" => Some(JS_KEYWORDS),
        "sh" | "bash" | "shell" | "zsh" | "fish" => Some(SHELL_KEYWORDS),
        _ => None,
    }
}

/// Produces styled spans for one line inside a fenced code block.
///
/// When the language has a keyword table, each whitespace-delimited token is
/// checked against that table and highlighted with the accent color when it
/// matches.  Non-keyword tokens use the normal foreground.  If the language is
/// unknown the entire line renders as a single `fg`-colored span with the
/// standard code-block indentation.
fn highlight_code_line(raw: &str, lang: &str, palette: &Palette) -> Vec<Span<'static>> {
    let code_style = Style::new().fg(palette.fg).bg(palette.bg_overlay);
    let keyword_style = Style::new()
        .fg(palette.accent)
        .bg(palette.bg_overlay)
        .add_modifier(Modifier::BOLD);

    let Some(keywords) = keywords_for_lang(lang) else {
        // Unknown language: plain rendering with code background.
        return vec![Span::styled(format!("  {raw}"), code_style)];
    };

    let mut spans = vec![Span::styled("  ".to_owned(), code_style)];
    // Tokenize the line, preserving whitespace runs as separate tokens so the
    // output reconstructs the original spacing faithfully.
    let mut buf = String::new();
    let mut in_word = false;

    let emit = |buf: &mut String, is_word: bool, spans: &mut Vec<Span<'static>>| {
        if buf.is_empty() {
            return;
        }
        let text = std::mem::take(buf);
        if is_word && keywords.contains(&text.as_str()) {
            spans.push(Span::styled(text, keyword_style));
        } else {
            spans.push(Span::styled(text, code_style));
        }
    };

    for ch in raw.chars() {
        let is_word_char = ch.is_alphanumeric() || ch == '_';
        if is_word_char != in_word {
            emit(&mut buf, in_word, &mut spans);
            in_word = is_word_char;
        }
        buf.push(ch);
    }
    emit(&mut buf, in_word, &mut spans);
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

    // ---- italic ----

    #[test]
    fn underscore_italic_applies_italic_modifier() {
        let spans = render_inline("_hello_", &palette());
        assert!(
            spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::ITALIC))
        );
    }

    #[test]
    fn star_italic_applies_italic_modifier() {
        let spans = render_inline("*hello*", &palette());
        assert!(
            spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::ITALIC))
        );
    }

    #[test]
    fn unmatched_underscore_is_literal() {
        let spans = render_inline("_hello world", &palette());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "_hello world");
    }

    // ---- strikethrough ----

    #[test]
    fn strikethrough_applies_crossed_out_modifier() {
        let spans = render_inline("~~deleted~~", &palette());
        assert!(
            spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT))
        );
    }

    #[test]
    fn unmatched_double_tilde_is_literal() {
        let spans = render_inline("~~no close", &palette());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains("~~"));
    }

    // ---- links ----

    #[test]
    fn link_renders_label_and_muted_url() {
        let p = palette();
        let spans = render_inline("[foo](https://example.com)", &p);
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains("foo"), "label must appear");
        assert!(joined.contains("https://example.com"), "url must appear");
    }

    #[test]
    fn malformed_link_no_paren_is_literal() {
        let spans = render_inline("[foo]", &palette());
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains('[') || joined.contains("foo"));
    }

    // ---- syntax highlighting ----

    #[test]
    fn rust_keyword_gets_accent_color() {
        let p = palette();
        let spans = highlight_code_line("fn main() {}", "rust", &p);
        // "fn" should be highlighted with the accent.
        let highlighted = spans
            .iter()
            .any(|s| s.content.trim() == "fn" && s.style.fg == Some(p.accent));
        assert!(highlighted, "fn should be highlighted as a Rust keyword");
    }

    #[test]
    fn unknown_lang_renders_plain() {
        let p = palette();
        let spans = highlight_code_line("hello world", "cobol", &p);
        // Single span (plus the indent span) with no keyword coloring.
        assert!(spans.len() <= 2);
    }

    #[test]
    fn python_keyword_highlighted() {
        let p = palette();
        let spans = highlight_code_line("def foo():", "python", &p);
        assert!(
            spans
                .iter()
                .any(|s| s.content.trim() == "def" && s.style.fg == Some(p.accent))
        );
    }
}

// Rust guideline compliant 2026-02-21
