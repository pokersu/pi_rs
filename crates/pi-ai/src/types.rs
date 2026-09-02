//! Rust 翻译自 packages/ai/src/types.ts（核心类型层）
//!
//! 只翻译 agent 运行时依赖的类型，以及 openai/deepseek provider 所需的兼容配置。
//! 其余 40+ provider 的专有类型（Anthropic/Bedrock/OpenRouter/Vercel 等）后续按需补充。

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::utils::event_stream::AssistantMessageEventStream;

/// 对应 `Api = KnownApi | (string & {})`。Rust 中用字符串表示。
pub type Api = String;

/// 对应 `ProviderId`。
pub type ProviderId = String;

/// 对应 `ProviderEnv = Record<string, string>`
pub type ProviderEnv = BTreeMap<String, String>;

/// 对应 `ProviderHeaders = Record<string, string | null>`
pub type ProviderHeaders = BTreeMap<String, Option<String>>;

/// 对应 `ImagesApi` / `ImagesProviderId`。
pub type ImagesApi = String;
pub type ImagesProviderId = String;

/// 对应 `ToolChoice = "auto" | "none"`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    Auto,
    None,
}

/// 对应 `ThinkingLevel`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// 对应 `ModelThinkingLevel = "off" | ThinkingLevel`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// 对应 `ThinkingLevelMap = Partial<Record<ModelThinkingLevel, string | null>>`
pub type ThinkingLevelMap = BTreeMap<ModelThinkingLevel, Option<String>>;

/// 对应 `ThinkingBudgets`（token-based providers only）
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgets {
    pub minimal: Option<u64>,
    pub low: Option<u64>,
    pub medium: Option<u64>,
    pub high: Option<u64>,
}

/// 对应 `CacheRetention = "none" | "short" | "long"`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

/// 对应 `Transport = "sse" | "websocket" | "websocket-cached" | "auto"`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}

/// 对应 `AbortSignal`。基于 `CancellationToken` 的轻量封装。
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct AbortSignal {
    token: CancellationToken,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    /// 对应 `signal.aborted`
    pub fn aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    /// 对应 `signal.throwIfAborted()`
    pub fn throw_if_aborted(&self) -> Result<(), AbortError> {
        if self.aborted() {
            Err(AbortError)
        } else {
            Ok(())
        }
    }

    /// 对应触发 abort。
    pub fn abort(&self) {
        self.token.cancel();
    }

    /// 等待 abort（返回 `Future`）。
    pub fn cancelled(&self) -> impl Future<Output = ()> {
        self.token.cancelled()
    }

    /// 访问底层 token（供 provider 适配层使用）。
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// 对应 `AbortSignal.timeout(ms)`：创建在 `duration` 后自动 abort 的 signal。
    pub fn timeout(duration: std::time::Duration) -> Self {
        let signal = Self::new();
        let token = signal.token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            token.cancel();
        });
        signal
    }

    /// 对应 `AbortSignal.any(signals)`：任一子 signal abort 时本 signal 也 abort。
    pub fn any(signals: &[AbortSignal]) -> Self {
        let signal = Self::new();
        for child in signals {
            let child_token = child.token.clone();
            let token = signal.token.clone();
            tokio::spawn(async move {
                child_token.cancelled().await;
                token.cancel();
            });
        }
        signal
    }
}

impl std::fmt::Debug for AbortSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbortSignal")
            .field("aborted", &self.aborted())
            .finish()
    }
}

/// 对应 `throwIfAborted()` 抛出的错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbortError;
impl std::fmt::Display for AbortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation aborted")
    }
}

impl std::error::Error for AbortError {}

/// 对应 `TextContent`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    #[serde(rename = "type")]
    pub kind: TextKind,
    pub text: String,
    pub text_signature: Option<String>,
}

/// 对应 `TextContent.type: "text"` 的字面量（序列化为字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextKind;

impl Serialize for TextKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("text")
    }
}

impl<'de> Deserialize<'de> for TextKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == "text" {
            Ok(TextKind)
        } else {
            Err(serde::de::Error::custom("expected \"text\""))
        }
    }
}

/// 对应 `ThinkingContent`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    #[serde(rename = "type")]
    pub kind: ThinkingKind,
    pub thinking: String,
    pub thinking_signature: Option<String>,
    pub redacted: Option<bool>,
}

/// 对应 `ThinkingContent.type: "thinking"` 的字面量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThinkingKind;

impl Serialize for ThinkingKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("thinking")
    }
}

impl<'de> Deserialize<'de> for ThinkingKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == "thinking" {
            Ok(ThinkingKind)
        } else {
            Err(serde::de::Error::custom("expected \"thinking\""))
        }
    }
}

/// 对应 `ImageContent`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    #[serde(rename = "type")]
    pub kind: ImageKind,
    /// base64 编码的图像数据。
    pub data: String,
    /// 例如 "image/jpeg", "image/png"
    pub mime_type: String,
}

/// 对应 `ImageContent.type: "image"` 的字面量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageKind;

impl Serialize for ImageKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("image")
    }
}

impl<'de> Deserialize<'de> for ImageKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == "image" {
            Ok(ImageKind)
        } else {
            Err(serde::de::Error::custom("expected \"image\""))
        }
    }
}

/// 对应 `ToolCall`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    #[serde(rename = "type")]
    pub kind: ToolCallKind,
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub thought_signature: Option<String>,
    pub namespace: Option<String>,
}

/// 对应 `ToolCall.type: "toolCall"` 的字面量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolCallKind;

impl Serialize for ToolCallKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("toolCall")
    }
}

impl<'de> Deserialize<'de> for ToolCallKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s == "toolCall" {
            Ok(ToolCallKind)
        } else {
            Err(serde::de::Error::custom("expected \"toolCall\""))
        }
    }
}

/// 对应消息内容块（`TextContent | ThinkingContent | ToolCall`），图片块单独处理。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text(TextContent),
    Thinking(ThinkingContent),
    Image(ImageContent),
    ToolCall(ToolCall),
}

/// 对应 `Usage`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 仅 Anthropic 报告此拆分（`cacheWrite` 中 1h 保留的部分）。
    pub cache_write_1h: Option<u64>,
    /// reasoning/thinking tokens（是 `output` 的子集）。
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

/// 对应 `Usage.cost`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

/// 对应 `StopReason`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

/// 对应 `DeferredHandle`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: String,
    pub id: String,
    pub expires_at: Option<u64>,
    pub poll_after_ms: Option<u64>,
    pub data: Option<serde_json::Value>,
}

/// 对应 `UserMessage`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: UserContent,
    pub timestamp: u64,
}

/// 对应 `UserMessage.content: string | (TextContent | ImageContent)[]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<TextOrImageContent>),
}

/// 对应 `TextContent | ImageContent`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TextOrImageContent {
    Text(TextContent),
    Image(ImageContent),
}

/// 对应 `AssistantMessage`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: Api,
    pub provider: ProviderId,
    pub model: String,
    pub response_model: Option<String>,
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub deferred: Option<DeferredHandle>,
    pub error_message: Option<String>,
    pub raw_stop_reason: Option<String>,
    pub end_turn: Option<bool>,
    pub timestamp: u64,
}

/// 对应 `ToolResultMessage`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<TextOrImageContent>,
    pub details: Option<serde_json::Value>,
    pub usage: Option<Usage>,
    pub added_tool_names: Option<Vec<String>>,
    pub is_error: bool,
    pub timestamp: u64,
}

/// 对应 `Message = UserMessage | AssistantMessage | ToolResultMessage`
// TS 中是引用语义的 union，Rust 中为保持结构一致不装箱（`Box`），故允许 variant 大小差异。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn role(&self) -> &'static str {
        match self {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "toolResult",
        }
    }
}

/// 对应 `Tool<TParameters extends TSchema>`。
/// TS 中 `parameters` 为 TypeBox schema；Rust 中用 JSON schema（`serde_json::Value`）表示。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub constrained_sampling: Option<ConstrainedSamplingConfig>,
}

/// 对应 `ConstrainedSamplingConfig`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSamplingConfig {
    JsonSchema { strict: StrictMode },
    Grammar { variants: BTreeMap<String, String> },
}

/// 对应 `strict: "prefer" | "require"`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StrictMode {
    Prefer,
    Require,
}

/// 对应 `Context`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Option<Vec<Tool>>,
}

/// 对应 `AssistantMessageEvent`（`AssistantMessageEventStream` 的事件协议）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantMessageEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ToolCallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: AssistantMessage,
    },
    Done {
        reason: TerminalStopReason,
        message: AssistantMessage,
    },
    Error {
        reason: ErrorStopReason,
        error: AssistantMessage,
    },
}

/// 对应 `done` 事件的 reason（`stop | length | toolUse | deferred`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TerminalStopReason {
    Stop,
    Length,
    ToolUse,
    Deferred,
}

/// 对应 `error` 事件的 reason（`aborted | error`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorStopReason {
    Aborted,
    Error,
}

/// 对应 `ModelCostRates`（$/million tokens）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// 对应 `ModelCostTier`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    pub input_tokens_above: u64,
}

/// 对应 `ModelCost`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    pub tiers: Option<Vec<ModelCostTier>>,
}

/// 对应 `Model.input` 的元素（`"text" | "image"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    Text,
    Image,
}

/// 对应 `Model<TApi extends Api>`。
/// TS 中 `compat` 为按 api 区分的条件类型；Rust 中简化为 JSON 值，按需解析。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    pub base_url: String,
    pub reasoning: bool,
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<InputModality>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    pub sampling_params: Option<serde_json::Value>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<serde_json::Value>,
}

/// 对应 `ProviderRequestOptions` 的核心字段（去掉 fetch/onPayload/onResponse 等回调，
/// 这些在 provider 适配层处理；`telemetryContext` 的传递方案待 telemetry 层统一设计）。
#[derive(Debug, Clone, Default)]
pub struct ProviderRequestOptions {
    pub signal: Option<AbortSignal>,
    pub api_key: Option<String>,
    pub headers: Option<BTreeMap<String, Option<String>>>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u64>,
    pub max_retry_delay_ms: Option<u64>,
}

/// 对应 `StreamOptions`
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    pub request: ProviderRequestOptions,
    pub temperature: Option<f64>,
    pub sampling_params: Option<serde_json::Value>,
    pub max_tokens: Option<u64>,
    pub transport: Option<Transport>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub websocket_connect_timeout_ms: Option<u64>,
    pub metadata: Option<serde_json::Value>,
}

/// 对应 `SimpleStreamOptions`
#[derive(Debug, Clone, Default)]
pub struct SimpleStreamOptions {
    pub stream: StreamOptions,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Option<ThinkingLevel>,
    pub deferred: Option<serde_json::Value>,
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// 对应 `StreamFunction` / agent 的 `StreamFn`。
///
/// 契约：
/// - 返回 `AssistantMessageEventStream`。
/// - 请求/模型/运行时失败必须编码进返回的流（`error` 事件 + stopReason），不得抛出。
pub type StreamFunction = Arc<
    dyn Fn(&Model, &Context, Option<&SimpleStreamOptions>) -> AssistantMessageEventStream
        + Send
        + Sync,
>;
