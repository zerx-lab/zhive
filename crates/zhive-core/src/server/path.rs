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
        assert!(p.as_os_str().is_empty().not_if_macos());
    }

    // Tiny helper so the assertion above reads naturally regardless of OS.
    trait BoolExt {
        fn not_if_macos(self) -> bool;
    }
    impl BoolExt for bool {
        fn not_if_macos(self) -> bool {
            !self
        }
    }
}

// Rust guideline compliant 2026-02-21
