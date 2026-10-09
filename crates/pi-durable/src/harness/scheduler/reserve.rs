//! 对应 `scheduler.ts` 的预留决策：`#waitingOn` / `#resolve` / `#fit` / `#inspectTask`。
//!
//! 这些判定决定「一个待处理记录现在能不能被保留」：它等的活动任务、它能否被注册表里的某个定义接手、
//! 以及旧版本记录要不要迁移。上游把它们放在 `TaskScheduler` 的私有方法里，但除了 `#resolve` 的失败
//! 上报（写 `#failedMigrations` + `#report`）之外都是纯读，因此 Rust 侧单独成模块，
//! [`SchedulerState`] 直接作为状态来源，可用构造的记录单测。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游 `#resolve` 用 `try`/`catch` 包住迁移：`migrate` 抛错即 `migration_failed`。Rust 的定义接口
//!   是 `migrate(...) -> Option<(input, checkpoint)>`，无法抛错。`has_migrate()` 为真而 `migrate`
//!   返回 `None` 的矛盾定义也归为 `migration_failed`，但**不产生**上游那条异常文案（那本来也不存在）。
//! - 上游 `#fit` 用引用相等比较「失败迁移记住的定义」（`failed?.task === task`）；Rust 用 `Arc::ptr_eq`。
//! - `#resolve` 的 `this.#report(error)` 在这里变成 [`ResolutionOutcome::report`]，由执行层上报。
//! - `#inspectTask` 依赖 `#invocations`（执行层），因此它多收一个 `has_invocation` 参数。
//!   上游 `TaskInspectionState.Blocked` 里带的 `error` 字段在 P5b 落地时已确定不带（见 `harness/types.rs`）。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::harness::scheduler::ownership::{AnyTaskRecord, BlockedReason, Fit};
use crate::harness::scheduler::state::{FailedMigration, SchedulerState};
use crate::harness::types::{RegistrySnapshot, TaskBlockedReason, TaskInspectionState};
use crate::types::{Task, TaskId, TaskState, TaskStatus};

/// 对应 `Resolution`：一个可运行记录能被接手，或被阻塞。
// 与 pi-ai 的 `Message` 同理：为与上游结构一致不装箱（`Box`），因此允许变体大小差异。
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum Resolution {
    /// 可接手；`record` 可能是迁移后的版本。
    Ready {
        /// 定义。
        task: Arc<Task>,
        /// 接手用的记录（未迁移时就是输入记录）。
        record: AnyTaskRecord,
    },
    /// 不可接手。
    Blocked {
        /// 原因。
        reason: BlockedReason,
    },
}

/// 对应 `#resolve(record, snapshot)` 的结果，外加需要上报的错误。
pub struct ResolutionOutcome {
    /// 判定。
    pub resolution: Resolution,
    /// 需要上报给 host 的错误（上游在这里调用 `#report`）。
    pub report: Option<String>,
}

/// 便于阅读：记录在状态表里的键（`TaskId` 不带结果类型参数）。
fn key_of(record: &AnyTaskRecord) -> TaskId {
    TaskId::new(record.id.get())
}

/// 对应 `#waitingOn(record, owned)`：任务在下一次调用之前等待的活动任务。
///
/// 被 abort 标记的任务等它的**活动普通自有工作**（abort 自底向上跑），否则等它 `on` 里仍然活动的那部分。
pub fn waiting_on(
    state: &SchedulerState,
    record: &AnyTaskRecord,
    owned: &BTreeMap<TaskId, Vec<TaskId>>,
) -> Vec<TaskId> {
    if record.abort_requested {
        return owned.get(&key_of(record)).cloned().unwrap_or_default();
    }
    match &record.state {
        TaskState::Waiting { on, .. } => on
            .iter()
            .copied()
            .filter(|id| state.live.contains_key(id))
            .collect(),
        _ => Vec::new(),
    }
}

/// 对应 `#fit(record, task)`：一个定义能否接手这条记录。
pub fn fit(state: &SchedulerState, record: &AnyTaskRecord, task: Option<&Arc<Task>>) -> Fit {
    let Some(task) = task else {
        return Fit::Blocked {
            reason: BlockedReason::MissingTask,
        };
    };
    let version = task.definition().version();
    if version == record.version {
        return Fit::Ready {
            task: Arc::clone(task),
            migrates: false,
        };
    }
    if version < record.version {
        return Fit::Blocked {
            reason: BlockedReason::TaskTooOld,
        };
    }
    if let Some(failed) = state.failed_migrations.get(&key_of(record))
        && Arc::ptr_eq(&failed.task, task)
    {
        return Fit::Blocked {
            reason: BlockedReason::MigrationFailed,
        };
    }
    Fit::Ready {
        task: Arc::clone(task),
        migrates: true,
    }
}

/// 对应 `missingMigration(record, definition)`。
pub fn missing_migration(record: &AnyTaskRecord, task: &Arc<Task>) -> String {
    format!(
        "Task {} version {} has no migration from {}",
        record.kind,
        task.definition().version(),
        record.version
    )
}

/// 对应 `#resolve(record, snapshot)`：按 `kind` 找到定义，必要时迁移更旧的存储版本。
pub fn resolve(
    state: &mut SchedulerState,
    record: &AnyTaskRecord,
    snapshot: &RegistrySnapshot,
) -> ResolutionOutcome {
    let task = snapshot.task(&record.kind).cloned();
    let (task, migrates) = match fit(state, record, task.as_ref()) {
        Fit::Blocked { reason } => {
            return ResolutionOutcome {
                resolution: Resolution::Blocked { reason },
                report: None,
            };
        }
        Fit::Ready { task, migrates } => (task, migrates),
    };
    if !migrates {
        return ResolutionOutcome {
            resolution: Resolution::Ready {
                task,
                record: record.clone(),
            },
            report: None,
        };
    }
    let definition = task.definition();
    if !definition.has_migrate() {
        let error = missing_migration(record, &task);
        state.failed_migrations.insert(
            key_of(record),
            FailedMigration {
                task: Arc::clone(&task),
                error: error.clone(),
            },
        );
        return ResolutionOutcome {
            resolution: Resolution::Blocked {
                reason: BlockedReason::MigrationFailed,
            },
            report: Some(error),
        };
    }
    let checkpoint = checkpoint_of(&record.state)
        .cloned()
        .unwrap_or(JsonValue::Null);
    match definition.migrate(&record.input, &checkpoint, record.version) {
        Some((input, checkpoint)) => {
            let mut migrated = record.clone();
            migrated.version = definition.version();
            migrated.input = input;
            migrated.state = with_checkpoint(&record.state, checkpoint);
            ResolutionOutcome {
                resolution: Resolution::Ready {
                    task,
                    record: migrated,
                },
                report: None,
            }
        }
        None => ResolutionOutcome {
            resolution: Resolution::Blocked {
                reason: BlockedReason::MigrationFailed,
            },
            report: None,
        },
    }
}

/// 对应 `#inspectTask(record, snapshot, owned)`：任务在调度视角下的状态。
///
/// 不跑任何任务代码：待处理的迁移显示为带 `migrates` 的 `ready`，只有调度器**已经试过**的迁移
/// （或根本不可能存在的迁移）才显示为失败。
pub fn inspect_task(
    state: &SchedulerState,
    record: &AnyTaskRecord,
    snapshot: &RegistrySnapshot,
    owned: &BTreeMap<TaskId, Vec<TaskId>>,
    has_invocation: bool,
) -> TaskInspectionState {
    if has_invocation {
        return TaskInspectionState::Running;
    }
    if record.state.status() == TaskStatus::Completing {
        return TaskInspectionState::Completing;
    }
    let on = waiting_on(state, record, owned);
    if !on.is_empty() {
        return TaskInspectionState::Waiting { on };
    }
    match fit(state, record, snapshot.task(&record.kind)) {
        Fit::Blocked { reason } => TaskInspectionState::Blocked {
            reason: blocked_reason(reason),
        },
        Fit::Ready { task, migrates } => {
            if migrates && !task.definition().has_migrate() {
                return TaskInspectionState::Blocked {
                    reason: TaskBlockedReason::MigrationFailed,
                };
            }
            TaskInspectionState::Ready { migrates }
        }
    }
}

/// `BlockedReason` → `TaskBlockedReason`（两个枚举的取值一一对应）。
fn blocked_reason(reason: BlockedReason) -> TaskBlockedReason {
    match reason {
        BlockedReason::MissingTask => TaskBlockedReason::MissingTask,
        BlockedReason::TaskTooOld => TaskBlockedReason::TaskTooOld,
        BlockedReason::MigrationFailed => TaskBlockedReason::MigrationFailed,
    }
}

/// 可运行状态的 checkpoint（`pending` / `running` / `waiting` 都有）。
fn checkpoint_of(state: &TaskState<JsonValue, JsonValue>) -> Option<&JsonValue> {
    match state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => Some(checkpoint),
        TaskState::Completing { .. } | TaskState::Terminal { .. } => None,
    }
}

/// 对应迁移时 `{ ...record.state, checkpoint: migrated.checkpoint }`：只换 checkpoint，保留变体与其余字段。
fn with_checkpoint(
    state: &TaskState<JsonValue, JsonValue>,
    checkpoint: JsonValue,
) -> TaskState<JsonValue, JsonValue> {
    match state {
        TaskState::Pending { .. } => TaskState::Pending { checkpoint },
        TaskState::Running { .. } => TaskState::Running { checkpoint },
        TaskState::Waiting { on, policy, .. } => TaskState::Waiting {
            checkpoint,
            on: on.clone(),
            policy: *policy,
        },
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::scheduler::state::SchedulerState;
    use crate::types::{
        ConversationId, JoinPolicy, TaskDefinitionSpec, TaskOutcome, TaskRecord, TaskStatus,
    };
    use serde_json::json;

    struct Demo {
        version: u32,
        migrate: bool,
    }

    impl TaskDefinitionSpec for Demo {
        fn name(&self) -> &str {
            "demo"
        }

        fn version(&self) -> u32 {
            self.version
        }

        fn initial(&self, _input: &JsonValue) -> JsonValue {
            json!({"phase": "a"})
        }

        fn phases(&self) -> &[&'static str] {
            &["a"]
        }

        fn has_migrate(&self) -> bool {
            self.migrate
        }

        fn migrate(
            &self,
            _input: &JsonValue,
            _checkpoint: &JsonValue,
            _from_version: u32,
        ) -> Option<(JsonValue, JsonValue)> {
            if !self.migrate {
                return None;
            }
            Some((json!({"migrated": true}), json!({"phase": "b"})))
        }
    }

    fn demo_task(version: u32, migrate: bool) -> Arc<Task> {
        Arc::new(Task::new(Arc::new(Demo { version, migrate })))
    }

    fn snapshot(tasks: Vec<Arc<Task>>) -> RegistrySnapshot {
        RegistrySnapshot::new(Vec::new(), tasks)
    }

    fn record(id: u64, version: u32, status: TaskStatus) -> AnyTaskRecord {
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
            conversation_id: ConversationId::new(1),
            kind: "demo".to_string(),
            version,
            input: JsonValue::Null,
            owner: None,
            background: false,
            abort_requested: false,
            started_at: None,
            ended_at: None,
            state,
            memos: None,
        }
    }

    #[test]
    fn fit_blocks_a_missing_definition() {
        let state = SchedulerState::new();
        let record = record(1, 1, TaskStatus::Pending);
        assert!(matches!(
            fit(&state, &record, None),
            Fit::Blocked {
                reason: BlockedReason::MissingTask
            }
        ));
    }

    #[test]
    fn fit_accepts_the_exact_version_without_migrating() {
        let state = SchedulerState::new();
        let record = record(1, 2, TaskStatus::Pending);
        let Fit::Ready { migrates, .. } = fit(&state, &record, Some(&demo_task(2, true))) else {
            panic!("同版本应当可直接接手");
        };
        assert!(!migrates);
    }

    #[test]
    fn fit_rejects_an_older_definition() {
        let state = SchedulerState::new();
        let record = record(1, 5, TaskStatus::Pending);
        assert!(matches!(
            fit(&state, &record, Some(&demo_task(4, true))),
            Fit::Blocked {
                reason: BlockedReason::TaskTooOld
            }
        ));
    }

    #[test]
    fn fit_remembers_a_failed_migration_for_the_same_definition() {
        let mut state = SchedulerState::new();
        let record = record(1, 1, TaskStatus::Pending);
        let task = demo_task(2, true);

        assert!(matches!(
            fit(&state, &record, Some(&task)),
            Fit::Ready { migrates: true, .. }
        ));

        state.failed_migrations.insert(
            TaskId::new(1),
            FailedMigration {
                task: Arc::clone(&task),
                error: "boom".to_string(),
            },
        );
        assert!(matches!(
            fit(&state, &record, Some(&task)),
            Fit::Blocked {
                reason: BlockedReason::MigrationFailed
            }
        ));

        // 另一个定义对象不算「已试过」。
        assert!(matches!(
            fit(&state, &record, Some(&demo_task(2, true))),
            Fit::Ready { migrates: true, .. }
        ));
    }

    #[test]
    fn resolve_reports_a_missing_migration_and_remembers_it() {
        let mut state = SchedulerState::new();
        let record = record(1, 1, TaskStatus::Pending);
        let task = demo_task(2, false);
        let outcome = resolve(&mut state, &record, &snapshot(vec![Arc::clone(&task)]));

        assert!(matches!(
            outcome.resolution,
            Resolution::Blocked {
                reason: BlockedReason::MigrationFailed
            }
        ));
        assert_eq!(
            outcome.report.as_deref(),
            Some("Task demo version 2 has no migration from 1")
        );
        let failed = state
            .failed_migrations
            .get(&TaskId::new(1))
            .expect("已记住");
        assert!(Arc::ptr_eq(&failed.task, &task));
    }

    #[test]
    fn resolve_migrates_and_bumps_the_version() {
        let mut state = SchedulerState::new();
        let mut record = record(1, 1, TaskStatus::Running);
        record.input = json!({"old": true});
        let outcome = resolve(&mut state, &record, &snapshot(vec![demo_task(3, true)]));

        let Resolution::Ready {
            record: migrated, ..
        } = outcome.resolution
        else {
            panic!("应当可接手");
        };
        assert_eq!(migrated.version, 3);
        assert_eq!(migrated.input, json!({"migrated": true}));
        assert_eq!(
            checkpoint_of(&migrated.state),
            Some(&json!({"phase": "b"})),
            "迁移只替换 checkpoint，保留状态变体"
        );
        assert!(matches!(migrated.state, TaskState::Running { .. }));
        assert_eq!(outcome.report, None);
        assert!(state.failed_migrations.is_empty());
    }

    #[test]
    fn resolve_leaves_a_current_record_untouched() {
        let mut state = SchedulerState::new();
        let record = record(1, 3, TaskStatus::Pending);
        let outcome = resolve(&mut state, &record, &snapshot(vec![demo_task(3, true)]));
        let Resolution::Ready { record: same, .. } = outcome.resolution else {
            panic!("应当可接手");
        };
        assert_eq!(same, record);
    }

    #[test]
    fn waiting_on_returns_owned_work_for_an_abort_marked_task() {
        let state = SchedulerState::new();
        let mut record = record(1, 1, TaskStatus::Running);
        record.abort_requested = true;
        let mut owned = BTreeMap::new();
        owned.insert(TaskId::new(1), vec![TaskId::new(2), TaskId::new(3)]);

        assert_eq!(
            waiting_on(&state, &record, &owned),
            vec![TaskId::new(2), TaskId::new(3)],
            "abort 自底向上：先等自己的活动自有工作"
        );

        owned.clear();
        assert!(waiting_on(&state, &record, &owned).is_empty());
    }

    #[test]
    fn waiting_on_filters_to_live_dependencies() {
        let mut state = SchedulerState::new();
        state
            .live
            .insert(TaskId::new(2), record(2, 1, TaskStatus::Pending));
        let mut waiting = record(1, 1, TaskStatus::Pending);
        waiting.state = TaskState::Waiting {
            checkpoint: json!({"phase": "a"}),
            on: vec![TaskId::new(2), TaskId::new(3)],
            policy: JoinPolicy::AllSettled,
        };

        assert_eq!(
            waiting_on(&state, &waiting, &BTreeMap::new()),
            vec![TaskId::new(2)],
            "只有仍然活动的依赖还在等"
        );
    }

    #[test]
    fn inspect_task_prefers_running_then_completing_then_waiting() {
        let state = SchedulerState::new();
        let rec = record(1, 1, TaskStatus::Pending);
        let registry = snapshot(vec![demo_task(1, false)]);

        assert_eq!(
            inspect_task(&state, &rec, &registry, &BTreeMap::new(), true),
            TaskInspectionState::Running
        );

        let completing = record(2, 1, TaskStatus::Completing);
        assert_eq!(
            inspect_task(&state, &completing, &registry, &BTreeMap::new(), false),
            TaskInspectionState::Completing
        );

        let mut waiting = record(3, 1, TaskStatus::Pending);
        waiting.state = TaskState::Waiting {
            checkpoint: json!({"phase": "a"}),
            on: vec![TaskId::new(9)],
            policy: JoinPolicy::AllSettled,
        };
        let mut live = SchedulerState::new();
        live.live
            .insert(TaskId::new(9), record(9, 1, TaskStatus::Running));
        assert_eq!(
            inspect_task(&live, &waiting, &registry, &BTreeMap::new(), false),
            TaskInspectionState::Waiting {
                on: vec![TaskId::new(9)]
            }
        );

        // 定义缺失：报告阻塞原因（而不是崩溃）。
        assert_eq!(
            inspect_task(&state, &rec, &snapshot(vec![]), &BTreeMap::new(), false),
            TaskInspectionState::Blocked {
                reason: TaskBlockedReason::MissingTask
            }
        );
    }
}
