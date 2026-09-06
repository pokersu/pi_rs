//! Rust 翻译自 packages/agent/src/harness/runtime/drive/tools.ts
//!
//! 一个完整 durable tool batch 的执行、恢复、stage 与源序放置。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use futures::future::BoxFuture;
use pi_ai::{TextContent, TextKind, TextOrImageContent, ToolResultMessage};

use crate::harness::agent_harness::HarnessEvent;
use crate::harness::context::Context;
use crate::harness::execution::effect_gate::GateError;
use crate::harness::execution::tools::{
    ClearedToolCall, FinalizedToolCall, apply_before_tool_decision, create_tool_result_message,
    execute_tool_call, finalize_tool_call, prepare_tool_call, tool_result_from_message,
};
use crate::harness::runtime::drive::tool_placement::{
    ToolBatchSource, materialize_ready, read_tool_batch_source, tool_call_for, with_tool_batch,
};
use crate::harness::runtime::progress::{ProgressChannel, open_tool_progress};
use crate::harness::runtime::types::{
    ContinueOperationResult, Drive, Lane, LaneRuntimeState, OperationCommand, ProcedureResult,
};
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{OperationState, ToolBatch, ToolCall, Write};
use crate::harness::session::values::{
    delete_value, operation_tool_args, operation_tool_memo, operation_tool_memo_prefix,
    pending_entry, pending_tool_output, set_value,
};
use crate::harness::types::{
    AgentHarnessTool, AgentHarnessToolInvocation, AgentHarnessToolUpdateCallback,
};
use crate::types::AgentToolResult;

const INTERRUPTION_MARKER: &str = "[Tool execution was interrupted. The preceding output is the latest durable progress snapshot; newer live output may be missing, and the external outcome is unknown.]";

struct ToolOutcome {
    tool_call: pi_ai::ToolCall,
    message: ToolResultMessage,
    terminate: bool,
}

type ToolCallTask = BoxFuture<'static, ()>;

enum PreparedToolInvocation {
    Ready { cleared: ClearedToolCall },
    Outcome { outcome: ToolOutcome },
}

fn synthetic_message(
    tool_call: &pi_ai::ToolCall,
    content: Vec<TextOrImageContent>,
) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        content,
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: true,
        timestamp: pi_ai::utils::uuid::now_ms() as u64,
    }
}

fn aborted_outcome(tool_call: pi_ai::ToolCall) -> ToolOutcome {
    ToolOutcome {
        tool_call: tool_call.clone(),
        message: synthetic_message(
            &tool_call,
            vec![TextOrImageContent::Text(TextContent {
                kind: TextKind,
                text: "Tool execution was cancelled before completion.".to_string(),
                text_signature: None,
            })],
        ),
        terminate: false,
    }
}

fn interrupted_outcome(
    tool_call: pi_ai::ToolCall,
    checkpoint: Option<AgentToolResult>,
) -> ToolOutcome {
    let mut content: Vec<TextOrImageContent> = checkpoint
        .as_ref()
        .map(|c| c.content.clone())
        .unwrap_or_default();
    content.push(TextOrImageContent::Text(TextContent {
        kind: TextKind,
        text: INTERRUPTION_MARKER.to_string(),
        text_signature: None,
    }));
    let mut message = synthetic_message(&tool_call, content);
    if let Some(checkpoint) = checkpoint {
        message.details = if checkpoint.details.is_null() {
            None
        } else {
            Some(checkpoint.details)
        };
        message.usage = checkpoint.usage;
    }
    ToolOutcome {
        tool_call: tool_call.clone(),
        message,
        terminate: false,
    }
}

fn truncated_outcome(tool_call: pi_ai::ToolCall) -> ToolOutcome {
    ToolOutcome {
        tool_call: tool_call.clone(),
        message: synthetic_message(
            &tool_call,
            vec![TextOrImageContent::Text(TextContent {
                kind: TextKind,
                text: format!(
                    "Tool call {:?} was not executed because the assistant response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                    tool_call.name
                ),
                text_signature: None,
            })],
        ),
        terminate: false,
    }
}

fn outcome_from_finalized_call(finalized: FinalizedToolCall) -> ToolOutcome {
    let terminate = finalized.terminate;
    let message = create_tool_result_message(&finalized);
    ToolOutcome {
        tool_call: finalized.tool_call,
        message,
        terminate,
    }
}

fn current_batch(state: &LaneRuntimeState) -> Option<OperationState> {
    let operation = state.operation.as_ref()?;
    if matches!(operation.state, OperationState::Tools { .. }) {
        Some(operation.state.clone())
    } else {
        None
    }
}

fn replace_call(batch: &ToolBatch, replacement: ToolCall) -> ToolBatch {
    ToolBatch {
        assistant_entry_id: batch.assistant_entry_id.clone(),
        configuration: batch.configuration.clone(),
        turn_id: batch.turn_id.clone(),
        calls: batch
            .calls
            .iter()
            .map(|call| {
                if call.source_index() == replacement.source_index()
                    && call.result_entry_id() == replacement.result_entry_id()
                {
                    replacement.clone()
                } else {
                    call.clone()
                }
            })
            .collect(),
    }
}

fn validate_memo_name(name: &str) {
    if name.is_empty() {
        panic!("Tool invocation memo name must not be empty");
    }
    if name.contains(':') {
        panic!("Tool invocation memo name must not contain ':'");
    }
}

struct InvocationCapability<L: Lane> {
    lane: Arc<L>,
    drive_context: Context,
    operation_id: String,
    turn_id: String,
    invocation_id: String,
    result_entry_id: String,
    active: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl<L: Lane + 'static> AgentHarnessToolInvocation for InvocationCapability<L> {
    fn invocation_id(&self) -> &str {
        &self.invocation_id
    }
    fn operation_id(&self) -> &str {
        &self.operation_id
    }
    fn turn_id(&self) -> &str {
        &self.turn_id
    }
    async fn get_memo(&self, name: &str) -> Option<serde_json::Value> {
        validate_memo_name(name);
        if !self.active.load(Ordering::SeqCst) {
            panic!("Tool invocation no longer owns its durable effect");
        }
        let address = operation_tool_memo(&self.operation_id, &self.result_entry_id, name);
        let context = self.drive_context.clone();
        self.lane
            .command(
                Box::new(move |_state, reader| {
                    let address = address.clone();
                    let context = context.clone();
                    Box::pin(async move {
                        let stored = reader
                            .get_value(&address, &context)
                            .await
                            .expect("get_value");
                        crate::harness::runtime::types::LaneCommand::Return {
                            result: stored.map(|s| s.value),
                        }
                    })
                }),
                &self.drive_context,
            )
            .await
            .unwrap_or(None)
    }
    async fn set_memo(&self, name: &str, value: Option<serde_json::Value>) {
        validate_memo_name(name);
        if !self.active.load(Ordering::SeqCst) {
            panic!("Tool invocation no longer owns its durable effect");
        }
        let address = operation_tool_memo(&self.operation_id, &self.result_entry_id, name);
        let _ = self
            .lane
            .command(
                Box::new(move |state, _reader| {
                    let address = address.clone();
                    let value = value.clone();
                    Box::pin(async move {
                        let writes = match value {
                            Some(v) => vec![Write::Value(set_value(&address, v))],
                            None => vec![Write::Value(delete_value(&address))],
                        };
                        crate::harness::runtime::types::LaneCommand::Commit {
                            writes,
                            next: state,
                            materialize: Box::new(|_| ()),
                            events: None,
                        }
                    })
                }),
                &self.drive_context,
            )
            .await;
    }
}

/// 对应 `publishToolIntent`。
#[allow(clippy::too_many_arguments)]
async fn publish_tool_intent<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    run: &OperationState,
    planned: &ToolCall,
    tool_call: &pi_ai::ToolCall,
    args: serde_json::Value,
    replay: &crate::harness::session::types::ReplayPolicy,
    recovery: bool,
) -> Result<ContinueOperationResult<ToolCall>, String> {
    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let turn_id = match run {
        OperationState::Tools { batch, .. } => batch.turn_id.clone(),
        _ => String::new(),
    };
    let planned_source = planned.source_index();
    let planned_result = planned.result_entry_id();
    let replay = *replay;
    let tool_call = tool_call.clone();
    lane.continue_operation(
        run,
        Box::new(move |_state, current, _meta, _reader| {
            let lane_name = lane_name.clone();
            let operation_id = operation_id.clone();
            let turn_id = turn_id.clone();
            let args = args.clone();
            let tool_call = tool_call.clone();
            Box::pin(async move {
                let OperationState::Tools { batch, .. } = &current else {
                    panic!(
                        "{}",
                        SessionInvariantError::new("Tool intent requires a tools state")
                    );
                };
                let effect_pending = ToolCall::EffectPending {
                    source_index: planned_source,
                    result_entry_id: planned_result.clone(),
                    replay,
                };
                let next = with_tool_batch(&current, replace_call(batch, effect_pending.clone()));
                OperationCommand::Commit {
                    writes: vec![Write::Value(set_value(
                        &operation_tool_args(&operation_id, &turn_id, planned_source),
                        args.clone(),
                    ))],
                    operation_state: next,
                    lane: None,
                    materialize: Box::new(move |_| effect_pending),
                    events: Some(Box::new(move |_| {
                        vec![HarnessEvent::ToolStart {
                            lane: lane_name,
                            run_id: operation_id,
                            turn_id,
                            tool_call_id: tool_call.id,
                            tool_name: tool_call.name,
                            args,
                            recovery: Some(recovery),
                        }]
                    })),
                }
            })
        }),
        &drive.context,
    )
    .await
}

async fn publish_tool_outcome<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    capability: &OperationState,
    call: &ToolCall,
    finalized: &ToolOutcome,
    recovery: bool,
) -> Result<(), String> {
    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let turn_id = match capability {
        OperationState::Tools { batch, .. } => batch.turn_id.clone(),
        _ => String::new(),
    };
    let call_source = call.source_index();
    let call_result = call.result_entry_id();
    let tool_call = finalized.tool_call.clone();
    let message = finalized.message.clone();
    let terminate = finalized.terminate;
    let was_planned = matches!(call, ToolCall::Planned { .. });
    let drive_context = drive.context.clone();

    lane.settle_operation(
        capability,
        Box::new(move |_state, run, _meta, reader| {
            let lane_name = lane_name.clone();
            let operation_id = operation_id.clone();
            let turn_id = turn_id.clone();
            let tool_call = tool_call.clone();
            let message = message.clone();
            let context = drive_context.clone();
            Box::pin(async move {
                let OperationState::Tools { batch, .. } = &run else {
                    panic!(
                        "{}",
                        SessionInvariantError::new("Tool outcome requires a tools state")
                    );
                };
                let durable_terminate = matches!(
                    run.scope().control,
                    crate::harness::session::types::Control::Running
                ) && terminate;
                let memos = reader
                    .scan_values(
                        &operation_tool_memo_prefix(&operation_id, Some(&call_result)),
                        &context,
                    )
                    .await
                    .expect("scan_values");
                let outcome = ToolCall::OutcomeReady {
                    source_index: call_source,
                    result_entry_id: call_result.clone(),
                    terminate: durable_terminate,
                };
                let mut writes: Vec<Write> = vec![
                    Write::Value(set_value(
                        &pending_entry(&call_result),
                        serde_json::json!({ "type": "message", "payload": message }),
                    )),
                    Write::Value(delete_value(&pending_tool_output(
                        &operation_id,
                        &call_result,
                    ))),
                ];
                for stored in memos {
                    writes.push(Write::Value(delete_value(&stored.address)));
                }
                let next = with_tool_batch(&run, replace_call(batch, outcome));
                OperationCommand::Commit {
                    writes,
                    operation_state: next,
                    lane: None,
                    materialize: Box::new(|_| ()),
                    events: Some(Box::new(move |_| {
                        let mut events: Vec<HarnessEvent> = Vec::new();
                        if was_planned {
                            events.push(HarnessEvent::ToolStart {
                                lane: lane_name.clone(),
                                run_id: operation_id.clone(),
                                turn_id: turn_id.clone(),
                                tool_call_id: tool_call.id.clone(),
                                tool_name: tool_call.name.clone(),
                                args: tool_call.arguments.clone(),
                                recovery: Some(recovery),
                            });
                        }
                        events.push(HarnessEvent::ToolEnd {
                            lane: lane_name.clone(),
                            run_id: operation_id.clone(),
                            turn_id: turn_id.clone(),
                            tool_call_id: tool_call.id.clone(),
                            tool_name: tool_call.name.clone(),
                            result: tool_result_from_message(&message, durable_terminate),
                            is_error: message.is_error,
                            terminate: durable_terminate,
                            recovery: Some(recovery),
                        });
                        events
                    })),
                }
            })
        }),
        &drive.context,
    )
    .await
}

async fn clear_replay_checkpoint<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    batch: &ToolBatch,
    call: &ToolCall,
    tool_call: &pi_ai::ToolCall,
) -> Result<serde_json::Value, String> {
    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let turn_id = batch.turn_id.clone();
    let source_index = call.source_index();
    let result_entry_id = call.result_entry_id();
    let tool_call = tool_call.clone();
    let drive_context = drive.context.clone();
    lane.command(
        Box::new(move |state, reader| {
            let lane_name = lane_name.clone();
            let operation_id = operation_id.clone();
            let turn_id = turn_id.clone();
            let tool_call = tool_call.clone();
            let context = drive_context.clone();
            Box::pin(async move {
                let address = operation_tool_args(&operation_id, &turn_id, source_index);
                let stored = reader
                    .get_value(&address, &context)
                    .await
                    .expect("get_value");
                let Some(stored) = stored else {
                    panic!(
                        "{}",
                        SessionInvariantError::new(format!(
                            "Tool call {result_entry_id} is missing persisted arguments"
                        ))
                    );
                };
                let stored_value = stored.value.clone();
                crate::harness::runtime::types::LaneCommand::Commit {
                    writes: vec![Write::Value(delete_value(&pending_tool_output(
                        &operation_id,
                        &result_entry_id,
                    )))],
                    next: state,
                    materialize: Box::new(move |_| stored_value),
                    events: Some(Box::new(move |_| {
                        vec![HarnessEvent::ToolStart {
                            lane: lane_name,
                            run_id: operation_id,
                            turn_id,
                            tool_call_id: tool_call.id,
                            tool_name: tool_call.name,
                            args: stored.value,
                            recovery: Some(true),
                        }]
                    })),
                }
            })
        }),
        &drive.context,
    )
    .await
}

async fn read_checkpoint<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    call: &ToolCall,
) -> Result<Option<AgentToolResult>, String> {
    let operation_id = drive.operation_id.clone();
    let result_entry_id = call.result_entry_id();
    let drive_context = drive.context.clone();
    lane.command(
        Box::new(move |_state, reader| {
            let operation_id = operation_id.clone();
            let result_entry_id = result_entry_id.clone();
            let context = drive_context.clone();
            Box::pin(async move {
                let stored = reader
                    .get_value(
                        &pending_tool_output(&operation_id, &result_entry_id).erased(),
                        &context,
                    )
                    .await
                    .expect("get_value");
                let result = stored.and_then(|s| serde_json::from_value(s.value).ok());
                crate::harness::runtime::types::LaneCommand::Return { result }
            })
        }),
        &drive.context,
    )
    .await
}

async fn perform_tool_invocation<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    batch: &ToolBatch,
    call: &ToolCall,
    cleared: &ClearedToolCall,
    recovery: bool,
) -> Result<ToolOutcome, GateError> {
    let active = Arc::new(AtomicBool::new(true));
    let capability = InvocationCapability {
        lane: Arc::clone(&lane),
        drive_context: drive.context.clone(),
        operation_id: drive.operation_id.clone(),
        turn_id: batch.turn_id.clone(),
        invocation_id: call.result_entry_id(),
        result_entry_id: call.result_entry_id(),
        active: Arc::clone(&active),
    };
    let invocation: Arc<dyn AgentHarnessToolInvocation> = Arc::new(capability);
    let progress: Arc<dyn ProgressChannel<AgentToolResult>> = Arc::new(open_tool_progress(
        Arc::clone(&lane),
        drive.context.clone(),
        drive.operation_id.clone(),
        batch.turn_id.clone(),
        call.source_index(),
        call.result_entry_id(),
    ));

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let turn_id = batch.turn_id.clone();
    let tool_call_id = cleared.tool_call.id.clone();
    let tool_call_name = cleared.tool_call.name.clone();

    let latest_update: Arc<StdMutex<Option<BoxFuture<'static, ()>>>> =
        Arc::new(StdMutex::new(None));
    let publish_update = {
        let lane = Arc::clone(&lane);
        let latest_update = Arc::clone(&latest_update);
        let drive_context = drive.context.clone();
        move |partial: AgentToolResult| {
            let lane = Arc::clone(&lane);
            let drive_context = drive_context.clone();
            let lane_name = lane_name.clone();
            let operation_id = operation_id.clone();
            let turn_id = turn_id.clone();
            let tool_call_id = tool_call_id.clone();
            let tool_call_name = tool_call_name.clone();
            let fut: BoxFuture<'static, ()> = Box::pin(async move {
                let _ = lane
                    .emit_batch(
                        vec![HarnessEvent::ToolUpdate {
                            lane: lane_name,
                            run_id: operation_id,
                            turn_id,
                            tool_call_id,
                            tool_name: tool_call_name,
                            partial_result: partial,
                            recovery: Some(recovery),
                        }],
                        &drive_context,
                    )
                    .await;
            });
            *latest_update.lock().unwrap() = Some(fut);
        }
    };

    let gate = drive.gate.clone();
    let cleared = cleared.clone();
    let cleared_tool_call = cleared.tool_call.clone();
    let cleared_for_finalize = cleared.clone();
    let context = drive.context.clone();
    let progress_for_write = Arc::clone(&progress);
    let on_update: AgentHarnessToolUpdateCallback = Box::new(move |partial, options| {
        publish_update(partial.clone());
        if options.map(|o| o.checkpoint).unwrap_or(false) {
            progress_for_write.write(partial);
        }
    });

    let executed = execute_tool_call(cleared, &gate, on_update, invocation, &context).await;

    active.store(false, Ordering::SeqCst);
    progress.seal();
    let latest_fut = latest_update.lock().unwrap().take();
    if let Some(fut) = latest_fut {
        fut.await;
    }
    progress.drain().await;

    let executed = match executed {
        Ok(executed) => executed,
        Err(GateError::AbortRequested(e)) => {
            let _ = (e.cancellation)().await;
            return Ok(if recovery {
                interrupted_outcome(cleared_tool_call.clone(), None)
            } else {
                aborted_outcome(cleared_tool_call.clone())
            });
        }
        Err(GateError::Closed(e)) => return Err(GateError::Closed(e)),
    };

    let patch = lane
        .hooks()
        .run_tool_with_gate(
            crate::harness::hooks::HookName::AfterTool,
            serde_json::json!({
                "lane": lane.name(),
                "runId": drive.operation_id,
                "toolCallId": cleared_for_finalize.tool_call.id,
                "toolName": cleared_for_finalize.tool_call.name,
                "args": cleared_for_finalize.args,
                "content": executed.result.content,
                "details": executed.result.details,
                "isError": executed.is_error,
                "usage": executed.result.usage,
            }),
            &drive.gate,
            &drive.context,
        )
        .await;

    let patch = match patch {
        Ok(patch) if !patch.is_null() => {
            serde_json::from_value::<crate::harness::execution::tools::AfterToolPatch>(patch).ok()
        }
        _ => None,
    };
    let finalized = finalize_tool_call(&cleared_for_finalize, executed, patch);
    Ok(ToolOutcome {
        tool_call: finalized.tool_call.clone(),
        message: create_tool_result_message(&finalized),
        terminate: finalized.terminate,
    })
}

async fn prepare_tool_invocation<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    sources: &ToolBatchSource,
    call: &ToolCall,
    tools: &[AgentHarnessTool],
) -> Result<PreparedToolInvocation, GateError> {
    let tool_call = tool_call_for(sources, call);
    if sources.assistant.stop_reason == pi_ai::StopReason::Length {
        return Ok(PreparedToolInvocation::Outcome {
            outcome: truncated_outcome(tool_call),
        });
    }
    let prepared = prepare_tool_call(&tool_call, tools);
    let prepared = match prepared {
        crate::harness::execution::tools::PreparedOrImmediate::Immediate(immediate) => {
            return Ok(PreparedToolInvocation::Outcome {
                outcome: outcome_from_finalized_call(
                    crate::harness::execution::tools::FinalizedToolCall {
                        tool_call: immediate.tool_call,
                        result: immediate.result,
                        is_error: immediate.is_error,
                        terminate: immediate.terminate,
                    },
                ),
            });
        }
        crate::harness::execution::tools::PreparedOrImmediate::Prepared(prepared) => prepared,
    };

    let decision = lane
        .hooks()
        .run_tool_with_gate(
            crate::harness::hooks::HookName::BeforeTool,
            serde_json::json!({
                "lane": lane.name(),
                "runId": drive.operation_id,
                "toolCallId": tool_call.id,
                "toolName": tool_call.name,
                "args": prepared.args,
            }),
            &drive.gate,
            &drive.context,
        )
        .await;

    let decision = match decision {
        Ok(decision) => {
            serde_json::from_value::<crate::harness::execution::tools::BeforeToolDecision>(decision)
                .ok()
        }
        Err(GateError::AbortRequested(e)) => {
            let _ = (e.cancellation)().await;
            return Ok(PreparedToolInvocation::Outcome {
                outcome: aborted_outcome(tool_call),
            });
        }
        Err(GateError::Closed(e)) => return Err(GateError::Closed(e)),
    };

    let cleared = apply_before_tool_decision(&prepared, decision.as_ref());
    match cleared {
        crate::harness::execution::tools::ClearedOrImmediate::Immediate(immediate) => {
            Ok(PreparedToolInvocation::Outcome {
                outcome: outcome_from_finalized_call(
                    crate::harness::execution::tools::FinalizedToolCall {
                        tool_call: immediate.tool_call,
                        result: immediate.result,
                        is_error: immediate.is_error,
                        terminate: immediate.terminate,
                    },
                ),
            })
        }
        crate::harness::execution::tools::ClearedOrImmediate::Cleared(cleared) => {
            Ok(PreparedToolInvocation::Ready { cleared })
        }
    }
}

async fn start_tool_invocation<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    run: &OperationState,
    sources: &ToolBatchSource,
    call: &ToolCall,
    tools: &[AgentHarnessTool],
    recovery: bool,
) -> Result<ToolCallTask, GateError> {
    let prepared = prepare_tool_invocation(&*lane, drive, sources, call, tools).await?;
    let prepared = match prepared {
        PreparedToolInvocation::Outcome { outcome } => {
            let lane = Arc::clone(&lane);
            let drive = drive.clone();
            let run = run.clone();
            let call = call.clone();
            return Ok(Box::pin(async move {
                let _ = publish_tool_outcome(&*lane, &drive, &run, &call, &outcome, recovery).await;
            }));
        }
        PreparedToolInvocation::Ready { cleared } => cleared,
    };

    let replay = prepared
        .tool
        .replay
        .unwrap_or(crate::harness::session::types::ReplayPolicy::Never);
    let effect_pending = publish_tool_intent(
        &*lane,
        drive,
        run,
        call,
        &prepared.tool_call,
        prepared.args.clone(),
        &replay,
        recovery,
    )
    .await
    .map_err(GateError::Closed)?;

    let ContinueOperationResult::Result {
        value: effect_pending,
    } = effect_pending
    else {
        let lane = Arc::clone(&lane);
        let drive = drive.clone();
        let run = run.clone();
        let call = call.clone();
        let outcome = aborted_outcome(prepared.tool_call.clone());
        return Ok(Box::pin(async move {
            let _ = publish_tool_outcome(&*lane, &drive, &run, &call, &outcome, recovery).await;
        }));
    };

    let lane_for_perform = Arc::clone(&lane);
    let drive = drive.clone();
    let run_owned = run.clone();
    let batch = match run {
        OperationState::Tools { batch, .. } => batch.clone(),
        _ => return Err(GateError::Closed("not a tools state".to_string())),
    };
    let prepared_clone = prepared.clone();
    Ok(Box::pin(async move {
        let outcome = perform_tool_invocation(
            lane_for_perform,
            &drive,
            &batch,
            &effect_pending,
            &prepared_clone,
            recovery,
        )
        .await;
        if let Ok(outcome) = outcome {
            let _ = publish_tool_outcome(
                &*lane,
                &drive,
                &run_owned,
                &effect_pending,
                &outcome,
                recovery,
            )
            .await;
        }
    }))
}

async fn recover_tool_invocation<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    run: &OperationState,
    sources: &ToolBatchSource,
    call: &ToolCall,
    tools_by_name: &BTreeMap<String, AgentHarnessTool>,
    cancelled: bool,
) -> Result<ToolCallTask, GateError> {
    let tool_call = tool_call_for(sources, call);
    let tool = tools_by_name.get(&tool_call.name);
    let can_replay = !cancelled
        && matches!(
            call,
            ToolCall::EffectPending {
                replay: crate::harness::session::types::ReplayPolicy::Safe,
                ..
            }
        )
        && tool
            .map(|t| {
                matches!(
                    t.replay,
                    Some(crate::harness::session::types::ReplayPolicy::Safe)
                )
            })
            .unwrap_or(false);
    if can_replay {
        let args = clear_replay_checkpoint(&*lane, drive, &run_batch(run), call, &tool_call)
            .await
            .map_err(GateError::Closed)?;
        let cleared = ClearedToolCall {
            tool_call: tool_call.clone(),
            tool: tool.unwrap().clone(),
            args,
        };
        let lane_for_perform = Arc::clone(&lane);
        let drive = drive.clone();
        let batch = run_batch(run);
        let call = call.clone();
        let run = run.clone();
        return Ok(Box::pin(async move {
            let outcome =
                perform_tool_invocation(lane_for_perform, &drive, &batch, &call, &cleared, true)
                    .await;
            if let Ok(outcome) = outcome {
                let _ = publish_tool_outcome(&*lane, &drive, &run, &call, &outcome, true).await;
            }
        }));
    }
    let checkpoint = read_checkpoint(&*lane, drive, call)
        .await
        .map_err(GateError::Closed)?;
    let outcome = interrupted_outcome(tool_call.clone(), checkpoint);
    let lane = Arc::clone(&lane);
    let drive = drive.clone();
    let run = run.clone();
    let call = call.clone();
    Ok(Box::pin(async move {
        let _ = publish_tool_outcome(&*lane, &drive, &run, &call, &outcome, true).await;
    }))
}

fn run_batch(run: &OperationState) -> ToolBatch {
    match run {
        OperationState::Tools { batch, .. } => batch.clone(),
        _ => unreachable!(),
    }
}

async fn run_sequential<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    run: &OperationState,
    sources: &ToolBatchSource,
    tools: &[AgentHarnessTool],
    tools_by_name: &BTreeMap<String, AgentHarnessTool>,
    recovery: bool,
) -> Result<ProcedureResult, GateError> {
    let batch = run_batch(run);
    for call in &batch.calls {
        if matches!(
            call,
            ToolCall::Completed { .. } | ToolCall::OutcomeReady { .. }
        ) {
            continue;
        }
        if matches!(
            run.scope().control,
            crate::harness::session::types::Control::CancelRequested { .. }
        ) {
            let tool_call = tool_call_for(sources, call);
            if matches!(call, ToolCall::Planned { .. }) {
                let outcome = aborted_outcome(tool_call);
                publish_tool_outcome(&*lane, drive, run, call, &outcome, recovery)
                    .await
                    .map_err(GateError::Closed)?;
            } else {
                let checkpoint = read_checkpoint(&*lane, drive, call)
                    .await
                    .map_err(GateError::Closed)?;
                let outcome = interrupted_outcome(tool_call, checkpoint);
                publish_tool_outcome(&*lane, drive, run, call, &outcome, recovery)
                    .await
                    .map_err(GateError::Closed)?;
            }
            continue;
        }
        let started = if matches!(call, ToolCall::Planned { .. }) {
            start_tool_invocation(
                Arc::clone(&lane),
                drive,
                run,
                sources,
                call,
                tools,
                recovery,
            )
            .await?
        } else {
            recover_tool_invocation(
                Arc::clone(&lane),
                drive,
                run,
                sources,
                call,
                tools_by_name,
                false,
            )
            .await?
        };
        started.await;
    }
    Ok(ProcedureResult::Continue)
}

async fn run_parallel<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    run: &OperationState,
    sources: &ToolBatchSource,
    tools: &[AgentHarnessTool],
    tools_by_name: &BTreeMap<String, AgentHarnessTool>,
    recovery: bool,
) -> Result<ProcedureResult, GateError> {
    let batch = run_batch(run);
    let mut jobs: Vec<ToolCallTask> = Vec::new();
    for call in &batch.calls {
        if matches!(
            call,
            ToolCall::Completed { .. } | ToolCall::OutcomeReady { .. }
        ) {
            continue;
        }
        let cancelled = matches!(
            lane.state().operation.as_ref().map(|o| &o.state),
            Some(OperationState::Tools { scope, .. })
                if matches!(scope.control, crate::harness::session::types::Control::CancelRequested { .. })
        );
        let started = if matches!(call, ToolCall::Planned { .. }) {
            start_tool_invocation(
                Arc::clone(&lane),
                drive,
                run,
                sources,
                call,
                tools,
                recovery,
            )
            .await?
        } else {
            recover_tool_invocation(
                Arc::clone(&lane),
                drive,
                run,
                sources,
                call,
                tools_by_name,
                cancelled,
            )
            .await?
        };
        let lane_for_mat = Arc::clone(&lane);
        let drive = drive.clone();
        let run = run.clone();
        let sources = sources.clone();
        jobs.push(Box::pin(async move {
            started.await;
            let _ = materialize_ready(&*lane_for_mat, &drive, &run, &sources, recovery).await;
        }));
    }
    for job in jobs {
        job.await;
    }
    let _ = materialize_ready(&*lane, drive, run, sources, recovery).await;
    Ok(ProcedureResult::Continue)
}

/// 对应 `runTools`：执行、恢复、stage 并源序放置一个完整 durable tool batch。
pub async fn run_tools<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    run: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let batch = run_batch(run);
    let recovery = batch.calls.iter().any(|call| {
        matches!(
            call,
            ToolCall::EffectPending { .. } | ToolCall::OutcomeReady { .. }
        )
    });
    if recovery {
        lane.emit_batch(
            vec![HarnessEvent::TurnStart {
                lane: lane.name().to_string(),
                run_id: drive.operation_id.clone(),
                turn_id: batch.turn_id.clone(),
                recovery: Some(true),
            }],
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;
    }
    let sources = read_tool_batch_source(&*lane, drive, &batch)
        .await
        .map_err(GateError::Closed)?;
    materialize_ready(&*lane, drive, run, &sources, recovery)
        .await
        .map_err(GateError::Closed)?;
    let state = lane.state();
    let current = current_batch(&state);
    let Some(current) = current else {
        return Ok(ProcedureResult::Continue);
    };
    if matches!(
        current.scope().control,
        crate::harness::session::types::Control::CancelRequested { .. }
    ) {
        return run_sequential(
            Arc::clone(&lane),
            drive,
            &current,
            &sources,
            &[],
            &BTreeMap::new(),
            recovery,
        )
        .await;
    }
    let config = lane.read_config();
    let active: std::collections::HashSet<&str> = batch
        .configuration
        .active_tool_names
        .iter()
        .map(|s| s.as_str())
        .collect();
    let tools: Vec<AgentHarnessTool> = config
        .tools
        .iter()
        .filter(|tool| active.contains(tool.tool.name.as_str()))
        .cloned()
        .collect();
    let tools_by_name: BTreeMap<String, AgentHarnessTool> = tools
        .iter()
        .map(|tool| (tool.tool.name.clone(), tool.clone()))
        .collect();
    if matches!(
        run.scope().settings.tool_execution,
        crate::harness::session::types::ToolExecution::Sequential
    ) {
        run_sequential(
            lane,
            drive,
            &current,
            &sources,
            &tools,
            &tools_by_name,
            recovery,
        )
        .await
    } else {
        run_parallel(
            lane,
            drive,
            &current,
            &sources,
            &tools,
            &tools_by_name,
            recovery,
        )
        .await
    }
}

#[allow(dead_code)]
fn _unused(_: &Drive) {}
