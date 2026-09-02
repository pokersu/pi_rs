//! Rust 翻译自 packages/agent/src/harness/session/types.ts
//!
//! session 持久化的类型基础：Entry / LaneRecord 系统、查询类型、存储 trait。

use pi_ai::Usage;
use serde::{Deserialize, Serialize};

use crate::types::AgentMessage;

/// 对应 `SessionStopReason`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

/// 对应 `EntryBase`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryBase {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub seq: u64,
    pub parent_id: Option<String>,
    pub timestamp: u64,
}

/// 对应 `MessageEntry`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "message", rename_all = "camelCase")]
pub struct MessageEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub message: AgentMessage,
    pub terminate: Option<bool>,
}

/// 对应 `ModelChangeEntry`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "model_change", rename_all = "camelCase")]
pub struct ModelChangeEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub provider: String,
    pub model_id: String,
}

/// 对应 `ThinkingLevelEntry`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename = "thinking_level_change",
    rename_all = "camelCase"
)]
pub struct ThinkingLevelEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub thinking_level: String,
}

/// 对应 `ActiveToolsEntry`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "active_tools_change", rename_all = "camelCase")]
pub struct ActiveToolsEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub active_tool_names: Vec<String>,
}

/// 对应 `CompactionEntry`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "compaction", rename_all = "camelCase")]
pub struct CompactionEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub summary: String,
    pub retained_tail: Vec<AgentMessage>,
    pub tokens_before: u64,
    pub details: Option<serde_json::Value>,
    pub usage: Option<Usage>,
}

/// 对应 `BranchSummaryEntry`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "branch_summary", rename_all = "camelCase")]
pub struct BranchSummaryEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub from_id: String,
    pub summary: String,
    pub details: Option<serde_json::Value>,
    pub usage: Option<Usage>,
}

/// 对应 `CustomEntry`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "custom", rename_all = "camelCase")]
pub struct CustomEntry {
    #[serde(flatten)]
    pub base: EntryBase,
    pub custom_type: String,
    pub data: Option<serde_json::Value>,
}

/// 对应 `Entry`
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Entry {
    Message(MessageEntry),
    ModelChange(ModelChangeEntry),
    ThinkingLevelChange(ThinkingLevelEntry),
    ActiveToolsChange(ActiveToolsEntry),
    Compaction(CompactionEntry),
    BranchSummary(BranchSummaryEntry),
    Custom(CustomEntry),
}

impl Entry {
    pub fn id(&self) -> &str {
        match self {
            Entry::Message(e) => &e.base.id,
            Entry::ModelChange(e) => &e.base.id,
            Entry::ThinkingLevelChange(e) => &e.base.id,
            Entry::ActiveToolsChange(e) => &e.base.id,
            Entry::Compaction(e) => &e.base.id,
            Entry::BranchSummary(e) => &e.base.id,
            Entry::Custom(e) => &e.base.id,
        }
    }
}

/// 对应 `RecordBase`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordBase {
    pub id: String,
    pub seq: u64,
    pub lane: String,
    pub timestamp: u64,
}

/// 对应 `OperationStartedRecord.intent`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum OperationIntent {
    Run {
        original_prompt: Vec<AgentMessage>,
        initial_messages: Vec<serde_json::Value>,
        system_prompt_override: Option<String>,
        resume_data: Option<serde_json::Value>,
    },
    Compaction {
        custom_instructions: Option<String>,
        result_entry_id: String,
    },
    Navigation {
        target_id: Option<String>,
        summarize: bool,
        custom_instructions: Option<String>,
        label: Option<String>,
        summary_entry_id: Option<String>,
    },
}

/// 对应 `OperationStartedRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "operation_started", rename_all = "camelCase")]
pub struct OperationStartedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub source_leaf_id: Option<String>,
    pub intent: OperationIntent,
}

impl OperationStartedRecord {
    pub fn kind_str(&self) -> String {
        match &self.intent {
            OperationIntent::Run { .. } => "run",
            OperationIntent::Compaction { .. } => "compaction",
            OperationIntent::Navigation { .. } => "navigation",
        }
        .to_string()
    }
}

/// 对应 `AbortRequestedRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "abort_requested", rename_all = "camelCase")]
pub struct AbortRequestedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
}

/// 对应 `OperationFinishedRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "operation_finished", rename_all = "camelCase")]
pub struct OperationFinishedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    pub outcome: String,
    pub error: Option<serde_json::Value>,
}

/// 对应 `CompactionReason`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// 对应 `StepAttemptRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "step_attempt", rename_all = "camelCase")]
pub struct StepAttemptRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    pub step: String,
    pub attempt: u32,
    pub result_entry_id: String,
    pub compaction_reason: Option<CompactionReason>,
}

/// 对应 `ToolStartedRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "tool_started", rename_all = "camelCase")]
pub struct ToolStartedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    pub assistant_entry_id: String,
    pub tool_index: u32,
    pub tool_call_id: String,
    pub tool_name: String,
    pub effective_args: serde_json::Value,
    pub result_entry_id: String,
    pub replay: String,
}

/// 对应 `QueueEnqueuedRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "queue_enqueued", rename_all = "camelCase")]
pub struct QueueEnqueuedRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub queue: String,
    pub run_id: Option<String>,
    pub target: serde_json::Value,
}

/// 对应 `QueueCancelledRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "queue_cancelled", rename_all = "camelCase")]
pub struct QueueCancelledRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: Option<String>,
    pub entry_id: String,
}

/// 对应 `WriteDeferredRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "write_deferred", rename_all = "camelCase")]
pub struct WriteDeferredRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub run_id: String,
    pub target: serde_json::Value,
}

/// 对应 `UsageRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "usage", rename_all = "camelCase")]
pub struct UsageRecord {
    #[serde(flatten)]
    pub base: RecordBase,
    pub usage: Usage,
    pub cause: String,
    pub run_id: Option<String>,
    pub entry_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub attempt: Option<u32>,
    pub stop_reason: Option<SessionStopReason>,
    pub details: Option<serde_json::Value>,
}

/// 对应 `LaneRecord`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum LaneRecord {
    OperationStarted(OperationStartedRecord),
    AbortRequested(AbortRequestedRecord),
    OperationFinished(OperationFinishedRecord),
    StepAttempt(StepAttemptRecord),
    ToolStarted(ToolStartedRecord),
    QueueEnqueued(QueueEnqueuedRecord),
    QueueCancelled(QueueCancelledRecord),
    WriteDeferred(WriteDeferredRecord),
    Usage(UsageRecord),
}

/// 对应 `EntryOrder`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryOrder {
    NewestFirst,
    OldestFirst,
}

/// 对应 `EntryCursor`
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct EntryCursor {
    pub after_seq: u64,
}

/// 对应 `EntryQuery`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntryQuery {
    pub kind: Option<String>,
    pub custom_type: Option<String>,
    pub order: Option<EntryOrder>,
    pub limit: Option<usize>,
    pub cursor: Option<EntryCursor>,
}

/// 对应 `BranchBounds`：分支扫描的边界。默认：整个路径（叶到根）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchBounds {
    /// 默认：视图所在 lane 的叶子。
    pub start: Option<String>,
    /// 扫描在首个匹配 type 后停止（包含该 entry）。
    pub stop_at_type: Option<String>,
    /// 扫描到该 id 后停止（包含该 entry）。
    pub stop_at_id: Option<String>,
}

/// 对应 `RecordQuery`
#[derive(Debug, Clone, Default)]
pub struct RecordQuery {
    pub lane: Option<String>,
    pub kind: Option<String>,
    pub run_id: Option<String>,
    pub operation_kind: Option<String>,
    pub after_seq: Option<u64>,
    pub order: Option<EntryOrder>,
    pub limit: Option<usize>,
}

/// 对应 `SessionMetadata`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub id: String,
    pub created_at: u64,
    pub parent_session_id: Option<String>,
}

/// 对应 `SessionStats`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    pub message_count: u64,
    pub cached_tokens: u64,
    pub uncached_tokens: u64,
    pub total_tokens: u64,
    pub cost_total: f64,
}

/// 对应 `LanePointer`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanePointer {
    pub lane: String,
    pub leaf_id: Option<String>,
}

/// 对应 `LogItem`
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LogItem {
    Entry {
        seq: u64,
        entry: Entry,
    },
    Record {
        seq: u64,
        record: LaneRecord,
    },
    Lane {
        seq: u64,
        lane: String,
        leaf_id: Option<String>,
    },
    FactName {
        seq: u64,
        name: Option<String>,
    },
    FactLabel {
        seq: u64,
        target_id: String,
        label: Option<String>,
    },
}

/// 对应 `SessionErrorCode`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionErrorCode {
    NotFound,
    AlreadyExists,
    InvalidEntry,
    InvalidPayload,
    InvalidLane,
    InvalidQuery,
    InvalidForkTarget,
    Storage,
}

/// 对应 `SessionError`
#[derive(Debug, Clone)]
pub struct SessionError {
    pub code: SessionErrorCode,
    pub message: String,
}

impl SessionError {
    pub fn new(code: SessionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SessionError {}

/// 对应 `ForkPosition`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    Before,
    At,
}

/// 对应 `ForkOptions`（默认 `scope` 为 `branch`）。
#[derive(Debug, Clone)]
pub enum ForkOptions {
    Branch {
        entry_id: Option<String>,
        position: Option<ForkPosition>,
    },
    Tree,
}

impl Default for ForkOptions {
    fn default() -> Self {
        ForkOptions::Branch {
            entry_id: None,
            position: None,
        }
    }
}

/// 对应 `SessionStorage`（简化：去掉泛型元数据参数）。
#[async_trait::async_trait]
pub trait SessionStorage: Send + Sync {
    async fn get_metadata(&self) -> Result<SessionMetadata, SessionError>;
    async fn get_lanes(&self) -> Result<Vec<LanePointer>, SessionError>;
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> Result<(), SessionError>;
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> Result<(), SessionError>;
    async fn append_entry(&self, entry: Entry, lane: &str) -> Result<Entry, SessionError>;
    async fn append_record(&self, record: LaneRecord) -> Result<LaneRecord, SessionError>;
    async fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError>;
    async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError>;
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        start: &str,
        stop_at_type: Option<&str>,
        stop_at_id: Option<&str>,
    ) -> Result<Vec<Entry>, SessionError>;
    async fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError>;
    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<OperationStartedRecord>, SessionError>;
    async fn get_log(
        &self,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Vec<LogItem>, SessionError>;
    async fn get_name(&self) -> Result<Option<String>, SessionError>;
    async fn set_name(&self, name: Option<&str>) -> Result<(), SessionError>;
    async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError>;
    async fn set_label(&self, id: &str, label: Option<&str>) -> Result<(), SessionError>;
    async fn get_stats(&self) -> Result<SessionStats, SessionError>;
}

/// 对应 `SessionTree`（简化）。
#[async_trait::async_trait]
pub trait SessionTree: Send + Sync {
    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError>;
    async fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError>;
    async fn get_stats(&self) -> Result<SessionStats, SessionError>;
    async fn get_name(&self) -> Result<Option<String>, SessionError>;
    async fn set_name(&self, name: Option<&str>) -> Result<(), SessionError>;
    async fn get_label(&self, target_id: &str) -> Result<Option<String>, SessionError>;
    async fn set_label(&self, target_id: &str, label: Option<&str>) -> Result<(), SessionError>;
    async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError>;
    async fn find_entry(&self, query: &EntryQuery) -> Result<Option<Entry>, SessionError>;
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError>;
    async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Option<Entry>, SessionError>;
    async fn append_message(&self, message: AgentMessage) -> Result<String, SessionError>;
    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError>;
}
