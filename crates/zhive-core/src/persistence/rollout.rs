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
use zhive_proto::domain::Item;

use super::error::{StorageError, StorageResult};

/// Current rollout JSONL schema version stamped on every [`RolloutEntry::Session`]
/// header.
///
/// Bumped only on a breaking change to the on-disk line shape; the reader
/// tolerates older versions by best-effort field defaulting. Kept here as the
/// single source of truth so header writers (the persistence writer and the
/// fork path) cannot drift to different version numbers.
pub const SESSION_VERSION: u32 = 3;

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
        let entry = RolloutEntry::Session {
            version: SESSION_VERSION,
            id: id.to_owned(),
            timestamp,
            cwd: cwd.to_owned(),
            parent_session: parent_session.map(str::to_owned),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_and_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");

        let mut writer = RolloutWriter::open(path.clone()).await.unwrap();
        let session = RolloutEntry::Session {
            version: 3,
            id: "thread:native/test".into(),
            timestamp: 1_700_000_000,
            cwd: "/tmp".into(),
            parent_session: None,
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
                ..
            } => {
                assert_eq!(*version, SESSION_VERSION);
                assert_eq!(id, "thread:native/fork/1");
                assert_eq!(parent_session.as_deref(), Some("thread:native/src"));
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
}

// Rust guideline compliant 2026-02-21
