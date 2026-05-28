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
//!
//! `Defer` outcomes are surfaced verbatim; the engine actor is
//! responsible for parking the turn until a matching
//! `Submission::ResumePermission` arrives.

pub mod pending;

#[doc(inline)]
pub use pending::{InvalidRequestId, PendingPermissions, RequestKey};

use std::sync::Arc;
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
#[derive(Debug, Clone)]
pub struct PermissionReducer {
    pending: Arc<PendingPermissions>,
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
            timeout: DEFAULT_PERMISSION_TIMEOUT,
        }
    }

    /// Returns a clone of the pending store. Cheap (`Arc<...>`).
    #[must_use]
    pub fn pending(&self) -> Arc<PendingPermissions> {
        Arc::clone(&self.pending)
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

    /// Resolves a pending request with the supplied outcome.
    ///
    /// # Errors
    ///
    /// Returns [`ReducerError::UnknownRequest`] when `key` has no
    /// matching entry, or [`ReducerError::Abandoned`] when the waiter
    /// went away before the outcome could be delivered (typically the
    /// `wait` future was dropped because of a timeout or cancel).
    pub fn resolve(&self, key: RequestKey, outcome: PermissionOutcome) -> Result<(), ReducerError> {
        self.pending
            .remove(key)
            .ok_or(ReducerError::UnknownRequest(key))
            .and_then(|tx| {
                tx.send(outcome)
                    .map_err(|_send_err| ReducerError::Abandoned)
            })
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
        let key = RequestKey::from_wire(id)?;
        self.resolve(key, outcome)
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
}

// Rust guideline compliant 2026-02-21
