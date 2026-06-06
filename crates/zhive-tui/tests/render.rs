//! Offline render verification using ratatui's [`TestBackend`].
//!
//! Builds an [`App`], folds a small conversation in through the same
//! `on_engine` path the live loop uses, renders one frame to an in-memory
//! buffer, and asserts the role gutters and message text are present — proving
//! the reduce → render pipeline works without a real terminal.

use std::sync::Arc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use zhive_proto::domain::{Item, ItemContent, ItemId, ThreadId, ToolCallStatus, ToolKind, TurnId};
use zhive_tui::app::App;
use zhive_tui::config::TuiConfig;
use zhive_tui::protocol::EngineNotification;
use zhive_tui::ui;

fn thread() -> ThreadId {
    ThreadId(Arc::from("thread:native/render"))
}

fn turn() -> TurnId {
    TurnId(Arc::from("turn:render/0"))
}

/// Collects every cell symbol into one string for substring assertions.
fn screen_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

#[test]
fn renders_user_and_agent_messages() {
    let mut app = App::new(TuiConfig::default(), thread());
    app.on_engine(&EngineNotification::TurnStarted {
        thread_id: thread(),
        turn_id: turn(),
    });
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::UserMessage {
            id: ItemId(Arc::from("item:u0")),
            content: vec![ItemContent::Text {
                text: "hello world".to_owned(),
                annotations: None,
            }],
        }),
    });
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::AgentMessage {
            id: ItemId(Arc::from("item:a0")),
            text: "hi there".to_owned(),
        }),
    });
    app.on_engine(&EngineNotification::TurnCompleted {
        thread_id: thread(),
        turn_id: turn(),
    });

    let mut terminal = Terminal::new(TestBackend::new(70, 20)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");

    let text = screen_text(&terminal);
    // The user message carries the `❯` role glyph; the agent message has no
    // glyph (role shown by color), so its presence is asserted via its text.
    assert!(text.contains('❯'), "user role glyph present");
    assert!(text.contains("hello world"), "user text rendered");
    assert!(text.contains("hi there"), "agent text rendered");
    assert!(text.contains("demo"), "model label present in top bar");
}

#[test]
fn renders_skill_invocation_as_collapsible_chip() {
    let mut app = App::new(TuiConfig::default(), thread());
    app.on_engine(&EngineNotification::TurnStarted {
        thread_id: thread(),
        turn_id: turn(),
    });
    // A `/skill:commit` run submits this block as the user message.
    let block = "<skill name=\"commit\" location=\"/x/SKILL.md\">\nthe full body\n</skill>";
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::UserMessage {
            id: ItemId(Arc::from("item:s0")),
            content: vec![ItemContent::Text {
                text: block.to_owned(),
                annotations: None,
            }],
        }),
    });
    app.on_engine(&EngineNotification::TurnCompleted {
        thread_id: thread(),
        turn_id: turn(),
    });

    let mut terminal = Terminal::new(TestBackend::new(70, 20)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");
    let collapsed = screen_text(&terminal);
    assert!(collapsed.contains("[skill]"), "skill chip marker present");
    assert!(collapsed.contains("commit"), "skill name present");
    assert!(collapsed.contains("ctrl+o"), "expand hint present");
    assert!(
        !collapsed.contains("<skill name="),
        "raw XML hidden when collapsed; got: {collapsed}"
    );
    assert!(
        !collapsed.contains("the full body"),
        "body hidden when collapsed"
    );

    // ctrl+o (global toggle) expands every chip → the body becomes visible.
    app.details_expanded = true;
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");
    let expanded = screen_text(&terminal);
    assert!(
        expanded.contains("the full body"),
        "body shown when expanded; got: {expanded}"
    );
}

#[test]
fn command_output_collapses_by_default_and_expands_on_ctrl_o() {
    let mut app = App::new(TuiConfig::default(), thread());
    app.on_engine(&EngineNotification::TurnStarted {
        thread_id: thread(),
        turn_id: turn(),
    });
    // 12 output lines; the collapsed preview caps at 8 (CMD_OUTPUT_LINES).
    let output = (1..=12)
        .map(|n| format!("file{n}.txt"))
        .collect::<Vec<_>>()
        .join("\n");
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::CommandExecution {
            id: ItemId(Arc::from("item:c0")),
            command: "ls".to_owned(),
            cwd: std::path::PathBuf::from("/repo"),
            status: zhive_proto::domain::CommandExecutionStatus::Completed,
            exit_code: Some(0),
            aggregated_output: Some(output),
            duration_ms: Some(5),
        }),
    });
    app.on_engine(&EngineNotification::TurnCompleted {
        thread_id: thread(),
        turn_id: turn(),
    });

    let mut terminal = Terminal::new(TestBackend::new(70, 30)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");
    let collapsed = screen_text(&terminal);
    assert!(collapsed.contains("file1.txt"), "first output line shown");
    assert!(
        collapsed.contains("click or ctrl+o"),
        "expand hint present; got: {collapsed}"
    );
    assert!(
        !collapsed.contains("file12.txt"),
        "later lines hidden when collapsed"
    );

    // ctrl+o reveals the full output.
    app.details_expanded = true;
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");
    let expanded = screen_text(&terminal);
    assert!(
        expanded.contains("file12.txt"),
        "all lines shown when expanded; got: {expanded}"
    );
}

#[test]
fn tool_call_header_is_compact_with_inline_arg() {
    let mut app = App::new(TuiConfig::default(), thread());
    app.on_engine(&EngineNotification::TurnStarted {
        thread_id: thread(),
        turn_id: turn(),
    });
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::ToolCall {
            id: ItemId(Arc::from("item:turn:render/0/0")),
            name: "bash".to_owned(),
            kind: ToolKind::default(),
            status: ToolCallStatus::Completed,
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: Some(serde_json::json!({ "command": "ls", "cwd": "/repo" })),
            raw_output: None,
            provider_tool_call_id: None,
        }),
    });
    app.on_engine(&EngineNotification::TurnCompleted {
        thread_id: thread(),
        turn_id: turn(),
    });

    let mut terminal = Terminal::new(TestBackend::new(70, 20)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");
    let text = screen_text(&terminal);
    assert!(text.contains("bash"), "tool name shown; got: {text}");
    assert!(text.contains("ls"), "primary command arg inline in header");
    assert!(text.contains("ok"), "status shown");
    assert!(
        !text.contains("args:"),
        "the verbose `args: {{json}}` line is gone; got: {text}"
    );
}

#[test]
fn tool_call_persists_in_view_after_turn_completes_with_agent_reply() {
    // Repro for the reported "tool block vanishes after the turn": a turn with
    // user → tool_call → agent_message must keep ALL three visible once done.
    let mut app = App::new(TuiConfig::default(), thread());
    app.on_engine(&EngineNotification::TurnStarted {
        thread_id: thread(),
        turn_id: turn(),
    });
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::UserMessage {
            id: ItemId(Arc::from("item:turn:render/0/0")),
            content: vec![ItemContent::Text {
                text: "run ls".to_owned(),
                annotations: None,
            }],
        }),
    });
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::ToolCall {
            id: ItemId(Arc::from("item:turn:render/0/1")),
            name: "bash".to_owned(),
            kind: ToolKind::default(),
            status: ToolCallStatus::Completed,
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: Some(serde_json::json!({ "command": "ls" })),
            raw_output: None,
            provider_tool_call_id: None,
        }),
    });
    app.on_engine(&EngineNotification::ItemDelta {
        thread_id: thread(),
        turn_id: turn(),
        delta: "the listing".to_owned(),
    });
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::AgentMessage {
            id: ItemId(Arc::from("item:turn:render/0/2")),
            text: "here is the listing".to_owned(),
        }),
    });
    app.on_engine(&EngineNotification::TurnCompleted {
        thread_id: thread(),
        turn_id: turn(),
    });

    let mut terminal = Terminal::new(TestBackend::new(70, 20)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");
    let text = screen_text(&terminal);
    assert!(
        text.contains("bash"),
        "the tool call must remain visible after the turn; got: {text}"
    );
    assert!(text.contains("here is the listing"), "agent reply shown");
}

#[test]
fn renders_subagent_summary_under_agent_tool_call() {
    let mut app = App::new(TuiConfig::default(), thread());
    app.on_engine(&EngineNotification::TurnStarted {
        thread_id: thread(),
        turn_id: turn(),
    });
    // The parent flow issues an `agent` tool call that spawns a subagent.
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::ToolCall {
            id: ItemId(Arc::from("item:turn:render/0/0")),
            name: "agent".to_owned(),
            kind: ToolKind::default(),
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: None,
            raw_output: None,
            provider_tool_call_id: None,
        }),
    });
    let child = ThreadId(Arc::from("thread:subagent/render/1"));
    app.on_engine(&EngineNotification::SubagentStarted {
        parent_thread_id: thread(),
        child_thread_id: child.clone(),
        agent_type: Some("researcher".to_owned()),
        description: Some("scan the repo".to_owned()),
    });
    // A child-thread tool call drives the `N toolcalls · running <tool>` line.
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: child.clone(),
        turn_id: TurnId(Arc::from("turn:subagent/render/1/0")),
        item: Box::new(Item::ToolCall {
            id: ItemId(Arc::from("item:turn:subagent/render/1/0/0")),
            name: "grep".to_owned(),
            kind: ToolKind::default(),
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: None,
            raw_output: None,
            provider_tool_call_id: None,
        }),
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");

    let text = screen_text(&terminal);
    assert!(
        text.contains("researcher"),
        "subagent type shown in summary; got: {text}"
    );
    assert!(
        text.contains("scan the repo"),
        "subagent description shown; got: {text}"
    );
    assert!(
        text.contains("toolcalls"),
        "subagent toolcall count row shown; got: {text}"
    );
}

#[test]
fn renders_welcome_when_empty() {
    let mut app = App::new(TuiConfig::default(), thread());
    let mut terminal = Terminal::new(TestBackend::new(70, 20)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");

    let text = screen_text(&terminal);
    assert!(text.contains("demo"), "model label shown in top bar");
    assert!(
        text.contains('█'),
        "welcome screen renders the zhive wordmark; got: {text}"
    );
}

#[test]
fn renders_queued_messages_preview() {
    let mut app = App::new(TuiConfig::default(), thread());
    // A turn is in flight and the user has queued a follow-up message.
    app.on_engine(&EngineNotification::TurnStarted {
        thread_id: thread(),
        turn_id: turn(),
    });
    app.message_queue
        .push_back("queued message text".to_owned());

    let mut terminal = Terminal::new(TestBackend::new(70, 20)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");

    let text = screen_text(&terminal);
    assert!(text.contains("queued"), "queue header shown; got: {text}");
    assert!(
        text.contains("queued message text"),
        "queued preview shown; got: {text}"
    );
}

/// [13] regression: `wrap_line` used to drop `line.style`, so plain body and
/// headings reached the buffer with the terminal-default fg. Assert the palette
/// colors actually land in the rendered cells (text-only tests can't catch it).
#[test]
fn markdown_body_carries_palette_colors() {
    let mut app = App::new(TuiConfig::default(), thread());
    app.on_engine(&EngineNotification::TurnStarted {
        thread_id: thread(),
        turn_id: turn(),
    });
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::AgentMessage {
            id: ItemId(Arc::from("item:md")),
            text: "# Bright Heading\n\nplain body words here\n\n```rust\nfn demo() {}\n```"
                .to_owned(),
        }),
    });
    app.on_engine(&EngineNotification::TurnCompleted {
        thread_id: thread(),
        turn_id: turn(),
    });

    let mut terminal = Terminal::new(TestBackend::new(70, 24)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");

    let cells = terminal.backend().buffer().content().to_vec();
    let p = &app.palette;
    let has_fg = |want| {
        cells
            .iter()
            .any(|c| c.fg == want && !c.symbol().trim().is_empty())
    };

    assert!(
        has_fg(p.fg_bright),
        "heading must render with fg_bright (line.style survived wrap)"
    );
    assert!(
        has_fg(p.fg),
        "plain body must render with palette.fg, not terminal default"
    );
}

/// Diff rows must carry the `diff_add_bg` / `diff_del_bg` backgrounds end-to-end.
#[test]
fn diff_rows_carry_diff_backgrounds() {
    let mut app = App::new(TuiConfig::default(), thread());
    app.on_engine(&EngineNotification::TurnStarted {
        thread_id: thread(),
        turn_id: turn(),
    });
    app.on_engine(&EngineNotification::ItemAppended {
        thread_id: thread(),
        turn_id: turn(),
        item: Box::new(Item::Diff {
            id: ItemId(Arc::from("item:diff")),
            path: std::path::PathBuf::from("src/x.rs"),
            old_text: Some("alpha\n".to_owned()),
            new_text: "bravo\n".to_owned(),
        }),
    });
    app.on_engine(&EngineNotification::TurnCompleted {
        thread_id: thread(),
        turn_id: turn(),
    });

    let mut terminal = Terminal::new(TestBackend::new(70, 24)).expect("terminal");
    terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");

    let cells = terminal.backend().buffer().content().to_vec();
    let p = &app.palette;
    let has_bg = |want| cells.iter().any(|c| c.bg == want);

    assert!(has_bg(p.diff_add_bg), "added line carries diff_add_bg");
    assert!(has_bg(p.diff_del_bg), "deleted line carries diff_del_bg");
}
