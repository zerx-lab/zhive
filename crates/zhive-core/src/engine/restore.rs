//! Workspace file revert ("undo"): restore files and rewind the conversation.
//!
//! Two engine-side capabilities back the rewind picker:
//!
//! * [`EngineInner::list_checkpoints`] surfaces the per-turn snapshots recorded
//!   in the `turn_snapshots` projection, annotated with how many workspace files
//!   each one would revert.
//! * [`EngineInner::restore_to_checkpoint`] reverts the workspace files to a
//!   checkpoint's snapshot tree and then forks the conversation to just before
//!   the target turn — file state and conversation state rewind to the same
//!   point, avoiding the codex "files reverted but the agent still thinks it
//!   changed them" inconsistency.
//!
//! The destructive file revert runs while the engine holds the
//! [`EnginePhase::Restore`] phase (compare-and-set + a Drop guard, mirroring
//! [`super::fork`]'s `BranchSummary` claim) so it can never race a live turn.
//! The conversation fork is then delegated to the already-verified
//! [`EngineInner::fork_thread`], which manages its own phase claim.

use std::sync::Arc;
use std::time::Duration;

use zhive_proto::domain::{Checkpoint, ItemId, ThreadId, TurnId};
use zhive_proto::hook::EnginePhase;

use crate::persistence::rollout::{RolloutEntry, read_all_tolerant};
use crate::persistence::writer::StorageWriteOp;

use super::event::EngineEvent;
use super::inner::EngineInner;
use super::submission::{ForkReply, ListCheckpointsError, RestoreError, RestoreReply};

/// How long restore waits for the flush `ack` before reverting.
///
/// Bounds the wait so a wedged or shutting-down writer cannot stall the revert;
/// on timeout the revert proceeds against the current on-disk rollout, matching
/// [`super::fork`]'s best-effort flush semantics.
const RESTORE_FLUSH_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on a single snapshot `track()` so a pathologically large or slow
/// workspace cannot stall the start of a turn indefinitely.
///
/// On timeout the checkpoint is skipped (best-effort): the turn proceeds without
/// a revert point rather than blocking on git. Generous enough that a normal
/// repository always completes well within it.
const SNAPSHOT_TRACK_TIMEOUT: Duration = Duration::from_secs(30);

impl EngineInner {
    /// Captures the workspace state for a top-level turn and records it durably.
    ///
    /// No-op when snapshots are unavailable. Called once at the start of every
    /// top-level user turn, before any tool can write to disk (see
    /// [`super::turn`]).
    pub(in crate::engine) async fn capture_turn_snapshot(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        preview: String,
    ) {
        let Some(repo) = self.shadow_repo().await else {
            return;
        };
        match tokio::time::timeout(SNAPSHOT_TRACK_TIMEOUT, repo.track()).await {
            Ok(Ok(tree)) => {
                self.enqueue_storage_op(StorageWriteOp::Snapshot {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    timestamp: super::lifecycle::unix_now_pub(),
                    tree,
                    preview,
                });
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    name: "zhive.engine.snapshot.track_failed",
                    error = %err,
                    thread_id = %thread_id.0,
                    turn_id = %turn_id.0,
                    "workspace snapshot capture failed; this turn will not be revertable"
                );
            }
            Err(_) => {
                tracing::warn!(
                    name: "zhive.engine.snapshot.track_timeout",
                    thread_id = %thread_id.0,
                    turn_id = %turn_id.0,
                    "workspace snapshot capture timed out; this turn will not be revertable"
                );
            }
        }
    }

    /// Lists a thread's revertable checkpoints, oldest first.
    ///
    /// Each checkpoint's `files_changed` is computed by diffing its snapshot
    /// tree against the live workspace, so the picker can show the revert
    /// impact.
    ///
    /// # Errors
    ///
    /// * [`ListCheckpointsError::StorageUnavailable`] — no persistent storage.
    /// * [`ListCheckpointsError::ReadFailed`] — the projection read failed.
    pub(in crate::engine) async fn list_checkpoints(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Checkpoint>, ListCheckpointsError> {
        let storage = self
            .storage()
            .ok_or(ListCheckpointsError::StorageUnavailable)?;
        let mut checkpoints = storage
            .state
            .list_checkpoints(thread_id)
            .await
            .map_err(|e| ListCheckpointsError::ReadFailed {
                message: e.to_string(),
            })?;
        if let Some(repo) = self.shadow_repo().await {
            for cp in &mut checkpoints {
                if let Ok(n) = repo.changed_since(&cp.tree).await {
                    cp.files_changed = u32::try_from(n).unwrap_or(u32::MAX);
                }
            }
        }
        Ok(checkpoints)
    }

    /// Reverts the workspace to a checkpoint and rewinds the conversation.
    ///
    /// See the [module docs](self) for the full flow. Returns the new branch
    /// thread plus revert counts.
    ///
    /// # Errors
    ///
    /// * [`RestoreError::Unavailable`] — snapshots are not available.
    /// * [`RestoreError::CheckpointNotFound`] — no snapshot for `target_turn_id`.
    /// * [`RestoreError::EngineBusy`] — the engine was not `Idle`.
    /// * [`RestoreError::SnapshotFailed`] — reverting files failed.
    /// * [`RestoreError::ReplayFailed`] — the conversation fork failed.
    pub(in crate::engine) async fn restore_to_checkpoint(
        self: &Arc<Self>,
        thread_id: ThreadId,
        target_turn_id: TurnId,
    ) -> Result<RestoreReply, RestoreError> {
        let storage = Arc::clone(self.storage().ok_or(RestoreError::Unavailable)?);
        let repo = self.shadow_repo().await.ok_or(RestoreError::Unavailable)?;

        // Look up the snapshot tree for the target turn before claiming the
        // phase, so a missing checkpoint fails cheaply.
        let target_tree = storage
            .state
            .snapshot_tree_for_turn(&thread_id, &target_turn_id)
            .await
            .map_err(|e| RestoreError::SnapshotFailed {
                message: e.to_string(),
            })?
            .ok_or(RestoreError::CheckpointNotFound)?;

        // Compute the conversation truncation boundary (last item kept) before
        // touching files, from the rollout (the source of truth).
        let boundary = boundary_before_turn(&storage, &thread_id, &target_turn_id).await;

        // Claim the Restore phase so the destructive revert cannot race a turn.
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Restore) {
            return Err(RestoreError::EngineBusy {
                current: err.actual(),
            });
        }
        let _ = self.events_tx().send(EngineEvent::PhaseChanged {
            thread_id: Some(thread_id.clone()),
            from: EnginePhase::Idle,
            to: EnginePhase::Restore,
        });
        let phase_guard = RestoreGuard {
            inner: self,
            thread_id: &thread_id,
        };

        // Flush the source rollout so any in-flight snapshot/items are durable
        // before we read and revert. Best-effort with a timeout.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.enqueue_storage_op(StorageWriteOp::Flush {
            thread_id: thread_id.clone(),
            ack: Some(ack_tx),
        });
        let _ = tokio::time::timeout(RESTORE_FLUSH_ACK_TIMEOUT, ack_rx).await;

        // Revert the workspace files. Holding the phase makes this exclusive.
        let outcome =
            repo.restore_to(&target_tree)
                .await
                .map_err(|e| RestoreError::SnapshotFailed {
                    message: e.to_string(),
                });

        // Release the Restore phase before forking (fork claims its own phase).
        drop(phase_guard);

        let outcome = outcome?;
        let reverted = u32::try_from(outcome.reverted.len()).unwrap_or(u32::MAX);
        let deleted = u32::try_from(outcome.deleted.len()).unwrap_or(u32::MAX);

        tracing::info!(
            name: "zhive.engine.restore.files_reverted",
            thread_id = %thread_id.0,
            turn_id = %target_turn_id.0,
            reverted,
            deleted,
            "workspace files reverted to checkpoint"
        );

        // Fork the conversation to the checkpoint. Reuses the verified fork path
        // (its own BranchSummary phase claim + durable replay). `boundary` is the
        // last item to keep; `None` keeps the full prior history (only when the
        // target turn has no preceding item, i.e. the earliest checkpoint).
        let fork = self
            .fork_thread(thread_id.clone(), boundary, false)
            .await
            .map_err(|e| RestoreError::ReplayFailed {
                message: e.to_string(),
            })?;
        let ForkReply::Forked {
            new_thread_id,
            items_replayed,
            ..
        } = fork;

        let _ = self.events_tx().send(EngineEvent::Restored {
            source_thread_id: thread_id,
            new_thread_id: new_thread_id.clone(),
            reverted,
            deleted,
        });

        Ok(RestoreReply::Restored {
            new_thread_id,
            reverted,
            deleted,
            items_replayed,
        })
    }
}

/// Finds the id of the last item that precedes `target_turn`'s first item.
///
/// This is the inclusive truncation boundary handed to
/// [`EngineInner::fork_thread`]: keep every item up to and including it, drop
/// the target turn and everything after. Returns `None` when the target turn is
/// the first turn (no preceding item) — the fork then keeps the full history,
/// which the earliest-checkpoint case documents as a known limitation.
async fn boundary_before_turn(
    storage: &crate::persistence::Storage,
    thread_id: &ThreadId,
    target_turn: &TurnId,
) -> Option<ItemId> {
    let path = storage.rollout_path(&thread_id.0);
    let entries = read_all_tolerant(&path).await.ok()?;
    let mut last_before: Option<ItemId> = None;
    for entry in entries {
        if let RolloutEntry::Item { turn_id, item, .. } = entry {
            if turn_id == target_turn.0.as_ref() {
                return last_before;
            }
            last_before = Some(item.id().clone());
        }
    }
    last_before
}

/// Restores `Idle` from `Restore` on drop, even across a panic.
///
/// Mirrors [`super::fork`]'s `BranchSummaryGuard`: holding the rollback in a
/// guard means a panic during the revert still unwinds the phase so the engine
/// cannot wedge in `Restore` forever.
struct RestoreGuard<'a> {
    inner: &'a EngineInner,
    thread_id: &'a ThreadId,
}

impl Drop for RestoreGuard<'_> {
    fn drop(&mut self) {
        if let Err(err) = self
            .inner
            .try_set_phase_atomic(EnginePhase::Restore, EnginePhase::Idle)
        {
            tracing::error!(
                name: "zhive.engine.phase.restore_rollback_failed",
                actual = ?err.actual(),
                "engine phase was not Restore when finishing restore; state machine drift"
            );
        } else {
            let _ = self.inner.events_tx().send(EngineEvent::PhaseChanged {
                thread_id: Some(self.thread_id.clone()),
                from: EnginePhase::Restore,
                to: EnginePhase::Idle,
            });
        }
    }
}

// Rust guideline compliant 2026-02-21

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast;
    use zhive_proto::domain::{Item, ItemContent};

    use crate::persistence::writer::PersistenceWriter;
    use crate::persistence::{RolloutWriter, Storage};
    use crate::provider::ScriptedModel;

    use super::*;

    /// Builds an engine inner backed by on-disk storage, with its workspace root
    /// pinned to `work_dir` (so the shadow repo and tools agree on one root).
    async fn inner_for_workspace(
        work_dir: &std::path::Path,
    ) -> (Arc<EngineInner>, tempfile::TempDir) {
        let data = tempfile::tempdir().expect("data tempdir");
        let storage = Arc::new(Storage::open(data.path()).await.expect("open storage"));
        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));
        let (events_tx, _) = broadcast::channel::<EngineEvent>(64);
        let inner = Arc::new(EngineInner::new_with_hooks_tools_storage(
            events_tx,
            ScriptedModel::new("noop", "noop", vec![]).into_dyn(),
            Arc::new(crate::hooks::HookHost::new()),
            Arc::new(crate::tools::ToolRegistry::new()),
            crate::engine::TurnLimits::default(),
            None,
            None,
            Some(tx),
            Some(handle),
            None,
            Some(storage),
            work_dir.to_path_buf(),
        ));
        (inner, data)
    }

    fn user_item(id: &str, text: &str) -> Item {
        Item::UserMessage {
            id: ItemId(Arc::from(id)),
            content: vec![ItemContent::Text {
                text: text.to_owned(),
                annotations: None,
            }],
        }
    }

    async fn drain(inner: &EngineInner, thread_id: &ThreadId) {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        inner.enqueue_storage_op(StorageWriteOp::Flush {
            thread_id: thread_id.clone(),
            ack: Some(ack_tx),
        });
        let _ = tokio::time::timeout(Duration::from_secs(5), ack_rx).await;
    }

    /// Full path: capture a checkpoint, change files, then restore reverts the
    /// modified file, deletes the new file, and forks the conversation.
    #[tokio::test]
    async fn restore_reverts_files_and_forks_conversation() {
        let work = tempfile::tempdir().expect("work tempdir");
        let file_a = work.path().join("a.txt");
        tokio::fs::write(&file_a, b"v1").await.expect("seed a");

        let (inner, _data) = inner_for_workspace(work.path()).await;

        // Skip when git is unavailable in the test environment.
        if inner.shadow_repo().await.is_none() {
            eprintln!("skipping: shadow repo unavailable (no git)");
            return;
        }

        let thread = ThreadId(Arc::from("thread:native/restore-it"));
        let turn0 = TurnId(Arc::from("turn:thread:native/restore-it/0"));
        let turn1 = TurnId(Arc::from("turn:thread:native/restore-it/1"));

        // Seed a two-turn rollout so fork has history and a boundary exists.
        let mut w = RolloutWriter::open(inner.storage().unwrap().rollout_path(&thread.0))
            .await
            .expect("open rollout");
        w.append(&RolloutEntry::Session {
            version: 4,
            id: thread.0.to_string(),
            timestamp: 0,
            cwd: work.path().to_string_lossy().into_owned(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .expect("header");
        for (turn, item) in [
            (&turn0, user_item("item:u0", "first")),
            (&turn1, user_item("item:u1", "second")),
        ] {
            w.append(&RolloutEntry::Item {
                thread_id: thread.0.to_string(),
                turn_id: turn.0.to_string(),
                timestamp: 0,
                item: Box::new(item),
            })
            .await
            .expect("item");
        }
        w.sync_all().await.expect("sync");
        drop(w);

        // Capture the checkpoint for turn 1 against the current (v1) workspace.
        inner
            .capture_turn_snapshot(&thread, &turn1, "second".to_owned())
            .await;
        drain(&inner, &thread).await;

        // Mutate the workspace after the checkpoint.
        tokio::fs::write(&file_a, b"v2-modified")
            .await
            .expect("modify a");
        let file_b = work.path().join("b.txt");
        tokio::fs::write(&file_b, b"created later")
            .await
            .expect("create b");

        // The checkpoint should be listed with a non-zero changed-file count.
        let checkpoints = inner.list_checkpoints(&thread).await.expect("list");
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].turn_id, turn1);
        assert!(checkpoints[0].files_changed >= 1);

        // Restore to the checkpoint.
        let reply = inner
            .restore_to_checkpoint(thread.clone(), turn1.clone())
            .await
            .expect("restore ok");
        let RestoreReply::Restored {
            reverted, deleted, ..
        } = reply;
        assert!(reverted >= 1, "a.txt should be reverted");
        assert!(deleted >= 1, "b.txt should be deleted");

        // Files are back to the checkpoint state.
        assert_eq!(
            tokio::fs::read_to_string(&file_a).await.expect("read a"),
            "v1"
        );
        assert!(!file_b.exists(), "b.txt should be gone");
    }

    /// Restoring a turn with no recorded checkpoint fails cleanly.
    #[tokio::test]
    async fn restore_unknown_checkpoint_errors() {
        let work = tempfile::tempdir().expect("work tempdir");
        let (inner, _data) = inner_for_workspace(work.path()).await;
        if inner.shadow_repo().await.is_none() {
            return;
        }
        let thread = ThreadId(Arc::from("thread:native/no-cp"));
        let turn = TurnId(Arc::from("turn:thread:native/no-cp/0"));
        let err = inner
            .restore_to_checkpoint(thread, turn)
            .await
            .expect_err("should fail");
        assert!(matches!(err, RestoreError::CheckpointNotFound));
    }
}
