//! 对应 `harness/scheduler.ts` 的所有权与派生逻辑。
//!
//! 上游把这些函数放在 `TaskScheduler` 的私有方法与模块级函数里；它们全部是**纯逻辑**（读任务记录与
//! 所有者边，不做 I/O），因此 Rust 侧单独成模块，可以用构造的记录直接测试。
//!
//! 任务与会话构成一棵所有权树（spec §5.5）：任务的父节点是它的拥有者任务，否则是它的会话；
//! 会话的父节点是拥有它的任务（若有）。沿这棵树向上的遍历决定级联、空闲范围，以及一个任务是否还有
//! 活动的「普通自有工作」——后者会让它的结果先以 `completing` 保留，并推迟它的 abort 处理器。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游的 `#node` / `#edge` 直接读 scheduler 的私有表；Rust 用 [`OwnerLookup`] 让遍历逻辑与状态机解耦。
//! - 上游的生成器 `#above()` → Rust 返回 `Vec<Step>`（调用方按序消费，语义一致）。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::session::transaction::Transaction;
use crate::types::{ConversationId, Task, TaskId, TaskRecord, TaskState, TaskStatus};

/// 对应 `AnyTaskRecord`。
pub type AnyTaskRecord = TaskRecord<JsonValue, JsonValue, JsonValue>;

/// 对应 `TaskNode`：一个任务不可变的所有权字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskNode {
    /// 所属会话。
    pub conversation_id: ConversationId,
    /// 拥有者任务；会话拥有的任务缺省。
    pub owner: Option<TaskId>,
    /// 是否后台。
    pub background: bool,
}

/// 对应 `Up`：沿所有权树向上继续的位置——一个拥有者任务，或一个会话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Up {
    /// 拥有者任务。
    Task(TaskId),
    /// 会话。
    Conversation(ConversationId),
}

/// 对应 `Step`：向上走的一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// 一个拥有者任务及其节点。
    Task {
        /// 任务 ID。
        task: TaskId,
        /// 所有权字段。
        node: TaskNode,
    },
    /// 一个会话。
    Conversation(ConversationId),
    /// 一条尚未加载的所有者边。
    Unknown,
}

/// 对应 `Overlay`：一次提交暂存的候选记录，在所有权遍历中覆盖已提交记录。
#[derive(Debug, Clone, Default)]
pub struct Overlay {
    /// 暂存的任务记录。
    pub tasks: BTreeMap<TaskId, AnyTaskRecord>,
    /// 暂存的会话所有者边（`None` 表示无主）。
    pub edges: BTreeMap<ConversationId, Option<TaskId>>,
}

/// 对应 scheduler 的 `#node` / `#edge`：读取任务节点与所有者边。
///
/// `overlay` 里的候选优先于已提交状态；未加载的边返回 `None`。
pub trait OwnerLookup {
    /// 一个任务的节点；未加载时为 `None`。
    fn node(&self, id: TaskId, overlay: Option<&Overlay>) -> Option<TaskNode>;

    /// 一个会话的拥有者任务：`Some(None)` 表示无主，`None` 表示尚未加载。
    fn edge(&self, id: ConversationId, overlay: Option<&Overlay>) -> Option<Option<TaskId>>;
}

/// 对应 `Scope`：普通所有权遍历的起点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 一个会话。
    Conversation(ConversationId),
    /// 每一个无主会话。
    Roots,
}

/// 对应 `BlockedReason`：在某个注册表快照下无法保留一个待处理任务的原因（推导得出，从不持久化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedReason {
    /// `missing_task`。
    MissingTask,
    /// `task_too_old`。
    TaskTooOld,
    /// `migration_failed`。
    MigrationFailed,
}

/// 对应 `Fit`：一个定义能接手该记录，或没有定义能接手及其原因。
#[derive(Clone)]
pub enum Fit {
    /// 可接手；`migrates` 表示需要迁移。
    Ready {
        /// 定义。
        task: Arc<Task>,
        /// 是否需要迁移。
        migrates: bool,
    },
    /// 不可接手。
    Blocked {
        /// 原因。
        reason: BlockedReason,
    },
}

/// 对应 `cancellationIntent(record)`：一个活动拥有者的持久取消意图——它的 abort 标记，或一个非
/// `completed` 的保留结果。
pub fn cancellation_intent(record: &AnyTaskRecord) -> bool {
    record.state.status() != TaskStatus::Terminal
        && (record.abort_requested || failed_outcome(record))
}

/// 对应 `failedOutcome(record)`：记录保留或最终落定为非 `completed` 的结果。
pub fn failed_outcome(record: &AnyTaskRecord) -> bool {
    match &record.state {
        TaskState::Completing { outcome } | TaskState::Terminal { outcome } => {
            !matches!(outcome, crate::types::TaskOutcome::Completed { .. })
        }
        _ => false,
    }
}

/// 对应 `parentOf(node)`。
pub fn parent_of(node: &TaskNode) -> Up {
    match node.owner {
        Some(owner) => Up::Task(owner),
        None => Up::Conversation(node.conversation_id),
    }
}

/// 对应 `nodeOf(record)`。
pub fn node_of(record: &AnyTaskRecord) -> TaskNode {
    TaskNode {
        conversation_id: record.conversation_id,
        owner: record.owner.map(|owner| TaskId::new(owner.get())),
        background: record.background,
    }
}

/// 对应 `withState(record, state)`：替换活动记录的状态；结果一旦落定，memos 消失。
pub fn with_state(record: &AnyTaskRecord, state: TaskState<JsonValue, JsonValue>) -> AnyTaskRecord {
    let mut next = record.clone();
    match state.status() {
        TaskStatus::Terminal | TaskStatus::Completing => next.memos = None,
        _ => {}
    }
    next.state = state;
    next
}

/// 对应 `canReserve(task, record)`：定义能接手该任务——同版本，或更新的版本且带迁移。
pub fn can_reserve(task: &Task, record: &AnyTaskRecord) -> bool {
    let definition = task.definition();
    definition.version() == record.version
        || (definition.version() > record.version && definition.has_migrate())
}

/// 对应 `memoOf(record, name)`：只认自有 memo 条目。
pub fn memo_of<'a>(record: Option<&'a AnyTaskRecord>, name: &str) -> Option<&'a JsonValue> {
    record?.memos.as_ref()?.get(name)
}

/// 对应 `overlayOf(tx)`：本次提交暂存的候选记录。
pub fn overlay_of(tx: &Transaction) -> Overlay {
    Overlay {
        tasks: tx
            .staged_tasks()
            .into_iter()
            .map(|record| (TaskId::new(record.id.get()), record))
            .collect(),
        edges: tx
            .staged_conversations()
            .into_iter()
            .map(|record| (record.id, record.owner.map(|owner| owner.task_id)))
            .collect(),
    }
}

/// 对应 `jsonEqual(left, right)`：两个 JSON 值的结构相等；对象键顺序无关。
pub fn json_equal(left: Option<&JsonValue>, right: Option<&JsonValue>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => json_equal_value(left, right),
        _ => false,
    }
}

fn json_equal_value(left: &JsonValue, right: &JsonValue) -> bool {
    match (left, right) {
        (JsonValue::Null, JsonValue::Null) => true,
        (JsonValue::Bool(left), JsonValue::Bool(right)) => left == right,
        (JsonValue::Number(left), JsonValue::Number(right)) => left == right,
        (JsonValue::String(left), JsonValue::String(right)) => left == right,
        (JsonValue::Array(left), JsonValue::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_equal_value(left, right))
        }
        (JsonValue::Object(left), JsonValue::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|other| json_equal_value(value, other))
                })
        }
        _ => false,
    }
}

/// 对应 `#above(start, overlay)`：从 `start` 向上走：拥有者任务与会话，终止于无主根或尚未加载的边。
pub fn above(lookup: &dyn OwnerLookup, start: Up, overlay: Option<&Overlay>) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut at = Some(start);
    while let Some(current) = at {
        match current {
            Up::Task(task) => {
                let Some(node) = lookup.node(task, overlay) else {
                    steps.push(Step::Unknown);
                    return steps;
                };
                steps.push(Step::Task { task, node });
                at = Some(parent_of(&node));
            }
            Up::Conversation(conversation) => {
                steps.push(Step::Conversation(conversation));
                match lookup.edge(conversation, overlay) {
                    None => {
                        steps.push(Step::Unknown);
                        return steps;
                    }
                    Some(None) => at = None,
                    Some(Some(task)) => at = Some(Up::Task(task)),
                }
            }
        }
    }
    steps
}

/// 对应 `#chainKnown(start, overlay)`：`start` 之上的每条所有者边都已加载。
pub fn chain_known(lookup: &dyn OwnerLookup, start: Up, overlay: Option<&Overlay>) -> bool {
    !above(lookup, start, overlay)
        .iter()
        .any(|step| matches!(step, Step::Unknown))
}

/// 对应 `#liveRecords(overlay)`：活动记录，`overlay` 的候选替换已提交记录；终态候选不在其中。
pub fn live_records(
    live: &BTreeMap<TaskId, AnyTaskRecord>,
    overlay: Option<&Overlay>,
) -> Vec<AnyTaskRecord> {
    let mut records: Vec<AnyTaskRecord> = live
        .values()
        .filter(|record| {
            let candidate = overlay
                .and_then(|overlay| overlay.tasks.get(&TaskId::new(record.id.get())))
                .unwrap_or(record);
            candidate.state.status() != TaskStatus::Terminal
        })
        .cloned()
        .collect();
    if let Some(overlay) = overlay {
        for record in overlay.tasks.values() {
            if !live.contains_key(&TaskId::new(record.id.get()))
                && record.state.status() != TaskStatus::Terminal
            {
                records.push(record.clone());
            }
        }
    }
    records
}

/// 对应 `#ownedLive(overlay)`：每个还有活动普通自有工作的任务，映射到那份工作。
///
/// 每个活动非后台任务为它之上的**每一个**拥有者任务计数，直到并包含第一个后台任务为止。
/// 调用方必须已加载所有者链。
pub fn owned_live(
    lookup: &dyn OwnerLookup,
    live: &BTreeMap<TaskId, AnyTaskRecord>,
    overlay: Option<&Overlay>,
) -> BTreeMap<TaskId, Vec<TaskId>> {
    let mut owned: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
    for record in live_records(live, overlay) {
        if record.background {
            continue;
        }
        let node = node_of(&record);
        for step in above(lookup, parent_of(&node), overlay) {
            let Step::Task { task, node } = step else {
                match step {
                    Step::Unknown => break,
                    _ => continue,
                }
            };
            owned
                .entry(task)
                .or_default()
                .push(TaskId::new(record.id.get()));
            if node.background {
                break;
            }
        }
    }
    owned
}

/// 对应 `#inScope(start, scope, crossBackground)`：从 `scope` 出发的普通遍历能否到达 `start`。
///
/// 有边尚未加载时返回 `None`。
pub fn in_scope(
    lookup: &dyn OwnerLookup,
    start: Up,
    scope: Scope,
    cross_background: bool,
) -> Option<bool> {
    for step in above(lookup, start, None) {
        match step {
            Step::Unknown => return None,
            Step::Conversation(conversation) => {
                if let Scope::Conversation(expected) = scope
                    && conversation == expected
                {
                    return Some(true);
                }
            }
            Step::Task { node, .. } => {
                if node.background && !cross_background {
                    return Some(false);
                }
            }
        }
    }
    Some(matches!(scope, Scope::Roots))
}

/// 对应 `#belowCancelled(start)`：一个活动拥有者的取消意图能否到达 `start`。
///
/// 向上走会在遇到一个没有该意图的后台拥有者之前先遇到带意图的拥有者。终态拥有者从不级联（spec §5.4）。
pub fn below_cancelled(
    lookup: &dyn OwnerLookup,
    live: &BTreeMap<TaskId, AnyTaskRecord>,
    start: Up,
) -> bool {
    for step in above(lookup, start, None) {
        let Step::Task { task, node } = step else {
            match step {
                Step::Unknown => return false,
                _ => continue,
            }
        };
        if let Some(record) = live.get(&task)
            && cancellation_intent(record)
        {
            return true;
        }
        if node.background {
            return false;
        }
    }
    false
}

/// 对应 `#idle(conversationId)` 的记录侧判定：该范围内没有活动的非后台任务。
///
/// 所有者边尚未加载的任务算在范围内（即「不空闲」）。
pub fn idle(
    lookup: &dyn OwnerLookup,
    live: &BTreeMap<TaskId, AnyTaskRecord>,
    conversation_id: Option<ConversationId>,
) -> bool {
    let scope = match conversation_id {
        Some(id) => Scope::Conversation(id),
        None => Scope::Roots,
    };
    for record in live.values() {
        if record.background {
            continue;
        }
        let node = node_of(record);
        if in_scope(lookup, parent_of(&node), scope, false) != Some(false) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{TaskOutcome, TaskOutcomeError};
    use serde_json::json;

    fn conversation(id: u64) -> ConversationId {
        ConversationId::new(id)
    }

    fn task_id(id: u64) -> TaskId {
        TaskId::new(id)
    }

    fn record(
        id: u64,
        conversation_id: u64,
        owner: Option<u64>,
        background: bool,
        status: TaskStatus,
    ) -> AnyTaskRecord {
        let state = match status {
            TaskStatus::Pending => TaskState::Pending {
                checkpoint: json!({"phase": "a"}),
            },
            TaskStatus::Running => TaskState::Running {
                checkpoint: json!({"phase": "a"}),
            },
            TaskStatus::Waiting => TaskState::Waiting {
                checkpoint: json!({"phase": "a"}),
                on: Vec::new(),
                policy: crate::types::JoinPolicy::AllSettled,
            },
            TaskStatus::Completing => TaskState::Completing {
                outcome: TaskOutcome::Completed {
                    result: JsonValue::Null,
                },
            },
            TaskStatus::Terminal => TaskState::Terminal {
                outcome: TaskOutcome::Completed {
                    result: JsonValue::Null,
                },
            },
        };
        TaskRecord {
            id: TaskId::new(id),
            conversation_id: conversation(conversation_id),
            kind: "demo".to_string(),
            version: 1,
            input: JsonValue::Null,
            owner: owner.map(task_id),
            background,
            abort_requested: false,
            started_at: None,
            ended_at: None,
            state,
            memos: None,
        }
    }

    /// 一个简易的所有者查询：任务节点来自一张表，边来自另一张表。
    struct Lookup {
        nodes: BTreeMap<u64, TaskNode>,
        edges: BTreeMap<u64, Option<TaskId>>,
    }

    impl Lookup {
        fn new() -> Self {
            Self {
                nodes: BTreeMap::new(),
                edges: BTreeMap::new(),
            }
        }

        fn with_node(mut self, id: u64, node: TaskNode) -> Self {
            self.nodes.insert(id, node);
            self
        }

        fn with_edge(mut self, conversation: u64, owner: Option<TaskId>) -> Self {
            self.edges.insert(conversation, owner);
            self
        }
    }

    impl OwnerLookup for Lookup {
        fn node(&self, id: TaskId, overlay: Option<&Overlay>) -> Option<TaskNode> {
            if let Some(record) = overlay.and_then(|overlay| overlay.tasks.get(&id)) {
                return Some(node_of(record));
            }
            self.nodes.get(&id.get()).copied()
        }

        fn edge(&self, id: ConversationId, overlay: Option<&Overlay>) -> Option<Option<TaskId>> {
            if let Some(edge) = overlay.and_then(|overlay| overlay.edges.get(&id)) {
                return Some(*edge);
            }
            self.edges.get(&id.get()).copied()
        }
    }

    #[test]
    fn parent_of_follows_the_owner_then_the_conversation() {
        let owned = node_of(&record(1, 2, Some(9), false, TaskStatus::Running));
        assert_eq!(parent_of(&owned), Up::Task(task_id(9)));

        let root = node_of(&record(1, 2, None, false, TaskStatus::Running));
        assert_eq!(parent_of(&root), Up::Conversation(conversation(2)));
    }

    #[test]
    fn with_state_drops_memos_once_the_outcome_is_decided() {
        let mut source = record(1, 1, None, false, TaskStatus::Running);
        let mut memos = BTreeMap::new();
        memos.insert("k".to_string(), json!(1));
        source.memos = Some(memos);

        let running = with_state(
            &source,
            TaskState::Running {
                checkpoint: json!({"phase": "b"}),
            },
        );
        assert!(running.memos.is_some(), "运行中保留 memos");

        let completing = with_state(
            &source,
            TaskState::Completing {
                outcome: TaskOutcome::Completed {
                    result: JsonValue::Null,
                },
            },
        );
        assert!(completing.memos.is_none(), "结果已定即丢弃 memos");

        let terminal = with_state(
            &source,
            TaskState::Terminal {
                outcome: TaskOutcome::Completed {
                    result: JsonValue::Null,
                },
            },
        );
        assert!(terminal.memos.is_none());
        assert_eq!(terminal.kind, source.kind, "其余字段保留");
    }

    #[test]
    fn failed_outcome_covers_held_and_terminal_failures() {
        let mut completed = record(1, 1, None, false, TaskStatus::Completing);
        completed.state = TaskState::Completing {
            outcome: TaskOutcome::Completed {
                result: JsonValue::Null,
            },
        };
        assert!(!failed_outcome(&completed));

        completed.state = TaskState::Completing {
            outcome: TaskOutcome::Failed {
                error: TaskOutcomeError {
                    message: "boom".to_string(),
                    detail: None,
                },
                result: None,
            },
        };
        assert!(failed_outcome(&completed), "保留的失败算失败");

        completed.state = TaskState::Terminal {
            outcome: TaskOutcome::Aborted {
                reason: None,
                result: None,
            },
        };
        assert!(failed_outcome(&completed), "中止也是非 completed");

        let running = record(1, 1, None, false, TaskStatus::Running);
        assert!(!failed_outcome(&running), "运行中的记录没有结果");
    }

    #[test]
    fn cancellation_intent_needs_a_non_terminal_record() {
        let mut source = record(1, 1, None, false, TaskStatus::Running);
        assert!(!cancellation_intent(&source), "既无标记也无结果");

        source.abort_requested = true;
        assert!(cancellation_intent(&source));

        source.abort_requested = false;
        source.state = TaskState::Completing {
            outcome: TaskOutcome::Faulted {
                error: TaskOutcomeError {
                    message: "boom".to_string(),
                    detail: None,
                },
            },
        };
        assert!(cancellation_intent(&source), "保留的失败是取消意图");

        source.state = TaskState::Terminal {
            outcome: TaskOutcome::Faulted {
                error: TaskOutcomeError {
                    message: "boom".to_string(),
                    detail: None,
                },
            },
        };
        assert!(!cancellation_intent(&source), "终态拥有者从不级联");
    }

    #[test]
    fn above_walks_task_then_conversation_then_stops_at_an_ownerless_root() {
        let lookup = Lookup::new()
            .with_node(
                1,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: Some(task_id(3)),
                    background: false,
                },
            )
            .with_node(
                3,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: None,
                    background: false,
                },
            )
            .with_edge(2, None);

        let steps = above(&lookup, Up::Task(task_id(1)), None);
        assert_eq!(steps.len(), 3);
        assert!(matches!(steps[0], Step::Task { task, .. } if task == task_id(1)));
        assert!(matches!(steps[1], Step::Task { task, .. } if task == task_id(3)));
        assert_eq!(steps[2], Step::Conversation(conversation(2)));
        assert!(chain_known(&lookup, Up::Task(task_id(1)), None));
    }

    #[test]
    fn above_reports_an_unloaded_edge_as_unknown() {
        let lookup = Lookup::new().with_node(
            1,
            TaskNode {
                conversation_id: conversation(2),
                owner: None,
                background: false,
            },
        );
        let steps = above(&lookup, Up::Task(task_id(1)), None);
        assert_eq!(
            steps,
            vec![
                Step::Task {
                    task: task_id(1),
                    node: TaskNode {
                        conversation_id: conversation(2),
                        owner: None,
                        background: false,
                    }
                },
                Step::Conversation(conversation(2)),
                Step::Unknown,
            ]
        );
        assert!(!chain_known(&lookup, Up::Task(task_id(1)), None));
    }

    #[test]
    fn above_reads_the_overlay_before_the_committed_state() {
        let lookup = Lookup::new().with_node(
            1,
            TaskNode {
                conversation_id: conversation(2),
                owner: Some(task_id(3)),
                background: false,
            },
        );
        let mut overlay = Overlay::default();
        overlay
            .tasks
            .insert(task_id(1), record(1, 2, None, false, TaskStatus::Running));
        overlay.edges.insert(conversation(2), None);

        let steps = above(&lookup, Up::Task(task_id(1)), Some(&overlay));
        assert_eq!(steps.len(), 2, "覆盖后直接到会话");

        let lookup = lookups_are_independent();
        let _ = lookup;
    }

    #[test]
    fn owned_live_counts_each_live_task_for_every_owner_up_to_a_background_one() {
        let lookup = Lookup::new()
            .with_node(
                1,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: Some(task_id(3)),
                    background: false,
                },
            )
            .with_node(
                3,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: Some(task_id(5)),
                    background: true,
                },
            );
        let mut live = BTreeMap::new();
        live.insert(
            task_id(1),
            record(1, 2, Some(3), false, TaskStatus::Running),
        );
        live.insert(task_id(3), record(3, 2, Some(5), true, TaskStatus::Running));
        live.insert(task_id(5), record(5, 2, None, false, TaskStatus::Running));

        let owned = owned_live(&lookup, &live, None);
        assert_eq!(
            owned.get(&task_id(3)),
            Some(&vec![task_id(1)]),
            "后台拥有者仍计入它自己的那份工作"
        );
        assert!(
            !owned.contains_key(&task_id(5)),
            "遍历在第一个后台拥有者处停止"
        );
    }

    #[test]
    fn owned_live_ignores_background_tasks() {
        let lookup = Lookup::new().with_node(
            1,
            TaskNode {
                conversation_id: conversation(2),
                owner: Some(task_id(3)),
                background: false,
            },
        );
        let mut live = BTreeMap::new();
        live.insert(task_id(1), record(1, 2, Some(3), true, TaskStatus::Running));
        assert!(owned_live(&lookup, &live, None).is_empty());
    }

    #[test]
    fn in_scope_matches_the_conversation_and_stops_at_a_background_owner() {
        let lookup = Lookup::new()
            .with_node(
                1,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: Some(task_id(3)),
                    background: false,
                },
            )
            .with_node(
                3,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: None,
                    background: true,
                },
            )
            .with_edge(2, None)
            .with_edge(7, Some(task_id(3)));

        // 到达目标会话。
        assert_eq!(
            in_scope(
                &lookup,
                Up::Conversation(conversation(7)),
                Scope::Conversation(conversation(7)),
                false
            ),
            Some(true)
        );
        // 后台拥有者挡住到达根。
        assert_eq!(
            in_scope(&lookup, Up::Task(task_id(1)), Scope::Roots, false),
            Some(false)
        );
        assert_eq!(
            in_scope(&lookup, Up::Task(task_id(1)), Scope::Roots, true),
            Some(true),
            "crossBackground 穿过后台边界"
        );
        // 不匹配的会话且未到根。
        assert_eq!(
            in_scope(
                &lookup,
                Up::Task(task_id(1)),
                Scope::Conversation(conversation(99)),
                false
            ),
            Some(false)
        );
    }

    #[test]
    fn in_scope_is_unknown_while_an_edge_is_unloaded() {
        let lookup = Lookup::new().with_node(
            1,
            TaskNode {
                conversation_id: conversation(2),
                owner: None,
                background: false,
            },
        );
        assert_eq!(
            in_scope(&lookup, Up::Task(task_id(1)), Scope::Roots, false),
            None
        );
    }

    #[test]
    fn below_cancelled_walks_up_to_the_first_intent() {
        let lookup = Lookup::new()
            .with_node(
                1,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: Some(task_id(3)),
                    background: false,
                },
            )
            .with_node(
                3,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: None,
                    background: false,
                },
            )
            .with_edge(2, None);

        let mut live = BTreeMap::new();
        live.insert(
            task_id(1),
            record(1, 2, Some(3), false, TaskStatus::Running),
        );
        let mut owner = record(3, 2, None, false, TaskStatus::Running);
        owner.abort_requested = true;
        live.insert(task_id(3), owner);

        assert!(below_cancelled(&lookup, &live, Up::Task(task_id(1))));
    }

    #[test]
    fn below_cancelled_stops_at_a_background_owner_without_intent() {
        let lookup = Lookup::new()
            .with_node(
                1,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: Some(task_id(3)),
                    background: false,
                },
            )
            .with_node(
                3,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: Some(task_id(5)),
                    background: true,
                },
            )
            .with_node(
                5,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: None,
                    background: false,
                },
            )
            .with_edge(2, None);

        let mut live = BTreeMap::new();
        live.insert(
            task_id(1),
            record(1, 2, Some(3), false, TaskStatus::Running),
        );
        live.insert(task_id(3), record(3, 2, Some(5), true, TaskStatus::Running));
        let mut cancelled = record(5, 2, None, false, TaskStatus::Running);
        cancelled.abort_requested = true;
        live.insert(task_id(5), cancelled);

        assert!(
            !below_cancelled(&lookup, &live, Up::Task(task_id(1))),
            "后台拥有者截断级联"
        );
    }

    #[test]
    fn idle_ignores_background_tasks_and_unloaded_chains_count_as_inside() {
        let lookup = Lookup::new()
            .with_node(
                1,
                TaskNode {
                    conversation_id: conversation(2),
                    owner: None,
                    background: false,
                },
            )
            .with_edge(2, None);

        let mut live = BTreeMap::new();
        live.insert(task_id(1), record(1, 2, None, false, TaskStatus::Running));
        assert!(!idle(&lookup, &live, Some(conversation(2))));
        assert!(
            idle(&lookup, &live, Some(conversation(9))),
            "另一个会话空闲"
        );

        let mut background = BTreeMap::new();
        background.insert(task_id(1), record(1, 2, None, true, TaskStatus::Running));
        assert!(
            idle(&lookup, &background, Some(conversation(2))),
            "后台任务不影响空闲"
        );
    }

    #[test]
    fn memos_only_resolve_own_entries() {
        let mut source = record(1, 1, None, false, TaskStatus::Running);
        let mut memos = BTreeMap::new();
        memos.insert("k".to_string(), json!(7));
        source.memos = Some(memos);
        assert_eq!(memo_of(Some(&source), "k"), Some(&json!(7)));
        assert_eq!(memo_of(Some(&source), "missing"), None);
        assert_eq!(memo_of(None, "k"), None);
    }

    #[test]
    fn json_equal_ignores_object_key_order() {
        assert!(json_equal(
            Some(&json!({"a": 1, "b": [1, 2]})),
            Some(&json!({"b": [1, 2], "a": 1}))
        ));
        assert!(!json_equal(Some(&json!({"a": 1})), Some(&json!({"a": 2}))));
        assert!(!json_equal(Some(&json!([1])), Some(&json!([1, 2]))));
        assert!(json_equal(None, None));
        assert!(!json_equal(Some(&json!(null)), None));
    }

    #[test]
    fn can_reserve_follows_version_and_migration_rules() {
        struct Definition {
            version: u32,
            migrates: bool,
        }

        impl crate::types::TaskDefinitionSpec for Definition {
            fn name(&self) -> &str {
                "demo"
            }
            fn version(&self) -> u32 {
                self.version
            }
            fn initial(&self, _input: &JsonValue) -> JsonValue {
                JsonValue::Null
            }
            fn phases(&self) -> &[&'static str] {
                &["a"]
            }
            fn has_migrate(&self) -> bool {
                self.migrates
            }
        }

        let stored = record(1, 1, None, false, TaskStatus::Running);

        let same = Task::new(Arc::new(Definition {
            version: 1,
            migrates: false,
        }));
        assert!(can_reserve(&same, &stored));

        let newer_without = Task::new(Arc::new(Definition {
            version: 2,
            migrates: false,
        }));
        assert!(!can_reserve(&newer_without, &stored));

        let newer_with = Task::new(Arc::new(Definition {
            version: 2,
            migrates: true,
        }));
        assert!(can_reserve(&newer_with, &stored));

        let older = Task::new(Arc::new(Definition {
            version: 0,
            migrates: true,
        }));
        assert!(!can_reserve(&older, &stored), "更旧的定义不能接手");
    }

    fn lookups_are_independent() -> Lookup {
        Lookup::new()
    }
}
