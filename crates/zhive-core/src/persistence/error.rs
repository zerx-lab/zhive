//! Error type shared by every persistence module.

use thiserror::Error;

/// Failure modes the persistence layer surfaces.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StorageError {
    /// Underlying sqlx error (driver-level, query-level, or pool-level).
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// A migration failed to apply.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// Local filesystem I/O failure (rollout reads / writes).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON encoding / decoding failure on a rollout entry.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// A rollout file contained a malformed line.
    #[error("rollout corrupted at line {line}: {reason}")]
    RolloutCorrupted {
        /// 1-based line number that failed to parse.
        line: usize,
        /// Human-readable explanation.
        reason: String,
    },
}

/// Convenience [`Result`] alias used throughout the persistence layer.
pub type StorageResult<T> = Result<T, StorageError>;

// Rust guideline compliant 2026-02-21
