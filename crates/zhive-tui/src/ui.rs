//! Rendering of the conversation shell: top bar, transcript, and composer.
//!
//! Follows the `zap-tui-design` shell — a top status bar (brand, `cwd · branch
//! · session` breadcrumb, right-aligned model pill), shown in every state, the
//! conversation body
//! (a 2-cell role-glyph gutter beside each message), and the composer panel
//! whose right-aligned status slot carries the ready / working / disconnected
//! signal (there is no bottom key-hint bar). Messages are pre-wrapped with
//! [`crate::wrap`] so continuation rows stay indented under the gutter. Modal
//! overlays ([`Overlay`]) draw over a [`Clear`]ed centered rect.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use zhive_proto::domain::{
    CommandExecutionStatus, Item, ItemContent, ItemToolCallContent, NoticeLevel, PlanStepStatus,
    ToolCallStatus,
};

use crate::app::App;
use crate::conversation::{SubagentStatus, SubagentView, TurnLifecycle};
use crate::logo;
use crate::theme::Palette;
use crate::widgets;
use crate::{markdown, wrap};

/// Width of the left role-glyph gutter, in cells (1 glyph + 1 space).
///
/// Crate-visible because the selection mapping in [`crate::app`] subtracts it to
/// translate a mouse column into a content-relative cell.
pub(crate) const GUTTER: u16 = 2;

/// Maximum queued-message preview rows shown above the composer.
const QUEUE_PREVIEW_ROWS: usize = 3;

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

/// Maximum characters of the inline primary argument shown in a tool header.
///
/// Shorter than [`TOOL_ARG_SUMMARY_MAX`] so the `name arg   status` header
/// stays on one line beside the status label.
const TOOL_HEADER_ARG_MAX: usize = 56;

/// Maximum lines of command output shown before truncation.
///
/// Mirrors [`TOOL_PREVIEW_LINES`] for visual consistency.
const CMD_OUTPUT_LINES: usize = 8;

/// Draws the entire frame for the current [`App`] state.
///
/// Takes `&mut App` so the transcript renderer can anchor the scroll position
/// when streaming output lands while the user is reading history (see
/// [`render_body`]).
pub fn draw(frame: &mut Frame, app: &mut App) {
    let p = app.palette;
    let area = frame.area();
    // Background fill.
    frame.render_widget(Paragraph::new("").style(Style::new().bg(p.bg)), area);

    // The composer spans the full width; its wrappable text width is the panel
    // width minus the left/right borders (2) and the panel's horizontal padding
    // (1 each, see `widgets::panel`). This must match `block.inner(...).width` in
    // `render_composer` exactly, or the height estimate and the rendered wrap
    // disagree and a boundary-filling line can scroll itself out of view.
    let composer_inner_width = area.width.saturating_sub(4);
    let composer_h = composer_height(app, composer_inner_width);
    let queue_h = queue_height(app);
    let [top, body, queue, composer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(queue_h),
        Constraint::Length(composer_h),
    ])
    .areas(area);

    render_top_bar(frame, app, top);
    render_body(frame, app, body);
    render_queue(frame, app, queue);
    let (composer_inner, composer_scroll) = render_composer(frame, app, composer);

    // Place the caret in the composer when no overlay is capturing input. The
    // caret follows the same soft-wrap layout as the rendered text, offset by
    // the composer's vertical scroll so it stays visible in a tall draft.
    if app.overlay.is_none() {
        let (col, row) = app.input.cursor_visual_col_row(composer_inner.width);
        let cx = composer_inner.x.saturating_add(col);
        let cy = composer_inner
            .y
            .saturating_add(row.saturating_sub(composer_scroll));
        if cx < composer_inner.x.saturating_add(composer_inner.width)
            && cy < composer_inner.y.saturating_add(composer_inner.height)
        {
            frame.set_cursor_position((cx, cy));
        }
    }

    // The slash palette floats above the composer while a command is typed;
    // the `@`-mention file picker takes the same slot while a mention is typed.
    if app.overlay.is_none() && app.palette_query().is_some() {
        crate::overlays::render_palette(frame, app, composer);
    } else if app.overlay.is_none() && app.mention_query().is_some() {
        crate::overlays::render_mention(frame, app, composer);
    }

    if let Some(overlay) = &app.overlay {
        crate::overlays::render_overlay(frame, app, overlay, area);
    }
}

/// Computes the composer panel height (input rows + borders), clamped.
///
/// `inner_width` is the wrappable text width (panel width minus its borders);
/// the row count reflects soft-wrapped lines, not just hard newlines, so a long
/// line that wraps grows the panel instead of being clipped.
fn composer_height(app: &App, inner_width: u16) -> u16 {
    let wrapped = app.input.wrap_rows(inner_width).len();
    // Grow to keep the caret's row in view (a row filled exactly to the edge
    // pushes the caret onto a phantom next row).
    let (_, cursor_row) = app.input.cursor_visual_col_row(inner_width);
    let rows = wrapped.max(usize::from(cursor_row) + 1);
    u16::try_from(rows).unwrap_or(1).clamp(1, 6) + 2
}

/// Rows reserved for the queued-message preview (0 when the queue is empty).
///
/// One header row plus up to [`QUEUE_PREVIEW_ROWS`] preview rows.
fn queue_height(app: &App) -> u16 {
    let shown = app.message_queue.len().min(QUEUE_PREVIEW_ROWS);
    if shown == 0 {
        0
    } else {
        u16::try_from(shown).unwrap_or(3) + 1
    }
}

/// Renders the top status bar: brand, `cwd · branch · session` breadcrumb, and
/// a right-aligned model pill (with a token count once a turn has run).
///
/// Shown in every state, including the welcome screen, so the header always
/// carries the brand and working directory — the welcome card stays lean and
/// does not repeat them.
fn render_top_bar(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;
    let bar_bg = Style::new().bg(p.bg_elev);
    let sep = || Span::styled(" · ", Style::new().fg(p.fg_mute));
    // The bar leads with the working directory; a single leading space keeps
    // it off the edge.
    let mut left = vec![
        Span::raw(" "),
        Span::styled(app.config.cwd_display(), Style::new().fg(p.fg_dim)),
    ];
    if let Some(branch) = &app.config.branch {
        left.push(sep());
        left.push(Span::styled(branch.clone(), Style::new().fg(p.fg_dim)));
    }
    if let Some(session) = &app.config.session_name {
        left.push(sep());
        left.push(Span::styled(session.clone(), Style::new().fg(p.fg_dim)));
    }

    // Build the right-aligned section: optional token count then model pill.
    let mut right_spans = Vec::new();
    if let Some((input, output)) = app.last_usage {
        right_spans.push(Span::styled(
            format!("↑{input} ↓{output} tok  "),
            Style::new().fg(p.fg_dim),
        ));
    }
    // Active reasoning depth, shown only above `Off` and accent-colored so it
    // stands out next to the muted model pill (mirrors opencode's variant tag).
    if app.thinking_effort.is_enabled() {
        right_spans.push(Span::styled(
            format!("think:{}  ", app.thinking_effort.label()),
            Style::new().fg(p.accent),
        ));
    }
    // Muted model label — a single brand accent (the `⚡ zhive` mark) is enough;
    // a highlighted pill here reads as loud next to it.
    right_spans.push(Span::styled(
        format!("{} · {}", app.config.provider_label, app.config.model_label),
        Style::new().fg(p.fg_dim),
    ));
    let pill = Line::from(right_spans).right_aligned();

    frame.render_widget(Paragraph::new(Line::from(left)).style(bar_bg), area);
    frame.render_widget(Paragraph::new(pill).style(bar_bg), area);
}

/// Renders the conversation body (or the welcome view when empty).
///
/// Takes `&mut App` to anchor the scroll position: when the user has scrolled
/// up and the transcript grows, [`App::scrollback`] is bumped by the growth so
/// the viewed content stays put instead of sliding up under streaming output.
/// `p` is an owned [`Palette`] copy so the later scroll mutation does not clash
/// with a live borrow of `app`.
fn render_body(frame: &mut Frame, app: &mut App, area: Rect) {
    let p = app.palette;

    // The welcome screen draws on the bare body area — no `conversation` panel
    // frame — so the guidance floats on open whitespace below the top bar.
    if app.conversation.is_empty() {
        render_welcome(frame, app, area);
        return;
    }

    let status = if app.conversation.busy {
        format!("{} working", widgets::spinner(app.spinner_tick))
    } else {
        format!("idle · {} msgs", app.conversation.item_count())
    };
    // Border carries the activity signal: accent while busy, dim when idle.
    let block = widgets::panel("conversation", Some(&status), app.conversation.busy, &p);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = transcript_lines(app, inner.width);
    let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let visible = inner.height;
    let max_scroll = total.saturating_sub(visible);
    // Anchor the viewport when the user has scrolled up and new output streamed
    // in below the fold: bump scrollback by the line growth so the content under
    // the eye stays fixed. Skipped at the tail (`scrollback == 0` keeps
    // following) and on a width change (a resize reflows lines, so the delta is
    // not newly streamed output).
    if app.scrollback > 0 && inner.width == app.last_width && total > app.last_total {
        let grew = total - app.last_total;
        app.scrollback = app.scrollback.saturating_add(grew).min(max_scroll);
    }
    app.last_total = total;
    app.last_width = inner.width;
    // Record the rendered max so the key handler's ctrl+Home can pin to the
    // exact top (see `App::viewport_max_scroll`).
    app.viewport_max_scroll.set(max_scroll);
    // Clamp scrollback to [0, max_scroll] so we never scroll past the top or
    // get stuck above it when the transcript shrinks (e.g. after /clear).
    let scrollback = app.scrollback.min(max_scroll);
    let scroll_y = max_scroll.saturating_sub(scrollback);

    // Capture geometry + per-line body text for mouse selection. The text clone
    // is gated on an active selection (there is always a redraw between
    // mouse-down and copy), so an idle transcript pays nothing.
    //
    // LOAD-BEARING: the lines are already wrapped to `inner.width`, so the
    // `Paragraph` below must NOT call `.wrap()` — one transcript line is exactly
    // one screen row, which is what makes `scroll_y + row` an exact line index
    // for both the hit-test and the highlight. Adding wrapping breaks selection.
    app.sel_geom.set(crate::app::SelGeom::new(inner, scroll_y));
    if app.selection.is_some() {
        let mut body = app.sel_lines.borrow_mut();
        body.clear();
        body.extend(lines.iter().map(line_body_text));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((scroll_y, 0)),
        inner,
    );

    // Overpaint the selection background on top of the rendered text.
    if let Some(sel) = app.selection {
        paint_selection(frame, app, inner, scroll_y, sel);
    }

    // Scroll-to-bottom affordance when the view is not pinned to the tail.
    if scrollback > 0 && inner.height > 0 {
        let hint = " ↓ end ";
        let hw = u16::try_from(UnicodeWidthStr::width(hint)).unwrap_or(0);
        if inner.width > hw {
            let hint_area = Rect {
                x: inner.x + inner.width - hw,
                y: inner.y + inner.height - 1,
                width: hw,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::styled(hint, Style::new().fg(p.fg_mute).bg(p.bg_elev))),
                hint_area,
            );
        }
    }
}

/// Plain body text of a transcript line, with the role gutter removed.
///
/// Concatenates the line's spans and drops the leading [`GUTTER`] display
/// columns (the role glyph + pad, or the blank continuation gutter) so copied
/// text never carries the gutter.
fn line_body_text(line: &Line<'_>) -> String {
    let full: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    drop_leading_cells(&full, GUTTER).to_owned()
}

/// Returns `text` with its first `cells` display columns removed.
///
/// Width-aware so a wide leading glyph is skipped by its true column span.
fn drop_leading_cells(text: &str, cells: u16) -> &str {
    let target = u32::from(cells);
    let mut col: u32 = 0;
    for (idx, ch) in text.char_indices() {
        if col >= target {
            return text.get(idx..).unwrap_or("");
        }
        col += u32::try_from(ch.width().unwrap_or(0)).unwrap_or(0);
    }
    ""
}

/// Recolors every span on `line` to the muted foreground, preserving modifiers.
///
/// Used to render a compaction handoff summary as folded history: the markdown
/// structure (bold, code) is kept but uniformly dimmed so it reads as condensed
/// context rather than a fresh agent message.
fn dim_line(line: Line<'static>, p: &Palette) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|span| {
            let style = span.style.fg(p.fg_dim);
            Span::styled(span.content, style)
        })
        .collect();
    Line::from(spans)
}

/// Overpaints the selection background onto the visible transcript rows.
///
/// Sets the background of each selected content cell to `sel_bg`. Uses
/// [`ratatui::buffer::Buffer::cell_mut`] so an out-of-bounds cell is skipped
/// rather than panicking.
fn paint_selection(
    frame: &mut Frame,
    app: &App,
    body: Rect,
    scroll_y: u16,
    sel: crate::app::Selection,
) {
    let sel_bg = app.palette.sel_bg;
    let lines = app.sel_lines.borrow();
    let content_x0 = body.x.saturating_add(GUTTER);
    let right = body.x.saturating_add(body.width);
    let buf = frame.buffer_mut();
    for row in 0..body.height {
        let line_idx = usize::from(scroll_y) + usize::from(row);
        let Some(text) = lines.get(line_idx) else {
            break;
        };
        let line_width = u16::try_from(UnicodeWidthStr::width(text.as_str())).unwrap_or(u16::MAX);
        let Some((from, to)) = crate::app::cell_range_for_line(sel, line_idx, line_width) else {
            continue;
        };
        let y = body.y.saturating_add(row);
        let start = content_x0.saturating_add(from);
        let end = content_x0.saturating_add(to).min(right);
        for x in start..end {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(sel_bg);
            }
        }
    }
}

/// The tool name the engine uses for a subagent spawn (drives summary inlining).
const SUBAGENT_TOOL_NAME: &str = "agent";

/// Builds the fully-wrapped, gutter-prefixed transcript lines.
///
/// Also rebuilds [`App::toggle_zones`]: one clickable region per collapsible
/// item, recording the transcript line range its block occupies so a left click
/// can toggle just that item.
fn transcript_lines(app: &App, inner_width: u16) -> Vec<Line<'static>> {
    let p = &app.palette;
    let content_width = inner_width.saturating_sub(GUTTER);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut zones: Vec<crate::app::ToggleZone> = Vec::new();
    // Subagents are consumed in spawn order: each `agent` tool call in the main
    // flow pulls the next registered subagent's live summary in beneath it.
    let mut next_subagent = 0usize;

    for turn in &app.conversation.turns {
        for item in &turn.items {
            let (label, color) = role_of(item, p);
            let start = out.len();
            let body = item_body(
                item,
                app.item_is_expanded(item.id()),
                p,
                content_width,
                &app.render_cache,
            );
            push_message(&mut out, label, color, body);
            if is_subagent_call(item)
                && let Some(sub) = app.conversation.subagents.get(next_subagent)
            {
                out.extend(subagent_summary_lines(sub, app.spinner_tick, p));
                next_subagent += 1;
            }
            // Register the rendered block as clickable so a left click toggles
            // just this item's expansion (mirrors the global ctrl+o baseline).
            if item_collapsible(item) {
                zones.push(crate::app::ToggleZone::new(
                    start,
                    out.len(),
                    item.id().clone(),
                ));
            }
            out.push(Line::raw(""));
        }
        if let TurnLifecycle::Failed { message } = &turn.status {
            // Wrap the (often long) provider error to the content width so the
            // full message is visible across rows instead of being clipped.
            push_message(
                &mut out,
                "✗",
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
        let reasoning = app.conversation.revealed_reasoning();
        let answer = app.conversation.revealed_streaming();
        // Live reasoning trace (dim italic), streamed above the answer. A trailing
        // cursor marks it as in-flight only until the answer body starts. It is
        // superseded by the finalised `Item::Reasoning` once the block closes.
        if !reasoning.is_empty() {
            let mut rlines = wrap_plain(
                reasoning,
                Style::new().fg(p.fg_dim).add_modifier(Modifier::ITALIC),
                content_width,
            );
            if answer.is_empty()
                && let Some(last) = rlines.last_mut()
            {
                last.push_span(Span::styled("▌", Style::new().fg(p.accent)));
            }
            push_message(&mut out, "", p.role_zap, rlines);
        }
        if !answer.is_empty() {
            // Render the live partial text as markdown with a trailing cursor.
            // Heal the tail first so an unclosed marker mid-stream renders in its
            // eventual style instead of flashing a literal `**` / `` ` `` / `[`.
            let healed = crate::heal::heal_tail(answer);
            let mut lines = Vec::new();
            for line in markdown::render(&healed, p) {
                lines.extend(wrap::wrap_line(&line, content_width));
            }
            if let Some(last) = lines.last_mut() {
                last.push_span(Span::styled("▌", Style::new().fg(p.accent)));
            }
            push_message(&mut out, "", p.role_zap, lines);
        } else if reasoning.is_empty() {
            // Nothing revealed yet — show the generic working placeholder.
            push_message(
                &mut out,
                "",
                p.role_zap,
                vec![Line::styled(
                    format!("{} thinking…", widgets::spinner(app.spinner_tick)),
                    Style::new().fg(p.fg_dim),
                )],
            );
        }
    }

    // Live compaction progress / failure panel at the transcript tail.
    if let Some(view) = &app.compaction {
        out.extend(compaction_panel_lines(
            view,
            app.spinner_tick,
            content_width,
            p,
        ));
    }
    // Hand the freshly-built click regions to the event loop for hit-testing.
    *app.toggle_zones.borrow_mut() = zones;
    out
}

/// Whether `item` renders as a collapsible block (so a click can toggle it).
///
/// Tool calls, command executions, diffs, and file edits always collapse their
/// output; a user message collapses only when it is a `/skill:<name>` chip.
fn item_collapsible(item: &Item) -> bool {
    match item {
        Item::ToolCall { .. }
        | Item::CommandExecution { .. }
        | Item::Diff { .. }
        | Item::FileEdit { .. } => true,
        Item::UserMessage { content, .. } => skill_invocation_name(&user_text(content)).is_some(),
        _ => false,
    }
}

/// Builds the live compaction panel: a streaming summary, or a failure notice.
///
/// In progress: a spinner divider plus the summary streamed so far. On failure:
/// a persistent `✗` divider plus the wrapped reason, so it stays visible in the
/// transcript rather than vanishing with a transient flash.
fn compaction_panel_lines(
    view: &crate::app::CompactionView,
    spinner_tick: usize,
    width: u16,
    p: &Palette,
) -> Vec<Line<'static>> {
    let mut out = vec![Line::raw("")];
    if let Some(reason) = &view.error {
        out.push(Line::styled(
            "─── ✗ compaction failed ───",
            Style::new().fg(p.error),
        ));
        out.extend(wrap_plain(reason, Style::new().fg(p.error), width));
        return out;
    }
    let label = if view.entries > 0 {
        format!(
            "─── {} compacting… ({} entries) ───",
            widgets::spinner(spinner_tick),
            view.entries
        )
    } else {
        format!("─── {} compacting… ───", widgets::spinner(spinner_tick))
    };
    out.push(Line::styled(label, Style::new().fg(p.warn)));
    let body = view.summary.trim();
    if body.is_empty() {
        out.push(Line::styled(
            "preparing summary…",
            Style::new().fg(p.fg_dim),
        ));
    } else {
        for line in markdown::render(body, p) {
            out.extend(wrap::wrap_line(&line, width));
        }
    }
    out
}

/// `true` when `item` is the `agent` tool call that spawns a subagent.
fn is_subagent_call(item: &Item) -> bool {
    matches!(item, Item::ToolCall { name, .. } if name == SUBAGENT_TOOL_NAME)
}

/// Builds the inline opencode-style live summary for one subagent.
///
/// Two indented rows under the parent's `agent` call: a status icon plus
/// `agent_type · description`, then `↳ N toolcalls · <current tool | done>`.
/// The icon is a spinner while running, `✓` on success, `✗` on failure.
fn subagent_summary_lines(
    sub: &SubagentView,
    spinner_tick: usize,
    p: &Palette,
) -> Vec<Line<'static>> {
    let (icon, icon_color) = match sub.status {
        SubagentStatus::Running => (widgets::spinner(spinner_tick).to_owned(), p.warn),
        SubagentStatus::Completed { .. } => ("✓".to_owned(), p.success),
        SubagentStatus::Failed => ("✗".to_owned(), p.error),
    };
    let kind = sub.agent_type.as_deref().unwrap_or("subagent");
    let header = if let Some(desc) = sub.description.as_deref().filter(|d| !d.is_empty()) {
        format!("{kind} · {desc}")
    } else {
        kind.to_owned()
    };

    let count = sub.tool_call_count();
    let tail = match sub.status {
        SubagentStatus::Running => sub
            .current_tool()
            .map_or_else(|| "working".to_owned(), |t| format!("running {t}")),
        SubagentStatus::Completed { .. } | SubagentStatus::Failed => "done".to_owned(),
    };

    vec![
        Line::from(vec![
            Span::styled(format!("  {icon} "), Style::new().fg(icon_color)),
            Span::styled(
                header,
                Style::new().fg(p.role_zap).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![Span::styled(
            format!("    ↳ {count} toolcalls · {tail}"),
            Style::new().fg(p.fg_dim),
        )]),
    ]
}

/// Appends a role-gutter-prefixed message (glyph on row 0, blank on the rest).
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
    let pad = usize::from(GUTTER);
    for (i, line) in body.into_iter().enumerate() {
        let gutter = if i == 0 {
            // Role glyph (or blank), left-padded to the gutter width.
            Span::styled(
                format!("{label:<pad$}"),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            )
        } else {
            // Continuation rows: blank gutter keeps the body left edge aligned.
            Span::raw(" ".repeat(pad))
        };
        let mut spans = vec![gutter];
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
}

/// Returns the `(glyph, color)` for an item's 2-cell role gutter.
///
/// User messages get a colored `❯`; agent messages carry no glyph (role shown
/// by color alone); system notices get a thin `·`.
fn role_of(item: &Item, p: &Palette) -> (&'static str, ratatui::style::Color) {
    match item {
        Item::UserMessage { .. } => ("❯", p.role_you),
        Item::AgentMessage { .. } | Item::AgentThought { .. } | Item::Reasoning { .. } => {
            ("", p.role_zap)
        }
        Item::SystemNotice { .. } => ("·", p.role_system),
        _ => ("", p.fg_dim),
    }
}

/// Renders an item's body into wrapped lines (no gutter).
///
/// `expanded` controls whether a `/skill:<name>` invocation message shows its
/// full injected block (ctrl+o) or a one-line `[skill] <name>` chip.
#[expect(
    clippy::too_many_lines,
    reason = "exhaustive per-Item-variant dispatch; splitting would scatter related arms"
)]
fn item_body(
    item: &Item,
    expanded: bool,
    p: &Palette,
    width: u16,
    cache: &crate::render_cache::MarkdownCache,
) -> Vec<Line<'static>> {
    match item {
        Item::UserMessage { content, .. } => user_message_lines(content, expanded, p, width),
        Item::AgentMessage { text, .. } => {
            // A compaction handoff summary carries the "[context summary]"
            // prefix. Strip that marker line and dim the body so the folded
            // history reads as muted, visually distinct from live agent output
            // (the `─── Compaction ───` divider above already labels it).
            if let Some(body) = text.strip_prefix("[context summary]\n") {
                let mut out = Vec::new();
                for line in cache.render(body, p) {
                    for wrapped in wrap::wrap_line(&line, width) {
                        out.push(dim_line(wrapped, p));
                    }
                }
                out
            } else {
                let mut out = Vec::new();
                for line in cache.render(text, p) {
                    out.extend(wrap::wrap_line(&line, width));
                }
                out
            }
        }
        Item::AgentThought { text, .. } => wrap_plain(
            text,
            Style::new().fg(p.fg_dim).add_modifier(Modifier::ITALIC),
            width,
        ),
        Item::Reasoning { summary, .. } => wrap_plain(
            &summary.join(" "),
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
            expanded,
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
            expanded,
            p,
            width,
        ),
        Item::Diff {
            path,
            old_text,
            new_text,
            ..
        } => crate::diff::render_file_diff(
            path.to_string_lossy().as_ref(),
            old_text.as_deref(),
            Some(new_text.as_str()),
            expanded,
            p,
            width,
        ),
        Item::FileEdit { changes, .. } => {
            let mut out = Vec::new();
            for change in changes {
                out.extend(crate::diff::render_file_change(change, expanded, p, width));
            }
            out
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
            vec![Line::styled("─── Compaction ───", Style::new().fg(p.info))]
        }
        other => vec![Line::styled(
            format!("[{}]", item_kind_name(other)),
            Style::new().fg(p.fg_mute),
        )],
    }
}

/// Renders a user message: a `/skill:<name>` invocation as a chip, else plain text.
///
/// A `/skill:<name>` run injects a `<skill>…</skill>` block as the user message;
/// it is shown like a tool call (a collapsible `[skill] <name>` chip) rather than
/// as raw XML.
fn user_message_lines(
    content: &[ItemContent],
    expanded: bool,
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let text = user_text(content);
    if let Some(name) = skill_invocation_name(&text) {
        skill_invocation_lines(name, &text, expanded, p, width)
    } else if text.contains("<file path=\"") && text.contains("</file>") {
        mention_message_lines(&text, p, width)
    } else {
        wrap_plain(&text, Style::new().fg(p.fg), width)
    }
}

/// Renders a user message carrying inlined `@`-mention file blocks as chips.
///
/// Each `<file path="…" type="…">…</file>` block — appended by
/// [`crate::files::expand_mentions`] so the model receives the referenced
/// contents — is collapsed to a compact `dir|file <path>` chip. The block body
/// is intentionally dropped from the transcript: the user only needs to see
/// *which* path was attached (mirroring opencode / codex / pi), while the full
/// content still reaches the model. Surrounding prose (the typed `@path`)
/// renders normally.
fn mention_message_lines(text: &str, p: &Palette, width: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<file path=\"") {
        // Prose preceding this block (trim the blank separator lines).
        let lead = rest[..start].trim_matches('\n');
        if !lead.is_empty() {
            out.extend(wrap_plain(lead, Style::new().fg(p.fg), width));
        }
        let after = &rest[start..];
        let Some(end) = after.find("</file>") else {
            // Malformed (no closing tag): show the remainder verbatim and stop.
            out.extend(wrap_plain(after, Style::new().fg(p.fg), width));
            return out;
        };
        out.push(file_chip(&after[..end], p));
        rest = &after[end + "</file>".len()..];
    }
    let tail = rest.trim_matches('\n');
    if !tail.is_empty() {
        out.extend(wrap_plain(tail, Style::new().fg(p.fg), width));
    }
    out
}

/// Builds a compact `dir|file <path>` chip from a `<file …>` opening tag.
fn file_chip(block: &str, p: &Palette) -> Line<'static> {
    let path = tag_attr(block, "path=\"").unwrap_or_default();
    let kind = match tag_attr(block, "type=\"").as_deref() {
        Some("directory") => "dir",
        _ => "file",
    };
    Line::from(vec![
        Span::styled(
            format!(" {kind} "),
            Style::new()
                .fg(p.bg)
                .bg(p.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(path, Style::new().fg(p.fg_dim)),
    ])
}

/// Extracts the double-quoted value following the first `key` in `block`.
///
/// `key` is the attribute lead-in including the opening quote, e.g. `path="`.
/// Returns `None` when the attribute or its closing quote is absent.
fn tag_attr(block: &str, key: &str) -> Option<String> {
    let rest = block.split(key).nth(1)?;
    let (value, _) = rest.split_once('"')?;
    Some(value.to_owned())
}

/// Extracts the skill name when `text` is a `<skill name="…">…</skill>` block.
///
/// This is the invocation block injected by a `/skill:<name>` run; ordinary user
/// messages return `None` and render verbatim.
fn skill_invocation_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("<skill name=\"")?;
    let (name, _) = rest.split_once('"')?;
    // Require the closing tag so arbitrary text that merely starts with the
    // prefix is not mistaken for a skill block.
    if text.contains("</skill>") {
        Some(name)
    } else {
        None
    }
}

/// Renders a skill invocation as a collapsible chip, like a tool call.
///
/// Collapsed (default): a one-line `[skill] <name> (ctrl+o to expand)`.
/// Expanded: the same header plus the full injected block, dimmed.
fn skill_invocation_lines(
    name: &str,
    raw: &str,
    expanded: bool,
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let hint = if expanded {
        "click or ctrl+o to collapse"
    } else {
        "click or ctrl+o to expand"
    };
    let header = Line::from(vec![
        Span::styled(
            "[skill] ",
            Style::new().fg(p.role_zap).add_modifier(Modifier::BOLD),
        ),
        Span::styled(name.to_owned(), Style::new().fg(p.fg)),
        Span::styled(format!("  ({hint})"), Style::new().fg(p.fg_dim)),
    ]);
    if !expanded {
        return vec![header];
    }
    let mut out = vec![header];
    out.extend(wrap_plain(raw, Style::new().fg(p.fg_dim), width));
    out
}

/// Renders a tool-call block: header, argument summary, and output.
///
/// `expanded` (the global ctrl+o toggle) shows the full output; otherwise the
/// output is capped at [`TOOL_PREVIEW_LINES`] with a `ctrl+o to expand` hint.
#[expect(
    clippy::too_many_arguments,
    reason = "leaf renderer: each argument is distinct display data"
)]
fn tool_call_lines(
    name: &str,
    status: ToolCallStatus,
    raw_input: Option<&serde_json::Value>,
    raw_output: Option<&serde_json::Value>,
    content: &[ItemToolCallContent],
    expanded: bool,
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    // An active call draws attention (accent + bold); a finished one recedes
    // into muted history, like a skill chip. A failure keeps its red status so
    // it stays noticeable even when dimmed.
    let active = matches!(status, ToolCallStatus::Pending | ToolCallStatus::InProgress);
    let (status_label, status_color) = match status {
        ToolCallStatus::Pending => ("pending", p.fg_dim),
        ToolCallStatus::InProgress => ("running", p.warn),
        ToolCallStatus::Completed => ("ok", p.fg_mute),
        ToolCallStatus::Failed => ("failed", p.error),
        _ => ("…", p.fg_dim),
    };

    // Compact single-line header (pi-style): `▸ name primary-arg   status`. The
    // most salient argument (command / path / pattern) sits inline beside the
    // name instead of on a separate verbose `args: {json}` row.
    let name_style = if active {
        Style::new().fg(p.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(p.fg_dim)
    };
    let arg_style = Style::new().fg(if active { p.fg } else { p.fg_mute });
    let mut header = vec![Span::styled(format!("▸ {name}"), name_style)];
    if let Some(arg) = raw_input.and_then(primary_arg_summary) {
        header.push(Span::styled(format!(" {arg}"), arg_style));
    }
    header.push(Span::raw("  "));
    header.push(Span::styled(
        status_label.to_owned(),
        Style::new().fg(status_color),
    ));
    let mut out = wrap::wrap_line(&Line::from(header), width);

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
        out.extend(detail_output_lines(
            &text,
            TOOL_PREVIEW_LINES,
            expanded,
            !active,
            p,
        ));
    }

    out
}

/// Renders captured output as indented lines, collapsed or expanded.
///
/// Collapsed: at most `preview` lines, then `… N more lines · ctrl+o to expand`.
/// Expanded: every line, then `ctrl+o to collapse` when it had been truncatable.
/// `dimmed` mutes the result text once the call has finished so a completed
/// tool block recedes into history. Shared by [`tool_call_lines`] and
/// [`command_lines`].
fn detail_output_lines(
    text: &str,
    preview: usize,
    expanded: bool,
    dimmed: bool,
    p: &Palette,
) -> Vec<Line<'static>> {
    let lines: Vec<&str> = text.lines().collect();
    let visible = if expanded {
        lines.len()
    } else {
        lines.len().min(preview)
    };

    let text_fg = if dimmed { p.fg_dim } else { p.fg };
    let mut out: Vec<Line<'static>> = lines[..visible]
        .iter()
        .map(|line| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    (*line).to_owned(),
                    Style::new().fg(text_fg).bg(p.bg_overlay),
                ),
            ])
        })
        .collect();

    let hidden = lines.len() - visible;
    if hidden > 0 {
        out.push(Line::styled(
            format!("  … {hidden} more lines · click or ctrl+o"),
            Style::new().fg(p.fg_mute),
        ));
    } else if expanded && lines.len() > preview {
        out.push(Line::styled(
            "  click or ctrl+o to collapse".to_owned(),
            Style::new().fg(p.fg_mute),
        ));
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

/// Extracts the most salient argument from a tool's `raw_input` for the inline
/// header.
///
/// Prefers a known primary key (command / path / pattern / …); otherwise falls
/// back to a compact JSON summary. Returns `None` for argument-less or empty
/// inputs so the header omits the slot entirely.
fn primary_arg_summary(input: &serde_json::Value) -> Option<String> {
    const KEYS: [&str; 7] = [
        "command",
        "path",
        "file_path",
        "pattern",
        "query",
        "url",
        "name",
    ];
    for key in KEYS {
        if let Some(s) = input.get(key).and_then(serde_json::Value::as_str)
            && !s.is_empty()
        {
            return Some(clip(s, TOOL_HEADER_ARG_MAX));
        }
    }
    let summary = json_summary(input);
    if summary.is_empty() || summary == "{}" {
        None
    } else {
        Some(clip(&summary, TOOL_HEADER_ARG_MAX))
    }
}

/// Clips `s` to at most `max` characters, appending `…` when shortened.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
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
///
/// `expanded` (the global ctrl+o toggle) shows the full output; otherwise it is
/// capped at [`CMD_OUTPUT_LINES`] with a `ctrl+o to expand` hint.
#[expect(
    clippy::too_many_arguments,
    reason = "leaf renderer: each argument is distinct display data"
)]
fn command_lines(
    command: &str,
    status: CommandExecutionStatus,
    exit_code: Option<i32>,
    aggregated_output: Option<&str>,
    duration_ms: Option<i64>,
    expanded: bool,
    p: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let head = Line::from(vec![
        Span::styled("$ ", Style::new().fg(p.accent)),
        Span::styled(command.to_owned(), Style::new().fg(p.fg)),
    ]);
    out.extend(wrap::wrap_line(&head, width));

    if let Some(output) = aggregated_output {
        // Mute the captured output once the command has finished, matching the
        // tool-call treatment so completed blocks recede into history.
        let done = matches!(
            status,
            CommandExecutionStatus::Completed | CommandExecutionStatus::Failed
        );
        out.extend(detail_output_lines(
            output,
            CMD_OUTPUT_LINES,
            expanded,
            done,
            p,
        ));
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

/// One-line tagline shown under the welcome wordmark.
const WELCOME_TAGLINE: &str = "the terminal coding agent";

/// Renders the welcome screen: a centered `ZHIVE` wordmark over a tagline.
///
/// The block-letter logo is a single muted gray and plays a white ripple from
/// the click point on a left click (see [`crate::logo`]); a muted
/// tagline sits one row below. The logo's screen rect is recorded in
/// [`App::logo_hit`] so the event loop can hit-test clicks; it is cleared first
/// and only re-set when the group fits, so a too-small body never leaves a
/// stale rect behind.
fn render_welcome(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;
    // Clear any prior hit-rect; re-set below only if the logo is placed.
    app.logo_hit.set(None);
    let tag_w = u16::try_from(UnicodeWidthStr::width(WELCOME_TAGLINE)).unwrap_or(0);
    let group_w = logo::WIDTH.max(tag_w);
    // Logo rows, a blank spacer row, then the tagline.
    let group_h = logo::HEIGHT + 2;
    if area.width < group_w || area.height < group_h {
        return; // body too small to place the wordmark cleanly
    }
    // Center the group as a whole, both axes.
    let top = area.y + area.height.saturating_sub(group_h) / 2;
    let logo_x = area.x + area.width.saturating_sub(logo::WIDTH) / 2;
    let logo_area = Rect {
        x: logo_x,
        y: top,
        width: logo::WIDTH,
        height: logo::HEIGHT,
    };
    let lines = app.logo.render();
    frame.render_widget(Paragraph::new(Text::from(lines)), logo_area);
    app.logo_hit.set(Some(logo_area));

    let tag_x = area.x + area.width.saturating_sub(tag_w) / 2;
    let tag_area = Rect {
        x: tag_x,
        y: top + logo::HEIGHT + 1,
        width: tag_w,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::styled(WELCOME_TAGLINE, Style::new().fg(p.fg_dim))),
        tag_area,
    );
}

/// Renders the composer panel and returns its inner rect and scroll offset.
///
/// The returned `u16` is the number of visual rows scrolled off the top so the
/// caller can place the caret in the same coordinate space as the rendered text.
fn render_composer(frame: &mut Frame, app: &App, area: Rect) -> (Rect, u16) {
    let p = &app.palette;
    let busy = app.conversation.busy;
    // Right-aligned status carries the most important signal: a persistent
    // disconnect notice, then a transient flash, then the busy/ready dot. The
    // bottom key-hint bar was removed, so this is where those signals surface.
    let (status, dot) = if app.disconnected {
        ("⚠ disconnected".to_owned(), p.error)
    } else if app.compaction.as_ref().is_some_and(|v| v.error.is_none()) {
        (
            format!("{} compacting…", widgets::spinner(app.spinner_tick)),
            p.warn,
        )
    } else if let Some(flash) = &app.flash {
        (flash.clone(), p.warn)
    } else if busy {
        ("◐ working".to_owned(), p.warn)
    } else {
        ("● ready".to_owned(), p.success)
    };
    // Border carries the activity signal (inverse of the old logic): accent
    // while executing, neutral with a pending draft, dim when idle and empty.
    let border_style = if busy {
        Style::new().fg(p.accent).add_modifier(Modifier::BOLD)
    } else if !app.input.is_blank() {
        Style::new().fg(p.fg)
    } else {
        Style::new().fg(p.border)
    };
    // Empty title keeps the input panel unlabeled (Claude-style); the right
    // status dot still carries the ready/working signal.
    let block = widgets::panel("", None, busy, p)
        .border_style(border_style)
        .title(Line::from(Span::styled(status, Style::new().fg(dot))).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Soft-wrap the draft to the inner width and scroll so the caret's
    // row stays visible once the draft grows past the panel's visible height.
    let (text, scroll) = if app.input.value().is_empty() && !busy {
        (
            Text::from(Line::styled(
                composer_placeholder(app),
                Style::new().fg(p.fg_mute),
            )),
            0,
        )
    } else {
        let lines: Vec<Line> = app
            .input
            .wrap_rows(inner.width)
            .into_iter()
            .map(|row| highlight_image_tokens(&row, p))
            .collect();
        let (_, cursor_row) = app.input.cursor_visual_col_row(inner.width);
        let scroll = cursor_row.saturating_sub(inner.height.saturating_sub(1));
        (Text::from(lines).style(Style::new().fg(p.fg)), scroll)
    };
    frame.render_widget(Paragraph::new(text).scroll((scroll, 0)), inner);
    (inner, scroll)
}

/// Splits a wrapped text row on `[Image #N]` tokens and styles them in accent.
///
/// Plain text segments keep the default foreground; each `[Image #N]` token is
/// rendered bold in the accent colour so it stands out as a non-text placeholder.
fn highlight_image_tokens<'a>(row: &str, p: &Palette) -> Line<'a> {
    const TAG: &str = "[Image #";
    let mut spans: Vec<Span<'a>> = Vec::new();
    let mut rest = row.to_owned();
    loop {
        if let Some(start) = rest.find(TAG) {
            if start > 0 {
                spans.push(Span::raw(rest[..start].to_owned()));
            }
            let after = rest[start + TAG.len()..].to_owned();
            if let Some(end) = after.find(']') {
                let token = format!("{}{}]", TAG, &after[..end]);
                spans.push(Span::styled(
                    token,
                    Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
                ));
                after[end + 1..].clone_into(&mut rest);
            } else {
                spans.push(Span::raw(rest[start..].to_owned()));
                break;
            }
        } else {
            if !rest.is_empty() {
                spans.push(Span::raw(rest));
            }
            break;
        }
    }
    Line::from(spans)
}

/// A rotating composer placeholder, varied by the number of turns so far.
fn composer_placeholder(app: &App) -> &'static str {
    /// Inviting prompts cycled as the conversation grows (codex-style).
    const PLACEHOLDERS: [&str; 4] = [
        "message…  (↵ send · ⌥↵ newline · /help)",
        "ask about this codebase…",
        "describe a change and press ↵…",
        "summarize recent commits…  (/help for commands)",
    ];
    PLACEHOLDERS[app.conversation.turns.len() % PLACEHOLDERS.len()]
}

/// Renders the queued-message preview rows between the body and composer.
///
/// A dim header plus up to [`QUEUE_PREVIEW_ROWS`] italic `↳`-prefixed previews,
/// foreshadowing input that is sent once the current turn completes.
fn render_queue(frame: &mut Frame, app: &App, area: Rect) {
    if app.message_queue.is_empty() || area.height == 0 {
        return;
    }
    let p = &app.palette;
    // Frame the header for the actual state: in-flight turn vs. a stalled queue.
    let tail = if app.conversation.busy {
        "sent after this turn"
    } else {
        "↵ to continue"
    };
    let mut lines = vec![Line::styled(
        format!("queued {} — {tail}", app.message_queue.len()),
        Style::new().fg(p.fg_mute),
    )];
    for text in app.message_queue.iter().take(QUEUE_PREVIEW_ROWS) {
        let preview = truncate_one_line(text, 60);
        lines.push(Line::from(Span::styled(
            format!("  ↳ {preview}"),
            Style::new().fg(p.fg_dim).add_modifier(Modifier::ITALIC),
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Collapses `text` to one line and truncates to `max` display cells with `…`.
fn truncate_one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if UnicodeWidthStr::width(flat.as_str()) <= max {
        return flat;
    }
    // Accumulate by display width so CJK/emoji don't overrun the queue row.
    let mut out = String::new();
    let mut width = 0usize;
    for c in flat.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + w > max {
            break;
        }
        width += w;
        out.push(c);
    }
    out.push('…');
    out
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

    // ---- skill invocation chip ----

    #[test]
    fn skill_invocation_name_detects_block() {
        let block = "<skill name=\"commit\" location=\"/x/SKILL.md\">\nbody\n</skill>";
        assert_eq!(skill_invocation_name(block), Some("commit"));
    }

    #[test]
    fn skill_invocation_name_rejects_non_blocks() {
        assert_eq!(skill_invocation_name("just a normal message"), None);
        // Starts like a block but never closes → not a skill block.
        assert_eq!(skill_invocation_name("<skill name=\"x\"> unclosed"), None);
    }

    #[test]
    fn skill_chip_collapsed_is_one_line_expanded_shows_body() {
        let p = Palette::resolve(crate::theme::Theme::Dark, crate::theme::Accent::default());
        let raw = "<skill name=\"commit\" location=\"/x/SKILL.md\">\nthe full body\n</skill>";

        let collapsed = skill_invocation_lines("commit", raw, false, &p, 80);
        assert_eq!(collapsed.len(), 1, "collapsed chip is a single line");

        let expanded = skill_invocation_lines("commit", raw, true, &p, 80);
        assert!(expanded.len() > 1, "expanded shows the header plus body");
    }

    // ---- @ mention file chip ----

    #[test]
    fn at_mention_renders_compact_chip_hiding_body() {
        let p = Palette::resolve(crate::theme::Theme::Dark, crate::theme::Accent::default());
        let text = "@plans/\n\n<file path=\"plans/\" type=\"directory\">\nphase1-core-native-research/\n</file>";
        let lines = mention_message_lines(text, &p, 80);
        let rendered: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // The typed `@path` prose stays visible.
        assert!(rendered.contains("@plans/"));
        // A compact `dir <path>` chip is shown.
        assert!(rendered.contains("dir"));
        assert!(rendered.contains("plans/"));
        // The inlined directory body is dropped from the transcript.
        assert!(!rendered.contains("phase1-core-native-research"));
    }

    #[test]
    fn at_mention_file_chip_uses_file_badge() {
        let p = Palette::resolve(crate::theme::Theme::Dark, crate::theme::Accent::default());
        let text = "@a.rs\n\n<file path=\"a.rs\" type=\"file\">\nfn main() {}\n</file>";
        let lines = mention_message_lines(text, &p, 80);
        let rendered: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("file"));
        assert!(rendered.contains("a.rs"));
        assert!(!rendered.contains("fn main"));
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

    // ---- top bar token display ----

    fn test_app_with_usage(usage: Option<(u64, u64)>) -> crate::app::App {
        use std::sync::Arc;

        use zhive_proto::domain::ThreadId;

        let mut app = crate::app::App::new(crate::TuiConfig::default(), ThreadId(Arc::from("t")));
        app.last_usage = usage;
        app
    }

    /// Renders the top bar into a `TestBackend` and returns all text on that row.
    fn render_top_bar_text(app: &crate::app::App) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(80, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 80, 1);
                render_top_bar(frame, app, area);
            })
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect::<String>()
    }

    #[test]
    fn top_bar_shows_token_count_when_usage_present() {
        let app = test_app_with_usage(Some((120, 45)));
        let text = render_top_bar_text(&app);
        assert!(
            text.contains("120") && text.contains("45"),
            "top bar must include token counts; got: {text:?}"
        );
    }

    #[test]
    fn top_bar_hides_token_count_when_no_usage() {
        let app = test_app_with_usage(None);
        let text = render_top_bar_text(&app);
        // Arrow symbols used only for token display.
        assert!(
            !text.contains("tok"),
            "top bar must not show 'tok' without usage; got: {text:?}"
        );
    }

    /// Renders the whole frame into a `width`×`height` backend and returns the
    /// buffer as one joined string (used to detect a blank composer).
    fn render_frame_text(app: &mut crate::app::App, width: u16, height: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect::<String>()
    }

    #[test]
    fn composer_keeps_text_when_line_fills_width_exactly() {
        // Regression: a draft that exactly fills the composer's inner width must
        // not scroll itself out of view. The inner width is `area.width - 4`
        // (borders + horizontal padding); fill it precisely and assert the text
        // is still rendered rather than a blank box.
        let mut app = test_app_with_usage(None);
        let width = 24u16;
        let inner = usize::from(width) - 4;
        let draft = "a".repeat(inner);
        app.input.insert_str(&draft);
        let text = render_frame_text(&mut app, width, 10);
        assert!(
            text.contains(&draft),
            "boundary-filling draft must stay visible; got: {text:?}"
        );
    }

    // ---- composer disconnect status ----
    //
    // The bottom key-hint bar was removed; the disconnect notice now surfaces in
    // the composer's right-aligned status slot, so it is asserted there instead.

    fn render_composer_text(app: &crate::app::App) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(60, 3);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = ratatui::layout::Rect::new(0, 0, 60, 3);
                render_composer(frame, app, area);
            })
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect::<String>()
    }

    #[test]
    fn composer_shows_disconnect_status_when_disconnected() {
        let mut app = test_app_with_usage(None);
        app.disconnected = true;
        let text = render_composer_text(&app);
        assert!(
            text.to_lowercase().contains("disconnected"),
            "composer status must show disconnect notice; got: {text:?}"
        );
    }

    #[test]
    fn composer_hides_disconnect_status_when_connected() {
        let app = test_app_with_usage(None);
        let text = render_composer_text(&app);
        assert!(
            !text.to_lowercase().contains("disconnected"),
            "composer status must not show disconnect notice when connected; got: {text:?}"
        );
    }
}

// Rust guideline compliant 2026-02-21
