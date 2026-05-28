//! JSON Schema re-validation cache (red line 11).
//!
//! After a `PreToolUse` hook mutates `tool_input`, the engine must
//! re-validate the new payload against the tool's declared schema
//! before dispatching the call — otherwise a hook could smuggle a
//! malformed argument set into the tool.
//!
//! The cache compiles each schema once and stores the compiled
//! validator behind a [`std::sync::RwLock`]; concurrent reads share
//! the compiled value.

use std::collections::HashMap;
use std::sync::RwLock;

use jsonschema::Validator;
use serde_json::Value;
use thiserror::Error;

/// Reasons a re-validation can fail.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ValidatorError {
    /// The supplied schema text was not valid JSON Schema.
    #[error("invalid schema for tool {tool}: {reason}")]
    InvalidSchema {
        /// Offending tool name.
        tool: String,
        /// Parser diagnostic.
        reason: String,
    },
    /// The schema did exist, but `tool_input` did not satisfy it.
    #[error("re-validation failed for tool {tool}: {reason}")]
    RevalidationFailed {
        /// Offending tool name.
        tool: String,
        /// Aggregated validator diagnostic.
        reason: String,
    },
    /// The tool name has no registered schema; the host must refuse the
    /// mutation rather than skip validation silently.
    #[error("no schema registered for tool {tool}")]
    UnknownTool {
        /// Offending tool name.
        tool: String,
    },
}

/// Compiles tool schemas once and reuses the result for every
/// re-validation.
#[derive(Default)]
pub struct SchemaCache {
    schemas: RwLock<HashMap<String, Validator>>,
}

impl std::fmt::Debug for SchemaCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaCache")
            .field("count", &self.schemas.read().map_or(0, |m| m.len()))
            .finish()
    }
}

impl SchemaCache {
    /// Builds an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a JSON Schema for the named tool, replacing any
    /// previous schema.
    ///
    /// # Errors
    ///
    /// Returns [`ValidatorError::InvalidSchema`] when the schema does
    /// not compile.
    pub fn register(&self, tool: &str, schema: &Value) -> Result<(), ValidatorError> {
        let compiled =
            jsonschema::validator_for(schema).map_err(|e| ValidatorError::InvalidSchema {
                tool: tool.to_string(),
                reason: e.to_string(),
            })?;
        let mut guard = self.schemas.write().expect("schema cache lock poisoned");
        guard.insert(tool.to_string(), compiled);
        Ok(())
    }

    /// Re-validates `input` against the registered schema for `tool`.
    ///
    /// # Errors
    ///
    /// * [`ValidatorError::UnknownTool`] when no schema was registered.
    /// * [`ValidatorError::RevalidationFailed`] when `input` does not
    ///   satisfy the schema; the message contains every reported
    ///   error.
    pub fn revalidate(&self, tool: &str, input: &Value) -> Result<(), ValidatorError> {
        let guard = self.schemas.read().expect("schema cache lock poisoned");
        let Some(validator) = guard.get(tool) else {
            return Err(ValidatorError::UnknownTool {
                tool: tool.to_string(),
            });
        };
        let errors: Vec<String> = validator
            .iter_errors(input)
            .map(|e| e.to_string())
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidatorError::RevalidationFailed {
                tool: tool.to_string(),
                reason: errors.join("; "),
            })
        }
    }

    /// Number of registered tool schemas.
    #[must_use]
    pub fn len(&self) -> usize {
        self.schemas.read().map_or(0, |m| m.len())
    }

    /// `true` when no schemas have been registered yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["msg"],
            "properties": { "msg": { "type": "string" } },
            "additionalProperties": false
        })
    }

    #[test]
    fn revalidate_accepts_matching_input() {
        let cache = SchemaCache::new();
        cache.register("echo", &echo_schema()).unwrap();
        cache
            .revalidate("echo", &serde_json::json!({ "msg": "hi" }))
            .unwrap();
    }

    #[test]
    fn revalidate_rejects_extra_field() {
        let cache = SchemaCache::new();
        cache.register("echo", &echo_schema()).unwrap();
        let err = cache
            .revalidate("echo", &serde_json::json!({ "msg": "hi", "stowaway": 1 }))
            .unwrap_err();
        assert!(matches!(err, ValidatorError::RevalidationFailed { .. }));
    }

    #[test]
    fn revalidate_unknown_tool_reports_unknown_tool() {
        let cache = SchemaCache::new();
        let err = cache
            .revalidate("missing", &serde_json::json!({}))
            .unwrap_err();
        assert!(matches!(err, ValidatorError::UnknownTool { .. }));
    }

    #[test]
    fn invalid_schema_returns_invalid_schema() {
        let cache = SchemaCache::new();
        // `type` must be a string, not an integer.
        let err = cache
            .register("oops", &serde_json::json!({ "type": 7 }))
            .unwrap_err();
        assert!(matches!(err, ValidatorError::InvalidSchema { .. }));
    }
}

// Rust guideline compliant 2026-02-21
