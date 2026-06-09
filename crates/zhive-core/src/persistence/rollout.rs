//! JSONL+Leaf rollout writer (D-011 source of truth).
//!
//! Every persistence-relevant event is appended to a per-thread JSONL
//! file before any SQL index is touched. On a crash the JSONL stays
//! intact and the SQL indices can be rebuilt from it (see
//! `Storage::rebuild_indices`).
//!
//! ## Leaf pointer (Pi pattern)
//!
//! The last entry of the file may be a [`RolloutEntry::Leaf`] that
//! points at the active branch head. Forking writes a new leaf without
//! losing the previous branch's tail — the reader walks the file
//! backwards from the leaf to reconstruct any branch.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use zhive_proto::domain::{Item, ThreadSource};
use zhive_proto::permission::RequestPermissionRequest;

use super::error::{StorageError, StorageResult};

/// Current rollout JSONL schema version stamped on every [`RolloutEntry::Session`]
/// header.
///
/// Bumped only on a breaking change to the on-disk line shape; the reader
/// tolerates older versions by best-effort field defaulting. Kept here as the
/// single source of truth so header writers (the persistence writer and the
/// fork path) cannot drift to different version numbers.
///
/// **Wave4 (v4)**: `Session` header gains `subagent_parent` and `source`
/// fields so rebuild/resume can recover the parent-child relationship and
/// thread origin without hard-coding defaults.  Old files (v3) never contain
/// those fields; serde `#[default]` on both ensures they decode as `None` /
/// `None` respectively, making v3 rollouts byte-semantically identical to
/// before.
pub const SESSION_VERSION: u32 = 4;

/// One line in the rollout JSONL stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RolloutEntry {
    /// Session header; always the first line of a rollout file.
    Session {
        /// Format version of the JSONL schema.
        version: u32,
        /// Owning thread id (e.g. `thread:native/0190…`).
        id: String,
        /// Unix-seconds timestamp the rollout was opened.
        timestamp: i64,
        /// Working directory when the rollout was opened.
        cwd: String,
        /// Optional id of a parent rollout this one was forked from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_session: Option<String>,
        /// Parent thread id when this thread was spawned as a subagent.
        ///
        /// Absent in rollouts written before Wave4 (v4); defaults to `None`.
        /// Distinct from `parent_session` (which tracks fork origin): a subagent
        /// parent is a concurrent task relationship, not a branch relationship.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_parent: Option<String>,
        /// Thread origin (`user`, `subagent`, or `memory_consolidation`).
        ///
        /// Absent in pre-Wave4 rollouts; `None` on read → rebuild treats as
        /// [`ThreadSource::User`] (the historic hard-coded default so old files
        /// rebuild identically to before).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<ThreadSource>,
    },
    /// One zhive [`Item`] payload (the bulk of the file).
    Item {
        /// Thread the item belongs to.
        thread_id: String,
        /// Turn the item belongs to.
        turn_id: String,
        /// Unix-seconds timestamp at append time.
        timestamp: i64,
        /// Boxed payload to keep the enum size moderate.
        item: Box<Item>,
    },
    /// Branch head pointer (last entry of the file).
    Leaf {
        /// Item id at the current branch head, or `None` for an empty
        /// branch.
        target_id: Option<String>,
    },
    /// Context-compaction checkpoint: records that history up to this point
    /// was folded into a summary, plus the replacement transcript that
    /// supersedes it.
    ///
    /// On rebuild/resume, encountering this entry **discards all prior `Item`
    /// entries of this thread accumulated so far** and replaces them with
    /// `replacement`. Backward compatible: a rollout written before this
    /// variant existed simply never contains it, so the old full-replay path
    /// is unchanged.
    ///
    /// **Single-direction compatibility note**: a rollout file containing this
    /// entry cannot be read by a binary that predates the variant — the old
    /// reader will return [`StorageError::RolloutCorrupted`] for the
    /// `"type":"compaction"` line. Mixed deployments (old reader / new writer)
    /// are therefore unsupported; upgrade the reader before writing compacted
    /// rollouts.
    Compaction {
        /// Thread the compaction belongs to.
        thread_id: String,
        /// Synthetic compaction turn id (e.g. `<thread>::compaction-1`).
        turn_id: String,
        /// Unix-seconds timestamp at append time.
        timestamp: i64,
        /// Handoff summary text (without any prefix; stored verbatim for
        /// diagnostics and events).
        summary: String,
        /// Post-compaction transcript that replaces all prior items: the
        /// `[marker, summary]` pair installed in memory. Rebuild splices
        /// this in verbatim.
        replacement: Vec<Box<Item>>,
        /// Number of original items that were compacted away (for diagnostics).
        entries_compacted: u32,
    },
    /// A turn suspended awaiting a deferred permission decision (B6).
    ///
    /// Written when a [`zhive_proto::permission::PermissionDecision::Defer`]
    /// decision parks a turn.  On resume the engine re-registers the pending
    /// request so a reconnecting client can answer it via
    /// `session/resume_permission`.
    ///
    /// Backward compatible: pre-Wave4 rollouts never contain this entry.
    /// A matching [`RolloutEntry::PermissionResolved`] (or a later turn-
    /// completion `Leaf`) supersedes it; resume only re-surfaces requests
    /// that have no matching `PermissionResolved` following them.
    PendingPermission {
        /// Thread that owns the suspended turn.
        thread_id: String,
        /// Suspended turn id.
        turn_id: String,
        /// Unix-seconds timestamp at append time.
        timestamp: i64,
        /// Wire-form request id (e.g. `"perm:7"`) the client echoes back to
        /// resume via `session/resume_permission`.
        request_id: String,
        /// Full request payload re-emitted to the client on resume so the UI
        /// can re-render the approval prompt (tool name, reason, options).
        ///
        /// Boxed to keep the enum size in line with
        /// [`RolloutEntry::Compaction`].
        request: Box<RequestPermissionRequest>,
    },
    /// Marks a previously-suspended permission request as resolved (B6).
    ///
    /// Written when the client answers a deferred request (or when the turn
    /// is cancelled), superseding the matching [`RolloutEntry::PendingPermission`]
    /// entry so that resume does not re-surface an already-answered prompt.
    ///
    /// Backward compatible: pre-Wave4 rollouts never contain this entry.
    PermissionResolved {
        /// Thread the request belonged to.
        thread_id: String,
        /// Wire-form request id that was resolved.
        request_id: String,
        /// Unix-seconds timestamp at append time.
        timestamp: i64,
    },
    /// Per-turn workspace file snapshot backing the revert ("undo") feature.
    ///
    /// Records the shadow-git `tree` id captured at the start of a top-level
    /// user turn, plus a short `preview` of that turn's user message. Enqueued
    /// before the turn's first tool write so the checkpoint is durable even if
    /// the turn later crashes mid-flight. Projected into the `turn_snapshots`
    /// SQL table.
    ///
    /// **Single-direction compatibility note**: like [`RolloutEntry::Compaction`],
    /// a rollout containing this entry cannot be read by a binary that predates
    /// the variant. Upgrade the reader before writing snapshot rollouts.
    Snapshot {
        /// Thread the snapshot belongs to.
        thread_id: String,
        /// Turn whose start state this snapshot captured.
        turn_id: String,
        /// Unix-seconds timestamp at append time.
        timestamp: i64,
        /// 40-hex shadow-git tree id of the captured workspace state.
        tree: String,
        /// Short preview of the turn's user message, for the rewind picker.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        preview: String,
    },
}

/// Append-only writer for one thread's JSONL rollout.
#[derive(Debug)]
pub struct RolloutWriter {
    file: BufWriter<tokio::fs::File>,
    path: PathBuf,
}

impl RolloutWriter {
    /// Opens (or creates) the rollout file at `path`.
    ///
    /// The file is opened in append mode; existing content is left
    /// untouched. The parent directory is created when missing so the
    /// writer is usable outside the canonical
    /// [`super::Storage::open`] bootstrap path.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Io`] when the parent directory cannot be
    /// created or the file cannot be opened.
    pub async fn open(path: PathBuf) -> StorageResult<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self {
            file: BufWriter::new(file),
            path,
        })
    }

    /// Appends one entry, flushing the buffer to disk.
    ///
    /// Does **not** call `fsync`. Use [`Self::sync_all`] to wait for the
    /// kernel to durably persist the data.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Json`] when the entry fails to encode,
    /// [`StorageError::Io`] when writing fails.
    pub async fn append(&mut self, entry: &RolloutEntry) -> StorageResult<()> {
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        self.file.write_all(&line).await?;
        self.file.flush().await?;
        Ok(())
    }

    /// Appends a session header line, optionally naming a parent rollout.
    ///
    /// Convenience over [`Self::append`] for the
    /// [`RolloutEntry::Session`] variant; used by the fork path to write the
    /// first line of a forked thread's rollout with
    /// `parent_session = Some(source)`. The schema `version` is stamped to the
    /// current rollout format ([`SESSION_VERSION`]). Flushes the buffer but does
    /// **not** `fsync`; call [`Self::sync_all`] for a durable save point.
    ///
    /// For subagent threads that need to record `subagent_parent` and `source`,
    /// use [`Self::append_session_header_full`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Json`] when the entry fails to encode,
    /// [`StorageError::Io`] when writing fails.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use zhive_core::persistence::{RolloutWriter, RolloutEntry, read_all};
    /// let dir = tempfile::tempdir().expect("tempdir");
    /// let path = dir.path().join("forked.jsonl");
    /// let mut w = RolloutWriter::open(path.clone()).await?;
    /// w.append_session_header("thread:native/fork/1", 0, "/tmp", Some("thread:native/src"))
    ///     .await?;
    /// w.sync_all().await?;
    /// drop(w);
    /// let entries = read_all(&path).await?;
    /// assert!(matches!(
    ///     &entries[0],
    ///     RolloutEntry::Session { parent_session: Some(p), .. } if p == "thread:native/src"
    /// ));
    /// # Ok(())
    /// # }
    /// # tokio::runtime::Runtime::new().unwrap().block_on(demo()).unwrap();
    /// ```
    pub async fn append_session_header(
        &mut self,
        id: &str,
        timestamp: i64,
        cwd: &str,
        parent_session: Option<&str>,
    ) -> StorageResult<()> {
        self.append_session_header_full(id, timestamp, cwd, parent_session, None, None)
            .await
    }

    /// Appends a full session header including subagent provenance fields.
    ///
    /// Identical to [`Self::append_session_header`] but also records
    /// `subagent_parent` (the parent thread id when this thread was spawned as
    /// a child) and `source` (thread origin). These fields are stored with
    /// `skip_serializing_if = "Option::is_none"` so old readers that predate
    /// Wave4 are unaffected when both are `None`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Json`] when the entry fails to encode,
    /// [`StorageError::Io`] when writing fails.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use zhive_core::persistence::{RolloutWriter, RolloutEntry, read_all};
    /// use zhive_proto::domain::ThreadSource;
    /// let dir = tempfile::tempdir().expect("tempdir");
    /// let path = dir.path().join("subagent.jsonl");
    /// let mut w = RolloutWriter::open(path.clone()).await?;
    /// w.append_session_header_full(
    ///     "thread:native/child/1", 0, "/tmp",
    ///     None,
    ///     Some("thread:native/parent"),
    ///     Some(ThreadSource::Subagent),
    /// ).await?;
    /// w.sync_all().await?;
    /// drop(w);
    /// let entries = read_all(&path).await?;
    /// assert!(matches!(
    ///     &entries[0],
    ///     RolloutEntry::Session {
    ///         subagent_parent: Some(p),
    ///         source: Some(ThreadSource::Subagent),
    ///         ..
    ///     } if p == "thread:native/parent"
    /// ));
    /// # Ok(())
    /// # }
    /// # tokio::runtime::Runtime::new().unwrap().block_on(demo()).unwrap();
    /// ```
    pub async fn append_session_header_full(
        &mut self,
        id: &str,
        timestamp: i64,
        cwd: &str,
        parent_session: Option<&str>,
        subagent_parent: Option<&str>,
        source: Option<ThreadSource>,
    ) -> StorageResult<()> {
        let entry = RolloutEntry::Session {
            version: SESSION_VERSION,
            id: id.to_owned(),
            timestamp,
            cwd: cwd.to_owned(),
            parent_session: parent_session.map(str::to_owned),
            subagent_parent: subagent_parent.map(str::to_owned),
            source,
        };
        self.append(&entry).await
    }

    /// Appends a [`RolloutEntry::Leaf`] pointing at the active branch head.
    ///
    /// `target_id = Some(id)` marks a fork / branch leaf at item `id`;
    /// `target_id = None` is the turn-completion save-point marker written by
    /// the writer at [`super::writer::StorageWriteOp::TurnEnded`]. Flushes the
    /// buffer but does **not** `fsync`; the caller chooses the durability save
    /// point via [`Self::sync_all`].
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Json`] when the entry fails to encode,
    /// [`StorageError::Io`] when writing fails.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
    /// use zhive_core::persistence::{RolloutWriter, RolloutEntry, read_all};
    /// let dir = tempfile::tempdir().expect("tempdir");
    /// let path = dir.path().join("leaf.jsonl");
    /// let mut w = RolloutWriter::open(path.clone()).await?;
    /// w.set_leaf_id(Some("item:turn/0")).await?;
    /// w.sync_all().await?;
    /// drop(w);
    /// let entries = read_all(&path).await?;
    /// assert!(matches!(
    ///     entries.last(),
    ///     Some(RolloutEntry::Leaf { target_id: Some(t) }) if t == "item:turn/0"
    /// ));
    /// # Ok(())
    /// # }
    /// # tokio::runtime::Runtime::new().unwrap().block_on(demo()).unwrap();
    /// ```
    pub async fn set_leaf_id(&mut self, target_id: Option<&str>) -> StorageResult<()> {
        let entry = RolloutEntry::Leaf {
            target_id: target_id.map(str::to_owned),
        };
        self.append(&entry).await
    }

    /// Forces the kernel to flush dirty pages to the underlying device.
    ///
    /// Call this at every save point that must survive a crash.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Io`] when the syscall fails.
    pub async fn sync_all(&mut self) -> StorageResult<()> {
        self.file.flush().await?;
        self.file.get_ref().sync_all().await?;
        Ok(())
    }

    /// Returns the rollout file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Reads every entry from a rollout file into a [`Vec`].
///
/// Suitable for index rebuild paths; production reads of large rollouts
/// should stream line by line.
///
/// # Errors
///
/// Returns [`StorageError::Io`] when the file cannot be opened,
/// [`StorageError::Json`] / [`StorageError::RolloutCorrupted`] when a
/// line fails to decode.
pub async fn read_all(path: &Path) -> StorageResult<Vec<RolloutEntry>> {
    let file = tokio::fs::File::open(path).await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let mut entries = Vec::new();
    let mut line_no = 0usize;
    while let Some(line) = lines.next_line().await? {
        line_no += 1;
        if line.trim().is_empty() {
            continue;
        }
        let entry: RolloutEntry =
            serde_json::from_str(&line).map_err(|e| StorageError::RolloutCorrupted {
                line: line_no,
                reason: e.to_string(),
            })?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Reads every entry, tolerating a single corrupt or truncated trailing line.
///
/// A crash during an append can leave a half-written final JSONL line.  This
/// reader discards that trailing bad line, keeps the valid prefix, and emits
/// a `warn` tracing event.  A corrupt line **before** the last one is treated
/// as a real corruption and still returns
/// [`StorageError::RolloutCorrupted`] — only the tail position can plausibly
/// be caused by a crash-truncation.
///
/// For the strict (all-or-nothing) variant, use [`read_all`].
///
/// # Errors
///
/// Returns [`StorageError::Io`] when the file cannot be opened.
/// Returns [`StorageError::RolloutCorrupted`] when a non-trailing line fails
/// to decode.
///
/// # Examples
///
/// ```
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use zhive_core::persistence::rollout::read_all_tolerant;
/// let dir = tempfile::tempdir().expect("tempdir");
/// let path = dir.path().join("crash.jsonl");
/// let good  = r#"{"type":"session","version":4,"id":"t","timestamp":0,"cwd":"/"}"#;
/// let trunc = r#"{"type":"item","thread_id":"t","turn"#; // truncated / corrupt
/// tokio::fs::write(&path, format!("{good}\n{trunc}")).await?;
/// let entries = read_all_tolerant(&path).await?;
/// assert_eq!(entries.len(), 1, "trailing bad line discarded");
/// # Ok(())
/// # }
/// # tokio::runtime::Runtime::new().unwrap().block_on(demo()).unwrap();
/// ```
pub async fn read_all_tolerant(path: &Path) -> StorageResult<Vec<RolloutEntry>> {
    let file = tokio::fs::File::open(path).await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let mut entries = Vec::new();
    let mut line_no = 0usize;

    // `pending_bad` holds the line number + parse error of a failed line that
    // has not yet been classified as trailing or mid-file.  If a subsequent
    // non-empty line arrives it becomes mid-file corruption (error); if we reach
    // EOF it is the trailing bad line (discarded with a warning).
    let mut pending_bad: Option<(usize, String)> = None;

    while let Some(line) = lines.next_line().await? {
        line_no += 1;
        if line.trim().is_empty() {
            continue;
        }

        // A previously-failed line is now confirmed NOT to be the trailing line,
        // because the current non-empty line comes after it.
        if let Some((bad_line, reason)) = pending_bad.take() {
            return Err(StorageError::RolloutCorrupted {
                line: bad_line,
                reason,
            });
        }

        match serde_json::from_str::<RolloutEntry>(&line) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                // Don't fail immediately — remember this as potentially trailing.
                pending_bad = Some((line_no, e.to_string()));
            }
        }
    }

    // If `pending_bad` is still set after exhausting the file, the bad line was
    // the trailing one (crash-truncation scenario): discard it with a warning.
    if let Some((bad_line, reason)) = pending_bad {
        tracing::warn!(
            name: "zhive.persistence.rollout.trailing_line_discarded",
            path = %path.display(),
            line = bad_line,
            reason = %reason,
            "trailing corrupt line discarded; valid prefix will be used for rebuild"
        );
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");

        let mut writer = RolloutWriter::open(path.clone()).await.unwrap();
        let session = RolloutEntry::Session {
            version: SESSION_VERSION,
            id: "thread:native/test".into(),
            timestamp: 1_700_000_000,
            cwd: "/tmp".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        };
        writer.append(&session).await.unwrap();
        writer.sync_all().await.unwrap();
        drop(writer);

        let entries = read_all(&path).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], session);
    }

    #[tokio::test]
    async fn set_leaf_id_round_trips_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");

        let mut writer = RolloutWriter::open(path.clone()).await.unwrap();
        writer.set_leaf_id(Some("item:turn:t/0")).await.unwrap();
        writer.sync_all().await.unwrap();
        drop(writer);

        let entries = read_all(&path).await.unwrap();
        assert_eq!(
            entries.last(),
            Some(&RolloutEntry::Leaf {
                target_id: Some("item:turn:t/0".to_owned()),
            })
        );
    }

    #[tokio::test]
    async fn append_session_header_writes_parent_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");

        let mut writer = RolloutWriter::open(path.clone()).await.unwrap();
        writer
            .append_session_header(
                "thread:native/fork/1",
                7,
                "/work",
                Some("thread:native/src"),
            )
            .await
            .unwrap();
        writer.sync_all().await.unwrap();
        drop(writer);

        let entries = read_all(&path).await.unwrap();
        match &entries[0] {
            RolloutEntry::Session {
                version,
                id,
                parent_session,
                subagent_parent,
                source,
                ..
            } => {
                assert_eq!(*version, SESSION_VERSION);
                assert_eq!(id, "thread:native/fork/1");
                assert_eq!(parent_session.as_deref(), Some("thread:native/src"));
                // Fork headers leave subagent fields absent (serialized as None).
                assert!(subagent_parent.is_none());
                assert!(source.is_none());
            }
            other => panic!("expected Session header, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/subdir/rollout.jsonl");
        let mut w = RolloutWriter::open(path.clone()).await.unwrap();
        w.append(&RolloutEntry::Leaf { target_id: None })
            .await
            .unwrap();
        assert!(path.exists());
        assert!(path.parent().unwrap().is_dir());
    }

    #[tokio::test]
    async fn corrupted_line_reports_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let good_session = r#"{"type":"session","version":3,"id":"x","timestamp":0,"cwd":"/"}"#;
        tokio::fs::write(&path, format!("{good_session}\n{{not json\n"))
            .await
            .unwrap();
        let err = read_all(&path).await.unwrap_err();
        assert!(
            matches!(err, StorageError::RolloutCorrupted { line: 2, .. }),
            "expected RolloutCorrupted at line 2, got {err:?}"
        );
    }

    // ----------------------------------------------------------------
    // B8: read_all_tolerant tests
    // ----------------------------------------------------------------

    /// A truncated / corrupt final line is discarded; valid prefix is returned.
    #[tokio::test]
    async fn read_all_tolerant_discards_trailing_truncated_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crash.jsonl");
        let session = r#"{"type":"session","version":4,"id":"t","timestamp":0,"cwd":"/"}"#;
        // Item uses `itemKind` discriminator (not `type`); see domain.rs Item enum.
        let item = r#"{"type":"item","thread_id":"t","turn_id":"turn/0","timestamp":0,"item":{"itemKind":"agent_message","id":"i0","text":"hi"}}"#;
        // Trailing half-written line (no closing brace, no newline).
        let trunc = r#"{"type":"item","thread_id":"t","turn"#;
        tokio::fs::write(&path, format!("{session}\n{item}\n{trunc}"))
            .await
            .unwrap();

        let entries = read_all_tolerant(&path).await.unwrap();
        assert_eq!(
            entries.len(),
            2,
            "session + item retained; trailing bad line dropped"
        );
        assert!(matches!(entries[0], RolloutEntry::Session { .. }));
        assert!(matches!(entries[1], RolloutEntry::Item { .. }));
    }

    /// A corrupt line in the MIDDLE of the file is still treated as real corruption.
    #[tokio::test]
    async fn read_all_tolerant_still_errors_on_mid_file_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mid_corrupt.jsonl");
        let session = r#"{"type":"session","version":4,"id":"t","timestamp":0,"cwd":"/"}"#;
        // Missing `type` field → serde fails to decode as RolloutEntry.
        let bad = r#"{"not_a_type":"unknown","data":"corrupt"}"#;
        let item = r#"{"type":"item","thread_id":"t","turn_id":"turn/0","timestamp":0,"item":{"itemKind":"agent_message","id":"i1","text":"after"}}"#;
        tokio::fs::write(&path, format!("{session}\n{bad}\n{item}\n"))
            .await
            .unwrap();

        let err = read_all_tolerant(&path).await.unwrap_err();
        assert!(
            matches!(err, StorageError::RolloutCorrupted { line: 2, .. }),
            "mid-file corruption must error at line 2, got {err:?}"
        );
    }

    /// The strict `read_all` is not affected by the tolerant variant and still
    /// errors on a trailing bad line (regression guard).
    #[tokio::test]
    async fn read_all_strict_still_errors_on_trailing_bad_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strict.jsonl");
        let session = r#"{"type":"session","version":4,"id":"t","timestamp":0,"cwd":"/"}"#;
        let trunc = r#"{"type":"item","thread_id":"t","turn"#;
        tokio::fs::write(&path, format!("{session}\n{trunc}"))
            .await
            .unwrap();

        let err = read_all(&path).await.unwrap_err();
        assert!(
            matches!(err, StorageError::RolloutCorrupted { .. }),
            "strict read_all must still error on trailing corrupt line, got {err:?}"
        );
    }

    // ----------------------------------------------------------------
    // B9: append_session_header_full round-trip
    // ----------------------------------------------------------------

    /// `append_session_header_full` persists `subagent_parent` and `source` and
    /// they round-trip through `read_all`.
    #[tokio::test]
    async fn append_session_header_full_round_trips_subagent_and_source() {
        use zhive_proto::domain::ThreadSource;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subagent.jsonl");

        let mut w = RolloutWriter::open(path.clone()).await.unwrap();
        w.append_session_header_full(
            "thread:native/child/1",
            42,
            "/work",
            None,
            Some("thread:native/parent"),
            Some(ThreadSource::Subagent),
        )
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        let entries = read_all(&path).await.unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            RolloutEntry::Session {
                id,
                subagent_parent,
                source,
                ..
            } => {
                assert_eq!(id, "thread:native/child/1");
                assert_eq!(subagent_parent.as_deref(), Some("thread:native/parent"));
                assert_eq!(*source, Some(ThreadSource::Subagent));
            }
            other => panic!("expected Session, got {other:?}"),
        }
    }

    // ----------------------------------------------------------------
    // B6: PendingPermission + PermissionResolved round-trip tests
    // ----------------------------------------------------------------

    fn make_request(thread_id: &str, tool: &str) -> RequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "threadId": thread_id,
            "resourceType": "tool",
            "name": tool,
            "reason": "test",
            "options": []
        }))
        .expect("request fixture")
    }

    /// `PendingPermission` and `PermissionResolved` entries survive a JSONL
    /// round-trip through `read_all`.
    #[tokio::test]
    async fn pending_permission_entry_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("perm_rt.jsonl");
        let mut w = RolloutWriter::open(path.clone()).await.unwrap();

        let session = RolloutEntry::Session {
            version: SESSION_VERSION,
            id: "t".into(),
            timestamp: 0,
            cwd: "/".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        };
        let request = make_request("t", "bash");
        let pending = RolloutEntry::PendingPermission {
            thread_id: "t".into(),
            turn_id: "turn:0".into(),
            timestamp: 1,
            request_id: "perm:7".into(),
            request: Box::new(request.clone()),
        };
        let resolved = RolloutEntry::PermissionResolved {
            thread_id: "t".into(),
            request_id: "perm:7".into(),
            timestamp: 2,
        };

        w.append(&session).await.unwrap();
        w.append(&pending).await.unwrap();
        w.append(&resolved).await.unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        let entries = read_all(&path).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0], RolloutEntry::Session { .. }));
        match &entries[1] {
            RolloutEntry::PendingPermission {
                thread_id,
                request_id,
                ..
            } => {
                assert_eq!(thread_id, "t");
                assert_eq!(request_id, "perm:7");
            }
            other => panic!("expected PendingPermission, got {other:?}"),
        }
        match &entries[2] {
            RolloutEntry::PermissionResolved { request_id, .. } => {
                assert_eq!(request_id, "perm:7");
            }
            other => panic!("expected PermissionResolved, got {other:?}"),
        }
    }

    /// A legacy rollout (no `PendingPermission` / `PermissionResolved` entries)
    /// parses cleanly — backward compatibility lock.
    #[tokio::test]
    async fn legacy_rollout_without_pending_permission_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_perm.jsonl");
        // Deliberate v3 Session entry with no new fields.
        let content = concat!(
            r#"{"type":"session","version":3,"id":"old","timestamp":1,"cwd":"/old"}"#,
            "\n",
            r#"{"type":"item","thread_id":"old","turn_id":"turn:0","timestamp":1,"item":{"itemKind":"agent_message","id":"i0","text":"hi"}}"#,
            "\n"
        );
        tokio::fs::write(&path, content).await.unwrap();

        let entries = read_all(&path).await.unwrap();
        assert_eq!(entries.len(), 2);
        // Neither a PendingPermission nor PermissionResolved should appear.
        for e in &entries {
            assert!(
                !matches!(
                    e,
                    RolloutEntry::PendingPermission { .. }
                        | RolloutEntry::PermissionResolved { .. }
                ),
                "legacy rollout must not contain B6 entries"
            );
        }
    }

    /// A v3 (legacy) rollout without `subagent_parent` / `source` deserialises
    /// cleanly with both fields as `None` — backward compatibility lock.
    #[tokio::test]
    async fn legacy_v3_session_without_new_fields_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.jsonl");
        // Deliberately omit subagent_parent and source (as a v3 file would).
        let legacy = r#"{"type":"session","version":3,"id":"old","timestamp":1,"cwd":"/legacy"}"#;
        tokio::fs::write(&path, format!("{legacy}\n"))
            .await
            .unwrap();

        let entries = read_all(&path).await.unwrap();
        match &entries[0] {
            RolloutEntry::Session {
                version,
                id,
                subagent_parent,
                source,
                ..
            } => {
                assert_eq!(*version, 3);
                assert_eq!(id, "old");
                assert!(
                    subagent_parent.is_none(),
                    "v3 file must decode subagent_parent as None"
                );
                assert!(source.is_none(), "v3 file must decode source as None");
            }
            other => panic!("expected Session, got {other:?}"),
        }
    }
}

// Rust guideline compliant 2026-02-21
// B6 compliance: PendingPermission + PermissionResolved variants added, tests below.
