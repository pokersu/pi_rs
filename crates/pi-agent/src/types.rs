//! Rust 翻译自 packages/agent/src/types.ts
//!
//! agent 运行时核心类型。

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pi_ai::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, Message, Model, SimpleStreamOptions,
    StreamFunction, TextOrImageContent, Tool, ToolCall, ToolResultMessage, Usage,
};

/// 对应 `StreamFn`。与 `pi_ai::StreamFunction` 同形（agent 复用 provider 的统一流契约）。
pub type StreamFn = StreamFunction;

/// 对应 `ToolExecutionMode = "sequential" | "parallel"`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    Parallel,
}

/// 对应 `QueueMode = "all" | "one-at-a-time"`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    All,
    OneAtATime,
}

/// 对应 `AgentToolCall`（即 assistant 消息中的 toolCall 块）。
pub type AgentToolCall = ToolCall;

/// 对应 `ThinkingLevel`（与 `pi_ai::ModelThinkingLevel` 相同）。
pub type ThinkingLevel = pi_ai::ModelThinkingLevel;

/// 对应 `BashExecutionMessage`
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
    pub timestamp: u64,
    pub exclude_from_context: bool,
}

/// 对应 `CustomMessage.content: string | (TextContent | ImageContent)[]`
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum CustomMessageContent {
    Text(String),
    Blocks(Vec<TextOrImageContent>),
}

/// 对应 `CustomMessage`
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub custom_type: String,
    pub content: CustomMessageContent,
    pub display: bool,
    pub details: Option<serde_json::Value>,
    pub timestamp: u64,
}

/// 对应 `BranchSummaryMessage`
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryMessage {
    pub summary: String,
    pub from_id: String,
    pub timestamp: u64,
}

/// 对应 `CompactionSummaryMessage`
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryMessage {
    pub summary: String,
    pub tokens_before: u64,
    pub timestamp: u64,
}

/// 对应 `AgentMessage = Message | CustomAgentMessages`。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum AgentMessage {
    User(pi_ai::UserMessage),
    Assistant(pi_ai::AssistantMessage),
    ToolResult(pi_ai::ToolResultMessage),
    BashExecution(BashExecutionMessage),
    Custom(CustomMessage),
    BranchSummary(BranchSummaryMessage),
    CompactionSummary(CompactionSummaryMessage),
}

impl AgentMessage {
    pub fn role(&self) -> &'static str {
        match self {
            AgentMessage::User(_) => "user",
            AgentMessage::Assistant(_) => "assistant",
            AgentMessage::ToolResult(_) => "toolResult",
            AgentMessage::BashExecution(_) => "bashExecution",
            AgentMessage::Custom(_) => "custom",
            AgentMessage::BranchSummary(_) => "branchSummary",
            AgentMessage::CompactionSummary(_) => "compactionSummary",
        }
    }
}

/// 把 agent 的 thinking level 转成 ai 的 `ThinkingLevel`（`off` 映射为 `None`）。
pub fn to_ai_thinking_level(level: ThinkingLevel) -> Option<pi_ai::ThinkingLevel> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some(pi_ai::ThinkingLevel::Minimal),
        ThinkingLevel::Low => Some(pi_ai::ThinkingLevel::Low),
        ThinkingLevel::Medium => Some(pi_ai::ThinkingLevel::Medium),
        ThinkingLevel::High => Some(pi_ai::ThinkingLevel::High),
        ThinkingLevel::Xhigh => Some(pi_ai::ThinkingLevel::Xhigh),
        ThinkingLevel::Max => Some(pi_ai::ThinkingLevel::Max),
    }
}

/// 对应 `BeforeToolCallResult`
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

/// 对应 `AfterToolCallResult`
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<TextOrImageContent>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}

/// 对应 `AgentToolResult<T>`（`details` 用 JSON 值表示）。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentToolResult {
    pub content: Vec<TextOrImageContent>,
    pub details: serde_json::Value,
    pub usage: Option<Usage>,
    pub added_tool_names: Option<Vec<String>>,
    pub terminate: bool,
}

/// 对应 `AgentToolUpdateCallback<T>`
pub type AgentToolUpdateCallback = Box<dyn Fn(AgentToolResult) + Send>;

/// 对应 `AgentTool.execute` 的签名（参数用 JSON 值表示）。
pub type AgentToolExecuteFn = Arc<
    dyn Fn(
            String,
            serde_json::Value,
            Option<AbortSignal>,
            Option<AgentToolUpdateCallback>,
        ) -> Pin<Box<dyn Future<Output = AgentToolResult> + Send>>
        + Send
        + Sync,
>;

/// 对应 `AgentTool<TParameters, TDetails>`。
/// TS 中 `parameters` 为 TypeBox schema 并据此推断参数类型；Rust 中参数为 JSON 值。
#[derive(Clone)]
pub struct AgentTool {
    /// 对应 `label`
    pub label: String,
    /// 对应继承自 `Tool` 的字段（name/description/parameters/constrainedSampling）。
    pub tool: Tool,
    /// 对应 `execute`
    pub execute: AgentToolExecuteFn,
    /// 对应 `executionMode`
    pub execution_mode: Option<ToolExecutionMode>,
}

impl AgentTool {
    pub fn name(&self) -> &str {
        &self.tool.name
    }
}

impl std::fmt::Debug for AgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentTool")
            .field("label", &self.label)
            .field("name", &self.tool.name)
            .field("execution_mode", &self.execution_mode)
            .finish()
    }
}

/// 对应 `AgentContext`（传入底层循环的上下文快照）。
#[derive(Debug, Clone)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Option<Vec<AgentTool>>,
}

/// 对应 `ShouldStopAfterTurnContext` / `PrepareNextTurnContext`
#[derive(Debug, Clone)]
pub struct ShouldStopAfterTurnContext {
    pub message: AssistantMessage,
    pub tool_results: Vec<ToolResultMessage>,
    pub context: AgentContext,
    pub new_messages: Vec<AgentMessage>,
}

/// 对应 `PrepareNextTurnContext`
pub type PrepareNextTurnContext = ShouldStopAfterTurnContext;

/// 对应 `AgentLoopTurnUpdate`
#[derive(Debug, Clone)]
pub struct AgentLoopTurnUpdate {
    pub context: Option<AgentContext>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

/// 对应 `BeforeToolCallContext`
#[derive(Debug, Clone)]
pub struct BeforeToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    pub args: serde_json::Value,
    pub context: AgentContext,
}

/// 对应 `AfterToolCallContext`
#[derive(Debug, Clone)]
pub struct AfterToolCallContext {
    pub assistant_message: AssistantMessage,
    pub tool_call: AgentToolCall,
    pub args: serde_json::Value,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub context: AgentContext,
}

/// 对应 `AgentState`（公开 agent 状态）。
#[derive(Debug, Clone)]
pub struct AgentState {
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<AgentTool>,
    pub messages: Vec<AgentMessage>,
    pub is_streaming: bool,
    pub streaming_message: Option<AgentMessage>,
    pub pending_tool_calls: BTreeSet<String>,
    pub error_message: Option<String>,
}

/// 对应 `AgentEvent`
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    // Agent lifecycle
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    // Turn lifecycle
    TurnStart,
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    // Message lifecycle
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AgentMessage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    // Tool execution lifecycle
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        partial_result: serde_json::Value,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: serde_json::Value,
        is_error: bool,
    },
}

// --- 回调类型 ---

/// 对应 `convertToLlm`：把 `AgentMessage[]` 转成 LLM 可理解的 `Message[]`。
pub type ConvertToLlmFn = Arc<dyn Fn(Vec<AgentMessage>) -> Vec<Message> + Send + Sync>;

/// 对应 `transformContext`
pub type TransformContextFn = Arc<
    dyn Fn(
            Vec<AgentMessage>,
            Option<AbortSignal>,
        ) -> Pin<Box<dyn Future<Output = Vec<AgentMessage>> + Send>>
        + Send
        + Sync,
>;

/// 对应 `getApiKey`（`(provider) => string | undefined`，可能异步）。
pub type GetApiKeyFn =
    Arc<dyn Fn(&str) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync>;

/// 对应 `shouldStopAfterTurn`
pub type ShouldStopAfterTurnFn = Arc<
    dyn Fn(&ShouldStopAfterTurnContext) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync,
>;

/// 对应 `prepareNextTurn`
pub type PrepareNextTurnFn = Arc<
    dyn Fn(
            &PrepareNextTurnContext,
        ) -> Pin<Box<dyn Future<Output = Option<AgentLoopTurnUpdate>> + Send>>
        + Send
        + Sync,
>;

/// 对应 `getSteeringMessages` / `getFollowUpMessages`
pub type GetMessagesFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<AgentMessage>> + Send>> + Send + Sync>;

/// 对应 `beforeToolCall`
pub type BeforeToolCallFn = Arc<
    dyn Fn(
            &BeforeToolCallContext,
            Option<AbortSignal>,
        ) -> Pin<Box<dyn Future<Output = Option<BeforeToolCallResult>> + Send>>
        + Send
        + Sync,
>;

/// 对应 `afterToolCall`
pub type AfterToolCallFn = Arc<
    dyn Fn(
            &AfterToolCallContext,
            Option<AbortSignal>,
        ) -> Pin<Box<dyn Future<Output = Option<AfterToolCallResult>> + Send>>
        + Send
        + Sync,
>;

/// 对应 `AgentLoopConfig`
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    /// 对应 TS 中 `AgentLoopConfig extends SimpleStreamOptions` 的流选项部分。
    pub stream: SimpleStreamOptions,
    pub convert_to_llm: ConvertToLlmFn,
    pub transform_context: Option<TransformContextFn>,
    pub get_api_key: Option<GetApiKeyFn>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    pub get_steering_messages: Option<GetMessagesFn>,
    pub get_follow_up_messages: Option<GetMessagesFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub tool_execution: ToolExecutionMode,
}
