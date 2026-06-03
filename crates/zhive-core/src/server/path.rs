//! UDS socket path resolution.
//!
//! Picks the first viable directory:
//!
//! 1. `$XDG_RUNTIME_DIR/zhive.sock` — preferred per the freedesktop.org
//!    base directory spec; the kernel cleans the directory on logout.
//! 2. `/tmp/zhive-<uid>.sock` — last-resort fallback when
//!    `XDG_RUNTIME_DIR` is unset; the uid is read through
//!    [`rustix::process::getuid`] to avoid the unsafe `libc::getuid`
//!    binding (red line 2).

use std::path::PathBuf;

/// Returns the default UDS socket path for the current user.
///
/// # Examples
///
/// ```
/// let p = zhive_core::server::path::default_socket_path();
/// assert!(p.to_string_lossy().ends_with(".sock"));
/// ```
#[must_use]
pub fn default_socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("zhive.sock");
    }
    fallback_tmp_path()
}

/// Returns the startup lock file path for the current user.
///
/// The lock file is placed in the same directory as the default socket
/// path (see [`default_socket_path`]) and named `zhive-startup.lock`.
/// [`serve_uds`](crate::server::serve_uds) acquires an exclusive `flock`
/// on this file to serialise concurrent server startup attempts across
/// processes, preventing a race where two processes each see a stale
/// socket and both attempt to bind.
///
/// # Examples
///
/// ```
/// let p = zhive_core::server::path::startup_lock_path();
/// assert!(p.to_string_lossy().ends_with(".lock"));
/// ```
#[must_use]
pub fn startup_lock_path() -> PathBuf {
    let socket = default_socket_path();
    // `default_socket_path()` always returns an absolute path with at least
    // one parent component (either `$XDG_RUNTIME_DIR/zhive.sock` or
    // `/tmp/zhive-<uid>.sock`), so `parent()` is infallible in practice.
    // We use `map_or_else` to stay panic-free in the extremely unlikely
    // case of a path with no parent.
    let dir = socket
        .parent()
        .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
    dir.join("zhive-startup.lock")
}

#[cfg(unix)]
fn fallback_tmp_path() -> PathBuf {
    let uid = rustix::process::getuid().as_raw();
    PathBuf::from(format!("/tmp/zhive-{uid}.sock"))
}

#[cfg(not(unix))]
fn fallback_tmp_path() -> PathBuf {
    PathBuf::from("zhive.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_not_empty() {
        let p = fallback_tmp_path();
        assert!(!p.as_os_str().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fallback_includes_uid() {
        let p = fallback_tmp_path();
        let uid = rustix::process::getuid().as_raw();
        let s = p.to_string_lossy();
        assert!(s.contains(&uid.to_string()));
        assert!(s.ends_with(".sock"));
    }

    #[test]
    fn startup_lock_path_ends_with_lock() {
        let p = startup_lock_path();
        assert!(p.to_string_lossy().ends_with(".lock"));
    }

    #[test]
    fn startup_lock_path_same_dir_as_socket() {
        let socket = default_socket_path();
        let lock = startup_lock_path();
        // Both must share the same parent directory.
        assert_eq!(socket.parent(), lock.parent());
    }
}

// Rust guideline compliant 2026-02-21
