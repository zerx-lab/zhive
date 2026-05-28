//! Hook host: red line 10 (provenance required) + red line 11
//! (re-validate after mutation).
//!
//! The host owns a [`SchemaCache`] for tool input re-validation and a
//! list of [`HookRegistration`] entries. Dispatch is **serial** within
//! each event (D-008 decision); panics inside a callback are isolated
//! with [`futures::FutureExt::catch_unwind`].

pub mod scope;
pub mod validator;

#[doc(inline)]
pub use scope::{ExtensionScope, RegistrationId};
#[doc(inline)]
pub use validator::{SchemaCache, ValidatorError};

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use futures::FutureExt;
use thiserror::Error;
use zhive_proto::hook::{ExtensionRef, HookEvent};
use zhive_proto::permission::HookOutput;

use scope::RegistrationIdAllocator;

/// Trait implemented by individual hook callbacks.
#[async_trait]
pub trait HookFn: Send + Sync {
    /// Handles `event` and returns an optional [`HookOutput`].
    ///
    /// Callbacks should be deterministic and side-effect-free inside
    /// the host process; long-running work belongs in a subprocess or
    /// a queued background task.
    async fn call(&self, event: &HookEvent) -> Option<HookOutput>;
}

/// Reasons hook registration or dispatch fails.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HookHostError {
    /// `registered_by.id` was empty (red line 10).
    #[error("hook registration must carry an ExtensionRef id (red line 10)")]
    MissingProvenance,

    /// A hook returned a mutated `tool_input` that does not satisfy
    /// the tool schema (red line 11).
    #[error("schema re-validation failed: {0}")]
    Revalidation(#[from] ValidatorError),

    /// A callback panicked. The host catches the panic and turns it
    /// into this error so the engine can fail the turn cleanly.
    #[error("hook callback panicked: {reason}")]
    CallbackPanic {
        /// Human-readable description; the original panic payload is
        /// dropped to avoid leaking sensitive data into logs.
        reason: String,
    },
}

/// Filter that limits a hook to specific event variants and/or tools.
#[derive(Debug, Clone, Default)]
pub struct HookFilter {
    /// Optional set of tool names. When empty, every tool matches.
    pub tools: Vec<String>,
}

impl HookFilter {
    /// Returns `true` when `event` should reach the registered hook.
    #[must_use]
    pub fn matches(&self, event: &HookEvent) -> bool {
        if self.tools.is_empty() {
            return true;
        }
        let name = match event {
            HookEvent::PreToolUse(p) => Some(&p.tool_name),
            HookEvent::PostToolUse(p) => Some(&p.tool_name),
            HookEvent::PostToolUseFailure(p) => Some(&p.tool_name),
            HookEvent::PermissionRequest(p) => Some(&p.tool_name),
            HookEvent::ToolApprovalChange(p) => Some(&p.tool_name),
            _ => None,
        };
        name.is_some_and(|n| self.tools.iter().any(|t| t == n))
    }
}

/// One registered hook entry.
pub struct HookRegistration {
    /// Stable id used to deregister via [`ExtensionScope`].
    pub id: RegistrationId,
    /// Owning manifest reference; required by red line 10.
    pub registered_by: ExtensionRef,
    /// Optional event-time filter.
    pub filter: HookFilter,
    /// Higher values dispatch first within the same event.
    pub priority: i32,
    /// Callback body.
    pub callback: Arc<dyn HookFn>,
}

impl std::fmt::Debug for HookRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistration")
            .field("id", &self.id)
            .field("registered_by", &self.registered_by)
            .field("filter", &self.filter)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

/// Aggregated hook callback registry.
#[derive(Default)]
pub struct HookHost {
    registrations: RwLock<Vec<HookRegistration>>,
    schemas: Arc<SchemaCache>,
    ids: RegistrationIdAllocator,
}

impl std::fmt::Debug for HookHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookHost")
            .field(
                "registration_count",
                &self.registrations.read().map_or(0, |v| v.len()),
            )
            .field("schema_count", &self.schemas.len())
            .finish_non_exhaustive()
    }
}

impl HookHost {
    /// Builds an empty host with a fresh schema cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the schema cache so callers can pre-register tool
    /// schemas at manifest load time.
    #[must_use]
    pub fn schemas(&self) -> &Arc<SchemaCache> {
        &self.schemas
    }

    /// Registers a hook callback.
    ///
    /// # Errors
    ///
    /// Returns [`HookHostError::MissingProvenance`] when
    /// `registered_by.id` is empty (red line 10).
    pub fn register(
        self: &Arc<Self>,
        registered_by: ExtensionRef,
        filter: HookFilter,
        priority: i32,
        callback: Arc<dyn HookFn>,
    ) -> Result<ExtensionScope, HookHostError> {
        if registered_by.id.trim().is_empty() {
            return Err(HookHostError::MissingProvenance);
        }
        let id = self.ids.next();
        let reg = HookRegistration {
            id,
            registered_by,
            filter,
            priority,
            callback,
        };
        let mut guard = self
            .registrations
            .write()
            .expect("registration lock poisoned");
        guard.push(reg);
        guard.sort_by_key(|r| std::cmp::Reverse(r.priority));
        drop(guard);

        let host = Arc::clone(self);
        Ok(ExtensionScope::new(id, move |id| {
            host.unregister(id);
        }))
    }

    /// Explicitly removes a registration by id.
    pub fn unregister(&self, id: RegistrationId) {
        let mut guard = self
            .registrations
            .write()
            .expect("registration lock poisoned");
        guard.retain(|r| r.id != id);
    }

    /// Dispatches `event` to every matching registration **serially**.
    ///
    /// Panics inside callbacks are caught and reported as
    /// [`HookHostError::CallbackPanic`]; the remaining hooks still run
    /// (later hooks see the unmodified event because dispatch is
    /// effectively read-only here — mutations to `tool_input` flow
    /// through the returned [`HookOutput`]).
    ///
    /// # Errors
    ///
    /// See [`HookHostError`].
    pub async fn dispatch(&self, event: &HookEvent) -> Result<Vec<HookOutput>, HookHostError> {
        let snapshot: Vec<HookRegistration> = {
            let guard = self
                .registrations
                .read()
                .expect("registration lock poisoned");
            guard
                .iter()
                .filter(|r| r.filter.matches(event))
                .map(|r| HookRegistration {
                    id: r.id,
                    registered_by: r.registered_by.clone(),
                    filter: r.filter.clone(),
                    priority: r.priority,
                    callback: Arc::clone(&r.callback),
                })
                .collect()
        };

        let mut outputs = Vec::with_capacity(snapshot.len());
        for reg in snapshot {
            let fut = std::panic::AssertUnwindSafe(reg.callback.call(event));
            match fut.catch_unwind().await {
                Ok(Some(output)) => outputs.push(output),
                Ok(None) => {}
                Err(payload) => {
                    let reason = if let Some(s) = payload.downcast_ref::<&'static str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "(non-string panic payload)".to_string()
                    };
                    return Err(HookHostError::CallbackPanic { reason });
                }
            }
        }
        Ok(outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counting {
        inner: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl HookFn for Counting {
        async fn call(&self, _event: &HookEvent) -> Option<HookOutput> {
            self.inner.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            None
        }
    }

    struct Panicking;

    #[async_trait]
    impl HookFn for Panicking {
        async fn call(&self, _event: &HookEvent) -> Option<HookOutput> {
            panic!("intentional");
        }
    }

    fn provenance(id: &str) -> ExtensionRef {
        // `ExtensionRef` is `#[non_exhaustive]`, so build it via JSON.
        serde_json::from_value(serde_json::json!({
            "id": id,
            "version": "0.1.0",
            "source": "builtin",
        }))
        .expect("static fixture must deserialise")
    }

    fn stop_event(reg: &ExtensionRef) -> HookEvent {
        // Same trick for `HookEvent::Stop`; the wire shape is stable.
        serde_json::from_value(serde_json::json!({
            "hook_event_name": "Stop",
            "sessionId": "s",
            "cwd": "/",
            "registeredBy": {
                "id": reg.id,
                "version": reg.version,
                "source": "builtin",
            },
            "stopHookActive": false,
        }))
        .expect("static fixture must deserialise")
    }

    #[tokio::test]
    async fn registration_missing_provenance_rejected() {
        let host = Arc::new(HookHost::new());
        let err = host
            .register(
                provenance(""),
                HookFilter::default(),
                0,
                Arc::new(Counting {
                    inner: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
            )
            .unwrap_err();
        assert!(matches!(err, HookHostError::MissingProvenance));
    }

    #[tokio::test]
    async fn dispatch_runs_callbacks_serially() {
        let host = Arc::new(HookHost::new());
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _scope = host
            .register(
                provenance("test"),
                HookFilter::default(),
                0,
                Arc::new(Counting {
                    inner: Arc::clone(&counter),
                }),
            )
            .unwrap();
        host.dispatch(&stop_event(&provenance("test")))
            .await
            .unwrap();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dropping_scope_deregisters_hook() {
        let host = Arc::new(HookHost::new());
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let _scope = host
                .register(
                    provenance("test"),
                    HookFilter::default(),
                    0,
                    Arc::new(Counting {
                        inner: Arc::clone(&counter),
                    }),
                )
                .unwrap();
        }
        host.dispatch(&stop_event(&provenance("test")))
            .await
            .unwrap();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn callback_panic_surfaces_as_callback_panic() {
        let host = Arc::new(HookHost::new());
        let _scope = host
            .register(
                provenance("test"),
                HookFilter::default(),
                0,
                Arc::new(Panicking),
            )
            .unwrap();
        let err = host
            .dispatch(&stop_event(&provenance("test")))
            .await
            .unwrap_err();
        assert!(matches!(err, HookHostError::CallbackPanic { .. }));
    }
}

// Rust guideline compliant 2026-02-21
