//! Rust 翻译自 packages/ai/src/types.ts（核心类型层）
//!
//! 只翻译 agent 运行时依赖的类型，以及 openai/deepseek provider 所需的兼容配置。
//! 其余 40+ provider 的专有类型（Anthropic/Bedrock/OpenRouter/Vercel 等）后续按需补充。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;
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

/// 对应 `SamplingParams = Record<string, unknown>`。
pub type SamplingParams = serde_json::Value;

/// 对应 `SamplingParamsByThinkingLevel = Partial<Record<ModelThinkingLevel, SamplingParams>>`。
pub type SamplingParamsByThinkingLevel = BTreeMap<ModelThinkingLevel, SamplingParams>;

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
    // `ContentBlock` 是 internally tagged：`type` 会被外层标签消费，因此这里必须可缺省。
    #[serde(rename = "type", default)]
    pub kind: TextKind,
    pub text: String,
    pub text_signature: Option<String>,
}

/// 对应 `TextContent.type: "text"` 的字面量（序列化为字符串）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    // `ContentBlock` 是 internally tagged：`type` 会被外层标签消费，因此这里必须可缺省。
    #[serde(rename = "type", default)]
    pub kind: ThinkingKind,
    pub thinking: String,
    pub thinking_signature: Option<String>,
    pub redacted: Option<bool>,
}

/// 对应 `ThinkingContent.type: "thinking"` 的字面量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    // `ContentBlock` 是 internally tagged：`type` 会被外层标签消费，因此这里必须可缺省。
    #[serde(rename = "type", default)]
    pub kind: ImageKind,
    /// base64 编码的图像数据。
    pub data: String,
    /// 例如 "image/jpeg", "image/png"
    pub mime_type: String,
}

/// 对应 `ImageContent.type: "image"` 的字面量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    // `ContentBlock` 是 internally tagged：`type` 会被外层标签消费，因此这里必须可缺省。
    #[serde(rename = "type", default)]
    pub kind: ToolCallKind,
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub thought_signature: Option<String>,
    pub namespace: Option<String>,
}

/// 对应 `ToolCall.type: "toolCall"` 的字面量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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

/// 对应 `DeferredFetchOptions`（`ProviderRequestOptions` + `wait`）。
#[derive(Debug, Clone, Default)]
pub struct DeferredFetchOptions {
    pub request: ProviderRequestOptions,
    /// 最大 provider long-poll 时长（毫秒）。默认 0，只做一次状态检查。
    pub wait: Option<u64>,
}

/// 对应 `DeferredCancelOptions`。
pub type DeferredCancelOptions = ProviderRequestOptions;

/// 对应 `SystemMessage.content: string | TextContent[]`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemContent {
    Text(String),
    Blocks(Vec<TextContent>),
}

/// 对应 `SystemMessage`。
///
/// 让 system prompt 文本与工具变更成为 transcript 的一部分：
/// 首条消息是 base prompt，后续消息是追加指令；工具可用性变化由
/// `tools_added` / `tools_removed` 记录，而不是静默重写请求起始条件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    pub content: SystemContent,
    /// 具名、有序的提示分节：首条声明，后续按名替换，`None` 表示删除。
    /// 用 `IndexMap` 保留插入序（上游为 JS 对象的键序）。
    #[serde(default)]
    pub sections: Option<IndexMap<String, Option<String>>>,
    /// 在此点变为可用的工具的完整定义。
    #[serde(default)]
    pub tools_added: Option<Vec<Tool>>,
    /// 在此点失效的工具。
    #[serde(default)]
    pub tools_removed: Option<Vec<ToolReference>>,
    pub timestamp: u64,
}

/// 对应 `ToolReference`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolReference {
    pub name: String,
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
    /// 对应 `providerThinkingLevel`：provider 实际使用的 effort level（legacy 或 unmanaged 响应为 None）。
    #[serde(default)]
    pub provider_thinking_level: Option<String>,
    /// 对应 `AssistantMessage.thinkingLevel`：agent loop 为本次响应请求的 thinking level
    /// （Rust 侧 `off` 表示为 `None`）。
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub deferred: Option<DeferredHandle>,
    pub error_message: Option<String>,
    pub raw_stop_reason: Option<String>,
    pub end_turn: Option<bool>,
    pub timestamp: u64,
    /// 对应 `durationMs`：从 `timestamp` 到响应结束的毫秒数（单调钟测）。
    /// 由 `AssistantMessageEventStream` 在它看到开始的最终消息上设置；旧消息与后取回的 deferred 结果缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// 对应 `ToolResultMessage`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<TextOrImageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    pub is_error: bool,
    pub timestamp: u64,
    /// 工具执行耗时（毫秒）；旧结果缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// 对应 `Message = SystemMessage | UserMessage | AssistantMessage | ToolResultMessage`
// TS 中是引用语义的 union，Rust 中为保持结构一致不装箱（`Box`），故允许 variant 大小差异。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn role(&self) -> &'static str {
        match self {
            Message::System(_) => "system",
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

/// 对应 `TranscriptContext`：已把 `systemPrompt`/`tools` 折叠进 messages 的上下文。
///
/// 上游用品牌字段防止把普通 `Context` 误传给 provider；Rust 里类型本身即可区分。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptContext {
    pub messages: Vec<Message>,
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

/// 对应 `ModelPromptCache = Partial<Record<Exclude<CacheRetention, "none">, number>>`。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPromptCache {
    pub short: Option<u64>,
    pub long: Option<u64>,
}

/// 对应 `ModelImageResizeOptions`。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelImageResizeOptions {
    pub max_width: Option<u64>,
    pub max_height: Option<u64>,
    pub max_bytes: Option<u64>,
    pub jpeg_quality: Option<u64>,
}

/// 对应 `ModelImageInputLimits`。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelImageInputLimits {
    pub resize: Option<ModelImageResizeOptions>,
    pub max_per_message: Option<u64>,
    pub max_per_request: Option<u64>,
}

/// 对应 `ModelInputLimits`。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInputLimits {
    pub max_request_bytes: Option<u64>,
    pub images: Option<ModelImageInputLimits>,
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
    /// 对应 `samplingParamsByThinkingLevel`：按有效 pi thinking level 选择的采样参数覆盖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params_by_thinking_level: Option<SamplingParamsByThinkingLevel>,
    pub headers: Option<BTreeMap<String, String>>,
    pub compat: Option<serde_json::Value>,
    /// 对应 `promptCache`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    /// 对应 `inputLimits`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
}

/// 对应 `ProviderResponse = { status: number; headers: Record<string, string> }`。
#[derive(Debug, Clone, Default)]
pub struct ProviderResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
}

/// 对应 `onPayload`：请求体发出前调用；返回 `Some(value)` 替换 payload，`None` 保持原样。
#[allow(clippy::type_complexity)]
pub struct OnPayloadFn(
    pub  Arc<
        dyn Fn(
                &serde_json::Value,
                &Model,
            ) -> Pin<Box<dyn Future<Output = Option<serde_json::Value>> + Send>>
            + Send
            + Sync,
    >,
);

impl Clone for OnPayloadFn {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for OnPayloadFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnPayloadFn")
    }
}

impl std::ops::Deref for OnPayloadFn {
    type Target = dyn Fn(
            &serde_json::Value,
            &Model,
        ) -> Pin<Box<dyn Future<Output = Option<serde_json::Value>> + Send>>
        + Send
        + Sync;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// 对应 `onResponse`：HTTP 响应后调用。
#[allow(clippy::type_complexity)]
pub struct OnResponseFn(
    pub  Arc<
        dyn Fn(&ProviderResponse, &Model) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
    >,
);

impl Clone for OnResponseFn {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for OnResponseFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnResponseFn")
    }
}

impl std::ops::Deref for OnResponseFn {
    type Target =
        dyn Fn(&ProviderResponse, &Model) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// 对应 `onProviderStreamEvent`：每个 provider 流事件解析后调用。
#[allow(clippy::type_complexity)]
pub struct OnProviderStreamEventFn(
    pub  Arc<
        dyn Fn(&serde_json::Value, &Model) -> Pin<Box<dyn Future<Output = ()> + Send>>
            + Send
            + Sync,
    >,
);

impl Clone for OnProviderStreamEventFn {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for OnProviderStreamEventFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnProviderStreamEventFn")
    }
}

impl std::ops::Deref for OnProviderStreamEventFn {
    type Target = dyn Fn(&serde_json::Value, &Model) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// 对应 `ProviderRequestOptions` 的核心字段（去掉 fetch 回调与 telemetryContext；
/// `onPayload`/`onResponse` 保留并在 provider 适配层调用）。
#[derive(Debug, Clone, Default)]
pub struct ProviderRequestOptions {
    pub signal: Option<AbortSignal>,
    pub api_key: Option<String>,
    pub headers: Option<BTreeMap<String, Option<String>>>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u64>,
    pub max_retry_delay_ms: Option<u64>,
    pub on_payload: Option<OnPayloadFn>,
    pub on_response: Option<OnResponseFn>,
    /// 对应 `ProviderRequestOptions.env`：provider 作用域环境覆盖（优先于 process env）。
    pub env: Option<ProviderEnv>,
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
    pub on_provider_stream_event: Option<OnProviderStreamEventFn>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `ContentBlock` 是 internally tagged：`type` 由外层标签提供，内层结构的同名字段必须可缺省，
    /// 否则序列化后的 JSON 无法读回（内层的 `type` 已被标签消费）。
    #[test]
    fn content_block_round_trips_through_json() {
        let blocks = vec![
            ContentBlock::Text(TextContent {
                kind: TextKind,
                text: "hi".to_string(),
                text_signature: None,
            }),
            ContentBlock::Thinking(ThinkingContent {
                kind: ThinkingKind,
                thinking: "why".to_string(),
                thinking_signature: None,
                redacted: None,
            }),
            ContentBlock::Image(ImageContent {
                kind: ImageKind,
                data: "AAAA".to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::ToolCall(ToolCall {
                kind: ToolCallKind,
                id: "call-1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({"path": "a"}),
                thought_signature: None,
                namespace: None,
            }),
        ];
        for block in blocks {
            let json = serde_json::to_value(&block).expect("serialise");
            assert_eq!(
                json.get("type").and_then(|value| value.as_str()),
                Some(match &block {
                    ContentBlock::Text(_) => "text",
                    ContentBlock::Thinking(_) => "thinking",
                    ContentBlock::Image(_) => "image",
                    ContentBlock::ToolCall(_) => "toolCall",
                }),
                "标签写在内层"
            );
            let back: ContentBlock = serde_json::from_value(json).expect("deserialise");
            assert_eq!(back, block, "往返必须一致");
        }
    }

    /// 独立序列化的内层结构仍带 `type`（`TextOrImageContent` 是 untagged，不会消费它）。
    #[test]
    fn text_or_image_content_keeps_its_type_field() {
        let content = TextOrImageContent::Text(TextContent {
            kind: TextKind,
            text: "hi".to_string(),
            text_signature: None,
        });
        let json = serde_json::to_value(&content).expect("serialise");
        assert_eq!(
            json.get("type").and_then(|value| value.as_str()),
            Some("text")
        );
        let back: TextOrImageContent = serde_json::from_value(json).expect("deserialise");
        assert_eq!(back, content);
    }
}
