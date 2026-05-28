//! Persistence layer: four sibling `SQLite` databases plus a JSONL+Leaf
//! rollout (D-011 revised 2026-05-28; sqlx replaces rusqlite per user
//! decision).
//!
//! ## Layout
//!
//! ```text
//! <base>/
//! ├── state.db        (Thread / Turn / Item index)
//! ├── logs.db         (structured log sink)
//! ├── memories.db     (FTS5-backed memories)
//! ├── goals.db        (task / goal tracker)
//! └── rollouts/
//!     └── <thread_id>.jsonl
//! ```
//!
//! ## Write ordering
//!
//! JSONL rollout is the **source of truth**: every state mutation lands
//! there first (with `fsync` on save points). The SQL indices catch up
//! asynchronously and can be rebuilt from the rollout after a crash.

pub mod error;
pub mod goals_db;
pub mod logs_db;
pub mod memories_db;
pub mod rollout;
pub mod state_db;

#[doc(inline)]
pub use error::{StorageError, StorageResult};
#[doc(inline)]
pub use goals_db::GoalsDb;
#[doc(inline)]
pub use logs_db::LogsDb;
#[doc(inline)]
pub use memories_db::MemoriesDb;
#[doc(inline)]
pub use rollout::{RolloutEntry, RolloutWriter, read_all};
#[doc(inline)]
pub use state_db::StateDb;

use std::path::{Path, PathBuf};

/// Aggregate handle to the four sibling databases.
#[derive(Debug, Clone)]
pub struct Storage {
    /// Thread / Turn / Item index.
    pub state: StateDb,
    /// Structured log sink.
    pub logs: LogsDb,
    /// FTS5-backed memories.
    pub memories: MemoriesDb,
    /// Task / goal tracker.
    pub goals: GoalsDb,
    /// Base directory of the four database files and the `rollouts/`
    /// subdirectory.
    base_dir: PathBuf,
}

impl Storage {
    /// Opens (or creates) the four databases under `base_dir`.
    ///
    /// The base directory is created when missing; each database is
    /// migrated to the latest schema before [`Storage`] returns.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when any of the underlying opens or
    /// migrations fails.
    pub async fn open(base_dir: &Path) -> StorageResult<Self> {
        tokio::fs::create_dir_all(base_dir).await?;
        tokio::fs::create_dir_all(base_dir.join("rollouts")).await?;
        let state = StateDb::open(&base_dir.join("state.db")).await?;
        let logs = LogsDb::open(&base_dir.join("logs.db")).await?;
        let memories = MemoriesDb::open(&base_dir.join("memories.db")).await?;
        let goals = GoalsDb::open(&base_dir.join("goals.db")).await?;
        Ok(Self {
            state,
            logs,
            memories,
            goals,
            base_dir: base_dir.to_path_buf(),
        })
    }

    /// Returns the rollout JSONL path for `thread_id`.
    #[must_use]
    pub fn rollout_path(&self, thread_id: &str) -> PathBuf {
        let sanitised = thread_id.replace(['/', ':'], "_");
        self.base_dir
            .join("rollouts")
            .join(format!("{sanitised}.jsonl"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_creates_all_four_databases() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).await.unwrap();
        // Every pool should respond to a trivial query.
        let _: (i64,) = sqlx::query_as("SELECT 1")
            .fetch_one(storage.state.pool())
            .await
            .unwrap();
        let _: (i64,) = sqlx::query_as("SELECT 1")
            .fetch_one(storage.logs.pool())
            .await
            .unwrap();
        let _: (i64,) = sqlx::query_as("SELECT 1")
            .fetch_one(storage.memories.pool())
            .await
            .unwrap();
        let _: (i64,) = sqlx::query_as("SELECT 1")
            .fetch_one(storage.goals.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rollout_path_sanitises_identifier() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).await.unwrap();
        let p = storage.rollout_path("thread:native/01");
        assert!(p.ends_with("thread_native_01.jsonl"));
    }
}

// Rust guideline compliant 2026-02-21
