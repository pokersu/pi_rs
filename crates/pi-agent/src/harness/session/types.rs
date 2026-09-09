//! Rust 翻译自 packages/agent/src/harness/session/types.ts
//!
//! durable session 的完整类型系统：Entry、Operation 状态机、Storage 接口、Session 接口。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pi_ai::{AssistantMessage, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::harness::compaction::compaction::CompactionSettings;
use crate::harness::context::Context;
use crate::harness::session::values::{
    ListElement, ListReadOptions, ListWrite, StoredValue, Value, ValueList, ValueWrite,
};
use crate::harness::types::AgentHarnessStreamOptions;
use crate::types::{AgentMessage, QueueMode, ThinkingLevel};

/// 对应 `SettledAssistantMessage = AssistantMessage & { stopReason: Exclude<StopReason, "pending"> }`。
/// Rust 中 `Exclude` 无法静态表达，故用 type alias 并在消费处按非 `pending` 约定处理。
pub type SettledAssistantMessage = AssistantMessage;

/// 对应 `EntryType`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    Message,
    Compaction,
    BranchSummary,
    Custom,
}

/// 对应 `EntryBase`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryBase {
    pub id: String,
    pub parent_id: Option<String>,
    pub seq: u64,
    pub timestamp: u64,
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    pub custom_type: Option<String>,
}

/// 对应 `MessageEntry`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub message: AgentMessage,
    pub terminate: Option<bool>,
}

/// 对应 `CompactionEntry`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub summary: String,
    pub retained_tail: Vec<AgentMessage>,
    pub tokens_before: u64,
    pub details: Option<Json>,
    pub usage: Option<Usage>,
    pub from_hook: bool,
}

/// 对应 `BranchSummaryEntry`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub from_id: Option<String>,
    pub summary: String,
    pub details: Option<Json>,
    pub usage: Option<Usage>,
    pub from_hook: bool,
}

/// 对应 `CustomEntry`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub custom_type: String,
    pub data: Option<Json>,
}

/// 对应 `Entry`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Entry {
    Message(MessageEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    Custom(CustomEntry),
}

impl Entry {
    pub fn base(&self) -> &EntryBase {
        match self {
            Entry::Message(e) => &e.base,
            Entry::Compaction(e) => &e.base,
            Entry::BranchSummary(e) => &e.base,
            Entry::Custom(e) => &e.base,
        }
    }

    pub fn id(&self) -> &str {
        &self.base().id
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.base().parent_id.as_deref()
    }

    pub fn entry_type(&self) -> EntryType {
        self.base().entry_type
    }
}

/// 对应 `EntryProjector`：将应用自定义 entry 转换为模型上下文。
pub type EntryProjector = Arc<
    dyn Fn(
            &CustomEntry,
            &Context,
        ) -> Pin<Box<dyn Future<Output = Option<Vec<AgentMessage>>> + Send>>
        + Send
        + Sync,
>;

/// 对应 `NewEntry = Omit<Entry, "seq" | "timestamp">`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum NewEntry {
    Message(NewMessageEntry),
    Compaction(NewCompactionEntry),
    BranchSummary(NewBranchSummaryEntry),
    Custom(NewCustomEntry),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewMessageEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub custom_type: Option<String>,
    pub message: AgentMessage,
    pub terminate: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewCompactionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub custom_type: Option<String>,
    pub summary: String,
    pub retained_tail: Vec<AgentMessage>,
    pub tokens_before: u64,
    pub details: Option<Json>,
    pub usage: Option<Usage>,
    pub from_hook: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewBranchSummaryEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub custom_type: Option<String>,
    pub from_id: Option<String>,
    pub summary: String,
    pub details: Option<Json>,
    pub usage: Option<Usage>,
    pub from_hook: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewCustomEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub custom_type: String,
    pub data: Option<Json>,
}

impl NewEntry {
    pub fn id(&self) -> &str {
        match self {
            NewEntry::Message(e) => &e.id,
            NewEntry::Compaction(e) => &e.id,
            NewEntry::BranchSummary(e) => &e.id,
            NewEntry::Custom(e) => &e.id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            NewEntry::Message(e) => e.parent_id.as_deref(),
            NewEntry::Compaction(e) => e.parent_id.as_deref(),
            NewEntry::BranchSummary(e) => e.parent_id.as_deref(),
            NewEntry::Custom(e) => e.parent_id.as_deref(),
        }
    }

    pub fn entry_type(&self) -> EntryType {
        match self {
            NewEntry::Message(_) => EntryType::Message,
            NewEntry::Compaction(_) => EntryType::Compaction,
            NewEntry::BranchSummary(_) => EntryType::BranchSummary,
            NewEntry::Custom(_) => EntryType::Custom,
        }
    }

    pub fn custom_type(&self) -> Option<&str> {
        match self {
            NewEntry::Message(e) => e.custom_type.as_deref(),
            NewEntry::Compaction(e) => e.custom_type.as_deref(),
            NewEntry::BranchSummary(e) => e.custom_type.as_deref(),
            NewEntry::Custom(e) => Some(&e.custom_type),
        }
    }

    pub fn materialize(self, seq: u64, timestamp: u64) -> Entry {
        match self {
            NewEntry::Message(e) => Entry::Message(MessageEntry {
                base: EntryBase {
                    id: e.id,
                    parent_id: e.parent_id,
                    seq,
                    timestamp,
                    entry_type: EntryType::Message,
                    custom_type: e.custom_type,
                },
                message: e.message,
                terminate: e.terminate,
            }),
            NewEntry::Compaction(e) => Entry::Compaction(CompactionEntry {
                base: EntryBase {
                    id: e.id,
                    parent_id: e.parent_id,
                    seq,
                    timestamp,
                    entry_type: EntryType::Compaction,
                    custom_type: e.custom_type,
                },
                summary: e.summary,
                retained_tail: e.retained_tail,
                tokens_before: e.tokens_before,
                details: e.details,
                usage: e.usage,
                from_hook: e.from_hook,
            }),
            NewEntry::BranchSummary(e) => Entry::BranchSummary(BranchSummaryEntry {
                base: EntryBase {
                    id: e.id,
                    parent_id: e.parent_id,
                    seq,
                    timestamp,
                    entry_type: EntryType::BranchSummary,
                    custom_type: e.custom_type,
                },
                from_id: e.from_id,
                summary: e.summary,
                details: e.details,
                usage: e.usage,
                from_hook: e.from_hook,
            }),
            NewEntry::Custom(e) => Entry::Custom(CustomEntry {
                base: EntryBase {
                    id: e.id,
                    parent_id: e.parent_id,
                    seq,
                    timestamp,
                    entry_type: EntryType::Custom,
                    custom_type: None,
                },
                custom_type: e.custom_type,
                data: e.data,
            }),
        }
    }
}

/// 对应 `LaneConfiguration`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneConfiguration {
    pub model: ModelIdentity,
    pub thinking_level: ThinkingLevel,
    pub active_tool_names: Vec<String>,
}

/// 对应 `{ provider, modelId }`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelIdentity {
    pub provider: String,
    pub model_id: String,
}

/// 对应 `OperationMeta.intent`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationIntent {
    Run {
        prompt_entry_ids: Vec<String>,
    },
    Compaction {
        custom_instructions: Option<String>,
    },
    Navigation {
        target_id: Option<String>,
        summarize: bool,
        label: Option<String>,
        custom_instructions: Option<String>,
    },
}

/// 对应 `OperationMeta`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationMeta {
    pub operation_id: String,
    pub lane: String,
    pub source_tip_id: Option<String>,
    pub started_at: u64,
    pub intent: OperationIntent,
}

/// 对应 `Control`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Control {
    Running,
    CancelRequested { requested_at: u64 },
}

/// 对应 `OperationError`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationError {
    pub code: String,
    pub message: String,
    pub details: Option<Json>,
}

/// 对应 `TerminalStatus`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    Completed,
    Declined,
    Aborted,
    Failed,
}

/// 对应 `OperationResultRecord`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationResultRecord {
    pub operation_id: String,
    pub kind: OperationIntent,
    pub status: TerminalStatus,
    pub error: Option<OperationError>,
    pub from_tip_id: Option<String>,
    pub tip_id: Option<String>,
    pub started_at: u64,
    pub ended_at: u64,
}

/// 对应 `Continuation`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Continuation {
    NeedAssistant { overflow_recovery_used: bool },
    MayFinish { include_final_assistant: bool },
}

/// 对应 `CheckpointData`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckpointData {
    pub continuation: Continuation,
    pub trigger_entry_id: String,
}

/// 对应 `InboxItemKind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InboxItemKind {
    Steer,
    FollowUp,
    NextRun,
    Write,
}

/// 对应 `InboxItem`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxItem {
    pub entry_id: String,
    pub kind: InboxItemKind,
}

/// 对应 `NormalizedRetryPolicy`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedRetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_agent_delay_ms: u64,
}

/// 对应 `GenerationContext`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationContext {
    pub step_id: String,
    pub trigger_entry_id: String,
    pub configuration: LaneConfiguration,
    pub stream_options: AgentHarnessStreamOptions,
    pub retry_policy: NormalizedRetryPolicy,
    pub overflow_recovery_used: bool,
}

/// 对应 `ToolCall`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolCall {
    Planned {
        source_index: usize,
        result_entry_id: String,
    },
    EffectPending {
        source_index: usize,
        result_entry_id: String,
        replay: ReplayPolicy,
    },
    OutcomeReady {
        source_index: usize,
        result_entry_id: String,
        terminate: bool,
    },
    Completed {
        source_index: usize,
        result_entry_id: String,
        terminate: bool,
    },
}

impl ToolCall {
    pub fn source_index(&self) -> usize {
        match self {
            ToolCall::Planned { source_index, .. }
            | ToolCall::EffectPending { source_index, .. }
            | ToolCall::OutcomeReady { source_index, .. }
            | ToolCall::Completed { source_index, .. } => *source_index,
        }
    }

    pub fn result_entry_id(&self) -> String {
        match self {
            ToolCall::Planned {
                result_entry_id, ..
            }
            | ToolCall::EffectPending {
                result_entry_id, ..
            }
            | ToolCall::OutcomeReady {
                result_entry_id, ..
            }
            | ToolCall::Completed {
                result_entry_id, ..
            } => result_entry_id.clone(),
        }
    }
}

/// 对应 `replay: "never" | "safe"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    Never,
    Safe,
}

/// 对应 `ToolBatch`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolBatch {
    pub assistant_entry_id: String,
    pub configuration: LaneConfiguration,
    pub turn_id: String,
    pub calls: Vec<ToolCall>,
}

/// 对应 `SummaryContext`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryContext {
    pub result_entry_id: String,
    pub configuration: LaneConfiguration,
    pub stream_options: AgentHarnessStreamOptions,
    pub retry_policy: NormalizedRetryPolicy,
}

/// 对应 `Cancellable`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cancellable {
    pub control: Control,
}

/// 对应 `RunSettings`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSettings {
    pub compaction: CompactionSettings,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub tool_execution: ToolExecution,
}

/// 对应 `toolExecution: "sequential" | "parallel"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecution {
    Sequential,
    Parallel,
}

/// 对应 `OperationScope`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationScope {
    pub control: Control,
    pub settings: RunSettings,
    pub latest_assistant_entry_id: Option<String>,
}

/// 对应 `RetryWait`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryWait {
    pub next_attempt: u32,
    pub not_before: u64,
    pub error_message: String,
}

/// 对应 `AssistantGenerationScope`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantGenerationScope {
    pub generation_context: GenerationContext,
}

/// 对应 `ResultBoundary`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResultBoundary {
    ResumeCheckpoint {
        resume_after: CheckpointData,
    },
    Finish,
    CommitNavigation {
        target_id: String,
        label: Option<String>,
    },
}

/// 对应 `SummaryTask`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryTask {
    pub task_id: String,
    pub reason: Option<String>,
    pub custom_instructions: Option<String>,
    pub boundary: ResultBoundary,
}

/// 对应 `SummaryGenerationScope`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryGenerationScope {
    pub task: SummaryTask,
    pub summary_context: SummaryContext,
}

/// 对应 `DeferredScope`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredScope {
    pub control: Control,
    pub settings: RunSettings,
    pub latest_assistant_entry_id: Option<String>,
    pub step_id: String,
    pub source_entry_id: String,
    pub poll: u64,
    pub configuration: LaneConfiguration,
    pub stream_options: AgentHarnessStreamOptions,
}

/// 对应 `OperationState`（13 种 flat leaf）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "at")]
pub enum OperationState {
    #[serde(rename = "starting")]
    Starting {
        #[serde(flatten)]
        scope: OperationScope,
    },
    #[serde(rename = "checkpoint")]
    Checkpoint {
        #[serde(flatten)]
        scope: OperationScope,
        continuation: Continuation,
        trigger_entry_id: String,
    },
    #[serde(rename = "assistant.ready")]
    AssistantReady {
        #[serde(flatten)]
        scope: OperationScope,
        generation_context: GenerationContext,
        next_attempt: u32,
    },
    #[serde(rename = "assistant.effect_pending")]
    AssistantEffectPending {
        #[serde(flatten)]
        scope: OperationScope,
        generation_context: GenerationContext,
        attempt: u32,
        response_entry_id: String,
        usage_id: String,
        intended_output_limit: u64,
        context_window: u64,
    },
    #[serde(rename = "assistant.retry_wait")]
    AssistantRetryWait {
        #[serde(flatten)]
        scope: OperationScope,
        generation_context: GenerationContext,
        next_attempt: u32,
        not_before: u64,
        error_message: String,
    },
    #[serde(rename = "tools")]
    Tools {
        #[serde(flatten)]
        scope: OperationScope,
        batch: ToolBatch,
    },
    #[serde(rename = "deferred.suspended")]
    DeferredSuspended {
        #[serde(flatten)]
        scope: DeferredScope,
    },
    #[serde(rename = "deferred.effect_pending")]
    DeferredEffectPending {
        #[serde(flatten)]
        scope: DeferredScope,
        response_entry_id: String,
        usage_id: String,
    },
    #[serde(rename = "summary.deciding")]
    SummaryDeciding {
        #[serde(flatten)]
        scope: OperationScope,
        task: SummaryTask,
    },
    #[serde(rename = "summary.ready")]
    SummaryReady {
        #[serde(flatten)]
        scope: OperationScope,
        task: SummaryTask,
        summary_context: SummaryContext,
        next_attempt: u32,
    },
    #[serde(rename = "summary.effect_pending")]
    SummaryEffectPending {
        #[serde(flatten)]
        scope: OperationScope,
        task: SummaryTask,
        summary_context: SummaryContext,
        attempt: u32,
        request: Option<SummaryRequest>,
        usage_ids: Vec<String>,
    },
    #[serde(rename = "summary.retry_wait")]
    SummaryRetryWait {
        #[serde(flatten)]
        scope: OperationScope,
        task: SummaryTask,
        summary_context: SummaryContext,
        next_attempt: u32,
        not_before: u64,
        error_message: String,
    },
    #[serde(rename = "navigation.ready_to_commit")]
    NavigationReadyToCommit {
        #[serde(flatten)]
        scope: OperationScope,
        target_id: Option<String>,
        label: Option<String>,
    },
}

/// 对应 `SummaryRequest = { index, usageId }`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRequest {
    pub index: usize,
    pub usage_id: String,
}

impl OperationState {
    pub fn scope(&self) -> &OperationScope {
        match self {
            OperationState::Starting { scope }
            | OperationState::Checkpoint { scope, .. }
            | OperationState::AssistantReady { scope, .. }
            | OperationState::AssistantEffectPending { scope, .. }
            | OperationState::AssistantRetryWait { scope, .. }
            | OperationState::Tools { scope, .. }
            | OperationState::SummaryDeciding { scope, .. }
            | OperationState::SummaryReady { scope, .. }
            | OperationState::SummaryEffectPending { scope, .. }
            | OperationState::SummaryRetryWait { scope, .. }
            | OperationState::NavigationReadyToCommit { scope, .. } => scope,
            OperationState::DeferredSuspended { scope: _ }
            | OperationState::DeferredEffectPending { scope: _, .. } => {
                // DeferredScope 与 OperationScope 字段同名，此处借用 scope 的公共字段。
                unreachable!("deferred scope handled separately")
            }
        }
    }

    pub fn at(&self) -> &'static str {
        match self {
            OperationState::Starting { .. } => "starting",
            OperationState::Checkpoint { .. } => "checkpoint",
            OperationState::AssistantReady { .. } => "assistant.ready",
            OperationState::AssistantEffectPending { .. } => "assistant.effect_pending",
            OperationState::AssistantRetryWait { .. } => "assistant.retry_wait",
            OperationState::Tools { .. } => "tools",
            OperationState::DeferredSuspended { .. } => "deferred.suspended",
            OperationState::DeferredEffectPending { .. } => "deferred.effect_pending",
            OperationState::SummaryDeciding { .. } => "summary.deciding",
            OperationState::SummaryReady { .. } => "summary.ready",
            OperationState::SummaryEffectPending { .. } => "summary.effect_pending",
            OperationState::SummaryRetryWait { .. } => "summary.retry_wait",
            OperationState::NavigationReadyToCommit { .. } => "navigation.ready_to_commit",
        }
    }
}

/// 对应 `Operation = { meta, state }`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub meta: OperationMeta,
    pub state: OperationState,
}

/// 对应 `LaneState`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneState {
    pub current_operation_id: Option<String>,
    pub last_operation_id: Option<String>,
    pub inbox: Vec<InboxItem>,
}

/// 对应 `PendingEntry`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum PendingEntry {
    Message {
        payload: AgentMessage,
    },
    Custom {
        custom_type: String,
        payload: Option<Json>,
    },
}

/// 对应 `DurableFileOperations`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableFileOperations {
    pub read: Vec<String>,
    pub written: Vec<String>,
    pub edited: Vec<String>,
}

/// 对应 `DurableStructuralPreparation`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableStructuralPreparation {
    Compaction {
        messages_to_summarize: Vec<AgentMessage>,
        turn_prefix_messages: Vec<AgentMessage>,
        retained_tail: Vec<AgentMessage>,
        is_split_turn: bool,
        tokens_before: u64,
        previous_summary: Option<String>,
        file_ops: DurableFileOperations,
        settings: CompactionSettings,
    },
    BranchSummary {
        messages: Vec<AgentMessage>,
        file_ops: DurableFileOperations,
        total_tokens: u64,
    },
}

/// 对应 `UsageRow`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub id: String,
    pub seq: u64,
    pub usage: Usage,
    pub entry_id: Option<String>,
    pub adjustment: bool,
    pub details: Option<Json>,
}

/// 对应 `EntryWrite`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryWrite {
    pub kind: WriteKind,
    pub entry: NewEntry,
}

/// 对应 `UsageWrite`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWrite {
    pub kind: WriteKind,
    pub row: UsageRow,
}

/// 对应 `Write.kind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    Entry,
    Usage,
    Value,
    List,
}

/// 对应 `Write`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Write {
    Entry(EntryWrite),
    Usage(UsageWrite),
    Value(ValueWrite),
    List(ListWrite),
}

/// 对应 `CommitResult`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitResult {
    pub first_seq: u64,
    pub seqs: Vec<u64>,
    pub timestamp: u64,
    pub stats: SessionStats,
}

/// 对应 `EntryStructure`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryStructure {
    pub id: String,
    pub parent_id: Option<String>,
    pub seq: u64,
    pub timestamp: u64,
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    pub custom_type: Option<String>,
}

/// 对应 `EntryCursor`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryCursor {
    pub seq: u64,
}

/// 对应 `BranchScan`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchScan {
    pub start: Option<String>,
    pub stop_at_type: Option<EntryType>,
    pub stop_at_id: Option<String>,
    #[serde(rename = "type")]
    pub entry_type: Option<EntryType>,
    pub custom_type: Option<String>,
    pub order: Option<ScanOrder>,
    pub limit: Option<usize>,
    pub cursor: Option<EntryCursor>,
}

/// 对应 `order: "newestFirst" | "oldestFirst" | "asc" | "desc"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScanOrder {
    NewestFirst,
    OldestFirst,
    Asc,
    Desc,
}

/// 对应 `StorageBranchScan`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageBranchScan {
    #[serde(flatten)]
    pub scan: BranchScan,
    pub start: String,
}

/// 对应 `EntryScan`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryScan {
    #[serde(rename = "type")]
    pub entry_type: Option<EntryType>,
    pub custom_type: Option<String>,
    pub from_seq: Option<u64>,
    pub to_seq: Option<u64>,
    pub order: Option<ScanOrder>,
    pub limit: Option<usize>,
}

/// 对应 `UsageScan`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageScan {
    pub from_seq: Option<u64>,
    pub to_seq: Option<u64>,
    pub order: Option<ScanOrder>,
    pub limit: Option<usize>,
}

/// 对应 `SessionStats`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub message_count: u64,
    pub usage: Usage,
}

/// 对应 `SessionMetadata`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    pub id: String,
    pub created_at: u64,
    pub storage_version: u32,
    pub cwd: Option<String>,
    pub parent_session_id: Option<String>,
    pub legacy_parent_session_path: Option<String>,
}

/// 对应 `IdGenerator`。
pub trait IdGenerator: Send + Sync {
    fn next(&self, timestamp_ms: Option<u64>) -> String;
}

/// 对应 `EntryQuery`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryQuery {
    #[serde(rename = "type")]
    pub entry_type: Option<EntryType>,
    pub custom_type: Option<String>,
    pub order: Option<ScanOrder>,
    pub limit: Option<usize>,
    pub cursor: Option<EntryCursor>,
}

/// 对应 `Storage`。
#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    async fn commit(&self, writes: Vec<Write>, context: &Context) -> Result<CommitResult, String>;
    async fn get_entries(
        &self,
        ids: &[String],
        context: &Context,
    ) -> Result<BTreeMap<String, Entry>, String>;
    async fn get_value(
        &self,
        address: &Value<Json>,
        context: &Context,
    ) -> Result<Option<StoredValue<Json>>, String>;
    async fn scan_values(
        &self,
        prefix: &Value<Json>,
        context: &Context,
    ) -> Result<Vec<StoredValue<Json>>, String>;
    async fn read_list(
        &self,
        address: &ValueList<Json>,
        options: Option<ListReadOptions>,
        context: &Context,
    ) -> Result<Vec<ListElement<Json>>, String>;
    async fn scan_branch(
        &self,
        query: StorageBranchScan,
        context: &Context,
    ) -> Result<Vec<Entry>, String>;
    async fn scan_branch_structure(
        &self,
        query: StorageBranchScan,
        context: &Context,
    ) -> Result<Vec<EntryStructure>, String>;
    async fn scan_entries(&self, query: EntryScan, context: &Context)
    -> Result<Vec<Entry>, String>;
    async fn scan_usage(
        &self,
        query: UsageScan,
        context: &Context,
    ) -> Result<Vec<UsageRow>, String>;
    async fn get_stats(&self, context: &Context) -> Result<SessionStats, String>;
    async fn close(&self, context: &Context) -> Result<(), String>;
}

/// 对应 `SessionReader`。
#[async_trait::async_trait]
pub trait SessionReader: Send + Sync {
    async fn get_entries(
        &self,
        ids: &[String],
        context: &Context,
    ) -> Result<BTreeMap<String, Entry>, String>;
    async fn get_stats(&self, context: &Context) -> Result<SessionStats, String>;
    async fn get_value(
        &self,
        address: &Value<Json>,
        context: &Context,
    ) -> Result<Option<StoredValue<Json>>, String>;
    async fn scan_values(
        &self,
        prefix: &Value<Json>,
        context: &Context,
    ) -> Result<Vec<StoredValue<Json>>, String>;
    async fn read_list(
        &self,
        address: &ValueList<Json>,
        options: Option<ListReadOptions>,
        context: &Context,
    ) -> Result<Vec<ListElement<Json>>, String>;
    async fn scan_branch(
        &self,
        query: StorageBranchScan,
        context: &Context,
    ) -> Result<Vec<Entry>, String>;
}

/// 对应 `SessionMutation`。
#[async_trait::async_trait]
pub trait SessionMutation: SessionReader {
    async fn commit(&self, writes: Vec<Write>, context: &Context) -> Result<CommitResult, String>;
    async fn end(&self, context: &Context) -> Result<(), String>;
}

/// 对应 `SessionMutationCallback`。
pub type SessionMutationCallback<T> = Arc<
    dyn Fn(
            Arc<dyn SessionMutation>,
            Context,
        ) -> Pin<Box<dyn Future<Output = Result<T, String>> + Send>>
        + Send
        + Sync,
>;

/// 对应 `Branch`。
#[async_trait::async_trait]
pub trait Branch: Send + Sync {
    fn name(&self) -> &str;
    async fn get_tip_id(&self, context: &Context) -> Result<Option<String>, String>;
    async fn find_entries(
        &self,
        query: Option<BranchScan>,
        context: &Context,
    ) -> Result<Vec<Entry>, String>;
    async fn find_entry(
        &self,
        query: Option<BranchScan>,
        context: &Context,
    ) -> Result<Option<Entry>, String>;
    async fn append_message(
        &self,
        message: AgentMessage,
        context: &Context,
    ) -> Result<String, String>;
    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<Json>,
        context: &Context,
    ) -> Result<String, String>;
}

/// 对应 `Session`。
#[async_trait::async_trait]
pub trait Session: SessionReader + Send + Sync {
    fn metadata(&self) -> &SessionMetadata;
    fn id_generator(&self) -> Arc<dyn IdGenerator>;
    async fn get_entry(&self, id: &str, context: &Context) -> Result<Option<Entry>, String>;
    async fn get_name(&self, context: &Context) -> Result<Option<String>, String>;
    async fn get_label(&self, target_id: &str, context: &Context)
    -> Result<Option<String>, String>;
    async fn find_entries(
        &self,
        query: Option<EntryQuery>,
        context: &Context,
    ) -> Result<Vec<Entry>, String>;
    async fn find_entry(
        &self,
        query: Option<EntryQuery>,
        context: &Context,
    ) -> Result<Option<Entry>, String>;
    async fn branch(
        &self,
        name: &str,
        context: &Context,
    ) -> Result<Option<Arc<dyn Branch>>, String>;
    async fn create_branch(
        &self,
        name: &str,
        at: Option<String>,
        context: &Context,
    ) -> Result<Arc<dyn Branch>, String>;
    async fn begin_mutation(&self, context: &Context) -> Result<Arc<dyn SessionMutation>, String>;
    async fn set_value(
        &self,
        address: &Value<Json>,
        next: Json,
        context: &Context,
    ) -> Result<(), String>;
    async fn delete_value(&self, address: &Value<Json>, context: &Context) -> Result<(), String>;
    async fn append_list(
        &self,
        address: &ValueList<Json>,
        element: Json,
        context: &Context,
    ) -> Result<(), String>;
    async fn delete_list(&self, address: &ValueList<Json>, context: &Context)
    -> Result<(), String>;
    async fn set_name(&self, name: Option<String>, context: &Context) -> Result<(), String>;
    async fn set_label(
        &self,
        target_id: &str,
        label: Option<String>,
        context: &Context,
    ) -> Result<(), String>;
    async fn close(&self, context: &Context) -> Result<(), String>;
}

/// 对应 `Session.mutate` 的 free function 版本（Rust 泛型方法不 dyn 兼容）。
pub async fn mutate<T: Send + 'static>(
    session: &dyn Session,
    mutation: SessionMutationCallback<T>,
    context: &Context,
) -> Result<T, String> {
    let mutator = session.begin_mutation(context).await?;
    let result = mutation(Arc::clone(&mutator), context.clone()).await?;
    let _ = mutator.end(context).await;
    Ok(result)
}

/// 对应 `SessionCreateOptions`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateOptions {
    pub id: Option<String>,
    pub parent_session_id: Option<String>,
}

/// 对应 `ForkOptions`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ForkOptions {
    Branch {
        branch: String,
        entry_id: Option<String>,
        position: Option<ForkPosition>,
        id: Option<String>,
    },
    Tree {
        id: Option<String>,
    },
}

/// 对应 `position: "before" | "at"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkPosition {
    Before,
    At,
}

/// 对应 `SessionRepo`。
#[async_trait::async_trait]
pub trait SessionRepo: Send + Sync {
    async fn create(
        &self,
        options: SessionCreateOptions,
        context: &Context,
    ) -> Result<Arc<dyn Session>, String>;
    async fn open(
        &self,
        metadata: SessionMetadata,
        context: &Context,
    ) -> Result<Arc<dyn Session>, String>;
    async fn list(&self, context: &Context) -> Result<Vec<SessionMetadata>, String>;
    async fn delete(&self, metadata: SessionMetadata, context: &Context) -> Result<(), String>;
    async fn fork(
        &self,
        source: SessionMetadata,
        options: ForkOptions,
        context: &Context,
    ) -> Result<Arc<dyn Session>, String>;
}
