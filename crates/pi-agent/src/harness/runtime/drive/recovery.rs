//! Rust 翻译自 packages/agent/src/harness/runtime/drive/recovery.ts
//!
//! 从已提交的 frame 前缀合成 settle 一个孤立的 assistant/deferred 效果。

use std::sync::Arc;

use pi_ai::utils::assistant_message_frame::reduce_assistant_message_frames;
use pi_ai::{AssistantMessage, StopReason, Usage, UsageCost};

use crate::harness::agent_harness::HarnessEvent;
use crate::harness::execution::effect_gate::GateError;
use crate::harness::runtime::drive::response::publish_response;
use crate::harness::runtime::progress::read_assistant_frames;
use crate::harness::runtime::types::{
    ContinueOperationResult, Drive, Lane, OperationCommand, ProcedureResult,
};
use crate::harness::session::types::{LaneConfiguration, OperationState, SettledAssistantMessage};
use crate::types::AgentMessage;

fn zero_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: UsageCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    }
}

fn interrupted_assistant_message(
    identity: &crate::harness::session::types::ModelIdentity,
    partial: Option<AssistantMessage>,
) -> SettledAssistantMessage {
    let warning = "Assistant request was interrupted. The preceding content is the latest committed partial; newer live output may be missing and the external outcome is unknown.";
    match partial {
        None => AssistantMessage {
            content: Vec::new(),
            api: "unknown".to_string(),
            provider: identity.provider.clone(),
            model: identity.model_id.clone(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            usage: zero_usage(),
            stop_reason: StopReason::Error,
            deferred: None,
            error_message: Some(warning.to_string()),
            raw_stop_reason: None,
            end_turn: None,
            timestamp: pi_ai::utils::uuid::now_ms() as u64,
        },
        Some(mut partial) => {
            partial.usage = zero_usage();
            partial.stop_reason = StopReason::Error;
            partial.error_message = Some(warning.to_string());
            partial
        }
    }
}

/// 对应 `recoverAssistantGeneration`。
pub async fn recover_assistant_generation<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    generation: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let response_entry_id = match generation {
        OperationState::AssistantEffectPending {
            response_entry_id, ..
        } => response_entry_id.clone(),
        _ => return Ok(ProcedureResult::Continue),
    };
    let operation_id = drive.operation_id.clone();
    let drive_context = drive.context.clone();
    let frames = lane
        .continue_operation(
            generation,
            Box::new(move |_state, _current, _meta, reader| {
                let operation_id = operation_id.clone();
                let response_entry_id = response_entry_id.clone();
                let context = drive_context.clone();
                Box::pin(async move {
                    let frames = read_assistant_frames(
                        reader.as_ref(),
                        &operation_id,
                        &response_entry_id,
                        &context,
                    )
                    .await
                    .expect("read_assistant_frames failed");
                    OperationCommand::Return { result: frames }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;

    let ContinueOperationResult::Result { value: frames } = frames else {
        return Ok(ProcedureResult::Continue);
    };

    let identity = match generation {
        OperationState::AssistantEffectPending {
            generation_context, ..
        } => generation_context.configuration.model.clone(),
        _ => unreachable!(),
    };
    let message = interrupted_assistant_message(&identity, reduce_assistant_message_frames(frames));

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let response_entry_id = match generation {
        OperationState::AssistantEffectPending {
            response_entry_id, ..
        } => response_entry_id.clone(),
        _ => unreachable!(),
    };
    lane.emit_batch(
        vec![
            HarnessEvent::MessageStart {
                lane: lane_name.clone(),
                run_id: Some(operation_id.clone()),
                message: AgentMessage::Assistant(message.clone()),
                recovery: Some(true),
            },
            HarnessEvent::MessageEnd {
                lane: lane_name.clone(),
                run_id: Some(operation_id.clone()),
                message: AgentMessage::Assistant(message.clone()),
                entry_id: Some(response_entry_id.clone()),
                recovery: Some(true),
            },
        ],
        &drive.context,
    )
    .await
    .map_err(GateError::Closed)?;

    publish_response(Arc::clone(&lane), drive, generation, message, Some(true)).await
}

/// 对应 `recoverCancelledAssistantEffect`。
pub async fn recover_cancelled_assistant_effect<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    effect: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let response_entry_id = match effect {
        OperationState::AssistantEffectPending {
            response_entry_id, ..
        }
        | OperationState::DeferredEffectPending {
            response_entry_id, ..
        } => response_entry_id.clone(),
        _ => return Ok(ProcedureResult::Continue),
    };
    let operation_id = drive.operation_id.clone();
    let drive_context = drive.context.clone();
    let frames = lane
        .settle_operation(
            effect,
            Box::new(move |_state, _current, _meta, reader| {
                let operation_id = operation_id.clone();
                let response_entry_id = response_entry_id.clone();
                let context = drive_context.clone();
                Box::pin(async move {
                    let frames = read_assistant_frames(
                        reader.as_ref(),
                        &operation_id,
                        &response_entry_id,
                        &context,
                    )
                    .await
                    .expect("read_assistant_frames failed");
                    OperationCommand::Return { result: frames }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;

    let identity: LaneConfiguration = match effect {
        OperationState::AssistantEffectPending {
            generation_context, ..
        } => generation_context.configuration.clone(),
        OperationState::DeferredEffectPending { scope, .. } => scope.configuration.clone(),
        _ => unreachable!(),
    };
    let message =
        interrupted_assistant_message(&identity.model, reduce_assistant_message_frames(frames));

    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let response_entry_id = match effect {
        OperationState::AssistantEffectPending {
            response_entry_id, ..
        }
        | OperationState::DeferredEffectPending {
            response_entry_id, ..
        } => response_entry_id.clone(),
        _ => unreachable!(),
    };
    lane.emit_batch(
        vec![
            HarnessEvent::MessageStart {
                lane: lane_name.clone(),
                run_id: Some(operation_id.clone()),
                message: AgentMessage::Assistant(message.clone()),
                recovery: Some(true),
            },
            HarnessEvent::MessageEnd {
                lane: lane_name.clone(),
                run_id: Some(operation_id.clone()),
                message: AgentMessage::Assistant(message.clone()),
                entry_id: Some(response_entry_id.clone()),
                recovery: Some(true),
            },
        ],
        &drive.context,
    )
    .await
    .map_err(GateError::Closed)?;

    publish_response(Arc::clone(&lane), drive, effect, message, Some(true)).await
}
