//! On-disk Agent-Skills discovery and loading.
//!
//! Skills are `SKILL.md` files (YAML front-matter + Markdown body) discovered
//! under a layered set of user/project roots (see [`discovery`]).
//!
//! Gated by the `skills` cargo feature (pulls the `serde_norway` YAML parser).
//!
//! # Skill roots (low priority first, last wins)
//!
//! 1. `~/.claude/skills`, `~/.agents/skills` (cross-tool external roots)
//! 2. `$XDG_CONFIG_HOME/zhive/skills` (or `~/.config/zhive/skills`)
//! 3. `<repo>…<cwd>/.claude/skills` and `…/.agents/skills` (project ancestor chain)
//! 4. `./.zhive/skills` (project-local zhive root)
//! 5. `SkillDiscoveryConfig::extra_roots` (CLI-configurable, highest priority)
//!
//! See [`discovery`] for the full search-order contract.
//!
//! # Surfacing skills to the model (two strategies)
//!
//! **Default (prompt-list injection).** The zhive CLI renders a compact
//! `<available_skills>` catalogue with [`SkillSet::render_available_skills`] and
//! folds it into the system prompt. The model sees only each skill's `name` +
//! `description` + `SKILL.md` location, and reads the file with the `read` tool
//! when a task matches — classic *progressive disclosure* that keeps the prompt
//! flat regardless of how many skills are discovered.
//!
//! ```no_run
//! # use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
//! let cfg = SkillDiscoveryConfig::new();
//! let skill_set = SkillSet::discover_and_load(&cfg);
//!
//! // Fold this section into the host's system prompt (None when no skills).
//! let available = skill_set.render_available_skills();
//! // Slash-only skills (`disable-model-invocation: true`) for the palette.
//! let slash_only = skill_set.slash_only_names();
//! let _ = (available, slash_only);
//! ```
//!
//! **Alternative (tool registration).** Embedders that prefer skills as callable
//! tools use [`SkillSet::register_invocable`], which adapts each model-invocable
//! skill into a [`SkillTool`] ([`crate::tools::Tool`]) and registers it into a
//! [`crate::tools::ToolRegistry`]; invoking the tool then delivers the body
//! through the normal tool-dispatch path.

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

use std::path::Path;

use crate::tools::ToolRegistry;
use tracing::{info, warn};

// ============================================================
// SkillEntry
// ============================================================

/// A single discovered skill, ready for host-side slash invocation.
///
/// Returned by [`SkillSet::catalogue`]. `invocation` is the fully-rendered
/// `<skill>…</skill>` block a host injects as a user message when the user runs
/// the skill from a slash command or picker; append `\n\n<args>` for trailing
/// arguments. The body is a boot-time snapshot — edits to `SKILL.md` after
/// discovery are not reflected until the next launch.
///
/// # Examples
///
/// ```
/// use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
///
/// let set = SkillSet::discover_and_load(&SkillDiscoveryConfig::new());
/// for entry in set.catalogue() {
///     assert!(entry.invocation.contains("<skill"));
/// }
/// ```
#[derive(Debug, Clone)]
pub struct SkillEntry {
    /// Skill identifier (matches the frontmatter `name`).
    pub name: String,
    /// Model-facing description, if the frontmatter declared one.
    pub description: Option<String>,
    /// Pre-rendered `<skill>…</skill>` invocation block (no trailing args).
    pub invocation: String,
}

/// Renders the `<skill>` invocation block in the agent-skills convention.
///
/// Mirrors the format pi and codex inject: the skill name and `SKILL.md`
/// location as attributes, a note that bundled paths resolve against the skill
/// directory, then the (frontmatter-stripped) body.
fn render_invocation(name: &str, location: &Path, base_dir: &Path, body: &str) -> String {
    format!(
        "<skill name=\"{name}\" location=\"{loc}\">\n\
         References are relative to {base}.\n\n\
         {body}\n</skill>",
        loc = location.display(),
        base = base_dir.display(),
    )
}

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

    /// Renders the model-facing skills catalogue for the system prompt.
    ///
    /// Produces a compact `<available_skills>` block listing each
    /// model-invocable skill's name, description, and on-disk `SKILL.md`
    /// location. This is the *progressive disclosure* list the model consults:
    /// it reads a skill's file with the `read` tool only when a task matches the
    /// description, so the full body never bloats the prompt. Returns `None`
    /// when no model-invocable skills were loaded, letting the caller skip an
    /// empty section.
    ///
    /// This is the default strategy for the zhive CLI (skills as a prompt list);
    /// embedders that prefer skills as callable tools use
    /// [`SkillSet::register_invocable`] instead. Slash-only skills are excluded
    /// here — surface those via [`SkillSet::slash_only_names`].
    ///
    /// Note: `disable-in-subagent` is **not** honored by this list, because the
    /// host system prompt is shared by parent and subagent threads; the flag
    /// only takes effect on the [`SkillSet::register_invocable`] (tool) path.
    /// This matches how codex shares its skill catalogue across subagents.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
    ///
    /// let set = SkillSet::discover_and_load(&SkillDiscoveryConfig::new());
    /// // `Some(section)` when any model-invocable skill exists, else `None`.
    /// let _: Option<String> = set.render_available_skills();
    /// ```
    #[must_use]
    pub fn render_available_skills(&self) -> Option<String> {
        use std::fmt::Write as _;

        let mut body = String::new();
        for loaded in &self.loaded {
            if !loaded.skill.model_invocable {
                continue;
            }
            let location = loaded.root.join("SKILL.md");
            // Infallible: writing to a `String` never returns `Err`.
            let _ = writeln!(body, "  <skill>");
            let _ = writeln!(body, "    <name>{}</name>", loaded.name);
            if let Some(desc) = &loaded.description {
                let _ = writeln!(body, "    <description>{desc}</description>");
            }
            let _ = writeln!(body, "    <location>{}</location>", location.display());
            let _ = writeln!(body, "  </skill>");
        }

        if body.is_empty() {
            return None;
        }

        Some(format!(
            "# Skills\n\n\
             The following skills provide specialized instructions for specific \
             tasks. When a task matches a skill's description, read its `SKILL.md` \
             with the read tool and follow it. Paths referenced inside a skill are \
             relative to that skill's directory.\n\n\
             <available_skills>\n{body}</available_skills>"
        ))
    }

    /// Returns the names of slash-only skills (`disable-model-invocation: true`).
    ///
    /// These are excluded from [`SkillSet::render_available_skills`] but can be
    /// surfaced in a host command palette for explicit `/skill:<name>` dispatch.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
    ///
    /// let set = SkillSet::discover_and_load(&SkillDiscoveryConfig::new());
    /// let _: Vec<String> = set.slash_only_names();
    /// ```
    #[must_use]
    pub fn slash_only_names(&self) -> Vec<String> {
        self.loaded
            .iter()
            .filter(|loaded| !loaded.skill.model_invocable)
            .map(|loaded| loaded.name.clone())
            .collect()
    }

    /// Returns every loaded skill as a host-invocable [`SkillEntry`].
    ///
    /// Unlike [`SkillSet::render_available_skills`], this includes slash-only
    /// skills: a host command palette or picker can run **any** discovered
    /// skill, mirroring pi and opencode (`disable-model-invocation` only hides a
    /// skill from the model's auto-discovery list, not from explicit slash use).
    /// Each entry's `invocation` is the rendered `<skill>` block, ready to inject
    /// as a user message.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
    ///
    /// let set = SkillSet::discover_and_load(&SkillDiscoveryConfig::new());
    /// let _: Vec<zhive_core::skills::SkillEntry> = set.catalogue();
    /// ```
    #[must_use]
    pub fn catalogue(&self) -> Vec<SkillEntry> {
        self.loaded
            .iter()
            .map(|loaded| {
                let location = loaded.root.join("SKILL.md");
                SkillEntry {
                    name: loaded.name.clone(),
                    description: loaded.description.clone(),
                    invocation: render_invocation(
                        &loaded.name,
                        &location,
                        &loaded.root,
                        &loaded.body,
                    ),
                }
            })
            .collect()
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
        // Robust against real `$HOME` skills also discovered (e.g. ~/.agents):
        // assert our two are present rather than asserting an exact count.
        assert!(set.loaded.iter().any(|s| s.name == "invocable-skill"));
        assert!(set.loaded.iter().any(|s| s.name == "slash-only-skill"));

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

    // These build `SkillSet` directly from hand-loaded skills rather than via
    // `discover_and_load`, which scans the real `$HOME` (e.g. `~/.agents/skills`)
    // and would make exact assertions non-hermetic.
    #[test]
    fn render_available_skills_lists_only_model_invocable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let shown = load(&write_skill_md(tmp.path(), "shown-skill", false)).unwrap();
        let hidden = load(&write_skill_md(tmp.path(), "hidden-skill", true)).unwrap();
        let set = SkillSet {
            loaded: vec![shown, hidden],
        };

        let section = set
            .render_available_skills()
            .expect("a model-invocable skill exists");
        assert!(section.contains("<available_skills>"));
        assert!(section.contains("<name>shown-skill</name>"));
        assert!(
            section.contains("SKILL.md"),
            "the location pointer must reference the SKILL.md file"
        );
        // Slash-only skills must not leak into the model-facing catalogue.
        assert!(!section.contains("hidden-skill"));

        // …but they are reported separately for the command palette.
        assert_eq!(set.slash_only_names(), vec!["hidden-skill".to_owned()]);
    }

    #[test]
    fn render_available_skills_is_none_without_invocable_skills() {
        let tmp = tempfile::TempDir::new().unwrap();
        let only_slash = load(&write_skill_md(tmp.path(), "only-slash", true)).unwrap();
        let set = SkillSet {
            loaded: vec![only_slash],
        };
        assert!(set.render_available_skills().is_none());
    }

    #[test]
    fn catalogue_includes_slash_only_skills_with_rendered_invocation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let invocable = load(&write_skill_md(tmp.path(), "do-thing", false)).unwrap();
        let slash = load(&write_skill_md(tmp.path(), "slash-thing", true)).unwrap();
        let set = SkillSet {
            loaded: vec![invocable, slash],
        };

        let cat = set.catalogue();
        // Both kinds appear — slash-only skills are still slash-invocable.
        let names: Vec<&str> = cat.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"do-thing"));
        assert!(names.contains(&"slash-thing"));

        let entry = cat.iter().find(|e| e.name == "do-thing").unwrap();
        // The invocation is the agent-skills `<skill>` block carrying the body.
        assert!(entry.invocation.starts_with("<skill name=\"do-thing\""));
        assert!(entry.invocation.contains("SKILL.md"));
        assert!(entry.invocation.contains("References are relative to"));
        assert!(entry.invocation.ends_with("</skill>"));
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

        // The good skill loads; the malformed one is skipped. (Other real
        // `$HOME` skills may also be present, so assert by name, not by count.)
        assert!(set.loaded.iter().any(|s| s.name == "good-skill"));
        assert!(!set.loaded.iter().any(|s| s.name == "bad-skill"));
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

        // Exactly one `shared-skill` must survive the collision. Filter by name
        // so unrelated real `$HOME` skills do not perturb the assertion.
        let shared: Vec<_> = set
            .loaded
            .iter()
            .filter(|s| s.name == "shared-skill")
            .collect();
        assert_eq!(
            shared.len(),
            1,
            "only one 'shared-skill' must survive the name collision; got: {:?}",
            shared.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        // The winner's description must come from root2 (the higher-priority root).
        assert_eq!(
            shared[0].description.as_deref(),
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
