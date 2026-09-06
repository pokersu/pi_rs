//! Rust 翻译自 packages/agent/src/harness/runtime/drive/terminal.ts

use std::collections::HashSet;

use crate::harness::context::Context;
use crate::harness::session::types::{
    OperationError, OperationMeta, OperationResultRecord, OperationState, SessionReader,
    TerminalStatus, Write,
};
use crate::harness::session::values::{
    delete_list, delete_value, operation_meta, operation_preparation_prefix, operation_state,
    operation_tool_args_prefix, operation_tool_memo_prefix, pending_assistant_frames,
    pending_entry, pending_tool_output_prefix,
};

/// 对应 `operationCleanupWrites`。
pub async fn operation_cleanup_writes(
    reader: &dyn SessionReader,
    operation_id: &str,
    state: &OperationState,
    context: &Context,
) -> Result<Vec<Write>, String> {
    let tool_arguments = reader
        .scan_values(&operation_tool_args_prefix(operation_id, None), context)
        .await?;
    let tool_memos = reader
        .scan_values(&operation_tool_memo_prefix(operation_id, None), context)
        .await?;
    let preparations = reader
        .scan_values(
            &operation_preparation_prefix(operation_id).erased(),
            context,
        )
        .await?;
    let tool_outputs = reader
        .scan_values(&pending_tool_output_prefix(operation_id).erased(), context)
        .await?;

    let mut pending_ids = HashSet::new();
    if let OperationState::Tools { batch, .. } = state {
        for call in &batch.calls {
            let crate::harness::session::types::ToolCall::OutcomeReady {
                result_entry_id, ..
            } = call
            else {
                continue;
            };
            pending_ids.insert(result_entry_id.clone());
        }
    }

    let frame_delete = match state {
        OperationState::AssistantEffectPending {
            response_entry_id, ..
        }
        | OperationState::DeferredEffectPending {
            response_entry_id, ..
        } => Some(delete_list(&pending_assistant_frames(
            operation_id,
            response_entry_id,
        ))),
        _ => None,
    };

    let mut writes: Vec<Write> = Vec::new();
    writes.push(Write::Value(delete_value(&operation_meta(operation_id))));
    writes.push(Write::Value(delete_value(&operation_state(operation_id))));
    for stored in tool_arguments {
        writes.push(Write::Value(delete_value(&stored.address)));
    }
    for stored in tool_memos {
        writes.push(Write::Value(delete_value(&stored.address)));
    }
    for stored in preparations {
        writes.push(Write::Value(delete_value(&stored.address)));
    }
    for stored in tool_outputs {
        writes.push(Write::Value(delete_value(&stored.address)));
    }
    if let Some(frame_delete) = frame_delete {
        writes.push(Write::List(frame_delete));
    }
    for id in pending_ids {
        writes.push(Write::Value(delete_value(&pending_entry(&id))));
    }
    Ok(writes)
}

/// 对应 `operationResultRecord`。
pub fn operation_result_record(
    meta: &OperationMeta,
    status: TerminalStatus,
    tip_id: Option<String>,
    error: Option<OperationError>,
) -> OperationResultRecord {
    if (status == TerminalStatus::Failed) != error.is_some() {
        panic!("Only a failed operation result may carry an error");
    }
    OperationResultRecord {
        operation_id: meta.operation_id.clone(),
        kind: meta.intent.clone(),
        status,
        error,
        from_tip_id: meta.source_tip_id.clone(),
        tip_id,
        started_at: meta.started_at,
        ended_at: pi_ai::utils::uuid::now_ms() as u64,
    }
}
