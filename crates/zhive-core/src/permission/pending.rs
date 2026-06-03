//! In-flight permission request map.
//!
//! Keyed by a monotonically increasing [`RequestKey`]; values are
//! [`tokio::sync::oneshot::Sender`] handles waiting for the client's
//! reply.
//!
//! ## Wire form
//!
//! The numeric key is also surfaced on the JSON-RPC wire as
//! `PermissionRequestId` (string form `"perm:<n>"`) — see
//! [`RequestKey::from_wire`] / [`RequestKey::to_wire`]. Engine
//! submissions receive the wire form and call `from_wire` before
//! dispatching to the reducer.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::oneshot;
use zhive_proto::domain::{ThreadId, TurnId};
use zhive_proto::permission::PermissionOutcome;

use crate::engine::submission::PermissionRequestId;

/// Stable wire prefix for [`RequestKey::to_wire`].
///
/// Kept as a constant so the encoder, decoder and any external
/// log/grep target agree on one literal.
const WIRE_PREFIX: &str = "perm:";

/// Opaque identifier for a pending permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestKey(pub u64);

/// Error returned by [`RequestKey::from_wire`] when the string does not
/// match the canonical `perm:<n>` shape.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid PermissionRequestId wire form: {0}")]
pub struct InvalidRequestId(pub String);

/// Thread + turn a pending permission request belongs to.
///
/// Stored alongside the waiter so the engine actor can emit a
/// [`crate::engine::event::EngineEvent::TurnResumed`] for the right turn when
/// a deferred request is discharged, without the resolving task having to know
/// the turn context. Top-level `Ask` flows enroll without context; only the
/// `Defer` (suspendable) path records it.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_core::permission::RequestContext;
/// use zhive_proto::domain::{ThreadId, TurnId};
///
/// let ctx = RequestContext {
///     thread_id: ThreadId(Arc::from("thread:native/example")),
///     turn_id: TurnId(Arc::from("turn:0000")),
/// };
/// assert_eq!(&*ctx.thread_id.0, "thread:native/example");
/// assert_eq!(&*ctx.turn_id.0, "turn:0000");
/// // RequestContext is Clone and PartialEq.
/// assert_eq!(ctx.clone(), ctx);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContext {
    /// Thread that owns the suspended turn.
    pub thread_id: ThreadId,
    /// The suspended turn.
    pub turn_id: TurnId,
}

impl RequestKey {
    /// Encodes the key into its wire form.
    ///
    /// The result is suitable for stuffing into
    /// [`PermissionRequestId`]; the round-trip
    /// `from_wire(&to_wire())` always succeeds.
    #[must_use]
    pub fn to_wire(self) -> PermissionRequestId {
        PermissionRequestId(format!("{WIRE_PREFIX}{}", self.0).into())
    }

    /// Decodes a wire-form [`PermissionRequestId`] back into a key.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidRequestId`] when the input is missing the
    /// `perm:` prefix or the numeric suffix fails to parse.
    pub fn from_wire(id: &PermissionRequestId) -> Result<Self, InvalidRequestId> {
        let raw: &str = &id.0;
        let Some(rest) = raw.strip_prefix(WIRE_PREFIX) else {
            return Err(InvalidRequestId(raw.to_string()));
        };
        let n: u64 = rest
            .parse()
            .map_err(|_e| InvalidRequestId(raw.to_string()))?;
        Ok(Self(n))
    }
}

/// One pending entry: the waiter and the turn it belongs to (if known).
struct PendingEntry {
    /// Sender that wakes the waiting tool-dispatch / spawner future.
    tx: oneshot::Sender<PermissionOutcome>,
    /// Turn context for `TurnResumed`; `None` for context-free `Ask` flows.
    context: Option<RequestContext>,
}

/// Concurrent map from [`RequestKey`] to its pending entry.
#[derive(Default)]
pub struct PendingPermissions {
    next: AtomicU64,
    map: Mutex<HashMap<RequestKey, PendingEntry>>,
}

impl std::fmt::Debug for PendingPermissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingPermissions")
            .field("pending", &self.len())
            .finish_non_exhaustive()
    }
}

impl PendingPermissions {
    /// Builds an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts `tx` with no turn context and returns a fresh key.
    pub fn insert(&self, tx: oneshot::Sender<PermissionOutcome>) -> RequestKey {
        self.insert_with_context(tx, None)
    }

    /// Inserts `tx` plus an optional turn `context` and returns a fresh key.
    ///
    /// The context is surfaced again from [`Self::remove`] so the engine can
    /// emit a `TurnResumed` for the originating turn when the request resolves.
    pub fn insert_with_context(
        &self,
        tx: oneshot::Sender<PermissionOutcome>,
        context: Option<RequestContext>,
    ) -> RequestKey {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let key = RequestKey(id);
        let mut guard = self.lock();
        guard.insert(key, PendingEntry { tx, context });
        key
    }

    /// Removes and returns the sender plus any turn context for `key`.
    pub fn remove(
        &self,
        key: RequestKey,
    ) -> Option<(oneshot::Sender<PermissionOutcome>, Option<RequestContext>)> {
        let mut guard = self.lock();
        guard.remove(&key).map(|entry| (entry.tx, entry.context))
    }

    /// Drains every pending sender (used during cancellation).
    pub fn drain(&self) -> Vec<oneshot::Sender<PermissionOutcome>> {
        let mut guard = self.lock();
        guard.drain().map(|(_k, entry)| entry.tx).collect()
    }

    /// Number of pending requests.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.map.lock() {
            Ok(g) => g.len(),
            Err(poisoned) => poisoned.get_ref().len(),
        }
    }

    /// `true` when no requests are pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RequestKey, PendingEntry>> {
        // A poisoned permission map is the consequence of a panic in
        // another task; recover the inner value rather than amplify
        // the failure into a second panic.
        match self.map.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn request_key_round_trips_through_wire_form() {
        let k = RequestKey(42);
        let wire = k.to_wire();
        assert_eq!(&*wire.0, "perm:42");
        let back = RequestKey::from_wire(&wire).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn from_wire_rejects_missing_prefix() {
        let id = PermissionRequestId(Arc::from("42"));
        assert!(RequestKey::from_wire(&id).is_err());
    }

    #[test]
    fn from_wire_rejects_non_numeric_suffix() {
        let id = PermissionRequestId(Arc::from("perm:not-a-number"));
        assert!(RequestKey::from_wire(&id).is_err());
    }
}

// Rust guideline compliant 2026-02-21
