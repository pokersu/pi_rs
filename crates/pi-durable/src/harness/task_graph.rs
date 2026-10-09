//! 对应 `harness/task-graph.ts`：一个 Session 的全部活动任务（spec §9.5）。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - **挂载值以 JSON 保存**：与 [`crate::harness::view`] 同一手法（上游把 `TaskGraph` 对象直接交给
//!   chord 并就地推进）。
//! - 复用 [`crate::harness::view::ViewObserver`]：`state()` / `watch()` 需要的观察者形状与视图挂载一致
//!   （`advance` + `closeSession`），不再另立一份。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::chord::context::{Context, without_abort_signal};
use crate::chord::delta::{Op, PathSegment, apply_immutable};
use crate::chord::state::{AttachedReplicatedState, attach_replicated_state_source};
use crate::harness::util::{closed_error, scan_all};
use crate::harness::view::{ReleaseHandle, ReleaseSlot, ViewObserver};
use crate::session::observation::{CommittedStateSource, CommittedWatch};
use crate::session::{SessionError, SessionImpl, SessionSubscription};
use crate::types::{
    CommitChange, CommitPublication, ConversationId, ConversationQuery, Cursor, JoinPolicy, Page,
    Storage, TableCommitChange, TaskId, TaskOutcome, TaskQuery, TaskRecord, TaskState, TaskStatus,
};

/// 对应 `AnyTaskRecord`。
type AnyTaskRecord = TaskRecord<JsonValue, JsonValue, JsonValue>;

/// 对应 `LIVE_STATUSES`。
const LIVE_STATUSES: [TaskStatus; 4] = [
    TaskStatus::Pending,
    TaskStatus::Running,
    TaskStatus::Waiting,
    TaskStatus::Completing,
];

/// 对应 `SCAN_PAGE_SIZE`。
const SCAN_PAGE_SIZE: usize = 256;

/// 对应 `TaskGraphState`：一个活动任务的持久状态（不含 checkpoint 与结果载荷）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum TaskGraphState {
    /// `pending`。
    Pending {
        /// 当前阶段。
        phase: String,
    },
    /// `running`。
    Running {
        /// 当前阶段。
        phase: String,
    },
    /// `waiting`：等 `on` 中任务全部终态。
    Waiting {
        /// 当前阶段。
        phase: String,
        /// 所等待的任务。
        on: Vec<TaskId>,
        /// 汇合策略。
        policy: JoinPolicy,
    },
    /// `completing`：结果保留到其自有普通工作排空。
    Completing {
        /// 结果判别符。
        outcome: TaskOutcomeStatus,
    },
}

/// 对应 `TaskOutcome<JsonValue>["status"]`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskOutcomeStatus {
    /// `completed`。
    Completed,
    /// `failed`。
    Failed,
    /// `aborted`。
    Aborted,
    /// `orphaned`。
    Orphaned,
    /// `faulted`。
    Faulted,
}

/// 对应 `TaskGraphNode`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskGraphNode {
    /// 任务 ID。
    pub id: TaskId,
    /// 定义名。
    pub kind: String,
    /// 所属会话。
    pub conversation_id: ConversationId,
    /// 拥有者任务；会话拥有的任务缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    /// 是否后台。
    pub background: bool,
    /// 中止标记。
    pub abort_requested: bool,
    /// 状态。
    pub state: TaskGraphState,
    /// 该任务拥有的会话（按 ID 顺序）。
    pub conversations: Vec<ConversationId>,
}

/// 对应 `TaskGraph`：一个 Session 的全部活动任务。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskGraph {
    /// 活动任务，按十进制 ID 为键。
    pub tasks: BTreeMap<String, TaskGraphNode>,
}

/// 一个挂载：其当前修订与观察者。
struct Mount {
    value: Mutex<JsonValue>,
    observers: Mutex<Vec<Arc<dyn ViewObserver>>>,
}

/// Harness 的任务图挂载：由第一个观察者在 Session 线上建立、随最后一个观察者丢弃。
///
/// 它从 Session 的提交出版物推进，而那些出版物是持久的。
pub struct TaskGraphView {
    session: Arc<SessionImpl>,
    storage: Arc<dyn Storage>,
    mount: Mutex<Option<Arc<Mount>>>,
    closed: AtomicBool,
    commit_subscription: Mutex<Option<SessionSubscription>>,
    close_subscription: Mutex<Option<SessionSubscription>>,
    self_ref: OnceLock<Weak<TaskGraphView>>,
}

impl TaskGraphView {
    /// 对应 `new TaskGraphView(session, storage)`：立即订阅提交与关闭。
    pub fn new(session: Arc<SessionImpl>, storage: Arc<dyn Storage>) -> Arc<Self> {
        let view = Arc::new(Self {
            session: Arc::clone(&session),
            storage,
            mount: Mutex::new(None),
            closed: AtomicBool::new(false),
            commit_subscription: Mutex::new(None),
            close_subscription: Mutex::new(None),
            self_ref: OnceLock::new(),
        });
        let _ = view.self_ref.set(Arc::downgrade(&view));

        let weak = Arc::downgrade(&view);
        let commit_subscription =
            session.subscribe_commits(Arc::new(move |publication, context| {
                let Some(view) = weak.upgrade() else {
                    return;
                };
                let mount = view.mount.lock().expect("mount").clone();
                if let Some(mount) = mount {
                    advance(&mount, publication, context);
                }
            }));

        let weak = Arc::downgrade(&view);
        let close_subscription = session.subscribe_close(Arc::new(move || {
            let Some(view) = weak.upgrade() else {
                return;
            };
            view.closed.store(true, Ordering::Release);
            let mount = view.mount.lock().expect("mount").take();
            if let Some(mount) = mount {
                let observers: Vec<Arc<dyn ViewObserver>> =
                    mount.observers.lock().expect("observers").clone();
                for observer in observers {
                    observer.close_session();
                }
            }
        }));
        *view.commit_subscription.lock().expect("subscription") = commit_subscription.ok();
        *view.close_subscription.lock().expect("subscription") = close_subscription.ok();
        view
    }

    /// 对应 `state(context)`：任务图的可释放只读 chord 状态。
    pub async fn state(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<AttachedReplicatedState, SessionError> {
        let (source, _detach) = self
            .attach(
                |value, handle| Ok(CommittedStateSource::new(value, handle.into_release())),
                context,
            )
            .await?;
        Ok(attach_replicated_state_source(source.as_ref(), None))
    }

    /// 对应 `watch(context)`：任务图的串行精确帧观察；取消 `context` 会停止它。
    pub async fn watch(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<Arc<CommittedWatch>, SessionError> {
        let signal = context.abort_signal();
        let (watch, _detach) = self
            .attach(
                |value, handle| Ok(CommittedWatch::new(value, handle.into_release(), None)),
                Arc::clone(&context),
            )
            .await?;
        if signal.as_ref().is_some_and(|signal| signal.aborted()) {
            watch.cancel();
            return Err(SessionError::Aborted(pi_ai::AbortError));
        }
        if let Some(signal) = signal {
            watch.observe_cancellation(signal);
        }
        Ok(watch)
    }

    /// 对应 `#attach(create, context)`：在 Session 线上原子地注册一个从当前修订创建的观察者。
    pub async fn attach<O, F>(
        &self,
        create: F,
        context: Arc<dyn Context>,
    ) -> Result<(Arc<O>, ReleaseSlot), SessionError>
    where
        O: ViewObserver + 'static,
        F: FnOnce(JsonValue, ReleaseHandle) -> Result<O, SessionError>,
    {
        let session = Arc::clone(&self.session);
        let view = self
            .self_ref
            .get()
            .and_then(Weak::upgrade)
            .expect("task graph self reference");
        session
            .read_on_line(move || {
                let view = Arc::clone(&view);
                async move {
                    // 先取出已有挂载并释放锁：`std::sync::Mutex` 不可重入。
                    let existing = view.mount.lock().expect("mount").clone();
                    let mount = match existing {
                        Some(mount) => mount,
                        None => {
                            let value = view.build(&context).await?;
                            let mount = Arc::new(Mount {
                                value: Mutex::new(value),
                                observers: Mutex::new(Vec::new()),
                            });
                            *view.mount.lock().expect("mount") = Some(Arc::clone(&mount));
                            mount
                        }
                    };

                    let slot = ReleaseSlot::new();
                    let value = mount.value.lock().expect("value").clone();
                    let observer = Arc::new(create(value, ReleaseHandle::from_slot(slot.clone()))?);

                    // 关闭或取消可能在挂载构建期间开始；此时不注册任何东西。
                    if view.closed.load(Ordering::Acquire) {
                        return Err(closed_error());
                    }
                    if let Some(signal) = context.abort_signal() {
                        signal.throw_if_aborted()?;
                    }

                    mount
                        .observers
                        .lock()
                        .expect("observers")
                        .push(Arc::clone(&observer) as Arc<dyn ViewObserver>);

                    let mount_for_release = Arc::clone(&mount);
                    let observer_for_release: Arc<dyn ViewObserver> =
                        Arc::clone(&observer) as Arc<dyn ViewObserver>;
                    let view_for_release = Arc::downgrade(&view);
                    slot.set(Box::new(move || {
                        mount_for_release
                            .observers
                            .lock()
                            .expect("observers")
                            .retain(|candidate| !Arc::ptr_eq(candidate, &observer_for_release));
                        if mount_for_release
                            .observers
                            .lock()
                            .expect("observers")
                            .is_empty()
                            && let Some(view) = view_for_release.upgrade()
                        {
                            let mut guard = view.mount.lock().expect("mount");
                            if guard
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &mount_for_release))
                            {
                                *guard = None;
                            }
                        }
                    }));
                    Ok((observer, slot))
                }
            })
            .await
    }

    /// 对应 `#build(context)`。
    async fn build(&self, context: &Arc<dyn Context>) -> Result<JsonValue, SessionError> {
        let mut records: Vec<AnyTaskRecord> = Vec::new();
        for status in LIVE_STATUSES {
            let query = TaskQuery {
                conversation_id: None,
                kind: None,
                status: Some(status),
                abort_requested: None,
                background: None,
                order: None,
            };
            let scanned: Vec<AnyTaskRecord> = scan_all(|cursor: Option<Cursor>| {
                // `TaskQuery` 不是 `Copy`，而 `scan_all` 每页都调用一次闭包。
                let query = query.clone();
                async move {
                    self.storage
                        .scan_tasks(query, SCAN_PAGE_SIZE, cursor, context.as_ref())
                        .await
                }
            })
            .await
            .map_err(SessionError::from)?;
            records.extend(scanned);
        }
        records.sort_by_key(|record| record.id);

        let mut tasks: BTreeMap<String, TaskGraphNode> = BTreeMap::new();
        for record in &records {
            let query = ConversationQuery {
                owner_conversation_id: None,
                owner_task_id: Some(TaskId::new(record.id.get())),
                order: None,
            };
            let owned: Vec<ConversationId> = scan_all(|cursor: Option<Cursor>| async move {
                let page: Result<Page<crate::types::ConversationRecord, Cursor>, _> = self
                    .storage
                    .scan_conversations(query, SCAN_PAGE_SIZE, cursor, context.as_ref())
                    .await;
                page
            })
            .await
            .map_err(SessionError::from)?
            .into_iter()
            .map(|conversation| conversation.id)
            .collect();
            let mut conversations = owned;
            conversations.sort();
            tasks.insert(record.id.get().to_string(), node_of(record, &conversations));
        }
        serde_json::to_value(TaskGraph { tasks })
            .map_err(|error| SessionError::Message(format!("task graph serialises: {error}")))
    }
}

/// 对应 `advance(mount, publication, context)`：从一个发布派生操作、应用并交给每个观察者。
fn advance(mount: &Mount, publication: &CommitPublication, context: &Arc<dyn Context>) {
    let current = mount.value.lock().expect("value").clone();
    let mut ops: Vec<Op> = Vec::new();
    // 本发布设置或删除的节点（相对于挂载的值）。
    let mut changed: BTreeMap<String, Option<TaskGraphNode>> = BTreeMap::new();

    for change in &publication.changes {
        let CommitChange::Table(TableCommitChange::Task(record)) = change else {
            continue;
        };
        let key = record.id.get().to_string();
        let previous = match changed.get(&key) {
            Some(node) => node.clone(),
            None => read_node(&current, &key),
        };
        if record.state.status() == TaskStatus::Terminal {
            if previous.is_none() {
                continue;
            }
            ops.push(Op::Delete(vec![
                PathSegment::Key("tasks".to_string()),
                PathSegment::Key(key.clone()),
            ]));
            changed.insert(key, None);
            continue;
        }
        let next = node_of(
            record,
            previous
                .as_ref()
                .map(|node| node.conversations.as_slice())
                .unwrap_or_default(),
        );
        if previous.as_ref() == Some(&next) {
            continue;
        }
        ops.push(Op::Set(
            vec![
                PathSegment::Key("tasks".to_string()),
                PathSegment::Key(key.clone()),
            ],
            serde_json::to_value(&next).expect("node serialises"),
        ));
        changed.insert(key, Some(next));
    }

    // 放在任务之后，因此在同一个提交里连同拥有者任务一起创建的会话能找到拥有者的节点。
    // 一次发布内的变更顺序未指定，所以每个拥有者的列表都要重新排序。
    let mut created: BTreeMap<String, Vec<ConversationId>> = BTreeMap::new();
    for change in &publication.changes {
        let CommitChange::Table(TableCommitChange::Conversation(record)) = change else {
            continue;
        };
        let Some(owner) = record.owner else {
            continue;
        };
        let key = owner.task_id.get().to_string();
        let node = match changed.get(&key) {
            Some(node) => node.clone(),
            None => read_node(&current, &key),
        };
        if node.is_some() {
            created.entry(key).or_default().push(record.id);
        }
    }
    for (key, ids) in created {
        let node = match changed.get(&key) {
            Some(node) => node.clone(),
            None => read_node(&current, &key),
        };
        let Some(node) = node else {
            continue;
        };
        let mut conversations = node.conversations.clone();
        conversations.extend(ids);
        conversations.sort();
        ops.push(Op::Set(
            vec![
                PathSegment::Key("tasks".to_string()),
                PathSegment::Key(key),
                PathSegment::Key("conversations".to_string()),
            ],
            serde_json::to_value(&conversations).expect("conversations serialise"),
        ));
    }

    if ops.is_empty() {
        return;
    }
    let next = match apply_immutable(Some(current), &ops) {
        Ok(value) => value,
        // 操作由本模块构造，失败说明内部不一致；保持原值并跳过本次推进。
        Err(_) => return,
    };
    *mount.value.lock().expect("value") = next.clone();
    let frame_context = without_abort_signal(Arc::clone(context));
    let observers: Vec<Arc<dyn ViewObserver>> = mount.observers.lock().expect("observers").clone();
    for observer in observers {
        observer.advance(next.clone(), &ops, Arc::clone(&frame_context));
    }
}

/// 从挂载值里读一个节点（对应 `mount.value.tasks[key]`）。
fn read_node(value: &JsonValue, key: &str) -> Option<TaskGraphNode> {
    value
        .get("tasks")
        .and_then(|tasks| tasks.get(key))
        .and_then(|node| serde_json::from_value(node.clone()).ok())
}

/// 对应 `nodeOf(record, conversations)`。
fn node_of(record: &AnyTaskRecord, conversations: &[ConversationId]) -> TaskGraphNode {
    TaskGraphNode {
        id: TaskId::new(record.id.get()),
        kind: record.kind.clone(),
        conversation_id: record.conversation_id,
        owner: record.owner.map(|owner| TaskId::new(owner.get())),
        background: record.background,
        abort_requested: record.abort_requested,
        state: state_of(&record.state),
        conversations: conversations.to_vec(),
    }
}

/// 对应 `stateOf(record)`。
fn state_of<S, R>(state: &TaskState<S, R>) -> TaskGraphState
where
    S: AsJsonPhase,
{
    match state {
        TaskState::Pending { checkpoint } => TaskGraphState::Pending {
            phase: checkpoint.phase(),
        },
        TaskState::Running { checkpoint } => TaskGraphState::Running {
            phase: checkpoint.phase(),
        },
        TaskState::Waiting {
            checkpoint,
            on,
            policy,
        } => TaskGraphState::Waiting {
            phase: checkpoint.phase(),
            on: on.iter().map(|task| TaskId::new(task.get())).collect(),
            policy: *policy,
        },
        // 终态记录不会到这里：它们会离开图。
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => {
            TaskGraphState::Completing {
                outcome: outcome_status(outcome),
            }
        }
    }
}

/// 对应 `phaseOf(checkpoint)`：checkpoint 的 `phase` 字段。
trait AsJsonPhase {
    /// 当前阶段名。
    fn phase(&self) -> String;
}

impl AsJsonPhase for JsonValue {
    fn phase(&self) -> String {
        self.get("phase")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string()
    }
}

/// 对应 `state.outcome.status`。
fn outcome_status<R>(outcome: &TaskOutcome<R>) -> TaskOutcomeStatus {
    match outcome {
        TaskOutcome::Completed { .. } => TaskOutcomeStatus::Completed,
        TaskOutcome::Failed { .. } => TaskOutcomeStatus::Failed,
        TaskOutcome::Aborted { .. } => TaskOutcomeStatus::Aborted,
        TaskOutcome::Orphaned { .. } => TaskOutcomeStatus::Orphaned,
        TaskOutcome::Faulted { .. } => TaskOutcomeStatus::Faulted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConversationRecord, EntryId, Seq, TaskOutcomeError, TaskStatus as Status};
    use serde_json::json;

    fn record(id: u64, status: Status) -> AnyTaskRecord {
        TaskRecord {
            id: TaskId::new(id),
            conversation_id: ConversationId::new(1),
            kind: "pi.generation".to_string(),
            version: 1,
            input: JsonValue::Null,
            owner: None,
            background: false,
            abort_requested: false,
            started_at: None,
            ended_at: None,
            memos: None,
            state: match status {
                Status::Pending => TaskState::Pending {
                    checkpoint: json!({"phase": "generate"}),
                },
                Status::Running => TaskState::Running {
                    checkpoint: json!({"phase": "tools"}),
                },
                Status::Waiting => TaskState::Waiting {
                    checkpoint: json!({"phase": "wait"}),
                    on: vec![TaskId::new(9)],
                    policy: JoinPolicy::AllSettled,
                },
                Status::Completing => TaskState::Completing {
                    outcome: TaskOutcome::Completed {
                        result: JsonValue::Null,
                    },
                },
                Status::Terminal => TaskState::Terminal {
                    outcome: TaskOutcome::Failed {
                        error: TaskOutcomeError {
                            message: "boom".to_string(),
                            detail: None,
                        },
                        result: None,
                    },
                },
            },
        }
    }

    #[test]
    fn node_matches_the_upstream_shape() {
        let node = node_of(&record(3, Status::Running), &[ConversationId::new(1)]);
        let json = serde_json::to_value(&node).unwrap();
        assert_eq!(json["id"], json!(3));
        assert_eq!(json["kind"], json!("pi.generation"));
        assert_eq!(json["conversationId"], json!(1));
        assert_eq!(json["state"]["status"], json!("running"));
        assert_eq!(json["state"]["phase"], json!("tools"));
        assert_eq!(json["conversations"], json!([1]));
        assert!(json.get("owner").is_none(), "会话拥有的任务不写 owner 键");
    }

    #[test]
    fn waiting_nodes_carry_their_join_policy() {
        let node = node_of(&record(4, Status::Waiting), &[]);
        let json = serde_json::to_value(&node).unwrap();
        assert_eq!(json["state"]["status"], json!("waiting"));
        assert_eq!(json["state"]["on"], json!([9]));
        assert_eq!(json["state"]["policy"], json!("allSettled"));
        assert_eq!(json["conversations"], json!([]));
    }

    #[test]
    fn completing_nodes_carry_their_outcome_status() {
        let node = node_of(&record(5, Status::Completing), &[]);
        let json = serde_json::to_value(&node).unwrap();
        assert_eq!(json["state"]["status"], json!("completing"));
        assert_eq!(json["state"]["outcome"], json!("completed"));
    }

    #[test]
    fn owner_is_serialised_when_present() {
        let mut value = record(6, Status::Pending);
        value.owner = Some(TaskId::new(2));
        let json = serde_json::to_value(node_of(&value, &[])).unwrap();
        assert_eq!(json["owner"], json!(2));
    }

    #[test]
    fn graph_round_trips() {
        let mut tasks = BTreeMap::new();
        tasks.insert(
            "1".to_string(),
            node_of(&record(1, Status::Running), &[ConversationId::new(2)]),
        );
        let graph = TaskGraph { tasks };
        let json = serde_json::to_value(&graph).unwrap();
        assert_eq!(json["tasks"]["1"]["id"], json!(1));
        let back: TaskGraph = serde_json::from_value(json).unwrap();
        assert_eq!(back, graph);
    }

    #[test]
    fn advance_adds_and_removes_nodes() {
        let mount = Mount {
            value: Mutex::new(json!({"tasks": {}})),
            observers: Mutex::new(Vec::new()),
        };

        let running = record(1, Status::Running);
        let publication = CommitPublication {
            seq: Seq::new(1),
            changes: vec![CommitChange::Table(TableCommitChange::Task(
                running.clone(),
            ))],
        };
        advance(
            &mount,
            &publication,
            &crate::chord::context::BACKGROUND_CONTEXT.clone(),
        );
        let value = mount.value.lock().expect("value").clone();
        assert_eq!(value["tasks"]["1"]["state"]["status"], json!("running"));

        // 终态记录离开图。
        let terminal = record(1, Status::Terminal);
        let publication = CommitPublication {
            seq: Seq::new(2),
            changes: vec![CommitChange::Table(TableCommitChange::Task(terminal))],
        };
        advance(
            &mount,
            &publication,
            &crate::chord::context::BACKGROUND_CONTEXT.clone(),
        );
        let value = mount.value.lock().expect("value").clone();
        assert_eq!(value["tasks"], json!({}));
    }

    #[test]
    fn advance_ignores_a_removal_of_an_unknown_node() {
        let mount = Mount {
            value: Mutex::new(json!({"tasks": {}})),
            observers: Mutex::new(Vec::new()),
        };
        let publication = CommitPublication {
            seq: Seq::new(1),
            changes: vec![CommitChange::Table(TableCommitChange::Task(record(
                7,
                Status::Terminal,
            )))],
        };
        advance(
            &mount,
            &publication,
            &crate::chord::context::BACKGROUND_CONTEXT.clone(),
        );
        assert_eq!(
            mount.value.lock().expect("value").clone(),
            json!({"tasks": {}}),
            "未知节点的终态记录不产生操作"
        );
    }

    #[test]
    fn advance_appends_owned_conversations() {
        let mount = Mount {
            value: Mutex::new(
                serde_json::to_value(TaskGraph {
                    tasks: {
                        let mut tasks = BTreeMap::new();
                        tasks.insert("1".to_string(), node_of(&record(1, Status::Running), &[]));
                        tasks
                    },
                })
                .unwrap(),
            ),
            observers: Mutex::new(Vec::new()),
        };
        let mut conversation = ConversationRecord {
            id: ConversationId::new(3),
            parent: None,
            owner: None,
        };
        conversation.owner = Some(crate::types::ConversationOwner {
            conversation_id: ConversationId::new(1),
            task_id: TaskId::new(1),
        });
        let publication = CommitPublication {
            seq: Seq::new(3),
            changes: vec![CommitChange::Table(TableCommitChange::Conversation(
                conversation,
            ))],
        };
        advance(
            &mount,
            &publication,
            &crate::chord::context::BACKGROUND_CONTEXT.clone(),
        );
        let value = mount.value.lock().expect("value").clone();
        assert_eq!(value["tasks"]["1"]["conversations"], json!([3]));
    }

    #[test]
    fn advance_skips_an_unchanged_node() {
        let mount = Mount {
            value: Mutex::new(
                serde_json::to_value(TaskGraph {
                    tasks: {
                        let mut tasks = BTreeMap::new();
                        tasks.insert("1".to_string(), node_of(&record(1, Status::Running), &[]));
                        tasks
                    },
                })
                .unwrap(),
            ),
            observers: Mutex::new(Vec::new()),
        };
        let before = mount.value.lock().expect("value").clone();
        let publication = CommitPublication {
            seq: Seq::new(4),
            changes: vec![CommitChange::Table(TableCommitChange::Task(record(
                1,
                Status::Running,
            )))],
        };
        advance(
            &mount,
            &publication,
            &crate::chord::context::BACKGROUND_CONTEXT.clone(),
        );
        assert_eq!(
            mount.value.lock().expect("value").clone(),
            before,
            "未变化的节点不产生操作"
        );
    }

    #[test]
    fn live_statuses_match_upstream() {
        assert_eq!(
            LIVE_STATUSES,
            [
                TaskStatus::Pending,
                TaskStatus::Running,
                TaskStatus::Waiting,
                TaskStatus::Completing
            ]
        );
        assert!(!LIVE_STATUSES.contains(&TaskStatus::Terminal));
    }

    #[test]
    fn unused_entry_id_marker() {
        let _ = EntryId::new(1);
    }
}
