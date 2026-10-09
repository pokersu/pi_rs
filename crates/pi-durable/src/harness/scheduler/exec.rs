//! 对应 `harness/scheduler.ts` 的执行层：`TaskScheduler` 的 I/O 方法与任务调用循环。
//!
//! 上游是 1435 行的单一类。Rust 侧把**纯逻辑**拆到了
//! [`ownership`](super::ownership) / [`state`](super::state) / [`reserve`](super::reserve) /
//! [`expiry`](super::expiry)；本模块承载剩下的**执行层**：
//!
//! - 生命周期：`open` / `resume` / `join` / `abort` / `wait_for_task` / `wait_for_idle` /
//!   `abort_conversation` / `seal`
//! - 调度推进：`reconcile` / `finalize` / `load_scopes` / `load_chain` / `drain` / `reserve`
//! - 任务调用：`create_invocation` / `run` / `decide` / `run_abort` / `step` / `terminate` /
//!   `commit_state` / `validate_wait` / `end`
//! - 调用运行时：`runtime` / `read` / `gated` / `sleep` / `watch_doc`（实现 [`TaskRuntime`]）
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游用 `Promise.withResolvers()` / `setTimeout` / `queueMicrotask` / `AbortSignal.any`；
//!   Rust 用 `oneshot` + `Shared`、`tokio::time::sleep`、`tokio::spawn`、`AbortSignal::any`。
//! - 上游 `#gated` 的 `change` 是 `(tx, current) => T | Promise<T>`；Rust 统一为
//!   `FnOnce(&Transaction, RunningTask) -> BoxFuture<Result<T, SessionError>>`，`current` 按值
//!   move（见 [`TaskCommit`] 的说明）。
//! - 上游 `now` / `report` 在调用结束后 `throw`；Rust 侧它们无 `Result`，改为 panic（对等语义，
//!   与 `Storage::mint_id` 的处理一致）。
//! - `#unsubscribeRegistry` 是 `() => void`；Rust 用 `Box<dyn Fn()>`，并存于 `SchedulerShared`。

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::{BoxFuture, Shared};
use pi_ai::{AbortError, AbortSignal};

use crate::chord::context::{Context, await_with_context, with_abort_signal};
use crate::documents::AnyDocToken;
use crate::env::ExecutionEnv;
use crate::harness::agent::agent_hooks;
use crate::harness::context::read_context_from;
use crate::harness::scheduler::expiry::{self, ExpiryPlan, IdleKey, MAX_TIMER_DELAY};
use crate::harness::scheduler::ownership::{
    AnyTaskRecord, BlockedReason, Overlay, OwnerLookup, Scope, Step, Up, can_reserve, json_equal,
    memo_of, node_of, parent_of, with_state,
};
use crate::harness::scheduler::reserve::{Resolution, inspect_task, resolve, waiting_on};
use crate::harness::scheduler::state::{KeptContext, SchedulerState};
use crate::harness::types::{
    Agent, ContextView, ConversationHandle, HookHandlers, HookRunner, NextTaskState,
    RegistryReader, RegistrySnapshot, RunningTask, SchedulingState, Settings, SettledTask,
    TaskCommit, TaskInspection, TaskRuntime,
};
use crate::harness::util::{Waiters, closed_error, scan_all};
use crate::session::transaction::{Transaction, TransactionScope};
use crate::session::{DocumentWatch, SessionError, SessionImpl, SessionSubscription};
use crate::types::{
    ConversationId, EntryId, EntryRecord, JoinPolicy, JsonObject, SubmissionQuery,
    SubmissionStatus, Task, TaskId, TaskOutcome, TaskOutcomeError, TaskQuery, TaskState,
    TaskStatus, WatchHandle,
};

/// 对应 `LIVE_STATUSES`：`open` 时扫入 `#live` 的状态。
const LIVE_STATUSES: [TaskStatus; 4] = [
    TaskStatus::Pending,
    TaskStatus::Running,
    TaskStatus::Waiting,
    TaskStatus::Completing,
];

/// 对应 `SCAN_PAGE_SIZE`。
const SCAN_PAGE_SIZE: usize = 256;

/// 对应 `SchedulerOutcome`：调度器在不跑任务代码的情况下写下的终态结果。
#[derive(Debug, Clone, PartialEq)]
pub enum SchedulerOutcome {
    /// `faulted`。
    Faulted { error: TaskOutcomeError },
    /// `orphaned`。
    Orphaned { reason: String },
}

/// 对应 `Invocation.mode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationMode {
    /// `run`。
    Run,
    /// `abort`。
    Abort,
}

/// 对应 `InvocationBinding`：会话句柄绑定到一次调用。
#[derive(Clone)]
pub struct InvocationBinding {
    /// 调用结束或关闭时中止的信号。
    pub signal: AbortSignal,
    /// 调用结束后抛错的检查。
    pub check: Arc<dyn Fn() -> Result<(), SessionError> + Send + Sync>,
}

/// 对应 `Abort` 的返回：`"marked" | "terminal"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortResult {
    /// `marked`。
    Marked,
    /// `terminal`。
    Terminal,
}

/// 对应 `Invocation`：一次任务的 in-memory 执行（run 或 abort 模式）。
pub struct Invocation {
    task_id: TaskId,
    conversation_id: ConversationId,
    mode: InvocationMode,
    controller: AbortSignal,
    context: Arc<dyn Context>,
    watches: Mutex<Vec<Arc<DocumentWatch>>>,
    ended: AtomicBool,
    done: Shared<oneshot::Receiver<()>>,
    finish_sender: Mutex<Option<oneshot::Sender<()>>>,
}

impl Invocation {
    fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    /// 对应 `invocation.finish()`：结清 `done`（幂等，只有第一次有效）。
    fn finish(&self) {
        if let Some(sender) = self.finish_sender.lock().expect("finish").take() {
            let _ = sender.send(());
        }
    }

    /// 对应 `invocation.done`：调用结束后立即 resolve。
    async fn done(&self) {
        let _ = self.done.clone().await;
    }
}

/// 对应 `Reservation`：一次保留——调用、定义与快照。
struct Reservation {
    invocation: Arc<Invocation>,
    task: Arc<Task>,
    snapshot: RegistrySnapshot,
}

/// 对应 `Phase`：一个 phase 处理器所读的快照、任务名与懒解析的 agent。
struct Phase {
    snapshot: Arc<Mutex<RegistrySnapshot>>,
    task_name: String,
    agent: Arc<Mutex<Option<Result<Agent, SessionError>>>>,
}

/// 对应 `PhaseResult`：刚返回的 phase 的结果，由下一步判定。
#[derive(Clone)]
struct PhaseResult {
    checkpoint: serde_json::Value,
    failure: Option<SessionError>,
}

/// 对应 `Decision`：继续、结束，或以 `faulted` 结束。
enum Decision {
    /// `true`：继续下一 phase。
    Continue,
    /// `false`：结束调用（不写）。
    End,
    /// `{ fault }`：结束调用并写 `faulted`。
    Fault { message: String },
}

/// 对应 `waitForTask` 里「已终态」与「挂起等待」两种结果。
enum WaitForTaskPromise {
    Resolved(SettledTask),
    Waiter(BoxFuture<'static, Result<SettledTask, SessionError>>),
}

/// 对应 `TaskSchedulerOptions.agent`。
pub type SchedulerAgent = Arc<
    dyn Fn(
            ConversationId,
            RegistrySnapshot,
            Arc<dyn Context>,
        ) -> BoxFuture<'static, Result<Agent, SessionError>>
        + Send
        + Sync,
>;

/// 对应 `TaskSchedulerOptions.settings`。
pub type SchedulerSettings = Arc<dyn Fn() -> Settings + Send + Sync>;

/// 对应 `TaskSchedulerOptions.env`。
pub type SchedulerEnv = Arc<
    dyn Fn(
            ConversationId,
            Arc<dyn Context>,
        ) -> BoxFuture<'static, Result<Option<Arc<dyn ExecutionEnv>>, SessionError>>
        + Send
        + Sync,
>;

/// 对应 `TaskSchedulerOptions.now`。
pub type SchedulerNow = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 对应 `TaskSchedulerOptions.report`。
pub type SchedulerReport = Arc<dyn Fn(SessionError) + Send + Sync>;

/// 对应 `TaskSchedulerOptions.settleOutcome`。
pub type SchedulerSettleOutcome = Arc<
    dyn for<'a> Fn(
            &'a Transaction,
            AnyTaskRecord,
            SchedulerOutcome,
        ) -> BoxFuture<'a, Result<(), SessionError>>
        + Send
        + Sync,
>;

/// 对应 `TaskSchedulerOptions.withdrawInputs`。
pub type SchedulerWithdrawInputs = Arc<
    dyn for<'a> Fn(&'a Transaction, ConversationId) -> BoxFuture<'a, Result<(), SessionError>>
        + Send
        + Sync,
>;

/// 对应 `TaskSchedulerOptions.conversation`。
pub type SchedulerConversation = Arc<
    dyn Fn(
            ConversationId,
            InvocationBinding,
            Arc<dyn Context>,
        ) -> BoxFuture<'static, Result<Option<Arc<dyn ConversationHandle>>, SessionError>>
        + Send
        + Sync,
>;

/// 对应 `TaskSchedulerOptions`：调度器的宿主依赖。
pub struct TaskSchedulerOptions {
    /// 会话内核。
    pub session: Arc<SessionImpl>,
    /// 存储。
    pub storage: Arc<dyn crate::types::Storage>,
    /// 注册表只读视图。
    pub registry: Arc<dyn RegistryReader>,
    /// 模型。
    pub models: Arc<pi_ai::Models>,
    /// 解析会话 agent。
    pub agent: SchedulerAgent,
    /// 每次访问重新解析设置。
    pub settings: SchedulerSettings,
    /// 构建会话环境。
    pub env: SchedulerEnv,
    /// Harness 时钟。
    pub now: SchedulerNow,
    /// 上报非致命失败。
    pub report: SchedulerReport,
    /// 调度器自写结果落定时的 Harness 清理。
    pub settle_outcome: SchedulerSettleOutcome,
    /// 撤回一个会话的排队输入。
    pub withdraw_inputs: SchedulerWithdrawInputs,
    /// 已有会话的调用期绑定句柄。
    pub conversation: SchedulerConversation,
    /// 调度器提交与调用的上下文（不携带调用方取消）。
    pub context: Arc<dyn Context>,
}

/// 对应 `ExpiryHandle`：已排定的上下文过期定时器。
struct ExpiryHandle {
    at: u64,
    signal: AbortSignal,
}

struct SchedulerDeps {
    session: Arc<SessionImpl>,
    storage: Arc<dyn crate::types::Storage>,
    registry: Arc<dyn RegistryReader>,
    models: Arc<pi_ai::Models>,
    agent: SchedulerAgent,
    settings: SchedulerSettings,
    env: SchedulerEnv,
    now: SchedulerNow,
    report: SchedulerReport,
    settle_outcome: SchedulerSettleOutcome,
    withdraw_inputs: SchedulerWithdrawInputs,
    conversation: SchedulerConversation,
    context: Arc<dyn Context>,
}

struct SchedulerShared {
    state: Mutex<SchedulerState>,
    invocations: Mutex<BTreeMap<TaskId, Arc<Invocation>>>,
    task_waiters: Arc<Waiters<TaskId, SettledTask>>,
    idle_waiters: Arc<Waiters<IdleKey, ()>>,
    expiry: Mutex<Option<ExpiryHandle>>,
    commit_subscription: Mutex<Option<SessionSubscription>>,
    close_subscription: Mutex<Option<SessionSubscription>>,
    unsubscribe_registry: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

/// `TaskScheduler` 的内部共享状态（字段私有；仅作 `Deref` 目标）。
pub struct TaskSchedulerInner {
    deps: Arc<SchedulerDeps>,
    shared: Arc<SchedulerShared>,
}

/// 对应 `TaskScheduler`：一个 Harness 的 durable 任务调度器。
#[derive(Clone)]
pub struct TaskScheduler(Arc<TaskSchedulerInner>);

impl std::ops::Deref for TaskScheduler {
    type Target = TaskSchedulerInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TaskScheduler {
    /// 由宿主依赖构造；调用 [`Self::open`] 开始装载活动任务。
    pub fn new(options: TaskSchedulerOptions) -> Self {
        let deps = Arc::new(SchedulerDeps {
            session: options.session,
            storage: options.storage,
            registry: options.registry,
            models: options.models,
            agent: options.agent,
            settings: options.settings,
            env: options.env,
            now: options.now,
            report: options.report,
            settle_outcome: options.settle_outcome,
            withdraw_inputs: options.withdraw_inputs,
            conversation: options.conversation,
            context: options.context,
        });
        let shared = Arc::new(SchedulerShared {
            state: Mutex::new(SchedulerState::new()),
            invocations: Mutex::new(BTreeMap::new()),
            task_waiters: Arc::new(Waiters::new()),
            idle_waiters: Arc::new(Waiters::new()),
            expiry: Mutex::new(None),
            commit_subscription: Mutex::new(None),
            close_subscription: Mutex::new(None),
            unsubscribe_registry: Mutex::new(None),
        });
        TaskScheduler(Arc::new(TaskSchedulerInner { deps, shared }))
    }

    // ─── 生命周期 ─────────────────────────────────────────────────────────

    /// 对应 `open(context)`：装载活动任务、把崩溃遗留的 `running` 改回 `pending`，然后推进调度。
    pub async fn open(&self, context: Arc<dyn Context>) -> Result<(), SessionError> {
        let commit_sub = {
            let scheduler = self.clone();
            self.deps
                .session
                .subscribe_commits(Arc::new(move |publication, _ctx| {
                    scheduler.observe(publication);
                }))?
        };
        let close_sub = {
            let scheduler = self.clone();
            self.deps
                .session
                .subscribe_close(Arc::new(move || scheduler.seal()))?
        };
        let unsubscribe = self.deps.registry.subscribe(Arc::new({
            let scheduler = self.clone();
            move || scheduler.kick()
        }));
        *self.shared.commit_subscription.lock().expect("commit_sub") = Some(commit_sub);
        *self.shared.close_subscription.lock().expect("close_sub") = Some(close_sub);
        *self.shared.unsubscribe_registry.lock().expect("unsub") = Some(unsubscribe);

        self.deps
            .session
            .commit_with(
                move |tx| {
                    let scheduler = self.clone();
                    Box::pin(async move {
                        let mut records: Vec<AnyTaskRecord> = Vec::new();
                        for status in LIVE_STATUSES {
                            let page = scan_all(|cursor| {
                                let query = TaskQuery {
                                    conversation_id: None,
                                    kind: None,
                                    status: Some(status),
                                    abort_requested: None,
                                    background: None,
                                    order: None,
                                };
                                async move { tx.scan_tasks(query, SCAN_PAGE_SIZE, cursor).await }
                            })
                            .await?;
                            records.extend(page);
                        }
                        for record in records {
                            let key = key_of(&record);
                            let mut state = scheduler.shared.state.lock().expect("state");
                            state.live.insert(key, record.clone());
                            if record.state.status() == TaskStatus::Running
                                && let TaskState::Running { checkpoint } = &record.state
                            {
                                tx.set_task(with_state(
                                    &record,
                                    TaskState::Pending {
                                        checkpoint: checkpoint.clone(),
                                    },
                                ));
                            }
                            if matches!(
                                &record.state,
                                TaskState::Waiting {
                                    policy: JoinPolicy::FailFast,
                                    ..
                                }
                            ) {
                                state.fail_fast_checks.insert(key);
                            }
                        }
                        Ok(())
                    })
                },
                context,
                None,
            )
            .await?;

        self.shared.state.lock().expect("state").cascade_pending = true;
        self.schedule_reconcile();
        Ok(())
    }

    /// 对应 `resume()`：启用调度。
    pub fn resume(&self) {
        self.shared.state.lock().expect("state").enabled = true;
        self.kick();
    }

    /// 对应 `join()`：等每一个被 `seal()` 触发的调用结束。
    pub async fn join(&self) {
        let invocations: Vec<Arc<Invocation>> = self
            .shared
            .invocations
            .lock()
            .expect("inv")
            .values()
            .cloned()
            .collect();
        for invocation in invocations {
            invocation.done().await;
        }
    }

    /// 对应 `abort(id, context)`。
    pub async fn abort(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<AbortResult, SessionError> {
        let id_key = TaskId::new(id.get());
        let context_for_commit = Arc::clone(&context);
        let marked = self
            .deps
            .session
            .commit_with(
                move |tx| {
                    let scheduler = self.clone();
                    Box::pin(async move {
                        let current = tx.task(id_key).await?;
                        let Some(current) = current else {
                            return Err(SessionError::Message(format!(
                                "Task {id_key} does not exist"
                            )));
                        };
                        if current.state.status() == TaskStatus::Terminal {
                            return Ok((AbortResult::Terminal, None));
                        }
                        let invocation = scheduler
                            .shared
                            .invocations
                            .lock()
                            .expect("inv")
                            .get(&id_key)
                            .cloned();
                        if invocation.is_none() && current.state.status() != TaskStatus::Completing
                        {
                            scheduler.load_scopes(false).await?;
                            let owned = {
                                let state = scheduler.shared.state.lock().expect("state");
                                state.owned_live(None)
                            };
                            if !owned.contains_key(&id_key) {
                                let snapshot = scheduler.deps.registry.snapshot();
                                let outcome = {
                                    let mut state = scheduler.shared.state.lock().expect("state");
                                    resolve(&mut state, &current, &snapshot)
                                };
                                if let Some(report) = &outcome.report {
                                    (scheduler.deps.report)(SessionError::Message(report.clone()));
                                }
                                if let Resolution::Blocked { reason } = outcome.resolution {
                                    scheduler
                                        .terminate(
                                            tx,
                                            &current,
                                            SchedulerOutcome::Orphaned {
                                                reason: blocked_reason_str(reason),
                                            },
                                        )
                                        .await?;
                                    return Ok((AbortResult::Marked, None));
                                }
                            }
                        }
                        if !current.abort_requested {
                            let mut marked = current.clone();
                            marked.abort_requested = true;
                            tx.set_task(marked);
                        }
                        let run = invocation.filter(|inv| inv.mode == InvocationMode::Run);
                        Ok((AbortResult::Marked, run))
                    })
                },
                context_for_commit,
                None,
            )
            .await?;

        if let Some(run) = marked.1 {
            await_with_context(run.done(), context.as_ref())
                .await
                .map_err(SessionError::Aborted)?;
        }
        Ok(marked.0)
    }

    /// 对应 `waitForTask(id, context)`。
    pub async fn wait_for_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<SettledTask, SessionError> {
        let id_key = TaskId::new(id.get());
        let scheduler = self.clone();
        let promise = self
            .deps
            .session
            .read_on_line(move || {
                let scheduler = scheduler.clone();
                let context = Arc::clone(&context);
                async move {
                    if scheduler.is_closing() {
                        return Err(closed_error());
                    }
                    if scheduler
                        .shared
                        .state
                        .lock()
                        .expect("state")
                        .live
                        .contains_key(&id_key)
                    {
                        return Ok(WaitForTaskPromise::Waiter(
                            scheduler.shared.task_waiters.add(id_key, context),
                        ));
                    }
                    let record = scheduler
                        .deps
                        .storage
                        .task(id_key, scheduler.deps.context.as_ref())
                        .await
                        .map_err(SessionError::Storage)?;
                    let Some(record) = record else {
                        return Err(SessionError::Message(format!(
                            "Task {id_key} does not exist"
                        )));
                    };
                    Ok(WaitForTaskPromise::Resolved(SettledTask { record }))
                }
            })
            .await?;
        match promise {
            WaitForTaskPromise::Resolved(settled) => Ok(settled),
            WaitForTaskPromise::Waiter(future) => future.await,
        }
    }

    /// 对应 `waitForIdle(conversationId, context)`。
    pub async fn wait_for_idle(
        &self,
        conversation_id: Option<ConversationId>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        if self.is_closing() {
            return Err(closed_error());
        }
        if self
            .shared
            .state
            .lock()
            .expect("state")
            .idle(conversation_id)
        {
            return Ok(());
        }
        self.schedule_reconcile();
        self.shared.idle_waiters.add(conversation_id, context).await
    }

    /// 对应 `abortConversation(conversationId, background, context)`。
    pub async fn abort_conversation(
        &self,
        conversation_id: ConversationId,
        background: bool,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let reached = self
            .deps
            .session
            .commit_with(
                move |tx| {
                    let scheduler = self.clone();
                    Box::pin(async move {
                        let queued = scheduler.load_scopes(true).await?;
                        let scope = Scope::Conversation(conversation_id);
                        let live: Vec<AnyTaskRecord> = scheduler
                            .shared
                            .state
                            .lock()
                            .expect("state")
                            .live
                            .values()
                            .cloned()
                            .collect();
                        let mut reached = Vec::new();
                        for record in live {
                            if record.background && !background {
                                continue;
                            }
                            let parent = parent_of(&node_of(&record));
                            let in_scope = {
                                let state = scheduler.shared.state.lock().expect("state");
                                state.in_scope(parent, scope, background)
                            };
                            if in_scope != Some(true) {
                                continue;
                            }
                            reached.push(key_of(&record));
                            if !record.abort_requested {
                                let mut marked = record.clone();
                                marked.abort_requested = true;
                                tx.set_task(marked);
                            }
                        }
                        for id in queued {
                            let in_scope = {
                                let state = scheduler.shared.state.lock().expect("state");
                                state.in_scope(Up::Conversation(id), scope, background)
                            };
                            if in_scope == Some(true) {
                                (scheduler.deps.withdraw_inputs)(tx, id).await?;
                            }
                        }
                        Ok(reached)
                    })
                },
                context.clone(),
                None,
            )
            .await?;

        if background {
            for id in reached {
                self.wait_for_task(id, context.clone()).await?;
            }
        }
        self.wait_for_idle(Some(conversation_id), context).await
    }

    // ─── 提交监听与推进 ────────────────────────────────────────────────────

    /// 对应 `#observe` 之后执行层的副作用（状态表已由 [`SchedulerState::observe`] 更新）。
    fn observe(&self, publication: &crate::types::CommitPublication) {
        let outcome = self
            .shared
            .state
            .lock()
            .expect("state")
            .observe(publication);
        for id in &outcome.abort_runs {
            if let Some(invocation) = self
                .shared
                .invocations
                .lock()
                .expect("inv")
                .get(id)
                .cloned()
                && invocation.mode == InvocationMode::Run
            {
                invocation.controller.abort();
            }
        }
        for record in outcome.settled {
            self.shared
                .task_waiters
                .resolve(&key_of(&record), SettledTask { record });
        }
        if outcome.reconcile {
            self.schedule_reconcile();
        }
        if outcome.settle_idle {
            self.settle_idle();
        }
        if outcome.kick {
            self.kick();
        }
    }

    /// 对应 `#seal()`。
    fn seal(&self) {
        {
            let mut state = self.shared.state.lock().expect("state");
            state.closing = true;
            state.contexts.clear();
        }
        if let Some(unsubscribe) = self
            .shared
            .unsubscribe_registry
            .lock()
            .expect("unsub")
            .take()
        {
            unsubscribe();
        }
        let error = closed_error();
        self.shared.task_waiters.reject_all(error.clone());
        self.shared.idle_waiters.reject_all(error.clone());
        if let Some(handle) = self.shared.expiry.lock().expect("expiry").take() {
            handle.signal.abort();
        }
        let invocations: Vec<Arc<Invocation>> = self
            .shared
            .invocations
            .lock()
            .expect("inv")
            .values()
            .cloned()
            .collect();
        for invocation in invocations {
            invocation.controller.abort();
        }
    }

    /// 对应 `#kick()`。
    fn kick(&self) {
        let should_drain = {
            let mut state = self.shared.state.lock().expect("state");
            state.dirty = true;
            if state.draining || !state.enabled || state.closing {
                return;
            }
            state.draining = true;
            true
        };
        if should_drain {
            let scheduler = self.clone();
            tokio::spawn(async move { scheduler.drain().await });
        }
    }

    /// 对应 `#drain()`。
    async fn drain(&self) {
        let result = self.drain_inner().await;
        let retry = {
            let mut state = self.shared.state.lock().expect("state");
            state.draining = false;
            state.dirty
        };
        if let Err(error) = result
            && !self.is_closing()
        {
            (self.deps.report)(error);
        }
        if retry {
            self.kick();
        }
    }

    async fn drain_inner(&self) -> Result<(), SessionError> {
        loop {
            {
                let mut state = self.shared.state.lock().expect("state");
                if !(state.dirty && state.enabled && !state.closing) {
                    return Ok(());
                }
                state.dirty = false;
            }
            let reservations = self.reserve().await?;
            for reservation in reservations {
                self.start(reservation);
            }
        }
    }

    /// 对应 `#scheduleReconcile()`。
    fn schedule_reconcile(&self) {
        let should_run = {
            let mut state = self.shared.state.lock().expect("state");
            if state.reconcile_scheduled || state.closing {
                return;
            }
            state.reconcile_scheduled = true;
            true
        };
        if should_run {
            let scheduler = self.clone();
            tokio::spawn(async move { scheduler.reconcile().await });
        }
    }

    /// 对应 `#reconcile()`。
    async fn reconcile(&self) {
        let (cascade, checks) = {
            let mut state = self.shared.state.lock().expect("state");
            state.reconcile_scheduled = false;
            let cascade = state.cascade_pending;
            state.cascade_pending = false;
            let checks: Vec<TaskId> = state.fail_fast_checks.iter().copied().collect();
            state.fail_fast_checks.clear();
            (cascade, checks)
        };
        let checks_for_commit = checks.clone();

        let result = self
            .deps
            .session
            .commit_with(
                move |tx| {
                    let scheduler = self.clone();
                    let checks = checks_for_commit.clone();
                    Box::pin(async move {
                        if scheduler.is_closing() {
                            return Ok(());
                        }
                        let queued = scheduler.load_scopes(cascade).await?;
                        let mut marked: BTreeSet<TaskId> = BTreeSet::new();
                        let live: Vec<AnyTaskRecord> = scheduler
                            .shared
                            .state
                            .lock()
                            .expect("state")
                            .live
                            .values()
                            .cloned()
                            .collect();
                        for record in &live {
                            if record.background {
                                continue;
                            }
                            let below = {
                                let state = scheduler.shared.state.lock().expect("state");
                                state.below_cancelled(parent_of(&node_of(record)))
                            };
                            if below {
                                mark(tx, record, &mut marked);
                            }
                        }
                        for id in &checks {
                            let waiter = scheduler
                                .shared
                                .state
                                .lock()
                                .expect("state")
                                .live
                                .get(id)
                                .cloned();
                            let Some(waiter) = waiter else { continue };
                            let TaskState::Waiting { on, .. } = &waiter.state else {
                                continue;
                            };
                            if !scheduler.any_failed(on).await? {
                                continue;
                            }
                            for member in on {
                                let record = scheduler
                                    .shared
                                    .state
                                    .lock()
                                    .expect("state")
                                    .live
                                    .get(member)
                                    .cloned();
                                if record.is_some()
                                    && !crate::harness::scheduler::ownership::failed_outcome(
                                        record.as_ref().unwrap(),
                                    )
                                {
                                    mark(tx, record.as_ref().unwrap(), &mut marked);
                                }
                            }
                        }
                        for id in queued {
                            let below = {
                                let state = scheduler.shared.state.lock().expect("state");
                                state.below_cancelled(Up::Conversation(id))
                            };
                            if below {
                                (scheduler.deps.withdraw_inputs)(tx, id).await?;
                            }
                        }
                        scheduler.finalize(tx).await
                    })
                },
                self.deps.context.clone(),
                None,
            )
            .await;

        if let Err(error) = result {
            {
                let mut state = self.shared.state.lock().expect("state");
                state.cascade_pending = true;
                for id in &checks {
                    state.fail_fast_checks.insert(*id);
                }
            }
            if !self.is_closing() {
                (self.deps.report)(error);
            }
        }
        self.settle_idle();
    }

    /// 对应 `#anyFailed(ids)`。
    async fn any_failed(&self, ids: &[TaskId]) -> Result<bool, SessionError> {
        for id in ids {
            let record = {
                let record = self
                    .shared
                    .state
                    .lock()
                    .expect("state")
                    .live
                    .get(id)
                    .cloned();
                match record {
                    Some(record) => Some(record),
                    None => self
                        .deps
                        .storage
                        .task(*id, self.deps.context.as_ref())
                        .await
                        .map_err(SessionError::Storage)?,
                }
            };
            if record.is_some()
                && crate::harness::scheduler::ownership::failed_outcome(record.as_ref().unwrap())
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 对应 `#finalize(tx)`。
    async fn finalize(&self, tx: &Transaction) -> Result<(), SessionError> {
        loop {
            let overlay = overlay_of(tx);
            let done = {
                let state = self.shared.state.lock().expect("state");
                let owned = state.owned_live(Some(&overlay));
                state
                    .live_records(Some(&overlay))
                    .into_iter()
                    .filter(|record| {
                        record.state.status() == TaskStatus::Completing
                            && !owned.contains_key(&key_of(record))
                    })
                    .collect::<Vec<_>>()
            };
            if done.is_empty() {
                return Ok(());
            }
            for record in done {
                let outcome = match &record.state {
                    TaskState::Completing { outcome } => outcome.clone(),
                    _ => continue,
                };
                tx.set_task(with_state(
                    &record,
                    TaskState::Terminal {
                        outcome: outcome.clone(),
                    },
                ));
                match &outcome {
                    TaskOutcome::Faulted { error } => {
                        (self.deps.settle_outcome)(
                            tx,
                            record.clone(),
                            SchedulerOutcome::Faulted {
                                error: error.clone(),
                            },
                        )
                        .await?;
                    }
                    TaskOutcome::Orphaned { reason } => {
                        (self.deps.settle_outcome)(
                            tx,
                            record.clone(),
                            SchedulerOutcome::Orphaned {
                                reason: reason.clone(),
                            },
                        )
                        .await?;
                    }
                    _ => {}
                }
            }
        }
    }

    /// 对应 `#loadScopes(queued)`。
    async fn load_scopes(&self, queued: bool) -> Result<Vec<ConversationId>, SessionError> {
        let live: Vec<AnyTaskRecord> = self
            .shared
            .state
            .lock()
            .expect("state")
            .live
            .values()
            .cloned()
            .collect();
        for record in &live {
            let parent = parent_of(&node_of(record));
            let known = {
                let state = self.shared.state.lock().expect("state");
                state.chain_known(parent, None)
            };
            if !known {
                self.load_chain(parent, None).await?;
            }
        }
        if !queued {
            return Ok(Vec::new());
        }
        let submissions = scan_all(|cursor| {
            let query = SubmissionQuery {
                conversation_id: None,
                status: Some(SubmissionStatus::Queued),
                order: None,
            };
            async move {
                self.deps
                    .storage
                    .scan_submissions(query, SCAN_PAGE_SIZE, cursor, self.deps.context.as_ref())
                    .await
                    .map_err(SessionError::Storage)
            }
        })
        .await?;
        let mut conversations: BTreeSet<ConversationId> = BTreeSet::new();
        for submission in submissions {
            conversations.insert(submission.identity().conversation_id);
        }
        for id in &conversations {
            self.load_chain(Up::Conversation(*id), None).await?;
        }
        Ok(conversations.into_iter().collect())
    }

    /// 对应 `#loadChain(start, overlay)`。
    async fn load_chain(&self, start: Up, overlay: Option<&Overlay>) -> Result<(), SessionError> {
        let mut at = Some(start);
        while let Some(current) = at {
            match current {
                Up::Task(id) => {
                    let node = {
                        let state = self.shared.state.lock().expect("state");
                        state.task_node(id, overlay)
                    };
                    let node = match node {
                        Some(node) => node,
                        None => {
                            let record = self
                                .deps
                                .storage
                                .task(id, self.deps.context.as_ref())
                                .await
                                .map_err(SessionError::Storage)?;
                            let Some(record) = record else { return Ok(()) };
                            let node = node_of(&record);
                            if record.state.status() == TaskStatus::Terminal {
                                self.shared
                                    .state
                                    .lock()
                                    .expect("state")
                                    .settled
                                    .insert(key_of(&record), node);
                            }
                            node
                        }
                    };
                    at = Some(parent_of(&node));
                }
                Up::Conversation(id) => {
                    let edge = {
                        let state = self.shared.state.lock().expect("state");
                        state.edge(id, overlay)
                    };
                    let edge = match edge {
                        Some(edge) => edge,
                        None => {
                            let record = self
                                .deps
                                .storage
                                .conversation(id, self.deps.context.as_ref())
                                .await
                                .map_err(SessionError::Storage)?;
                            let edge =
                                record.and_then(|record| record.owner.map(|owner| owner.task_id));
                            self.shared.state.lock().expect("state").set_edge(id, edge);
                            edge
                        }
                    };
                    at = edge.map(Up::Task);
                }
            }
        }
        Ok(())
    }

    /// 对应 `#reserve()` 的 I/O 部分。
    async fn reserve(&self) -> Result<Vec<Reservation>, SessionError> {
        let reservations: Arc<Mutex<Vec<Reservation>>> = Arc::new(Mutex::new(Vec::new()));
        let reservations_for_commit = Arc::clone(&reservations);
        let result = self
            .deps
            .session
            .commit_with(
                move |tx| {
                    let scheduler = self.clone();
                    let reservations = Arc::clone(&reservations_for_commit);
                    Box::pin(async move {
                        if !scheduler.is_enabled() || scheduler.is_closing() {
                            return Ok(());
                        }
                        scheduler.load_scopes(false).await?;
                        let owned = {
                            let state = scheduler.shared.state.lock().expect("state");
                            state.owned_live(None)
                        };
                        let mut snapshot: Option<RegistrySnapshot> = None;
                        let live: Vec<AnyTaskRecord> = scheduler
                            .shared
                            .state
                            .lock()
                            .expect("state")
                            .live
                            .values()
                            .cloned()
                            .collect();
                        for record in live {
                            let key = key_of(&record);
                            if scheduler
                                .shared
                                .invocations
                                .lock()
                                .expect("inv")
                                .contains_key(&key)
                            {
                                continue;
                            }
                            if !waiting_on(
                                &scheduler.shared.state.lock().expect("state"),
                                &record,
                                &owned,
                            )
                            .is_empty()
                            {
                                continue;
                            }
                            if record.state.status() == TaskStatus::Completing {
                                continue;
                            }
                            let mode = if record.abort_requested {
                                InvocationMode::Abort
                            } else {
                                InvocationMode::Run
                            };
                            if snapshot.is_none() {
                                snapshot = Some(scheduler.deps.registry.snapshot());
                            }
                            let snap = snapshot.as_ref().expect("snapshot").clone();
                            let outcome = {
                                let mut state = scheduler.shared.state.lock().expect("state");
                                resolve(&mut state, &record, &snap)
                            };
                            if let Some(report) = &outcome.report {
                                (scheduler.deps.report)(SessionError::Message(report.clone()));
                            }
                            match outcome.resolution {
                                Resolution::Blocked { reason } => {
                                    if mode == InvocationMode::Abort {
                                        scheduler
                                            .terminate(
                                                tx,
                                                &record,
                                                SchedulerOutcome::Orphaned {
                                                    reason: blocked_reason_str(reason),
                                                },
                                            )
                                            .await?;
                                    }
                                }
                                Resolution::Ready {
                                    task,
                                    record: resolved,
                                } => {
                                    let needs_write = resolved != record
                                        || record.state.status() != TaskStatus::Running;
                                    if needs_write {
                                        let checkpoint = checkpoint_of(&resolved.state)
                                            .cloned()
                                            .unwrap_or_default();
                                        tx.set_task(with_state(
                                            &resolved,
                                            TaskState::Running { checkpoint },
                                        ));
                                    }
                                    let invocation = scheduler.create_invocation(&record, mode);
                                    reservations.lock().expect("resv").push(Reservation {
                                        invocation,
                                        task,
                                        snapshot: snap.clone(),
                                    });
                                }
                            }
                        }
                        Ok(())
                    })
                },
                self.deps.context.clone(),
                None,
            )
            .await;

        if let Err(error) = result {
            let reservations = reservations.lock().expect("resv");
            for reservation in reservations.iter() {
                self.shared
                    .invocations
                    .lock()
                    .expect("inv")
                    .remove(&reservation.invocation.task_id);
                reservation.invocation.finish();
            }
            return Err(error);
        }
        Ok(Arc::try_unwrap(reservations)
            .map(|mutex| std::mem::take(&mut *mutex.lock().expect("resv")))
            .unwrap_or_else(|arc| std::mem::take(&mut *arc.lock().expect("resv"))))
    }

    /// 对应 `#inspect`（scheduler 部分；`submissions` 由 Harness 层补充）。
    pub async fn inspect(
        &self,
        snapshot: RegistrySnapshot,
    ) -> Result<(SchedulingState, Vec<TaskInspection>), SessionError> {
        self.load_scopes(false).await?;
        let state = self.shared.state.lock().expect("state");
        let owned = state.owned_live(None);
        let mut tasks = Vec::new();
        for record in state.live.values() {
            let has_invocation = self
                .shared
                .invocations
                .lock()
                .expect("inv")
                .contains_key(&key_of(record));
            let task_state = inspect_task(&state, record, &snapshot, &owned, has_invocation);
            tasks.push(TaskInspection {
                record: record.clone(),
                state: task_state,
            });
        }
        let scheduling = if state.closing {
            SchedulingState::Closing
        } else if state.enabled {
            SchedulingState::Running
        } else {
            SchedulingState::Paused
        };
        Ok((scheduling, tasks))
    }

    // ─── 任务调用 ─────────────────────────────────────────────────────────

    /// 对应 `#createInvocation(record, mode)`。
    fn create_invocation(&self, record: &AnyTaskRecord, mode: InvocationMode) -> Arc<Invocation> {
        let controller = AbortSignal::new();
        let (sender, receiver) = oneshot::channel();
        let context = with_abort_signal(controller.clone(), self.deps.context.clone());
        let invocation = Arc::new(Invocation {
            task_id: key_of(record),
            conversation_id: record.conversation_id,
            mode,
            controller,
            context,
            watches: Mutex::new(Vec::new()),
            ended: AtomicBool::new(false),
            done: receiver.shared(),
            finish_sender: Mutex::new(Some(sender)),
        });
        self.shared
            .invocations
            .lock()
            .expect("inv")
            .insert(key_of(record), Arc::clone(&invocation));
        invocation
    }

    /// 对应 `#start(reservation)`。
    fn start(&self, reservation: Reservation) {
        let scheduler = self.clone();
        tokio::spawn(async move { scheduler.run_reservation(reservation).await });
    }

    async fn run_reservation(&self, reservation: Reservation) {
        let invocation = Arc::clone(&reservation.invocation);
        let result = match invocation.mode {
            InvocationMode::Run => self.run(&reservation).await,
            InvocationMode::Abort => self.run_abort(&reservation).await,
        };
        if let Err(error) = result {
            (self.deps.report)(error);
        }
        self.end(&invocation);
        invocation.finish();
        self.kick();
    }

    /// 对应 `#run(reservation)`。
    async fn run(&self, reservation: &Reservation) -> Result<(), SessionError> {
        let invocation = Arc::clone(&reservation.invocation);
        let snapshot = Arc::new(Mutex::new(reservation.snapshot.clone()));
        let task = Arc::new(Mutex::new(reservation.task.clone()));
        let reported: Arc<Mutex<Option<Option<Arc<Task>>>>> = Arc::new(Mutex::new(None));
        let phase = Arc::new(Phase {
            snapshot: Arc::clone(&snapshot),
            task_name: reservation.task.definition().name().to_string(),
            agent: Arc::new(Mutex::new(None)),
        });
        let runtime = self.runtime(&invocation, &phase);

        let mut previous: Option<PhaseResult> = None;
        loop {
            let current = {
                let scheduler = self.clone();
                let snapshot = Arc::clone(&snapshot);
                let task = Arc::clone(&task);
                let reported = Arc::clone(&reported);
                let previous = previous.clone();
                self.step(&invocation, move |tx, current| {
                    scheduler.decide(tx, current, previous.as_ref(), &snapshot, &task, &reported)
                })
                .await
            };
            let Some(current) = current else {
                return Ok(());
            };
            if self.is_closing() {
                return Ok(());
            }
            let checkpoint = match &current.0.state {
                TaskState::Running { checkpoint } => checkpoint.clone(),
                _ => return Ok(()),
            };
            *phase.agent.lock().expect("agent") = None;
            let phase_name = phase_of(&checkpoint).map(str::to_owned);
            let failure = match phase_name {
                Some(name) => {
                    let task = task.lock().expect("task").clone();
                    task.definition()
                        .run_phase(
                            &name,
                            current.clone(),
                            Arc::clone(&runtime),
                            invocation.context.clone(),
                        )
                        .await
                        .err()
                }
                None => Some(SessionError::Message(format!(
                    "Task {} checkpoint has no phase",
                    current.0.kind
                ))),
            };
            previous = Some(PhaseResult {
                checkpoint,
                failure,
            });
        }
    }

    /// 对应 `#decide(...)`。
    fn decide(
        &self,
        tx: &Transaction,
        current: &RunningTask,
        previous: Option<&PhaseResult>,
        snapshot: &Mutex<RegistrySnapshot>,
        task: &Mutex<Arc<Task>>,
        reported: &Mutex<Option<Option<Arc<Task>>>>,
    ) -> Decision {
        if current.0.abort_requested {
            return Decision::End;
        }
        let Some(previous) = previous else {
            return Decision::Continue;
        };
        if let Some(error) = &previous.failure {
            return Decision::Fault {
                message: error.to_string(),
            };
        }
        if json_equal(checkpoint_of(&current.0.state), Some(&previous.checkpoint)) {
            return Decision::Fault {
                message: format!(
                    "Task {} phase {} returned without durable progress",
                    current.0.kind,
                    phase_of(&previous.checkpoint).unwrap_or("")
                ),
            };
        }
        let next_snapshot = self.deps.registry.snapshot();
        *snapshot.lock().expect("snapshot") = next_snapshot.clone();
        let next = next_snapshot.task(&current.0.kind).cloned();
        let differs = {
            let current_task = task.lock().expect("task");
            match &next {
                Some(next) => !Arc::ptr_eq(next, &current_task),
                None => true,
            }
        };
        if differs {
            if next
                .as_ref()
                .is_some_and(|next| can_reserve(next, &current.0))
            {
                let checkpoint = checkpoint_of(&current.0.state).cloned().unwrap_or_default();
                tx.set_task(with_state(&current.0, TaskState::Pending { checkpoint }));
                return Decision::End;
            }
            let already_same = {
                let already_guard = reported.lock().expect("reported");
                let already = already_guard.as_ref().and_then(|inner| inner.as_ref());
                match (already, next.as_ref()) {
                    (Some(already), Some(next)) => Arc::ptr_eq(already, next),
                    _ => false,
                }
            };
            if !already_same {
                *reported.lock().expect("reported") = Some(next.clone());
                let cause = if next.is_none() {
                    "missing_task"
                } else {
                    "incompatible_task"
                };
                (self.deps.report)(SessionError::Message(format!(
                    "Task {} keeps running under its old {} definition ({cause})",
                    current.0.id, current.0.kind
                )));
            }
        }
        Decision::Continue
    }

    /// 对应 `#runAbort(reservation)`。
    async fn run_abort(&self, reservation: &Reservation) -> Result<(), SessionError> {
        let invocation = Arc::clone(&reservation.invocation);
        let current = {
            let record = self
                .shared
                .state
                .lock()
                .expect("state")
                .live
                .get(&invocation.task_id)
                .cloned();
            record.map(RunningTask::new)
        };
        let Some(current) = current else {
            return Ok(());
        };
        if self.is_closing() {
            return Ok(());
        }
        let phase = Arc::new(Phase {
            snapshot: Arc::new(Mutex::new(reservation.snapshot.clone())),
            task_name: reservation.task.definition().name().to_string(),
            agent: Arc::new(Mutex::new(None)),
        });
        let runtime = self.runtime(&invocation, &phase);
        let failure = reservation
            .task
            .definition()
            .abort(current, runtime, invocation.context.clone())
            .await
            .err();
        let fault = failure.unwrap_or_else(|| {
            SessionError::Message(format!(
                "Abort handler of task {} returned without a terminal outcome",
                invocation.task_id
            ))
        });
        self.step(&invocation, move |_tx, _current| Decision::Fault {
            message: fault.to_string(),
        })
        .await;
        Ok(())
    }

    /// 对应 `#step(invocation, decide)`。
    async fn step<F>(&self, invocation: &Arc<Invocation>, decide: F) -> Option<RunningTask>
    where
        F: FnOnce(&Transaction, &RunningTask) -> Decision + Send + 'static,
    {
        match self.commit_step(invocation, decide).await {
            Ok(current) => current,
            Err(error) => {
                self.end(invocation);
                if !self.is_closing() {
                    (self.deps.report)(error);
                }
                None
            }
        }
    }

    async fn commit_step<F>(
        &self,
        invocation: &Arc<Invocation>,
        decide: F,
    ) -> Result<Option<RunningTask>, SessionError>
    where
        F: FnOnce(&Transaction, &RunningTask) -> Decision + Send + 'static,
    {
        let task_id = invocation.task_id;
        let invocation2 = Arc::clone(invocation);
        let scheduler = self.clone();
        self.deps
            .session
            .commit_with(
                move |tx| {
                    let invocation = Arc::clone(&invocation2);
                    let scheduler = scheduler.clone();
                    Box::pin(async move {
                        let found = {
                            let state = scheduler.shared.state.lock().expect("state");
                            state.live.get(&task_id).cloned()
                        };
                        let current = found
                            .filter(|record| record.state.status() == TaskStatus::Running)
                            .map(RunningTask::new);
                        let decision = match &current {
                            Some(current) if !scheduler.is_closing() => decide(tx, current),
                            _ => Decision::End,
                        };
                        match decision {
                            Decision::Continue => Ok(current),
                            _ => {
                                scheduler.end(&invocation);
                                if let Decision::Fault { message } = decision {
                                    let record = current.expect("fault 时 current 必存在");
                                    scheduler
                                        .terminate(
                                            tx,
                                            &record.0,
                                            SchedulerOutcome::Faulted {
                                                error: TaskOutcomeError {
                                                    message,
                                                    detail: None,
                                                },
                                            },
                                        )
                                        .await?;
                                }
                                Ok(None)
                            }
                        }
                    })
                },
                self.deps.context.clone(),
                None,
            )
            .await
    }

    /// 对应 `#terminate(tx, record, outcome)`。
    async fn terminate(
        &self,
        tx: &Transaction,
        record: &AnyTaskRecord,
        outcome: SchedulerOutcome,
    ) -> Result<(), SessionError> {
        self.load_scopes(false).await?;
        let overlay = overlay_of(tx);
        let owned = {
            let state = self.shared.state.lock().expect("state");
            state.owned_live(Some(&overlay))
        };
        let task_outcome = scheduler_outcome_to_task_outcome(&outcome);
        if owned.contains_key(&key_of(record)) {
            tx.set_task(with_state(
                record,
                TaskState::Completing {
                    outcome: task_outcome,
                },
            ));
            return Ok(());
        }
        tx.set_task(with_state(
            record,
            TaskState::Terminal {
                outcome: task_outcome,
            },
        ));
        (self.deps.settle_outcome)(tx, record.clone(), outcome).await
    }

    /// 对应 `#commitState(tx, invocation, current, next)`。
    async fn commit_state(
        &self,
        tx: &Transaction,
        invocation: &Arc<Invocation>,
        current: &RunningTask,
        next: NextTaskState,
    ) -> Result<(), SessionError> {
        if let NextTaskState::Waiting { on, policy, .. } = &next {
            self.validate_wait(tx, invocation, current, on, *policy)
                .await?;
        }
        if let NextTaskState::Terminal { outcome } = &next {
            let overlay = overlay_of(tx);
            self.load_scopes(false).await?;
            for record in overlay.tasks.values() {
                self.load_chain(parent_of(&node_of(record)), Some(&overlay))
                    .await?;
            }
            let owned = {
                let state = self.shared.state.lock().expect("state");
                state.owned_live(Some(&overlay))
            };
            if owned.contains_key(&key_of(&current.0)) {
                tx.set_task(with_state(
                    &current.0,
                    TaskState::Completing {
                        outcome: outcome.clone(),
                    },
                ));
                return Ok(());
            }
        }
        tx.set_task(with_state(&current.0, next_task_state_to_task_state(next)));
        Ok(())
    }

    /// 对应 `#validateWait(...)`。
    async fn validate_wait(
        &self,
        _tx: &Transaction,
        invocation: &Arc<Invocation>,
        current: &RunningTask,
        on: &[TaskId],
        policy: JoinPolicy,
    ) -> Result<(), SessionError> {
        if invocation.mode == InvocationMode::Abort {
            return Err(SessionError::Message(format!(
                "Abort handler of task {} cannot wait",
                current.0.id
            )));
        }
        let overlay = overlay_of(_tx);
        let parent = parent_of(&node_of(&current.0));
        self.load_chain(parent, None).await?;
        let owners: BTreeSet<TaskId> = {
            let state = self.shared.state.lock().expect("state");
            state
                .above(parent, None)
                .into_iter()
                .filter_map(|step| match step {
                    Step::Task { task, .. } => Some(task),
                    _ => None,
                })
                .collect()
        };
        let self_key = key_of(&current.0);
        for id in on {
            let id_key = TaskId::new(id.get());
            if id_key == self_key || owners.contains(&id_key) {
                return Err(SessionError::Message(format!(
                    "Task {} cannot wait on itself or its owner {id_key}",
                    current.0.id
                )));
            }
            let member = overlay.tasks.get(&id_key).cloned().or_else(|| {
                self.shared
                    .state
                    .lock()
                    .expect("state")
                    .live
                    .get(&id_key)
                    .cloned()
            });
            let member = match member {
                Some(member) => Some(member),
                None => self
                    .deps
                    .storage
                    .task(id_key, self.deps.context.as_ref())
                    .await
                    .map_err(SessionError::Storage)?,
            };
            let Some(member) = member else {
                return Err(SessionError::Message(format!(
                    "Task {id_key} does not exist"
                )));
            };
            if policy == JoinPolicy::FailFast && member.owner != Some(self_key) {
                return Err(SessionError::Message(format!(
                    "Task {} can wait failFast only on tasks it owns; {id_key} is not one",
                    current.0.id
                )));
            }
        }
        Ok(())
    }

    /// 对应 `#end(invocation)`。
    fn end(&self, invocation: &Arc<Invocation>) {
        if invocation.ended.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut invocations = self.shared.invocations.lock().expect("inv");
        if invocations
            .get(&invocation.task_id)
            .is_some_and(|current| Arc::ptr_eq(current, invocation))
        {
            invocations.remove(&invocation.task_id);
        }
        drop(invocations);
        let watches: Vec<Arc<DocumentWatch>> =
            std::mem::take(&mut *invocation.watches.lock().expect("watches"));
        for watch in watches {
            let _ = watch.stop();
        }
        invocation.controller.abort();
    }

    /// 对应 `#idle(conversationId)`。
    fn idle(&self, conversation_id: Option<ConversationId>) -> bool {
        self.shared
            .state
            .lock()
            .expect("state")
            .idle(conversation_id)
    }

    // ─── 调用运行时 ────────────────────────────────────────────────────────

    /// 对应 `#runtime(invocation, phase)`。
    fn runtime(&self, invocation: &Arc<Invocation>, phase: &Arc<Phase>) -> Arc<dyn TaskRuntime> {
        Arc::new(InvocationRuntime {
            scheduler: self.clone(),
            invocation: Arc::clone(invocation),
            phase: Arc::clone(phase),
        })
    }

    /// 对应 `#read(invocation, read)`。
    async fn read<T, F, Fut>(
        &self,
        invocation: &Arc<Invocation>,
        read: F,
    ) -> Result<T, SessionError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, SessionError>>,
    {
        if invocation.is_ended() {
            return Err(ended_error(invocation));
        }
        read().await
    }

    /// 对应 `#gated(invocation, change, context)`。
    async fn gated<T>(
        &self,
        invocation: &Arc<Invocation>,
        change: impl for<'a> FnOnce(
            &'a Transaction,
            RunningTask,
        ) -> BoxFuture<'a, Result<T, SessionError>>
        + Send
        + 'static,
        context: Arc<dyn Context>,
    ) -> Result<T, SessionError>
    where
        T: Send + 'static,
    {
        if invocation.is_ended() {
            return Err(ended_error(invocation));
        }
        let task_id = invocation.task_id;
        let conversation_id = invocation.conversation_id;
        let mode = invocation.mode;
        let invocation2 = Arc::clone(invocation);
        let scheduler = self.clone();
        self.deps
            .session
            .commit_with(
                move |tx| {
                    let invocation = Arc::clone(&invocation2);
                    let scheduler = scheduler.clone();
                    Box::pin(async move {
                        if invocation.is_ended() {
                            return Err(ended_error(&invocation));
                        }
                        if scheduler.shared.state.lock().expect("state").closing {
                            return Err(closed_error());
                        }
                        let found = {
                            let state = scheduler.shared.state.lock().expect("state");
                            state.live.get(&task_id).cloned()
                        };
                        let Some(found) = found else {
                            return Err(SessionError::Message(format!(
                                "Task {task_id} is terminal"
                            )));
                        };
                        if found.state.status() != TaskStatus::Running {
                            return Err(SessionError::Message(format!(
                                "Task {task_id} is {}",
                                task_status_str(found.state.status())
                            )));
                        }
                        let current = RunningTask::new(found);
                        if mode == InvocationMode::Run && current.0.abort_requested {
                            return Err(SessionError::Message(format!(
                                "Task {task_id} has a durable abort mark"
                            )));
                        }
                        change(tx, current).await
                    })
                },
                context,
                Some(TransactionScope {
                    conversation_id: Some(conversation_id),
                    task_id: Some(task_id),
                }),
            )
            .await
    }

    /// 对应 `#sleep(invocation, until, context)`。
    async fn sleep(
        &self,
        invocation: &Arc<Invocation>,
        until: u64,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        if invocation.is_ended() {
            return Err(ended_error(invocation));
        }
        let mut signals = vec![invocation.controller.clone()];
        if let Some(signal) = context.abort_signal() {
            signals.push(signal);
        }
        let signal = AbortSignal::any(&signals);
        loop {
            signal.throw_if_aborted().map_err(SessionError::Aborted)?;
            let remaining = until.saturating_sub((self.deps.now)());
            if remaining == 0 {
                return Ok(());
            }
            let delay = remaining.min(MAX_TIMER_DELAY);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                _ = signal.cancelled() => {
                    return Err(SessionError::Aborted(AbortError));
                }
            }
        }
    }

    /// 对应 `#watchDoc(invocation, ...)`。
    async fn watch_doc(
        &self,
        invocation: &Arc<Invocation>,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<DocumentWatch>>, SessionError> {
        if invocation.is_ended() {
            return Err(ended_error(invocation));
        }
        let watch = self
            .deps
            .session
            .watch_doc(token, owner, key, context)
            .await?;
        let Some(watch) = watch else { return Ok(None) };
        if invocation.is_ended() {
            let _ = watch.stop();
            return Err(ended_error(invocation));
        }
        invocation
            .watches
            .lock()
            .expect("watches")
            .push(Arc::clone(&watch));
        let closed = watch.closed();
        let invocation_for_watch = Arc::clone(invocation);
        let watch_clone = Arc::clone(&watch);
        tokio::spawn(async move {
            let _ = closed.await;
            invocation_for_watch
                .watches
                .lock()
                .expect("watches")
                .retain(|current| !Arc::ptr_eq(current, &watch_clone));
        });
        Ok(Some(watch))
    }

    // ─── 内部辅助 ─────────────────────────────────────────────────────────

    fn is_closing(&self) -> bool {
        self.shared.state.lock().expect("state").closing
    }

    fn is_enabled(&self) -> bool {
        self.shared.state.lock().expect("state").enabled
    }

    /// 对应 `#settleIdle()`。
    fn settle_idle(&self) {
        let plan = {
            let now = (self.deps.now)();
            let retention = expiry::context_retention_ms(&(self.deps.settings)());
            let current_expiry = self
                .shared
                .expiry
                .lock()
                .expect("expiry")
                .as_ref()
                .map(|h| h.at);
            let mut state = self.shared.state.lock().expect("state");
            expiry::settle_idle(
                &mut state,
                now,
                retention,
                &self.shared.idle_waiters,
                current_expiry,
            )
        };
        self.apply_expiry_plan(plan);
    }

    /// 对应 `#scheduleExpiry()` 的执行部分。
    fn schedule_expiry(&self) {
        let plan = {
            let now = (self.deps.now)();
            let retention = expiry::context_retention_ms(&(self.deps.settings)());
            let current_expiry = self
                .shared
                .expiry
                .lock()
                .expect("expiry")
                .as_ref()
                .map(|h| h.at);
            let state = self.shared.state.lock().expect("state");
            expiry::schedule_expiry(&state, now, retention, current_expiry)
        };
        self.apply_expiry_plan(plan);
    }

    fn apply_expiry_plan(&self, plan: ExpiryPlan) {
        match plan {
            ExpiryPlan::Unchanged => {}
            ExpiryPlan::Cancel => {
                if let Some(handle) = self.shared.expiry.lock().expect("expiry").take() {
                    handle.signal.abort();
                }
            }
            ExpiryPlan::Schedule { at, after_ms } => {
                if let Some(handle) = self.shared.expiry.lock().expect("expiry").take() {
                    handle.signal.abort();
                }
                let signal = AbortSignal::new();
                let scheduler = self.clone();
                let task_signal = signal.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(after_ms)) => {
                            *scheduler.shared.expiry.lock().expect("expiry") = None;
                            scheduler.settle_idle();
                        }
                        _ = task_signal.cancelled() => {}
                    }
                });
                *self.shared.expiry.lock().expect("expiry") = Some(ExpiryHandle { at, signal });
            }
        }
    }
}

/// 对应 `#runtime` 返回的 `ErasedRuntime`。
struct InvocationRuntime {
    scheduler: TaskScheduler,
    invocation: Arc<Invocation>,
    phase: Arc<Phase>,
}

/// 对应 hooks 的 [`HookRunner`]：读取 phase 里懒解析的 agent。
struct InvocationHooks {
    invocation: Arc<Invocation>,
    phase: Arc<Phase>,
}

impl HookRunner for InvocationHooks {
    fn handlers(&self) -> Vec<Arc<HookHandlers>> {
        if self.invocation.is_ended() {
            return Vec::new();
        }
        let cache = self.phase.agent.lock().expect("agent");
        let Some(Ok(agent)) = cache.as_ref() else {
            return Vec::new();
        };
        agent_hooks(agent, &self.phase.task_name)
            .into_iter()
            .map(|handlers| Arc::new(handlers.clone()))
            .collect()
    }
}

#[async_trait::async_trait]
impl TaskRuntime for InvocationRuntime {
    fn task_id(&self) -> TaskId {
        self.invocation.task_id
    }

    fn conversation_id(&self) -> ConversationId {
        self.invocation.conversation_id
    }

    fn signal(&self) -> AbortSignal {
        self.invocation.controller.clone()
    }

    fn registry(&self) -> RegistrySnapshot {
        self.phase.snapshot.lock().expect("snapshot").clone()
    }

    async fn agent(&self, context: Arc<dyn Context>) -> Result<Agent, SessionError> {
        if self.invocation.is_ended() {
            return Err(ended_error(&self.invocation));
        }
        {
            let cache = self.phase.agent.lock().expect("agent");
            if let Some(result) = cache.as_ref() {
                return result.clone();
            }
        }
        let snapshot = self.phase.snapshot.lock().expect("snapshot").clone();
        let future = (self.scheduler.deps.agent)(
            self.invocation.conversation_id,
            snapshot,
            self.invocation.context.clone(),
        );
        let result = match await_with_context(future, context.as_ref()).await {
            Ok(result) => result,
            Err(AbortError) => Err(SessionError::Aborted(AbortError)),
        };
        let mut cache = self.phase.agent.lock().expect("agent");
        if cache.is_none() {
            *cache = Some(result.clone());
        }
        result
    }

    fn settings(&self) -> Settings {
        (self.scheduler.deps.settings)()
    }

    fn models(&self) -> Arc<pi_ai::Models> {
        Arc::clone(&self.scheduler.deps.models)
    }

    async fn env(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn ExecutionEnv>>, SessionError> {
        if self.invocation.is_ended() {
            return Err(ended_error(&self.invocation));
        }
        (self.scheduler.deps.env)(self.invocation.conversation_id, context).await
    }

    fn hooks(&self) -> Arc<dyn HookRunner> {
        Arc::new(InvocationHooks {
            invocation: Arc::clone(&self.invocation),
            phase: Arc::clone(&self.phase),
        })
    }

    async fn commit(
        &self,
        change: TaskCommit<'static>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let invocation_for_closure = Arc::clone(&invocation);
        let scheduler = self.scheduler.clone();
        let scheduler_for_closure = scheduler.clone();
        scheduler
            .gated(
                &invocation,
                move |tx, current| {
                    let invocation = Arc::clone(&invocation_for_closure);
                    let scheduler = scheduler_for_closure.clone();
                    let next = change(tx, current.clone());
                    Box::pin(async move {
                        let next = next.await?;
                        if let Some(next) = next {
                            scheduler
                                .commit_state(tx, &invocation, &current, next)
                                .await?;
                        }
                        Ok(())
                    })
                },
                context,
            )
            .await
    }

    async fn memo(
        &self,
        name: &str,
        _context: Arc<dyn Context>,
    ) -> Result<Option<serde_json::Value>, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let invocation_for_closure = Arc::clone(&invocation);
        let scheduler = self.scheduler.clone();
        let name = name.to_string();
        let scheduler_closure = scheduler.clone();
        scheduler
            .read(&invocation, move || {
                let scheduler = scheduler_closure.clone();
                let invocation = invocation_for_closure.clone();
                let name = name.clone();
                async move {
                    let record = scheduler
                        .shared
                        .state
                        .lock()
                        .expect("state")
                        .live
                        .get(&invocation.task_id)
                        .cloned();
                    Ok(memo_of(record.as_ref(), &name).cloned())
                }
            })
            .await
    }

    async fn memo_or(
        &self,
        name: &str,
        candidate: serde_json::Value,
        context: Arc<dyn Context>,
    ) -> Result<serde_json::Value, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let scheduler = self.scheduler.clone();
        let name = name.to_string();
        scheduler
            .gated(
                &invocation,
                move |tx, current| {
                    let winner = memo_of(Some(&current.0), &name).cloned();
                    let name = name.clone();
                    let candidate = candidate.clone();
                    Box::pin(async move {
                        if let Some(winner) = winner {
                            return Ok(winner);
                        }
                        let mut updated = current.0.clone();
                        let memos = updated.memos.get_or_insert_with(Default::default);
                        memos.insert(name, candidate.clone());
                        tx.set_task(updated);
                        Ok(candidate)
                    })
                },
                context,
            )
            .await
    }

    async fn get_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<Option<AnyTaskRecord>, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let scheduler = self.scheduler.clone();
        let id_key = TaskId::new(id.get());
        let scheduler_closure = scheduler.clone();
        scheduler
            .read(&invocation, move || {
                let scheduler = scheduler_closure.clone();
                let context = Arc::clone(&context);
                let session = scheduler.deps.session.clone();
                let storage = scheduler.deps.storage.clone();
                async move {
                    session
                        .read_on_line(move || {
                            let storage = storage.clone();
                            let context = Arc::clone(&context);
                            async move {
                                storage
                                    .task(id_key, context.as_ref())
                                    .await
                                    .map_err(SessionError::Storage)
                            }
                        })
                        .await
                }
            })
            .await
    }

    async fn wait_for_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<SettledTask, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let invocation_closure = invocation.clone();
        let scheduler = self.scheduler.clone();
        let scheduler_closure = scheduler.clone();
        scheduler
            .read(&invocation, move || {
                let scheduler = scheduler_closure.clone();
                let invocation = invocation_closure.clone();
                let context = with_abort_signal(invocation.controller.clone(), context);
                async move { scheduler.wait_for_task(id, context).await }
            })
            .await
    }

    async fn outcomes(
        &self,
        ids: &[TaskId],
        context: Arc<dyn Context>,
    ) -> Result<Vec<TaskOutcome<serde_json::Value>>, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let scheduler = self.scheduler.clone();
        let ids: Vec<TaskId> = ids.to_vec();
        let scheduler_closure = scheduler.clone();
        scheduler
            .read(&invocation, move || {
                let scheduler = scheduler_closure.clone();
                let context = Arc::clone(&context);
                let session = scheduler.deps.session.clone();
                let storage = scheduler.deps.storage.clone();
                async move {
                    session
                        .read_on_line(move || {
                            let storage = storage.clone();
                            let context = Arc::clone(&context);
                            let ids = ids.clone();
                            async move {
                                let mut outcomes = Vec::new();
                                for id in ids {
                                    let record = storage
                                        .task(id, context.as_ref())
                                        .await
                                        .map_err(SessionError::Storage)?;
                                    let Some(record) = record else {
                                        return Err(SessionError::Message(format!(
                                            "Task {id} is not terminal"
                                        )));
                                    };
                                    match record.state {
                                        TaskState::Terminal { outcome } => outcomes.push(outcome),
                                        _ => {
                                            return Err(SessionError::Message(format!(
                                                "Task {id} is not terminal"
                                            )));
                                        }
                                    }
                                }
                                Ok(outcomes)
                            }
                        })
                        .await
                }
            })
            .await
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn ConversationHandle>>, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let scheduler = self.scheduler.clone();
        let scheduler_closure = scheduler.clone();
        let invocation_closure = invocation.clone();
        scheduler
            .read(&invocation, move || {
                let invocation = Arc::clone(&invocation_closure);
                let scheduler = scheduler_closure.clone();
                let context = Arc::clone(&context);
                let signal = invocation.controller.clone();
                let check: Arc<dyn Fn() -> Result<(), SessionError> + Send + Sync> = {
                    let invocation = Arc::clone(&invocation);
                    Arc::new(move || {
                        if invocation.is_ended() {
                            Err(ended_error(&invocation))
                        } else {
                            Ok(())
                        }
                    })
                };
                let conversation = scheduler.deps.conversation.clone();
                async move { conversation(id, InvocationBinding { signal, check }, context).await }
            })
            .await
    }

    async fn entry(
        &self,
        id: EntryId,
        context: Arc<dyn Context>,
    ) -> Result<Option<EntryRecord>, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let scheduler = self.scheduler.clone();
        let conversation_id = self.invocation.conversation_id;
        let scheduler_closure = scheduler.clone();
        scheduler
            .read(&invocation, move || {
                let scheduler = scheduler_closure.clone();
                let context = Arc::clone(&context);
                let session = scheduler.deps.session.clone();
                let storage = scheduler.deps.storage.clone();
                async move {
                    session
                        .read_on_line(move || {
                            let storage = storage.clone();
                            let context = Arc::clone(&context);
                            async move {
                                let found = storage
                                    .entry_in_conversation(conversation_id, id, context.as_ref())
                                    .await
                                    .map_err(SessionError::Storage)?;
                                Ok(found.map(|(entry, _seq)| entry))
                            }
                        })
                        .await
                }
            })
            .await
    }

    async fn context_view(
        &self,
        conversation_id: ConversationId,
        context: Arc<dyn Context>,
        at: Option<EntryId>,
    ) -> Result<ContextView, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let scheduler = self.scheduler.clone();
        let scheduler_closure = scheduler.clone();
        let invocation_closure = invocation.clone();
        scheduler
            .read(&invocation, move || {
                let invocation = Arc::clone(&invocation_closure);
                let scheduler = scheduler_closure.clone();
                let context = Arc::clone(&context);
                async move {
                    let previous = scheduler
                        .shared
                        .state
                        .lock()
                        .expect("state")
                        .contexts
                        .get(&conversation_id)
                        .map(|kept| kept.range.clone());
                    let (view, range) = read_context_from(
                        &scheduler.deps.session,
                        scheduler.deps.storage.as_ref(),
                        conversation_id,
                        context,
                        at,
                        previous,
                    )
                    .await?;
                    let Some(range) = range else { return Ok(view) };
                    let tail = range.bounds.tail;
                    let kept_tail = scheduler
                        .shared
                        .state
                        .lock()
                        .expect("state")
                        .contexts
                        .get(&conversation_id)
                        .map(|kept| kept.range.bounds.tail);
                    if scheduler.is_closing()
                        || invocation.is_ended()
                        || kept_tail.is_some_and(|kept| kept > tail)
                    {
                        return Ok(view);
                    }
                    let idle = scheduler.idle(Some(conversation_id));
                    if !idle {
                        scheduler
                            .shared
                            .state
                            .lock()
                            .expect("state")
                            .contexts
                            .insert(
                                conversation_id,
                                KeptContext {
                                    range,
                                    idle_since: None,
                                },
                            );
                    } else {
                        let retention = expiry::context_retention_ms(&(scheduler.deps.settings)());
                        if retention > 0 {
                            let now = (scheduler.deps.now)();
                            let idle_since = scheduler
                                .shared
                                .state
                                .lock()
                                .expect("state")
                                .contexts
                                .get(&conversation_id)
                                .and_then(|kept| kept.idle_since)
                                .unwrap_or(now);
                            scheduler
                                .shared
                                .state
                                .lock()
                                .expect("state")
                                .contexts
                                .insert(
                                    conversation_id,
                                    KeptContext {
                                        range,
                                        idle_since: Some(idle_since),
                                    },
                                );
                            scheduler.schedule_expiry();
                        }
                    }
                    Ok(view)
                }
            })
            .await
    }

    fn now(&self) -> u64 {
        assert!(
            !self.invocation.is_ended(),
            "Task {} invocation has ended",
            self.invocation.task_id
        );
        (self.scheduler.deps.now)()
    }

    fn report(&self, error: SessionError) {
        assert!(
            !self.invocation.is_ended(),
            "Task {} invocation has ended",
            self.invocation.task_id
        );
        (self.scheduler.deps.report)(error)
    }

    async fn sleep(&self, until: u64, context: Arc<dyn Context>) -> Result<(), SessionError> {
        self.scheduler.sleep(&self.invocation, until, context).await
    }
}

#[async_trait::async_trait]
impl crate::session::DocumentReader for InvocationRuntime {
    async fn snapshot(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let scheduler = self.scheduler.clone();
        let scheduler_closure = scheduler.clone();
        scheduler
            .read(&invocation, move || {
                let scheduler = scheduler_closure.clone();
                let session = scheduler.deps.session.clone();
                async move { session.snapshot(token, owner, key, context).await }
            })
            .await
    }

    async fn snapshot_as_of(
        &self,
        token: &dyn AnyDocToken,
        owner: u64,
        key: Option<String>,
        at: EntryId,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        let invocation = Arc::clone(&self.invocation);
        let scheduler = self.scheduler.clone();
        let scheduler_closure = scheduler.clone();
        scheduler
            .read(&invocation, move || {
                let scheduler = scheduler_closure.clone();
                let session = scheduler.deps.session.clone();
                async move { session.snapshot_as_of(token, owner, key, at, context).await }
            })
            .await
    }
}

#[async_trait::async_trait]
impl crate::session::DocumentObserver for InvocationRuntime {
    async fn watch_doc(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<DocumentWatch>>, SessionError> {
        self.scheduler
            .watch_doc(&self.invocation, token, owner, key, context)
            .await
    }
}

// ─── 辅助函数 ─────────────────────────────────────────────────────────────

fn key_of(record: &AnyTaskRecord) -> TaskId {
    TaskId::new(record.id.get())
}

fn checkpoint_of(
    state: &TaskState<serde_json::Value, serde_json::Value>,
) -> Option<&serde_json::Value> {
    match state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => Some(checkpoint),
        TaskState::Completing { .. } | TaskState::Terminal { .. } => None,
    }
}

fn phase_of(checkpoint: &serde_json::Value) -> Option<&str> {
    checkpoint.get("phase").and_then(|value| value.as_str())
}

fn ended_error(invocation: &Invocation) -> SessionError {
    SessionError::Message(format!("Task {} invocation has ended", invocation.task_id))
}

fn blocked_reason_str(reason: BlockedReason) -> String {
    match reason {
        BlockedReason::MissingTask => "missing_task".to_string(),
        BlockedReason::TaskTooOld => "task_too_old".to_string(),
        BlockedReason::MigrationFailed => "migration_failed".to_string(),
    }
}

fn task_status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Running => "running",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Completing => "completing",
        TaskStatus::Terminal => "terminal",
    }
}

fn scheduler_outcome_to_task_outcome(outcome: &SchedulerOutcome) -> TaskOutcome<serde_json::Value> {
    match outcome {
        SchedulerOutcome::Faulted { error } => TaskOutcome::Faulted {
            error: error.clone(),
        },
        SchedulerOutcome::Orphaned { reason } => TaskOutcome::Orphaned {
            reason: reason.clone(),
        },
    }
}

fn next_task_state_to_task_state(
    next: NextTaskState,
) -> TaskState<serde_json::Value, serde_json::Value> {
    match next {
        NextTaskState::Running { checkpoint } => TaskState::Running { checkpoint },
        NextTaskState::Waiting {
            checkpoint,
            on,
            policy,
        } => TaskState::Waiting {
            checkpoint,
            on,
            policy,
        },
        NextTaskState::Terminal { outcome } => TaskState::Terminal { outcome },
    }
}

fn overlay_of(tx: &Transaction) -> Overlay {
    Overlay {
        tasks: tx
            .staged_tasks()
            .into_iter()
            .map(|record| (key_of(&record), record))
            .collect(),
        edges: tx
            .staged_conversations()
            .into_iter()
            .map(|record| (record.id, record.owner.map(|owner| owner.task_id)))
            .collect(),
    }
}

fn mark(tx: &Transaction, record: &AnyTaskRecord, marked: &mut BTreeSet<TaskId>) {
    let key = key_of(record);
    if record.abort_requested || marked.contains(&key) {
        return;
    }
    marked.insert(key);
    let mut updated = record.clone();
    updated.abort_requested = true;
    tx.set_task(updated);
}
