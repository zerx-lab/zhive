//! MCP (Model Context Protocol) client for zhive.
//!
//! Connects to configured MCP servers — stdio child processes and
//! Streamable-HTTP endpoints, via the [`rmcp`] SDK — discovers their tools,
//! and adapts each into a [`zhive_core::tools::Tool`] so the engine dispatches
//! it through its normal hook → permission → execute pipeline.
//!
//! # Overview
//!
//! 1. Map your front end's configuration into [`McpServerConfig`] values (one
//!    per server), each naming a transport ([`McpTransport`]).
//! 2. Call [`McpManager::connect_all`] at boot. It connects to every server in
//!    parallel and skips (with a warning) any that fail to start.
//! 3. Register [`McpManager::tools`] into the engine's tool registry. Each tool
//!    is named `mcp__<server>__<tool>`.
//! 4. Keep the [`McpManager`] alive for the engine's lifetime, then call
//!    [`McpManager::shutdown`] on teardown.
//!
//! # Examples
//!
//! ```no_run
//! # async fn run() {
//! use zhive_mcp::{McpManager, McpServerConfig, McpTransport, McpConnectOptions};
//!
//! let servers = vec![McpServerConfig {
//!     name: "filesystem".to_owned(),
//!     transport: McpTransport::Stdio {
//!         command: "npx".to_owned(),
//!         args: vec!["-y".to_owned(), "@modelcontextprotocol/server-filesystem".to_owned()],
//!         env: Default::default(),
//!         cwd: None,
//!     },
//! }];
//!
//! let manager = McpManager::connect_all(servers, McpConnectOptions::default()).await;
//! for tool in manager.tools() {
//!     println!("discovered {}", tool.name());
//! }
//! manager.shutdown().await;
//! # }
//! ```

pub mod convert;

mod config;
mod error;
mod manager;
mod tool;

#[cfg(test)]
mod loopback_test;

#[doc(inline)]
pub use config::{McpConnectOptions, McpServerConfig, McpTransport};
#[doc(inline)]
pub use error::McpError;
#[doc(inline)]
pub use manager::{DiscoveredPrompt, DiscoveredResource, McpManager};
#[doc(inline)]
pub use tool::McpTool;

// Rust guideline compliant 2026-02-21
