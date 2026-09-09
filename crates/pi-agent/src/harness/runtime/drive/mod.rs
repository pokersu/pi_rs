//! Rust 翻译自 packages/agent/src/harness/runtime/drive/（目录）与 `drive.ts` 入口。

use std::sync::Arc;

use crate::harness::agent_harness::DriveOutcome;
use crate::harness::execution::effect_gate::GateError;
use crate::harness::hooks::HookName;
use crate::harness::runtime::types::{Drive, Lane, ProcedureResult};
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{Control, Operation, OperationIntent, OperationState};

pub mod boundary;
pub mod checkpoint;
pub mod deferred;
pub mod generation;
pub mod reconcile;
pub mod recovery;
pub mod response;
pub mod retry;
pub mod structural;
pub mod terminal;
#[path = "tool-placement.rs"]
pub mod tool_placement;
pub mod tools;

pub use retry::{retry_not_before, wait_until};
pub use terminal::{operation_cleanup_writes, operation_result_record};

/// 对应 `currentOperation`：读取并校验当前 operation 属于本 drive。
fn current_operation<L: Lane + ?Sized>(lane: &L, drive: &Drive) -> Operation {
    let operation = lane.state().operation.clone().unwrap_or_else(|| {
        panic!(
            "{}",
            SessionInvariantError::new(format!(
                "Drive {} has no matching current operation",
                drive.operation_id
            ))
        )
    });
    if operation.meta.operation_id != drive.operation_id {
        panic!(
            "{}",
            SessionInvariantError::new(format!(
                "Drive {} has no matching current operation",
                drive.operation_id
            ))
        );
    }
    operation
}

/// 提取任意 leaf 的 `Control`（含 deferred scope）。
fn state_control(state: &OperationState) -> &Control {
    match state {
        OperationState::Starting { scope, .. }
        | OperationState::Checkpoint { scope, .. }
        | OperationState::AssistantReady { scope, .. }
        | OperationState::AssistantEffectPending { scope, .. }
        | OperationState::AssistantRetryWait { scope, .. }
        | OperationState::Tools { scope, .. }
        | OperationState::SummaryDeciding { scope, .. }
        | OperationState::SummaryReady { scope, .. }
        | OperationState::SummaryEffectPending { scope, .. }
        | OperationState::SummaryRetryWait { scope, .. }
        | OperationState::NavigationReadyToCommit { scope, .. } => &scope.control,
        OperationState::DeferredSuspended { scope } => &scope.control,
        OperationState::DeferredEffectPending { scope, .. } => &scope.control,
    }
}

pub fn intent_kind(intent: &OperationIntent) -> &'static str {
    match intent {
        OperationIntent::Run { .. } => "run",
        OperationIntent::Compaction { .. } => "compaction",
        OperationIntent::Navigation { .. } => "navigation",
    }
}

/// 对应 `driveOperation` 的叶子分派。
async fn dispatch<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
    state: &OperationState,
) -> Result<ProcedureResult, GateError> {
    if matches!(state_control(state), Control::CancelRequested { .. }) {
        return reconcile::reconcile_operation(lane, drive).await;
    }
    match state {
        OperationState::Starting { .. } => checkpoint::start_run(lane.as_ref(), drive, state).await,
        OperationState::Checkpoint { .. } => {
            checkpoint::run_checkpoint(lane.as_ref(), drive, state).await
        }
        OperationState::AssistantReady { .. } => {
            generation::run_generation(lane, drive, state).await
        }
        OperationState::AssistantRetryWait { .. } => {
            generation::run_retry_wait(lane.as_ref(), drive, state).await
        }
        OperationState::AssistantEffectPending { .. } => {
            recovery::recover_assistant_generation(lane, drive, state).await
        }
        OperationState::Tools { .. } => tools::run_tools(lane, drive, state).await,
        OperationState::DeferredSuspended { .. } | OperationState::DeferredEffectPending { .. } => {
            deferred::run_deferred(lane, drive, state).await
        }
        OperationState::SummaryDeciding { .. } => {
            structural::run_structural_decision(lane.as_ref(), drive, state).await
        }
        OperationState::SummaryReady { .. } => {
            structural::run_structural_generation(lane.as_ref(), drive, state).await
        }
        OperationState::SummaryEffectPending { .. } => {
            structural::recover_structural_generation(lane.as_ref(), drive, state).await
        }
        OperationState::SummaryRetryWait { .. } => {
            structural::run_structural_retry_wait(lane.as_ref(), drive, state).await
        }
        OperationState::NavigationReadyToCommit { .. } => {
            structural::commit_navigation(lane.as_ref(), drive, state).await
        }
    }
}

/// 对应 `driveOperation`：推进一次安装的 durable procedure 直到 settle 或 durable wait。
pub async fn drive_operation<L: Lane + 'static>(
    lane: Arc<L>,
    drive: &Drive,
) -> Result<DriveOutcome, GateError> {
    let mut operation = current_operation(lane.as_ref(), drive);
    if matches!(state_control(&operation.state), Control::Running) {
        let event = serde_json::json!({
            "lane": lane.name(),
            "runId": drive.operation_id,
            "operation": intent_kind(&operation.meta.intent),
        });
        match lane
            .hooks()
            .run_with_gate(HookName::BeforeDrive, event, &drive.gate, &drive.context)
            .await
        {
            Ok(_) => {}
            Err(GateError::AbortRequested(abort)) => {
                (abort.cancellation)().await;
            }
            Err(error) => return Err(error),
        }
    }

    loop {
        operation = current_operation(lane.as_ref(), drive);
        let state = operation.state.clone();
        let result = match dispatch(Arc::clone(&lane), drive, &state).await {
            Ok(value) => value,
            Err(GateError::AbortRequested(abort)) => {
                (abort.cancellation)().await;
                ProcedureResult::Continue
            }
            Err(error) => return Err(error),
        };
        match result {
            ProcedureResult::Settled { outcome } => return Ok(DriveOutcome::Settled { outcome }),
            ProcedureResult::Waiting { outcome } => return Ok(outcome),
            ProcedureResult::Continue => {
                let next = current_operation(lane.as_ref(), drive).state;
                if next == state && !matches!(state_control(&next), Control::CancelRequested { .. })
                {
                    panic!(
                        "{}",
                        SessionInvariantError::new(format!(
                            "Drive procedure made no progress from {}",
                            state.at()
                        ))
                    );
                }
            }
        }
    }
}
