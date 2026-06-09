//! Modal overlays and the slash-command palette popup.
//!
//! Split out of [`crate::ui`] so that module stays focused on the conversation
//! shell. Each overlay clears a centered rect and draws a rounded panel over the
//! dimmed body; the palette floats just above the composer while a `/command`
//! is being composed. All styling comes from the active [`Palette`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph};
use zhive_proto::permission::RequestPermissionRequest;

use crate::app::{App, Overlay};
use crate::theme::Palette;
use crate::widgets;

/// Dispatches to the renderer for the active modal overlay.
pub(crate) fn render_overlay(frame: &mut Frame, app: &App, overlay: &Overlay, area: Rect) {
    match overlay {
        Overlay::Help => render_help(frame, app, area),
        Overlay::Settings => render_settings(frame, app, area),
        Overlay::Approval { request, .. } => render_approval(frame, app, area, request),
        Overlay::SessionList {
            entries,
            selected,
            query,
            filter_mode,
        } => render_session_list(frame, app, area, entries, *selected, query, *filter_mode),
        Overlay::SkillList { selected, query } => {
            render_skill_list(frame, app, area, *selected, query);
        }
        Overlay::ModelList {
            models,
            selected,
            query,
        } => render_model_list(frame, app, area, models, *selected, query),
        Overlay::CheckpointList {
            entries,
            selected,
            confirm,
        } => render_checkpoint_list(frame, app, area, entries, *selected, *confirm),
    }
}

/// A single row's display fields for [`render_select_list`].
///
/// `prefix` is a small leading marker (e.g. a `●` for the current session or a
/// blank), `primary` is the highlighted title, and `secondary` is dimmed
/// trailing context (preview, relative time). All strings are owned so callers
/// can build rows from formatted data without lifetime juggling.
pub(crate) struct SelectRow {
    /// Leading marker drawn before the title (e.g. `●` or two spaces).
    pub prefix: String,
    /// The main, highlighted text of the row.
    pub primary: String,
    /// Dimmed trailing context shown after the title.
    pub secondary: String,
}

/// Renders a generic filterable select list into `inner` (already a popup body).
///
/// Draws the query line, then each row with the selected one reverse-highlighted
/// (matching [`render_palette`]'s scheme), then a bottom hint strip. The caller
/// owns the popup chrome and supplies pre-filtered `rows` plus the highlighted
/// `selected` index; this function is pure presentation so it stays reusable for
/// future pickers (model list, etc.).
pub(crate) fn render_select_list(
    frame: &mut Frame,
    p: &Palette,
    inner: Rect,
    query: &str,
    rows: &[SelectRow],
    selected: usize,
    hint: &str,
) {
    // Chrome rows consumed by the query line, a blank, and the hint line. The
    // rest is the scrollable row viewport.
    const CHROME_ROWS: usize = 3;
    let selected = selected.min(rows.len().saturating_sub(1));
    let viewport = usize::from(inner.height).saturating_sub(CHROME_ROWS).max(1);
    // Scroll the window so the selected row stays visible even when the list is
    // taller than the popup (otherwise the selection is clipped off-screen).
    let start = if selected >= viewport {
        selected + 1 - viewport
    } else {
        0
    };
    let end = (start + viewport).min(rows.len());

    let mut lines: Vec<Line<'static>> = Vec::new();
    // Filter query line, plus a `current/total` counter so the user keeps their
    // place even when the list scrolls past the viewport.
    let counter = if rows.is_empty() {
        String::new()
    } else {
        format!("  {}/{}", selected + 1, rows.len())
    };
    lines.push(Line::from(vec![
        Span::styled("/ ", Style::new().fg(p.fg_mute)),
        Span::styled(query.to_owned(), Style::new().fg(p.fg)),
        Span::styled("▌", Style::new().fg(p.accent)),
        Span::styled(counter, Style::new().fg(p.fg_mute)),
    ]));

    if rows.is_empty() {
        lines.push(Line::styled("  no matches", Style::new().fg(p.fg_mute)));
    }
    for (i, row) in rows.iter().enumerate().skip(start).take(end - start) {
        let is_sel = i == selected;
        let primary_style = if is_sel {
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(p.fg)
        };
        // The selected row gets `▸`; the edge rows show `▲`/`▼` when more rows
        // exist off-screen above/below.
        let marker = if is_sel {
            "▸ "
        } else if i == start && start > 0 {
            "▲ "
        } else if i + 1 == end && end < rows.len() {
            "▼ "
        } else {
            "  "
        };
        let line = Line::from(vec![
            Span::styled(marker, Style::new().fg(p.accent)),
            Span::styled(row.prefix.clone(), Style::new().fg(p.success)),
            Span::styled(row.primary.clone(), primary_style),
            Span::raw("  "),
            Span::styled(row.secondary.clone(), Style::new().fg(p.fg_dim)),
        ]);
        let line = if is_sel {
            line.style(Style::new().bg(p.sel_bg))
        } else {
            line
        };
        lines.push(line);
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(hint.to_owned(), Style::new().fg(p.fg_mute)));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Clears `popup`, draws a rounded titled block, and returns the inner rect.
fn open_popup(frame: &mut Frame, popup: Rect, title: &str, p: &Palette) -> Rect {
    frame.render_widget(Clear, popup);
    let block = widgets::panel_rounded(title, None, true, p);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    inner
}

/// A two-column help row: a highlighted key and its description.
fn hint_row(key: &str, desc: &str, p: &Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<16}"), Style::new().fg(p.accent)),
        Span::styled(desc.to_owned(), Style::new().fg(p.fg)),
    ])
}

/// Renders the help overlay (keybindings and commands).
fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;
    let popup = area.centered(Constraint::Length(60), Constraint::Length(22));
    let inner = open_popup(frame, popup, "⌘ help", p);
    let lines = vec![
        hint_row("↵", "send · queue while busy · run command", p),
        hint_row("⌥↵ / ⌃J", "insert newline", p),
        hint_row("esc", "clear selection · interrupt · clear queue", p),
        hint_row("⌃X", "pull last queued message back to edit", p),
        hint_row("↑↓ (single-line)", "browse input history", p),
        hint_row("PgUp PgDn · wheel", "scroll transcript", p),
        hint_row("⌃Home ⌃End", "jump to top / tail", p),
        hint_row("⌃← ⌃→", "word-left / word-right", p),
        hint_row("drag", "select transcript text · ⌃C copies it", p),
        hint_row("shift+drag", "native select (bypasses mouse capture)", p),
        hint_row("/ then ⌃N ⌃P", "navigate the command palette", p),
        hint_row("@ then ⌃N ⌃P", "fuzzy-pick a file or folder", p),
        hint_row("/copy", "copy the last assistant message", p),
        hint_row("/clear, /compact", "fresh thread · summarize", p),
        hint_row("/theme, /accent", "restyle the UI", p),
        hint_row("⌃C", "copy selection · else clear the composer", p),
        hint_row("⌃D", "quit (on a blank composer)", p),
        Line::raw(""),
        Line::styled("press any key to close", Style::new().fg(p.fg_mute)),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Renders the settings overlay (read-only view of active preferences).
fn render_settings(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;
    let popup = area.centered(Constraint::Length(54), Constraint::Length(14));
    let inner = open_popup(frame, popup, "⚙ settings", p);
    let theme = format!("{:?}", app.theme).to_lowercase();
    let accent = format!("{:?}", app.accent).to_lowercase();
    let density = format!("{:?}", app.config.density).to_lowercase();
    let lines = vec![
        hint_row("theme", &theme, p),
        hint_row("accent", &accent, p),
        hint_row("density", &density, p),
        hint_row("provider", app.config.provider_label.as_str(), p),
        hint_row("model", app.config.model_label.as_str(), p),
        Line::raw(""),
        Line::styled("keys", Style::new().fg(p.fg_dim)),
        hint_row("↵ / ⌥↵", "send / newline", p),
        hint_row("/theme /accent", "restyle live", p),
        Line::raw(""),
        Line::styled("press any key to close", Style::new().fg(p.fg_mute)),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Computes the approval popup height from the number of options.
///
/// Base rows: title + reason + blank + footer + blank + key-hint = 6.
/// Each option adds one row.  A minimum of 8 rows keeps the panel readable;
/// there is no hard cap — tall option lists render fully without clipping.
fn approval_height(option_count: usize) -> u16 {
    /// Rows for the fixed header and footer of the approval panel.
    const APPROVAL_FIXED_ROWS: u16 = 6;
    /// Minimum total height so the panel is always readable.
    const APPROVAL_MIN_HEIGHT: u16 = 8;
    let rows = APPROVAL_FIXED_ROWS.saturating_add(u16::try_from(option_count).unwrap_or(u16::MAX));
    rows.max(APPROVAL_MIN_HEIGHT)
}

/// Renders the destructive-operation approval overlay (warn-bordered).
fn render_approval(frame: &mut Frame, app: &App, area: Rect, request: &RequestPermissionRequest) {
    let p = &app.palette;
    let height = approval_height(request.options.len());
    let popup = area.centered(Constraint::Percentage(70), Constraint::Length(height));
    frame.render_widget(Clear, popup);
    let block = widgets::panel_rounded("⚠ approval required", None, true, p)
        .border_style(Style::new().fg(p.warn));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let mut lines = vec![
        Line::styled(
            format!("{} · {}", request.resource_type, request.name),
            Style::new().fg(p.warn).add_modifier(Modifier::BOLD),
        ),
        Line::styled(request.reason.clone(), Style::new().fg(p.fg_dim)),
        Line::raw(""),
    ];
    for (i, opt) in request.options.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", i + 1), Style::new().fg(p.bg).bg(p.accent)),
            Span::raw(" "),
            Span::styled(opt.description.clone(), Style::new().fg(p.fg)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "[y] allow  [n] deny  [1-9] choose  [esc] cancel",
        Style::new().fg(p.fg_dim),
    ));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Renders the `/session` history picker as a centered filterable list.
///
/// Each row shows a `●` marker for the current conversation, the session title
/// (falling back to the preview, then a short id when both are empty), and a
/// dimmed `preview · relative-time` trailer. Filtering, selection, and the
/// cwd/all scope toggle are driven by [`crate::app::App`]'s key handler; this
/// only draws the filtered view and surfaces the active scope in the hint.
fn render_session_list(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    entries: &[crate::rpc::SessionEntry],
    selected: usize,
    query: &str,
    filter_mode: crate::app::SessionFilter,
) {
    let p = &app.palette;
    let popup = area.centered(Constraint::Percentage(70), Constraint::Percentage(70));
    let inner = open_popup(frame, popup, "⌘ resume session", p);

    let current = &app.conversation.thread_id;
    let now = now_unix();
    let rows: Vec<SelectRow> = crate::app::filter_sessions(entries, query)
        .into_iter()
        .map(|e| {
            // Fallback order for the row title: explicit name → first-message
            // preview → a short id tail (so an unnamed, preview-less legacy
            // session is still recognisable and selectable).
            let title = e
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| {
                    if e.preview.is_empty() {
                        short_id(e.id.0.as_ref())
                    } else {
                        truncate(&e.preview, 40)
                    }
                });
            let prefix = if &e.id == current { "● " } else { "  " }.to_owned();
            let secondary = format!(
                "{} · {}",
                truncate(&e.preview, 48),
                relative_time(now, e.updated_at)
            );
            SelectRow {
                prefix,
                primary: title,
                secondary,
            }
        })
        .collect();

    // The scope line names the active filter and the key that toggles it; the
    // `▸`-marked mode is the one currently in effect.
    let scope = match filter_mode {
        crate::app::SessionFilter::All => "scope: ▸all  cwd   ",
        crate::app::SessionFilter::Cwd => "scope:  all  ▸cwd  ",
    };
    let hint = format!("{scope}· tab toggle · ↑↓ select · ↵ resume · esc cancel · type to filter");

    render_select_list(frame, p, inner, query, &rows, selected, &hint);
}

/// Returns a short, recognisable tail of a thread id for unnamed sessions.
///
/// Keeps the segment after the last `/` (the per-thread suffix, e.g. a
/// timestamp-counter), capped so it stays compact in the list row.
fn short_id(id: &str) -> String {
    let tail = id.rsplit('/').next().unwrap_or(id);
    truncate(tail, 24)
}

/// Renders the `/skills` picker: a filterable list of skill name + description.
///
/// Reuses [`render_select_list`]; filters [`App::skills`] live by the typed
/// query. Selecting a row fills the composer with `/skill:<name> ` (handled in
/// the app key reducer, not here).
fn render_skill_list(frame: &mut Frame, app: &App, area: Rect, selected: usize, query: &str) {
    let p = &app.palette;
    let popup = area.centered(Constraint::Percentage(70), Constraint::Percentage(70));
    let inner = open_popup(frame, popup, "✦ run skill", p);

    let rows: Vec<SelectRow> = crate::app::filter_skills(&app.skills, query)
        .into_iter()
        .map(|s| SelectRow {
            prefix: "  ".to_owned(),
            primary: s.name.clone(),
            secondary: truncate(&s.description, 56),
        })
        .collect();

    let hint = "↑↓ select · ↵ pick · esc cancel · type to filter";
    render_select_list(frame, p, inner, query, &rows, selected, hint);
}

/// Renders the `/models` picker: a filterable list of provider models.
///
/// Reuses [`render_select_list`]; filters the fetched models live by the typed
/// query. Each row shows the active marker, the model label, and a dimmed line
/// of id / context window / reasoning depth. Selecting a row hot-swaps the
/// engine's active model (handled in the app key reducer, not here).
fn render_model_list(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    models: &[zhive_proto::rpc::ModelDescriptor],
    selected: usize,
    query: &str,
) {
    let p = &app.palette;
    let popup = area.centered(Constraint::Percentage(70), Constraint::Percentage(70));
    let inner = open_popup(frame, popup, "✦ switch model", p);

    let rows: Vec<SelectRow> = crate::app::filter_models(models, query)
        .into_iter()
        .map(|m| {
            let primary = m.display_name.clone().unwrap_or_else(|| m.id.clone());
            // Dimmed context: the id (when it differs from the label), context
            // window, and the deepest reasoning depth the model supports.
            let mut bits: Vec<String> = Vec::new();
            if primary != m.id {
                bits.push(m.id.clone());
            }
            bits.push(format_context_window(m.context_window));
            if let Some(depth) = depth_badge(m) {
                bits.push(depth);
            }
            SelectRow {
                prefix: if m.active {
                    "● ".to_owned()
                } else {
                    "  ".to_owned()
                },
                primary,
                secondary: truncate(&bits.join(" · "), 64),
            }
        })
        .collect();

    let hint = "↑↓ select · ↵ switch · esc cancel · type to filter";
    render_select_list(frame, p, inner, query, &rows, selected, hint);
}

/// Renders the rewind checkpoint picker (double-Esc).
///
/// Each row shows the user-message preview, the relative age, and the count of
/// files that would be reverted. The most recent checkpoint is tagged
/// `(current)`. When `confirm` is set the hint becomes a destructive-revert
/// confirmation prompt (the revert overwrites disk files).
fn render_checkpoint_list(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    entries: &[zhive_proto::domain::Checkpoint],
    selected: usize,
    confirm: bool,
) {
    let p = &app.palette;
    let popup = area.centered(Constraint::Percentage(70), Constraint::Percentage(70));
    let inner = open_popup(frame, popup, "↶ rewind to checkpoint", p);

    let now = now_unix();
    let last = entries.len().saturating_sub(1);
    let rows: Vec<SelectRow> = entries
        .iter()
        .enumerate()
        .map(|(i, cp)| {
            let primary = if cp.preview.is_empty() {
                format!("turn {}", short_id(cp.turn_id.0.as_ref()))
            } else {
                truncate(&cp.preview, 48)
            };
            let current = if i == last { " (current)" } else { "" };
            let files = if cp.files_changed == 0 {
                String::new()
            } else {
                format!(" · +{}F", cp.files_changed)
            };
            SelectRow {
                prefix: "  ".to_owned(),
                primary,
                secondary: format!("{}{current}{files}", relative_time(now, cp.created_at)),
            }
        })
        .collect();

    let hint = if confirm {
        let files = entries.get(selected).map_or(0, |c| c.files_changed);
        format!("⚠ revert {files} file(s) on disk · ↵ confirm rewind · esc cancel")
    } else {
        "↑↓ select · ↵ rewind · esc cancel".to_owned()
    };
    // No live filtering for this picker: the query line stays empty.
    render_select_list(frame, p, inner, "", &rows, selected, &hint);
}

/// Formats a context window (max input tokens) as a compact `ctx` badge.
///
/// Uses integer K/M suffixes to avoid float precision lints; `None` renders as
/// an em dash. Examples: `Some(1_000_000)` → `"1M ctx"`, `Some(200_000)` →
/// `"200K ctx"`.
fn format_context_window(window: Option<u64>) -> String {
    /// One million tokens — the M-suffix threshold.
    const MILLION: u64 = 1_000_000;
    /// One thousand tokens — the K-suffix threshold.
    const THOUSAND: u64 = 1_000;
    match window {
        None => "— ctx".to_owned(),
        Some(n) if n >= MILLION => format!("{}M ctx", n / MILLION),
        Some(n) if n >= THOUSAND => format!("{}K ctx", n / THOUSAND),
        Some(n) => format!("{n} ctx"),
    }
}

/// Returns a reasoning-depth badge for a model, or `None` when it has none.
///
/// Shows the deepest supported effort (the last of the `Off`-first cycle) as
/// `↯<level>`; a model with only `Off` but thinking support shows `↯think`;
/// a model with neither returns `None`.
fn depth_badge(model: &zhive_proto::rpc::ModelDescriptor) -> Option<String> {
    let deepest = model
        .supported_efforts
        .iter()
        .rev()
        .find(|e| e.is_enabled());
    match deepest {
        Some(level) => Some(format!("↯{}", level.label())),
        None if model.thinking_supported => Some("↯think".to_owned()),
        None => None,
    }
}

/// Truncates `s` to at most `max` chars, appending `…` when shortened.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Current Unix time in seconds, or `0` if the clock predates the epoch.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Formats `then` (Unix seconds) as a coarse relative time versus `now`.
///
/// Thresholds are chosen for a compact single-token label ("3h", "2d"); a
/// future or zero timestamp renders as "just now" rather than a negative value.
fn relative_time(now: i64, then: i64) -> String {
    /// Seconds in a minute, hour, and day for the relative-time buckets.
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let delta = now.saturating_sub(then);
    if delta < MINUTE {
        "just now".to_owned()
    } else if delta < HOUR {
        format!("{}m ago", delta / MINUTE)
    } else if delta < DAY {
        format!("{}h ago", delta / HOUR)
    } else {
        format!("{}d ago", delta / DAY)
    }
}

/// Renders the slash-command palette popup anchored above the composer.
pub(crate) fn render_palette(frame: &mut Frame, app: &App, composer: Rect) {
    // Show at most MAX_ROWS at once; the window scrolls to keep the selection
    // visible when more commands match than fit.
    const MAX_ROWS: usize = 8;
    let p = &app.palette;
    let matches = app.palette_matches();
    if matches.is_empty() {
        return;
    }
    let visible = matches.len().min(MAX_ROWS);
    let rows = u16::try_from(visible).unwrap_or(8);
    let height = rows + 2;
    let width = composer.width.min(58);
    let popup = Rect {
        x: composer.x,
        y: composer.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = widgets::panel("⌘ commands", None, true, p);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let selected = app.palette_index.min(matches.len().saturating_sub(1));
    let start = if selected >= visible {
        selected + 1 - visible
    } else {
        0
    };
    let lines: Vec<Line<'static>> = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, cmd)| {
            let is_sel = i == selected;
            let name_style = if is_sel {
                Style::new().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(p.fg)
            };
            let line = Line::from(vec![
                Span::styled(if is_sel { "▸ " } else { "  " }, Style::new().fg(p.accent)),
                Span::styled(format!("/{:<10}", cmd.name), name_style),
                Span::styled(cmd.help.clone(), Style::new().fg(p.fg_dim)),
            ]);
            if is_sel {
                line.style(Style::new().bg(p.sel_bg))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Renders the `@`-mention file picker floating just above the composer.
///
/// Mirrors [`render_palette`]: a rounded popup of fuzzy-ranked workspace paths
/// with the highlighted row reverse-styled. The window scrolls to keep the
/// selection visible, and long paths are clipped to the popup width. Shows a
/// `no matching files` line when the query rules everything out.
pub(crate) fn render_mention(frame: &mut Frame, app: &App, composer: Rect) {
    const MAX_ROWS: usize = 8;
    let p = &app.palette;
    let matches = app.mention_matches();
    let visible = matches.len().clamp(1, MAX_ROWS);
    let rows = u16::try_from(visible).unwrap_or(8);
    let height = rows + 2;
    let width = composer.width.min(72);
    let popup = Rect {
        x: composer.x,
        y: composer.y.saturating_sub(height),
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = widgets::panel("@ files", None, true, p);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if matches.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "  no matching files",
                Style::new().fg(p.fg_mute),
            )),
            inner,
        );
        return;
    }

    let selected = app.mention_index.min(matches.len().saturating_sub(1));
    let start = if selected >= visible {
        selected + 1 - visible
    } else {
        0
    };
    // Leave room for the two-cell marker before clipping the path.
    let path_width = usize::from(inner.width).saturating_sub(2);
    let lines: Vec<Line<'static>> = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, path)| {
            let is_sel = i == selected;
            let name_style = if is_sel {
                Style::new().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(p.fg)
            };
            let line = Line::from(vec![
                Span::styled(
                    if is_sel { "\u{25b8} " } else { "  " },
                    Style::new().fg(p.accent),
                ),
                Span::styled(truncate(path, path_width), name_style),
            ]);
            if is_sel {
                line.style(Style::new().bg(p.sel_bg))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

// Rust guideline compliant 2026-02-21
