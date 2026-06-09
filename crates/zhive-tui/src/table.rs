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

/// Per-column display-width ceiling, so one huge cell cannot blow out the grid
/// when the terminal is wide enough to grant every column its natural width.
const MAX_COL_WIDTH: usize = 40;

/// Upper bound on the width charged for a single unbroken word when computing a
/// column's minimum width. A longer word is hard-split rather than forcing the
/// whole column (and table) wider than the terminal. Mirrors pi's
/// `maxUnbrokenWordWidth`.
const MAX_UNBROKEN_WORD_WIDTH: usize = 30;

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

/// Renders parsed table `rows` as an aligned box-drawing grid that fits within
/// `available_width` display cells.
///
/// Column widths adapt to the available width (pi's algorithm): when every
/// column's natural width fits, each keeps it; otherwise columns shrink from
/// their minimum (longest unbreakable word) proportionally to their slack, and
/// over-long cell text wraps onto extra rows instead of being lost. When the
/// width cannot host even one cell per column, the table degrades to plain
/// wrapped text rather than a broken grid.
pub(crate) fn render_table(
    rows: &[Vec<String>],
    p: &Palette,
    available_width: u16,
) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return Vec::new();
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }
    let available = usize::from(available_width);

    // Border overhead: leading "│ ", an inner " │ " between each pair of
    // columns, and a trailing " │" => 2 + 3*(cols-1) + 2 = 3*cols + 1.
    let border_overhead = 3 * cols + 1;
    let available_for_cells = available.saturating_sub(border_overhead);
    if available_for_cells < cols {
        // Too narrow for a stable grid: degrade to plain wrapped text.
        return degrade_to_text(rows, p, available_width);
    }

    // Natural width (ideal, capped) and minimum width (longest unbreakable word,
    // capped) per column.
    let mut natural = vec![0usize; cols];
    let mut min_word = vec![1usize; cols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            natural[c] = natural[c].max(UnicodeWidthStr::width(cell.as_str()).min(MAX_COL_WIDTH));
            min_word[c] = min_word[c].max(longest_word_width(cell, MAX_UNBROKEN_WORD_WIDTH));
        }
    }

    let widths = solve_widths(&natural, &min_word, available_for_cells);

    let mut out = Vec::with_capacity(rows.len() * 2 + 3);
    out.push(border(&widths, p, "┌", "┬", "┐"));
    out.extend(render_row(&rows[0], &widths, p, true));
    out.push(border(&widths, p, "├", "┼", "┤"));
    for row in &rows[1..] {
        out.extend(render_row(row, &widths, p, false));
    }
    out.push(border(&widths, p, "└", "┴", "┘"));
    out
}

/// Allocates a display width to each column within `available_for_cells`.
///
/// When the natural widths fit, they are used as-is (floored at the column
/// minimum). Otherwise each column starts at its minimum and the leftover space
/// is shared in proportion to each column's slack (`natural - min`), with any
/// rounding remainder distributed one cell at a time toward natural widths.
fn solve_widths(natural: &[usize], min_word: &[usize], available_for_cells: usize) -> Vec<usize> {
    let total_natural: usize = natural.iter().sum();
    if total_natural <= available_for_cells {
        return natural
            .iter()
            .zip(min_word)
            .map(|(&n, &m)| n.max(m))
            .collect();
    }

    let total_slack: usize = natural
        .iter()
        .zip(min_word)
        .map(|(&n, &m)| n.saturating_sub(m))
        .sum();
    let min_total: usize = min_word.iter().sum();
    let extra = available_for_cells.saturating_sub(min_total);

    let mut widths: Vec<usize> = min_word
        .iter()
        .zip(natural)
        .map(|(&m, &n)| {
            let slack = n.saturating_sub(m);
            let grow = (slack * extra).checked_div(total_slack).unwrap_or(0);
            m + grow
        })
        .collect();

    // Distribute rounding remainder toward columns still below their natural
    // width, one cell at a time.
    let allocated: usize = widths.iter().sum();
    let mut remaining = available_for_cells.saturating_sub(allocated);
    while remaining > 0 {
        let mut grew = false;
        for (w, &n) in widths.iter_mut().zip(natural) {
            if remaining == 0 {
                break;
            }
            if *w < n {
                *w += 1;
                remaining -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    widths
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

/// Renders one table row as one or more visual lines (a cell that wraps spills
/// onto extra rows, with empty padding in the columns that have run out).
fn render_row(row: &[String], widths: &[usize], p: &Palette, header: bool) -> Vec<Line<'static>> {
    let cell_style = if header {
        Style::new().fg(p.fg_bright).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(p.fg)
    };

    // Wrap each cell to its column width; the row is as tall as the tallest cell.
    let wrapped: Vec<Vec<String>> = widths
        .iter()
        .enumerate()
        .map(|(c, &w)| wrap_cell(row.get(c).map_or("", String::as_str), w))
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);

    let mut lines = Vec::with_capacity(height);
    for r in 0..height {
        let mut spans = vec![bar(p)];
        for (c, &w) in widths.iter().enumerate() {
            let text = wrapped[c].get(r).map_or("", String::as_str);
            let pad = w.saturating_sub(UnicodeWidthStr::width(text));
            spans.push(Span::styled(
                format!(" {text}{} ", " ".repeat(pad)),
                cell_style,
            ));
            spans.push(bar(p));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// A muted vertical cell separator span.
fn bar(p: &Palette) -> Span<'static> {
    Span::styled("│".to_owned(), Style::new().fg(p.fg_mute))
}

/// Wraps `cell` into rows no wider than `max` display cells, breaking on spaces
/// and hard-splitting any single word that exceeds `max`.
fn wrap_cell(cell: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![String::new()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in cell.split(' ') {
        let ww = UnicodeWidthStr::width(word);
        if cur_w > 0 && cur_w + 1 + ww > max {
            rows.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if ww > max {
            // Word wider than the column: hard-split it across rows.
            if cur_w > 0 {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            for ch in word.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_w + cw > max && cur_w > 0 {
                    rows.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += cw;
            }
        } else {
            if cur_w > 0 {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += ww;
        }
    }
    if rows.is_empty() || !cur.is_empty() {
        rows.push(cur);
    }
    rows
}

/// Width of the widest single space-delimited word in `cell`, capped at `cap`.
fn longest_word_width(cell: &str, cap: usize) -> usize {
    cell.split_whitespace()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
        .min(cap)
        .max(1)
}

/// Degrades an un-renderable (too-narrow) table to plain `a | b` rows, soft
/// wrapped to `available_width`, so the content stays readable without a broken
/// grid.
fn degrade_to_text(rows: &[Vec<String>], p: &Palette, available_width: u16) -> Vec<Line<'static>> {
    let max = usize::from(available_width.max(1));
    let style = Style::new().fg(p.fg);
    let mut out = Vec::new();
    for row in rows {
        let joined = row.join(" | ");
        for chunk in wrap_cell(&joined, max) {
            out.push(Line::styled(chunk, style));
        }
    }
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
        let lines = render_table(&rows, &palette(), 80);
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
        let lines = render_table(&rows, &palette(), 80);
        // Must not panic and must produce a bordered grid.
        assert!(lines.len() >= 5);
    }

    /// Total display width of a rendered line (sum of its spans' widths).
    fn line_width(line: &Line) -> usize {
        line.spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum()
    }

    #[test]
    fn narrow_table_never_exceeds_available_width() {
        // A wide two-column table forced into a narrow terminal must wrap its
        // cells so every rendered row stays within the available width — this is
        // the regression that produced the broken box-drawing artifact.
        let rows = vec![
            vec!["item".to_owned(), "verification result".to_owned()],
            vec![
                "TodoTool / plan".to_owned(),
                "grep TodoTool PlanTool update_plan empty fully unimplemented".to_owned(),
            ],
        ];
        let width = 40u16;
        let lines = render_table(&rows, &palette(), width);
        for line in &lines {
            assert!(
                line_width(line) <= usize::from(width),
                "row {:?} exceeds available width {width}",
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>(),
            );
        }
        // The grid is still drawn (not degraded) at this width.
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains('┌'));
    }

    #[test]
    fn long_cell_wraps_onto_multiple_rows() {
        // A single data row whose cell does not fit must spill onto extra visual
        // lines rather than being truncated, so a one-data-row table renders
        // more than the minimal 5 lines (3 borders + 1 header + 1 data).
        let rows = vec![
            vec!["k".to_owned(), "v".to_owned()],
            vec![
                "x".to_owned(),
                "alpha beta gamma delta epsilon zeta eta theta".to_owned(),
            ],
        ];
        let lines = render_table(&rows, &palette(), 24);
        assert!(
            lines.len() > 5,
            "expected the long cell to wrap onto extra rows, got {} lines",
            lines.len()
        );
    }

    #[test]
    fn extremely_narrow_table_degrades_to_text() {
        // When even one cell per column cannot fit, no box-drawing grid is
        // produced; content degrades to plain wrapped text instead.
        let rows = vec![
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            vec!["1".to_owned(), "2".to_owned(), "3".to_owned()],
        ];
        let lines = render_table(&rows, &palette(), 6);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            !joined.contains('┌'),
            "must not draw a grid when too narrow"
        );
        assert!(joined.contains('a') && joined.contains('1'));
    }

    #[test]
    fn solve_widths_shrinks_proportionally_when_overflowing() {
        // natural=[10,10] cannot fit in 8 cells; both shrink toward their
        // minimums and the total never exceeds the budget.
        let widths = solve_widths(&[10, 10], &[2, 2], 8);
        assert_eq!(widths.iter().sum::<usize>(), 8);
        assert!(widths.iter().all(|&w| w >= 2));
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
