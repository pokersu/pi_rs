//! Rust 翻译自 packages/agent/src/harness/runtime/drive/deferred.ts
//!
//! durable deferred 响应的轮询。perform 阶段通过 `Models::stream_deferred`
//! 轮询 source handle（对齐原版 `streamDeferred`）。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pi_ai::{DeferredFetchOptions, DeferredHandle, ProviderRequestOptions};

use crate::harness::agent_harness::HarnessEvent;
use crate::harness::context::Context;
use crate::harness::execution::assistant::consume_assistant_stream;
use crate::harness::execution::effect_gate::GateError;
use crate::harness::hooks::apply_stream_options_patch;
use crate::harness::runtime::drive::response::{
    open_assistant_response, publish_configuration_failure, publish_response,
};
use crate::harness::runtime::types::{
    ContinueOperationResult, Drive, Lane, OperationCommand, ProcedureResult,
};
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{
    DeferredScope, Entry, LaneConfiguration, OperationError, OperationState, SessionReader,
    SettledAssistantMessage,
};
use crate::harness::session::values::{delete_list, pending_assistant_frames};
use crate::types::AgentMessage;

/// `source`/`stream_options` 待 pi-ai 补 stream_deferred 后使用。
#[allow(dead_code)]
struct PreparedDeferredPoll {
    source: DeferredHandle,
    model: pi_ai::Model,
    poll: u64,
    stream_options: crate::harness::types::AgentHarnessStreamOptions,
}

#[allow(clippy::large_enum_variant)]
enum DeferredPreparation {
    Ready(PreparedDeferredPoll),
    CancelRequested,
    Waiting { source: DeferredHandle },
    ConfigurationFailure,
}

fn configuration_error(identity: &crate::harness::session::types::ModelIdentity) -> OperationError {
    OperationError {
        code: "model_unavailable".to_string(),
        message: "The configured model is unavailable in this process".to_string(),
        details: Some(serde_json::to_value(identity).unwrap_or(serde_json::Value::Null)),
    }
}

fn deferred_scope(state: &OperationState) -> Option<&DeferredScope> {
    match state {
        OperationState::DeferredSuspended { scope }
        | OperationState::DeferredEffectPending { scope, .. } => Some(scope),
        _ => None,
    }
}

/// 对应 `readDeferredSourceHandle`。
pub async fn read_deferred_source_handle(
    reader: &dyn SessionReader,
    source_entry_id: &str,
    configuration: &LaneConfiguration,
    context: &Context,
) -> Result<DeferredHandle, String> {
    let entries = reader
        .get_entries(&[source_entry_id.to_string()], context)
        .await?;
    let Some(Entry::Message(source)) = entries.get(source_entry_id) else {
        return Err(SessionInvariantError::new(format!(
            "Deferred source {source_entry_id} is missing its assistant handle"
        ))
        .to_string());
    };
    let AgentMessage::Assistant(assistant) = &source.message else {
        return Err(SessionInvariantError::new(format!(
            "Deferred source {source_entry_id} is missing its assistant handle"
        ))
        .to_string());
    };
    let Some(handle) = &assistant.deferred else {
        return Err(SessionInvariantError::new(format!(
            "Deferred source {source_entry_id} is missing its assistant handle"
        ))
        .to_string());
    };
    if assistant.stop_reason != pi_ai::StopReason::Deferred {
        return Err(SessionInvariantError::new(format!(
            "Deferred source {source_entry_id} is missing its assistant handle"
        ))
        .to_string());
    }
    let identity = &configuration.model;
    if handle.id.is_empty()
        || handle.provider != identity.provider
        || handle.model_id != identity.model_id
        || handle.api != assistant.api
    {
        return Err(SessionInvariantError::new(format!(
            "Deferred source {source_entry_id} has an invalid handle"
        ))
        .to_string());
    }
    Ok(handle.clone())
}

async fn read_source_handle<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    deferred: &OperationState,
) -> Result<ContinueOperationResult<DeferredHandle>, String> {
    let source_entry_id = deferred_scope(deferred).unwrap().source_entry_id.clone();
    let configuration = deferred_scope(deferred).unwrap().configuration.clone();
    let drive_context = drive.context.clone();
    lane.continue_operation(
        deferred,
        Box::new(move |_state, _current, _meta, reader| {
            let source_entry_id = source_entry_id.clone();
            let configuration = configuration.clone();
            let context = drive_context.clone();
            Box::pin(async move {
                let handle = read_deferred_source_handle(
                    reader.as_ref(),
                    &source_entry_id,
                    &configuration,
                    &context,
                )
                .await
                .expect("read_deferred_source_handle failed");
                OperationCommand::Return { result: handle }
            })
        }),
        &drive.context,
    )
    .await
}

async fn prepare_deferred_poll<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    expected: &OperationState,
) -> Result<DeferredPreparation, GateError> {
    let source = read_source_handle(lane, drive, expected)
        .await
        .map_err(GateError::Closed)?;
    let ContinueOperationResult::Result { value: source } = source else {
        return Ok(DeferredPreparation::CancelRequested);
    };
    if drive.deferred_permits.load(Ordering::SeqCst) == 0 {
        return Ok(DeferredPreparation::Waiting { source });
    }

    let scope = deferred_scope(expected).unwrap();
    let identity = &scope.configuration.model;
    let model = lane
        .models()
        .get_model(&identity.provider, &identity.model_id);
    let Some(model) = model else {
        return Ok(DeferredPreparation::ConfigurationFailure);
    };
    let mut base_options = scope.stream_options.clone();
    base_options.deferred = Some(serde_json::json!(false));
    let poll = if matches!(expected, OperationState::DeferredSuspended { .. }) {
        scope.poll + 1
    } else {
        scope.poll
    };

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let before_request = lane
        .hooks()
        .run_with_gate(
            crate::harness::hooks::HookName::BeforeRequest,
            serde_json::json!({
                "lane": lane_name,
                "runId": operation_id,
                "model": model,
                "step": "deferred",
                "attempt": poll,
                "streamOptions": base_options,
            }),
            &drive.gate,
            &drive.context,
        )
        .await?;

    let stream_options = match before_request.get("streamOptions") {
        Some(patch) if !patch.is_null() => {
            if let Ok(patch) = serde_json::from_value(patch.clone()) {
                apply_stream_options_patch(&base_options, &patch)
            } else {
                base_options
            }
        }
        _ => base_options,
    };

    Ok(DeferredPreparation::Ready(PreparedDeferredPoll {
        source,
        model,
        poll,
        stream_options,
    }))
}

async fn publish_poll_intent<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    deferred: &OperationState,
    prepared: &PreparedDeferredPoll,
    recovery: bool,
) -> Result<ContinueOperationResult<OperationState>, String> {
    let scope = deferred_scope(deferred).unwrap();
    let at = pi_ai::utils::uuid::now_ms() as u64;
    let response_entry_id = lane.session().id_generator().next(Some(at));
    let usage_id = lane.session().id_generator().next(Some(at));
    let step_id = scope.step_id.clone();
    let source_entry_id = scope.source_entry_id.clone();
    let poll = prepared.poll;
    let configuration = scope.configuration.clone();
    let stream_options = scope.stream_options.clone();
    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let deferred_permits = Arc::clone(&drive.deferred_permits);

    let is_effect_pending = matches!(deferred, OperationState::DeferredEffectPending { .. });
    let old_response_entry_id = match deferred {
        OperationState::DeferredEffectPending {
            response_entry_id, ..
        } => Some(response_entry_id.clone()),
        _ => None,
    };

    lane.continue_operation(
        deferred,
        Box::new(move |_state, current, _meta, _reader| {
            let lane_name = lane_name.clone();
            let operation_id = operation_id.clone();
            let step_id = step_id.clone();
            let source_entry_id = source_entry_id.clone();
            let response_entry_id = response_entry_id.clone();
            let usage_id = usage_id.clone();
            let configuration = configuration.clone();
            let stream_options = stream_options.clone();
            let deferred_permits = Arc::clone(&deferred_permits);
            Box::pin(async move {
                let current_scope = deferred_scope(&current).unwrap().clone();
                let turn_step_id = step_id.clone();
                let next_state = OperationState::DeferredEffectPending {
                    scope: DeferredScope {
                        control: current_scope.control.clone(),
                        settings: current_scope.settings.clone(),
                        latest_assistant_entry_id: current_scope.latest_assistant_entry_id.clone(),
                        step_id,
                        source_entry_id,
                        poll,
                        configuration,
                        stream_options,
                    },
                    response_entry_id: response_entry_id.clone(),
                    usage_id,
                };
                let writes = if is_effect_pending {
                    let old = old_response_entry_id.clone().unwrap_or_default();
                    vec![crate::harness::session::types::Write::List(delete_list(
                        &pending_assistant_frames(&operation_id, &old),
                    ))]
                } else {
                    Vec::new()
                };
                let turn_id = format!("{}:poll:{}", turn_step_id, poll);
                OperationCommand::Commit {
                    writes,
                    operation_state: next_state.clone(),
                    lane: None,
                    materialize: Box::new(move |_| {
                        deferred_permits.fetch_sub(1, Ordering::SeqCst);
                        next_state
                    }),
                    events: Some(Box::new(move |_| {
                        vec![
                            HarnessEvent::RunResume {
                                lane: lane_name.clone(),
                                run_id: operation_id.clone(),
                                recovery: Some(recovery),
                            },
                            HarnessEvent::TurnStart {
                                lane: lane_name.clone(),
                                run_id: operation_id.clone(),
                                turn_id: turn_id.clone(),
                                recovery: Some(recovery),
                            },
                        ]
                    })),
                }
            })
        }),
        &drive.context,
    )
    .await
}

async fn perform_deferred_poll<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    prepared: &PreparedDeferredPoll,
    intent: &OperationState,
    recovery: bool,
) -> Result<SettledAssistantMessage, String> {
    let OperationState::DeferredEffectPending {
        response_entry_id, ..
    } = intent
    else {
        return Err("Deferred intent is not deferred.effect_pending".to_string());
    };
    let response = open_assistant_response(
        Arc::clone(&lane),
        drive,
        response_entry_id.clone(),
        recovery,
    );

    let deferred_options = DeferredFetchOptions {
        request: ProviderRequestOptions {
            signal: Some(drive.gate.signal.clone()),
            api_key: None,
            headers: prepared.stream_options.headers.clone().map(|headers| {
                headers
                    .into_iter()
                    .map(|(key, value)| (key, Some(value)))
                    .collect()
            }),
            timeout_ms: prepared.stream_options.timeout_ms,
            max_retries: prepared.stream_options.max_retries,
            max_retry_delay_ms: prepared.stream_options.max_retry_delay_ms,
        },
        wait: Some(0),
    };
    let stream = drive
        .gate
        .admit(|| {
            lane.models().stream_deferred(
                &prepared.model,
                &prepared.source,
                Some(&deferred_options),
            )
        })
        .map_err(|e| e.to_string())?;

    let result = consume_assistant_stream(
        stream,
        &response.observer,
        Some(Arc::new(move |message, context| {
            let after = response.after_response.clone();
            Box::pin(async move {
                after(
                    message,
                    crate::harness::execution::assistant::AssistantResponseMetadata::default(),
                    context,
                )
                .await
            })
        })),
        &drive.context,
    )
    .await;
    (response.close)().await;
    Ok(result)
}

async fn poll_deferred<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    expected: &OperationState,
    recovery: bool,
) -> Result<ProcedureResult, GateError> {
    let prepared = prepare_deferred_poll(&*lane, drive, expected).await?;
    match prepared {
        DeferredPreparation::CancelRequested => Ok(ProcedureResult::Continue),
        DeferredPreparation::Waiting { source } => Ok(ProcedureResult::Waiting {
            outcome: crate::harness::agent_harness::DriveOutcome::WaitingDeferred {
                operation_id: drive.operation_id.clone(),
                deferred: source,
            },
        }),
        DeferredPreparation::ConfigurationFailure => {
            let scope = deferred_scope(expected).unwrap();
            publish_configuration_failure(
                &*lane,
                drive,
                expected,
                configuration_error(&scope.configuration.model),
            )
            .await
        }
        DeferredPreparation::Ready(prepared) => {
            let intent = publish_poll_intent(&*lane, drive, expected, &prepared, recovery)
                .await
                .map_err(GateError::Closed)?;
            let ContinueOperationResult::Result { value: intent } = intent else {
                return Ok(ProcedureResult::Continue);
            };
            let response =
                perform_deferred_poll(Arc::clone(&lane), drive, &prepared, &intent, recovery)
                    .await
                    .map_err(GateError::Closed)?;
            publish_response(
                lane,
                drive,
                &intent,
                response,
                if recovery { Some(true) } else { None },
            )
            .await
        }
    }
}

/// 对应 `runDeferredSuspended`。
pub async fn run_deferred_suspended<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    deferred: &OperationState,
) -> Result<ProcedureResult, GateError> {
    poll_deferred(lane, drive, deferred, false).await
}

/// 对应 `recoverDeferredPoll`。
pub async fn recover_deferred_poll<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    deferred: &OperationState,
) -> Result<ProcedureResult, GateError> {
    poll_deferred(lane, drive, deferred, true).await
}

/// 对应 `runDeferred`。
pub async fn run_deferred<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    deferred: &OperationState,
) -> Result<ProcedureResult, GateError> {
    if matches!(deferred, OperationState::DeferredSuspended { .. }) {
        run_deferred_suspended(lane, drive, deferred).await
    } else {
        recover_deferred_poll(lane, drive, deferred).await
    }
}
