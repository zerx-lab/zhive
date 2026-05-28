//! Memories database wrapper.
//!
//! Hosts user / project / session-scoped memories with an FTS5 index
//! for full-text search. FTS5 is part of bundled `SQLite` so no extra
//! feature gate is required.

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

use super::error::StorageResult;

static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/memories");

/// Thin wrapper around the memories-database pool.
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
}

// Rust guideline compliant 2026-02-21
