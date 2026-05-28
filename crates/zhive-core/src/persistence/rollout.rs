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
    /// untouched.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Io`] when the file cannot be opened.
    pub async fn open(path: PathBuf) -> StorageResult<Self> {
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
