//! Mockable abstraction over the state-database read/write surface.
//!
//! [`ThreadStorage`] captures the thread / turn / item operations the engine
//! and lazy-load read path use, so tests can inject an in-memory fake without
//! a real `SQLite` file. It is implemented for [`StateDb`] by forwarding to the
//! inherent methods.
//!
//! ## Why RPITIT, not `async_trait` or `dyn`
//!
//! The trait uses return-position `impl Trait` in trait (`async fn` in trait,
//! stable since Rust 2024) rather than the `async_trait` macro. This keeps the
//! futures un-boxed and lets the compiler infer `Send` from the implementor.
//! The trade-off is that the trait is **not** object-safe — it cannot be used
//! as `dyn ThreadStorage`. That is deliberate: production code holds a concrete
//! [`StateDb`] / `Storage`, and this trait exists only for generic bounds and
//! mock injection, neither of which needs a trait object. Avoiding `dyn` also
//! sidesteps boxing every call on the hot persistence path.

use zhive_proto::domain::{Item, Thread, ThreadId, TurnError, TurnId, TurnStatus};

use super::error::StorageResult;
use super::state_db::StateDb;

/// Thread / turn / item operations backing the engine's persistence index.
///
/// Implemented by [`StateDb`]; a test fake can implement it over in-memory
/// maps. Every method returns a [`StorageResult`]; see [`StateDb`] for the
/// authoritative per-method semantics (the impl forwards directly).
///
/// Not object-safe by design (RPITIT) — use it as a generic bound, not as
/// `dyn ThreadStorage`. See the module header for the rationale.
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::StateDb;
/// use zhive_core::persistence::storage_trait::ThreadStorage;
///
/// // `StateDb` satisfies the trait; a generic helper can take any impl.
/// async fn count_threads<S: ThreadStorage>(s: &S) -> zhive_core::persistence::StorageResult<usize> {
///     Ok(s.list_threads(None).await?.len())
/// }
///
/// let db = StateDb::open(Path::new("/tmp/demo/state.db")).await?;
/// assert_eq!(count_threads(&db).await?, 0);
/// # Ok(())
/// # }
/// ```
pub trait ThreadStorage: Send + Sync {
    /// Inserts or updates a [`Thread`] row.
    ///
    /// # Errors
    ///
    /// Propagates any [`super::StorageError`] from the backing store.
    fn upsert_thread(&self, thread: &Thread) -> impl Future<Output = StorageResult<()>> + Send;

    /// Records a turn start (`status = InProgress`).
    ///
    /// # Errors
    ///
    /// Propagates any [`super::StorageError`] from the backing store.
    fn record_turn_start(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        started_at: i64,
    ) -> impl Future<Output = StorageResult<()>> + Send;

    /// Appends (or replaces) one item row at `(turn_id, seq)`.
    ///
    /// # Errors
    ///
    /// Propagates any [`super::StorageError`] from the backing store.
    fn append_item(
        &self,
        turn_id: &TurnId,
        seq: i64,
        item: &Item,
    ) -> impl Future<Output = StorageResult<()>> + Send;

    /// Records a terminal turn state.
    ///
    /// # Errors
    ///
    /// Propagates any [`super::StorageError`] from the backing store.
    fn record_turn_end(
        &self,
        turn_id: &TurnId,
        status: TurnStatus,
        error: Option<&TurnError>,
        completed_at: i64,
        duration_ms: Option<i64>,
    ) -> impl Future<Output = StorageResult<()>> + Send;

    /// Lists threads newest first, optionally scoped to a `cwd`.
    ///
    /// `cwd_filter` of `Some(path)` returns only threads created under that
    /// (normalised) working directory; `None` returns every thread.
    ///
    /// # Errors
    ///
    /// Propagates any [`super::StorageError`] from the backing store.
    fn list_threads(
        &self,
        cwd_filter: Option<&str>,
    ) -> impl Future<Output = StorageResult<Vec<Thread>>> + Send;

    /// Fetches one thread by id, or `None`.
    ///
    /// # Errors
    ///
    /// Propagates any [`super::StorageError`] from the backing store.
    fn get_thread(
        &self,
        id: &ThreadId,
    ) -> impl Future<Output = StorageResult<Option<Thread>>> + Send;

    /// Fetches every item of a turn, `seq`-ordered.
    ///
    /// # Errors
    ///
    /// Propagates any [`super::StorageError`] from the backing store.
    fn get_turn_items(
        &self,
        turn_id: &TurnId,
    ) -> impl Future<Output = StorageResult<Vec<Item>>> + Send;

    /// Fetches a `seq`-ordered page of a turn's items (lazy-load entry point).
    ///
    /// # Errors
    ///
    /// Propagates any [`super::StorageError`] from the backing store.
    fn load_items_page(
        &self,
        turn_id: &TurnId,
        offset: i64,
        limit: i64,
    ) -> impl Future<Output = StorageResult<Vec<Item>>> + Send;
}

impl ThreadStorage for StateDb {
    async fn upsert_thread(&self, thread: &Thread) -> StorageResult<()> {
        StateDb::upsert_thread(self, thread).await
    }

    async fn record_turn_start(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        started_at: i64,
    ) -> StorageResult<()> {
        StateDb::record_turn_start(self, thread_id, turn_id, started_at).await
    }

    async fn append_item(&self, turn_id: &TurnId, seq: i64, item: &Item) -> StorageResult<()> {
        StateDb::append_item(self, turn_id, seq, item).await
    }

    async fn record_turn_end(
        &self,
        turn_id: &TurnId,
        status: TurnStatus,
        error: Option<&TurnError>,
        completed_at: i64,
        duration_ms: Option<i64>,
    ) -> StorageResult<()> {
        StateDb::record_turn_end(self, turn_id, status, error, completed_at, duration_ms).await
    }

    async fn list_threads(&self, cwd_filter: Option<&str>) -> StorageResult<Vec<Thread>> {
        StateDb::list_threads(self, cwd_filter).await
    }

    async fn get_thread(&self, id: &ThreadId) -> StorageResult<Option<Thread>> {
        StateDb::get_thread(self, id).await
    }

    async fn get_turn_items(&self, turn_id: &TurnId) -> StorageResult<Vec<Item>> {
        StateDb::get_turn_items(self, turn_id).await
    }

    async fn load_items_page(
        &self,
        turn_id: &TurnId,
        offset: i64,
        limit: i64,
    ) -> StorageResult<Vec<Item>> {
        StateDb::load_items_page(self, turn_id, offset, limit).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zhive_proto::domain::{ThreadSource, ThreadStatus};

    use super::*;

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
            subagent_parent: None,
            preview: "p".into(),
            ephemeral: false,
            model_provider: "test".into(),
            created_at: 1,
            updated_at: 1,
            status: ThreadStatus::Idle,
            cwd: std::path::PathBuf::from("/tmp"),
            source: ThreadSource::User,
            name: None,
            turns: vec![],
        }
    }

    /// A generic helper bound only by [`ThreadStorage`] can round-trip a thread
    /// through any implementor, proving the trait forwards correctly and is
    /// usable as a generic bound (the mock-injection use case).
    #[tokio::test]
    async fn statedb_satisfies_trait_round_trip() {
        async fn upsert_and_get<S: ThreadStorage>(
            s: &S,
            t: &Thread,
        ) -> StorageResult<Option<Thread>> {
            s.upsert_thread(t).await?;
            s.get_thread(&t.id).await
        }

        let (_dir, db) = open_temp().await;
        let t = make_thread("thread:native/trait");
        let got = upsert_and_get(&db, &t)
            .await
            .unwrap()
            .expect("thread present");
        assert_eq!(got.id.0.as_ref(), "thread:native/trait");
    }
}

// Rust guideline compliant 2026-02-21
