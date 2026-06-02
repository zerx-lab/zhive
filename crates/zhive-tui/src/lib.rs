//! Ratatui terminal UI for zhive.
//!
//! Per D-002 the TUI is a JSON-RPC client like any other (IDE, Web UI,
//! remote): it depends on `zhive-client-native` and `zhive-proto`, never on
//! `zhive-core`. It connects an already-handshaked [`zhive_client_native::Client`],
//! subscribes to the engine's `events/*` notification stream, folds those into
//! a [`conversation::Conversation`], and renders the result with the
//! `zap-tui-design` palette ([`theme`]).
//!
//! The host (`zhive-cli`) owns process concerns — config files, provider
//! credentials, spawning the engine — and hands this crate a connected client
//! plus a distilled [`config::TuiConfig`]. The entry point is [`run`].

#![forbid(unsafe_code)]

pub mod app;
pub mod config;
pub mod conversation;
pub mod error;
pub mod id;
pub mod input;
pub mod markdown;
mod overlays;
pub mod protocol;
pub mod rpc;
pub mod theme;
pub mod ui;
pub mod widgets;
pub mod wrap;

#[doc(inline)]
pub use config::TuiConfig;
#[doc(inline)]
pub use error::{Result, TuiError};
#[doc(inline)]
pub use theme::{Accent, Density, Theme};

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind, MouseEventKind};
use futures::StreamExt;
use zhive_client_native::{Client, ClientEvent};

use crate::app::{Action, App};

/// How often the UI ticks to animate the spinner and repaint.
const TICK: Duration = Duration::from_millis(90);

/// Reports this crate's package version.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Runs the conversation TUI against a connected engine `client`.
///
/// Takes ownership of the terminal (raw mode + alternate screen via
/// [`ratatui::init`], which also installs a panic hook that restores it),
/// generates a fresh thread id, subscribes to the engine's event stream, and
/// loops until the user quits. The terminal is always restored before return.
///
/// This does not shut the engine down — the host owns that lifecycle, so the
/// same TUI can attach to a long-lived daemon without killing it on exit.
///
/// # Errors
///
/// Returns [`TuiError::Io`] on terminal failures or [`TuiError::Client`] if the
/// initial subscription cannot be established.
///
/// `extra_commands` are host-supplied palette entries (`(name, help)` pairs,
/// e.g. slash-only skills discovered at boot) merged with the built-in slash
/// commands.
pub async fn run(
    client: Client,
    config: TuiConfig,
    extra_commands: Vec<(String, String)>,
) -> Result<()> {
    let thread = id::new_thread_id();
    let mut app = App::new(config, thread);
    // Surface host-discovered slash commands (e.g. slash-only skills) in the
    // palette alongside the built-ins.
    if !extra_commands.is_empty() {
        app.set_extra_commands(
            extra_commands
                .into_iter()
                .map(|(name, help)| crate::app::SlashCommand { name, help })
                .collect(),
        );
    }

    let mut term_events = EventStream::new();
    let mut engine_events = client.subscribe_events();
    let mut ticker = tokio::time::interval(TICK);

    let mut terminal = ratatui::init();
    let outcome = event_loop(
        &mut terminal,
        &client,
        &mut app,
        &mut term_events,
        &mut engine_events,
        &mut ticker,
    )
    .await;
    ratatui::restore();
    outcome
}

/// The core select loop, factored out so the terminal is always restored.
async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    client: &Client,
    app: &mut App,
    term_events: &mut EventStream,
    engine_events: &mut zhive_client_native::ClientEventStream,
    ticker: &mut tokio::time::Interval,
) -> Result<()> {
    // Once the engine stream ends, `next_event()` returns `None` forever, so
    // its select branch would be permanently ready and spin the loop at 100%
    // CPU. The `engine_alive` guard disables that branch after disconnect.
    let mut engine_alive = true;
    // RPC calls run on detached tasks so a slow round-trip (e.g. compaction)
    // never freezes input or rendering; their user-facing outcome flows back
    // here as a flash message. The loop keeps a sender so `recv()` never ends.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<String>(16);
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_input = term_events.next() => match maybe_input {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                    let action = app.on_key(key);
                    perform(client, app, action, &cmd_tx);
                }
                Some(Ok(Event::Paste(text))) => app.input.insert_str(&text),
                Some(Ok(Event::Mouse(mouse))) => {
                    // Scroll wheel adjusts the transcript scrollback.
                    match mouse.kind {
                        MouseEventKind::ScrollUp => {
                            app.scrollback = app.scrollback.saturating_add(3);
                        }
                        MouseEventKind::ScrollDown => {
                            app.scrollback = app.scrollback.saturating_sub(3);
                        }
                        _ => {} // clicks / moves: redraw next loop
                    }
                }
                Some(Ok(_)) => {}            // resize / focus: redraw next loop
                Some(Err(err)) => {
                    app.flash = Some(format!("input error: {err}"));
                }
                None => return Ok(()),        // stdin closed
            },
            maybe_event = engine_events.next_event(), if engine_alive => {
                engine_alive = handle_engine(app, maybe_event);
            }
            Some(msg) = cmd_rx.recv() => app.flash = Some(msg),
            _ = ticker.tick() => app.tick(),
        }
    }
}

/// Applies one engine stream item; returns `false` once the stream has ended.
fn handle_engine(app: &mut App, event: Option<ClientEvent>) -> bool {
    match event {
        Some(ClientEvent::Notification(notif)) => {
            let decoded = protocol::decode(&notif.method, notif.params);
            app.on_engine(&decoded);
            true
        }
        Some(ClientEvent::Disconnected { reason }) => {
            app.on_disconnected();
            app.conversation.last_error = Some(format!("engine disconnected: {reason}"));
            // No flash here: the persistent banner in the footer supersedes it.
            false
        }
        Some(ClientEvent::Lagged(n)) => {
            // A gap may have swallowed a terminal event; recover conservatively
            // so the UI cannot wedge in the busy state. Live events re-derive it.
            app.conversation.busy = false;
            app.conversation.streaming.clear();
            app.flash = Some(format!("dropped {n} events (slow render)"));
            true
        }
        // Stream closed with no terminal event: stop polling this branch.
        None => false,
        // Reverse requests are not used on the events-forwarding path; ignore.
        _ => true,
    }
}

/// Dispatches an [`Action`]: local mutations inline, RPCs on detached tasks.
///
/// Engine round-trips never block the render loop — each spawns a task that
/// performs the call and reports any user-facing outcome back through `cmd_tx`.
fn perform(
    client: &Client,
    app: &mut App,
    action: Action,
    cmd_tx: &tokio::sync::mpsc::Sender<String>,
) {
    let thread = app.conversation.thread_id.clone();
    match action {
        Action::None => {}
        Action::Quit => app.should_quit = true,
        Action::Clear => {
            app.reset_thread(id::new_thread_id());
            app.flash = Some("started a new thread".to_owned());
        }
        Action::Submit(text) => spawn_rpc(client, cmd_tx, move |client, _tx| async move {
            rpc::start_turn(&client, &thread, &text)
                .await
                .err()
                .map(|e| format!("send failed: {e}"))
        }),
        Action::Cancel => spawn_rpc(client, cmd_tx, move |client, _tx| async move {
            rpc::cancel_turn(&client, &thread)
                .await
                .err()
                .map(|e| format!("cancel failed: {e}"))
        }),
        Action::Compact => spawn_rpc(client, cmd_tx, move |client, _tx| async move {
            match rpc::compact(&client, &thread).await {
                Ok(outcome) => Some(format!(
                    "compact: {} ({} entries)",
                    outcome.status, outcome.entries_compacted
                )),
                Err(e) => Some(format!("compact failed: {e}")),
            }
        }),
        Action::ResolvePermission {
            request_id,
            outcome,
        } => spawn_rpc(client, cmd_tx, move |client, _tx| async move {
            rpc::resume_permission(&client, &request_id, outcome)
                .await
                .err()
                .map(|e| format!("permission failed: {e}"))
        }),
    }
}

/// Spawns `f` on a detached task, forwarding any returned flash to `cmd_tx`.
fn spawn_rpc<F, Fut>(client: &Client, cmd_tx: &tokio::sync::mpsc::Sender<String>, f: F)
where
    F: FnOnce(Client, tokio::sync::mpsc::Sender<String>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Option<String>> + Send,
{
    let client = client.clone();
    let tx = cmd_tx.clone();
    tokio::spawn(async move {
        if let Some(message) = f(client, tx.clone()).await {
            let _ = tx.send(message).await;
        }
    });
}

// Rust guideline compliant 2026-02-21
