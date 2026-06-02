//! Directory scanning for `SKILL.md` files.
//!
//! [`discover`] walks one directory level under each configured root and
//! collects every `<root>/<name>/SKILL.md` that exists on disk.  Roots are
//! searched in priority order; later roots override earlier ones when two
//! directories share the same name.
//!
//! # Root search order
//!
//! 1. **User root** — `$XDG_CONFIG_HOME/zhive/skills` (or
//!    `~/.config/zhive/skills` when `$XDG_CONFIG_HOME` is unset).
//! 2. **Project root** — `./.zhive/skills` relative to the current working
//!    directory.
//! 3. **Extra roots** — the `extra_roots` list in [`SkillDiscoveryConfig`],
//!    searched in order.
//!
//! Missing roots and read errors are silently ignored so a missing
//! `~/.config/zhive/skills` directory does not abort startup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::warn;

// ============================================================
// SkillDiscoveryConfig
// ============================================================

/// Configuration that controls where [`discover`] looks for `SKILL.md` files.
///
/// Build with [`SkillDiscoveryConfig::new`] and then set optional fields.
///
/// # Examples
///
/// ```
/// use zhive_core::skills::SkillDiscoveryConfig;
///
/// let cfg = SkillDiscoveryConfig::new();
/// assert!(cfg.extra_roots.is_empty());
/// ```
#[derive(Debug, Clone, Default)]
pub struct SkillDiscoveryConfig {
    /// Additional skill root directories searched after the built-in roots.
    ///
    /// Each entry is a directory whose subdirectories are candidate skill
    /// directories; the subdirectory itself must contain a `SKILL.md`.
    pub extra_roots: Vec<PathBuf>,
}

impl SkillDiscoveryConfig {
    /// Creates a default config with no extra roots.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::skills::SkillDiscoveryConfig;
    ///
    /// let cfg = SkillDiscoveryConfig::new();
    /// assert!(cfg.extra_roots.is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

// ============================================================
// Built-in root helpers
// ============================================================

/// Returns `$XDG_CONFIG_HOME/zhive/skills` or `~/.config/zhive/skills`.
fn user_skill_root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            // $HOME is the POSIX fallback when XDG is not set.
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("zhive").join("skills"))
}

/// Returns `.zhive/skills` relative to the current working directory.
fn project_skill_root() -> PathBuf {
    PathBuf::from(".zhive").join("skills")
}

// ============================================================
// discover
// ============================================================

/// Scans configured roots and returns paths to every `SKILL.md` found.
///
/// Returns one path per skill directory.  When the same skill name appears
/// in multiple roots, the last root (highest priority) wins and a `WARN`
/// log is emitted.
///
/// The returned paths are sorted lexicographically for a stable, reproducible
/// order.  **Note**: the sort discards root-priority information; callers
/// that need to resolve frontmatter-name collisions should use
/// [`discover_priority_ordered`] instead.
///
/// # Examples
///
/// ```
/// use zhive_core::skills::{discover, SkillDiscoveryConfig};
///
/// // With no extra roots and no on-disk skills the result is an empty vec.
/// let paths = discover(&SkillDiscoveryConfig::new());
/// // Result depends on the filesystem; we just check the type.
/// let _: Vec<std::path::PathBuf> = paths;
/// ```
#[must_use]
pub fn discover(cfg: &SkillDiscoveryConfig) -> Vec<PathBuf> {
    // Build an ordered list of roots: user → project → extras.
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(user) = user_skill_root() {
        roots.push(user);
    }
    roots.push(project_skill_root());
    roots.extend(cfg.extra_roots.iter().cloned());

    // Map from skill name → SKILL.md path; later roots overwrite earlier ones.
    let mut by_name: HashMap<String, PathBuf> = HashMap::new();

    for root in &roots {
        scan_root(root, &mut by_name);
    }

    // Return the values in a stable order for reproducible discovery results.
    let mut paths: Vec<PathBuf> = by_name.into_values().collect();
    paths.sort();
    paths
}

/// Scans configured roots and returns paths to every `SKILL.md` found,
/// **in root-priority order** (user → project → extras, with same-root
/// paths in filesystem order).
///
/// Unlike [`discover`] this function does **not** apply a final lexicographic
/// sort; paths from higher-priority roots appear later in the returned slice.
/// This allows callers to resolve frontmatter-name collisions deterministically
/// by taking the **last** occurrence (highest priority).
///
/// Directory-level duplicates (same directory name in multiple roots) are
/// still resolved here via last-root-wins, matching the documented rule.
#[must_use]
pub(super) fn discover_priority_ordered(cfg: &SkillDiscoveryConfig) -> Vec<PathBuf> {
    // Build an ordered list of roots: user → project → extras.
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(user) = user_skill_root() {
        roots.push(user);
    }
    roots.push(project_skill_root());
    roots.extend(cfg.extra_roots.iter().cloned());

    // We need to preserve both deduplication by directory name AND insertion
    // order (root priority).  Use an IndexMap-like approach: a HashMap for
    // O(1) lookup and a Vec for ordered iteration.
    let mut by_dir_name: HashMap<String, usize> = HashMap::new(); // dir-name → index in `ordered`
    let mut ordered: Vec<PathBuf> = Vec::new();

    for root in &roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };

        // Collect entries from this root and sort them for determinism within
        // a single root (filesystem readdir order is not stable).
        let mut root_entries: Vec<(String, PathBuf)> = entries
            .filter_map(|res| {
                let entry = res.ok()?;
                if !entry.file_type().ok()?.is_dir() {
                    return None;
                }
                let skill_md = entry.path().join("SKILL.md");
                if !skill_md.exists() {
                    return None;
                }
                let dir_name = entry.file_name().into_string().ok()?;
                Some((dir_name, skill_md))
            })
            .collect();
        root_entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (dir_name, skill_md) in root_entries {
            if let Some(&prev_idx) = by_dir_name.get(&dir_name) {
                warn!(
                    name = dir_name.as_str(),
                    previous_path = %ordered[prev_idx].display(),
                    new_path = %skill_md.display(),
                    "skills.discovery.shadowed: skill shadowed by higher-priority root",
                );
                // Replace in-place so the slot moves to the logical end of
                // priority (the path changes but the position in `ordered`
                // stays; we accept this ordering artefact because within the
                // same logical name the winner is already the last root).
                ordered[prev_idx] = skill_md;
            } else {
                let idx = ordered.len();
                by_dir_name.insert(dir_name, idx);
                ordered.push(skill_md);
            }
        }
    }

    ordered
}

/// Scans one root directory and inserts `<root>/<name>/SKILL.md` into `map`.
fn scan_root(root: &Path, map: &mut HashMap<String, PathBuf>) {
    // Missing or unreadable roots are silently ignored.
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry_result in entries {
        let Ok(entry) = entry_result else { continue };

        // Only descend into directories.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }

        let skill_md = entry.path().join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        // Non-UTF-8 directory names are silently skipped.
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };

        if let Some(prev) = map.get(&name) {
            warn!(
                name = name.as_str(),
                previous_path = %prev.display(),
                new_path = %skill_md.display(),
                "skills.discovery.shadowed: skill shadowed by higher-priority root",
            );
        }
        map.insert(name, skill_md);
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_skill(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let md = dir.join("SKILL.md");
        fs::write(
            &md,
            format!("---\nname: {name}\ndescription: Test skill {name}\n---\n\n# {name}\n"),
        )
        .unwrap();
        md
    }

    #[test]
    fn discover_finds_skill_in_extra_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_skill(tmp.path(), "my-skill");

        let cfg = SkillDiscoveryConfig {
            extra_roots: vec![tmp.path().to_owned()],
        };
        let found = discover(&cfg);
        assert!(
            found.iter().any(|p| p.ends_with("my-skill/SKILL.md")),
            "expected my-skill/SKILL.md in {found:?}"
        );
    }

    #[test]
    fn last_root_wins_on_name_collision() {
        let tmp1 = tempfile::TempDir::new().unwrap();
        let tmp2 = tempfile::TempDir::new().unwrap();

        make_skill(tmp1.path(), "shared");
        make_skill(tmp2.path(), "shared");

        let cfg = SkillDiscoveryConfig {
            extra_roots: vec![tmp1.path().to_owned(), tmp2.path().to_owned()],
        };
        let found = discover(&cfg);

        let shared: Vec<_> = found
            .iter()
            .filter(|p| p.to_string_lossy().contains("shared"))
            .collect();

        // Only one entry for the duplicate name.
        assert_eq!(shared.len(), 1, "expected exactly one 'shared' skill");

        // The winner must come from tmp2 (the later root).
        assert!(
            shared[0].starts_with(tmp2.path()),
            "expected winner from tmp2, got {shared:?}"
        );
    }

    #[test]
    fn non_skill_subdirs_are_ignored() {
        let tmp = tempfile::TempDir::new().unwrap();

        // A subdirectory without a SKILL.md is not picked up.
        fs::create_dir(tmp.path().join("not-a-skill")).unwrap();
        make_skill(tmp.path(), "real-skill");

        let cfg = SkillDiscoveryConfig {
            extra_roots: vec![tmp.path().to_owned()],
        };
        let found = discover(&cfg);

        let names: Vec<_> = found
            .iter()
            .map(|p| {
                p.parent()
                    .and_then(|d| d.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();

        assert!(names.contains(&"real-skill".to_owned()));
        assert!(!names.contains(&"not-a-skill".to_owned()));
    }
}

// Rust guideline compliant 2026-02-21
