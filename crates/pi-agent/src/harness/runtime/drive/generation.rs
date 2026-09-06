//! Rust 翻译自 packages/agent/src/harness/runtime/drive/generation.ts
//!
//! 一次 assistant 生成：准备 → 发布 intent → 流式执行 → 发布响应。

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use pi_ai::{Model, Tool};

use crate::harness::agent_harness::HarnessEvent;
use crate::harness::context::{Context, with_abort_signal};
use crate::harness::execution::assistant::{
    HarnessAssistantStreamConfig, HarnessRequestContext, stream_harness_assistant,
};
use crate::harness::execution::effect_gate::GateError;
use crate::harness::hooks::apply_stream_options_patch;
use crate::harness::runtime::drive::response::{
    open_assistant_response, publish_configuration_failure, publish_response,
};
use crate::harness::runtime::drive::retry::wait_until;
use crate::harness::runtime::transcript::read_bounded_context;
use crate::harness::runtime::types::{
    Config, ContinueOperationResult, Drive, Lane, OperationCommand, ProcedureResult, SystemPrompt,
};
use crate::harness::session::types::{OperationError, OperationState, SettledAssistantMessage};
use crate::types::AgentMessage;

type JsonValue = serde_json::Value;

#[derive(Clone)]
struct PreparedGeneration {
    model: Model,
    tools: Vec<Tool>,
    messages: Vec<AgentMessage>,
    system_prompt: String,
    stream_options: crate::harness::types::AgentHarnessStreamOptions,
    to_provider_messages: crate::harness::runtime::types::ToProviderMessagesFn,
}

#[allow(clippy::large_enum_variant)]
enum GenerationPreparation {
    Ready(PreparedGeneration),
    ConfigurationFailure { error: OperationError },
    CancelRequested,
}

fn configuration_error(code: &str, details: JsonValue) -> OperationError {
    OperationError {
        code: code.to_string(),
        message: if code == "model_unavailable" {
            "The configured model is unavailable in this process".to_string()
        } else {
            "One or more configured tools are unavailable in this process".to_string()
        },
        details: Some(details),
    }
}

async fn resolve_system_prompt(config: &Config, context: &Context) -> String {
    match &config.system_prompt {
        None => String::new(),
        Some(SystemPrompt::Static(s)) => s.clone(),
        Some(SystemPrompt::Dynamic(f)) => {
            let tool_context: Arc<dyn Any + Send + Sync> =
                config.tool_context.clone().unwrap_or_else(|| Arc::new(()));
            f(tool_context, context.clone()).await
        }
    }
}

async fn prepare_generation<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    generation: &OperationState,
) -> Result<GenerationPreparation, String> {
    let OperationState::AssistantReady {
        generation_context,
        next_attempt,
        ..
    } = generation
    else {
        return Ok(GenerationPreparation::CancelRequested);
    };
    let identity = &generation_context.configuration.model;
    let model = lane
        .models()
        .get_model(&identity.provider, &identity.model_id);
    let Some(model) = model else {
        return Ok(GenerationPreparation::ConfigurationFailure {
            error: configuration_error(
                "model_unavailable",
                serde_json::to_value(identity).unwrap_or(JsonValue::Null),
            ),
        });
    };

    let config = lane.read_config();
    let tools_by_name: BTreeMap<&str, &crate::harness::types::AgentHarnessTool> = config
        .tools
        .iter()
        .map(|tool| (tool.tool.name.as_str(), tool))
        .collect();
    let missing_tools: Vec<String> = generation_context
        .configuration
        .active_tool_names
        .iter()
        .filter(|name| !tools_by_name.contains_key(name.as_str()))
        .cloned()
        .collect();
    if !missing_tools.is_empty() {
        return Ok(GenerationPreparation::ConfigurationFailure {
            error: configuration_error(
                "configured_tools_unavailable",
                serde_json::json!({ "tools": missing_tools }),
            ),
        });
    }
    let tools: Vec<Tool> = generation_context
        .configuration
        .active_tool_names
        .iter()
        .map(|name| {
            let tool = tools_by_name
                .get(name.as_str())
                .expect("Configured tool disappeared during resolution");
            tool.tool.clone()
        })
        .collect();

    let messages = read_bounded_context(lane, drive, generation).await?;
    let ContinueOperationResult::Result { value: messages } = messages else {
        return Ok(GenerationPreparation::CancelRequested);
    };
    let system_prompt = resolve_system_prompt(&config, &drive.context).await;

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
                "step": "assistant",
                "attempt": next_attempt,
                "streamOptions": generation_context.stream_options,
            }),
            &drive.gate,
            &drive.context,
        )
        .await
        .map_err(|e| e.to_string())?;

    let stream_options = match before_request.get("streamOptions") {
        Some(patch) if !patch.is_null() => {
            if let Ok(patch) = serde_json::from_value(patch.clone()) {
                apply_stream_options_patch(&generation_context.stream_options, &patch)
            } else {
                generation_context.stream_options.clone()
            }
        }
        _ => generation_context.stream_options.clone(),
    };

    Ok(GenerationPreparation::Ready(PreparedGeneration {
        model,
        tools,
        messages,
        system_prompt,
        stream_options,
        to_provider_messages: config.to_provider_messages.clone(),
    }))
}

async fn publish_generation_intent<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    ready: &OperationState,
    prepared: &PreparedGeneration,
) -> Result<ContinueOperationResult<OperationState>, String> {
    let OperationState::AssistantReady {
        generation_context,
        next_attempt,
        ..
    } = ready
    else {
        return Ok(ContinueOperationResult::CancelRequested);
    };
    let at = pi_ai::utils::uuid::now_ms() as u64;
    let response_entry_id = lane.session().id_generator().next(Some(at));
    let usage_id = lane.session().id_generator().next(Some(at));
    let intended_output_limit = prepared.model.max_tokens;
    let context_window = prepared.model.context_window;
    let step_id = generation_context.step_id.clone();
    let generation_context = generation_context.clone();
    let next_attempt = *next_attempt;

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();

    let result = lane
        .continue_operation(
            ready,
            Box::new(move |_state, current, _meta, _reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let step_id = step_id.clone();
                let generation_context = generation_context.clone();
                let response_entry_id = response_entry_id.clone();
                let usage_id = usage_id.clone();
                let next_attempt = next_attempt;
                Box::pin(async move {
                    let next_state = OperationState::AssistantEffectPending {
                        scope: current.scope().clone(),
                        generation_context,
                        attempt: next_attempt,
                        response_entry_id,
                        usage_id,
                        intended_output_limit,
                        context_window,
                    };
                    let is_first = next_attempt == 1;
                    OperationCommand::Commit {
                        writes: Vec::new(),
                        operation_state: next_state.clone(),
                        lane: None,
                        materialize: Box::new(move |_| next_state),
                        events: Some(Box::new(move |_| {
                            if is_first {
                                vec![HarnessEvent::TurnStart {
                                    lane: lane_name.clone(),
                                    run_id: operation_id.clone(),
                                    turn_id: step_id.clone(),
                                    recovery: None,
                                }]
                            } else {
                                Vec::new()
                            }
                        })),
                    }
                })
            }),
            &drive.context,
        )
        .await?;

    Ok(result)
}

async fn perform_generation<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    intent: &OperationState,
    prepared: &PreparedGeneration,
) -> Result<SettledAssistantMessage, String> {
    let OperationState::AssistantEffectPending {
        generation_context,
        response_entry_id,
        ..
    } = intent
    else {
        return Err("Generation intent is not assistant.effect_pending".to_string());
    };
    let response =
        open_assistant_response(Arc::clone(&lane), drive, response_entry_id.clone(), false);

    let thinking_level = generation_context.configuration.thinking_level;

    let config = HarnessAssistantStreamConfig {
        model: prepared.model.clone(),
        system_prompt: prepared.system_prompt.clone(),
        tools: Some(prepared.tools.clone()),
        thinking_level,
        stream_options: prepared.stream_options.clone(),
        transform_context: Some(Arc::new({
            let lane = Arc::clone(&lane);
            let lane_name = lane.name().to_string();
            let operation_id = drive.operation_id.clone();
            let gate = drive.gate.clone();
            move |request_context: HarnessRequestContext, context: Context| {
                let lane = Arc::clone(&lane);
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let gate = gate.clone();
                Box::pin(async move {
                    let result = lane
                        .hooks()
                        .run_with_gate(
                            crate::harness::hooks::HookName::TransformContext,
                            serde_json::json!({
                                "lane": lane_name,
                                "runId": operation_id,
                                "messages": request_context.messages,
                                "systemPrompt": request_context.system_prompt,
                            }),
                            &gate,
                            &context,
                        )
                        .await;
                    match result {
                        Ok(result) => HarnessRequestContext {
                            messages: result
                                .get("messages")
                                .and_then(|m| serde_json::from_value(m.clone()).ok())
                                .unwrap_or(request_context.messages),
                            system_prompt: result
                                .get("systemPrompt")
                                .and_then(|m| m.as_str().map(|s| s.to_string()))
                                .unwrap_or(request_context.system_prompt),
                        },
                        Err(_) => request_context,
                    }
                }) as BoxFuture<HarnessRequestContext>
            }
        })),
        to_provider_messages: prepared.to_provider_messages.clone(),
        before_payload: Some(Arc::new({
            let lane = Arc::clone(&lane);
            let lane_name = lane.name().to_string();
            let operation_id = drive.operation_id.clone();
            let gate = drive.gate.clone();
            move |payload: JsonValue, model: Model, context: Context| {
                let lane = Arc::clone(&lane);
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let gate = gate.clone();
                Box::pin(async move {
                    let result = lane
                        .hooks()
                        .run_with_gate(
                            crate::harness::hooks::HookName::BeforePayload,
                            serde_json::json!({
                                "lane": lane_name,
                                "runId": operation_id,
                                "model": model,
                                "payload": payload,
                            }),
                            &gate,
                            &context,
                        )
                        .await;
                    match result {
                        Ok(result) => result.get("payload").cloned(),
                        Err(_) => None,
                    }
                }) as BoxFuture<Option<JsonValue>>
            }
        })),
        after_response: Some(Arc::new({
            let after = response.after_response.clone();
            move |message, metadata, context| {
                let after = after.clone();
                Box::pin(async move { after(message, metadata, context).await })
                    as BoxFuture<SettledAssistantMessage>
            }
        })),
        request: Arc::new({
            let lane = Arc::clone(&lane);
            let gate = drive.gate.clone();
            let model = prepared.model.clone();
            let session_id = format!("{}:{}", lane.session().metadata().id, lane.name());
            move |ai_context, options, context| {
                let lane = Arc::clone(&lane);
                let gate = gate.clone();
                let model = model.clone();
                let session_id = session_id.clone();
                Box::pin(async move {
                    let admitted = with_abort_signal(&gate.signal, &context);
                    let _ = gate.admit(|| ());
                    let mut options = options;
                    options.stream.session_id = Some(session_id);
                    options.stream.request.signal = admitted.abort_signal().cloned();
                    lane.models()
                        .stream_simple(&model, &ai_context, Some(&options))
                }) as BoxFuture<pi_ai::AssistantMessageEventStream>
            }
        }),
        observer: response.observer.clone(),
    };

    let result = stream_harness_assistant(prepared.messages.clone(), &config, &drive.context).await;
    (response.close)().await;
    Ok(result)
}

/// 对应 `runRetryWait`。
pub async fn run_retry_wait<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    generation: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let OperationState::AssistantRetryWait {
        generation_context,
        next_attempt,
        not_before,
        ..
    } = generation
    else {
        return Ok(ProcedureResult::Continue);
    };
    let now = pi_ai::utils::uuid::now_ms() as u64;
    if now < *not_before {
        if !drive.wait_for_retry {
            return Ok(ProcedureResult::Waiting {
                outcome: crate::harness::agent_harness::DriveOutcome::WaitingRetry {
                    operation_id: drive.operation_id.clone(),
                    not_before: *not_before,
                },
            });
        }
        let _ = drive
            .gate
            .admit(|| wait_until(*not_before, &drive.gate.signal))
            .map_err(|_| GateError::Closed("aborted".to_string()))?
            .await;
    }

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let step_id = generation_context.step_id.clone();
    let generation_context = generation_context.clone();
    let next_attempt = *next_attempt;
    let result = lane
        .continue_operation(
            generation,
            Box::new(move |_state, current, _meta, _reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let step_id = step_id.clone();
                let generation_context = generation_context.clone();
                let next_attempt = next_attempt;
                Box::pin(async move {
                    let next_state = OperationState::AssistantReady {
                        scope: current.scope().clone(),
                        generation_context,
                        next_attempt,
                    };
                    OperationCommand::Commit {
                        writes: Vec::new(),
                        operation_state: next_state,
                        lane: None,
                        materialize: Box::new(|_| ProcedureResult::Continue),
                        events: Some(Box::new(move |_| {
                            vec![HarnessEvent::RetryStart {
                                lane: lane_name.clone(),
                                run_id: operation_id.clone(),
                                step: step_id.clone(),
                                attempt: next_attempt,
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

/// 对应 `runGeneration`：执行一次 ready 的 assistant 生成或推进其 retry wait。
pub async fn run_generation<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    generation: &OperationState,
) -> Result<ProcedureResult, GateError> {
    if matches!(generation, OperationState::AssistantRetryWait { .. }) {
        return run_retry_wait(&*lane, drive, generation).await;
    }

    let prepared = prepare_generation(&*lane, drive, generation)
        .await
        .map_err(GateError::Closed)?;
    let GenerationPreparation::Ready(prepared) = prepared else {
        return match prepared {
            GenerationPreparation::ConfigurationFailure { error } => {
                publish_configuration_failure(&*lane, drive, generation, error).await
            }
            GenerationPreparation::CancelRequested => Ok(ProcedureResult::Continue),
            GenerationPreparation::Ready(_) => unreachable!(),
        };
    };

    let intent = publish_generation_intent(&*lane, drive, generation, &prepared)
        .await
        .map_err(GateError::Closed)?;
    let ContinueOperationResult::Result { value: intent } = intent else {
        return Ok(ProcedureResult::Continue);
    };
    let response = perform_generation(Arc::clone(&lane), drive, &intent, &prepared)
        .await
        .map_err(GateError::Closed)?;
    publish_response(lane, drive, &intent, response, None).await
}
