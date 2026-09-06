//! Rust 翻译自 packages/agent/src/harness/agent-harness.ts
//!
//! AgentHarness 整合层。TS 原版的操作方法大多为 `unavailable()` 占位（返回
//! HarnessNotImplemented），此处同样保留占位语义，仅 getter/setter 为真实实现。

use pi_ai::{AssistantMessage, DeferredHandle, ImageContent, Model, SimpleStreamOptions, Usage};

use crate::types::{AgentMessage, AgentTool, QueueMode, ThinkingLevel};

/// 对应 `HarnessFault`
#[derive(Debug)]
pub struct HarnessFault {
    pub message: String,
}

impl std::fmt::Display for HarnessFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for HarnessFault {}

/// 对应 `HarnessClosed`
#[derive(Debug)]
pub struct HarnessClosed;

impl std::fmt::Display for HarnessClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentHarness was closed while the operation was active")
    }
}
impl std::error::Error for HarnessClosed {}

/// 对应 `HarnessNotImplemented`
#[derive(Debug)]
pub struct HarnessNotImplemented {
    pub operation: String,
}

impl std::fmt::Display for HarnessNotImplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AgentHarness.{} is not implemented yet", self.operation)
    }
}
impl std::error::Error for HarnessNotImplemented {}

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

/// 对应 `RunOutcome`
#[derive(Debug, Clone)]
pub enum RunOutcome {
    Completed {
        leaf_id: String,
        final_entry_id: String,
        final_message: AssistantMessage,
    },
    Aborted {
        leaf_id: String,
        final_entry_id: String,
        final_message: AssistantMessage,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
        final_entry_id: Option<String>,
        final_message: Option<AssistantMessage>,
    },
    Suspended {
        leaf_id: String,
        final_entry_id: String,
        deferred: DeferredHandle,
    },
}

/// 对应 `CompactionOutcome`
#[derive(Debug, Clone)]
pub enum CompactionOutcome {
    Completed {
        leaf_id: String,
    },
    Declined {
        leaf_id: String,
    },
    Aborted {
        leaf_id: String,
    },
    Failed {
        leaf_id: String,
        error: OperationError,
    },
}

/// 对应 `NavigationOutcome`
#[derive(Debug, Clone)]
pub enum NavigationOutcome {
    Completed {
        new_leaf_id: Option<String>,
    },
    Declined {
        leaf_id: Option<String>,
    },
    Aborted {
        leaf_id: Option<String>,
    },
    Failed {
        leaf_id: Option<String>,
        error: OperationError,
    },
}

/// 对应 `ResumeOutcome`
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ResumeOutcome {
    Run {
        run_id: String,
        outcome: RunOutcome,
    },
    Compaction {
        run_id: String,
        outcome: CompactionOutcome,
    },
    Navigation {
        run_id: String,
        outcome: NavigationOutcome,
    },
}

/// 对应 `NavigateOptions`
#[derive(Debug, Clone, Default)]
pub struct NavigateOptions {
    pub summarize: Option<bool>,
    pub custom_instructions: Option<String>,
    pub label: Option<String>,
}

/// 对应 `SuspendedOperation`
#[derive(Debug, Clone)]
pub struct SuspendedOperation {
    pub lane: String,
    pub kind: String,
    pub id: String,
    pub started_at: u64,
    pub reason: String,
    pub prompt: Option<Vec<AgentMessage>>,
    pub deferred: Option<DeferredHandle>,
    pub missing_tools: Vec<String>,
    pub missing_models: Vec<String>,
}

/// 对应 `LaneInfo`
#[derive(Debug, Clone)]
pub struct LaneInfo {
    pub name: String,
    pub leaf_id: Option<String>,
    pub operation: Option<LaneOperationInfo>,
}

#[derive(Debug, Clone)]
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

/// 对应 `LaneSnapshot`
#[derive(Debug, Clone)]
pub struct LaneSnapshot {
    pub lane: String,
    pub transcript: Vec<crate::harness::session::types::Entry>,
    pub leaf_id: Option<String>,
    pub operation: Option<LaneOperationInfo>,
    pub steer: Vec<QueuedItem>,
    pub follow_up: Vec<QueuedItem>,
    pub next_run: Vec<QueuedItem>,
    pub faulted: bool,
}

/// 对应 `SessionSnapshot`
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub lanes: Vec<LaneInfo>,
    pub faulted: bool,
}

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

/// 对应 `AgentHarnessOptions`
#[derive(Clone)]
pub struct AgentHarnessOptions {
    pub model: Model,
    pub thinking_level: Option<ThinkingLevel>,
    pub active_tool_names: Option<Vec<String>>,
    pub tools: Option<Vec<AgentTool>>,
    pub system_prompt: Option<String>,
    pub steering_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
}

/// 对应 `AgentHarness` 类（操作方法是占位，getter/setter 是真实实现）。
pub struct AgentHarness {
    pub name: &'static str,
    model: Model,
    thinking_level: ThinkingLevel,
    active_tool_names: Vec<String>,
    tools: Vec<AgentTool>,
    closed: bool,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
}

impl AgentHarness {
    /// 对应 `create`
    pub async fn create(
        options: AgentHarnessOptions,
    ) -> Result<(AgentHarness, Vec<SuspendedOperation>), HarnessError> {
        Ok((Self::new(options), Vec::new()))
    }

    fn new(options: AgentHarnessOptions) -> Self {
        Self {
            name: "main",
            model: options.model,
            thinking_level: options.thinking_level.unwrap_or(ThinkingLevel::Off),
            active_tool_names: options.active_tool_names.unwrap_or_else(|| {
                options
                    .tools
                    .as_ref()
                    .map(|t| t.iter().map(|x| x.name().to_string()).collect())
                    .unwrap_or_default()
            }),
            tools: options.tools.unwrap_or_default(),
            closed: false,
            steering_mode: options.steering_mode.unwrap_or(QueueMode::OneAtATime),
            follow_up_mode: options.follow_up_mode.unwrap_or(QueueMode::OneAtATime),
        }
    }

    fn unavailable<T>(&self, operation: &str) -> Result<T, HarnessError> {
        if self.closed {
            panic!("{}", HarnessClosed);
        }
        panic!(
            "{}",
            HarnessNotImplemented {
                operation: operation.to_string()
            }
        );
    }

    /// 对应 `getModel`
    pub async fn get_model(&self) -> Model {
        self.model.clone()
    }

    /// 对应 `setModel`
    pub async fn set_model(&mut self, model: Model) {
        self.model = model;
    }

    /// 对应 `getThinkingLevel`
    pub async fn get_thinking_level(&self) -> ThinkingLevel {
        self.thinking_level
    }

    /// 对应 `setThinkingLevel`
    pub async fn set_thinking_level(&mut self, level: ThinkingLevel) {
        self.thinking_level = level;
    }

    /// 对应 `getActiveTools`
    pub async fn get_active_tools(&self) -> Vec<String> {
        self.active_tool_names.clone()
    }

    /// 对应 `setActiveTools`
    pub async fn set_active_tools(&mut self, names: Vec<String>) {
        self.active_tool_names = names;
    }

    /// 对应 `getTools`
    pub async fn get_tools(&self) -> Vec<AgentTool> {
        self.tools.clone()
    }

    /// 对应 `setTools`
    pub async fn set_tools(&mut self, tools: Vec<AgentTool>, active_names: Option<Vec<String>>) {
        self.active_tool_names =
            active_names.unwrap_or_else(|| tools.iter().map(|t| t.name().to_string()).collect());
        self.tools = tools;
    }

    /// 对应 `getSteeringMode`
    pub async fn get_steering_mode(&self) -> QueueMode {
        self.steering_mode
    }

    /// 对应 `setSteeringMode`
    pub async fn set_steering_mode(&mut self, mode: QueueMode) {
        self.steering_mode = mode;
    }

    /// 对应 `getFollowUpMode`
    pub async fn get_follow_up_mode(&self) -> QueueMode {
        self.follow_up_mode
    }

    /// 对应 `setFollowUpMode`
    pub async fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.follow_up_mode = mode;
    }

    /// 对应 `close`
    pub async fn close(&mut self) {
        self.closed = true;
    }

    /// 对应 `prompt`（占位）。
    pub async fn prompt(&self, _input: AgentMessage) -> Result<(), HarnessError> {
        self.unavailable("prompt")
    }

    /// 对应 `steer`（占位）。
    pub async fn steer(&self, _input: AgentMessage) -> Result<(), HarnessError> {
        self.unavailable("steer")
    }

    /// 对应 `followUp`（占位）。
    pub async fn follow_up(&self, _input: AgentMessage) -> Result<(), HarnessError> {
        self.unavailable("followUp")
    }

    /// 对应 `compact`（占位）。
    pub async fn compact(&self) -> Result<(), HarnessError> {
        self.unavailable("compact")
    }

    /// 对应 `resume`（占位）。
    pub async fn resume(&self) -> Result<(), HarnessError> {
        self.unavailable("resume")
    }
}

// 保留未使用类型引用，避免告警。
#[allow(unused)]
fn _unused(_: ImageContent, _: SimpleStreamOptions, _: Usage) {}
