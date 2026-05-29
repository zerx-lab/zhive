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
use tracing::Instrument as _;
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
    #[error("hook registration must carry a non-empty ExtensionRef id (red line 10)")]
    MissingProvenanceId,

    /// `registered_by.version` was empty (red line 10).
    #[error("hook registration must carry a non-empty ExtensionRef version (red line 10)")]
    MissingProvenanceVersion,

    /// A hook returned a mutated `tool_input` that does not satisfy
    /// the tool schema (red line 11).
    #[error("schema re-validation failed: {0}")]
    Revalidation(#[from] ValidatorError),

    /// The shared registration list was poisoned (a previous holder of
    /// the lock panicked). The host surfaces the failure instead of
    /// continuing with a possibly-inconsistent registry.
    #[error("hook registration list is poisoned")]
    RegistryPoisoned,

    /// A callback panicked.
    ///
    /// Retained for external callers that construct or match this
    /// variant.  [`HookHost::dispatch`] no longer returns this error;
    /// panicking hooks are isolated and dispatch continues (B5 §3.4).
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
        let count = match self.registrations.read() {
            Ok(v) => v.len(),
            Err(poisoned) => poisoned.get_ref().len(),
        };
        f.debug_struct("HookHost")
            .field("registration_count", &count)
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
    /// Registrations are sorted by descending priority so dispatch
    /// always invokes the highest-priority callback first.
    ///
    /// # Errors
    ///
    /// * [`HookHostError::MissingProvenanceId`] / [`HookHostError::MissingProvenanceVersion`]
    ///   when `registered_by` is incomplete (red line 10).
    /// * [`HookHostError::RegistryPoisoned`] when a previous panic left
    ///   the internal registration lock in a poisoned state.
    pub fn register(
        self: &Arc<Self>,
        registered_by: ExtensionRef,
        filter: HookFilter,
        priority: i32,
        callback: Arc<dyn HookFn>,
    ) -> Result<ExtensionScope, HookHostError> {
        if registered_by.id.trim().is_empty() {
            return Err(HookHostError::MissingProvenanceId);
        }
        if registered_by.version.trim().is_empty() {
            return Err(HookHostError::MissingProvenanceVersion);
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
            .map_err(|_poisoned| HookHostError::RegistryPoisoned)?;
        // Insert sorted by descending priority: locate the first entry with
        // a strictly lower priority and splice in there. Stable order keeps
        // ties resolved by registration sequence.
        let insert_at = guard.partition_point(|r| r.priority >= reg.priority);
        guard.insert(insert_at, reg);
        drop(guard);

        let host = Arc::clone(self);
        Ok(ExtensionScope::new(id, move |id| {
            host.unregister(id);
        }))
    }

    /// Explicitly removes a registration by id.
    ///
    /// Silently no-ops when the registry is poisoned; callers that need
    /// the failure surface should call [`Self::register`] which returns
    /// [`HookHostError::RegistryPoisoned`] in the same situation.
    pub fn unregister(&self, id: RegistrationId) {
        let registration_id = id.0;
        let Ok(mut guard) = self.registrations.write() else {
            tracing::warn!(
                name: "zhive.hooks.unregister.poisoned",
                registration_id,
                "hook registry lock poisoned during unregister; entry retained"
            );
            return;
        };
        guard.retain(|r| r.id != id);
    }

    /// Dispatches `event` to every matching registration **serially**.
    ///
    /// Snapshot only captures the `(RegistrationId, Arc<dyn HookFn>)`
    /// pair per matching registration so the dispatch path avoids
    /// cloning the surrounding `ExtensionRef` / `HookFilter` payloads on
    /// every call. The lock is released before any callback is
    /// awaited.
    ///
    /// Opens a `zhive.hook` span with `hook.event` set to the event name
    /// string (e.g. `"PreToolUse"`, `"Stop"`, …).  The span is
    /// instrumented with `Instrument` so it is entered and exited
    /// correctly across each callback's `.await`, satisfying the `Send`
    /// bound required by `tokio::spawn`.
    ///
    /// Panics inside callbacks are caught, logged at `WARN` level
    /// (`zhive.hooks.callback_panic`), and **isolated**: the panicking
    /// hook is skipped and dispatch continues with the remaining hooks
    /// (B5 §3.4 error-isolation contract).  Later hooks still see the
    /// unmodified event because dispatch is effectively read-only here
    /// — mutations to `tool_input` flow through the returned
    /// [`HookOutput`].
    ///
    /// # Errors
    ///
    /// See [`HookHostError`].
    pub async fn dispatch(&self, event: &HookEvent) -> Result<Vec<HookOutput>, HookHostError> {
        // Extract the event name for the span field before acquiring the lock.
        // `HookEvent::hook_event_name()` is defined on the wire type; we fall
        // back to a generic string for unknown variants.
        let event_name = hook_event_name(event);

        // Open a `zhive.hook` span for this dispatch invocation.
        //
        // Span name is a literal; spans::HOOK is the single source of
        // truth — the observability test `span_literals_match_constants`
        // asserts the literal matches the constant.
        //
        // B9 §3.1 rule 3 mandates `session.id` on every span.
        let session_id = hook_session_id(event);
        let span = tracing::info_span!(
            "zhive.hook",
            "session.id" = session_id,
            "hook.event" = event_name,
        );

        self.dispatch_inner(event).instrument(span).await
    }

    /// Inner body of [`Self::dispatch`], instrumented by the caller.
    async fn dispatch_inner(&self, event: &HookEvent) -> Result<Vec<HookOutput>, HookHostError> {
        let snapshot: Vec<(RegistrationId, Arc<dyn HookFn>)> = {
            let guard = self
                .registrations
                .read()
                .map_err(|_poisoned| HookHostError::RegistryPoisoned)?;
            guard
                .iter()
                .filter(|r| r.filter.matches(event))
                .map(|r| (r.id, Arc::clone(&r.callback)))
                .collect()
        };

        let mut outputs = Vec::with_capacity(snapshot.len());
        for (_id, callback) in snapshot {
            let fut = std::panic::AssertUnwindSafe(callback.call(event));
            match fut.catch_unwind().await {
                Ok(Some(output)) => outputs.push(output),
                Ok(None) => {}
                Err(payload) => {
                    // B5 §3.4 panic-isolation: a panicking hook must not
                    // abort the entire dispatch chain.  Log the failure and
                    // continue with the remaining hooks so higher-or-equal-
                    // priority hooks are not silently suppressed.
                    let reason = if let Some(s) = payload.downcast_ref::<&'static str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "(non-string panic payload)".to_string()
                    };
                    tracing::warn!(
                        name: "zhive.hooks.callback_panic",
                        reason,
                        "hook callback panicked; isolating and continuing"
                    );
                }
            }
        }
        Ok(outputs)
    }
}

/// Extracts a stable event-name string from a [`HookEvent`] for use as a
/// span field.
///
/// The returned value is the canonical `hook_event_name` wire string
/// (e.g. `"PreToolUse"`, `"PostToolUse"`, `"Stop"`, …).  For unknown
/// `#[non_exhaustive]` variants that do not match a known arm the
/// fallback `"Unknown"` is returned.
fn hook_event_name(event: &HookEvent) -> &'static str {
    match event {
        HookEvent::PreToolUse(_) => "PreToolUse",
        HookEvent::PostToolUse(_) => "PostToolUse",
        HookEvent::PostToolUseFailure(_) => "PostToolUseFailure",
        HookEvent::UserPromptSubmit(_) => "UserPromptSubmit",
        HookEvent::PermissionRequest(_) => "PermissionRequest",
        HookEvent::ToolApprovalChange(_) => "ToolApprovalChange",
        HookEvent::Stop(_) => "Stop",
        HookEvent::Notification(_) => "Notification",
        HookEvent::Setup(_) => "Setup",
        HookEvent::SubagentStart(_) => "SubagentStart",
        HookEvent::SubagentStop(_) => "SubagentStop",
        HookEvent::PreCompact(_) => "PreCompact",
        HookEvent::SessionStart(_) => "SessionStart",
        HookEvent::SessionEnd(_) => "SessionEnd",
        HookEvent::PhaseTransition(_) => "PhaseTransition",
        // `HookEvent` is `#[non_exhaustive]`; forward-compat fallback.
        _ => "Unknown",
    }
}

/// Extracts the `session.id` from a [`HookEvent`] for use as the mandatory
/// `session.id` span field (B9 §3.1 rule 3: every span must contain
/// `session.id`).
///
/// Every [`HookEvent`] variant wraps a payload struct that carries a
/// `pub base: HookEventBase` field with `pub session_id: String`.
/// For unknown `#[non_exhaustive]` variants the empty string is returned
/// as a forward-compat fallback.
fn hook_session_id(event: &HookEvent) -> &str {
    match event {
        HookEvent::PreToolUse(p) => &p.base.session_id,
        HookEvent::PostToolUse(p) => &p.base.session_id,
        HookEvent::PostToolUseFailure(p) => &p.base.session_id,
        HookEvent::UserPromptSubmit(p) => &p.base.session_id,
        HookEvent::PermissionRequest(p) => &p.base.session_id,
        HookEvent::ToolApprovalChange(p) => &p.base.session_id,
        HookEvent::Stop(p) => &p.base.session_id,
        HookEvent::Notification(p) => &p.base.session_id,
        HookEvent::Setup(p) => &p.base.session_id,
        HookEvent::SubagentStart(p) => &p.base.session_id,
        HookEvent::SubagentStop(p) => &p.base.session_id,
        HookEvent::PreCompact(p) => &p.base.session_id,
        HookEvent::SessionStart(p) => &p.base.session_id,
        HookEvent::SessionEnd(p) => &p.base.session_id,
        HookEvent::PhaseTransition(p) => &p.base.session_id,
        // `HookEvent` is `#[non_exhaustive]`; forward-compat fallback.
        _ => "",
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
    async fn registration_missing_provenance_id_rejected() {
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
        assert!(matches!(err, HookHostError::MissingProvenanceId));
    }

    #[tokio::test]
    async fn registration_missing_provenance_version_rejected() {
        let host = Arc::new(HookHost::new());
        // Build an ExtensionRef whose `version` is empty; `id` is fine.
        let bad: ExtensionRef = serde_json::from_value(serde_json::json!({
            "id": "ext",
            "version": "",
            "source": "builtin",
        }))
        .expect("fixture");
        let err = host
            .register(
                bad,
                HookFilter::default(),
                0,
                Arc::new(Counting {
                    inner: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
            )
            .unwrap_err();
        assert!(matches!(err, HookHostError::MissingProvenanceVersion));
    }

    struct Probe {
        tag: i32,
        sink: Arc<std::sync::Mutex<Vec<i32>>>,
    }
    #[async_trait]
    impl HookFn for Probe {
        async fn call(&self, _event: &HookEvent) -> Option<HookOutput> {
            self.sink.lock().unwrap().push(self.tag);
            None
        }
    }

    #[tokio::test]
    async fn registrations_are_inserted_sorted_by_priority() {
        let host = Arc::new(HookHost::new());
        let order = Arc::new(std::sync::Mutex::new(Vec::<i32>::new()));

        // Register out of order; higher priority should dispatch first.
        let _s1 = host
            .register(
                provenance("low"),
                HookFilter::default(),
                -1,
                Arc::new(Probe {
                    tag: -1,
                    sink: Arc::clone(&order),
                }),
            )
            .unwrap();
        let _s2 = host
            .register(
                provenance("high"),
                HookFilter::default(),
                5,
                Arc::new(Probe {
                    tag: 5,
                    sink: Arc::clone(&order),
                }),
            )
            .unwrap();
        let _s3 = host
            .register(
                provenance("mid"),
                HookFilter::default(),
                0,
                Arc::new(Probe {
                    tag: 0,
                    sink: Arc::clone(&order),
                }),
            )
            .unwrap();

        host.dispatch(&stop_event(&provenance("any")))
            .await
            .unwrap();
        let recorded = order.lock().unwrap().clone();
        assert_eq!(recorded, vec![5, 0, -1]);
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

    /// A panicking hook is isolated: dispatch succeeds and returns an
    /// empty output list (the panicking hook contributes no decision),
    /// while subsequent hooks in the chain are still executed.
    #[tokio::test]
    async fn callback_panic_is_isolated_and_dispatch_continues() {
        let host = Arc::new(HookHost::new());
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Register: panic hook (priority 1) runs before the counting hook
        // (priority 0).  After the fix the counting hook must still fire.
        let _p = host
            .register(
                provenance("panic-hook"),
                HookFilter::default(),
                1,
                Arc::new(Panicking),
            )
            .unwrap();
        let _c = host
            .register(
                provenance("count-hook"),
                HookFilter::default(),
                0,
                Arc::new(Counting {
                    inner: Arc::clone(&counter),
                }),
            )
            .unwrap();

        // Dispatch must succeed (Ok) even though one hook panicked.
        let outputs = host
            .dispatch(&stop_event(&provenance("any")))
            .await
            .unwrap();
        // Panicking hook contributes no HookOutput.
        assert!(outputs.is_empty());
        // Subsequent counting hook must have run.
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

// Rust guideline compliant 2026-02-21
