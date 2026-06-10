//! Filesystem discovery for extension `manifest.json` documents.
//!
//! [`discover`] scans `<root>/extensions/<name>/manifest.json` under each
//! configured setting root and returns one [`DiscoveredManifest`] per file,
//! tagged with the [`ExtensionSource`] it came from so the loader can mint
//! provenance.
//!
//! # Setting roots (low priority first)
//!
//! Per A5 / D-013 the extension setting sources are, in increasing priority:
//!
//! 1. **User** — `$HOME/.zhive` ([`ExtensionSource::User`]).
//! 2. **Project** — `./.zhive` relative to the cwd ([`ExtensionSource::Project`]).
//! 3. **Local** — `./.zhive.local` relative to the cwd ([`ExtensionSource::Local`]).
//! 4. **Extra roots** — the `extra_roots` in [`ExtensionDiscoveryConfig`],
//!    tagged [`ExtensionSource::User`] (used by the host/tests to inject roots).
//!
//! Unlike the cross-tool skills convention, extensions are discovered ONLY
//! under `~/.zhive` / `.zhive` / `.zhive.local`; the broader `~/.claude` /
//! `~/.agents` skill roots are owned by [`super::super::skills`], which the
//! unified registry aggregates separately. Missing or unreadable roots are
//! silently ignored so an absent directory never aborts startup.

use std::path::{Path, PathBuf};

use zhive_proto::manifest::ExtensionSource;

/// Name of the manifest file inside each extension directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Sub-directory under a setting root that holds extension directories.
const EXTENSIONS_DIR: &str = "extensions";

/// One extension manifest located on disk, with its discovery source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredManifest {
    /// Which setting source the manifest came from (for provenance).
    pub source: ExtensionSource,
    /// Absolute or cwd-relative path to the `manifest.json` file.
    pub path: PathBuf,
}

/// Configuration that controls where [`discover`] looks for manifests.
///
/// Build with [`ExtensionDiscoveryConfig::new`] and then set optional fields.
///
/// # Examples
///
/// ```
/// use zhive_core::extensions::ExtensionDiscoveryConfig;
///
/// let cfg = ExtensionDiscoveryConfig::new();
/// assert!(cfg.extra_roots.is_empty());
/// ```
#[derive(Debug, Clone, Default)]
pub struct ExtensionDiscoveryConfig {
    /// Additional setting roots searched after the built-in ones.
    ///
    /// Each entry is a directory that may contain an `extensions/` subtree;
    /// entries are tagged [`ExtensionSource::User`] for provenance.
    pub extra_roots: Vec<PathBuf>,
}

impl ExtensionDiscoveryConfig {
    /// Creates a default config with no extra roots.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::extensions::ExtensionDiscoveryConfig;
    ///
    /// assert!(ExtensionDiscoveryConfig::new().extra_roots.is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Returns the user's home directory, or `None` when it cannot be determined.
fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

/// Builds the ordered `(source, setting-root)` list, low priority first.
///
/// The returned roots are the `.zhive`-style directories themselves; the
/// `extensions/` sub-directory is joined by [`discover`].
fn setting_roots(cfg: &ExtensionDiscoveryConfig) -> Vec<(ExtensionSource, PathBuf)> {
    let mut roots: Vec<(ExtensionSource, PathBuf)> = Vec::new();
    if let Some(home) = home_dir() {
        roots.push((ExtensionSource::User, home.join(".zhive")));
    }
    roots.push((ExtensionSource::Project, PathBuf::from(".zhive")));
    roots.push((ExtensionSource::Local, PathBuf::from(".zhive.local")));
    roots.extend(
        cfg.extra_roots
            .iter()
            .map(|root| (ExtensionSource::User, root.clone())),
    );
    roots
}

/// Scans configured roots for `extensions/<name>/manifest.json` files.
///
/// Returns one [`DiscoveredManifest`] per file found, in setting-root priority
/// order (low to high); de-duplication by manifest name is left to the unified
/// registry. Missing roots and unreadable directories are silently skipped.
///
/// # Examples
///
/// ```
/// use zhive_core::extensions::{discover, ExtensionDiscoveryConfig};
///
/// // With no extra roots and no on-disk extensions the result is empty-ish;
/// // we just assert the return type here.
/// let found = discover(&ExtensionDiscoveryConfig::new());
/// let _: Vec<zhive_core::extensions::DiscoveredManifest> = found;
/// ```
#[must_use]
pub fn discover(cfg: &ExtensionDiscoveryConfig) -> Vec<DiscoveredManifest> {
    let mut found: Vec<DiscoveredManifest> = Vec::new();
    for (source, root) in setting_roots(cfg) {
        scan_extensions_dir(source, &root.join(EXTENSIONS_DIR), &mut found);
    }
    found
}

/// Scans `<extensions_dir>/<name>/manifest.json` into `out`.
fn scan_extensions_dir(
    source: ExtensionSource,
    extensions_dir: &Path,
    out: &mut Vec<DiscoveredManifest>,
) {
    let Ok(entries) = std::fs::read_dir(extensions_dir) else {
        return;
    };
    // Sort directory entries for a deterministic, reproducible order.
    let mut manifests: Vec<PathBuf> = entries
        .filter_map(|res| {
            let entry = res.ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            let manifest = entry.path().join(MANIFEST_FILE);
            manifest.exists().then_some(manifest)
        })
        .collect();
    manifests.sort();
    out.extend(
        manifests
            .into_iter()
            .map(|path| DiscoveredManifest { source, path }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_manifest(root: &Path, name: &str, json: &str) -> PathBuf {
        let dir = root.join(EXTENSIONS_DIR).join(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(MANIFEST_FILE);
        fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn setting_roots_orders_local_after_project() {
        let cfg = ExtensionDiscoveryConfig::new();
        let roots = setting_roots(&cfg);
        // Project then Local, both relative to cwd.
        assert!(roots.contains(&(ExtensionSource::Project, PathBuf::from(".zhive"))));
        assert!(roots.contains(&(ExtensionSource::Local, PathBuf::from(".zhive.local"))));
        let project_idx = roots
            .iter()
            .position(|(s, _)| *s == ExtensionSource::Project)
            .unwrap();
        let local_idx = roots
            .iter()
            .position(|(s, _)| *s == ExtensionSource::Local)
            .unwrap();
        assert!(local_idx > project_idx, "local must outrank project");
    }

    #[test]
    fn extra_roots_are_tagged_user_and_last() {
        let extra = PathBuf::from("/opt/zhive-ext");
        let cfg = ExtensionDiscoveryConfig {
            extra_roots: vec![extra.clone()],
        };
        let roots = setting_roots(&cfg);
        assert_eq!(roots.last(), Some(&(ExtensionSource::User, extra)));
    }

    #[test]
    fn discover_finds_manifest_in_extra_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "git-helper",
            r#"{"kind":"extension","schemaVersion":"1","name":"git-helper","displayName":"Git","version":"0.1.0","entrypoint":"builtin"}"#,
        );
        let cfg = ExtensionDiscoveryConfig {
            extra_roots: vec![tmp.path().to_owned()],
        };
        let found = discover(&cfg);
        assert!(
            found
                .iter()
                .any(|m| m.path.ends_with("git-helper/manifest.json")
                    && m.source == ExtensionSource::User),
            "expected git-helper manifest tagged User in {found:?}"
        );
    }

    #[test]
    fn directories_without_manifest_are_ignored() {
        let tmp = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(EXTENSIONS_DIR).join("empty")).unwrap();
        write_manifest(
            tmp.path(),
            "real",
            r#"{"kind":"extension","schemaVersion":"1","name":"real","displayName":"Real","version":"0.1.0","entrypoint":"builtin"}"#,
        );
        let cfg = ExtensionDiscoveryConfig {
            extra_roots: vec![tmp.path().to_owned()],
        };
        let found = discover(&cfg);
        assert_eq!(found.len(), 1, "only the dir with a manifest.json counts");
        assert!(found[0].path.ends_with("real/manifest.json"));
    }
}

// Rust guideline compliant 2026-02-21
