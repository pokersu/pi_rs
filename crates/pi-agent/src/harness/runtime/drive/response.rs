//! Rust 翻译自 packages/agent/src/harness/runtime/drive/response.ts
//!
//! assistant-generation / deferred-poll 响应的分类与原子 settle。

use std::sync::{Arc, Mutex as StdMutex};

use futures::future::BoxFuture;
use pi_ai::utils::assistant_message_frame::AssistantMessageFrameEncoder;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, StopReason, is_context_overflow,
    is_recoverable_length, is_retryable_assistant_error,
};

use crate::harness::agent_harness::HarnessEvent;
use crate::harness::context::Context;
use crate::harness::execution::assistant::{AssistantResponseMetadata, AssistantStreamObserver};
use crate::harness::execution::effect_gate::GateError;
use crate::harness::runtime::drive::retry::retry_not_before;
use crate::harness::runtime::drive::structural::prepare_overflow_compaction;
use crate::harness::runtime::drive::terminal::{operation_cleanup_writes, operation_result_record};
use crate::harness::runtime::progress::{ProgressChannel, open_frame_progress};
use crate::harness::runtime::types::{
    ContinueOperationResult, Drive, Lane, LanePatch, OperationCommand, ProcedureResult,
};
use crate::harness::session::commit::{insert_entry, insert_usage, materialize_committed_entry};
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{
    Continuation, DeferredScope, NewEntry, NewMessageEntry, OperationError, OperationState,
    SettledAssistantMessage, ToolCall, UsageRow, Write,
};
use crate::harness::session::values::{
    branch_tip, delete_list, operation_preparation, pending_assistant_frames, set_value,
};
use crate::types::AgentMessage;

type AfterResponseFn = Arc<
    dyn Fn(
            SettledAssistantMessage,
            AssistantResponseMetadata,
            Context,
        ) -> BoxFuture<'static, SettledAssistantMessage>
        + Send
        + Sync,
>;
type CloseFn = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;
type EventsFn =
    Arc<dyn Fn(&crate::harness::session::types::CommitResult) -> Vec<HarnessEvent> + Send + Sync>;

/// 对应 `AssistantResponseLifecycle`。
pub struct AssistantResponseLifecycle {
    pub observer: Arc<dyn AssistantStreamObserver>,
    pub after_response: AfterResponseFn,
    pub close: CloseFn,
}

struct ResponseObserver<L: Lane> {
    lane: Arc<L>,
    progress:
        Arc<dyn ProgressChannel<pi_ai::utils::assistant_message_frame::AssistantMessageFrame>>,
    frame_encoder: StdMutex<AssistantMessageFrameEncoder>,
    lane_name: String,
    run_id: String,
    recovery: bool,
    response_entry_id: String,
}

#[async_trait::async_trait]
impl<L: Lane + 'static> AssistantStreamObserver for ResponseObserver<L> {
    async fn start(
        &self,
        message: AssistantMessage,
        event: AssistantMessageEvent,
        context: &Context,
    ) {
        if let Some(frame) = self.frame_encoder.lock().unwrap().encode(&event) {
            self.progress.write(frame);
        }
        let _ = self
            .lane
            .emit_batch(
                vec![HarnessEvent::MessageStart {
                    lane: self.lane_name.clone(),
                    run_id: Some(self.run_id.clone()),
                    message: AgentMessage::Assistant(message),
                    recovery: Some(self.recovery),
                }],
                context,
            )
            .await;
    }

    async fn update(
        &self,
        message: AssistantMessage,
        event: AssistantMessageEvent,
        context: &Context,
    ) {
        let frame = self.frame_encoder.lock().unwrap().encode(&event);
        if let Some(frame) = &frame {
            self.progress.write(frame.clone());
        }
        let _ = self
            .lane
            .emit_batch(
                vec![HarnessEvent::MessageUpdate {
                    lane: self.lane_name.clone(),
                    run_id: self.run_id.clone(),
                    message: AgentMessage::Assistant(message),
                    event,
                    frame,
                    recovery: Some(self.recovery),
                }],
                context,
            )
            .await;
    }

    async fn end(&self, message: SettledAssistantMessage, context: &Context) {
        let _ = self
            .lane
            .emit_batch(
                vec![HarnessEvent::MessageEnd {
                    lane: self.lane_name.clone(),
                    run_id: Some(self.run_id.clone()),
                    message: AgentMessage::Assistant(message),
                    entry_id: Some(self.response_entry_id.clone()),
                    recovery: Some(self.recovery),
                }],
                context,
            )
            .await;
    }
}

/// 对应 `openAssistantResponse`。
pub fn open_assistant_response<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    response_entry_id: String,
    recovery: bool,
) -> AssistantResponseLifecycle {
    let progress: Arc<
        dyn ProgressChannel<pi_ai::utils::assistant_message_frame::AssistantMessageFrame>,
    > = Arc::new(open_frame_progress(
        Arc::clone(&lane),
        drive.context.clone(),
        drive.operation_id.clone(),
        response_entry_id.clone(),
    ));
    let observer = Arc::new(ResponseObserver {
        lane: Arc::clone(&lane),
        progress: Arc::clone(&progress),
        frame_encoder: StdMutex::new(AssistantMessageFrameEncoder::new()),
        lane_name: lane.name().to_string(),
        run_id: drive.operation_id.clone(),
        recovery,
        response_entry_id: response_entry_id.clone(),
    });

    let close_progress = Arc::clone(&progress);
    let close: CloseFn = Arc::new(move || {
        let progress = Arc::clone(&close_progress);
        Box::pin(async move {
            progress.seal();
            progress.drain().await;
        })
    });

    let after_lane = Arc::clone(&lane);
    let after_operation = drive.operation_id.clone();
    let after_gate = drive.gate.clone();
    let after_context = drive.context.clone();
    let close_for_after = Arc::clone(&close);
    let after_response: AfterResponseFn = Arc::new(move |message, metadata, _context| {
        let lane = Arc::clone(&after_lane);
        let operation_id = after_operation.clone();
        let gate = after_gate.clone();
        let drive_context = after_context.clone();
        let close = Arc::clone(&close_for_after);
        Box::pin(async move {
            close().await;
            let result = lane
                .hooks()
                .run_with_gate(
                    crate::harness::hooks::HookName::AfterResponse,
                    serde_json::json!({
                        "lane": lane.name(),
                        "runId": operation_id,
                        "status": metadata.status,
                        "headers": metadata.headers,
                        "message": message,
                    }),
                    &gate,
                    &drive_context,
                )
                .await;
            match result {
                Ok(result) => result
                    .get("message")
                    .and_then(|m| serde_json::from_value(m.clone()).ok())
                    .unwrap_or(message),
                Err(_) => message,
            }
        })
    });

    AssistantResponseLifecycle {
        observer,
        after_response,
        close,
    }
}

/// 对应 `publishConfigurationFailure`。
pub async fn publish_configuration_failure<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    capability: &OperationState,
    error: OperationError,
) -> Result<ProcedureResult, GateError> {
    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let drive_context = drive.context.clone();

    let result = lane
        .continue_operation(
            capability,
            Box::new(move |state, current, meta, reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let context = drive_context.clone();
                let error = error.clone();
                Box::pin(async move {
                    let Some(tip_id) = state.tip_id.clone() else {
                        panic!(
                            "{}",
                            SessionInvariantError::new("Failed run has no Branch tip")
                        );
                    };
                    let record = operation_result_record(
                        &meta,
                        crate::harness::session::types::TerminalStatus::Failed,
                        Some(tip_id.clone()),
                        Some(error.clone()),
                    );
                    let cleanup = operation_cleanup_writes(
                        reader.as_ref(),
                        &operation_id,
                        &current,
                        &context,
                    )
                    .await
                    .expect("operation_cleanup_writes failed");
                    let record_for_materialize = record.clone();
                    let record_for_events = record.clone();
                    OperationCommand::Finish {
                        writes: cleanup,
                        record,
                        lane: Some(LanePatch {
                            tip_id: Some(tip_id.clone()),
                            configuration: None,
                            inbox: None,
                        }),
                        materialize: Box::new(move |_| ProcedureResult::Settled {
                            outcome: record_for_materialize,
                        }),
                        events: Some(Box::new(move |_| {
                            vec![HarnessEvent::RunEnd {
                                lane: lane_name,
                                run_id: operation_id,
                                from_tip_id: meta.source_tip_id,
                                tip_id: Some(tip_id),
                                ended_at: record_for_events.ended_at,
                                status: crate::harness::session::types::TerminalStatus::Failed,
                                error: Some(error),
                                recovery: None,
                            }]
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

fn uuid_v7_timestamp(id: &str) -> u64 {
    let hex = format!("{}{}", &id[0..8], &id[9..13]);
    let timestamp = u64::from_str_radix(&hex, 16).unwrap_or(u64::MAX);
    if timestamp > 9_007_199_254_740_991 {
        panic!(
            "{}",
            SessionInvariantError::new(format!("Invalid reserved UUIDv7 {id}"))
        );
    }
    timestamp
}

fn provider_error(source: &str, message: &SettledAssistantMessage) -> OperationError {
    OperationError {
        code: "assistant_error".to_string(),
        message: message.error_message.clone().unwrap_or_else(|| {
            let prefix = if source == "assistant" {
                "Assistant"
            } else {
                "Deferred"
            };
            format!("{prefix} request ended with {:?}", message.stop_reason)
        }),
        details: None,
    }
}

fn normalize_error(
    message: &SettledAssistantMessage,
    error_message: &str,
) -> SettledAssistantMessage {
    let mut normalized = message.clone();
    normalized.stop_reason = StopReason::Error;
    normalized.error_message = Some(error_message.to_string());
    normalized
}

fn normalize_aborted(source: &str, message: &SettledAssistantMessage) -> SettledAssistantMessage {
    let mut normalized = message.clone();
    normalized.stop_reason = StopReason::Aborted;
    normalized.error_message = Some(message.error_message.clone().unwrap_or_else(|| {
        let prefix = if source == "assistant" {
            "Assistant"
        } else {
            "Deferred"
        };
        format!("{prefix} request was cancelled")
    }));
    normalized
}

fn deferred_handle_is_valid(
    message: &SettledAssistantMessage,
    configuration: &crate::harness::session::types::LaneConfiguration,
) -> bool {
    let Some(handle) = &message.deferred else {
        return false;
    };
    message.stop_reason == StopReason::Deferred
        && !handle.id.is_empty()
        && handle.provider == configuration.model.provider
        && handle.model_id == configuration.model.model_id
        && handle.api == message.api
}

/// 对应 `publishResponse`：分类并原子 settle 一个 assistant-generation 或 deferred-poll 响应。
#[allow(clippy::too_many_arguments)]
pub async fn publish_response<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    intent: &OperationState,
    response: SettledAssistantMessage,
    options: Option<bool>,
) -> Result<ProcedureResult, GateError> {
    let recovery = options.unwrap_or(false);
    let overflow = matches!(intent, OperationState::AssistantEffectPending { .. })
        && (is_context_overflow(&response, intent_context_window(intent))
            || is_recoverable_length(&response, intent_output_limit(intent)));

    let overflow_preparation = if overflow && !intent_overflow_recovery_used(intent) {
        prepare_overflow_compaction(&*lane, drive, intent)
            .await
            .map_err(GateError::Closed)?
    } else {
        None
    };

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let drive_context = drive.context.clone();
    let id_generator = lane.session().id_generator();
    let response_entry_id = intent_response_entry_id(intent);
    let usage_id = intent_usage_id(intent);

    let result = lane
        .settle_operation(
            intent,
            Box::new(move |state, current, meta, reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let context = drive_context.clone();
                let id_generator = id_generator.clone();
                let overflow_preparation = overflow_preparation.clone();
                let response_entry_id = response_entry_id.clone();
                let usage_id = usage_id.clone();
                Box::pin(async move {
                    let source = if matches!(current, OperationState::AssistantEffectPending { .. }) {
                        "assistant"
                    } else {
                        "deferred"
                    };
                    let (configuration, turn_id, scope) = match &current {
                        OperationState::AssistantEffectPending {
                            scope,
                            generation_context,
                            ..
                        } => (
                            generation_context.configuration.clone(),
                            generation_context.step_id.clone(),
                            scope.clone(),
                        ),
                        OperationState::DeferredEffectPending { scope, .. } => (
                            scope.configuration.clone(),
                            format!("{}:poll:{}", scope.step_id, scope.poll),
                            crate::harness::session::types::OperationScope {
                                control: scope.control.clone(),
                                settings: scope.settings.clone(),
                                latest_assistant_entry_id: scope.latest_assistant_entry_id.clone(),
                            },
                        ),
                        _ => panic!(
                            "{}",
                            SessionInvariantError::new("Response intent is not an effect-pending state")
                        ),
                    };
                    let mut scope = scope;
                    scope.latest_assistant_entry_id = Some(response_entry_id.clone());

                    let mut committed = response.clone();
                    let mut settled: Option<OperationState> = None;
                    let mut failure: Option<OperationError> = None;

                    if matches!(
                        scope.control,
                        crate::harness::session::types::Control::CancelRequested { .. }
                    ) {
                        committed = normalize_aborted(source, &response);
                        settled = Some(OperationState::Checkpoint {
                            scope: scope.clone(),
                            continuation: Continuation::MayFinish {
                                include_final_assistant: true,
                            },
                            trigger_entry_id: response_entry_id.clone(),
                        });
                    } else if response.stop_reason == StopReason::Aborted {
                        panic!(
                            "{}",
                            SessionInvariantError::new(format!(
                                "{} response is aborted while durable control is running",
                                if source == "assistant" { "Assistant" } else { "Deferred" }
                            ))
                        );
                    } else if matches!(current, OperationState::AssistantEffectPending { .. }) && overflow {
                        committed = normalize_error(
                            &response,
                            response
                                .error_message
                                .as_deref()
                                .unwrap_or("Assistant request exceeded the context window"),
                        );
                        let (overflow_recovery_used, generation_trigger) = match &current {
                            OperationState::AssistantEffectPending {
                                generation_context, ..
                            } => (
                                generation_context.overflow_recovery_used,
                                generation_context.trigger_entry_id.clone(),
                            ),
                            _ => unreachable!(),
                        };
                        if overflow_recovery_used || overflow_preparation.is_none() {
                            failure = Some(provider_error(source, &committed));
                        } else {
                            let (task_id, _preparation) = overflow_preparation.clone().unwrap();
                            settled = Some(OperationState::SummaryDeciding {
                                scope: scope.clone(),
                                task: crate::harness::session::types::SummaryTask {
                                    task_id,
                                    reason: Some("overflow".to_string()),
                                    custom_instructions: None,
                                    boundary: crate::harness::session::types::ResultBoundary::ResumeCheckpoint {
                                        resume_after: crate::harness::session::types::CheckpointData {
                                            continuation: Continuation::NeedAssistant {
                                                overflow_recovery_used: true,
                                            },
                                            trigger_entry_id: generation_trigger,
                                        },
                                    },
                                },
                            });
                        }
                    } else if response.stop_reason == StopReason::Deferred {
                        if matches!(current, OperationState::AssistantEffectPending { .. }) {
                            let configuration = configuration.clone();
                            if deferred_handle_is_valid(&response, &configuration) {
                                let (step_id, stream_options) = match &current {
                                    OperationState::AssistantEffectPending {
                                        generation_context, ..
                                    } => (
                                        generation_context.step_id.clone(),
                                        generation_context.stream_options.clone(),
                                    ),
                                    _ => unreachable!(),
                                };
                                settled = Some(OperationState::DeferredSuspended {
                                    scope: DeferredScope {
                                        control: scope.control.clone(),
                                        settings: scope.settings.clone(),
                                        latest_assistant_entry_id: Some(response_entry_id.clone()),
                                        step_id,
                                        source_entry_id: response_entry_id.clone(),
                                        poll: 0,
                                        configuration,
                                        stream_options,
                                    },
                                });
                            } else {
                                committed = normalize_error(
                                    &response,
                                    "Provider returned an invalid deferred handle",
                                );
                                failure = Some(provider_error(source, &committed));
                            }
                        } else {
                            let (step_id, source_entry_id, poll, stream_options) = match &current {
                                OperationState::DeferredEffectPending { scope, .. } => (
                                    scope.step_id.clone(),
                                    scope.source_entry_id.clone(),
                                    scope.poll,
                                    scope.stream_options.clone(),
                                ),
                                _ => unreachable!(),
                            };
                            settled = Some(OperationState::DeferredSuspended {
                                scope: DeferredScope {
                                    control: scope.control.clone(),
                                    settings: scope.settings.clone(),
                                    latest_assistant_entry_id: Some(response_entry_id.clone()),
                                    step_id,
                                    source_entry_id,
                                    poll,
                                    configuration,
                                    stream_options,
                                },
                            });
                        }
                    } else if response.stop_reason == StopReason::Error {
                        let (attempt, max_attempts, base_delay) = match &current {
                            OperationState::AssistantEffectPending {
                                attempt,
                                generation_context,
                                ..
                            } => (
                                *attempt,
                                generation_context.retry_policy.max_attempts,
                                generation_context.retry_policy.base_delay_ms,
                            ),
                            _ => (0, 1, 0),
                        };
                        if matches!(current, OperationState::AssistantEffectPending { .. })
                            && (recovery || is_retryable_assistant_error(&response))
                            && attempt < max_attempts
                        {
                            let generation_context = match &current {
                                OperationState::AssistantEffectPending {
                                    generation_context, ..
                                } => generation_context.clone(),
                                _ => unreachable!(),
                            };
                            settled = Some(OperationState::AssistantRetryWait {
                                scope: scope.clone(),
                                generation_context,
                                next_attempt: attempt + 1,
                                not_before: retry_not_before(
                                    base_delay,
                                    attempt,
                                    pi_ai::utils::uuid::now_ms() as u64,
                                ),
                                error_message: response
                                    .error_message
                                    .clone()
                                    .unwrap_or_else(|| "Assistant request failed".to_string()),
                            });
                        } else {
                            failure = Some(provider_error(source, &response));
                        }
                    } else {
                        let mut calls: Vec<(usize, String)> = Vec::new();
                        for (source_index, content) in response.content.iter().enumerate() {
                            if matches!(content, pi_ai::ContentBlock::ToolCall(_)) {
                                calls.push((source_index, id_generator.next(Some(uuid_v7_timestamp(&response_entry_id)))));
                            }
                        }
                        if !calls.is_empty() {
                            let planned: Vec<ToolCall> = calls
                                .into_iter()
                                .map(|(source_index, result_entry_id)| ToolCall::Planned {
                                    source_index,
                                    result_entry_id,
                                })
                                .collect();
                            settled = Some(OperationState::Tools {
                                scope: scope.clone(),
                                batch: crate::harness::session::types::ToolBatch {
                                    assistant_entry_id: response_entry_id.clone(),
                                    configuration,
                                    turn_id,
                                    calls: planned,
                                },
                            });
                        } else if response.stop_reason == StopReason::ToolUse {
                            committed = normalize_error(
                                &response,
                                "Provider reported tool use without any tool calls",
                            );
                            failure = Some(provider_error(source, &committed));
                        } else {
                            settled = Some(OperationState::Checkpoint {
                                scope: scope.clone(),
                                continuation: Continuation::MayFinish {
                                    include_final_assistant: true,
                                },
                                trigger_entry_id: response_entry_id.clone(),
                            });
                        }
                    }

                    let response_entry = NewEntry::Message(NewMessageEntry {
                        id: response_entry_id.clone(),
                        parent_id: state.tip_id.clone(),
                        custom_type: None,
                        message: crate::types::AgentMessage::Assistant(committed.clone()),
                        terminate: None,
                    });
                    let usage_row = UsageRow {
                        id: usage_id.clone(),
                        seq: 0,
                        usage: committed.usage.clone(),
                        entry_id: Some(response_entry_id.clone()),
                        adjustment: false,
                        details: None,
                    };
                    if settled.is_none() && failure.is_none() {
                        panic!(
                            "{}",
                            SessionInvariantError::new("Response settlement has no durable disposition")
                        );
                    }
                    let record = failure.as_ref().map(|failure| {
                        operation_result_record(
                            &meta,
                            crate::harness::session::types::TerminalStatus::Failed,
                            Some(response_entry_id.clone()),
                            Some(failure.clone()),
                        )
                    });
                    let cleanup = if record.is_some() {
                        operation_cleanup_writes(reader.as_ref(), &operation_id, &current, &context)
                            .await
                            .expect("operation_cleanup_writes failed")
                    } else {
                        Vec::new()
                    };

                    let mut writes: Vec<Write> = vec![
                        insert_entry(response_entry.clone()),
                        insert_usage(usage_row.clone()),
                        Write::Value(set_value(
                            &branch_tip(&lane_name),
                            serde_json::json!(response_entry_id),
                        )),
                    ];
                    if record.is_none() {
                        writes.push(Write::List(delete_list(&pending_assistant_frames(
                            &operation_id,
                            &response_entry_id,
                        ))));
                    } else {
                        writes.extend(cleanup);
                    }
                    if matches!(&settled, Some(OperationState::SummaryDeciding { .. }))
                        && let Some((task_id, preparation)) = &overflow_preparation
                    {
                        writes.push(Write::Value(set_value(
                            &operation_preparation(&operation_id, task_id),
                            serde_json::to_value(preparation).unwrap_or(serde_json::Value::Null),
                        )));
                    }

                    let events_fn: EventsFn = {
                        let lane_name = lane_name.clone();
                        let response_entry = response_entry.clone();
                        let usage_row = usage_row.clone();
                        Arc::new(move |commit| {
                            let mut row = usage_row.clone();
                            row.seq = commit.seqs.get(1).copied().unwrap_or(0);
                            let seq = commit.seqs.first().copied().unwrap_or(0);
                            let materialized =
                                materialize_committed_entry(&response_entry, seq, commit.timestamp);
                            vec![
                                HarnessEvent::EntryAdded {
                                    lane: lane_name.clone(),
                                    entry: materialized,
                                    recovery: Some(recovery),
                                },
                                HarnessEvent::Usage {
                                    lane: lane_name.clone(),
                                    row,
                                    totals: commit.stats.usage.clone(),
                                },
                            ]
                        })
                    };

                    if let Some(record) = record {
                        let record_for_field = record.clone();
                        let record_for_materialize = record.clone();
                        let record_for_events = record.clone();
                        let events_fn = Arc::clone(&events_fn);
                        let lane_name = lane_name.clone();
                        let operation_id = operation_id.clone();
                        let response_entry_id = response_entry_id.clone();
                        let meta_source_tip = meta.source_tip_id.clone();
                        let failure = failure.clone();
                        return OperationCommand::Finish {
                            writes,
                            record: record_for_field,
                            lane: Some(LanePatch {
                                tip_id: Some(response_entry_id.clone()),
                                configuration: None,
                                inbox: None,
                            }),
                            materialize: Box::new(move |_| ProcedureResult::Settled {
                                outcome: record_for_materialize,
                            }),
                            events: Some(Box::new(move |commit| {
                                let mut batch = events_fn(&commit);
                                if let Some(failure) = &failure {
                                    batch.push(HarnessEvent::RunEnd {
                                        lane: lane_name,
                                        run_id: operation_id,
                                        from_tip_id: meta_source_tip,
                                        tip_id: Some(response_entry_id),
                                        ended_at: record_for_events.ended_at,
                                        status: crate::harness::session::types::TerminalStatus::Failed,
                                        error: Some(failure.clone()),
                                        recovery: None,
                                    });
                                }
                                batch
                            })),
                        };
                    }

                    let Some(settled) = settled else {
                        panic!(
                            "{}",
                            SessionInvariantError::new("Response settlement is missing its next state")
                        );
                    };
                    OperationCommand::Commit {
                        writes,
                        operation_state: settled,
                        lane: Some(LanePatch {
                            tip_id: Some(response_entry_id.clone()),
                            configuration: None,
                            inbox: None,
                        }),
                        materialize: Box::new(|_| ProcedureResult::Continue),
                        events: Some(Box::new(move |commit| events_fn(&commit))),
                    }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;

    Ok(result)
}

fn intent_response_entry_id(intent: &OperationState) -> String {
    match intent {
        OperationState::AssistantEffectPending {
            response_entry_id, ..
        }
        | OperationState::DeferredEffectPending {
            response_entry_id, ..
        } => response_entry_id.clone(),
        _ => String::new(),
    }
}

fn intent_usage_id(intent: &OperationState) -> String {
    match intent {
        OperationState::AssistantEffectPending { usage_id, .. }
        | OperationState::DeferredEffectPending { usage_id, .. } => usage_id.clone(),
        _ => String::new(),
    }
}

fn intent_context_window(intent: &OperationState) -> Option<u64> {
    match intent {
        OperationState::AssistantEffectPending { context_window, .. } => Some(*context_window),
        _ => None,
    }
}

fn intent_output_limit(intent: &OperationState) -> u64 {
    match intent {
        OperationState::AssistantEffectPending {
            intended_output_limit,
            ..
        } => *intended_output_limit,
        _ => 0,
    }
}

fn intent_overflow_recovery_used(intent: &OperationState) -> bool {
    match intent {
        OperationState::AssistantEffectPending {
            generation_context, ..
        } => generation_context.overflow_recovery_used,
        _ => false,
    }
}
