//! Rust 翻译自 packages/agent/src/index.ts（`@earendil-works/pi-agent-core`，v1.1.0）。
//!
//! 上游 v1.0.0 已将 harness（sessions/storage/tools/compaction/…）从本包移除，
//! 只保留 `Agent`、agent loop、proxy stream 及类型；那些能力现在位于 `pi-durable`。

pub mod agent;
#[path = "agent-loop.rs"]
pub mod agent_loop;
#[path = "proxy.rs"]
pub mod proxy;
#[path = "stream-fn.rs"]
pub mod stream_fn;
pub mod types;

pub use agent::{Agent, AgentOptions, default_convert_to_llm};
pub use agent_loop::{
    AgentToolCallOutcome, RunToolCallOptions, agent_loop, agent_loop_continue, run_agent_loop,
    run_agent_loop_continue, run_tool_call,
};
pub use stream_fn::set_default_stream_fn;
pub use types::*;
