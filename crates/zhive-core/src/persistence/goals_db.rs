//! Goals database wrapper.
//!
//! Lightweight task / goal tracking; intentionally split from the state
//! database so the larger session index does not need to migrate every
//! time a goal-related column is added.
//!
//! All domain methods use runtime `sqlx::query("…").bind(…)` — no
//! `query!` macro, so `cargo check` needs no `DATABASE_URL`.

use std::path::Path;

use sqlx::Row as _;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

use super::error::StorageResult;

static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/goals");

/// One goal / task row.
#[derive(Debug, Clone, PartialEq)]
pub struct Goal {
    /// Auto-assigned row id; `0` when not yet persisted.
    pub id: i64,
    /// Optional thread the goal is associated with.
    pub thread_id: Option<String>,
    /// Human-readable goal description.
    pub description: String,
    /// Status string (e.g. `"pending"`, `"done"`).
    pub status: String,
    /// Unix-seconds creation timestamp.
    pub created_at: i64,
    /// Unix-seconds completion timestamp; `None` when still pending.
    pub completed_at: Option<i64>,
}

/// Thin wrapper around the goals-database pool.
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::GoalsDb;
///
/// let db = GoalsDb::open(Path::new("/tmp/demo/goals.db")).await?;
/// let id = db.add_goal(None, "write more tests", 1_000).await?;
/// db.mark_done(id, 2_000).await?;
/// let goals = db.list_goals(None).await?;
/// assert_eq!(goals.len(), 1);
/// assert_eq!(goals[0].status, "done");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct GoalsDb {
    pool: SqlitePool,
}

impl GoalsDb {
    /// Opens (or creates) the goals database at `path` and runs every
    /// pending migration.
    ///
    /// # Errors
    ///
    /// Same surface as [`super::StateDb::open`].
    pub async fn open(path: &Path) -> StorageResult<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePool::connect_with(opts).await?;
        MIGRATIONS.run(&pool).await?;
        Ok(Self { pool })
    }

    /// Returns the underlying connection pool for ad-hoc queries.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Inserts a new goal with `status = "pending"` and returns its
    /// auto-assigned row id.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::StorageError::Sqlx`] on failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::path::Path;
    /// use zhive_core::persistence::GoalsDb;
    ///
    /// let db = GoalsDb::open(Path::new("/tmp/demo/goals.db")).await?;
    /// let id = db.add_goal(Some("thread:native/01"), "finish the PR", 1_000).await?;
    /// assert!(id > 0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn add_goal(
        &self,
        thread_id: Option<&str>,
        description: &str,
        created_at: i64,
    ) -> StorageResult<i64> {
        let result = sqlx::query(
            r"
            INSERT INTO goals (thread_id, description, status, created_at)
            VALUES (?1, ?2, 'pending', ?3)
            ",
        )
        .bind(thread_id)
        .bind(description)
        .bind(created_at)
        .execute(&self.pool)
        .await?;

        Ok(result.last_insert_rowid())
    }

    /// Sets `status = "done"` and `completed_at` on the goal with the given `id`.
    ///
    /// A no-op when no row with that id exists.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::StorageError::Sqlx`] on failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::path::Path;
    /// use zhive_core::persistence::GoalsDb;
    ///
    /// let db = GoalsDb::open(Path::new("/tmp/demo/goals.db")).await?;
    /// let id = db.add_goal(None, "do the thing", 1_000).await?;
    /// db.mark_done(id, 2_000).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn mark_done(&self, id: i64, completed_at: i64) -> StorageResult<()> {
        sqlx::query(
            r"
            UPDATE goals
            SET status = 'done', completed_at = ?1
            WHERE id = ?2
            ",
        )
        .bind(completed_at)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Returns all goals optionally filtered by `thread_id`, ordered by
    /// `created_at DESC`.
    ///
    /// When `thread_id` is `None`, goals from all threads (including those
    /// without a thread) are returned.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::StorageError::Sqlx`] on failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::path::Path;
    /// use zhive_core::persistence::GoalsDb;
    ///
    /// let db = GoalsDb::open(Path::new("/tmp/demo/goals.db")).await?;
    /// let all = db.list_goals(None).await?;
    /// let for_thread = db.list_goals(Some("thread:native/01")).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_goals(&self, thread_id: Option<&str>) -> StorageResult<Vec<Goal>> {
        let rows = if let Some(tid) = thread_id {
            sqlx::query(
                r"
                SELECT id, thread_id, description, status, created_at, completed_at
                FROM goals
                WHERE thread_id = ?1
                ORDER BY created_at DESC
                ",
            )
            .bind(tid)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r"
                SELECT id, thread_id, description, status, created_at, completed_at
                FROM goals
                ORDER BY created_at DESC
                ",
            )
            .fetch_all(&self.pool)
            .await?
        };

        rows.into_iter().map(|row| row_to_goal(&row)).collect()
    }
}

fn row_to_goal(row: &sqlx::sqlite::SqliteRow) -> StorageResult<Goal> {
    Ok(Goal {
        id: row.try_get("id")?,
        thread_id: row.try_get("thread_id")?,
        description: row.try_get("description")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        completed_at: row.try_get("completed_at")?,
    })
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_temp() -> (tempfile::TempDir, GoalsDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = GoalsDb::open(&dir.path().join("goals.db")).await.unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn goals_db_add_and_list() {
        let (_dir, db) = open_temp().await;

        assert!(db.list_goals(None).await.unwrap().is_empty());

        let id1 = db
            .add_goal(Some("thread:native/01"), "goal 1", 1_000)
            .await
            .unwrap();
        let id2 = db.add_goal(None, "global goal", 2_000).await.unwrap();

        assert!(id1 > 0);
        assert!(id2 > 0);
        assert_ne!(id1, id2);

        let all = db.list_goals(None).await.unwrap();
        assert_eq!(all.len(), 2);

        let filtered = db.list_goals(Some("thread:native/01")).await.unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].description, "goal 1");
        assert_eq!(filtered[0].status, "pending");
    }

    #[tokio::test]
    async fn goals_db_mark_done() {
        let (_dir, db) = open_temp().await;

        let id = db.add_goal(None, "finish increment", 1_000).await.unwrap();
        db.mark_done(id, 2_000).await.unwrap();

        let goals = db.list_goals(None).await.unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[0].status, "done");
        assert_eq!(goals[0].completed_at, Some(2_000));
    }
}

// Rust guideline compliant 2026-02-21
