//! Rust 翻译自 packages/agent/src/harness/runtime/restore.ts

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::harness::context::Context;
use crate::harness::runtime::types::LaneRuntimeState;
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{
    self, LaneState as DurableLaneState, Operation, OperationIntent, OperationMeta, OperationState,
    ResultBoundary, Session, SessionMutationCallback, SessionReader, SummaryTask,
};
use crate::harness::session::values::{
    StoredValue, branch_tip, branch_tip_inventory_prefix, lane_config,
    lane_state as lane_state_value, operation_meta, operation_state,
};

fn state_task(state: &OperationState) -> Option<&SummaryTask> {
    match state {
        OperationState::SummaryDeciding { task, .. }
        | OperationState::SummaryReady { task, .. }
        | OperationState::SummaryEffectPending { task, .. }
        | OperationState::SummaryRetryWait { task, .. } => Some(task),
        _ => None,
    }
}

/// 对应 `stateMatchesIntent`。
fn state_matches_intent(intent: &OperationIntent, state: &OperationState) -> bool {
    match intent {
        OperationIntent::Compaction { .. } => state_task(state)
            .map(|t| matches!(t.boundary, ResultBoundary::Finish))
            .unwrap_or(false),
        OperationIntent::Navigation {
            target_id,
            summarize,
            label,
            custom_instructions,
        } => {
            if let OperationState::NavigationReadyToCommit {
                target_id: state_target,
                label: state_label,
                ..
            } = state
            {
                return !summarize && state_target == target_id && state_label == label;
            }
            *summarize
                && state_task(state)
                    .map(|t| {
                        matches!(
                            &t.boundary,
                            ResultBoundary::CommitNavigation {
                                target_id: t_id,
                                label: t_label
                            } if target_id.as_deref() == Some(t_id.as_str()) && t_label == label
                        ) && t.custom_instructions == *custom_instructions
                    })
                    .unwrap_or(false)
        }
        OperationIntent::Run { .. } => {
            !matches!(state, OperationState::NavigationReadyToCommit { .. })
                && state_task(state)
                    .map(|t| matches!(t.boundary, ResultBoundary::ResumeCheckpoint { .. }))
                    .unwrap_or(true)
        }
    }
}

/// 对应 `ClassifiedLaneStorage`。
#[derive(Debug, Clone)]
pub enum ClassifiedLaneStorage {
    Absent,
    Branch {
        tip: StoredValue<serde_json::Value>,
    },
    Lane {
        tip: StoredValue<serde_json::Value>,
        configuration: StoredValue<serde_json::Value>,
        lane_state: StoredValue<serde_json::Value>,
    },
}

/// 对应 `classifyLaneStorage`。
fn classify_lane_storage(
    lane: &str,
    tip: Option<StoredValue<serde_json::Value>>,
    configuration: Option<StoredValue<serde_json::Value>>,
    lane_state: Option<StoredValue<serde_json::Value>>,
) -> ClassifiedLaneStorage {
    match (tip, configuration, lane_state) {
        (None, None, None) => ClassifiedLaneStorage::Absent,
        (Some(tip), None, None) => ClassifiedLaneStorage::Branch { tip },
        (None, _, _) => panic!(
            "{}",
            SessionInvariantError::new(format!("Lane {lane:?} is missing branch.tip"))
        ),
        (Some(_), None, _) => panic!(
            "{}",
            SessionInvariantError::new(format!("Lane {lane:?} is missing lane.config"))
        ),
        (Some(_), Some(_), None) => panic!(
            "{}",
            SessionInvariantError::new(format!("Lane {lane:?} is missing lane.state"))
        ),
        (Some(tip), Some(configuration), Some(lane_state)) => ClassifiedLaneStorage::Lane {
            tip,
            configuration,
            lane_state,
        },
    }
}

/// 对应 `readLaneStorage`。
pub async fn read_lane_storage(
    reader: &dyn SessionReader,
    lane: &str,
    context: &Context,
) -> Result<ClassifiedLaneStorage, String> {
    let tip = reader
        .get_value(&branch_tip(lane).erased(), context)
        .await?;
    let configuration = reader
        .get_value(&lane_config(lane).erased(), context)
        .await?;
    let lane_state = reader
        .get_value(&lane_state_value(lane).erased(), context)
        .await?;
    Ok(classify_lane_storage(lane, tip, configuration, lane_state))
}

/// 对应 `restoreLaneState`。
pub async fn restore_lane_state(
    reader: &dyn SessionReader,
    lane: &str,
    stored: &ClassifiedLaneStorage,
    context: &Context,
) -> Result<LaneRuntimeState, String> {
    let ClassifiedLaneStorage::Lane {
        tip,
        configuration,
        lane_state,
    } = stored
    else {
        panic!(
            "{}",
            SessionInvariantError::new(format!("Lane {lane:?} is not configured"))
        )
    };
    let durable: DurableLaneState = serde_json::from_value(lane_state.value.clone())
        .unwrap_or_else(|e| panic!("parse lane state: {e}"));
    let operation_id = durable.current_operation_id;
    let mut operation = None;
    if let Some(operation_id) = operation_id {
        let meta = reader
            .get_value(&operation_meta(&operation_id).erased(), context)
            .await?
            .unwrap_or_else(|| {
                panic!(
                    "{}",
                    SessionInvariantError::new(format!(
                        "Operation {operation_id} is missing op.meta"
                    ))
                )
            });
        let state = reader
            .get_value(&operation_state(&operation_id).erased(), context)
            .await?
            .unwrap_or_else(|| {
                panic!(
                    "{}",
                    SessionInvariantError::new(format!(
                        "Operation {operation_id} is missing op.state"
                    ))
                )
            });
        let meta: OperationMeta = serde_json::from_value(meta.value)
            .unwrap_or_else(|e| panic!("parse operation meta: {e}"));
        let state: OperationState = serde_json::from_value(state.value)
            .unwrap_or_else(|e| panic!("parse operation state: {e}"));
        if meta.operation_id != operation_id {
            panic!(
                "{}",
                SessionInvariantError::new(format!(
                    "Operation {operation_id} metadata names operation {:?}",
                    meta.operation_id
                ))
            );
        }
        if meta.lane != lane {
            panic!(
                "{}",
                SessionInvariantError::new(format!(
                    "Operation {operation_id} belongs to lane {:?}, not {lane:?}",
                    meta.lane
                ))
            );
        }
        if !state_matches_intent(&meta.intent, &state) {
            panic!(
                "{}",
                SessionInvariantError::new(format!(
                    "Operation {operation_id} intent {:?} does not match state {}",
                    meta.intent,
                    state.at()
                ))
            );
        }
        operation = Some(Operation { meta, state });
    }

    let configuration = serde_json::from_value(configuration.value.clone())
        .unwrap_or_else(|e| panic!("parse lane configuration: {e}"));
    let tip_id =
        serde_json::from_value(tip.value.clone()).unwrap_or_else(|e| panic!("parse tip: {e}"));

    Ok(LaneRuntimeState {
        tip_id,
        configuration,
        inbox: durable.inbox,
        last_operation_id: durable.last_operation_id,
        operation,
    })
}

/// 对应 `restoreSession`（Arc 版本）。
pub async fn restore_session_arc(
    session: &Arc<dyn Session>,
    context: &Context,
) -> Result<BTreeMap<String, LaneRuntimeState>, String> {
    let context = context.clone();
    let session_arc = Arc::clone(session);
    let mutation: SessionMutationCallback<BTreeMap<String, LaneRuntimeState>> =
        Arc::new(move |reader, ctx| {
            Box::pin(async move {
                let tips = reader
                    .scan_values(&branch_tip_inventory_prefix().erased(), &ctx)
                    .await?;
                let configurations = reader.scan_values(&lane_config("").erased(), &ctx).await?;
                let states = reader
                    .scan_values(&lane_state_value("").erased(), &ctx)
                    .await?;
                let tip_by_lane: BTreeMap<String, StoredValue<serde_json::Value>> = tips
                    .into_iter()
                    .map(|v| (v.address.key.clone(), v))
                    .collect();
                let configuration_by_lane: BTreeMap<String, StoredValue<serde_json::Value>> =
                    configurations
                        .into_iter()
                        .map(|v| (v.address.key.clone(), v))
                        .collect();
                let state_by_lane: BTreeMap<String, StoredValue<serde_json::Value>> = states
                    .into_iter()
                    .map(|v| (v.address.key.clone(), v))
                    .collect();
                let names: BTreeSet<String> = tip_by_lane
                    .keys()
                    .chain(configuration_by_lane.keys())
                    .chain(state_by_lane.keys())
                    .cloned()
                    .collect();
                let mut restored = BTreeMap::new();
                for lane in names {
                    let stored = classify_lane_storage(
                        &lane,
                        tip_by_lane.get(&lane).cloned(),
                        configuration_by_lane.get(&lane).cloned(),
                        state_by_lane.get(&lane).cloned(),
                    );
                    if !matches!(stored, ClassifiedLaneStorage::Lane { .. }) {
                        continue;
                    }
                    let state = restore_lane_state(reader.as_ref(), &lane, &stored, &ctx).await?;
                    restored.insert(lane, state);
                }
                Ok(restored)
            })
        });
    types::mutate(session_arc.as_ref(), mutation, &context).await
}

/// 对应 `restoreLane`：恢复单个已配置 lane。
pub async fn restore_lane(
    session: &Arc<dyn Session>,
    lane: &str,
    context: &Context,
) -> Result<LaneRuntimeState, String> {
    let lane_owned = lane.to_string();
    let context = context.clone();
    let session_arc = Arc::clone(session);
    let mutation: SessionMutationCallback<LaneRuntimeState> = Arc::new(move |reader, ctx| {
        let lane = lane_owned.clone();
        Box::pin(async move {
            let stored = read_lane_storage(reader.as_ref(), &lane, &ctx).await?;
            match stored {
                ClassifiedLaneStorage::Absent => Err(SessionInvariantError::new(format!(
                    "Lane {lane:?} is missing branch.tip"
                ))
                .to_string()),
                ClassifiedLaneStorage::Branch { .. } => Err(SessionInvariantError::new(format!(
                    "Lane {lane:?} is missing lane.config"
                ))
                .to_string()),
                ClassifiedLaneStorage::Lane { .. } => {
                    restore_lane_state(reader.as_ref(), &lane, &stored, &ctx).await
                }
            }
        })
    });
    types::mutate(session_arc.as_ref(), mutation, &context).await
}
