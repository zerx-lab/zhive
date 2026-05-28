//! Initialize handshake and capability negotiation (D-007).
//!
//! Every zhive transport opens with an `initialize` request from the client
//! followed by an `initialize` response from the server and an optional
//! `initialized` notification once the client is ready. The three messages
//! pin down the protocol version, declare what optional features each side
//! implements, and exchange human-friendly identifiers.
//!
//! # Method names
//!
//! The wire method strings are bare `"initialize"` and `"initialized"` —
//! **no `v1/` or `v2/` prefix**. Source-level v1/v2 split is captured by
//! Rust modules and by the [`ProtocolVersion`] field; experimental methods
//! are gated by [`Capabilities::experimental_api`] instead of a wire
//! namespace. See `plans/phase1-core-native-research/decision-diffs.md`
//! §1.3 for the rationale.
//!
//! # Versioning policy
//!
//! [`ProtocolVersion`] is a monotonically increasing [`u16`] (ACP style),
//! not a semver string. A version bump means a breaking change to the wire
//! schema; non-breaking growth happens through new capability flags.
//!
//! # Field alignment with ACP
//!
//! `client_info` and `server_info` are **required** in zhive even though
//! ACP makes them optional, because D-007 mandates a complete handshake.
//! ACP's `auth_methods` is intentionally dropped; clients that need auth
//! negotiation can carry it through [`Capabilities::meta`] until a future
//! protocol version adds first-class support.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "schema")]
use schemars::JsonSchema;

// ============================================================
// ProtocolVersion
// ============================================================

/// Monotonically increasing wire protocol version.
///
/// Stored as a [`u16`] for cheap comparison and exact ACP alignment.
/// Bumped only for breaking changes; additive growth uses [`Capabilities`]
/// flags instead.
///
/// # Examples
///
/// ```
/// use zhive_proto::initialize::ProtocolVersion;
/// assert_eq!(ProtocolVersion::V1.0, 1);
/// assert_eq!(ProtocolVersion::LATEST, ProtocolVersion::V1);
/// assert!(ProtocolVersion::V0 < ProtocolVersion::V1);
/// ```
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(transparent)]
pub struct ProtocolVersion(pub u16);

impl ProtocolVersion {
    /// Sentinel returned when the wire value cannot be decoded; never sent.
    pub const V0: Self = Self(0);
    /// First stable protocol release (Phase 1 baseline).
    pub const V1: Self = Self(1);
    /// Latest protocol release advertised by the current build.
    pub const LATEST: Self = Self::V1;
}

// ============================================================
// Implementation identity
// ============================================================

/// Implementation identity card exchanged at handshake.
///
/// Mirrors ACP `Implementation` (and codex `ClientInfo`). Both [`name`]
/// and [`version`] are required; [`title`] is an optional human-friendly
/// label for UIs.
///
/// [`name`]: Self::name
/// [`version`]: Self::version
/// [`title`]: Self::title
///
/// # Examples
///
/// ```
/// use zhive_proto::initialize::Implementation;
/// let me: Implementation = serde_json::from_str(
///     r#"{"name":"zhive-cli","title":"Zhive CLI","version":"0.1.0"}"#,
/// )
/// .unwrap();
/// assert_eq!(me.name, "zhive-cli");
/// assert_eq!(me.version, "0.1.0");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Implementation {
    /// Machine-readable identifier (`"zhive-cli"`, `"zed"`, `"codex_vscode"`).
    pub name: String,
    /// Optional human-friendly display name; omitted from the wire when
    /// `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Implementation version (free-form, typically semver).
    pub version: String,
}

// ============================================================
// Streaming capability
// ============================================================

/// Three-queue streaming capability flags (Pi-style).
///
/// The only nested capability object. Each flag covers one of the three
/// injection queues; absent flags default to `false`. The `next_turn`
/// queue is driven by an out-of-band RPC method, so callers that only
/// negotiate `steer` and `follow_up` will not lose any features.
///
/// # Examples
///
/// ```
/// use zhive_proto::initialize::StreamingCapability;
/// let s: StreamingCapability = serde_json::from_str(r#"{"steer":true}"#).unwrap();
/// assert!(s.steer);
/// assert!(!s.follow_up);
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StreamingCapability {
    /// Implementation honours `session/enqueue_steer` mid-turn.
    #[serde(default)]
    pub steer: bool,
    /// Implementation honours `session/enqueue_follow_up` post-turn.
    #[serde(default)]
    pub follow_up: bool,
    /// Implementation honours `session/next_turn` queueing across aborts.
    #[serde(default)]
    pub next_turn: bool,
}

// ============================================================
// Capabilities
// ============================================================

/// Optional features the peer claims to implement.
///
/// Sent twice during the handshake: the client emits its set in
/// [`InitializeRequest::client_capabilities`] and the server replies with
/// its set in [`InitializeResponse::server_capabilities`]. Both sides
/// converge on the intersection.
///
/// All flags default to `false` except [`cancellation`], which defaults
/// to `true` because cancellation is treated as a baseline transport
/// feature (matching LSP and ACP conventions).
///
/// [`cancellation`]: Self::cancellation
///
/// # Examples
///
/// ```
/// use zhive_proto::initialize::Capabilities;
/// let caps = Capabilities::default();
/// assert!(caps.cancellation, "cancellation defaults to true");
/// assert!(!caps.hooks);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
#[expect(
    clippy::struct_excessive_bools,
    reason = "feature-flag carrier; each flag is independent and adding a state machine would obscure the wire shape"
)]
pub struct Capabilities {
    /// Implements Claude Code-shaped `hook/run` callbacks and
    /// `permission/request` reverse RPC.
    #[serde(default)]
    pub hooks: bool,

    /// Can host subagent fanout and `PermissionScope` inheritance.
    #[serde(default)]
    pub subagents: bool,

    /// Pi three-queue streaming flags.
    #[serde(default)]
    pub streaming: StreamingCapability,

    /// Implements `session/cancel` and respects in-flight cancellation
    /// signals. Defaults to `true` because cancellation is baseline.
    #[serde(default = "default_true")]
    pub cancellation: bool,

    /// Implements bare `permission/request` reverse RPC (distinct from
    /// the [`hooks`](Self::hooks) flag which carries the Claude Code
    /// payload shape).
    #[serde(default)]
    pub permission: bool,

    /// Implements `extension/list` and `extension/load`, including SDK
    /// Skills discovery.
    #[serde(default)]
    pub extension: bool,

    /// Permits methods marked `#[experimental]` in the source. Field name
    /// matches codex so a bridge does not have to rename the flag.
    #[serde(default)]
    pub experimental_api: bool,

    /// Optional list of notification methods the peer wants the other
    /// side to suppress (codex bandwidth/noise control).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_out_notification_methods: Option<Vec<String>>,

    /// Free-form extension channel; aligns with ACP `_meta`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            hooks: false,
            subagents: false,
            streaming: StreamingCapability::default(),
            cancellation: true,
            permission: false,
            extension: false,
            experimental_api: false,
            opt_out_notification_methods: None,
            meta: None,
        }
    }
}

/// Serde helper: returns `true` for the default value of
/// [`Capabilities::cancellation`].
const fn default_true() -> bool {
    true
}

// ============================================================
// Initialize request / response
// ============================================================

/// Payload of the `initialize` request sent by the client.
///
/// # Examples
///
/// ```
/// use zhive_proto::initialize::{InitializeRequest, ProtocolVersion};
/// let req: InitializeRequest = serde_json::from_str(
///     r#"{
///         "protocolVersion": 1,
///         "clientInfo": {"name": "zhive-cli", "version": "0.1.0"}
///     }"#,
/// )
/// .unwrap();
/// assert_eq!(req.protocol_version, ProtocolVersion::V1);
/// assert_eq!(req.client_info.name, "zhive-cli");
/// ```
///
/// # Errors
///
/// The server may reply with these JSON-RPC error codes:
///
/// * `-32001 ProtocolVersionUnsupported` — the request asked for a higher
///   version than the server supports and no fallback was negotiated.
/// * `-32002 CapabilityRequired` — the client demanded a feature the
///   server does not implement.
/// * `-32600 InvalidRequest` — the payload failed schema validation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct InitializeRequest {
    /// Highest wire version the client supports.
    pub protocol_version: ProtocolVersion,

    /// Optional feature flags the client claims to implement; defaults to
    /// an empty capability set when omitted.
    #[serde(default)]
    pub client_capabilities: Capabilities,

    /// Client identity card. Required by D-007.
    pub client_info: Implementation,

    /// Free-form extension channel; aligns with ACP `_meta`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Payload of the `initialize` response returned by the server.
///
/// `protocol_version` is the version the server selected (always `≤` the
/// client's requested version). Any optional features the client asked
/// for but the server cannot honour will be absent from
/// [`server_capabilities`].
///
/// [`server_capabilities`]: Self::server_capabilities
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct InitializeResponse {
    /// Wire version chosen by the server, never above the request.
    pub protocol_version: ProtocolVersion,

    /// Optional feature flags the server implements; defaults to an empty
    /// capability set when omitted.
    #[serde(default)]
    pub server_capabilities: Capabilities,

    /// Server identity card. Required by D-007.
    pub server_info: Implementation,

    /// Free-form extension channel; aligns with ACP `_meta`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

// ============================================================
// Initialized notification
// ============================================================

/// Payload of the `initialized` notification (client to server).
///
/// Optional but recommended; signals that the client has fully processed
/// the `initialize` response and is ready to accept requests. Aligns
/// with codex `ClientNotification::Initialized` and LSP's eponymous
/// notification. ACP has no equivalent and silently ignores it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Initialized {
    /// Free-form extension channel; aligns with ACP `_meta`.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_default_has_cancellation_true() {
        let c = Capabilities::default();
        assert!(c.cancellation);
        assert!(!c.hooks);
        assert!(!c.subagents);
        assert!(!c.experimental_api);
    }

    #[test]
    fn capabilities_round_trip_via_json() {
        let c = Capabilities {
            hooks: true,
            streaming: StreamingCapability {
                steer: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: Capabilities = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn capabilities_empty_object_deserialises_to_default() {
        let c: Capabilities = serde_json::from_str("{}").unwrap();
        assert!(c.cancellation, "cancellation defaults to true");
        assert!(!c.hooks);
    }

    #[test]
    fn initialize_request_serialises_camel_case() {
        let req = InitializeRequest {
            protocol_version: ProtocolVersion::V1,
            client_capabilities: Capabilities::default(),
            client_info: Implementation {
                name: "zhive-cli".into(),
                title: None,
                version: "0.1.0".into(),
            },
            meta: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("clientInfo").is_some());
        assert!(v.get("client_info").is_none());
        assert_eq!(v["protocolVersion"], 1);
    }

    #[test]
    fn protocol_version_ordering() {
        assert!(ProtocolVersion::V0 < ProtocolVersion::V1);
        assert_eq!(ProtocolVersion::LATEST, ProtocolVersion::V1);
    }

    #[test]
    fn initialized_round_trip() {
        let n = Initialized::default();
        let s = serde_json::to_string(&n).unwrap();
        assert_eq!(s, "{}");
        let back: Initialized = serde_json::from_str(&s).unwrap();
        assert_eq!(n, back);
    }
}

// Rust guideline compliant 2026-02-21
