//! Thread lifecycle admin operations: delete, rename, search, and tool discovery.
//!
//! These operations sit beside the read-history surface in [`super::resume`]:
//! they are metadata-writes or metadata-reads that the engine actor can handle
//! without entering the `Turn` / `Compaction` phase.
//!
//! ## Delete ordering
//!
//! Thread deletion removes the SQL row **before** the rollout. Normal startup
//! reads thread state from SQL — a full JSONL reindex via
//! `rebuild_indexes_from_jsonl` is a manual recovery tool, never run
//! automatically — so dropping the SQL row first means a failure or crash
//! before the rollout is removed simply drops the thread from every query
//! surface, rather than leaving a dangling SQL row that lists fine but can
//! never resume. A rollout-removal failure after the SQL row is already gone is
//! logged and leaves an inert orphan file; it is not surfaced as an error.
//!
//! ## Known phase-1 limitation
//!
//! The [`PersistenceWriter`]'s in-memory `header_written` and `rollouts` sets
//! are **not** purged when a thread is deleted. Deleting a thread and
//! immediately re-creating one with the same id will produce a rollout without a
//! session header, which is recorded in the return documentation of
//! [`EngineInner::delete_thread`].

use zhive_proto::domain::{Thread, ThreadId};
use zhive_proto::rpc::ToolSpec;

use super::inner::EngineInner;
use super::submission::{DeleteError, DeleteReply, RenameError, RenameReply};
use crate::persistence::writer::StorageWriteOp;

impl EngineInner {
    /// Deletes `thread_id` from persistent storage and the in-memory thread store.
    ///
    /// The delete is refused when the thread has an active turn in flight to
    /// prevent corrupting an ongoing item-append sequence. Deletion removes the
    /// SQL row first, then the JSONL rollout (see module-level ordering note).
    ///
    /// `deleted` in the returned [`DeleteReply`] is `true` when at least the SQL
    /// row or the rollout file existed and was removed; `false` when both were
    /// absent (the thread was unknown).
    ///
    /// ## Known limitation
    ///
    /// After a successful delete the [`PersistenceWriter`]'s in-memory
    /// `header_written` and `rollouts` tracking sets still reference the old
    /// thread id. Re-creating a thread with the same id immediately after
    /// deletion will produce a rollout without a session header — behaviour is
    /// undefined until the engine process is restarted.
    ///
    /// # Errors
    ///
    /// Returns [`DeleteError::StorageUnavailable`] on an in-memory engine,
    /// [`DeleteError::ThreadHasActiveTurn`] when the thread is busy, or
    /// [`DeleteError::DeleteFailed`] on an I/O / SQL failure.
    pub(in crate::engine) async fn delete_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<DeleteReply, DeleteError> {
        let storage = self.storage().ok_or(DeleteError::StorageUnavailable)?;

        // Refuse to delete a thread that has an active turn in flight. The
        // `active_turn` mutex is held only across brief critical sections (set
        // on turn start, cleared on finish), so awaiting it here reflects the
        // live Some/None state without the false positives a `try_lock` would
        // observe while another task is mid-update.
        let has_active = match self.threads().get(&thread_id).await {
            Some(handle) => handle.active_turn.lock().await.is_some(),
            None => false,
        };
        if has_active {
            return Err(DeleteError::ThreadHasActiveTurn);
        }

        // Step 1 — remove the SQL row first (cascades to turns and items). On
        // failure the rollout is left intact, so nothing is deleted and the
        // thread stays fully consistent.
        let sql_deleted = storage.state.delete_thread(&thread_id).await.map_err(|e| {
            DeleteError::DeleteFailed {
                message: e.to_string(),
            }
        })?;

        // Step 2 — remove the JSONL rollout. If this fails after the SQL row is
        // already gone the thread is no longer visible anywhere, so we log and
        // still report success instead of resurrecting a half-deleted row; the
        // orphan file is inert until a manual reindex.
        let rollout_path = storage.rollout_path(&thread_id.0);
        let rollout_existed = match tokio::fs::remove_file(&rollout_path).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                tracing::warn!(
                    name: "zhive.engine.thread_admin.rollout_remove_failed",
                    thread_id = %thread_id.0,
                    error = %e,
                    "SQL row deleted but rollout removal failed; orphan rollout left on disk"
                );
                false
            }
        };

        // Step 3 — evict from in-memory thread store.
        self.threads().remove(&thread_id).await;

        Ok(DeleteReply {
            deleted: sql_deleted || rollout_existed,
        })
    }

    /// Renames `thread_id` by queuing a [`StorageWriteOp::SessionNameSet`] op.
    ///
    /// The rename is queued to the persistence writer asynchronously; the reply
    /// acknowledges receipt rather than waiting for the write to land on disk
    /// (same best-effort semantics as [`InjectionAck`]). An empty `name` clears
    /// the stored display name (stored as `NULL`).
    ///
    /// # Errors
    ///
    /// Returns [`RenameError::StorageUnavailable`] on an in-memory engine.
    ///
    /// [`InjectionAck`]: zhive_proto::rpc::InjectionAck
    pub(in crate::engine) fn rename_thread(
        &self,
        thread_id: ThreadId,
        name: String,
    ) -> Result<RenameReply, RenameError> {
        // Rename requires storage; silently queued ops on an in-memory engine
        // would be lost, so we surface the error explicitly.
        if self.storage().is_none() {
            return Err(RenameError::StorageUnavailable);
        }
        self.enqueue_storage_op(StorageWriteOp::SessionNameSet { thread_id, name });
        Ok(RenameReply { renamed: true })
    }

    /// Returns threads whose name, preview, or cwd matches `query`.
    ///
    /// Delegates to [`crate::persistence::StateDb::search_threads`]. Returns an
    /// empty list on an in-memory engine or on a database error (the error is
    /// logged at `warn` level). An empty `query` returns all (optionally
    /// cwd-scoped) threads, matching the behaviour of `list_threads`.
    pub(in crate::engine) async fn search_threads(
        &self,
        query: &str,
        cwd_filter: Option<&str>,
    ) -> Vec<Thread> {
        let Some(storage) = self.storage() else {
            return Vec::new();
        };
        match storage.state.search_threads(query, cwd_filter).await {
            Ok(threads) => threads,
            Err(err) => {
                tracing::warn!(
                    name: "zhive.engine.thread_admin.search_threads_failed",
                    error = %err,
                    "search_threads query failed; returning empty list"
                );
                Vec::new()
            }
        }
    }

    /// Returns the tool specs for every tool in the registry, sorted by name.
    ///
    /// Reads from the in-memory [`crate::tools::ToolRegistry`]; always available
    /// regardless of storage configuration. The `kind` field is derived from the
    /// tool's [`crate::tools::ToolKind`] via `From<domain::ToolKind>`.
    pub(in crate::engine) fn list_tools(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools()
            .iter()
            .map(|(_, tool)| {
                let kind = zhive_proto::rpc::ToolSpecKind::from(
                    zhive_proto::domain::ToolKind::from(tool.kind()),
                );
                ToolSpec::new(
                    tool.name().to_owned(),
                    tool.description(),
                    kind,
                    tool.input_schema(),
                )
            })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }
}

// Rust guideline compliant 2026-02-21
