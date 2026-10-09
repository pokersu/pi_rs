//! 对应 `harness/harness.ts`：把一个 Session 扩成 durable agent harness 的组装层（P5h）。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - **继承 → 委托 + 钩子**：上游 `HarnessImpl extends SessionImpl` 并覆盖 `conversationCreated` /
//!   `beforeClose`；Rust 用 [`SessionHooks`] 注入这两个钩子（见 [`HarnessHooks`]），`HarnessImpl`
//!   持有 `Arc<SessionImpl>` 并把 [`Session`] 的方法面委托过去。
//! - **循环依赖用 `OnceLock` 打破**：`TaskScheduler` 的 `conversation` 依赖 `Submissions`，
//!   `Submissions` 的 `resume` 又依赖 `TaskScheduler`；`HarnessImpl` 自身还要被 scheduler 的
//!   `agent` / `env` 闭包引用。三者各自用 `OnceLock` 延迟绑定。
//! - **`WeakMap<object, …>` 的 `conversationViews` 改为 Harness 直接持有 [`ConversationViews`]**。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use pi_ai::{Message, UserMessage};
use serde_json::Value as JsonValue;

use crate::chord::context::{Context, with_abort_signal, without_abort_signal};
use crate::chord::state::AttachedReplicatedState;
use crate::entries::RESET_ENTRY;
use crate::harness::agent::{AGENT_DOC, configure, create_agent, resolve_agent, resolve_settings};
use crate::harness::compaction::{CompactionInput, CreateCompaction, make_create_compaction};
use crate::harness::context::read_context;
use crate::harness::generation::make_start_run;
use crate::harness::inbox::{INBOX_DOC, QueueModes, withdraw_queued_inputs};
use crate::harness::live::{LIVE_DOC, settle_scheduler_outcome};
use crate::harness::provider::PROVIDER_DOC;
use crate::harness::scheduler::exec::{
    AbortResult, InvocationBinding, SchedulerAgent, SchedulerConversation, SchedulerEnv,
    SchedulerSettings, SchedulerSettleOutcome, SchedulerWithdrawInputs, TaskScheduler,
    TaskSchedulerOptions,
};
use crate::harness::submissions::{Submissions, SubmissionsOptions};
use crate::harness::task_graph::TaskGraphView;
use crate::harness::types::{
    Agent, AgentChange, AgentState, CommitOperation, Conversation, ConversationAbortOptions,
    ConversationCreateOptions, ConversationCreated, ConversationHandle, ConversationInit,
    ConversationWatch, EnvBuilder, EnvTarget, Harness, HarnessAbortSubmission, HarnessInspection,
    HarnessOptions, RegistryReader, RegistrySnapshot, Submission, SubmissionAbort, SubmissionDraft,
    TaskAbort, UserInput,
};
use crate::harness::usage::{USAGE_DOC, UsageState, add_usage_state};
use crate::harness::util::scan_all;
use crate::harness::view::ConversationViews;
use crate::session::session::{CloseListener, CommitListener};
use crate::session::transaction::{AnyTaskRecord, Transaction, TransactionScope};
use crate::session::{
    DocumentObserver, DocumentReader, Session, SessionError, SessionHooks, SessionImpl,
    SessionOptions, SessionSubscription, default_now,
};
use crate::types::{
    ConversationId, ConversationOwnership, ConversationQuery, ConversationRecord, Cursor,
    DocAccess, EntryDraft, EntryHead, EntryId, EntryQuery, EntryRecord, JsonObject, Page,
    ROOT_CONVERSATION_ID, Storage, SubmissionId, SubmissionQuery, SubmissionStatus, TaskId,
    WatchHandle,
};

/// 对应 `SCAN_PAGE_SIZE`。
const SCAN_PAGE_SIZE: usize = 256;

/// 内置任务名（对应上游 `BUILTIN_TASKS` 的三个定义名）。
const BUILTIN_TASK_NAMES: [&str; 3] = ["pi.generation", "pi.tool", "pi.compaction"];

/// 对应 `HarnessImpl` 覆盖的 `conversationCreated` / `beforeClose`。
struct HarnessHooks {
    conversation_created: Option<ConversationCreated>,
    tasks: Arc<OnceLock<TaskScheduler>>,
}

#[async_trait::async_trait]
impl SessionHooks for HarnessHooks {
    async fn conversation_created(
        &self,
        tx: &Transaction,
        record: &ConversationRecord,
    ) -> Result<(), SessionError> {
        let access = DocAccess {
            owner: Some(record.id.get()),
            key: None,
        };
        tx.doc(&*LIVE_DOC, access.clone(), None).await?;
        tx.doc(&*INBOX_DOC, access.clone(), None).await?;
        tx.doc(&*USAGE_DOC, access.clone(), None).await?;
        tx.doc(&*PROVIDER_DOC, access, None).await?;
        create_agent(tx, record).await?;
        if let Some(hook) = &self.conversation_created {
            hook(tx, *record).await?;
        }
        Ok(())
    }

    async fn before_close(&self) -> Result<(), SessionError> {
        if let Some(tasks) = self.tasks.get() {
            tasks.join().await;
        }
        Ok(())
    }
}

/// 对应 `CreateTarget`：会话创建的三种目标。
enum CreateTarget {
    Root,
    Independent,
    Fork { parent: ConversationId, at: EntryId },
}

/// 对应 `HarnessImpl`：一个 Session 上的 durable agent harness。
pub struct HarnessImpl {
    session: Arc<SessionImpl>,
    storage: Arc<dyn Storage>,
    registry: Arc<dyn RegistryReader>,
    settings: Option<crate::harness::types::HarnessSettings>,
    env: Option<EnvBuilder>,
    report: Arc<dyn Fn(SessionError) + Send + Sync>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    tasks: TaskScheduler,
    submissions: Arc<Submissions>,
    task_graph: Arc<TaskGraphView>,
    views: Arc<ConversationViews>,
    create_compaction: CreateCompaction,
    closed: AtomicBool,
    self_ref: OnceLock<Weak<HarnessImpl>>,
}

impl HarnessImpl {
    fn self_arc(&self) -> Arc<Self> {
        self.self_ref
            .get()
            .and_then(Weak::upgrade)
            .expect("harness self reference")
    }

    fn assert_open(&self) -> Result<(), SessionError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(crate::harness::util::closed_error());
        }
        Ok(())
    }

    /// 对应 `HarnessImpl` 构造函数：组装 session、scheduler、submissions 与视图。
    fn new(
        storage: Arc<dyn Storage>,
        options: HarnessOptions,
        context: Arc<dyn Context>,
    ) -> Arc<Self> {
        let HarnessOptions {
            models,
            registry,
            settings,
            env,
            conversation_created,
            now: now_option,
            on_report,
        } = options;

        let now = now_option.unwrap_or_else(|| Arc::new(default_now));
        let report: Arc<dyn Fn(SessionError) + Send + Sync> =
            on_report.unwrap_or_else(|| Arc::new(|_| {}));

        let self_cell: Arc<OnceLock<Arc<HarnessImpl>>> = Arc::new(OnceLock::new());
        let tasks_cell: Arc<OnceLock<TaskScheduler>> = Arc::new(OnceLock::new());
        let submissions_cell: Arc<OnceLock<Arc<Submissions>>> = Arc::new(OnceLock::new());

        // 内置任务从注册表快照取得（打开时校验过它们存在）。
        let snapshot = registry.snapshot();
        let generation_task = snapshot
            .task("pi.generation")
            .expect("builtin pi.generation")
            .clone();
        let compaction_task = snapshot
            .task("pi.compaction")
            .expect("builtin pi.compaction")
            .clone();
        let start_run = make_start_run(Arc::new(move || Arc::clone(&generation_task)));
        let create_compaction = make_create_compaction(Arc::clone(&compaction_task));

        let hooks = Arc::new(HarnessHooks {
            conversation_created,
            tasks: Arc::clone(&tasks_cell),
        });
        let session = SessionImpl::create_with_hooks(
            Arc::clone(&storage),
            hooks,
            SessionOptions {
                now: Some(Arc::clone(&now)),
            },
        );

        let agent: SchedulerAgent = Arc::new({
            let cell = Arc::clone(&self_cell);
            move |id, snapshot, ctx| {
                let cell = Arc::clone(&cell);
                Box::pin(async move {
                    let harness = cell.get().expect("harness").clone();
                    harness.resolve_agent(id, Some(snapshot), ctx).await
                })
            }
        });

        let settings_fn: SchedulerSettings = Arc::new({
            let settings = settings.clone();
            move || resolve_settings(settings.as_ref())
        });

        let env_fn: SchedulerEnv = Arc::new({
            let cell = Arc::clone(&self_cell);
            move |id, ctx| {
                let cell = Arc::clone(&cell);
                Box::pin(async move {
                    let harness = cell.get().expect("harness").clone();
                    harness.build_env(id, ctx).await
                })
            }
        });

        let settle_outcome: SchedulerSettleOutcome =
            Arc::new(|tx, record, outcome| Box::pin(settle_scheduler_outcome(tx, record, outcome)));
        let withdraw_inputs: SchedulerWithdrawInputs =
            Arc::new(|tx, id| Box::pin(withdraw_queued_inputs(tx, id)));

        let conversation: SchedulerConversation = Arc::new({
            let tasks_cell = Arc::clone(&tasks_cell);
            let submissions_cell = Arc::clone(&submissions_cell);
            let storage = Arc::clone(&storage);
            let session = Arc::clone(&session);
            move |id, binding, ctx| {
                let tasks = tasks_cell.get().expect("tasks").clone();
                let submissions = submissions_cell.get().expect("submissions").clone();
                let storage = Arc::clone(&storage);
                let session = Arc::clone(&session);
                Box::pin(async move {
                    let record = session
                        .read_on_line({
                            let storage = Arc::clone(&storage);
                            let ctx = Arc::clone(&ctx);
                            move || {
                                let storage = Arc::clone(&storage);
                                let ctx = Arc::clone(&ctx);
                                async move {
                                    storage
                                        .conversation(id, ctx.as_ref())
                                        .await
                                        .map_err(SessionError::from)
                                }
                            }
                        })
                        .await?;
                    Ok(record
                        .map(|record| bound_conversation(record.id, binding, submissions, tasks)))
                })
            }
        });

        let tasks = TaskScheduler::new(TaskSchedulerOptions {
            session: Arc::clone(&session),
            storage: Arc::clone(&storage),
            registry: Arc::clone(&registry),
            models,
            agent,
            settings: settings_fn,
            env: env_fn,
            now: Arc::clone(&now),
            report: Arc::clone(&report),
            settle_outcome,
            withdraw_inputs,
            conversation,
            context: without_abort_signal(Arc::clone(&context)),
        });
        tasks_cell.set(tasks.clone()).ok();

        let submissions = Submissions::new(SubmissionsOptions {
            session: Arc::clone(&session),
            storage: Arc::clone(&storage),
            now: Arc::clone(&now),
            queue_modes: {
                let settings = settings.clone();
                Arc::new(move || QueueModes::from_settings(&resolve_settings(settings.as_ref())))
            },
            resume: {
                let cell = Arc::clone(&tasks_cell);
                Arc::new(move || cell.get().expect("tasks").resume())
            },
            start_run,
        });
        submissions_cell.set(Arc::clone(&submissions)).ok();

        let task_graph = TaskGraphView::new(Arc::clone(&session), Arc::clone(&storage));
        let views = ConversationViews::new(Arc::clone(&session), Arc::clone(&storage));

        let harness = Arc::new(HarnessImpl {
            session,
            storage,
            registry,
            settings,
            env,
            report,
            now,
            tasks,
            submissions,
            task_graph,
            views,
            create_compaction,
            closed: AtomicBool::new(false),
            self_ref: OnceLock::new(),
        });
        let _ = harness.self_ref.set(Arc::downgrade(&harness));
        harness
    }

    /// 对应 `openTasks(context)`：装载活动任务并把崩溃遗留的 `running` 改回 `pending`。
    async fn open_tasks(&self, context: Arc<dyn Context>) -> Result<(), SessionError> {
        self.tasks.open(context).await
    }

    /// 对应 `resolveAgent`：以 `snapshot`（或当前快照）与当前设置解析会话 agent。
    async fn resolve_agent(
        &self,
        id: ConversationId,
        snapshot: Option<RegistrySnapshot>,
        context: Arc<dyn Context>,
    ) -> Result<Agent, SessionError> {
        let registry = snapshot.unwrap_or_else(|| self.registry.snapshot());
        let state = self
            .session
            .snapshot(&*AGENT_DOC, Some(id.get()), None, Arc::clone(&context))
            .await?;
        let state: Option<AgentState> = state
            .map(|json| serde_json::from_value::<AgentState>(JsonValue::Object(json)))
            .transpose()
            .map_err(|error| SessionError::Message(format!("pi.agent deserialises: {error}")))?;
        let settings = resolve_settings(self.settings.as_ref());
        let report = |message: String| (self.report)(SessionError::Message(message));
        Ok(resolve_agent(state.as_ref(), &registry, &settings, &report))
    }

    /// 对应 `buildEnv`：从当前 `cwd` 构建会话环境；无 `env` 选项时为 `None`。
    async fn build_env(
        self: &Arc<Self>,
        id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn crate::env::ExecutionEnv>>, SessionError> {
        let Some(build) = &self.env else {
            return Ok(None);
        };
        let cwd = self
            .session
            .snapshot(&*AGENT_DOC, Some(id.get()), None, Arc::clone(&context))
            .await?
            .and_then(|json| serde_json::from_value::<AgentState>(JsonValue::Object(json)).ok())
            .and_then(|state| state.cwd);
        let target = EnvTarget {
            conversation_id: id,
            cwd,
            read: Arc::clone(self) as Arc<dyn DocumentReader>,
        };
        build(target, context).await
    }

    /// 对应 `#create(target, options, context)`。
    async fn create(
        self: &Arc<Self>,
        target: CreateTarget,
        options: ConversationCreateOptions,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Conversation>, SessionError> {
        self.assert_open()?;
        let ownership = options.ownership;
        let agent = options.agent.clone();
        let init = options.init.clone();
        let id = self
            .session
            .commit_with(
                move |tx| {
                    let agent = agent.clone();
                    let init = init.clone();
                    Box::pin(async move {
                        let id = match target {
                            CreateTarget::Root => {
                                if tx.conversation(ROOT_CONVERSATION_ID).await?.is_some() {
                                    ROOT_CONVERSATION_ID
                                } else {
                                    tx.create_root_conversation().await?.id
                                }
                            }
                            CreateTarget::Independent => {
                                tx.create_conversation(ownership).await?.id
                            }
                            CreateTarget::Fork { parent, at } => {
                                tx.fork_conversation(parent, at, ownership).await?.id
                            }
                        };
                        if let Some(agent) = agent {
                            configure(tx, id, &agent).await?;
                        }
                        if let Some(init) = init {
                            init(tx, id).await?;
                        }
                        Ok(id)
                    })
                },
                context,
                None,
            )
            .await?;
        Ok(Arc::new(ConversationImpl {
            id,
            harness: Arc::clone(self),
        }))
    }
}

#[async_trait::async_trait]
impl DocumentReader for HarnessImpl {
    async fn snapshot(
        &self,
        token: &dyn crate::documents::AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        self.session.snapshot(token, owner, key, context).await
    }

    async fn snapshot_as_of(
        &self,
        token: &dyn crate::documents::AnyDocToken,
        owner: u64,
        key: Option<String>,
        at: EntryId,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        self.session
            .snapshot_as_of(token, owner, key, at, context)
            .await
    }
}

#[async_trait::async_trait]
impl DocumentObserver for HarnessImpl {
    async fn watch_doc(
        &self,
        token: &dyn crate::documents::AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<crate::session::DocumentWatch>>, SessionError> {
        self.session.watch_doc(token, owner, key, context).await
    }
}

#[async_trait::async_trait]
impl Session for HarnessImpl {
    async fn close(&self, context: Arc<dyn Context>) -> Result<(), SessionError> {
        self.closed.store(true, Ordering::Release);
        self.session.close(context).await
    }

    fn subscribe_commits(
        &self,
        listener: CommitListener,
    ) -> Result<SessionSubscription, SessionError> {
        self.session.subscribe_commits(listener)
    }

    fn subscribe_close(
        &self,
        listener: CloseListener,
    ) -> Result<SessionSubscription, SessionError> {
        self.session.subscribe_close(listener)
    }

    async fn document_state(
        &self,
        token: &dyn crate::documents::AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<crate::session::DocumentState>, SessionError> {
        self.session
            .document_state(token, owner, key, context)
            .await
    }
}

#[async_trait::async_trait]
impl Harness for HarnessImpl {
    fn resume(&self) {
        if self.closed.load(Ordering::Acquire) {
            panic!("Harness is closed");
        }
        self.tasks.resume();
    }

    async fn root(
        &self,
        context: Arc<dyn Context>,
        agent: Option<AgentChange>,
        init: Option<ConversationInit>,
    ) -> Result<Arc<dyn Conversation>, SessionError> {
        self.self_arc()
            .create(
                CreateTarget::Root,
                ConversationCreateOptions {
                    ownership: ConversationOwnership::Ownerless,
                    agent,
                    init,
                },
                context,
            )
            .await
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn Conversation>>, SessionError> {
        self.assert_open()?;
        let record = self
            .session
            .read_on_line({
                let storage = Arc::clone(&self.storage);
                let ctx = Arc::clone(&context);
                move || {
                    let storage = Arc::clone(&storage);
                    let ctx = Arc::clone(&ctx);
                    async move {
                        storage
                            .conversation(id, ctx.as_ref())
                            .await
                            .map_err(SessionError::from)
                    }
                }
            })
            .await?;
        Ok(record.map(|record| {
            Arc::new(ConversationImpl {
                id: record.id,
                harness: self.self_arc(),
            }) as Arc<dyn Conversation>
        }))
    }

    async fn create_conversation(
        &self,
        options: ConversationCreateOptions,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Conversation>, SessionError> {
        self.self_arc()
            .create(CreateTarget::Independent, options, context)
            .await
    }

    async fn get_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<Option<AnyTaskRecord>, SessionError> {
        self.session
            .read_on_line({
                let storage = Arc::clone(&self.storage);
                let ctx = Arc::clone(&context);
                move || {
                    let storage = Arc::clone(&storage);
                    let ctx = Arc::clone(&ctx);
                    async move {
                        storage
                            .task(id, ctx.as_ref())
                            .await
                            .map_err(SessionError::from)
                    }
                }
            })
            .await
    }

    async fn inspect(&self, context: Arc<dyn Context>) -> Result<HarnessInspection, SessionError> {
        let (scheduling, tasks) = self.tasks.inspect(self.registry.snapshot()).await?;
        let scan = |status: SubmissionStatus| {
            let storage = Arc::clone(&self.storage);
            let ctx = Arc::clone(&context);
            async move {
                scan_all(move |cursor| {
                    let storage = Arc::clone(&storage);
                    let ctx = Arc::clone(&ctx);
                    async move {
                        storage
                            .scan_submissions(
                                SubmissionQuery {
                                    conversation_id: None,
                                    status: Some(status),
                                    order: None,
                                },
                                SCAN_PAGE_SIZE,
                                cursor,
                                ctx.as_ref(),
                            )
                            .await
                    }
                })
                .await
                .map_err(SessionError::from)
            }
        };
        let mut submissions = scan(SubmissionStatus::Queued).await?;
        submissions.extend(scan(SubmissionStatus::Placed).await?);
        submissions.sort_by_key(|record| record.identity().id);
        Ok(HarnessInspection {
            scheduling,
            tasks,
            submissions,
        })
    }

    async fn submission(
        &self,
        id: SubmissionId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn Submission>>, SessionError> {
        self.submissions.get(id, context).await
    }

    async fn abort_submission(
        &self,
        id: SubmissionId,
        context: Arc<dyn Context>,
        conversation_id: Option<ConversationId>,
    ) -> Result<HarnessAbortSubmission, SessionError> {
        let result = self.submissions.abort(id, context, conversation_id).await?;
        Ok(match result {
            None => HarnessAbortSubmission::NotFound,
            Some(SubmissionAbort::Aborted) => HarnessAbortSubmission::Aborted,
            Some(SubmissionAbort::AlreadyPlaced) => HarnessAbortSubmission::AlreadyPlaced,
            Some(SubmissionAbort::Settled) => HarnessAbortSubmission::Settled,
        })
    }

    async fn abort_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<TaskAbort, SessionError> {
        let result = self.tasks.abort(id, context).await?;
        Ok(match result {
            AbortResult::Marked => TaskAbort::Marked,
            AbortResult::Terminal => TaskAbort::Terminal,
        })
    }

    async fn wait_for_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<crate::harness::types::SettledTask, SessionError> {
        self.tasks.resume();
        self.tasks.wait_for_task(id, context).await
    }

    async fn wait_for_idle(&self, context: Arc<dyn Context>) -> Result<(), SessionError> {
        self.tasks.resume();
        self.tasks.wait_for_idle(None, context).await
    }

    async fn usage(&self, context: Arc<dyn Context>) -> Result<UsageState, SessionError> {
        let conversations = self
            .session
            .read_on_line({
                let storage = Arc::clone(&self.storage);
                let ctx = Arc::clone(&context);
                move || {
                    let storage = Arc::clone(&storage);
                    let ctx = Arc::clone(&ctx);
                    async move {
                        scan_all(move |cursor| {
                            let storage = Arc::clone(&storage);
                            let ctx = Arc::clone(&ctx);
                            async move {
                                storage
                                    .scan_conversations(
                                        ConversationQuery::default(),
                                        SCAN_PAGE_SIZE,
                                        cursor,
                                        ctx.as_ref(),
                                    )
                                    .await
                            }
                        })
                        .await
                        .map_err(SessionError::from)
                    }
                }
            })
            .await?;
        let mut total = UsageState::default();
        for record in conversations {
            if let Some(json) = self
                .session
                .snapshot(
                    &*USAGE_DOC,
                    Some(record.id.get()),
                    None,
                    Arc::clone(&context),
                )
                .await?
            {
                let state: UsageState =
                    serde_json::from_value(JsonValue::Object(json)).map_err(|error| {
                        SessionError::Message(format!("pi.usage deserialises: {error}"))
                    })?;
                add_usage_state(&mut total, &state);
            }
        }
        Ok(total)
    }

    async fn task_graph(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<AttachedReplicatedState, SessionError> {
        self.task_graph.state(context).await
    }

    async fn watch_task_graph(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn WatchHandle>, SessionError> {
        Ok(self.task_graph.watch(context).await? as Arc<dyn WatchHandle>)
    }
}

/// 对应 `ConversationImpl`：绑定到 Harness 的一个会话句柄。
struct ConversationImpl {
    id: ConversationId,
    harness: Arc<HarnessImpl>,
}

#[async_trait::async_trait]
impl Conversation for ConversationImpl {
    fn id(&self) -> ConversationId {
        self.id
    }

    async fn agent(&self, context: Arc<dyn Context>) -> Result<Agent, SessionError> {
        self.harness.resolve_agent(self.id, None, context).await
    }

    async fn configure(
        &self,
        change: AgentChange,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let id = self.id;
        self.harness
            .session
            .commit_with(
                move |tx| Box::pin(async move { configure(tx, id, &change).await }),
                context,
                None,
            )
            .await
    }

    async fn submit(
        &self,
        submission: SubmissionDraft,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Submission>, SessionError> {
        self.harness
            .submissions
            .submit(self.id, submission, context)
            .await
    }

    async fn reset(
        &self,
        handoff: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let model = handoff.map(|content| {
            vec![Message::User(UserMessage {
                content: UserInput::Text(content),
                timestamp: (self.harness.now)(),
            })]
        });
        let entry = EntryDraft {
            kind: RESET_ENTRY.kind().to_string(),
            model,
            data: None,
            head: Some(EntryHead::SelfEntry),
            edits: None,
        };
        self.harness
            .submissions
            .submit(
                self.id,
                SubmissionDraft::Write(crate::harness::types::WriteSubmissionDraft {
                    request_id: None,
                    entry,
                }),
                context,
            )
            .await?;
        Ok(())
    }

    async fn compact(
        &self,
        instructions: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<TaskId<crate::harness::types::CompactionResult>, SessionError> {
        self.harness.tasks.resume();
        let input = CompactionInput {
            reason: crate::harness::types::CompactionReason::Manual,
            instructions,
        };
        let create = Arc::clone(&self.harness.create_compaction);
        let id = self.id;
        let task_id = self
            .harness
            .session
            .commit_with(
                move |tx| {
                    let create = Arc::clone(&create);
                    Box::pin(async move { create(tx, id, input, None).await })
                },
                context,
                None,
            )
            .await?;
        Ok(TaskId::new(task_id.get()))
    }

    async fn commit(
        &self,
        change: CommitOperation<'static>,
        context: Arc<dyn Context>,
    ) -> Result<JsonValue, SessionError> {
        let id = self.id;
        self.harness
            .session
            .commit_with(
                move |tx| Box::pin(async move { change(tx).await }),
                context,
                Some(TransactionScope {
                    conversation_id: Some(id),
                    task_id: None,
                }),
            )
            .await
    }

    async fn context(
        &self,
        context: Arc<dyn Context>,
        at: Option<EntryId>,
    ) -> Result<crate::harness::types::ContextView, SessionError> {
        read_context(
            &self.harness.session,
            self.harness.storage.as_ref(),
            self.id,
            context,
            at,
        )
        .await
    }

    async fn entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: Arc<dyn Context>,
    ) -> Result<Page<EntryRecord, Cursor>, SessionError> {
        let bounded = EntryQuery {
            conversation_id: self.id,
            min_entry_id: query.min_entry_id,
            max_entry_id: query.max_entry_id,
            order: query.order,
        };
        let storage = Arc::clone(&self.harness.storage);
        self.harness
            .session
            .read_on_line(move || {
                let storage = Arc::clone(&storage);
                let context = Arc::clone(&context);
                async move {
                    storage
                        .scan_entries(bounded, limit, cursor, context.as_ref())
                        .await
                        .map_err(SessionError::from)
                }
            })
            .await
    }

    async fn fork(
        &self,
        at: EntryId,
        options: ConversationCreateOptions,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Conversation>, SessionError> {
        self.harness
            .create(
                CreateTarget::Fork {
                    parent: self.id,
                    at,
                },
                options,
                context,
            )
            .await
    }

    async fn abort(
        &self,
        context: Arc<dyn Context>,
        options: ConversationAbortOptions,
    ) -> Result<(), SessionError> {
        self.harness.tasks.resume();
        self.harness
            .tasks
            .abort_conversation(self.id, options.background == Some(true), context)
            .await
    }

    async fn wait_for_idle(&self, context: Arc<dyn Context>) -> Result<(), SessionError> {
        self.harness.tasks.resume();
        self.harness
            .tasks
            .wait_for_idle(Some(self.id), context)
            .await
    }

    async fn view_state(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<AttachedReplicatedState, SessionError> {
        self.harness.views.state(self.id, context).await
    }

    async fn watch(&self, context: Arc<dyn Context>) -> Result<ConversationWatch, SessionError> {
        Ok(self.harness.views.watch(self.id, context).await? as Arc<dyn WatchHandle>)
    }
}

/// 对应 `BoundConversation`：调用期绑定的会话句柄实现。
struct BoundConversation {
    id: ConversationId,
    binding: InvocationBinding,
    submissions: Arc<Submissions>,
    tasks: TaskScheduler,
}

impl BoundConversation {
    fn bind(&self, context: Arc<dyn Context>) -> Arc<dyn Context> {
        with_abort_signal(self.binding.signal.clone(), context)
    }
}

/// 对应 `BoundSubmission`：调用期绑定的提交句柄。
struct BoundSubmission {
    inner: Arc<dyn Submission>,
    binding: InvocationBinding,
}

impl BoundSubmission {
    fn bind(&self, context: Arc<dyn Context>) -> Arc<dyn Context> {
        with_abort_signal(self.binding.signal.clone(), context)
    }
}

#[async_trait::async_trait]
impl Submission for BoundSubmission {
    fn id(&self) -> SubmissionId {
        self.inner.id()
    }

    async fn status(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<crate::types::SubmissionRecord, SessionError> {
        (self.binding.check)()?;
        self.inner.status(self.bind(context)).await
    }

    async fn wait(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<crate::harness::types::SettledSubmissionRecord, SessionError> {
        (self.binding.check)()?;
        self.inner.wait(self.bind(context)).await
    }

    async fn abort(&self, context: Arc<dyn Context>) -> Result<SubmissionAbort, SessionError> {
        (self.binding.check)()?;
        self.inner.abort(self.bind(context)).await
    }
}

#[async_trait::async_trait]
impl ConversationHandle for BoundConversation {
    fn id(&self) -> ConversationId {
        self.id
    }

    async fn submit(
        &self,
        submission: crate::harness::types::InputSubmissionDraft,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Submission>, SessionError> {
        (self.binding.check)()?;
        let submission = self
            .submissions
            .submit(
                self.id,
                SubmissionDraft::Input(submission),
                self.bind(context),
            )
            .await?;
        Ok(Arc::new(BoundSubmission {
            inner: submission,
            binding: self.binding.clone(),
        }))
    }

    async fn abort(
        &self,
        context: Arc<dyn Context>,
        options: Option<ConversationAbortOptions>,
    ) -> Result<(), SessionError> {
        (self.binding.check)()?;
        self.tasks
            .abort_conversation(
                self.id,
                options
                    .map(|options| options.background == Some(true))
                    .unwrap_or(false),
                self.bind(context),
            )
            .await
    }

    async fn wait_for_idle(&self, context: Arc<dyn Context>) -> Result<(), SessionError> {
        (self.binding.check)()?;
        self.tasks
            .wait_for_idle(Some(self.id), self.bind(context))
            .await
    }
}

/// 对应 `boundConversation`：任务与工具的调用期绑定句柄。
fn bound_conversation(
    id: ConversationId,
    binding: InvocationBinding,
    submissions: Arc<Submissions>,
    tasks: TaskScheduler,
) -> Arc<dyn ConversationHandle> {
    Arc::new(BoundConversation {
        id,
        binding,
        submissions,
        tasks,
    })
}

/// 对应 `Harness.open`：在存储上打开一个 Harness。注册表在 Harness 运行期间可继续变化。
pub async fn open(
    storage: Arc<dyn Storage>,
    options: HarnessOptions,
    context: Arc<dyn Context>,
) -> Result<Arc<HarnessImpl>, SessionError> {
    if let Some(signal) = context.abort_signal() {
        signal.throw_if_aborted().map_err(SessionError::Aborted)?;
    }
    let snapshot = options.registry.snapshot();
    let missing: Vec<&str> = BUILTIN_TASK_NAMES
        .iter()
        .copied()
        .filter(|name| snapshot.task(name).is_none())
        .collect();
    if !missing.is_empty() {
        let names = missing.join(", ");
        return Err(SessionError::Message(format!(
            "Registry lacks built-in tasks {names}; create it with createRegistry()"
        )));
    }
    let harness = HarnessImpl::new(storage, options, Arc::clone(&context));
    match harness.open_tasks(Arc::clone(&context)).await {
        Ok(()) => Ok(harness),
        Err(error) => {
            // 调用方的 context 可能是打开失败的原因：不带它关闭，再重抛打开错误。
            if let Err(close_error) = harness.close(without_abort_signal(context)).await {
                (harness.report)(close_error);
            }
            Err(error)
        }
    }
}
