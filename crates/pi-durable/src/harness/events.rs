//! 对应 `harness/events.ts`：一个会话的 agent 事件流（spec §9.4，实验性）。
//!
//! # 与上游的差异
//!
//! - **`watchEvents()` 推迟到 P5h**：它需要 `conversationViews(harness)`，即 Harness 对象作键；
//!   本文件先落地类型与**纯翻译逻辑**（`translate` / `message_changes` / `tool_update`），
//!   它们不依赖 Harness。
//! - **`SnapshotEvent` 不带 `type` 字段**：上游的结构体自带 `type: "snapshot"`；Rust 侧由
//!   [`AgentEvent`] 的内部标签提供同一个 JSON 形状。
//! - `op[1] as Path` 之类的元组索引 → [`Op`] 的模式匹配。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};

use pi_ai::{AssistantMessage, ContentBlock, Message, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::chord::context::Context;
use crate::chord::delta::{Op, Path, PathSegment};
use crate::harness::inbox::InboxItem;
use crate::harness::live::{
    CompactionStatus, DeferredPoll, LiveGeneration, LiveRun, RetryBackoff, ToolSlot, ToolSlotStatus,
};
use crate::harness::types::{AgentState, CompactionReason, ToolDiagnostic};
use crate::harness::usage::UsageState;
use crate::harness::util::scan_all;
use crate::harness::view::{ConversationView, ConversationViews, ReleaseHandle, ViewObserver};
use crate::session::SessionError;
use crate::session::observation::CommittedWatch;
use crate::types::{
    CommitChange, CommitPublication, ConversationId, EntryRecord, JsonObject, Storage,
    SubmissionId, SubmissionRecord, TableCommitChange, TaskId, TaskQuery, TaskRecord, TaskStatus,
    WatchEnd, WatchHandle,
};

/// 对应 `TaskChange`：一次任务变更的完整记录。
type TaskChange = TaskRecord<JsonValue, JsonValue, JsonValue>;

/// 对应 `QueuedItem`：队列里的一项（只留 ID 与模式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedItem {
    /// 提交 ID。
    pub id: SubmissionId,
    /// 排队模式。
    pub mode: QueuedMode,
}

/// 对应 `InboxItem["mode"]`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueuedMode {
    /// `steer`。
    #[serde(rename = "steer")]
    Steer,
    /// `followUp`。
    #[serde(rename = "followUp")]
    FollowUp,
    /// `write`。
    #[serde(rename = "write")]
    Write,
}

/// 对应 `MessageChange`：对在途 assistant 消息的一次改动（相对该消息）。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum MessageChange {
    /// `text_start`。
    TextStart {
        /// 内容块下标。
        content_index: usize,
        /// 起始块。
        block: ContentBlock,
    },
    /// `thinking_start`。
    ThinkingStart {
        /// 内容块下标。
        content_index: usize,
        /// 起始块。
        block: ContentBlock,
    },
    /// `toolcall_start`。
    ToolcallStart {
        /// 内容块下标。
        content_index: usize,
        /// 起始块。
        block: ContentBlock,
    },
    /// `text_delta`。
    TextDelta {
        /// 内容块下标。
        content_index: usize,
        /// 增量文本。
        delta: String,
    },
    /// `thinking_delta`。
    ThinkingDelta {
        /// 内容块下标。
        content_index: usize,
        /// 增量文本。
        delta: String,
    },
    /// `toolcall_delta`。
    ToolcallDelta {
        /// 内容块下标。
        content_index: usize,
        /// 参数内的路径。
        path: Vec<PathSegment>,
        /// 增量文本。
        delta: String,
    },
    /// `block`：整块替换。
    Block {
        /// 内容块下标。
        content_index: usize,
        /// 块内容。
        block: ContentBlock,
    },
    /// `message`：整条消息替换。
    Message {
        /// 消息。
        message: AssistantMessage,
    },
}

/// 对应 `snapshot` 事件里 `run` 的形状。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRun {
    /// 该运行结算的输入。
    pub inputs: Vec<SubmissionId>,
}

/// 对应 `SnapshotEvent.generation`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotGeneration {
    /// 尝试次数。
    pub attempt: u32,
    /// 在途局部消息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<AssistantMessage>,
    /// 重试退避。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryBackoff>,
    /// provider 端延迟轮询。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredPoll>,
}

/// 对应 `SnapshotEvent`：接入时的完整状态快照。
///
/// 上游的结构体自带 `type: "snapshot"`；Rust 由 [`AgentEvent::Snapshot`] 的内部标签提供。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEvent {
    /// 活动条目。
    pub entries: Vec<EntryRecord>,
    /// 当前运行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<SnapshotRun>,
    /// 当前生成尝试。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<SnapshotGeneration>,
    /// 当前工具轮。
    pub tools: Vec<ToolSlot>,
    /// 活动压缩。
    pub compactions: Vec<CompactionStatus>,
    /// 队列。
    pub inbox: Vec<QueuedItem>,
    /// `pi.agent`；缺席时为 `{}`。
    pub agent: AgentState,
    /// `pi.usage`。
    pub usage: UsageState,
}

/// 对应 `tool_execution_update` 的 `output`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, rename_all_fields = "camelCase")]
pub enum ToolOutputUpdate {
    /// 前端裁剪后再追加，或仅仅是其中一种。
    TrimAndAppend {
        /// 裁掉的前缀长度。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trim_start: Option<usize>,
        /// 追加的文本。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        append: Option<String>,
    },
    /// 整体替换。
    Set {
        /// 替换后的文本。
        set: String,
    },
}

/// 对应 `AgentEvent`：一次提交引发的全部事件（spec §9.4 顺序）。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    /// 快照（用于溢出替换）。
    Snapshot(SnapshotEvent),
    /// 运行开始。
    RunStart {
        /// 输入。
        inputs: Vec<SubmissionId>,
    },
    /// 运行结束。
    RunEnd {
        /// 输入。
        inputs: Vec<SubmissionId>,
    },
    /// 回合开始。
    TurnStart,
    /// 回合结束。
    TurnEnd,
    /// 消息开始。
    MessageStart {
        /// 消息。
        message: Message,
    },
    /// 消息更新。
    MessageUpdate {
        /// 当前花费。
        usage: Usage,
        /// 相对上一次的改动。
        changes: Vec<MessageChange>,
    },
    /// 消息结束。
    MessageEnd {
        /// 条目。
        entry: EntryRecord,
    },
    /// 工具执行开始。
    ToolExecutionStart {
        /// 调用 ID。
        tool_call_id: String,
        /// 工具名。
        tool_name: String,
        /// 参数。
        args: JsonObject,
    },
    /// 工具执行更新。
    ToolExecutionUpdate {
        /// 调用 ID。
        tool_call_id: String,
        /// 工具名。
        tool_name: String,
        /// 输出改动。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<ToolOutputUpdate>,
        /// 细节。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        /// 诊断。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostics: Option<Vec<ToolDiagnostic>>,
    },
    /// 工具执行结束（`entry` 在 faulted / orphaned 时缺席）。
    ToolExecutionEnd {
        /// 调用 ID。
        tool_call_id: String,
        /// 工具名。
        tool_name: String,
        /// 结果条目。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<EntryRecord>,
    },
    /// 队列更新。
    InboxUpdate {
        /// 队列项。
        items: Vec<QueuedItem>,
    },
    /// 提交记录。
    Submission {
        /// 记录。
        record: SubmissionRecord,
    },
    /// 自动重试开始。
    AutoRetryStart {
        /// 尝试次数。
        attempt: u32,
        /// 重试时刻。
        at: u64,
        /// 上次错误。
        error_message: String,
    },
    /// 自动重试结束。
    AutoRetryEnd {
        /// 尝试次数。
        attempt: u32,
    },
    /// provider 端延迟轮询。
    DeferredPoll {
        /// 轮询时刻。
        poll_at: u64,
    },
    /// 条目追加。
    EntryAppended {
        /// 条目。
        entry: EntryRecord,
    },
    /// agent 变更。
    AgentChanged {
        /// agent 状态。
        agent: AgentState,
    },
    /// 花费变更。
    UsageChanged {
        /// 花费账本。
        usage: UsageState,
    },
    /// 任务失败。
    TaskFailed {
        /// 任务 ID。
        task_id: TaskId,
        /// 定义名。
        kind: String,
        /// 错误消息。
        message: String,
    },
    /// 压缩开始。
    CompactionStart {
        /// 任务 ID。
        task_id: TaskId,
        /// 原因。
        reason: CompactionReason,
        /// 是否阻塞运行。
        blocking: bool,
    },
    /// 压缩结束。
    CompactionEnd {
        /// 任务 ID。
        task_id: TaskId,
        /// 原因。
        reason: CompactionReason,
    },
}

/// 对应 `AgentEventStream`：一个会话的事件批次流，每次提交一批。
///
/// 实现见 P5h（需要 `watchEvents` 与 Harness）。
pub trait AgentEventStream: Send + Sync {
    /// 接入时的 `snapshot` 事件。
    fn snapshot(&self) -> SnapshotEvent;

    /// 对应 `start(listener)`。
    fn start(&self, listener: EventBatchListener);

    /// 对应 `stop()`。
    fn stop(&self) -> crate::types::ClosedWatch;

    /// 对应 `closed`。
    fn closed(&self) -> crate::types::ClosedWatch;
}

/// 对应 `start(listener)` 的监听者签名：`(events, context) => Promise<void>`。
pub type EventBatchListener = std::sync::Arc<
    dyn Fn(
            Vec<AgentEvent>,
            std::sync::Arc<dyn crate::chord::context::Context>,
        ) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// 对应 `watchEvents` 的内部：既是挂载观察者，又是 [`AgentEventStream`] 的实现。
struct AgentEventsObserver {
    conversation_id: ConversationId,
    held: Mutex<HashSet<TaskId>>,
    snapshot: SnapshotEvent,
    watch: Arc<CommittedWatch>,
    /// 当前挂载值（反序列化前），供 overflow 替换与 `snapshot` 重算。
    current: Arc<Mutex<JsonValue>>,
}

impl ViewObserver for AgentEventsObserver {
    fn publication(
        &self,
        before: &JsonValue,
        after: &JsonValue,
        ops: &[Op],
        publication: &CommitPublication,
        context: Arc<dyn Context>,
    ) {
        let Ok(before_view) = serde_json::from_value::<ConversationView>(before.clone()) else {
            return;
        };
        let Ok(after_view) = serde_json::from_value::<ConversationView>(after.clone()) else {
            return;
        };
        *self.current.lock().expect("current") = after.clone();
        let mut held = self.held.lock().expect("held");
        let events = translate(
            self.conversation_id,
            &before_view,
            &after_view,
            ops,
            publication,
            &mut held,
        );
        if events.is_empty() {
            return;
        }
        let value = serde_json::to_value(&events).expect("events serialise");
        self.watch.advance(value, Vec::new(), context);
    }

    fn close_session(&self) {
        self.watch.close_session();
    }
}

impl AgentEventStream for AgentEventsObserver {
    fn snapshot(&self) -> SnapshotEvent {
        self.snapshot.clone()
    }

    fn start(&self, listener: EventBatchListener) {
        self.watch.start(std::sync::Arc::new(
            move |value: JsonValue, _ops: Vec<Op>, context: std::sync::Arc<dyn Context>| {
                let events: Vec<AgentEvent> = serde_json::from_value(value).unwrap_or_default();
                Box::pin(listener(events, context))
            },
        ));
    }

    fn stop(&self) -> crate::types::ClosedWatch {
        self.watch.stop()
    }

    fn closed(&self) -> crate::types::ClosedWatch {
        self.watch.closed()
    }
}

/// 对应 `watchEvents`：接入一个会话的 agent 事件（spec §9.4，实验性）。
///
/// snapshot 与后续提交的注册在 Session 线上原子地获取；溢出以单个 snapshot 替换未投递批次。
pub async fn watch_events(
    views: &std::sync::Arc<ConversationViews>,
    conversation_id: ConversationId,
    context: std::sync::Arc<dyn Context>,
) -> Result<std::sync::Arc<dyn AgentEventStream>, SessionError> {
    let (observer, _detach) = views
        .attach(
            conversation_id,
            {
                let context = std::sync::Arc::clone(&context);
                move |initial: JsonValue,
                      release: ReleaseHandle,
                      storage: std::sync::Arc<dyn Storage>| {
                    Box::pin(async move {
                        // held outcome 已结束其回合的 generation，在线上与 snapshot 一起读取。
                        let query = TaskQuery {
                            conversation_id: Some(conversation_id),
                            kind: Some("pi.generation".to_string()),
                            status: Some(TaskStatus::Completing),
                            abort_requested: None,
                            background: None,
                            order: None,
                        };
                        let completing = scan_all({
                            let storage = std::sync::Arc::clone(&storage);
                            let context = std::sync::Arc::clone(&context);
                            move |cursor| {
                                let storage = std::sync::Arc::clone(&storage);
                                let context = std::sync::Arc::clone(&context);
                                let query = query.clone();
                                async move {
                                    storage
                                        .scan_tasks(query, 100, cursor, context.as_ref())
                                        .await
                                }
                            }
                        })
                        .await?;
                        let held: HashSet<TaskId> = completing
                            .into_iter()
                            .map(|record| TaskId::new(record.id.get()))
                            .collect();
                        let initial_view: ConversationView =
                            serde_json::from_value(initial.clone()).map_err(|error| {
                                SessionError::Message(format!(
                                    "conversation view deserialises: {error}"
                                ))
                            })?;
                        let snapshot = snapshot_of(&initial_view);
                        let current = std::sync::Arc::new(Mutex::new(initial));
                        let watch = CommittedWatch::new(
                            JsonValue::Array(Vec::new()),
                            release.into_release(),
                            Some({
                                let current = std::sync::Arc::clone(&current);
                                std::sync::Arc::new(move || {
                                    let view: ConversationView = serde_json::from_value(
                                        current.lock().expect("current").clone(),
                                    )
                                    .expect("conversation view deserialises");
                                    serde_json::to_value([AgentEvent::Snapshot(snapshot_of(&view))])
                                        .expect("snapshot serialises")
                                })
                            }),
                        );
                        Ok(AgentEventsObserver {
                            conversation_id,
                            held: Mutex::new(held),
                            snapshot,
                            watch: std::sync::Arc::new(watch),
                            current,
                        })
                    })
                }
            },
            std::sync::Arc::clone(&context),
        )
        .await?;

    // 与普通观察一样，接入 context 决定流的一生。
    let signal = context.abort_signal();
    if signal.as_ref().is_some_and(|signal| signal.aborted()) {
        observer.watch.cancel();
        return Err(SessionError::Aborted(pi_ai::AbortError));
    }
    if let Some(signal) = signal {
        observer.watch.observe_cancellation(signal);
    }
    Ok(observer)
}

/// 对应 `Parts`：事件读取的视图的具类型部分。
#[derive(Debug, Clone, Default)]
pub struct Parts {
    /// `pi.live`（缺席时为空状态）。
    pub live: crate::harness::live::LiveState,
    /// `pi.inbox`。
    pub inbox: Option<crate::harness::inbox::InboxState>,
    /// `pi.agent`。
    pub agent: Option<AgentState>,
    /// `pi.usage`。
    pub usage: Option<UsageState>,
}

/// 对应 `parts(view)`。
pub fn parts(view: &ConversationView) -> Parts {
    let parse = |kind: &str| view.docs.get(kind).cloned().map(JsonValue::Object);
    Parts {
        live: parse("pi.live")
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default(),
        inbox: parse("pi.inbox").and_then(|value| serde_json::from_value(value).ok()),
        agent: parse("pi.agent").and_then(|value| serde_json::from_value(value).ok()),
        usage: parse("pi.usage").and_then(|value| serde_json::from_value(value).ok()),
    }
}

/// 对应 `snapshotOf(view)`。
pub fn snapshot_of(view: &ConversationView) -> SnapshotEvent {
    let Parts {
        live,
        inbox,
        agent,
        usage,
    } = parts(view);
    SnapshotEvent {
        entries: view.entries.clone(),
        run: live.run.as_ref().map(|run| SnapshotRun {
            inputs: run.inputs.clone(),
        }),
        generation: live
            .generation
            .as_ref()
            .map(|generation| SnapshotGeneration {
                attempt: generation.attempt,
                message: generation.message.clone(),
                retry: generation.retry.clone(),
                deferred: generation.deferred,
            }),
        tools: live.tools.clone().unwrap_or_default(),
        compactions: live.compactions.clone().unwrap_or_default(),
        inbox: queued(inbox.as_ref()),
        agent: agent.unwrap_or_else(default_agent_state),
        usage: usage.unwrap_or_default(),
    }
}

/// 对应 `AgentDoc.definition.initial()`：`pi.agent` 的初始值。
fn default_agent_state() -> AgentState {
    AgentState::default()
}

/// 对应 `queued(inbox)`。
pub fn queued(inbox: Option<&crate::harness::inbox::InboxState>) -> Vec<QueuedItem> {
    inbox
        .map(|inbox| {
            inbox
                .items
                .iter()
                .filter_map(InboxItem::from_json)
                .map(|item| QueuedItem {
                    id: item.id(),
                    mode: match &item {
                        InboxItem::Input { mode, .. } => match mode {
                            crate::harness::inbox::InboxInputMode::Steer => QueuedMode::Steer,
                            crate::harness::inbox::InboxInputMode::FollowUp => QueuedMode::FollowUp,
                        },
                        InboxItem::Write { .. } => QueuedMode::Write,
                    },
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 对应 `resultOf(entries, callId)`：`entries` 里 `callId` 的工具结果。
pub fn result_of<'a>(entries: &'a [EntryRecord], call_id: &str) -> Option<&'a EntryRecord> {
    entries.iter().find(|entry| {
        matches!(
            entry.model.as_ref().and_then(|model| model.first()),
            Some(Message::ToolResult(result)) if result.tool_call_id == call_id
        )
    })
}

/// 对应 `translate(...)`：一次发布引发的全部事件，按 spec §9.4 的顺序。
pub fn translate(
    conversation_id: ConversationId,
    before: &ConversationView,
    after: &ConversationView,
    view_ops: &[Op],
    publication: &CommitPublication,
    held: &mut HashSet<TaskId>,
) -> Vec<AgentEvent> {
    let mut entries: Vec<EntryRecord> = Vec::new();
    let mut tasks: BTreeMap<TaskId, TaskChange> = BTreeMap::new();
    let mut submissions: Vec<SubmissionRecord> = Vec::new();
    for change in &publication.changes {
        match change {
            CommitChange::Table(TableCommitChange::Entry(entry)) => {
                if entry.conversation_id == conversation_id {
                    entries.push(entry.clone());
                }
            }
            CommitChange::Table(TableCommitChange::Task(task)) => {
                if task.conversation_id == conversation_id {
                    tasks.insert(TaskId::new(task.id.get()), task.clone());
                }
            }
            CommitChange::Table(TableCommitChange::Submission(record))
                if record.identity().conversation_id == conversation_id =>
            {
                submissions.push(record.clone());
            }
            _ => {}
        }
    }
    if view_ops.is_empty() && entries.is_empty() && tasks.is_empty() && submissions.is_empty() {
        return Vec::new();
    }
    // 条目按 ID 顺序追加；提交记录按提交首次触及它们的顺序发布。
    submissions.sort_by_key(|record| record.identity().id);

    let was = parts(before);
    let now = parts(after);
    let mut events: Vec<AgentEvent> = Vec::new();

    // 进度：工具开始、在途消息、工具更新、重试与延迟状态。
    let slots_before: BTreeMap<String, ToolSlot> = was
        .live
        .tools
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|slot| (slot.call_id.clone(), slot))
        .collect();
    let slots: Vec<ToolSlot> = now.live.tools.clone().unwrap_or_default();
    for slot in &slots {
        if slot.status != ToolSlotStatus::Running
            || slots_before
                .get(&slot.call_id)
                .map(|previous| previous.status)
                == Some(ToolSlotStatus::Running)
        {
            continue;
        }
        let checkpoint = slot
            .task_id
            .and_then(|task_id| tasks.get(&task_id))
            .map(|task| task_checkpoint(task.state.clone()));
        let args = checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.get("arguments"))
            .and_then(JsonValue::as_object)
            .cloned()
            .unwrap_or_default();
        events.push(AgentEvent::ToolExecutionStart {
            tool_call_id: slot.call_id.clone(),
            tool_name: slot.name.clone(),
            args,
        });
    }
    let partial_before = was
        .live
        .generation
        .as_ref()
        .and_then(|generation| generation.message.clone());
    let partial = now
        .live
        .generation
        .as_ref()
        .and_then(|generation| generation.message.clone());
    match (&partial, &partial_before) {
        (Some(partial), None) => events.push(AgentEvent::MessageStart {
            message: Message::Assistant(partial.clone()),
        }),
        (Some(partial), Some(previous)) if partial != previous => {
            events.push(AgentEvent::MessageUpdate {
                usage: partial.usage.clone(),
                changes: message_changes(view_ops, partial),
            });
        }
        _ => {}
    }
    for (index, slot) in slots.iter().enumerate() {
        let previous = slots_before.get(&slot.call_id);
        if slot.status != ToolSlotStatus::Running
            || previous.map(|previous| previous.status) != Some(ToolSlotStatus::Running)
        {
            continue;
        }
        let Some(previous) = previous else {
            continue;
        };
        let Some(update) = tool_update(view_ops, index, slot, previous) else {
            continue;
        };
        events.push(AgentEvent::ToolExecutionUpdate {
            tool_call_id: slot.call_id.clone(),
            tool_name: slot.name.clone(),
            output: update.output,
            details: update.details,
            diagnostics: update.diagnostics,
        });
    }
    let generation = now.live.generation.clone();
    let generation_before = was.live.generation.clone();
    if let Some(generation) = &generation
        && let Some(retry) = &generation.retry
        && generation_before
            .as_ref()
            .and_then(|before| before.retry.as_ref())
            .is_none()
    {
        events.push(AgentEvent::AutoRetryStart {
            attempt: generation.attempt,
            at: retry.at,
            error_message: retry.error.clone(),
        });
    }
    if let Some(before) = &generation_before
        && before.retry.is_some()
        && generation
            .as_ref()
            .and_then(|generation| generation.retry.as_ref())
            .is_none()
    {
        events.push(AgentEvent::AutoRetryEnd {
            attempt: before.attempt,
        });
    }
    let deferred = generation
        .as_ref()
        .and_then(|generation| generation.deferred);
    let deferred_before = generation_before
        .as_ref()
        .and_then(|generation| generation.deferred);
    if deferred.is_some() && deferred != deferred_before {
        events.push(AgentEvent::DeferredPoll {
            poll_at: deferred
                .map(|deferred| deferred.poll_at)
                .unwrap_or_default(),
        });
    }

    // 本次提交里结束的工具：变成 done 的槽位、创建即 done 的槽位（未被提供的调用），
    // 或其运行结束时消失且仍未完成的槽位。消失的 done 槽位更早结束。
    let mut tool_ends: Vec<AgentEvent> = Vec::new();
    let mut end_tool = |call_id: &str, name: &str, entry_id: Option<EntryIdLike>| {
        let entry = entry_id.and_then(|entry_id| {
            entries
                .iter()
                .find(|candidate| candidate.id == entry_id)
                .cloned()
        });
        tool_ends.push(AgentEvent::ToolExecutionEnd {
            tool_call_id: call_id.to_string(),
            tool_name: name.to_string(),
            entry,
        });
    };
    for previous in slots_before.values() {
        if previous.status == ToolSlotStatus::Done {
            continue;
        }
        match slots.iter().find(|slot| slot.call_id == previous.call_id) {
            Some(slot) if slot.status == ToolSlotStatus::Done => {
                end_tool(&previous.call_id, &previous.name, slot.entry);
            }
            None => {
                // 运行在本提交结束的槽位可能随它一起追加了结果，未开始的调用即如此。
                let entry = result_of(&entries, &previous.call_id).map(|entry| entry.id);
                end_tool(&previous.call_id, &previous.name, entry);
            }
            _ => {}
        }
    }
    for slot in &slots {
        if slot.status == ToolSlotStatus::Done && !slots_before.contains_key(&slot.call_id) {
            end_tool(&slot.call_id, &slot.name, slot.entry);
        }
    }

    // 条目按追加顺序；工具的结束紧接其结果消息之前，与 coding agent 一致。
    let mut assistant_appended = false;
    for entry in &entries {
        for end in tool_ends.iter().filter(|end| match end {
            AgentEvent::ToolExecutionEnd {
                entry: Some(candidate),
                ..
            } => candidate.id == entry.id,
            _ => false,
        }) {
            events.push(end.clone());
        }
        let Some(message) = entry.model.as_ref().and_then(|model| model.first()) else {
            events.push(AgentEvent::EntryAppended {
                entry: entry.clone(),
            });
            continue;
        };
        // 已流式开始的回答由它的第一个局部消息开启。
        let streamed = matches!(message, Message::Assistant(_))
            && partial_before.is_some()
            && !assistant_appended;
        if matches!(message, Message::Assistant(_)) {
            assistant_appended = true;
        }
        if !streamed {
            events.push(AgentEvent::MessageStart {
                message: message.clone(),
            });
        }
        events.push(AgentEvent::MessageEnd {
            entry: entry.clone(),
        });
    }
    // 没有结果条目的结束：faulted / orphaned 的工具，或其运行结束的工具。
    for end in tool_ends.iter().filter(|end| match end {
        AgentEvent::ToolExecutionEnd { entry, .. } => entry.is_none(),
        _ => false,
    }) {
        events.push(end.clone());
    }

    // 压缩结束、任务失败，然后回合与运行结束。
    let compactions_before = was.live.compactions.clone().unwrap_or_default();
    let compactions = now.live.compactions.clone().unwrap_or_default();
    for status in &compactions_before {
        if !compactions
            .iter()
            .any(|candidate| candidate.task_id == status.task_id)
        {
            events.push(AgentEvent::CompactionEnd {
                task_id: status.task_id,
                reason: status.reason,
            });
        }
    }
    // 一个生成的回合在其结果被提交时结束：无论先到的是 `completing` 保留还是终态，
    // 因此在保留处创建的后继任务会在它之后开始。
    let mut turn_ended = false;
    for task in tasks.values() {
        let task_id = TaskId::new(task.id.get());
        let status = task_status(&task.state);
        if task.kind == "pi.generation"
            && status == TaskStatusKind::Completing
            && !held.contains(&task_id)
        {
            held.insert(task_id);
            turn_ended = true;
        }
        if status != TaskStatusKind::Terminal {
            continue;
        }
        if task.kind == "pi.generation" && !held.remove(&task_id) {
            turn_ended = true;
        }
        match task_outcome(&task.state) {
            Some(TaskOutcomeLike::Faulted { message }) => events.push(AgentEvent::TaskFailed {
                task_id,
                kind: task.kind.clone(),
                message,
            }),
            Some(TaskOutcomeLike::Orphaned { reason }) => events.push(AgentEvent::TaskFailed {
                task_id,
                kind: task.kind.clone(),
                message: reason,
            }),
            _ => {}
        }
    }
    if turn_ended {
        events.push(AgentEvent::TurnEnd);
    }
    let run = now.live.run.clone();
    let run_before = was.live.run.clone();
    let run_changed = run.as_ref().and_then(|run| run.inputs.first().copied())
        != run_before
            .as_ref()
            .and_then(|run| run.inputs.first().copied());
    if let Some(run_before) = &run_before
        && run_changed
    {
        events.push(AgentEvent::RunEnd {
            inputs: run_before.inputs.clone(),
        });
    }

    // 提交、文档状态，然后是新开始的东西。
    for record in &submissions {
        events.push(AgentEvent::Submission {
            record: record.clone(),
        });
    }
    if now.inbox != was.inbox {
        events.push(AgentEvent::InboxUpdate {
            items: queued(now.inbox.as_ref()),
        });
    }
    // 已退役的文档读作其初始值，与快照一致。
    if now.agent != was.agent {
        events.push(AgentEvent::AgentChanged {
            agent: now.agent.clone().unwrap_or_else(default_agent_state),
        });
    }
    if now.usage != was.usage {
        events.push(AgentEvent::UsageChanged {
            usage: now.usage.clone().unwrap_or_default(),
        });
    }
    for status in &compactions {
        if !compactions_before
            .iter()
            .any(|candidate| candidate.task_id == status.task_id)
        {
            events.push(AgentEvent::CompactionStart {
                task_id: status.task_id,
                reason: status.reason,
                blocking: status.blocking,
            });
        }
    }
    if let Some(run) = &run
        && run_changed
    {
        events.push(AgentEvent::RunStart {
            inputs: run.inputs.clone(),
        });
    }
    if let Some(run) = &run
        && Some(run.task_id) != run_before.as_ref().map(|before| before.task_id)
        && tasks
            .get(&run.task_id)
            .is_some_and(|task| task.kind == "pi.generation")
    {
        events.push(AgentEvent::TurnStart);
    }
    events
}

/// 便于阅读：`endTool` 的 `entryId` 参数。
type EntryIdLike = crate::types::EntryId;

/// `TaskState` 的判别符（`translate` 只需比较）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskStatusKind {
    Pending,
    Running,
    Waiting,
    Completing,
    Terminal,
}

fn task_status<S, R>(state: &crate::types::TaskState<S, R>) -> TaskStatusKind {
    match state {
        crate::types::TaskState::Pending { .. } => TaskStatusKind::Pending,
        crate::types::TaskState::Running { .. } => TaskStatusKind::Running,
        crate::types::TaskState::Waiting { .. } => TaskStatusKind::Waiting,
        crate::types::TaskState::Completing { .. } => TaskStatusKind::Completing,
        crate::types::TaskState::Terminal { .. } => TaskStatusKind::Terminal,
    }
}

fn task_checkpoint<S: Clone, R>(state: crate::types::TaskState<S, R>) -> S {
    match state {
        crate::types::TaskState::Pending { checkpoint }
        | crate::types::TaskState::Running { checkpoint }
        | crate::types::TaskState::Waiting { checkpoint, .. } => checkpoint,
        crate::types::TaskState::Completing { .. } | crate::types::TaskState::Terminal { .. } => {
            unreachable!("checkpoint 只在未结态可用")
        }
    }
}

/// 便于阅读：`translate` 需要的 outcome 判别。
enum TaskOutcomeLike {
    Faulted { message: String },
    Orphaned { reason: String },
}

fn task_outcome<S, R>(state: &crate::types::TaskState<S, R>) -> Option<TaskOutcomeLike> {
    let outcome = match state {
        crate::types::TaskState::Completing { outcome }
        | crate::types::TaskState::Terminal { outcome } => outcome,
        _ => return None,
    };
    match outcome {
        crate::types::TaskOutcome::Faulted { error } => Some(TaskOutcomeLike::Faulted {
            message: error.message.clone(),
        }),
        crate::types::TaskOutcome::Orphaned { reason } => Some(TaskOutcomeLike::Orphaned {
            reason: reason.clone(),
        }),
        _ => None,
    }
}

/// 在途消息的路径（对应 `PARTIAL_PATH`）。
fn partial_path() -> Path {
    ["docs", "pi.live", "generation", "message"]
        .into_iter()
        .map(|segment| PathSegment::Key(segment.to_string()))
        .collect()
}

/// 对应 `messageChanges(viewOps, message)`：把视图上针对在途消息的操作翻译成消息改动。
pub fn message_changes(view_ops: &[Op], message: &AssistantMessage) -> Vec<MessageChange> {
    let mut changes: Vec<MessageChange> = Vec::new();
    // 整体发送的块已经含本批次中它之后的全部改动。
    let mut whole: BTreeSet<usize> = BTreeSet::new();
    let partial = partial_path();
    for op in view_ops {
        // 视图操作从不替换根。
        let path = op_path(op);
        if !path_prefix_matches(path, &partial) {
            // 整条消息或整个生成被替换了。
            if path_prefix_matches(&partial, path) {
                return vec![MessageChange::Message {
                    message: message.clone(),
                }];
            }
            continue;
        }
        let rest = &path[partial.len()..];
        match rest.first() {
            Some(PathSegment::Key(key)) if key == "usage" => continue,
            Some(PathSegment::Key(key)) if key == "content" => {}
            _ => {
                return vec![MessageChange::Message {
                    message: message.clone(),
                }];
            }
        }
        if rest.len() == 1 {
            let Op::Splice(_, start, delete_count, items) = op else {
                return vec![MessageChange::Message {
                    message: message.clone(),
                }];
            };
            if *delete_count != 0 {
                return vec![MessageChange::Message {
                    message: message.clone(),
                }];
            }
            for (offset, item) in items.iter().enumerate() {
                let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) else {
                    return vec![MessageChange::Message {
                        message: message.clone(),
                    }];
                };
                let content_index = start + offset;
                let change = match &block {
                    ContentBlock::Text(_) => MessageChange::TextStart {
                        content_index,
                        block,
                    },
                    ContentBlock::Thinking(_) => MessageChange::ThinkingStart {
                        content_index,
                        block,
                    },
                    _ => MessageChange::ToolcallStart {
                        content_index,
                        block,
                    },
                };
                changes.push(change);
            }
            continue;
        }
        let content_index = match rest.get(1) {
            Some(PathSegment::Index(index)) => *index,
            _ => {
                return vec![MessageChange::Message {
                    message: message.clone(),
                }];
            }
        };
        let field = rest.get(2).and_then(|segment| match segment {
            PathSegment::Key(key) => Some(key.as_str()),
            PathSegment::Index(_) => None,
        });
        if whole.contains(&content_index) {
            continue;
        }
        match (op, field, rest.len()) {
            (Op::Append(_, delta), Some("text"), 3) => changes.push(MessageChange::TextDelta {
                content_index,
                delta: delta.clone(),
            }),
            (Op::Append(_, delta), Some("thinking"), 3) => {
                changes.push(MessageChange::ThinkingDelta {
                    content_index,
                    delta: delta.clone(),
                })
            }
            (Op::Append(_, delta), Some("arguments"), _) => {
                changes.push(MessageChange::ToolcallDelta {
                    content_index,
                    path: rest[3..].to_vec(),
                    delta: delta.clone(),
                })
            }
            _ => {
                whole.insert(content_index);
                let Some(block) = message.content.get(content_index).cloned() else {
                    return vec![MessageChange::Message {
                        message: message.clone(),
                    }];
                };
                changes.push(MessageChange::Block {
                    content_index,
                    block,
                });
            }
        }
    }
    changes
}

/// 对应 `toolUpdate(...)` 的结果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolUpdate {
    /// 输出改动。
    pub output: Option<ToolOutputUpdate>,
    /// 细节（`None` 表示未变；上游用 `null` 表示已清除）。
    pub details: Option<JsonValue>,
    /// 诊断。
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
}

/// 对应 `toolUpdate(viewOps, index, slot, previous)`：运行中槽位的输出、细节与诊断改动。
pub fn tool_update(
    view_ops: &[Op],
    index: usize,
    slot: &ToolSlot,
    previous: &ToolSlot,
) -> Option<ToolUpdate> {
    let output_path: Path = [
        PathSegment::Key("docs".to_string()),
        PathSegment::Key("pi.live".to_string()),
        PathSegment::Key("tools".to_string()),
        PathSegment::Index(index),
        PathSegment::Key("output".to_string()),
    ]
    .into_iter()
    .collect();
    let mut trim_start = 0usize;
    let mut append = String::new();
    let mut set = false;
    for op in view_ops {
        let path = op_path(op);
        if !path_prefix_matches(path, &output_path) {
            continue;
        }
        match op {
            Op::Truncate(_, count) => trim_start += count,
            Op::Append(_, text) => append.push_str(text),
            _ => set = true,
        }
    }
    let output = if set || (slot.output != previous.output && trim_start == 0 && append.is_empty())
    {
        Some(ToolOutputUpdate::Set {
            set: slot.output.clone().unwrap_or_default(),
        })
    } else if trim_start > 0 || !append.is_empty() {
        Some(ToolOutputUpdate::TrimAndAppend {
            trim_start: (trim_start > 0).then_some(trim_start),
            append: (!append.is_empty()).then_some(append),
        })
    } else {
        None
    };
    // 一次安全重放会清掉运行中槽位的进度：被移除的细节发 `null`，被移除的诊断发 `[]`。
    let details =
        (slot.details != previous.details).then(|| slot.details.clone().unwrap_or(JsonValue::Null));
    let diagnostics = (slot.diagnostics != previous.diagnostics)
        .then(|| slot.diagnostics.clone().unwrap_or_default());
    if output.is_none() && details.is_none() && diagnostics.is_none() {
        return None;
    }
    Some(ToolUpdate {
        output,
        details,
        diagnostics,
    })
}

/// 取一个 op 的路径（对应上游的 `op[1] as Path`；`Replace` 没有路径）。
fn op_path(op: &Op) -> &[PathSegment] {
    match op {
        Op::Replace(_) => &[],
        Op::Set(path, _)
        | Op::Delete(path)
        | Op::Append(path, _)
        | Op::Truncate(path, _)
        | Op::Splice(path, ..)
        | Op::Move(path, _) => path,
    }
}

/// 对应 `startsWith(path, prefix)`。
fn path_prefix_matches(path: &[PathSegment], prefix: &[PathSegment]) -> bool {
    prefix.len() <= path.len() && prefix.iter().zip(path).all(|(left, right)| left == right)
}

/// 便于阅读：`SnapshotEvent` 里 `run` 的字段（上游内联为 `{ inputs }`）。
#[allow(dead_code)]
type SnapshotRunFields = LiveRun;

/// 便于阅读：`generation` 的字段（上游内联）。
#[allow(dead_code)]
type SnapshotGenerationFields = LiveGeneration;

/// 便于阅读：`WatchEnd` 供 [`AgentEventStream`] 使用。
#[allow(dead_code)]
type StreamEnd = WatchEnd;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::live::{LiveState, ToolSlotStatus};
    use crate::types::{ConversationId as Conversation, EntryId, Seq};
    use serde_json::json;

    fn conversation_view() -> ConversationView {
        ConversationView {
            conversation: crate::types::ConversationRecord {
                id: Conversation::new(1),
                parent: None,
                owner: None,
            },
            entries: Vec::new(),
            docs: BTreeMap::new(),
        }
    }

    fn view_with(docs: Vec<(&str, JsonValue)>) -> ConversationView {
        let mut view = conversation_view();
        for (kind, value) in docs {
            view.docs.insert(
                kind.to_string(),
                value.as_object().cloned().unwrap_or_default(),
            );
        }
        view
    }

    fn entry(id: u64) -> EntryRecord {
        EntryRecord {
            id: EntryId::new(id),
            conversation_id: Conversation::new(1),
            kind: "pi.user".to_string(),
            model: None,
            data: None,
            head: None,
            edits: None,
            by_task_id: None,
        }
    }

    fn slot(call_id: &str, status: ToolSlotStatus) -> ToolSlot {
        ToolSlot {
            call_id: call_id.to_string(),
            name: "bash".to_string(),
            task_id: Some(TaskId::new(1)),
            status,
            output: None,
            dropped_bytes: None,
            dropped_lines: None,
            details: None,
            diagnostics: None,
            entry: None,
        }
    }

    #[test]
    fn event_types_serialise_with_the_upstream_tags() {
        let snapshot = AgentEvent::Snapshot(SnapshotEvent {
            entries: vec![entry(1)],
            run: None,
            generation: None,
            tools: vec![slot("a", ToolSlotStatus::Done)],
            compactions: Vec::new(),
            inbox: vec![QueuedItem {
                id: SubmissionId::new(2),
                mode: QueuedMode::FollowUp,
            }],
            agent: AgentState::default(),
            usage: UsageState::default(),
        });
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["type"], json!("snapshot"));
        assert_eq!(json["inbox"][0]["mode"], json!("followUp"));
        assert_eq!(json["tools"][0]["callId"], json!("a"));

        let event = AgentEvent::AutoRetryStart {
            attempt: 2,
            at: 1_700,
            error_message: "boom".to_string(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], json!("auto_retry_start"));
        assert_eq!(json["errorMessage"], json!("boom"));

        let update = AgentEvent::ToolExecutionUpdate {
            tool_call_id: "a".to_string(),
            tool_name: "bash".to_string(),
            output: Some(ToolOutputUpdate::TrimAndAppend {
                trim_start: Some(3),
                append: Some("more".to_string()),
            }),
            details: Some(json!(null)),
            diagnostics: None,
        };
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["type"], json!("tool_execution_update"));
        assert_eq!(json["output"]["trimStart"], json!(3));
        assert_eq!(json["output"]["append"], json!("more"));
        assert_eq!(json["details"], json!(null));
        assert!(json.get("diagnostics").is_none());
    }

    #[test]
    fn message_changes_translate_splices_and_appends() {
        let assistant = AssistantMessage {
            content: vec![
                ContentBlock::Text(pi_ai::TextContent {
                    kind: pi_ai::TextKind,
                    text: "hi".to_string(),
                    text_signature: None,
                }),
                ContentBlock::Thinking(pi_ai::ThinkingContent {
                    kind: pi_ai::ThinkingKind,
                    thinking: "why".to_string(),
                    thinking_signature: None,
                    redacted: None,
                }),
            ],
            ..default_assistant()
        };
        let partial = partial_path();
        let content_path: Path = partial
            .iter()
            .cloned()
            .chain([PathSegment::Key("content".to_string())])
            .collect();
        let splices = Op::Splice(
            content_path.clone(),
            0,
            0,
            vec![serde_json::to_value(&assistant.content[0]).unwrap()],
        );
        let changes = message_changes(&[splices], &assistant);
        assert_eq!(
            changes,
            vec![MessageChange::TextStart {
                content_index: 0,
                block: assistant.content[0].clone(),
            }]
        );

        let append_path: Path = content_path
            .iter()
            .cloned()
            .chain([PathSegment::Index(0), PathSegment::Key("text".to_string())])
            .collect();
        let changes = message_changes(&[Op::Append(append_path, " there".to_string())], &assistant);
        assert_eq!(
            changes,
            vec![MessageChange::TextDelta {
                content_index: 0,
                delta: " there".to_string(),
            }]
        );
    }

    #[test]
    fn message_changes_fall_back_to_a_whole_message() {
        let assistant = default_assistant();
        // 路径不是 PARTIAL_PATH 的前缀，也不是它的扩展：整条消息。
        let other: Path = vec![PathSegment::Key("docs".to_string())];
        let changes = message_changes(&[Op::Set(other, json!(1))], &assistant);
        assert_eq!(
            changes,
            vec![MessageChange::Message {
                message: assistant.clone()
            }]
        );

        // 路径是 PARTIAL_PATH 的前缀：整个生成被替换，仍是整条消息。
        let prefix: Path = partial_path()[..2].to_vec();
        let changes = message_changes(&[Op::Set(prefix, json!({}))], &assistant);
        assert_eq!(changes, vec![MessageChange::Message { message: assistant }]);
    }

    #[test]
    fn message_changes_ignore_usage_operations() {
        let assistant = default_assistant();
        let usage_path: Path = partial_path()
            .iter()
            .cloned()
            .chain([PathSegment::Key("usage".to_string())])
            .collect();
        let changes = message_changes(&[Op::Set(usage_path, json!(1))], &assistant);
        assert!(changes.is_empty(), "usage 的改动不产生消息改动");
    }

    #[test]
    fn tool_update_reports_a_front_trim_and_append() {
        let mut previous = slot("a", ToolSlotStatus::Running);
        previous.output = Some("abcdef".to_string());
        let mut current = previous.clone();
        current.output = Some("cdefghi".to_string());

        let mut path: Path = vec![
            PathSegment::Key("docs".to_string()),
            PathSegment::Key("pi.live".to_string()),
            PathSegment::Key("tools".to_string()),
            PathSegment::Index(0),
            PathSegment::Key("output".to_string()),
        ];
        let update = tool_update(
            &[
                Op::Truncate(path.clone(), 2),
                Op::Append(path.clone(), "ghi".to_string()),
            ],
            0,
            &current,
            &previous,
        )
        .expect("update");
        assert_eq!(
            update.output,
            Some(ToolOutputUpdate::TrimAndAppend {
                trim_start: Some(2),
                append: Some("ghi".to_string()),
            })
        );

        // 没有操作时，输出差异整体替换。
        path.push(PathSegment::Key("tail".to_string()));
        let update = tool_update(&[], 0, &current, &previous).expect("update");
        assert_eq!(
            update.output,
            Some(ToolOutputUpdate::Set {
                set: "cdefghi".to_string()
            })
        );
    }

    #[test]
    fn tool_update_reports_cleared_details_and_diagnostics() {
        let mut previous = slot("a", ToolSlotStatus::Running);
        previous.details = Some(json!({"n": 1}));
        previous.diagnostics = Some(vec![ToolDiagnostic {
            severity: crate::harness::types::ToolDiagnosticSeverity::Warn,
            message: "careful".to_string(),
            code: None,
        }]);
        let current = slot("a", ToolSlotStatus::Running);

        let update = tool_update(&[], 0, &current, &previous).expect("update");
        assert_eq!(update.details, Some(json!(null)), "清除的细节发 null");
        assert_eq!(update.diagnostics, Some(Vec::new()), "清除的诊断发空数组");
        assert!(update.output.is_none());
    }

    #[test]
    fn tool_update_without_changes_is_none() {
        let previous = slot("a", ToolSlotStatus::Running);
        assert!(tool_update(&[], 0, &previous, &previous).is_none());
    }

    #[test]
    fn translate_ignores_a_publication_without_view_or_table_changes() {
        let before = conversation_view();
        let after = conversation_view();
        let publication = CommitPublication {
            seq: Seq::new(1),
            changes: Vec::new(),
        };
        let events = translate(
            Conversation::new(1),
            &before,
            &after,
            &[],
            &publication,
            &mut HashSet::new(),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn translate_emits_entry_appended_for_entries_without_messages() {
        let before = conversation_view();
        let after = conversation_view();
        let publication = CommitPublication {
            seq: Seq::new(2),
            changes: vec![CommitChange::Table(TableCommitChange::Entry(entry(7)))],
        };
        let events = translate(
            Conversation::new(1),
            &before,
            &after,
            &[],
            &publication,
            &mut HashSet::new(),
        );
        assert_eq!(events, vec![AgentEvent::EntryAppended { entry: entry(7) }]);
    }

    #[test]
    fn translate_reports_document_changes_in_order() {
        let before = view_with(vec![("pi.agent", json!({"cwd": "/a"}))]);
        let mut after = before.clone();
        after.docs.insert(
            "pi.agent".to_string(),
            json!({"cwd": "/b"}).as_object().cloned().unwrap(),
        );
        after.docs.insert(
            "pi.usage".to_string(),
            json!({"models": {}, "tools": {}})
                .as_object()
                .cloned()
                .unwrap(),
        );
        let publication = CommitPublication {
            seq: Seq::new(3),
            changes: Vec::new(),
        };
        let events = translate(
            Conversation::new(1),
            &before,
            &after,
            &[Op::Set(vec![PathSegment::Key("x".to_string())], json!(1))],
            &publication,
            &mut HashSet::new(),
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AgentEvent::AgentChanged { .. }));
        assert!(matches!(events[1], AgentEvent::UsageChanged { .. }));
    }

    #[test]
    fn translate_reports_a_retired_document_as_its_initial_value() {
        let before = view_with(vec![("pi.agent", json!({"cwd": "/a"}))]);
        let after = view_with(vec![]);
        let publication = CommitPublication {
            seq: Seq::new(4),
            changes: Vec::new(),
        };
        let events = translate(
            Conversation::new(1),
            &before,
            &after,
            &[Op::Delete(vec![PathSegment::Key("docs".to_string())])],
            &publication,
            &mut HashSet::new(),
        );
        match &events[0] {
            AgentEvent::AgentChanged { agent } => {
                assert!(agent.cwd.is_none(), "退役的 pi.agent 读作初始值");
            }
            other => panic!("expected agent_changed, got {other:?}"),
        }
    }

    #[test]
    fn translate_reports_run_start_and_end() {
        let before = view_with(vec![(
            "pi.live",
            json!({"run": {"taskId": 1, "inputs": [10]}}),
        )]);
        let after = view_with(vec![(
            "pi.live",
            json!({"run": {"taskId": 2, "inputs": [11]}}),
        )]);
        let publication = CommitPublication {
            seq: Seq::new(5),
            changes: Vec::new(),
        };
        let events = translate(
            Conversation::new(1),
            &before,
            &after,
            &[Op::Set(vec![PathSegment::Key("x".to_string())], json!(1))],
            &publication,
            &mut HashSet::new(),
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::RunEnd { .. })),
            "旧运行结束"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::RunStart { .. })),
            "新运行开始"
        );
    }

    #[test]
    fn translate_keeps_queued_modes() {
        let before = conversation_view();
        let after = view_with(vec![(
            "pi.inbox",
            json!({"items": [{"id": 1, "mode": "steer", "content": "hi"}, {"id": 2, "mode": "write", "entry": {}}]}),
        )]);
        let publication = CommitPublication {
            seq: Seq::new(6),
            changes: Vec::new(),
        };
        let events = translate(
            Conversation::new(1),
            &before,
            &after,
            &[Op::Set(vec![PathSegment::Key("x".to_string())], json!(1))],
            &publication,
            &mut HashSet::new(),
        );
        let inbox_update = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::InboxUpdate { items } => Some(items.clone()),
                _ => None,
            })
            .expect("inbox_update");
        assert_eq!(inbox_update.len(), 2);
        assert_eq!(inbox_update[0].mode, QueuedMode::Steer);
        assert_eq!(inbox_update[1].mode, QueuedMode::Write);
    }

    #[test]
    fn snapshot_of_defaults_absent_documents() {
        let snapshot = snapshot_of(&conversation_view());
        assert!(snapshot.run.is_none());
        assert!(snapshot.tools.is_empty());
        assert!(snapshot.inbox.is_empty());
        assert!(snapshot.agent.cwd.is_none());
        assert!(snapshot.usage.models.is_empty());
    }

    #[test]
    fn snapshot_of_reads_live_state() {
        let view = view_with(vec![
            (
                "pi.live",
                json!({
                    "run": {"taskId": 3, "inputs": [7]},
                    "generation": {"attempt": 2, "retry": {"at": 5, "error": "boom"}},
                    "tools": [{"callId": "a", "name": "bash", "status": "running"}],
                }),
            ),
            ("pi.agent", json!({"cwd": "/work"})),
        ]);
        let snapshot = snapshot_of(&view);
        assert_eq!(snapshot.run.expect("run").inputs.len(), 1);
        let generation = snapshot.generation.expect("generation");
        assert_eq!(generation.attempt, 2);
        assert_eq!(generation.retry.expect("retry").error, "boom");
        assert_eq!(snapshot.tools.len(), 1);
        assert_eq!(snapshot.agent.cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn live_state_round_trips_through_the_parts_helper() {
        let view = view_with(vec![(
            "pi.live",
            json!({"tools": [{"callId": "a", "name": "bash", "status": "done"}]}),
        )]);
        let parsed = parts(&view);
        assert_eq!(parsed.live.tools.expect("tools").len(), 1);
        assert!(parsed.inbox.is_none());
    }

    #[test]
    fn unused_live_state_marker() {
        let _ = std::mem::size_of::<LiveState>();
    }

    fn default_assistant() -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "openai-completions".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: pi_ai::default_usage(),
            stop_reason: pi_ai::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 1,
            duration_ms: None,
        }
    }
}
