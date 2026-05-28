//! JSON-RPC 2.0 wire schema and length-delimited framing for zhive.
//!
//! This crate is the single source of truth for the protocol bytes that
//! cross every zhive process boundary (TUI -> core, bridge-stdio -> core,
//! future Web UI -> remote). It contains:
//!
//! * [`Message`] / [`Request`] / [`Response`] / [`Notification`] -- the
//!   JSON-RPC 2.0 envelope types ([spec][jsonrpc-spec]).
//! * [`framing`] -- LSP-style `Content-Length:` length-delimited codec over
//!   any [`tokio::io::AsyncRead`] + [`tokio::io::AsyncWrite`] pair.
//!
//! Why hand-rolled framing? `jsonrpsee` does not support stdio transports
//! ([paritytech/jsonrpsee#5][jsonrpsee-5]) and D-004 mandates stdio + UDS in
//! Phase 1. The LSP framing is < 200 lines, has 15+ years of production
//! validation across editors, and is what ACP / MCP already speak.
//!
//! All public types are `serde` (de)serializable; with the `schema`
//! feature on, they also derive [`schemars::JsonSchema`] so downstream
//! tooling can emit JSON Schema for editor / browser clients.
//!
//! [jsonrpc-spec]: https://www.jsonrpc.org/specification
//! [jsonrpsee-5]: https://github.com/paritytech/jsonrpsee/issues/5

#![forbid(unsafe_code)]

pub mod domain;
pub mod framing;
pub mod hook;
pub mod initialize;
pub mod manifest;
pub mod permission;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Top-level JSON-RPC 2.0 envelope.
///
/// Either side of a session may send any of the three variants; the
/// `client` and `server` labels are application-level roles and impose no
/// wire-layer restriction (this is how LSP `$/`reverse requests and ACP
/// `permission/request` work, see D-008).
///
/// # Examples
/// ```
/// use zhive_proto::{Message, Notification};
/// let n = Notification::new("session/cancel", None);
/// let bytes = serde_json::to_vec(&Message::Notification(n)).unwrap();
/// assert!(bytes.starts_with(b"{"));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Message {
    /// Carries a method invocation expecting a paired [`Response`].
    Request(Request),
    /// Carries the outcome of a previously sent [`Request`].
    Response(Response),
    /// Fire-and-forget event, no response is sent or expected.
    Notification(Notification),
}

/// JSON-RPC request: method + params + correlatable id.
///
/// `id` is required by spec; use [`Notification`] instead for fire-and-forget
/// calls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Request {
    /// Always the literal string `"2.0"`. Enforced on deserialize.
    pub jsonrpc: Version,
    /// Correlation id; the [`Response`] echoes the same value.
    pub id: Id,
    /// Method name, e.g. `"session/prompt"`.
    pub method: String,
    /// Method parameters; serde omits the field when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC response: either `result` or `error`, never both.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: Version,
    /// Correlation id echoed from the matching [`Request`].
    pub id: Id,
    /// Either a successful `result` payload, or a typed `error` object.
    #[serde(flatten)]
    pub outcome: ResponseOutcome,
}

/// Mutually exclusive `result` / `error` content of a [`Response`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome {
    /// Successful outcome with a free-form JSON payload.
    Result(Value),
    /// Failure outcome carrying a structured error object.
    Error(ErrorObject),
}

/// JSON-RPC notification: method + params, no id, no response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Notification {
    /// Always `"2.0"`.
    pub jsonrpc: Version,
    /// Method name, e.g. `"session/update"`.
    pub method: String,
    /// Method parameters; serde omits the field when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    /// Builds a notification with the `2.0` protocol stamp pre-filled.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: Version,
            method: method.into(),
            params,
        }
    }
}

impl Request {
    /// Builds a request with the `2.0` protocol stamp pre-filled.
    #[must_use]
    pub fn new(id: Id, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: Version,
            id,
            method: method.into(),
            params,
        }
    }
}

impl Response {
    /// Builds a successful response carrying `result`.
    #[must_use]
    pub fn ok(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: Version,
            id,
            outcome: ResponseOutcome::Result(result),
        }
    }

    /// Builds an error response carrying `error`.
    #[must_use]
    pub fn err(id: Id, error: ErrorObject) -> Self {
        Self {
            jsonrpc: Version,
            id,
            outcome: ResponseOutcome::Error(error),
        }
    }
}

/// JSON-RPC `id` field; numbers, strings and `null` are all valid per spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Id {
    /// 64-bit signed integer; covers all real-world client counters.
    Number(i64),
    /// Opaque string; used by clients that prefer UUIDs.
    String(String),
    /// JSON `null`; permitted by spec but discouraged.
    Null,
}

/// Structured error payload (JSON-RPC 2.0 spec [section 5.1][err]).
///
/// [err]: https://www.jsonrpc.org/specification#error_object
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ErrorObject {
    /// Numeric error code; -32700 to -32600 are reserved by spec.
    pub code: i64,
    /// Short human-readable error string.
    pub message: String,
    /// Optional free-form diagnostic payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Phantom marker for the `"jsonrpc": "2.0"` field.
///
/// Serialises to `"2.0"` and rejects any other value on deserialise; this
/// makes wire-protocol drift detectable at the type system level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <&str>::deserialize(d)?;
        if raw == "2.0" {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom(format!(
                "expected jsonrpc=\"2.0\", got {raw:?}"
            )))
        }
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Version {
    fn schema_name() -> String {
        "JsonRpcVersion".to_string()
    }
    fn json_schema(_generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        serde_json::from_value(serde_json::json!({
            "type": "string",
            "const": "2.0"
        }))
        .expect("static schema is well-formed")
    }
}

// Rust guideline compliant 2026-02-21
