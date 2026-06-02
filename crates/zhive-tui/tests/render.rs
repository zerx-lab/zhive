//! Offline render verification using ratatui's [`TestBackend`].
//!
//! Builds an [`App`], folds a small conversation in through the same
//! `on_engine` path the live loop uses, renders one frame to an in-memory
//! buffer, and asserts the role gutters and message text are present — proving
//! the reduce → render pipeline works without a real terminal.

use std::sync::Arc;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use zhive_proto::domain::{Item, ItemContent, ItemId, ThreadId, TurnId};
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
    assert!(text.contains("you"), "user role gutter present");
    assert!(text.contains("zap"), "agent role gutter present");
    assert!(text.contains("hello world"), "user text rendered");
    assert!(text.contains("hi there"), "agent text rendered");
    assert!(text.contains("zap"), "model pill / brand present");
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
