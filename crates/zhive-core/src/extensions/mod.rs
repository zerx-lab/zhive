//! Unified extension-manifest discovery and loading (Phase 1, declarative).
//!
//! This module is the host-side consumer of the [`zhive_proto::manifest`] wire
//! schema. It discovers `manifest.json` documents under the `.zhive` setting
//! roots, parses them via strict kind-dispatch (never the corrupt untagged
//! [`ManifestSpec`](zhive_proto::manifest::ManifestSpec) round-trip — see
//! [`loader`]), and mints [`ExtensionRef`] provenance for each.
//!
//! # Scope (Phase 1)
//!
//! Loading is **declarative**: a Phase-1 manifest's `entrypoint` is restricted
//! to `"builtin"`, so contributed tools/hooks carry no executable. This module
//! produces typed, validated manifests; routing their declarative contributions
//! into the runtime is the unified registry's job, and executable third-party
//! entrypoints (wasm / subprocess) are deferred to Phase 2.
//!
//! Skills keep their own cross-tool [`super::skills`] discovery path
//! (`SKILL.md` under `~/.claude` / `~/.agents` / `.zhive`); this loader handles
//! only the `.zhive/extensions/*/manifest.json` convention. The two converge in
//! the registry, never in the parser.

mod discovery;
mod error;
mod loader;

pub use discovery::{DiscoveredManifest, ExtensionDiscoveryConfig, MANIFEST_FILE, discover};
pub use error::ExtensionError;
pub use loader::{load_manifest_file, parse_manifest};

use std::path::PathBuf;

use tracing::warn;
use zhive_proto::manifest::{
    ExtensionManifest, ExtensionRef, ExtensionSource, ManifestKind, PromptManifest, SkillManifest,
};

/// The kind-specific body of a [`ParsedManifest`].
///
/// Unlike the proto [`ManifestSpec`](zhive_proto::manifest::ManifestSpec), this
/// core-local enum is produced only by strict kind-dispatch in [`loader`], so a
/// `Skill` body is never silently routed to `Prompt`.
#[derive(Debug, Clone, PartialEq)]
pub enum ManifestBody {
    /// `kind: extension` — tools / hooks / slash commands / shortcuts / flags.
    Extension(Box<ExtensionManifest>),
    /// `kind: prompt`.
    Prompt(PromptManifest),
    /// `kind: skill`.
    Skill(SkillManifest),
}

/// A manifest parsed via strict kind-dispatch.
///
/// The core-local counterpart to the proto [`Manifest`](zhive_proto::manifest::Manifest),
/// built by [`parse_manifest`] without the corrupt untagged round-trip. Holds
/// the shared root metadata plus the kind-correct [`ManifestBody`].
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedManifest {
    /// Which manifest family this document carries.
    pub kind: ManifestKind,
    /// Manifest schema version (Phase 1 accepts `"1"`).
    pub schema_version: String,
    /// Unique name within the manifest's namespace.
    pub name: String,
    /// Optional human-readable summary.
    pub description: Option<String>,
    /// The kind-correct body.
    pub body: ManifestBody,
}

/// A discovered, validated extension manifest with its provenance.
///
/// Produced by [`discover_and_load`]. The `manifest` carries the kind-dispatched
/// document; `provenance` identifies which extension and setting source
/// contributed it (red line 10).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedExtension {
    /// Stable identity + version + discovery source of this manifest.
    pub provenance: ExtensionRef,
    /// The kind-dispatched, validated manifest document.
    pub manifest: ParsedManifest,
    /// Path to the `manifest.json` this was loaded from.
    pub path: PathBuf,
}

/// Result of a discovery + load pass over the extension setting roots.
///
/// Per-manifest failures are isolated into `errors` (and logged at `WARN`) so a
/// single malformed manifest never aborts discovery, mirroring
/// [`super::skills::SkillSet`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtensionLoadOutput {
    /// Successfully loaded manifests, in discovery order.
    pub extensions: Vec<LoadedExtension>,
    /// Per-manifest load failures, isolated so boot is not aborted.
    pub errors: Vec<ExtensionError>,
}

/// Discovers and loads every `manifest.json` under the configured setting roots.
///
/// Each manifest is parsed with strict kind-dispatch and tagged with provenance.
/// Malformed manifests are collected into [`ExtensionLoadOutput::errors`] rather
/// than aborting the pass.
///
/// # Examples
///
/// ```
/// use zhive_core::extensions::{discover_and_load, ExtensionDiscoveryConfig};
///
/// // No extra roots and no on-disk extensions -> empty, error-free output.
/// let out = discover_and_load(&ExtensionDiscoveryConfig::new());
/// let _ = out.extensions;
/// let _ = out.errors;
/// ```
#[must_use]
pub fn discover_and_load(cfg: &ExtensionDiscoveryConfig) -> ExtensionLoadOutput {
    let mut output = ExtensionLoadOutput::default();
    for discovered in discover(cfg) {
        match load_manifest_file(&discovered.path) {
            Ok(manifest) => {
                let provenance = mint_provenance(&manifest, discovered.source);
                output.extensions.push(LoadedExtension {
                    provenance,
                    manifest,
                    path: discovered.path,
                });
            }
            Err(err) => {
                warn!(
                    path = %discovered.path.display(),
                    error = %err,
                    "extensions.load.failed: skipping malformed manifest",
                );
                output.errors.push(err);
            }
        }
    }
    output
}

/// Mints an [`ExtensionRef`] from a manifest's identity and source.
///
/// The version comes from the extension body when present (only
/// [`ManifestBody::Extension`] carries a `version`); other kinds use `"0.0.0"`.
fn mint_provenance(manifest: &ParsedManifest, source: ExtensionSource) -> ExtensionRef {
    let version = match &manifest.body {
        ManifestBody::Extension(ext) => ext.version.clone(),
        ManifestBody::Prompt(_) | ManifestBody::Skill(_) => "0.0.0".to_string(),
    };
    ExtensionRef::new(manifest.name.clone(), version, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_manifest(root: &std::path::Path, name: &str, json: &str) {
        let dir = root.join("extensions").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(MANIFEST_FILE), json).unwrap();
    }

    #[test]
    fn discover_and_load_mints_provenance_and_isolates_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            "git-helper",
            r#"{"kind":"extension","schemaVersion":"1","name":"git-helper","displayName":"Git","version":"1.2.3","entrypoint":"builtin"}"#,
        );
        // A malformed manifest (non-builtin entrypoint) must not abort the pass.
        write_manifest(
            tmp.path(),
            "broken",
            r#"{"kind":"extension","schemaVersion":"1","name":"broken","displayName":"X","version":"0.1.0","entrypoint":"wasm"}"#,
        );

        let cfg = ExtensionDiscoveryConfig {
            extra_roots: vec![tmp.path().to_owned()],
        };
        let out = discover_and_load(&cfg);

        assert_eq!(out.extensions.len(), 1, "only the valid manifest loads");
        assert_eq!(out.errors.len(), 1, "the broken manifest is isolated");

        let loaded = &out.extensions[0];
        assert_eq!(loaded.manifest.kind, ManifestKind::Extension);
        assert_eq!(loaded.provenance.id, "git-helper");
        assert_eq!(loaded.provenance.version, "1.2.3", "version from the body");
        assert_eq!(loaded.provenance.source, ExtensionSource::User);
        assert!(
            !loaded.provenance.id.is_empty(),
            "provenance id is non-empty (red line 10)"
        );
    }
}

// Rust guideline compliant 2026-02-21
