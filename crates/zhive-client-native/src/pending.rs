//! In-flight outbound request map.
//!
//! Mirrors `zhive-core::server::reverse_rpc::ReverseRpcTracker` but
//! lives on the client side: a [`PendingRequests`] keeps a oneshot
//! sender per outstanding [`Id`] and resolves them when the matching
//! [`zhive_proto::Response`] arrives on the reader task.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;
use zhive_proto::{Id, ResponseOutcome};

/// Concurrent JSON-RPC id → oneshot map.
///
/// Cheap to clone via `Arc`; the internal `std::sync::Mutex` is held
/// across constant-time operations only (no awaits, no allocations
/// inside the critical section beyond a single hashmap mutation).
#[derive(Debug, Default)]
pub struct PendingRequests {
    pending: Mutex<HashMap<Id, oneshot::Sender<PendingSlot>>>,
}

/// The payload carried in each pending oneshot channel.
///
/// Separating this from `ResponseOutcome` lets the reader task deliver
/// a human-readable disconnect reason without adding a sentinel error
/// code to the normal response path.
#[derive(Debug)]
pub(crate) enum PendingSlot {
    /// A JSON-RPC response arrived from the server.
    Response(ResponseOutcome),
    /// The connection closed before the response arrived; contains a
    /// human-readable reason string.
    Disconnected(String),
}

/// Outcome of [`PendingRequests::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResolveResult {
    /// Response was delivered to its awaiter.
    Delivered,
    /// Awaiter was dropped before the response arrived (cancellation
    /// race).
    AwaiterDropped,
    /// No pending request matched the response id.
    NoMatch,
}

impl PendingRequests {
    /// Registers `id` and returns the awaiter the caller should
    /// `.await` for the matching response.
    ///
    /// The caller is responsible for shipping a [`zhive_proto::Request`]
    /// carrying the same `id` over the wire.
    pub(crate) fn register(&self, id: Id) -> oneshot::Receiver<PendingSlot> {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.lock();
        guard.insert(id, tx);
        rx
    }

    /// Resolves the pending entry for `id`, if any.
    pub fn resolve(&self, id: &Id, outcome: ResponseOutcome) -> ResolveResult {
        let tx = {
            let mut guard = self.lock();
            guard.remove(id)
        };
        let Some(tx) = tx else {
            return ResolveResult::NoMatch;
        };
        if tx.send(PendingSlot::Response(outcome)).is_ok() {
            ResolveResult::Delivered
        } else {
            ResolveResult::AwaiterDropped
        }
    }

    /// Drains every pending awaiter with a disconnected payload carrying
    /// `reason`.
    ///
    /// In-flight `call` futures resolve to
    /// [`crate::ClientError::Disconnected`] rather than hanging.
    pub fn drain_with_disconnected(&self, reason: &str) {
        let drained: Vec<_> = {
            let mut guard = self.lock();
            guard.drain().map(|(_, tx)| tx).collect()
        };
        for tx in drained {
            // If the receiver was already dropped (e.g. the caller
            // gave up on the call) this is a benign no-op.
            let _ = tx.send(PendingSlot::Disconnected(reason.to_owned()));
        }
    }

    /// Drops every pending awaiter; the matching `call` futures
    /// observe a [`crate::ClientError::Disconnected`].
    ///
    /// Prefer [`Self::drain_with_disconnected`] when a reason string is
    /// available so callers receive the more informative
    /// `ClientError::Disconnected` variant.
    pub fn drain(&self) {
        self.drain_with_disconnected("connection closed");
    }

    /// Number of in-flight requests.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.pending.lock() {
            Ok(g) => g.len(),
            Err(poisoned) => poisoned.get_ref().len(),
        }
    }

    /// `true` when no requests are outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Id, oneshot::Sender<PendingSlot>>> {
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
    async fn register_then_resolve_delivers() {
        let p = PendingRequests::default();
        let rx = p.register(Id::Number(1));
        assert_eq!(p.len(), 1);
        let outcome = ResponseOutcome::Result(serde_json::json!("ok"));
        assert_eq!(
            p.resolve(&Id::Number(1), outcome.clone()),
            ResolveResult::Delivered
        );
        let got = rx.await.unwrap();
        assert!(matches!(got, PendingSlot::Response(_)));
        assert!(p.is_empty());
    }

    #[tokio::test]
    async fn resolve_unknown_returns_no_match() {
        let p = PendingRequests::default();
        let outcome = ResponseOutcome::Result(serde_json::Value::Null);
        assert_eq!(p.resolve(&Id::Number(99), outcome), ResolveResult::NoMatch);
    }

    #[tokio::test]
    async fn resolve_with_dropped_awaiter_reports_awaiter_dropped() {
        let p = PendingRequests::default();
        let rx = p.register(Id::Number(2));
        drop(rx);
        let outcome = ResponseOutcome::Result(serde_json::Value::Null);
        assert_eq!(
            p.resolve(&Id::Number(2), outcome),
            ResolveResult::AwaiterDropped
        );
    }

    #[tokio::test]
    async fn drain_drops_every_pending() {
        let p = PendingRequests::default();
        let rx1 = p.register(Id::Number(1));
        let rx2 = p.register(Id::Number(2));
        p.drain();
        assert!(p.is_empty());
        // drain sends Disconnected, so receivers get Ok(Disconnected)
        // not Err(RecvError).
        assert!(rx1.await.is_ok());
        assert!(rx2.await.is_ok());
    }

    #[tokio::test]
    async fn drain_with_disconnected_carries_reason() {
        let p = PendingRequests::default();
        let rx = p.register(Id::Number(3));
        p.drain_with_disconnected("test reason");
        let slot = rx.await.unwrap();
        assert!(
            matches!(slot, PendingSlot::Disconnected(ref r) if r == "test reason"),
            "expected Disconnected with the provided reason"
        );
    }
}

// Rust guideline compliant 2026-02-21
