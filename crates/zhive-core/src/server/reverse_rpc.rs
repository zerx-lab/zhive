//! Reverse-RPC tracker: server-initiated request / response matchmaker.
//!
//! When the engine needs to ask the connected client a question (the
//! canonical example is `permission/request`), the request rides on
//! the same JSON-RPC wire. A pending map keyed by JSON-RPC `id`
//! records the outstanding requests so that the [`super::serve_loop`]
//! can route a matching [`Response`] back to the engine.
//!
//! ## Scope
//!
//! This module covers the *server-side* of the round trip:
//!
//! * [`ReverseRpcTracker::issue`] allocates a fresh id and a
//!   [`tokio::sync::oneshot::Receiver`] that the caller awaits.
//! * The [`super::serve_loop`] funnels every incoming [`Response`]
//!   into [`ReverseRpcTracker::resolve`].
//! * [`ReverseRpcTracker::cancel_all`] drops every pending sender so
//!   the awaiters surface a clean failure during shutdown.
//!
//! Outbound writing (turning the issued [`Request`] into bytes on the
//! wire) is the caller's responsibility — the tracker stays
//! transport-agnostic. The first real consumer (hook host + permission
//! reducer wiring) lands together with B5/B7.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;
use tokio::sync::oneshot;
use zhive_proto::{ErrorObject, Id, Request, Response, ResponseOutcome};

/// Classification of [`ReverseRpcTracker::resolve`] outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveOutcome {
    /// The response matched a pending request and the awaiting
    /// `oneshot` was woken with the outcome.
    Delivered,
    /// The response matched a pending request but the awaiting
    /// future had already been dropped (typically a cancellation
    /// race). The caller should treat this as expected at `debug`
    /// level.
    AwaiterDropped,
    /// No pending request matched the response id. Usually a
    /// protocol bug or a buggy peer; the caller should log at `warn`.
    NoMatch,
}

/// Failure modes surfaced by [`ReverseRpcTracker`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ReverseRpcError {
    /// The peer responded but the body carried a JSON-RPC error
    /// object.
    #[error("peer returned JSON-RPC error: {0:?}")]
    Rpc(ErrorObject),

    /// The pending sender was dropped before a response arrived
    /// (engine shutdown, transport closed, …).
    #[error("reverse RPC was cancelled before a response arrived")]
    Cancelled,
}

/// Outcome of awaiting an in-flight reverse RPC.
pub type ReverseRpcResult = Result<serde_json::Value, ReverseRpcError>;

/// Tracks outstanding server-initiated requests.
///
/// Cheap to clone (`Arc`-wrapped at the call site). Internally guarded
/// by a `std::sync::Mutex` because every operation is constant-time
/// and never awaits while holding the lock.
#[derive(Debug, Default)]
pub struct ReverseRpcTracker {
    next_id: AtomicU64,
    pending: Mutex<HashMap<Id, oneshot::Sender<ReverseRpcResult>>>,
}

impl ReverseRpcTracker {
    /// Builds an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates a fresh JSON-RPC id and prepares a pending entry.
    ///
    /// Returns the constructed [`Request`] (ready to ship over the
    /// wire) and the awaiter; the caller must publish the request
    /// before awaiting `rx`.
    pub fn issue(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> (Request, oneshot::Receiver<ReverseRpcResult>) {
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Reverse RPC ids are namespaced with `rev:` so they cannot
        // collide with a client-issued numeric counter, and so logs /
        // dashboards can grep server-initiated requests at a glance.
        let id = Id::String(format!("rev:{seq}"));
        let (tx, rx) = oneshot::channel();
        let mut guard = self.lock();
        guard.insert(id.clone(), tx);
        drop(guard);
        (Request::new(id, method, params), rx)
    }

    /// Routes an incoming [`Response`] to its matching awaiter.
    ///
    /// Returns the classified [`ResolveOutcome`] so the caller can log
    /// strays at `warn` and dropped-awaiter races at `debug` instead of
    /// conflating the two.
    pub fn resolve(&self, response: Response) -> ResolveOutcome {
        let tx = {
            let mut guard = self.lock();
            guard.remove(&response.id)
        };
        let Some(tx) = tx else {
            return ResolveOutcome::NoMatch;
        };
        let outcome = match response.outcome {
            ResponseOutcome::Result(v) => Ok(v),
            ResponseOutcome::Error(e) => Err(ReverseRpcError::Rpc(e)),
        };
        if tx.send(outcome).is_ok() {
            ResolveOutcome::Delivered
        } else {
            ResolveOutcome::AwaiterDropped
        }
    }

    /// Drops every pending awaiter; the matching `wait` calls resolve
    /// to [`ReverseRpcError::Cancelled`].
    pub fn cancel_all(&self) {
        let drained: Vec<_> = {
            let mut guard = self.lock();
            guard.drain().map(|(_id, tx)| tx).collect()
        };
        for tx in drained {
            let _ = tx.send(Err(ReverseRpcError::Cancelled));
        }
    }

    /// Number of in-flight reverse RPCs.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.pending.lock() {
            Ok(g) => g.len(),
            Err(poisoned) => poisoned.get_ref().len(),
        }
    }

    /// `true` when no reverse RPCs are outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Id, oneshot::Sender<ReverseRpcResult>>> {
        match self.pending.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn issue_then_resolve_delivers_result() {
        let tracker = ReverseRpcTracker::new();
        let (req, rx) = tracker.issue("permission/request", None);
        assert_eq!(tracker.len(), 1);
        let response = Response::ok(req.id, serde_json::json!({"ok": true}));
        assert_eq!(tracker.resolve(response), ResolveOutcome::Delivered);
        let value = rx.await.unwrap().unwrap();
        assert_eq!(value, serde_json::json!({"ok": true}));
        assert!(tracker.is_empty());
    }

    #[tokio::test]
    async fn cancel_all_makes_waiters_observe_cancelled() {
        let tracker = ReverseRpcTracker::new();
        let (_req, rx) = tracker.issue("permission/request", None);
        tracker.cancel_all();
        let err = rx.await.unwrap().unwrap_err();
        assert!(matches!(err, ReverseRpcError::Cancelled));
    }

    #[tokio::test]
    async fn resolve_with_no_pending_reports_no_match() {
        let tracker = ReverseRpcTracker::new();
        let response = Response::ok(Id::String("rev:42".into()), serde_json::Value::Null);
        assert_eq!(tracker.resolve(response), ResolveOutcome::NoMatch);
    }

    #[tokio::test]
    async fn resolve_with_dropped_awaiter_reports_awaiter_dropped() {
        let tracker = ReverseRpcTracker::new();
        let (req, rx) = tracker.issue("permission/request", None);
        drop(rx);
        let response = Response::ok(req.id, serde_json::Value::Null);
        assert_eq!(tracker.resolve(response), ResolveOutcome::AwaiterDropped);
    }

    #[tokio::test]
    async fn resolve_forwards_error_object() {
        let tracker = ReverseRpcTracker::new();
        let (req, rx) = tracker.issue("permission/request", None);
        let err_obj = ErrorObject {
            code: -32000,
            message: "denied".into(),
            data: None,
        };
        let response = Response::err(req.id, err_obj.clone());
        assert_eq!(tracker.resolve(response), ResolveOutcome::Delivered);
        let err = rx.await.unwrap().unwrap_err();
        match err {
            ReverseRpcError::Rpc(e) => assert_eq!(e.code, err_obj.code),
            other => panic!("expected Rpc variant, got {other:?}"),
        }
    }
}

// Rust guideline compliant 2026-02-21
