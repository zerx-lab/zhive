//! Rendering of the conversation shell: top bar, transcript, composer, footer.
//!
//! Follows the `zap-tui-design` three-part shell — a branded top bar with a
//! `cwd · branch · session` breadcrumb and a right-aligned model pill, the
//! conversation body (an 8-cell role gutter beside each message), the composer
//! panel, and a bottom key-hint strip. Messages are pre-wrapped with
//! [`crate::wrap`] so continuation rows stay indented under the gutter. Modal
//! overlays ([`Overlay`]) draw over a [`Clear`]ed centered rect.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use zhive_proto::domain::{
    CommandExecutionStatus, FileUpdateChange, Item, ItemContent, ItemToolCallContent, NoticeLevel,
    PatchChangeKind, PlanStepStatus, ToolCallStatus,
};

use crate::app::App;
use crate::conversation::TurnLifecycle;
use crate::theme::Palette;
use crate::widgets::{self, Hint};
use crate::{markdown, wrap};

/// Width of the left role-label gutter, in cells.
const GUTTER: u16 = 8;

/// Maximum lines of tool-call input/output shown before truncation.
///
/// Keeps busy tool calls from flooding the transcript; the full content
/// is always stored in the item and readable via the engine log.
const TOOL_PREVIEW_LINES: usize = 8;

/// Maximum characters of a JSON argument summary before ellipsis truncation.
///
/// Long argument values (e.g. full file paths or multi-kb content) would
/// otherwise overflow the content column on a standard 80-column terminal.
const TOOL_ARG_SUMMARY_MAX: usize = 120;

/// Maximum lines of command output shown before truncation.
///
/// Mirrors [`TOOL_PREVIEW_LINES`] for visual consistency.
const CMD_OUTPUT_LINES: usize = 8;

/// Draws the entire frame for the current [`App`] state.
pub fn draw(frame: &mut Frame, app: &App) {
    let p = &app.palette;
    let area = frame.area();
    // Background fill.
    frame.render_widget(Paragraph::new("").style(Style::new().bg(p.bg)), area);

    let composer_h = composer_height(app);
    let [top, body, composer, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(composer_h),
        Constraint::Length(1),
    ])
    .areas(area);

    render_top_bar(frame, app, top);
    render_body(frame, app, body);
    let composer_inner = render_composer(frame, app, composer);
    render_footer(frame, app, footer);

    // Place the caret in the composer when no overlay is capturing input.
    if app.overlay.is_none() {
        let (col, row) = app.input.cursor_col_row();
        let cx = composer_inner.x.saturating_add(col);
        let cy = composer_inner.y.saturating_add(row);
        if cx < composer_inner.x.saturating_add(composer_inner.width)
            && cy < composer_inner.y.saturating_add(composer_inner.height)
        {
            frame.set_cursor_position((cx, cy));
        }
    }

    // The slash palette floats above the composer while a command is typed.
    if app.overlay.is_none() && app.palette_query().is_some() {
        crate::overlays::render_palette(frame, app, composer);
    }

    if let Some(overlay) = &app.overlay {
        crate::overlays::render_overlay(frame, app, overlay, area);
    }
}

/// Computes the composer panel height (input rows + borders), clamped.
fn composer_height(app: &App) -> u16 {
    let rows = u16::try_from(app.input.value().split('\n').count()).unwrap_or(1);
    rows.clamp(1, 6) + 2
}

/// Renders the branded top bar with breadcrumb and model pill.
fn render_top_bar(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;
    let sep = || Span::styled(" · ", Style::new().fg(p.fg_mute));
    let mut left = vec![
        Span::styled(
            "⚡ zap",
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(app.config.cwd_display(), Style::new().fg(p.fg)),
    ];
    if let Some(branch) = &app.config.branch {
        left.push(sep());
        left.push(Span::styled(branch.clone(), Style::new().fg(p.fg_dim)));
    }
    if let Some(session) = &app.config.session_name {
        left.push(sep());
        left.push(Span::styled(session.clone(), Style::new().fg(p.fg_dim)));
    }

    let pill = Line::from(vec![Span::styled(
        format!(
            " {} · {} ",
            app.config.provider_label, app.config.model_label
        ),
        Style::new().fg(p.accent).bg(p.bg_elev),
    )])
    .right_aligned();

    let bar_bg = Style::new().bg(p.bg_elev);
    frame.render_widget(Paragraph::new(Line::from(left)).style(bar_bg), area);
    frame.render_widget(Paragraph::new(pill).style(bar_bg), area);
}

/// Renders the conversation body (or the welcome view when empty).
fn render_body(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;
    let status = if app.conversation.busy {
        format!("{} working", widgets::spinner(app.spinner_tick))
    } else {
        format!("idle · {} msgs", app.conversation.item_count())
    };
    let block = widgets::panel("⌬ conversation", Some(&status), !app.conversation.busy, p);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.conversation.is_empty() {
        render_welcome(frame, app, inner);
        return;
    }

    let lines = transcript_lines(app, inner.width);
    let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let visible = inner.height;
    let max_scroll = total.saturating_sub(visible);
    // Clamp scrollback to [0, max_scroll] so we never scroll past the top or
    // get stuck above it when the transcript shrinks (e.g. after /clear).
    let scrollback = app.scrollback.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(scrollback);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((scroll_y, 0)),
        inner,
    );
}

/// Builds the fully-wrapped, gutter-prefixed transcript lines.
fn transcript_lines(app: &App, inner_width: u16) -> Vec<Line<'static>> {
    let p = &app.palette;
    let content_width = inner_width.saturating_sub(GUTTER);
    let mut out: Vec<Line<'static>> = Vec::new();

    for turn in &app.conversation.turns {
        for item in &turn.items {
            let (label, color) = role_of(item, p);
            let body = item_body(item, p, content_width);
            push_message(&mut out, label, color, body);
            out.push(Line::raw(""));
        }
        if let TurnLifecycle::Failed { message } = &turn.status {
            // Wrap the (often long) provider error to the content width so the
            // full message is visible across rows instead of being clipped.
            push_message(
                &mut out,
                "sys",
                p.error,
                wrap_plain(
                    &format!("turn failed: {message}"),
                    Style::new().fg(p.error),
                    content_width,
                ),
            );
            out.push(Line::raw(""));
        }
    }

    if app.conversation.busy {
        let body = if app.conversation.streaming.is_empty() {
            vec![Line::styled(
                format!("{} thinking…", widgets::spinner(app.spinner_tick)),
                Style::new().fg(p.fg_dim),
            )]
        } else {
            // Render the live partial text as markdown with a trailing cursor.
            let mut lines = Vec::new();
            for line in markdown::render(&app.conversation.streaming, p) {
                lines.extend(wrap::wrap_line(&line, content_width));
            }
            if let Some(last) = lines.last_mut() {
                last.push_span(Span::styled("▌", Style::new().fg(p.accent)));
            }
            lines
        };
        push_message(&mut out, "zap", p.role_zap, body);
    }
    out
}

/// Appends a role-gutter-prefixed message (first row labeled, rest indented).
fn push_message(
    out: &mut Vec<Line<'static>>,
    label: &str,
    color: ratatui::style::Color,
    body: Vec<Line<'static>>,
) {
    let body = if body.is_empty() {
        vec![Line::raw("")]
    } else {
        body
    };
    for (i, line) in body.into_iter().enumerate() {
        let gutter = if i == 0 {
            Span::styled(
                format!("{label:<8}"),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("        ")
        };
        let mut spans = vec![gutter];
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
}

/// Returns the `(label, color)` for an item's role gutter.
fn role_of(item: &Item, p: &Palette) -> (&'static str, ratatui::style::Color) {
    match item {
        Item::UserMessage { .. } => ("you", p.role_you),
        Item::AgentMessage { .. } | Item::AgentThought { .. } | Item::Reasoning { .. } => {
            ("zap", p.role_zap)
        }
        Item::SystemNotice { .. } => ("sys", p.role_system),
        _ => ("", p.fg_dim),
    }
}

/// Renders an item's body into wrapped lines (no gutter).
fn item_body(item: &Item, p: &Palette, width: u16) -> Vec<Line<'static>> {
    match item {
        Item::UserMessage { content, .. } => {
            let text = user_text(content);
            wrap_plain(&text, Style::new().fg(p.fg), width)
        }
        Item::AgentMessage { text, .. } => {
            let mut out = Vec::new();
            for line in markdown::render(text, p) {
                out.extend(wrap::wrap_line(&line, width));
            }
            out
        }
        Item::AgentThought { text, .. } => wrap_plain(
            &format!("💭 {text}"),
            Style::new().fg(p.fg_dim).add_modifier(Modifier::ITALIC),
            width,
        ),
        Item::Reasoning { summary, .. } => wrap_plain(
            &format!("💭 {}", summary.join(" ")),
            Style::new().fg(p.fg_dim).add_modifier(Modifier::ITALIC),
            width,
        ),
        Item::ToolCall {
            name,
            status,
            raw_input,
            raw_output,
            content,
            ..
        } => tool_call_lines(
            name,
            *status,
            raw_input.as_ref(),
            raw_output.as_ref(),
            content,
            p,
            width,
        ),
        Item::CommandExecution {
            command,
            status,
            exit_code,
            aggregated_output,
            duration_ms,
            ..
        } => command_lines(
            command,
            *status,
            *exit_code,
            aggregated_output.as_deref(),
            *duration_ms,
            p,
            width,
        ),
        Item::Diff {
            path,
            old_text,
            new_text,
            ..
        } => diff_lines(
            path.to_string_lossy().as_ref(),
            old_text.as_deref(),
            new_text,
            p,
            width,
        ),
        Item::FileEdit { changes, .. } => file_edit_lines(changes, p),
        Item::Plan { steps, .. } => steps
            .iter()
            .map(|s| {
                let (mark, color) = match s.status {
                    PlanStepStatus::Completed => ("✓", p.success),
                    PlanStepStatus::InProgress => ("◐", p.warn),
                    _ => ("○", p.fg_mute),
                };
                Line::from(vec![
                    Span::styled(format!("{mark} "), Style::new().fg(color)),
                    Span::styled(s.step.clone(), Style::new().fg(p.fg)),
                ])
            })
            .collect(),
        Item::SystemNotice { level, message, .. } => {
            let color = match level {
                NoticeLevel::Warn => p.warn,
                NoticeLevel::Error => p.error,
                _ => p.info,
            };
            wrap_plain(message, Style::new().fg(color), width)
        }
        Item::ContextCompaction { .. } => {
            vec![Line::styled(
                "─── context compacted ───",
                Style::new().fg(p.fg_mute),
            )]
        }
        other => vec![Line::styled(
            format!("[{}]", item_kind_name(other)),
            Style::new().fg(p.fg_mute),
        )],
    }
}

/// Renders a tool-call block: header, argument summary, and output preview.
fn tool_call_lines(
    name: &str,
    status: ToolCallStatus,
    raw_input: Option<&serde_json::Value>,
    raw_output: Option<&serde_json::Value>,
    content: &[ItemToolCallContent],
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let (status_label, status_color) = match status {
        ToolCallStatus::Pending => ("pending", p.fg_dim),
        ToolCallStatus::InProgress => ("running", p.warn),
        ToolCallStatus::Completed => ("ok", p.success),
        ToolCallStatus::Failed => ("failed", p.error),
        _ => ("…", p.fg_dim),
    };

    let mut out = vec![Line::from(vec![
        Span::styled(
            format!("▸ {name}"),
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(status_label.to_owned(), Style::new().fg(status_color)),
    ])];

    // Argument summary from raw_input (JSON → compact one-liner, truncated).
    if let Some(input) = raw_input {
        let summary = json_summary(input);
        if !summary.is_empty() {
            out.extend(wrap_plain(
                &format!("  args: {summary}"),
                Style::new().fg(p.fg_dim),
                width,
            ));
        }
    }

    // Output: prefer structured `content` list, fall back to raw_output JSON.
    let output_text = if !content.is_empty() {
        Some(tool_content_text(content))
    } else if let Some(raw) = raw_output {
        let s = json_summary(raw);
        if s.is_empty() { None } else { Some(s) }
    } else {
        None
    };

    if let Some(text) = output_text {
        let lines: Vec<&str> = text.lines().collect();
        let visible = lines.len().min(TOOL_PREVIEW_LINES);
        let truncated = lines.len() > TOOL_PREVIEW_LINES;
        for line in &lines[..visible] {
            out.push(Line::from(vec![
                Span::styled("  ", Style::new()),
                Span::styled((*line).to_owned(), Style::new().fg(p.fg).bg(p.bg_overlay)),
            ]));
        }
        if truncated {
            out.push(Line::styled(
                format!("  … ({} more lines)", lines.len() - TOOL_PREVIEW_LINES),
                Style::new().fg(p.fg_mute),
            ));
        }
    }

    out
}

/// Extracts a plain-text summary from a list of tool-call content blocks.
fn tool_content_text(content: &[ItemToolCallContent]) -> String {
    let mut parts = Vec::new();
    for block in content {
        // Only Content blocks carry displayable text; Diff and Terminal are skipped.
        if let ItemToolCallContent::Content {
            content: ItemContent::Text { text, .. },
            ..
        } = block
        {
            parts.push(text.as_str());
        }
    }
    parts.join("\n")
}

/// Produces a compact one-line summary of a JSON value, truncated at
/// [`TOOL_ARG_SUMMARY_MAX`] characters.
fn json_summary(value: &serde_json::Value) -> String {
    let raw = value.to_string();
    if raw.len() <= TOOL_ARG_SUMMARY_MAX {
        raw
    } else {
        format!("{}…", &raw[..TOOL_ARG_SUMMARY_MAX])
    }
}

/// Renders a command-execution block: `$ cmd`, optional output, status + timing.
fn command_lines(
    command: &str,
    status: CommandExecutionStatus,
    exit_code: Option<i32>,
    aggregated_output: Option<&str>,
    duration_ms: Option<i64>,
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let head = Line::from(vec![
        Span::styled("$ ", Style::new().fg(p.accent)),
        Span::styled(command.to_owned(), Style::new().fg(p.fg)),
    ]);
    out.extend(wrap::wrap_line(&head, width));

    // Show the first N lines of captured output.
    if let Some(output) = aggregated_output {
        let lines: Vec<&str> = output.lines().collect();
        let visible = lines.len().min(CMD_OUTPUT_LINES);
        let truncated = lines.len() > CMD_OUTPUT_LINES;
        for line in &lines[..visible] {
            out.push(Line::from(vec![
                Span::styled("  ", Style::new()),
                Span::styled((*line).to_owned(), Style::new().fg(p.fg).bg(p.bg_overlay)),
            ]));
        }
        if truncated {
            out.push(Line::styled(
                format!("  … (truncated, {} total lines)", lines.len()),
                Style::new().fg(p.fg_mute),
            ));
        }
    }

    let (status_label, color) = match status {
        CommandExecutionStatus::InProgress => ("running".to_owned(), p.warn),
        CommandExecutionStatus::Completed => {
            (format!("exit {}", exit_code.unwrap_or(0)), p.success)
        }
        CommandExecutionStatus::Failed => (format!("exit {}", exit_code.unwrap_or(-1)), p.error),
        _ => ("…".to_owned(), p.fg_dim),
    };

    // Append duration when available.
    let footer = if let Some(ms) = duration_ms {
        format!("{status_label}  ({ms}ms)")
    } else {
        status_label
    };
    out.push(Line::styled(footer, Style::new().fg(color)));
    out
}

/// Renders a unified-diff view for `Item::Diff`, coloring +/- lines with
/// the theme's `diff_add_bg` / `diff_del_bg` tokens.
fn diff_lines(
    path: &str,
    old_text: Option<&str>,
    new_text: &str,
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();

    // File header line.
    out.push(Line::from(vec![
        Span::styled("± ", Style::new().fg(p.info)),
        Span::styled(
            path.to_owned(),
            Style::new().fg(p.info).add_modifier(Modifier::BOLD),
        ),
    ]));

    // Generate and render unified diff lines.
    let unified = build_unified_diff(old_text.unwrap_or(""), new_text);
    for diff_line in unified {
        let styled = style_diff_line(&diff_line, p, width);
        out.push(styled);
    }

    out
}

/// A single classified diff output line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLineKind {
    /// A line added in the new version (`+` prefix).
    Add,
    /// A line removed from the old version (`-` prefix).
    Del,
    /// An unchanged context line (` ` prefix).
    Context,
    /// A hunk header (`@@` range line).
    Header,
}

/// Classifies a raw diff output line by its leading character.
#[must_use]
fn classify_diff_line(line: &str) -> DiffLineKind {
    if line.starts_with('+') {
        DiffLineKind::Add
    } else if line.starts_with('-') {
        DiffLineKind::Del
    } else if line.starts_with("@@") {
        DiffLineKind::Header
    } else {
        DiffLineKind::Context
    }
}

/// Converts a raw diff line into a styled ratatui [`Line`].
fn style_diff_line(raw: &str, p: &Palette, _width: u16) -> Line<'static> {
    match classify_diff_line(raw) {
        DiffLineKind::Add => Line::from(vec![Span::styled(
            raw.to_owned(),
            Style::new().fg(p.diff_add_fg).bg(p.diff_add_bg),
        )]),
        DiffLineKind::Del => Line::from(vec![Span::styled(
            raw.to_owned(),
            Style::new().fg(p.diff_del_fg).bg(p.diff_del_bg),
        )]),
        DiffLineKind::Header => Line::from(vec![Span::styled(
            raw.to_owned(),
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        )]),
        DiffLineKind::Context => Line::from(vec![Span::styled(
            raw.to_owned(),
            Style::new().fg(p.diff_ctx_fg),
        )]),
    }
}

/// Builds a minimal unified-diff between `old` and `new`.
///
/// Uses a simple LCS-based diff limited to 3-line context windows.
/// Does not introduce any external dependency — the diff is computed
/// entirely with stdlib slices.
fn build_unified_diff(old: &str, new_text: &str) -> Vec<String> {
    /// Lines of context to show around each changed hunk.
    const CONTEXT: usize = 3;

    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();

    // Compute edit script via Myers-style shortest-edit-sequence.
    let edits = compute_edits(&old_lines, &new_lines);

    // Group edits into hunks with context.
    group_into_hunks(&old_lines, &new_lines, &edits, CONTEXT)
}

/// Edit operation for one line in the old/new sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    /// Keep line at `old_idx` (appears in both sides).
    Keep { old_idx: usize, new_idx: usize },
    /// Delete line `old_idx` (only in old).
    Delete { old_idx: usize },
    /// Insert line `new_idx` (only in new).
    Insert { new_idx: usize },
}

/// Computes the edit script between two line slices using a greedy LCS.
///
/// Returns an ordered list of [`Edit`] operations.
fn compute_edits(old: &[&str], new_s: &[&str]) -> Vec<Edit> {
    // Build the longest-common-subsequence table.
    let m = old.len();
    let n = new_s.len();
    // `dp[i][j]` = LCS length of `old[..i]` vs `new[..j]`.
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if old[i] == new_s[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    // Trace back the edit script.
    let mut edits = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < m || j < n {
        if i < m && j < n && old[i] == new_s[j] {
            edits.push(Edit::Keep {
                old_idx: i,
                new_idx: j,
            });
            i += 1;
            j += 1;
        } else if j < n && (i >= m || dp[i + 1][j] <= dp[i][j + 1]) {
            edits.push(Edit::Insert { new_idx: j });
            j += 1;
        } else {
            edits.push(Edit::Delete { old_idx: i });
            i += 1;
        }
    }
    edits
}

/// Emits unified-diff output strings from an edit script.
fn group_into_hunks(old: &[&str], new_s: &[&str], edits: &[Edit], context: usize) -> Vec<String> {
    if edits.is_empty() {
        return Vec::new();
    }

    // Identify changed positions in the edit list.
    let changed: Vec<usize> = edits
        .iter()
        .enumerate()
        .filter(|(_, e)| !matches!(e, Edit::Keep { .. }))
        .map(|(i, _)| i)
        .collect();

    if changed.is_empty() {
        return Vec::new();
    }

    // Build hunk ranges [start, end) with context padding.
    let mut hunk_ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = changed[0].saturating_sub(context);
    let mut end = (changed[0] + context + 1).min(edits.len());
    for &ci in &changed[1..] {
        if ci.saturating_sub(context) <= end {
            end = (ci + context + 1).min(edits.len());
        } else {
            hunk_ranges.push((start, end));
            start = ci.saturating_sub(context);
            end = (ci + context + 1).min(edits.len());
        }
    }
    hunk_ranges.push((start, end));

    let mut out = Vec::new();
    for (hstart, hend) in hunk_ranges {
        // Compute old/new line number ranges for the @@ header.
        let old_start = hunk_old_start(&edits[hstart..hend]);
        let new_start = hunk_new_start(&edits[hstart..hend]);
        let old_count = edits[hstart..hend]
            .iter()
            .filter(|e| matches!(e, Edit::Keep { .. } | Edit::Delete { .. }))
            .count();
        let new_count = edits[hstart..hend]
            .iter()
            .filter(|e| matches!(e, Edit::Keep { .. } | Edit::Insert { .. }))
            .count();
        out.push(format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        ));
        for edit in &edits[hstart..hend] {
            match edit {
                Edit::Keep { old_idx, .. } => out.push(format!(" {}", old[*old_idx])),
                Edit::Delete { old_idx } => out.push(format!("-{}", old[*old_idx])),
                Edit::Insert { new_idx } => out.push(format!("+{}", new_s[*new_idx])),
            }
        }
    }
    out
}

/// First old-file line number (1-based) referenced in a hunk's edit slice.
fn hunk_old_start(slice: &[Edit]) -> usize {
    for e in slice {
        match e {
            Edit::Keep { old_idx, .. } | Edit::Delete { old_idx } => {
                return old_idx + 1;
            }
            Edit::Insert { .. } => {}
        }
    }
    1
}

/// First new-file line number (1-based) referenced in a hunk's edit slice.
fn hunk_new_start(slice: &[Edit]) -> usize {
    for e in slice {
        match e {
            Edit::Keep { new_idx, .. } | Edit::Insert { new_idx } => {
                return new_idx + 1;
            }
            Edit::Delete { .. } => {}
        }
    }
    1
}

/// Renders a `FileEdit` as one line per changed file.
fn file_edit_lines(changes: &[FileUpdateChange], p: &Palette) -> Vec<Line<'static>> {
    if changes.is_empty() {
        return vec![Line::styled(
            "✎ file edit · no changes",
            Style::new().fg(p.info),
        )];
    }
    let mut out = Vec::new();
    for change in changes {
        let (mark, color) = match change.kind {
            PatchChangeKind::Create => ("+", p.diff_add_fg),
            PatchChangeKind::Delete => ("-", p.diff_del_fg),
            PatchChangeKind::Rename => ("↷", p.warn),
            PatchChangeKind::Update | _ => ("~", p.info),
        };
        let path = change.path.to_string_lossy();
        out.push(Line::from(vec![
            Span::styled(
                format!("{mark} "),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(path.into_owned(), Style::new().fg(p.fg)),
        ]));
    }
    out
}

/// Short stable name for an item kind, for fallback rendering.
fn item_kind_name(item: &Item) -> &'static str {
    match item {
        Item::Terminal { .. } => "terminal",
        Item::AvailableCommands { .. } => "commands",
        Item::ModeChange { .. } => "mode change",
        _ => "item",
    }
}

/// Joins the textual parts of a user message, noting non-text blocks.
fn user_text(content: &[ItemContent]) -> String {
    let mut parts = Vec::new();
    for c in content {
        match c {
            ItemContent::Text { text, .. } => parts.push(text.clone()),
            ItemContent::Image { .. } => parts.push("[image]".to_owned()),
            ItemContent::Audio { .. } => parts.push("[audio]".to_owned()),
            ItemContent::ResourceLink { name, uri, .. } => {
                parts.push(format!("[{}]", name.clone().unwrap_or_else(|| uri.clone())));
            }
            ItemContent::Resource { .. } => parts.push("[resource]".to_owned()),
            _ => parts.push("[content]".to_owned()),
        }
    }
    parts.join("")
}

/// Splits `text` on newlines and word-wraps each segment to `width`.
fn wrap_plain(text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for segment in text.split('\n') {
        let line = Line::from(Span::styled(segment.to_owned(), style));
        out.extend(wrap::wrap_line(&line, width));
    }
    out
}

/// Renders the welcome view shown before the first message.
fn render_welcome(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;
    let lines = vec![
        Line::raw(""),
        Line::styled(
            "⚡ zap",
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "terminal copilot · talk to your codebase",
            Style::new().fg(p.fg_dim),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled("▸ ", Style::new().fg(p.accent)),
            Span::styled("type a message and press ", Style::new().fg(p.fg)),
            Span::styled("↵", Style::new().fg(p.fg_bright)),
            Span::styled(" to start", Style::new().fg(p.fg)),
        ]),
        Line::from(vec![
            Span::styled("▸ ", Style::new().fg(p.accent)),
            Span::styled("/help", Style::new().fg(p.fg_bright)),
            Span::styled("  commands · ", Style::new().fg(p.fg)),
            Span::styled("/theme", Style::new().fg(p.fg_bright)),
            Span::styled(" dark|light|mono · ", Style::new().fg(p.fg)),
            Span::styled("/accent", Style::new().fg(p.fg_bright)),
            Span::styled(" cyan|amber|lime|magenta", Style::new().fg(p.fg)),
        ]),
        Line::from(vec![
            Span::styled("▸ ", Style::new().fg(p.accent)),
            Span::styled("model: ", Style::new().fg(p.fg)),
            Span::styled(
                format!("{} · {}", app.config.provider_label, app.config.model_label),
                Style::new().fg(p.accent),
            ),
        ]),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Renders the composer panel and returns its inner rect (for the caret).
fn render_composer(frame: &mut Frame, app: &App, area: Rect) -> Rect {
    let p = &app.palette;
    let (title, dot) = if app.conversation.busy {
        ("◐ working", p.warn)
    } else {
        ("● ready", p.success)
    };
    let title = Span::styled(title, Style::new().fg(dot));
    let block = widgets::panel("", None, !app.conversation.busy, p).title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = if app.input.value().is_empty() && !app.conversation.busy {
        Text::from(Line::styled(
            "message zap…  (↵ send · ⌥↵ newline · /help)",
            Style::new().fg(p.fg_mute),
        ))
    } else {
        Text::from(app.input.value().to_owned()).style(Style::new().fg(p.fg))
    };
    frame.render_widget(Paragraph::new(text), inner);
    inner
}

/// Renders the bottom key-hint strip (plus any transient flash message).
fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;
    let bar_bg = Style::new().bg(p.bg_elev);
    if let Some(flash) = &app.flash {
        frame.render_widget(
            Paragraph::new(Line::styled(flash.clone(), Style::new().fg(p.warn))).style(bar_bg),
            area,
        );
        return;
    }
    let hints = [
        Hint::new("↵", "send"),
        Hint::new("⌥↵", "newline"),
        Hint::new("esc", "stop"),
        Hint::new("/help", "cmds"),
        Hint::new("⌃C", "quit"),
    ];
    frame.render_widget(
        Paragraph::new(widgets::kbd_hints(&hints, p)).style(bar_bg),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- scrollback clamping ----

    #[test]
    fn scrollback_clamp_prevents_over_scroll() {
        // transcript has 5 lines, visible area has 10 rows → max_scroll = 0
        // scrollback = 99 should be clamped to 0 so scroll_y stays 0.
        let total: u16 = 5;
        let visible: u16 = 10;
        let max_scroll = total.saturating_sub(visible);
        let scrollback: u16 = 99;
        let clamped = scrollback.min(max_scroll);
        assert_eq!(clamped, 0);
    }

    #[test]
    fn scrollback_clamp_allows_valid_range() {
        let total: u16 = 100;
        let visible: u16 = 20;
        let max_scroll = total.saturating_sub(visible); // 80
        let scrollback: u16 = 40;
        let clamped = scrollback.min(max_scroll);
        assert_eq!(clamped, 40);
    }

    // ---- diff classification ----

    #[test]
    fn diff_classify_add() {
        assert_eq!(classify_diff_line("+new line"), DiffLineKind::Add);
    }

    #[test]
    fn diff_classify_del() {
        assert_eq!(classify_diff_line("-old line"), DiffLineKind::Del);
    }

    #[test]
    fn diff_classify_context() {
        assert_eq!(classify_diff_line(" ctx line"), DiffLineKind::Context);
    }

    #[test]
    fn diff_classify_header() {
        assert_eq!(classify_diff_line("@@ -1,3 +1,4 @@"), DiffLineKind::Header);
    }

    // ---- unified diff generation ----

    #[test]
    fn build_unified_diff_identical_files_empty() {
        let diff = build_unified_diff("a\nb\n", "a\nb\n");
        assert!(diff.is_empty(), "no diff for identical content");
    }

    #[test]
    fn build_unified_diff_added_line() {
        let diff = build_unified_diff("a\n", "a\nb\n");
        let has_add = diff.iter().any(|l| l.starts_with('+'));
        assert!(has_add);
    }

    #[test]
    fn build_unified_diff_deleted_line() {
        let diff = build_unified_diff("a\nb\n", "a\n");
        let has_del = diff.iter().any(|l| l.starts_with('-'));
        assert!(has_del);
    }

    #[test]
    fn json_summary_short_passthrough() {
        let v = serde_json::json!({"x": 1});
        let s = json_summary(&v);
        assert!(s.len() <= TOOL_ARG_SUMMARY_MAX);
    }

    #[test]
    fn json_summary_truncates_long_values() {
        let long: String = "x".repeat(300);
        let v = serde_json::json!({ "k": long });
        let s = json_summary(&v);
        // `…` is a 3-byte UTF-8 char appended to a TOOL_ARG_SUMMARY_MAX-byte prefix.
        assert!(s.len() <= TOOL_ARG_SUMMARY_MAX + 3, "len={}", s.len());
        assert!(s.ends_with('…'));
    }
}

// Rust guideline compliant 2026-02-21
