//! Goals database wrapper.
//!
//! Lightweight task / goal tracking; intentionally split from the state
//! database so the larger session index does not need to migrate every
//! time a goal-related column is added.

use std::path::Path;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

use super::error::StorageResult;

static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/goals");

/// Thin wrapper around the goals-database pool.
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
}

// Rust guideline compliant 2026-02-21
