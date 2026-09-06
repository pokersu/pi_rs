//! Rust 翻译自 packages/agent/src/harness/execution/（目录）

#[path = "effect-gate.rs"]
pub mod effect_gate;

pub use effect_gate::{AbortRequested, Gate, GateControl, create_gate};
