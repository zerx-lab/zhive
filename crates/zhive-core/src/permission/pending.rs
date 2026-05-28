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

/// Concurrent map from [`RequestKey`] to oneshot sender.
#[derive(Default)]
pub struct PendingPermissions {
    next: AtomicU64,
    map: Mutex<HashMap<RequestKey, oneshot::Sender<PermissionOutcome>>>,
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

    /// Inserts `tx` and returns a fresh key.
    pub fn insert(&self, tx: oneshot::Sender<PermissionOutcome>) -> RequestKey {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let key = RequestKey(id);
        let mut guard = self.lock();
        guard.insert(key, tx);
        key
    }

    /// Removes and returns the sender for `key` when present.
    pub fn remove(&self, key: RequestKey) -> Option<oneshot::Sender<PermissionOutcome>> {
        let mut guard = self.lock();
        guard.remove(&key)
    }

    /// Drains every pending sender (used during cancellation).
    pub fn drain(&self) -> Vec<oneshot::Sender<PermissionOutcome>> {
        let mut guard = self.lock();
        guard.drain().map(|(_k, v)| v).collect()
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

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<RequestKey, oneshot::Sender<PermissionOutcome>>> {
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
