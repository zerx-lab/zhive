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
pub mod preview;
pub mod rollout;
pub mod session_index;
pub mod state_db;
pub mod storage_trait;
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
pub use preview::{PREVIEW_MAX_CHARS, derive_preview_from_items, truncate_preview};
#[doc(inline)]
pub use rollout::{RolloutEntry, RolloutWriter, read_all};
#[doc(inline)]
pub use session_index::SessionIndexEntry;
#[doc(inline)]
pub use state_db::StateDb;
#[doc(inline)]
pub use storage_trait::ThreadStorage;

use std::path::{Path, PathBuf};

use zhive_proto::domain::{Item, ItemId, ThreadId};

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

    /// Replays the full item history of `source`, optionally truncated to
    /// `up_to`.
    ///
    /// Reads the source thread's JSONL rollout directly (the source of truth),
    /// keeping every [`RolloutEntry::Item`] payload in file order. This reads
    /// the **complete** history — not just the in-memory tail window — so it is
    /// the hard prerequisite for cross-thread fork, which must reconstruct a
    /// source thread's transcript regardless of how much of it is still
    /// resident in memory.
    ///
    /// When `up_to` is `Some(id)`, replay stops **after** the item whose id
    /// equals `id` (inclusive); items past that point are dropped. When `up_to`
    /// is `None`, every item is returned. An id that never appears returns the
    /// whole history (no truncation). A missing rollout file yields an empty
    /// `Vec` (a fork source that was never persisted has no history).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Io`] when the rollout exists but cannot be read,
    /// or [`StorageError::RolloutCorrupted`] / [`StorageError::Json`] when a
    /// line fails to decode.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::sync::Arc;
    /// use std::path::Path;
    /// use zhive_core::persistence::{RolloutWriter, RolloutEntry, Storage};
    /// use zhive_proto::domain::{Item, ItemId, ThreadId};
    ///
    /// let storage = Storage::open(Path::new("/tmp/demo-replay")).await?;
    /// let source = ThreadId(Arc::from("thread:native/src"));
    ///
    /// // Seed a two-item rollout.
    /// let mut w = RolloutWriter::open(storage.rollout_path(&source.0)).await?;
    /// w.append(&RolloutEntry::Session {
    ///     version: 3, id: source.0.to_string(), timestamp: 0,
    ///     cwd: "/".into(), parent_session: None,
    /// }).await?;
    /// for n in 0..2 {
    ///     w.append(&RolloutEntry::Item {
    ///         thread_id: source.0.to_string(),
    ///         turn_id: format!("turn:{}/0", source.0),
    ///         timestamp: 0,
    ///         item: Box::new(Item::AgentMessage {
    ///             id: ItemId(Arc::from(format!("item:{n}").as_str())),
    ///             text: format!("m{n}"),
    ///         }),
    ///     }).await?;
    /// }
    /// w.sync_all().await?;
    /// drop(w);
    ///
    /// let all = storage.replay_thread_items(&source, None).await?;
    /// assert_eq!(all.len(), 2);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn replay_thread_items(
        &self,
        source: &ThreadId,
        up_to: Option<&ItemId>,
    ) -> StorageResult<Vec<Item>> {
        let path = self.rollout_path(&source.0);
        let entries = match read_all(&path).await {
            Ok(e) => e,
            Err(StorageError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(other) => return Err(other),
        };

        let mut items = Vec::new();
        for entry in entries {
            if let RolloutEntry::Item { item, .. } = entry {
                let reached_boundary = up_to.is_some_and(|id| item.id() == id);
                items.push(*item);
                if reached_boundary {
                    // Inclusive truncation: keep the boundary item, drop the
                    // rest.
                    //
                    // SEMANTICS: truncation is ITEM-level, not turn-level. The
                    // cut happens at the exact boundary item even if it sits in
                    // the middle of a turn — the items that preceded it within
                    // the same turn are kept and the items that followed it
                    // (including later items of that same turn) are dropped. A
                    // fork at `up_to = <mid-turn item>` therefore yields a
                    // partial turn. Callers that need turn-aligned boundaries
                    // must pass the last item id of a turn.
                    break;
                }
            }
        }
        Ok(items)
    }

    /// Backfills the `preview` and `cwd` index columns from each thread's
    /// rollout for threads recorded before the live turn path filled them.
    ///
    /// For every thread in the state index whose `preview` column is still
    /// empty, this reads the thread's JSONL rollout (the source of truth) and:
    ///
    /// * derives a preview from the first [`Item::UserMessage`] via the shared
    ///   [`preview::derive_preview_from_items`], so a backfilled preview is
    ///   byte-identical to one the engine would have produced live, and writes
    ///   it back through [`StateDb::set_thread_preview_if_empty`]; and
    /// * reads the working directory from the rollout's
    ///   [`RolloutEntry::Session`] header and writes it through
    ///   [`StateDb::set_thread_cwd`].
    ///
    /// The operation is **idempotent**: the preview is only filled while it is
    /// empty (`set_thread_preview_if_empty`), and a rollout that yields no user
    /// message simply leaves the preview empty for a later run. A thread with a
    /// missing rollout file is skipped (its row predates the rollout or the file
    /// was pruned). The returned [`BackfillStats`] reports how many rows were
    /// scanned and how many had a preview / cwd written.
    ///
    /// This is invoked best-effort at startup (see the CLI `open_storage` path):
    /// it must never block boot, so callers should log and continue on `Err`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the thread list cannot be read, a rollout
    /// fails to decode (other than a missing file), or a DB write fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use std::path::Path;
    /// use zhive_core::persistence::Storage;
    ///
    /// let storage = Storage::open(Path::new("/tmp/demo-backfill")).await?;
    /// let stats = storage.backfill_thread_metadata().await?;
    /// println!("scanned {} thread(s)", stats.scanned);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn backfill_thread_metadata(&self) -> StorageResult<BackfillStats> {
        let mut stats = BackfillStats::default();

        // List every thread, then act only on those still missing a preview.
        // Listing all and filtering in memory keeps this on the existing
        // `list_threads` read surface rather than adding a bespoke query.
        let threads = self.state.list_threads(None).await?;
        for thread in threads {
            if !thread.preview.is_empty() {
                continue;
            }
            stats.scanned += 1;

            let path = self.rollout_path(&thread.id.0);
            let entries = match read_all(&path).await {
                Ok(e) => e,
                Err(StorageError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => {
                    // No rollout on disk for this row: nothing to backfill from.
                    continue;
                }
                Err(other) => return Err(other),
            };

            // Pull the cwd from the Session header (first matching entry) and
            // the items used to derive the preview, in a single pass.
            let mut header_cwd: Option<String> = None;
            let mut items: Vec<Item> = Vec::new();
            for entry in entries {
                match entry {
                    RolloutEntry::Session { cwd, .. } if header_cwd.is_none() => {
                        header_cwd = Some(cwd);
                    }
                    RolloutEntry::Item { item, .. } => items.push(*item),
                    _ => {}
                }
            }

            let derived = preview::derive_preview_from_items(&items);
            if !derived.is_empty() {
                self.state
                    .set_thread_preview_if_empty(&thread.id, &derived)
                    .await?;
                stats.previews_filled += 1;
            }

            if let Some(cwd) = header_cwd {
                self.state.set_thread_cwd(&thread.id, &cwd).await?;
                stats.cwds_filled += 1;
            }
        }

        Ok(stats)
    }
}

/// Outcome counts from [`Storage::backfill_thread_metadata`].
///
/// `scanned` counts threads with an empty preview that were inspected;
/// `previews_filled` and `cwds_filled` count the rows that actually had a value
/// written (a scanned thread may fill neither, e.g. a rollout with no user
/// message and no readable header).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackfillStats {
    /// Threads with an empty preview that were inspected.
    pub scanned: u64,
    /// Threads whose preview column was written from a derived user message.
    pub previews_filled: u64,
    /// Threads whose cwd column was written from a rollout Session header.
    pub cwds_filled: u64,
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

    #[tokio::test]
    async fn replay_thread_items_reads_full_history_and_truncates() {
        use std::sync::Arc;
        use zhive_proto::domain::{Item, ItemId};

        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).await.unwrap();
        let source = ThreadId(Arc::from("thread:native/replay-src"));

        // Seed Session + 2 items via the rollout writer.
        let mut w = RolloutWriter::open(storage.rollout_path(&source.0))
            .await
            .unwrap();
        w.append(&RolloutEntry::Session {
            version: 3,
            id: source.0.to_string(),
            timestamp: 0,
            cwd: "/".into(),
            parent_session: None,
        })
        .await
        .unwrap();
        for n in 0..2 {
            w.append(&RolloutEntry::Item {
                thread_id: source.0.to_string(),
                turn_id: format!("turn:{}/0", source.0),
                timestamp: 0,
                item: Box::new(Item::AgentMessage {
                    id: ItemId(Arc::from(format!("item:{n}").as_str())),
                    text: format!("m{n}"),
                }),
            })
            .await
            .unwrap();
        }
        w.sync_all().await.unwrap();
        drop(w);

        // None → full history.
        let all = storage.replay_thread_items(&source, None).await.unwrap();
        assert_eq!(all.len(), 2);

        // up_to=item:0 → inclusive truncation to one item.
        let item0 = ItemId(Arc::from("item:0"));
        let truncated = storage
            .replay_thread_items(&source, Some(&item0))
            .await
            .unwrap();
        assert_eq!(truncated.len(), 1);
        assert_eq!(truncated[0].id().0.as_ref(), "item:0");

        // Missing source → empty vec, not an error.
        let missing = ThreadId(Arc::from("thread:native/no-such"));
        assert!(
            storage
                .replay_thread_items(&missing, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn backfill_fills_preview_from_user_message_and_cwd_from_header() {
        use std::path::PathBuf;
        use std::sync::Arc;
        use zhive_proto::domain::{Item, ItemContent, ItemId, Thread, ThreadSource, ThreadStatus};

        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).await.unwrap();
        let tid = ThreadId(Arc::from("thread:native/backfill"));

        // Seed a thread row with an EMPTY preview and a placeholder cwd, as a
        // legacy recording would have left it.
        storage
            .state
            .upsert_thread(&Thread {
                id: tid.clone(),
                session_id: None,
                forked_from: None,
                subagent_parent: None,
                preview: String::new(),
                ephemeral: false,
                model_provider: "unknown".into(),
                created_at: 0,
                updated_at: 0,
                status: ThreadStatus::Idle,
                cwd: PathBuf::from("."),
                source: ThreadSource::User,
                name: None,
                turns: vec![],
            })
            .await
            .unwrap();

        // Write a rollout: a Session header with a real cwd, then a first
        // UserMessage the preview should be derived from.
        let mut w = RolloutWriter::open(storage.rollout_path(&tid.0))
            .await
            .unwrap();
        w.append(&RolloutEntry::Session {
            version: 3,
            id: tid.0.to_string(),
            timestamp: 0,
            cwd: "/work/project".into(),
            parent_session: None,
        })
        .await
        .unwrap();
        w.append(&RolloutEntry::Item {
            thread_id: tid.0.to_string(),
            turn_id: format!("turn:{}/0", tid.0),
            timestamp: 0,
            item: Box::new(Item::UserMessage {
                id: ItemId(Arc::from("item:turn/0/0")),
                content: vec![ItemContent::Text {
                    text: "  hello backfill world  ".into(),
                    annotations: None,
                }],
            }),
        })
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        let stats = storage.backfill_thread_metadata().await.unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.previews_filled, 1);
        assert_eq!(stats.cwds_filled, 1);

        let row = storage
            .state
            .get_thread(&tid)
            .await
            .unwrap()
            .expect("thread present");
        assert_eq!(row.preview, "hello backfill world");
        assert_eq!(row.cwd, PathBuf::from("/work/project"));

        // Idempotent: a second pass leaves the (now non-empty) preview alone and
        // does not re-scan the already-filled thread.
        let again = storage.backfill_thread_metadata().await.unwrap();
        assert_eq!(again.scanned, 0, "filled thread is no longer scanned");
        let row2 = storage.state.get_thread(&tid).await.unwrap().unwrap();
        assert_eq!(row2.preview, "hello backfill world");
    }

    #[tokio::test]
    async fn backfill_skips_threads_without_user_message_but_fills_cwd() {
        use std::path::PathBuf;
        use std::sync::Arc;
        use zhive_proto::domain::{Item, ItemId, Thread, ThreadSource, ThreadStatus};

        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).await.unwrap();
        let tid = ThreadId(Arc::from("thread:native/no-user-msg"));

        storage
            .state
            .upsert_thread(&Thread {
                id: tid.clone(),
                session_id: None,
                forked_from: None,
                subagent_parent: None,
                preview: String::new(),
                ephemeral: false,
                model_provider: "unknown".into(),
                created_at: 0,
                updated_at: 0,
                status: ThreadStatus::Idle,
                cwd: PathBuf::from("."),
                source: ThreadSource::User,
                name: None,
                turns: vec![],
            })
            .await
            .unwrap();

        // Rollout with a header (real cwd) but only an AgentMessage — no user
        // message to derive a preview from (mirrors the legacy recordings).
        let mut w = RolloutWriter::open(storage.rollout_path(&tid.0))
            .await
            .unwrap();
        w.append(&RolloutEntry::Session {
            version: 3,
            id: tid.0.to_string(),
            timestamp: 0,
            cwd: "/legacy/dir".into(),
            parent_session: None,
        })
        .await
        .unwrap();
        w.append(&RolloutEntry::Item {
            thread_id: tid.0.to_string(),
            turn_id: format!("turn:{}/0", tid.0),
            timestamp: 0,
            item: Box::new(Item::AgentMessage {
                id: ItemId(Arc::from("item:turn/0/0")),
                text: "agent only".into(),
            }),
        })
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        let stats = storage.backfill_thread_metadata().await.unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.previews_filled, 0, "no user message → no preview");
        assert_eq!(stats.cwds_filled, 1, "cwd still backfilled from header");

        let row = storage.state.get_thread(&tid).await.unwrap().unwrap();
        assert!(row.preview.is_empty());
        assert_eq!(row.cwd, PathBuf::from("/legacy/dir"));
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
