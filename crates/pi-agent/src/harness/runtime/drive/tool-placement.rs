//! Rust 翻译自 packages/agent/src/harness/runtime/drive/tool-placement.ts
//!
//! 工具结果到 transcript 的放置（place）：读取 staged 结果、提交 placement、物化。

use std::collections::BTreeMap;

use pi_ai::{AssistantMessage, ToolResultMessage};

use crate::harness::agent_harness::HarnessEvent;
use crate::harness::harness_event::ConfigUpdatePayload;
use crate::harness::runtime::types::{Drive, Lane, LanePatch, LaneRuntimeState, OperationCommand};
use crate::harness::session::commit::{insert_entry, insert_usage};
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{
    Continuation, NewEntry, NewMessageEntry, OperationState, ToolBatch, ToolCall, UsageRow, Write,
};
use crate::harness::session::values::{
    branch_tip, delete_value, lane_config, operation_tool_args_prefix, pending_entry, set_value,
};
use crate::types::AgentMessage;

#[derive(Clone)]
pub struct ToolBatchSource {
    pub assistant: AssistantMessage,
    pub calls: BTreeMap<usize, pi_ai::ToolCall>,
}

/// 对应 `readToolBatchSource`。
pub async fn read_tool_batch_source<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    batch: &ToolBatch,
) -> Result<ToolBatchSource, String> {
    let assistant_entry_id = batch.assistant_entry_id.clone();
    let calls = batch.calls.clone();
    let drive_context = drive.context.clone();
    lane.command(
        Box::new(move |_state, reader| {
            let assistant_entry_id = assistant_entry_id.clone();
            let calls = calls.clone();
            let context = drive_context.clone();
            Box::pin(async move {
                let entries = reader
                    .get_entries(std::slice::from_ref(&assistant_entry_id), &context)
                    .await
                    .expect("get_entries failed");
                let Some(crate::harness::session::types::Entry::Message(entry)) =
                    entries.get(&assistant_entry_id)
                else {
                    panic!(
                        "{}",
                        SessionInvariantError::new("Tool batch assistant entry is invalid")
                    );
                };
                let AgentMessage::Assistant(assistant) = &entry.message else {
                    panic!(
                        "{}",
                        SessionInvariantError::new("Tool batch assistant entry is invalid")
                    );
                };
                let mut source_calls = BTreeMap::new();
                for call in &calls {
                    let block = assistant.content.get(call.source_index());
                    if !matches!(block, Some(pi_ai::ContentBlock::ToolCall(_))) {
                        panic!(
                            "{}",
                            SessionInvariantError::new(format!(
                                "Tool call source index {} does not name a tool-call block",
                                call.source_index()
                            ))
                        );
                    }
                    let pi_ai::ContentBlock::ToolCall(tool_call) = block.unwrap() else {
                        unreachable!();
                    };
                    source_calls.insert(call.source_index(), tool_call.clone());
                }
                crate::harness::runtime::types::LaneCommand::Return {
                    result: ToolBatchSource {
                        assistant: assistant.clone(),
                        calls: source_calls,
                    },
                }
            })
        }),
        &drive.context,
    )
    .await
}

/// 对应 `toolCallFor`。
pub fn tool_call_for(sources: &ToolBatchSource, call: &ToolCall) -> pi_ai::ToolCall {
    sources
        .calls
        .get(&call.source_index())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "{}",
                SessionInvariantError::new(format!(
                    "Tool call source index {} is invalid",
                    call.source_index()
                ))
            )
        })
}

/// 对应 `withToolBatch`。
pub fn with_tool_batch(run: &OperationState, batch: ToolBatch) -> OperationState {
    OperationState::Tools {
        scope: run.scope().clone(),
        batch,
    }
}

#[derive(Clone)]
struct PlacementItem {
    call: ToolCall,
    message: ToolResultMessage,
}

#[derive(Clone)]
struct PlacementRead {
    items: Vec<PlacementItem>,
    turn_results: Option<Vec<ToolResultMessage>>,
}

async fn read_placement<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    sources: &ToolBatchSource,
) -> Result<Option<PlacementRead>, String> {
    let sources = sources.clone();
    let drive_context = drive.context.clone();
    lane.command(
        Box::new(move |state, reader| {
            let sources = sources.clone();
            let context = drive_context.clone();
            Box::pin(async move {
                let Some(operation) = state.operation.clone() else {
                    return crate::harness::runtime::types::LaneCommand::Return { result: None };
                };
                let OperationState::Tools { batch: current, .. } = &operation.state else {
                    return crate::harness::runtime::types::LaneCommand::Return { result: None };
                };
                let first = current
                    .calls
                    .iter()
                    .position(|call| !matches!(call, ToolCall::Completed { .. }));
                let Some(mut first) = first else {
                    return crate::harness::runtime::types::LaneCommand::Return { result: None };
                };
                let mut ready: Vec<ToolCall> = Vec::new();
                while first < current.calls.len() {
                    let call = &current.calls[first];
                    if !matches!(call, ToolCall::OutcomeReady { .. }) {
                        break;
                    }
                    ready.push(call.clone());
                    first += 1;
                }
                if ready.is_empty() {
                    return crate::harness::runtime::types::LaneCommand::Return { result: None };
                }

                let mut items: Vec<PlacementItem> = Vec::new();
                for call in &ready {
                    let stored = reader
                        .get_value(&pending_entry(&call.result_entry_id()).erased(), &context)
                        .await
                        .expect("get_value failed");
                    let stored = stored.expect("staged result missing");
                    let crate::harness::session::types::PendingEntry::Message { payload } =
                        serde_json::from_value(stored.value.clone()).expect("pending entry decode")
                    else {
                        panic!(
                            "{}",
                            SessionInvariantError::new(format!(
                                "Tool call {} is missing its staged result",
                                call.result_entry_id()
                            ))
                        );
                    };
                    let AgentMessage::ToolResult(message) = payload else {
                        panic!(
                            "{}",
                            SessionInvariantError::new(format!(
                                "Tool call {} is missing its staged result",
                                call.result_entry_id()
                            ))
                        );
                    };
                    let source = tool_call_for(&sources, call);
                    if message.tool_call_id != source.id || message.tool_name != source.name {
                        panic!(
                            "{}",
                            SessionInvariantError::new(format!(
                                "Tool call {} has a mismatched staged result",
                                call.result_entry_id()
                            ))
                        );
                    }
                    items.push(PlacementItem {
                        call: call.clone(),
                        message,
                    });
                }

                let mut turn_results: Option<Vec<ToolResultMessage>> = None;
                if first == current.calls.len() {
                    let placed_ids: Vec<String> = current
                        .calls
                        .iter()
                        .filter(|call| matches!(call, ToolCall::Completed { .. }))
                        .map(|call| call.result_entry_id())
                        .collect();
                    let placed = reader
                        .get_entries(&placed_ids, &context)
                        .await
                        .expect("get_entries failed");
                    let staged: BTreeMap<String, ToolResultMessage> = items
                        .iter()
                        .map(|item| (item.call.result_entry_id(), item.message.clone()))
                        .collect();
                    let mut results = Vec::new();
                    for call in &current.calls {
                        let message = staged
                            .get(&call.result_entry_id())
                            .cloned()
                            .or_else(|| {
                                placed
                                    .get(&call.result_entry_id())
                                    .and_then(|entry| match entry {
                                        crate::harness::session::types::Entry::Message(e) => {
                                            match &e.message {
                                                AgentMessage::ToolResult(m) => Some(m.clone()),
                                                _ => None,
                                            }
                                        }
                                        _ => None,
                                    })
                            })
                            .expect("completed tool result missing");
                        results.push(message);
                    }
                    turn_results = Some(results);
                }

                crate::harness::runtime::types::LaneCommand::Return {
                    result: Some(PlacementRead {
                        items,
                        turn_results,
                    }),
                }
            })
        }),
        &drive.context,
    )
    .await
}

async fn commit_placement<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    capability: &OperationState,
    read: PlacementRead,
) -> Result<bool, String> {
    let usage_ids: Vec<Option<String>> = read
        .items
        .iter()
        .map(|item| {
            if item.message.usage.is_some() {
                Some(lane.session().id_generator().next(None))
            } else {
                None
            }
        })
        .collect();

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let turn_id = match capability {
        OperationState::Tools { batch, .. } => batch.turn_id.clone(),
        _ => String::new(),
    };
    let drive_context = drive.context.clone();

    lane.settle_operation(
        capability,
        Box::new(move |state, run, _meta, reader| {
            let lane_name = lane_name.clone();
            let operation_id = operation_id.clone();
            let turn_id = turn_id.clone();
            let context = drive_context.clone();
            let read_items = read.items.clone();
            let usage_ids = usage_ids.clone();
            let _turn_results = read.turn_results.clone();
            Box::pin(async move {
                let OperationState::Tools { batch: current, .. } = &run else {
                    panic!(
                        "{}",
                        SessionInvariantError::new("Placement requires a tools state")
                    );
                };
                let mut writes: Vec<Write> = Vec::new();
                let mut parent_id = state.tip_id.clone();
                let mut completed_calls: Vec<ToolCall> = current.calls.clone();
                let mut added_names: Vec<String> = Vec::new();
                let mut event_entries: Vec<(NewEntry, usize)> = Vec::new();
                let mut event_usage: Vec<(UsageRow, usize)> = Vec::new();

                for (index, item) in read_items.iter().enumerate() {
                    let entry = NewEntry::Message(NewMessageEntry {
                        id: item.call.result_entry_id(),
                        parent_id: parent_id.clone(),
                        custom_type: None,
                        message: AgentMessage::ToolResult(item.message.clone()),
                        terminate: if matches!(
                            item.call,
                            ToolCall::OutcomeReady {
                                terminate: true,
                                ..
                            }
                        ) {
                            Some(true)
                        } else {
                            None
                        },
                    });
                    event_entries.push((entry.clone(), writes.len()));
                    writes.push(insert_entry(entry));
                    writes.push(Write::Value(delete_value(&pending_entry(
                        &item.call.result_entry_id(),
                    ))));

                    if let Some(usage_id) = &usage_ids[index]
                        && let Some(usage) = &item.message.usage
                    {
                        let row = UsageRow {
                            id: usage_id.clone(),
                            seq: 0,
                            usage: usage.clone(),
                            entry_id: Some(item.call.result_entry_id()),
                            adjustment: false,
                            details: None,
                        };
                        event_usage.push((row.clone(), writes.len()));
                        writes.push(insert_usage(row));
                    }

                    for name in item.message.added_tool_names.clone().unwrap_or_default() {
                        if !next_config_contains(&state, &added_names, &name) {
                            added_names.push(name);
                        }
                    }
                    parent_id = Some(item.call.result_entry_id());

                    for call in &mut completed_calls {
                        if call.source_index() == item.call.source_index()
                            && call.result_entry_id() == item.call.result_entry_id()
                        {
                            let terminate = matches!(
                                item.call,
                                ToolCall::OutcomeReady {
                                    terminate: true,
                                    ..
                                }
                            );
                            *call = ToolCall::Completed {
                                source_index: call.source_index(),
                                result_entry_id: call.result_entry_id(),
                                terminate,
                            };
                        }
                    }
                }

                let complete = completed_calls
                    .iter()
                    .all(|call| matches!(call, ToolCall::Completed { .. }));
                let mut next_configuration = state.configuration.clone();
                if !added_names.is_empty() {
                    for name in &added_names {
                        if !next_configuration.active_tool_names.contains(name) {
                            next_configuration.active_tool_names.push(name.clone());
                        }
                    }
                    writes.push(Write::Value(set_value(
                        &lane_config(&lane_name),
                        serde_json::to_value(&next_configuration)
                            .unwrap_or(serde_json::Value::Null),
                    )));
                }
                writes.push(Write::Value(set_value(
                    &branch_tip(&lane_name),
                    serde_json::json!(parent_id),
                )));

                let next_run = if complete {
                    let all_terminate = completed_calls.iter().all(|call| {
                        matches!(
                            call,
                            ToolCall::Completed {
                                terminate: true,
                                ..
                            }
                        )
                    });
                    let checkpoint = OperationState::Checkpoint {
                        scope: run.scope().clone(),
                        continuation: if all_terminate {
                            Continuation::MayFinish {
                                include_final_assistant: false,
                            }
                        } else {
                            Continuation::NeedAssistant {
                                overflow_recovery_used: false,
                            }
                        },
                        trigger_entry_id: parent_id.clone().unwrap_or_default(),
                    };
                    let args = reader
                        .scan_values(
                            &operation_tool_args_prefix(&operation_id, Some(&turn_id)),
                            &context,
                        )
                        .await
                        .expect("scan_values failed");
                    for stored in args {
                        writes.push(Write::Value(delete_value(&stored.address)));
                    }
                    checkpoint
                } else {
                    with_tool_batch(
                        &run,
                        ToolBatch {
                            assistant_entry_id: current.assistant_entry_id.clone(),
                            configuration: current.configuration.clone(),
                            turn_id: current.turn_id.clone(),
                            calls: completed_calls.clone(),
                        },
                    )
                };

                let record_complete = complete;
                let lane_name_events = lane_name.clone();
                let previous_tools = state.configuration.active_tool_names.clone();
                let value_tools = next_configuration.active_tool_names.clone();
                OperationCommand::Commit {
                    writes,
                    operation_state: next_run,
                    lane: Some(LanePatch {
                        tip_id: parent_id.clone(),
                        configuration: Some(next_configuration.clone()),
                        inbox: None,
                    }),
                    materialize: Box::new(move |_| record_complete),
                    events: Some(Box::new(move |commit| {
                        let mut events: Vec<HarnessEvent> = Vec::new();
                        for (entry, seq_index) in &event_entries {
                            let seq = commit.seqs.get(*seq_index).copied().unwrap_or(0);
                            let materialized =
                                crate::harness::session::commit::materialize_committed_entry(
                                    entry,
                                    seq,
                                    commit.timestamp,
                                );
                            events.push(HarnessEvent::EntryAdded {
                                lane: lane_name_events.clone(),
                                entry: materialized.clone(),
                                recovery: None,
                            });
                            if let Some((row, row_index)) = event_usage
                                .iter()
                                .find(|(row, _)| row.entry_id.as_deref() == Some(entry.id()))
                            {
                                let mut row = row.clone();
                                row.seq = commit.seqs.get(*row_index).copied().unwrap_or(0);
                                events.push(HarnessEvent::Usage {
                                    lane: lane_name_events.clone(),
                                    row,
                                    totals: commit.stats.usage.clone(),
                                });
                            }
                        }
                        if !added_names.is_empty() {
                            events.push(HarnessEvent::ConfigUpdate {
                                lane: Some(lane_name_events.clone()),
                                payload: ConfigUpdatePayload::ActiveTools {
                                    value: value_tools.clone(),
                                    previous: previous_tools.clone(),
                                },
                                recovery: None,
                            });
                        }
                        events
                    })),
                }
            })
        }),
        &drive.context,
    )
    .await
}

fn next_config_contains(state: &LaneRuntimeState, added: &[String], name: &str) -> bool {
    state
        .configuration
        .active_tool_names
        .contains(&name.to_string())
        || added.contains(&name.to_string())
}

/// 对应 `materializeReady`。
pub async fn materialize_ready<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    capability: &OperationState,
    sources: &ToolBatchSource,
    recovery: bool,
) -> Result<(), String> {
    let read = read_placement(lane, drive, sources).await?;
    let Some(read) = read else {
        return Ok(());
    };

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let mut batch_events: Vec<HarnessEvent> = Vec::new();
    for item in &read.items {
        batch_events.push(HarnessEvent::MessageStart {
            lane: lane_name.clone(),
            run_id: Some(operation_id.clone()),
            message: AgentMessage::ToolResult(item.message.clone()),
            recovery: Some(recovery),
        });
        batch_events.push(HarnessEvent::MessageEnd {
            lane: lane_name.clone(),
            run_id: Some(operation_id.clone()),
            message: AgentMessage::ToolResult(item.message.clone()),
            entry_id: Some(item.call.result_entry_id()),
            recovery: Some(recovery),
        });
    }
    let _ = lane.emit_batch(batch_events, &drive.context).await;

    let turn_results = read.turn_results.clone();
    let complete = commit_placement(lane, drive, capability, read).await?;
    if complete && let Some(turn_results) = &turn_results {
        let turn_id = match capability {
            OperationState::Tools { batch, .. } => batch.turn_id.clone(),
            _ => String::new(),
        };
        let _ = lane
            .emit_batch(
                vec![HarnessEvent::TurnEnd {
                    lane: lane_name,
                    run_id: operation_id,
                    turn_id,
                    message: sources.assistant.clone(),
                    tool_results: turn_results.clone(),
                    recovery: Some(recovery),
                }],
                &drive.context,
            )
            .await;
    }
    Ok(())
}
