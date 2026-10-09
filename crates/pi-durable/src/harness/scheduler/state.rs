//! 对应 `scheduler.ts` 的状态表（`TaskScheduler` 的 `#live` / `#edges` / `#settled` / `#contexts` / 标志位）
//! 与它们的访问器。
//!
//! 上游把这些表放在 `TaskScheduler` 的私有字段里，`#node` / `#edge` / `#liveRecords` / `#ownedLive` /
//! `#inScope` / `#belowCancelled` / `#idle` 直接读它们。Rust 侧把表与纯访问器抽成 [`SchedulerState`]：
//! 它实现 [`OwnerLookup`]，于是 [`ownership`](crate::harness::scheduler::ownership) 里的遍历与派生函数
//! 可以直接作用在真实状态上，而 I/O 层（commit 监听、reconcile、reserve、执行）随后与之组合。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游用 `Map` / `Set`；Rust 用 `BTreeMap` / `BTreeSet`，遍历顺序因此是**确定**的（上游未定义顺序）。
//!   依赖顺序的下游只有 `#liveRecords` 的输出顺序，而它只被当作集合使用（`overlay` 候选追加在尾部）。
//! - `#invocations` / `#taskWaiters` / `#idleWaiters` 不在这里：它们携带 `Arc` 与取消信号，属于执行层。
//! - `#failedMigrations` 的 `error: unknown` 在 Rust 侧存为 `String`（只需要报告，不需要再抛出）。
//!
//! # 状态机主体仍然待落地
//!
//! 上游 `TaskScheduler` 的 I/O 方法（`open` / `resume` / `join` / `abort` / `waitForTask` / `waitForIdle` /
//! `abortConversation` / `#observe` / `#reconcile` / `#reserve` / `#run` / `#step` / `#runtime` 等，
//! 第 239–1340 行）尚未移植；本模块是它们的地基。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::harness::context::ContextRange;
use crate::harness::scheduler::ownership::{
    AnyTaskRecord, Overlay, OwnerLookup, Scope, Step, TaskNode, Up, above, below_cancelled,
    cancellation_intent, chain_known, failed_outcome, idle, in_scope, live_records, node_of,
    owned_live, parent_of,
};
use crate::types::{
    CommitChange, CommitPublication, ConversationId, JoinPolicy, SubmissionRecord,
    SubmissionStatus, TableCommitChange, Task, TaskId, TaskState, TaskStatus,
};

/// 对应 `#contexts` 的值：一个会话最后一次经由任务运行时读取的上下文区间，以及首次看到它空闲的时间。
#[derive(Debug, Clone)]
pub struct KeptContext {
    /// 最后一次读到的区间。
    pub range: ContextRange,
    /// 首次看到该会话空闲的 Harness 时间（毫秒）；有活动工作时为 `None`。
    pub idle_since: Option<u64>,
}

/// 对应 `#failedMigrations` 的值：迁移失败的定义，注册表解析出别的定义之前不再重试。
#[derive(Clone)]
pub struct FailedMigration {
    /// 当时匹配到的定义。
    pub task: Arc<Task>,
    /// 失败的描述（上游是 `unknown`，这里只用于上报）。
    pub error: String,
}

/// 对应 `TaskScheduler` 的持久状态镜像与派生的所有权表。
///
/// 上游不变式：`#live` 镜像每一个已提交的非终态任务记录，由同步的提交监听器在 Session 线上维护，
/// 因此跑在线上（on the line）的代码从它读到的正是已提交状态。本结构只承载这些表——维护它们的
/// 提交监听属于状态机主体。
#[derive(Default)]
pub struct SchedulerState {
    /// 对应 `#live`：每个已提交的非终态任务记录。
    pub live: BTreeMap<TaskId, AnyTaskRecord>,
    /// 对应 `#edges`：每个已加载会话的拥有者任务；`None` 表示该会话无主。
    pub edges: BTreeMap<ConversationId, Option<TaskId>>,
    /// 对应 `#conversationOwners`：拥有已加载会话的（可能是终态的）任务。
    pub conversation_owners: BTreeSet<TaskId>,
    /// 对应 `#settled`：遍历会经过的终态任务的所有权字段。
    pub settled: BTreeMap<TaskId, TaskNode>,
    /// 对应 `#failedMigrations`。
    pub failed_migrations: BTreeMap<TaskId, FailedMigration>,
    /// 对应 `#failFastChecks`：下一次 reconcile 要检查 `on` 里是否有失败任务的 `failFast` 等待者。
    pub fail_fast_checks: BTreeSet<TaskId>,
    /// 对应 `#contexts`：保留的上下文区间（只是缓存，丢弃是安全的）。
    pub contexts: BTreeMap<ConversationId, KeptContext>,
    /// 对应 `#reconcileScheduled`。
    pub reconcile_scheduled: bool,
    /// 对应 `#cascadePending`。
    pub cascade_pending: bool,
    /// 对应 `#enabled`。
    pub enabled: bool,
    /// 对应 `#closing`。
    pub closing: bool,
    /// 对应 `#dirty`。
    pub dirty: bool,
    /// 对应 `#draining`。
    pub draining: bool,
}

impl SchedulerState {
    /// 空状态（对应刚构造、尚未 `open` 的调度器）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 对应 `#setEdge(conversationId, owner)`：记录边的同时登记拥有该会话的任务。
    pub fn set_edge(&mut self, conversation_id: ConversationId, owner: Option<TaskId>) {
        self.edges.insert(conversation_id, owner);
        if let Some(owner) = owner {
            self.conversation_owners.insert(owner);
        }
    }

    /// 对应 `#node(id, overlay)`：先看提交暂存，再看活动记录，最后看终态表。
    pub fn task_node(&self, id: TaskId, overlay: Option<&Overlay>) -> Option<TaskNode> {
        <Self as OwnerLookup>::node(self, id, overlay)
    }

    /// 对应 `#above(start, overlay)`：从 `start` 向上的每一步。
    pub fn above(&self, start: Up, overlay: Option<&Overlay>) -> Vec<Step> {
        above(self, start, overlay)
    }

    /// 对应 `#chainKnown(start, overlay)`：`start` 之上的每条所有者边都已加载。
    pub fn chain_known(&self, start: Up, overlay: Option<&Overlay>) -> bool {
        chain_known(self, start, overlay)
    }

    /// 对应 `#liveRecords(overlay)`：活动记录，暂存候选替换已提交记录。
    pub fn live_records(&self, overlay: Option<&Overlay>) -> Vec<AnyTaskRecord> {
        live_records(&self.live, overlay)
    }

    /// 对应 `#ownedLive(overlay)`：每个还有活动普通自有工作的任务，映射到那份工作。
    pub fn owned_live(&self, overlay: Option<&Overlay>) -> BTreeMap<TaskId, Vec<TaskId>> {
        owned_live(self, &self.live, overlay)
    }

    /// 对应 `#inScope(start, scope, crossBackground)`。
    pub fn in_scope(&self, start: Up, scope: Scope, cross_background: bool) -> Option<bool> {
        in_scope(self, start, scope, cross_background)
    }

    /// 对应 `#belowCancelled(start)`：一个活动拥有者的取消意图能否到达 `start`。
    pub fn below_cancelled(&self, start: Up) -> bool {
        below_cancelled(self, &self.live, start)
    }

    /// 对应 `#idle(conversationId)`：该范围内（`None` 表示整个 Harness）没有活动的非后台任务。
    pub fn idle(&self, conversation_id: Option<ConversationId>) -> bool {
        idle(self, &self.live, conversation_id)
    }

    /// 对应 `#scheduleReconcile()` 的状态部分：`#reconcileScheduled` 的置位与抑制条件。
    ///
    /// 上游在这里还会排一个微任务；那一步由执行层根据 [`ObserveOutcome::reconcile`] 完成。
    fn schedule_reconcile(&mut self, outcome: &mut ObserveOutcome) {
        if self.reconcile_scheduled || self.closing {
            return;
        }
        self.reconcile_scheduled = true;
        outcome.reconcile = true;
    }

    /// 对应 `#kick()` 的状态部分：置 `#dirty`，并在可以推进时报告执行层。
    fn kick(&mut self, outcome: &mut ObserveOutcome) {
        self.dirty = true;
        if self.draining || !self.enabled || self.closing {
            return;
        }
        outcome.kick = true;
    }

    /// 对应 `#observe(publication)`：提交监听器在 Session 线上维护状态表。
    ///
    /// 上游在这里直接做副作用（排微任务、中止调用、结算等待者、`#settleIdle()`、`#kick()`）；
    /// Rust 把这些收集进 [`ObserveOutcome`]（状态表本身的改动已经应用），由执行层按序执行。
    /// 收集是安全的：上游这些副作用幂等（`#reconcileScheduled` / `#dirty` 都是标志）；
    /// 唯一有序的是终态记录与等待者的结算，它们按出现顺序排在 `settled` 里。
    pub fn observe(&mut self, publication: &CommitPublication) -> ObserveOutcome {
        let mut outcome = ObserveOutcome::default();
        let mut updated: Vec<AnyTaskRecord> = Vec::new();
        let mut failed: Vec<TaskId> = Vec::new();
        let mut changed = false;

        for change in &publication.changes {
            let CommitChange::Table(TableCommitChange::Task(record)) = change else {
                continue;
            };
            changed = true;
            let key = TaskId::new(record.id.get());
            let previous = self.live.get(&key).cloned();
            if failed_outcome(record)
                && previous
                    .as_ref()
                    .is_none_or(|before| !failed_outcome(before))
            {
                failed.push(key);
            }
            if record.state.status() == TaskStatus::Terminal {
                self.live.remove(&key);
                self.failed_migrations.remove(&key);
                self.fail_fast_checks.remove(&key);
                if self.conversation_owners.contains(&key) {
                    self.settled.insert(key, node_of(record));
                }
                outcome.settled.push(record.clone());
                // 它的拥有者现在可能可以落定。
                self.schedule_reconcile(&mut outcome);
                continue;
            }
            if record.abort_requested
                && previous
                    .as_ref()
                    .is_none_or(|before| !before.abort_requested)
            {
                self.cascade_pending = true;
                // 新标记的任务若正在运行，它的下一步会结束它。
                outcome.abort_runs.push(key);
            }
            let status = record.state.status();
            if status == TaskStatus::Completing
                && previous
                    .as_ref()
                    .is_none_or(|before| before.state.status() != TaskStatus::Completing)
            {
                if cancellation_intent(record) {
                    self.cascade_pending = true;
                }
                self.schedule_reconcile(&mut outcome);
            }
            if status == TaskStatus::Waiting
                && matches!(
                    record.state,
                    TaskState::Waiting {
                        policy: JoinPolicy::FailFast,
                        ..
                    }
                )
                && previous
                    .as_ref()
                    .is_none_or(|before| before.state.status() != TaskStatus::Waiting)
            {
                self.fail_fast_checks.insert(key);
                self.schedule_reconcile(&mut outcome);
            }
            self.live.insert(key, record.clone());
            updated.push(record.clone());
        }

        for id in &failed {
            let waiters: Vec<TaskId> = self
                .live
                .iter()
                .filter(|(_, record)| {
                    matches!(&record.state, TaskState::Waiting { policy: JoinPolicy::FailFast, on, .. } if on.contains(id))
                })
                .map(|(key, _)| *key)
                .collect();
            for key in waiters {
                self.fail_fast_checks.insert(key);
                self.schedule_reconcile(&mut outcome);
            }
        }

        for change in &publication.changes {
            if let CommitChange::Table(TableCommitChange::Conversation(record)) = change
                && !self.edges.contains_key(&record.id)
            {
                self.set_edge(record.id, record.owner.map(|owner| owner.task_id));
            }
        }

        for change in &publication.changes {
            // 位于已取消拥有者之下的排队输入会被撤回，即使级联已经跑过。
            let CommitChange::Table(TableCommitChange::Submission(record)) = change else {
                continue;
            };
            if record.status() != SubmissionStatus::Queued
                || !matches!(record, SubmissionRecord::Input { .. })
            {
                continue;
            }
            let up = Up::Conversation(record.identity().conversation_id);
            if !self.chain_known(up, None) || self.below_cancelled(up) {
                self.cascade_pending = true;
            }
        }

        for record in &updated {
            // 位于已取消拥有者之下新建的工作，即使级联已经跑过，也要中止。
            let up = parent_of(&node_of(record));
            if !self.chain_known(up, None) {
                self.schedule_reconcile(&mut outcome);
            } else if !record.background && !record.abort_requested && self.below_cancelled(up) {
                self.cascade_pending = true;
            }
        }

        // 提交失败的级联也会借下一次任意提交重试。
        if self.cascade_pending {
            self.schedule_reconcile(&mut outcome);
        }
        if !changed {
            return outcome;
        }
        outcome.settle_idle = true;
        self.kick(&mut outcome);
        outcome
    }
}

/// 对应 `#observe(publication)` 之后需要执行层做的事（状态表本身的改动已由 [`SchedulerState::observe`] 应用）。
#[derive(Debug, Default, PartialEq)]
pub struct ObserveOutcome {
    /// 需要中止运行调用的任务（本次提交里新出现的 abort 标记）。
    pub abort_runs: Vec<TaskId>,
    /// 已终态、需要结算等待者的记录（按提交里的出现顺序）。
    pub settled: Vec<AnyTaskRecord>,
    /// 至少请求了一次 reconcile。
    pub reconcile: bool,
    /// 需要结算空闲等待者（仅当本次提交动了任务表）。
    pub settle_idle: bool,
    /// 需要推进调度（对应 `#kick()` 启动 `#drain()` 的那一半）。
    pub kick: bool,
}

impl OwnerLookup for SchedulerState {
    fn node(&self, id: TaskId, overlay: Option<&Overlay>) -> Option<TaskNode> {
        if let Some(record) = overlay.and_then(|overlay| overlay.tasks.get(&id)) {
            return Some(node_of(record));
        }
        self.live
            .get(&id)
            .map(node_of)
            .or_else(|| self.settled.get(&id).copied())
    }

    fn edge(&self, id: ConversationId, overlay: Option<&Overlay>) -> Option<Option<TaskId>> {
        if let Some(owner) = overlay.and_then(|overlay| overlay.edges.get(&id)) {
            return Some(*owner);
        }
        self.edges.get(&id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CommitChange, CommitPublication, ConversationOwner, ConversationRecord,
        InputSubmissionStatus, JoinPolicy, Seq, SubmissionId, SubmissionIdentity, TaskOutcome,
        TaskRecord, TaskState, TaskStatus,
    };
    use serde_json::{Value as JsonValue, json};

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
        abort_requested: bool,
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
                policy: JoinPolicy::AllSettled,
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
            abort_requested,
            started_at: None,
            ended_at: None,
            state,
            memos: None,
        }
    }

    /// 会话 1 无主；任务 1 无主会话 1 之下、任务 2 属于任务 1、任务 3 属于任务 2。
    fn chain() -> SchedulerState {
        let mut state = SchedulerState::new();
        state.set_edge(conversation(1), None);
        state.live.insert(
            task_id(1),
            record(1, 1, None, false, TaskStatus::Running, false),
        );
        state.live.insert(
            task_id(2),
            record(2, 1, Some(1), false, TaskStatus::Running, false),
        );
        state.live.insert(
            task_id(3),
            record(3, 1, Some(2), false, TaskStatus::Running, false),
        );
        state
    }

    #[test]
    fn node_prefers_overlay_then_live_then_settled() {
        let mut state = chain();
        state.settled.insert(
            task_id(9),
            TaskNode {
                conversation_id: conversation(1),
                owner: Some(task_id(1)),
                background: false,
            },
        );

        // live
        assert_eq!(
            state.task_node(task_id(2), None).expect("live 节点"),
            TaskNode {
                conversation_id: conversation(1),
                owner: Some(task_id(1)),
                background: false,
            }
        );
        // 已经终态、只留在 #settled 的任务
        assert_eq!(
            state
                .task_node(task_id(9), None)
                .expect("settled 节点")
                .owner,
            Some(task_id(1))
        );
        // 未加载
        assert_eq!(state.task_node(task_id(42), None), None);

        // overlay 覆盖已提交记录
        let mut overlay = Overlay::default();
        overlay.tasks.insert(
            task_id(2),
            record(2, 1, None, true, TaskStatus::Pending, false),
        );
        let node = state
            .task_node(task_id(2), Some(&overlay))
            .expect("overlay 节点");
        assert_eq!(node.owner, None);
        assert!(node.background, "暂存候选优先于 #live");
    }

    #[test]
    fn edge_prefers_overlay_then_committed() {
        let mut state = SchedulerState::new();
        state.set_edge(conversation(1), None);
        state.set_edge(conversation(2), Some(task_id(7)));

        assert_eq!(
            <SchedulerState as OwnerLookup>::edge(&state, conversation(1), None),
            Some(None),
            "无主会话是 Some(None)，而不是「未加载」"
        );
        assert_eq!(
            <SchedulerState as OwnerLookup>::edge(&state, conversation(2), None),
            Some(Some(task_id(7)))
        );
        assert_eq!(
            <SchedulerState as OwnerLookup>::edge(&state, conversation(3), None),
            None,
            "未加载的边"
        );

        let mut overlay = Overlay::default();
        overlay.edges.insert(conversation(1), Some(task_id(7)));
        assert_eq!(
            <SchedulerState as OwnerLookup>::edge(&state, conversation(1), Some(&overlay)),
            Some(Some(task_id(7)))
        );
    }

    #[test]
    fn set_edge_registers_the_owning_task_only_for_owned_conversations() {
        let mut state = SchedulerState::new();
        state.set_edge(conversation(1), Some(task_id(5)));
        state.set_edge(conversation(2), None);
        assert!(state.conversation_owners.contains(&task_id(5)));
        assert_eq!(state.conversation_owners.len(), 1, "无主会话不登记拥有者");
    }

    #[test]
    fn owned_live_counts_every_owner_up_to_the_first_background_one() {
        let state = chain();
        let owned = state.owned_live(None);
        assert_eq!(owned.get(&task_id(1)), Some(&vec![task_id(2), task_id(3)]));
        assert_eq!(owned.get(&task_id(2)), Some(&vec![task_id(3)]));
        assert_eq!(owned.get(&task_id(3)), None, "任务 3 没有下属");

        // 任务 1 改成后台：链在它那里停住，任务 2 / 3 仍计入它。
        let mut backgrounded = chain();
        backgrounded.live.insert(
            task_id(1),
            record(1, 1, None, true, TaskStatus::Running, false),
        );
        let owned = backgrounded.owned_live(None);
        assert_eq!(owned.get(&task_id(1)), Some(&vec![task_id(2), task_id(3)]));
        assert_eq!(owned.get(&task_id(2)), Some(&vec![task_id(3)]));
    }

    #[test]
    fn a_settled_task_leaves_owned_live() {
        // `#live` 的不变式只含非终态记录：终态任务由提交监听器移除。
        let mut state = chain();
        state.live.remove(&task_id(3));
        state.settled.insert(
            task_id(3),
            TaskNode {
                conversation_id: conversation(1),
                owner: Some(task_id(2)),
                background: false,
            },
        );
        let owned = state.owned_live(None);
        assert_eq!(owned.get(&task_id(2)), None, "终态任务不再为拥有者计数");
        assert_eq!(owned.get(&task_id(1)), Some(&vec![task_id(2)]));
    }

    #[test]
    fn in_scope_walks_to_the_conversation_and_stops_at_background_owners() {
        let state = chain();
        assert_eq!(
            state.in_scope(
                Up::Task(task_id(3)),
                Scope::Conversation(conversation(1)),
                false
            ),
            Some(true)
        );
        assert_eq!(
            state.in_scope(
                Up::Task(task_id(3)),
                Scope::Conversation(conversation(2)),
                false
            ),
            Some(false)
        );
        assert_eq!(
            state.in_scope(Up::Task(task_id(3)), Scope::Roots, false),
            Some(true),
            "根范围：走到无主会话即为命中"
        );

        // 任务 1 变成后台：跨后台时仍算在范围内，不跨时不算。
        let mut backgrounded = chain();
        backgrounded.live.insert(
            task_id(1),
            record(1, 1, None, true, TaskStatus::Running, false),
        );
        assert_eq!(
            backgrounded.in_scope(
                Up::Task(task_id(3)),
                Scope::Conversation(conversation(1)),
                false
            ),
            Some(false)
        );
        assert_eq!(
            backgrounded.in_scope(
                Up::Task(task_id(3)),
                Scope::Conversation(conversation(1)),
                true
            ),
            Some(true)
        );

        // 边未加载：走到被问的会话之前就断了，结论未知。
        let mut unloaded = chain();
        unloaded.edges.clear();
        assert_eq!(
            unloaded.in_scope(
                Up::Task(task_id(3)),
                Scope::Conversation(conversation(2)),
                false
            ),
            None
        );
        assert_eq!(
            unloaded.in_scope(
                Up::Task(task_id(3)),
                Scope::Conversation(conversation(1)),
                false
            ),
            Some(true),
            "会话步骤在未知的边之前抵达，因此仍能命中"
        );
    }

    #[test]
    fn below_cancelled_finds_the_nearest_abort_marked_owner() {
        let state = chain();
        assert!(!state.below_cancelled(Up::Task(task_id(3))));

        let mut marked = chain();
        marked.live.insert(
            task_id(1),
            record(1, 1, None, false, TaskStatus::Running, true),
        );
        assert!(
            marked.below_cancelled(Up::Task(task_id(3))),
            "任务 1 的 abort 标记沿所有权树向下到达任务 3"
        );

        // 后台拥有者挡住继续向上：它自己没有被标记就不再上溯。
        let mut blocked = chain();
        blocked.live.insert(
            task_id(1),
            record(1, 1, None, true, TaskStatus::Running, true),
        );
        blocked.live.insert(
            task_id(2),
            record(2, 1, Some(1), true, TaskStatus::Running, false),
        );
        assert!(!blocked.below_cancelled(Up::Task(task_id(3))));
    }

    #[test]
    fn idle_uses_the_scope_of_each_live_task() {
        let mut state = chain();
        state.set_edge(conversation(2), None);
        assert!(!state.idle(Some(conversation(1))), "会话 1 里有活动任务");
        assert!(state.idle(Some(conversation(2))), "会话 2 里没有任务");

        // 全部落定后整个 Harness 空闲（终态任务离开 `#live`）。
        for id in [1_u64, 2, 3] {
            state.live.remove(&task_id(id));
        }
        assert!(state.idle(None));
    }

    // ─── #observe ────────────────────────────────────────────────

    fn publication(seq: u64, changes: Vec<CommitChange>) -> CommitPublication {
        CommitPublication {
            seq: Seq::new(seq),
            changes,
        }
    }

    fn task_change(record: AnyTaskRecord) -> CommitChange {
        CommitChange::Table(TableCommitChange::Task(record))
    }

    /// 一条已经终态、以 `aborted` 落定的记录（非 `completed`，因此算「失败」）。
    fn aborted_record(id: u64, conversation_id: u64, owner: Option<u64>) -> AnyTaskRecord {
        let mut record = record(
            id,
            conversation_id,
            owner,
            false,
            TaskStatus::Pending,
            false,
        );
        record.state = TaskState::Terminal {
            outcome: TaskOutcome::Aborted {
                reason: Some("stopped".to_string()),
                result: None,
            },
        };
        record
    }

    fn waiting_fail_fast(id: u64, conversation_id: u64, on: Vec<u64>) -> AnyTaskRecord {
        let mut record = record(id, conversation_id, None, false, TaskStatus::Pending, false);
        record.state = TaskState::Waiting {
            checkpoint: json!({"phase": "wait"}),
            on: on.into_iter().map(task_id).collect(),
            policy: JoinPolicy::FailFast,
        };
        record
    }

    fn conversation_change(id: u64, owner: Option<u64>) -> CommitChange {
        CommitChange::Table(TableCommitChange::Conversation(ConversationRecord {
            id: conversation(id),
            parent: None,
            owner: owner.map(|task| ConversationOwner {
                conversation_id: conversation(id),
                task_id: task_id(task),
            }),
        }))
    }

    fn queued_input_change(id: u64, conversation_id: u64) -> CommitChange {
        CommitChange::Table(TableCommitChange::Submission(SubmissionRecord::Input {
            identity: SubmissionIdentity {
                id: SubmissionId::new(id),
                conversation_id: conversation(conversation_id),
                request_id: None,
            },
            status: InputSubmissionStatus::Queued,
        }))
    }

    #[test]
    fn observe_mirrors_task_records_and_reports_progress() {
        let mut state = SchedulerState::new();
        state.enabled = true;

        // 新任务：进入 `#live`；结束时要求结算空闲与推进调度。
        let outcome = state.observe(&publication(
            1,
            vec![task_change(record(
                1,
                1,
                None,
                false,
                TaskStatus::Running,
                false,
            ))],
        ));
        assert!(state.live.contains_key(&task_id(1)));
        assert!(outcome.settle_idle, "任务表动了就要结算空闲");
        assert!(outcome.kick, "启用中的调度器会被推进");
        assert!(
            outcome.reconcile,
            "所有权链尚未加载，因此请求一次 reconcile"
        );
        // 已经排程时不再重复请求（对应 `#scheduleReconcile()` 的头一个判断）。
        assert!(
            !state
                .observe(&publication(
                    2,
                    vec![task_change(record(
                        1,
                        1,
                        None,
                        false,
                        TaskStatus::Running,
                        false,
                    ))],
                ))
                .reconcile
        );

        // 执行层跑完那次 reconcile。
        state.reconcile_scheduled = false;

        // 终态：离开 `#live`，结算等待者，并要求 reconcile（它的拥有者可能可以落定）。
        let outcome = state.observe(&publication(
            3,
            vec![task_change(aborted_record(1, 1, None))],
        ));
        assert!(!state.live.contains_key(&task_id(1)));
        assert_eq!(outcome.settled.len(), 1);
        assert_eq!(outcome.settled[0].id.get(), 1);
        assert!(outcome.reconcile);
    }

    #[test]
    fn a_terminal_conversation_owner_stays_in_settled() {
        let mut state = SchedulerState::new();
        state.enabled = true;
        state.set_edge(conversation(1), Some(task_id(1)));

        state.observe(&publication(
            1,
            vec![task_change(aborted_record(1, 1, None))],
        ));
        assert!(
            state.settled.contains_key(&task_id(1)),
            "拥有过会话的终态任务留在 `#settled`，供遍历经过"
        );
    }

    #[test]
    fn observe_signals_a_run_to_abort_on_a_new_abort_mark() {
        let mut state = SchedulerState::new();
        state.enabled = true;
        let running = record(1, 1, None, false, TaskStatus::Running, false);
        state.observe(&publication(1, vec![task_change(running.clone())]));
        assert!(!state.cascade_pending);

        let mut marked = running.clone();
        marked.abort_requested = true;
        let outcome = state.observe(&publication(2, vec![task_change(marked.clone())]));
        assert_eq!(outcome.abort_runs, vec![task_id(1)], "运行调用会被信号中止");
        assert!(state.cascade_pending);

        // 同一个已标记记录再次提交：不再重复上报。
        let outcome = state.observe(&publication(3, vec![task_change(marked)]));
        assert!(outcome.abort_runs.is_empty());
    }

    #[test]
    fn observe_registers_fail_fast_waiters_of_a_newly_failed_task() {
        let mut state = SchedulerState::new();
        state.enabled = true;
        state
            .live
            .insert(task_id(2), waiting_fail_fast(2, 1, vec![1]));

        let outcome = state.observe(&publication(
            1,
            vec![task_change(aborted_record(1, 1, None))],
        ));
        assert!(
            state.fail_fast_checks.contains(&task_id(2)),
            "等 `on` 里失败任务的 failFast 等待者会被登记"
        );
        assert!(outcome.reconcile);
    }

    #[test]
    fn observe_registers_an_edge_once() {
        let mut state = SchedulerState::new();
        let outcome = state.observe(&publication(1, vec![conversation_change(1, None)]));
        assert_eq!(state.edges.get(&conversation(1)), Some(&None));
        assert!(!outcome.kick, "只有会话变更时不推进调度");
        assert!(!outcome.settle_idle);

        // 已经加载过的边不会被后来的提交覆盖。
        state.observe(&publication(2, vec![conversation_change(1, Some(7))]));
        assert_eq!(state.edges.get(&conversation(1)), Some(&None));
    }

    #[test]
    fn observe_flags_a_queued_input_below_a_cancelled_owner() {
        let mut state = SchedulerState::new();
        state.enabled = true;
        // 任务 1 拥有会话 1，而它自己跑在会话 2 里（否则所有权链会自引用成环）。
        state.set_edge(conversation(1), Some(task_id(1)));
        state.set_edge(conversation(2), None);
        state.live.insert(
            task_id(1),
            record(1, 2, None, false, TaskStatus::Running, true),
        );

        let outcome = state.observe(&publication(1, vec![queued_input_change(1, 1)]));
        assert!(state.cascade_pending, "已取消拥有者之下的排队输入");
        assert!(outcome.reconcile);

        // 拥有者没有取消意图时不动。
        let mut healthy = SchedulerState::new();
        healthy.set_edge(conversation(1), Some(task_id(1)));
        healthy.set_edge(conversation(2), None);
        healthy.live.insert(
            task_id(1),
            record(1, 2, None, false, TaskStatus::Running, false),
        );
        healthy.observe(&publication(1, vec![queued_input_change(1, 1)]));
        assert!(!healthy.cascade_pending);
    }
}
