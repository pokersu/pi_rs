//! Rust 翻译自 packages/agent/src/harness/runtime/reducer.ts

use pi_ai::StopReason;

use crate::harness::harness_event::{
    DeferredSnapshot, HarnessEvent, LaneOperationSnapshot, LaneSnapshot, LaneSnapshotTool,
    RetrySnapshot,
};
use crate::harness::session::types::{Entry, OperationIntent, OperationResultRecord};
use crate::types::AgentMessage;

/// 对应 `LaneSnapshotReduction = "rebase" | undefined`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneSnapshotReduction {
    Rebase,
}

fn upsert_tool(operation: &mut LaneOperationSnapshot, tool: LaneSnapshotTool) {
    let call_id = match &tool {
        LaneSnapshotTool::Running { tool_call_id, .. } => tool_call_id.clone(),
        LaneSnapshotTool::Settled { tool_call_id, .. } => tool_call_id.clone(),
    };
    let index = operation.running_tools.iter().position(|candidate| {
        let id = match candidate {
            LaneSnapshotTool::Running { tool_call_id, .. } => tool_call_id,
            LaneSnapshotTool::Settled { tool_call_id, .. } => tool_call_id,
        };
        id == &call_id
    });
    match index {
        None => operation.running_tools.push(tool),
        Some(index) => operation.running_tools[index] = tool,
    }
}

fn matching_operation<'a>(
    snapshot: &'a mut LaneSnapshot,
    operation_id: &str,
) -> Option<&'a mut LaneOperationSnapshot> {
    let operation = snapshot.operation.as_mut()?;
    if operation.id == operation_id {
        Some(operation)
    } else {
        None
    }
}

/// 对应 `reduceLaneSnapshot`：把一个 harness 事件应用到可变的 lane 快照。
pub fn reduce_lane_snapshot(
    snapshot: &mut LaneSnapshot,
    event: &HarnessEvent,
) -> Option<LaneSnapshotReduction> {
    if let Some(lane) = event.lane()
        && lane != snapshot.lane
        && !matches!(event, HarnessEvent::Usage { .. })
    {
        return None;
    }
    match event {
        HarnessEvent::RunStart {
            run_id, started_at, ..
        } => {
            snapshot.operation = Some(LaneOperationSnapshot {
                id: run_id.clone(),
                kind: "run".to_string(),
                started_at: *started_at,
                from_tip_id: snapshot.tip_id.clone(),
                status: "open".to_string(),
                retry: None,
                deferred: None,
                streaming_message: None,
                running_tools: Vec::new(),
            });
        }
        HarnessEvent::CompactionStart {
            run_id, started_at, ..
        } => {
            if snapshot.operation.is_some() {
                return None;
            }
            snapshot.operation = Some(LaneOperationSnapshot {
                id: run_id.clone(),
                kind: "compaction".to_string(),
                started_at: *started_at,
                from_tip_id: snapshot.tip_id.clone(),
                status: "open".to_string(),
                retry: None,
                deferred: None,
                streaming_message: None,
                running_tools: Vec::new(),
            });
        }
        HarnessEvent::NavigationStart {
            run_id, started_at, ..
        } => {
            snapshot.operation = Some(LaneOperationSnapshot {
                id: run_id.clone(),
                kind: "navigation".to_string(),
                started_at: *started_at,
                from_tip_id: snapshot.tip_id.clone(),
                status: "open".to_string(),
                retry: None,
                deferred: None,
                streaming_message: None,
                running_tools: Vec::new(),
            });
        }
        HarnessEvent::OperationAbort { operation_id, .. } => {
            if let Some(operation) = matching_operation(snapshot, operation_id) {
                operation.status = "aborting".to_string();
            }
        }
        HarnessEvent::RunResume { run_id, .. } => {
            if let Some(operation) = matching_operation(snapshot, run_id) {
                operation.deferred = None;
            }
        }
        HarnessEvent::RunSuspend {
            run_id,
            deferred,
            poll,
            ..
        } => {
            let operation = matching_operation(snapshot, run_id)?;
            operation.streaming_message = None;
            operation.deferred = Some(DeferredSnapshot {
                handle: deferred.clone(),
                poll: *poll,
            });
        }
        HarnessEvent::RetryScheduled {
            run_id,
            attempt,
            max_attempts,
            not_before,
            ..
        } => {
            if let Some(operation) = matching_operation(snapshot, run_id) {
                operation.retry = Some(RetrySnapshot {
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                    next_attempt_at: *not_before,
                });
            }
        }
        HarnessEvent::RetryStart { run_id, .. } | HarnessEvent::RetryEnd { run_id, .. } => {
            if let Some(operation) = matching_operation(snapshot, run_id) {
                operation.retry = None;
            }
        }
        HarnessEvent::MessageStart {
            run_id, message, ..
        } => {
            if let (Some(run_id), AgentMessage::Assistant(assistant)) = (run_id, message)
                && assistant.stop_reason == StopReason::Pending
                && let Some(operation) = matching_operation(snapshot, run_id)
            {
                operation.streaming_message = Some(assistant.clone());
            }
        }
        HarnessEvent::MessageUpdate {
            run_id, message, ..
        } => {
            if let AgentMessage::Assistant(assistant) = message
                && let Some(operation) = matching_operation(snapshot, run_id)
            {
                operation.streaming_message = Some(assistant.clone());
            }
        }
        HarnessEvent::MessageEnd { run_id, .. } => {
            if let Some(run_id) = run_id
                && let Some(operation) = matching_operation(snapshot, run_id)
            {
                operation.streaming_message = None;
            }
        }
        HarnessEvent::ToolStart {
            run_id,
            tool_call_id,
            tool_name,
            args,
            ..
        } => {
            if let Some(operation) = matching_operation(snapshot, run_id) {
                upsert_tool(
                    operation,
                    LaneSnapshotTool::Running {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                        result: None,
                    },
                );
            }
        }
        HarnessEvent::ToolUpdate {
            run_id,
            tool_call_id,
            partial_result,
            ..
        } => {
            let operation = matching_operation(snapshot, run_id);
            let operation = operation?;
            let index = operation.running_tools.iter().position(|candidate| {
                let id = match candidate {
                    LaneSnapshotTool::Running { tool_call_id, .. } => tool_call_id,
                    LaneSnapshotTool::Settled { tool_call_id, .. } => tool_call_id,
                };
                id == tool_call_id
            })?;
            if let LaneSnapshotTool::Running { result, .. } = &mut operation.running_tools[index] {
                *result = Some(partial_result.clone());
            }
        }
        HarnessEvent::ToolEnd {
            run_id,
            tool_call_id,
            tool_name,
            result,
            is_error,
            ..
        } => {
            let operation = matching_operation(snapshot, run_id)?;
            let index = operation.running_tools.iter().position(|candidate| {
                let id = match candidate {
                    LaneSnapshotTool::Running { tool_call_id, .. } => tool_call_id,
                    LaneSnapshotTool::Settled { tool_call_id, .. } => tool_call_id,
                };
                id == tool_call_id
            })?;
            let args = match &operation.running_tools[index] {
                LaneSnapshotTool::Running { args, .. } => args.clone(),
                LaneSnapshotTool::Settled { args, .. } => args.clone(),
            };
            operation.running_tools[index] = LaneSnapshotTool::Settled {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args,
                result: result.clone(),
                is_error: *is_error,
            };
        }
        HarnessEvent::EntryAdded { entry, .. } => {
            if let Entry::Message(message_entry) = entry
                && let AgentMessage::ToolResult(tool_result) = &message_entry.message
                && let Some(operation) = snapshot.operation.as_mut()
            {
                let tool_call_id = tool_result.tool_call_id.clone();
                operation.running_tools.retain(|candidate| {
                    let id = match candidate {
                        LaneSnapshotTool::Running { tool_call_id, .. } => tool_call_id,
                        LaneSnapshotTool::Settled { tool_call_id, .. } => tool_call_id,
                    };
                    id != &tool_call_id
                });
            }
            if matches!(entry, Entry::Compaction(_)) {
                snapshot.transcript.clear();
                snapshot.transcript.push(entry.clone());
            } else {
                snapshot.transcript.push(entry.clone());
            }
            snapshot.tip_id = Some(entry.id().to_string());
            if matches!(entry, Entry::Message(_)) {
                snapshot.stats.message_count += 1;
            }
        }
        HarnessEvent::QueueUpdate { queues, .. } => {
            snapshot.queues = queues.clone();
        }
        HarnessEvent::Usage { totals, .. } => {
            snapshot.stats.usage = totals.clone();
        }
        HarnessEvent::ConfigUpdate { payload, .. } => match payload {
            crate::harness::harness_event::ConfigUpdatePayload::Model { value, .. } => {
                snapshot.configuration.model = value.clone();
            }
            crate::harness::harness_event::ConfigUpdatePayload::ThinkingLevel { value, .. } => {
                snapshot.configuration.thinking_level = *value;
            }
            crate::harness::harness_event::ConfigUpdatePayload::ActiveTools { value, .. } => {
                snapshot.configuration.active_tool_names = value.clone();
            }
            _ => {}
        },
        HarnessEvent::RunEnd {
            run_id,
            status,
            error,
            from_tip_id,
            tip_id,
            ended_at,
            ..
        } => {
            let operation = matching_operation(snapshot, run_id)?;
            if operation.kind != "run" {
                return None;
            }
            let record = OperationResultRecord {
                operation_id: run_id.clone(),
                kind: OperationIntent::Run {
                    prompt_entry_ids: Vec::new(),
                },
                status: *status,
                error: error.clone(),
                from_tip_id: from_tip_id.clone(),
                tip_id: tip_id.clone(),
                started_at: operation.started_at,
                ended_at: *ended_at,
            };
            snapshot.last_result = Some(record);
            snapshot.operation = None;
            snapshot.tip_id = tip_id.clone();
        }
        HarnessEvent::CompactionEnd {
            run_id,
            status,
            error,
            ended_at,
            ..
        } => {
            let (from_tip_id, started_at) = {
                let operation = matching_operation(snapshot, run_id);
                let operation = operation?;
                if operation.kind != "compaction" {
                    return None;
                }
                (operation.from_tip_id.clone(), operation.started_at)
            };
            let tip_id = snapshot.tip_id.clone();
            let record = OperationResultRecord {
                operation_id: run_id.clone(),
                kind: OperationIntent::Compaction {
                    custom_instructions: None,
                },
                status: *status,
                error: error.clone(),
                from_tip_id,
                tip_id,
                started_at,
                ended_at: *ended_at,
            };
            snapshot.last_result = Some(record);
            snapshot.operation = None;
        }
        HarnessEvent::NavigationEnd { .. } => {
            return Some(LaneSnapshotReduction::Rebase);
        }
        HarnessEvent::Fault { .. } => {
            snapshot.faulted = true;
        }
        HarnessEvent::HandlerError { .. }
        | HarnessEvent::TurnStart { .. }
        | HarnessEvent::TurnEnd { .. }
        | HarnessEvent::ValueUpdate { .. }
        | HarnessEvent::LaneCreated { .. } => {}
    }
    None
}
