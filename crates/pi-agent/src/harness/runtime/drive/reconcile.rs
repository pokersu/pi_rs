//! Rust 翻译自 packages/agent/src/harness/runtime/drive/reconcile.ts
//!
//! 取消的 durable leaf 的推进：不启动新的普通工作，只 settle 已准入效果。

use std::sync::Arc;

use pi_ai::{DeferredHandle, ProviderRequestOptions};

use crate::harness::agent_harness::HarnessEvent;
use crate::harness::execution::effect_gate::{Cancellation, GateError};
use crate::harness::runtime::drive::deferred::read_deferred_source_handle;
use crate::harness::runtime::drive::recovery::recover_cancelled_assistant_effect;
use crate::harness::runtime::drive::terminal::{operation_cleanup_writes, operation_result_record};
use crate::harness::runtime::drive::tools::run_tools;
use crate::harness::runtime::types::{Drive, Lane, OperationCommand, ProcedureResult};
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::OperationState;

async fn cancel_deferred_best_effort<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    deferred: &OperationState,
    handle: &DeferredHandle,
) {
    let (configuration, stream_options) = match deferred {
        OperationState::DeferredSuspended { scope }
        | OperationState::DeferredEffectPending { scope, .. } => {
            (scope.configuration.clone(), scope.stream_options.clone())
        }
        _ => return,
    };
    let identity = configuration.model;
    let Some(model) = lane
        .models()
        .get_model(&identity.provider, &identity.model_id)
    else {
        return;
    };
    let options = ProviderRequestOptions {
        signal: Some(drive.close_signal.clone()),
        api_key: None,
        headers: stream_options.headers.map(|headers| {
            headers
                .into_iter()
                .map(|(key, value)| (key, Some(value)))
                .collect()
        }),
        timeout_ms: stream_options.timeout_ms,
        max_retries: stream_options.max_retries,
        max_retry_delay_ms: stream_options.max_retry_delay_ms,
    };
    // best-effort：远程取消失败不影响本地 durable reconcile。
    let _ = lane
        .models()
        .cancel_deferred(&model, handle, Some(&options))
        .await;
}

fn control_of(state: &OperationState) -> &crate::harness::session::types::Control {
    match state {
        OperationState::DeferredSuspended { scope }
        | OperationState::DeferredEffectPending { scope, .. } => &scope.control,
        _ => &state.scope().control,
    }
}

fn deferred_configuration(
    deferred: &OperationState,
) -> crate::harness::session::types::LaneConfiguration {
    match deferred {
        OperationState::DeferredSuspended { scope }
        | OperationState::DeferredEffectPending { scope, .. } => scope.configuration.clone(),
        _ => unreachable!(),
    }
}

async fn read_deferred_handle<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    deferred: &OperationState,
) -> Result<DeferredHandle, GateError> {
    let source_entry_id = match deferred {
        OperationState::DeferredSuspended { scope }
        | OperationState::DeferredEffectPending { scope, .. } => scope.source_entry_id.clone(),
        _ => unreachable!(),
    };
    let configuration = deferred_configuration(deferred);
    let drive_context = drive.context.clone();
    let handle = lane
        .settle_operation(
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
        .map_err(GateError::Closed)?;
    Ok(handle)
}

async fn publish_aborted_terminal<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    capability: &OperationState,
) -> Result<ProcedureResult, GateError> {
    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let drive_context = drive.context.clone();
    let result = lane
        .settle_operation(
            capability,
            Box::new(move |state, current, meta, reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let context = drive_context.clone();
                Box::pin(async move {
                    if !matches!(
                        current.scope().control,
                        crate::harness::session::types::Control::CancelRequested { .. }
                    ) {
                        panic!(
                            "{}",
                            SessionInvariantError::new(
                                "Cancellation reconciliation requires cancelled durable control"
                            )
                        );
                    }
                    let record = operation_result_record(
                        &meta,
                        crate::harness::session::types::TerminalStatus::Aborted,
                        state.tip_id.clone(),
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

                    let mut events: Vec<HarnessEvent> = Vec::new();
                    match &meta.intent {
                        crate::harness::session::types::OperationIntent::Run { .. } => {
                            let mut summary_reason: Option<String> = None;
                            if let OperationState::SummaryDeciding { task, .. }
                            | OperationState::SummaryReady { task, .. }
                            | OperationState::SummaryEffectPending { task, .. }
                            | OperationState::SummaryRetryWait { task, .. } = current
                            {
                                if !matches!(
                                    task.boundary,
                                    crate::harness::session::types::ResultBoundary::ResumeCheckpoint { .. }
                                ) || task.reason.is_none()
                                {
                                    panic!(
                                        "{}",
                                        SessionInvariantError::new(
                                            "Cancelled run summary has an invalid result boundary"
                                        )
                                    );
                                }
                                summary_reason = task.reason.clone();
                            }
                            if let Some(reason) = summary_reason {
                                events.push(HarnessEvent::CompactionEnd {
                                    lane: lane_name.clone(),
                                    run_id: operation_id.clone(),
                                    reason,
                                    ended_at: record.ended_at,
                                    status:
                                        crate::harness::session::types::TerminalStatus::Aborted,
                                    entry_id: None,
                                    error: None,
                                    recovery: None,
                                });
                            }
                            events.push(HarnessEvent::RunEnd {
                                lane: lane_name.clone(),
                                run_id: operation_id.clone(),
                                from_tip_id: meta.source_tip_id.clone(),
                                tip_id: state.tip_id.clone(),
                                ended_at: record.ended_at,
                                status: crate::harness::session::types::TerminalStatus::Aborted,
                                error: None,
                                recovery: None,
                            });
                        }
                        crate::harness::session::types::OperationIntent::Compaction { .. } => {
                            events.push(HarnessEvent::CompactionEnd {
                                lane: lane_name.clone(),
                                run_id: operation_id.clone(),
                                reason: "manual".to_string(),
                                ended_at: record.ended_at,
                                status: crate::harness::session::types::TerminalStatus::Aborted,
                                entry_id: None,
                                error: None,
                                recovery: None,
                            });
                        }
                        crate::harness::session::types::OperationIntent::Navigation { .. } => {
                            events.push(HarnessEvent::NavigationEnd {
                                lane: lane_name.clone(),
                                run_id: operation_id.clone(),
                                from_tip_id: meta.source_tip_id.clone(),
                                tip_id: state.tip_id.clone(),
                                ended_at: record.ended_at,
                                status: crate::harness::session::types::TerminalStatus::Aborted,
                                error: None,
                                recovery: None,
                            });
                        }
                    }

                    let record_for_materialize = record.clone();
                    OperationCommand::Finish {
                        writes: cleanup,
                        record,
                        lane: Some(crate::harness::runtime::types::LanePatch {
                            tip_id: state.tip_id.clone(),
                            configuration: None,
                            inbox: None,
                        }),
                        materialize: Box::new(move |_| ProcedureResult::Settled {
                            outcome: record_for_materialize,
                        }),
                        events: Some(Box::new(move |_| events)),
                    }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(GateError::Closed)?;

    Ok(result)
}

/// 对应 `reconcileOperation`：推进一个已取消的 durable leaf，不启动新的普通工作。
pub async fn reconcile_operation<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
) -> Result<ProcedureResult, GateError> {
    let operation = lane.state().operation.clone();
    let Some(operation) = operation else {
        panic!(
            "{}",
            SessionInvariantError::new(format!(
                "Drive {} has no matching operation to reconcile",
                drive.operation_id
            ))
        );
    };
    if operation.meta.operation_id != drive.operation_id {
        panic!(
            "{}",
            SessionInvariantError::new(format!(
                "Drive {} has no matching operation to reconcile",
                drive.operation_id
            ))
        );
    }
    if !matches!(
        control_of(&operation.state),
        crate::harness::session::types::Control::CancelRequested { .. }
    ) {
        panic!(
            "{}",
            SessionInvariantError::new(format!(
                "Operation {} is not cancelled",
                drive.operation_id
            ))
        );
    }

    let cancellation: Cancellation = Arc::new(|| Box::pin(async {}));
    drive.begin_abort(cancellation);
    drive.signal_abort();

    let state = operation.state.clone();
    match &state {
        OperationState::AssistantEffectPending { .. } => {
            recover_cancelled_assistant_effect(Arc::clone(&lane), drive, &state).await
        }
        OperationState::Tools { .. } => run_tools(Arc::clone(&lane), drive, &state).await,
        OperationState::DeferredSuspended { .. } => {
            let handle = read_deferred_handle(&*lane, drive, &state).await?;
            cancel_deferred_best_effort(&*lane, drive, &state, &handle).await;
            publish_aborted_terminal(&*lane, drive, &state).await
        }
        OperationState::DeferredEffectPending { .. } => {
            let handle = read_deferred_handle(&*lane, drive, &state).await?;
            cancel_deferred_best_effort(&*lane, drive, &state, &handle).await;
            recover_cancelled_assistant_effect(Arc::clone(&lane), drive, &state).await
        }
        _ => publish_aborted_terminal(&*lane, drive, &state).await,
    }
}
