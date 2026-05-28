//! Cancellation propagation tree.
//!
//! The engine maintains a hierarchy of [`CancellationToken`] values
//! mirroring the Engine → Turn → `ToolCall` / Hook / Subagent topology
//! described in B7. Tokens are arranged as parent / child pairs: when
//! a parent token fires, every descendant fires automatically; child
//! firings stay scoped.
//!
//! Phase 1 keeps the surface minimal — the engine actor consumes
//! [`CancellationTree::root`] to plumb cancellation through the agent
//! loop, while individual tool dispatchers obtain scoped children via
//! [`CancellationTree::child_for_turn`] / [`Self::child_for_tool`].

use tokio_util::sync::CancellationToken;

/// Root of the engine-wide cancellation hierarchy.
#[derive(Debug, Clone, Default)]
pub struct CancellationTree {
    root: CancellationToken,
}

impl CancellationTree {
    /// Builds a fresh, un-cancelled tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the root token (engine-wide cancellation).
    #[must_use]
    pub fn root(&self) -> CancellationToken {
        self.root.clone()
    }

    /// Returns a scoped child token for one turn.
    ///
    /// Firing the child cancels the turn but **not** the engine;
    /// firing the engine root cancels every turn.
    #[must_use]
    pub fn child_for_turn(&self) -> CancellationToken {
        self.root.child_token()
    }

    /// Returns a scoped child token for one tool call (parent = turn).
    #[must_use]
    pub fn child_for_tool(parent: &CancellationToken) -> CancellationToken {
        parent.child_token()
    }

    /// Cancels the entire tree.
    pub fn cancel_all(&self) {
        self.root.cancel();
    }

    /// Returns `true` when the root has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.root.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn child_inherits_parent_cancel() {
        let tree = CancellationTree::new();
        let turn = tree.child_for_turn();
        let tool = CancellationTree::child_for_tool(&turn);
        assert!(!tool.is_cancelled());

        tree.cancel_all();
        assert!(turn.is_cancelled());
        assert!(tool.is_cancelled());
    }

    #[tokio::test]
    async fn child_cancel_does_not_propagate_to_parent() {
        let tree = CancellationTree::new();
        let turn = tree.child_for_turn();
        turn.cancel();
        assert!(turn.is_cancelled());
        assert!(!tree.is_cancelled());
    }
}

// Rust guideline compliant 2026-02-21
