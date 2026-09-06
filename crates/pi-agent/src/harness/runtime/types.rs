//! Rust 翻译自 packages/agent/src/harness/runtime/types.ts
//!
//! runtime 驱动的核心类型：Config、Lane 运行时状态、Drive、Procedure 结果。

use std::sync::{Arc, Mutex};

use pi_ai::AbortSignal;

use crate::harness::agent_harness::{DriveOutcome, HarnessEvent, Resources};
use crate::harness::context::Context;
use crate::harness::execution::effect_gate::{Gate, GateControl, create_gate};
use crate::harness::session::types::{
    CommitResult, InboxItem, LaneConfiguration, Operation, OperationResultRecord, OperationState,
    Write,
};

/// 对应 `SliceNotImplemented`。
#[derive(Debug)]
pub struct SliceNotImplemented {
    pub operation: String,
}

impl std::fmt::Display for SliceNotImplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is not implemented until its later AgentHarness slice",
            self.operation
        )
    }
}
impl std::error::Error for SliceNotImplemented {}

/// 对应 `Config`（进程本地 harness 配置；复杂回调字段以 Json 占位）。
#[derive(Debug, Clone)]
pub struct Config {
    pub tools: Vec<serde_json::Value>,
    pub resources: Resources,
    pub stream_options: crate::harness::types::AgentHarnessStreamOptions,
    pub retry_policy: pi_ai::utils::retry::RetryPolicy,
    pub compaction: crate::harness::compaction::compaction::CompactionSettings,
    pub steering_mode: crate::types::QueueMode,
    pub follow_up_mode: crate::types::QueueMode,
    pub tool_execution: ToolExecution,
    pub tool_context: Option<serde_json::Value>,
    pub system_prompt: Option<serde_json::Value>,
    pub to_provider_messages: Option<serde_json::Value>,
    pub entry_projectors: Vec<serde_json::Value>,
}

/// 对应 `toolExecution: "sequential" | "parallel"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecution {
    Sequential,
    #[default]
    Parallel,
}

/// 对应 `LaneState`（一个 lane 的运行时快照，区别于 session 的 durable LaneState）。
#[derive(Debug, Clone)]
pub struct LaneRuntimeState {
    pub tip_id: Option<String>,
    pub configuration: LaneConfiguration,
    pub inbox: Vec<InboxItem>,
    pub last_operation_id: Option<String>,
    pub operation: Option<Operation>,
}

/// 对应 `CommitDecision`。
pub struct CommitDecision {
    pub writes: Vec<Write>,
}

/// 对应 `LaneCommand`（序列化 mutation 线上的 effect-free decision）。
pub enum LaneCommand<T> {
    Commit {
        decision: CommitDecision,
        next: LaneRuntimeState,
        materialize: Box<dyn FnOnce(CommitResult) -> T + Send>,
        events: Option<Box<dyn FnOnce(CommitResult) -> Vec<HarnessEvent> + Send>>,
    },
    Return {
        result: T,
    },
    Reject {
        error: String,
    },
}

/// 对应 `OperationCommand`。
pub enum OperationCommand<T> {
    Commit {
        decision: CommitDecision,
        operation_state: OperationState,
        materialize: Box<dyn FnOnce(CommitResult) -> T + Send>,
        events: Option<Box<dyn FnOnce(CommitResult) -> Vec<HarnessEvent> + Send>>,
    },
    Finish {
        writes: Vec<Write>,
        record: OperationResultRecord,
        materialize: Box<dyn FnOnce(CommitResult) -> T + Send>,
        events: Option<Box<dyn FnOnce(CommitResult) -> Vec<HarnessEvent> + Send>>,
    },
    Return {
        result: T,
    },
}

struct DriveInner {
    outcome: Option<DriveOutcome>,
}

/// 对应 `Drive`：一次安装的进程本地 drive pass。
pub struct Drive {
    pub operation_id: String,
    pub gate: Gate,
    pub context: Context,
    pub wait_for_retry: bool,
    pub close_signal: AbortSignal,
    pub deferred_permits: usize,
    inner: Arc<Mutex<DriveInner>>,
    control: GateControl,
}

impl Drive {
    /// 对应 `constructor`。
    pub fn new(options: &crate::harness::agent_harness::DriveOptions, context: Context) -> Self {
        let (gate, control) = create_gate();
        let close_signal = AbortSignal::new();
        Self {
            operation_id: options.operation_id.clone(),
            gate,
            context,
            wait_for_retry: options.wait_for_retry,
            close_signal,
            deferred_permits: if options.poll_deferred { 1 } else { 0 },
            inner: Arc::new(Mutex::new(DriveInner { outcome: None })),
            control,
        }
    }

    /// 对应 `settle`。
    pub fn settle(&self, outcome: DriveOutcome) {
        let mut inner = self.inner.lock().unwrap();
        inner.outcome = Some(outcome);
    }

    /// 对应 `fail`。
    pub fn fail(&self, error: &str) {
        let mut inner = self.inner.lock().unwrap();
        let _ = error;
        inner.outcome = None;
    }

    /// 对应 `beginAbort`。
    pub fn begin_abort(&self) {
        self.control.begin_abort();
    }

    /// 对应 `signalAbort`。
    pub fn signal_abort(&self) {
        self.control.signal_abort();
    }

    /// 对应 `closeGate`。
    pub fn close_gate(&self, error: String) {
        self.control.close(error.clone());
        if !self.close_signal.aborted() {
            self.close_signal.abort();
        }
        let mut inner = self.inner.lock().unwrap();
        inner.outcome = None;
    }

    /// 读取当前 outcome。
    pub fn outcome(&self) -> Option<DriveOutcome> {
        self.inner.lock().unwrap().outcome.clone()
    }
}

/// 对应 `ProcedureResult`。
#[derive(Debug, Clone)]
pub enum ProcedureResult {
    Continue,
    Waiting { outcome: DriveOutcome },
    Settled { outcome: OperationResultRecord },
}
