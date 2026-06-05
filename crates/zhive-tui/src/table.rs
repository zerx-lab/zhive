//! GFM table extraction and box-drawing rendering.
//!
//! tui-markdown does not enable `pulldown-cmark`'s table extension, so a GFM
//! table would otherwise collapse into one run-together line.  [`split_segments`]
//! pulls table blocks out of the source (leaving the rest for tui-markdown), and
//! [`render_table`] draws each as an aligned grid with box-drawing borders.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Palette;

/// Per-column display-width ceiling, so one huge cell cannot blow out the grid.
const MAX_COL_WIDTH: usize = 40;

/// A slice of source: either Markdown for tui-markdown, or a parsed table.
pub(crate) enum Segment {
    /// Markdown text (rendered by tui-markdown).
    Markdown(String),
    /// A table's rows of cells; `rows[0]` is the header.
    Table(Vec<Vec<String>>),
}

/// Splits `src` into Markdown and table segments, skipping code fences.
pub(crate) fn split_segments(src: &str) -> Vec<Segment> {
    let lines: Vec<&str> = src.split('\n').collect();
    let mut segments = Vec::new();
    let mut md = String::new();
    let mut in_fence = false;
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            push_line(&mut md, line, i, lines.len());
            i += 1;
            continue;
        }
        if !in_fence && is_table_start(&lines, i) {
            if !md.is_empty() {
                segments.push(Segment::Markdown(std::mem::take(&mut md)));
            }
            let (rows, next) = collect_table(&lines, i);
            segments.push(Segment::Table(rows));
            i = next;
            continue;
        }
        push_line(&mut md, line, i, lines.len());
        i += 1;
    }

    if !md.is_empty() {
        segments.push(Segment::Markdown(md));
    }
    segments
}

/// Appends `line` to `md`, re-adding the `\n` that `split('\n')` removed
/// (except after the final line, which had no trailing newline).
fn push_line(md: &mut String, line: &str, idx: usize, total: usize) {
    md.push_str(line);
    if idx + 1 < total {
        md.push('\n');
    }
}

/// `true` when `lines[i]` is a table header followed by a separator row.
fn is_table_start(lines: &[&str], i: usize) -> bool {
    lines[i].contains('|')
        && !lines[i].trim().is_empty()
        && !is_separator(lines[i])
        && i + 1 < lines.len()
        && is_separator(lines[i + 1])
}

/// `true` for a GFM separator row like `|---|:--:|` (dashes, colons, pipes).
fn is_separator(line: &str) -> bool {
    let core = line.trim().trim_matches('|');
    // Require a pipe so a bare `---` thematic break / setext underline is not
    // mistaken for a separator row.
    line.contains('|')
        && !core.trim().is_empty()
        && core.contains('-')
        && core.chars().all(|c| matches!(c, '-' | ':' | '|' | ' '))
}

/// Collects the header + data rows of a table starting at `start`.
///
/// Returns the parsed rows (separator row dropped) and the index just past it.
fn collect_table(lines: &[&str], start: usize) -> (Vec<Vec<String>>, usize) {
    let mut rows = vec![parse_row(lines[start])];
    let mut i = start + 2; // skip header + separator
    while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
        // Parse every interior row verbatim; do not re-skip separator-looking
        // rows (a `| --- |` data cell is literal text, which GFM keeps).
        rows.push(parse_row(lines[i]));
        i += 1;
    }
    (rows, i)
}

/// Parses one `| a | b |` row into trimmed cell strings.
fn parse_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_owned()).collect()
}

/// Renders parsed table `rows` as an aligned box-drawing grid.
pub(crate) fn render_table(rows: &[Vec<String>], p: &Palette) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return Vec::new();
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }

    let mut widths = vec![0usize; cols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            let w = UnicodeWidthStr::width(cell.as_str()).min(MAX_COL_WIDTH);
            if w > widths[c] {
                widths[c] = w;
            }
        }
    }

    let mut out = Vec::with_capacity(rows.len() + 3);
    out.push(border(&widths, p, "┌", "┬", "┐"));
    out.push(render_row(&rows[0], &widths, p, true));
    out.push(border(&widths, p, "├", "┼", "┤"));
    for row in &rows[1..] {
        out.push(render_row(row, &widths, p, false));
    }
    out.push(border(&widths, p, "└", "┴", "┘"));
    out
}

/// Builds a horizontal border line with the given corner/junction glyphs.
fn border(widths: &[usize], p: &Palette, left: &str, mid: &str, right: &str) -> Line<'static> {
    let mut s = String::from(left);
    let last = widths.len().saturating_sub(1);
    for (c, w) in widths.iter().enumerate() {
        s.push_str(&"─".repeat(w + 2));
        s.push_str(if c == last { right } else { mid });
    }
    Line::styled(s, Style::new().fg(p.fg_mute))
}

/// Renders one table row, padding each cell to its column width.
fn render_row(row: &[String], widths: &[usize], p: &Palette, header: bool) -> Line<'static> {
    let bar = || Span::styled("│".to_owned(), Style::new().fg(p.fg_mute));
    let cell_style = if header {
        Style::new().fg(p.fg_bright).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(p.fg)
    };

    let mut spans = vec![bar()];
    for (c, w) in widths.iter().enumerate() {
        let raw = row.get(c).map_or("", String::as_str);
        let cell = truncate_cell(raw, *w);
        let pad = w.saturating_sub(UnicodeWidthStr::width(cell.as_str()));
        spans.push(Span::styled(
            format!(" {cell}{} ", " ".repeat(pad)),
            cell_style,
        ));
        spans.push(bar());
    }
    Line::from(spans)
}

/// Truncates a cell to `max` display cells, appending `…` when clipped.
fn truncate_cell(cell: &str, max: usize) -> String {
    if UnicodeWidthStr::width(cell) <= max {
        return cell.to_owned();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in cell.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::default()
    }

    fn render(src: &str) -> Vec<Segment> {
        split_segments(src)
    }

    #[test]
    fn table_is_split_from_surrounding_text() {
        let src = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\noutro";
        let segs = render(src);
        let tables = segs
            .iter()
            .filter(|s| matches!(s, Segment::Table(_)))
            .count();
        assert_eq!(tables, 1, "exactly one table segment");
    }

    #[test]
    fn table_rows_parse_into_cells() {
        let segs = render("| a | b |\n|---|---|\n| 1 | 2 |");
        let Some(Segment::Table(rows)) = segs.into_iter().next() else {
            panic!("expected a table");
        };
        assert_eq!(rows[0], vec!["a", "b"]);
        assert_eq!(rows[1], vec!["1", "2"]);
    }

    #[test]
    fn plain_text_is_not_a_table() {
        let segs = render("just a | pipe in prose");
        assert!(segs.iter().all(|s| matches!(s, Segment::Markdown(_))));
    }

    #[test]
    fn pipe_inside_code_fence_is_not_a_table() {
        let src = "```\n| a | b |\n|---|---|\n```\n";
        let segs = render(src);
        assert!(segs.iter().all(|s| matches!(s, Segment::Markdown(_))));
    }

    #[test]
    fn incomplete_table_without_separator_stays_markdown() {
        // Header with no separator row yet (mid-stream) → not a table.
        let segs = render("| a | b |\n| 1 | 2 |");
        assert!(segs.iter().all(|s| matches!(s, Segment::Markdown(_))));
    }

    #[test]
    fn render_table_draws_grid_with_cells() {
        let rows = vec![
            vec!["Name".to_owned(), "Age".to_owned()],
            vec!["Alice".to_owned(), "30".to_owned()],
        ];
        let lines = render_table(&rows, &palette());
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("Name") && joined.contains("Alice"));
        assert!(joined.contains('┌') && joined.contains('│') && joined.contains('┘'));
    }

    #[test]
    fn render_table_handles_cjk_width() {
        let rows = vec![
            vec!["列一".to_owned(), "列二".to_owned()],
            vec!["值".to_owned(), "x".to_owned()],
        ];
        let lines = render_table(&rows, &palette());
        // Must not panic and must produce a bordered grid.
        assert!(lines.len() >= 5);
    }

    #[test]
    fn thematic_break_with_pipe_is_not_a_table() {
        // A prose line with a pipe followed by a bare `---` (thematic break /
        // setext underline) must not be misdetected as a table.
        let segs = render("text | more\n---\nbody");
        assert!(segs.iter().all(|s| matches!(s, Segment::Markdown(_))));
    }

    #[test]
    fn dash_data_cell_is_preserved() {
        let segs = render("| a | b |\n|---|---|\n| --- | x |");
        let Some(Segment::Table(rows)) = segs.into_iter().next() else {
            panic!("expected a table");
        };
        // The `---` data cell must survive (not dropped as a separator row).
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0], "---");
    }
}
