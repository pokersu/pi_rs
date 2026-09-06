//! Rust 翻译自 packages/agent/src/harness/execution/（目录）

pub mod assistant;
#[path = "effect-gate.rs"]
pub mod effect_gate;
pub mod tools;

pub use assistant::{
    AssistantResponseMetadata, AssistantStreamObserver, HarnessAssistantStreamConfig,
    HarnessRequestContext, consume_assistant_stream, stream_harness_assistant,
};
pub use effect_gate::{AbortRequested, Cancellation, Gate, GateControl, GateError, create_gate};
pub use tools::{
    AfterToolPatch, BeforeToolBlock, BeforeToolDecision, ClearedOrImmediate, ClearedToolCall,
    ExecutedToolCall, FinalizedToolCall, ImmediateToolOutcome, PreparedOrImmediate,
    PreparedToolCall, apply_before_tool_decision, create_tool_result_message, execute_tool_call,
    finalize_tool_call, prepare_tool_call, tool_result_from_message,
};
