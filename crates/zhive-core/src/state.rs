//! Engine-resident state: threads, active turns, and the tail of items
//! retained in memory before the rollout loader takes over.
//!
//! Phase 1 keeps state in process memory. B2 layers the lazy
//! `from_jsonl` loader on top; B3 wires the `SQLite` indices in.

pub mod thread;

#[doc(inline)]
pub use thread::{ActiveTurn, ThreadHandle, ThreadStore};

// Rust guideline compliant 2026-02-21
