//! State database wrapper (Thread / Turn / Item index).
//!
//! Holds the queryable projection of the JSONL rollout: list threads,
//! fetch turn metadata, look up an item by id. Heavy reads stay here so
//! the rollout file can grow large without slowing the UI.

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

use super::error::StorageResult;

static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/state");

/// Thin wrapper around the state-database pool.
#[derive(Debug, Clone)]
pub struct StateDb {
    pool: SqlitePool,
}

impl StateDb {
    /// Opens (or creates) the state database at `path` and runs every
    /// pending migration.
    ///
    /// # Errors
    ///
    /// Returns [`super::error::StorageError::Sqlx`] when the underlying
    /// `SQLite` open fails, or
    /// [`super::error::StorageError::Migrate`] when a migration cannot
    /// be applied.
    pub async fn open(path: &Path) -> StorageResult<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true);
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
