//! Rust 翻译自 packages/agent/src/harness/session/fork-policy.ts

use serde_json::Value as Json;

use crate::harness::session::commit::CommittedWrite;
use crate::harness::session::types::{ForkPosition, LaneState};

/// 对应 `ForkCurrentStatePlan`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkCurrentStatePlan {
    Branch {
        branch: String,
        destination_tip: Option<String>,
    },
    Tree,
}

/// 对应 `selectBranchFork`：沿 source branch 的 tip 祖先链选择要复制的 entries。
pub fn select_branch_fork(
    branch: &str,
    tip: Option<Option<String>>,
    entry_id: Option<String>,
    position: Option<ForkPosition>,
    get_parent: impl Fn(&str) -> Option<Option<String>>,
    mut select_entry: impl FnMut(&str),
) -> ForkCurrentStatePlan {
    let Some(tip) = tip else {
        panic!("Unknown source branch: {branch:?}");
    };
    let requested = entry_id.or(tip.clone());
    let mut found = requested.is_none();
    let mut destination_tip: Option<String> = None;
    let mut current: Option<String> = tip;
    while let Some(entry_id) = current {
        let Some(parent_id) = get_parent(&entry_id) else {
            panic!("Corrupt source branch: missing parent {entry_id}");
        };
        if Some(entry_id.clone()) == requested {
            found = true;
            destination_tip = if position == Some(ForkPosition::Before) {
                parent_id.clone()
            } else {
                Some(entry_id.clone())
            };
            if position != Some(ForkPosition::Before) {
                select_entry(&entry_id);
            }
        } else if found {
            select_entry(&entry_id);
        }
        current = parent_id;
    }
    if !found {
        panic!("Fork entry {requested:?} is not on source branch {branch:?}");
    }
    ForkCurrentStatePlan::Branch {
        branch: branch.to_string(),
        destination_tip,
    }
}

/// 对应 `projectForkCurrentStateWrite`：投影单个当前值行/幸存列表元素到 destination。
pub fn project_fork_current_state_write(
    write: &CommittedWrite,
    plan: &ForkCurrentStatePlan,
    is_entry_copied: impl Fn(&str) -> bool,
) -> Option<CommittedWrite> {
    let (namespace, key) = match write {
        CommittedWrite::ValueSet { namespace, key, .. }
        | CommittedWrite::ListAppend { namespace, key, .. } => (namespace, key),
        _ => return None,
    };

    let with_value = |new_value: Json| {
        let mut cloned = write.clone();
        match &mut cloned {
            CommittedWrite::ValueSet { value, .. } | CommittedWrite::ListAppend { value, .. } => {
                *value = new_value;
            }
            _ => unreachable!(),
        }
        cloned
    };

    match namespace.as_str() {
        "pi.session.name" => Some(write.clone()),
        "pi.entry.label" => {
            if is_entry_copied(key) {
                Some(write.clone())
            } else {
                None
            }
        }
        "pi.branch.tip" => match plan {
            ForkCurrentStatePlan::Tree => Some(write.clone()),
            ForkCurrentStatePlan::Branch {
                destination_tip, ..
            } if key == plan_branch(plan) => Some(with_value(
                serde_json::to_value(destination_tip).unwrap_or(Json::Null),
            )),
            ForkCurrentStatePlan::Branch { .. } => None,
        },
        "pi.lane.config" => match plan {
            ForkCurrentStatePlan::Tree => Some(write.clone()),
            ForkCurrentStatePlan::Branch { .. } if key == plan_branch(plan) => Some(write.clone()),
            ForkCurrentStatePlan::Branch { .. } => None,
        },
        "pi.lane.state" => match plan {
            ForkCurrentStatePlan::Tree => Some(write.clone()),
            ForkCurrentStatePlan::Branch { .. } if key == plan_branch(plan) => Some(with_value(
                serde_json::to_value(LaneState {
                    current_operation_id: None,
                    last_operation_id: None,
                    inbox: Vec::new(),
                })
                .unwrap_or(Json::Null),
            )),
            ForkCurrentStatePlan::Branch { .. } => None,
        },
        "pi.result" => None,
        _ => {
            if namespace.starts_with("pi.op.") || namespace.starts_with("pi.pending.") {
                return None;
            }
            if namespace == "pi" || namespace.starts_with("pi.") {
                panic!("Unknown reserved fork namespace: {namespace}");
            }
            match plan {
                ForkCurrentStatePlan::Tree => Some(write.clone()),
                ForkCurrentStatePlan::Branch { .. } => None,
            }
        }
    }
}

fn plan_branch(plan: &ForkCurrentStatePlan) -> &str {
    match plan {
        ForkCurrentStatePlan::Branch { branch, .. } => branch,
        ForkCurrentStatePlan::Tree => "",
    }
}
