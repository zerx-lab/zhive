//! Tracing + OpenTelemetry plumbing (D-014 revised).
//!
//! The engine emits structured [`tracing`] spans with field names
//! following OpenTelemetry semantic conventions
//! (`thread.id`, `turn.id`, `tool.name`, `db.operation`, `error.type`,
//! ...), so attaching an OTLP exporter at deployment time does not
//! force a rename round.
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
    /// Thread id (e.g. `thread:native/01900…`).
    pub const THREAD_ID: &str = "thread.id";
    /// Turn id.
    pub const TURN_ID: &str = "turn.id";
    /// Tool name (e.g. `read_file`).
    pub const TOOL_NAME: &str = "tool.name";
    /// Database operation classifier.
    pub const DB_OPERATION: &str = "db.operation";
    /// Error class (`Timeout`, `PermissionDenied`, …).
    pub const ERROR_TYPE: &str = "error.type";
    /// Free-form error message.
    pub const ERROR_MESSAGE: &str = "error.message";
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
        assert_eq!(fields::THREAD_ID, "thread.id");
        assert_eq!(fields::ERROR_TYPE, "error.type");
    }

    #[test]
    fn noop_provider_builds() {
        let _p = noop_tracer_provider();
    }
}

// Rust guideline compliant 2026-02-21
