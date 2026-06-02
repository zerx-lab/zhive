//! Shared bridge state: ACP `SessionId` ↔ zhive `ThreadId` mapping.
//!
//! [`AgentState`] is the single mutable handle threaded through every ACP
//! callback. It is an `Arc<Mutex<…>>` so it can be cloned cheaply into each
//! closure; the lock is only ever held for a map lookup or insert, never across
//! an `await` on the engine.
//!
//! The bridge mints a fresh [`ThreadId`] per ACP session at `session/new`, so
//! one editor session maps to exactly one engine thread for the lifetime of the
//! connection. The reverse map (thread → session) lets the prompt loop label
//! outbound `session/update` notifications without re-deriving the id.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::{ClientCapabilities, SessionId};
use zhive_proto::domain::ThreadId;

/// Per-session bookkeeping held by the bridge.
#[derive(Debug, Clone)]
struct SessionEntry {
    thread_id: ThreadId,
    #[expect(dead_code, reason = "retained for future fs/terminal routing (v1.1)")]
    cwd: PathBuf,
}

/// Inner, lock-guarded state.
#[derive(Debug, Default)]
struct Inner {
    sessions: HashMap<SessionId, SessionEntry>,
    client_capabilities: ClientCapabilities,
}

/// Cloneable handle to the bridge's session map and negotiated capabilities.
///
/// Clones share one underlying map (`Arc`). Cheap to pass into every callback.
///
/// # Examples
///
/// ```
/// use zhive_bridge_acp::state::AgentState;
/// let state = AgentState::new();
/// let session = state.new_session("/tmp");
/// assert!(state.thread_for_session(&session).is_some());
/// ```
#[derive(Debug, Clone, Default)]
pub struct AgentState {
    inner: Arc<Mutex<Inner>>,
}

impl AgentState {
    /// Creates an empty bridge state.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_bridge_acp::state::AgentState;
    /// let state = AgentState::new();
    /// assert!(state.thread_for_session(&"missing".into()).is_none());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the client capabilities negotiated at `initialize`.
    ///
    /// # Panics
    ///
    /// Panics only if the internal lock is poisoned by a prior panic, which
    /// would already mean the process is unwinding.
    pub fn set_client_capabilities(&self, caps: ClientCapabilities) {
        let mut inner = self.lock();
        inner.client_capabilities = caps;
    }

    /// Returns the client capabilities negotiated at `initialize`.
    #[must_use]
    pub fn client_capabilities(&self) -> ClientCapabilities {
        self.lock().client_capabilities.clone()
    }

    /// Mints a fresh ACP session bound to a new zhive thread and stores it.
    ///
    /// Returns the new [`SessionId`]. The thread id is derived as
    /// `thread:acp/<uuid-like>` so logs filter bridge-origin threads at a
    /// glance (matching [`zhive_proto::domain::Provenance::Acp`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_bridge_acp::state::AgentState;
    /// let state = AgentState::new();
    /// let a = state.new_session("/work");
    /// let b = state.new_session("/work");
    /// assert_ne!(a, b, "each session gets a unique id");
    /// ```
    #[must_use]
    pub fn new_session(&self, cwd: impl Into<PathBuf>) -> SessionId {
        let unique = next_id();
        let session_id = SessionId::new(Arc::<str>::from(format!("acp-{unique}").as_str()));
        let thread_id = ThreadId(Arc::from(format!("thread:acp/{unique}").as_str()));
        let mut inner = self.lock();
        inner.sessions.insert(
            session_id.clone(),
            SessionEntry {
                thread_id,
                cwd: cwd.into(),
            },
        );
        session_id
    }

    /// Returns the zhive [`ThreadId`] bound to `session`, if any.
    #[must_use]
    pub fn thread_for_session(&self, session: &SessionId) -> Option<ThreadId> {
        self.lock()
            .sessions
            .get(session)
            .map(|e| e.thread_id.clone())
    }
}

impl AgentState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means another task already panicked; recover the
        // guard rather than cascading a second panic into the connection loop.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Monotonic counter used to mint unique session / thread suffixes.
///
/// A process-local counter is sufficient: each `zhive acp` subprocess serves a
/// single editor, so ids only need to be unique within one connection.
static SESSION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_id() -> u64 {
    SESSION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_round_trips() {
        let state = AgentState::new();
        let session = state.new_session("/tmp/work");
        let thread = state.thread_for_session(&session).expect("bound");
        assert!(thread.0.starts_with("thread:acp/"));
    }

    #[test]
    fn unknown_session_returns_none() {
        let state = AgentState::new();
        assert!(state.thread_for_session(&SessionId::new("nope")).is_none());
    }

    #[test]
    fn sessions_are_unique() {
        let state = AgentState::new();
        let a = state.new_session("/w");
        let b = state.new_session("/w");
        assert_ne!(a, b);
        assert_ne!(
            state.thread_for_session(&a),
            state.thread_for_session(&b),
            "distinct sessions bind distinct threads"
        );
    }

    #[test]
    fn capabilities_persist() {
        let state = AgentState::new();
        state.set_client_capabilities(ClientCapabilities::default());
        // Round-trips without panicking; the default has no fs/terminal flags.
        let _ = state.client_capabilities();
    }
}

// Rust guideline compliant 2026-02-21
