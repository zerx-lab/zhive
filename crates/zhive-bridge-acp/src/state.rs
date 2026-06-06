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
//!
//! ## Session lifetime and cleanup
//!
//! ACP has no `session/close` message; a session is implicitly alive for the
//! duration of the connection. The primary cleanup path is
//! [`AgentState::remove_session`], called when the engine emits
//! [`EngineEvent::SessionAborted`] — at that point the underlying thread is
//! gone and the session can never receive another turn.
//!
//! As a safety net against clients that disconnect without triggering an abort,
//! [`MAX_SESSIONS`] caps the total number of live entries: inserting a new
//! session beyond the cap evicts the oldest entry (insertion-order LRU via a
//! [`VecDeque`] shadow queue). Active in-flight sessions are not protected from
//! eviction by the cap alone; the evicted entry simply becomes unknown to the
//! bridge, which returns a JSON-RPC error for any subsequent prompt — a safe
//! degradation.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::{ClientCapabilities, SessionId};
use zhive_proto::domain::{ThinkingEffort, ThreadId};

/// Maximum number of concurrent ACP sessions tracked in the bridge.
///
/// A single `zhive acp` process serves one editor connection. Editors
/// typically open one session per workspace window, so 1 024 provides a
/// generous upper bound while guaranteeing the session map never grows
/// without limit. When the cap is reached the oldest (by insertion order)
/// entry is evicted before a new one is inserted. Any subsequent prompt
/// for an evicted session receives a JSON-RPC "unknown session" error,
/// which is the same response as for any other unknown id.
///
/// Raise this constant if a use-case is found that legitimately requires
/// more concurrent sessions per connection; lower it to tighten memory
/// guarantees. Changing it does not affect the protocol wire format.
pub const MAX_SESSIONS: usize = 1_024;

/// Per-session bookkeeping held by the bridge.
#[derive(Debug, Clone)]
struct SessionEntry {
    thread_id: ThreadId,
    #[expect(dead_code, reason = "retained for future fs/terminal routing (v1.1)")]
    cwd: PathBuf,
    /// Reasoning depth chosen for this session via `session/set_config_option`.
    ///
    /// `None` means the client never picked one, so the turn runs without an
    /// explicit reasoning override (the engine default). Per-session because the
    /// effort is a per-turn parameter, unlike the model, which is engine-global.
    effort: Option<ThinkingEffort>,
}

/// Inner, lock-guarded state.
#[derive(Debug)]
struct Inner {
    sessions: HashMap<SessionId, SessionEntry>,
    /// Insertion-order queue used to evict the oldest session when
    /// [`MAX_SESSIONS`] is reached. Always kept in sync with `sessions`.
    eviction_queue: VecDeque<SessionId>,
    client_capabilities: ClientCapabilities,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            sessions: HashMap::with_capacity(MAX_SESSIONS),
            eviction_queue: VecDeque::with_capacity(MAX_SESSIONS),
            client_capabilities: ClientCapabilities::default(),
        }
    }
}

/// Cloneable handle to the bridge's session map and negotiated capabilities.
///
/// Clones share one underlying map (`Arc`). Cheap to pass into every callback.
/// Sessions are bounded by [`MAX_SESSIONS`]; older entries are evicted when the
/// cap is exceeded. Use [`remove_session`](AgentState::remove_session) to clean
/// up a session when its engine thread terminates.
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
    /// If inserting this session would exceed [`MAX_SESSIONS`], the oldest
    /// session (by insertion order) is evicted first to keep the map bounded.
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

        // Evict the oldest session when the cap is reached so the map stays
        // bounded. The evicted session becomes unknown to the bridge; any
        // subsequent prompt for it receives a "unknown session" error.
        if inner.sessions.len() >= MAX_SESSIONS
            && let Some(oldest) = inner.eviction_queue.pop_front()
        {
            inner.sessions.remove(&oldest);
            tracing::warn!(
                name: "zhive.acp.session.evicted",
                session = %oldest.0,
                cap = MAX_SESSIONS,
                "session evicted; MAX_SESSIONS cap reached"
            );
        }

        inner.sessions.insert(
            session_id.clone(),
            SessionEntry {
                thread_id,
                cwd: cwd.into(),
                effort: None,
            },
        );
        inner.eviction_queue.push_back(session_id.clone());
        session_id
    }

    /// Removes a session from the bridge state.
    ///
    /// Cleans up the map entry and the eviction-queue slot for `session_id`.
    /// This is the primary cleanup path: call it whenever the engine emits
    /// [`EngineEvent::SessionAborted`] for the bound thread, at which point
    /// the session can never receive another turn.
    ///
    /// If `session_id` is unknown (already removed or never inserted) the call
    /// is a no-op.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_bridge_acp::state::AgentState;
    /// let state = AgentState::new();
    /// let s = state.new_session("/work");
    /// assert!(state.thread_for_session(&s).is_some());
    /// state.remove_session(&s);
    /// assert!(state.thread_for_session(&s).is_none());
    /// ```
    pub fn remove_session(&self, session_id: &SessionId) {
        let mut inner = self.lock();
        if inner.sessions.remove(session_id).is_some() {
            // Keep the eviction queue in sync. Linear scan is acceptable here:
            // `remove_session` is called at most once per session lifetime
            // (on abort), not in a hot loop. The queue is bounded by
            // MAX_SESSIONS so the scan is O(MAX_SESSIONS) at worst.
            inner.eviction_queue.retain(|id| id != session_id);
        }
    }

    /// Returns the zhive [`ThreadId`] bound to `session`, if any.
    #[must_use]
    pub fn thread_for_session(&self, session: &SessionId) -> Option<ThreadId> {
        self.lock()
            .sessions
            .get(session)
            .map(|e| e.thread_id.clone())
    }

    /// Records the reasoning depth chosen for `session`.
    ///
    /// Set from a `session/set_config_option` request; read back per prompt to
    /// drive `engine.start_turn_with_reasoning`. A no-op for an unknown session.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_bridge_acp::state::AgentState;
    /// use zhive_proto::domain::ThinkingEffort;
    /// let state = AgentState::new();
    /// let s = state.new_session("/work");
    /// state.set_session_effort(&s, ThinkingEffort::High);
    /// assert_eq!(state.session_effort(&s), Some(ThinkingEffort::High));
    /// ```
    pub fn set_session_effort(&self, session: &SessionId, effort: ThinkingEffort) {
        let mut inner = self.lock();
        if let Some(entry) = inner.sessions.get_mut(session) {
            entry.effort = Some(effort);
        }
    }

    /// Returns the reasoning depth chosen for `session`, if any.
    ///
    /// `None` means the client never picked one, so the turn runs with the
    /// engine's default reasoning.
    #[must_use]
    pub fn session_effort(&self, session: &SessionId) -> Option<ThinkingEffort> {
        self.lock().sessions.get(session).and_then(|e| e.effort)
    }

    /// Replaces the thread bound to `session` with a brand-new one.
    ///
    /// Used by the `/new` and `/clear` slash commands: the session keeps its
    /// ACP id but all subsequent prompts go to a fresh engine thread, effectively
    /// clearing the conversation history. The old thread persists in engine
    /// storage but the bridge drops its reference.
    ///
    /// The per-session reasoning depth is also reset so the new thread starts
    /// with the engine default.
    ///
    /// Returns the new [`ThreadId`], or `None` when `session_id` is not known.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_bridge_acp::state::AgentState;
    ///
    /// let state = AgentState::new();
    /// let session = state.new_session("/work");
    /// let old_thread = state.thread_for_session(&session).unwrap();
    /// let new_thread = state.rebind_session(&session).unwrap();
    /// assert_ne!(old_thread, new_thread, "rebind must produce a distinct thread");
    /// assert_eq!(state.thread_for_session(&session), Some(new_thread));
    /// ```
    #[must_use]
    pub fn rebind_session(&self, session_id: &SessionId) -> Option<ThreadId> {
        let mut inner = self.lock();
        let entry = inner.sessions.get_mut(session_id)?;
        let unique = next_id();
        let new_thread = ThreadId(Arc::from(format!("thread:acp/{unique}").as_str()));
        entry.thread_id = new_thread.clone();
        entry.effort = None;
        Some(new_thread)
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

    // --- removal tests ---

    #[test]
    fn remove_session_clears_entry() {
        let state = AgentState::new();
        let a = state.new_session("/w");
        let b = state.new_session("/w");
        let c = state.new_session("/w");

        // Remove the middle entry; the others must remain intact.
        state.remove_session(&b);

        assert!(
            state.thread_for_session(&b).is_none(),
            "removed session must be gone"
        );
        assert!(
            state.thread_for_session(&a).is_some(),
            "sibling session a must still be present"
        );
        assert!(
            state.thread_for_session(&c).is_some(),
            "sibling session c must still be present"
        );
    }

    #[test]
    fn remove_unknown_session_is_noop() {
        let state = AgentState::new();
        let session = state.new_session("/w");
        // Removing an unknown id must not panic or disturb existing entries.
        state.remove_session(&SessionId::new("ghost"));
        assert!(
            state.thread_for_session(&session).is_some(),
            "existing session must be unaffected"
        );
    }

    #[test]
    fn remove_session_twice_is_noop() {
        let state = AgentState::new();
        let s = state.new_session("/w");
        state.remove_session(&s);
        // Second call must be a no-op (no panic).
        state.remove_session(&s);
        assert!(state.thread_for_session(&s).is_none());
    }

    // --- LRU cap tests ---

    /// Inserts `MAX_SESSIONS + 1` sessions and verifies the oldest is evicted.
    #[test]
    fn lru_cap_evicts_oldest() {
        let state = AgentState::new();
        // Fill the map to capacity.
        let first = state.new_session("/w");
        for _ in 1..MAX_SESSIONS {
            let _ = state.new_session("/w");
        }
        // The map is now full; the very next insert must evict `first`.
        let extra = state.new_session("/w");
        assert!(
            state.thread_for_session(&first).is_none(),
            "oldest session must be evicted when cap is reached"
        );
        assert!(
            state.thread_for_session(&extra).is_some(),
            "newly inserted session must be present"
        );
    }

    /// Active recent sessions are not evicted when only the very oldest is
    /// pushed out by one new insert.
    #[test]
    fn lru_cap_preserves_recent_sessions() {
        // Start fresh so we can track the second inserted.
        let state2 = AgentState::new();
        let _first2 = state2.new_session("/w");
        let second2 = state2.new_session("/w");
        for _ in 2..MAX_SESSIONS {
            let _ = state2.new_session("/w");
        }
        // Trigger eviction of _first2.
        let _ = state2.new_session("/w");
        // second2 must still be alive after _first2 was evicted.
        assert!(
            state2.thread_for_session(&second2).is_some(),
            "second-oldest session must survive a single eviction"
        );
    }
}

// Rust guideline compliant 2026-02-21
