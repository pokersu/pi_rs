//! Rust 翻译自 packages/agent/src/harness/runtime/drive/boundary.ts
//!
//! 一次 run 的 boundary 推进：inbox 规划、checkpoint 提交、assistant-ready 转换、finish 调解。

use pi_ai::UserContent;

use crate::harness::agent_harness::{HarnessEvent, LaneQueuedItem};
use crate::harness::context::Context;
use crate::harness::execution::effect_gate::GateError;
use crate::harness::runtime::drive::terminal::{operation_cleanup_writes, operation_result_record};
use crate::harness::runtime::transcript::{
    committed_entry_events, entry_lifecycle_events, read_bounded_context, read_lane_queues,
};
use crate::harness::runtime::types::{
    ContinueOperationResult, Drive, Lane, LaneRuntimeState, OperationCommand, ProcedureResult,
};
use crate::harness::session::commit::insert_entry;
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{
    CommitResult, GenerationContext, InboxItem, InboxItemKind, NewCustomEntry, NewEntry,
    NewMessageEntry, NormalizedRetryPolicy, OperationScope, OperationState, PendingEntry,
    SessionReader, Write,
};
use crate::harness::session::values::{branch_tip, delete_value, pending_entry, set_value};
use crate::types::AgentMessage;

/// 对应 `BoundaryFinishPending`。
#[derive(Debug, Clone)]
pub struct BoundaryFinishPending {
    pub entry_ids: Vec<String>,
}

/// 对应 `BoundaryPlacement`。
pub struct BoundaryPlacement {
    pub entries: Vec<NewEntry>,
    pub writes: Vec<Write>,
    pub tip_id: Option<String>,
    pub inbox: Vec<InboxItem>,
    pub trigger_entry_id: Option<String>,
    pub queues: Option<Vec<LaneQueuedItem>>,
}

/// 对应 `normalizedRetryPolicy`。
pub fn normalized_retry_policy(retry: &pi_ai::utils::retry::RetryPolicy) -> NormalizedRetryPolicy {
    let max_agent_delay_ms = retry
        .max_agent_delay_ms
        .unwrap_or(pi_ai::utils::retry::DEFAULT_MAX_AGENT_RETRY_DELAY_MS);
    if retry.enabled {
        NormalizedRetryPolicy {
            max_attempts: retry.max_retries as u32 + 1,
            base_delay_ms: retry.base_delay_ms,
            max_agent_delay_ms,
        }
    } else {
        NormalizedRetryPolicy {
            max_attempts: 1,
            base_delay_ms: retry.base_delay_ms,
            max_agent_delay_ms,
        }
    }
}

/// 对应 `assistantReadyAtBoundary`。
pub fn assistant_ready_at_boundary(
    config: &crate::harness::runtime::types::Config,
    id_generator: &dyn crate::harness::session::types::IdGenerator,
    state: &LaneRuntimeState,
    scope: OperationScope,
    trigger_entry_id: String,
    overflow_recovery_used: bool,
) -> OperationState {
    OperationState::AssistantReady {
        scope,
        generation_context: GenerationContext {
            step_id: id_generator.next(None),
            trigger_entry_id,
            configuration: state.configuration.clone(),
            stream_options: config.stream_options.clone(),
            retry_policy: normalized_retry_policy(&config.retry_policy),
            overflow_recovery_used,
        },
        next_attempt: 1,
    }
}

/// 对应 `planBoundaryInbox`：选择并物化一个 boundary 的 lane-owned 输入（不提交）。
#[allow(clippy::too_many_arguments)]
pub async fn plan_boundary_inbox(
    config: &crate::harness::runtime::types::Config,
    lane_name: &str,
    context: &Context,
    state: &LaneRuntimeState,
    scope: &OperationScope,
    reader: &dyn SessionReader,
    tip_id: Option<String>,
    follow_up_when_no_trigger: bool,
) -> Result<BoundaryPlacement, String> {
    let steer: Vec<InboxItem> = state
        .inbox
        .iter()
        .filter(|item| item.kind == InboxItemKind::Steer)
        .cloned()
        .collect();
    let selected_steer = if scope.settings.steering_mode == crate::types::QueueMode::OneAtATime {
        steer.iter().take(1).cloned().collect::<Vec<_>>()
    } else {
        steer.clone()
    };
    let mut selected: Vec<InboxItem> = state
        .inbox
        .iter()
        .filter(|item| {
            item.kind == InboxItemKind::Write
                || selected_steer
                    .iter()
                    .any(|candidate| candidate.entry_id == item.entry_id)
        })
        .cloned()
        .collect();

    async fn load(
        items: &[InboxItem],
        reader: &dyn SessionReader,
        context: &Context,
    ) -> Result<Vec<(InboxItem, PendingEntry)>, String> {
        let mut pending = Vec::new();
        for item in items {
            let stored = reader
                .get_value(&pending_entry(&item.entry_id).erased(), context)
                .await?;
            let stored = stored.ok_or_else(|| {
                SessionInvariantError::new(format!(
                    "Pending {:?} entry {} is missing its payload",
                    item.kind, item.entry_id
                ))
                .to_string()
            })?;
            if item.kind != InboxItemKind::Write {
                let pending_value: PendingEntry =
                    serde_json::from_value(stored.value.clone()).map_err(|e| e.to_string())?;
                if !matches!(pending_value, PendingEntry::Message { .. }) {
                    return Err(SessionInvariantError::new(format!(
                        "Queued {:?} entry {} is not a message",
                        item.kind, item.entry_id
                    ))
                    .to_string());
                }
            }
            let pending_value: PendingEntry =
                serde_json::from_value(stored.value).map_err(|e| e.to_string())?;
            pending.push((item.clone(), pending_value));
        }
        Ok(pending)
    }

    let mut pending = load(&selected, reader, context).await?;
    let projects =
        |config: &crate::harness::runtime::types::Config, value: &PendingEntry| -> bool {
            match value {
                PendingEntry::Message { .. } => true,
                PendingEntry::Custom { custom_type, .. } => {
                    config.entry_projectors.contains_key(custom_type)
                }
            }
        };
    if follow_up_when_no_trigger && !pending.iter().any(|(_, value)| projects(config, value)) {
        let follow_up: Vec<InboxItem> = state
            .inbox
            .iter()
            .filter(|item| item.kind == InboxItemKind::FollowUp)
            .cloned()
            .collect();
        let selected_follow_up =
            if scope.settings.follow_up_mode == crate::types::QueueMode::OneAtATime {
                follow_up.iter().take(1).cloned().collect::<Vec<_>>()
            } else {
                follow_up.clone()
            };
        selected.extend(selected_follow_up);
        selected.sort_by_key(|item| {
            state
                .inbox
                .iter()
                .position(|i| i.entry_id == item.entry_id)
                .unwrap_or(usize::MAX)
        });
        pending = load(&selected, reader, context).await?;
    }

    let mut parent_id = tip_id;
    let mut trigger_entry_id: Option<String> = None;
    let mut entries: Vec<NewEntry> = Vec::new();
    for (item, value) in &pending {
        let entry = match value {
            PendingEntry::Message { payload } => NewEntry::Message(NewMessageEntry {
                id: item.entry_id.clone(),
                parent_id: parent_id.clone(),
                custom_type: None,
                message: payload.clone(),
                terminate: None,
            }),
            PendingEntry::Custom {
                custom_type,
                payload,
            } => NewEntry::Custom(NewCustomEntry {
                id: item.entry_id.clone(),
                parent_id: parent_id.clone(),
                custom_type: custom_type.clone(),
                data: payload.clone(),
            }),
        };
        parent_id = Some(item.entry_id.clone());
        if projects(config, value) {
            trigger_entry_id = Some(item.entry_id.clone());
        }
        entries.push(entry);
    }

    let selected_ids: std::collections::HashSet<&str> =
        selected.iter().map(|item| item.entry_id.as_str()).collect();
    let inbox: Vec<InboxItem> = state
        .inbox
        .iter()
        .filter(|item| !selected_ids.contains(item.entry_id.as_str()))
        .cloned()
        .collect();
    let queues = if selected.is_empty() {
        None
    } else {
        Some(read_lane_queues(reader, &inbox, context).await?)
    };

    let mut writes: Vec<Write> = Vec::new();
    for entry in &entries {
        writes.push(insert_entry(entry.clone()));
    }
    for item in &selected {
        writes.push(Write::Value(delete_value(&pending_entry(&item.entry_id))));
    }
    if !entries.is_empty() {
        writes.push(Write::Value(set_value(
            &branch_tip(lane_name),
            serde_json::json!(parent_id),
        )));
    }

    Ok(BoundaryPlacement {
        entries,
        writes,
        tip_id: parent_id,
        inbox,
        trigger_entry_id,
        queues,
    })
}

/// 对应 `boundaryPlacementEvents`。
pub fn boundary_placement_events(
    placement: &BoundaryPlacement,
    commit: &CommitResult,
    first_write_index: usize,
    lane: &str,
    run_id: &str,
) -> Vec<HarnessEvent> {
    let mut events = committed_entry_events(
        &placement.entries,
        commit,
        lane,
        Some(run_id),
        first_write_index,
    );
    if let Some(queues) = &placement.queues {
        events.push(HarnessEvent::QueueUpdate {
            lane: lane.to_string(),
            queues: queues.clone(),
            recovery: None,
        });
    }
    events
}

/// 对应 `finishRunBoundary`：after before_run_end 重规划并提交续作或终态结果。
pub async fn finish_run_boundary<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    capability: &OperationState,
    continuation: crate::harness::session::types::Continuation,
    planned_entry_ids: &[String],
    pending_events: Vec<HarnessEvent>,
) -> Result<ProcedureResult, GateError> {
    let context = read_bounded_context(lane, drive, capability)
        .await
        .map_err(GateError::Closed)?;
    let ContinueOperationResult::Result { value: messages } = context else {
        return Ok(ProcedureResult::Continue);
    };

    let hook = lane
        .hooks()
        .run_with_gate(
            crate::harness::hooks::HookName::BeforeRunEnd,
            serde_json::json!({
                "lane": lane.name(),
                "runId": drive.operation_id,
                "messages": messages,
            }),
            &drive.gate,
            &drive.context,
        )
        .await?;

    let follow_up = hook.get("followUp").and_then(|v| v.as_str()).map(|text| {
        (
            lane.session().id_generator().next(None),
            AgentMessage::User(pi_ai::UserMessage {
                content: UserContent::Text(text.to_string()),
                timestamp: pi_ai::utils::uuid::now_ms() as u64,
            }),
        )
    });

    let config = lane.read_config();
    let id_generator = lane.session().id_generator();
    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let drive_context = drive.context.clone();
    let planned_entry_ids_owned = planned_entry_ids.to_vec();

    let result = lane
        .continue_operation(
            capability,
            Box::new(move |state, current, meta, reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let context = drive_context.clone();
                let pending_events = pending_events.clone();
                let follow_up = follow_up.clone();
                let planned_entry_ids = planned_entry_ids_owned.clone();
                let config = config.clone();
                let id_generator = id_generator.clone();
                Box::pin(async move {
                    let placement = plan_boundary_inbox(
                        &config,
                        &lane_name,
                        &context,
                        &state,
                        current.scope(),
                        reader.as_ref(),
                        state.tip_id.clone(),
                        true,
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
                            lane: Some(crate::harness::runtime::types::LanePatch {
                                tip_id,
                                configuration: None,
                                inbox: Some(inbox),
                            }),
                            materialize: Box::new(|_| ProcedureResult::Continue),
                            events: Some(Box::new(move |commit| {
                                let mut events = pending_events.clone();
                                events.extend(boundary_placement_events(
                                    &placement,
                                    &commit,
                                    0,
                                    &lane_name,
                                    &operation_id,
                                ));
                                events
                            })),
                        };
                    }

                    let hook_plan_is_current = placement.entries.len() == planned_entry_ids.len()
                        && placement
                            .entries
                            .iter()
                            .zip(planned_entry_ids.iter())
                            .all(|(entry, id)| entry.id() == id);
                    if hook_plan_is_current
                        && let Some((follow_up_id, follow_up_message)) = follow_up
                    {
                        let entry = NewEntry::Message(NewMessageEntry {
                            id: follow_up_id.clone(),
                            parent_id: placement.tip_id.clone(),
                            custom_type: None,
                            message: follow_up_message,
                            terminate: None,
                        });
                        let mut writes = placement.writes.clone();
                        let entry_write_index = writes.len();
                        writes.push(insert_entry(entry.clone()));
                        writes.push(Write::Value(set_value(
                            &branch_tip(&lane_name),
                            serde_json::json!(follow_up_id),
                        )));
                        let inbox = placement.inbox.clone();
                        let tip_id = Some(follow_up_id.clone());
                        let next = assistant_ready_at_boundary(
                            &config,
                            id_generator.as_ref(),
                            &state,
                            current.scope().clone(),
                            follow_up_id,
                            false,
                        );
                        return OperationCommand::Commit {
                            writes,
                            operation_state: next,
                            lane: Some(crate::harness::runtime::types::LanePatch {
                                tip_id,
                                configuration: None,
                                inbox: Some(inbox),
                            }),
                            materialize: Box::new(|_| ProcedureResult::Continue),
                            events: Some(Box::new(move |commit| {
                                let mut events = pending_events.clone();
                                events.extend(boundary_placement_events(
                                    &placement,
                                    &commit,
                                    0,
                                    &lane_name,
                                    &operation_id,
                                ));
                                let seq = commit.seqs.get(entry_write_index).copied().unwrap_or(0);
                                let materialized =
                                    crate::harness::session::commit::materialize_committed_entry(
                                        &entry,
                                        seq,
                                        commit.timestamp,
                                    );
                                events.extend(entry_lifecycle_events(
                                    &materialized,
                                    &lane_name,
                                    Some(&operation_id),
                                ));
                                events
                            })),
                        };
                    }

                    let Some(tip_id) = placement.tip_id.clone() else {
                        panic!("{}", SessionInvariantError::new("Completed run has no tip"));
                    };
                    let include_final = match continuation {
                        crate::harness::session::types::Continuation::MayFinish {
                            include_final_assistant,
                        } => include_final_assistant,
                        _ => false,
                    };
                    if include_final && current.scope().latest_assistant_entry_id.is_none() {
                        panic!(
                            "{}",
                            SessionInvariantError::new(
                                "Completed run is missing its final assistant"
                            )
                        );
                    }
                    let record = operation_result_record(
                        &meta,
                        crate::harness::session::types::TerminalStatus::Completed,
                        Some(tip_id.clone()),
                        None,
                    );
                    let cleanup = operation_cleanup_writes(
                        reader.as_ref(),
                        &operation_id,
                        &current,
                        &context,
                    )
                    .await
                    .expect("operation_cleanup_writes failed");
                    let mut writes = placement.writes.clone();
                    writes.extend(cleanup);
                    let inbox = placement.inbox.clone();
                    let record_for_materialize = record.clone();
                    OperationCommand::Finish {
                        writes,
                        record: record.clone(),
                        lane: Some(crate::harness::runtime::types::LanePatch {
                            tip_id: Some(tip_id.clone()),
                            configuration: None,
                            inbox: Some(inbox),
                        }),
                        materialize: Box::new(move |_| ProcedureResult::Settled {
                            outcome: record_for_materialize,
                        }),
                        events: Some(Box::new(move |commit| {
                            let mut events = pending_events.clone();
                            events.extend(boundary_placement_events(
                                &placement,
                                &commit,
                                0,
                                &lane_name,
                                &operation_id,
                            ));
                            events.push(HarnessEvent::RunEnd {
                                lane: lane_name,
                                run_id: operation_id,
                                from_tip_id: meta.source_tip_id,
                                tip_id: Some(tip_id),
                                ended_at: record.ended_at,
                                status: crate::harness::session::types::TerminalStatus::Completed,
                                error: None,
                                recovery: None,
                            });
                            events
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
