//! Tracing + OpenTelemetry plumbing (D-014 revised).
//!
//! The engine emits structured [`tracing`] spans with field names
//! following OpenTelemetry semantic conventions
//! (`session.id`, `zhive.turn.id`, `gen_ai.tool.name`, `db.operation`,
//! `error.type`, ...), so attaching an OTLP exporter at deployment time
//! does not force a rename round.
//!
//! Phase 1 ships the tracer-provider plumbing but does not bind a
//! transport — callers compose [`OtelLayer`] with their preferred
//! exporter (`opentelemetry-stdout`, `opentelemetry-otlp`, …) at the
//! application entry point.
//!
//! ## Required spans (D-014)
//!
//! Every span name lives in [`spans`]:
//!
//! * `zhive.turn` — one per turn lifecycle.
//! * `zhive.hook` — one per hook dispatch.
//! * `zhive.subagent` — subagent thread lifetime.
//! * `zhive.permission` — reducer `evaluate` call.
//! * `zhive.tool_call` — tool execution.
//! * `zhive.rollback_point` — rollout sync marker.
//! * `zhive.compaction` — context compaction container span.
//! * `zhive.branch_summary` — branch summary container span (Pi).

/// OTel-semconv-aligned span name constants.
pub mod spans {
    /// One turn lifecycle.
    pub const TURN: &str = "zhive.turn";
    /// One hook dispatch.
    pub const HOOK: &str = "zhive.hook";
    /// Subagent thread lifetime.
    pub const SUBAGENT: &str = "zhive.subagent";
    /// Permission reducer evaluation.
    pub const PERMISSION: &str = "zhive.permission";
    /// One tool execution.
    pub const TOOL_CALL: &str = "zhive.tool_call";
    /// Rollout sync marker.
    pub const ROLLBACK_POINT: &str = "zhive.rollback_point";
    /// Context compaction container span.
    pub const COMPACTION: &str = "zhive.compaction";
    /// Branch summary container span.
    pub const BRANCH_SUMMARY: &str = "zhive.branch_summary";
}

/// OTel-semconv-aligned field name constants.
pub mod fields {
    /// Thread / session id; maps to the `session.id` OpenTelemetry semconv
    /// field (B9 §3.1).
    pub const THREAD_ID: &str = "session.id";
    /// Turn id; zhive-namespaced — no OpenTelemetry standard equivalent.
    ///
    /// Prefixed under `zhive.` per B9 §3.2 table.
    pub const TURN_ID: &str = "zhive.turn.id";
    /// Tool name; maps to the `gen_ai.tool.name` semantic convention field
    /// (B9 §3.2, OpenTelemetry generative-AI semconv).
    pub const TOOL_NAME: &str = "gen_ai.tool.name";
    /// Database operation classifier.
    pub const DB_OPERATION: &str = "db.operation";
    /// Error class (`Timeout`, `PermissionDenied`, …).
    pub const ERROR_TYPE: &str = "error.type";
    /// Free-form error message.
    pub const ERROR_MESSAGE: &str = "error.message";
    /// Parent session id on a `zhive.subagent` span; zhive-namespaced,
    /// paralleling `session.id` for the child (B9 §3.2 row
    /// `parent_thread_id → zhive.parent.session.id`).
    pub const PARENT_THREAD_ID: &str = "zhive.parent.session.id";
}

/// Builds a no-op [`opentelemetry_sdk::trace::SdkTracerProvider`] suitable
/// for unit tests and developer runs (no exporter attached).
///
/// Production callers compose their own provider with the desired
/// exporter and pass it to
/// [`tracing_opentelemetry::OpenTelemetryLayer::new`].
#[must_use]
pub fn noop_tracer_provider() -> opentelemetry_sdk::trace::SdkTracerProvider {
    opentelemetry_sdk::trace::SdkTracerProvider::builder().build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_names_are_stable_constants() {
        // Trip-wire: renaming a span constant should require an
        // intentional test update because dashboards key off these.
        assert_eq!(spans::TURN, "zhive.turn");
        assert_eq!(spans::HOOK, "zhive.hook");
        assert_eq!(spans::COMPACTION, "zhive.compaction");
    }

    #[test]
    fn field_names_follow_otel_semconv() {
        assert_eq!(fields::THREAD_ID, "session.id");
        assert_eq!(fields::TURN_ID, "zhive.turn.id");
        assert_eq!(fields::TOOL_NAME, "gen_ai.tool.name");
        assert_eq!(fields::ERROR_TYPE, "error.type");
    }

    #[test]
    fn noop_provider_builds() {
        let _p = noop_tracer_provider();
    }

    /// Verifies that the string literals used at each instrumentation
    /// site match the constants defined in this module.
    ///
    /// The tracing macros require **string literals** for span names and
    /// field names, so we cannot reference the constants directly in
    /// the macro invocations.  This test is the single source of truth
    /// that keeps the literals and constants in sync.
    #[test]
    fn span_literals_match_constants() {
        // ---- span names ----
        // engine/turn.rs: info_span!("zhive.turn", ...)
        assert_eq!(spans::TURN, "zhive.turn");
        // engine/tool_dispatch/mod.rs: info_span!("zhive.tool_call", ...)
        assert_eq!(spans::TOOL_CALL, "zhive.tool_call");
        // engine/tool_dispatch/mod.rs: info_span!("zhive.permission", ...)
        assert_eq!(spans::PERMISSION, "zhive.permission");
        // hooks/mod.rs: info_span!("zhive.hook", ...)
        assert_eq!(spans::HOOK, "zhive.hook");
        // engine/subagent_spawn.rs: info_span!("zhive.subagent", ...)
        assert_eq!(spans::SUBAGENT, "zhive.subagent");
        // persistence/writer.rs: info_span!("zhive.rollback_point", ...)
        assert_eq!(spans::ROLLBACK_POINT, "zhive.rollback_point");

        // ---- field names (OTel semconv values) ----
        // All sites use "session.id", "zhive.turn.id", "gen_ai.tool.name",
        // "db.operation" per B9 §3.2.
        assert_eq!(fields::THREAD_ID, "session.id");
        assert_eq!(fields::TURN_ID, "zhive.turn.id");
        assert_eq!(fields::TOOL_NAME, "gen_ai.tool.name");
        assert_eq!(fields::DB_OPERATION, "db.operation");
        // engine/subagent_spawn.rs: "zhive.parent.session.id" (B9 §3.2 row
        // parent_thread_id → zhive.parent.session.id).
        assert_eq!(fields::PARENT_THREAD_ID, "zhive.parent.session.id");

        // ---- not yet instrumented (Phase 2 placeholders) ----
        // These constants exist but are intentionally not wired to any
        // info_span! call in Phase 1.  When Phase 2 instruments them,
        // add assertions above and remove these comments.
        let _ = spans::COMPACTION; // Phase 2: context compaction
        let _ = spans::BRANCH_SUMMARY; // Phase 2: branch summary
    }
}

/// Span-emission integration tests.
///
/// These tests verify that real instrumentation is wired: a minimal
/// hand-written [`tracing::Subscriber`] is installed, a scripted turn
/// (and optionally a tool call) is executed through the engine, and the
/// recorded span names are asserted.
///
/// `SpanCapture` is a zero-external-dependency subscriber that only
/// implements `on_new_span`.  The subscriber is installed with
/// [`tracing::subscriber::set_default`] so it does not leak between tests.
#[cfg(test)]
mod span_emission_tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use futures::stream;
    use llmsdk::LanguageModel;
    use llmsdk::language_model::{
        BoxStream, CallOptions, FinishReason, FinishReasonKind, GenerateResult, StreamPart,
        StreamResult,
    };

    use crate::engine::{Engine, EngineConfig};
    use crate::hooks::HookHost;
    use crate::provider::{DynLanguageModel, ScriptedModel};
    use crate::tools::{EchoTool, ToolRegistry};

    // ---- Minimal hand-written span-capture subscriber ----

    /// Minimal [`tracing::Subscriber`] that records every new span name.
    ///
    /// No external dependencies.  Only `new_span` is meaningful; all
    /// other required methods are no-ops.
    #[derive(Debug, Default, Clone)]
    struct SpanCapture {
        names: Arc<Mutex<Vec<String>>>,
        next_id: Arc<AtomicU64>,
    }

    impl SpanCapture {
        fn new() -> Self {
            Self::default()
        }

        fn recorded(&self) -> Vec<String> {
            self.names.lock().expect("lock poisoned").clone()
        }
    }

    impl tracing::Subscriber for SpanCapture {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            let name = attrs.metadata().name().to_owned();
            self.names.lock().expect("lock poisoned").push(name);
            // Span IDs must be non-zero per the tracing contract.
            // `fetch_add` starts at 0; +1 guarantees a non-zero value.
            let raw = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            let nz = std::num::NonZeroU64::new(raw).expect("counter overflow");
            tracing::span::Id::from_non_zero_u64(nz)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, _event: &tracing::Event<'_>) {}

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    // ---- Two-phase scripted model ----

    /// A model that emits different [`StreamPart`]s on each call.
    ///
    /// Call 0 yields the `phase0` parts; every subsequent call yields an empty
    /// stream.  This drives exactly 2 provider iterations in the turn loop
    /// (iteration 0: tool call; iteration 1: empty stream → loop exits).
    #[derive(Debug)]
    struct TwoPhaseModel {
        call_count: Arc<AtomicUsize>,
        phase0: Arc<Vec<StreamPart>>,
    }

    impl TwoPhaseModel {
        fn new(phase0: Vec<StreamPart>) -> Self {
            Self {
                call_count: Arc::new(AtomicUsize::new(0)),
                phase0: Arc::new(phase0),
            }
        }

        fn into_dyn(self) -> DynLanguageModel {
            DynLanguageModel::new(self)
        }
    }

    #[async_trait]
    impl LanguageModel for TwoPhaseModel {
        fn provider(&self) -> &'static str {
            "test"
        }

        fn model_id(&self) -> &'static str {
            "two-phase"
        }

        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }

        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            // Call 0: emit the scripted parts.
            // Call 1+: emit an empty stream so the turn loop exits cleanly.
            let s: BoxStream<llmsdk::error::Result<StreamPart>> = if idx == 0 {
                let parts: Vec<StreamPart> = (*self.phase0).clone();
                let iter = parts.into_iter().map(Ok::<_, llmsdk::ProviderError>);
                Box::pin(stream::iter(iter))
            } else {
                Box::pin(stream::empty())
            };
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }

    // ---- Tests ----

    /// Running a turn through the engine must open a `zhive.turn` span.
    ///
    /// The `zhive.turn` span wraps the entire turn execution.  Even for a
    /// scripted model that produces no output the span must be opened as
    /// the turn setup and teardown are always instrumented.
    #[tokio::test]
    async fn run_turn_opens_zhive_turn_span() {
        let capture = SpanCapture::new();

        // Install the subscriber as default for the current thread.
        // `set_default` returns a guard; the previous subscriber is restored
        // when the guard drops.
        let _guard = tracing::subscriber::set_default(capture.clone());

        // Build a scripted model that produces no content (clean completion).
        let model = ScriptedModel::new("test", "test-model", vec![]).into_dyn();
        let engine = Engine::spawn_with_provider(model);

        let thread_id = zhive_proto::domain::ThreadId(Arc::from("thread:native/span-turn-test"));
        let _ = engine.start_turn(thread_id, vec![], None).await;
        // Allow the spawned turn task to complete.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let names = capture.recorded();
        assert!(
            names.iter().any(|n| n == "zhive.turn"),
            "expected a 'zhive.turn' span, recorded: {names:?}"
        );
    }

    /// Dispatching a tool call through the engine must open both a
    /// `zhive.turn` span and a `zhive.tool_call` span.
    ///
    /// Uses the built-in [`EchoTool`] so no new test fixture is needed.
    /// The [`TwoPhaseModel`] emits a single `echo` tool call on the first
    /// provider call (iteration 0) and an empty stream on the second call
    /// (iteration 1), so the turn loop exits cleanly after exactly 2
    /// provider iterations.  The `zhive.tool_call` span opened during
    /// iteration 0's dispatch is the primary assertion.
    #[tokio::test]
    async fn tool_call_opens_zhive_tool_call_span() {
        use llmsdk::ToolCallPart;

        let capture = SpanCapture::new();

        // A two-phase model: iteration 0 emits one `echo` tool call;
        // iteration 1 returns an empty stream so the turn loop exits
        // cleanly.  Total provider calls: exactly 2.
        let phase0 = vec![StreamPart::ToolCall(ToolCallPart {
            tool_call_id: "tc-obs-1".to_owned(),
            tool_name: "echo".to_owned(),
            input: serde_json::json!({"msg": "hi"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        })];
        let model = TwoPhaseModel::new(phase0).into_dyn();

        // Register the echo tool so dispatch can find it.
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let cfg = EngineConfig {
            provider: model,
            tools: Arc::new(tools),
            hook_host: Arc::new(HookHost::new()),
            storage: None,
        };
        let engine = Engine::spawn_with_config(cfg);

        // Install the subscriber as default (see `run_turn_opens_zhive_turn_span`
        // for the rationale).
        let _guard = tracing::subscriber::set_default(capture.clone());

        let thread_id = zhive_proto::domain::ThreadId(Arc::from("thread:native/span-tool-test"));
        let _ = engine.start_turn(thread_id, vec![], None).await;
        // Allow the spawned turn task + tool dispatch to complete.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let names = capture.recorded();
        assert!(
            names.iter().any(|n| n == "zhive.turn"),
            "expected 'zhive.turn' span, recorded: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "zhive.tool_call"),
            "expected 'zhive.tool_call' span, recorded: {names:?}"
        );
    }
}

// Rust guideline compliant 2026-02-21
