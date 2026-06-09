//! Independent shadow git repository for workspace file snapshots.
//!
//! A [`ShadowRepo`] maintains its own `GIT_DIR` (under the engine's data
//! directory) pointed at a workspace root via `--work-tree`. It never touches
//! the user's `.git`. Snapshots are content-addressed git *tree* objects
//! produced by `git write-tree` (no commits, so no git identity is required and
//! no reflog is polluted). A 40-hex tree id is the entire durable artefact: the
//! same blob is stored once regardless of how many turns reference it, binaries
//! are handled natively, and reverting reads file contents straight out of the
//! tree object.
//!
//! This mirrors opencode's shadow-git snapshot mechanism. Every git invocation
//! is isolated from the host environment so a snapshot of an LFS / hook-using
//! repository cannot corrupt the user's files (see [`ShadowRepo::git`]).
//!
//! All operations on a single repo are serialised by an internal
//! [`tokio::sync::Mutex`] because they share one on-disk index (`index.lock`
//! contention would otherwise corrupt concurrent `git add`s).

use std::path::{Path, PathBuf};
use std::process::Output;

use thiserror::Error;
use tokio::process::Command;
use tokio::sync::Mutex;

/// FNV-1a 64-bit offset basis.
///
/// Used to derive a deterministic per-workspace shadow-repo directory name.
/// `std::hash::DefaultHasher` is deliberately avoided because it is not
/// guaranteed stable across Rust versions or platforms — a toolchain change
/// would relocate the shadow repo and orphan every prior snapshot.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Failures surfaced by [`ShadowRepo`] operations.
///
/// All variants render a human-readable summary via `Display`; the engine maps
/// any of them to an honest "undo unavailable" outcome rather than pretending a
/// revert succeeded.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// The `git` binary could not be found or executed.
    #[error("git is not available: {0}")]
    GitUnavailable(String),

    /// A git subcommand exited with a non-zero status.
    #[error("git {command} failed (status {status}): {stderr}")]
    GitFailed {
        /// The git subcommand (argv joined by spaces) that failed.
        command: String,
        /// Process exit status code, or `-1` when terminated by a signal.
        status: i32,
        /// Captured standard error, trimmed for diagnostics.
        stderr: String,
    },

    /// Spawning or awaiting a git child process failed at the OS level.
    #[error("i/o error running git: {0}")]
    Io(String),
}

/// Result of restoring the workspace to a snapshot tree.
///
/// Every path is workspace-relative. `reverted` files had their content
/// rewritten from the tree; `deleted` files were absent from the tree (created
/// after the snapshot) and removed from disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RestoreOutcome {
    /// Files whose content was restored from the target tree.
    pub reverted: Vec<String>,
    /// Files that did not exist in the target tree and were deleted.
    pub deleted: Vec<String>,
}

/// A workspace's independent shadow git repository.
///
/// Construct with [`ShadowRepo::open`]. Cheap to hold; the heavyweight work
/// happens inside [`ShadowRepo::track`] / [`ShadowRepo::restore_to`], each of
/// which spawns git subprocesses.
#[derive(Debug)]
pub struct ShadowRepo {
    /// `GIT_DIR` for the shadow repo (under the engine data directory).
    git_dir: PathBuf,
    /// Canonical workspace root used as the git `--work-tree`.
    work_tree: PathBuf,
    /// Serialises all git operations against the single shared index.
    lock: Mutex<()>,
}

impl ShadowRepo {
    /// Opens (initialising on first use) the shadow repo for `workspace_root`.
    ///
    /// `base_dir` is the parent directory under which per-workspace shadow
    /// repositories live (e.g. `~/.local/share/zhive/shadow`). The repo
    /// directory name is a deterministic function of `workspace_root` so the
    /// same workspace always resolves to the same shadow repo across restarts.
    ///
    /// `workspace_root` should already be canonicalised by the caller so that
    /// equivalent paths (symlinks, `.` components) map to one shadow repo.
    ///
    /// # Errors
    ///
    /// * [`SnapshotError::GitUnavailable`] — no usable `git` binary on `PATH`.
    /// * [`SnapshotError::Io`] — the shadow directory could not be created.
    /// * [`SnapshotError::GitFailed`] — `git init` reported an error.
    pub async fn open(
        base_dir: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, SnapshotError> {
        ensure_git_available().await?;
        let work_tree = workspace_root.as_ref().to_path_buf();
        let git_dir = base_dir.as_ref().join(shadow_key(&work_tree));
        let repo = Self {
            git_dir,
            work_tree,
            lock: Mutex::new(()),
        };
        repo.init_if_needed().await?;
        Ok(repo)
    }

    /// Captures the current workspace state and returns its 40-hex tree id.
    ///
    /// Stages every non-ignored file (`git add -A`, which honours `.gitignore`
    /// files found in the work tree) then writes a tree object. No commit is
    /// created. Repeated calls that observe an unchanged workspace return the
    /// same hash, so callers can cheaply detect "no file changes this turn".
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if either git invocation fails.
    pub async fn track(&self) -> Result<String, SnapshotError> {
        let _guard = self.lock.lock().await;
        self.run(&["add", "-A"]).await?;
        let out = self.run(&["write-tree"]).await?;
        Ok(stdout_trimmed(&out))
    }

    /// Reverts the workspace to `target_tree`, file by file.
    ///
    /// Only files that actually differ between the current workspace and
    /// `target_tree` are touched (computed via a staged diff). Files present in
    /// the tree are rewritten from it; files absent from the tree (created after
    /// the snapshot) are deleted from disk. Files the snapshot never knew about
    /// and that did not change are left untouched, so a user's unrelated manual
    /// edits are not collateral damage.
    ///
    /// `target_tree` must be a tree id previously returned by
    /// [`ShadowRepo::track`].
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if staging, diffing, or a per-file checkout /
    /// delete fails at the OS level.
    pub async fn restore_to(&self, target_tree: &str) -> Result<RestoreOutcome, SnapshotError> {
        let _guard = self.lock.lock().await;
        // Stage current state so newly-created (untracked) files surface in the
        // cached diff against the target tree as additions to be deleted.
        self.run(&["add", "-A"]).await?;
        let diff = self
            .run(&["diff", "--cached", "--name-only", "-z", target_tree])
            .await?;
        let mut outcome = RestoreOutcome::default();
        for rel in split_nul(&diff.stdout) {
            let checkout = self.spawn(&["checkout", target_tree, "--", &rel]).await?;
            if checkout.status.success() {
                outcome.reverted.push(rel);
            } else {
                // The path is not in the target tree: it was created after the
                // snapshot. Remove just the file (or symlink — `remove_file`
                // does not follow links). A missing file is already in the
                // desired state and is not an error.
                let abs = self.work_tree.join(&rel);
                match tokio::fs::remove_file(&abs).await {
                    Ok(()) => outcome.deleted.push(rel),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(SnapshotError::Io(e.to_string())),
                }
            }
        }
        Ok(outcome)
    }

    /// Counts how many files differ between `target_tree` and the live workspace.
    ///
    /// Used to annotate a checkpoint with the number of files that would be
    /// reverted if the user selected it. Stages the workspace first so newly
    /// created files are included.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if staging or diffing fails.
    pub async fn changed_since(&self, target_tree: &str) -> Result<usize, SnapshotError> {
        let _guard = self.lock.lock().await;
        self.run(&["add", "-A"]).await?;
        let diff = self
            .run(&["diff", "--cached", "--name-only", "-z", target_tree])
            .await?;
        Ok(split_nul(&diff.stdout).len())
    }

    /// Initialises the shadow repo on first use (idempotent).
    async fn init_if_needed(&self) -> Result<(), SnapshotError> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        if let Some(parent) = self.git_dir.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| SnapshotError::Io(e.to_string()))?;
        }
        // `git init` with explicit GIT_DIR / GIT_WORK_TREE creates the shadow
        // repo without a commit identity and without touching the user's repo.
        let mut cmd = Command::new("git");
        cmd.env("GIT_DIR", &self.git_dir);
        cmd.env("GIT_WORK_TREE", &self.work_tree);
        cmd.arg("init");
        cmd.kill_on_drop(true);
        let out = cmd
            .output()
            .await
            .map_err(|e| SnapshotError::Io(e.to_string()))?;
        if !out.status.success() {
            return Err(SnapshotError::GitFailed {
                command: "init".to_owned(),
                status: out.status.code().unwrap_or(-1),
                stderr: stderr_trimmed(&out),
            });
        }
        Ok(())
    }

    /// Builds a `git` command pre-configured with isolation flags and paths.
    ///
    /// The `-c` overrides neutralise host configuration that could corrupt a
    /// snapshot or run untrusted code:
    /// * `core.hooksPath=/dev/null` — never run repo hooks.
    /// * `filter.lfs.*` set to identity — store/restore LFS-tracked files
    ///   verbatim instead of replacing them with pointer text.
    /// * `core.autocrlf=false` / `core.quotepath=false` — byte-faithful paths
    ///   and contents.
    fn git(&self) -> Command {
        let mut c = Command::new("git");
        c.current_dir(&self.work_tree);
        c.args(["-c", "core.hooksPath=/dev/null"]);
        c.args(["-c", "filter.lfs.smudge=cat"]);
        c.args(["-c", "filter.lfs.clean=cat"]);
        c.args(["-c", "filter.lfs.process="]);
        c.args(["-c", "core.autocrlf=false"]);
        c.args(["-c", "core.quotepath=false"]);
        c.arg("--git-dir").arg(&self.git_dir);
        c.arg("--work-tree").arg(&self.work_tree);
        c.kill_on_drop(true);
        c
    }

    /// Runs a git subcommand, erroring on a non-zero exit status.
    async fn run(&self, args: &[&str]) -> Result<Output, SnapshotError> {
        let out = self.spawn(args).await?;
        if !out.status.success() {
            return Err(SnapshotError::GitFailed {
                command: args.join(" "),
                status: out.status.code().unwrap_or(-1),
                stderr: stderr_trimmed(&out),
            });
        }
        Ok(out)
    }

    /// Spawns a git subcommand and returns its raw output (success or failure).
    async fn spawn(&self, args: &[&str]) -> Result<Output, SnapshotError> {
        let mut cmd = self.git();
        cmd.args(args);
        cmd.output()
            .await
            .map_err(|e| SnapshotError::Io(e.to_string()))
    }
}

/// Probes for a usable `git` binary.
///
/// # Errors
///
/// Returns [`SnapshotError::GitUnavailable`] when `git --version` cannot be
/// spawned or reports failure.
async fn ensure_git_available() -> Result<(), SnapshotError> {
    let mut cmd = Command::new("git");
    cmd.arg("--version");
    cmd.kill_on_drop(true);
    match cmd.output().await {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(SnapshotError::GitUnavailable(stderr_trimmed(&out))),
        Err(e) => Err(SnapshotError::GitUnavailable(e.to_string())),
    }
}

/// Derives a stable, filesystem-safe shadow-repo directory name for `path`.
///
/// Combines a deterministic FNV-1a hash of the path bytes (collision-resistant,
/// stable across toolchains) with a sanitised, human-readable tail so the
/// directory is identifiable when inspecting the data dir.
fn shadow_key(path: &Path) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    let tail: String = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if tail.is_empty() {
        format!("{hash:016x}")
    } else {
        format!("{hash:016x}-{tail}")
    }
}

/// Returns a command's stdout, trimmed of trailing whitespace, lossily decoded.
fn stdout_trimmed(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Returns a command's stderr, trimmed of trailing whitespace, lossily decoded.
fn stderr_trimmed(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_owned()
}

/// Splits NUL-separated git output (from `-z`) into workspace-relative paths.
///
/// Paths are decoded lossily; the empty trailing element after the final NUL is
/// dropped.
fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skips a test when no `git` binary is present in the test environment.
    macro_rules! require_git {
        () => {
            if ensure_git_available().await.is_err() {
                eprintln!("skipping: git not available");
                return;
            }
        };
    }

    #[test]
    fn shadow_key_is_deterministic_and_sanitised() {
        let a = shadow_key(Path::new("/home/user/My Project!"));
        let b = shadow_key(Path::new("/home/user/My Project!"));
        assert_eq!(a, b, "same path must map to same key");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "key must be filesystem-safe: {a}"
        );
        assert_ne!(shadow_key(Path::new("/a")), shadow_key(Path::new("/b")));
    }

    #[test]
    fn split_nul_drops_trailing_empty() {
        assert_eq!(split_nul(b"a\0b\0"), vec!["a".to_owned(), "b".to_owned()]);
        assert!(split_nul(b"").is_empty());
    }

    #[tokio::test]
    async fn track_then_restore_round_trips_modification() {
        require_git!();
        let base = tempfile::tempdir().expect("base tempdir");
        let work = tempfile::tempdir().expect("work tempdir");
        let file = work.path().join("a.txt");
        tokio::fs::write(&file, b"original").await.expect("write");

        let repo = ShadowRepo::open(base.path(), work.path())
            .await
            .expect("open shadow repo");
        let snap = repo.track().await.expect("track");

        tokio::fs::write(&file, b"modified").await.expect("rewrite");
        let outcome = repo.restore_to(&snap).await.expect("restore");

        assert_eq!(outcome.reverted, vec!["a.txt".to_owned()]);
        let content = tokio::fs::read_to_string(&file).await.expect("read");
        assert_eq!(content, "original");
    }

    #[tokio::test]
    async fn restore_deletes_files_created_after_snapshot() {
        require_git!();
        let base = tempfile::tempdir().expect("base tempdir");
        let work = tempfile::tempdir().expect("work tempdir");
        tokio::fs::write(work.path().join("keep.txt"), b"keep")
            .await
            .expect("seed");

        let repo = ShadowRepo::open(base.path(), work.path())
            .await
            .expect("open");
        let snap = repo.track().await.expect("track");

        let created = work.path().join("new.txt");
        tokio::fs::write(&created, b"created after snapshot")
            .await
            .expect("create");
        let outcome = repo.restore_to(&snap).await.expect("restore");

        assert_eq!(outcome.deleted, vec!["new.txt".to_owned()]);
        assert!(!created.exists(), "new file should be deleted");
    }

    #[tokio::test]
    async fn unchanged_workspace_tracks_to_stable_hash() {
        require_git!();
        let base = tempfile::tempdir().expect("base tempdir");
        let work = tempfile::tempdir().expect("work tempdir");
        tokio::fs::write(work.path().join("x.txt"), b"x")
            .await
            .expect("seed");
        let repo = ShadowRepo::open(base.path(), work.path())
            .await
            .expect("open");
        let first = repo.track().await.expect("track 1");
        let second = repo.track().await.expect("track 2");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn changed_since_counts_modified_files() {
        require_git!();
        let base = tempfile::tempdir().expect("base tempdir");
        let work = tempfile::tempdir().expect("work tempdir");
        tokio::fs::write(work.path().join("a.txt"), b"a")
            .await
            .expect("seed a");
        let repo = ShadowRepo::open(base.path(), work.path())
            .await
            .expect("open");
        let snap = repo.track().await.expect("track");
        tokio::fs::write(work.path().join("a.txt"), b"aa")
            .await
            .expect("modify a");
        tokio::fs::write(work.path().join("b.txt"), b"b")
            .await
            .expect("add b");
        assert_eq!(repo.changed_since(&snap).await.expect("count"), 2);
    }
}

// Rust guideline compliant 2026-02-21
