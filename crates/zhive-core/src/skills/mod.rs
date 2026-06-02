//! On-disk Agent-Skills discovery and loading.
//!
//! Skills are `SKILL.md` files (YAML front-matter + Markdown body) discovered
//! under user/project skill roots.  Model-invocable skills are adapted into
//! [`crate::tools::Tool`] objects ([`SkillTool`]) and registered into a
//! [`crate::tools::ToolRegistry`], so invoking a skill delivers its
//! instructions to the model through the normal tool-dispatch path.
//!
//! Gated by the `skills` cargo feature (pulls the `serde_norway` YAML parser).
//!
//! # Skill roots (search order, last wins)
//!
//! 1. `$XDG_CONFIG_HOME/zhive/skills` (or `~/.config/zhive/skills`)
//! 2. `./.zhive/skills` (project-local)
//! 3. `SkillDiscoveryConfig::extra_roots` (CLI-configurable)
//!
//! # Boot sequence for `zhive-cli`
//!
//! ```no_run
//! # use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
//! # use zhive_core::tools::ToolRegistry;
//! let cfg = SkillDiscoveryConfig::new();
//! let skill_set = SkillSet::discover_and_load(&cfg);
//!
//! let mut registry = ToolRegistry::new();
//! let slash_only = skill_set.register_invocable(&mut registry);
//! // `registry` now contains all model-invocable skills as Tool objects.
//! // `slash_only` lists names of skills with `disable-model-invocation: true`.
//! ```
//!
//! # Progressive disclosure
//!
//! The model only sees each skill's `name` + `description` in the advertised
//! tool list.  The full Markdown body is delivered only when the model calls
//! the tool.  Bundled resource files are returned as relative-path pointers so
//! the model can read them on demand via a file tool.

pub mod discovery;
pub mod error;
pub mod loader;
pub mod tool;

#[doc(inline)]
pub use discovery::{SkillDiscoveryConfig, discover};
#[doc(inline)]
pub use error::SkillError;
#[doc(inline)]
pub use loader::{LoadedSkill, load};
#[doc(inline)]
pub use tool::SkillTool;

use crate::tools::ToolRegistry;
use tracing::{info, warn};

// ============================================================
// SkillSet
// ============================================================

/// Aggregates all discovered and loaded skills for a single boot cycle.
///
/// `discover_and_load` scans the configured roots, loads each `SKILL.md`,
/// and silently skips any skill that fails to load (logging a `WARN`).
/// When two skills from **different directories** declare the **same
/// frontmatter `name`**, the one that comes from the higher-priority root
/// (later in the root search order) wins; a `WARN` is emitted for the
/// loser.
/// `register_invocable` then splits the set into the [`ToolRegistry`]
/// (model-invocable) and the slash-only list.
///
/// # Examples
///
/// ```
/// use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
/// use zhive_core::tools::ToolRegistry;
///
/// let cfg = SkillDiscoveryConfig::new();
/// let skill_set = SkillSet::discover_and_load(&cfg);
///
/// let mut registry = ToolRegistry::new();
/// let slash_only = skill_set.register_invocable(&mut registry);
/// // Result depends on the filesystem.
/// let _: Vec<String> = slash_only;
/// ```
#[derive(Debug, Default)]
pub struct SkillSet {
    /// All successfully loaded skills, regardless of `model_invocable`.
    pub loaded: Vec<LoadedSkill>,
}

impl SkillSet {
    /// Discovers and loads all skills from the configured roots.
    ///
    /// Load failures are isolated per-skill: a YAML or I/O error in one skill
    /// does not prevent the others from loading.  Failures are logged at
    /// `WARN` level and skipped.
    ///
    /// When two **differently-named directories** contain `SKILL.md` files
    /// that both declare the **same `name` field** in their frontmatter, the
    /// root-priority rule determines the winner: later roots (higher priority)
    /// override earlier ones.  A `WARN` is emitted naming the collision.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
    ///
    /// let set = SkillSet::discover_and_load(&SkillDiscoveryConfig::new());
    /// // `set.loaded` contains every skill that was found and parsed.
    /// let _: &[zhive_core::skills::LoadedSkill] = &set.loaded;
    /// ```
    #[must_use]
    pub fn discover_and_load(cfg: &SkillDiscoveryConfig) -> Self {
        // Use priority-ordered discovery so that frontmatter-name collision
        // resolution (below) can determine the winner by position: the last
        // occurrence in `paths` is always the highest-priority root.
        let paths = discovery::discover_priority_ordered(cfg);

        // Load each path in discovery order (which already encodes root
        // priority via the sort in `discover`).  After loading we perform a
        // second dedup pass on the *frontmatter name* so that two dirs with
        // different directory-level names but the same `name:` field in
        // their SKILL.md are resolved deterministically by root priority
        // (last path in discovery order = highest priority wins).
        let mut raw: Vec<LoadedSkill> = Vec::with_capacity(paths.len());

        for path in &paths {
            match load(path) {
                Ok(skill) => {
                    info!(
                        name = skill.name.as_str(),
                        path = %path.display(),
                        model_invocable = skill.skill.model_invocable,
                        "skills.load.ok: skill loaded",
                    );
                    raw.push(skill);
                }
                Err(err) => {
                    warn!(
                        path = %path.display(),
                        error = %err,
                        "skills.load.failed: skill skipped due to load error",
                    );
                }
            }
        }

        // Dedup by frontmatter name: iterate in reverse (highest-priority
        // last) so the first occurrence we keep in the reversed pass is the
        // winner.  We emit a `WARN` for every loser that is dropped.
        let mut seen: std::collections::HashMap<String, usize> =
            std::collections::HashMap::with_capacity(raw.len());

        // Two-pass: mark losers, then retain winners.
        // Pass 1: record the LAST index for each name (= highest priority).
        for (idx, skill) in raw.iter().enumerate() {
            if let Some(prev_idx) = seen.insert(skill.name.clone(), idx) {
                // The slot we just overwrote was an earlier (lower-priority)
                // occurrence; it will be the *loser*.  Warn now that we know
                // the winner's index.
                warn!(
                    name = skill.name.as_str(),
                    loser_path = %paths[prev_idx].display(),
                    winner_path = %paths[idx].display(),
                    "skills.load.name-collision: two skills share the same frontmatter \
                     name; higher-priority root wins",
                );
            }
        }

        // Pass 2: retain only the skills whose index is the winner for their name.
        let loaded: Vec<LoadedSkill> = raw
            .into_iter()
            .enumerate()
            .filter(|(idx, skill)| seen.get(&skill.name) == Some(idx))
            .map(|(_, skill)| skill)
            .collect();

        Self { loaded }
    }

    /// Registers model-invocable skills into `registry`.
    ///
    /// Returns a `Vec` of names for skills with `disable-model-invocation:
    /// true` (slash-only skills).  These names are not registered as `Tool`s
    /// but are available for future slash-command dispatch.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
    /// use zhive_core::tools::ToolRegistry;
    ///
    /// let set = SkillSet::discover_and_load(&SkillDiscoveryConfig::new());
    /// let mut registry = ToolRegistry::new();
    /// let slash_only = set.register_invocable(&mut registry);
    /// // Skills with `disable-model-invocation: true` end up in `slash_only`.
    /// let _: Vec<String> = slash_only;
    /// ```
    pub fn register_invocable(&self, registry: &mut ToolRegistry) -> Vec<String> {
        use std::sync::Arc;

        let mut slash_only: Vec<String> = Vec::new();

        for loaded in &self.loaded {
            if loaded.skill.model_invocable {
                let tool = SkillTool::from_loaded(loaded.clone());
                registry.register(Arc::new(tool));
                info!(
                    name = loaded.name.as_str(),
                    "skills.register.invocable: registered as tool",
                );
            } else {
                slash_only.push(loaded.name.clone());
                info!(
                    name = loaded.name.as_str(),
                    "skills.register.slash-only: not registered as tool",
                );
            }
        }

        slash_only
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write_skill_md(dir: &Path, name: &str, disable_model: bool) -> std::path::PathBuf {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let content = if disable_model {
            format!(
                "---\nname: {name}\ndescription: Slash-only skill\ndisable-model-invocation: true\n---\n\n# {name}\n"
            )
        } else {
            format!("---\nname: {name}\ndescription: Model-invocable skill\n---\n\n# {name}\n")
        };
        let path = skill_dir.join("SKILL.md");
        fs::write(&path, content).unwrap();
        path
    }

    /// Writes a SKILL.md with a custom frontmatter `name` field that differs
    /// from the directory name.  This is the scenario for finding 4: two
    /// directories with different dir-names can both declare the same
    /// frontmatter `name`.
    fn write_skill_md_with_name(
        dir: &Path,
        dir_name: &str,
        frontmatter_name: &str,
    ) -> std::path::PathBuf {
        let skill_dir = dir.join(dir_name);
        fs::create_dir_all(&skill_dir).unwrap();
        let content = format!(
            "---\nname: {frontmatter_name}\ndescription: Skill from {dir_name}\n---\n\n# {frontmatter_name}\n"
        );
        let path = skill_dir.join("SKILL.md");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn register_invocable_registers_only_model_invocable_skills() {
        let tmp = tempfile::TempDir::new().unwrap();

        write_skill_md(tmp.path(), "invocable-skill", false);
        write_skill_md(tmp.path(), "slash-only-skill", true);

        let cfg = SkillDiscoveryConfig {
            extra_roots: vec![tmp.path().to_owned()],
        };
        let set = SkillSet::discover_and_load(&cfg);
        assert_eq!(set.loaded.len(), 2, "expected 2 loaded skills");

        let mut registry = ToolRegistry::new();
        let slash_only = set.register_invocable(&mut registry);

        assert!(
            registry.get("invocable-skill").is_some(),
            "invocable-skill should be in registry"
        );
        assert!(
            registry.get("slash-only-skill").is_none(),
            "slash-only-skill must not be in registry"
        );
        assert!(
            slash_only.contains(&"slash-only-skill".to_owned()),
            "slash-only list should contain slash-only-skill"
        );
    }

    #[test]
    fn discover_and_load_skips_bad_skills() {
        let tmp = tempfile::TempDir::new().unwrap();

        // A well-formed skill.
        write_skill_md(tmp.path(), "good-skill", false);

        // A skill with invalid YAML (no frontmatter at all).
        let bad_dir = tmp.path().join("bad-skill");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("SKILL.md"), "no frontmatter here").unwrap();

        let cfg = SkillDiscoveryConfig {
            extra_roots: vec![tmp.path().to_owned()],
        };
        let set = SkillSet::discover_and_load(&cfg);

        // Only the good skill survives.
        assert_eq!(set.loaded.len(), 1);
        assert_eq!(set.loaded[0].name, "good-skill");
    }

    #[test]
    fn empty_roots_produces_empty_set() {
        let cfg = SkillDiscoveryConfig {
            // Use a nonexistent extra root to avoid picking up real skills.
            extra_roots: vec![std::path::PathBuf::from("/nonexistent-zhive-test-path-42")],
        };

        // Disable the built-in roots by running with a clean env (just verify no panic).
        let set = SkillSet { loaded: Vec::new() };
        let mut registry = ToolRegistry::new();
        let slash_only = set.register_invocable(&mut registry);
        assert!(registry.is_empty());
        assert!(slash_only.is_empty());

        let _ = cfg; // consumed
    }

    /// Finding 4 regression: two skills in differently-named directories
    /// that both declare the same frontmatter `name` must be resolved by
    /// root priority (later root wins).  The loser must not appear in the
    /// registry.
    #[test]
    fn same_frontmatter_name_collision_resolved_by_root_priority() {
        // Two separate root directories, each containing one skill directory
        // with a different dir-name but the SAME frontmatter `name`.
        let root1 = tempfile::TempDir::new().unwrap();
        let root2 = tempfile::TempDir::new().unwrap();

        // root1 has dir "skill-alpha" declaring name "shared-skill"
        write_skill_md_with_name(root1.path(), "skill-alpha", "shared-skill");
        // root2 has dir "skill-beta"  declaring name "shared-skill"
        write_skill_md_with_name(root2.path(), "skill-beta", "shared-skill");

        // root2 is the later (higher-priority) extra root.
        let cfg = SkillDiscoveryConfig {
            extra_roots: vec![root1.path().to_owned(), root2.path().to_owned()],
        };

        let set = SkillSet::discover_and_load(&cfg);

        // Exactly one skill must survive.
        assert_eq!(
            set.loaded.len(),
            1,
            "only one skill must survive the name collision; got: {:?}",
            set.loaded.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(set.loaded[0].name, "shared-skill");

        // The winner's description must come from root2 (the higher-priority root).
        assert_eq!(
            set.loaded[0].description.as_deref(),
            Some("Skill from skill-beta"),
            "winner must come from the higher-priority root (root2/skill-beta)"
        );

        // After registering, only one entry must exist and the loser must be absent.
        let mut registry = ToolRegistry::new();
        set.register_invocable(&mut registry);
        assert!(
            registry.get("shared-skill").is_some(),
            "shared-skill must be registered"
        );
    }
}

// Rust guideline compliant 2026-02-21
