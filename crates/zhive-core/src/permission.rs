//! Permission reducer host (D-008 + ACP 0.12 Cancelled outcome).
//!
//! Wraps [`zhive_proto::permission::reduce`] with the engine-side
//! plumbing that hook outputs cannot supply on their own:
//!
//! * **`BypassPermissions` short-circuit** — when the active mode is
//!   `BypassPermissions`, the reducer answers `Allow` without
//!   consulting any hook (Claude Code safety hazard contract).
//! * **`ask_user` round-trip** — when the folded decision is `Ask`, the
//!   reducer ships a [`RequestPermissionRequest`] to the client and
//!   awaits a [`PermissionOutcome`] with a configurable timeout
//!   (default 30 s). Timeouts surface as [`ReducerError::TimedOut`] so
//!   the engine can choose to deny the call, defer it, or surface a
//!   user-visible error; the reducer deliberately does **not** silently
//!   map them onto a `Cancelled` outcome.
//! * **`Cancelled` injection** — when a turn is cancelled, every
//!   pending request resolves to [`PermissionOutcome::Cancelled`]
//!   (ACP 0.12 schema verbatim) via [`PermissionReducer::cancel_all`].
//! * **Allow-always memoization** — when the user picks `AllowAlways`
//!   for a tool, its name is recorded in a per-engine, session-scoped set
//!   (in-memory, keyed strictly by tool name). The next `Ask` for the same
//!   tool is then downgraded to `Allow` without re-prompting. This only
//!   relaxes calls that *would have asked*; a folded `Deny` always wins.
//!
//! `Defer` outcomes are surfaced verbatim; the engine actor is
//! responsible for parking the turn until a matching
//! `Submission::ResumePermission` arrives.

pub mod pending;

#[doc(inline)]
pub use pending::{InvalidRequestId, PendingPermissions, RequestContext, RequestKey};

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::oneshot;
use zhive_proto::permission::{
    PermissionDecision, PermissionMode, PermissionOutcome, PermissionScope,
    RequestPermissionRequest, reduce,
};

/// Default deadline for a `permission/request` reverse RPC.
///
/// Picked to match the codex `PendingPermissionTimeout` value; long
/// enough to let the user pick a UI option without leaving an abandoned
/// tool call dangling for hours.
pub const DEFAULT_PERMISSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Reasons the reducer fails.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReducerError {
    /// The pending-request store had no matching entry when a resume
    /// arrived. Typically a programmer error or a duplicate resume.
    #[error("no pending permission request: {0:?}")]
    UnknownRequest(RequestKey),

    /// The wire-form [`crate::engine::submission::PermissionRequestId`]
    /// did not parse into a [`RequestKey`]; the engine treats it as a
    /// protocol error and discards the resume.
    #[error("invalid permission request id on the wire: {0}")]
    InvalidRequestId(#[from] InvalidRequestId),

    /// The waiter dropped before a resume / cancel arrived; the engine
    /// must treat the turn as failed.
    #[error("permission request abandoned without resolution")]
    Abandoned,

    /// The client did not respond within the reducer's timeout window.
    ///
    /// Distinct from [`PermissionOutcome::Cancelled`]: a timeout reflects
    /// an unresponsive client (or a network stall), whereas `Cancelled`
    /// reflects an engine- or user-initiated abort of the surrounding
    /// turn. The engine decides how to map a timeout onto a final
    /// permission decision (typically `Deny`).
    #[error("permission request timed out after {0:?}")]
    TimedOut(Duration),
}

/// Pure helper: folds an iterator of [`PermissionDecision`] values.
///
/// Thin wrapper around [`zhive_proto::permission::reduce`] kept here so
/// the engine module does not have to import the proto crate directly.
#[must_use]
pub fn fold(decisions: &[PermissionDecision]) -> PermissionDecision {
    reduce(decisions)
}

/// Folds an iterator of decisions *with* mode awareness.
///
/// Returns `Allow` immediately when `mode == BypassPermissions` (Claude
/// Code Subagents safety contract); otherwise delegates to [`fold`].
#[must_use]
pub fn evaluate(
    scope: &PermissionScope,
    hook_decisions: &[PermissionDecision],
) -> PermissionDecision {
    if matches!(
        scope.permission_mode,
        Some(PermissionMode::BypassPermissions)
    ) {
        return PermissionDecision::Allow;
    }
    fold(hook_decisions)
}

/// Coordinates `permission/request` reverse RPCs.
///
/// Also owns the per-engine, session-scoped **allow-always** set: when the
/// user picks `AllowAlways` for a tool, its name is recorded here so the
/// next [`PermissionDecision::Ask`] for the *same tool name* is downgraded
/// to `Allow` without raising another prompt. The set is in-memory only
/// (never persisted to disk) and keyed strictly by tool name, so it can
/// never widen access to a tool the user did not approve.
///
/// `Clone` is shallow: every clone shares the same pending store **and**
/// the same allow-always set via `Arc`, matching the reverse-RPC tracker
/// sharing contract.
#[derive(Debug, Clone)]
pub struct PermissionReducer {
    pending: Arc<PendingPermissions>,
    /// Tool names the user approved with `AllowAlways` this session.
    ///
    /// Shared across clones so a record made on one handle is visible to
    /// every other handle (the engine, the reverse-RPC tracker, …).
    allow_always: Arc<Mutex<HashSet<String>>>,
    timeout: Duration,
}

impl Default for PermissionReducer {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionReducer {
    /// Builds a reducer with the [`DEFAULT_PERMISSION_TIMEOUT`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Arc::new(PendingPermissions::new()),
            allow_always: Arc::new(Mutex::new(HashSet::new())),
            timeout: DEFAULT_PERMISSION_TIMEOUT,
        }
    }

    /// Returns a clone of the pending store. Cheap (`Arc<...>`).
    #[must_use]
    pub fn pending(&self) -> Arc<PendingPermissions> {
        Arc::clone(&self.pending)
    }

    /// Returns `true` when `tool_name` was approved with `AllowAlways`.
    ///
    /// Used by the dispatch path to downgrade an [`PermissionDecision::Ask`]
    /// to `Allow` without re-prompting. The lookup is strictly by tool
    /// name; it never widens access to any other tool, and it has **no**
    /// effect on a folded `Deny` (the dispatch path only consults it on the
    /// `Ask` branch, so `Deny` always wins — see the module docs).
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::permission::PermissionReducer;
    /// let reducer = PermissionReducer::new();
    /// assert!(!reducer.is_tool_allow_always("read_file"));
    /// reducer.record_allow_always("read_file");
    /// assert!(reducer.is_tool_allow_always("read_file"));
    /// assert!(!reducer.is_tool_allow_always("bash"));
    /// ```
    #[must_use]
    pub fn is_tool_allow_always(&self, tool_name: &str) -> bool {
        self.lock_allow_always().contains(tool_name)
    }

    /// Records that the user approved `tool_name` with `AllowAlways`.
    ///
    /// The record is session-scoped and in-memory; subsequent
    /// [`PermissionDecision::Ask`] checks for the same tool name are
    /// downgraded to `Allow`. Calling this twice for the same name is a
    /// no-op (set semantics). It never affects any other tool name.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::permission::PermissionReducer;
    /// let a = PermissionReducer::new();
    /// let b = a.clone(); // clones share one allow-always set
    /// a.record_allow_always("edit_file");
    /// assert!(b.is_tool_allow_always("edit_file"));
    /// ```
    pub fn record_allow_always(&self, tool_name: &str) {
        self.lock_allow_always().insert(tool_name.to_owned());
    }

    /// Locks the allow-always set, recovering from a poisoned mutex.
    ///
    /// A poisoned mutex is the consequence of a panic in another task;
    /// recover the inner value rather than amplify the failure into a
    /// second panic (mirrors [`PendingPermissions`]'s lock policy).
    fn lock_allow_always(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        match self.allow_always.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Overrides the default reverse-RPC timeout.
    ///
    /// Mostly for tests; production callers should use the default.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Registers a pending request and returns the associated key and
    /// the wire payload to ship to the client.
    ///
    /// The caller awaits resolution by passing the same `request` to
    /// [`Self::wait`] (or [`Self::wait_with_dispatch`]). The
    /// `Cancelled` outcome is reserved for engine-driven cancellation
    /// via [`Self::cancel_all`].
    #[must_use = "the returned key is required to discharge the request"]
    pub fn enroll(
        &self,
        request: RequestPermissionRequest,
    ) -> (
        RequestKey,
        RequestPermissionRequest,
        oneshot::Receiver<PermissionOutcome>,
    ) {
        let (tx, rx) = oneshot::channel();
        let key = self.pending.insert(tx);
        (key, request, rx)
    }

    /// Enrolls a request and records the turn context for later resumption.
    ///
    /// Like [`Self::enroll`] but also stores the supplied turn [`RequestContext`].
    /// The context is returned by [`Self::resolve_by_wire_id_with_context`] so
    /// the engine can emit a `TurnResumed` for the originating turn once a
    /// deferred request is discharged. Used by the suspendable `Defer` path.
    ///
    /// Engine-internal: callers (`tool_dispatch`, `spawner`) construct the
    /// `#[non_exhaustive]` [`RequestPermissionRequest`] in-crate; external code
    /// only ever receives it over the wire, so this is `pub(crate)`.
    #[must_use = "the returned key is required to discharge the request"]
    pub(crate) fn enroll_with_context(
        &self,
        request: RequestPermissionRequest,
        context: RequestContext,
    ) -> (
        RequestKey,
        RequestPermissionRequest,
        oneshot::Receiver<PermissionOutcome>,
    ) {
        let (tx, rx) = oneshot::channel();
        let key = self.pending.insert_with_context(tx, Some(context));
        (key, request, rx)
    }

    /// Waits for the client to resolve `rx`, applying the reducer's
    /// timeout.
    ///
    /// # Errors
    ///
    /// * [`ReducerError::Abandoned`] when the matching sender was
    ///   dropped without a resolution (typically engine shutdown
    ///   without going through [`Self::cancel_all`]).
    /// * [`ReducerError::TimedOut`] when the configured timeout fires
    ///   first. The caller decides how to translate that into a
    ///   [`PermissionDecision`]; the reducer does **not** map it onto
    ///   [`PermissionOutcome::Cancelled`].
    pub async fn wait(
        &self,
        rx: oneshot::Receiver<PermissionOutcome>,
    ) -> Result<PermissionOutcome, ReducerError> {
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(_recv_err)) => Err(ReducerError::Abandoned),
            Err(_elapsed) => Err(ReducerError::TimedOut(self.timeout)),
        }
    }

    /// Waits for the client to resolve `rx` with **no timeout**.
    ///
    /// Used by the `Defer` decision path: a deferred tool call suspends the
    /// turn indefinitely until the client sends `resume_permission`
    /// (`Selected`/`Cancelled`) or the engine cancels it via
    /// [`Self::cancel_all`]. Unlike [`Self::wait`], it never yields
    /// [`ReducerError::TimedOut`]; the pending entry is the materialised
    /// "suspended turn" registry.
    ///
    /// # Errors
    ///
    /// Returns [`ReducerError::Abandoned`] when the matching sender is dropped
    /// without a resolution (e.g. engine shutdown without `cancel_all`).
    pub async fn wait_unbounded(
        &self,
        rx: oneshot::Receiver<PermissionOutcome>,
    ) -> Result<PermissionOutcome, ReducerError> {
        rx.await.map_err(|_recv_err| ReducerError::Abandoned)
    }

    /// Resolves a pending request with the supplied outcome.
    ///
    /// # Errors
    ///
    /// Returns [`ReducerError::UnknownRequest`] when `key` has no
    /// matching entry, or [`ReducerError::Abandoned`] when the waiter
    /// went away before the outcome could be delivered (typically the
    /// `wait` future was dropped because of a timeout or cancel).
    pub fn resolve(&self, key: RequestKey, outcome: PermissionOutcome) -> Result<(), ReducerError> {
        self.resolve_with_context(key, outcome).map(|_ctx| ())
    }

    /// Resolves a pending request and returns its recorded turn context.
    ///
    /// Identical to [`Self::resolve`] but surfaces the [`RequestContext`]
    /// captured at [`Self::enroll_with_context`] time (or `None` for a
    /// context-free `Ask` request) so the engine can emit a `TurnResumed` for
    /// the right turn.
    ///
    /// # Errors
    ///
    /// Same as [`Self::resolve`].
    ///
    /// Engine-internal (`pub(crate)`): returns the [`RequestContext`] recorded
    /// by [`Self::enroll_with_context`], or `None` for a context-free enroll.
    pub(crate) fn resolve_with_context(
        &self,
        key: RequestKey,
        outcome: PermissionOutcome,
    ) -> Result<Option<RequestContext>, ReducerError> {
        let (tx, context) = self
            .pending
            .remove(key)
            .ok_or(ReducerError::UnknownRequest(key))?;
        tx.send(outcome)
            .map_err(|_send_err| ReducerError::Abandoned)?;
        Ok(context)
    }

    /// Resolves a pending request by its wire-form id.
    ///
    /// Convenience over [`Self::resolve`] for callers that hold the
    /// JSON-RPC [`crate::engine::submission::PermissionRequestId`] (e.g.
    /// the engine actor dispatching a
    /// [`crate::engine::submission::Submission::ResumePermission`]).
    ///
    /// # Errors
    ///
    /// * [`ReducerError::InvalidRequestId`] when the wire form does
    ///   not parse.
    /// * [`ReducerError::UnknownRequest`] / [`ReducerError::Abandoned`]
    ///   as in [`Self::resolve`].
    pub fn resolve_by_wire_id(
        &self,
        id: &crate::engine::submission::PermissionRequestId,
        outcome: PermissionOutcome,
    ) -> Result<(), ReducerError> {
        self.resolve_by_wire_id_with_context(id, outcome)
            .map(|_ctx| ())
    }

    /// Resolves a pending request by wire id, returning its turn context.
    ///
    /// Combines [`RequestKey::from_wire`] with [`Self::resolve_with_context`]
    /// so the engine actor can emit a `TurnResumed` for the resumed turn.
    ///
    /// # Errors
    ///
    /// * [`ReducerError::InvalidRequestId`] when the wire form does not parse.
    /// * [`ReducerError::UnknownRequest`] / [`ReducerError::Abandoned`] as in
    ///   [`Self::resolve`].
    ///
    /// Engine-internal (`pub(crate)`): the engine actor (`inner.rs`) calls this
    /// to recover the suspended turn's [`RequestContext`] and emit `TurnResumed`.
    pub(crate) fn resolve_by_wire_id_with_context(
        &self,
        id: &crate::engine::submission::PermissionRequestId,
        outcome: PermissionOutcome,
    ) -> Result<Option<RequestContext>, ReducerError> {
        let key = RequestKey::from_wire(id)?;
        self.resolve_with_context(key, outcome)
    }

    /// Resolves **every** pending request with
    /// [`PermissionOutcome::Cancelled`] (cancel propagation path).
    pub fn cancel_all(&self) {
        for tx in self.pending.drain() {
            let _ = tx.send(PermissionOutcome::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(mode: Option<PermissionMode>) -> PermissionScope {
        // PermissionScope is #[non_exhaustive]; build through JSON.
        let json = match mode {
            Some(m) => serde_json::json!({ "permissionMode": m }),
            None => serde_json::json!({}),
        };
        serde_json::from_value(json).expect("scope fixture")
    }

    fn ask_request() -> RequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "threadId": "thread:native/a",
            "resourceType": "tool",
            "name": "read_file",
            "reason": "test",
            "options": []
        }))
        .expect("request fixture")
    }

    #[test]
    fn bypass_short_circuits_to_allow() {
        let s = scope(Some(PermissionMode::BypassPermissions));
        let d = evaluate(&s, &[PermissionDecision::Deny]);
        assert_eq!(d, PermissionDecision::Allow);
    }

    #[test]
    fn fold_falls_through_otherwise() {
        let s = scope(Some(PermissionMode::Default));
        let d = evaluate(&s, &[PermissionDecision::Ask, PermissionDecision::Allow]);
        assert_eq!(d, PermissionDecision::Ask);
    }

    #[tokio::test]
    async fn enroll_then_resolve_round_trip() {
        let reducer = PermissionReducer::new();
        let (key, _req, rx) = reducer.enroll(ask_request());
        reducer
            .resolve(
                key,
                PermissionOutcome::Selected {
                    option_id: "allow_once".into(),
                },
            )
            .unwrap();
        let outcome = reducer.wait(rx).await.unwrap();
        assert!(matches!(outcome, PermissionOutcome::Selected { .. }));
    }

    #[tokio::test]
    async fn timeout_surfaces_timed_out_error() {
        let reducer = PermissionReducer::new().with_timeout(Duration::from_millis(20));
        let (_key, _req, rx) = reducer.enroll(ask_request());
        let err = reducer.wait(rx).await.unwrap_err();
        assert!(matches!(err, ReducerError::TimedOut(_)));
    }

    /// `wait_unbounded` must NOT time out: even with a tiny configured
    /// timeout, it waits until the request is resolved (the Defer path).
    #[tokio::test]
    async fn wait_unbounded_ignores_timeout_and_resolves() {
        let reducer = PermissionReducer::new().with_timeout(Duration::from_millis(10));
        let (key, _req, rx) = reducer.enroll(ask_request());

        // Resolve only after a delay that exceeds the reducer timeout; a
        // bounded `wait` would have timed out, but `wait_unbounded` must not.
        let resolver = reducer.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = resolver.resolve(
                key,
                PermissionOutcome::Selected {
                    option_id: "allow_once".into(),
                },
            );
        });

        let outcome = reducer.wait_unbounded(rx).await.unwrap();
        assert!(matches!(outcome, PermissionOutcome::Selected { .. }));
    }

    /// A suspended `Defer` wait must receive `Cancelled` when the engine
    /// drains the pending map via `cancel_all` (the `cancel_turn` path).
    #[tokio::test]
    async fn wait_unbounded_receives_cancelled_on_cancel_all() {
        let reducer = PermissionReducer::new();
        let (_key, _req, rx) = reducer.enroll(ask_request());
        reducer.cancel_all();
        let outcome = reducer.wait_unbounded(rx).await.unwrap();
        assert!(matches!(outcome, PermissionOutcome::Cancelled));
    }

    #[tokio::test]
    async fn cancel_all_is_distinguishable_from_timeout() {
        let reducer = PermissionReducer::new();
        let (_key, _req, rx) = reducer.enroll(ask_request());
        reducer.cancel_all();
        let outcome = reducer.wait(rx).await.unwrap();
        // Engine-driven cancel propagates the canonical Cancelled
        // outcome, *not* a TimedOut error.
        assert_eq!(outcome, PermissionOutcome::Cancelled);
    }

    #[tokio::test]
    async fn cancel_all_resolves_pending() {
        let reducer = PermissionReducer::new();
        let (_key, _req, rx) = reducer.enroll(ask_request());
        reducer.cancel_all();
        let outcome = reducer.wait(rx).await.unwrap();
        assert_eq!(outcome, PermissionOutcome::Cancelled);
    }

    #[test]
    fn resolve_unknown_key_returns_error() {
        let reducer = PermissionReducer::new();
        let err = reducer
            .resolve(RequestKey(99), PermissionOutcome::Cancelled)
            .unwrap_err();
        assert!(matches!(err, ReducerError::UnknownRequest(_)));
    }

    #[test]
    fn allow_always_record_then_query_round_trip() {
        let reducer = PermissionReducer::new();
        assert!(!reducer.is_tool_allow_always("read_file"));
        reducer.record_allow_always("read_file");
        assert!(reducer.is_tool_allow_always("read_file"));
        // Strictly keyed by name: a different tool stays un-allowed.
        assert!(!reducer.is_tool_allow_always("bash"));
    }

    #[test]
    fn allow_always_is_idempotent() {
        let reducer = PermissionReducer::new();
        reducer.record_allow_always("edit_file");
        reducer.record_allow_always("edit_file");
        assert!(reducer.is_tool_allow_always("edit_file"));
    }

    #[test]
    fn allow_always_set_is_shared_across_clones() {
        let original = PermissionReducer::new();
        let clone = original.clone();
        // A record made on the clone is visible on the original, proving
        // both handles share the same Arc-backed set.
        clone.record_allow_always("write_file");
        assert!(original.is_tool_allow_always("write_file"));
        // And vice versa.
        original.record_allow_always("grep");
        assert!(clone.is_tool_allow_always("grep"));
    }
}

// Rust guideline compliant 2026-02-21
