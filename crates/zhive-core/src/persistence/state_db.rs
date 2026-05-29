//! State database wrapper (Thread / Turn / Item index).
//!
//! Holds the queryable projection of the JSONL rollout: list threads,
//! fetch turn metadata, look up an item by id. Heavy reads stay here so
//! the rollout file can grow large without slowing the UI.
//!
//! All write methods use runtime `sqlx::query("…").bind(…)` (never the
//! `query!` macro) so `cargo check` needs no `DATABASE_URL` environment
//! variable.

use std::path::Path;

use sqlx::Row as _;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use zhive_proto::domain::{
    Item, Thread, ThreadId, ThreadSource, ThreadStatus, TurnError, TurnId, TurnStatus,
};

use super::error::StorageResult;

static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/state");

/// Thin wrapper around the state-database pool.
///
/// All domain methods return [`StorageResult`].  The pool is `Clone + Send +
/// Sync` so this wrapper can be shared across tokio tasks without extra
/// wrapping.
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::StateDb;
/// let db = StateDb::open(Path::new("/tmp/demo/state.db")).await?;
/// let threads = db.list_threads().await?;
/// assert!(threads.is_empty());
/// # Ok(())
/// # }
/// ```
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
    /// Returns [`StorageError::Sqlx`] when the underlying
    /// `SQLite` open fails, or
    /// [`StorageError::Migrate`] when a migration cannot be applied.
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

    // ------------------------------------------------------------------
    // Thread methods
    // ------------------------------------------------------------------

    /// Inserts or updates a [`Thread`] row using an `INSERT … ON CONFLICT`
    /// upsert, so it is safe to call unconditionally on every state change.
    ///
    /// Enum fields (`status`, `source`) are serialised to their lower-case
    /// wire strings via [`serde_json`]; the `ephemeral` bool maps to the
    /// `SQLite` integer `0` / `1` convention.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Sqlx`] on a database-level failure, or
    /// [`StorageError::Json`] when a field serialisation fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::path::{Path, PathBuf};
    /// use std::sync::Arc;
    /// use zhive_core::persistence::StateDb;
    /// use zhive_proto::domain::{Thread, ThreadId, ThreadSource, ThreadStatus};
    ///
    /// let db = StateDb::open(Path::new("/tmp/demo/state.db")).await?;
    /// let t = Thread {
    ///     id: ThreadId(Arc::from("thread:native/01")),
    ///     session_id: None, forked_from: None,
    ///     preview: "hello".into(), ephemeral: false,
    ///     model_provider: "anthropic".into(),
    ///     created_at: 0, updated_at: 0,
    ///     status: ThreadStatus::Idle,
    ///     cwd: PathBuf::from("/"),
    ///     source: ThreadSource::User,
    ///     name: None, turns: vec![],
    /// };
    /// db.upsert_thread(&t).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn upsert_thread(&self, thread: &Thread) -> StorageResult<()> {
        let status = thread_status_to_str(&thread.status);
        let source = thread_source_to_str(thread.source);
        let ephemeral: i64 = i64::from(thread.ephemeral);
        let cwd = thread.cwd.to_str().unwrap_or("").to_owned();
        let session_id = thread.session_id.as_ref().map(|s| s.0.as_ref().to_owned());
        let forked_from = thread.forked_from.as_ref().map(|s| s.0.as_ref().to_owned());

        sqlx::query(
            r"
            INSERT INTO threads
                (id, session_id, forked_from, preview, ephemeral, model_provider,
                 created_at, updated_at, status, cwd, source, name)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            ON CONFLICT(id) DO UPDATE SET
                session_id    = excluded.session_id,
                forked_from   = excluded.forked_from,
                preview       = excluded.preview,
                ephemeral     = excluded.ephemeral,
                model_provider = excluded.model_provider,
                updated_at    = excluded.updated_at,
                status        = excluded.status,
                cwd           = excluded.cwd,
                source        = excluded.source,
                name          = excluded.name
            ",
        )
        .bind(thread.id.0.as_ref())
        .bind(session_id)
        .bind(forked_from)
        .bind(&thread.preview)
        .bind(ephemeral)
        .bind(&thread.model_provider)
        .bind(thread.created_at)
        .bind(thread.updated_at)
        .bind(status)
        .bind(cwd)
        .bind(source)
        .bind(thread.name.as_deref())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Inserts a new turn row with `status = InProgress` and `started_at`
    /// set to the given Unix-seconds timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Sqlx`] on failure (including foreign-key
    /// violations when the parent thread row is absent).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::sync::Arc;
    /// use std::path::Path;
    /// use zhive_core::persistence::StateDb;
    /// use zhive_proto::domain::{ThreadId, TurnId};
    ///
    /// let db = StateDb::open(Path::new("/tmp/demo/state.db")).await?;
    /// let tid = ThreadId(Arc::from("thread:native/01"));
    /// let turn_id = TurnId(Arc::from("turn:thread:native/01/0"));
    /// db.record_turn_start(&tid, &turn_id, 0).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn record_turn_start(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        started_at: i64,
    ) -> StorageResult<()> {
        sqlx::query(
            r"
            INSERT OR IGNORE INTO turns (id, thread_id, status, started_at)
            VALUES (?1, ?2, 'inProgress', ?3)
            ",
        )
        .bind(turn_id.0.as_ref())
        .bind(thread_id.0.as_ref())
        .bind(started_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Updates the terminal fields of an existing turn row (status, error,
    /// `completed_at`, `duration_ms`).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Sqlx`] on failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::sync::Arc;
    /// use std::path::Path;
    /// use zhive_core::persistence::StateDb;
    /// use zhive_proto::domain::{TurnId, TurnStatus};
    ///
    /// let db = StateDb::open(Path::new("/tmp/demo/state.db")).await?;
    /// let turn_id = TurnId(Arc::from("turn:thread:native/01/0"));
    /// db.record_turn_end(&turn_id, TurnStatus::Completed, None, 1_000, Some(500)).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn record_turn_end(
        &self,
        turn_id: &TurnId,
        status: TurnStatus,
        error: Option<&TurnError>,
        completed_at: i64,
        duration_ms: Option<i64>,
    ) -> StorageResult<()> {
        let status_str = turn_status_to_str(status);
        let error_message = error.map(|e| e.message.as_str());
        let error_details = error.and_then(|e| e.additional_details.as_deref());

        sqlx::query(
            r"
            UPDATE turns
            SET status       = ?1,
                error_message = ?2,
                error_details = ?3,
                completed_at  = ?4,
                duration_ms   = ?5
            WHERE id = ?6
            ",
        )
        .bind(status_str)
        .bind(error_message)
        .bind(error_details)
        .bind(completed_at)
        .bind(duration_ms)
        .bind(turn_id.0.as_ref())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Inserts or replaces an item row for `(turn_id, seq, item)`.
    ///
    /// `INSERT OR REPLACE` is intentional: a `ToolCall` item with the same
    /// [`ItemId`] is written twice within a turn (once `InProgress`, once
    /// `Completed`/`Failed`).  The replace semantics give last-write-wins,
    /// matching the finalize-on-boundary model.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Sqlx`] on a database failure or
    /// [`StorageError::Json`] when the item fails to serialise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::sync::Arc;
    /// use std::path::Path;
    /// use zhive_core::persistence::StateDb;
    /// use zhive_proto::domain::{Item, ItemId, TurnId};
    ///
    /// let db = StateDb::open(Path::new("/tmp/demo/state.db")).await?;
    /// let turn_id = TurnId(Arc::from("turn:thread:native/01/0"));
    /// let item = Item::AgentMessage { id: ItemId(Arc::from("item:turn:t/0/0")), text: "hi".into() };
    /// db.append_item(&turn_id, 0, &item).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn append_item(&self, turn_id: &TurnId, seq: i64, item: &Item) -> StorageResult<()> {
        let item_id = item.id().0.as_ref();
        let item_kind = item_kind_str(item);
        let payload = serde_json::to_string(item)?;

        sqlx::query(
            r"
            INSERT OR REPLACE INTO items (id, turn_id, seq, item_kind, payload)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ",
        )
        .bind(item_id)
        .bind(turn_id.0.as_ref())
        .bind(seq)
        .bind(item_kind)
        .bind(payload)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // ------------------------------------------------------------------
    // Read methods
    // ------------------------------------------------------------------

    /// Returns all threads ordered by `updated_at DESC`.
    ///
    /// Turns are NOT populated (the `turns` field is always empty); callers
    /// that need turn data should fetch them separately.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Sqlx`] on a query failure or
    /// [`StorageError::Json`] when a row fails to deserialise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::path::Path;
    /// use zhive_core::persistence::StateDb;
    /// let db = StateDb::open(Path::new("/tmp/demo/state.db")).await?;
    /// let threads = db.list_threads().await?;
    /// for t in threads {
    ///     println!("{}", t.id.0);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_threads(&self) -> StorageResult<Vec<Thread>> {
        let rows = sqlx::query(
            r"
            SELECT id, session_id, forked_from, preview, ephemeral, model_provider,
                   created_at, updated_at, status, cwd, source, name
            FROM threads
            ORDER BY updated_at DESC
            ",
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(|row| row_to_thread(&row)).collect()
    }

    /// Returns the thread with the given `id`, or `None` when not found.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Sqlx`] on a database failure or
    /// [`StorageError::Json`] when a row field fails to deserialise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::sync::Arc;
    /// use std::path::Path;
    /// use zhive_core::persistence::StateDb;
    /// use zhive_proto::domain::ThreadId;
    /// let db = StateDb::open(Path::new("/tmp/demo/state.db")).await?;
    /// let id = ThreadId(Arc::from("thread:native/01"));
    /// let thread = db.get_thread(&id).await?;
    /// assert!(thread.is_none()); // not inserted yet
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_thread(&self, id: &ThreadId) -> StorageResult<Option<Thread>> {
        let row = sqlx::query(
            r"
            SELECT id, session_id, forked_from, preview, ephemeral, model_provider,
                   created_at, updated_at, status, cwd, source, name
            FROM threads
            WHERE id = ?1
            ",
        )
        .bind(id.0.as_ref())
        .fetch_optional(&self.pool)
        .await?;

        row.map(|r| row_to_thread(&r)).transpose()
    }

    /// Returns all items for the given turn, ordered by `seq`.
    ///
    /// Each row's `payload` column is deserialised into an [`Item`]; a
    /// corrupt payload produces [`StorageError::Json`].
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Sqlx`] on a database failure or
    /// [`StorageError::Json`] when a payload cannot be deserialised.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::sync::Arc;
    /// use std::path::Path;
    /// use zhive_core::persistence::StateDb;
    /// use zhive_proto::domain::TurnId;
    /// let db = StateDb::open(Path::new("/tmp/demo/state.db")).await?;
    /// let turn_id = TurnId(Arc::from("turn:t/0"));
    /// let items = db.get_turn_items(&turn_id).await?;
    /// assert!(items.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_turn_items(&self, turn_id: &TurnId) -> StorageResult<Vec<Item>> {
        let rows = sqlx::query(
            r"
            SELECT payload
            FROM items
            WHERE turn_id = ?1
            ORDER BY seq
            ",
        )
        .bind(turn_id.0.as_ref())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let payload: String = row.try_get("payload")?;
                let item: Item = serde_json::from_str(&payload)?;
                Ok(item)
            })
            .collect()
    }
}

// ------------------------------------------------------------------
// Row mappers
// ------------------------------------------------------------------

fn row_to_thread(row: &sqlx::sqlite::SqliteRow) -> StorageResult<Thread> {
    use std::path::PathBuf;
    use std::sync::Arc;
    use zhive_proto::domain::AcpSessionId;

    let id: String = row.try_get("id")?;
    let session_id: Option<String> = row.try_get("session_id")?;
    let forked_from: Option<String> = row.try_get("forked_from")?;
    let preview: String = row.try_get("preview")?;
    let ephemeral: i64 = row.try_get("ephemeral")?;
    let model_provider: String = row.try_get("model_provider")?;
    let created_at: i64 = row.try_get("created_at")?;
    let updated_at: i64 = row.try_get("updated_at")?;
    let status_str: String = row.try_get("status")?;
    let cwd_str: String = row.try_get("cwd")?;
    let source_str: String = row.try_get("source")?;
    let name: Option<String> = row.try_get("name")?;

    let status = thread_status_from_str(&status_str);
    let source = thread_source_from_str(&source_str);

    Ok(Thread {
        id: ThreadId(Arc::from(id.as_str())),
        session_id: session_id.map(|s| AcpSessionId(Arc::from(s.as_str()))),
        forked_from: forked_from.map(|s| ThreadId(Arc::from(s.as_str()))),
        preview,
        ephemeral: ephemeral != 0,
        model_provider,
        created_at,
        updated_at,
        status,
        cwd: PathBuf::from(cwd_str),
        source,
        name,
        turns: vec![],
    })
}

// ------------------------------------------------------------------
// Enum wire serialisation helpers
// ------------------------------------------------------------------

/// Serialises [`ThreadStatus`] to the wire string stored in the database.
fn thread_status_to_str(status: &ThreadStatus) -> &'static str {
    match status {
        ThreadStatus::NotLoaded => "notLoaded",
        ThreadStatus::Active { .. } => "active",
        ThreadStatus::SystemError => "systemError",
        // ThreadStatus::Idle and any future variants default to "idle"
        _ => "idle",
    }
}

fn thread_status_from_str(s: &str) -> ThreadStatus {
    match s {
        "notLoaded" => ThreadStatus::NotLoaded,
        "active" => ThreadStatus::Active {
            active_flags: vec![],
        },
        "systemError" => ThreadStatus::SystemError,
        _ => ThreadStatus::Idle,
    }
}

/// Serialises [`ThreadSource`] to the wire string stored in the database.
fn thread_source_to_str(source: ThreadSource) -> &'static str {
    match source {
        ThreadSource::Subagent => "subagent",
        ThreadSource::MemoryConsolidation => "memory_consolidation",
        // ThreadSource::User and any future variants default to "user"
        _ => "user",
    }
}

fn thread_source_from_str(s: &str) -> ThreadSource {
    match s {
        "subagent" => ThreadSource::Subagent,
        "memory_consolidation" => ThreadSource::MemoryConsolidation,
        _ => ThreadSource::User,
    }
}

/// Serialises [`TurnStatus`] to the wire string stored in the database.
fn turn_status_to_str(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::InProgress => "inProgress",
        TurnStatus::Interrupted => "interrupted",
        TurnStatus::Failed => "failed",
        // TurnStatus::Completed and any future variants default to "completed"
        _ => "completed",
    }
}

/// Returns the `itemKind` discriminant string for the `items.item_kind` column.
fn item_kind_str(item: &Item) -> &'static str {
    match item {
        Item::UserMessage { .. } => "user_message",
        Item::AgentMessage { .. } => "agent_message",
        Item::AgentThought { .. } => "agent_thought",
        Item::Reasoning { .. } => "reasoning",
        Item::ToolCall { .. } => "tool_call",
        Item::CommandExecution { .. } => "command_execution",
        Item::FileEdit { .. } => "file_edit",
        Item::Diff { .. } => "diff",
        Item::Terminal { .. } => "terminal",
        Item::Plan { .. } => "plan",
        Item::AvailableCommands { .. } => "available_commands",
        Item::ModeChange { .. } => "mode_change",
        Item::ContextCompaction { .. } => "context_compaction",
        Item::SystemNotice { .. } => "system_notice",
        // Keep non-exhaustive pattern so a future Item variant is caught at compile time.
        _ => "unknown",
    }
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use zhive_proto::domain::ItemId;

    use super::*;

    /// Open a fresh temporary state database.
    ///
    /// Returns `(TempDir, StateDb)` so the caller holds `TempDir` for the
    /// test's duration, keeping the directory alive (and `SQLite` WAL files
    /// accessible) until the test ends.
    async fn open_temp() -> (tempfile::TempDir, StateDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::open(&dir.path().join("state.db")).await.unwrap();
        (dir, db)
    }

    fn make_thread(id: &str) -> Thread {
        Thread {
            id: ThreadId(Arc::from(id)),
            session_id: None,
            forked_from: None,
            preview: "test preview".into(),
            ephemeral: false,
            model_provider: "anthropic".into(),
            created_at: 1_000,
            updated_at: 2_000,
            status: ThreadStatus::Idle,
            cwd: PathBuf::from("/tmp"),
            source: ThreadSource::User,
            name: None,
            turns: vec![],
        }
    }

    // ------------------------------------------------------------------
    // Part A round-trip: thread → turn → items
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn state_db_thread_round_trip() {
        let (_dir, db) = open_temp().await;

        // Initially empty.
        let threads = db.list_threads().await.unwrap();
        assert!(threads.is_empty());
        assert!(
            db.get_thread(&ThreadId(Arc::from("thread:native/01")))
                .await
                .unwrap()
                .is_none()
        );

        // Insert.
        let t = make_thread("thread:native/01");
        db.upsert_thread(&t).await.unwrap();

        // Verify via get_thread.
        let fetched = db
            .get_thread(&ThreadId(Arc::from("thread:native/01")))
            .await
            .unwrap()
            .expect("must be present");
        assert_eq!(fetched.id.0.as_ref(), "thread:native/01");
        assert_eq!(fetched.preview, "test preview");

        // Verify via list_threads.
        let list = db.list_threads().await.unwrap();
        assert_eq!(list.len(), 1);

        // Upsert (update).
        let mut updated = t.clone();
        updated.preview = "updated".into();
        updated.updated_at = 3_000;
        db.upsert_thread(&updated).await.unwrap();

        let fetched2 = db
            .get_thread(&ThreadId(Arc::from("thread:native/01")))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched2.preview, "updated");
    }

    #[tokio::test]
    async fn state_db_thread_turn_items_round_trip() {
        let (_dir, db) = open_temp().await;

        let thread_id = ThreadId(Arc::from("thread:native/rt"));
        let turn_id = TurnId(Arc::from("turn:thread:native/rt/0"));

        // Must upsert thread first (FK constraint).
        db.upsert_thread(&make_thread("thread:native/rt"))
            .await
            .unwrap();

        // record_turn_start
        db.record_turn_start(&thread_id, &turn_id, 100)
            .await
            .unwrap();

        // append two items
        let item0 = Item::AgentMessage {
            id: ItemId(Arc::from("item:turn:t/rt/0/0")),
            text: "hello".into(),
        };
        let item1 = Item::AgentMessage {
            id: ItemId(Arc::from("item:turn:t/rt/0/1")),
            text: "world".into(),
        };
        db.append_item(&turn_id, 0, &item0).await.unwrap();
        db.append_item(&turn_id, 1, &item1).await.unwrap();

        // get_turn_items returns them in seq order
        let items = db.get_turn_items(&turn_id).await.unwrap();
        assert_eq!(items.len(), 2);
        let Item::AgentMessage { text: t0, .. } = &items[0] else {
            panic!("expected AgentMessage");
        };
        let Item::AgentMessage { text: t1, .. } = &items[1] else {
            panic!("expected AgentMessage");
        };
        assert_eq!(t0, "hello");
        assert_eq!(t1, "world");

        // record_turn_end
        db.record_turn_end(&turn_id, TurnStatus::Completed, None, 500, Some(400))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn append_item_replace_semantics_for_tool_call() {
        let (_dir, db) = open_temp().await;

        let thread_id = ThreadId(Arc::from("thread:native/tc"));
        let turn_id = TurnId(Arc::from("turn:thread:native/tc/0"));

        db.upsert_thread(&make_thread("thread:native/tc"))
            .await
            .unwrap();
        db.record_turn_start(&thread_id, &turn_id, 0).await.unwrap();

        // Write InProgress tool call.
        let in_progress = Item::ToolCall {
            id: ItemId(Arc::from("item:tc/0")),
            name: "echo".into(),
            kind: zhive_proto::domain::ToolKind::Other,
            status: zhive_proto::domain::ToolCallStatus::InProgress,
            content: vec![],
            locations: vec![],
            raw_input: None,
            raw_output: None,
            provider_tool_call_id: None,
        };
        db.append_item(&turn_id, 0, &in_progress).await.unwrap();

        // Write Completed — must replace.
        let completed = Item::ToolCall {
            id: ItemId(Arc::from("item:tc/0")),
            name: "echo".into(),
            kind: zhive_proto::domain::ToolKind::Other,
            status: zhive_proto::domain::ToolCallStatus::Completed,
            content: vec![],
            locations: vec![],
            raw_input: None,
            raw_output: Some(serde_json::json!("done")),
            provider_tool_call_id: None,
        };
        db.append_item(&turn_id, 0, &completed).await.unwrap();

        // Only one row — the completed one.
        let items = db.get_turn_items(&turn_id).await.unwrap();
        assert_eq!(items.len(), 1);
        let Item::ToolCall { status, .. } = &items[0] else {
            panic!("expected ToolCall");
        };
        assert_eq!(*status, zhive_proto::domain::ToolCallStatus::Completed);
    }
}

// Rust guideline compliant 2026-02-21
