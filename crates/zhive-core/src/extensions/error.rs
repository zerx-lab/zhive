//! Error type for the extension manifest loader.
//!
//! All failure paths in extension discovery and `manifest.json` loading
//! converge on [`ExtensionError`]. Callers can match individual variants or
//! treat the error opaquely via its `Display` / `std::error::Error`
//! implementation.
//!
//! # Design notes
//!
//! * [`ExtensionError::Json`] stores the parser diagnostic as a `String` so the
//!   enum stays `Clone` + `PartialEq`, mirroring [`super::super::skills::SkillError`]
//!   and letting [`super::discover_and_load`] isolate per-manifest failures.
//! * [`ExtensionError::Manifest`] wraps [`zhive_proto::manifest::ManifestError`],
//!   the schema-level constraint error raised by the kind-dispatched loader.

use std::path::PathBuf;

use thiserror::Error;
use zhive_proto::manifest::ManifestError;

/// All errors that can arise when discovering or loading an extension manifest.
///
/// Returned by [`super::load_manifest_file`] and surfaced (without aborting
/// boot) by [`super::discover_and_load`].
///
/// # Errors
///
/// | Variant | When |
/// |---------|------|
/// | `Io` | The `manifest.json` file could not be read. |
/// | `Json` | The file is not syntactically valid JSON. |
/// | `Manifest` | A schema-level constraint was violated (bad `kind`, missing field, non-`builtin` entrypoint, …). |
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ExtensionError {
    /// The `manifest.json` file could not be read from disk.
    #[error("I/O error loading extension manifest at {path}: {reason}")]
    Io {
        /// Path where the I/O error occurred.
        path: PathBuf,
        /// Operating-system error message.
        reason: String,
    },

    /// The file contents were not syntactically valid JSON.
    #[error("JSON parse error in extension manifest at {path}: {reason}")]
    Json {
        /// Path of the `manifest.json` file with the bad JSON.
        path: PathBuf,
        /// Human-readable diagnostic from the JSON parser.
        reason: String,
    },

    /// A schema-level constraint on the manifest was violated.
    #[error("manifest validation error in extension at {path}: {source}")]
    Manifest {
        /// Path of the offending `manifest.json` file.
        path: PathBuf,
        /// Underlying manifest validation error from the kind-dispatched loader.
        source: ManifestError,
    },
}

// Rust guideline compliant 2026-02-21
