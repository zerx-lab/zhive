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
    terminal.draw(|frame| ui::draw(frame, &app)).expect("draw");

    let text = screen_text(&terminal);
    // The user message carries the `❯` role glyph; the agent message has no
    // glyph (role shown by color), so its presence is asserted via its text.
    assert!(text.contains('❯'), "user role glyph present");
    assert!(text.contains("hello world"), "user text rendered");
    assert!(text.contains("hi there"), "agent text rendered");
    assert!(text.contains("zap"), "model pill / brand present");
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
    terminal.draw(|frame| ui::draw(frame, &app)).expect("draw");

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
    let app = App::new(TuiConfig::default(), thread());
    let mut terminal = Terminal::new(TestBackend::new(70, 20)).expect("terminal");
    terminal.draw(|frame| ui::draw(frame, &app)).expect("draw");

    let text = screen_text(&terminal);
    assert!(text.contains("zap"), "brand shown on welcome");
    assert!(text.contains("/help"), "welcome lists commands");
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
    terminal.draw(|frame| ui::draw(frame, &app)).expect("draw");

    let text = screen_text(&terminal);
    assert!(text.contains("queued"), "queue header shown; got: {text}");
    assert!(
        text.contains("queued message text"),
        "queued preview shown; got: {text}"
    );
}
