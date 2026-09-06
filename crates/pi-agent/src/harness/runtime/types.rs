//! Rust 翻译自 packages/agent/src/harness/runtime/types.ts
//!
//! runtime 驱动的核心类型：Config、Lane 运行时状态、Drive、Procedure 结果。

use std::any::Any;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use pi_ai::AbortSignal;
use pi_ai::Message;
use pi_ai::models::Models;
use tokio::sync::Notify;

use crate::harness::agent_harness::{DriveOutcome, HarnessEvent, Resources};
use crate::harness::context::{Context, await_with_context};
use crate::harness::execution::effect_gate::{Gate, GateControl, create_gate};
use crate::harness::hooks::HookRegistry;
use crate::harness::session::types::{
    CommitResult, EntryProjector, InboxItem, LaneConfiguration, Operation, OperationMeta,
    OperationResultRecord, OperationState, Session, SessionReader, ToolExecution, Write,
};
use crate::harness::types::AgentHarnessTool;
use crate::types::{AgentMessage, QueueMode};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 对应 `toProviderMessages`。
pub type ToProviderMessagesFn =
    Arc<dyn Fn(Vec<AgentMessage>, Context) -> BoxFuture<Vec<Message>> + Send + Sync>;

/// 对应 `systemPrompt` 的动态形式。
pub type SystemPromptFn =
    Arc<dyn Fn(Arc<dyn Any + Send + Sync>, Context) -> BoxFuture<String> + Send + Sync>;

/// 对应 `systemPrompt: string | ((toolContext, context) => string)`。
/// `TContext` 在 Rust 中擦除为 `Arc<dyn Any + Send + Sync>`。
#[derive(Clone)]
pub enum SystemPrompt {
    Static(String),
    Dynamic(SystemPromptFn),
}

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

/// 对应 `Config<TContext extends object | undefined>`。
#[derive(Clone)]
pub struct Config {
    pub tools: Vec<AgentHarnessTool>,
    pub resources: Resources,
    pub stream_options: crate::harness::types::AgentHarnessStreamOptions,
    pub retry_policy: pi_ai::utils::retry::RetryPolicy,
    pub compaction: crate::harness::compaction::compaction::CompactionSettings,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub tool_execution: ToolExecution,
    pub tool_context: Option<Arc<dyn Any + Send + Sync>>,
    pub system_prompt: Option<SystemPrompt>,
    pub to_provider_messages: ToProviderMessagesFn,
    pub entry_projectors: BTreeMap<String, EntryProjector>,
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

/// 对应 `Synchronous<TResult>`：materialize 必须同步返回。
/// 对应 `CommitDecision`（扁平化的 commit 分支）。
/// 对应 `LanePatch = Partial<Pick<LaneState, "tipId" | "configuration" | "inbox">>`。
#[derive(Debug, Clone, Default)]
pub struct LanePatch {
    pub tip_id: Option<String>,
    pub configuration: Option<LaneConfiguration>,
    pub inbox: Option<Vec<InboxItem>>,
}

/// 对应 `LaneCommand<TResult>`（序列化 mutation 线上的 effect-free decision）。
#[allow(clippy::large_enum_variant)]
pub enum LaneCommand<T> {
    Commit {
        writes: Vec<Write>,
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

/// 对应 `ContinueOperationResult<TResult>`。
#[derive(Debug, Clone)]
pub enum ContinueOperationResult<T> {
    CancelRequested,
    Result { value: T },
}

/// 对应 `OperationCommand<TResult>`（durable operation 转换）。
#[allow(clippy::large_enum_variant)]
pub enum OperationCommand<T> {
    Commit {
        writes: Vec<Write>,
        operation_state: OperationState,
        lane: Option<LanePatch>,
        materialize: Box<dyn FnOnce(CommitResult) -> T + Send>,
        events: Option<Box<dyn FnOnce(CommitResult) -> Vec<HarnessEvent> + Send>>,
    },
    Finish {
        writes: Vec<Write>,
        record: OperationResultRecord,
        lane: Option<LanePatch>,
        materialize: Box<dyn FnOnce(CommitResult) -> T + Send>,
        events: Option<Box<dyn FnOnce(CommitResult) -> Vec<HarnessEvent> + Send>>,
    },
    Return {
        result: T,
    },
}

/// 对应 `Drive`：一次安装的进程本地 drive pass。
#[derive(Clone)]
pub struct Drive {
    pub operation_id: String,
    pub gate: Gate,
    pub context: Context,
    pub wait_for_retry: bool,
    pub close_signal: AbortSignal,
    pub deferred_permits: Arc<AtomicUsize>,
    completion: Arc<Mutex<DriveCompletion>>,
    completion_notify: Arc<Notify>,
    control: GateControl,
}

enum DriveCompletion {
    Pending,
    Settled(Box<DriveOutcome>),
    Failed(String),
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
            deferred_permits: Arc::new(AtomicUsize::new(if options.poll_deferred { 1 } else { 0 })),
            completion: Arc::new(Mutex::new(DriveCompletion::Pending)),
            completion_notify: Arc::new(Notify::new()),
            control,
        }
    }

    /// 对应 `settle`。
    pub fn settle(&self, outcome: DriveOutcome) {
        *self.completion.lock().unwrap() = DriveCompletion::Settled(Box::new(outcome));
        self.completion_notify.notify_waiters();
    }

    /// 对应 `fail`。
    pub fn fail(&self, error: String) {
        *self.completion.lock().unwrap() = DriveCompletion::Failed(error);
        self.completion_notify.notify_waiters();
    }

    /// 对应 `beginAbort`。
    pub fn begin_abort(&self, cancellation: crate::harness::execution::effect_gate::Cancellation) {
        self.control.begin_abort(cancellation);
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
        *self.completion.lock().unwrap() = DriveCompletion::Failed(error);
        self.completion_notify.notify_waiters();
    }

    /// 对应 `completion` promise：异步等待 settle/fail（感知 context abort）。
    pub async fn completion_wait(&self, context: &Context) -> Result<DriveOutcome, String> {
        loop {
            let state = match &*self.completion.lock().unwrap() {
                DriveCompletion::Settled(outcome) => Some(Ok((**outcome).clone())),
                DriveCompletion::Failed(error) => Some(Err(error.clone())),
                DriveCompletion::Pending => None,
            };
            match state {
                Some(result) => return result,
                None => {
                    await_with_context(self.completion_notify.notified(), context)
                        .await
                        .map_err(|_| "aborted".to_string())?;
                }
            }
        }
    }

    /// 对应 `completion` promise 的读取：返回 settle 结果，或将 fail 的错误上抛。
    pub fn completion(&self) -> Result<Option<DriveOutcome>, String> {
        match &*self.completion.lock().unwrap() {
            DriveCompletion::Settled(outcome) => Ok(Some((**outcome).clone())),
            DriveCompletion::Failed(error) => Err(error.clone()),
            DriveCompletion::Pending => Ok(None),
        }
    }
}

/// 对应 `ProcedureResult`。
#[derive(Debug, Clone)]
pub enum ProcedureResult {
    Continue,
    Waiting { outcome: DriveOutcome },
    Settled { outcome: OperationResultRecord },
}

/// 对应 `Lane` 的 drive 面向能力接口。
///
/// `Lane` 是具体实现（见 `lane.rs`），drive 叶子依赖此 trait 以避免与
/// `driveOperation` 形成类型级循环依赖。
#[async_trait::async_trait]
pub trait Lane: Send + Sync {
    fn name(&self) -> &str;
    fn state(&self) -> LaneRuntimeState;
    fn hooks(&self) -> &HookRegistry;
    fn session(&self) -> &dyn Session;
    fn models(&self) -> &Models;
    fn read_config(&self) -> Config;

    /// 对应 `emitBatch`：发布一批 harness 事件。
    async fn emit_batch(
        &self,
        events: Vec<crate::harness::agent_harness::HarnessEvent>,
        context: &Context,
    ) -> Result<(), String>;

    /// 对应 `command`：在 lane 序列化 mutation 线上执行一个 effect-free 决策。
    async fn command<T: Send + 'static>(
        &self,
        plan: LaneCommandPlanner<T>,
        context: &Context,
    ) -> Result<T, String>;

    /// 对应 `continueOperation`：仅在 durable control running 时执行。
    async fn continue_operation<T: Send + 'static>(
        &self,
        capability: &OperationState,
        plan: OperationPlanner<T>,
        context: &Context,
    ) -> Result<ContinueOperationResult<T>, String>;

    /// 对应 `settleOperation`：即使取消已请求也执行，用于 settle 已准入效果。
    async fn settle_operation<T: Send + 'static>(
        &self,
        capability: &OperationState,
        plan: OperationPlanner<T>,
        context: &Context,
    ) -> Result<T, String>;
}

/// 对应 `plan: (state, current, meta, reader) => OperationCommand<TResult>`。
/// 参数以 owned 快照传入（Rust 化以避开借用生命周期），`reader` 用 `Arc`。
pub type OperationPlanner<T> = Box<
    dyn FnOnce(
            LaneRuntimeState,
            OperationState,
            OperationMeta,
            Arc<dyn SessionReader>,
        ) -> BoxFuture<OperationCommand<T>>
        + Send,
>;

/// 对应 `plan: (state, reader) => LaneCommand<TResult>`。
pub type LaneCommandPlanner<T> =
    Box<dyn FnOnce(LaneRuntimeState, Arc<dyn SessionReader>) -> BoxFuture<LaneCommand<T>> + Send>;
