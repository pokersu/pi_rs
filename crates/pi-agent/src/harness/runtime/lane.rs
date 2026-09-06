//! Rust 翻译自 packages/agent/src/harness/runtime/lane.ts
//!
//! 一个已配置 lane 的运行时实现：序列化 mutation 线上的 effect-free 命令，
//! 以及 drive 面向的 operation 转换（continue/settle）。agent-harness 面向的
//! accept/drive/request_abort 由 `AgentLane` trait 提供，见文件尾部。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pi_ai::models::Models;
use pi_ai::utils::assistant_message_frame::reduce_assistant_message_frames;
use pi_ai::{ImageContent, Model};
use tokio::sync::Notify;

use crate::harness::agent_harness::{
    AbortOutcome, AbortResult, AgentLane, CancelQueuedKind, CancelQueuedResult, Closed,
    CompactionResult, CompactionSettlement, DriveOptions, DriveOutcome, DriveResult, HarnessError,
    HarnessEvent, InvalidMessage, InvalidNavigation, LaneBusy, NavigateOptions, NavigationResult,
    NavigationSettlement, NoActiveOperation, NothingToCompact, NothingToResume, OperationAdmission,
    OperationMismatch, OperationRequest, QueueEntry, QueueResult, RecordUsageOutcome,
    RecordUsageResult, ResumeResult, RunResult, RunSettlement, SuspendedRun, UnknownSkill,
    UnknownTarget, UnknownTemplate, WatchHandle,
};
use crate::harness::compaction::branch_summarization::prepare_branch_entries;
use crate::harness::compaction::compaction::prepare_compaction;
use crate::harness::context::{Context, await_with_context};
use crate::harness::events::{BufferedEventWatcher, ResnapshotCapture, WatchHandler};
use crate::harness::execution::tools::tool_result_from_message;
use crate::harness::harness_event::{
    ConfigUpdatePayload, DeferredSnapshot, LaneOperationSnapshot, LaneSnapshot, LaneSnapshotTool,
    RetrySnapshot,
};
use crate::harness::hooks::HookRegistry;
use crate::harness::prompt_templates::format_prompt_template_invocation;
use crate::harness::runtime::drive::drive_operation;
use crate::harness::runtime::drive::structural::{
    durable_branch_preparation, durable_compaction_preparation,
};
use crate::harness::runtime::progress::read_assistant_frames;
use crate::harness::runtime::transcript::{committed_entry_events, read_lane_queues};
use crate::harness::runtime::types::{
    Config, ContinueOperationResult, Drive, Lane, LaneCommand, LaneCommandPlanner, LanePatch,
    LaneRuntimeState, OperationCommand, OperationPlanner,
};
use crate::harness::session::commit::{insert_entry, insert_usage};
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{
    self, BranchScan, CommitResult, Control, Entry, EntryType, InboxItem, InboxItemKind,
    LaneState as DurableLaneState, ModelIdentity, NewCustomEntry, NewEntry, NewMessageEntry,
    Operation, OperationIntent, OperationMeta, OperationResultRecord, OperationScope,
    OperationState, PendingEntry, RunSettings, ScanOrder, Session, SessionMutationCallback,
    SessionReader, StorageBranchScan, UsageRow, Write,
};
use crate::harness::session::values::{
    branch_tip, delete_value, lane_config as lane_config_value, lane_state as lane_state_value,
    operation_meta as operation_meta_value, operation_result as operation_result_value,
    operation_state as operation_state_value, operation_tool_args, pending_entry,
    pending_tool_output, set_value,
};
use crate::harness::skills::format_skill_invocation;
use crate::types::{AgentMessage, QueueMode, ThinkingLevel};

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type EmitBatch = Arc<dyn Fn(Vec<HarnessEvent>, Context) -> BoxFuture + Send + Sync>;
pub type FaultHandler = Arc<dyn Fn(String, Context) -> String + Send + Sync>;

/// 对应 `OperationIntent.kind` 的字符串表示。
fn intent_kind(intent: &OperationIntent) -> &'static str {
    match intent {
        OperationIntent::Run { .. } => "run",
        OperationIntent::Compaction { .. } => "compaction",
        OperationIntent::Navigation { .. } => "navigation",
    }
}

/// 对应 `durableLaneState`：把 owned lane 投影序列化为 durable 的 lane state。
fn durable_lane_state(
    current_operation_id: Option<String>,
    inbox: Vec<InboxItem>,
    last_operation_id: Option<String>,
) -> DurableLaneState {
    DurableLaneState {
        current_operation_id,
        last_operation_id,
        inbox,
    }
}

/// 对应 `selectAcceptedInbox`。
fn select_accepted_inbox(
    inbox: &[InboxItem],
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
) -> (Vec<InboxItem>, Vec<InboxItem>) {
    let mut steer_taken = false;
    let mut follow_up_taken = false;
    let mut selected = Vec::new();
    let mut remainder = Vec::new();
    for item in inbox {
        let eligible = item.kind == InboxItemKind::Write
            || item.kind == InboxItemKind::NextRun
            || (item.kind == InboxItemKind::Steer
                && (steering_mode == QueueMode::All || !steer_taken))
            || (item.kind == InboxItemKind::FollowUp
                && (follow_up_mode == QueueMode::All || !follow_up_taken));
        if eligible {
            if item.kind == InboxItemKind::Steer {
                steer_taken = true;
            }
            if item.kind == InboxItemKind::FollowUp {
                follow_up_taken = true;
            }
            selected.push(item.clone());
        } else {
            remainder.push(item.clone());
        }
    }
    (selected, remainder)
}

/// 对应 `capturedSettings`。
fn captured_settings(config: &Config) -> RunSettings {
    RunSettings {
        compaction: config.compaction.clone(),
        steering_mode: config.steering_mode,
        follow_up_mode: config.follow_up_mode,
        tool_execution: config.tool_execution,
    }
}

/// 对应 `pendingEntryWrite`。
fn pending_entry_write(entry_id: String, pending: PendingEntry) -> NewEntry {
    match pending {
        PendingEntry::Message { payload } => NewEntry::Message(NewMessageEntry {
            id: entry_id,
            parent_id: None,
            custom_type: None,
            message: payload,
            terminate: None,
        }),
        PendingEntry::Custom {
            custom_type,
            payload,
        } => NewEntry::Custom(NewCustomEntry {
            id: entry_id,
            parent_id: None,
            custom_type,
            data: payload,
        }),
    }
}

/// 对应 `chainEntries`：串接 parentId 链，保持 `NewEntry` 类型。
fn chain_new_entries(parent_id: Option<String>, entries: Vec<NewEntry>) -> Vec<NewEntry> {
    let mut parent = parent_id;
    let mut result = Vec::with_capacity(entries.len());
    for mut entry in entries {
        let id = entry.id().to_string();
        match &mut entry {
            NewEntry::Message(e) => e.parent_id = parent.clone(),
            NewEntry::Compaction(e) => e.parent_id = parent.clone(),
            NewEntry::BranchSummary(e) => e.parent_id = parent.clone(),
            NewEntry::Custom(e) => e.parent_id = parent.clone(),
        }
        result.push(entry);
        parent = Some(id);
    }
    result
}

/// 对应 `state.control` 的提取（含 deferred scope）。
pub fn state_control(state: &OperationState) -> &Control {
    match state {
        OperationState::Starting { scope, .. }
        | OperationState::Checkpoint { scope, .. }
        | OperationState::AssistantReady { scope, .. }
        | OperationState::AssistantEffectPending { scope, .. }
        | OperationState::AssistantRetryWait { scope, .. }
        | OperationState::Tools { scope, .. }
        | OperationState::SummaryDeciding { scope, .. }
        | OperationState::SummaryReady { scope, .. }
        | OperationState::SummaryEffectPending { scope, .. }
        | OperationState::SummaryRetryWait { scope, .. }
        | OperationState::NavigationReadyToCommit { scope, .. } => &scope.control,
        OperationState::DeferredSuspended { scope } => &scope.control,
        OperationState::DeferredEffectPending { scope, .. } => &scope.control,
    }
}

fn apply_lane_patch(state: LaneRuntimeState, patch: Option<LanePatch>) -> LaneRuntimeState {
    let Some(patch) = patch else {
        return state;
    };
    LaneRuntimeState {
        tip_id: patch.tip_id.or(state.tip_id),
        configuration: patch.configuration.unwrap_or(state.configuration),
        inbox: patch.inbox.unwrap_or(state.inbox),
        last_operation_id: state.last_operation_id,
        operation: state.operation,
    }
}

/// 为任意 leaf 附加 `cancel_requested` control（保持其它字段不变）。
fn with_cancel_requested(state: OperationState, requested_at: u64) -> OperationState {
    let control = Control::CancelRequested { requested_at };
    match state {
        OperationState::Starting { mut scope } => {
            scope.control = control;
            OperationState::Starting { scope }
        }
        OperationState::Checkpoint {
            mut scope,
            continuation,
            trigger_entry_id,
        } => {
            scope.control = control;
            OperationState::Checkpoint {
                scope,
                continuation,
                trigger_entry_id,
            }
        }
        OperationState::AssistantReady {
            mut scope,
            generation_context,
            next_attempt,
        } => {
            scope.control = control;
            OperationState::AssistantReady {
                scope,
                generation_context,
                next_attempt,
            }
        }
        OperationState::AssistantEffectPending {
            mut scope,
            generation_context,
            attempt,
            response_entry_id,
            usage_id,
            intended_output_limit,
            context_window,
        } => {
            scope.control = control;
            OperationState::AssistantEffectPending {
                scope,
                generation_context,
                attempt,
                response_entry_id,
                usage_id,
                intended_output_limit,
                context_window,
            }
        }
        OperationState::AssistantRetryWait {
            mut scope,
            generation_context,
            next_attempt,
            not_before,
            error_message,
        } => {
            scope.control = control;
            OperationState::AssistantRetryWait {
                scope,
                generation_context,
                next_attempt,
                not_before,
                error_message,
            }
        }
        OperationState::Tools { mut scope, batch } => {
            scope.control = control;
            OperationState::Tools { scope, batch }
        }
        OperationState::DeferredSuspended { mut scope } => {
            scope.control = control;
            OperationState::DeferredSuspended { scope }
        }
        OperationState::DeferredEffectPending {
            mut scope,
            response_entry_id,
            usage_id,
        } => {
            scope.control = control;
            OperationState::DeferredEffectPending {
                scope,
                response_entry_id,
                usage_id,
            }
        }
        OperationState::SummaryDeciding { mut scope, task } => {
            scope.control = control;
            OperationState::SummaryDeciding { scope, task }
        }
        OperationState::SummaryReady {
            mut scope,
            task,
            summary_context,
            next_attempt,
        } => {
            scope.control = control;
            OperationState::SummaryReady {
                scope,
                task,
                summary_context,
                next_attempt,
            }
        }
        OperationState::SummaryEffectPending {
            mut scope,
            task,
            summary_context,
            attempt,
            request,
            usage_ids,
        } => {
            scope.control = control;
            OperationState::SummaryEffectPending {
                scope,
                task,
                summary_context,
                attempt,
                request,
                usage_ids,
            }
        }
        OperationState::SummaryRetryWait {
            mut scope,
            task,
            summary_context,
            next_attempt,
            not_before,
            error_message,
        } => {
            scope.control = control;
            OperationState::SummaryRetryWait {
                scope,
                task,
                summary_context,
                next_attempt,
                not_before,
                error_message,
            }
        }
        OperationState::NavigationReadyToCommit {
            mut scope,
            target_id,
            label,
        } => {
            scope.control = control;
            OperationState::NavigationReadyToCommit {
                scope,
                target_id,
                label,
            }
        }
    }
}

/// 一个 command 的临时产物。
enum CommandOutcome<T> {
    Return {
        result: T,
        delivery: Option<BoxFuture>,
    },
    Reject {
        error: String,
    },
}

struct LaneInner {
    state: LaneRuntimeState,
    idle_owner: Option<Arc<Notify>>,
    generation: u64,
    closed_error: Option<String>,
    active_drive: Option<crate::harness::runtime::types::Drive>,
}

/// 对应 `Lane`（runtime 实现）。
pub struct LaneImpl {
    name: String,
    session: Arc<dyn Session>,
    models: Arc<Models>,
    hooks: Arc<HookRegistry>,
    emit_batch: EmitBatch,
    install_watch: WatchHandler<LaneSnapshot>,
    on_fault: FaultHandler,
    config: Arc<dyn Fn() -> Config + Send + Sync>,
    inner: Arc<Mutex<LaneInner>>,
    notify: Arc<Notify>,
    self_weak: std::sync::Weak<LaneImpl>,
}

impl LaneImpl {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        session: Arc<dyn Session>,
        models: Arc<Models>,
        hooks: Arc<HookRegistry>,
        state: LaneRuntimeState,
        on_fault: FaultHandler,
        emit_batch: EmitBatch,
        install_watch: WatchHandler<LaneSnapshot>,
        config: Arc<dyn Fn() -> Config + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            name,
            session,
            models,
            hooks,
            emit_batch,
            install_watch,
            on_fault,
            config,
            inner: Arc::new(Mutex::new(LaneInner {
                state,
                idle_owner: None,
                generation: 0,
                closed_error: None,
                active_drive: None,
            })),
            notify: Arc::new(Notify::new()),
            self_weak: weak.clone(),
        })
    }

    fn self_arc(&self) -> Arc<Self> {
        self.self_weak.upgrade().expect("LaneImpl self reference")
    }

    /// 对应 `seal`。
    pub fn seal(&self, error: String) {
        let mut guard = self.inner.lock().unwrap();
        if guard.closed_error.is_none() {
            guard.closed_error = Some(error.clone());
        }
        if let Some(drive) = &guard.active_drive {
            drive.close_gate(error.clone());
        }
        guard.generation += 1;
        drop(guard);
        self.notify.notify_waiters();
    }

    fn assert_open(&self) -> Result<(), String> {
        let inner = self.inner.lock().unwrap();
        if let Some(error) = &inner.closed_error {
            return Err(error.clone());
        }
        Ok(())
    }

    fn signal_state_change(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.generation += 1;
        drop(inner);
        self.notify.notify_waiters();
    }

    /// 等待 idle_owner 消失或 state 变化（含 context abort）。
    async fn wait_idle_change(&self, context: &Context) -> Result<(), String> {
        self.assert_open()?;
        let idle_owner = self.inner.lock().unwrap().idle_owner.clone();
        let wait = async {
            if let Some(owner) = idle_owner {
                tokio::select! {
                    _ = owner.notified() => {}
                    _ = self.notify.notified() => {}
                }
            } else {
                self.notify.notified().await;
            }
        };
        await_with_context(wait, context)
            .await
            .map_err(|_| "aborted".to_string())
    }

    async fn run_command<T: Send + 'static>(
        &self,
        plan: LaneCommandPlanner<T>,
        context: &Context,
    ) -> Result<CommandOutcome<T>, String> {
        let session = Arc::clone(&self.session);
        let inner = Arc::clone(&self.inner);
        let notify = Arc::clone(&self.notify);
        let emit_batch = self.emit_batch.clone();
        let context = context.clone();

        let plan = Mutex::new(Some(plan));
        let mutation: SessionMutationCallback<CommandOutcome<T>> = Arc::new(move |mutator, ctx| {
            let plan = plan
                .lock()
                .unwrap()
                .take()
                .expect("command planner already consumed");
            let inner = Arc::clone(&inner);
            let notify = Arc::clone(&notify);
            let emit_batch = emit_batch.clone();
            Box::pin(async move {
                let state = inner.lock().unwrap().state.clone();
                let reader: Arc<dyn SessionReader> = mutator.clone();
                let decision = plan(state, reader).await;
                match decision {
                    LaneCommand::Return { result } => Ok(CommandOutcome::Return {
                        result,
                        delivery: None,
                    }),
                    LaneCommand::Reject { error } => Ok(CommandOutcome::Reject { error }),
                    LaneCommand::Commit {
                        writes,
                        next,
                        materialize,
                        events,
                    } => {
                        let commit = mutator.commit(writes, &ctx).await?;
                        {
                            let mut guard = inner.lock().unwrap();
                            guard.state = next;
                            guard.generation += 1;
                        }
                        notify.notify_waiters();
                        let result = materialize(commit.clone());
                        let events = events.map(|f| f(commit.clone())).unwrap_or_default();
                        let delivery = if events.is_empty() {
                            None
                        } else {
                            Some(emit_batch(events, ctx.clone()))
                        };
                        Ok(CommandOutcome::Return { result, delivery })
                    }
                }
            })
        });

        types::mutate(session.as_ref(), mutation, &context).await
    }
}

#[async_trait::async_trait]
impl Lane for LaneImpl {
    fn name(&self) -> &str {
        &self.name
    }

    fn state(&self) -> LaneRuntimeState {
        self.inner.lock().unwrap().state.clone()
    }

    fn hooks(&self) -> &HookRegistry {
        self.hooks.as_ref()
    }

    fn session(&self) -> &dyn Session {
        self.session.as_ref()
    }

    fn models(&self) -> &Models {
        self.models.as_ref()
    }

    fn read_config(&self) -> Config {
        (self.config)()
    }

    async fn emit_batch(&self, events: Vec<HarnessEvent>, context: &Context) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        let delivery = (self.emit_batch)(events, context.clone());
        delivery.await;
        Ok(())
    }

    async fn command<T: Send + 'static>(
        &self,
        plan: LaneCommandPlanner<T>,
        context: &Context,
    ) -> Result<T, String> {
        self.assert_open()?;
        loop {
            if self.inner.lock().unwrap().idle_owner.is_none() {
                break;
            }
            self.wait_idle_change(context).await?;
            self.assert_open()?;
        }
        let outcome = self.run_command(plan, context).await?;
        match outcome {
            CommandOutcome::Reject { error } => Err(error),
            CommandOutcome::Return { result, delivery } => {
                if let Some(delivery) = delivery {
                    delivery.await;
                }
                Ok(result)
            }
        }
    }

    async fn settle_operation<T: Send + 'static>(
        &self,
        _capability: &OperationState,
        plan: OperationPlanner<T>,
        context: &Context,
    ) -> Result<T, String> {
        let name = self.name.clone();
        let wrapped: LaneCommandPlanner<T> = Box::new(move |state, reader| {
            let name = name.clone();
            Box::pin(async move {
                let operation = state.operation.clone().unwrap_or_else(|| {
                    panic!(
                        "{}",
                        SessionInvariantError::new(format!(
                            "Settle operation has no current operation for lane {name}"
                        ))
                    )
                });
                let decision = plan(
                    state.clone(),
                    operation.state.clone(),
                    operation.meta.clone(),
                    reader,
                )
                .await;
                match decision {
                    OperationCommand::Return { result } => LaneCommand::Return { result },
                    OperationCommand::Commit {
                        writes,
                        operation_state,
                        lane,
                        materialize,
                        events,
                    } => {
                        let mut writes = writes;
                        writes.push(Write::Value(set_value(
                            &operation_state_value(&operation.meta.operation_id),
                            serde_json::to_value(&operation_state)
                                .unwrap_or(serde_json::Value::Null),
                        )));
                        let lane_inbox = lane.as_ref().and_then(|l| l.inbox.clone());
                        if let Some(inbox) = lane_inbox {
                            writes.push(Write::Value(set_value(
                                &lane_state_value(&name),
                                serde_json::to_value(durable_lane_state(
                                    Some(operation.meta.operation_id.clone()),
                                    inbox,
                                    state.last_operation_id.clone(),
                                ))
                                .unwrap_or(serde_json::Value::Null),
                            )));
                        }
                        let mut next = apply_lane_patch(state, lane);
                        next.operation = Some(Operation {
                            meta: operation.meta,
                            state: operation_state,
                        });
                        LaneCommand::Commit {
                            writes,
                            next,
                            materialize,
                            events,
                        }
                    }
                    OperationCommand::Finish {
                        writes,
                        record,
                        lane,
                        materialize,
                        events,
                    } => {
                        let inbox = lane
                            .as_ref()
                            .and_then(|l| l.inbox.clone())
                            .unwrap_or_else(|| state.inbox.clone());
                        let mut writes = writes;
                        writes.push(Write::Value(set_value(
                            &operation_result_value(&operation.meta.operation_id),
                            serde_json::to_value(&record).unwrap_or(serde_json::Value::Null),
                        )));
                        writes.push(Write::Value(set_value(
                            &lane_state_value(&name),
                            serde_json::to_value(durable_lane_state(
                                None,
                                inbox.clone(),
                                Some(operation.meta.operation_id.clone()),
                            ))
                            .unwrap_or(serde_json::Value::Null),
                        )));
                        let mut next = apply_lane_patch(state, lane);
                        next.inbox = inbox;
                        next.last_operation_id = Some(operation.meta.operation_id);
                        next.operation = None;
                        LaneCommand::Commit {
                            writes,
                            next,
                            materialize,
                            events,
                        }
                    }
                }
            })
        });
        self.command(wrapped, context).await
    }

    async fn continue_operation<T: Send + 'static>(
        &self,
        capability: &OperationState,
        plan: OperationPlanner<T>,
        context: &Context,
    ) -> Result<ContinueOperationResult<T>, String> {
        let capability = capability.clone();
        let wrapped: OperationPlanner<ContinueOperationResult<T>> =
            Box::new(move |state, latest, meta, reader| {
                Box::pin(async move {
                    if matches!(state_control(&latest), Control::CancelRequested { .. }) {
                        return OperationCommand::Return {
                            result: ContinueOperationResult::CancelRequested,
                        };
                    }
                    let decision = plan(state, latest, meta, reader).await;
                    match decision {
                        OperationCommand::Return { result } => OperationCommand::Return {
                            result: ContinueOperationResult::Result { value: result },
                        },
                        OperationCommand::Commit {
                            writes,
                            operation_state,
                            lane,
                            materialize,
                            events,
                        } => OperationCommand::Commit {
                            writes,
                            operation_state,
                            lane,
                            materialize: Box::new(move |commit| ContinueOperationResult::Result {
                                value: materialize(commit),
                            }),
                            events,
                        },
                        OperationCommand::Finish {
                            writes,
                            record,
                            lane,
                            materialize,
                            events,
                        } => OperationCommand::Finish {
                            writes,
                            record,
                            lane,
                            materialize: Box::new(move |commit| ContinueOperationResult::Result {
                                value: materialize(commit),
                            }),
                            events,
                        },
                    }
                })
            });
        self.settle_operation(&capability, wrapped, context).await
    }
}

// 保留引用以消除潜在 unused warning（FaultHandler / OperationMeta / Write 等）。
#[allow(dead_code)]
fn _keep_types(_: &FaultHandler, _: &OperationMeta, _: &Write, _: &CommitResult) {}

/// 对应 `driveOperation` 里 `DriveClaim` 的 claim 结果。
enum DriveClaim {
    Settled { outcome: OperationResultRecord },
    Mismatch { error: HarnessError },
    Occupied { drive: Drive },
    Observe { drive: Drive, installed: bool },
}

impl LaneImpl {
    /// 对应 `waitForIdle`。
    pub async fn wait_for_idle(&self, context: &Context) -> Result<(), String> {
        loop {
            let idle = self
                .command(
                    Box::new(|state, _reader| {
                        Box::pin(async move {
                            LaneCommand::Return {
                                result: state.operation.is_none(),
                            }
                        })
                    }),
                    context,
                )
                .await?;
            if idle {
                return Ok(());
            }
            self.wait_idle_change(context).await?;
        }
    }

    /// 对应 `runWhenIdle`。
    pub async fn run_when_idle(
        &self,
        callback: Arc<dyn Fn(Context) -> BoxFuture + Send + Sync>,
        context: &Context,
    ) -> Result<(), String> {
        let owner = Arc::new(Notify::new());
        loop {
            let inner = Arc::clone(&self.inner);
            let notify = Arc::clone(&self.notify);
            let owner = Arc::clone(&owner);
            let claimed = self
                .command(
                    Box::new(move |state, _reader| {
                        let inner = Arc::clone(&inner);
                        let notify = Arc::clone(&notify);
                        let owner = Arc::clone(&owner);
                        Box::pin(async move {
                            if state.operation.is_some()
                                || inner.lock().unwrap().active_drive.is_some()
                            {
                                return LaneCommand::Return { result: false };
                            }
                            {
                                let mut guard = inner.lock().unwrap();
                                guard.idle_owner = Some(owner);
                                guard.generation += 1;
                            }
                            notify.notify_waiters();
                            LaneCommand::Return { result: true }
                        })
                    }),
                    context,
                )
                .await?;
            if claimed {
                break;
            }
            self.wait_idle_change(context).await?;
        }

        callback(context.clone()).await;
        {
            let mut guard = self.inner.lock().unwrap();
            if guard
                .idle_owner
                .as_ref()
                .map(|o| Arc::ptr_eq(o, &owner))
                .unwrap_or(false)
            {
                guard.idle_owner = None;
            }
        }
        owner.notify_waiters();
        self.signal_state_change();
        Ok(())
    }

    async fn command_drive_claim(
        &self,
        options: &DriveOptions,
        context: &Context,
    ) -> Result<DriveClaim, HarnessError> {
        let operation_id = options.operation_id.clone();
        let options_owned = options.clone();
        let name = self.name.clone();
        let inner = Arc::clone(&self.inner);
        let notify = Arc::clone(&self.notify);
        let context_for_closure = context.clone();

        self.command(
            Box::new(move |state, reader| {
                let operation_id = operation_id.clone();
                let options_owned = options_owned.clone();
                let name = name.clone();
                let inner = Arc::clone(&inner);
                let notify = Arc::clone(&notify);
                let context = context_for_closure.clone();
                Box::pin(async move {
                    if let Some(signal) = context.abort_signal()
                        && signal.aborted()
                    {
                        return LaneCommand::Reject {
                            error: "The operation was aborted".to_string(),
                        };
                    }
                    let current = state
                        .operation
                        .as_ref()
                        .map(|o| o.meta.operation_id.clone());
                    if current.as_deref() == Some(operation_id.as_str()) {
                        let (drive, installed) = {
                            let mut guard = inner.lock().unwrap();
                            if let Some(existing) = &guard.active_drive {
                                (existing.clone(), false)
                            } else {
                                let drive = Drive::new(&options_owned, context.clone());
                                guard.active_drive = Some(drive.clone());
                                guard.generation += 1;
                                (drive, true)
                            }
                        };
                        if installed {
                            notify.notify_waiters();
                        }
                        if drive.operation_id == operation_id {
                            return LaneCommand::Return {
                                result: DriveClaim::Observe { drive, installed },
                            };
                        }
                        return LaneCommand::Return {
                            result: DriveClaim::Occupied { drive },
                        };
                    }

                    let stored = reader
                        .get_value(&operation_result_value(&operation_id).erased(), &context)
                        .await
                        .unwrap_or_else(|e| panic!("{e}"));
                    match stored {
                        None => LaneCommand::Return {
                            result: DriveClaim::Mismatch {
                                error: HarnessError::OperationMismatch(OperationMismatch::new(
                                    name.clone(),
                                    operation_id.clone(),
                                    current,
                                    state.last_operation_id,
                                    format!("Operation {operation_id} does not own lane {name:?}"),
                                )),
                            },
                        },
                        Some(stored) => LaneCommand::Return {
                            result: DriveClaim::Settled {
                                outcome: serde_json::from_value(stored.value)
                                    .unwrap_or_else(|e| panic!("parse operation result: {e}")),
                            },
                        },
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))
    }

    async fn accept_run(
        &self,
        request: &OperationRequest,
        operation_id: String,
        started_at: u64,
        acceptance_config: &Config,
        context: &Context,
    ) -> Result<OperationAdmission, HarnessError> {
        let messages: Vec<AgentMessage> = match request {
            OperationRequest::Prompt { prompt, images, .. } => {
                if prompt.is_string() {
                    let prompt_str = prompt.as_str().unwrap();
                    let images = images.clone().unwrap_or_default();
                    if prompt_str.is_empty() && images.is_empty() {
                        Vec::new()
                    } else {
                        let mut content = Vec::new();
                        if !prompt_str.is_empty() {
                            content.push(serde_json::json!({ "type": "text", "text": prompt_str }));
                        }
                        for image in &images {
                            content.push(
                                serde_json::to_value(image).unwrap_or(serde_json::Value::Null),
                            );
                        }
                        let message: AgentMessage = serde_json::from_value(serde_json::json!({
                            "role": "user",
                            "content": content,
                            "timestamp": started_at,
                        }))
                        .unwrap_or_else(|e| panic!("construct prompt message: {e}"));
                        vec![message]
                    }
                } else if prompt.is_array() {
                    serde_json::from_value(prompt.clone())
                        .unwrap_or_else(|e| panic!("parse prompt messages: {e}"))
                } else {
                    let message: AgentMessage = serde_json::from_value(prompt.clone())
                        .unwrap_or_else(|e| panic!("parse prompt message: {e}"));
                    vec![message]
                }
            }
            OperationRequest::Skill {
                name,
                additional_instructions,
                ..
            } => {
                let skill = acceptance_config
                    .resources
                    .skills
                    .iter()
                    .find(|c| c.name == *name);
                let Some(skill) = skill else {
                    return Err(HarnessError::UnknownSkill(UnknownSkill::new(
                        name.clone(),
                        format!("Unknown skill: {name}"),
                    )));
                };
                let text = format_skill_invocation(skill, additional_instructions.as_deref());
                let message: AgentMessage = serde_json::from_value(serde_json::json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": text }],
                    "timestamp": started_at,
                }))
                .unwrap_or_else(|e| panic!("construct skill message: {e}"));
                vec![message]
            }
            OperationRequest::PromptTemplate { name, args, .. } => {
                let template = acceptance_config
                    .resources
                    .prompt_templates
                    .iter()
                    .find(|c| c.name == *name);
                let Some(template) = template else {
                    return Err(HarnessError::UnknownTemplate(UnknownTemplate::new(
                        name.clone(),
                        format!("Unknown prompt template: {name}"),
                    )));
                };
                let content =
                    format_prompt_template_invocation(template, args.as_deref().unwrap_or(&[]));
                if content.is_empty() {
                    Vec::new()
                } else {
                    let message: AgentMessage = serde_json::from_value(serde_json::json!({
                        "role": "user",
                        "content": [{ "type": "text", "text": content }],
                        "timestamp": started_at,
                    }))
                    .unwrap_or_else(|e| panic!("construct template message: {e}"));
                    vec![message]
                }
            }
            _ => unreachable!(),
        };

        for message in &messages {
            if let AgentMessage::Assistant(a) = message
                && a.stop_reason == pi_ai::StopReason::Pending
            {
                return Err(HarnessError::InvalidMessage(InvalidMessage::new(
                    self.name.clone(),
                    "pending_assistant".to_string(),
                    "Cannot accept a pending assistant message".to_string(),
                )));
            }
        }
        let prompt: Vec<(String, AgentMessage)> = messages
            .into_iter()
            .map(|message| (self.session.id_generator().next(Some(started_at)), message))
            .collect();

        let name = self.name.clone();
        let context_for_closure = context.clone();
        let acceptance_config_owned = acceptance_config.clone();
        let prompt_for_closure = prompt.clone();

        self.command(
            Box::new(move |state, reader| {
                let name = name.clone();
                let context = context_for_closure.clone();
                let acceptance_config = acceptance_config_owned.clone();
                let operation_id = operation_id.clone();
                let prompt = prompt_for_closure.clone();
                Box::pin(async move {
                    if let Some(op) = &state.operation {
                        return LaneCommand::Return {
                            result: Err(HarnessError::LaneBusy(LaneBusy::new(
                                name.clone(),
                                op.meta.operation_id.clone(),
                                intent_kind(&op.meta.intent).to_string(),
                                format!("Lane {name:?} already has an active operation"),
                            ))),
                        };
                    }
                    let (selected_items, inbox) = select_accepted_inbox(
                        &state.inbox,
                        acceptance_config.steering_mode,
                        acceptance_config.follow_up_mode,
                    );
                    let mut captured = Vec::new();
                    for item in &selected_items {
                        let stored = reader
                            .get_value(&pending_entry(&item.entry_id).erased(), &context)
                            .await
                            .unwrap_or_else(|e| panic!("{e}"));
                        let stored = stored.unwrap_or_else(|| {
                            panic!(
                                "{}",
                                SessionInvariantError::new(format!(
                                    "Pending {:?} entry {} is missing its payload",
                                    item.kind, item.entry_id
                                ))
                            )
                        });
                        let pending: PendingEntry = serde_json::from_value(stored.value)
                            .unwrap_or_else(|e| panic!("parse pending entry: {e}"));
                        if item.kind != InboxItemKind::Write
                            && !matches!(pending, PendingEntry::Message { .. })
                        {
                            panic!(
                                "{}",
                                SessionInvariantError::new(format!(
                                    "Pending {:?} entry {} is not a message",
                                    item.kind, item.entry_id
                                ))
                            );
                        }
                        if let PendingEntry::Message { payload } = &pending
                            && let AgentMessage::Assistant(a) = payload
                            && a.stop_reason == pi_ai::StopReason::Pending
                        {
                            panic!(
                                "{}",
                                SessionInvariantError::new(format!(
                                    "Pending {:?} entry {} contains a pending assistant",
                                    item.kind, item.entry_id
                                ))
                            );
                        }
                        captured.push((item.clone(), pending));
                    }
                    let has_captured_conversation = selected_items
                        .iter()
                        .any(|item| item.kind != InboxItemKind::Write);
                    if prompt.is_empty() && !has_captured_conversation {
                        return LaneCommand::Return {
                            result: Err(HarnessError::InvalidMessage(InvalidMessage::new(
                                name.clone(),
                                "empty".to_string(),
                                "Acceptance must append at least one message".to_string(),
                            ))),
                        };
                    }

                    let mut new_entries: Vec<NewEntry> = Vec::new();
                    for (item, pending) in &captured {
                        new_entries
                            .push(pending_entry_write(item.entry_id.clone(), pending.clone()));
                    }
                    for (id, message) in &prompt {
                        new_entries.push(NewEntry::Message(NewMessageEntry {
                            id: id.clone(),
                            parent_id: None,
                            custom_type: None,
                            message: message.clone(),
                            terminate: None,
                        }));
                    }
                    let entries = chain_new_entries(state.tip_id.clone(), new_entries);
                    let parent_id = entries.last().map(|e| e.id().to_string());
                    let meta = OperationMeta {
                        operation_id: operation_id.clone(),
                        lane: name.clone(),
                        source_tip_id: state.tip_id.clone(),
                        started_at,
                        intent: OperationIntent::Run {
                            prompt_entry_ids: prompt.iter().map(|(id, _)| id.clone()).collect(),
                        },
                    };
                    let operation_state = OperationState::Starting {
                        scope: OperationScope {
                            control: Control::Running,
                            settings: captured_settings(&acceptance_config),
                            latest_assistant_entry_id: None,
                        },
                    };
                    let remaining_queues = read_lane_queues(reader.as_ref(), &inbox, &context)
                        .await
                        .unwrap_or_else(|e| panic!("{e}"));
                    let last_operation_id = state.last_operation_id.clone();
                    let next = LaneRuntimeState {
                        tip_id: parent_id.clone(),
                        inbox: inbox.clone(),
                        operation: Some(Operation {
                            meta: meta.clone(),
                            state: operation_state.clone(),
                        }),
                        ..state
                    };

                    let mut writes: Vec<Write> = Vec::new();
                    for entry in &entries {
                        writes.push(insert_entry(entry.clone()));
                    }
                    for item in &selected_items {
                        writes.push(Write::Value(delete_value(&pending_entry(&item.entry_id))));
                    }
                    if let Some(parent_id) = &parent_id {
                        writes.push(Write::Value(set_value(
                            &branch_tip(&name),
                            serde_json::json!(parent_id),
                        )));
                    }
                    writes.push(Write::Value(set_value(
                        &operation_meta_value(&operation_id),
                        serde_json::to_value(&meta).unwrap_or(serde_json::Value::Null),
                    )));
                    writes.push(Write::Value(set_value(
                        &operation_state_value(&operation_id),
                        serde_json::to_value(&operation_state).unwrap_or(serde_json::Value::Null),
                    )));
                    writes.push(Write::Value(set_value(
                        &lane_state_value(&name),
                        serde_json::to_value(durable_lane_state(
                            Some(operation_id.clone()),
                            inbox.clone(),
                            last_operation_id,
                        ))
                        .unwrap_or(serde_json::Value::Null),
                    )));

                    let name_for_events = name.clone();
                    let operation_id_for_events = operation_id.clone();
                    let entries_for_events = entries.clone();
                    LaneCommand::Commit {
                        writes,
                        next,
                        materialize: Box::new(move |_commit| {
                            Ok(OperationAdmission {
                                operation_id: operation_id.clone(),
                                kind: "run".to_string(),
                                started_at,
                            })
                        }),
                        events: Some(Box::new(move |commit| {
                            let mut events = vec![HarnessEvent::RunStart {
                                lane: name_for_events.clone(),
                                run_id: operation_id_for_events.clone(),
                                started_at,
                                recovery: None,
                            }];
                            events.extend(committed_entry_events(
                                &entries_for_events,
                                &commit,
                                &name_for_events,
                                Some(&operation_id_for_events),
                                0,
                            ));
                            if !selected_items.is_empty() {
                                events.push(HarnessEvent::QueueUpdate {
                                    lane: name_for_events.clone(),
                                    queues: remaining_queues.clone(),
                                    recovery: None,
                                });
                            }
                            events
                        })),
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))?
    }

    async fn accept_compaction(
        &self,
        request: &OperationRequest,
        operation_id: String,
        started_at: u64,
        acceptance_config: &Config,
        context: &Context,
    ) -> Result<OperationAdmission, HarnessError> {
        let OperationRequest::Compaction {
            custom_instructions,
            ..
        } = request
        else {
            unreachable!();
        };
        let task_id = self.session.id_generator().next(Some(started_at));
        let name = self.name.clone();
        let context_for_closure = context.clone();
        let acceptance_config_owned = acceptance_config.clone();
        let custom_instructions = custom_instructions.clone();

        self.command(
            Box::new(move |state, reader| {
                let name = name.clone();
                let context = context_for_closure.clone();
                let acceptance_config = acceptance_config_owned.clone();
                let operation_id = operation_id.clone();
                let task_id = task_id.clone();
                let custom_instructions = custom_instructions.clone();
                Box::pin(async move {
                    if let Some(op) = &state.operation {
                        return LaneCommand::Return {
                            result: Err(HarnessError::LaneBusy(LaneBusy::new(
                                name.clone(),
                                op.meta.operation_id.clone(),
                                intent_kind(&op.meta.intent).to_string(),
                                format!("Lane {name:?} already has an active operation"),
                            ))),
                        };
                    }
                    let path = match &state.tip_id {
                        None => Vec::new(),
                        Some(tip_id) => {
                            let scanned = reader
                                .scan_branch(
                                    StorageBranchScan {
                                        scan: BranchScan {
                                            stop_at_type: Some(EntryType::Compaction),
                                            order: Some(ScanOrder::NewestFirst),
                                            ..Default::default()
                                        },
                                        start: tip_id.clone(),
                                    },
                                    &context,
                                )
                                .await
                                .unwrap_or_else(|e| panic!("{e}"));
                            scanned.into_iter().rev().collect()
                        }
                    };
                    let prepared = prepare_compaction(&path, &acceptance_config.compaction)
                        .unwrap_or_else(|e| panic!("prepare_compaction: {e}"));
                    let Some(prepared) = prepared else {
                        return LaneCommand::Return {
                            result: Err(HarnessError::NothingToCompact(NothingToCompact::new(
                                name.clone(),
                                format!("Lane {name:?} has nothing to compact"),
                            ))),
                        };
                    };
                    let meta = OperationMeta {
                        operation_id: operation_id.clone(),
                        lane: name.clone(),
                        source_tip_id: state.tip_id.clone(),
                        started_at,
                        intent: OperationIntent::Compaction {
                            custom_instructions: custom_instructions.clone(),
                        },
                    };
                    let operation_state = OperationState::SummaryDeciding {
                        scope: OperationScope {
                            control: Control::Running,
                            settings: captured_settings(&acceptance_config),
                            latest_assistant_entry_id: None,
                        },
                        task: crate::harness::session::types::SummaryTask {
                            task_id: task_id.clone(),
                            reason: Some("manual".to_string()),
                            custom_instructions: custom_instructions.clone(),
                            boundary: crate::harness::session::types::ResultBoundary::Finish,
                        },
                    };
                    let writes = vec![
                        Write::Value(set_value(
                            &crate::harness::session::values::operation_preparation(
                                &operation_id,
                                &task_id,
                            ),
                            serde_json::to_value(durable_compaction_preparation(&prepared))
                                .unwrap_or(serde_json::Value::Null),
                        )),
                        Write::Value(set_value(
                            &operation_meta_value(&operation_id),
                            serde_json::to_value(&meta).unwrap_or(serde_json::Value::Null),
                        )),
                        Write::Value(set_value(
                            &operation_state_value(&operation_id),
                            serde_json::to_value(&operation_state)
                                .unwrap_or(serde_json::Value::Null),
                        )),
                        Write::Value(set_value(
                            &lane_state_value(&name),
                            serde_json::to_value(durable_lane_state(
                                Some(operation_id.clone()),
                                state.inbox.clone(),
                                state.last_operation_id.clone(),
                            ))
                            .unwrap_or(serde_json::Value::Null),
                        )),
                    ];
                    let next = LaneRuntimeState {
                        operation: Some(Operation {
                            meta: meta.clone(),
                            state: operation_state,
                        }),
                        ..state
                    };
                    let name_for_events = name.clone();
                    let operation_id_for_events = operation_id.clone();
                    LaneCommand::Commit {
                        writes,
                        next,
                        materialize: Box::new(move |_commit| {
                            Ok(OperationAdmission {
                                operation_id: operation_id.clone(),
                                kind: "compaction".to_string(),
                                started_at,
                            })
                        }),
                        events: Some(Box::new(move |_commit| {
                            vec![HarnessEvent::CompactionStart {
                                lane: name_for_events,
                                run_id: operation_id_for_events,
                                reason: "manual".to_string(),
                                started_at,
                                recovery: None,
                            }]
                        })),
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))?
    }

    async fn accept_navigation(
        &self,
        request: &OperationRequest,
        operation_id: String,
        started_at: u64,
        acceptance_config: &Config,
        context: &Context,
    ) -> Result<OperationAdmission, HarnessError> {
        let OperationRequest::Navigation {
            target_id,
            summarize,
            label,
            custom_instructions,
            ..
        } = request
        else {
            unreachable!();
        };
        let target_id = target_id.clone();
        let summarize = *summarize;
        let label = label.clone();
        let custom_instructions = custom_instructions.clone();
        let task_id = self.session.id_generator().next(Some(started_at));

        loop {
            let observed_tip_id = self.state().tip_id;
            let mut preparation = None;
            if summarize
                && let (Some(target_id_ref), Some(old_tip)) =
                    (target_id.as_ref(), observed_tip_id.as_ref())
            {
                let target = self
                    .session
                    .get_entries(std::slice::from_ref(target_id_ref), context)
                    .await
                    .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
                if target.contains_key(target_id_ref) {
                    let old_path = self
                        .session
                        .scan_branch(
                            StorageBranchScan {
                                scan: BranchScan {
                                    order: Some(ScanOrder::NewestFirst),
                                    ..Default::default()
                                },
                                start: old_tip.clone(),
                            },
                            context,
                        )
                        .await
                        .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
                    let target_path = self
                        .session
                        .scan_branch(
                            StorageBranchScan {
                                scan: BranchScan {
                                    order: Some(ScanOrder::NewestFirst),
                                    ..Default::default()
                                },
                                start: target_id_ref.clone(),
                            },
                            context,
                        )
                        .await
                        .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
                    let old_ids: std::collections::HashSet<String> =
                        old_path.iter().map(|e| e.id().to_string()).collect();
                    let common_ancestor_id = target_path
                        .iter()
                        .find(|e| old_ids.contains(e.id()))
                        .map(|e| e.id().to_string());
                    let end = match &common_ancestor_id {
                        None => old_path.len(),
                        Some(ancestor) => old_path
                            .iter()
                            .position(|e| e.id() == ancestor)
                            .unwrap_or(old_path.len()),
                    };
                    preparation = Some(prepare_branch_entries(
                        &old_path[..end].iter().rev().cloned().collect::<Vec<_>>(),
                        0,
                    ));
                }
            }

            let name = self.name.clone();
            let context_for_closure = context.clone();
            let acceptance_config_owned = acceptance_config.clone();
            let target_id_for_closure = target_id.clone();
            let label_for_closure = label.clone();
            let custom_instructions_for_closure = custom_instructions.clone();
            let task_id_for_closure = task_id.clone();
            let preparation_for_closure = preparation.clone();
            let operation_id_for_closure = operation_id.clone();

            let accepted = self
                .command(
                    Box::new(move |state, reader| {
                        let name = name.clone();
                        let context = context_for_closure.clone();
                        let acceptance_config = acceptance_config_owned.clone();
                        let operation_id = operation_id_for_closure.clone();
                        let target_id = target_id_for_closure.clone();
                        let label = label_for_closure.clone();
                        let custom_instructions = custom_instructions_for_closure.clone();
                        let task_id = task_id_for_closure.clone();
                        let preparation = preparation_for_closure.clone();
                        Box::pin(async move {
                            if let Some(op) = &state.operation {
                                return LaneCommand::Return {
                                    result: Err(HarnessError::LaneBusy(LaneBusy::new(
                                        name.clone(),
                                        op.meta.operation_id.clone(),
                                        intent_kind(&op.meta.intent).to_string(),
                                        format!("Lane {name:?} already has an active operation"),
                                    ))),
                                };
                            }
                            if state.tip_id != observed_tip_id {
                                return LaneCommand::Return { result: Ok(None) };
                            }
                            if target_id == state.tip_id {
                                return LaneCommand::Return {
                                    result: Err(HarnessError::InvalidNavigation(
                                        InvalidNavigation::new(
                                            name.clone(),
                                            "current_tip".to_string(),
                                            "Navigation target must differ from the current tip".to_string(),
                                        ),
                                    )),
                                };
                            }
                            if target_id.is_none() && label.is_some() {
                                return LaneCommand::Return {
                                    result: Err(HarnessError::InvalidNavigation(
                                        InvalidNavigation::new(
                                            name.clone(),
                                            "root_label".to_string(),
                                            "Root navigation cannot set a label".to_string(),
                                        ),
                                    )),
                                };
                            }
                            if summarize && (state.tip_id.is_none() || target_id.is_none()) {
                                return LaneCommand::Return {
                                    result: Err(HarnessError::InvalidNavigation(
                                        InvalidNavigation::new(
                                            name.clone(),
                                            if state.tip_id.is_none() { "source_root" } else { "target_root" }.to_string(),
                                            "Summarized navigation requires non-root source and target entries".to_string(),
                                        ),
                                    )),
                                };
                            }
                            if let Some(target_id) = &target_id {
                                let found = reader
                                    .get_entries(std::slice::from_ref(target_id), &context)
                                    .await
                                    .unwrap_or_else(|e| panic!("{e}"));
                                if !found.contains_key(target_id) {
                                    return LaneCommand::Return {
                                        result: Err(HarnessError::UnknownTarget(
                                            UnknownTarget::new(
                                                target_id.clone(),
                                                format!("Unknown target: {target_id}"),
                                            ),
                                        )),
                                    };
                                }
                            }

                            let intent = OperationIntent::Navigation {
                                target_id: target_id.clone(),
                                summarize,
                                label: label.clone(),
                                custom_instructions: custom_instructions.clone(),
                            };
                            let meta = OperationMeta {
                                operation_id: operation_id.clone(),
                                lane: name.clone(),
                                source_tip_id: state.tip_id.clone(),
                                started_at,
                                intent,
                            };
                            let scope = OperationScope {
                                control: Control::Running,
                                settings: captured_settings(&acceptance_config),
                                latest_assistant_entry_id: None,
                            };
                            let operation_state = if summarize {
                                let preparation = preparation.expect(
                                    "Validated summarized navigation is missing its preparation",
                                );
                                let writes = vec![Write::Value(set_value(
                                    &crate::harness::session::values::operation_preparation(
                                        &operation_id,
                                        &task_id,
                                    ),
                                    serde_json::to_value(durable_branch_preparation(&preparation))
                                        .unwrap_or(serde_json::Value::Null),
                                ))];
                                let state = OperationState::SummaryDeciding {
                                    scope,
                                    task: crate::harness::session::types::SummaryTask {
                                        task_id: task_id.clone(),
                                        reason: None,
                                        custom_instructions: custom_instructions.clone(),
                                        boundary:
                                            crate::harness::session::types::ResultBoundary::CommitNavigation {
                                                target_id: target_id.clone().unwrap(),
                                                label: label.clone(),
                                            },
                                    },
                                };
                                (state, writes)
                            } else {
                                (
                                    OperationState::NavigationReadyToCommit {
                                        scope,
                                        target_id: target_id.clone(),
                                        label: label.clone(),
                                    },
                                    Vec::new(),
                                )
                            };
                            let (operation_state, mut writes) = operation_state;
                            writes.push(Write::Value(set_value(
                                &operation_meta_value(&operation_id),
                                serde_json::to_value(&meta).unwrap_or(serde_json::Value::Null),
                            )));
                            writes.push(Write::Value(set_value(
                                &operation_state_value(&operation_id),
                                serde_json::to_value(&operation_state)
                                    .unwrap_or(serde_json::Value::Null),
                            )));
                            writes.push(Write::Value(set_value(
                                &lane_state_value(&name),
                                serde_json::to_value(durable_lane_state(
                                    Some(operation_id.clone()),
                                    state.inbox.clone(),
                                    state.last_operation_id.clone(),
                                ))
                                .unwrap_or(serde_json::Value::Null),
                            )));
                            let next = LaneRuntimeState {
                                operation: Some(Operation {
                                    meta: meta.clone(),
                                    state: operation_state,
                                }),
                                ..state
                            };
                            let name_for_events = name.clone();
                            let target_id_for_events = target_id.clone();
                            let operation_id_for_events = operation_id.clone();
                            LaneCommand::Commit {
                                writes,
                                next,
                                materialize: Box::new(move |_commit| {
                                    Ok(Some(OperationAdmission {
                                        operation_id: operation_id.clone(),
                                        kind: "navigation".to_string(),
                                        started_at,
                                    }))
                                }),
                                events: Some(Box::new(move |_commit| {
                                    vec![HarnessEvent::NavigationStart {
                                        lane: name_for_events,
                                        run_id: operation_id_for_events,
                                        target_id: target_id_for_events,
                                        started_at,
                                        recovery: None,
                                    }]
                                })),
                            }
                        })
                    }),
                    context,
                )
                .await
                .map_err(|e| HarnessError::Closed(Closed::new(e)))?;

            match accepted {
                Ok(Some(admission)) => return Ok(admission),
                Ok(None) => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn captured_model(&self, state: &OperationState) -> Option<ModelIdentity> {
        match state {
            OperationState::AssistantReady {
                generation_context, ..
            }
            | OperationState::AssistantEffectPending {
                generation_context, ..
            }
            | OperationState::AssistantRetryWait {
                generation_context, ..
            } => Some(generation_context.configuration.model.clone()),
            OperationState::Tools { batch, .. } => Some(batch.configuration.model.clone()),
            OperationState::DeferredSuspended { scope }
            | OperationState::DeferredEffectPending { scope, .. } => {
                Some(scope.configuration.model.clone())
            }
            OperationState::SummaryReady {
                summary_context, ..
            }
            | OperationState::SummaryEffectPending {
                summary_context, ..
            }
            | OperationState::SummaryRetryWait {
                summary_context, ..
            } => Some(summary_context.configuration.model.clone()),
            _ => None,
        }
    }

    /// 对应原版 `mismatch` helper。Rust 中 `command` 闭包为 `move`，无法持有 `&self`，
    /// 故 drive/abort 内联处使用等价的 `OperationMismatch::new` 构造；此 helper 保留以对齐
    /// 原版方法面。
    #[allow(dead_code)]
    fn mismatch(
        &self,
        expected: String,
        current: Option<String>,
        last: Option<String>,
    ) -> HarnessError {
        HarnessError::OperationMismatch(OperationMismatch::new(
            self.name.clone(),
            expected.clone(),
            current,
            last,
            format!("Operation {expected} does not own lane {:?}", self.name),
        ))
    }

    /// 对应 `readLane`：在 mutation 内读取 lane 投影。
    async fn read_lane<T, F>(&self, read: F, context: &Context) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(
                LaneRuntimeState,
                Arc<dyn SessionReader>,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send>>
            + Send
            + 'static,
    {
        self.assert_open()?;
        let session = Arc::clone(&self.session);
        let inner = Arc::clone(&self.inner);
        let context = context.clone();
        let read = Arc::new(Mutex::new(Some(read)));
        let mutation: SessionMutationCallback<T> = Arc::new(move |reader, _ctx| {
            let read = Arc::clone(&read);
            let inner = Arc::clone(&inner);
            Box::pin(async move {
                let read = read
                    .lock()
                    .unwrap()
                    .take()
                    .expect("read_lane callback already consumed");
                let state = inner.lock().unwrap().state.clone();
                read(state, reader).await
            })
        });
        crate::harness::session::types::mutate(session.as_ref(), mutation, &context).await
    }

    /// 对应 `captureLaneSnapshot`。
    async fn capture_lane_snapshot_inner(
        &self,
        state: &LaneRuntimeState,
        reader: &dyn SessionReader,
        context: &Context,
    ) -> Result<LaneSnapshot, String> {
        let transcript: Vec<Entry> = match &state.tip_id {
            None => Vec::new(),
            Some(tip_id) => {
                let scanned = reader
                    .scan_branch(
                        StorageBranchScan {
                            scan: BranchScan {
                                stop_at_type: Some(EntryType::Compaction),
                                order: Some(ScanOrder::NewestFirst),
                                ..Default::default()
                            },
                            start: tip_id.clone(),
                        },
                        context,
                    )
                    .await?;
                scanned.into_iter().rev().collect()
            }
        };
        let queues = read_lane_queues(reader, &state.inbox, context).await?;
        let last_result = match &state.last_operation_id {
            None => None,
            Some(id) => {
                let stored = reader
                    .get_value(&operation_result_value(id).erased(), context)
                    .await?
                    .ok_or_else(|| {
                        SessionInvariantError::new(format!(
                            "Lane {:?} is missing result {id}",
                            self.name
                        ))
                        .to_string()
                    })?;
                Some(serde_json::from_value(stored.value).map_err(|e| e.to_string())?)
            }
        };
        let stats = reader.get_stats(context).await?;
        let operation_snapshot = match &state.operation {
            None => None,
            Some(op) => {
                let mut running_tools: Vec<LaneSnapshotTool> = Vec::new();
                let mut streaming_message = None;
                let mut retry = None;
                let mut deferred = None;
                match &op.state {
                    OperationState::AssistantRetryWait {
                        generation_context,
                        next_attempt,
                        not_before,
                        ..
                    } => {
                        retry = Some(RetrySnapshot {
                            attempt: *next_attempt,
                            max_attempts: generation_context.retry_policy.max_attempts,
                            next_attempt_at: *not_before,
                        });
                    }
                    OperationState::AssistantEffectPending {
                        response_entry_id, ..
                    } => {
                        streaming_message = reduce_assistant_message_frames(
                            read_assistant_frames(
                                reader,
                                &op.meta.operation_id,
                                response_entry_id,
                                context,
                            )
                            .await?,
                        );
                    }
                    OperationState::DeferredSuspended { scope }
                    | OperationState::DeferredEffectPending { scope, .. } => {
                        let source = reader
                            .get_entries(std::slice::from_ref(&scope.source_entry_id), context)
                            .await?
                            .get(&scope.source_entry_id)
                            .cloned();
                        if let Some(Entry::Message(m)) = &source
                            && let AgentMessage::Assistant(a) = &m.message
                            && let Some(handle) = &a.deferred
                        {
                            deferred = Some(DeferredSnapshot {
                                handle: handle.clone(),
                                poll: scope.poll,
                            });
                        }
                        if let OperationState::DeferredEffectPending {
                            response_entry_id, ..
                        } = &op.state
                        {
                            streaming_message = reduce_assistant_message_frames(
                                read_assistant_frames(
                                    reader,
                                    &op.meta.operation_id,
                                    response_entry_id,
                                    context,
                                )
                                .await?,
                            );
                        }
                    }
                    OperationState::Tools { batch, .. } => {
                        let assistant = reader
                            .get_entries(std::slice::from_ref(&batch.assistant_entry_id), context)
                            .await?
                            .get(&batch.assistant_entry_id)
                            .cloned();
                        if let Some(Entry::Message(m)) = &assistant
                            && let AgentMessage::Assistant(a) = &m.message
                        {
                            for call in &batch.calls {
                                if matches!(
                                    call,
                                    crate::harness::session::types::ToolCall::Planned { .. }
                                        | crate::harness::session::types::ToolCall::Completed { .. }
                                ) {
                                    continue;
                                }
                                let idx = call.source_index();
                                let Some(pi_ai::ContentBlock::ToolCall(block)) = a.content.get(idx)
                                else {
                                    panic!(
                                        "{}",
                                        SessionInvariantError::new(format!(
                                            "Tool call source index {idx} does not name a tool-call block"
                                        ))
                                    );
                                };
                                let args = reader
                                    .get_value(
                                        &operation_tool_args(
                                            &op.meta.operation_id,
                                            &batch.turn_id,
                                            idx,
                                        ),
                                        context,
                                    )
                                    .await?;
                                match call {
                                    crate::harness::session::types::ToolCall::EffectPending {
                                        ..
                                    } => {
                                        let checkpoint = reader
                                            .get_value(
                                                &pending_tool_output(
                                                    &op.meta.operation_id,
                                                    &call.result_entry_id(),
                                                )
                                                .erased(),
                                                context,
                                            )
                                            .await?;
                                        running_tools.push(LaneSnapshotTool::Running {
                                            tool_call_id: block.id.clone(),
                                            tool_name: block.name.clone(),
                                            args: args
                                                .map(|v| v.value)
                                                .unwrap_or_else(|| block.arguments.clone()),
                                            result: checkpoint
                                                .and_then(|v| serde_json::from_value(v.value).ok()),
                                        });
                                    }
                                    crate::harness::session::types::ToolCall::OutcomeReady {
                                        terminate,
                                        ..
                                    } => {
                                        let staged = reader
                                            .get_value(
                                                &pending_entry(&call.result_entry_id()).erased(),
                                                context,
                                            )
                                            .await?;
                                        if let Some(staged) = staged
                                            && let Ok(pending) =
                                                serde_json::from_value::<PendingEntry>(staged.value)
                                            && let PendingEntry::Message {
                                                payload: AgentMessage::ToolResult(tr),
                                            } = pending
                                        {
                                            running_tools.push(LaneSnapshotTool::Settled {
                                                tool_call_id: block.id.clone(),
                                                tool_name: block.name.clone(),
                                                args: args
                                                    .map(|v| v.value)
                                                    .unwrap_or_else(|| block.arguments.clone()),
                                                result: tool_result_from_message(&tr, *terminate),
                                                is_error: tr.is_error,
                                            });
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    OperationState::SummaryRetryWait {
                        summary_context,
                        next_attempt,
                        not_before,
                        ..
                    } => {
                        retry = Some(RetrySnapshot {
                            attempt: *next_attempt,
                            max_attempts: summary_context.retry_policy.max_attempts,
                            next_attempt_at: *not_before,
                        });
                    }
                    _ => {}
                }
                Some(LaneOperationSnapshot {
                    id: op.meta.operation_id.clone(),
                    kind: intent_kind(&op.meta.intent).to_string(),
                    started_at: op.meta.started_at,
                    from_tip_id: op.meta.source_tip_id.clone(),
                    status: if matches!(state_control(&op.state), Control::CancelRequested { .. }) {
                        "aborting".to_string()
                    } else {
                        "open".to_string()
                    },
                    retry,
                    deferred,
                    streaming_message,
                    running_tools,
                })
            }
        };
        Ok(LaneSnapshot {
            lane: self.name.clone(),
            transcript,
            tip_id: state.tip_id.clone(),
            last_result,
            configuration: state.configuration.clone(),
            stats,
            operation: operation_snapshot,
            queues,
            faulted: self.inner.lock().unwrap().closed_error.is_some(),
        })
    }

    async fn drive_run_request(&self, request: OperationRequest, context: &Context) -> RunResult {
        let admission = self.accept(request, context).await;
        let admission = match admission {
            Ok(a) => a,
            Err(e) => {
                if matches!(
                    e,
                    HarnessError::LaneBusy(_)
                        | HarnessError::InvalidMessage(_)
                        | HarnessError::UnknownSkill(_)
                        | HarnessError::UnknownTemplate(_)
                        | HarnessError::Closed(_)
                ) {
                    return Err(e);
                }
                panic!(
                    "{}",
                    SessionInvariantError::new(format!("Run acceptance returned {e:?}"))
                );
            }
        };
        self.drive_run_result(admission.operation_id, true, context)
            .await
    }

    async fn drive_run_result(
        &self,
        operation_id: String,
        wait_for_retry: bool,
        context: &Context,
    ) -> RunResult {
        let driven = self
            .drive(
                DriveOptions {
                    operation_id: operation_id.clone(),
                    wait_for_retry,
                    poll_deferred: false,
                },
                context,
            )
            .await;
        let driven = match driven {
            Ok(d) => d,
            Err(e) => {
                if matches!(e, HarnessError::Closed(_)) {
                    return Err(e);
                }
                panic!(
                    "{}",
                    SessionInvariantError::new(format!(
                        "Accepted run {operation_id} no longer matches its lane"
                    ))
                );
            }
        };
        match driven {
            DriveOutcome::Settled { outcome } => Ok(RunSettlement::Settled(outcome)),
            DriveOutcome::WaitingDeferred { deferred, .. } => {
                Ok(RunSettlement::Suspended(SuspendedRun {
                    operation_id,
                    deferred,
                }))
            }
            DriveOutcome::WaitingRetry { .. } => panic!(
                "{}",
                SessionInvariantError::new(format!(
                    "Run {operation_id} returned an unwaited retry"
                ))
            ),
        }
    }

    async fn drive_structural_admission(
        &self,
        admission: &OperationAdmission,
        context: &Context,
    ) -> Result<OperationResultRecord, HarnessError> {
        let driven = self
            .drive(
                DriveOptions {
                    operation_id: admission.operation_id.clone(),
                    wait_for_retry: true,
                    poll_deferred: false,
                },
                context,
            )
            .await;
        let driven = match driven {
            Ok(d) => d,
            Err(e) => {
                if matches!(e, HarnessError::Closed(_)) {
                    return Err(e);
                }
                panic!(
                    "{}",
                    SessionInvariantError::new(format!(
                        "Accepted {} {} no longer matches its lane",
                        admission.kind, admission.operation_id
                    ))
                );
            }
        };
        match driven {
            DriveOutcome::Settled { outcome } => Ok(outcome),
            other => panic!(
                "{}",
                SessionInvariantError::new(format!(
                    "{} {} returned {other:?}",
                    admission.kind, admission.operation_id
                ))
            ),
        }
    }

    async fn continue_after_structural(
        &self,
        record: &OperationResultRecord,
        context: &Context,
    ) -> Result<Option<RunSettlement>, HarnessError> {
        if record.status == crate::harness::session::types::TerminalStatus::Aborted {
            return Ok(None);
        }
        let admission = self
            .accept(
                OperationRequest::Prompt {
                    operation_id: None,
                    prompt: serde_json::json!(""),
                    images: None,
                },
                context,
            )
            .await;
        let admission = match admission {
            Ok(a) => a,
            Err(e) => {
                if matches!(
                    e,
                    HarnessError::InvalidMessage(_)
                        | HarnessError::LaneBusy(_)
                        | HarnessError::Closed(_)
                ) {
                    return Ok(None);
                }
                if matches!(e, HarnessError::Closed(_)) {
                    return Err(e);
                }
                panic!(
                    "{}",
                    SessionInvariantError::new(format!(
                        "Structural continuation acceptance returned {e:?}"
                    ))
                );
            }
        };
        let result = self
            .drive_run_result(admission.operation_id, true, context)
            .await?;
        Ok(Some(result))
    }

    async fn enqueue(
        &self,
        kind: InboxItemKind,
        input: serde_json::Value,
        images: Option<Vec<ImageContent>>,
        context: &Context,
    ) -> QueueResult {
        if let Some(error) = &self.inner.lock().unwrap().closed_error {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let at = pi_ai::utils::uuid::now_ms() as u64;
        let message: AgentMessage = if input.is_string() {
            let text = input.as_str().unwrap();
            let images = images.clone().unwrap_or_default();
            if text.is_empty() && images.is_empty() {
                return Err(HarnessError::InvalidMessage(InvalidMessage::new(
                    self.name.clone(),
                    "empty".to_string(),
                    "Queued input must contain text or an image".to_string(),
                )));
            }
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(serde_json::json!({ "type": "text", "text": text }));
            }
            for image in &images {
                content.push(serde_json::to_value(image).unwrap_or(serde_json::Value::Null));
            }
            serde_json::from_value(
                serde_json::json!({ "role": "user", "content": content, "timestamp": at }),
            )
            .unwrap_or_else(|e| panic!("construct queue message: {e}"))
        } else {
            serde_json::from_value(input.clone())
                .unwrap_or_else(|e| panic!("parse queue message: {e}"))
        };

        let entry_id = self.session.id_generator().next(Some(at));
        let name = self.name.clone();
        let context_for_closure = context.clone();
        self.command(
            Box::new(move |state, reader| {
                let entry_id = entry_id.clone();
                let name = name.clone();
                let context = context_for_closure.clone();
                let kind = kind;
                let message = message.clone();
                Box::pin(async move {
                    let mut inbox = state.inbox.clone();
                    inbox.push(InboxItem {
                        entry_id: entry_id.clone(),
                        kind,
                    });
                    let queues = read_lane_queues(reader.as_ref(), &inbox, &context)
                        .await
                        .unwrap_or_else(|e| panic!("{e}"));
                    let mut writes = vec![Write::Value(set_value(
                        &pending_entry(&entry_id),
                        serde_json::to_value(&PendingEntry::Message {
                            payload: message.clone(),
                        })
                        .unwrap_or(serde_json::Value::Null),
                    ))];
                    writes.push(Write::Value(set_value(
                        &lane_state_value(&name),
                        serde_json::to_value(durable_lane_state(
                            state
                                .operation
                                .as_ref()
                                .map(|o| o.meta.operation_id.clone()),
                            inbox.clone(),
                            state.last_operation_id.clone(),
                        ))
                        .unwrap_or(serde_json::Value::Null),
                    )));
                    let next = LaneRuntimeState { inbox, ..state };
                    LaneCommand::Commit {
                        writes,
                        next,
                        materialize: Box::new(move |_| {
                            Ok(QueueEntry {
                                entry_id: entry_id.clone(),
                            })
                        }),
                        events: Some(Box::new(move |_| {
                            vec![HarnessEvent::QueueUpdate {
                                lane: name.clone(),
                                queues: queues.clone(),
                                recovery: None,
                            }]
                        })),
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))?
    }

    async fn append(
        &self,
        pending: PendingEntry,
        context: &Context,
    ) -> Result<String, HarnessError> {
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        if let PendingEntry::Message { payload } = &pending
            && let AgentMessage::Assistant(a) = payload
            && a.stop_reason == pi_ai::StopReason::Pending
        {
            return Err(HarnessError::InvalidMessage(InvalidMessage::new(
                self.name.clone(),
                "pending_assistant".to_string(),
                "Cannot append a pending assistant message".to_string(),
            )));
        }
        let id = self.session.id_generator().next(None);
        let name = self.name.clone();
        let context_for_closure = context.clone();
        self.command(
            Box::new(move |state, reader| {
                let id = id.clone();
                let name = name.clone();
                let context = context_for_closure.clone();
                let pending = pending.clone();
                Box::pin(async move {
                    if state.operation.is_none() {
                        let mut entries = Vec::new();
                        for item in state
                            .inbox
                            .iter()
                            .filter(|i| i.kind == InboxItemKind::Write)
                        {
                            let stored = reader
                                .get_value(&pending_entry(&item.entry_id).erased(), &context)
                                .await
                                .unwrap_or_else(|e| panic!("{e}"));
                            let stored = stored.unwrap_or_else(|| {
                                panic!(
                                    "{}",
                                    SessionInvariantError::new(format!(
                                        "Pending write {} is missing its payload",
                                        item.entry_id
                                    ))
                                )
                            });
                            let parsed: PendingEntry = serde_json::from_value(stored.value)
                                .unwrap_or_else(|e| panic!("parse pending write: {e}"));
                            entries.push(pending_entry_write(item.entry_id.clone(), parsed));
                        }
                        entries.push(pending_entry_write(id.clone(), pending.clone()));
                        let chained = chain_new_entries(state.tip_id.clone(), entries);
                        let inbox: Vec<InboxItem> = state
                            .inbox
                            .iter()
                            .filter(|i| i.kind != InboxItemKind::Write)
                            .cloned()
                            .collect();
                        let mut writes: Vec<Write> =
                            chained.iter().map(|e| insert_entry(e.clone())).collect();
                        for item in state
                            .inbox
                            .iter()
                            .filter(|i| i.kind == InboxItemKind::Write)
                        {
                            writes.push(Write::Value(delete_value(&pending_entry(&item.entry_id))));
                        }
                        writes.push(Write::Value(set_value(
                            &branch_tip(&name),
                            serde_json::json!(id),
                        )));
                        writes.push(Write::Value(set_value(
                            &lane_state_value(&name),
                            serde_json::to_value(durable_lane_state(
                                None,
                                inbox.clone(),
                                state.last_operation_id.clone(),
                            ))
                            .unwrap_or(serde_json::Value::Null),
                        )));
                        let next = LaneRuntimeState {
                            tip_id: Some(id.clone()),
                            inbox,
                            ..state
                        };
                        return LaneCommand::Commit {
                            writes,
                            next,
                            materialize: Box::new(move |_| id.clone()),
                            events: Some(Box::new(move |commit| {
                                committed_entry_events(&chained, &commit, &name, None, 0)
                            })),
                        };
                    }

                    let mut inbox = state.inbox.clone();
                    inbox.push(InboxItem {
                        entry_id: id.clone(),
                        kind: InboxItemKind::Write,
                    });
                    let queues = read_lane_queues(reader.as_ref(), &inbox, &context)
                        .await
                        .unwrap_or_else(|e| panic!("{e}"));
                    let mut writes = vec![Write::Value(set_value(
                        &pending_entry(&id),
                        serde_json::to_value(&pending).unwrap_or(serde_json::Value::Null),
                    ))];
                    writes.push(Write::Value(set_value(
                        &lane_state_value(&name),
                        serde_json::to_value(durable_lane_state(
                            state
                                .operation
                                .as_ref()
                                .map(|o| o.meta.operation_id.clone()),
                            inbox.clone(),
                            state.last_operation_id.clone(),
                        ))
                        .unwrap_or(serde_json::Value::Null),
                    )));
                    let next = LaneRuntimeState { inbox, ..state };
                    LaneCommand::Commit {
                        writes,
                        next,
                        materialize: Box::new(move |_| id.clone()),
                        events: Some(Box::new(move |_| {
                            vec![HarnessEvent::QueueUpdate {
                                lane: name.clone(),
                                queues: queues.clone(),
                                recovery: None,
                            }]
                        })),
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))
    }

    async fn set_configuration_identity(
        &self,
        identity: ModelIdentity,
        context: &Context,
    ) -> Result<(), HarnessError> {
        let name = self.name.clone();
        let _context_for_closure = context.clone();
        self.command(
            Box::new(move |state, _reader| {
                let name = name.clone();
                Box::pin(async move {
                    let previous = state.configuration.clone();
                    let mut configuration = previous.clone();
                    configuration.model = identity.clone();
                    let next = LaneRuntimeState {
                        configuration: configuration.clone(),
                        ..state
                    };
                    let writes = vec![Write::Value(set_value(
                        &lane_config_value(&name),
                        serde_json::to_value(&configuration).unwrap_or(serde_json::Value::Null),
                    ))];
                    let identity_for_event = identity.clone();
                    let previous_model = previous.model.clone();
                    LaneCommand::Commit {
                        writes,
                        next,
                        materialize: Box::new(|_| ()),
                        events: Some(Box::new(move |_| {
                            vec![HarnessEvent::ConfigUpdate {
                                lane: Some(name.clone()),
                                payload: ConfigUpdatePayload::Model {
                                    value: identity_for_event.clone(),
                                    previous: serde_json::to_value(&previous_model)
                                        .unwrap_or(serde_json::Value::Null),
                                },
                                recovery: None,
                            }]
                        })),
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl AgentLane for LaneImpl {
    fn name(&self) -> &str {
        &self.name
    }

    async fn accept(
        &self,
        request: OperationRequest,
        context: &Context,
    ) -> Result<OperationAdmission, HarnessError> {
        if let Some(error) = &self.inner.lock().unwrap().closed_error {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let started_at = pi_ai::utils::uuid::now_ms() as u64;
        let operation_id = match &request {
            OperationRequest::Prompt { operation_id, .. }
            | OperationRequest::Skill { operation_id, .. }
            | OperationRequest::PromptTemplate { operation_id, .. }
            | OperationRequest::Compaction { operation_id, .. }
            | OperationRequest::Navigation { operation_id, .. } => operation_id.clone(),
        };
        let operation_id =
            operation_id.unwrap_or_else(|| self.session.id_generator().next(Some(started_at)));
        let acceptance_config = self.read_config();
        match request {
            OperationRequest::Compaction { .. } => {
                self.accept_compaction(
                    &request,
                    operation_id,
                    started_at,
                    &acceptance_config,
                    context,
                )
                .await
            }
            OperationRequest::Navigation { .. } => {
                self.accept_navigation(
                    &request,
                    operation_id,
                    started_at,
                    &acceptance_config,
                    context,
                )
                .await
            }
            _ => {
                self.accept_run(
                    &request,
                    operation_id,
                    started_at,
                    &acceptance_config,
                    context,
                )
                .await
            }
        }
    }

    async fn drive(&self, options: DriveOptions, context: &Context) -> DriveResult {
        if let Some(error) = &self.inner.lock().unwrap().closed_error {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;

        loop {
            let claim = self.command_drive_claim(&options, context).await?;
            match claim {
                DriveClaim::Settled { outcome } => return Ok(DriveOutcome::Settled { outcome }),
                DriveClaim::Mismatch { error } => return Err(error),
                DriveClaim::Occupied { drive } => {
                    let completion = await_with_context(drive.completion_wait(context), context)
                        .await
                        .map_err(|_| HarnessError::Closed(Closed::new("aborted".to_string())))?;
                    completion.map_err(|e| HarnessError::Closed(Closed::new(e)))?;
                    continue;
                }
                DriveClaim::Observe { drive, installed } => {
                    if installed {
                        let lane_arc = self.self_arc();
                        let inner = Arc::clone(&self.inner);
                        let notify = Arc::clone(&self.notify);
                        let on_fault = self.on_fault.clone();
                        let drive_clone = drive.clone();
                        let drive_ctx = drive.context.clone();
                        tokio::spawn(async move {
                            let outcome = drive_operation(lane_arc, &drive_clone).await;
                            match outcome {
                                Ok(outcome) => drive_clone.settle(outcome),
                                Err(error) => {
                                    let failure = on_fault(error.to_string(), drive_ctx);
                                    drive_clone.fail(failure);
                                }
                            }
                            let mut guard = inner.lock().unwrap();
                            if guard
                                .active_drive
                                .as_ref()
                                .map(|d| d.operation_id == drive_clone.operation_id)
                                .unwrap_or(false)
                            {
                                guard.active_drive = None;
                            }
                            guard.generation += 1;
                            drop(guard);
                            notify.notify_waiters();
                        });
                    }
                    let outcome = drive
                        .completion_wait(context)
                        .await
                        .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
                    return Ok(outcome);
                }
            }
        }
    }

    async fn request_abort(
        &self,
        operation_id: String,
        context: &Context,
    ) -> Result<serde_json::Value, HarnessError> {
        if let Some(error) = &self.inner.lock().unwrap().closed_error {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;

        let active_drive = {
            let inner = self.inner.lock().unwrap();
            inner
                .active_drive
                .as_ref()
                .filter(|d| d.operation_id == operation_id)
                .cloned()
        };

        let cancel_notify = Arc::new(Notify::new());
        let cancellation: crate::harness::execution::effect_gate::Cancellation = Arc::new({
            let cancel_notify = Arc::clone(&cancel_notify);
            move || {
                let cancel_notify = Arc::clone(&cancel_notify);
                Box::pin(async move {
                    cancel_notify.notified().await;
                })
            }
        });
        if let Some(drive) = &active_drive {
            drive.begin_abort(cancellation);
        }

        let settle_gate: Arc<dyn Fn(bool) + Send + Sync> = {
            let gate_settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cancel_notify = Arc::clone(&cancel_notify);
            let active_drive = active_drive.clone();
            Arc::new(move |signal: bool| {
                if gate_settled.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                cancel_notify.notify_waiters();
                if signal && let Some(drive) = &active_drive {
                    drive.signal_abort();
                }
            })
        };

        let operation_id_clone = operation_id.clone();
        let name = self.name.clone();
        let context_for_closure = context.clone();
        let settle_gate_after = Arc::clone(&settle_gate);

        let result = self
            .command(
                Box::new(move |state, reader| {
                    let operation_id = operation_id_clone.clone();
                    let name = name.clone();
                    let context = context_for_closure.clone();
                    Box::pin(async move {
                        let operation = state.operation.clone();
                        let Some(operation) = operation else {
                            return LaneCommand::Return {
                                result: Err(HarnessError::OperationMismatch(
                                    OperationMismatch::new(
                                        name.clone(),
                                        operation_id.clone(),
                                        None,
                                        state.last_operation_id,
                                        format!(
                                            "Operation {operation_id} does not own lane {name:?}"
                                        ),
                                    ),
                                )),
                            };
                        };
                        if operation.meta.operation_id != operation_id {
                            return LaneCommand::Return {
                                result: Err(HarnessError::OperationMismatch(
                                    OperationMismatch::new(
                                        name.clone(),
                                        operation_id.clone(),
                                        Some(operation.meta.operation_id),
                                        state.last_operation_id,
                                        format!(
                                            "Operation {operation_id} does not own lane {name:?}"
                                        ),
                                    ),
                                )),
                            };
                        }
                        if matches!(
                            state_control(&operation.state),
                            Control::CancelRequested { .. }
                        ) {
                            return LaneCommand::Return {
                                result: Ok(serde_json::json!({
                                    "operationId": operation_id,
                                    "newlyRequested": false,
                                    "steer": [],
                                    "followUp": [],
                                })),
                            };
                        }

                        let removed: Vec<InboxItem> = state
                            .inbox
                            .iter()
                            .filter(|item| {
                                item.kind == InboxItemKind::Steer
                                    || item.kind == InboxItemKind::FollowUp
                            })
                            .cloned()
                            .collect();
                        let mut steer = Vec::new();
                        let mut follow_up = Vec::new();
                        for item in &removed {
                            let stored = reader
                                .get_value(&pending_entry(&item.entry_id).erased(), &context)
                                .await
                                .unwrap_or_else(|e| panic!("{e}"));
                            let stored = stored.unwrap_or_else(|| {
                                panic!(
                                    "{}",
                                    SessionInvariantError::new(format!(
                                        "Pending {:?} entry {} is missing its message",
                                        item.kind, item.entry_id
                                    ))
                                )
                            });
                            let pending: PendingEntry = serde_json::from_value(stored.value)
                                .unwrap_or_else(|e| panic!("parse pending entry: {e}"));
                            if let PendingEntry::Message { payload } = pending {
                                if item.kind == InboxItemKind::Steer {
                                    steer.push(payload);
                                } else {
                                    follow_up.push(payload);
                                }
                            }
                        }
                        let removed_ids: std::collections::HashSet<String> =
                            removed.iter().map(|i| i.entry_id.clone()).collect();
                        let inbox: Vec<InboxItem> = state
                            .inbox
                            .iter()
                            .filter(|item| !removed_ids.contains(&item.entry_id))
                            .cloned()
                            .collect();
                        let queues = read_lane_queues(reader.as_ref(), &inbox, &context)
                            .await
                            .unwrap_or_else(|e| panic!("{e}"));
                        let operation_state = with_cancel_requested(
                            operation.state.clone(),
                            pi_ai::utils::uuid::now_ms() as u64,
                        );
                        let mut writes: Vec<Write> = Vec::new();
                        for item in &removed {
                            writes.push(Write::Value(delete_value(&pending_entry(&item.entry_id))));
                        }
                        writes.push(Write::Value(set_value(
                            &operation_state_value(&operation.meta.operation_id),
                            serde_json::to_value(&operation_state)
                                .unwrap_or(serde_json::Value::Null),
                        )));
                        writes.push(Write::Value(set_value(
                            &lane_state_value(&name),
                            serde_json::to_value(durable_lane_state(
                                Some(operation.meta.operation_id.clone()),
                                inbox.clone(),
                                state.last_operation_id.clone(),
                            ))
                            .unwrap_or(serde_json::Value::Null),
                        )));
                        let next = LaneRuntimeState {
                            inbox,
                            operation: Some(Operation {
                                meta: operation.meta,
                                state: operation_state,
                            }),
                            ..state
                        };
                        let steer_for_materialize = steer.clone();
                        let follow_up_for_materialize = follow_up.clone();
                        let name_for_events = name.clone();
                        let operation_id_for_events = operation_id.clone();
                        let settle_gate_for_materialize = Arc::clone(&settle_gate);
                        LaneCommand::Commit {
                            writes,
                            next,
                            materialize: Box::new(move |_commit| {
                                settle_gate_for_materialize(true);
                                Ok(serde_json::json!({
                                    "operationId": operation_id,
                                    "newlyRequested": true,
                                    "steer": steer_for_materialize,
                                    "followUp": follow_up_for_materialize,
                                }))
                            }),
                            events: Some(Box::new(move |_commit| {
                                let mut events = vec![HarnessEvent::OperationAbort {
                                    lane: name_for_events.clone(),
                                    operation_id: operation_id_for_events,
                                    steer,
                                    follow_up,
                                    recovery: None,
                                }];
                                if !removed.is_empty() {
                                    events.push(HarnessEvent::QueueUpdate {
                                        lane: name_for_events,
                                        queues,
                                        recovery: None,
                                    });
                                }
                                events
                            })),
                        }
                    })
                }),
                context,
            )
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;

        let newly_requested = result
            .as_ref()
            .ok()
            .and_then(|v| v.get("newlyRequested").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        settle_gate_after(!newly_requested);
        result
    }

    async fn get_tip_id(&self, _context: &Context) -> Result<Option<String>, HarnessError> {
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        Ok(self.state().tip_id)
    }

    async fn find_entries(
        &self,
        query: Option<BranchScan>,
        context: &Context,
    ) -> Result<Vec<Entry>, HarnessError> {
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let query = query.unwrap_or_default();
        let start = query.start.clone().or(self.state().tip_id);
        let Some(start) = start else {
            return Ok(Vec::new());
        };
        let scan = StorageBranchScan {
            scan: BranchScan {
                order: Some(query.order.unwrap_or(ScanOrder::NewestFirst)),
                ..query
            },
            start,
        };
        self.session
            .scan_branch(scan, context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))
    }

    async fn find_entry(
        &self,
        query: Option<BranchScan>,
        context: &Context,
    ) -> Result<Option<Entry>, HarnessError> {
        let mut query = query.unwrap_or_default();
        query.limit = Some(query.limit.map(|l| l.min(1)).unwrap_or(1));
        Ok(self
            .find_entries(Some(query), context)
            .await?
            .into_iter()
            .next())
    }

    async fn append_message(
        &self,
        message: AgentMessage,
        context: &Context,
    ) -> Result<String, HarnessError> {
        self.append(PendingEntry::Message { payload: message }, context)
            .await
    }

    async fn append_custom_entry(
        &self,
        custom_type: String,
        data: Option<serde_json::Value>,
        context: &Context,
    ) -> Result<String, HarnessError> {
        self.append(
            PendingEntry::Custom {
                custom_type,
                payload: data,
            },
            context,
        )
        .await
    }

    async fn get_result(
        &self,
        operation_id: String,
        context: &Context,
    ) -> Result<Option<OperationResultRecord>, HarnessError> {
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let stored = self
            .session
            .get_value(&operation_result_value(&operation_id).erased(), context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        Ok(stored.and_then(|v| serde_json::from_value(v.value).ok()))
    }

    async fn inspect_execution(
        &self,
        _context: &Context,
    ) -> Result<serde_json::Value, HarnessError> {
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let state = self.state();
        let operation = state.operation;
        let captured = operation
            .as_ref()
            .and_then(|op| self.captured_model(&op.state));
        let current = operation.map(|op| serde_json::json!({
            "id": op.meta.operation_id,
            "kind": intent_kind(&op.meta.intent),
            "status": if matches!(state_control(&op.state), Control::CancelRequested { .. }) { "aborting" } else { "open" },
            "startedAt": op.meta.started_at,
            "capturedModel": captured,
        }));
        Ok(serde_json::json!({
            "lane": self.name,
            "tipId": state.tip_id,
            "configuredModel": state.configuration.model,
            "current": current,
            "lastOperationId": state.last_operation_id,
        }))
    }

    async fn prompt(
        &self,
        prompt: serde_json::Value,
        images: Option<Vec<pi_ai::ImageContent>>,
        context: &Context,
    ) -> RunResult {
        self.drive_run_request(
            OperationRequest::Prompt {
                operation_id: None,
                prompt,
                images,
            },
            context,
        )
        .await
    }

    async fn skill(
        &self,
        name: String,
        additional_instructions: Option<String>,
        context: &Context,
    ) -> RunResult {
        self.drive_run_request(
            OperationRequest::Skill {
                operation_id: None,
                name,
                additional_instructions,
            },
            context,
        )
        .await
    }

    async fn prompt_from_template(
        &self,
        name: String,
        args: Option<Vec<String>>,
        context: &Context,
    ) -> RunResult {
        self.drive_run_request(
            OperationRequest::PromptTemplate {
                operation_id: None,
                name,
                args,
            },
            context,
        )
        .await
    }

    async fn compact(
        &self,
        custom_instructions: Option<String>,
        context: &Context,
    ) -> CompactionResult {
        let admission = self
            .accept(
                OperationRequest::Compaction {
                    operation_id: None,
                    custom_instructions,
                },
                context,
            )
            .await;
        let admission = match admission {
            Ok(a) => a,
            Err(e) => {
                if matches!(
                    e,
                    HarnessError::LaneBusy(_)
                        | HarnessError::NothingToCompact(_)
                        | HarnessError::Closed(_)
                ) {
                    return Err(e);
                }
                panic!(
                    "{}",
                    SessionInvariantError::new(format!("Compaction acceptance returned {e:?}"))
                );
            }
        };
        let compacted = self.drive_structural_admission(&admission, context).await?;
        let continuation = self.continue_after_structural(&compacted, context).await?;
        Ok(CompactionSettlement {
            compaction: compacted,
            run: continuation,
        })
    }

    async fn navigate_tree(
        &self,
        target_id: Option<String>,
        options: Option<NavigateOptions>,
        context: &Context,
    ) -> NavigationResult {
        let options = options.unwrap_or_default();
        let summarize = options.summarize.unwrap_or(false);
        let admission = self
            .accept(
                OperationRequest::Navigation {
                    operation_id: None,
                    target_id,
                    summarize,
                    label: options.label,
                    custom_instructions: options.custom_instructions,
                },
                context,
            )
            .await;
        let admission = match admission {
            Ok(a) => a,
            Err(e) => {
                if matches!(
                    e,
                    HarnessError::LaneBusy(_)
                        | HarnessError::InvalidNavigation(_)
                        | HarnessError::UnknownTarget(_)
                        | HarnessError::Closed(_)
                ) {
                    return Err(e);
                }
                panic!(
                    "{}",
                    SessionInvariantError::new(format!("Navigation acceptance returned {e:?}"))
                );
            }
        };
        let navigated = self.drive_structural_admission(&admission, context).await?;
        let continuation = self.continue_after_structural(&navigated, context).await?;
        Ok(NavigationSettlement {
            navigation: navigated,
            run: continuation,
        })
    }

    async fn resume(&self, context: &Context) -> ResumeResult {
        if let Some(error) = &self.inner.lock().unwrap().closed_error {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let inspected = self
            .command(
                Box::new(|state, _reader| {
                    Box::pin(async move {
                        let operation_id = state
                            .operation
                            .as_ref()
                            .map(|op| op.meta.operation_id.clone());
                        LaneCommand::Return {
                            result: operation_id,
                        }
                    })
                }),
                context,
            )
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let Some(operation_id) = inspected else {
            return Err(HarnessError::NothingToResume(NothingToResume::new(
                self.name.clone(),
                format!("Lane {:?} has no active operation to resume", self.name),
            )));
        };
        self.drive_run_result(operation_id, true, context).await
    }

    async fn abort(&self, context: &Context) -> AbortResult {
        if let Some(error) = &self.inner.lock().unwrap().closed_error {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let operation_id = self
            .command(
                Box::new(|state, _reader| {
                    Box::pin(async move {
                        LaneCommand::Return {
                            result: state
                                .operation
                                .as_ref()
                                .map(|op| op.meta.operation_id.clone()),
                        }
                    })
                }),
                context,
            )
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let Some(operation_id) = operation_id else {
            return Err(HarnessError::NoActiveOperation(NoActiveOperation::new(
                self.name.clone(),
                format!("Lane {:?} has no active operation", self.name),
            )));
        };
        let requested = self.request_abort(operation_id.clone(), context).await?;
        let _ = self
            .drive(
                DriveOptions {
                    operation_id: operation_id.clone(),
                    wait_for_retry: false,
                    poll_deferred: false,
                },
                context,
            )
            .await
            .map_err(|e| {
                if matches!(e, HarnessError::Closed(_)) {
                    e
                } else {
                    HarnessError::Closed(Closed::new(format!(
                        "Cancelled operation {operation_id} no longer matches its lane"
                    )))
                }
            })?;
        Ok(AbortOutcome {
            operation_id,
            steer: requested
                .get("steer")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect()
                })
                .unwrap_or_default(),
            follow_up: requested
                .get("followUp")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    async fn steer(
        &self,
        message: serde_json::Value,
        images: Option<Vec<pi_ai::ImageContent>>,
        context: &Context,
    ) -> QueueResult {
        self.enqueue(InboxItemKind::Steer, message, images, context)
            .await
    }

    async fn follow_up(
        &self,
        message: serde_json::Value,
        images: Option<Vec<pi_ai::ImageContent>>,
        context: &Context,
    ) -> QueueResult {
        self.enqueue(InboxItemKind::FollowUp, message, images, context)
            .await
    }

    async fn next_run(
        &self,
        message: serde_json::Value,
        images: Option<Vec<pi_ai::ImageContent>>,
        context: &Context,
    ) -> QueueResult {
        self.enqueue(InboxItemKind::NextRun, message, images, context)
            .await
    }

    async fn cancel_queued(&self, entry_id: String, context: &Context) -> CancelQueuedResult {
        if let Some(error) = &self.inner.lock().unwrap().closed_error {
            return Err(HarnessError::Closed(Closed::new(error.clone())));
        }
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let name = self.name.clone();
        let context_for_closure = context.clone();
        self.command(
            Box::new(move |state, reader| {
                let entry_id = entry_id.clone();
                let name = name.clone();
                let context = context_for_closure.clone();
                Box::pin(async move {
                    let queued = state.inbox.iter().find(|item| item.entry_id == entry_id);
                    if queued.is_none() {
                        let consumed = reader
                            .get_entries(std::slice::from_ref(&entry_id), &context)
                            .await
                            .unwrap_or_else(|e| panic!("{e}"));
                        return LaneCommand::Return {
                            result: Ok(if consumed.contains_key(&entry_id) {
                                CancelQueuedKind::AlreadyConsumed
                            } else {
                                CancelQueuedKind::NotFound
                            }),
                        };
                    }
                    let inbox: Vec<InboxItem> = state
                        .inbox
                        .iter()
                        .filter(|item| item.entry_id != entry_id)
                        .cloned()
                        .collect();
                    let queues = read_lane_queues(reader.as_ref(), &inbox, &context)
                        .await
                        .unwrap_or_else(|e| panic!("{e}"));
                    let mut writes = vec![Write::Value(delete_value(&pending_entry(&entry_id)))];
                    writes.push(Write::Value(set_value(
                        &lane_state_value(&name),
                        serde_json::to_value(durable_lane_state(
                            state
                                .operation
                                .as_ref()
                                .map(|o| o.meta.operation_id.clone()),
                            inbox.clone(),
                            state.last_operation_id.clone(),
                        ))
                        .unwrap_or(serde_json::Value::Null),
                    )));
                    let next = LaneRuntimeState { inbox, ..state };
                    LaneCommand::Commit {
                        writes,
                        next,
                        materialize: Box::new(|_| Ok(CancelQueuedKind::Cancelled)),
                        events: Some(Box::new(move |_| {
                            vec![HarnessEvent::QueueUpdate {
                                lane: name.clone(),
                                queues: queues.clone(),
                                recovery: None,
                            }]
                        })),
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))?
    }

    async fn record_usage(
        &self,
        usage: pi_ai::Usage,
        entry_id: Option<String>,
        details: Option<serde_json::Value>,
        context: &Context,
    ) -> RecordUsageResult {
        let usage_id = self.session.id_generator().next(None);
        let name = self.name.clone();
        let _context_for_closure = context.clone();
        self.command(
            Box::new(move |state, _reader| {
                let usage_id = usage_id.clone();
                let _name = name.clone();
                Box::pin(async move {
                    let row = UsageRow {
                        id: usage_id.clone(),
                        seq: 0,
                        usage: usage.clone(),
                        entry_id: entry_id.clone(),
                        adjustment: false,
                        details: details.clone(),
                    };
                    let writes = vec![insert_usage(row)];
                    LaneCommand::Commit {
                        writes,
                        next: state,
                        materialize: Box::new(move |_| {
                            Ok(RecordUsageOutcome {
                                usage_id: usage_id.clone(),
                            })
                        }),
                        events: None,
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))?
    }

    async fn wait_for_idle(&self, context: &Context) -> Result<(), HarnessError> {
        LaneImpl::wait_for_idle(self, context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))
    }

    async fn run_when_idle(
        &self,
        callback: Arc<dyn Fn(Context) -> BoxFuture + Send + Sync>,
        context: &Context,
    ) -> Result<(), HarnessError> {
        LaneImpl::run_when_idle(self, callback, context)
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))
    }

    async fn get_model(&self, _context: &Context) -> Result<Option<Model>, HarnessError> {
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        let state = self.state();
        let identity = &state.configuration.model;
        Ok(self
            .models()
            .get_model(&identity.provider, &identity.model_id))
    }

    async fn set_model(
        &self,
        model: crate::harness::agent_harness::ModelIdentity,
        context: &Context,
    ) -> Result<(), HarnessError> {
        let identity = crate::harness::session::types::ModelIdentity {
            provider: model.provider,
            model_id: model.model_id,
        };
        self.set_configuration_identity(identity, context).await
    }

    async fn get_thinking_level(&self, _context: &Context) -> Result<ThinkingLevel, HarnessError> {
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        Ok(self.state().configuration.thinking_level)
    }

    async fn set_thinking_level(
        &self,
        level: ThinkingLevel,
        context: &Context,
    ) -> Result<(), HarnessError> {
        let name = self.name.clone();
        let _context_for_closure = context.clone();
        self.command(
            Box::new(move |state, _reader| {
                let name = name.clone();
                Box::pin(async move {
                    let previous = state.configuration.clone();
                    let mut configuration = previous.clone();
                    configuration.thinking_level = level;
                    let next = LaneRuntimeState {
                        configuration: configuration.clone(),
                        ..state
                    };
                    let writes = vec![Write::Value(set_value(
                        &lane_config_value(&name),
                        serde_json::to_value(&configuration).unwrap_or(serde_json::Value::Null),
                    ))];
                    let previous_level = previous.thinking_level;
                    LaneCommand::Commit {
                        writes,
                        next,
                        materialize: Box::new(|_| ()),
                        events: Some(Box::new(move |_| {
                            vec![HarnessEvent::ConfigUpdate {
                                lane: Some(name.clone()),
                                payload: ConfigUpdatePayload::ThinkingLevel {
                                    value: level,
                                    previous: previous_level,
                                },
                                recovery: None,
                            }]
                        })),
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        Ok(())
    }

    async fn get_active_tools(&self, _context: &Context) -> Result<Vec<String>, HarnessError> {
        self.assert_open()
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        Ok(self.state().configuration.active_tool_names)
    }

    async fn set_active_tools(
        &self,
        names: Vec<String>,
        context: &Context,
    ) -> Result<(), HarnessError> {
        let name = self.name.clone();
        let _context_for_closure = context.clone();
        self.command(
            Box::new(move |state, _reader| {
                let name = name.clone();
                Box::pin(async move {
                    let previous = state.configuration.clone();
                    let mut configuration = previous.clone();
                    configuration.active_tool_names = names.clone();
                    let next = LaneRuntimeState {
                        configuration: configuration.clone(),
                        ..state
                    };
                    let writes = vec![Write::Value(set_value(
                        &lane_config_value(&name),
                        serde_json::to_value(&configuration).unwrap_or(serde_json::Value::Null),
                    ))];
                    let previous_names = previous.active_tool_names.clone();
                    LaneCommand::Commit {
                        writes,
                        next,
                        materialize: Box::new(|_| ()),
                        events: Some(Box::new(move |_| {
                            vec![HarnessEvent::ConfigUpdate {
                                lane: Some(name.clone()),
                                payload: ConfigUpdatePayload::ActiveTools {
                                    value: names.clone(),
                                    previous: previous_names,
                                },
                                recovery: None,
                            }]
                        })),
                    }
                })
            }),
            context,
        )
        .await
        .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        Ok(())
    }

    async fn watch(
        &self,
        context: &Context,
    ) -> Result<Arc<dyn WatchHandle<LaneSnapshot>>, HarnessError> {
        let self_arc = self.self_arc();
        let name = self.name.clone();
        let install_watch = Arc::clone(&self.install_watch);
        let read_context = context.clone();
        let watcher: Arc<BufferedEventWatcher<LaneSnapshot>> = self
            .read_lane(
                {
                    let self_arc = Arc::clone(&self_arc);
                    let install_watch = Arc::clone(&install_watch);
                    let name = name.clone();
                    let context = context.clone();
                    move |state, reader| {
                        let self_arc = Arc::clone(&self_arc);
                        let install_watch = Arc::clone(&install_watch);
                        let name = name.clone();
                        let context = context.clone();
                        Box::pin(async move {
                            let filter_name = name;
                            let filter: Arc<dyn Fn(&HarnessEvent) -> bool + Send + Sync> =
                                Arc::new(move |event| {
                                    event.event_type() == "usage"
                                        || event.lane().is_none()
                                        || event.lane() == Some(filter_name.as_str())
                                });
                            let self_for_resnapshot = Arc::clone(&self_arc);
                            let resnapshot: ResnapshotCapture<LaneSnapshot> =
                                Arc::new(move |ctx, mark_boundary| {
                                    let self_arc = Arc::clone(&self_for_resnapshot);
                                    Box::pin(async move {
                                        let self_arc_for_closure = Arc::clone(&self_arc);
                                        let ctx_for_closure = ctx.clone();
                                        self_arc
                                            .read_lane(
                                                move |latest, latest_reader| {
                                                    let self_arc =
                                                        Arc::clone(&self_arc_for_closure);
                                                    let ctx = ctx_for_closure.clone();
                                                    let mark_boundary = Arc::clone(&mark_boundary);
                                                    Box::pin(async move {
                                                        let snapshot = self_arc
                                                            .capture_lane_snapshot_inner(
                                                                &latest,
                                                                latest_reader.as_ref(),
                                                                &ctx,
                                                            )
                                                            .await?;
                                                        mark_boundary();
                                                        Ok(snapshot)
                                                    })
                                                },
                                                &ctx,
                                            )
                                            .await
                                            .unwrap_or_else(|e| {
                                                panic!("resnapshot readLane failed: {e}")
                                            })
                                    })
                                });
                            let watcher =
                                install_watch(None, filter, context.clone(), Some(resnapshot));
                            match self_arc
                                .capture_lane_snapshot_inner(&state, reader.as_ref(), &context)
                                .await
                            {
                                Ok(snapshot) => {
                                    watcher.set_snapshot(snapshot);
                                    Ok(watcher)
                                }
                                Err(e) => {
                                    watcher.unsubscribe();
                                    Err(e)
                                }
                            }
                        })
                    }
                },
                &read_context,
            )
            .await
            .map_err(|e| HarnessError::Closed(Closed::new(e)))?;
        Ok(watcher as Arc<dyn WatchHandle<LaneSnapshot>>)
    }
}
