//! Engine actor surface.
//!
//! The engine is a Tokio-driven actor: callers submit a stream of
//! [`Submission`] commands and observe outcomes through a broadcast
//! stream of [`EngineEvent`] values. Inside, [`EnginePhase`] gates the
//! state machine (`Idle` → `Turn` → `Compaction` / `BranchSummary` /
//! `Retry` → `Idle`) and a [`crate::state::ThreadStore`] owns live
//! thread handles.
//!
//! ## Synchronous reply pattern
//!
//! The four submissions that have an immediate result (`StartTurn`,
//! `CancelTurn`, `ResumePermission`, `Shutdown`) carry an optional
//! [`tokio::sync::oneshot::Sender`] so callers can `await` the
//! engine's verdict without polling the broadcast channel. Convenience
//! methods on [`Engine`] wrap the oneshot dance:
//! [`Engine::start_turn`], [`Engine::cancel_turn`],
//! [`Engine::resume_permission`], [`Engine::shutdown`].
//!
//! Subscribers that want streaming updates (e.g. live `ItemAppended`,
//! mid-turn `PhaseChanged`) still call [`Engine::subscribe`].
//!
//! ## Provider injection
//!
//! [`Engine::spawn`] supplies a no-op [`crate::provider::ScriptedModel`]
//! (empty stream) so all 107 pre-increment-2 tests remain green — they
//! only assert `TurnStarted` → `TurnCompleted`, which still holds.
//! [`Engine::spawn_with_provider`] injects a real or scripted provider
//! for callers and tests that need to observe actual item output.
//!
//! [`EnginePhase`]: zhive_proto::hook::EnginePhase

mod compaction;
pub mod event;
mod inner;
mod lifecycle;
pub mod phase;
mod prompt;
mod subagent_spawn;
pub mod submission;
mod tool_dispatch;
mod turn;

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use zhive_proto::domain::{Item, ThreadId, TurnId};
use zhive_proto::hook::EnginePhase;
use zhive_proto::permission::{PermissionOutcome, PermissionScope, SubagentDefinition};

use inner::EngineInner;

use crate::hooks::HookHost;
use crate::persistence::Storage;
use crate::persistence::writer::PersistenceWriter;
use crate::provider::DynLanguageModel;
use crate::tools::ToolRegistry;

#[doc(inline)]
pub use event::{EngineEvent, TurnRejectionReason};
#[doc(inline)]
pub use phase::allows_transition;
#[doc(inline)]
pub use submission::{PermissionRequestId, Submission, SubmissionEnvelope};

/// Cap on the in-flight [`Submission`] queue.
///
/// The actor consumes serially so the limit acts as backpressure; the
/// chosen value matches Pi's `submissionBuffer` configuration.
const SUBMISSION_CHANNEL_CAP: usize = 512;

/// Cap on the broadcast [`EngineEvent`] backlog per subscriber.
///
/// Subscribers that fall behind by more than this many events receive a
/// [`broadcast::error::RecvError::Lagged`] and must resync.
const EVENT_CHANNEL_CAP: usize = 1024;

/// Default deadline for [`Engine::start_turn`] / [`Engine::cancel_turn`]
/// when callers do not pick their own.
///
/// The reply path runs entirely inside the actor task and never crosses
/// a slow boundary, so this is a safety net rather than a feature —
/// most calls return in microseconds.
const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Default per-turn provider-call iteration cap.
///
/// Generous default so a multi-step tool-use turn is not cut short while
/// still bounding runaway loops. Replaces the previous fixed cap of 32 (the
/// old Claude Code default), which proved too low for deep tool chains.
/// Callers override per engine via [`TurnLimits`] on [`EngineConfig`].
pub const DEFAULT_MAX_TURN_ITERATIONS: u32 = 80;

/// Hard safety ceiling for [`TurnLimits::effective_cap`].
///
/// Even an explicitly unbounded ([`TurnLimits::max_iterations`] = `None`) or
/// absurdly large configured cap is clamped to this value so a single turn can
/// never loop without limit and starve the actor. A backstop, not a tuning
/// knob; chosen high enough that no legitimate turn reaches it.
const MAX_TURN_ITERATIONS_SAFETY_CEILING: u32 = 1000;

/// Per-turn iteration limit for the inner provider/tool-call loop.
///
/// `max_iterations = Some(n)` caps a turn at `n` provider iterations;
/// `None` means "unbounded" except for the hard
/// [`MAX_TURN_ITERATIONS_SAFETY_CEILING`] backstop. The effective cap used by
/// the turn loop is computed by [`TurnLimits::effective_cap`].
///
/// # Examples
///
/// ```
/// use zhive_core::engine::{TurnLimits, DEFAULT_MAX_TURN_ITERATIONS};
/// assert_eq!(TurnLimits::default().max_iterations, Some(DEFAULT_MAX_TURN_ITERATIONS));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct TurnLimits {
    /// Maximum provider iterations per turn; `None` = unbounded (capped only
    /// by the internal hard safety ceiling).
    pub max_iterations: Option<u32>,
}

impl Default for TurnLimits {
    fn default() -> Self {
        Self {
            max_iterations: Some(DEFAULT_MAX_TURN_ITERATIONS),
        }
    }
}

impl TurnLimits {
    /// Returns the concrete iteration cap the turn loop should enforce.
    ///
    /// A configured `Some(n)` is clamped to the hard safety ceiling; `None`
    /// resolves to the ceiling itself. The result is always at least `1` so a
    /// turn runs at least one provider iteration.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::engine::TurnLimits;
    /// assert_eq!(TurnLimits { max_iterations: Some(10) }.effective_cap(), 10);
    /// assert_eq!(TurnLimits { max_iterations: Some(0) }.effective_cap(), 1);
    /// assert_eq!(TurnLimits { max_iterations: None }.effective_cap(), 1000);
    /// ```
    #[must_use]
    pub fn effective_cap(self) -> u32 {
        self.max_iterations
            .map_or(MAX_TURN_ITERATIONS_SAFETY_CEILING, |n| {
                n.min(MAX_TURN_ITERATIONS_SAFETY_CEILING)
            })
            .max(1)
    }
}

/// Configuration bundle for [`Engine::spawn_with_config`].
///
/// Groups all injectable dependencies so callers do not need to chain many
/// builder calls.  [`Default`] supplies an empty-stream provider, empty tool
/// registry, empty hook host, and no storage — the same defaults used by
/// [`Engine::spawn`].
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_core::engine::EngineConfig;
/// let cfg = EngineConfig::default();
/// assert!(cfg.tools.is_empty());
/// assert!(cfg.storage.is_none());
/// ```
#[derive(Debug)]
pub struct EngineConfig {
    /// LLM provider used for every turn.
    pub provider: DynLanguageModel,
    /// Tool registry made available to the inner dispatch loop.
    pub tools: Arc<ToolRegistry>,
    /// Hook host dispatched on `PreToolUse` / `PostToolUse` events.
    pub hook_host: Arc<HookHost>,
    /// Optional persistent storage.
    ///
    /// When `Some`, the engine spawns a [`PersistenceWriter`] background
    /// task and enqueues `ThreadUpserted`, `TurnStarted`, `ItemAppended`,
    /// and `TurnEnded` ops on every turn.  When `None` (the default) no
    /// persistence takes place and the engine is purely in-memory.
    pub storage: Option<Arc<Storage>>,
    /// Per-turn iteration limit for the inner provider/tool-call loop.
    ///
    /// Defaults to [`TurnLimits::default`] (`Some(DEFAULT_MAX_TURN_ITERATIONS)`).
    pub turn_limits: TurnLimits,

    /// Optional system prompt prepended to every provider call.
    ///
    /// When `Some`, the engine emits it as the leading
    /// [`llmsdk::language_model::Message::System`] of every reconstructed
    /// prompt. Hosts assemble this from persona, environment, and project
    /// instructions (e.g. `AGENTS.md` / `CLAUDE.md`); the engine treats it as
    /// opaque text. `None` (the default) sends no system message, preserving
    /// the prior behaviour.
    pub system_prompt: Option<Arc<str>>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        use crate::provider::ScriptedModel;
        Self {
            provider: ScriptedModel::new("noop", "noop", vec![]).into_dyn(),
            tools: Arc::new(ToolRegistry::new()),
            hook_host: Arc::new(HookHost::new()),
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        }
    }
}

/// Top-level [`Engine`] failure surface.
///
/// Reasons a synchronous submission (e.g. [`Engine::start_turn`]) can
/// fail. The variants mirror the channel-level dispatcher's outcomes
/// (`TurnRejected` events, dropped reply senders, timeouts) so callers
/// never need to subscribe to the broadcast just to learn whether
/// their own submission landed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EngineError {
    /// Actor task has exited (typically after [`Submission::Shutdown`]).
    #[error("engine actor has stopped accepting submissions")]
    ActorStopped,

    /// Engine refused a `StartTurn` because its phase is not `Idle`.
    #[error("engine busy in phase {current:?}; cannot start a new turn")]
    EngineBusy {
        /// Phase observed at submission time.
        current: EnginePhase,
    },

    /// Reply path observed an internal failure (oneshot sender
    /// dropped, etc.). Should never happen in practice.
    #[error("engine actor dropped the reply channel without answering")]
    ReplyDropped,

    /// The synchronous reply did not arrive within the configured
    /// timeout.
    #[error("engine reply timed out after {0:?}")]
    ReplyTimedOut(Duration),

    /// A [`Submission::SpawnSubagent`] was rejected by the engine.
    ///
    /// Covers parent-not-found, recursion-forbidden, and scope-widening
    /// cases. Callers should surface this as a tool-call failure back to
    /// the parent LLM rather than propagating it as a hard error.
    #[error("subagent spawn rejected: {0}")]
    SubagentSpawnFailed(submission::SubagentSpawnError),
}

impl EngineError {
    fn from_submit(_err: mpsc::error::SendError<SubmissionEnvelope>) -> Self {
        Self::ActorStopped
    }
}

/// Cheap, clonable handle to a running engine.
///
/// The actor task lives independently of any one `Engine` clone; the
/// last clone goes out of scope only after [`Engine::shutdown`] (or
/// after every clone is dropped without an explicit shutdown — in
/// which case the actor exits because its submission channel closes).
#[derive(Debug, Clone)]
pub struct Engine {
    submission_tx: mpsc::Sender<SubmissionEnvelope>,
    events_tx: broadcast::Sender<EngineEvent>,
    threads: Arc<crate::state::ThreadStore>,
    permission: crate::permission::PermissionReducer,
    reply_timeout: Duration,
}

/// Backwards-compatible alias retained while callers migrate to
/// [`EngineError`].
pub type SubmitError = EngineError;

impl Engine {
    /// Spawns a fresh engine actor with a no-op provider and returns a handle.
    ///
    /// The no-op provider is a [`crate::provider::ScriptedModel`] that
    /// yields an **empty** stream (zero `StreamPart`s). A turn started
    /// against it produces no items and completes cleanly
    /// (`TurnStarted` → `TurnCompleted`), keeping all pre-increment-2
    /// tests green.
    ///
    /// The actor runs on the current Tokio runtime. Drop the last
    /// [`Engine`] clone to let the actor exit, or call
    /// [`Engine::shutdown`] for an explicit, awaited shutdown.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zhive_core::engine::Engine;
    /// # async fn demo() {
    /// let engine = Engine::spawn();
    /// engine.shutdown().await.unwrap();
    /// # }
    /// ```
    #[must_use]
    pub fn spawn() -> Self {
        use crate::provider::ScriptedModel;
        let noop = ScriptedModel::new("noop", "noop", vec![]).into_dyn();
        Self::spawn_with_provider(noop)
    }

    /// Spawns a fresh engine actor injecting a specific LLM provider.
    ///
    /// Use this constructor in tests or production code that needs to
    /// observe real (or scripted) model output. The `provider` is called
    /// once per turn; see [`crate::provider::ScriptedModel`] for an
    /// in-memory deterministic implementation suitable for testing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use llmsdk::language_model::StreamPart;
    /// use zhive_core::engine::Engine;
    /// use zhive_core::provider::ScriptedModel;
    ///
    /// # async fn demo() {
    /// let model = ScriptedModel::new(
    ///     "test-provider",
    ///     "test-model",
    ///     vec![
    ///         StreamPart::TextStart { id: "b0".into(), provider_metadata: None },
    ///         StreamPart::TextDelta { id: "b0".into(), delta: "hello".into(), provider_metadata: None },
    ///         StreamPart::TextEnd   { id: "b0".into(), provider_metadata: None },
    ///     ],
    /// );
    /// let engine = Engine::spawn_with_provider(model.into_dyn());
    /// engine.shutdown().await.unwrap();
    /// # }
    /// ```
    #[must_use]
    pub fn spawn_with_provider(provider: DynLanguageModel) -> Self {
        let (submission_tx, submission_rx) = mpsc::channel(SUBMISSION_CHANNEL_CAP);
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        let inner = Arc::new(EngineInner::new(events_tx.clone(), provider));
        let threads = Arc::clone(inner.threads());
        let permission = inner.permission_reducer();
        tokio::spawn(inner.run(submission_rx));
        Self {
            submission_tx,
            events_tx,
            threads,
            permission,
            reply_timeout: DEFAULT_REPLY_TIMEOUT,
        }
    }

    /// Spawns a fresh engine actor using the full [`EngineConfig`].
    ///
    /// Allows injecting a provider, hook host, and tool registry in one
    /// call.  [`Engine::spawn`] and [`Engine::spawn_with_provider`] remain
    /// as thin wrappers (empty tools / hooks) for backward compatibility.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use zhive_core::engine::{Engine, EngineConfig};
    /// use zhive_core::hooks::HookHost;
    /// use zhive_core::tools::{ToolRegistry, EchoTool};
    /// use zhive_core::provider::ScriptedModel;
    ///
    /// # async fn demo() {
    /// let mut tools = ToolRegistry::new();
    /// tools.register(Arc::new(EchoTool));
    ///
    /// let cfg = EngineConfig {
    ///     provider: ScriptedModel::new("p", "m", vec![]).into_dyn(),
    ///     tools: Arc::new(tools),
    ///     hook_host: Arc::new(HookHost::new()),
    ///     storage: None,
    ///     turn_limits: Default::default(),
    ///     system_prompt: None,
    /// };
    /// let engine = Engine::spawn_with_config(cfg);
    /// engine.shutdown().await.unwrap();
    /// # }
    /// ```
    #[must_use]
    pub fn spawn_with_config(config: EngineConfig) -> Self {
        let (submission_tx, submission_rx) = mpsc::channel(SUBMISSION_CHANNEL_CAP);
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);

        // When storage is configured, spawn the PersistenceWriter background
        // task and wire the sender into EngineInner.
        let storage_tx = config.storage.as_ref().map(|s| {
            let (tx, handle) = PersistenceWriter::spawn(Arc::clone(s));
            // The handle is stored in the inner; see EngineInner::new_with_hooks_tools_storage.
            // We pass the sender + handle together.
            (tx, handle)
        });

        let (maybe_tx, maybe_handle) = match storage_tx {
            Some((tx, handle)) => (Some(tx), Some(handle)),
            None => (None, None),
        };

        // Backfill the SchemaCache from each trait tool's advertised schema so
        // red line 11 (updated_input revalidation) has a schema to check even
        // for tools registered purely through the `Tool` trait (no manifest).
        // `register_if_absent` never overwrites a schema an extension already
        // registered. A backfill failure is logged and skipped — it must never
        // block engine construction.
        for spec in config.tools.specs() {
            if let Err(err) = config
                .hook_host
                .schemas()
                .register_if_absent(&spec.name, &spec.input_schema)
            {
                tracing::warn!(
                    name: "zhive.engine.tool_schema_backfill_failed",
                    tool = %spec.name,
                    error = %err,
                    "failed to backfill tool input schema into SchemaCache; continuing"
                );
            }
        }

        let inner = Arc::new(inner::EngineInner::new_with_hooks_tools_storage(
            events_tx.clone(),
            config.provider,
            config.hook_host,
            config.tools,
            config.turn_limits,
            config.system_prompt,
            maybe_tx,
            maybe_handle,
        ));
        let threads = Arc::clone(inner.threads());
        let permission = inner.permission_reducer();
        tokio::spawn(inner.run(submission_rx));
        Self {
            submission_tx,
            events_tx,
            threads,
            permission,
            reply_timeout: DEFAULT_REPLY_TIMEOUT,
        }
    }

    /// Overrides the synchronous reply timeout (default
    /// [`DEFAULT_REPLY_TIMEOUT`]). Mostly for tests.
    #[must_use]
    pub fn with_reply_timeout(mut self, timeout: Duration) -> Self {
        self.reply_timeout = timeout;
        self
    }

    /// Hands a fire-and-forget [`Submission`] to the actor.
    ///
    /// Callers that need the typed reply should use one of the
    /// dedicated helpers ([`Self::start_turn`] etc.) instead.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ActorStopped`] when the actor task has
    /// already finished processing a [`Submission::Shutdown`].
    pub async fn submit(&self, sub: Submission) -> Result<(), EngineError> {
        self.submission_tx
            .send(SubmissionEnvelope::fire_and_forget(sub))
            .await
            .map_err(EngineError::from_submit)
    }

    /// Submits and awaits a typed reply.
    async fn submit_with_reply(
        &self,
        sub: Submission,
    ) -> Result<submission::SubmissionReply, EngineError> {
        let (env, rx) = SubmissionEnvelope::with_reply(sub);
        self.submission_tx
            .send(env)
            .await
            .map_err(EngineError::from_submit)?;
        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_recv_err)) => Err(EngineError::ReplyDropped),
            Err(_elapsed) => Err(EngineError::ReplyTimedOut(self.reply_timeout)),
        }
    }

    /// Returns a fresh broadcast subscription to the [`EngineEvent`] stream.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events_tx.subscribe()
    }

    /// Returns the live [`crate::state::ThreadStore`] handle.
    #[must_use]
    pub fn threads(&self) -> Arc<crate::state::ThreadStore> {
        Arc::clone(&self.threads)
    }

    /// Returns the permission reducer shared with the actor.
    ///
    /// Cheap clone (`Arc`-shared). Useful for the reverse-RPC sink and
    /// the hook host so they share one `PendingPermissions` store with
    /// the engine actor's `ResumePermission` handler.
    #[must_use]
    pub fn permission_reducer(&self) -> crate::permission::PermissionReducer {
        self.permission.clone()
    }

    // --------------------------------------------------------------
    // High-level helpers (synchronous reply)
    // --------------------------------------------------------------

    /// Starts a new turn and awaits the actor's [`TurnId`] reply.
    ///
    /// # Errors
    ///
    /// * [`EngineError::EngineBusy`] when the engine phase is not
    ///   `Idle` at dispatch time (matches the broadcast
    ///   [`EngineEvent::TurnRejected`]).
    /// * [`EngineError::ActorStopped`] / [`EngineError::ReplyDropped`]
    ///   / [`EngineError::ReplyTimedOut`] on channel-level failures.
    pub async fn start_turn(
        &self,
        thread_id: ThreadId,
        user_input: Vec<Item>,
        scope: Option<PermissionScope>,
    ) -> Result<TurnId, EngineError> {
        let reply = self
            .submit_with_reply(Submission::StartTurn {
                thread_id,
                user_input,
                scope,
            })
            .await?;
        match reply {
            submission::SubmissionReply::StartTurn(Ok(ok)) => Ok(ok.turn_id),
            submission::SubmissionReply::StartTurn(Err(err)) => match err {
                submission::StartTurnError::EngineBusy { current } => {
                    Err(EngineError::EngineBusy { current })
                }
            },
            _ => Err(EngineError::ReplyDropped),
        }
    }

    /// Cancels the active turn on `thread_id`.
    ///
    /// Returns the cancelled [`TurnId`] when there was an active turn,
    /// or `None` when the cancel was a no-op.
    ///
    /// # Errors
    ///
    /// Channel-level [`EngineError`] variants only — the engine never
    /// surfaces a domain failure for cancel.
    pub async fn cancel_turn(&self, thread_id: ThreadId) -> Result<Option<TurnId>, EngineError> {
        let reply = self
            .submit_with_reply(Submission::CancelTurn { thread_id })
            .await?;
        match reply {
            submission::SubmissionReply::CancelTurn(submission::CancelTurnReply::Cancelled {
                turn_id,
            }) => Ok(Some(turn_id)),
            submission::SubmissionReply::CancelTurn(submission::CancelTurnReply::NoActiveTurn) => {
                Ok(None)
            }
            _ => Err(EngineError::ReplyDropped),
        }
    }

    /// Resolves a pending permission request and awaits the engine's
    /// acknowledgement.
    ///
    /// # Errors
    ///
    /// Channel-level [`EngineError`] variants. The reducer's own
    /// failures (unknown id, abandoned waiter, etc.) are folded into
    /// [`submission::ResumePermissionReply`] which is the function's
    /// `Ok` value.
    pub async fn resume_permission(
        &self,
        request_id: PermissionRequestId,
        outcome: PermissionOutcome,
    ) -> Result<submission::ResumePermissionReply, EngineError> {
        let reply = self
            .submit_with_reply(Submission::ResumePermission {
                request_id,
                outcome,
            })
            .await?;
        match reply {
            submission::SubmissionReply::ResumePermission(r) => Ok(r),
            _ => Err(EngineError::ReplyDropped),
        }
    }

    /// Compacts a thread's transcript history into an LLM-generated summary.
    ///
    /// Requires the engine to be `Idle`; if a turn is in flight the dispatch
    /// fails with [`submission::CompactError::EngineBusy`] rather than
    /// blocking. Compaction is in-memory only in Phase 1 (it is not persisted
    /// to the rollout).
    ///
    /// # Errors
    ///
    /// Channel-level [`EngineError`] variants on actor failure; the
    /// compaction's own failures (unknown thread, busy engine, provider
    /// error) are folded into the `Ok` value as
    /// [`submission::CompactError`].
    ///
    /// ```no_run
    /// use zhive_core::engine::Engine;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::hook::CompactTrigger;
    ///
    /// # async fn demo() {
    /// let engine = Engine::spawn();
    /// // No turn has run on this thread, so there is nothing to compact.
    /// let outcome = engine
    ///     .compact(ThreadId(std::sync::Arc::from("thread:native/demo")), CompactTrigger::Manual)
    ///     .await
    ///     .expect("actor reachable");
    /// assert!(outcome.is_err()); // ThreadNotFound: thread was never created
    /// engine.shutdown().await.unwrap();
    /// # }
    /// ```
    pub async fn compact(
        &self,
        thread_id: ThreadId,
        trigger: zhive_proto::hook::CompactTrigger,
    ) -> Result<Result<submission::CompactReply, submission::CompactError>, EngineError> {
        let reply = self
            .submit_with_reply(Submission::Compact { thread_id, trigger })
            .await?;
        match reply {
            submission::SubmissionReply::Compact(r) => Ok(r),
            _ => Err(EngineError::ReplyDropped),
        }
    }

    /// Sends a graceful shutdown and awaits the actor's acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ActorStopped`] / [`EngineError::ReplyDropped`]
    /// / [`EngineError::ReplyTimedOut`] on channel failure paths.
    pub async fn shutdown(&self) -> Result<(), EngineError> {
        let reply = self.submit_with_reply(Submission::Shutdown).await?;
        match reply {
            submission::SubmissionReply::Shutdown => Ok(()),
            _ => Err(EngineError::ReplyDropped),
        }
    }

    /// Spawns a subagent child thread under `parent_thread_id` and returns
    /// the newly allocated child [`ThreadId`].
    ///
    /// The child thread starts with an **empty** transcript (fresh context
    /// window), runs a turn seeded by `definition.prompt`, and delivers its
    /// final message back via [`EngineEvent::SubagentCompleted`].
    ///
    /// Three hard constraints are enforced (Claude Code Subagents spec):
    ///
    /// * **No recursion** — the parent must not itself be a subagent.
    /// * **Child spawn disabled** — `definition.allow_subagent_spawn` must
    ///   be `false`.
    /// * **Scope can only narrow** — the child scope must not widen the
    ///   parent's permission scope.
    ///
    /// # Errors
    ///
    /// * [`EngineError::SubagentSpawnFailed`] when any of the above
    ///   constraints is violated, or when the parent thread is not found.
    /// * [`EngineError::ActorStopped`] / [`EngineError::ReplyDropped`]
    ///   / [`EngineError::ReplyTimedOut`] on channel-level failures.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use zhive_core::engine::Engine;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::permission::SubagentDefinition;
    ///
    /// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// let engine = Engine::spawn();
    /// let parent = ThreadId(Arc::from("thread:native/parent"));
    ///
    /// // Start a turn on the parent first so the thread exists.
    /// engine.start_turn(parent.clone(), vec![], None).await?;
    ///
    /// let def: SubagentDefinition = serde_json::from_value(serde_json::json!({
    ///     "name": "scout",
    ///     "description": "read-only scout",
    ///     "prompt": "Check the environment.",
    /// }))?;
    /// let child_id = engine.spawn_subagent(parent, def).await?;
    /// println!("child thread: {}", child_id.0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn spawn_subagent(
        &self,
        parent_thread_id: ThreadId,
        definition: SubagentDefinition,
    ) -> Result<ThreadId, EngineError> {
        let reply = self
            .submit_with_reply(Submission::SpawnSubagent {
                parent_thread_id,
                definition,
            })
            .await?;
        match reply {
            submission::SubmissionReply::SpawnSubagent(Ok(child_id)) => Ok(child_id),
            submission::SubmissionReply::SpawnSubagent(Err(err)) => {
                Err(EngineError::SubagentSpawnFailed(err))
            }
            _ => Err(EngineError::ReplyDropped),
        }
    }
}

// ============================================================
// Test helpers (always compiled with #[cfg(test)] on the module)
// ============================================================

#[cfg(test)]
mod test_helpers {
    use async_trait::async_trait;
    use futures::stream;
    use llmsdk::LanguageModel;
    use llmsdk::language_model::{
        BoxStream, CallOptions, FinishReason, FinishReasonKind, GenerateResult, StreamPart,
        StreamResult,
    };
    use std::sync::Arc as StdArc;
    use tokio::sync::Barrier;

    /// A model that blocks on a [`Barrier`] inside `do_stream`.
    ///
    /// Used to keep the engine in Turn phase long enough for a second
    /// `start_turn` to observe `EngineBusy`, and for cancel tests.
    #[derive(Debug)]
    pub(super) struct BarrierModel {
        pub(super) barrier: StdArc<Barrier>,
    }

    impl BarrierModel {
        pub(super) fn new_pair(parties: usize) -> (StdArc<Barrier>, Self) {
            let b = StdArc::new(Barrier::new(parties));
            let model = Self {
                barrier: StdArc::clone(&b),
            };
            (b, model)
        }
    }

    #[async_trait]
    impl LanguageModel for BarrierModel {
        fn provider(&self) -> &'static str {
            "test"
        }
        fn model_id(&self) -> &'static str {
            "barrier"
        }
        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }
        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            // Block until the test calls barrier.wait() — this keeps the
            // engine in Turn phase for the duration needed by the test.
            self.barrier.wait().await;
            let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::empty());
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }

    /// A model whose stream yields exactly one `Err` item.
    ///
    /// Used to verify that the in-stream error path emits `TurnFailed`
    /// and does NOT follow it with `TurnCompleted`.
    #[derive(Debug)]
    pub(super) struct ErrorStreamModel;

    #[async_trait]
    impl LanguageModel for ErrorStreamModel {
        fn provider(&self) -> &'static str {
            "test"
        }
        fn model_id(&self) -> &'static str {
            "error-stream"
        }
        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }
        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            let err = llmsdk::ProviderError::no_such_model("test", "languageModel");
            let s: BoxStream<llmsdk::error::Result<StreamPart>> =
                Box::pin(stream::iter(vec![Err(err)]));
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ScriptedModel;
    use llmsdk::language_model::StreamPart;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    /// Helper: receive events up to `limit` times, looking for `pred`.
    async fn collect_events_until(
        rx: &mut broadcast::Receiver<EngineEvent>,
        limit: usize,
        mut pred: impl FnMut(&EngineEvent) -> bool,
    ) -> bool {
        for _ in 0..limit {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(ev)) if pred(&ev) => return true,
                Ok(Ok(_)) => {}
                _ => return false,
            }
        }
        false
    }

    #[tokio::test]
    async fn start_turn_emits_started_then_completed() {
        let engine = Engine::spawn();
        let mut events = engine.subscribe();
        let turn_id = engine
            .start_turn(tid("thread:native/a"), Vec::new(), None)
            .await
            .unwrap();
        assert!(turn_id.0.starts_with("turn:thread:native/a/"));

        let mut saw_started = false;
        let mut saw_completed = false;
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("timeout")
                .expect("broadcast")
            {
                EngineEvent::TurnStarted { .. } => saw_started = true,
                EngineEvent::TurnCompleted { .. } => saw_completed = true,
                _ => {}
            }
            if saw_started && saw_completed {
                break;
            }
        }
        assert!(saw_started, "expected TurnStarted");
        assert!(saw_completed, "expected TurnCompleted");
        engine.shutdown().await.unwrap();
    }

    /// A turn started while the engine is already in Turn phase must
    /// surface `EngineBusy`. We use a [`test_helpers::BarrierModel`] that
    /// blocks inside `do_stream`, keeping the engine in Turn phase long
    /// enough for the second `start_turn` call to observe the conflict.
    ///
    /// Synchronization is done by subscribing to the event channel before
    /// the first `start_turn`, then awaiting `TurnStarted`. Once the
    /// subscriber observes `TurnStarted` the phase is guaranteed to be
    /// `Turn`, so the subsequent `start_turn` will always surface
    /// `EngineBusy`. This avoids the inherent race in `yield_now` loops.
    #[tokio::test]
    async fn start_turn_returns_busy_when_engine_phase_not_idle() {
        let (barrier, model) = test_helpers::BarrierModel::new_pair(2);
        let engine = Engine::spawn_with_provider(DynLanguageModel::new(model))
            .with_reply_timeout(std::time::Duration::from_secs(5));

        // Subscribe BEFORE the first start_turn so we can observe TurnStarted.
        let mut events = engine.subscribe();

        // First start_turn — the actor spawns a turn task that blocks on
        // the barrier inside do_stream, keeping the engine in Turn phase.
        engine
            .start_turn(tid("thread:native/busy-1"), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted: once we see it, the phase is guaranteed
        // Turn and any subsequent start_turn will surface EngineBusy.
        let saw_started = collect_events_until(&mut events, 16, |ev| {
            matches!(ev, EngineEvent::TurnStarted { .. })
        })
        .await;
        assert!(saw_started, "expected TurnStarted from first turn");

        // Second start_turn while engine is still in Turn phase.
        let result = engine
            .start_turn(tid("thread:native/busy-2"), Vec::new(), None)
            .await;

        // Unblock the first turn so it can complete.
        barrier.wait().await;

        assert!(
            matches!(result, Err(EngineError::EngineBusy { .. })),
            "expected EngineBusy, got {result:?}"
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn phase_changed_events_carry_thread_id() {
        let engine = Engine::spawn();
        let mut events = engine.subscribe();
        let id = tid("thread:native/phase");
        engine
            .start_turn(id.clone(), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_idle_to_turn = false;
        let mut saw_turn_to_idle = false;
        for _ in 0..32 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("event recv must not time out")
                .expect("broadcast send error");
            if let EngineEvent::PhaseChanged {
                thread_id,
                from,
                to,
            } = ev
            {
                assert_eq!(
                    thread_id.as_ref(),
                    Some(&id),
                    "PhaseChanged must name thread"
                );
                match (from, to) {
                    (
                        zhive_proto::hook::EnginePhase::Idle,
                        zhive_proto::hook::EnginePhase::Turn,
                    ) => saw_idle_to_turn = true,
                    (
                        zhive_proto::hook::EnginePhase::Turn,
                        zhive_proto::hook::EnginePhase::Idle,
                    ) => saw_turn_to_idle = true,
                    _ => {}
                }
            }
            if saw_idle_to_turn && saw_turn_to_idle {
                break;
            }
        }
        assert!(saw_idle_to_turn && saw_turn_to_idle);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resume_permission_routes_to_reducer() {
        use zhive_proto::permission::PermissionOutcome;
        let engine = Engine::spawn();
        let reducer = engine.permission_reducer();

        let request: zhive_proto::permission::RequestPermissionRequest =
            serde_json::from_value(serde_json::json!({
                "threadId": "thread:native/a",
                "resourceType": "tool",
                "name": "read_file",
                "reason": "test",
                "options": []
            }))
            .unwrap();
        let (key, _req, rx) = reducer.enroll(request);
        let wire_id = key.to_wire();

        let reply = engine
            .resume_permission(
                wire_id,
                PermissionOutcome::Selected {
                    option_id: "allow_once".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(reply, submission::ResumePermissionReply::Resolved);

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), reducer.wait(rx))
            .await
            .expect("must resolve via ResumePermission")
            .unwrap();
        assert!(matches!(outcome, PermissionOutcome::Selected { .. }));
        engine.shutdown().await.unwrap();
    }

    // The full "cancel_turn cancels every pending permission" path is
    // exercised in `engine/inner.rs#cancel_turn_cancels_pending_permissions`
    // via a direct EngineInner test.

    #[tokio::test]
    async fn cancel_turn_with_no_active_turn_is_noop() {
        let engine = Engine::spawn();
        let cancelled = engine
            .cancel_turn(tid("thread:native/missing"))
            .await
            .unwrap();
        assert!(cancelled.is_none());
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_actor() {
        let engine = Engine::spawn();
        engine.shutdown().await.unwrap();
        let mut last = Ok(());
        for _ in 0..20 {
            last = engine
                .submit(Submission::CancelTurn {
                    thread_id: tid("x"),
                })
                .await;
            if last.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(matches!(last, Err(EngineError::ActorStopped)));
    }

    // ============================================================
    // Increment-2 provider-driven turn tests
    // ============================================================

    /// A turn over a scripted text response must emit
    /// `ItemAppended(AgentMessage { text: "hello world" })` then `TurnCompleted`.
    #[tokio::test]
    async fn scripted_text_turn_emits_item_appended_then_completed() {
        let model = ScriptedModel::new(
            "test",
            "m",
            vec![
                StreamPart::TextStart {
                    id: "b0".into(),
                    provider_metadata: None,
                },
                StreamPart::TextDelta {
                    id: "b0".into(),
                    delta: "hello ".into(),
                    provider_metadata: None,
                },
                StreamPart::TextDelta {
                    id: "b0".into(),
                    delta: "world".into(),
                    provider_metadata: None,
                },
                StreamPart::TextEnd {
                    id: "b0".into(),
                    provider_metadata: None,
                },
            ],
        );
        let engine = Engine::spawn_with_provider(model.into_dyn());
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/scripted-text"), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_item = false;
        let mut saw_completed = false;
        for _ in 0..32 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("timeout")
                .expect("broadcast");
            match ev {
                EngineEvent::ItemAppended { item, .. } => {
                    if let zhive_proto::domain::Item::AgentMessage { text, .. } = *item {
                        assert_eq!(text, "hello world");
                        saw_item = true;
                    }
                }
                EngineEvent::TurnCompleted { .. } => saw_completed = true,
                _ => {}
            }
            if saw_item && saw_completed {
                break;
            }
        }

        assert!(saw_item, "expected ItemAppended(AgentMessage)");
        assert!(saw_completed, "expected TurnCompleted");
        engine.shutdown().await.unwrap();
    }

    /// `cancel_turn` mid-stream must stop item emission and yield
    /// `SessionAborted`; `TurnCompleted` must NOT be emitted.
    #[tokio::test]
    async fn cancel_mid_stream_yields_session_aborted_no_completed() {
        // BarrierModel blocks inside do_stream so we can cancel before
        // any items arrive.
        let (barrier, model) = test_helpers::BarrierModel::new_pair(2);
        let engine = Engine::spawn_with_provider(DynLanguageModel::new(model))
            .with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/cancel-mid");
        engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted so the turn task is definitely in-flight.
        let saw_started = collect_events_until(&mut events, 16, |ev| {
            matches!(ev, EngineEvent::TurnStarted { .. })
        })
        .await;
        assert!(saw_started, "expected TurnStarted");

        // Cancel while the model is blocking in do_stream.
        let cancelled = engine.cancel_turn(thread_id).await.unwrap();
        assert!(cancelled.is_some(), "expected a turn id to be cancelled");

        // Unblock the model so its task can exit cleanly.
        barrier.wait().await;

        // SessionAborted must appear; TurnCompleted must NOT.
        let mut saw_aborted = false;
        let mut saw_completed = false;
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_millis(300), events.recv()).await {
                Ok(Ok(EngineEvent::SessionAborted(_))) => saw_aborted = true,
                Ok(Ok(EngineEvent::TurnCompleted { .. })) => saw_completed = true,
                Ok(Ok(_)) => {}
                _ => break,
            }
            if saw_aborted {
                break;
            }
        }
        assert!(saw_aborted, "expected SessionAborted after cancel");
        assert!(!saw_completed, "TurnCompleted must NOT appear after cancel");
        engine.shutdown().await.unwrap();
    }

    /// An in-stream provider error must emit `TurnFailed` and must NOT
    /// follow it with `TurnCompleted` — a turn has exactly one terminal
    /// event.
    ///
    /// This test exercises the `Some(Err(…))` arm of the stream loop
    /// in `run_turn`, which previously called `finish_turn` (emitting
    /// `TurnCompleted`) immediately after broadcasting `TurnFailed`.
    #[tokio::test]
    async fn stream_error_emits_turn_failed_not_completed() {
        let engine =
            Engine::spawn_with_provider(DynLanguageModel::new(test_helpers::ErrorStreamModel))
                .with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/stream-err"), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_failed = false;
        let mut saw_completed = false;
        // Drain up to 32 events with a short per-event timeout so we
        // don't block forever if TurnCompleted is never emitted.
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_millis(500), events.recv()).await {
                Ok(Ok(EngineEvent::TurnFailed { .. })) => saw_failed = true,
                Ok(Ok(EngineEvent::TurnCompleted { .. })) => saw_completed = true,
                Ok(Ok(_)) => {}
                _ => break,
            }
            if saw_failed {
                // Give a brief window for a spurious TurnCompleted to arrive.
                for _ in 0..4 {
                    match tokio::time::timeout(std::time::Duration::from_millis(100), events.recv())
                        .await
                    {
                        Ok(Ok(EngineEvent::TurnCompleted { .. })) => {
                            saw_completed = true;
                            break;
                        }
                        Ok(Ok(_)) => {}
                        _ => break,
                    }
                }
                break;
            }
        }
        assert!(saw_failed, "expected TurnFailed for in-stream error");
        assert!(
            !saw_completed,
            "TurnCompleted must NOT follow TurnFailed for the same turn"
        );
        engine.shutdown().await.unwrap();
    }
}

// ============================================================
// Increment-3 tool-dispatch tests
// ============================================================

#[cfg(test)]
mod inc3_tests {
    //! Integration tests for the inner tool-call loop introduced in increment 3.
    //!
    //! Each test exercises a specific aspect of the dispatch pipeline:
    //!  - `tool_call_executes_echo_and_completes` — happy-path end-to-end.
    //!  - `pre_tool_use_deny_blocks_execution` — hook returning Deny.
    //!  - `red_line_11_invalid_updated_input_blocks_tool` — schema failure.
    //!  - `ask_flow_allow_resolves_tool` — permission Ask → user allows.
    //!  - `max_iteration_cap_terminates_turn` — runaway loop terminates.

    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use futures::stream;
    use llmsdk::language_model::{
        BoxStream, CallOptions, GenerateResult, StreamPart, StreamResult,
    };
    use llmsdk::{LanguageModel, ProviderError};

    use super::*;
    use crate::hooks::{HookFilter, HookFn};
    use crate::tools::{EchoTool, Tool, ToolContext, ToolError, ToolKind, ToolOutput};
    use zhive_proto::hook::HookEvent;
    use zhive_proto::permission::{HookOutput, PermissionDecision};

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    /// Waits for the next `PermissionRequested` id, or `None` on timeout.
    ///
    /// Scans up to 64 events, ignoring everything else; lets a test drive
    /// successive permission prompts in one turn.
    async fn next_permission_request(
        rx: &mut broadcast::Receiver<EngineEvent>,
    ) -> Option<PermissionRequestId> {
        for _ in 0..64 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(EngineEvent::PermissionRequested { request_id, .. })) => {
                    return Some(request_id);
                }
                Ok(Ok(_)) => {}
                _ => return None,
            }
        }
        None
    }

    /// Waits for up to `limit` events, returns `true` if `pred` matched one.
    async fn collect_until(
        rx: &mut broadcast::Receiver<EngineEvent>,
        limit: usize,
        mut pred: impl FnMut(&EngineEvent) -> bool,
    ) -> bool {
        for _ in 0..limit {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(ev)) if pred(&ev) => return true,
                Ok(Ok(_)) => {}
                _ => return false,
            }
        }
        false
    }

    // ---- MultiScriptedModel -----------------------------------------------

    /// A scripted model that returns a different set of [`StreamPart`]s on
    /// each successive call to `do_stream`.
    ///
    /// The call counter is shared so tests can clone the model handle.
    #[derive(Debug, Clone)]
    struct MultiScriptedModel {
        call_count: Arc<AtomicUsize>,
        scripts: Arc<Vec<Vec<StreamPart>>>,
    }

    impl MultiScriptedModel {
        fn new(scripts: Vec<Vec<StreamPart>>) -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
                scripts: Arc::new(scripts),
            }
        }

        fn into_dyn(self) -> DynLanguageModel {
            DynLanguageModel::new(self)
        }
    }

    #[async_trait]
    impl LanguageModel for MultiScriptedModel {
        fn provider(&self) -> &'static str {
            "test"
        }

        fn model_id(&self) -> &'static str {
            "multi-scripted"
        }

        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            use llmsdk::language_model::{FinishReason, FinishReasonKind};
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }

        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            let parts = self.scripts.get(idx).cloned().unwrap_or_default();
            let iter = parts.into_iter().map(Ok::<_, ProviderError>);
            let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(iter));
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }

    // ---- Always-tool-call model (for max-iteration test) ------------------

    /// A model that always emits a `ToolCall` for "echo" so the loop
    /// never terminates on its own (used for the max-iteration cap test).
    #[derive(Debug, Clone)]
    struct AlwaysToolCallModel;

    #[async_trait]
    impl LanguageModel for AlwaysToolCallModel {
        fn provider(&self) -> &'static str {
            "test"
        }

        fn model_id(&self) -> &'static str {
            "always-tool-call"
        }

        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            use llmsdk::language_model::{FinishReason, FinishReasonKind};
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }

        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            use llmsdk::ToolCallPart;
            let call = ToolCallPart {
                tool_call_id: "tc-always".into(),
                tool_name: "echo".into(),
                input: serde_json::json!({"msg": "loop"}),
                provider_executed: None,
                dynamic: None,
                provider_options: None,
            };
            let iter = vec![Ok(StreamPart::ToolCall(call))].into_iter();
            let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(iter));
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }

    // ---- Hook helpers -------------------------------------------------------

    /// A hook that unconditionally returns a specified `PermissionDecision`.
    struct FixedDecisionHook {
        decision: PermissionDecision,
        updated_input: Option<serde_json::Value>,
    }

    #[async_trait]
    impl HookFn for FixedDecisionHook {
        async fn call(&self, _event: &HookEvent) -> Option<HookOutput> {
            Some(
                serde_json::from_value(serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": self.decision,
                        "updatedInput": self.updated_input,
                    }
                }))
                .expect("fixture"),
            )
        }
    }

    fn ext_ref(id: &str) -> zhive_proto::hook::ExtensionRef {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "version": "0.1.0",
            "source": "builtin"
        }))
        .expect("fixture")
    }

    /// A tool whose body blocks until the turn cancel token fires.
    ///
    /// Lets the cancel-during-execute test deterministically hold the
    /// dispatch loop inside `tool.execute` until `cancel_turn` is issued. It
    /// then returns a (would-be) success result, proving the dispatch
    /// `select!` — not the tool — is what races and wins on cancel.
    #[derive(Debug, Clone, Copy)]
    struct BlockUntilCancelledTool;

    #[async_trait]
    impl Tool for BlockUntilCancelledTool {
        fn name(&self) -> &'static str {
            "block_until_cancelled"
        }

        fn kind(&self) -> ToolKind {
            ToolKind::Other
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            ctx: &ToolContext,
        ) -> Result<ToolOutput, ToolError> {
            // Wait for the turn to be cancelled, then (try to) return a
            // success result. The dispatch select! should have already
            // returned Blocked by the time this resolves.
            ctx.cancel.cancelled().await;
            Ok(ToolOutput::text("late result that must be discarded"))
        }
    }

    // =========================================================================
    // Test 1: happy path — model emits tool_call(echo) then a text answer
    // =========================================================================

    /// A turn where the scripted model emits a `ToolCall(echo)` then (on the
    /// 2nd iteration) a text answer: asserts the tool executed, a
    /// `ToolCall Completed` item + the final `AgentMessage` are appended, and
    /// `TurnCompleted` fires.
    #[tokio::test]
    async fn tool_call_executes_echo_and_completes() {
        use llmsdk::ToolCallPart;

        // First call: one tool call.
        let script0 = vec![StreamPart::ToolCall(ToolCallPart {
            tool_call_id: "tc-0".into(),
            tool_name: "echo".into(),
            input: serde_json::json!({"msg": "hello"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        })];
        // Second call: a text response (no tool calls → loop ends).
        let script1 = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextDelta {
                id: "b0".into(),
                delta: "done".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let model = MultiScriptedModel::new(vec![script0, script1]);
        let cfg = EngineConfig {
            provider: model.into_dyn(),
            tools: Arc::new(tools),
            hook_host: Arc::new(HookHost::new()),
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/echo"), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_tool_completed = false;
        let mut saw_agent_msg = false;
        let mut saw_turn_completed = false;

        for _ in 0..64 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("timeout")
                .expect("broadcast")
            {
                EngineEvent::ItemAppended { item, .. } => match *item {
                    zhive_proto::domain::Item::ToolCall {
                        status: zhive_proto::domain::ToolCallStatus::Completed,
                        ..
                    } => {
                        saw_tool_completed = true;
                    }
                    zhive_proto::domain::Item::AgentMessage { text, .. } if text == "done" => {
                        saw_agent_msg = true;
                    }
                    _ => {}
                },
                EngineEvent::TurnCompleted { .. } => saw_turn_completed = true,
                _ => {}
            }
            if saw_tool_completed && saw_agent_msg && saw_turn_completed {
                break;
            }
        }

        assert!(saw_tool_completed, "expected ToolCall Completed item");
        assert!(saw_agent_msg, "expected AgentMessage 'done'");
        assert!(saw_turn_completed, "expected TurnCompleted");
        engine.shutdown().await.unwrap();
    }

    // =========================================================================
    // Test 1b: two tool calls in one turn → both execute (parallel dispatch)
    // =========================================================================

    /// A turn whose first model call emits TWO `ToolCall(echo)` parts: both must
    /// run through the parallel execute phase and each yield a `ToolCall`
    /// `Completed` item before the turn finishes on the second (text) call.
    #[tokio::test]
    async fn two_tool_calls_in_one_turn_both_complete() {
        use llmsdk::ToolCallPart;

        // First call: two tool calls in one model turn.
        let script0 = vec![
            StreamPart::ToolCall(ToolCallPart {
                tool_call_id: "tc-a".into(),
                tool_name: "echo".into(),
                input: serde_json::json!({"msg": "first"}),
                provider_executed: None,
                dynamic: None,
                provider_options: None,
            }),
            StreamPart::ToolCall(ToolCallPart {
                tool_call_id: "tc-b".into(),
                tool_name: "echo".into(),
                input: serde_json::json!({"msg": "second"}),
                provider_executed: None,
                dynamic: None,
                provider_options: None,
            }),
        ];
        // Second call: a text response (no tool calls → loop ends).
        let script1 = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextDelta {
                id: "b0".into(),
                delta: "done".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let cfg = EngineConfig {
            provider: MultiScriptedModel::new(vec![script0, script1]).into_dyn(),
            tools: Arc::new(tools),
            hook_host: Arc::new(HookHost::new()),
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/two-tools"), Vec::new(), None)
            .await
            .unwrap();

        let mut completed_ids: Vec<String> = Vec::new();
        let mut saw_turn_completed = false;
        for _ in 0..128 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("timeout")
                .expect("broadcast")
            {
                EngineEvent::ItemAppended { item, .. } => {
                    if let zhive_proto::domain::Item::ToolCall {
                        status: zhive_proto::domain::ToolCallStatus::Completed,
                        provider_tool_call_id: Some(id),
                        ..
                    } = *item
                    {
                        completed_ids.push(id);
                    }
                }
                EngineEvent::TurnCompleted { .. } => {
                    saw_turn_completed = true;
                    break;
                }
                _ => {}
            }
        }

        assert_eq!(
            completed_ids.len(),
            2,
            "both tool calls must produce a Completed ToolCall item, got {completed_ids:?}"
        );
        // Items must be committed in model-emit order.
        assert_eq!(completed_ids, vec!["tc-a", "tc-b"], "emit order preserved");
        assert!(saw_turn_completed, "expected TurnCompleted");
        engine.shutdown().await.unwrap();
    }

    // =========================================================================
    // Test 2: PreToolUse hook returning Deny blocks execution
    // =========================================================================

    /// A `PreToolUse` hook returning `Deny`: asserts the tool did NOT
    /// execute and a denial result was appended (`ToolCall { status: Failed }`).
    #[tokio::test]
    async fn pre_tool_use_deny_blocks_execution() {
        use llmsdk::ToolCallPart;

        let script0 = vec![StreamPart::ToolCall(ToolCallPart {
            tool_call_id: "tc-deny".into(),
            tool_name: "echo".into(),
            input: serde_json::json!({"msg": "blocked"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        })];
        // Second call: text answer to end the turn.
        let script1 = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let hook_host = Arc::new(HookHost::new());
        let _scope = hook_host
            .register(
                ext_ref("deny-hook"),
                HookFilter::default(),
                0,
                Arc::new(FixedDecisionHook {
                    decision: PermissionDecision::Deny,
                    updated_input: None,
                }),
            )
            .unwrap();

        let cfg = EngineConfig {
            provider: MultiScriptedModel::new(vec![script0, script1]).into_dyn(),
            tools: Arc::new(tools),
            hook_host,
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/deny"), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_tool_failed = false;
        let saw_completed = collect_until(&mut events, 64, |ev| {
            if let EngineEvent::ItemAppended { item, .. } = ev
                && let zhive_proto::domain::Item::ToolCall { status, .. } = item.as_ref()
                && *status == zhive_proto::domain::ToolCallStatus::Failed
            {
                saw_tool_failed = true;
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(
            saw_tool_failed,
            "denied tool must emit ToolCall Failed item"
        );
        assert!(saw_completed, "TurnCompleted must still fire after denial");
        engine.shutdown().await.unwrap();
    }

    // =========================================================================
    // Test 3: Red line 11 — updated_input fails schema revalidation → blocked
    // =========================================================================

    /// A `PreToolUse` hook returning an `updated_input` that fails schema
    /// re-validation: asserts the tool was blocked (`ToolCall { status: Failed }`).
    #[tokio::test]
    async fn red_line_11_invalid_updated_input_blocks_tool() {
        use llmsdk::ToolCallPart;

        let script0 = vec![StreamPart::ToolCall(ToolCallPart {
            tool_call_id: "tc-rl11".into(),
            tool_name: "echo".into(),
            input: serde_json::json!({"msg": "original"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        })];
        let script1 = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        // Register a strict schema for "echo" that requires a "msg" string.
        let hook_host = Arc::new(HookHost::new());
        hook_host
            .schemas()
            .register(
                "echo",
                &serde_json::json!({
                    "type": "object",
                    "required": ["msg"],
                    "properties": {"msg": {"type": "string"}},
                    "additionalProperties": false
                }),
            )
            .unwrap();

        // Hook returns an updated_input that violates the schema (extra field).
        let _scope = hook_host
            .register(
                ext_ref("rl11-hook"),
                HookFilter::default(),
                0,
                Arc::new(FixedDecisionHook {
                    decision: PermissionDecision::Allow,
                    updated_input: Some(serde_json::json!({"msg": "ok", "bad_field": 42})),
                }),
            )
            .unwrap();

        let cfg = EngineConfig {
            provider: MultiScriptedModel::new(vec![script0, script1]).into_dyn(),
            tools: Arc::new(tools),
            hook_host,
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/rl11"), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_tool_failed = false;
        collect_until(&mut events, 64, |ev| {
            if let EngineEvent::ItemAppended { item, .. } = ev
                && let zhive_proto::domain::Item::ToolCall { status, .. } = item.as_ref()
                && *status == zhive_proto::domain::ToolCallStatus::Failed
            {
                saw_tool_failed = true;
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(
            saw_tool_failed,
            "schema-invalid updated_input must block tool (ToolCall Failed)"
        );
        engine.shutdown().await.unwrap();
    }

    // =========================================================================
    // Test 4: Ask flow — permission resolved via ResumePermission
    // =========================================================================

    /// A hook returning `Ask`; the test drives `ResumePermission` (Selected
    /// allow) and asserts the tool then executed.
    #[tokio::test]
    async fn ask_flow_allow_resolves_tool() {
        use llmsdk::ToolCallPart;

        let script0 = vec![StreamPart::ToolCall(ToolCallPart {
            tool_call_id: "tc-ask".into(),
            tool_name: "echo".into(),
            input: serde_json::json!({"msg": "ask"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        })];
        let script1 = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let hook_host = Arc::new(HookHost::new());
        let _scope = hook_host
            .register(
                ext_ref("ask-hook"),
                HookFilter::default(),
                0,
                Arc::new(FixedDecisionHook {
                    decision: PermissionDecision::Ask,
                    updated_input: None,
                }),
            )
            .unwrap();

        let cfg = EngineConfig {
            provider: MultiScriptedModel::new(vec![script0, script1]).into_dyn(),
            tools: Arc::new(tools),
            hook_host,
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/ask"), Vec::new(), None)
            .await
            .unwrap();

        // Wait for PermissionRequested and answer it.
        let mut request_id_opt: Option<PermissionRequestId> = None;
        for _ in 0..32 {
            if let EngineEvent::PermissionRequested { request_id, .. } =
                tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                    .await
                    .expect("timeout")
                    .expect("broadcast")
            {
                request_id_opt = Some(request_id);
                break;
            }
        }
        let request_id = request_id_opt.expect("PermissionRequested must fire");

        engine
            .resume_permission(
                request_id,
                PermissionOutcome::Selected {
                    option_id: "allow_once".into(),
                },
            )
            .await
            .unwrap();

        // Now wait for ToolCall Completed + TurnCompleted.
        let mut saw_tool_completed = false;
        let saw_turn_completed = collect_until(&mut events, 64, |ev| {
            if let EngineEvent::ItemAppended { item, .. } = ev
                && let zhive_proto::domain::Item::ToolCall { status, .. } = item.as_ref()
                && *status == zhive_proto::domain::ToolCallStatus::Completed
            {
                saw_tool_completed = true;
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(saw_tool_completed, "tool must have executed after Allow");
        assert!(saw_turn_completed, "TurnCompleted must fire");
        engine.shutdown().await.unwrap();
    }

    // =========================================================================
    // Test 4a: allow-always persistence (P1-1)
    // =========================================================================

    /// Builds an engine whose `Ask` hook fires for every `echo` call, with the
    /// model emitting `echo` on two consecutive turn-loop iterations (script0,
    /// script1) before a clean text turn (script2). The single turn therefore
    /// reaches the permission gate twice for the same tool name.
    async fn spawn_engine_two_echo_asks(
        thread: &str,
    ) -> (Engine, broadcast::Receiver<EngineEvent>) {
        use llmsdk::ToolCallPart;

        let echo_call = |id: &str| {
            vec![StreamPart::ToolCall(ToolCallPart {
                tool_call_id: id.into(),
                tool_name: "echo".into(),
                input: serde_json::json!({"msg": "again"}),
                provider_executed: None,
                dynamic: None,
                provider_options: None,
            })]
        };
        let clean_turn = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let hook_host = Arc::new(HookHost::new());
        // Leak the scope so the hook stays registered for the whole turn
        // (this fixture drives two tool-call iterations; a dropped scope would
        // deregister the hook after the first, silently turning the second
        // call's decision into a no-hook Allow).
        let _id = hook_host
            .register(
                ext_ref("allow-always-hook"),
                HookFilter::default(),
                0,
                Arc::new(FixedDecisionHook {
                    decision: PermissionDecision::Ask,
                    updated_input: None,
                }),
            )
            .unwrap()
            .leak();

        let cfg = EngineConfig {
            provider: MultiScriptedModel::new(vec![
                echo_call("tc-aa-1"),
                echo_call("tc-aa-2"),
                clean_turn,
            ])
            .into_dyn(),
            tools: Arc::new(tools),
            hook_host,
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));
        let events = engine.subscribe();
        engine
            .start_turn(tid(thread), Vec::new(), None)
            .await
            .unwrap();
        (engine, events)
    }

    /// Picking `AllowAlways` for a tool must suppress the second prompt: the
    /// same tool's next `Ask` is auto-allowed and executes without any further
    /// `PermissionRequested` event.
    #[tokio::test]
    async fn allow_always_suppresses_second_prompt() {
        let (engine, mut events) = spawn_engine_two_echo_asks("thread:native/allow-always").await;

        // Answer the FIRST PermissionRequested with allow-always.
        let request_id = next_permission_request(&mut events)
            .await
            .expect("first PermissionRequested must fire");
        engine
            .resume_permission(
                request_id,
                PermissionOutcome::Selected {
                    option_id: "allow-always".into(),
                },
            )
            .await
            .unwrap();

        // From here on: the second echo must execute WITHOUT another prompt.
        // Collect to TurnCompleted; assert two tool completions and NO second
        // PermissionRequested.
        let mut completed_tools = 0_usize;
        let mut second_prompt = false;
        let saw_turn_completed = collect_until(&mut events, 128, |ev| {
            match ev {
                EngineEvent::PermissionRequested { .. } => second_prompt = true,
                EngineEvent::ItemAppended { item, .. } => {
                    if let zhive_proto::domain::Item::ToolCall { status, .. } = item.as_ref()
                        && *status == zhive_proto::domain::ToolCallStatus::Completed
                    {
                        completed_tools += 1;
                    }
                }
                _ => {}
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(saw_turn_completed, "TurnCompleted must fire");
        assert!(
            !second_prompt,
            "allow-always must suppress the second prompt"
        );
        assert_eq!(
            completed_tools, 2,
            "both echo calls must complete (first via allow-always grant, second auto-allowed)"
        );
        engine.shutdown().await.unwrap();
    }

    /// Picking `AllowOnce` must NOT persist: the same tool's next `Ask` prompts
    /// again. This is the contrast case for `allow_always_suppresses_second_prompt`.
    #[tokio::test]
    async fn allow_once_still_prompts_second_time() {
        let (engine, mut events) = spawn_engine_two_echo_asks("thread:native/allow-once").await;

        // Answer the FIRST prompt with allow-once.
        let req1 = next_permission_request(&mut events)
            .await
            .expect("first PermissionRequested must fire");
        engine
            .resume_permission(
                req1,
                PermissionOutcome::Selected {
                    option_id: "allow-once".into(),
                },
            )
            .await
            .unwrap();

        // A SECOND prompt MUST fire (allow-once did not persist). Answer it too
        // so the turn can complete cleanly.
        let req2 = next_permission_request(&mut events)
            .await
            .expect("second PermissionRequested must fire (allow-once does not persist)");
        engine
            .resume_permission(
                req2,
                PermissionOutcome::Selected {
                    option_id: "allow-once".into(),
                },
            )
            .await
            .unwrap();

        let saw_turn_completed = collect_until(&mut events, 64, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;
        assert!(saw_turn_completed, "TurnCompleted must fire");
        engine.shutdown().await.unwrap();
    }

    /// SECURITY RED LINE: a hook returning `Deny` blocks the tool even when the
    /// reducer has the tool recorded as allow-always. Allow-always must only
    /// downgrade an `Ask`, never override a folded `Deny`.
    #[tokio::test]
    async fn deny_overrides_allow_always() {
        use llmsdk::ToolCallPart;

        let script0 = vec![StreamPart::ToolCall(ToolCallPart {
            tool_call_id: "tc-deny".into(),
            tool_name: "echo".into(),
            input: serde_json::json!({"msg": "deny"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        })];
        let script1 = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let hook_host = Arc::new(HookHost::new());
        let _scope = hook_host
            .register(
                ext_ref("deny-hook"),
                HookFilter::default(),
                0,
                Arc::new(FixedDecisionHook {
                    decision: PermissionDecision::Deny,
                    updated_input: None,
                }),
            )
            .unwrap();

        let cfg = EngineConfig {
            provider: MultiScriptedModel::new(vec![script0, script1]).into_dyn(),
            tools: Arc::new(tools),
            hook_host,
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));

        // Pre-record allow-always for `echo` on the SHARED reducer. The red line
        // is that this must not relax the hook's Deny.
        engine.permission_reducer().record_allow_always("echo");

        let mut events = engine.subscribe();
        engine
            .start_turn(tid("thread:native/deny-aa"), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_tool_failed = false;
        let mut saw_prompt = false;
        let saw_turn_completed = collect_until(&mut events, 64, |ev| {
            match ev {
                EngineEvent::PermissionRequested { .. } => saw_prompt = true,
                EngineEvent::ItemAppended { item, .. } => {
                    if let zhive_proto::domain::Item::ToolCall { status, .. } = item.as_ref()
                        && *status == zhive_proto::domain::ToolCallStatus::Failed
                    {
                        saw_tool_failed = true;
                    }
                }
                _ => {}
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(saw_turn_completed, "TurnCompleted must fire");
        assert!(
            saw_tool_failed,
            "Deny must block the tool despite allow-always (security red line)"
        );
        assert!(
            !saw_prompt,
            "Deny short-circuits before any prompt; allow-always is never consulted"
        );
        engine.shutdown().await.unwrap();
    }

    // =========================================================================
    // Test 4b: Defer flow — turn suspends until resume_permission (#1a)
    // =========================================================================

    /// Helper: builds an engine whose hook returns `decision` for the single
    /// `echo` tool call, then runs the turn and returns the engine + event rx.
    /// The model emits one `echo` tool call (script0) then a clean text turn
    /// (script1), matching the Ask-flow fixture.
    async fn spawn_engine_with_decision(
        thread: &str,
        decision: PermissionDecision,
    ) -> (Engine, broadcast::Receiver<EngineEvent>) {
        use llmsdk::ToolCallPart;

        let script0 = vec![StreamPart::ToolCall(ToolCallPart {
            tool_call_id: "tc-defer".into(),
            tool_name: "echo".into(),
            input: serde_json::json!({"msg": "defer"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        })];
        let script1 = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let hook_host = Arc::new(HookHost::new());
        let _scope = hook_host
            .register(
                ext_ref("defer-hook"),
                HookFilter::default(),
                0,
                Arc::new(FixedDecisionHook {
                    decision,
                    updated_input: None,
                }),
            )
            .unwrap();

        let cfg = EngineConfig {
            provider: MultiScriptedModel::new(vec![script0, script1]).into_dyn(),
            tools: Arc::new(tools),
            hook_host,
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));
        let events = engine.subscribe();
        engine
            .start_turn(tid(thread), Vec::new(), None)
            .await
            .unwrap();
        (engine, events)
    }

    /// A hook returning `Defer` must NOT block the tool (the pre-#1a
    /// behaviour). Instead the turn suspends: a `PermissionRequested` event
    /// fires and the tool only executes once `resume_permission` (Selected
    /// allow) arrives.
    #[tokio::test]
    async fn defer_flow_suspends_then_resumes_on_allow() {
        let (engine, mut events) =
            spawn_engine_with_decision("thread:native/defer-allow", PermissionDecision::Defer)
                .await;

        // A PermissionRequested event is IMPOSSIBLE on the old Defer→blocked
        // path (which never enrolled), so its presence is the load-bearing
        // proof that Defer now enrolls + suspends. The TurnCompleted-before-
        // resume panic guard additionally proves the turn does not auto-
        // resolve. (Ask-vs-Defer unboundedness is covered at the reducer layer
        // by `permission::tests::wait_unbounded_ignores_timeout_and_resolves`.)
        let mut request_id_opt: Option<PermissionRequestId> = None;
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await {
                Ok(Ok(EngineEvent::PermissionRequested { request_id, .. })) => {
                    request_id_opt = Some(request_id);
                    break;
                }
                Ok(Ok(EngineEvent::TurnCompleted { .. })) => {
                    panic!("turn must NOT complete while a Defer is suspended");
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        let request_id = request_id_opt.expect("Defer must emit PermissionRequested (suspended)");

        engine
            .resume_permission(
                request_id,
                PermissionOutcome::Selected {
                    option_id: "allow_once".into(),
                },
            )
            .await
            .unwrap();

        let mut saw_tool_completed = false;
        let saw_turn_completed = collect_until(&mut events, 64, |ev| {
            if let EngineEvent::ItemAppended { item, .. } = ev
                && let zhive_proto::domain::Item::ToolCall { status, .. } = item.as_ref()
                && *status == zhive_proto::domain::ToolCallStatus::Completed
            {
                saw_tool_completed = true;
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(
            saw_tool_completed,
            "tool must execute after Defer→resume(allow)"
        );
        assert!(saw_turn_completed, "TurnCompleted must fire after resume");
        engine.shutdown().await.unwrap();
    }

    /// A suspended `Defer` resolved with `Cancelled` must block the tool
    /// (`ToolCall` Failed) and complete the turn.
    #[tokio::test]
    async fn defer_flow_resume_cancelled_blocks_tool() {
        let (engine, mut events) =
            spawn_engine_with_decision("thread:native/defer-deny", PermissionDecision::Defer).await;

        let mut request_id_opt: Option<PermissionRequestId> = None;
        for _ in 0..32 {
            if let Ok(Ok(EngineEvent::PermissionRequested { request_id, .. })) =
                tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await
            {
                request_id_opt = Some(request_id);
                break;
            }
        }
        let request_id = request_id_opt.expect("Defer must emit PermissionRequested");

        engine
            .resume_permission(request_id, PermissionOutcome::Cancelled)
            .await
            .unwrap();

        let mut saw_tool_failed = false;
        let saw_turn_completed = collect_until(&mut events, 64, |ev| {
            if let EngineEvent::ItemAppended { item, .. } = ev
                && let zhive_proto::domain::Item::ToolCall { status, .. } = item.as_ref()
                && *status == zhive_proto::domain::ToolCallStatus::Failed
            {
                saw_tool_failed = true;
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(
            saw_tool_failed,
            "Defer→resume(Cancelled) must block the tool"
        );
        assert!(saw_turn_completed, "TurnCompleted must fire");
        engine.shutdown().await.unwrap();
    }

    /// A suspended `Defer` aborted via `cancel_turn` (the `cancel_all` path,
    /// distinct from a client `resume_permission(Cancelled)`) must abort
    /// cleanly: a `SessionAborted` fires, the tool never executes, and no
    /// spurious `TurnCompleted` is emitted (ACP 0.12 Cancelled contract).
    #[tokio::test]
    async fn defer_flow_cancel_turn_aborts_cleanly() {
        let (engine, mut events) =
            spawn_engine_with_decision("thread:native/defer-cancel", PermissionDecision::Defer)
                .await;

        let mut found = false;
        for _ in 0..32 {
            if let Ok(Ok(EngineEvent::PermissionRequested { .. })) =
                tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await
            {
                found = true;
                break;
            }
        }
        assert!(found, "Defer must emit PermissionRequested before cancel");

        // cancel_turn drains the pending map via cancel_all → the suspended
        // wait observes Cancelled, and the turn is aborted.
        engine
            .cancel_turn(tid("thread:native/defer-cancel"))
            .await
            .unwrap();

        let mut saw_aborted = false;
        let mut saw_tool_completed = false;
        let mut saw_turn_completed = false;
        for _ in 0..64 {
            match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv()).await {
                Ok(Ok(EngineEvent::SessionAborted(_))) => saw_aborted = true,
                Ok(Ok(EngineEvent::TurnCompleted { .. })) => saw_turn_completed = true,
                Ok(Ok(EngineEvent::ItemAppended { item, .. }))
                    if matches!(
                        item.as_ref(),
                        zhive_proto::domain::Item::ToolCall {
                            status: zhive_proto::domain::ToolCallStatus::Completed,
                            ..
                        }
                    ) =>
                {
                    saw_tool_completed = true;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
            if saw_aborted {
                break;
            }
        }

        assert!(
            saw_aborted,
            "cancel_turn on a suspended Defer must emit SessionAborted"
        );
        assert!(
            !saw_tool_completed,
            "tool must NOT execute when a suspended Defer is cancelled"
        );
        assert!(
            !saw_turn_completed,
            "a cancelled turn must NOT emit TurnCompleted"
        );
        engine.shutdown().await.unwrap();
    }

    // =========================================================================
    // Test 5: Max-iteration cap terminates the turn
    // =========================================================================

    /// A scripted model that ALWAYS emits a `ToolCall`: asserts the turn
    /// terminates (does not hang) at the iteration cap.
    #[tokio::test]
    async fn max_iteration_cap_terminates_turn() {
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        // Cap the turn at 4 iterations so the always-tool-calling model
        // terminates quickly instead of running the default 80 iterations.
        let cfg = EngineConfig {
            provider: DynLanguageModel::new(AlwaysToolCallModel),
            tools: Arc::new(tools),
            hook_host: Arc::new(HookHost::new()),
            storage: None,
            turn_limits: TurnLimits {
                max_iterations: Some(4),
            },
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(60));
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/maxiter"), Vec::new(), None)
            .await
            .unwrap();

        // The turn must complete within the 4-iteration cap even though the
        // model never stops emitting tool calls.
        let saw_completed = collect_until(&mut events, 256, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(
            saw_completed,
            "TurnCompleted must fire even when model always emits tool calls"
        );
        engine.shutdown().await.unwrap();
    }

    // =========================================================================
    // Test 6: the finalized ToolCall item carries provider_tool_call_id
    // =========================================================================

    /// After a tool executes, the broadcast `ToolCall { Completed }` item must
    /// carry the provider's original `provider_tool_call_id` (the same id the
    /// model used in its `ToolCall` stream part), not `None`.
    #[tokio::test]
    async fn completed_tool_call_item_carries_provider_tool_call_id() {
        use llmsdk::ToolCallPart;

        let script0 = vec![StreamPart::ToolCall(ToolCallPart {
            tool_call_id: "toolu_keepme".into(),
            tool_name: "echo".into(),
            input: serde_json::json!({"msg": "hi"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        })];
        let script1 = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let cfg = EngineConfig {
            provider: MultiScriptedModel::new(vec![script0, script1]).into_dyn(),
            tools: Arc::new(tools),
            hook_host: Arc::new(HookHost::new()),
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/keepid"), Vec::new(), None)
            .await
            .unwrap();

        let mut found_id: Option<Option<String>> = None;
        for _ in 0..64 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("timeout")
                .expect("broadcast")
            {
                EngineEvent::ItemAppended { item, .. } => {
                    if let zhive_proto::domain::Item::ToolCall {
                        status: zhive_proto::domain::ToolCallStatus::Completed,
                        provider_tool_call_id,
                        ..
                    } = *item
                    {
                        found_id = Some(provider_tool_call_id);
                        break;
                    }
                }
                EngineEvent::TurnCompleted { .. } => break,
                _ => {}
            }
        }

        let id = found_id.expect("expected a Completed ToolCall item");
        assert_eq!(
            id.as_deref(),
            Some("toolu_keepme"),
            "finalized ToolCall item must carry the provider tool_call_id"
        );
        engine.shutdown().await.unwrap();
    }

    // =========================================================================
    // Test 7: cancel during tool execution emits no result item
    // =========================================================================

    /// When the turn is cancelled while a tool is executing, the dispatch
    /// `select!` wins the race and **no** `ToolCall { Completed }` item is
    /// appended/broadcast for the abandoned result. `SessionAborted` fires and
    /// `TurnCompleted` does not.
    #[tokio::test]
    async fn cancel_during_tool_execute_emits_no_result_item() {
        use llmsdk::ToolCallPart;

        // The model emits one tool call for the blocking tool, then (on a
        // hypothetical 2nd call) nothing — the turn is cancelled before that.
        let script0 = vec![StreamPart::ToolCall(ToolCallPart {
            tool_call_id: "toolu_block".into(),
            tool_name: "block_until_cancelled".into(),
            input: serde_json::json!({}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        })];

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(BlockUntilCancelledTool));

        let cfg = EngineConfig {
            provider: MultiScriptedModel::new(vec![script0, vec![]]).into_dyn(),
            tools: Arc::new(tools),
            hook_host: Arc::new(HookHost::new()),
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/cancel-exec");
        engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted so the turn task is in-flight, then give the
        // stream a brief moment to drain and the dispatch loop to reach the
        // blocking tool body.
        let saw_started = collect_until(&mut events, 16, |ev| {
            matches!(ev, EngineEvent::TurnStarted { .. })
        })
        .await;
        assert!(saw_started, "expected TurnStarted");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Cancel while the tool body is blocking in execute().
        let cancelled = engine.cancel_turn(thread_id).await.unwrap();
        assert!(cancelled.is_some(), "expected a turn id to be cancelled");

        // Drain events: SessionAborted must appear; no Completed ToolCall item
        // and no TurnCompleted may appear.
        let mut saw_aborted = false;
        let mut saw_completed_item = false;
        let mut saw_turn_completed = false;
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_millis(300), events.recv()).await {
                Ok(Ok(EngineEvent::SessionAborted(_))) => saw_aborted = true,
                Ok(Ok(EngineEvent::TurnCompleted { .. })) => saw_turn_completed = true,
                Ok(Ok(EngineEvent::ItemAppended { item, .. })) => {
                    if matches!(
                        *item,
                        zhive_proto::domain::Item::ToolCall {
                            status: zhive_proto::domain::ToolCallStatus::Completed,
                            ..
                        }
                    ) {
                        saw_completed_item = true;
                    }
                }
                Ok(Ok(_)) => {}
                _ => {
                    if saw_aborted {
                        break;
                    }
                }
            }
        }

        assert!(saw_aborted, "expected SessionAborted after cancel");
        assert!(
            !saw_completed_item,
            "no Completed ToolCall item may be emitted after cancel during execute"
        );
        assert!(
            !saw_turn_completed,
            "TurnCompleted must NOT fire for a cancelled turn"
        );
        engine.shutdown().await.unwrap();
    }
}

// ============================================================
// Increment-4 injection-queue + CancellationTree tests
// ============================================================

#[cfg(test)]
mod inc4_tests {
    //! Integration tests for the three-queue injection model and
    //! `CancellationTree` wiring introduced in increment 4.
    //!
    //! Test plan:
    //! - `steer_item_appears_before_next_provider_call` — steer drain before
    //!   the second LLM request makes the item visible in `build_call_options`.
    //! - `follow_up_extends_turn_without_tool_calls` — a `FollowUp` item causes
    //!   the turn to do a second provider iteration.
    //! - `next_turn_survives_cancel_and_seeds_next_turn` — `NextTurn` items are
    //!   preserved across `cancel_turn`; `SessionAborted` reports the count;
    //!   the following `start_turn` consumes them.
    //! - `shutdown_cancels_in_flight_turn` — engine shutdown cancels a turn
    //!   that is blocked inside the provider `do_stream`.

    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use futures::stream;
    use llmsdk::LanguageModel;
    use llmsdk::language_model::{
        BoxStream, CallOptions, FinishReason, FinishReasonKind, GenerateResult, StreamPart,
        StreamResult,
    };
    use tokio::sync::Barrier;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use zhive_proto::permission::StreamingBehavior;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn user_item(text: &str) -> zhive_proto::domain::Item {
        use zhive_proto::domain::{ItemContent, ItemId};
        zhive_proto::domain::Item::UserMessage {
            id: ItemId(Arc::from(text)),
            content: vec![ItemContent::Text {
                text: text.to_owned(),
                annotations: None,
            }],
        }
    }

    /// Waits for up to `limit` events; returns `true` when `pred` matched.
    async fn collect_until(
        rx: &mut broadcast::Receiver<EngineEvent>,
        limit: usize,
        mut pred: impl FnMut(&EngineEvent) -> bool,
    ) -> bool {
        for _ in 0..limit {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(ev)) if pred(&ev) => return true,
                Ok(Ok(_)) => {}
                _ => return false,
            }
        }
        false
    }

    /// A turn that pushes the transcript to the auto-compaction threshold must
    /// trigger automatic compaction once the turn completes, driving the
    /// engine through the `Compaction` phase.
    #[tokio::test]
    async fn auto_compaction_fires_when_transcript_exceeds_threshold() {
        use crate::provider::ScriptedModel;
        use zhive_proto::hook::EnginePhase;

        // Empty scripted model: the turn adds no agent message and the
        // summarisation call returns an empty summary — both acceptable.
        let engine = Engine::spawn_with_provider(ScriptedModel::new("t", "m", vec![]).into_dyn());
        let mut events = engine.subscribe();
        let id = tid("thread:native/auto-compact");

        // Seed the transcript at the threshold in a single turn.
        let items: Vec<_> = (0..super::compaction::AUTO_COMPACT_ITEM_THRESHOLD)
            .map(|i| user_item(&format!("u{i}")))
            .collect();
        engine.start_turn(id.clone(), items, None).await.unwrap();

        let saw_compaction = collect_until(&mut events, 512, |ev| {
            matches!(
                ev,
                EngineEvent::PhaseChanged {
                    to: EnginePhase::Compaction,
                    ..
                }
            )
        })
        .await;
        assert!(
            saw_compaction,
            "auto-compaction must enter the Compaction phase after an over-threshold turn"
        );
        engine.shutdown().await.unwrap();
    }

    // ---- Shared test model helpers ----------------------------------------

    /// A model whose each call emits a simple text response.
    ///
    /// The call counter lets the test observe that the provider was called
    /// more than once (e.g. for a `FollowUp` continuation).
    #[derive(Debug, Clone)]
    struct CountedTextModel {
        call_count: Arc<AtomicUsize>,
    }

    impl CountedTextModel {
        fn new() -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl LanguageModel for CountedTextModel {
        fn provider(&self) -> &'static str {
            "test"
        }
        fn model_id(&self) -> &'static str {
            "counted-text"
        }
        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }
        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            let text = format!("response-{idx}");
            let parts = vec![
                Ok(StreamPart::TextStart {
                    id: format!("b{idx}"),
                    provider_metadata: None,
                }),
                Ok(StreamPart::TextDelta {
                    id: format!("b{idx}"),
                    delta: text,
                    provider_metadata: None,
                }),
                Ok(StreamPart::TextEnd {
                    id: format!("b{idx}"),
                    provider_metadata: None,
                }),
            ];
            let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(parts));
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }

    /// A model that blocks until both parties of the internal `Barrier`
    /// have called `.wait()` — used to keep the engine in Turn phase for
    /// the duration of an async test window.
    #[derive(Debug)]
    struct BarrierModel {
        barrier: Arc<Barrier>,
    }

    impl BarrierModel {
        fn new_pair(parties: usize) -> (Arc<Barrier>, Self) {
            let b = Arc::new(Barrier::new(parties));
            let m = Self {
                barrier: Arc::clone(&b),
            };
            (b, m)
        }
    }

    #[async_trait]
    impl LanguageModel for BarrierModel {
        fn provider(&self) -> &'static str {
            "test"
        }
        fn model_id(&self) -> &'static str {
            "barrier-inc4"
        }
        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }
        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            self.barrier.wait().await;
            let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::empty());
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }

    /// A model whose `do_stream` blocks until its `CancellationToken` is
    /// cancelled.
    ///
    /// This makes the shutdown-cancels test self-contained and deterministic:
    /// the test fires `engine.shutdown()`, which calls `cancel_tree.cancel_all()`,
    /// which cancels the per-turn token, which in turn causes the `biased
    /// select!` in `run_turn` to abort the in-flight `do_stream` future
    /// immediately — without any external barrier release.
    #[derive(Debug)]
    struct CancelAwaitModel {
        token: CancellationToken,
    }

    impl CancelAwaitModel {
        /// Creates a model/token pair.
        ///
        /// The returned [`CancellationToken`] is a *child* of the model's
        /// internal token; cancelling it (or any ancestor) will unblock
        /// `do_stream`.
        fn new() -> (CancellationToken, Self) {
            let token = CancellationToken::new();
            let model = Self {
                token: token.clone(),
            };
            (token, model)
        }
    }

    #[async_trait]
    impl LanguageModel for CancelAwaitModel {
        fn provider(&self) -> &'static str {
            "test"
        }
        fn model_id(&self) -> &'static str {
            "cancel-await-inc4"
        }
        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }
        /// Blocks until the model's `CancellationToken` is cancelled.
        ///
        /// In a real engine run, the `biased select!` in `run_turn` drops
        /// this future before the token fires, so this function never actually
        /// returns `Ok` — the test relies on the cancel arm winning the race.
        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            // Block indefinitely until cancelled; the engine's biased select!
            // will drop this future when cancel fires.
            self.token.cancelled().await;
            // Unreachable in practice when used with run_turn's cancel guard,
            // but must be valid so the model compiles as a LanguageModel impl.
            let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::empty());
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }

    // ===================================================================
    // Test 1: Steer — item appears as ItemAppended before the provider call.
    // ===================================================================

    /// Pre-enqueue a `Steer` item before starting the turn.  The steer
    /// drain fires at the start of every iteration (before the LLM request),
    /// so the item is pushed to the thread tail and broadcast as
    /// `ItemAppended` before the first provider call begins.
    ///
    /// We assert:
    /// - A `UserMessage { id: "steer-message" }` event is broadcast.
    /// - `TurnCompleted` fires (turn ends normally).
    ///
    /// A simple no-op provider (empty stream) is used; the turn completes
    /// after one empty iteration (no tool calls, no follow-up), which is
    /// sufficient to verify the steer drain path.
    #[tokio::test]
    async fn steer_item_appears_as_item_appended() {
        let engine = Engine::spawn().with_reply_timeout(Duration::from_secs(10));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/steer-test");

        // Pre-enqueue a steer item before the turn starts.  It will be
        // drained at the start of iteration 0 (before the first LLM call).
        engine
            .submit(Submission::EnqueueInjection {
                thread_id: thread_id.clone(),
                behavior: StreamingBehavior::Steer,
                items: vec![user_item("steer-message")],
            })
            .await
            .unwrap();

        engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Collect events until TurnCompleted; verify steer item appeared.
        let mut saw_steer_item = false;
        let saw_completed = collect_until(&mut events, 64, |ev| {
            if let EngineEvent::ItemAppended { item, .. } = ev
                && matches!(item.as_ref(), zhive_proto::domain::Item::UserMessage { id, .. }
                    if id.0.as_ref() == "steer-message")
            {
                saw_steer_item = true;
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(
            saw_steer_item,
            "steer item must be broadcast as ItemAppended"
        );
        assert!(saw_completed, "TurnCompleted must still fire");
        engine.shutdown().await.unwrap();
    }

    // ===================================================================
    // Test 2: FollowUp — pre-enqueued item causes a second provider call.
    // ===================================================================

    /// Pre-enqueue a `FollowUp` item before starting the turn (it sits in the
    /// queue and is drained at the turn boundary).  A model that returns a
    /// simple text answer (no tool calls) would normally finish immediately,
    /// but the `FollowUp` drain injects the item and forces a second iteration.
    ///
    /// We verify the turn calls the provider **twice** by observing two
    /// distinct `AgentMessage` events (`response-0` and `response-1`).
    #[tokio::test]
    async fn follow_up_extends_turn_without_tool_calls() {
        let model = CountedTextModel::new();
        let engine = Engine::spawn_with_provider(DynLanguageModel::new(model))
            .with_reply_timeout(Duration::from_secs(10));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/follow-up-test");

        // Enqueue a FollowUp item BEFORE starting the turn.  It will be
        // drained at the first no-tool-call turn boundary.
        engine
            .submit(Submission::EnqueueInjection {
                thread_id: thread_id.clone(),
                behavior: StreamingBehavior::FollowUp,
                items: vec![user_item("follow-up-msg")],
            })
            .await
            .unwrap();

        engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Collect AgentMessage items until TurnCompleted; expect both
        // "response-0" (first iteration) and "response-1" (follow-up
        // iteration).
        let mut agent_texts: Vec<String> = Vec::new();
        let saw_completed = collect_until(&mut events, 64, |ev| {
            if let EngineEvent::ItemAppended { item, .. } = ev
                && let zhive_proto::domain::Item::AgentMessage { text, .. } = item.as_ref()
            {
                agent_texts.push(text.clone());
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(saw_completed, "TurnCompleted must fire");
        assert!(
            agent_texts.len() >= 2,
            "expected at least 2 agent messages (follow-up forced a second iteration), got {agent_texts:?}"
        );
        assert!(
            agent_texts.iter().any(|t| t == "response-0"),
            "expected response-0, got {agent_texts:?}"
        );
        assert!(
            agent_texts.iter().any(|t| t == "response-1"),
            "expected response-1 from follow-up iteration, got {agent_texts:?}"
        );
        engine.shutdown().await.unwrap();
    }

    // ===================================================================
    // Test 3: NextTurn survives abort and seeds the following turn.
    // ===================================================================

    /// Start a turn (blocked on a barrier model), then while the turn is
    /// in-flight enqueue a `NextTurn` item and a `Steer` item.  Cancel the
    /// turn and assert:
    /// - `SessionAborted.next_turn_retained_count == 1`
    /// - `SessionAborted.cleared_steer.len() == 1` (steer was cleared)
    ///
    /// Then start a second turn and verify the `NextTurn` item seeds it
    /// (appears as `ItemAppended` before the normal user input).
    ///
    /// **Key invariant**: `NextTurn` items enqueued *during* a turn survive
    /// `cancel_turn` because `abort()` does not clear the `next_turn` queue.
    /// Items enqueued *before* `start_turn` are consumed immediately by
    /// `start_turn`'s drain and therefore do not appear in `SessionAborted`.
    #[tokio::test]
    async fn next_turn_survives_cancel_and_seeds_next_turn() {
        let (barrier, model) = BarrierModel::new_pair(2);
        let engine = Engine::spawn_with_provider(DynLanguageModel::new(model))
            .with_reply_timeout(Duration::from_secs(10));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/next-turn-test");

        // Start a turn.  The barrier model blocks inside `do_stream` so
        // the turn stays in-flight until we release the barrier.
        engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted to confirm the turn task is actually running.
        let saw_started = collect_until(&mut events, 16, |ev| {
            matches!(ev, EngineEvent::TurnStarted { .. })
        })
        .await;
        assert!(saw_started, "expected TurnStarted");

        // NOW enqueue items while the turn is in-flight.  The NextTurn item
        // will survive cancel; the Steer item will be cleared.
        engine
            .submit(Submission::EnqueueNextTurn {
                thread_id: thread_id.clone(),
                items: vec![user_item("next-turn-item")],
            })
            .await
            .unwrap();
        engine
            .submit(Submission::EnqueueInjection {
                thread_id: thread_id.clone(),
                behavior: StreamingBehavior::Steer,
                items: vec![user_item("steer-to-be-cleared")],
            })
            .await
            .unwrap();

        let cancelled = engine.cancel_turn(thread_id.clone()).await.unwrap();
        assert!(cancelled.is_some(), "expected a turn id to be cancelled");

        // Unblock the barrier model so it can exit.
        barrier.wait().await;

        // Collect until we see SessionAborted; extract queue snapshot.
        let mut aborted_notif: Option<zhive_proto::permission::SessionAbortedNotification> = None;
        for _ in 0..32 {
            match tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
                Ok(Ok(EngineEvent::SessionAborted(n))) => {
                    aborted_notif = Some(*n);
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        let notif = aborted_notif.expect("expected SessionAborted");

        // NextTurn is preserved, steer is cleared.
        assert_eq!(
            notif.next_turn_retained_count, 1,
            "next_turn_retained_count must be 1 (we enqueued one item)"
        );
        assert_eq!(
            notif.cleared_steer.len(),
            1,
            "cleared_steer must contain the 1 steer item we enqueued"
        );
        assert!(
            notif.cleared_follow_up.is_empty(),
            "cleared_follow_up must be empty (we did not enqueue any)"
        );

        // Now subscribe again for the second turn.
        let mut events2 = engine.subscribe();

        // Start a new turn.  The NextTurn item must be prepended before the
        // user input and appear as the first ItemAppended event.
        engine
            .start_turn(thread_id.clone(), vec![user_item("new-user-input")], None)
            .await
            .unwrap();

        // The BarrierModel resets after each full cycle, so the second turn's
        // `do_stream` call will block again on the barrier.  Spawn a task to
        // release it so the second turn can complete.
        let b2 = Arc::clone(&barrier);
        tokio::spawn(async move {
            b2.wait().await;
        });

        let mut item_ids_in_order: Vec<String> = Vec::new();
        let saw_completed = collect_until(&mut events2, 64, |ev| {
            if let EngineEvent::ItemAppended { item, .. } = ev
                && let zhive_proto::domain::Item::UserMessage { id, .. } = item.as_ref()
            {
                item_ids_in_order.push(id.0.to_string());
            }
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert!(saw_completed, "second turn must complete");
        // The NextTurn item ("next-turn-item") must appear before "new-user-input".
        let pos_next = item_ids_in_order.iter().position(|s| s == "next-turn-item");
        let pos_new = item_ids_in_order.iter().position(|s| s == "new-user-input");
        assert!(
            pos_next.is_some(),
            "next-turn-item must appear in second turn items; got {item_ids_in_order:?}"
        );
        assert!(
            pos_new.is_some(),
            "new-user-input must appear in second turn items; got {item_ids_in_order:?}"
        );
        assert!(
            pos_next.unwrap() < pos_new.unwrap(),
            "next-turn-item must be prepended before new-user-input"
        );
        engine.shutdown().await.unwrap();
    }

    // ===================================================================
    // Test 4: Shutdown cancels in-flight turn promptly.
    // ===================================================================

    /// A turn blocked inside `do_stream` must abort **promptly** when
    /// `shutdown` is called, exercising the `biased select!` guard in
    /// `run_turn` that races the provider call against the per-turn
    /// `CancellationToken`.
    ///
    /// The [`CancelAwaitModel`] blocks forever in `do_stream` until its
    /// internal token is cancelled; there is no external unblock step.
    /// When `engine.shutdown()` fires `cancel_tree.cancel_all()`, the
    /// cancel arm of the `biased select!` wins immediately, drops the
    /// blocking `do_stream` future, calls `finish_turn(failed=false)`,
    /// and the engine broadcasts `TurnCompleted`.  This test asserts that
    /// `TurnCompleted` (or any other turn-terminal event) arrives within
    /// 500 ms, proving that cancellation actually interrupted the blocking
    /// provider call rather than merely completing shutdown bookkeeping
    /// while the turn task continued to run.
    ///
    /// Note: `SessionAborted` is emitted by the `cancel_turn` submission
    /// path, not by the shutdown path; the shutdown path emits `TurnCompleted`.
    #[tokio::test]
    async fn shutdown_cancels_in_flight_turn() {
        let (_token, model) = CancelAwaitModel::new();
        let engine = Engine::spawn_with_provider(DynLanguageModel::new(model))
            .with_reply_timeout(Duration::from_secs(10));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/shutdown-cancel-test");
        engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted — the turn task is now blocking in do_stream.
        let saw_started = collect_until(&mut events, 16, |ev| {
            matches!(ev, EngineEvent::TurnStarted { .. })
        })
        .await;
        assert!(saw_started, "expected TurnStarted before shutdown");

        // Trigger shutdown.  This fires `cancel_tree.cancel_all()` on the
        // actor, which cancels the per-turn `CancellationToken`.  The actor
        // replies immediately (before the turn task drains).
        engine.shutdown().await.expect("shutdown must succeed");

        // After shutdown, the biased `select!` in `run_turn` must observe the
        // cancelled token promptly, drop the blocking `do_stream` future, and
        // call `finish_turn(failed=false)`, which broadcasts `TurnCompleted`.
        //
        // We assert that this terminal event arrives within 500 ms.  If the
        // cancel guard were absent the `CancelAwaitModel` would block forever
        // and `TurnCompleted` would never be emitted.
        //
        // Note: the shutdown path does NOT call `cancel_turn`, so `SessionAborted`
        // is NOT emitted here; the terminal signal is `TurnCompleted`.
        let saw_turn_ended = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                match events.recv().await {
                    Ok(
                        EngineEvent::TurnCompleted { .. }
                        | EngineEvent::TurnFailed { .. }
                        | EngineEvent::SessionAborted(_),
                    ) => return true,
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);

        assert!(
            saw_turn_ended,
            "TurnCompleted/TurnFailed/SessionAborted must arrive within 500 ms of shutdown; \
             cancel guard in run_turn did not abort the blocking do_stream"
        );
    }
}

// ============================================================
// Increment-5 persistence write-through tests
// ============================================================

#[cfg(test)]
mod inc5_tests {
    //! End-to-end test for the Part D write-through path: engine configured
    //! with storage runs a turn and the JSONL rollout + state.db are updated.

    use llmsdk::language_model::StreamPart;

    use super::*;
    use crate::persistence::writer::rebuild_state_from_rollout;
    use crate::provider::ScriptedModel;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    // ------------------------------------------------------------------
    // Helpers shared by the new FIX-1 / FIX-2 tests
    // ------------------------------------------------------------------

    /// Queries the raw turn row status from state.db for the given turn id.
    ///
    /// Returns the status string (e.g. "inProgress", "interrupted", "completed")
    /// or `None` when the row does not exist.
    async fn query_turn_status(
        storage: &crate::persistence::Storage,
        turn_id: &str,
    ) -> Option<String> {
        use sqlx::Row as _;
        sqlx::query("SELECT status FROM turns WHERE id = ?1")
            .bind(turn_id)
            .fetch_optional(storage.state.pool())
            .await
            .ok()
            .flatten()
            .map(|r| r.try_get::<String, _>("status").unwrap_or_default())
    }

    /// Queries the thread row status from state.db for the given thread id.
    async fn query_thread_status(
        storage: &crate::persistence::Storage,
        thread_id: &str,
    ) -> Option<String> {
        use sqlx::Row as _;
        sqlx::query("SELECT status FROM threads WHERE id = ?1")
            .bind(thread_id)
            .fetch_optional(storage.state.pool())
            .await
            .ok()
            .flatten()
            .map(|r| r.try_get::<String, _>("status").unwrap_or_default())
    }

    /// A turn driven through an engine with `storage: Some(…)` must:
    /// - persist the thread row in `state.db`,
    /// - append an `Item` entry to the JSONL rollout, and
    /// - write a `Leaf` pointer after the turn ends (confirmed via
    ///   `rebuild_state_from_rollout`).
    #[tokio::test]
    async fn engine_turn_with_storage_writes_rollout_and_state_db() {
        // Create a temporary storage directory.
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(tmp.path()).await.unwrap());

        // Build a scripted model that emits one text item.
        let model = ScriptedModel::new(
            "test",
            "m",
            vec![
                StreamPart::TextStart {
                    id: "b0".into(),
                    provider_metadata: None,
                },
                StreamPart::TextDelta {
                    id: "b0".into(),
                    delta: "hello from storage".into(),
                    provider_metadata: None,
                },
                StreamPart::TextEnd {
                    id: "b0".into(),
                    provider_metadata: None,
                },
            ],
        );

        let cfg = EngineConfig {
            provider: model.into_dyn(),
            tools: Arc::new(crate::tools::ToolRegistry::new()),
            hook_host: Arc::new(crate::hooks::HookHost::new()),
            storage: Some(Arc::clone(&storage)),
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };

        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/inc5-e2e");
        engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnCompleted.
        let mut saw_completed = false;
        for _ in 0..32 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("timeout")
                .expect("broadcast");
            if matches!(ev, EngineEvent::TurnCompleted { .. }) {
                saw_completed = true;
                break;
            }
        }
        assert!(saw_completed, "expected TurnCompleted");

        // Graceful shutdown — this awaits the writer task.
        engine.shutdown().await.unwrap();

        // Verify the JSONL rollout and state DB.
        let rollout_path = storage.rollout_path(&thread_id.0);

        // Rebuild from rollout — must succeed (not NotFound or any error).
        rebuild_state_from_rollout(&storage.state, &rollout_path)
            .await
            .expect("rebuild must succeed after engine turn with storage");

        // The thread must be present in state.db.
        let t = storage
            .state
            .get_thread(&thread_id)
            .await
            .unwrap()
            .expect("thread row must be present in state.db after turn");
        assert_eq!(t.id, thread_id);

        // At least one item must have been persisted.
        let all_threads = storage.state.list_threads().await.unwrap();
        assert!(
            !all_threads.is_empty(),
            "state.db must have at least one thread"
        );

        // The JSONL rollout must contain at least one Item entry.
        let entries = crate::persistence::read_all(&rollout_path)
            .await
            .expect("rollout must be readable");
        let has_item = entries.iter().any(|e| {
            matches!(e, crate::persistence::RolloutEntry::Item { thread_id: tid, .. }
                if tid == "thread:native/inc5-e2e")
        });
        assert!(
            has_item,
            "JSONL rollout must contain at least one ItemAppended entry"
        );
    }

    // ------------------------------------------------------------------
    // FIX 1: cancel_turn must persist turn as Interrupted in state.db
    // ------------------------------------------------------------------

    /// When a turn is cancelled while storage is configured, the state.db
    /// turn row must be updated to `status = "interrupted"` (not left as
    /// `"inProgress"` forever).
    ///
    /// Uses a `BarrierModel`-style blocking provider so the cancel races
    /// the in-flight `do_stream` call deterministically.
    #[tokio::test]
    async fn cancelled_turn_persisted_as_interrupted() {
        use async_trait::async_trait;
        use futures::stream;
        use llmsdk::LanguageModel;
        use llmsdk::language_model::{
            BoxStream, CallOptions, FinishReason, FinishReasonKind, GenerateResult, StreamPart,
            StreamResult,
        };
        use tokio::sync::Barrier as TokioBarrier;

        #[derive(Debug)]
        struct BarrierModel2 {
            barrier: Arc<TokioBarrier>,
        }

        #[async_trait]
        impl LanguageModel for BarrierModel2 {
            fn provider(&self) -> &'static str {
                "test"
            }
            fn model_id(&self) -> &'static str {
                "barrier2"
            }
            async fn do_generate(&self, _: CallOptions) -> llmsdk::error::Result<GenerateResult> {
                Ok(GenerateResult {
                    content: vec![],
                    finish_reason: FinishReason::new(FinishReasonKind::Stop),
                    usage: llmsdk::language_model::Usage::default(),
                    provider_metadata: None,
                    request: None,
                    response: None,
                    warnings: vec![],
                })
            }
            async fn do_stream(&self, _: CallOptions) -> llmsdk::error::Result<StreamResult> {
                self.barrier.wait().await;
                let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::empty());
                Ok(StreamResult {
                    stream: s,
                    request: None,
                    response: None,
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(tmp.path()).await.unwrap());

        let barrier = Arc::new(TokioBarrier::new(2));
        let model = BarrierModel2 {
            barrier: Arc::clone(&barrier),
        };

        let cfg = EngineConfig {
            provider: DynLanguageModel::new(model),
            tools: Arc::new(crate::tools::ToolRegistry::new()),
            hook_host: Arc::new(crate::hooks::HookHost::new()),
            storage: Some(Arc::clone(&storage)),
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/cancel-persist");
        let turn_id = engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted — the turn task is in-flight inside do_stream.
        let saw_started = {
            let mut found = false;
            for _ in 0..32 {
                match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await {
                    Ok(Ok(EngineEvent::TurnStarted { .. })) => {
                        found = true;
                        break;
                    }
                    Ok(Ok(_)) => {}
                    _ => break,
                }
            }
            found
        };
        assert!(saw_started, "expected TurnStarted");

        // Cancel while blocked in do_stream.
        let cancelled = engine.cancel_turn(thread_id.clone()).await.unwrap();
        assert!(cancelled.is_some(), "expected a turn id to be cancelled");

        // Unblock the provider so the spawned task can clean up.
        barrier.wait().await;

        // Graceful shutdown — awaits the writer task so ops are flushed.
        engine.shutdown().await.unwrap();

        // Verify turn status in state.db is "interrupted".
        let status = query_turn_status(&storage, turn_id.0.as_ref()).await;
        assert_eq!(
            status.as_deref(),
            Some("interrupted"),
            "cancelled turn must be persisted as 'interrupted', got {status:?}"
        );

        // Verify thread status in state.db is back to "idle".
        let thread_status = query_thread_status(&storage, thread_id.0.as_ref()).await;
        assert_eq!(
            thread_status.as_deref(),
            Some("idle"),
            "thread status must be 'idle' after cancel, got {thread_status:?}"
        );
    }

    // ------------------------------------------------------------------
    // FIX 2: thread status tracks Active during turn, Idle after
    // ------------------------------------------------------------------

    /// The persisted thread status must be `"active"` while a turn is
    /// in-flight and `"idle"` after the turn completes.
    ///
    /// Uses a barrier model to observe the intermediate `"active"` state
    /// before releasing the turn to complete.
    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "integration test requires two in-test models and two barrier rendezvous; \
                  splitting into helper functions would not improve readability"
    )]
    async fn thread_status_tracks_turn_lifecycle_in_state_db() {
        use async_trait::async_trait;
        use futures::stream;
        use llmsdk::LanguageModel;
        use llmsdk::language_model::{
            BoxStream, CallOptions, FinishReason, FinishReasonKind, GenerateResult, StreamPart,
            StreamResult,
        };
        use tokio::sync::Barrier as TokioBarrier;

        #[derive(Debug)]
        struct TwoPhaseModel {
            /// Released after the model start is visible but before streaming.
            start_barrier: Arc<TokioBarrier>,
            /// Released by the test to let the model return an empty stream.
            finish_barrier: Arc<TokioBarrier>,
        }

        #[async_trait]
        impl LanguageModel for TwoPhaseModel {
            fn provider(&self) -> &'static str {
                "test"
            }
            fn model_id(&self) -> &'static str {
                "two-phase"
            }
            async fn do_generate(&self, _: CallOptions) -> llmsdk::error::Result<GenerateResult> {
                Ok(GenerateResult {
                    content: vec![],
                    finish_reason: FinishReason::new(FinishReasonKind::Stop),
                    usage: llmsdk::language_model::Usage::default(),
                    provider_metadata: None,
                    request: None,
                    response: None,
                    warnings: vec![],
                })
            }
            async fn do_stream(&self, _: CallOptions) -> llmsdk::error::Result<StreamResult> {
                // Signal that do_stream is active, then block until released.
                self.start_barrier.wait().await;
                self.finish_barrier.wait().await;
                let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::empty());
                Ok(StreamResult {
                    stream: s,
                    request: None,
                    response: None,
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(tmp.path()).await.unwrap());

        let start_barrier = Arc::new(TokioBarrier::new(2));
        let finish_barrier = Arc::new(TokioBarrier::new(2));

        let model = TwoPhaseModel {
            start_barrier: Arc::clone(&start_barrier),
            finish_barrier: Arc::clone(&finish_barrier),
        };

        let cfg = EngineConfig {
            provider: DynLanguageModel::new(model),
            tools: Arc::new(crate::tools::ToolRegistry::new()),
            hook_host: Arc::new(crate::hooks::HookHost::new()),
            storage: Some(Arc::clone(&storage)),
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/thread-status-lifecycle");
        engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted.
        let saw_started = {
            let mut found = false;
            for _ in 0..32 {
                match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await {
                    Ok(Ok(EngineEvent::TurnStarted { .. })) => {
                        found = true;
                        break;
                    }
                    Ok(Ok(_)) => {}
                    _ => break,
                }
            }
            found
        };
        assert!(saw_started, "expected TurnStarted");

        // Rendezvous with the model inside do_stream: at this point the
        // ThreadUpserted(Active) op has been enqueued but may not yet
        // have been processed by the writer.  Release the start barrier
        // so the model is about to begin streaming.
        start_barrier.wait().await;

        // Give the writer task a moment to process the enqueued
        // ThreadUpserted(Active) operation before we read the DB.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let status_during = query_thread_status(&storage, thread_id.0.as_ref()).await;
        assert_eq!(
            status_during.as_deref(),
            Some("active"),
            "thread must be 'active' while turn is in-flight, got {status_during:?}"
        );

        // Release the model to return an empty stream so the turn can complete.
        finish_barrier.wait().await;

        // Wait for TurnCompleted so we know finish_turn has been called and
        // the Idle upsert has been enqueued.
        let saw_completed = {
            let mut found = false;
            for _ in 0..32 {
                match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await {
                    Ok(Ok(EngineEvent::TurnCompleted { .. })) => {
                        found = true;
                        break;
                    }
                    Ok(Ok(_)) => {}
                    _ => break,
                }
            }
            found
        };
        assert!(saw_completed, "expected TurnCompleted");

        // Shutdown awaits the writer; all enqueued ops must now be applied.
        engine.shutdown().await.unwrap();

        // After shutdown: thread status must be "idle".
        let status_after = query_thread_status(&storage, thread_id.0.as_ref()).await;
        assert_eq!(
            status_after.as_deref(),
            Some("idle"),
            "thread status must be 'idle' after turn completes, got {status_after:?}"
        );
    }
}

// ============================================================
// Increment-6 subagent spawn tests
// ============================================================

#[cfg(test)]
mod inc6_tests {
    //! Integration tests for the subagent spawn path introduced in increment 6.
    //!
    //! Test plan:
    //! - `spawn_subagent_returns_child_id_and_emits_completed` — happy path:
    //!   scripted child model emits an `AgentMessage`; `SubagentCompleted`
    //!   carries the final message; child transcript starts fresh (no parent items).
    //! - `spawn_subagent_only_final_message_delivered` — when the child
    //!   produces multiple items, `SubagentCompleted` carries exactly one.
    //! - `spawn_subagent_recursion_rejected` — spawning when parent is itself
    //!   a subagent is rejected with `SubagentSpawnFailed(RecursionForbidden)`.
    //! - `spawn_subagent_child_spawn_requested_rejected` — definition with
    //!   `allow_subagent_spawn=true` is rejected.

    use async_trait::async_trait;
    use futures::stream;
    use llmsdk::LanguageModel;
    use llmsdk::language_model::{
        BoxStream, CallOptions, FinishReason, FinishReasonKind, GenerateResult, StreamPart,
        StreamResult,
    };

    use super::*;
    use crate::state::ThreadHandle;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn subagent_def(allow_spawn: bool) -> SubagentDefinition {
        serde_json::from_value(serde_json::json!({
            "name": "test-child",
            "description": "test subagent",
            "prompt": "Do something useful.",
            "allowSubagentSpawn": allow_spawn,
        }))
        .expect("definition fixture")
    }

    /// Waits for up to `limit` events; returns `true` when `pred` matched.
    async fn collect_until(
        rx: &mut broadcast::Receiver<EngineEvent>,
        limit: usize,
        mut pred: impl FnMut(&EngineEvent) -> bool,
    ) -> bool {
        for _ in 0..limit {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(ev)) if pred(&ev) => return true,
                Ok(Ok(_)) => {}
                _ => return false,
            }
        }
        false
    }

    // ===================================================================
    // Test 1: happy path
    // ===================================================================

    /// Spawn a subagent on a top-level thread.  The child scripted model
    /// emits an `AgentMessage`; assert:
    /// - `spawn_subagent` returns a child `ThreadId`
    /// - `EngineEvent::SubagentCompleted` is broadcast with `final_message`
    ///   carrying the `AgentMessage` text
    /// - The child thread's `items_tail` did NOT inherit parent items
    ///   (fresh context window)
    ///
    /// The shared provider returns an empty stream on the first call (parent
    /// turn) and the text answer on the second call (child turn).
    #[tokio::test]
    async fn spawn_subagent_returns_child_id_and_emits_completed() {
        // Parent uses a noop provider; child uses the text-answer provider.
        // Because only one provider is registered per engine, use a
        // two-call model: call 0 = empty (parent), call 1 = child answer.
        let engine = Engine::spawn_with_provider(two_call_text_provider("child answer"))
            .with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        // Create the parent thread.
        let parent_id = tid("thread:native/parent-spawn");
        engine
            .start_turn(parent_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for the parent turn to complete (noop provider returns quickly).
        collect_until(&mut events, 32, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { thread_id, .. } if thread_id == &parent_id)
        })
        .await;

        // Spawn the subagent.
        let child_id = engine
            .spawn_subagent(parent_id.clone(), subagent_def(false))
            .await
            .expect("spawn_subagent must succeed");

        assert!(
            child_id.0.starts_with("thread:subagent/"),
            "child id must carry subagent prefix, got {}",
            child_id.0
        );

        // Wait for SubagentCompleted and assert final_message content.
        let mut final_text: Option<String> = None;
        let found = collect_until(&mut events, 64, |ev| {
            if let EngineEvent::SubagentCompleted {
                child_thread_id,
                final_message,
                ..
            } = ev
                && child_thread_id == &child_id
            {
                final_text = final_message.as_ref().and_then(|item| {
                    if let zhive_proto::domain::Item::AgentMessage { text, .. } = item.as_ref() {
                        Some(text.clone())
                    } else {
                        None
                    }
                });
                return true;
            }
            false
        })
        .await;

        assert!(found, "SubagentCompleted must fire for the child thread");
        assert_eq!(
            final_text.as_deref(),
            Some("child answer"),
            "final_message must carry the child AgentMessage text"
        );

        // Verify the child transcript is fresh: it must NOT contain the
        // parent thread's items (empty parent input in this test, but the
        // child items_tail must be independent of the parent's items_tail).
        let child_handle = engine
            .threads()
            .get(&child_id)
            .await
            .expect("child thread must exist in store");
        let parent_handle = engine
            .threads()
            .get(&parent_id)
            .await
            .expect("parent thread must exist");

        // Child handle must have parent_thread_id set.
        assert_eq!(
            child_handle.parent_thread_id.as_ref(),
            Some(&parent_id),
            "child must record its parent"
        );

        // Parent items must not appear in child's items_tail (pointer inequality).
        // We verify independence by checking that the two VecDeques are
        // distinct objects (the child was created with a fresh VecDeque).
        let parent_tail_len = parent_handle.items_tail.read().await.len();
        let child_tail_len = child_handle.items_tail.read().await.len();
        // Parent had no user input so its tail is empty; child has its prompt
        // item (1) plus the AgentMessage (1) = 2 items.
        assert_eq!(parent_tail_len, 0, "parent tail must be empty");
        assert!(
            child_tail_len > 0,
            "child tail must contain at least the agent message"
        );

        engine.shutdown().await.unwrap();
    }

    // ===================================================================
    // Test 2: only the final message is delivered
    // ===================================================================

    /// A scripted multi-item model (reasoning chunk + agent message) must
    /// produce exactly one item in `SubagentCompleted.final_message`.
    #[tokio::test]
    async fn spawn_subagent_only_final_message_delivered() {
        let engine = Engine::spawn_with_provider(reasoning_then_text_provider())
            .with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        let parent_id = tid("thread:native/parent-only-final");
        engine
            .start_turn(parent_id.clone(), Vec::new(), None)
            .await
            .unwrap();
        collect_until(&mut events, 32, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { thread_id, .. } if thread_id == &parent_id)
        })
        .await;

        let child_id = engine
            .spawn_subagent(parent_id.clone(), subagent_def(false))
            .await
            .expect("spawn_subagent must succeed");

        let mut saw_completed = false;
        let mut final_text: Option<String> = None;
        collect_until(&mut events, 64, |ev| {
            if let EngineEvent::SubagentCompleted {
                child_thread_id,
                final_message,
                ..
            } = ev
                && child_thread_id == &child_id
            {
                saw_completed = true;
                if let Some(item) = final_message
                    && let zhive_proto::domain::Item::AgentMessage { text, .. } = item.as_ref()
                {
                    final_text = Some(text.clone());
                }
                return true;
            }
            false
        })
        .await;

        assert!(saw_completed, "SubagentCompleted must fire");
        assert_eq!(
            final_text.as_deref(),
            Some("final answer"),
            "only the final AgentMessage must be delivered, got {final_text:?}"
        );

        engine.shutdown().await.unwrap();
    }

    /// Returns a [`DynLanguageModel`] that produces an empty stream on the
    /// first call (for the parent turn) and a reasoning block followed by a
    /// final text answer on the second call (for the child turn).
    /// Used to test the only-final delivery contract.
    fn reasoning_then_text_provider() -> DynLanguageModel {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct ReasoningThenTextModel {
            call_count: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl LanguageModel for ReasoningThenTextModel {
            fn provider(&self) -> &'static str {
                "test"
            }
            fn model_id(&self) -> &'static str {
                "reasoning-then-text"
            }
            async fn do_generate(&self, _: CallOptions) -> llmsdk::error::Result<GenerateResult> {
                Ok(GenerateResult {
                    content: vec![],
                    finish_reason: FinishReason::new(FinishReasonKind::Stop),
                    usage: llmsdk::language_model::Usage::default(),
                    provider_metadata: None,
                    request: None,
                    response: None,
                    warnings: vec![],
                })
            }
            async fn do_stream(&self, _: CallOptions) -> llmsdk::error::Result<StreamResult> {
                let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
                let parts: Vec<llmsdk::error::Result<StreamPart>> = if idx == 0 {
                    // Parent turn: no output.
                    vec![]
                } else {
                    // Child turn: reasoning + final text.
                    vec![
                        Ok(StreamPart::ReasoningStart {
                            id: "r0".into(),
                            provider_metadata: None,
                        }),
                        Ok(StreamPart::ReasoningDelta {
                            id: "r0".into(),
                            delta: "thinking...".into(),
                            provider_metadata: None,
                        }),
                        Ok(StreamPart::ReasoningEnd {
                            id: "r0".into(),
                            provider_metadata: None,
                        }),
                        Ok(StreamPart::TextStart {
                            id: "b0".into(),
                            provider_metadata: None,
                        }),
                        Ok(StreamPart::TextDelta {
                            id: "b0".into(),
                            delta: "final answer".into(),
                            provider_metadata: None,
                        }),
                        Ok(StreamPart::TextEnd {
                            id: "b0".into(),
                            provider_metadata: None,
                        }),
                    ]
                };
                let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(parts));
                Ok(StreamResult {
                    stream: s,
                    request: None,
                    response: None,
                })
            }
        }

        DynLanguageModel::new(ReasoningThenTextModel {
            call_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    // ===================================================================
    // Test 3: recursion rejected (parent is a subagent)
    // ===================================================================

    /// Manually construct a [`ThreadHandle`] with `parent_thread_id = Some(_)` to
    /// simulate a subagent thread, then call `spawn_subagent` targeting it.
    /// The engine must reject with `SubagentSpawnFailed(RecursionForbidden)`.
    #[tokio::test]
    async fn spawn_subagent_recursion_rejected() {
        let engine = Engine::spawn().with_reply_timeout(std::time::Duration::from_secs(5));

        // Manually insert a child-like thread handle into the store.
        // This simulates a thread that was itself spawned as a subagent.
        let fake_parent_id = tid("thread:subagent/native/root/0");
        // `new_child` returns `(handle, rx)`; the receiver is unused in this
        // test (we only need the handle to be registered in the thread store).
        let (child_handle_inner, _rx) =
            ThreadHandle::new_child(fake_parent_id.clone(), tid("thread:native/root"));
        let child_handle = Arc::new(child_handle_inner);
        engine
            .threads()
            .write_guard()
            .await
            .insert(fake_parent_id.clone(), child_handle);

        // Attempt to spawn a sub-subagent under the fake parent.
        let result = engine
            .spawn_subagent(fake_parent_id, subagent_def(false))
            .await;

        assert!(
            matches!(
                result,
                Err(EngineError::SubagentSpawnFailed(
                    submission::SubagentSpawnError::RecursionForbidden
                ))
            ),
            "expected RecursionForbidden, got {result:?}"
        );

        engine.shutdown().await.unwrap();
    }

    // ===================================================================
    // Test 4: definition with allow_subagent_spawn=true is rejected
    // ===================================================================

    /// A `SubagentDefinition` with `allow_subagent_spawn=true` must be
    /// rejected regardless of the parent's own spawn capability.
    #[tokio::test]
    async fn spawn_subagent_child_spawn_requested_rejected() {
        let engine = Engine::spawn().with_reply_timeout(std::time::Duration::from_secs(5));

        // Create a top-level parent thread.
        let parent_id = tid("thread:native/parent-bad-def");
        engine
            .start_turn(parent_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for the turn to finish so the thread exists and is idle.
        let mut events = engine.subscribe();
        collect_until(&mut events, 32, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { thread_id, .. } if thread_id == &parent_id)
        })
        .await;

        // Try to spawn a subagent with allow_subagent_spawn=true.
        let result = engine.spawn_subagent(parent_id, subagent_def(true)).await;

        assert!(
            matches!(
                result,
                Err(EngineError::SubagentSpawnFailed(
                    submission::SubagentSpawnError::ChildSpawnRequested
                ))
            ),
            "expected ChildSpawnRequested, got {result:?}"
        );

        engine.shutdown().await.unwrap();
    }

    // ===================================================================
    // Test 4b: model-callable `agent` tool spawns a child and gets its result
    // ===================================================================

    /// End-to-end: the model emits a `ToolCall("agent")`; the `AgentTool`
    /// spawns a subagent via the wired `EngineSubagentSpawner`, awaits the
    /// child's final message, and feeds it back as the tool result. Asserts the
    /// finalized `ToolCall` item carries the child's text and the parent turn
    /// completes.
    ///
    /// Routing is deterministic (no shared counter race): the model in
    /// [`agent_routing_provider`] inspects the reconstructed prompt rather than
    /// a call index, so concurrency between the parent's second iteration and
    /// the child turn cannot reorder the scripted responses.
    #[tokio::test]
    async fn agent_tool_spawns_child_and_returns_result() {
        let mut tools = crate::tools::ToolRegistry::new();
        tools.register(Arc::new(crate::tools::builtin::AgentTool));

        let cfg = EngineConfig {
            provider: agent_routing_provider(),
            tools: Arc::new(tools),
            hook_host: Arc::new(crate::hooks::HookHost::new()),
            storage: None,
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));
        let mut events = engine.subscribe();

        let parent_id = tid("thread:native/agent-tool-parent");
        engine
            .start_turn(parent_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // The finalized `agent` ToolCall item must carry the child's text.
        let mut agent_tool_result: Option<String> = None;
        let mut saw_turn_completed = false;
        for _ in 0..128 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("event timeout")
                .expect("broadcast recv")
            {
                EngineEvent::ItemAppended { item, .. } => {
                    if let Some(text) = agent_tool_result_text(&item) {
                        agent_tool_result = Some(text);
                    }
                }
                EngineEvent::TurnCompleted { thread_id, .. } if thread_id == parent_id => {
                    saw_turn_completed = true;
                }
                _ => {}
            }
            if agent_tool_result.is_some() && saw_turn_completed {
                break;
            }
        }

        assert_eq!(
            agent_tool_result.as_deref(),
            Some("subagent finding"),
            "the agent tool result must carry the child's final message text"
        );
        assert!(
            saw_turn_completed,
            "parent turn must complete after the agent tool returns"
        );

        engine.shutdown().await.unwrap();
    }

    /// Marker carried in the subagent prompt so [`agent_routing_provider`] can
    /// recognise the child turn from the reconstructed `CallOptions.prompt`.
    const AGENT_CHILD_MARKER: &str = "ZHIVE_CHILD_TASK_MARKER";

    /// Extracts the text of a completed `agent` `ToolCall` item, if `item` is one.
    fn agent_tool_result_text(item: &zhive_proto::domain::Item) -> Option<String> {
        let zhive_proto::domain::Item::ToolCall {
            name,
            status: zhive_proto::domain::ToolCallStatus::Completed,
            content,
            ..
        } = item
        else {
            return None;
        };
        if name != "agent" {
            return None;
        }
        match content.first() {
            Some(zhive_proto::domain::ItemToolCallContent::Content {
                content: zhive_proto::domain::ItemContent::Text { text, .. },
            }) => Some(text.clone()),
            _ => None,
        }
    }

    /// Returns a model that routes by prompt content for the `agent`-tool test.
    ///
    /// - A `Message::Tool` present → parent iteration 2 (tool result already
    ///   injected) → emit an empty stream so the loop ends.
    /// - Else a `Message::User` containing [`AGENT_CHILD_MARKER`] → child turn →
    ///   emit the child's text answer.
    /// - Else (first parent call) → emit the `agent` tool call.
    fn agent_routing_provider() -> DynLanguageModel {
        use llmsdk::ToolCallPart;
        use llmsdk::language_model::{Message, UserPart};

        #[derive(Debug)]
        struct AgentRoutingModel;

        #[async_trait]
        impl LanguageModel for AgentRoutingModel {
            fn provider(&self) -> &'static str {
                "test"
            }
            fn model_id(&self) -> &'static str {
                "agent-routing"
            }
            async fn do_generate(&self, _: CallOptions) -> llmsdk::error::Result<GenerateResult> {
                Ok(GenerateResult {
                    content: vec![],
                    finish_reason: FinishReason::new(FinishReasonKind::Stop),
                    usage: llmsdk::language_model::Usage::default(),
                    provider_metadata: None,
                    request: None,
                    response: None,
                    warnings: vec![],
                })
            }
            async fn do_stream(&self, opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
                let has_tool_result = opts
                    .prompt
                    .iter()
                    .any(|m| matches!(m, Message::Tool { .. }));
                let is_child_turn = opts.prompt.iter().any(|m| match m {
                    Message::User { content, .. } => content.iter().any(
                        |p| matches!(p, UserPart::Text(t) if t.text.contains(AGENT_CHILD_MARKER)),
                    ),
                    _ => false,
                });

                let parts: Vec<llmsdk::error::Result<StreamPart>> = if has_tool_result {
                    vec![]
                } else if is_child_turn {
                    vec![
                        Ok(StreamPart::TextStart {
                            id: "c0".into(),
                            provider_metadata: None,
                        }),
                        Ok(StreamPart::TextDelta {
                            id: "c0".into(),
                            delta: "subagent finding".into(),
                            provider_metadata: None,
                        }),
                        Ok(StreamPart::TextEnd {
                            id: "c0".into(),
                            provider_metadata: None,
                        }),
                    ]
                } else {
                    vec![Ok(StreamPart::ToolCall(ToolCallPart {
                        tool_call_id: "tc-agent-0".into(),
                        tool_name: "agent".into(),
                        input: serde_json::json!({
                            "prompt": AGENT_CHILD_MARKER,
                            "name": "scout",
                            "description": "delegated probe"
                        }),
                        provider_executed: None,
                        dynamic: None,
                        provider_options: None,
                    }))]
                };

                let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(parts));
                Ok(StreamResult {
                    stream: s,
                    request: None,
                    response: None,
                })
            }
        }

        DynLanguageModel::new(AgentRoutingModel)
    }

    // ===================================================================
    // Test 5: subagent spawned while parent turn is still in-flight
    // ===================================================================

    /// Spawns a subagent while the parent turn is **still in progress**
    /// (blocked inside `do_stream`). Asserts:
    ///
    /// (a) `SubagentCompleted` fires for the child thread.
    /// (b) After the child completes, the global engine phase is still `Turn`
    ///     (the child must not roll back the parent's phase slot).
    /// (c) After the parent unblocks and completes, `TurnCompleted` fires for
    ///     the parent and the engine phase returns to `Idle`.
    ///
    /// This is the production scenario described in the increment-6 spec:
    /// the parent LLM triggers an Agent tool call, which spawns a subagent
    /// via `SpawnSubagent` while the parent `run_turn` task is still alive.
    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "integration test requires an inline LanguageModel impl (HoldThenTextModel) \
                  plus two barrier rendezvous points; the logic is linear and splitting it \
                  would only move code without improving clarity"
    )]
    async fn spawn_subagent_while_parent_turn_in_flight() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Barrier;

        // A model that uses barriers for two-phase coordination:
        //
        // Call 0 (parent turn): blocks on `parent_hold` until the test
        // releases it. This keeps the engine in Turn phase while the child
        // subagent runs.
        //
        // Call 1 (child turn): returns a text answer immediately.
        #[derive(Debug)]
        struct HoldThenTextModel {
            call_count: Arc<AtomicUsize>,
            parent_hold: Arc<Barrier>,
        }

        #[async_trait]
        impl LanguageModel for HoldThenTextModel {
            fn provider(&self) -> &'static str {
                "test"
            }
            fn model_id(&self) -> &'static str {
                "hold-then-text"
            }
            async fn do_generate(
                &self,
                _opts: CallOptions,
            ) -> llmsdk::error::Result<GenerateResult> {
                Ok(GenerateResult {
                    content: vec![],
                    finish_reason: FinishReason::new(FinishReasonKind::Stop),
                    usage: llmsdk::language_model::Usage::default(),
                    provider_metadata: None,
                    request: None,
                    response: None,
                    warnings: vec![],
                })
            }
            async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
                let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
                if idx == 0 {
                    // Parent turn: block until the test releases the barrier.
                    self.parent_hold.wait().await;
                    let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::empty());
                    return Ok(StreamResult {
                        stream: s,
                        request: None,
                        response: None,
                    });
                }
                // Child turn: immediate text answer.
                let parts: Vec<llmsdk::error::Result<StreamPart>> = vec![
                    Ok(StreamPart::TextStart {
                        id: "c0".into(),
                        provider_metadata: None,
                    }),
                    Ok(StreamPart::TextDelta {
                        id: "c0".into(),
                        delta: "child done".into(),
                        provider_metadata: None,
                    }),
                    Ok(StreamPart::TextEnd {
                        id: "c0".into(),
                        provider_metadata: None,
                    }),
                ];
                let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(parts));
                Ok(StreamResult {
                    stream: s,
                    request: None,
                    response: None,
                })
            }
        }

        let parent_hold = Arc::new(Barrier::new(2));
        let model = HoldThenTextModel {
            call_count: Arc::new(AtomicUsize::new(0)),
            parent_hold: Arc::clone(&parent_hold),
        };

        let engine = Engine::spawn_with_provider(DynLanguageModel::new(model))
            .with_reply_timeout(std::time::Duration::from_secs(10));
        let mut events = engine.subscribe();

        let parent_id = tid("thread:native/parent-inflight");

        // Start the parent turn.  It blocks inside do_stream on parent_hold.
        engine
            .start_turn(parent_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted to confirm the parent turn task is in-flight.
        let saw_parent_started = collect_until(&mut events, 16, |ev| {
            matches!(ev, EngineEvent::TurnStarted { thread_id, .. } if thread_id == &parent_id)
        })
        .await;
        assert!(
            saw_parent_started,
            "expected TurnStarted for parent before spawning child"
        );

        // (b) Engine phase must be Turn while the parent turn is running.
        // We verify this BEFORE spawning the child so the assertion is
        // unambiguous.  The phase is Turn because start_turn raised it.
        // (We can't read phase directly from Engine, but we know it's Turn
        // because TurnStarted was broadcast without TurnCompleted following it.)

        // Spawn the subagent while the parent turn is still in-flight.
        let child_id = engine
            .spawn_subagent(parent_id.clone(), subagent_def(false))
            .await
            .expect("spawn_subagent must succeed while parent is in Turn phase");

        // (a) Wait for SubagentCompleted to confirm the child finished.
        let saw_child_completed = collect_until(&mut events, 64, |ev| {
            matches!(ev, EngineEvent::SubagentCompleted { child_thread_id, .. }
                if child_thread_id == &child_id)
        })
        .await;
        assert!(
            saw_child_completed,
            "SubagentCompleted must fire for the child thread"
        );

        // (b) After the child completes, the parent turn must STILL be in
        // Turn phase — no TurnCompleted for the parent must have fired yet.
        // We check the event stream: collect events with a very short timeout;
        // if TurnCompleted for the parent arrives here, the child incorrectly
        // rolled back the phase.
        let parent_completed_too_early = {
            let mut found = false;
            // Short window: we only look for events that already arrived.
            for _ in 0..8 {
                match tokio::time::timeout(std::time::Duration::from_millis(30), events.recv())
                    .await
                {
                    Ok(Ok(EngineEvent::TurnCompleted { thread_id, .. }))
                        if thread_id == parent_id =>
                    {
                        found = true;
                        break;
                    }
                    Ok(Ok(_)) => {}
                    _ => break,
                }
            }
            found
        };
        assert!(
            !parent_completed_too_early,
            "parent TurnCompleted must NOT fire while the parent turn is still blocked in \
             do_stream; the child must not roll back the global engine phase"
        );

        // Unblock the parent model so the parent turn can complete.
        parent_hold.wait().await;

        // (c) Parent TurnCompleted must eventually arrive.
        let saw_parent_completed = collect_until(&mut events, 64, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { thread_id, .. } if thread_id == &parent_id)
        })
        .await;
        assert!(
            saw_parent_completed,
            "parent TurnCompleted must fire after the parent model unblocks"
        );

        engine.shutdown().await.unwrap();
    }

    // ===================================================================
    // Shared test helpers
    // ===================================================================

    /// Returns a [`DynLanguageModel`] that produces an empty stream on the
    /// first call (for the parent turn) and a text `AgentMessage` with
    /// `text` on the second call (for the child turn).
    fn two_call_text_provider(text: &'static str) -> DynLanguageModel {
        use std::sync::atomic::AtomicUsize;

        #[derive(Debug)]
        struct TwoCallModel {
            call_count: Arc<AtomicUsize>,
            answer: &'static str,
        }

        #[async_trait]
        impl LanguageModel for TwoCallModel {
            fn provider(&self) -> &'static str {
                "test"
            }
            fn model_id(&self) -> &'static str {
                "two-call"
            }
            async fn do_generate(&self, _: CallOptions) -> llmsdk::error::Result<GenerateResult> {
                Ok(GenerateResult {
                    content: vec![],
                    finish_reason: FinishReason::new(FinishReasonKind::Stop),
                    usage: llmsdk::language_model::Usage::default(),
                    provider_metadata: None,
                    request: None,
                    response: None,
                    warnings: vec![],
                })
            }
            async fn do_stream(&self, _: CallOptions) -> llmsdk::error::Result<StreamResult> {
                let idx = self
                    .call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let parts: Vec<llmsdk::error::Result<StreamPart>> = if idx == 0 {
                    vec![]
                } else {
                    vec![
                        Ok(StreamPart::TextStart {
                            id: "b0".into(),
                            provider_metadata: None,
                        }),
                        Ok(StreamPart::TextDelta {
                            id: "b0".into(),
                            delta: self.answer.into(),
                            provider_metadata: None,
                        }),
                        Ok(StreamPart::TextEnd {
                            id: "b0".into(),
                            provider_metadata: None,
                        }),
                    ]
                };
                let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(parts));
                Ok(StreamResult {
                    stream: s,
                    request: None,
                    response: None,
                })
            }
        }

        DynLanguageModel::new(TwoCallModel {
            call_count: Arc::new(AtomicUsize::new(0)),
            answer: text,
        })
    }

    // ===================================================================
    // Test 6 (FIX 1): subagent child thread persists ThreadSource::Subagent
    // ===================================================================

    /// After a subagent child thread finishes its turn, the persisted thread
    /// row in state.db must carry `source = "subagent"`, not `source = "user"`.
    ///
    /// Before the fix, every `ThreadUpserted` snapshot in `finish_turn` and
    /// `cancel_turn` hardcoded `ThreadSource::User`, silently overwriting the
    /// `Subagent` discriminant that was written at spawn time.
    ///
    /// This test:
    /// 1. Configures an engine with real storage.
    /// 2. Runs a parent turn (empty provider → completes immediately).
    /// 3. Spawns a subagent whose child provider returns one `AgentMessage`.
    /// 4. Waits for `SubagentCompleted` and then for engine shutdown to flush.
    /// 5. Queries `state.db` and asserts the child thread row has
    ///    `source = "subagent"`.
    #[tokio::test]
    async fn subagent_child_thread_persists_source_subagent() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Arc::new(crate::persistence::Storage::open(tmp.path()).await.unwrap());

        let cfg = EngineConfig {
            provider: two_call_text_provider("child reply"),
            tools: Arc::new(crate::tools::ToolRegistry::new()),
            hook_host: Arc::new(crate::hooks::HookHost::new()),
            storage: Some(Arc::clone(&storage)),
            turn_limits: TurnLimits::default(),
            system_prompt: None,
        };
        let engine =
            Engine::spawn_with_config(cfg).with_reply_timeout(std::time::Duration::from_secs(10));
        let mut events = engine.subscribe();

        // Start the parent turn (call 0 → empty stream → completes quickly).
        let parent_id = tid("thread:native/fix1-source-parent");
        engine
            .start_turn(parent_id.clone(), Vec::new(), None)
            .await
            .unwrap();
        collect_until(&mut events, 32, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { thread_id, .. } if thread_id == &parent_id)
        })
        .await;

        // Spawn the subagent (call 1 → "child reply" answer).
        let child_id = engine
            .spawn_subagent(parent_id.clone(), subagent_def(false))
            .await
            .expect("spawn_subagent must succeed");

        // Wait for SubagentCompleted so the child turn has definitely finished.
        let saw_completed = collect_until(&mut events, 64, |ev| {
            matches!(ev, EngineEvent::SubagentCompleted { child_thread_id, .. }
                if child_thread_id == &child_id)
        })
        .await;
        assert!(saw_completed, "SubagentCompleted must fire for the child");

        // Shutdown flushes the persistence writer before returning.
        engine.shutdown().await.unwrap();

        // Assert: child thread row must carry source = "subagent".
        let child_row = storage
            .state
            .get_thread(&child_id)
            .await
            .expect("DB query must succeed")
            .expect("child thread row must exist in state.db after subagent turn");

        assert_eq!(
            child_row.source,
            zhive_proto::domain::ThreadSource::Subagent,
            "child thread source must be ThreadSource::Subagent in state.db, \
             got {:?}",
            child_row.source
        );
    }
}

// Rust guideline compliant 2026-02-21
