//! 对应 `harness/compaction.ts`：内置压缩任务（spec §8.7）的选段与摘要文本。
//!
//! 本模块先落地**不依赖调度器**的部分：常量、输入/检查点类型，以及
//! `createCompaction`、`selectCut`、`summarizedMessages`、`estimateContext`、
//! `summaryText`、`summaryFailure`、`summaryPrompt`、`serializeConversation`、`contentText`、`truncate`。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游 `createCompaction` 直接引用模块级常量 `CompactionTask`；Rust 侧该定义要到 P5g-2 才落地，
//!   因此做成接受任务定义的工厂 [`make_create_compaction`]（与 `generation.rs` 的 `make_start_run` 同形）。
//! - 上游的 `contentText(content)` 同时接受 `string`（user 消息）与 `{ type, text? }[]`（工具结果）；
//!   Rust 没有隐式 union，拆成 `content_text_of_user` 与 `content_text_of_blocks` 两个函数。
//! - `estimateContext` 用 `view.messages.lastIndexOf(measured)` 定位已测量消息；TS 是**对象身份**比较，
//!   Rust 用 `PartialEq` 的最后一个相等元素代替。同一请求里若出现两条内容完全相同的 assistant 消息，
//!   两者可能取到不同的下标（实际上下文中不会出现，因为消息带有各自的 `timestamp`）。
//! - `serializeConversation` 里工具调用参数的顺序：JS 的 `Object.entries` 保持插入顺序，而
//!   serde_json 默认的 `Map` 是 `BTreeMap`，因此参数按**键的字典序**输出。这是本项目 JSON 容器的
//!   既有差异（见 `harness/json.rs`），会体现在摘要请求文本里。
//! - `truncate` 按 UTF-16 码元切分，以匹配 JS 的 `String.slice(0, maxChars)` 与 `text.length`。
//!
//! # 待落地（P5g-2 余项）
//!
//! `CompactionTask` 本体：上游第 103–233 行与第 393–453 行（phase 处理器、摘要放置、重试与部分消息）。
//! 目前 [`summary_text`] / [`summary_failure`] / [`summary_prompt`] 尚无调用者，标记为暂未使用。

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::utils::estimate::{calculate_context_tokens, estimate_message_tokens};
use pi_ai::utils::retry::{is_retryable_assistant_error, retry_delay_ms};
use pi_ai::{
    AssistantMessage, ContentBlock, Message, ModelThinkingLevel, ProviderRequestOptions,
    SimpleStreamOptions, StopReason, StreamOptions, SystemContent, TextKind, TextOrImageContent,
    UserContent, UserMessage,
};
use serde::{Deserialize, Serialize};

use crate::chord::context::Context;
use crate::chord::delta::PathSegment;
use crate::entries::COMPACTION_ENTRY;
use crate::harness::context::order_tool_results;
use crate::harness::inbox::QueueModes;
use crate::harness::live::{
    CompactionStatus, LIVE_DOC, add_compaction_status, compaction_status_index,
    remove_compaction_status,
};
use crate::harness::provider::ensure_provider_session_id;
use crate::harness::submissions::{StartRun, StartRunFn, admit_submission};
use crate::harness::types::{
    BeforeCompact, CompactDecision, CompactionReason, CompactionResult, ContextView,
    ConversationStreamOptions, HookApi, HookHandlers, ModelRef, NextTaskState, RunningTask,
    SubmissionDraft, TaskRuntime, WriteSubmissionDraft,
};
use crate::harness::usage::{UsageBucket, record_usage};
use crate::session::SessionError;
use crate::session::transaction::Transaction;
use crate::types::{
    ConversationId, DocAccess, Draft, EntryDraft, EntryHead, EntryId, Task, TaskDefinitionSpec,
    TaskId, TaskOptions, TaskOutcome, TaskOutcomeError, TaskOwnership, TaskState,
};

/// 对应 `CompactionInput`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionInput {
    /// 为什么压缩。
    pub reason: CompactionReason,
    /// 抽取摘要时的额外重点（`manual` 压缩可带）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// 对应 `SummaryRequest`：被钉住的摘要请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRequest {
    /// 尝试次数。
    pub attempt: u32,
    /// 摘要模型。
    pub model: ModelRef,
    /// thinking level。
    pub thinking_level: ModelThinkingLevel,
    /// 会话级请求选项。
    pub stream_options: ConversationStreamOptions,
    /// 摘要请求的最大输出。
    pub max_tokens: u64,
    /// 选段所依据的上下文的最新条目。
    pub tail: EntryId,
    /// 逐字保留的第一个条目；摘要的 `head`。
    pub first_kept: EntryId,
}

/// 对应 `CompactionCheckpoint`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum CompactionCheckpoint {
    /// `{ phase: "select" }`。
    Select,
    /// `{ phase: "summarize" } & SummaryRequest`。
    Summarize {
        /// 摘要请求。
        #[serde(flatten)]
        request: SummaryRequest,
    },
    /// `{ phase: "retry"; until: number } & SummaryRequest`。
    Retry {
        /// 下一次尝试的墙钟毫秒。
        until: u64,
        /// 摘要请求。
        #[serde(flatten)]
        request: SummaryRequest,
    },
}

/// 对应 `TOOL_RESULT_MAX_CHARS`：序列化摘要源时保留的最长工具结果文本。
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// 对应 `SUMMARY_PREFIX`。
pub const SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";

/// 对应 `SUMMARY_SUFFIX`。
pub const SUMMARY_SUFFIX: &str = "\n</summary>";

/// 对应 `SUMMARIZATION_SYSTEM_PROMPT`。
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

/// 对应 `SUMMARIZATION_PROMPT`。
pub const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work. If the conversation starts with an earlier summary, preserve its information and fold the newer messages into it.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// 对应 `createCompaction(tx, conversationId, input, owner?)` 的函数形状。
pub type CreateCompactionFn = dyn for<'a> Fn(
        &'a Transaction,
        ConversationId,
        CompactionInput,
        Option<TaskId>,
    ) -> BoxFuture<'a, Result<TaskId, SessionError>>
    + Send
    + Sync;

/// 对应 `createCompaction(tx, conversationId, input, owner?)`。
///
/// 由 compaction 层提供实现（P5g-2）；`startRun` 的压缩分支会在同一提交里调用它。
pub type CreateCompaction = Arc<CreateCompactionFn>;

/// 对应 `createCompaction(tx, conversationId, input, owner?)`：建出压缩任务并登记它的状态。
pub fn make_create_compaction(compaction_task: Arc<Task>) -> CreateCompaction {
    Arc::new(
        move |tx: &Transaction,
              conversation_id: ConversationId,
              input: CompactionInput,
              owner: Option<TaskId>| {
            let task = Arc::clone(&compaction_task);
            let future: BoxFuture<'_, Result<TaskId, SessionError>> = Box::pin(async move {
                let ownership = match owner {
                    None => TaskOwnership::Conversation,
                    Some(task_id) => TaskOwnership::Task { task_id },
                };
                let background = owner.is_none() && input.reason != CompactionReason::Manual;
                let input_json = serde_json::to_value(&input).expect("compaction input serialises");
                let task_id = tx
                    .create_task(
                        &task,
                        input_json,
                        TaskOptions {
                            ownership,
                            conversation_id: Some(conversation_id),
                            background: Some(background),
                        },
                    )
                    .await?;
                let status = CompactionStatus {
                    task_id,
                    reason: input.reason,
                    blocking: owner.is_some(),
                    attempt: 1,
                    retry: None,
                };
                let live = tx
                    .doc(
                        &*LIVE_DOC,
                        DocAccess {
                            owner: Some(conversation_id.get()),
                            key: None,
                        },
                        None,
                    )
                    .await?;
                add_compaction_status(&live, &status)?;
                Ok(task_id)
            });
            future
        },
    )
}

/// 对应 `selectCut(view, keepRecentTokens)`：摘要保留的第一个条目在 `view.entries` 中的下标；
/// 没有可压缩内容时返回 `None`（spec §8.7）。
///
/// 从尾部向前累计，直到保留满 `keepRecentTokens`，然后在**该位置或其后**的第一个候选处切：
/// 候选是贡献以 user 或 assistant 消息开头的条目，永远不是工具结果，也不是其后仍跟着前一个
/// assistant 调用结果的 user 条目。
pub fn select_cut(view: &ContextView, keep_recent_tokens: u64) -> Option<usize> {
    let contributions = &view.contributions;
    let start = if view.head.is_none() { 0 } else { 1 };
    let candidates: Vec<usize> = (start..contributions.len())
        .filter(|&index| is_candidate(contributions, index))
        .collect();
    let mut kept: u64 = 0;
    let mut cut: Option<usize> = None;
    for index in (start..contributions.len()).rev() {
        for message in &contributions[index] {
            kept += estimate_message_tokens(message);
        }
        if kept < keep_recent_tokens {
            continue;
        }
        cut = candidates
            .iter()
            .copied()
            .find(|&candidate| candidate >= index)
            .or_else(|| candidates.last().copied());
        break;
    }
    let cut = cut?;
    for entry in &contributions[start..cut] {
        if !entry.is_empty() {
            return Some(cut);
        }
    }
    None
}

/// 对应 `isCandidate(contributions, index)`。
fn is_candidate(contributions: &[Vec<Message>], index: usize) -> bool {
    let first = contributions[index].first();
    if matches!(first, Some(Message::Assistant(_))) {
        return true;
    }
    if !matches!(first, Some(Message::User(_))) {
        return false;
    }
    // 前一个 assistant 调用的、紧随本条目之后（下一个 assistant 之前）的结果属于本条目之前。
    let mut calls: Vec<String> = Vec::new();
    for before in (0..index).rev() {
        let assistant = contributions[before]
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            });
        let Some(assistant) = assistant else {
            continue;
        };
        calls = assistant
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call.id.clone()),
                _ => None,
            })
            .collect();
        break;
    }
    if calls.is_empty() {
        return true;
    }
    // `after > index` 等价于「不在第一个贡献里」，因此用偏移量表示。
    for (offset, entry) in contributions[index..].iter().enumerate() {
        for (position, message) in entry.iter().enumerate() {
            match message {
                Message::Assistant(_) if offset > 0 || position > 0 => return true,
                Message::ToolResult(result) if calls.contains(&result.tool_call_id) => {
                    return false;
                }
                _ => {}
            }
        }
    }
    true
}

/// 对应 `summarizedMessages(view, cut)`：`cut` 之前条目的模型消息，按模型上下文的顺序（spec §2.1）。
pub fn summarized_messages(view: &ContextView, cut: usize) -> Vec<Message> {
    let flat: Vec<Message> = view
        .contributions
        .get(..cut)
        .unwrap_or(&view.contributions)
        .iter()
        .flatten()
        .cloned()
        .collect();
    order_tool_results(&flat)
}

/// 对应 `estimateContext(view, extra)`：一次请求（`view` 之后接 `extra`）的大小（spec §8.3）。
///
/// 取 head 标记之后追加的最新 assistant 的 usage——它的请求包含该标记——加上其后消息的估算；
/// 没有这样的 assistant 时，估算每一条消息。
pub fn estimate_context(view: &ContextView, extra: &[Message]) -> u64 {
    let head_id = view.head.as_ref().map(|head| head.id);
    let mut measured: Option<&AssistantMessage> = None;
    for index in (0..view.entries.len()).rev() {
        if measured.is_some() {
            break;
        }
        if let Some(head_id) = head_id
            && view.entries[index].id <= head_id
        {
            continue;
        }
        measured = view.contributions[index]
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Assistant(assistant) if calculate_context_tokens(&assistant.usage) > 0 => {
                    Some(assistant)
                }
                _ => None,
            });
    }
    let from = measured
        .and_then(|assistant| {
            view.messages
                .iter()
                .rposition(|message| matches!(message, Message::Assistant(a) if a == assistant))
        })
        .map(|index| index + 1)
        .unwrap_or(0);
    let mut tokens = measured
        .map(|assistant| calculate_context_tokens(&assistant.usage))
        .unwrap_or(0);
    for message in &view.messages[from..] {
        tokens += estimate_message_tokens(message);
    }
    for message in extra {
        tokens += estimate_message_tokens(message);
    }
    tokens
}

/// 对应 `summaryText(message)`：一次干净的 `stop`、有文本且没有工具调用，才算摘要。
#[allow(dead_code)] // P5g-2 的 CompactionTask 落地后由 summarize 阶段使用。
fn summary_text(message: &AssistantMessage) -> Option<String> {
    if message.stop_reason != StopReason::Stop
        || message
            .content
            .iter()
            .any(|content| matches!(content, ContentBlock::ToolCall(_)))
    {
        return None;
    }
    let text = message
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// 对应 `summaryFailure(message)`：不是摘要时说明原因。
#[allow(dead_code)] // P5g-2 的 CompactionTask 落地后由 summarize 阶段使用。
fn summary_failure(message: &AssistantMessage) -> String {
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        let reason = match message.stop_reason {
            StopReason::Error => "error",
            StopReason::Aborted => "aborted",
            _ => unreachable!("已在上面的分支里限定"),
        };
        let detail = message
            .error_message
            .clone()
            .unwrap_or_else(|| reason.to_string());
        return format!("Summarization failed: {detail}");
    }
    if message.stop_reason == StopReason::Length {
        return "Summarization hit the token limit; the summary is incomplete".to_string();
    }
    if message
        .content
        .iter()
        .any(|content| matches!(content, ContentBlock::ToolCall(_)))
    {
        return "Summarization attempted to call a tool".to_string();
    }
    "Summarization produced no text".to_string()
}

/// 对应 `summaryPrompt(messages, instructions)`：摘要器的用户消息。
#[allow(dead_code)] // P5g-2 的 CompactionTask 落地后由 summarize 阶段使用。
fn summary_prompt(messages: &[Message], instructions: Option<&str>) -> String {
    let focus = match instructions {
        None => String::new(),
        Some(instructions) => format!("\n\nAdditional focus: {instructions}"),
    };
    format!(
        "<conversation>\n{}\n</conversation>\n\n{}{}",
        serialize_conversation(messages),
        SUMMARIZATION_PROMPT,
        focus
    )
}

/// 对应 `serializeConversation(messages)`：把消息渲染成纯文本，让摘要器「读」记录而不是继续它。
/// 系统消息被省略。
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let text = content_text_of_user(&user.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let thinking: Vec<&str> = assistant
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        ContentBlock::Thinking(thinking) => Some(thinking.thinking.as_str()),
                        _ => None,
                    })
                    .collect();
                let text: Vec<&str> = assistant
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        ContentBlock::ToolCall(call) => Some(format!(
                            "{}({})",
                            call.name,
                            arguments_text(&call.arguments)
                        )),
                        _ => None,
                    })
                    .collect();
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {}", text.join("\n")));
                }
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = content_text_of_blocks(&result.content);
                if !text.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate(&text, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            Message::System(_) => {}
        }
    }
    parts.join("\n\n")
}

/// 对应 `contentText(content)` 的 `string` 分支（user 消息）。
fn content_text_of_user(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_text_of_blocks(blocks),
    }
}

/// 对应 `contentText(content)` 的 `{ type, text? }[]` 分支。
fn content_text_of_blocks(blocks: &[TextOrImageContent]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            TextOrImageContent::Text(text) => Some(text.text.as_str()),
            TextOrImageContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 对应 `truncate(text, maxChars)`：按 UTF-16 码元多出的部分给出提示。
fn truncate(text: &str, max_chars: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max_chars {
        return text.to_string();
    }
    let head = String::from_utf16_lossy(&units[..max_chars]);
    format!(
        "{head}\n\n[... {} more characters truncated]",
        units.len() - max_chars
    )
}

/// 对应工具调用参数的 `` `${key}=${JSON.stringify(value)}` `` 拼接。
fn arguments_text(arguments: &serde_json::Value) -> String {
    let Some(entries) = arguments.as_object() else {
        return String::new();
    };
    entries
        .iter()
        .map(|(key, value)| {
            format!(
                "{key}={}",
                serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ─── CompactionTask 本体 ───────────────────────────────────────────────────

/// 对应 `CompactionTask`：内置压缩任务（spec §8.7）。
struct CompactionTaskDefinition {
    /// 会话自有压缩的摘要通过写入提交放置；放置后启动一次运行。
    start_run: StartRun,
}

/// 对应 `CompactionTask` 工厂：传入 `start_run`（generation 层提供）。
pub fn make_compaction_task(start_run: StartRun) -> Arc<Task> {
    Arc::new(Task::new(Arc::new(CompactionTaskDefinition { start_run })))
}

/// 对应 `ModelThinkingLevel` 中「非 off」的那部分，映射到 pi-ai 的 `ThinkingLevel`。
pub fn thinking_level(level: ModelThinkingLevel) -> Option<pi_ai::ThinkingLevel> {
    match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal => Some(pi_ai::ThinkingLevel::Minimal),
        ModelThinkingLevel::Low => Some(pi_ai::ThinkingLevel::Low),
        ModelThinkingLevel::Medium => Some(pi_ai::ThinkingLevel::Medium),
        ModelThinkingLevel::High => Some(pi_ai::ThinkingLevel::High),
        ModelThinkingLevel::Xhigh => Some(pi_ai::ThinkingLevel::Xhigh),
        ModelThinkingLevel::Max => Some(pi_ai::ThinkingLevel::Max),
    }
}

/// 对应 `ConversationStreamOptions` 到 pi-ai `SimpleStreamOptions` 的映射。
/// `signal` / `sessionId` / `maxTokens` / `cacheRetention` / `reasoning` 由调用方按需覆盖。
pub fn stream_options(settings: &ConversationStreamOptions) -> SimpleStreamOptions {
    SimpleStreamOptions {
        stream: StreamOptions {
            request: ProviderRequestOptions {
                signal: None,
                api_key: None,
                headers: settings.headers.as_ref().map(|headers| {
                    headers
                        .iter()
                        .map(|(key, value)| (key.clone(), Some(value.clone())))
                        .collect()
                }),
                timeout_ms: settings.timeout_ms,
                max_retries: settings.max_retries.map(|retries| retries as u64),
                max_retry_delay_ms: settings.max_retry_delay_ms,
                on_payload: None,
                on_response: None,
            },
            temperature: None,
            sampling_params: None,
            max_tokens: None,
            transport: settings.transport,
            cache_retention: settings.cache_retention,
            session_id: None,
            websocket_connect_timeout_ms: None,
            metadata: settings.metadata.clone().map(serde_json::Value::Object),
            on_provider_stream_event: None,
        },
        tool_choice: None,
        reasoning: None,
        deferred: settings
            .deferred
            .as_ref()
            .map(|deferred| serde_json::to_value(deferred).unwrap_or(serde_json::Value::Null)),
        thinking_budgets: None,
    }
}

/// 运行中的任务的 checkpoint（`serde_json::Value`）。
fn checkpoint_json(task: &RunningTask) -> serde_json::Value {
    match &task.0.state {
        TaskState::Running { checkpoint } => checkpoint.clone(),
        _ => serde_json::Value::Null,
    }
}

/// `pi.live.compactions[index].key` 的路径。
fn compaction_status_path(index: usize, key: &str) -> Vec<PathSegment> {
    vec![
        PathSegment::Key("compactions".to_string()),
        PathSegment::Index(index),
        PathSegment::Key(key.to_string()),
    ]
}

fn session_error(error: impl std::fmt::Display) -> SessionError {
    SessionError::Message(error.to_string())
}

impl CompactionTaskDefinition {
    /// 对应 `complete(runtime, context)`：移除状态，不写摘要就完成。
    async fn complete(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let runtime = Arc::clone(runtime);
        let runtime_for_closure = Arc::clone(&runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_closure);
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        remove_compaction_status(&live, runtime.task_id())
                            .map_err(session_error)?;
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Completed {
                                result: serde_json::to_value(CompactionResult::default())
                                    .expect("compaction result serialises"),
                            },
                        }))
                    })
                }),
                context,
            )
            .await
    }

    /// 对应 `failNoModel(runtime, ref, context)`。
    async fn fail_no_model(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        reference: Option<&ModelRef>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let message = match reference {
            None => "No model is configured".to_string(),
            Some(reference) => format!(
                "Model {}/{} is not available",
                reference.provider, reference.model_id
            ),
        };
        let runtime = Arc::clone(runtime);
        let runtime_for_closure = Arc::clone(&runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_closure);
                    let message = message.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        remove_compaction_status(&live, runtime.task_id())
                            .map_err(session_error)?;
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Failed {
                                error: TaskOutcomeError {
                                    message,
                                    detail: Some(serde_json::json!({ "reason": "no_model" })),
                                },
                                result: None,
                            },
                        }))
                    })
                }),
                context,
            )
            .await
    }

    /// 对应 `place(runtime, firstKept, summary, context)`：在各自提交里放置 hook 提供的摘要。
    async fn place(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        first_kept: EntryId,
        summary: String,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let runtime = Arc::clone(runtime);
        let runtime_for_closure = Arc::clone(&runtime);
        let start_run = Arc::clone(&self.start_run);
        runtime
            .commit(
                Box::new(move |tx, current| {
                    let runtime = Arc::clone(&runtime_for_closure);
                    let start_run = Arc::clone(&start_run);
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        let next = Self::place_summary(
                            tx,
                            &runtime,
                            &current,
                            &live,
                            first_kept,
                            summary,
                            start_run.as_ref(),
                        )
                        .await?;
                        Ok(Some(next))
                    })
                }),
                context,
            )
            .await
    }

    /// 对应 `placeSummary(tx, runtime, current, live, firstKept, summary)`（spec §8.7）。
    async fn place_summary(
        tx: &Transaction,
        runtime: &Arc<dyn TaskRuntime>,
        current: &RunningTask,
        live: &Draft,
        first_kept: EntryId,
        summary: String,
        start_run: &StartRunFn,
    ) -> Result<NextTaskState, SessionError> {
        remove_compaction_status(live, runtime.task_id()).map_err(session_error)?;
        let text = format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}");
        let input: CompactionInput =
            serde_json::from_value(current.0.input.clone()).map_err(session_error)?;
        let entry = EntryDraft {
            kind: COMPACTION_ENTRY.kind().to_string(),
            head: Some(EntryHead::Entry(first_kept)),
            model: Some(vec![Message::User(UserMessage {
                content: UserContent::Blocks(vec![TextOrImageContent::Text(pi_ai::TextContent {
                    kind: TextKind,
                    text,
                    text_signature: None,
                })]),
                timestamp: runtime.now(),
            })]),
            data: Some(serde_json::json!({ "reason": input.reason })),
            edits: None,
        };
        let result = if current.0.owner.is_none() {
            let submission_id = admit_submission(
                tx,
                runtime.conversation_id(),
                &SubmissionDraft::Write(WriteSubmissionDraft {
                    request_id: Some(format!("compaction:{}", runtime.task_id())),
                    entry,
                }),
                runtime.now(),
                QueueModes::from_settings(&runtime.settings()),
                start_run,
            )
            .await?;
            CompactionResult {
                entry_id: None,
                submission_id: Some(submission_id),
            }
        } else {
            let entry_record = tx
                .append_entry(None, runtime.conversation_id(), entry)
                .await?;
            CompactionResult {
                entry_id: Some(entry_record.id),
                submission_id: None,
            }
        };
        Ok(NextTaskState::Terminal {
            outcome: TaskOutcome::Completed {
                result: serde_json::to_value(result).expect("compaction result serialises"),
            },
        })
    }

    /// 对应 `select` phase：选一个旧前缀，摘要它，或决定放弃/直接给摘要。
    async fn run_select(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let conversation_id = runtime.conversation_id();
        let agent = runtime.agent(Arc::clone(&context)).await?;
        let settings = runtime.settings();
        let reference = agent.model.clone();
        let model = reference.as_ref().and_then(|reference| {
            runtime
                .models()
                .get_model(&reference.provider, &reference.model_id)
        });
        if reference.is_none() || model.is_none() {
            return self
                .fail_no_model(runtime, reference.as_ref(), context)
                .await;
        }
        let model = model.expect("model");
        let policy = settings.compaction;
        let view = runtime
            .context_view(conversation_id, Arc::clone(&context), None)
            .await?;
        let Some(cut) = select_cut(&view, policy.keep_recent_tokens) else {
            return self.complete(runtime, context).await;
        };
        let first_kept = view.entries[cut].id;
        let input: CompactionInput =
            serde_json::from_value(task.0.input.clone()).map_err(session_error)?;
        let compaction = BeforeCompact {
            reason: input.reason,
            entries: view.entries[..cut].to_vec(),
            messages: summarized_messages(&view, cut),
            first_kept,
            instructions: input.instructions.clone(),
        };
        let mut decision: Option<CompactDecision> = None;
        for handlers in runtime.hooks().handlers() {
            if let HookHandlers::Compaction(hook) = handlers.as_ref()
                && decision.is_none()
            {
                let api: Arc<dyn HookApi> =
                    Arc::new(crate::harness::types::RuntimeHookApi::new(runtime.clone()));
                decision = hook
                    .before_compact(&compaction, api, Arc::clone(&context))
                    .await?;
            }
        }
        if let Some(CompactDecision::Decline) = decision {
            return self.complete(runtime, context).await;
        }
        if let Some(CompactDecision::Summary(summary)) = decision {
            return self.place(runtime, first_kept, summary, context).await;
        }
        let tail =
            view.entries.iter().fold(
                first_kept,
                |tail, entry| {
                    if entry.id > tail { entry.id } else { tail }
                },
            );
        let max_tokens = {
            let floor = (0.8 * policy.reserve_tokens as f64).floor() as u64;
            if model.max_tokens > 0 {
                floor.min(model.max_tokens)
            } else {
                floor
            }
        };
        let request = SummaryRequest {
            attempt: 1,
            model: reference.expect("reference"),
            thinking_level: agent.thinking_level,
            stream_options: settings.stream,
            max_tokens,
            tail,
            first_kept,
        };
        let checkpoint = CompactionCheckpoint::Summarize { request };
        let runtime = Arc::clone(runtime);
        runtime
            .commit(
                Box::new(move |_tx, _current| {
                    let checkpoint = checkpoint.clone();
                    Box::pin(async move {
                        Ok(Some(NextTaskState::Running {
                            checkpoint: serde_json::to_value(&checkpoint).map_err(session_error)?,
                        }))
                    })
                }),
                context,
            )
            .await
    }

    /// 对应 `summarize` phase：钉住请求、重试、分类响应并放置摘要。
    async fn run_summarize(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let checkpoint: CompactionCheckpoint =
            serde_json::from_value(checkpoint_json(task)).map_err(session_error)?;
        let CompactionCheckpoint::Summarize { request } = checkpoint else {
            return Err(SessionError::Message(
                "pi.compaction summarize phase has no request".to_string(),
            ));
        };
        let reference = request.model.clone();
        let Some(model) = runtime
            .models()
            .get_model(&reference.provider, &reference.model_id)
        else {
            return self.fail_no_model(runtime, Some(&reference), context).await;
        };
        let conversation_id = runtime.conversation_id();
        let view = runtime
            .context_view(conversation_id, Arc::clone(&context), Some(request.tail))
            .await?;
        let cut = view
            .entries
            .iter()
            .position(|entry| entry.id == request.first_kept)
            .unwrap_or(0);
        let now = runtime.now();
        let input: CompactionInput =
            serde_json::from_value(task.0.input.clone()).map_err(session_error)?;
        let messages = vec![
            Message::System(pi_ai::SystemMessage {
                content: SystemContent::Text(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: now,
            }),
            Message::User(UserMessage {
                content: UserContent::Text(summary_prompt(
                    &summarized_messages(&view, cut),
                    input.instructions.as_deref(),
                )),
                timestamp: now,
            }),
        ];
        let mut options = stream_options(&request.stream_options);
        options.stream.cache_retention = None;
        options.stream.max_tokens = Some(request.max_tokens);
        options.stream.request.signal = Some(runtime.signal());
        options.stream.session_id =
            Some(ensure_provider_session_id(runtime, Arc::clone(&context)).await?);
        options.reasoning = thinking_level(request.thinking_level);
        let message = runtime
            .models()
            .complete_simple(
                model,
                pi_ai::Context {
                    system_prompt: None,
                    messages,
                    tools: None,
                },
                Some(options),
            )
            .await;
        runtime
            .signal()
            .throw_if_aborted()
            .map_err(SessionError::Aborted)?;
        let summary = summary_text(&message);
        let policy = runtime.settings().retry;
        let retry = message.stop_reason == StopReason::Error
            && is_retryable_assistant_error(&message)
            && policy.enabled
            && request.attempt <= policy.max_retries;
        let until = if retry {
            runtime.now()
                + retry_delay_ms(
                    policy.base_delay_ms,
                    policy.max_agent_delay_ms,
                    request.attempt as u64,
                )
        } else {
            0
        };
        let runtime = Arc::clone(runtime);
        let runtime_for_closure = Arc::clone(&runtime);
        let start_run = Arc::clone(&self.start_run);
        runtime
            .commit(
                Box::new(move |tx, current| {
                    let runtime = Arc::clone(&runtime_for_closure);
                    let start_run = Arc::clone(&start_run);
                    let message = message.clone();
                    let summary = summary.clone();
                    let request = request.clone();
                    Box::pin(async move {
                        record_usage(
                            tx,
                            runtime.conversation_id(),
                            UsageBucket::Models,
                            &format!("{}/{}", message.provider, message.model),
                            &message.usage,
                        )
                        .await?;
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        if let Some(summary) = summary {
                            return Ok(Some(
                                Self::place_summary(
                                    tx,
                                    &runtime,
                                    &current,
                                    &live,
                                    request.first_kept,
                                    summary,
                                    start_run.as_ref(),
                                )
                                .await?,
                            ));
                        }
                        if retry {
                            if let Some(index) = compaction_status_index(&live, runtime.task_id()) {
                                let backoff = crate::harness::live::RetryBackoff {
                                    at: until,
                                    error: message.error_message.clone().unwrap_or_default(),
                                };
                                live.set(
                                    compaction_status_path(index, "retry"),
                                    serde_json::to_value(&backoff).map_err(session_error)?,
                                )
                                .map_err(session_error)?;
                            }
                            return Ok(Some(NextTaskState::Running {
                                checkpoint: serde_json::to_value(&CompactionCheckpoint::Retry {
                                    until,
                                    request,
                                })
                                .map_err(session_error)?,
                            }));
                        }
                        remove_compaction_status(&live, runtime.task_id())
                            .map_err(session_error)?;
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Failed {
                                error: TaskOutcomeError {
                                    message: summary_failure(&message),
                                    detail: Some(serde_json::json!({ "reason": "model_error" })),
                                },
                                result: None,
                            },
                        }))
                    })
                }),
                context,
            )
            .await
    }

    /// 对应 `retry` phase：睡到 `until`，加一次尝试，回到 `summarize`。
    async fn run_retry(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let checkpoint: CompactionCheckpoint =
            serde_json::from_value(checkpoint_json(task)).map_err(session_error)?;
        let CompactionCheckpoint::Retry { until, request } = checkpoint else {
            return Err(SessionError::Message(
                "pi.compaction retry phase has no request".to_string(),
            ));
        };
        runtime.sleep(until, Arc::clone(&context)).await?;
        let attempt = request.attempt + 1;
        let runtime = Arc::clone(runtime);
        let runtime_for_closure = Arc::clone(&runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_closure);
                    let request = request.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        if let Some(index) = compaction_status_index(&live, runtime.task_id()) {
                            live.set(
                                compaction_status_path(index, "attempt"),
                                serde_json::Value::from(attempt),
                            )
                            .map_err(session_error)?;
                            live.delete(compaction_status_path(index, "retry"))
                                .map_err(session_error)?;
                        }
                        Ok(Some(NextTaskState::Running {
                            checkpoint: serde_json::to_value(&CompactionCheckpoint::Summarize {
                                request,
                            })
                            .map_err(session_error)?,
                        }))
                    })
                }),
                context,
            )
            .await
    }
}

/// 会话文档访问。
fn doc_access(conversation_id: ConversationId) -> DocAccess {
    DocAccess {
        owner: Some(conversation_id.get()),
        key: None,
    }
}

#[async_trait::async_trait]
impl TaskDefinitionSpec for CompactionTaskDefinition {
    fn name(&self) -> &str {
        "pi.compaction"
    }

    fn version(&self) -> u32 {
        1
    }

    fn initial(&self, _input: &serde_json::Value) -> serde_json::Value {
        serde_json::to_value(CompactionCheckpoint::Select).expect("select serialises")
    }

    fn phases(&self) -> &[&'static str] {
        &["select", "summarize", "retry"]
    }

    async fn run_phase(
        &self,
        phase: &str,
        task: RunningTask,
        runtime: Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        match phase {
            "select" => self.run_select(&task, &runtime, context).await,
            "summarize" => self.run_summarize(&task, &runtime, context).await,
            "retry" => self.run_retry(&task, &runtime, context).await,
            _ => Err(SessionError::Message(format!(
                "Task pi.compaction has no phase handler for {phase}"
            ))),
        }
    }

    async fn abort(
        &self,
        _task: RunningTask,
        runtime: Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let runtime_for_closure = Arc::clone(&runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_closure);
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        remove_compaction_status(&live, runtime.task_id())
                            .map_err(session_error)?;
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Aborted {
                                reason: None,
                                result: None,
                            },
                        }))
                    })
                }),
                context,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EntryRecord;
    use pi_ai::{TextContent, ToolCall, ToolCallKind, Usage, UsageCost};
    use serde_json::json;

    fn usage(input: u64) -> Usage {
        Usage {
            input,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input,
            cost: UsageCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.0,
            },
        }
    }

    fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
        AssistantMessage {
            content,
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: usage(0),
            stop_reason,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
            duration_ms: None,
        }
    }

    fn text_message(text: &str) -> Message {
        Message::Assistant(assistant(
            vec![ContentBlock::Text(TextContent {
                kind: Default::default(),
                text: text.to_string(),
                text_signature: None,
            })],
            StopReason::Stop,
        ))
    }

    fn user_message(text: &str) -> Message {
        Message::User(pi_ai::UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn tool_result(id: &str, text: &str) -> Message {
        Message::ToolResult(pi_ai::ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: "bash".to_string(),
            content: vec![TextOrImageContent::Text(TextContent {
                kind: Default::default(),
                text: text.to_string(),
                text_signature: None,
            })],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 0,
            duration_ms: None,
        })
    }

    fn view(contributions: Vec<Vec<Message>>) -> ContextView {
        ContextView {
            head: None,
            entries: Vec::<EntryRecord>::new(),
            contributions,
            messages: Vec::new(),
        }
    }

    #[test]
    fn select_cut_keeps_the_tail_and_cuts_at_the_first_later_candidate() {
        let view = view(vec![
            vec![user_message("one")],
            vec![text_message("two")],
            vec![user_message("three")],
            vec![text_message("four")],
        ]);
        // 保留 0 个 token 时立刻满足，切在「该位置或其后」的第一个候选——即最后一条。
        assert_eq!(select_cut(&view, 0), Some(3));
    }

    #[test]
    fn select_cut_returns_none_when_nothing_is_old_enough() {
        let view = view(vec![vec![user_message("one")], vec![text_message("two")]]);
        assert_eq!(
            select_cut(&view, u64::MAX),
            None,
            "整段上下文都算「最近」，没有可压缩的前缀"
        );
    }

    #[test]
    fn select_cut_returns_none_when_every_entry_before_the_cut_is_empty() {
        let view = view(vec![
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![text_message("tail")],
        ]);
        // 切点是 3，但 0..3 全是空贡献——没有可摘要的内容。
        assert_eq!(select_cut(&view, 0), None);
    }

    #[test]
    fn select_cut_skips_the_head_marker() {
        let contributions = vec![
            vec![text_message("summarised")],
            Vec::new(),
            vec![user_message("tail")],
        ];
        // 没有 head 时，第 0 条也能当候选：cut 之前有非空贡献 → Some(2)。
        assert_eq!(select_cut(&view(contributions.clone()), 0), Some(2));

        // 有 head 时 start = 1，第 0 条（已是摘要）不再参与；1..2 为空 → 没有可压缩内容。
        let mut with_head = view(contributions);
        with_head.head = Some(EntryRecord {
            id: EntryId::new(7),
            conversation_id: ConversationId::new(1),
            kind: "pi.custom".to_string(),
            model: None,
            data: None,
            head: None,
            edits: None,
            by_task_id: None,
        });
        assert_eq!(select_cut(&with_head, 0), None);
    }

    #[test]
    fn a_user_entry_followed_by_its_tool_result_is_not_a_candidate() {
        // assistant 调用了 call-1，其 toolResult 紧随 user 条目之后——该 user 条目不能当切点。
        let assistant_with_call = Message::Assistant(assistant(
            vec![ContentBlock::ToolCall(ToolCall {
                kind: ToolCallKind,
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: json!({"command": "ls"}),
                thought_signature: None,
                namespace: None,
            })],
            StopReason::ToolUse,
        ));
        let contributions = vec![
            vec![assistant_with_call],
            vec![user_message("steering")],
            vec![tool_result("call-1", "ok")],
        ];
        assert!(
            !is_candidate(&contributions, 1),
            "结果跟在后面时，这个 user 条目仍属于前一个 assistant 的调用"
        );
        assert!(is_candidate(&contributions, 0), "assistant 条目总是候选");
    }

    #[test]
    fn serialize_conversation_renders_each_role() {
        let assistant_with_everything = Message::Assistant(assistant(
            vec![
                ContentBlock::Thinking(pi_ai::ThinkingContent {
                    kind: Default::default(),
                    thinking: "weighing options".to_string(),
                    thinking_signature: None,
                    redacted: None,
                }),
                ContentBlock::Text(TextContent {
                    kind: Default::default(),
                    text: "done".to_string(),
                    text_signature: None,
                }),
                ContentBlock::ToolCall(ToolCall {
                    kind: ToolCallKind,
                    id: "call-1".to_string(),
                    name: "bash".to_string(),
                    arguments: json!({"command": "ls"}),
                    thought_signature: None,
                    namespace: None,
                }),
            ],
            StopReason::ToolUse,
        ));
        let text = serialize_conversation(&[
            user_message("hello"),
            assistant_with_everything,
            tool_result("call-1", "file.txt"),
        ]);
        assert_eq!(
            text,
            "[User]: hello\n\n\
             [Assistant thinking]: weighing options\n\n\
             [Assistant]: done\n\n\
             [Assistant tool calls]: bash(command=\"ls\")\n\n\
             [Tool result]: file.txt"
        );
    }

    #[test]
    fn serialize_conversation_omits_system_messages_and_empty_parts() {
        let text = serialize_conversation(&[
            Message::System(pi_ai::SystemMessage {
                content: pi_ai::SystemContent::Text("policy".to_string()),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: 0,
            }),
            user_message(""),
            text_message("   "),
        ]);
        // 「   」trim 后为空但上游只对 user/工具结果判空；assistant 文本原样输出。
        assert_eq!(text, "[Assistant]:    ");
    }

    #[test]
    fn serialize_conversation_truncates_long_tool_results() {
        let long = "x".repeat(TOOL_RESULT_MAX_CHARS + 5);
        let text = serialize_conversation(&[tool_result("call-1", &long)]);
        assert!(
            text.ends_with("[... 5 more characters truncated]"),
            "{text}"
        );
        assert_eq!(
            text.len(),
            "[Tool result]: ".len()
                + TOOL_RESULT_MAX_CHARS
                + "\n\n[... 5 more characters truncated]".len()
        );
    }

    #[test]
    fn summary_text_requires_a_clean_stop_without_tool_calls() {
        let ok = assistant(
            vec![ContentBlock::Text(TextContent {
                kind: Default::default(),
                text: "  the summary  ".to_string(),
                text_signature: None,
            })],
            StopReason::Stop,
        );
        assert_eq!(summary_text(&ok).as_deref(), Some("the summary"));

        let blank = assistant(
            vec![ContentBlock::Text(TextContent {
                kind: Default::default(),
                text: "   ".to_string(),
                text_signature: None,
            })],
            StopReason::Stop,
        );
        assert_eq!(summary_text(&blank), None, "没有文本不算摘要");

        let truncated = assistant(
            vec![ContentBlock::Text(TextContent {
                kind: Default::default(),
                text: "half".to_string(),
                text_signature: None,
            })],
            StopReason::Length,
        );
        assert_eq!(
            summary_text(&truncated),
            None,
            "被截断的停止原因不算干净的 stop"
        );

        let called = assistant(
            vec![ContentBlock::ToolCall(ToolCall {
                kind: ToolCallKind,
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
            })],
            StopReason::Stop,
        );
        assert_eq!(summary_text(&called), None, "调用工具的响应不是摘要");
    }

    #[test]
    fn summary_failure_names_the_reason() {
        let mut errored = assistant(Vec::new(), StopReason::Error);
        errored.error_message = Some("boom".to_string());
        assert_eq!(summary_failure(&errored), "Summarization failed: boom");

        let aborted = assistant(Vec::new(), StopReason::Aborted);
        assert_eq!(summary_failure(&aborted), "Summarization failed: aborted");

        let limited = assistant(Vec::new(), StopReason::Length);
        assert_eq!(
            summary_failure(&limited),
            "Summarization hit the token limit; the summary is incomplete"
        );

        let called = assistant(
            vec![ContentBlock::ToolCall(ToolCall {
                kind: ToolCallKind,
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
            })],
            StopReason::Stop,
        );
        assert_eq!(
            summary_failure(&called),
            "Summarization attempted to call a tool"
        );

        let empty = assistant(Vec::new(), StopReason::Stop);
        assert_eq!(summary_failure(&empty), "Summarization produced no text");
    }

    #[test]
    fn summary_prompt_wraps_the_transcript_and_appends_the_focus() {
        let prompt = summary_prompt(&[user_message("hello")], None);
        assert!(prompt.starts_with("<conversation>\n[User]: hello\n</conversation>\n\n"));
        assert!(
            prompt.ends_with("error messages."),
            "没有 instructions 时不追加 focus"
        );

        let focused = summary_prompt(&[user_message("hello")], Some("keep the todo list"));
        assert!(focused.ends_with("\n\nAdditional focus: keep the todo list"));
    }

    #[test]
    fn estimate_context_counts_estimates_when_nothing_was_measured() {
        let messages = vec![user_message("hello world"), text_message("hi")];
        let mut view = view(Vec::new());
        view.messages = messages.clone();
        let extra = vec![user_message("next")];
        let expected: u64 = messages
            .iter()
            .chain(extra.iter())
            .map(estimate_message_tokens)
            .sum();
        assert_eq!(estimate_context(&view, &extra), expected);
    }

    #[test]
    fn estimate_context_starts_after_the_measured_assistant() {
        // entries 里最后一条的贡献含一条「已测量」的 assistant；估算只从它之后开始。
        let measured = AssistantMessage {
            usage: usage(500),
            ..assistant(
                vec![ContentBlock::Text(TextContent {
                    kind: Default::default(),
                    text: "measured".to_string(),
                    text_signature: None,
                })],
                StopReason::Stop,
            )
        };
        let after = text_message("after");
        let mut view = view(vec![
            vec![Message::Assistant(measured.clone())],
            vec![after.clone()],
        ]);
        view.entries = vec![EntryRecord {
            id: EntryId::new(1),
            conversation_id: ConversationId::new(1),
            kind: "pi.custom".to_string(),
            model: None,
            data: None,
            head: None,
            edits: None,
            by_task_id: None,
        }];
        view.messages = vec![Message::Assistant(measured), after.clone()];
        let extra = vec![user_message("next")];
        let expected = 500 + estimate_message_tokens(&after) + estimate_message_tokens(&extra[0]);
        assert_eq!(estimate_context(&view, &extra), expected);
    }

    #[test]
    fn truncate_matches_javascript_code_unit_slicing() {
        assert_eq!(truncate("short", 10), "short");
        let text = "a".repeat(12);
        assert_eq!(
            truncate(&text, 10),
            "aaaaaaaaaa\n\n[... 2 more characters truncated]"
        );
    }

    #[test]
    fn compaction_checkpoint_round_trips_with_camel_case_keys() {
        let checkpoint = CompactionCheckpoint::Retry {
            until: 1_700,
            request: SummaryRequest {
                attempt: 2,
                model: ModelRef {
                    provider: "anthropic".to_string(),
                    model_id: "claude".to_string(),
                },
                thinking_level: ModelThinkingLevel::Off,
                stream_options: ConversationStreamOptions::default(),
                max_tokens: 4_096,
                tail: EntryId::new(9),
                first_kept: EntryId::new(3),
            },
        };
        let json = serde_json::to_value(&checkpoint).expect("serialises");
        assert_eq!(json["phase"], json!("retry"));
        assert_eq!(json["until"], json!(1_700));
        assert_eq!(json["maxTokens"], json!(4_096));
        assert_eq!(json["firstKept"], json!(3));
        assert_eq!(
            serde_json::from_value::<CompactionCheckpoint>(json).expect("round trips"),
            checkpoint
        );
        assert_eq!(
            serde_json::to_value(CompactionCheckpoint::Select).expect("serialises"),
            json!({"phase": "select"})
        );
    }

    #[test]
    fn compaction_task_metadata_matches_upstream() {
        let start_run: StartRun =
            Arc::new(|_tx, _cid, _live, _inputs| Box::pin(async move { Ok(()) }));
        let task = make_compaction_task(start_run);
        assert_eq!(task.definition().name(), "pi.compaction");
        assert_eq!(task.definition().version(), 1);
        assert_eq!(
            task.definition().phases(),
            &["select", "summarize", "retry"]
        );
        assert_eq!(
            task.definition().initial(&serde_json::Value::Null),
            json!({"phase": "select"})
        );
    }

    #[test]
    fn stream_options_maps_conversation_settings() {
        let settings = ConversationStreamOptions {
            transport: Some(pi_ai::Transport::Sse),
            timeout_ms: Some(5_000),
            max_retries: Some(3),
            max_retry_delay_ms: Some(60_000),
            headers: Some(std::collections::BTreeMap::from([(
                "x-test".to_string(),
                "y".to_string(),
            )])),
            metadata: Some(serde_json::Map::from_iter([(
                "m".to_string(),
                serde_json::json!(1),
            )])),
            cache_retention: Some(pi_ai::CacheRetention::Short),
            deferred: None,
        };
        let options = stream_options(&settings);
        assert_eq!(options.stream.transport, Some(pi_ai::Transport::Sse));
        assert_eq!(options.stream.request.timeout_ms, Some(5_000));
        assert_eq!(options.stream.request.max_retries, Some(3));
        assert_eq!(options.stream.request.max_retry_delay_ms, Some(60_000));
        assert_eq!(
            options.stream.cache_retention,
            Some(pi_ai::CacheRetention::Short)
        );
        assert_eq!(
            options.stream.request.headers,
            Some(std::collections::BTreeMap::from([(
                "x-test".to_string(),
                Some("y".to_string())
            )]))
        );
    }
}
