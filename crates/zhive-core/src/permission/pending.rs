//! In-flight permission request map.
//!
//! Keyed by a monotonically increasing [`RequestKey`]; values are
//! [`tokio::sync::oneshot::Sender`] handles waiting for the client's
//! reply.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::oneshot;
use zhive_proto::permission::PermissionOutcome;

/// Opaque identifier for a pending permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestKey(pub u64);

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
        let mut guard = self.map.lock().expect("pending map lock poisoned");
        guard.insert(key, tx);
        key
    }

    /// Removes and returns the sender for `key` when present.
    pub fn remove(&self, key: RequestKey) -> Option<oneshot::Sender<PermissionOutcome>> {
        let mut guard = self.map.lock().expect("pending map lock poisoned");
        guard.remove(&key)
    }

    /// Drains every pending sender (used during cancellation).
    pub fn drain(&self) -> Vec<oneshot::Sender<PermissionOutcome>> {
        let mut guard = self.map.lock().expect("pending map lock poisoned");
        guard.drain().map(|(_k, v)| v).collect()
    }

    /// Number of pending requests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.lock().map_or(0, |m| m.len())
    }

    /// `true` when no requests are pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// Rust guideline compliant 2026-02-21
