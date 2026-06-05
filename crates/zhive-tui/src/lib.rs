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
mod clipboard;
pub mod config;
pub mod conversation;
mod diff;
pub mod error;
mod farewell;
mod heal;
pub mod id;
pub mod input;
mod logo;
pub mod markdown;
mod math;
mod overlays;
pub mod protocol;
mod render_cache;
pub mod rpc;
mod table;
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

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyEventKind, MouseButton,
    MouseEventKind,
};
use futures::StreamExt;
use zhive_client_native::{Client, ClientEvent};

use crate::app::{Action, App};

/// How often the UI ticks to animate the spinner and repaint.
const TICK: Duration = Duration::from_millis(90);

/// A message from a detached RPC task back into the render loop.
///
/// RPCs run off-thread so they never block rendering; their results return
/// here for the loop to apply against [`App`] (which the tasks cannot touch).
#[derive(Debug)]
enum LoopMsg {
    /// Show a transient one-line status in the footer.
    Flash(String),
    /// A submitted turn failed to start: requeue the message and halt draining.
    ///
    /// `Action::Submit` marks the conversation busy before the RPC resolves so
    /// the queue drainer cannot re-fire. If the call never starts a turn, the
    /// un-sent `text` is pushed back to the front of the queue and draining is
    /// halted so a transient failure cannot cascade; the error is surfaced.
    SubmitFailed {
        /// Human-readable error for the footer flash.
        error: String,
        /// The message that failed to send, to requeue at the front.
        text: String,
    },
    /// Populate and open the `/session` picker with the fetched threads.
    ///
    /// Carries the scope the listing was fetched under so the re-opened overlay
    /// renders the matching hint and Tab continues toggling from the right mode.
    ShowSessions {
        /// The fetched session rows, newest first.
        entries: Vec<crate::rpc::SessionEntry>,
        /// The scope these rows were listed under (cwd vs. all).
        filter: crate::app::SessionFilter,
    },
    /// A resumed thread's restored history, to fold into a fresh view.
    Resumed {
        /// The thread that was resumed.
        thread_id: zhive_proto::domain::ThreadId,
        /// Its history items, in conversation order.
        items: Vec<zhive_proto::domain::Item>,
        /// Historical subagent children to reattach as nested summaries.
        subagents: Vec<crate::rpc::SubagentRestore>,
    },
    /// Re-synced history for the *current* thread after a dropped-event gap.
    ///
    /// A broadcast lag can swallow `ItemAppended` events (e.g. a tool call) that
    /// are never re-broadcast, so the view must be rebuilt from the persisted
    /// source of truth. Unlike [`LoopMsg::Resumed`] this does **not** rebind the
    /// thread or clear the message queue — the session is still live.
    Resynced {
        /// The current thread's full history, in conversation order.
        items: Vec<zhive_proto::domain::Item>,
        /// Its subagent children, to reattach as nested summaries.
        subagents: Vec<crate::rpc::SubagentRestore>,
    },
}

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
/// `skills` are the Agent-Skills discovered at boot, surfaced through the
/// `/skills` picker and `/skill:<name>` slash execution.
pub async fn run(
    client: Client,
    config: TuiConfig,
    skills: Vec<crate::app::SkillCommand>,
) -> Result<()> {
    let thread = id::new_thread_id();
    let mut app = App::new(config, thread);
    // Register discovered skills: stored for `/skill:<name>` execution and the
    // `/skills` picker, and surfaced in the `/` palette as bare-name commands
    // (deduped against built-ins, which win on a clash) so `/commit` both
    // autocompletes and runs directly — opencode-style bare names plus pi-style
    // `/skill:<name>`.
    if !skills.is_empty() {
        let builtin: std::collections::HashSet<String> = crate::app::builtin_commands()
            .into_iter()
            .map(|c| c.name)
            .collect();
        let palette: Vec<crate::app::SlashCommand> = skills
            .iter()
            .filter(|s| !builtin.contains(&s.name))
            .map(|s| crate::app::SlashCommand {
                name: s.name.clone(),
                help: s.description.clone(),
                takes_args: false,
            })
            .collect();
        app.set_extra_commands(palette);
        app.set_skills(skills);
    }

    let mut term_events = EventStream::new();
    let mut engine_events = client.subscribe_events();
    let mut ticker = tokio::time::interval(TICK);
    // The tick arm is gated off while idle, so the interval can sit unpolled for
    // a long time; Skip (not the default Burst) makes it fire once on re-enable
    // instead of replaying every missed deadline in a tight burst.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Install the mouse-release panic guard before ratatui::init so ratatui's
    // own restore hook ends up outermost (it runs first on panic, restoring the
    // terminal before our guard releases the mouse — a benign no-op in cooked
    // mode).
    // Eagerly load the syntax-highlighter assets before the first frame so the
    // first streamed code block does not pay the one-time deserialization on
    // the render path.
    markdown::prewarm();
    install_mouse_panic_guard();
    let mut terminal = ratatui::init();
    // Capture the mouse so the scroll wheel scrolls the transcript viewport
    // rather than the terminal's own scrollback (and never the input history).
    // Best-effort: a terminal that rejects it just keeps default wheel handling.
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture);
    let outcome = event_loop(
        &mut terminal,
        &client,
        &mut app,
        &mut term_events,
        &mut engine_events,
        &mut ticker,
    )
    .await;
    // Release the mouse before restoring so the terminal regains native
    // click-drag text selection on exit.
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    // After the alternate screen is gone, print a compact farewell to the normal
    // terminal (opencode-style) on a deliberate quit, so the session id and how
    // to resume it stay in scrollback. Skipped on stdin close or error exits,
    // where `should_quit` was never set. An empty conversation was never
    // persisted, so it prints the wordmark only — never a thread id `/session`
    // could not find.
    if app.should_quit {
        let session = (!app.conversation.is_empty()).then(|| app.conversation.thread_id.0.as_ref());
        farewell::print(&app.palette, session);
    }
    outcome
}

/// Augments the panic hook to disable mouse capture before unwinding.
///
/// [`ratatui::init`] installs a hook that restores the terminal (raw mode and
/// the alternate screen) but is unaware of our mouse capture. Without this, a
/// panic would leave the terminal emitting mouse escape sequences and unable to
/// select text. We wrap the existing hook to release the mouse first.
fn install_mouse_panic_guard() {
    use std::sync::atomic::{AtomicBool, Ordering};
    // Install at most once per process so repeated `run()` calls cannot stack
    // panic hooks (`run` is public; a host could call it more than once).
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        prev(info);
    }));
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
    // here as a [`LoopMsg`]. The loop keeps a sender so `recv()` never ends.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<LoopMsg>(16);
    // Repaint only when state changed. Streaming `ItemDelta`s deliberately do
    // NOT mark dirty: the reveal cursor surfaces buffered text on the next tick,
    // so per-delta repaints coalesce to the 90ms tick instead of one draw per
    // token. `dirty` starts true so the first frame always paints.
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            dirty = false;
        }
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_input = term_events.next() => {
                match maybe_input {
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
                            // A left click on the welcome logo sends a ripple from
                            // the click point. The hit-rect is only honored while
                            // the welcome screen is live, so a stale rect cannot
                            // fire a phantom ripple. Off the welcome screen, a
                            // left press begins a transcript text selection.
                            MouseEventKind::Down(MouseButton::Left) => {
                                let hit = app.logo_hit.get();
                                if app.welcome_active()
                                    && let Some(rect) = hit
                                    && rect.contains(ratatui::layout::Position {
                                        x: mouse.column,
                                        y: mouse.row,
                                    })
                                {
                                    app.spawn_logo_ripple(
                                        mouse.column.saturating_sub(rect.x),
                                        mouse.row.saturating_sub(rect.y),
                                    );
                                } else if !app.welcome_active() {
                                    app.selection_start(mouse.column, mouse.row);
                                }
                            }
                            // Drag extends the selection; release ends the drag
                            // (the selection persists for a subsequent Ctrl+C).
                            MouseEventKind::Drag(MouseButton::Left) => {
                                app.selection_update(mouse.column, mouse.row);
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                app.selection_finish();
                            }
                            _ => {} // other clicks / moves
                        }
                    }
                    Some(Ok(_)) => {}            // resize / focus
                    Some(Err(err)) => {
                        app.flash = Some(format!("input error: {err}"));
                    }
                    None => return Ok(()),        // stdin closed
                }
                // Any handled terminal event warrants a repaint.
                dirty = true;
            }
            maybe_event = engine_events.next_event(), if engine_alive => {
                let (alive, redraw) = handle_engine(app, maybe_event, client, &cmd_tx);
                engine_alive = alive;
                dirty |= redraw;
            }
            Some(msg) = cmd_rx.recv() => {
                apply_loop_msg(app, msg);
                dirty = true;
            }
            // Animate while busy (the spinner), while a logo ripple plays, or
            // while streamed text is still being revealed. A fully idle TUI —
            // including the static welcome screen — parks the timer so it burns
            // zero CPU until the next input or engine event arrives.
            _ = ticker.tick(),
                if app.conversation.busy
                    || app.logo.is_active()
                    || app.conversation.is_revealing()
                    || app.compaction.is_some() =>
            {
                app.tick();
                dirty = true;
            }
        }

        // After any event, dispatch one queued message when idle following a
        // completed turn (one-per-turn drain; `perform` re-marks the app busy
        // so a 90ms tick cannot fire the same message twice).
        if let Some(text) = app.take_next_queued() {
            perform(client, app, Action::Submit(text), &cmd_tx);
            dirty = true;
        }
    }
}

/// Applies one engine stream item, returning `(engine_alive, dirty)`.
///
/// `engine_alive` is `false` once the stream has ended. `dirty` is `false` only
/// for an `ItemDelta` — whose repaint is coalesced to the next reveal tick — and
/// `true` for every other event, so finalized items and lifecycle changes paint
/// immediately.
fn handle_engine(
    app: &mut App,
    event: Option<ClientEvent>,
    client: &Client,
    cmd_tx: &tokio::sync::mpsc::Sender<LoopMsg>,
) -> (bool, bool) {
    match event {
        Some(ClientEvent::Notification(notif)) => {
            let decoded = protocol::decode(&notif.method, notif.params);
            // Delta repaints are deferred to the reveal tick (see event loop).
            let redraw = !matches!(decoded, protocol::EngineNotification::ItemDelta { .. });
            app.on_engine(&decoded);
            (true, redraw)
        }
        Some(ClientEvent::Disconnected { reason }) => {
            app.on_disconnected();
            app.conversation.last_error = Some(format!("engine disconnected: {reason}"));
            // No flash here: the persistent banner in the footer supersedes it.
            (false, true)
        }
        Some(ClientEvent::Lagged(n)) => {
            // A gap swallowed events the engine will NOT re-broadcast (e.g. a
            // tool-call `ItemAppended`), so live events cannot re-derive them.
            // Reset the busy/stream state immediately, then re-sync the view
            // from the persisted history so dropped records reappear.
            app.conversation.busy = false;
            app.conversation.clear_streaming();
            app.flash = Some(format!("dropped {n} events (slow render) · re-syncing"));
            let thread = app.conversation.thread_id.clone();
            spawn_rpc(client, cmd_tx, move |client, _tx| async move {
                let items = rpc::get_thread_items(&client, &thread).await.ok()?;
                let subagents = rpc::resume_subagent_children(&client, &thread)
                    .await
                    .unwrap_or_default();
                Some(LoopMsg::Resynced { items, subagents })
            });
            (true, true)
        }
        // Stream closed with no terminal event: stop polling this branch.
        None => (false, true),
        // Reverse requests are not used on the events-forwarding path; ignore.
        _ => (true, true),
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
    cmd_tx: &tokio::sync::mpsc::Sender<LoopMsg>,
) {
    let thread = app.conversation.thread_id.clone();
    match action {
        Action::None => {}
        Action::Quit => app.should_quit = true,
        Action::Clear => {
            app.reset_thread(id::new_thread_id());
            app.flash = Some("started a new thread".to_owned());
        }
        Action::Submit(text) => {
            // Optimistically mark busy so the queue drainer won't re-fire on the
            // next tick before `TurnStarted` arrives; reset by `SubmitFailed`.
            app.conversation.busy = true;
            // Capture the current depth (Copy) so the moved closure sends the
            // level the UI shows at submit time.
            let reasoning = app.thinking_effort;
            spawn_rpc(client, cmd_tx, move |client, _tx| async move {
                let failed = text.clone();
                rpc::start_turn(&client, &thread, &text, reasoning)
                    .await
                    .err()
                    .map(|e| LoopMsg::SubmitFailed {
                        error: format!("send failed: {e}"),
                        text: failed,
                    })
            });
        }
        Action::Cancel => spawn_rpc(client, cmd_tx, move |client, _tx| async move {
            rpc::cancel_turn(&client, &thread)
                .await
                .err()
                .map(|e| LoopMsg::Flash(format!("cancel failed: {e}")))
        }),
        Action::Compact => spawn_rpc(client, cmd_tx, move |client, _tx| async move {
            match rpc::compact(&client, &thread).await {
                // Compaction started; live progress and the final summary /
                // failure surface via `events/compaction_*`, so nothing to flash.
                Ok(outcome) if outcome.status == "started" => None,
                Ok(outcome) if outcome.status == "nothing_to_compact" => {
                    Some(LoopMsg::Flash("nothing to compact".to_owned()))
                }
                // Defensive: a synchronous compaction (legacy path) still reports.
                Ok(outcome) => Some(LoopMsg::Flash(format!(
                    "compact: {} ({} entries)",
                    outcome.status, outcome.entries_compacted
                ))),
                // Fast terminal failures (engine busy, unknown thread) arrive here.
                Err(e) => Some(LoopMsg::Flash(format!("compact failed: {e}"))),
            }
        }),
        Action::ResolvePermission {
            request_id,
            outcome,
        } => spawn_rpc(client, cmd_tx, move |client, _tx| async move {
            rpc::resume_permission(&client, &request_id, outcome)
                .await
                .err()
                .map(|e| LoopMsg::Flash(format!("permission failed: {e}")))
        }),
        Action::OpenSessionList { filter } => {
            // Resolve the cwd filter at dispatch time from the host config: `All`
            // lists everything (`None`), `Cwd` scopes to the current project.
            let cwd = match filter {
                crate::app::SessionFilter::All => None,
                crate::app::SessionFilter::Cwd => {
                    Some(app.config.cwd.to_string_lossy().into_owned())
                }
            };
            spawn_rpc(client, cmd_tx, move |client, _tx| async move {
                match rpc::list_threads(&client, cwd.as_deref()).await {
                    Ok(entries) => Some(LoopMsg::ShowSessions { entries, filter }),
                    Err(e) => Some(LoopMsg::Flash(format!("session list failed: {e}"))),
                }
            });
        }
        // Resume restores the thread in the engine, then pulls its history for
        // replay plus any historical subagent children. All round-trips run
        // off-thread; the loop folds the result in.
        Action::Copy(text) => {
            // OSC 52 (+ best-effort native fallback). Out-of-band, so writing it
            // mid-frame between draws cannot corrupt the alternate screen.
            clipboard::copy(&text);
        }
        Action::ResumeSession { thread_id } => {
            spawn_rpc(client, cmd_tx, move |client, _tx| async move {
                if let Err(e) = rpc::resume_thread(&client, &thread_id).await {
                    return Some(LoopMsg::Flash(format!("resume failed: {e}")));
                }
                let items = match rpc::get_thread_items(&client, &thread_id).await {
                    Ok(items) => items,
                    Err(e) => return Some(LoopMsg::Flash(format!("resume failed: {e}"))),
                };
                // Best-effort: a failure to enumerate subagent children must not
                // fail the resume — the main transcript is the important part.
                let subagents = rpc::resume_subagent_children(&client, &thread_id)
                    .await
                    .unwrap_or_default();
                Some(LoopMsg::Resumed {
                    thread_id,
                    items,
                    subagents,
                })
            });
        }
    }
}

/// Applies a [`LoopMsg`] from a detached RPC task to the live [`App`] state.
fn apply_loop_msg(app: &mut App, msg: LoopMsg) {
    match msg {
        LoopMsg::Flash(text) => app.flash = Some(text),
        LoopMsg::SubmitFailed { error, text } => {
            // The optimistic busy flag never got a real turn. Reset it, put the
            // un-sent message back at the front, and halt draining so a
            // transient failure cannot cascade through (and drop) the queue.
            app.conversation.busy = false;
            app.message_queue.push_front(text);
            app.queue_halted = true;
            app.flash = Some(format!(
                "{error} · {} queued (↵ retry)",
                app.message_queue.len()
            ));
        }
        LoopMsg::ShowSessions { entries, filter } => {
            let count = entries.len();
            app.overlay = Some(crate::app::Overlay::SessionList {
                entries,
                selected: 0,
                query: String::new(),
                filter_mode: filter,
            });
            app.flash = Some(format!("{count} session(s) · {}", filter.label()));
        }
        LoopMsg::Resumed {
            thread_id,
            items,
            subagents,
        } => {
            let count = items.len();
            let sub_count = subagents.len();
            app.reset_thread(thread_id);
            app.conversation.load_history(items);
            // Reattach historical subagent children as nested summaries (after
            // load_history, which clears any existing subagents).
            restore_subagent_views(app, subagents);
            app.scrollback = 0;
            app.flash = if sub_count > 0 {
                Some(format!("resumed · {count} items · {sub_count} subagent(s)"))
            } else {
                Some(format!("resumed · {count} items"))
            };
        }
        LoopMsg::Resynced { items, subagents } => {
            // Rebuild the current thread's view from persisted history after a
            // dropped-event gap. No `reset_thread`: the thread binding and the
            // pending message queue must survive a mid-session re-sync.
            app.conversation.load_history(items);
            restore_subagent_views(app, subagents);
            // History was rebuilt, so any selection's line indices are stale.
            app.clear_selection();
        }
    }
}

/// Reattaches restored subagent children as completed nested summaries.
///
/// Call after [`crate::conversation::Conversation::load_history`], which clears
/// any existing subagents. A no-op when there are none.
fn restore_subagent_views(app: &mut App, subagents: Vec<crate::rpc::SubagentRestore>) {
    if subagents.is_empty() {
        return;
    }
    let views = subagents
        .into_iter()
        .map(|s| {
            crate::conversation::SubagentView::from_history(
                s.child_thread_id,
                s.agent_type,
                s.description,
                s.items,
            )
        })
        .collect();
    app.conversation.restore_subagents(views);
}

/// Spawns `f` on a detached task, forwarding any returned message to `cmd_tx`.
fn spawn_rpc<F, Fut>(client: &Client, cmd_tx: &tokio::sync::mpsc::Sender<LoopMsg>, f: F)
where
    F: FnOnce(Client, tokio::sync::mpsc::Sender<LoopMsg>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Option<LoopMsg>> + Send,
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
