//! `cargo xtask schema` — emit JSON Schema for every public proto wire type.
//!
//! Writes one `<TypeName>.json` file per exported proto type into
//! `proto/schema/` (relative to the workspace root).  The directory is
//! created if it does not already exist.
//!
//! # Usage
//!
//! ```text
//! cargo xtask schema
//! ```
//!
//! All output paths are printed to stderr.

use anyhow::Context as _;
use schemars::schema_for;
use std::path::PathBuf;

/// Workspace-relative output directory for generated schema files.
const SCHEMA_DIR: &str = "proto/schema";

/// Emits JSON Schema files for every public wire type in `zhive-proto`.
///
/// # Errors
///
/// Returns an error if the output directory cannot be created or any file
/// cannot be written.
pub fn run() -> anyhow::Result<()> {
    // Resolve the output directory relative to the workspace root.
    // CARGO_MANIFEST_DIR points at xtask/; walk one level up to the root.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .context("xtask manifest has no parent directory")?;
    let out_dir = workspace_root.join(SCHEMA_DIR);

    std::fs::create_dir_all(&out_dir).with_context(|| format!("create {}", out_dir.display()))?;

    macro_rules! emit {
        ($ty:ty) => {{
            let schema = schema_for!($ty);
            // Use only the final path segment as the filename, dropping any
            // crate-path prefix (e.g. "StartTurnParams", not "rpc::StartTurnParams").
            let name = stringify!($ty)
                .rsplit("::")
                .next()
                .unwrap_or(stringify!($ty));
            let path = out_dir.join(format!("{name}.json"));
            let json = serde_json::to_string_pretty(&schema)
                .with_context(|| format!("serialize schema for {name}"))?;
            std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
            eprintln!("  wrote {}", path.display());
        }};
    }

    eprintln!("cargo xtask schema: writing to {}", out_dir.display());

    // ------------------------------------------------------------------ //
    // Envelope and framing
    // ------------------------------------------------------------------ //
    emit!(zhive_proto::Message);

    // ------------------------------------------------------------------ //
    // Initialize handshake
    // ------------------------------------------------------------------ //
    emit!(zhive_proto::initialize::InitializeRequest);
    emit!(zhive_proto::initialize::InitializeResponse);
    emit!(zhive_proto::initialize::Initialized);

    // ------------------------------------------------------------------ //
    // Domain primitives
    // ------------------------------------------------------------------ //
    emit!(zhive_proto::domain::Thread);
    emit!(zhive_proto::domain::Item);
    emit!(zhive_proto::domain::TurnStartedNotification);
    emit!(zhive_proto::domain::TurnCompletedNotification);

    // ------------------------------------------------------------------ //
    // Permission
    // ------------------------------------------------------------------ //
    emit!(zhive_proto::permission::RequestPermissionRequest);
    emit!(zhive_proto::permission::PermissionOutcome);
    emit!(zhive_proto::permission::ResumePermissionParams);
    emit!(zhive_proto::permission::TurnSuspendedNotification);
    emit!(zhive_proto::permission::TurnResumedNotification);
    emit!(zhive_proto::permission::SessionAbortedNotification);
    emit!(zhive_proto::permission::SubagentDefinition);
    emit!(zhive_proto::permission::HookOutput);

    // ------------------------------------------------------------------ //
    // Hook
    // ------------------------------------------------------------------ //
    emit!(zhive_proto::hook::HookEvent);

    // ------------------------------------------------------------------ //
    // A1 RPC Params / Result types
    // ------------------------------------------------------------------ //
    emit!(zhive_proto::rpc::StartTurnParams);
    emit!(zhive_proto::rpc::StartTurnResult);
    emit!(zhive_proto::rpc::CancelTurnParams);
    emit!(zhive_proto::rpc::CancelTurnResult);
    emit!(zhive_proto::rpc::ResumePermissionResult);
    emit!(zhive_proto::rpc::CompactParams);
    emit!(zhive_proto::rpc::CompactResult);
    emit!(zhive_proto::rpc::ForkParams);
    emit!(zhive_proto::rpc::ForkResult);
    emit!(zhive_proto::rpc::ListThreadsParams);
    emit!(zhive_proto::rpc::ListThreadsResult);
    emit!(zhive_proto::rpc::ResumeThreadParams);
    emit!(zhive_proto::rpc::ResumeThreadResult);
    emit!(zhive_proto::rpc::GetItemsParams);
    emit!(zhive_proto::rpc::GetItemsResult);
    emit!(zhive_proto::rpc::InjectionParams);
    emit!(zhive_proto::rpc::InjectionAck);
    emit!(zhive_proto::rpc::SessionCancelParams);

    // ------------------------------------------------------------------ //
    // A2 event payload types
    // ------------------------------------------------------------------ //
    emit!(zhive_proto::events::UsagePayload);
    emit!(zhive_proto::events::TurnStartedPayload);
    emit!(zhive_proto::events::TurnRejectedPayload);
    emit!(zhive_proto::events::TurnCompletedPayload);
    emit!(zhive_proto::events::TurnFailedPayload);
    emit!(zhive_proto::events::ItemAppendedPayload);
    emit!(zhive_proto::events::ItemDeltaPayload);
    emit!(zhive_proto::events::PhaseChangedPayload);
    emit!(zhive_proto::events::PermissionRequestedPayload);
    emit!(zhive_proto::events::SubagentStartedPayload);
    emit!(zhive_proto::events::SubagentCompletedPayload);
    emit!(zhive_proto::events::ThreadForkedPayload);

    eprintln!("cargo xtask schema: done.");
    Ok(())
}

// Rust guideline compliant 2026-02-21
