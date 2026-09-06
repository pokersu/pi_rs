//! Rust 翻译自 packages/agent/src/harness/runtime/drive/checkpoint.ts
//!
//! run 的起点与 boundary 推进：startRun 消费 before_run 并提交初始 checkpoint，
//! runCheckpoint 以最多一次 commit 推进一个 durable run boundary。

use crate::harness::execution::effect_gate::GateError;
use crate::harness::harness_event::HarnessEvent;
use crate::harness::runtime::drive::boundary::{
    BoundaryFinishPending, assistant_ready_at_boundary, boundary_placement_events,
    finish_run_boundary, plan_boundary_inbox,
};
use crate::harness::runtime::drive::structural::prepare_compaction_threshold;
use crate::harness::runtime::transcript::committed_entry_events;
use crate::harness::runtime::types::{
    ContinueOperationResult, Drive, Lane, LanePatch, OperationCommand, ProcedureResult,
};
use crate::harness::session::commit::insert_entry;
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{
    Continuation, Entry, NewEntry, NewMessageEntry, OperationIntent, OperationState, Write,
};
use crate::harness::session::values::{branch_tip, operation_preparation, set_value};
use crate::types::AgentMessage;

/// `runCheckpoint` plan 的产出：普通 procedure 结果或 finish 待定。
#[allow(clippy::large_enum_variant)]
pub enum CheckpointPlanned {
    Procedure(ProcedureResult),
    FinishPending(BoundaryFinishPending),
}

/// 对应 `startRun`：消费 before_run 并提交初始 checkpoint。
pub async fn start_run<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    run: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let drive_context = drive.context.clone();
    let prompt = lane
        .continue_operation(
            run,
            Box::new(move |_state, _current, meta, reader| {
                let context = drive_context.clone();
                Box::pin(async move {
                    let OperationIntent::Run { prompt_entry_ids } = &meta.intent else {
                        panic!(
                            "{}",
                            SessionInvariantError::new("Run operation has non-run intent")
                        );
                    };
                    let entries = reader
                        .get_entries(prompt_entry_ids, &context)
                        .await
                        .expect("get_entries failed");
                    let mut messages = Vec::new();
                    for id in prompt_entry_ids {
                        let Some(Entry::Message(e)) = entries.get(id) else {
                            panic!(
                                "{}",
                                SessionInvariantError::new(format!(
                                    "Run prompt entry {id} is missing its message"
                                ))
                            );
                        };
                        messages.push(e.message.clone());
                    }
                    OperationCommand::Return { result: messages }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;

    let ContinueOperationResult::Result { value: messages } = prompt else {
        return Ok(ProcedureResult::Continue);
    };

    let hook = lane
        .hooks()
        .run_with_gate(
            crate::harness::hooks::HookName::BeforeRun,
            serde_json::json!({
                "lane": lane.name(),
                "runId": drive.operation_id,
                "prompt": messages,
                "resources": lane.read_config().resources,
            }),
            &drive.gate,
            &drive.context,
        )
        .await?;

    let injected: Vec<AgentMessage> = hook
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| serde_json::from_value(m.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    for message in &injected {
        if let AgentMessage::Assistant(a) = message
            && a.stop_reason == pi_ai::StopReason::Pending
        {
            panic!(
                "{}",
                SessionInvariantError::new("before_run returned a pending assistant message")
            );
        }
    }

    let id_generator = lane.session().id_generator();
    let reserved: Vec<(String, AgentMessage)> = injected
        .iter()
        .map(|m| (id_generator.next(None), m.clone()))
        .collect();

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();

    let result = lane
        .continue_operation(
            run,
            Box::new(move |state, current, _meta, _reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let reserved = reserved.clone();
                Box::pin(async move {
                    let mut entries: Vec<NewEntry> = Vec::new();
                    let mut parent_id = state.tip_id.clone();
                    for (id, message) in &reserved {
                        entries.push(NewEntry::Message(NewMessageEntry {
                            id: id.clone(),
                            parent_id: parent_id.clone(),
                            custom_type: None,
                            message: message.clone(),
                            terminate: None,
                        }));
                        parent_id = Some(id.clone());
                    }
                    let trigger_entry_id = entries
                        .last()
                        .map(|e| e.id().to_string())
                        .or_else(|| state.tip_id.clone());
                    let Some(trigger_entry_id) = trigger_entry_id else {
                        panic!(
                            "{}",
                            SessionInvariantError::new("Run start has no trigger entry")
                        );
                    };
                    let next_state = OperationState::Checkpoint {
                        scope: current.scope().clone(),
                        continuation: Continuation::NeedAssistant {
                            overflow_recovery_used: false,
                        },
                        trigger_entry_id: trigger_entry_id.clone(),
                    };
                    let mut writes: Vec<Write> =
                        entries.iter().cloned().map(insert_entry).collect();
                    if !entries.is_empty() {
                        writes.push(Write::Value(set_value(
                            &branch_tip(&lane_name),
                            serde_json::json!(trigger_entry_id),
                        )));
                    }
                    OperationCommand::Commit {
                        writes,
                        operation_state: next_state,
                        lane: Some(LanePatch {
                            tip_id: Some(trigger_entry_id),
                            configuration: None,
                            inbox: None,
                        }),
                        materialize: Box::new(|_| ProcedureResult::Continue),
                        events: Some(Box::new(move |commit| {
                            committed_entry_events(
                                &entries,
                                &commit,
                                &lane_name,
                                Some(&operation_id),
                                0,
                            )
                        })),
                    }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;

    Ok(match result {
        ContinueOperationResult::CancelRequested => ProcedureResult::Continue,
        ContinueOperationResult::Result { value } => value,
    })
}

/// 对应 `runCheckpoint`：以最多一次 commit 推进一个 durable run boundary。
pub async fn run_checkpoint<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    run: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let threshold = prepare_compaction_threshold(lane, drive, run)
        .await
        .map_err(GateError::Closed)?;
    let ContinueOperationResult::Result { value: threshold } = threshold else {
        return Ok(ProcedureResult::Continue);
    };

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let drive_context = drive.context.clone();
    let config = lane.read_config();
    let id_generator = lane.session().id_generator();

    let planned = lane
        .continue_operation(
            run,
            Box::new(move |state, current, _meta, reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let context = drive_context.clone();
                let config = config.clone();
                let id_generator = id_generator.clone();
                let threshold = threshold.clone();
                Box::pin(async move {
                    let placement = plan_boundary_inbox(
                        &config,
                        &lane_name,
                        &context,
                        &state,
                        current.scope(),
                        reader.as_ref(),
                        state.tip_id.clone(),
                        threshold.is_none()
                            && matches!(
                                current,
                                OperationState::Checkpoint {
                                    continuation: Continuation::MayFinish { .. },
                                    ..
                                }
                            ),
                    )
                    .await
                    .expect("plan_boundary_inbox failed");

                    if let Some(trigger) = placement.trigger_entry_id.clone() {
                        let writes = placement.writes.clone();
                        let inbox = placement.inbox.clone();
                        let tip_id = placement.tip_id.clone();
                        let next = assistant_ready_at_boundary(
                            &config,
                            id_generator.as_ref(),
                            &state,
                            current.scope().clone(),
                            trigger,
                            false,
                        );
                        return OperationCommand::Commit {
                            writes,
                            operation_state: next,
                            lane: Some(LanePatch {
                                tip_id,
                                configuration: None,
                                inbox: Some(inbox),
                            }),
                            materialize: Box::new(|_| CheckpointPlanned::Procedure(ProcedureResult::Continue)),
                            events: Some(Box::new(move |commit| {
                                boundary_placement_events(&placement, &commit, 0, &lane_name, &operation_id)
                            })),
                        };
                    }

                    if let Some((task_id, preparation)) = threshold {
                        let scope = current.scope().clone();
                        let checkpoint_data = match current {
                            OperationState::Checkpoint {
                                continuation,
                                trigger_entry_id,
                                ..
                            } => crate::harness::session::types::CheckpointData {
                                continuation,
                                trigger_entry_id: trigger_entry_id.clone(),
                            },
                            _ => panic!(
                                "{}",
                                SessionInvariantError::new("Checkpoint threshold requires a checkpoint state")
                            ),
                        };
                        let structural = OperationState::SummaryDeciding {
                            scope,
                            task: crate::harness::session::types::SummaryTask {
                                task_id: task_id.clone(),
                                reason: Some("threshold".to_string()),
                                custom_instructions: None,
                                boundary: crate::harness::session::types::ResultBoundary::ResumeCheckpoint {
                                    resume_after: checkpoint_data,
                                },
                            },
                        };
                        let mut writes = placement.writes.clone();
                        writes.push(Write::Value(set_value(
                            &operation_preparation(&operation_id, &task_id),
                            serde_json::to_value(&preparation).unwrap_or(serde_json::Value::Null),
                        )));
                        let inbox = placement.inbox.clone();
                        let tip_id = placement.tip_id.clone();
                        return OperationCommand::Commit {
                            writes,
                            operation_state: structural,
                            lane: Some(LanePatch {
                                tip_id,
                                configuration: None,
                                inbox: Some(inbox),
                            }),
                            materialize: Box::new(|_| CheckpointPlanned::Procedure(ProcedureResult::Continue)),
                            events: Some(Box::new(move |commit| {
                                let mut events = boundary_placement_events(
                                    &placement,
                                    &commit,
                                    0,
                                    &lane_name,
                                    &operation_id,
                                );
                                events.push(HarnessEvent::CompactionStart {
                                    lane: lane_name.clone(),
                                    run_id: operation_id.clone(),
                                    reason: "threshold".to_string(),
                                    started_at: commit.timestamp,
                                    recovery: None,
                                });
                                events
                            })),
                        };
                    }

                    let scope = current.scope().clone();
                    if let OperationState::Checkpoint {
                        continuation: Continuation::NeedAssistant {
                            overflow_recovery_used,
                        },
                        trigger_entry_id,
                        ..
                    } = current
                    {
                        let writes = placement.writes.clone();
                        let inbox = placement.inbox.clone();
                        let tip_id = placement.tip_id.clone();
                        let next = assistant_ready_at_boundary(
                            &config,
                            id_generator.as_ref(),
                            &state,
                            scope,
                            trigger_entry_id.clone(),
                            overflow_recovery_used,
                        );
                        return OperationCommand::Commit {
                            writes,
                            operation_state: next,
                            lane: Some(LanePatch {
                                tip_id,
                                configuration: None,
                                inbox: Some(inbox),
                            }),
                            materialize: Box::new(|_| CheckpointPlanned::Procedure(ProcedureResult::Continue)),
                            events: Some(Box::new(move |commit| {
                                boundary_placement_events(&placement, &commit, 0, &lane_name, &operation_id)
                            })),
                        };
                    }

                    OperationCommand::Return {
                        result: CheckpointPlanned::FinishPending(BoundaryFinishPending {
                            entry_ids: placement.entries.iter().map(|e| e.id().to_string()).collect(),
                        }),
                    }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;

    let ContinueOperationResult::Result { value: planned } = planned else {
        return Ok(ProcedureResult::Continue);
    };

    let CheckpointPlanned::FinishPending(BoundaryFinishPending { entry_ids }) = planned else {
        let CheckpointPlanned::Procedure(procedure) = planned else {
            unreachable!();
        };
        return Ok(procedure);
    };

    let OperationState::Checkpoint {
        continuation: Continuation::MayFinish { .. },
        ..
    } = run
    else {
        panic!(
            "{}",
            SessionInvariantError::new(
                "Checkpoint finish mediation requires a finish continuation"
            )
        );
    };

    finish_run_boundary(lane, drive, run, continuation_of(run), &entry_ids, vec![]).await
}

fn continuation_of(run: &OperationState) -> Continuation {
    match run {
        OperationState::Checkpoint { continuation, .. } => *continuation,
        _ => Continuation::NeedAssistant {
            overflow_recovery_used: false,
        },
    }
}
