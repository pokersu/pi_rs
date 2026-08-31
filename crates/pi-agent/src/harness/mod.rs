//! Rust 翻译自 packages/agent/src/harness/（目录）

#[path = "agent-harness.rs"]
pub mod agent_harness;
pub mod compaction;
pub mod env;
pub mod events;
pub mod messages;
#[path = "prompt-templates.rs"]
pub mod prompt_templates;
pub mod reducer;
pub mod result;
pub mod session;
pub mod skills;
#[path = "system-prompt.rs"]
pub mod system_prompt;
pub mod telemetry;
pub mod tools;
pub mod types;
pub mod utils;

pub use env::NodeExecutionEnv;
pub use types::*;
