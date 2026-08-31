//! Rust 翻译自 packages/agent/src/index.ts（核心 + harness 基础）

pub mod agent;
#[path = "agent-loop.rs"]
pub mod agent_loop;
pub mod harness;
pub mod node;
#[path = "proxy.rs"]
pub mod proxy;
pub mod search;
#[path = "stream-fn.rs"]
pub mod stream_fn;
pub mod types;

pub use agent::{Agent, AgentOptions};
pub use agent_loop::{agent_loop, agent_loop_continue, run_agent_loop, run_agent_loop_continue};
pub use stream_fn::{get_default_stream_fn, set_default_stream_fn};
pub use types::*;
