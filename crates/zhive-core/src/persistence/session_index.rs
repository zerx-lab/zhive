//! Append-only `thread_id` ↔ session-name index (`session_index.jsonl`).
//!
//! A single newline-delimited JSON file under the storage base directory
//! records one [`SessionIndexEntry`] per rename. The file is **append-only**:
//! a thread renamed twice produces two lines for the same `thread_id`, and the
//! *last* line wins. This mirrors the codex `session_index.rs` design but uses
//! `i64` Unix seconds (consistent with the rest of the schema) instead of an
//! RFC 3339 string, and stays fully `async` on `tokio::fs` without
//! `spawn_blocking`.
//!
//! ## Why append-only
//!
//! Renames are rare and the file is small, so rewriting it in place would add
//! locking complexity for no benefit. Readers scan the whole file and keep the
//! newest entry per id; a malformed line (e.g. a partial write torn by a crash)
//! is skipped rather than aborting the scan, so one bad line never hides every
//! valid name.

// TODO(phase2): wire `append_entry` into the live thread-rename path. Today this
// sidecar has no production writer — it is exercised only by the crash-recovery
// rebuild (which skips it as a non-rollout) and by tests; the authoritative name
// lives in the `state.db` `threads.name` column written by
// `StorageWriteOp::SessionNameSet` (see `writer::apply_session_name_set`). To
// wire it, append a `SessionIndexEntry` from `apply_session_name_set` after the
// SQL update; that needs a `Storage::base_dir()` accessor (currently private),
// since the sidecar lives at `<base>/session_index.jsonl` beside the DBs and
// outside `rollouts/`. Deferred: the SQL column already serves every current
// read, so wiring it now would add a disk write to the rename path with no
// consumer that yet needs the JSONL form.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::error::StorageResult;

/// File name of the session-index JSONL under the storage base directory.
///
/// Lives beside the database files (not inside `rollouts/`) so a rollout
/// directory scan never mistakes it for a per-thread rollout.
pub const SESSION_INDEX_FILE: &str = "session_index.jsonl";

/// One append-only `thread_id` → human-facing name record.
///
/// Serialised in `camelCase` to match the wire schema used elsewhere
/// (`threadId`, `updatedAt`). The newest entry for a given `thread_id` is the
/// authoritative name; see [`find_name_by_id`] and [`list_latest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionIndexEntry {
    /// Thread the name belongs to (e.g. `thread:native/0190…`).
    pub thread_id: String,
    /// Human-facing session name at the time the entry was written.
    pub name: String,
    /// Unix-seconds timestamp the entry was appended.
    pub updated_at: i64,
}

impl SessionIndexEntry {
    /// Builds an entry from its parts.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::persistence::session_index::SessionIndexEntry;
    /// let e = SessionIndexEntry::new("thread:native/01", "release", 1_700_000_000);
    /// assert_eq!(e.thread_id, "thread:native/01");
    /// assert_eq!(e.name, "release");
    /// ```
    #[must_use]
    pub fn new(thread_id: impl Into<String>, name: impl Into<String>, updated_at: i64) -> Self {
        Self {
            thread_id: thread_id.into(),
            name: name.into(),
            updated_at,
        }
    }
}

/// Appends one [`SessionIndexEntry`] line to `base_dir/session_index.jsonl`.
///
/// The file (and `base_dir`, if missing) is created on first write and opened
/// in append mode, then flushed. No `fsync` is issued: a lost trailing line
/// only forgets a recent rename, never corrupts an earlier one, and the name
/// is also held in the state database.
///
/// # Errors
///
/// Returns [`StorageError::Io`] when the directory or file cannot be created
/// or written, or [`StorageError::Json`] when the entry fails to encode.
///
/// [`StorageError::Io`]: super::StorageError::Io
/// [`StorageError::Json`]: super::StorageError::Json
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::session_index::{append_entry, SessionIndexEntry};
///
/// let entry = SessionIndexEntry::new("thread:native/01", "release planning", 1_700_000_000);
/// append_entry(Path::new("/tmp/demo"), &entry).await?;
/// # Ok(())
/// # }
/// ```
pub async fn append_entry(base_dir: &Path, entry: &SessionIndexEntry) -> StorageResult<()> {
    tokio::fs::create_dir_all(base_dir).await?;
    let path = base_dir.join(SESSION_INDEX_FILE);
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    file.write_all(&line).await?;
    file.flush().await?;
    Ok(())
}

/// Returns the newest name recorded for `thread_id`, or `None`.
///
/// Scans the whole file keeping the last matching line (latest wins);
/// malformed lines are skipped. A missing index file is treated as empty and
/// yields `Ok(None)`.
///
/// # Errors
///
/// Returns [`StorageError::Io`] when the file exists but cannot be read.
///
/// [`StorageError::Io`]: super::StorageError::Io
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::session_index::find_name_by_id;
/// let name = find_name_by_id(Path::new("/tmp/demo"), "thread:native/01").await?;
/// assert!(name.is_none()); // nothing written yet
/// # Ok(())
/// # }
/// ```
pub async fn find_name_by_id(base_dir: &Path, thread_id: &str) -> StorageResult<Option<String>> {
    let path = base_dir.join(SESSION_INDEX_FILE);
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut lines = BufReader::new(file).lines();
    let mut latest: Option<String> = None;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        // Skip malformed lines: a torn trailing write must not mask earlier,
        // valid renames.
        let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(&line) else {
            continue;
        };
        if entry.thread_id == thread_id {
            latest = Some(entry.name);
        }
    }
    Ok(latest)
}

/// Returns the newest entry for each distinct `thread_id`.
///
/// Later lines overwrite earlier ones for the same id, so the result holds one
/// entry per thread reflecting its current name. Order is unspecified.
/// Malformed lines are skipped; a missing file yields an empty `Vec`.
///
/// # Errors
///
/// Returns [`StorageError::Io`] when the file exists but cannot be read.
///
/// [`StorageError::Io`]: super::StorageError::Io
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::session_index::list_latest;
/// let entries = list_latest(Path::new("/tmp/demo")).await?;
/// assert!(entries.is_empty());
/// # Ok(())
/// # }
/// ```
pub async fn list_latest(base_dir: &Path) -> StorageResult<Vec<SessionIndexEntry>> {
    let path = base_dir.join(SESSION_INDEX_FILE);
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut lines = BufReader::new(file).lines();
    let mut latest: HashMap<String, SessionIndexEntry> = HashMap::new();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(&line) else {
            continue;
        };
        latest.insert(entry.thread_id.clone(), entry);
    }
    Ok(latest.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_then_find_returns_latest() {
        let dir = tempfile::tempdir().unwrap();
        append_entry(
            dir.path(),
            &SessionIndexEntry::new("thread:native/01", "first", 1),
        )
        .await
        .unwrap();
        append_entry(
            dir.path(),
            &SessionIndexEntry::new("thread:native/01", "second", 2),
        )
        .await
        .unwrap();

        let name = find_name_by_id(dir.path(), "thread:native/01")
            .await
            .unwrap();
        assert_eq!(name.as_deref(), Some("second"), "latest write must win");

        // Unknown id → None.
        assert!(
            find_name_by_id(dir.path(), "thread:native/missing")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn find_on_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            find_name_by_id(dir.path(), "thread:native/01")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_latest_deduplicates_by_id() {
        let dir = tempfile::tempdir().unwrap();
        // Three lines covering two ids; id "a" renamed once.
        append_entry(dir.path(), &SessionIndexEntry::new("a", "a1", 1))
            .await
            .unwrap();
        append_entry(dir.path(), &SessionIndexEntry::new("b", "b1", 2))
            .await
            .unwrap();
        append_entry(dir.path(), &SessionIndexEntry::new("a", "a2", 3))
            .await
            .unwrap();

        let mut entries = list_latest(dir.path()).await.unwrap();
        entries.sort_by(|l, r| l.thread_id.cmp(&r.thread_id));
        assert_eq!(entries.len(), 2, "two distinct ids");
        assert_eq!(entries[0].name, "a2", "id a keeps the latest name");
        assert_eq!(entries[1].name, "b1");
    }

    #[tokio::test]
    async fn malformed_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        append_entry(dir.path(), &SessionIndexEntry::new("a", "good", 1))
            .await
            .unwrap();
        // Append a torn line directly.
        let path = dir.path().join(SESSION_INDEX_FILE);
        let mut f = OpenOptions::new().append(true).open(&path).await.unwrap();
        f.write_all(b"{not json\n").await.unwrap();
        f.flush().await.unwrap();

        // The valid entry is still discoverable.
        assert_eq!(
            find_name_by_id(dir.path(), "a").await.unwrap().as_deref(),
            Some("good")
        );
    }
}

// Rust guideline compliant 2026-02-21
