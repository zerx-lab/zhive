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
    CommandExecutionStatus, Item, ItemContent, NoticeLevel, PlanStepStatus, ToolCallStatus,
};

use crate::app::App;
use crate::conversation::TurnLifecycle;
use crate::theme::Palette;
use crate::widgets::{self, Hint};
use crate::{markdown, wrap};

/// Width of the left role-label gutter, in cells.
const GUTTER: u16 = 8;

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
    let scroll_y = max_scroll.saturating_sub(app.scrollback);
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
            push_message(
                &mut out,
                "sys",
                p.error,
                vec![Line::styled(
                    format!("turn failed: {message}"),
                    Style::new().fg(p.error),
                )],
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
        Item::ToolCall { name, status, .. } => tool_call_lines(name, *status, p),
        Item::CommandExecution {
            command,
            status,
            exit_code,
            ..
        } => command_lines(command, *status, *exit_code, p, width),
        Item::Diff { path, .. } => vec![Line::styled(
            format!("± {}", path.display()),
            Style::new().fg(p.info),
        )],
        Item::FileEdit { changes, .. } => {
            let count = changes.len();
            vec![Line::styled(
                format!("✎ file edit · {count} change(s)"),
                Style::new().fg(p.info),
            )]
        }
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

/// Renders a tool-call header line: `▸ name  <status>`.
fn tool_call_lines(name: &str, status: ToolCallStatus, p: &Palette) -> Vec<Line<'static>> {
    let (label, color) = match status {
        ToolCallStatus::Pending => ("pending", p.fg_dim),
        ToolCallStatus::InProgress => ("running", p.warn),
        ToolCallStatus::Completed => ("ok", p.success),
        ToolCallStatus::Failed => ("failed", p.error),
        _ => ("…", p.fg_dim),
    };
    vec![Line::from(vec![
        Span::styled(
            format!("▸ {name}"),
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(label.to_owned(), Style::new().fg(color)),
    ])]
}

/// Renders a command-execution block: `$ cmd` plus a status line.
fn command_lines(
    command: &str,
    status: CommandExecutionStatus,
    exit_code: Option<i32>,
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let head = Line::from(vec![
        Span::styled("$ ", Style::new().fg(p.accent)),
        Span::styled(command.to_owned(), Style::new().fg(p.fg)),
    ]);
    out.extend(wrap::wrap_line(&head, width));
    let (label, color) = match status {
        CommandExecutionStatus::InProgress => ("running".to_owned(), p.warn),
        CommandExecutionStatus::Completed => {
            (format!("exit {}", exit_code.unwrap_or(0)), p.success)
        }
        CommandExecutionStatus::Failed => (format!("exit {}", exit_code.unwrap_or(-1)), p.error),
        _ => ("…".to_owned(), p.fg_dim),
    };
    out.push(Line::styled(label, Style::new().fg(color)));
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

// Rust guideline compliant 2026-02-21
