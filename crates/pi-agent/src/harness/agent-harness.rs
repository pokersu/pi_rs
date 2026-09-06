//! Rust 翻译自 packages/agent/src/harness/agent-harness.ts
//!
//! AgentHarness 整合层：类型定义（AgentLane / AgentHarnessApi / 事件等）与
//! 运行时接口。具体运行时实现在 `runtime/harness.rs`（`Harness` 类）与
//! `runtime/lane.rs`（`LaneImpl`）。

use pi_ai::{DeferredHandle, ImageContent, Model, Usage};

use crate::types::{AgentMessage, ThinkingLevel};

// 对应各 `TaggedError` 派生错误类（LaneBusy/MissingIdentities/... 等）。
crate::tagged_error!(LaneBusy, "LaneBusy", {
    lane: String,
    operation_id: String,
    operation_kind: String,
    message: String,
});
crate::tagged_error!(OperationMismatch, "OperationMismatch", {
    lane: String,
    expected_operation_id: String,
    current_operation_id: Option<String>,
    last_operation_id: Option<String>,
    message: String,
});
crate::tagged_error!(NoActiveRun, "NoActiveRun", { lane: String, message: String });
crate::tagged_error!(NoActiveOperation, "NoActiveOperation", { lane: String, message: String });
crate::tagged_error!(NothingToResume, "NothingToResume", { lane: String, message: String });
crate::tagged_error!(InvalidMessage, "InvalidMessage", { lane: String, reason: String, message: String });
crate::tagged_error!(InvalidNavigation, "InvalidNavigation", { lane: String, reason: String, message: String });
crate::tagged_error!(UnknownSkill, "UnknownSkill", { name: String, message: String });
crate::tagged_error!(UnknownTemplate, "UnknownTemplate", { name: String, message: String });
crate::tagged_error!(UnknownTarget, "UnknownTarget", { target_id: String, message: String });
crate::tagged_error!(InvalidLane, "InvalidLane", { lane: String, reason: String, message: String });
crate::tagged_error!(NothingToCompact, "NothingToCompact", { lane: String, message: String });
crate::tagged_error!(Closed, "Closed", { message: String });

/// 对应各错误联合类型（`RunRejected`/`CompactionRejected`/... 等）。
#[derive(Debug, Clone)]
pub enum HarnessError {
    LaneBusy(LaneBusy),
    OperationMismatch(OperationMismatch),
    NoActiveRun(NoActiveRun),
    NoActiveOperation(NoActiveOperation),
    NothingToResume(NothingToResume),
    NothingToCompact(NothingToCompact),
    InvalidMessage(InvalidMessage),
    InvalidNavigation(InvalidNavigation),
    UnknownSkill(UnknownSkill),
    UnknownTemplate(UnknownTemplate),
    UnknownTarget(UnknownTarget),
    InvalidLane(InvalidLane),
    Closed(Closed),
}

macro_rules! harness_error_delegate {
    ($($variant:ident),* $(,)?) => {
        impl std::fmt::Display for HarnessError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $(HarnessError::$variant(e) => std::fmt::Display::fmt(e, f),)*
                }
            }
        }
        impl std::error::Error for HarnessError {}
        impl crate::harness::result::TaggedError for HarnessError {
            fn tag(&self) -> &'static str {
                match self {
                    $(HarnessError::$variant(e) => crate::harness::result::TaggedError::tag(e),)*
                }
            }
            fn to_json(&self) -> serde_json::Value {
                match self {
                    $(HarnessError::$variant(e) => crate::harness::result::TaggedError::to_json(e),)*
                }
            }
        }
    };
}

harness_error_delegate!(
    LaneBusy,
    OperationMismatch,
    NoActiveRun,
    NoActiveOperation,
    NothingToResume,
    NothingToCompact,
    InvalidMessage,
    InvalidNavigation,
    UnknownSkill,
    UnknownTemplate,
    UnknownTarget,
    InvalidLane,
    Closed,
);

/// 对应 `OperationError`
#[derive(Debug, Clone)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

/// 对应 `SuspendedRun`。
#[derive(Debug, Clone, PartialEq)]
pub struct SuspendedRun {
    pub operation_id: String,
    pub deferred: DeferredHandle,
}

/// 对应 `RunResult` 的 Ok 联合：settled 或 suspended。
#[derive(Debug, Clone)]
pub enum RunSettlement {
    Settled(OperationResultRecord),
    Suspended(SuspendedRun),
}

pub type RunResult = Result<RunSettlement, HarnessError>;

/// 对应 `CompactionResult` 的 Ok。
#[derive(Debug, Clone)]
pub struct CompactionSettlement {
    pub compaction: OperationResultRecord,
    pub run: Option<RunSettlement>,
}
pub type CompactionResult = Result<CompactionSettlement, HarnessError>;

/// 对应 `NavigationResult` 的 Ok。
#[derive(Debug, Clone)]
pub struct NavigationSettlement {
    pub navigation: OperationResultRecord,
    pub run: Option<RunSettlement>,
}
pub type NavigationResult = Result<NavigationSettlement, HarnessError>;

pub type ResumeResult = RunResult;

/// 对应 `QueueResult` 的 Ok。
#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub entry_id: String,
}
pub type QueueResult = Result<QueueEntry, HarnessError>;

/// 对应 `CancelQueuedResult` 的 Ok。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelQueuedKind {
    Cancelled,
    AlreadyConsumed,
    NotFound,
}
pub type CancelQueuedResult = Result<CancelQueuedKind, HarnessError>;

/// 对应 `AbortResult` 的 Ok。
#[derive(Debug, Clone)]
pub struct AbortOutcome {
    pub operation_id: String,
    pub steer: Vec<AgentMessage>,
    pub follow_up: Vec<AgentMessage>,
}
pub type AbortResult = Result<AbortOutcome, HarnessError>;

/// 对应 `RecordUsageResult` 的 Ok。
#[derive(Debug, Clone)]
pub struct RecordUsageOutcome {
    pub usage_id: String,
}
pub type RecordUsageResult = Result<RecordUsageOutcome, HarnessError>;

/// 对应 `NavigateOptions`
#[derive(Debug, Clone, Default)]
pub struct NavigateOptions {
    pub summarize: Option<bool>,
    pub custom_instructions: Option<String>,
    pub label: Option<String>,
}

/// 对应 `LaneInfo`
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneInfo {
    pub name: String,
    pub leaf_id: Option<String>,
    pub operation: Option<LaneOperationInfo>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneOperationInfo {
    pub id: String,
    pub kind: String,
    pub status: String,
}

/// 对应 `QueuedItem`
#[derive(Debug, Clone)]
pub struct QueuedItem {
    pub entry_id: String,
    pub message: AgentMessage,
}

pub use crate::harness::harness_event::{LaneQueuedItem, LaneSnapshot, SessionSnapshot};

/// 对应 `ActionInfo`
#[derive(Debug, Clone)]
pub enum ActionInfo {
    AppendEntry {
        entry_type: String,
        entry_id: String,
    },
    AppendRecord {
        record_type: String,
    },
    MoveLane {
        to: Option<String>,
    },
    SetFact {
        fact: String,
    },
    FinishRun {
        outcome: String,
    },
    FinishOperation {
        outcome: String,
    },
    ConsumeQueueItem {
        queue: String,
        entry_id: String,
    },
    StreamAssistant {
        step: String,
        attempt: u32,
    },
    ExecuteTool {
        tool_call_id: String,
        tool_name: String,
    },
    FetchDeferred {
        provider: String,
        id: String,
    },
    CancelDeferred {
        provider: String,
        id: String,
    },
    Hook {
        name: String,
    },
    Sleep {
        delay_ms: u64,
    },
}

// ---------------------------------------------------------------------------
// R1 接口类型（runtime 驱动依赖，后续逐步对齐原版完整定义）。
// ---------------------------------------------------------------------------

use serde_json::Value as Json;

use crate::harness::session::types::{BranchScan, Entry, OperationResultRecord};
use crate::harness::types::{PromptTemplate, Skill};

/// 对应 `ModelIdentity`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelIdentity {
    pub provider: String,
    pub model_id: String,
}

/// 对应 `DriveOptions`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriveOptions {
    pub operation_id: String,
    pub wait_for_retry: bool,
    pub poll_deferred: bool,
}

/// 对应 `DriveOutcome`。
#[derive(Debug, Clone, PartialEq)]
pub enum DriveOutcome {
    Settled {
        outcome: OperationResultRecord,
    },
    WaitingRetry {
        operation_id: String,
        not_before: u64,
    },
    WaitingDeferred {
        operation_id: String,
        deferred: DeferredHandle,
    },
}

/// 对应 `DriveResult`。
pub type DriveResult = Result<DriveOutcome, HarnessError>;

/// 对应 `OperationStatus`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Running,
    Open,
    Aborting,
}

/// 对应 `CurrentOperationInfo`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentOperationInfo {
    pub id: String,
    pub kind: String,
    pub started_at: u64,
    pub status: OperationStatus,
    pub captured_model: Option<ModelIdentity>,
}

/// 对应 `OpenOperation`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOperation {
    pub lane: String,
    pub operation_id: String,
    pub kind: String,
    pub started_at: u64,
    pub aborting: Option<bool>,
}

/// 对应 `OperationRequest`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationRequest {
    Prompt {
        operation_id: Option<String>,
        prompt: Json,
        images: Option<Vec<ImageContent>>,
    },
    Skill {
        operation_id: Option<String>,
        name: String,
        additional_instructions: Option<String>,
    },
    PromptTemplate {
        operation_id: Option<String>,
        name: String,
        args: Option<Vec<String>>,
    },
    Compaction {
        operation_id: Option<String>,
        custom_instructions: Option<String>,
    },
    Navigation {
        operation_id: Option<String>,
        target_id: Option<String>,
        summarize: bool,
        label: Option<String>,
        custom_instructions: Option<String>,
    },
}

/// 对应 `OperationAdmission`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationAdmission {
    pub operation_id: String,
    pub kind: String,
    pub started_at: u64,
}

/// 对应 `Resources`。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    pub skills: Vec<Skill>,
    pub prompt_templates: Vec<PromptTemplate>,
}

/// 对应 `HarnessEvent`（29 种强类型事件，见 harness-event.rs）。
pub use crate::harness::harness_event::HarnessEvent;

/// 对应 `EventListener`（同步或异步事件回调，附带 context）。
pub type HarnessEventListener = std::sync::Arc<
    dyn Fn(
            HarnessEvent,
            crate::harness::context::Context,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// 对应 `WatchHandle<T>`：快照订阅句柄。
pub trait WatchHandle<T>: Send + Sync
where
    T: Clone + Send + Sync,
{
    fn snapshot(&self) -> T;
    fn start(&self, listener: HarnessEventListener);
    fn resnapshot<'a>(
        &'a self,
        context: &'a crate::harness::context::Context,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send + 'a>>;
    fn unsubscribe(&self);
}

/// 对应 `AgentLane`（runtime 提供实现）。
#[async_trait::async_trait]
pub trait AgentLane: Send + Sync {
    fn name(&self) -> &str;
    async fn accept(
        &self,
        request: OperationRequest,
        context: &crate::harness::context::Context,
    ) -> Result<OperationAdmission, HarnessError>;
    async fn drive(
        &self,
        options: DriveOptions,
        context: &crate::harness::context::Context,
    ) -> DriveResult;
    async fn request_abort(
        &self,
        operation_id: String,
        context: &crate::harness::context::Context,
    ) -> Result<Json, HarnessError>;

    async fn get_tip_id(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<Option<String>, HarnessError>;
    async fn find_entries(
        &self,
        query: Option<BranchScan>,
        context: &crate::harness::context::Context,
    ) -> Result<Vec<Entry>, HarnessError>;
    async fn find_entry(
        &self,
        query: Option<BranchScan>,
        context: &crate::harness::context::Context,
    ) -> Result<Option<Entry>, HarnessError>;
    async fn append_message(
        &self,
        message: AgentMessage,
        context: &crate::harness::context::Context,
    ) -> Result<String, HarnessError>;
    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<Json>,
        context: &crate::harness::context::Context,
    ) -> Result<String, HarnessError>;
    async fn get_result(
        &self,
        operation_id: String,
        context: &crate::harness::context::Context,
    ) -> Result<Option<OperationResultRecord>, HarnessError>;
    async fn inspect_execution(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<Json, HarnessError>;
    async fn prompt(
        &self,
        prompt: Json,
        images: Option<Vec<ImageContent>>,
        context: &crate::harness::context::Context,
    ) -> RunResult;
    async fn skill(
        &self,
        name: String,
        additional_instructions: Option<String>,
        context: &crate::harness::context::Context,
    ) -> RunResult;
    async fn prompt_from_template(
        &self,
        name: String,
        args: Option<Vec<String>>,
        context: &crate::harness::context::Context,
    ) -> RunResult;
    async fn compact(
        &self,
        custom_instructions: Option<String>,
        context: &crate::harness::context::Context,
    ) -> CompactionResult;
    async fn navigate_tree(
        &self,
        target_id: Option<String>,
        options: Option<NavigateOptions>,
        context: &crate::harness::context::Context,
    ) -> NavigationResult;
    async fn resume(&self, context: &crate::harness::context::Context) -> ResumeResult;
    async fn abort(&self, context: &crate::harness::context::Context) -> AbortResult;
    async fn steer(
        &self,
        message: Json,
        images: Option<Vec<ImageContent>>,
        context: &crate::harness::context::Context,
    ) -> QueueResult;
    async fn follow_up(
        &self,
        message: Json,
        images: Option<Vec<ImageContent>>,
        context: &crate::harness::context::Context,
    ) -> QueueResult;
    async fn next_run(
        &self,
        message: Json,
        images: Option<Vec<ImageContent>>,
        context: &crate::harness::context::Context,
    ) -> QueueResult;
    async fn cancel_queued(
        &self,
        entry_id: String,
        context: &crate::harness::context::Context,
    ) -> CancelQueuedResult;
    async fn record_usage(
        &self,
        usage: Usage,
        entry_id: Option<String>,
        details: Option<Json>,
        context: &crate::harness::context::Context,
    ) -> RecordUsageResult;
    async fn wait_for_idle(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn run_when_idle(
        &self,
        callback: std::sync::Arc<
            dyn Fn(
                    crate::harness::context::Context,
                )
                    -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
                + Send
                + Sync,
        >,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_model(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<Option<Model>, HarnessError>;
    async fn set_model(
        &self,
        model: ModelIdentity,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_thinking_level(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<ThinkingLevel, HarnessError>;
    async fn set_thinking_level(
        &self,
        level: ThinkingLevel,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_active_tools(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<Vec<String>, HarnessError>;
    async fn set_active_tools(
        &self,
        names: Vec<String>,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn watch(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<std::sync::Arc<dyn WatchHandle<LaneSnapshot>>, HarnessError>;
}

/// 对应 `AgentHarness`（runtime 提供实现）。
#[async_trait::async_trait]
pub trait AgentHarnessApi: Send + Sync {
    async fn lane(
        &self,
        name: &str,
        context: &crate::harness::context::Context,
    ) -> Result<Option<Arc<dyn AgentLane>>, HarnessError>;
    async fn lanes(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<Vec<OpenOperation>, HarnessError>;
    async fn close(&self, context: &crate::harness::context::Context) -> Result<(), HarnessError>;
    async fn get_name(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<Option<String>, HarnessError>;
    async fn set_name(
        &self,
        name: Option<String>,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_label(
        &self,
        target_id: &str,
        context: &crate::harness::context::Context,
    ) -> Result<Option<String>, HarnessError>;
    async fn set_label(
        &self,
        target_id: &str,
        label: Option<String>,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_tools(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<Vec<crate::harness::types::AgentHarnessTool>, HarnessError>;
    async fn set_tools(
        &self,
        tools: Vec<crate::harness::types::AgentHarnessTool>,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_resources(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<Resources, HarnessError>;
    async fn set_resources(
        &self,
        resources: Resources,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_stream_options(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<crate::harness::types::AgentHarnessStreamOptions, HarnessError>;
    async fn set_stream_options(
        &self,
        options: crate::harness::types::AgentHarnessStreamOptions,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_retry_policy(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<pi_ai::utils::retry::RetryPolicy, HarnessError>;
    async fn set_retry_policy(
        &self,
        policy: pi_ai::utils::retry::RetryPolicy,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_compaction_settings(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<crate::harness::compaction::compaction::CompactionSettings, HarnessError>;
    async fn set_compaction_settings(
        &self,
        settings: crate::harness::compaction::compaction::CompactionSettings,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_steering_mode(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<crate::types::QueueMode, HarnessError>;
    async fn set_steering_mode(
        &self,
        mode: crate::types::QueueMode,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn get_follow_up_mode(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<crate::types::QueueMode, HarnessError>;
    async fn set_follow_up_mode(
        &self,
        mode: crate::types::QueueMode,
        context: &crate::harness::context::Context,
    ) -> Result<(), HarnessError>;
    async fn watch_session(
        &self,
        context: &crate::harness::context::Context,
    ) -> Result<Json, HarnessError>;
}

use std::sync::Arc;
