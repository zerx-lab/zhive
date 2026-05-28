//! RAII handles for hook lifetimes (zombie-listener prevention).
//!
//! Every hook registration returns an [`ExtensionScope`]; dropping the
//! scope deregisters the callback even when the owning code panics.
//! The [`super::HookHost`] also exposes
//! [`super::HookHost::unregister_scope`] for explicit cleanup before
//! drop (Pi `invalidate()` analogue).

use std::sync::atomic::{AtomicU64, Ordering};

/// Stable identifier for one registered hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegistrationId(pub u64);

/// Issues monotonically increasing [`RegistrationId`] values.
#[derive(Debug, Default)]
pub struct RegistrationIdAllocator {
    next: AtomicU64,
}

impl RegistrationIdAllocator {
    /// Builds a fresh allocator starting at id 1.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    /// Returns the next free id.
    pub fn next(&self) -> RegistrationId {
        RegistrationId(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

/// RAII handle returned by [`super::HookHost::register`].
///
/// Dropping the scope deregisters the hook. A scope can be detached
/// with [`Self::leak`] when the owner wants registration to outlive
/// the call frame; in that case explicit cleanup runs through
/// [`super::HookHost::unregister`].
pub struct ExtensionScope {
    id: RegistrationId,
    cleanup: Option<Box<dyn FnOnce(RegistrationId) + Send>>,
}

impl std::fmt::Debug for ExtensionScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionScope")
            .field("id", &self.id)
            .field("attached", &self.cleanup.is_some())
            .finish()
    }
}

impl ExtensionScope {
    /// Internal constructor used by [`super::HookHost`].
    pub(crate) fn new<F>(id: RegistrationId, cleanup: F) -> Self
    where
        F: FnOnce(RegistrationId) + Send + 'static,
    {
        Self {
            id,
            cleanup: Some(Box::new(cleanup)),
        }
    }

    /// Returns the id of the underlying registration.
    #[must_use]
    pub const fn id(&self) -> RegistrationId {
        self.id
    }

    /// Detaches the scope so dropping it does **not** deregister the
    /// hook. The caller assumes responsibility for explicit cleanup.
    #[must_use = "leaking a scope without keeping the id strands the registration"]
    pub fn leak(mut self) -> RegistrationId {
        self.cleanup.take();
        self.id
    }
}

impl Drop for ExtensionScope {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup(self.id);
        }
    }
}

// Rust guideline compliant 2026-02-21
