//! [`EnginePhase`] transition graph.
//!
//! [`EnginePhase`] itself lives in `zhive-proto::hook` so the wire-level
//! [`PhaseTransition`] hook payload speaks the same vocabulary as the
//! host. This module supplies the legality table the engine consults
//! before mutating its phase state.
//!
//! [`PhaseTransition`]: zhive_proto::hook::HookEvent::PhaseTransition

use zhive_proto::hook::EnginePhase;

/// Returns `true` when moving from `from` to `to` is permitted.
///
/// The graph mirrors Pi's `AgentHarnessPhase` legality table:
/// only [`EnginePhase::Idle`] can launch a heavy phase, and every heavy
/// phase must come back to `Idle` before another can start.
/// [`EnginePhase::Turn`] may detour through [`EnginePhase::Retry`] or
/// [`EnginePhase::Compaction`] without first going to `Idle`.
///
/// # Examples
///
/// ```
/// use zhive_core::engine::phase::allows_transition;
/// use zhive_proto::hook::EnginePhase;
/// assert!(allows_transition(EnginePhase::Idle, EnginePhase::Turn));
/// assert!(!allows_transition(EnginePhase::Idle, EnginePhase::Retry));
/// ```
#[must_use]
pub fn allows_transition(from: EnginePhase, to: EnginePhase) -> bool {
    use EnginePhase::{BranchSummary, Compaction, Idle, Retry, Turn};
    matches!(
        (from, to),
        (Idle, Turn | Compaction | BranchSummary)
            | (Turn, Compaction | Retry | Idle)
            | (Compaction | BranchSummary, Idle)
            | (Retry, Turn | Idle)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_can_start_heavy_phases() {
        assert!(allows_transition(EnginePhase::Idle, EnginePhase::Turn));
        assert!(allows_transition(
            EnginePhase::Idle,
            EnginePhase::Compaction
        ));
        assert!(allows_transition(
            EnginePhase::Idle,
            EnginePhase::BranchSummary
        ));
    }

    #[test]
    fn idle_cannot_jump_to_retry() {
        assert!(!allows_transition(EnginePhase::Idle, EnginePhase::Retry));
    }

    #[test]
    fn turn_can_compact_or_retry() {
        assert!(allows_transition(
            EnginePhase::Turn,
            EnginePhase::Compaction
        ));
        assert!(allows_transition(EnginePhase::Turn, EnginePhase::Retry));
    }

    #[test]
    fn heavy_phases_return_to_idle() {
        assert!(allows_transition(
            EnginePhase::Compaction,
            EnginePhase::Idle
        ));
        assert!(allows_transition(
            EnginePhase::BranchSummary,
            EnginePhase::Idle
        ));
    }
}

// Rust guideline compliant 2026-02-21
