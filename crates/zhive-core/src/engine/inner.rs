//! Engine actor body.
//!
//! Owns the live thread map and the broadcast event channel, and
//! consumes the [`Submission`] stream serially. The actor dispatches
//! `StartTurn` by immediately replying with the assigned `TurnId`, then
//! spawns a separate async task (`run_turn`) that calls the LLM provider
//! and streams items back. This keeps the actor loop responsive to
//! `CancelTurn` while a turn is in flight.
//!
//! Turn lifecycle methods (`start_turn`, `finish_turn`, `cancel_turn`)
//! live in the sibling module [`super::lifecycle`] to keep both files
//! under the 600-line soft limit.
//!
//! ## Phase invariant
//!
//! The engine carries a single global [`EnginePhase`] (D-006 / B1).
//! `StartTurn` requires `Idle → Turn`; if the engine is already busy
//! the submission is refused via [`EngineEvent::TurnRejected`] rather
//! than silently dropped, so callers observe a deterministic outcome.
//! Phase + per-thread [`ThreadStatus`] transitions for one turn happen
//! inside the same critical section so external observers can never see
//! the two views disagree.
//!
//! ## Prompt mapping (Phase 1, documented deviations)
//!
//! `run_turn` maps the thread's resident transcript to `llmsdk::Prompt` with
//! the following Phase-1 rules:
//! - `Item::UserMessage` → `Message::User { content: [UserPart::Text(…)] }`
//! - `Item::AgentMessage` → `Message::Assistant { content: [AssistantPart::Text(…)] }`
//! - All other item kinds are skipped (no tool results, no context items, …).
//!
//! If the resulting prompt is empty (no convertible history), the call is
//! still issued — the provider may generate a greeting or refuse; both
//! outcomes are valid.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock};

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use zhive_proto::hook::EnginePhase;

use crate::cancel::CancellationTree;
use crate::hooks::HookHost;
use crate::permission::{PermissionReducer, ReducerError};
use crate::persistence::Storage;
use crate::persistence::writer::StorageWriteOp;
use crate::provider::DynLanguageModel;
use crate::queues::QueueTarget;
use crate::state::ThreadStore;
use crate::tools::ToolRegistry;

use super::event::EngineEvent;
use super::phase::allows_transition;
use super::submission::{
    PermissionRequestId, ResumePermissionReply, Submission, SubmissionEnvelope, SubmissionReply,
};
// `thread_admin` dispatch methods (`delete_thread`, `rename_thread`,
// `search_threads`, `list_tools`) live in the sibling module; the `impl
// EngineInner` block there is in scope via `pub(in crate::engine)` visibility.

// ============================================================
// StorageWriterState
// ============================================================

/// Bundles the persistence writer sender and its join handle so both can be
/// held in a single `Mutex` and taken together at shutdown.
struct StorageWriterState {
    /// Sender end of the write-op channel; drop to close the channel and
    /// signal the writer task to drain and exit.
    tx: Option<mpsc::Sender<StorageWriteOp>>,
    /// Join handle for the background writer task.
    handle: Option<JoinHandle<()>>,
}

// ============================================================
// EngineInner
// ============================================================

/// Shared state owned by the engine actor.
///
/// Wrapped in `Arc` so the actor loop and each spawned turn task can
/// both hold a reference without lifetime coupling.
pub(crate) struct EngineInner {
    threads: Arc<ThreadStore>,
    events_tx: broadcast::Sender<EngineEvent>,
    /// Engine-wide phase. A short-held `std::sync::Mutex` is preferred
    /// over a `tokio::sync::RwLock` because every transition runs to
    /// completion without an `await` point, and the synchronous lock
    /// makes the compare-and-set atomic against the broadcast emission.
    phase: Mutex<EnginePhase>,
    turn_counter: AtomicU64,
    /// Monotonically increasing counter for compaction operations on this
    /// engine. Incorporated into compaction turn and item ids to prevent
    /// duplicate-key collisions when a thread is compacted more than once
    /// within the same process lifetime. Counter starts at 1; 0 is
    /// deliberately never emitted so ids like `compaction-0` cannot appear.
    compaction_counter: AtomicU64,
    /// Permission reducer shared with the reverse-RPC tracker.
    ///
    /// Exposed through [`Self::permission_reducer`] so the server module
    /// (and future hook host) can use the same `PendingPermissions`
    /// store for outbound prompts.
    permission: PermissionReducer,
    /// LLM provider used for every turn. A no-op [`crate::provider::ScriptedModel`]
    /// (empty stream) is supplied by [`super::Engine::spawn`] for backward
    /// compatibility; real providers are injected via
    /// [`super::Engine::spawn_with_provider`].
    ///
    /// Held behind a `RwLock` so a runtime model switch
    /// ([`super::Engine::set_model`]) can swap it without restarting the
    /// engine. Reads are short — each turn clones the inner `Arc` out and
    /// drops the guard immediately (see [`Self::provider`]), so the lock is
    /// never held across an `await`.
    provider: RwLock<DynLanguageModel>,

    /// Active model's context window (maximum input tokens), or `0` when unknown.
    ///
    /// Seeded post-spawn from the host's model catalogue
    /// ([`super::Engine::with_context_window`]) and updated on every hot model
    /// switch. Read during the post-turn auto-compaction check to derive a
    /// token budget proportional to the model's window (see
    /// [`super::compaction::threshold_for_context_window`]). `0` falls back to
    /// the conservative default budget. Uses `Relaxed` ordering: the value is
    /// advisory for a heuristic, never a synchronisation point.
    context_window: AtomicU64,
    /// Active model's maximum output tokens, or `0` when unknown.
    ///
    /// Seeded post-spawn from the host's model catalogue
    /// ([`super::Engine::with_max_output_tokens`]) and updated on every hot
    /// model switch. Read when building each turn's request to cap the
    /// provider's output budget; `0` leaves the provider on its own fallback
    /// (which can be as low as 4096 and truncate a deep reasoning pass). Uses
    /// `Relaxed` ordering for the same reason as [`Self::context_window`].
    max_output_tokens: AtomicU64,
    /// Hook host shared across all turn tasks spawned by this engine.
    pub(in crate::engine) hook_host: Arc<HookHost>,
    /// Tool registry shared across all turn tasks spawned by this engine.
    pub(in crate::engine) tools: Arc<ToolRegistry>,
    /// Per-turn iteration limit applied by [`super::turn::run_turn`].
    turn_limits: super::TurnLimits,
    /// Optional system prompt prepended to every provider call.
    ///
    /// Cloned cheaply (`Arc`) into each [`super::prompt::build_call_options`]
    /// call; see [`super::EngineConfig::system_prompt`].
    system_prompt: Option<Arc<str>>,
    /// Optional summarization instruction for compaction / fork.
    ///
    /// `None` falls back to the built-in [`super::compaction::SUMMARY_INSTRUCTION`];
    /// see [`super::EngineConfig::compaction_prompt`] and [`Self::compaction_instruction`].
    compaction_prompt: Option<Arc<str>>,
    /// Engine-wide cancellation hierarchy.
    ///
    /// The root token represents engine shutdown.  Each turn gets a child
    /// token (`child_for_turn`) so that `shutdown` propagates to all
    /// in-flight turns without the per-turn cancel affecting other turns
    /// or the engine itself.  Each tool call gets a further child via
    /// `child_for_tool`.
    cancel_tree: CancellationTree,
    /// Optional persistence write-through sender.
    ///
    /// When `Some`, every lifecycle event (thread upserted, turn started,
    /// item appended, turn ended) is enqueued here.  The [`PersistenceWriter`]
    /// task drains it asynchronously.
    ///
    /// Wrapped in `Mutex<Option<…>>` so `run()` can `take()` the sender on
    /// shutdown (dropping it closes the channel, signalling the writer to
    /// drain and exit).
    ///
    /// [`PersistenceWriter`]: crate::persistence::writer::PersistenceWriter
    storage_writer: Mutex<StorageWriterState>,

    /// Token threshold that triggers automatic compaction when the most
    /// recently observed `input_tokens` meets or exceeds this value.
    ///
    /// `None` means compaction is governed solely by the item-count
    /// threshold ([`super::compaction::AUTO_COMPACT_ITEM_THRESHOLD`]).
    compact_token_threshold: Option<u64>,

    /// Input tokens reported by the most recent provider call in this
    /// engine. Written by [`super::turn`] after each stream completes;
    /// read during the post-turn auto-compaction check.
    ///
    /// Uses `Relaxed` ordering: the write and read are both on the same
    /// engine-owned async task, so no cross-thread synchronisation beyond
    /// "last writer wins" is needed.
    last_input_tokens: AtomicU64,

    /// Read handle to persistent storage, retained for cross-thread fork.
    ///
    /// `None` for a purely in-memory engine. Cross-thread fork reads the
    /// source thread's JSONL rollout directly (the source of truth, including
    /// history outside the in-memory window), so it requires storage; with
    /// `None`, [`super::fork`] returns
    /// [`super::submission::ForkError::SourceNotFound`].
    ///
    /// This is the engine's only **read** path into storage; writes still go
    /// through the [`Self::storage_writer`] channel so the turn loop never
    /// blocks on disk I/O.
    storage: Option<Arc<Storage>>,

    /// Working directory recorded on every thread this engine creates.
    ///
    /// Stamped onto every thread snapshot (`start_turn`, `finish_turn`,
    /// `cancel_turn`, subagent spawn, fork) so the persisted `cwd` column is
    /// stable across the upsert `ON CONFLICT` path. Defaults to `"."` for the
    /// bare [`Self::new`] constructor; real paths arrive via
    /// [`super::EngineConfig::cwd`].
    cwd: std::path::PathBuf,
}

impl EngineInner {
    pub(crate) fn new(
        events_tx: broadcast::Sender<EngineEvent>,
        provider: DynLanguageModel,
    ) -> Self {
        Self::new_with_hooks_tools_storage(
            events_tx,
            provider,
            Arc::new(HookHost::new()),
            Arc::new(ToolRegistry::new()),
            super::TurnLimits::default(),
            None,
            None,
            None,
            None,
            None,
            None,
            std::path::PathBuf::from("."),
        )
    }

    /// Full constructor used by [`super::Engine::spawn_with_config`].
    #[expect(
        clippy::too_many_arguments,
        reason = "constructor mirrors EngineConfig fields; a builder would add more complexity"
    )]
    pub(crate) fn new_with_hooks_tools_storage(
        events_tx: broadcast::Sender<EngineEvent>,
        provider: DynLanguageModel,
        hook_host: Arc<HookHost>,
        tools: Arc<ToolRegistry>,
        turn_limits: super::TurnLimits,
        system_prompt: Option<Arc<str>>,
        compaction_prompt: Option<Arc<str>>,
        storage_tx: Option<mpsc::Sender<StorageWriteOp>>,
        storage_handle: Option<JoinHandle<()>>,
        compact_token_threshold: Option<u64>,
        storage: Option<Arc<Storage>>,
        cwd: std::path::PathBuf,
    ) -> Self {
        Self {
            threads: Arc::new(ThreadStore::new()),
            events_tx,
            phase: Mutex::new(EnginePhase::Idle),
            turn_counter: AtomicU64::new(0),
            compaction_counter: AtomicU64::new(0),
            permission: PermissionReducer::new(),
            provider: RwLock::new(provider),
            context_window: AtomicU64::new(0),
            max_output_tokens: AtomicU64::new(0),
            hook_host,
            tools,
            turn_limits,
            system_prompt,
            compaction_prompt,
            cancel_tree: CancellationTree::new(),
            storage_writer: Mutex::new(StorageWriterState {
                tx: storage_tx,
                handle: storage_handle,
            }),
            compact_token_threshold,
            last_input_tokens: AtomicU64::new(0),
            storage,
            cwd,
        }
    }

    pub(crate) fn permission_reducer(&self) -> PermissionReducer {
        self.permission.clone()
    }

    /// Returns the event broadcast sender for sibling modules within
    /// the `engine` module (e.g. [`super::turn`]).
    pub(in crate::engine) fn events_tx(&self) -> &broadcast::Sender<EngineEvent> {
        &self.events_tx
    }

    /// Returns the active LLM provider for sibling modules (e.g. [`super::turn`]).
    ///
    /// Clones the provider handle out of the `RwLock` (a cheap `Arc` bump) and
    /// drops the guard immediately, so callers never hold the lock across an
    /// `await`. A poisoned lock is recovered rather than propagated: the
    /// provider is read-mostly and a panic in an unrelated writer must not
    /// wedge every future turn.
    pub(in crate::engine) fn provider(&self) -> DynLanguageModel {
        self.provider
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Swaps in a new provider, returning the previous one.
    ///
    /// Used by [`super::Engine::set_model`] to hot-swap the model bound to the
    /// running engine. The change takes effect on the next provider call; a
    /// turn already streaming keeps the provider it started with for that call.
    pub(in crate::engine) fn swap_provider(&self, provider: DynLanguageModel) -> DynLanguageModel {
        let mut guard = self
            .provider
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::replace(&mut guard, provider)
    }

    /// Returns the active model's context window, or `None` when unknown.
    pub(in crate::engine) fn context_window(&self) -> Option<u64> {
        match self.context_window.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// Records the active model's context window (`None` clears it to unknown).
    pub(in crate::engine) fn set_context_window(&self, window: Option<u64>) {
        self.context_window
            .store(window.unwrap_or(0), Ordering::Relaxed);
    }

    /// Returns the active model's max output tokens, or `None` when unknown.
    pub(in crate::engine) fn max_output_tokens(&self) -> Option<u64> {
        match self.max_output_tokens.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// Records the active model's max output tokens (`None` clears it).
    pub(in crate::engine) fn set_max_output_tokens(&self, max: Option<u64>) {
        self.max_output_tokens
            .store(max.unwrap_or(0), Ordering::Relaxed);
    }

    /// Returns the hook host for sibling modules.
    pub(in crate::engine) fn hook_host(&self) -> &Arc<HookHost> {
        &self.hook_host
    }

    /// Returns the tool registry for sibling modules.
    pub(in crate::engine) fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    /// Returns the configured system prompt for sibling modules, if any.
    pub(in crate::engine) fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    /// Returns the summarization instruction for compaction / fork.
    ///
    /// Falls back to the built-in [`super::compaction::SUMMARY_INSTRUCTION`]
    /// when no host instruction was configured, so behaviour is unchanged
    /// unless a host injects [`super::EngineConfig::compaction_prompt`].
    pub(in crate::engine) fn compaction_instruction(&self) -> &str {
        self.compaction_prompt
            .as_deref()
            .unwrap_or(super::compaction::SUMMARY_INSTRUCTION)
    }

    /// Returns the engine's working directory for sibling modules.
    ///
    /// Stamped onto every thread snapshot so the persisted `cwd` column is
    /// stable across the upsert `ON CONFLICT` path (see [`super::lifecycle`]).
    pub(in crate::engine) fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// Returns the effective per-turn iteration cap for [`super::turn`].
    pub(in crate::engine) fn max_turn_iterations(&self) -> u32 {
        self.turn_limits.effective_cap()
    }

    /// Returns the cancellation tree for sibling modules (e.g. lifecycle).
    pub(in crate::engine) fn cancel_tree(&self) -> &CancellationTree {
        &self.cancel_tree
    }

    /// Returns the optional token-based compaction threshold.
    ///
    /// `None` means only the item-count threshold governs auto-compaction.
    /// `Some(n)` means a turn whose `input_tokens` meets or exceeds `n`
    /// also triggers compaction regardless of transcript length.
    pub(in crate::engine) fn compact_token_threshold(&self) -> Option<u64> {
        self.compact_token_threshold
    }

    /// Returns the `input_tokens` reported by the most recent provider call.
    ///
    /// Returns `0` when no provider call has completed in this engine yet.
    pub(in crate::engine) fn last_input_tokens(&self) -> u64 {
        self.last_input_tokens.load(Ordering::Relaxed)
    }

    /// Stores the `input_tokens` from the most recent provider call.
    ///
    /// Called by [`super::turn`] immediately after the tracing usage event.
    pub(in crate::engine) fn set_last_input_tokens(&self, tokens: u64) {
        self.last_input_tokens.store(tokens, Ordering::Relaxed);
    }

    /// Non-blocking enqueue of a persistence write op.
    ///
    /// Logs at `warn` when the channel is full (back-pressure exceeded) but
    /// does not block or return an error — persistence is best-effort and
    /// must never stall the turn loop.
    pub(in crate::engine) fn enqueue_storage_op(&self, op: StorageWriteOp) {
        let guard = self
            .storage_writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(tx) = &guard.tx
            && let Err(err) = tx.try_send(op)
        {
            tracing::warn!(
                name: "zhive.engine.storage.enqueue_failed",
                error = %err,
                "persistence write op dropped; writer channel full or closed"
            );
        }
    }

    /// Flushes a thread's deferred session writes at a save point.
    ///
    /// Drains the thread's [`crate::state::PendingSessionWrites`] buffer in
    /// FIFO order, forwarding each produced [`StorageWriteOp`] to
    /// [`Self::enqueue_storage_op`].  Because `enqueue_storage_op` is itself
    /// best-effort (it only logs on a full / closed channel), the wrapping
    /// closure always reports success — the buffer is fully drained and a
    /// dropped op never stalls the turn loop.
    ///
    /// Returns whether the buffer held any deferred writes before the flush,
    /// so the caller can populate
    /// [`super::event::EngineEvent::SavePoint::had_pending_mutations`].
    ///
    /// The buffer lock is a `std::sync::Mutex` taken and released without
    /// crossing an `await` point, matching the engine's lock discipline.
    pub(in crate::engine) fn flush_pending_session_writes(
        &self,
        handle: &crate::state::ThreadHandle,
    ) -> bool {
        let mut pending = handle.pending_writes_lock();
        let had_pending = !pending.is_empty();
        let _ = pending.flush(|op| {
            self.enqueue_storage_op(op);
            Ok(())
        });
        had_pending
    }

    /// Returns the turn counter for sibling modules (e.g. lifecycle).
    pub(in crate::engine) fn turn_counter(&self) -> &AtomicU64 {
        &self.turn_counter
    }

    /// Advances the compaction sequence counter and returns the new value.
    ///
    /// Starts at 1 (the first call returns 1). Monotonically increasing within
    /// a process lifetime — values are not persisted across restarts, but
    /// duplicate ids are safe because a resumed engine truncates prior
    /// compaction turns from memory (see `resume.rs::read_rollout_turns`).
    pub(in crate::engine) fn next_compaction_seq(&self) -> u64 {
        self.compaction_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Runs the actor loop, consuming submissions until `Shutdown`.
    ///
    /// Takes `Arc<Self>` so the loop can clone the `Arc` into each
    /// spawned turn task without capturing `self` by reference.
    ///
    /// On receiving [`Submission::Shutdown`], the actor fires
    /// `cancel_tree.cancel_all()` so every in-flight turn task aborts
    /// promptly instead of waiting for its provider stream to drain.
    pub(crate) async fn run(
        self: Arc<Self>,
        mut submission_rx: mpsc::Receiver<SubmissionEnvelope>,
    ) {
        while let Some(env) = submission_rx.recv().await {
            let SubmissionEnvelope { submission, reply } = env;
            if matches!(submission, Submission::Shutdown) {
                // Cancel all in-flight turns before acknowledging the
                // shutdown so the spawned turn tasks abort promptly.
                self.cancel_tree.cancel_all();

                // Drop the persistence sender (closes the channel) then await
                // the writer task (best-effort, 5 s cap) so the last
                // completed turn is durable before the reply fires.
                let (writer_tx_drop, writer_handle) = {
                    let mut guard = self
                        .storage_writer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Take both fields out of the mutex guard so they are
                    // dropped *outside* the lock (the handle.await below must
                    // not hold the Mutex).
                    (guard.tx.take(), guard.handle.take())
                };
                // Drop the sender here to signal the writer task.
                drop(writer_tx_drop);
                if let Some(h) = writer_handle {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h).await;
                }

                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::Shutdown);
                }
                break;
            }
            self.dispatch(submission, reply).await;
        }
    }

    async fn dispatch(
        self: &Arc<Self>,
        sub: Submission,
        reply: Option<tokio::sync::oneshot::Sender<SubmissionReply>>,
    ) {
        // Other submissions (injection, spawn subagent) land in
        // B7 / B8. Phase 1 records the intent without acting on it.
        // `Shutdown` is filtered out before dispatch by [`Self::run`].
        match sub {
            Submission::StartTurn {
                thread_id,
                user_input,
                scope,
                reasoning,
            } => {
                let outcome = self
                    .start_turn(thread_id, user_input, scope, reasoning)
                    .await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::StartTurn(outcome));
                }
            }
            Submission::CancelTurn { thread_id } => {
                let outcome = self.cancel_turn(thread_id).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::CancelTurn(outcome));
                }
            }
            Submission::ResumePermission {
                request_id,
                outcome,
            } => {
                let r = self.resume_permission(&request_id, outcome);
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::ResumePermission(r));
                }
            }
            Submission::EnqueueInjection {
                thread_id,
                behavior,
                items,
            } => {
                self.enqueue_injection(thread_id, behavior, items).await;
                // Fire-and-forget: no typed reply for injection submissions.
                // If a reply channel was attached, drop it so the awaiter
                // surfaces `EngineError::ReplyDropped` immediately rather
                // than timing out.
                drop(reply);
            }
            Submission::EnqueueNextTurn { thread_id, items } => {
                self.enqueue_next_turn(thread_id, items).await;
                drop(reply);
            }
            Submission::SpawnSubagent {
                parent_thread_id,
                definition,
            } => {
                let outcome = self.spawn_subagent(parent_thread_id, definition).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::SpawnSubagent(outcome));
                }
            }
            Submission::Compact { thread_id, trigger } => {
                let outcome = self.compact_dispatch(thread_id, trigger).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::Compact(outcome));
                }
            }
            Submission::Fork {
                source_thread_id,
                up_to_item,
                summarize,
            } => {
                let outcome = self
                    .fork_thread(source_thread_id, up_to_item, summarize)
                    .await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::Fork(outcome));
                }
            }
            history @ (Submission::ListThreads { .. }
            | Submission::ResumeThread { .. }
            | Submission::GetItems { .. }) => {
                self.dispatch_history(history, reply).await;
            }
            admin @ (Submission::Delete { .. }
            | Submission::Rename { .. }
            | Submission::Search { .. }
            | Submission::ListTools) => {
                self.dispatch_thread_admin(admin, reply).await;
            }
            other => {
                tracing::debug!(
                    name: "zhive.engine.submission.unhandled",
                    submission_kind = ?std::mem::discriminant(&other),
                    "submission landed but this increment does not act on it yet"
                );
                // If the caller attached a reply oneshot, drop it
                // explicitly so the awaiter surfaces
                // `EngineError::ReplyDropped` instead of timing out.
                drop(reply);
            }
        }
    }

    /// Handles the thread-admin submissions (`Delete`, `Rename`, `Search`,
    /// `ListTools`).
    ///
    /// Split out of [`Self::dispatch`] so the main dispatch match stays under
    /// the clippy line cap. Each arm calls the matching method from
    /// [`super::thread_admin`] and discharges the reply oneshot. A submission
    /// outside this group is a caller bug and the reply is dropped.
    async fn dispatch_thread_admin(
        self: &Arc<Self>,
        sub: Submission,
        reply: Option<tokio::sync::oneshot::Sender<SubmissionReply>>,
    ) {
        match sub {
            Submission::Delete { thread_id } => {
                let outcome = self.delete_thread(thread_id).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::Delete(outcome));
                }
            }
            Submission::Rename { thread_id, name } => {
                let outcome = self.rename_thread(thread_id, name);
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::Rename(outcome));
                }
            }
            Submission::Search { query, cwd } => {
                let threads = self.search_threads(&query, cwd.as_deref()).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::Search(Box::new(threads)));
                }
            }
            Submission::ListTools => {
                let specs = self.list_tools();
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::ListTools(Box::new(specs)));
                }
            }
            _ => drop(reply),
        }
    }

    /// Handles the read-mostly history submissions (`ListThreads`,
    /// `ResumeThread`, `GetItems`).
    ///
    /// Split out of [`Self::dispatch`] so the main dispatch match stays under
    /// the clippy line cap. Each arm calls the matching [`super::resume`] method
    /// and discharges the reply oneshot. A submission outside this group is a
    /// caller bug (the `dispatch` match only routes the three history variants
    /// here) and the reply is dropped so the awaiter surfaces `ReplyDropped`.
    async fn dispatch_history(
        self: &Arc<Self>,
        sub: Submission,
        reply: Option<tokio::sync::oneshot::Sender<SubmissionReply>>,
    ) {
        match sub {
            Submission::ListThreads { cwd } => {
                let threads = self.list_threads(cwd.as_deref()).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::ListThreads(Box::new(threads)));
                }
            }
            Submission::ResumeThread { thread_id } => {
                let outcome = self.resume_thread(thread_id).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::ResumeThread(outcome));
                }
            }
            Submission::GetItems {
                thread_id,
                turn_id,
                offset,
                limit,
            } => {
                let outcome = self
                    .get_items(thread_id, turn_id, offset, limit)
                    .await
                    .map(Box::new);
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::GetItems(outcome));
                }
            }
            _ => drop(reply),
        }
    }

    /// Enqueues items into the steer or follow-up queue of the target thread.
    ///
    /// `behavior` is mapped via [`QueueTarget::try_from`]; unknown
    /// `StreamingBehavior` variants are logged and dropped rather than
    /// propagating a wire error.
    ///
    /// If the thread does not yet exist, it is created (idle) so the items
    /// are queued for when a turn eventually starts.  Items submitted before
    /// any turn runs accumulate and are drained at the appropriate point once
    /// the turn loop begins.
    async fn enqueue_injection(
        &self,
        thread_id: zhive_proto::domain::ThreadId,
        behavior: zhive_proto::permission::StreamingBehavior,
        items: Vec<zhive_proto::domain::Item>,
    ) {
        let target = match QueueTarget::try_from(behavior) {
            Ok(t) => t,
            Err(unknown) => {
                tracing::warn!(
                    name: "zhive.engine.injection.unknown_behavior",
                    ?unknown,
                    "EnqueueInjection: unknown StreamingBehavior variant; items dropped"
                );
                return;
            }
        };
        let handle = self.threads.get_or_init(&thread_id).await;
        handle.injection_lock().push_back(target, items);
        tracing::debug!(
            name: "zhive.engine.injection.enqueued",
            thread_id = %thread_id.0,
            queue = ?target,
            "items pushed to injection queue"
        );
    }

    /// Enqueues items into the next-turn queue of the target thread.
    ///
    /// Next-turn items survive `cancel_turn` and are drained at the **start**
    /// of the next turn, prepended before the user input.  They may be
    /// enqueued at any time (including while a turn is in progress or when
    /// the thread is idle).
    ///
    /// If the thread does not yet exist, it is created so the items persist
    /// until the next `StartTurn` drains them.
    async fn enqueue_next_turn(
        &self,
        thread_id: zhive_proto::domain::ThreadId,
        items: Vec<zhive_proto::domain::Item>,
    ) {
        let handle = self.threads.get_or_init(&thread_id).await;
        handle
            .injection_lock()
            .push_back(QueueTarget::NextTurn, items);
        tracing::debug!(
            name: "zhive.engine.injection.next_turn.enqueued",
            thread_id = %thread_id.0,
            "items pushed to next-turn queue"
        );
    }

    /// Resolves a pending permission request driven by the client's
    /// answer. Logs a warning on protocol errors so they are visible
    /// at the engine boundary rather than silently dropped, and
    /// returns a typed [`ResumePermissionReply`] so the synchronous
    /// caller learns the outcome.
    fn resume_permission(
        &self,
        request_id: &PermissionRequestId,
        outcome: zhive_proto::permission::PermissionOutcome,
    ) -> ResumePermissionReply {
        match self
            .permission
            .resolve_by_wire_id_with_context(request_id, outcome)
        {
            Ok(context) => {
                // If the resolved request belonged to a suspended turn, emit a
                // `TurnResumed` so subscribers learn the turn is live again.
                // This runs on the engine actor task (the submission loop), not
                // the parked turn task — that separation is what keeps the
                // two-layer suspend/resume free of deadlock: the resolve is
                // delivered by the actor while the turn task waits on the
                // reducer's oneshot.
                if let Some(ctx) = context {
                    let _ = self.events_tx().send(EngineEvent::TurnResumed {
                        thread_id: ctx.thread_id,
                        turn_id: ctx.turn_id,
                    });
                }
                ResumePermissionReply::Resolved
            }
            Err(err) => {
                let rid = std::sync::Arc::<str>::clone(&request_id.0);
                let kind = error_kind(&err);
                let message = err.to_string();
                tracing::warn!(
                    name: "zhive.permission.resume.rejected",
                    request_id = %rid,
                    error_type = %kind,
                    error_message = %message,
                    "ResumePermission could not be applied"
                );
                match err {
                    ReducerError::UnknownRequest(_) => ResumePermissionReply::UnknownRequest,
                    ReducerError::InvalidRequestId(_) => ResumePermissionReply::InvalidRequestId,
                    // Reducer does not surface TimedOut on the resolve
                    // path (timeouts originate in `wait`), but the
                    // pattern is exhaustive over `#[non_exhaustive]`
                    // anyway — fall back to Abandoned which is the
                    // closest semantic match for a delivery failure.
                    ReducerError::Abandoned | ReducerError::TimedOut(_) => {
                        ResumePermissionReply::Abandoned
                    }
                }
            }
        }
    }

    /// Performs an atomic phase compare-and-set.
    ///
    /// Returns `Ok(())` when the transition was applied; otherwise
    /// returns a [`PhaseTransitionError`] that tells the caller whether
    /// the failure was caused by a precondition mismatch or by an
    /// illegal transition (so they can be logged distinctly).
    pub(in crate::engine) fn try_set_phase_atomic(
        &self,
        from: EnginePhase,
        to: EnginePhase,
    ) -> Result<(), PhaseTransitionError> {
        // Take the lock once for both the legality check and the CAS
        // so the caller sees a self-consistent observation of `actual`
        // (a separate-lock attempt could report a phase that has
        // already moved on between the two acquires).
        let mut guard = self.phase_lock();
        if !allows_transition(from, to) {
            return Err(PhaseTransitionError::Illegal {
                from,
                to,
                actual: *guard,
            });
        }
        if *guard != from {
            return Err(PhaseTransitionError::PreconditionMismatch {
                expected: from,
                actual: *guard,
            });
        }
        *guard = to;
        Ok(())
    }

    pub(in crate::engine) fn phase_lock(&self) -> MutexGuard<'_, EnginePhase> {
        // Poisoned phase lock is a programming error; the only writer
        // is this actor task and it does not panic in normal paths. If
        // it happens, recover by returning the inner value rather than
        // tearing down the actor.
        match self.phase.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(crate) fn threads(&self) -> &Arc<ThreadStore> {
        &self.threads
    }

    /// Returns the retained read handle to persistent storage, if any.
    ///
    /// `None` for an in-memory engine. Used by [`super::fork`] to read a source
    /// thread's rollout when forking; the write path uses
    /// [`Self::enqueue_storage_op`] instead.
    pub(in crate::engine) fn storage(&self) -> Option<&Arc<Storage>> {
        self.storage.as_ref()
    }
}

// ============================================================
// PhaseTransitionError
// ============================================================

/// Failure modes for [`EngineInner::try_set_phase_atomic`].
///
/// Distinguishes between a caller passing a forbidden `from→to` pair
/// (programming error) and the engine simply not being in the
/// expected `from` phase right now (a race that may be benign).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::engine) enum PhaseTransitionError {
    /// `from→to` is not in the legality table; observed phase
    /// reported for diagnostics only.
    Illegal {
        from: EnginePhase,
        to: EnginePhase,
        actual: EnginePhase,
    },
    /// `from→to` is legal but the current phase is not `from`.
    PreconditionMismatch {
        expected: EnginePhase,
        actual: EnginePhase,
    },
}

impl PhaseTransitionError {
    /// Observed phase at the time of the failed CAS.
    pub(in crate::engine) fn actual(self) -> EnginePhase {
        match self {
            Self::Illegal { actual, .. } | Self::PreconditionMismatch { actual, .. } => actual,
        }
    }
}

// ============================================================
// Helpers
// ============================================================

/// Stable short classifier for [`ReducerError`] used in trace fields.
///
/// `ReducerError` is `#[non_exhaustive]`; this match deliberately omits
/// the wildcard so a future variant forces a deliberate review here
/// rather than being grouped under a generic `"other"` label.
fn error_kind(err: &ReducerError) -> &'static str {
    match err {
        ReducerError::UnknownRequest(_) => "unknown_request",
        ReducerError::InvalidRequestId(_) => "invalid_request_id",
        ReducerError::Abandoned => "abandoned",
        ReducerError::TimedOut(_) => "timed_out",
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_provider() -> DynLanguageModel {
        use crate::provider::ScriptedModel;
        ScriptedModel::new("noop", "noop", vec![]).into_dyn()
    }

    fn new_inner() -> Arc<EngineInner> {
        let (events_tx, _) = broadcast::channel::<EngineEvent>(16);
        Arc::new(EngineInner::new(events_tx, noop_provider()))
    }

    #[test]
    fn try_set_phase_atomic_reports_illegal_vs_mismatch() {
        let inner = new_inner();
        // Illegal: Idle → Retry is not in the legality table.
        let err = inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Retry)
            .unwrap_err();
        assert!(matches!(err, PhaseTransitionError::Illegal { .. }));
        // Precondition mismatch: Turn → Idle is legal but phase is Idle.
        let err = inner
            .try_set_phase_atomic(EnginePhase::Turn, EnginePhase::Idle)
            .unwrap_err();
        assert!(matches!(
            err,
            PhaseTransitionError::PreconditionMismatch {
                expected: EnginePhase::Turn,
                actual: EnginePhase::Idle,
            }
        ));
    }
}

// Rust guideline compliant 2026-02-21
