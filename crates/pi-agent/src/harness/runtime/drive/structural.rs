//! Rust 翻译自 packages/agent/src/harness/runtime/drive/structural.ts
//!
//! 结构化（compaction/branch-summary/navigation）驱动。本文件先落地不依赖
//! generation/response 的独立部分，runStructural* 系列待 generation/response 落地后补齐。

use std::sync::Arc;

use crate::harness::agent_harness::HarnessEvent;
use pi_ai::{AssistantMessage, Model, is_retryable_assistant_error};

use crate::harness::compaction::branch_summarization::{
    BranchPreparation, BranchSummaryResult, PreparedBranchSummaryOptions,
    generate_branch_summary_with_request,
};
use crate::harness::compaction::compaction::{
    CompactGenerationOptions, CompactResult, CompactionPreparation, SummaryRequest,
    compact_with_request, prepare_compaction, should_compact,
};
use crate::harness::compaction::utils::FileOperations;
use crate::harness::execution::effect_gate::GateError;
use crate::harness::runtime::drive::boundary::{BoundaryFinishPending, normalized_retry_policy};
use crate::harness::runtime::drive::retry::{retry_not_before, wait_until};
use crate::harness::runtime::drive::terminal::{operation_cleanup_writes, operation_result_record};
use crate::harness::runtime::transcript::read_bounded_entries;
use crate::harness::runtime::types::{
    ContinueOperationResult, Drive, Lane, LanePatch, LaneRuntimeState, OperationCommand,
    ProcedureResult,
};
use crate::harness::session::commit::{insert_entry, insert_usage};
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{
    Continuation, DurableFileOperations, DurableStructuralPreparation, NewCompactionEntry,
    NewEntry, OperationError, OperationState, SummaryContext, SummaryTask, UsageRow, Write,
};
use crate::harness::session::values::{branch_tip, entry_label, operation_preparation, set_value};

/// 对应 `durableFileOperations`。
fn durable_file_operations(file_ops: &FileOperations) -> DurableFileOperations {
    DurableFileOperations {
        read: file_ops.read.iter().cloned().collect(),
        written: file_ops.written.iter().cloned().collect(),
        edited: file_ops.edited.iter().cloned().collect(),
    }
}

/// 对应 `durableCompactionPreparation`。
pub fn durable_compaction_preparation(
    preparation: &CompactionPreparation,
) -> DurableStructuralPreparation {
    DurableStructuralPreparation::Compaction {
        messages_to_summarize: preparation.messages_to_summarize.clone(),
        turn_prefix_messages: preparation.turn_prefix_messages.clone(),
        retained_tail: preparation.retained_tail.clone(),
        is_split_turn: preparation.is_split_turn,
        tokens_before: preparation.tokens_before,
        previous_summary: preparation.previous_summary.clone(),
        file_ops: durable_file_operations(&preparation.file_ops),
        settings: preparation.settings.clone(),
    }
}

/// 对应 `durableBranchPreparation`。
pub fn durable_branch_preparation(preparation: &BranchPreparation) -> DurableStructuralPreparation {
    DurableStructuralPreparation::BranchSummary {
        messages: preparation.messages.clone(),
        file_ops: durable_file_operations(&preparation.file_ops),
        total_tokens: preparation.total_tokens,
    }
}

/// 对应 `fileOperations`（反向）。
fn file_operations(file_ops: &DurableFileOperations) -> FileOperations {
    FileOperations {
        read: file_ops.read.iter().cloned().collect(),
        written: file_ops.written.iter().cloned().collect(),
        edited: file_ops.edited.iter().cloned().collect(),
    }
}

/// 对应 `compactionPreparation`（反向）。
pub fn compaction_preparation(
    preparation: &DurableStructuralPreparation,
) -> Option<CompactionPreparation> {
    let DurableStructuralPreparation::Compaction {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    } = preparation
    else {
        return None;
    };
    Some(CompactionPreparation {
        messages_to_summarize: messages_to_summarize.clone(),
        turn_prefix_messages: turn_prefix_messages.clone(),
        retained_tail: retained_tail.clone(),
        is_split_turn: *is_split_turn,
        tokens_before: *tokens_before,
        previous_summary: previous_summary.clone(),
        file_ops: file_operations(file_ops),
        settings: settings.clone(),
    })
}

/// 对应 `branchPreparation`（反向）。
pub fn branch_preparation(preparation: &DurableStructuralPreparation) -> Option<BranchPreparation> {
    let DurableStructuralPreparation::BranchSummary {
        messages,
        file_ops,
        total_tokens,
    } = preparation
    else {
        return None;
    };
    Some(BranchPreparation {
        messages: messages.clone(),
        file_ops: file_operations(file_ops),
        total_tokens: *total_tokens,
    })
}

/// 对应 `prepareCompactionThreshold`。
pub async fn prepare_compaction_threshold<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    checkpoint: &OperationState,
) -> Result<ContinueOperationResult<Option<(String, DurableStructuralPreparation)>>, String> {
    let OperationState::Checkpoint { scope, .. } = checkpoint else {
        return Ok(ContinueOperationResult::Result { value: None });
    };
    let settings = &scope.settings.compaction;
    let identity = &lane.state().configuration.model;
    let model = lane
        .models()
        .get_model(&identity.provider, &identity.model_id);
    if !settings.enabled || model.is_none() {
        return Ok(ContinueOperationResult::Result { value: None });
    }
    let path = read_bounded_entries(lane, drive, checkpoint).await?;
    let ContinueOperationResult::Result { value: entries } = path else {
        return Ok(ContinueOperationResult::CancelRequested);
    };
    let trigger_index = entries
        .iter()
        .position(|entry| entry.base().id == checkpoint_trigger(checkpoint));
    let mut newest_compaction_index = -1i64;
    for index in (0..entries.len()).rev() {
        if matches!(
            entries[index],
            crate::harness::session::types::Entry::Compaction(_)
        ) {
            newest_compaction_index = index as i64;
            break;
        }
    }
    if newest_compaction_index >= trigger_index.map(|i| i as i64).unwrap_or(-1)
        && newest_compaction_index != -1
    {
        return Ok(ContinueOperationResult::Result { value: None });
    }
    if trigger_index.is_none() {
        return Err(
            SessionInvariantError::new("Checkpoint trigger is missing from its Branch").to_string(),
        );
    }
    let prepared = prepare_compaction(&entries, settings).map_err(|e| format!("{e}"))?;
    let Some(prepared) = prepared else {
        return Ok(ContinueOperationResult::Result { value: None });
    };
    let model = model.unwrap();
    if !should_compact(prepared.tokens_before, model.context_window, settings) {
        return Ok(ContinueOperationResult::Result { value: None });
    }
    let task_id = lane.session().id_generator().next(None);
    Ok(ContinueOperationResult::Result {
        value: Some((task_id, durable_compaction_preparation(&prepared))),
    })
}

fn checkpoint_trigger(checkpoint: &OperationState) -> String {
    match checkpoint {
        OperationState::Checkpoint {
            trigger_entry_id, ..
        } => trigger_entry_id.clone(),
        _ => String::new(),
    }
}

/// 对应 `prepareOverflowCompaction`。
pub async fn prepare_overflow_compaction<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    generation: &OperationState,
) -> Result<Option<(String, DurableStructuralPreparation)>, String> {
    let OperationState::AssistantEffectPending {
        generation_context,
        scope,
        ..
    } = generation
    else {
        return Ok(None);
    };
    if generation_context.overflow_recovery_used {
        return Ok(None);
    }
    let path = read_bounded_entries(lane, drive, generation).await?;
    let ContinueOperationResult::Result { value: entries } = path else {
        return Ok(None);
    };
    let prepared =
        prepare_compaction(&entries, &scope.settings.compaction).map_err(|e| format!("{e}"))?;
    let Some(prepared) = prepared else {
        return Ok(None);
    };
    let task_id = lane.session().id_generator().next(None);
    Ok(Some((task_id, durable_compaction_preparation(&prepared))))
}

/// 对应 `commitNavigation`：原子移动一个未摘要的导航并结束其 operation。
pub async fn commit_navigation<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    navigation: &OperationState,
) -> Result<ProcedureResult, crate::harness::execution::effect_gate::GateError> {
    let OperationState::NavigationReadyToCommit {
        target_id, label, ..
    } = navigation
    else {
        return Ok(ProcedureResult::Continue);
    };
    let lane_name = lane.name().to_string();
    let operation_id = drive.operation_id.clone();
    let context = drive.context.clone();
    let target_id = target_id.clone();
    let label = label.clone();

    let result = lane
        .continue_operation(
            navigation,
            Box::new(move |_state, current, meta, reader| {
                let lane_name = lane_name.clone();
                let operation_id = operation_id.clone();
                let context = context.clone();
                let target_id = target_id.clone();
                let label = label.clone();
                Box::pin(async move {
                    if let Some(target) = target_id.as_ref() {
                        let entries = reader
                            .get_entries(std::slice::from_ref(target), &context)
                            .await
                            .expect("get_entries failed");
                        if !entries.contains_key(target) {
                            panic!(
                                "{}",
                                SessionInvariantError::new(format!(
                                    "Navigation target {target} is missing"
                                ))
                            );
                        }
                    }
                    if target_id.as_deref() == meta.source_tip_id.as_deref() {
                        panic!(
                            "{}",
                            SessionInvariantError::new(
                                "Navigation target must differ from its source tip"
                            )
                        );
                    }
                    if target_id.is_none() && label.is_some() {
                        panic!(
                            "{}",
                            SessionInvariantError::new("Root navigation cannot set a label")
                        );
                    }
                    let mut writes: Vec<Write> = vec![Write::Value(set_value(
                        &branch_tip(&lane_name),
                        serde_json::json!(target_id),
                    ))];
                    if let (Some(label), Some(target)) = (label.as_ref(), target_id.as_ref()) {
                        writes.push(Write::Value(set_value(
                            &entry_label(target),
                            serde_json::json!(label),
                        )));
                    }
                    let cleanup = operation_cleanup_writes(
                        reader.as_ref(),
                        &operation_id,
                        &current,
                        &context,
                    )
                    .await
                    .expect("operation_cleanup_writes failed");
                    let record = operation_result_record(
                        &meta,
                        crate::harness::session::types::TerminalStatus::Completed,
                        target_id.clone(),
                        None,
                    );
                    writes.extend(cleanup);
                    let record_for_materialize = record.clone();
                    let record_for_events = record.clone();
                    let lane_name_for_events = lane_name.clone();
                    let operation_id_for_events = operation_id.clone();
                    let target_id_for_events = target_id.clone();
                    OperationCommand::Finish {
                        writes,
                        record,
                        lane: Some(crate::harness::runtime::types::LanePatch {
                            tip_id: target_id.clone(),
                            configuration: None,
                            inbox: None,
                        }),
                        materialize: Box::new(move |_| ProcedureResult::Settled {
                            outcome: record_for_materialize,
                        }),
                        events: Some(Box::new(move |_| {
                            vec![HarnessEvent::NavigationEnd {
                                lane: lane_name_for_events,
                                run_id: operation_id_for_events,
                                from_tip_id: meta.source_tip_id,
                                tip_id: target_id_for_events,
                                ended_at: record_for_events.ended_at,
                                status: crate::harness::session::types::TerminalStatus::Completed,
                                error: None,
                                recovery: None,
                            }]
                        })),
                    }
                })
            }),
            &drive.context,
        )
        .await
        .map_err(crate::harness::execution::effect_gate::GateError::Closed)?;

    Ok(match result {
        ContinueOperationResult::CancelRequested => ProcedureResult::Continue,
        ContinueOperationResult::Result { value } => value,
    })
}

include!("structural_run.rs");
