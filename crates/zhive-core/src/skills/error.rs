//! Error type for the Skills engine.
//!
//! All failure paths in discovery, loading, and YAML parsing converge on
//! [`SkillError`].  Callers can match individual variants or treat the error
//! opaquely via its `Display` / `std::error::Error` implementation.
//!
//! # Design notes
//!
//! * [`SkillError::Yaml`] stores the YAML error as a `String` so the enum
//!   remains `Clone` and `PartialEq`—useful in tests and in the
//!   `SkillSet::discover_and_load` accumulator that isolates per-skill
//!   failures.
//! * [`SkillError::Manifest`] wraps
//!   [`zhive_proto::manifest::ManifestError`], which itself is `Clone` +
//!   `PartialEq`, for the same reason.

use std::path::PathBuf;

use thiserror::Error;
use zhive_proto::manifest::ManifestError;

/// All errors that can arise when discovering or loading a skill.
///
/// Returned by [`super::loader::load`] and surfaced (without aborting
/// boot) by [`super::SkillSet::discover_and_load`].
///
/// # Errors
///
/// | Variant | When |
/// |---------|------|
/// | `Io` | The `SKILL.md` file could not be read. |
/// | `MissingFrontmatter` | The file does not begin with `---\n`. |
/// | `Yaml` | The YAML block between the fences failed to deserialize. |
/// | `Manifest` | A schema-level constraint was violated (e.g. empty name). |
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SkillError {
    /// The SKILL.md file could not be read from disk.
    #[error("I/O error loading skill at {path}: {reason}")]
    Io {
        /// Path where the I/O error occurred.
        path: PathBuf,
        /// Operating-system error message.
        reason: String,
    },

    /// The file does not start with a YAML frontmatter fence (`---`).
    #[error("missing YAML frontmatter in skill at {path}")]
    MissingFrontmatter {
        /// Path of the `SKILL.md` file that lacked a frontmatter fence.
        path: PathBuf,
    },

    /// The YAML block between the frontmatter fences could not be parsed.
    #[error("YAML parse error in skill at {path}: {reason}")]
    Yaml {
        /// Path of the `SKILL.md` file with the bad YAML.
        path: PathBuf,
        /// Human-readable diagnostic from the YAML parser.
        reason: String,
    },

    /// A schema-level constraint on the manifest was violated.
    #[error("manifest validation error in skill at {path}: {source}")]
    Manifest {
        /// Path of the offending `SKILL.md` file.
        path: PathBuf,
        /// Underlying manifest validation error.
        source: ManifestError,
    },
}

// Rust guideline compliant 2026-02-21
