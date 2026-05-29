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
pub mod writer;

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
    /// migrated to the latest schema before [`Storage`] returns. The
    /// four opens run concurrently via [`tokio::try_join!`] because they
    /// touch independent files; total wall time is bounded by the
    /// slowest migration rather than their sum.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when any of the underlying opens or
    /// migrations fails.
    pub async fn open(base_dir: &Path) -> StorageResult<Self> {
        tokio::fs::create_dir_all(base_dir).await?;
        tokio::fs::create_dir_all(base_dir.join("rollouts")).await?;
        let state_path = base_dir.join("state.db");
        let logs_path = base_dir.join("logs.db");
        let memories_path = base_dir.join("memories.db");
        let goals_path = base_dir.join("goals.db");
        let (state, logs, memories, goals) = tokio::try_join!(
            StateDb::open(&state_path),
            LogsDb::open(&logs_path),
            MemoriesDb::open(&memories_path),
            GoalsDb::open(&goals_path),
        )?;
        Ok(Self {
            state,
            logs,
            memories,
            goals,
            base_dir: base_dir.to_path_buf(),
        })
    }

    /// Returns the rollout JSONL path for `thread_id`.
    ///
    /// The id is reduced to an allowlist of `[A-Za-z0-9_-]` characters so a
    /// malicious or malformed id cannot escape the `rollouts/` directory or
    /// pick a reserved filename. Empty inputs and inputs that collapse to an
    /// empty string both fall back to the [`EMPTY_SENTINEL`]; an input that
    /// happens to equal that sentinel literally is rewritten to
    /// [`EMPTY_SENTINEL_LITERAL_DISAMBIGUATOR`] to keep the two cases on
    /// distinct files.
    #[must_use]
    pub fn rollout_path(&self, thread_id: &str) -> PathBuf {
        self.base_dir
            .join("rollouts")
            .join(format!("{}.jsonl", sanitise_thread_id(thread_id)))
    }
}

/// Sentinel used by [`sanitise_thread_id`] when the input would
/// otherwise produce an empty filename stem.
///
/// The string itself is in the allowlist (`_` is allowed verbatim), so
/// a benign caller could in theory supply this literal. The sanitiser
/// therefore detects the collision explicitly and rewrites a literal
/// `__empty__` input to a distinct stem so the two are kept apart on
/// disk.
const EMPTY_SENTINEL: &str = "__empty__";

/// Disambiguator appended to a literal `__empty__` input so it does not
/// share a rollout file with the synthetic empty fallback.
const EMPTY_SENTINEL_LITERAL_DISAMBIGUATOR: &str = "__empty__literal__";

/// Maps an arbitrary thread id to a filesystem-safe filename stem.
///
/// Characters outside `[A-Za-z0-9_-]` become `_`. Two corner cases:
///
/// * an input that sanitises to an empty string is mapped to
///   [`EMPTY_SENTINEL`].
/// * an input that already equals [`EMPTY_SENTINEL`] verbatim (allowed
///   because `_` is in the allowlist) is rewritten to
///   [`EMPTY_SENTINEL_LITERAL_DISAMBIGUATOR`] so it does not share a
///   rollout file with the synthetic empty fallback.
fn sanitise_thread_id(thread_id: &str) -> String {
    let mut out = String::with_capacity(thread_id.len());
    for ch in thread_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        EMPTY_SENTINEL.to_string()
    } else if out == EMPTY_SENTINEL {
        EMPTY_SENTINEL_LITERAL_DISAMBIGUATOR.to_string()
    } else {
        out
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

    #[test]
    fn sanitise_thread_id_rejects_traversal_and_separators() {
        assert_eq!(sanitise_thread_id("../etc/passwd"), "___etc_passwd");
        assert_eq!(sanitise_thread_id("a\\b"), "a_b");
        assert_eq!(sanitise_thread_id("a\0b"), "a_b");
        assert_eq!(sanitise_thread_id("a b"), "a_b");
        assert_eq!(sanitise_thread_id("ok-name_1"), "ok-name_1");
    }

    #[test]
    fn sanitise_thread_id_handles_empty_input() {
        assert_eq!(sanitise_thread_id(""), EMPTY_SENTINEL);
        // A string of only forbidden characters maps to a sequence of
        // underscores (one per char), NOT the empty sentinel: this is
        // a different input shape and should produce a distinct
        // filename.
        assert_eq!(sanitise_thread_id("////"), "____");
        assert_ne!(sanitise_thread_id("////"), EMPTY_SENTINEL);
    }

    #[test]
    fn sanitise_thread_id_does_not_collide_with_sentinel() {
        // A benign caller-supplied id without underscores cannot collide
        // (still good as a smoke check).
        assert_ne!(sanitise_thread_id("empty"), EMPTY_SENTINEL);

        // The non-trivial case: an input that already equals the
        // sentinel literally (allowed because `_` is in the allowlist)
        // MUST be rewritten so the empty fallback and the literal input
        // get distinct filenames.
        let literal = sanitise_thread_id(EMPTY_SENTINEL);
        assert_ne!(literal, EMPTY_SENTINEL);
        assert_eq!(literal, EMPTY_SENTINEL_LITERAL_DISAMBIGUATOR);

        // And of course the empty input still produces the sentinel.
        assert_eq!(sanitise_thread_id(""), EMPTY_SENTINEL);
    }
}

// Rust guideline compliant 2026-02-21
