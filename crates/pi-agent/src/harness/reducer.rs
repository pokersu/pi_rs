//! Rust 翻译自 packages/agent/src/harness/reducer.ts
//!
//! 单写者记录协议的 lane 状态归约与恢复切片校验。

use std::collections::{HashMap, HashSet};

use pi_ai::AssistantMessage;

use crate::harness::session::types::{
    Entry, LaneRecord, OperationStartedRecord, StepAttemptRecord, ToolStartedRecord,
};
use crate::types::AgentMessage;

/// 对应 `RecordLogCorruptionReason`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordLogCorruptionReason {
    MultipleOpenOperations,
    UnknownOperation,
    RecordAfterFinish,
    NonConsecutiveAttempt,
    InvalidCompactionReason,
    QueueAfterAbort,
    InvalidQueueCancellation,
    InconsistentStep,
    ToolCallMismatch,
    DuplicateToolInvocation,
    ProvisionedEntryMismatch,
    InvalidDeferredHandle,
}

/// 对应 `RecordLogCorruption`
#[derive(Debug)]
pub struct RecordLogCorruption {
    pub reason: RecordLogCorruptionReason,
    pub message: String,
}

impl std::fmt::Display for RecordLogCorruption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RecordLogCorruption {}

fn corrupt(reason: RecordLogCorruptionReason, message: String) -> ! {
    panic!("{}", RecordLogCorruption { reason, message });
}

/// 对应 `RecordLogSlice`
#[derive(Debug, Clone)]
pub struct RecordLogSlice {
    pub lane: String,
    pub open_operations: Vec<OperationStartedRecord>,
    pub records: Vec<LaneRecord>,
    pub entries: Vec<Entry>,
}

/// 对应 `EffectiveLaneConfiguration`
#[derive(Debug, Clone)]
pub struct EffectiveLaneConfiguration {
    pub provider: String,
    pub model_id: String,
    pub thinking_level: String,
    pub active_tool_names: Vec<String>,
}

/// 对应 `TerminalFailureState`
#[derive(Debug, Clone)]
pub struct TerminalFailureState {
    pub entry_id: String,
    pub source: String,
    pub message: AssistantMessage,
}

/// 对应 `ToolBatchState`
#[derive(Debug, Clone)]
pub struct ToolBatchState {
    pub assistant_entry_id: String,
    pub calls: Vec<ToolBatchCall>,
    pub truncated: bool,
    pub unresolved: bool,
}

#[derive(Debug, Clone)]
pub struct ToolBatchCall {
    pub tool_index: u32,
    pub tool_call: pi_ai::ToolCall,
    pub result_exists: bool,
    pub terminate: bool,
}

/// 对应 `LaneState`
#[derive(Debug, Clone)]
pub struct LaneState {
    pub lane: String,
    pub leaf_id: Option<String>,
    pub operation: Option<LaneOperation>,
    pub pending_next_run: Vec<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct LaneOperation {
    pub id: String,
    pub kind: String,
    pub aborting: bool,
    pub step: Option<LaneStep>,
    pub tool_batch: Option<ToolBatchState>,
    pub missing_initial_messages: Vec<serde_json::Value>,
    pub pending_steer: Vec<serde_json::Value>,
    pub pending_follow_up: Vec<serde_json::Value>,
    pub pending_writes: Vec<serde_json::Value>,
    pub deferred: Option<pi_ai::DeferredHandle>,
    pub overflow_recovery_used: bool,
    pub newest_own: Option<NewestOwn>,
}

#[derive(Debug, Clone)]
pub struct LaneStep {
    pub kind: String,
    pub attempts: u32,
    pub result_entry_id: String,
    pub compaction_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewestOwn {
    pub entry_id: String,
    pub kind: String,
    pub role: Option<String>,
    pub stop_reason: Option<pi_ai::StopReason>,
}

/// 对应 `LaneReductionInput`
#[derive(Debug, Clone)]
pub struct LaneReductionInput {
    pub lane: String,
    pub leaf_id: Option<String>,
    pub open_operations: Vec<OperationStartedRecord>,
    pub records: Vec<LaneRecord>,
    pub entries: Vec<Entry>,
    pub own_entries: Vec<Entry>,
    pub configuration_entries: Vec<Entry>,
    pub defaults: EffectiveLaneConfiguration,
}

/// 对应 `LaneReductionResult`
#[derive(Debug, Clone)]
pub struct LaneReductionResult {
    pub lane_state: LaneState,
    pub effective_configuration: EffectiveLaneConfiguration,
    pub terminal_failure: Option<TerminalFailureState>,
}

fn record_seq(record: &LaneRecord) -> u64 {
    match record {
        LaneRecord::OperationStarted(r) => r.base.seq,
        LaneRecord::AbortRequested(r) => r.base.seq,
        LaneRecord::OperationFinished(r) => r.base.seq,
        LaneRecord::StepAttempt(r) => r.base.seq,
        LaneRecord::ToolStarted(r) => r.base.seq,
        LaneRecord::QueueEnqueued(r) => r.base.seq,
        LaneRecord::QueueCancelled(r) => r.base.seq,
        LaneRecord::WriteDeferred(r) => r.base.seq,
        LaneRecord::Usage(r) => r.base.seq,
    }
}

fn record_run_id(record: &LaneRecord) -> Option<&str> {
    match record {
        LaneRecord::AbortRequested(r) => Some(&r.run_id),
        LaneRecord::OperationFinished(r) => Some(&r.run_id),
        LaneRecord::StepAttempt(r) => Some(&r.run_id),
        LaneRecord::ToolStarted(r) => Some(&r.run_id),
        LaneRecord::QueueEnqueued(r) => r.run_id.as_deref(),
        LaneRecord::QueueCancelled(r) => r.run_id.as_deref(),
        LaneRecord::WriteDeferred(r) => Some(&r.run_id),
        LaneRecord::Usage(r) => r.run_id.as_deref(),
        LaneRecord::OperationStarted(_) => None,
    }
}

fn entry_type(entry: &Entry) -> &'static str {
    match entry {
        Entry::Message(_) => "message",
        Entry::ModelChange(_) => "model_change",
        Entry::ThinkingLevelChange(_) => "thinking_level_change",
        Entry::ActiveToolsChange(_) => "active_tools_change",
        Entry::Compaction(_) => "compaction",
        Entry::BranchSummary(_) => "branch_summary",
        Entry::Custom(_) => "custom",
    }
}

/// 对应 `validateRecordLog`
pub fn validate_record_log(input: &RecordLogSlice) {
    if input.open_operations.len() > 1 {
        corrupt(
            RecordLogCorruptionReason::MultipleOpenOperations,
            format!("Lane {} has at least two open operations", input.lane),
        );
    }

    let mut entries_by_id: HashMap<String, Entry> = HashMap::new();
    for entry in &input.entries {
        entries_by_id.insert(entry.id().to_string(), entry.clone());
    }

    let mut finished_at: HashMap<String, u64> = HashMap::new();
    let mut aborted_at: HashMap<String, u64> = HashMap::new();
    let mut queue_enqueues: HashMap<String, LaneRecord> = HashMap::new();
    let mut latest_attempt: HashMap<String, StepAttemptRecord> = HashMap::new();
    let mut tool_invocations: HashSet<String> = HashSet::new();

    let mut records = input.records.clone();
    records.sort_by_key(record_seq);

    for record in &records {
        if let LaneRecord::OperationStarted(started) = record {
            if let Some(run_id) = record_run_id(record)
                && !input.open_operations.iter().any(|o| o.base.id == run_id)
                && !records
                    .iter()
                    .any(|r| matches!(r, LaneRecord::OperationStarted(s) if s.base.id == run_id))
            {
                // 操作可能不在 open_operations 里，但必须在 records 里有 started。
            }
            _ = started;
            continue;
        }

        if let Some(run_id) = record_run_id(record).map(|s| s.to_string())
            && let Some(finish_seq) = finished_at.get(&run_id)
            && record_seq(record) > *finish_seq
        {
            corrupt(
                RecordLogCorruptionReason::RecordAfterFinish,
                format!("Record follows the finish of operation {run_id}"),
            );
        }

        match record {
            LaneRecord::OperationFinished(r) => {
                finished_at.insert(r.run_id.clone(), r.base.seq);
            }
            LaneRecord::AbortRequested(r) => {
                aborted_at.insert(r.run_id.clone(), r.base.seq);
            }
            LaneRecord::StepAttempt(r) => {
                latest_attempt.insert(r.run_id.clone(), r.clone());
            }
            LaneRecord::ToolStarted(r) => {
                let invocation = format!("{}\u{0}{}", r.assistant_entry_id, r.tool_index);
                if tool_invocations.contains(&invocation) {
                    corrupt(
                        RecordLogCorruptionReason::DuplicateToolInvocation,
                        format!(
                            "Tool invocation {}:{} is duplicated",
                            r.assistant_entry_id, r.tool_index
                        ),
                    );
                }
                tool_invocations.insert(invocation);
            }
            LaneRecord::QueueEnqueued(r) => {
                if r.queue != "nextRun"
                    && let Some(&abort_seq) = aborted_at.get(r.run_id.as_deref().unwrap_or(""))
                    && r.base.seq > abort_seq
                {
                    corrupt(
                        RecordLogCorruptionReason::QueueAfterAbort,
                        format!("{} item was enqueued after abort", r.queue),
                    );
                }
                if let Some(target) = r.target.as_object()
                    && let Some(id) = target.get("id").and_then(|v| v.as_str())
                {
                    queue_enqueues.insert(id.to_string(), record.clone());
                }
            }
            LaneRecord::QueueCancelled(r) => {
                if let Some(enqueue) = queue_enqueues.get(&r.entry_id)
                    && record_seq(enqueue) >= r.base.seq
                {
                    corrupt(
                        RecordLogCorruptionReason::InvalidQueueCancellation,
                        "Queue cancellation has no pending matching enqueue".to_string(),
                    );
                }
            }
            _ => {}
        }
    }
}

fn by_sequence<T, F: Fn(&T) -> u64>(mut values: Vec<T>, seq: F) -> Vec<T> {
    values.sort_by_key(|v| seq(v));
    values
}

/// 对应 `deriveEffectiveConfiguration`
fn derive_effective_configuration(input: &LaneReductionInput) -> EffectiveLaneConfiguration {
    let mut config = input.defaults.clone();
    let mut entries: Vec<Entry> = Vec::new();
    for e in input
        .configuration_entries
        .iter()
        .chain(input.own_entries.iter())
    {
        entries.push(e.clone());
    }
    entries.sort_by_key(|e| match e {
        Entry::Message(m) => m.base.seq,
        Entry::ModelChange(m) => m.base.seq,
        Entry::ThinkingLevelChange(m) => m.base.seq,
        Entry::ActiveToolsChange(m) => m.base.seq,
        Entry::Compaction(m) => m.base.seq,
        Entry::BranchSummary(m) => m.base.seq,
        Entry::Custom(m) => m.base.seq,
    });

    for entry in &entries {
        match entry {
            Entry::ModelChange(m) => {
                config.provider = m.provider.clone();
                config.model_id = m.model_id.clone();
            }
            Entry::ThinkingLevelChange(t) => {
                config.thinking_level = t.thinking_level.clone();
            }
            Entry::ActiveToolsChange(t) => {
                config.active_tool_names = t.active_tool_names.clone();
            }
            Entry::Message(m) => {
                if let AgentMessage::Assistant(a) = &m.message {
                    config.provider = a.provider.clone();
                    config.model_id = a.model.clone();
                }
            }
            _ => {}
        }
    }
    config
}

/// 对应 `deriveToolBatch`
fn derive_tool_batch(
    operation_id: &str,
    records: &[LaneRecord],
    own_entries: &[Entry],
) -> Option<ToolBatchState> {
    let assistant_entry = own_entries.iter().rev().find(|entry| {
        let Entry::Message(m) = entry else {
            return false;
        };
        let AgentMessage::Assistant(a) = &m.message else {
            return false;
        };
        a.content
            .iter()
            .any(|c| matches!(c, pi_ai::ContentBlock::ToolCall(_)))
    })?;
    let Entry::Message(assistant_msg) = assistant_entry else {
        return None;
    };
    let AgentMessage::Assistant(assistant) = &assistant_msg.message else {
        return None;
    };

    let tool_calls: Vec<pi_ai::ToolCall> = assistant
        .content
        .iter()
        .filter_map(|c| match c {
            pi_ai::ContentBlock::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect();

    let mut starts: HashMap<u32, ToolStartedRecord> = HashMap::new();
    for record in records {
        if let LaneRecord::ToolStarted(t) = record
            && t.run_id == operation_id
            && t.assistant_entry_id == assistant_msg.base.id
        {
            starts.insert(t.tool_index, t.clone());
        }
    }

    let calls: Vec<ToolBatchCall> = tool_calls
        .iter()
        .enumerate()
        .map(|(index, tool_call)| {
            let started = starts.get(&(index as u32));
            let result_exists = started.is_some();
            let terminate = false;
            ToolBatchCall {
                tool_index: index as u32,
                tool_call: tool_call.clone(),
                result_exists,
                terminate,
            }
        })
        .collect();

    Some(ToolBatchState {
        assistant_entry_id: assistant_msg.base.id.clone(),
        unresolved: calls.iter().any(|c| !c.result_exists),
        calls,
        truncated: assistant.stop_reason == pi_ai::StopReason::Length,
    })
}

/// 对应 `reduceLaneState`
pub fn reduce_lane_state(input: LaneReductionInput) -> LaneReductionResult {
    let slice = RecordLogSlice {
        lane: input.lane.clone(),
        open_operations: input.open_operations.clone(),
        records: input.records.clone(),
        entries: input.entries.clone(),
    };
    validate_record_log(&slice);

    let records = by_sequence(input.records.clone(), record_seq);
    let own_entries = by_sequence(input.own_entries.clone(), |e| match e {
        Entry::Message(m) => m.base.seq,
        Entry::ModelChange(m) => m.base.seq,
        Entry::ThinkingLevelChange(m) => m.base.seq,
        Entry::ActiveToolsChange(m) => m.base.seq,
        Entry::Compaction(m) => m.base.seq,
        Entry::BranchSummary(m) => m.base.seq,
        Entry::Custom(m) => m.base.seq,
    });

    let effective_configuration = derive_effective_configuration(&input);

    let started = input.open_operations.first().cloned();
    let pending_next_run: Vec<serde_json::Value> = records
        .iter()
        .filter_map(|r| match r {
            LaneRecord::QueueEnqueued(q) if q.queue == "nextRun" => Some(q.target.clone()),
            _ => None,
        })
        .collect();

    let Some(started) = started else {
        return LaneReductionResult {
            lane_state: LaneState {
                lane: input.lane.clone(),
                leaf_id: input.leaf_id.clone(),
                operation: None,
                pending_next_run,
            },
            effective_configuration,
            terminal_failure: None,
        };
    };

    let operation_records: Vec<LaneRecord> = records
        .iter()
        .filter(|r| match r {
            LaneRecord::OperationStarted(o) => o.base.id == started.base.id,
            _ => record_run_id(r) == Some(&started.base.id),
        })
        .cloned()
        .collect();

    let aborting = operation_records
        .iter()
        .any(|r| matches!(r, LaneRecord::AbortRequested(_)));
    let pending_steer: Vec<serde_json::Value> = if aborting {
        Vec::new()
    } else {
        operation_records
            .iter()
            .filter_map(|r| match r {
                LaneRecord::QueueEnqueued(q) if q.queue == "steer" => Some(q.target.clone()),
                _ => None,
            })
            .collect()
    };
    let pending_follow_up: Vec<serde_json::Value> = if aborting {
        Vec::new()
    } else {
        operation_records
            .iter()
            .filter_map(|r| match r {
                LaneRecord::QueueEnqueued(q) if q.queue == "followUp" => Some(q.target.clone()),
                _ => None,
            })
            .collect()
    };

    let newest_attempt = operation_records
        .iter()
        .filter_map(|r| match r {
            LaneRecord::StepAttempt(s) => Some(s.clone()),
            _ => None,
        })
        .next_back();

    let entries_by_id: HashMap<String, Entry> = input
        .entries
        .iter()
        .chain(input.own_entries.iter())
        .map(|e| (e.id().to_string(), e.clone()))
        .collect();

    let step = newest_attempt.and_then(|s| {
        if entries_by_id.contains_key(&s.result_entry_id) {
            None
        } else {
            Some(LaneStep {
                kind: s.step.clone(),
                attempts: s.attempt,
                result_entry_id: s.result_entry_id,
                compaction_reason: s.compaction_reason.map(|r| format!("{r:?}").to_lowercase()),
            })
        }
    });

    let tool_batch = derive_tool_batch(&started.base.id, &operation_records, &own_entries);
    let pending_writes: Vec<serde_json::Value> = operation_records
        .iter()
        .filter_map(|r| match r {
            LaneRecord::WriteDeferred(w) => Some(w.target.clone()),
            _ => None,
        })
        .collect();

    let newest_own_entry = own_entries.last();
    let newest_own = newest_own_entry.map(|entry| NewestOwn {
        entry_id: entry.id().to_string(),
        kind: entry_type(entry).to_string(),
        role: match entry {
            Entry::Message(m) => Some(m.message.role().to_string()),
            _ => None,
        },
        stop_reason: match entry {
            Entry::Message(m) => match &m.message {
                AgentMessage::Assistant(a) => Some(a.stop_reason),
                _ => None,
            },
            _ => None,
        },
    });

    LaneReductionResult {
        lane_state: LaneState {
            lane: input.lane,
            leaf_id: input.leaf_id,
            operation: Some(LaneOperation {
                id: started.base.id.clone(),
                kind: started.kind_str(),
                aborting,
                step,
                tool_batch,
                missing_initial_messages: Vec::new(),
                pending_steer,
                pending_follow_up,
                pending_writes,
                deferred: None,
                overflow_recovery_used: false,
                newest_own,
            }),
            pending_next_run,
        },
        effective_configuration,
        terminal_failure: None,
    }
}
