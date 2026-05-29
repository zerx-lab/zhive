//! Memories database wrapper.
//!
//! Hosts user / project / session-scoped memories with an FTS5 index
//! for full-text search. FTS5 is part of bundled `SQLite` so no extra
//! feature gate is required.
//!
//! All domain methods use runtime `sqlx::query("…").bind(…)` — no
//! `query!` macro, so `cargo check` needs no `DATABASE_URL`.

use std::path::Path;

use sqlx::Row as _;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

use super::error::StorageResult;

static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/memories");

/// One memory entry, corresponding to a row in the `memories` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    /// Stable external identifier (e.g. `mem:user/uuid-v7`).
    pub id: String,
    /// Scope discriminant (e.g. `"user"`, `"project"`, `"session"`).
    pub scope: String,
    /// Plain-text content indexed by FTS5.
    pub content: String,
    /// Unix-seconds creation timestamp.
    pub created_at: i64,
    /// Unix-seconds last-update timestamp.
    pub updated_at: i64,
}

/// Thin wrapper around the memories-database pool.
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::MemoriesDb;
/// use zhive_core::persistence::memories_db::Memory;
///
/// let db = MemoriesDb::open(Path::new("/tmp/demo/memories.db")).await?;
/// let m = Memory { id: "mem:user/01".into(), scope: "user".into(),
///     content: "Rust is great".into(), created_at: 0, updated_at: 0 };
/// db.upsert_memory(&m).await?;
/// let results = db.search_memories("Rust", 10).await?;
/// assert!(!results.is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct MemoriesDb {
    pool: SqlitePool,
}

impl MemoriesDb {
    /// Opens (or creates) the memories database at `path` and runs
    /// every pending migration.
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

    /// Inserts or updates a memory row using `INSERT … ON CONFLICT`.
    ///
    /// The FTS5 triggers on the `memories` table keep `memories_fts` in
    /// sync automatically — no manual `INSERT INTO memories_fts` is needed.
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
    /// use zhive_core::persistence::MemoriesDb;
    /// use zhive_core::persistence::memories_db::Memory;
    ///
    /// let db = MemoriesDb::open(Path::new("/tmp/demo/memories.db")).await?;
    /// let m = Memory { id: "mem:user/01".into(), scope: "user".into(),
    ///     content: "The user prefers dark mode".into(),
    ///     created_at: 1_000, updated_at: 1_000 };
    /// db.upsert_memory(&m).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn upsert_memory(&self, memory: &Memory) -> StorageResult<()> {
        sqlx::query(
            r"
            INSERT INTO memories (id, scope, content, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(id) DO UPDATE SET
                scope      = excluded.scope,
                content    = excluded.content,
                updated_at = excluded.updated_at
            ",
        )
        .bind(&memory.id)
        .bind(&memory.scope)
        .bind(&memory.content)
        .bind(memory.created_at)
        .bind(memory.updated_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Full-text searches the `memories_fts` index for `query`, returning
    /// at most `limit` matches ordered by FTS5 rank (best match first).
    ///
    /// Empty `query` strings return an empty result without hitting the DB.
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
    /// use zhive_core::persistence::MemoriesDb;
    /// let db = MemoriesDb::open(Path::new("/tmp/demo/memories.db")).await?;
    /// let hits = db.search_memories("dark mode", 5).await?;
    /// for m in hits { println!("{}", m.content); }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn search_memories(&self, query: &str, limit: i64) -> StorageResult<Vec<Memory>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }

        let rows = sqlx::query(
            r"
            SELECT m.id, m.scope, m.content, m.created_at, m.updated_at
            FROM memories_fts fts
            JOIN memories m ON m.rowid = fts.rowid
            WHERE memories_fts MATCH ?1
            ORDER BY rank
            LIMIT ?2
            ",
        )
        .bind(query)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(|row| row_to_memory(&row)).collect()
    }
}

fn row_to_memory(row: &sqlx::sqlite::SqliteRow) -> StorageResult<Memory> {
    Ok(Memory {
        id: row.try_get("id")?,
        scope: row.try_get("scope")?,
        content: row.try_get("content")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn open_temp() -> (tempfile::TempDir, MemoriesDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = MemoriesDb::open(&dir.path().join("memories.db"))
            .await
            .unwrap();
        (dir, db)
    }

    fn mem(id: &str, content: &str) -> Memory {
        Memory {
            id: id.to_owned(),
            scope: "user".to_owned(),
            content: content.to_owned(),
            created_at: 1_000,
            updated_at: 1_000,
        }
    }

    #[tokio::test]
    async fn memories_db_upsert_and_search() {
        let (_dir, db) = open_temp().await;

        // Empty search returns nothing.
        assert!(db.search_memories("Rust", 10).await.unwrap().is_empty());

        db.upsert_memory(&mem("mem:user/01", "The user prefers Rust over Python"))
            .await
            .unwrap();
        db.upsert_memory(&mem("mem:user/02", "Dark mode is preferred"))
            .await
            .unwrap();

        let hits = db.search_memories("Rust", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "mem:user/01");

        let hits2 = db.search_memories("dark", 10).await.unwrap();
        assert_eq!(hits2.len(), 1);
        assert_eq!(hits2[0].id, "mem:user/02");
    }

    #[tokio::test]
    async fn memories_db_upsert_updates_existing() {
        let (_dir, db) = open_temp().await;

        db.upsert_memory(&mem("mem:user/01", "original content"))
            .await
            .unwrap();

        let mut updated = mem("mem:user/01", "updated content");
        updated.updated_at = 2_000;
        db.upsert_memory(&updated).await.unwrap();

        let hits = db.search_memories("updated", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "updated content");

        // Old content no longer matches.
        let old = db.search_memories("original", 10).await.unwrap();
        assert!(old.is_empty());
    }
}

// Rust guideline compliant 2026-02-21
