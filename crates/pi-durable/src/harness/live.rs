//! 对应 `harness/live.ts`：内置的活动会话状态（运行控制与当前生成/工具轮的呈现）。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游的 `endRun` / `finishSlot` / `clearProgress` 直接改写 Proxy 草稿的子对象；Rust 无法返回
//!   可变的草稿子引用，因此改为「定位下标 + 按路径写入」，产生的 Op 与上游一致。
//! - `settleSchedulerOutcome` 依赖 `SchedulerOutcome`（scheduler.ts）与 `convertPartial`
//!   （generation.ts），随 P5f/P5g 一并落地。

use std::sync::Arc;
use std::sync::LazyLock;

use pi_ai::AssistantMessage;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::chord::delta::{DeltaError, Path, PathSegment};
use crate::documents::define_doc;
use crate::harness::generation::convert_partial;
use crate::harness::scheduler::exec::SchedulerOutcome;
use crate::harness::types::{CompactionReason, CompactionResult, ToolDiagnostic};
use crate::session::SessionError;
use crate::session::transaction::{AnyTaskRecord, Transaction};
use crate::types::{
    ConversationFork, ConversationHistory, DocAccess, DocDefinitionSpec, DocToken,
    DocumentSemantics, EntryId, JsonObject, SubmissionId, SubmissionSettlement, TaskId,
};

/// 对应 `ToolSlot.status`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolSlotStatus {
    /// 尚未开始（串行轮）或本次请求未提供该调用。
    Pending,
    /// 执行中。
    Running,
    /// 已结束。
    Done,
}

/// 对应 `ToolSlot`：当前轮一次工具调用的呈现。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSlot {
    /// 调用 ID。
    pub call_id: String,
    /// 工具名。
    pub name: String,
    /// 尚未开始（串行轮）与本次请求未提供的调用为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// 状态。
    pub status: ToolSlotStatus,
    /// 保留的运行输出与被界限丢掉的部分。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// 丢弃的字节数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
    /// 丢弃的行数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_lines: Option<u64>,
    /// 最后一次 `details()` 的值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<JsonValue>,
    /// 通过 `api.diagnostic()` 记录的诊断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
    /// 完成后的结果条目；工具任务 faulted / orphaned 时缺席。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
}

/// 对应 `CompactionStatus`：一个活动压缩任务的呈现。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStatus {
    /// 压缩任务。
    pub task_id: TaskId,
    /// 压缩原因。
    pub reason: CompactionReason,
    /// 是否有 generation 在等它（该压缩由该 generation 拥有）。
    pub blocking: bool,
    /// 尝试次数。
    pub attempt: u32,
    /// 下一次摘要尝试之前的持久退避。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryBackoff>,
}

/// 对应 `retry?: { at: number; error: string }`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryBackoff {
    /// 重试时刻。
    pub at: u64,
    /// 上次错误。
    pub error: String,
}

/// 对应 `LiveState.run`：运行控制——结算该运行输入的任务与那些输入。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRun {
    /// 结束该运行的任务。
    pub task_id: TaskId,
    /// 该任务结算的输入。
    pub inputs: Vec<SubmissionId>,
}

/// 对应 `LiveState.generation`：当前生成尝试的呈现。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveGeneration {
    /// 尝试次数。
    pub attempt: u32,
    /// 在途响应的已提交节流局部。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<AssistantMessage>,
    /// 下一次尝试之前的持久退避。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryBackoff>,
    /// 正在轮询的 provider 端延迟响应。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredPoll>,
}

/// 对应 `deferred?: { pollAt: number }`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredPoll {
    /// 下次轮询时刻。
    pub poll_at: u64,
}

/// 对应 `LiveState`：内置的活动会话状态。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LiveState {
    /// 恰好忙时出现。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<LiveRun>,
    /// 当前生成尝试的呈现。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<LiveGeneration>,
    /// 当前工具轮（按调用顺序）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSlot>>,
    /// 活动压缩任务（按任务 ID 顺序）；没有时缺席。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactions: Option<Vec<CompactionStatus>>,
}

struct LiveDefinition;

impl DocDefinitionSpec for LiveDefinition {
    fn kind(&self) -> &str {
        "pi.live"
    }

    fn version(&self) -> u32 {
        1
    }

    fn semantics(&self) -> DocumentSemantics {
        DocumentSemantics::Conversation {
            history: ConversationHistory::Latest,
            fork: ConversationFork::Initial,
        }
    }

    fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
        JsonObject::new()
    }

    fn checkpoint_when(
        &self,
        value: &JsonObject,
        _ops: &[crate::chord::delta::Op],
        _info: &crate::types::CheckpointInfo,
    ) -> bool {
        // 没有任何东西在运行时使用完整 base：没有 generation，也没有处于 running 的工具槽。
        //
        // 这一点在空闲时、在把 generation 交给工具轮的提交里、以及工具之间都成立，因此 delta 链
        // 最多跨一个 generation 或一轮工具的重叠执行。槽位只在 running 时保存输出，所以每个 base
        // 都很小。**不要**再加 delta 计数上界：工具输出的基准测试会检查这条规则。
        let state: LiveState =
            serde_json::from_value(JsonValue::Object(value.clone())).unwrap_or_default();
        let any_running = state.tools.as_ref().is_some_and(|slots| {
            slots
                .iter()
                .any(|slot| slot.status == ToolSlotStatus::Running)
        });
        state.generation.is_none() && !any_running
    }
}

/// 对应 `LiveDoc`：内置活动状态文档。
pub static LIVE_DOC: LazyLock<DocToken> =
    LazyLock::new(|| define_doc(Arc::new(LiveDefinition)).expect("pi.live"));

/// 对应 `RUN_TASK_KINDS`：能拥有 `pi.live.run` 的内置任务种类。
pub const RUN_TASK_KINDS: [&str; 1] = ["pi.generation"];

/// 对应 `TOOL_TASK_KIND`。
pub const TOOL_TASK_KIND: &str = "pi.tool";

/// 对应 `COMPACTION_TASK_KIND`。
pub const COMPACTION_TASK_KIND: &str = "pi.compaction";

/// 对应 `settleSchedulerOutcome`：调度器在不跑任务代码的情况下写下终态结果时，Harness 的清理。
///
/// 只处理内置任务种类，因此不会在其他地方创建 `pi.live`；调度器不关心任务种类，由 Harness 传入。
/// 工具任务只结束其槽位，压缩任务移除其状态，其余非运行种类被忽略。运行种类把 partial 转成 aborted
/// assistant 条目（与 generation 的 abort 处理器一致），再以 `faulted` / `orphaned` 结算其输入。
pub async fn settle_scheduler_outcome(
    tx: &Transaction,
    record: AnyTaskRecord,
    outcome: SchedulerOutcome,
) -> Result<(), SessionError> {
    let access = DocAccess {
        owner: Some(record.conversation_id.get()),
        key: None,
    };
    let task_id = TaskId::new(record.id.get());
    if record.kind == TOOL_TASK_KIND {
        let live = tx.doc(&*LIVE_DOC, access, None).await?;
        if let Some(slot) = tool_slot_index(&live, task_id) {
            finish_slot(&live, slot, None)?;
        }
        return Ok(());
    }
    if record.kind == COMPACTION_TASK_KIND {
        let live = tx.doc(&*LIVE_DOC, access, None).await?;
        remove_compaction_status(&live, task_id)?;
        return Ok(());
    }
    if !RUN_TASK_KINDS.contains(&record.kind.as_str()) {
        return Ok(());
    }
    let live = tx.doc(&*LIVE_DOC, access, None).await?;
    if live_state(&live).run.map(|run| run.task_id) != Some(task_id) {
        return Ok(());
    }
    convert_partial(tx, &live, record.conversation_id).await?;
    let settlement = match &outcome {
        SchedulerOutcome::Faulted { error } => SubmissionSettlement::Unanswered {
            reason: "faulted".to_string(),
            detail: Some(JsonValue::String(error.message.clone())),
        },
        SchedulerOutcome::Orphaned { reason } => SubmissionSettlement::Unanswered {
            reason: reason.clone(),
            detail: None,
        },
    };
    end_run(tx, &live, task_id, settlement)?;
    Ok(())
}

/// 对应 `endRun(tx, live, taskId, settlement)`：结束 `taskId` 拥有的运行。
///
/// 结算它的每个输入并移除 `run`。总是移除 `generation` 与 `tools`，它们的呈现属于正在结束的运行。
pub fn end_run(
    tx: &Transaction,
    live: &crate::types::Draft,
    task_id: TaskId,
    settlement: SubmissionSettlement,
) -> Result<(), SessionError> {
    let state = live_state(live);
    if let Some(run) = state.run
        && run.task_id == task_id
    {
        for id in run.inputs {
            tx.settle_submission(id, settlement.clone());
        }
        live.delete(path_of("run")).map_err(delta_error)?;
    }
    live.delete(path_of("generation")).map_err(delta_error)?;
    live.delete(path_of("tools")).map_err(delta_error)?;
    Ok(())
}

/// 对应 `addCompactionStatus(live, status)`：加入本次提交创建的压缩任务状态；状态按任务 ID 顺序。
pub fn add_compaction_status(
    live: &crate::types::Draft,
    status: &CompactionStatus,
) -> Result<(), SessionError> {
    let state = live_state(live);
    let existing = state.compactions.map(|items| items.len()).unwrap_or(0);
    if existing == 0 {
        live.set(path_of("compactions"), JsonValue::Array(Vec::new()))
            .map_err(delta_error)?;
    }
    let value = serde_json::to_value(status).expect("compaction status serialises");
    live.splice(path_of("compactions"), existing, 0, vec![value])
        .map_err(delta_error)?;
    Ok(())
}

/// 对应 `compactionStatus(live, taskId)`：压缩任务 `taskId` 的下标（若已列出）。
pub fn compaction_status_index(live: &crate::types::Draft, task_id: TaskId) -> Option<usize> {
    live_state(live)
        .compactions?
        .iter()
        .position(|status| status.task_id == task_id)
}

/// 对应 `removeCompactionStatus(live, taskId)`：移除压缩任务 `taskId` 的状态，并在列表为空时移除列表。
pub fn remove_compaction_status(
    live: &crate::types::Draft,
    task_id: TaskId,
) -> Result<(), SessionError> {
    let state = live_state(live);
    let Some(statuses) = state.compactions else {
        return Ok(());
    };
    let Some(index) = statuses.iter().position(|status| status.task_id == task_id) else {
        return Ok(());
    };
    live.splice(path_of("compactions"), index, 1, Vec::new())
        .map_err(delta_error)?;
    if statuses.len() == 1 {
        live.delete(path_of("compactions")).map_err(delta_error)?;
    }
    Ok(())
}

/// 对应 `toolSlot(live, taskId)`：当前轮里工具任务 `taskId` 的下标（若该轮仍列出它）。
pub fn tool_slot_index(live: &crate::types::Draft, task_id: TaskId) -> Option<usize> {
    live_state(live)
        .tools?
        .iter()
        .position(|slot| slot.task_id == Some(task_id))
}

/// 对应 `finishSlot(slot, entry)`：把槽位标记为完成。
///
/// 结果条目（若有）现在承载其运行输出、细节与诊断。
pub fn finish_slot(
    live: &crate::types::Draft,
    index: usize,
    entry: Option<EntryId>,
) -> Result<(), SessionError> {
    live.set(
        slot_path(index, "status"),
        JsonValue::String("done".to_string()),
    )
    .map_err(delta_error)?;
    if let Some(entry) = entry {
        live.set(slot_path(index, "entry"), JsonValue::from(entry.get()))
            .map_err(delta_error)?;
    }
    clear_progress(live, index)
}

/// 对应 `clearProgress(slot)`：移除工具在运行时发布的内容；其结果条目或重跑会替换它。
pub fn clear_progress(live: &crate::types::Draft, index: usize) -> Result<(), SessionError> {
    for key in [
        "output",
        "droppedBytes",
        "droppedLines",
        "details",
        "diagnostics",
    ] {
        live.delete(slot_path(index, key)).map_err(delta_error)?;
    }
    Ok(())
}

/// 读取 `pi.live` 的当前值（含未提交改动）。
pub fn live_state(live: &crate::types::Draft) -> LiveState {
    serde_json::from_value(live.value()).unwrap_or_default()
}

fn slot_path(index: usize, key: &str) -> Path {
    vec![
        PathSegment::Key("tools".to_string()),
        PathSegment::Index(index),
        PathSegment::Key(key.to_string()),
    ]
}

fn path_of(key: &str) -> Path {
    vec![PathSegment::Key(key.to_string())]
}

fn delta_error(error: DeltaError) -> SessionError {
    SessionError::Message(error.to_string())
}

/// 便于阅读：`LiveState.generation.message` 的存储形态（对应上游 `JsonRepresentation<AssistantMessage>`）。
#[allow(dead_code)]
type LiveMessage = AssistantMessage;

/// 便于阅读：`CompactionStatus.taskId` 携带的结果类型。
#[allow(dead_code)]
type CompactionTaskId = TaskId<CompactionResult>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_kind_and_default_state_match_upstream() {
        let definition = LIVE_DOC.definition();
        assert_eq!(definition.kind(), "pi.live");
        assert_eq!(definition.initial(None), JsonObject::new());
    }

    #[test]
    fn checkpoint_rule_needs_no_generation_and_no_running_slot() {
        let definition = LIVE_DOC.definition();
        let check = |value: JsonValue| {
            definition.checkpoint_when(
                value.as_object().unwrap(),
                &[],
                &crate::types::CheckpointInfo {
                    deltas_since_base: 9,
                },
            )
        };

        assert!(check(serde_json::json!({})), "空闲时是完整 base");
        assert!(
            !check(serde_json::json!({"generation": {"attempt": 1}})),
            "有生成时不是完整 base"
        );
        assert!(check(
            serde_json::json!({"tools": [{"callId": "a", "name": "x", "status": "done"}]})
        ));
        assert!(!check(
            serde_json::json!({"tools": [{"callId": "a", "name": "x", "status": "running"}]})
        ));
    }

    #[test]
    fn slots_round_trip_and_use_camel_case() {
        let slot = ToolSlot {
            call_id: "call-1".to_string(),
            name: "bash".to_string(),
            task_id: Some(TaskId::new(5)),
            status: ToolSlotStatus::Running,
            output: Some("hi".to_string()),
            dropped_bytes: Some(0),
            dropped_lines: None,
            details: None,
            diagnostics: None,
            entry: None,
        };
        let json = serde_json::to_value(&slot).unwrap();
        assert_eq!(json["callId"], serde_json::json!("call-1"));
        assert_eq!(json["taskId"], serde_json::json!(5));
        assert_eq!(json["status"], serde_json::json!("running"));
        assert!(json.get("droppedLines").is_none(), "缺席字段不序列化");
        assert_eq!(serde_json::from_value::<ToolSlot>(json).unwrap(), slot);
    }

    #[test]
    fn live_state_default_is_an_empty_object() {
        let json = serde_json::to_value(LiveState::default()).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn compaction_status_round_trips() {
        let status = CompactionStatus {
            task_id: TaskId::new(3),
            reason: CompactionReason::Threshold,
            blocking: true,
            attempt: 2,
            retry: Some(RetryBackoff {
                at: 1_700,
                error: "boom".to_string(),
            }),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["taskId"], serde_json::json!(3));
        assert_eq!(json["reason"], serde_json::json!("threshold"));
        assert_eq!(json["retry"]["error"], serde_json::json!("boom"));
        assert_eq!(
            serde_json::from_value::<CompactionStatus>(json).unwrap(),
            status
        );
    }

    #[test]
    fn task_kind_constants_match_upstream() {
        assert_eq!(RUN_TASK_KINDS, ["pi.generation"]);
        assert_eq!(TOOL_TASK_KIND, "pi.tool");
        assert_eq!(COMPACTION_TASK_KIND, "pi.compaction");
    }
}
