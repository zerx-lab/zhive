//! Queryable registry over discovered + loaded extension manifests.
//!
//! [`ExtensionRegistry`] is the unified view the JSON-RPC layer queries for
//! `extension/list` and `extension/load`. It owns the loaded extension
//! manifests (de-duplicated by name, highest-priority setting root winning) and
//! exposes compact [`ExtensionSummary`] digests plus by-name lookup.
//!
//! # Namespace separation (D-013)
//!
//! This registry holds the **extension** namespace only. Skills keep their own
//! [`super::super::skills::SkillSet`] path (cross-tool `SKILL.md` discovery with
//! its own collision/priority resolution); the host aggregates the two at query
//! time so there is exactly one dedup layer per namespace — the registry never
//! re-parses or re-dedups skills.

use std::collections::BTreeMap;

use zhive_proto::manifest::{ExtensionRef, ManifestKind};

use super::{
    ExtensionDiscoveryConfig, ExtensionError, ExtensionLoadOutput, LoadedExtension, ManifestBody,
    discover_and_load,
};

/// Count of each declarative contribution an extension carries.
///
/// Mirrors the [`ExtensionManifest`](zhive_proto::manifest::ExtensionManifest)
/// sub-table list so a client can preview an extension without loading it.
/// Non-extension kinds (prompt / skill) carry no sub-contributions and report
/// all zeros.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContributionDigest {
    /// Number of `[[tools]]` declared.
    pub tools: usize,
    /// Number of `[[hooks]]` declared.
    pub hooks: usize,
    /// Number of `[[slash_commands]]` declared.
    pub slash_commands: usize,
    /// Number of `[[shortcuts]]` declared.
    pub shortcuts: usize,
    /// Number of `[[flags]]` declared.
    pub flags: usize,
}

impl ContributionDigest {
    /// Summarises the declarative contributions of a manifest body.
    fn for_body(body: &ManifestBody) -> Self {
        match body {
            ManifestBody::Extension(ext) => Self {
                tools: ext.tools.len(),
                hooks: ext.hooks.len(),
                slash_commands: ext.slash_commands.len(),
                shortcuts: ext.shortcuts.len(),
                flags: ext.flags.len(),
            },
            ManifestBody::Prompt(_) | ManifestBody::Skill(_) => Self::default(),
        }
    }

    /// Returns `true` when the extension declares no contributions at all.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::extensions::ContributionDigest;
    /// assert!(ContributionDigest::default().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools == 0
            && self.hooks == 0
            && self.slash_commands == 0
            && self.shortcuts == 0
            && self.flags == 0
    }
}

/// A compact, queryable summary of one loaded extension.
///
/// Returned by [`ExtensionRegistry::summaries`] and surfaced over
/// `extension/list` without forcing the client to load the full manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionSummary {
    /// Manifest name (unique within the registry).
    pub name: String,
    /// Manifest family (`extension` / `prompt` / `skill`).
    pub kind: ManifestKind,
    /// Provenance: identity, version, and setting source (red line 10).
    pub provenance: ExtensionRef,
    /// Optional human-readable summary.
    pub description: Option<String>,
    /// Declarative contribution counts.
    pub contributions: ContributionDigest,
}

/// Queryable registry over the loaded extension namespace.
///
/// Build with [`ExtensionRegistry::discover_and_load`]. Manifests are
/// de-duplicated by name, with the highest-priority setting root winning
/// (`Local` > `Project` > `User`).
#[derive(Debug, Clone, Default)]
pub struct ExtensionRegistry {
    by_name: BTreeMap<String, LoadedExtension>,
    errors: Vec<ExtensionError>,
}

impl ExtensionRegistry {
    /// Discovers and loads every extension manifest, then indexes it.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::extensions::{ExtensionDiscoveryConfig, ExtensionRegistry};
    ///
    /// let reg = ExtensionRegistry::discover_and_load(&ExtensionDiscoveryConfig::new());
    /// assert!(reg.is_empty()); // no on-disk extensions under the default roots
    /// ```
    #[must_use]
    pub fn discover_and_load(cfg: &ExtensionDiscoveryConfig) -> Self {
        Self::from_output(discover_and_load(cfg))
    }

    /// Indexes an already-produced [`ExtensionLoadOutput`].
    ///
    /// The input is consumed in discovery order (low to high priority), so a
    /// later manifest with the same name overrides an earlier one.
    #[must_use]
    pub fn from_output(output: ExtensionLoadOutput) -> Self {
        let mut by_name: BTreeMap<String, LoadedExtension> = BTreeMap::new();
        for loaded in output.extensions {
            // Last write wins: discovery yields low-priority roots first.
            by_name.insert(loaded.manifest.name.clone(), loaded);
        }
        Self {
            by_name,
            errors: output.errors,
        }
    }

    /// Returns one [`ExtensionSummary`] per loaded extension, ordered by name.
    #[must_use]
    pub fn summaries(&self) -> Vec<ExtensionSummary> {
        self.by_name
            .values()
            .map(|loaded| ExtensionSummary {
                name: loaded.manifest.name.clone(),
                kind: loaded.manifest.kind,
                provenance: loaded.provenance.clone(),
                description: loaded.manifest.description.clone(),
                contributions: ContributionDigest::for_body(&loaded.manifest.body),
            })
            .collect()
    }

    /// Looks up a loaded extension by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&LoadedExtension> {
        self.by_name.get(name)
    }

    /// Returns the number of distinct loaded extensions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Returns `true` when no extensions are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Returns the per-manifest load failures isolated during discovery.
    #[must_use]
    pub fn errors(&self) -> &[ExtensionError] {
        &self.errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use zhive_proto::manifest::ExtensionSource;

    fn write_manifest(root: &std::path::Path, dir: &str, json: &str) {
        let path = root.join("extensions").join(dir);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("manifest.json"), json).unwrap();
    }

    #[test]
    fn registry_summarises_contributions() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "git",
            r#"{
                "kind":"extension","schemaVersion":"1","name":"git","displayName":"Git",
                "version":"1.0.0","entrypoint":"builtin",
                "slashCommands":[{"name":"blame","target":"tool:git_blame"}],
                "flags":[{"name":"verbose","type":"boolean"}]
            }"#,
        );
        let cfg = ExtensionDiscoveryConfig {
            extra_roots: vec![tmp.path().to_owned()],
        };
        let reg = ExtensionRegistry::discover_and_load(&cfg);

        assert_eq!(reg.len(), 1);
        let summaries = reg.summaries();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.name, "git");
        assert_eq!(s.kind, ManifestKind::Extension);
        assert_eq!(s.provenance.source, ExtensionSource::User);
        assert_eq!(s.contributions.slash_commands, 1);
        assert_eq!(s.contributions.flags, 1);
        assert_eq!(s.contributions.tools, 0);
        assert!(!s.contributions.is_empty());

        assert!(reg.get("git").is_some());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn registry_dedups_by_name_last_wins() {
        // Two roots declaring the same extension name; the later (higher
        // priority) root must win.
        let low = tempfile::TempDir::new().unwrap();
        let high = tempfile::TempDir::new().unwrap();
        write_manifest(
            low.path(),
            "shared",
            r#"{"kind":"extension","schemaVersion":"1","name":"shared","displayName":"Low","version":"1.0.0","entrypoint":"builtin"}"#,
        );
        write_manifest(
            high.path(),
            "shared",
            r#"{"kind":"extension","schemaVersion":"1","name":"shared","displayName":"High","version":"2.0.0","entrypoint":"builtin"}"#,
        );
        let cfg = ExtensionDiscoveryConfig {
            extra_roots: vec![low.path().to_owned(), high.path().to_owned()],
        };
        let reg = ExtensionRegistry::discover_and_load(&cfg);

        assert_eq!(reg.len(), 1, "same name collapses to one entry");
        let loaded = reg.get("shared").expect("shared present");
        assert_eq!(
            loaded.provenance.version, "2.0.0",
            "higher-priority root wins"
        );
    }
}

// Rust guideline compliant 2026-02-21
