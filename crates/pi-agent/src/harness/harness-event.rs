//! 对应 packages/agent/src/harness/agent-harness.ts 里的 `HarnessEvent`/`LaneSnapshot`/
//! `SessionSnapshot`/`LaneQueuedItem` 等强类型（事件 + 快照）。

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use pi_ai::utils::assistant_message_frame::AssistantMessageFrame;
use pi_ai::utils::retry::RetryPolicy;
use pi_ai::{AssistantMessage, AssistantMessageEvent, DeferredHandle, ToolResultMessage, Usage};

use crate::harness::compaction::compaction::CompactionSettings;
use crate::harness::session::types::{
    Entry, LaneConfiguration, ModelIdentity, OperationError, SessionStats, TerminalStatus, UsageRow,
};
use crate::harness::types::AgentHarnessStreamOptions;
use crate::types::{AgentMessage, AgentToolResult, QueueMode, ThinkingLevel};

/// 对应 `LaneQueuedItem`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum LaneQueuedItem {
    Message {
        entry_id: String,
        kind: crate::harness::session::types::InboxItemKind,
        message: AgentMessage,
    },
    Custom {
        entry_id: String,
        kind: crate::harness::session::types::InboxItemKind,
        custom_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Json>,
    },
}

/// 对应 `LaneSnapshotTool`（runningTools 的元素）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum LaneSnapshotTool {
    Running {
        tool_call_id: String,
        tool_name: String,
        args: Json,
        result: Option<AgentToolResult>,
    },
    Settled {
        tool_call_id: String,
        tool_name: String,
        args: Json,
        result: AgentToolResult,
        is_error: bool,
    },
}

/// 对应 `LaneSnapshot.operation`（内联对象）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneOperationSnapshot {
    pub id: String,
    pub kind: String,
    pub started_at: u64,
    pub from_tip_id: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming_message: Option<AssistantMessage>,
    pub running_tools: Vec<LaneSnapshotTool>,
}

/// 对应 `LaneSnapshot.operation.retry`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrySnapshot {
    pub attempt: u32,
    pub max_attempts: u32,
    pub next_attempt_at: u64,
}

/// 对应 `LaneSnapshot.operation.deferred`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredSnapshot {
    pub handle: DeferredHandle,
    pub poll: u64,
}

/// 对应 `LaneSnapshot`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneSnapshot {
    pub lane: String,
    pub transcript: Vec<Entry>,
    pub tip_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_result: Option<crate::harness::session::types::OperationResultRecord>,
    pub configuration: LaneConfiguration,
    pub stats: SessionStats,
    pub operation: Option<LaneOperationSnapshot>,
    pub queues: Vec<LaneQueuedItem>,
    pub faulted: bool,
}

/// 对应 `SessionSnapshot`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub lanes: Vec<crate::harness::agent_harness::LaneInfo>,
    pub faulted: bool,
}

/// 对应 `value_update` 的子联合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "value", rename_all = "snake_case")]
pub enum ValueUpdatePayload {
    SessionName {
        name: Option<String>,
    },
    EntryLabel {
        target_id: String,
        label: Option<String>,
    },
}

/// 对应 `config_update` 的子联合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "property", rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)]
pub enum ConfigUpdatePayload {
    Model {
        value: ModelIdentity,
        previous: Json,
    },
    ThinkingLevel {
        value: ThinkingLevel,
        previous: ThinkingLevel,
    },
    ActiveTools {
        value: Vec<String>,
        previous: Vec<String>,
    },
    Tools,
    Resources,
    StreamOptions {
        value: AgentHarnessStreamOptions,
        previous: AgentHarnessStreamOptions,
    },
    RetryPolicy {
        value: RetryPolicy,
        previous: RetryPolicy,
    },
    CompactionSettings {
        value: CompactionSettings,
        previous: CompactionSettings,
    },
    SteeringMode {
        value: QueueMode,
        previous: QueueMode,
    },
    FollowUpMode {
        value: QueueMode,
        previous: QueueMode,
    },
}

/// 对应 `handler_error` 的 `kind` 子联合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandlerErrorKind {
    Hook { hook: String },
    Event { event: String },
}

/// 对应 `HarnessEvent`（29 种强类型事件）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum HarnessEvent {
    #[serde(rename_all = "camelCase")]
    RunStart {
        lane: String,
        run_id: String,
        started_at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    RunResume {
        lane: String,
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    RunSuspend {
        lane: String,
        run_id: String,
        reason: String,
        deferred: DeferredHandle,
        poll: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    OperationAbort {
        lane: String,
        operation_id: String,
        steer: Vec<AgentMessage>,
        follow_up: Vec<AgentMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    RunEnd {
        lane: String,
        run_id: String,
        from_tip_id: Option<String>,
        tip_id: Option<String>,
        ended_at: u64,
        status: TerminalStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<OperationError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    Fault { code: String, message: String },
    #[serde(rename_all = "camelCase")]
    HandlerError {
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stack: Option<String>,
        #[serde(flatten)]
        kind: HandlerErrorKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lane: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    TurnStart {
        lane: String,
        run_id: String,
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    TurnEnd {
        lane: String,
        run_id: String,
        turn_id: String,
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    RetryScheduled {
        lane: String,
        run_id: String,
        step: String,
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        not_before: u64,
        error_message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    RetryStart {
        lane: String,
        run_id: String,
        step: String,
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    RetryEnd {
        lane: String,
        run_id: String,
        step: String,
        attempt: u32,
        success: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    MessageStart {
        lane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        message: AgentMessage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    MessageUpdate {
        lane: String,
        run_id: String,
        message: AgentMessage,
        event: AssistantMessageEvent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<AssistantMessageFrame>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    MessageEnd {
        lane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        message: AgentMessage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    ToolStart {
        lane: String,
        run_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        args: Json,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    ToolUpdate {
        lane: String,
        run_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        partial_result: AgentToolResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    ToolEnd {
        lane: String,
        run_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
        terminate: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    EntryAdded {
        lane: String,
        entry: Entry,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    QueueUpdate {
        lane: String,
        queues: Vec<LaneQueuedItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    ValueUpdate {
        #[serde(flatten)]
        payload: ValueUpdatePayload,
    },
    #[serde(rename_all = "camelCase")]
    ConfigUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lane: Option<String>,
        #[serde(flatten)]
        payload: ConfigUpdatePayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    CompactionStart {
        lane: String,
        run_id: String,
        reason: String,
        started_at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    CompactionEnd {
        lane: String,
        run_id: String,
        reason: String,
        ended_at: u64,
        status: TerminalStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<OperationError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    NavigationStart {
        lane: String,
        run_id: String,
        target_id: Option<String>,
        started_at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    NavigationEnd {
        lane: String,
        run_id: String,
        from_tip_id: Option<String>,
        tip_id: Option<String>,
        ended_at: u64,
        status: TerminalStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<OperationError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    LaneCreated {
        lane: String,
        at: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recovery: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    Usage {
        lane: String,
        row: UsageRow,
        totals: Usage,
    },
}

impl HarnessEvent {
    /// 对应 `event.type`。
    pub fn event_type(&self) -> &'static str {
        match self {
            HarnessEvent::RunStart { .. } => "run_start",
            HarnessEvent::RunResume { .. } => "run_resume",
            HarnessEvent::RunSuspend { .. } => "run_suspend",
            HarnessEvent::OperationAbort { .. } => "operation_abort",
            HarnessEvent::RunEnd { .. } => "run_end",
            HarnessEvent::Fault { .. } => "fault",
            HarnessEvent::HandlerError { .. } => "handler_error",
            HarnessEvent::TurnStart { .. } => "turn_start",
            HarnessEvent::TurnEnd { .. } => "turn_end",
            HarnessEvent::RetryScheduled { .. } => "retry_scheduled",
            HarnessEvent::RetryStart { .. } => "retry_start",
            HarnessEvent::RetryEnd { .. } => "retry_end",
            HarnessEvent::MessageStart { .. } => "message_start",
            HarnessEvent::MessageUpdate { .. } => "message_update",
            HarnessEvent::MessageEnd { .. } => "message_end",
            HarnessEvent::ToolStart { .. } => "tool_start",
            HarnessEvent::ToolUpdate { .. } => "tool_update",
            HarnessEvent::ToolEnd { .. } => "tool_end",
            HarnessEvent::EntryAdded { .. } => "entry_added",
            HarnessEvent::QueueUpdate { .. } => "queue_update",
            HarnessEvent::ValueUpdate { .. } => "value_update",
            HarnessEvent::ConfigUpdate { .. } => "config_update",
            HarnessEvent::CompactionStart { .. } => "compaction_start",
            HarnessEvent::CompactionEnd { .. } => "compaction_end",
            HarnessEvent::NavigationStart { .. } => "navigation_start",
            HarnessEvent::NavigationEnd { .. } => "navigation_end",
            HarnessEvent::LaneCreated { .. } => "lane_created",
            HarnessEvent::Usage { .. } => "usage",
        }
    }

    /// 提取事件的 lane（若有）。对应原版里 LaneEvent 的 `lane` 字段。
    pub fn lane(&self) -> Option<&str> {
        match self {
            HarnessEvent::RunStart { lane, .. }
            | HarnessEvent::RunResume { lane, .. }
            | HarnessEvent::RunSuspend { lane, .. }
            | HarnessEvent::OperationAbort { lane, .. }
            | HarnessEvent::RunEnd { lane, .. }
            | HarnessEvent::TurnStart { lane, .. }
            | HarnessEvent::TurnEnd { lane, .. }
            | HarnessEvent::RetryScheduled { lane, .. }
            | HarnessEvent::RetryStart { lane, .. }
            | HarnessEvent::RetryEnd { lane, .. }
            | HarnessEvent::MessageStart { lane, .. }
            | HarnessEvent::MessageUpdate { lane, .. }
            | HarnessEvent::MessageEnd { lane, .. }
            | HarnessEvent::ToolStart { lane, .. }
            | HarnessEvent::ToolUpdate { lane, .. }
            | HarnessEvent::ToolEnd { lane, .. }
            | HarnessEvent::EntryAdded { lane, .. }
            | HarnessEvent::QueueUpdate { lane, .. }
            | HarnessEvent::CompactionStart { lane, .. }
            | HarnessEvent::CompactionEnd { lane, .. }
            | HarnessEvent::NavigationStart { lane, .. }
            | HarnessEvent::NavigationEnd { lane, .. }
            | HarnessEvent::LaneCreated { lane, .. }
            | HarnessEvent::Usage { lane, .. } => Some(lane),
            HarnessEvent::ConfigUpdate { lane, .. } | HarnessEvent::HandlerError { lane, .. } => {
                lane.as_deref()
            }
            HarnessEvent::Fault { .. } | HarnessEvent::ValueUpdate { .. } => None,
        }
    }
}
