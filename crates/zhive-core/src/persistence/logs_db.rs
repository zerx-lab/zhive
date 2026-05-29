//! Logs database wrapper.
//!
//! Append-only structured log sink. Independent of the state DB so heavy
//! log volume does not bloat the main session index.
//!
//! All domain methods use runtime `sqlx::query("…").bind(…)` — no
//! `query!` macro, so `cargo check` needs no `DATABASE_URL`.

use std::path::Path;

use sqlx::Row as _;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

use super::error::StorageResult;

static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/logs");

/// Structured log entry written to and read from the logs database.
///
/// All fields correspond 1:1 to the `logs` table columns.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    /// Auto-assigned row id; `0` when the entry has not been persisted yet.
    pub id: i64,
    /// Unix-seconds timestamp at log time.
    pub timestamp: i64,
    /// Log level (e.g. `"info"`, `"warn"`, `"error"`).
    pub level: String,
    /// Module path or component that produced the log.
    pub target: String,
    /// Human-readable message.
    pub message: String,
    /// Optional thread id the log line is associated with.
    pub thread_id: Option<String>,
    /// Optional JSON blob carrying structured fields.
    pub fields: Option<String>,
}

/// Thin wrapper around the logs-database pool.
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::LogsDb;
/// use zhive_core::persistence::logs_db::LogEntry;
///
/// let db = LogsDb::open(Path::new("/tmp/demo/logs.db")).await?;
/// let entry = LogEntry { id: 0, timestamp: 0, level: "info".into(),
///     target: "test".into(), message: "hello".into(),
///     thread_id: None, fields: None };
/// db.record_log(&entry).await?;
/// let rows = db.query_logs(None, 100).await?;
/// assert_eq!(rows.len(), 1);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct LogsDb {
    pool: SqlitePool,
}

impl LogsDb {
    /// Opens (or creates) the logs database at `path` and runs every
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

    /// Appends one structured log entry to the database.
    ///
    /// The `id` field of `entry` is ignored — the database assigns the
    /// auto-increment value.
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
    /// use zhive_core::persistence::LogsDb;
    /// use zhive_core::persistence::logs_db::LogEntry;
    ///
    /// let db = LogsDb::open(Path::new("/tmp/demo/logs.db")).await?;
    /// let entry = LogEntry { id: 0, timestamp: 1_000, level: "warn".into(),
    ///     target: "engine".into(), message: "slow turn".into(),
    ///     thread_id: Some("thread:native/01".into()), fields: None };
    /// db.record_log(&entry).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn record_log(&self, entry: &LogEntry) -> StorageResult<()> {
        sqlx::query(
            r"
            INSERT INTO logs (timestamp, level, target, message, thread_id, fields)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ",
        )
        .bind(entry.timestamp)
        .bind(&entry.level)
        .bind(&entry.target)
        .bind(&entry.message)
        .bind(entry.thread_id.as_deref())
        .bind(entry.fields.as_deref())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Returns at most `limit` log entries, newest first.
    ///
    /// When `thread_id` is `Some`, only rows matching that thread are
    /// returned; when `None`, all threads are included.
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
    /// use zhive_core::persistence::LogsDb;
    ///
    /// let db = LogsDb::open(Path::new("/tmp/demo/logs.db")).await?;
    /// let all = db.query_logs(None, 50).await?;
    /// let filtered = db.query_logs(Some("thread:native/01"), 50).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn query_logs(
        &self,
        thread_id: Option<&str>,
        limit: i64,
    ) -> StorageResult<Vec<LogEntry>> {
        let rows = if let Some(tid) = thread_id {
            sqlx::query(
                r"
                SELECT id, timestamp, level, target, message, thread_id, fields
                FROM logs
                WHERE thread_id = ?1
                ORDER BY timestamp DESC
                LIMIT ?2
                ",
            )
            .bind(tid)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r"
                SELECT id, timestamp, level, target, message, thread_id, fields
                FROM logs
                ORDER BY timestamp DESC
                LIMIT ?1
                ",
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };

        rows.into_iter().map(|row| row_to_log_entry(&row)).collect()
    }
}

fn row_to_log_entry(row: &sqlx::sqlite::SqliteRow) -> StorageResult<LogEntry> {
    Ok(LogEntry {
        id: row.try_get("id")?,
        timestamp: row.try_get("timestamp")?,
        level: row.try_get("level")?,
        target: row.try_get("target")?,
        message: row.try_get("message")?,
        thread_id: row.try_get("thread_id")?,
        fields: row.try_get("fields")?,
    })
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_temp() -> (tempfile::TempDir, LogsDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = LogsDb::open(&dir.path().join("logs.db")).await.unwrap();
        (dir, db)
    }

    fn entry(msg: &str, tid: Option<&str>) -> LogEntry {
        LogEntry {
            id: 0,
            timestamp: 1_000,
            level: "info".into(),
            target: "test".into(),
            message: msg.to_owned(),
            thread_id: tid.map(String::from),
            fields: None,
        }
    }

    #[tokio::test]
    async fn logs_db_record_and_query() {
        let (_dir, db) = open_temp().await;

        // Empty initially.
        assert!(db.query_logs(None, 100).await.unwrap().is_empty());

        db.record_log(&entry("msg1", Some("thread:native/01")))
            .await
            .unwrap();
        db.record_log(&entry("msg2", None)).await.unwrap();

        // All logs.
        let all = db.query_logs(None, 100).await.unwrap();
        assert_eq!(all.len(), 2);

        // Filtered by thread_id.
        let filtered = db.query_logs(Some("thread:native/01"), 100).await.unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].message, "msg1");
    }

    #[tokio::test]
    async fn logs_db_limit_respected() {
        let (_dir, db) = open_temp().await;

        for i in 0..5 {
            db.record_log(&entry(&format!("msg{i}"), None))
                .await
                .unwrap();
        }

        let rows = db.query_logs(None, 3).await.unwrap();
        assert_eq!(rows.len(), 3);
    }
}

// Rust guideline compliant 2026-02-21
