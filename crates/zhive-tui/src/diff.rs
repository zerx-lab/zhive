//! Diff rendering for `Item::Diff` and `Item::FileEdit`, backed by `similar`.
//!
//! Whole-file `old_text` / `new_text` are diffed line-by-line, grouped into
//! hunks with context, and colored with the palette's `diff_*` tokens.  Changed
//! lines additionally receive word-level intra-line emphasis (a brighter
//! background on the actually-changed fragments).  Long diffs fold to a preview
//! via [`truncate_styled_lines`], matching the tool-output collapse behavior.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, DiffOp, InlineChange, InlineChangeMode, InlineChangeOptions, TextDiff};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use zhive_proto::domain::{FileUpdateChange, PatchChangeKind};

use crate::theme::Palette;

/// Lines of unchanged context shown around each changed hunk.
const DIFF_CONTEXT: usize = 3;
/// Diff body lines shown before folding (mirrors `TOOL_PREVIEW_LINES`).
const DIFF_PREVIEW: usize = 8;
/// Per-side byte ceiling above which the diff is suppressed.
const MAX_DIFF_BYTES: usize = 200_000;

/// Renders one [`FileUpdateChange`] as a header line plus a folded diff body.
pub(crate) fn render_file_change(
    change: &FileUpdateChange,
    expanded: bool,
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let (sign, hdr_color) = match change.kind {
        PatchChangeKind::Create => ("+", p.diff_add_fg),
        PatchChangeKind::Delete => ("-", p.diff_del_fg),
        PatchChangeKind::Rename => ("↷", p.warn),
        // Update plus any future non_exhaustive kind: generic modification.
        _ => ("~", p.info),
    };
    let header = Line::from(vec![
        Span::styled(
            format!("{sign} "),
            Style::new().fg(hdr_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            change.path.to_string_lossy().into_owned(),
            Style::new().fg(p.fg),
        ),
    ]);

    let body = match change.kind {
        PatchChangeKind::Create => {
            build_diff_lines("", change.new_text.as_deref().unwrap_or(""), p, width)
        }
        PatchChangeKind::Delete => {
            build_diff_lines(change.old_text.as_deref().unwrap_or(""), "", p, width)
        }
        PatchChangeKind::Rename => match (change.old_text.as_deref(), change.new_text.as_deref()) {
            (Some(old), Some(new)) if old != new => build_diff_lines(old, new, p, width),
            _ => Vec::new(),
        },
        // Update plus any future non_exhaustive kind: full line diff.
        _ => build_diff_lines(
            change.old_text.as_deref().unwrap_or(""),
            change.new_text.as_deref().unwrap_or(""),
            p,
            width,
        ),
    };

    let mut out = vec![header];
    if !body.is_empty() {
        out.extend(truncate_styled_lines(body, DIFF_PREVIEW, expanded, p));
    }
    out
}

/// Renders an `Item::Diff` as a header line plus a folded diff body.
pub(crate) fn render_file_diff(
    path: &str,
    old_text: Option<&str>,
    new_text: Option<&str>,
    expanded: bool,
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let header = Line::from(vec![
        Span::styled("± ", Style::new().fg(p.info)),
        Span::styled(
            path.to_owned(),
            Style::new().fg(p.info).add_modifier(Modifier::BOLD),
        ),
    ]);
    let body = build_diff_lines(old_text.unwrap_or(""), new_text.unwrap_or(""), p, width);
    let mut out = vec![header];
    out.extend(truncate_styled_lines(body, DIFF_PREVIEW, expanded, p));
    out
}

/// Diffs `old` vs `new_str` line-by-line into styled hunk lines.
fn build_diff_lines(old: &str, new_str: &str, p: &Palette, width: u16) -> Vec<Line<'static>> {
    if guard_binary(old) || guard_binary(new_str) {
        return vec![notice("[binary file — diff suppressed]", p)];
    }
    if old.len() > MAX_DIFF_BYTES || new_str.len() > MAX_DIFF_BYTES {
        return vec![notice("[file too large — diff suppressed]", p)];
    }
    if old == new_str {
        return Vec::new();
    }

    let diff = TextDiff::from_lines(old, new_str);
    let mut opts = InlineChangeOptions::new();
    opts.mode(InlineChangeMode::Chars);

    let mut out = Vec::new();
    for hunk in diff.grouped_ops(DIFF_CONTEXT) {
        let (os, ol, ns, nl) = hunk_range(&hunk);
        out.push(Line::styled(
            format!("@@ -{os},{ol} +{ns},{nl} @@"),
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        ));
        for op in &hunk {
            for change in diff.iter_inline_changes_with_options(op, opts) {
                let line = match change.tag() {
                    ChangeTag::Insert => render_inline_change(
                        &change,
                        p.diff_add_bg,
                        p.diff_add_fg,
                        emph_bg(p.diff_add_bg),
                        width,
                    ),
                    ChangeTag::Delete => render_inline_change(
                        &change,
                        p.diff_del_bg,
                        p.diff_del_fg,
                        emph_bg(p.diff_del_bg),
                        width,
                    ),
                    ChangeTag::Equal => render_context(&change, p, width),
                };
                out.push(line);
            }
        }
    }
    out
}

/// Builds a `+`/`-` line with per-fragment intra-line emphasis.
fn render_inline_change(
    change: &InlineChange<'_, str>,
    bg: Color,
    fg: Color,
    emph: Color,
    width: u16,
) -> Line<'static> {
    let sign = if matches!(change.tag(), ChangeTag::Insert) {
        "+"
    } else {
        "-"
    };
    let mut spans = vec![Span::styled(
        sign.to_owned(),
        Style::new().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )];
    for (emphasized, fragment) in change.iter_strings_lossy() {
        let frag = fragment.trim_end_matches(['\n', '\r']).to_owned();
        if frag.is_empty() {
            continue;
        }
        let style = if emphasized {
            Style::new().fg(fg).bg(emph).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(fg).bg(bg)
        };
        spans.push(Span::styled(frag, style));
    }
    clip_line(Line::from(spans), width)
}

/// Builds a single dim context line (no intra-line emphasis).
fn render_context(change: &InlineChange<'_, str>, p: &Palette, width: u16) -> Line<'static> {
    let text: String = change
        .iter_strings_lossy()
        .map(|(_, f)| f.trim_end_matches(['\n', '\r']).to_owned())
        .collect();
    clip_line(
        Line::styled(format!(" {text}"), Style::new().fg(p.diff_ctx_fg)),
        width,
    )
}

/// Old/new 1-based start lines and counts for a hunk's `@@` header.
fn hunk_range(hunk: &[DiffOp]) -> (usize, usize, usize, usize) {
    let old_start = hunk.first().map_or(0, |op| op.old_range().start);
    let new_start = hunk.first().map_or(0, |op| op.new_range().start);
    let old_len: usize = hunk.iter().map(|op| op.old_range().len()).sum();
    let new_len: usize = hunk.iter().map(|op| op.new_range().len()).sum();
    (old_start + 1, old_len, new_start + 1, new_len)
}

/// Brightens a diff background ~20% toward white for emphasized fragments.
fn emph_bg(base: Color) -> Color {
    crate::theme::blend(Color::Rgb(0xff, 0xff, 0xff), base, 51)
}

/// Clips a line to `width` display cells, appending `…` when truncated.
///
/// Never wraps — wrapping would break the per-line background stripe.
fn clip_line(line: Line<'static>, width: u16) -> Line<'static> {
    let max = usize::from(width);
    let total: usize = line
        .spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if total <= max {
        return line;
    }
    let mut used = 0usize;
    let mut out_spans: Vec<Span<'static>> = Vec::new();
    for span in line.spans {
        let w = UnicodeWidthStr::width(span.content.as_ref());
        if used + w <= max.saturating_sub(1) {
            used += w;
            out_spans.push(span);
        } else {
            let budget = max.saturating_sub(1).saturating_sub(used);
            let mut clipped = String::new();
            let mut cw = 0usize;
            for ch in span.content.chars() {
                let chw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if cw + chw > budget {
                    break;
                }
                clipped.push(ch);
                cw += chw;
            }
            clipped.push('…');
            out_spans.push(Span::styled(clipped, span.style));
            break;
        }
    }
    Line::from(out_spans)
}

/// Folds a pre-styled line list to `preview` lines when not expanded.
///
/// Shared shape with the tool-output collapse: a `… N more lines · ctrl+o`
/// footer when collapsed, a `ctrl+o to collapse` footer when expanded past the
/// preview length.
pub(crate) fn truncate_styled_lines(
    lines: Vec<Line<'static>>,
    preview: usize,
    expanded: bool,
    p: &Palette,
) -> Vec<Line<'static>> {
    let total = lines.len();
    let visible = if expanded { total } else { total.min(preview) };
    let mut out: Vec<Line<'static>> = lines.into_iter().take(visible).collect();
    let hidden = total - visible;
    if hidden > 0 {
        out.push(Line::styled(
            format!("  … {hidden} more lines · ctrl+o to expand"),
            Style::new().fg(p.fg_mute),
        ));
    } else if expanded && total > preview {
        out.push(Line::styled(
            "  ctrl+o to collapse".to_owned(),
            Style::new().fg(p.fg_mute),
        ));
    }
    out
}

/// `true` when `text` looks binary (contains a NUL byte).
fn guard_binary(text: &str) -> bool {
    text.contains('\0')
}

/// A muted single-line notice (binary / too-large fallback).
fn notice(text: &str, p: &Palette) -> Line<'static> {
    Line::styled(text.to_owned(), Style::new().fg(p.fg_mute))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn palette() -> Palette {
        Palette::default()
    }

    fn change(kind: PatchChangeKind, old: Option<&str>, new: Option<&str>) -> FileUpdateChange {
        FileUpdateChange {
            path: PathBuf::from("src/lib.rs"),
            kind,
            old_text: old.map(str::to_owned),
            new_text: new.map(str::to_owned),
        }
    }

    #[test]
    fn update_shows_added_and_removed_lines() {
        let lines = render_file_change(
            &change(
                PatchChangeKind::Update,
                Some("a\nb\nc\n"),
                Some("a\nB\nc\n"),
            ),
            true,
            &palette(),
            80,
        );
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains('b'), "removed line present");
        assert!(joined.contains('B'), "added line present");
        assert!(joined.contains("@@"), "hunk header present");
    }

    #[test]
    fn create_is_all_additions() {
        let lines = render_file_change(
            &change(
                PatchChangeKind::Create,
                None,
                Some("new line one\nnew line two\n"),
            ),
            true,
            &palette(),
            80,
        );
        let has_plus = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.as_ref() == "+");
        assert!(has_plus, "create diff should have + sign spans");
    }

    #[test]
    fn delete_is_all_removals() {
        let lines = render_file_change(
            &change(PatchChangeKind::Delete, Some("gone one\ngone two\n"), None),
            true,
            &palette(),
            80,
        );
        let has_minus = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.as_ref() == "-");
        assert!(has_minus, "delete diff should have - sign spans");
    }

    #[test]
    fn identical_update_has_only_header() {
        let lines = render_file_change(
            &change(PatchChangeKind::Update, Some("same\n"), Some("same\n")),
            true,
            &palette(),
            80,
        );
        assert_eq!(lines.len(), 1, "no body for identical content");
    }

    #[test]
    fn binary_content_is_suppressed() {
        let lines = build_diff_lines("ok\n", "bad\0bytes\n", &palette(), 80);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("binary"), "binary diff suppressed");
    }

    #[test]
    fn long_diff_folds_when_collapsed() {
        use std::fmt::Write as _;
        let mut new = String::new();
        for i in 0..40 {
            let _ = writeln!(new, "line {i}");
        }
        let lines = render_file_change(
            &change(PatchChangeKind::Create, None, Some(&new)),
            false,
            &palette(),
            80,
        );
        let footer = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.as_ref().contains("more lines"));
        assert!(footer, "collapsed long diff shows a fold footer");
    }

    #[test]
    fn crlf_does_not_leak_carriage_return() {
        let lines = build_diff_lines("a\r\n", "b\r\n", &palette(), 80);
        let has_cr = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains('\r'));
        assert!(!has_cr, "carriage returns must be trimmed");
    }

    #[test]
    fn clip_line_truncates_overlong() {
        let line = Line::from(vec![Span::raw("x".repeat(200))]);
        let clipped = clip_line(line, 20);
        let w: usize = clipped
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert!(w <= 20, "clipped width within budget, got {w}");
    }
}
