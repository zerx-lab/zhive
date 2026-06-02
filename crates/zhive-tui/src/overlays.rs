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
        Overlay::ModelInfo => render_model_info(frame, app, area),
        Overlay::Settings => render_settings(frame, app, area),
        Overlay::Approval { request, .. } => render_approval(frame, app, area, request),
    }
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
    let popup = area.centered(Constraint::Length(58), Constraint::Length(16));
    let inner = open_popup(frame, popup, "⌘ help", p);
    let lines = vec![
        hint_row("↵", "send message", p),
        hint_row("⌥↵ / ⌃J", "insert newline", p),
        hint_row("esc", "interrupt the running turn", p),
        hint_row("↑↓ (single-line)", "browse input history", p),
        hint_row("PgUp PgDn", "scroll transcript", p),
        hint_row("⌃← ⌃→", "word-left / word-right", p),
        hint_row("/clear", "start a fresh thread", p),
        hint_row("/compact", "summarize the conversation", p),
        hint_row("/theme, /accent", "restyle the UI", p),
        hint_row("⌃C", "quit", p),
        Line::raw(""),
        Line::styled("press any key to close", Style::new().fg(p.fg_mute)),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Renders the current-model overlay.
fn render_model_info(frame: &mut Frame, app: &App, area: Rect) {
    let p = &app.palette;
    let popup = area.centered(Constraint::Length(54), Constraint::Length(9));
    let inner = open_popup(frame, popup, "⌘ model", p);
    let lines = vec![
        hint_row("provider", app.config.provider_label.as_str(), p),
        hint_row("model", app.config.model_label.as_str(), p),
        Line::raw(""),
        Line::styled(
            "the model is bound when zap launches; change it in",
            Style::new().fg(p.fg_dim),
        ),
        Line::styled(
            "config.toml or with --provider/--model, then relaunch.",
            Style::new().fg(p.fg_dim),
        ),
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

/// Renders the slash-command palette popup anchored above the composer.
pub(crate) fn render_palette(frame: &mut Frame, app: &App, composer: Rect) {
    let p = &app.palette;
    let matches = app.palette_matches();
    if matches.is_empty() {
        return;
    }
    let rows = u16::try_from(matches.len()).unwrap_or(8).min(8);
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
    let lines: Vec<Line<'static>> = matches
        .iter()
        .enumerate()
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

// Rust guideline compliant 2026-02-21
