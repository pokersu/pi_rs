//! Rust 翻译自 packages/agent/src/harness/session/fork.ts

use std::collections::{HashMap, HashSet};

use serde_json::Value as Json;

use crate::harness::session::commit::CommittedWrite;
use crate::harness::session::fork_policy::{
    ForkCurrentStatePlan, project_fork_current_state_write, select_branch_fork,
};
use crate::harness::session::types::{Entry, ForkOptions};
use crate::harness::session::values::{
    StoredValue, Value, branch_tip, lane_config, lane_state, value,
};

/// 对应 `ForkSourceSnapshot`。
#[derive(Debug, Clone)]
pub struct ForkSourceSnapshot {
    pub entries: Vec<Entry>,
    pub scalar_values: Vec<StoredValue<Json>>,
    pub entries_complete: Option<bool>,
}

/// 对应 `ForkDestinationSnapshot`。
#[derive(Debug, Clone)]
pub struct ForkDestinationSnapshot {
    pub entries: HashMap<String, Entry>,
    pub scalar_values: Vec<StoredValue<Json>>,
    pub next_seq: u64,
}

fn stored_values_in_namespace(
    values: &[StoredValue<Json>],
    address: &Value<Json>,
) -> Vec<StoredValue<Json>> {
    values
        .iter()
        .filter(|stored| stored.address.namespace == address.namespace)
        .cloned()
        .collect()
}

fn find_stored_value(
    values: &[StoredValue<Json>],
    address: &Value<Json>,
) -> Option<StoredValue<Json>> {
    values
        .iter()
        .find(|stored| {
            stored.address.namespace == address.namespace && stored.address.key == address.key
        })
        .cloned()
}

/// 对应 `createForkSnapshot`。
pub fn create_fork_snapshot(
    source: &ForkSourceSnapshot,
    options: &ForkOptions,
) -> ForkDestinationSnapshot {
    let source_entries: HashMap<String, Entry> = source
        .entries
        .iter()
        .map(|e| (e.base().id.clone(), e.clone()))
        .collect();
    let source_tips = stored_values_in_namespace(&source.scalar_values, &branch_tip("").erased());
    validate_fork_source_snapshot(source, &source_entries, &source_tips, options);

    let (entry_ids, plan) = select_fork_contents(&source_entries, &source_tips, options);
    let mut entries = HashMap::new();
    for id in &entry_ids {
        entries.insert(id.clone(), source_entries.get(id).unwrap().clone());
    }

    let mut scalar_values: Vec<StoredValue<Json>> = Vec::new();
    let mut next_seq = entries.values().map(|e| e.base().seq).max().unwrap_or(0) + 1;
    for stored in &source.scalar_values {
        let projected = project_fork_current_state_write(
            &CommittedWrite::ValueSet {
                seq: stored.seq,
                namespace: stored.address.namespace.clone(),
                key: stored.address.key.clone(),
                value: stored.value.clone(),
            },
            &plan,
            |entry_id| entry_ids.contains(entry_id),
        );
        if let Some(CommittedWrite::ValueSet {
            namespace,
            key,
            value: projected_value,
            ..
        }) = projected
        {
            scalar_values.push(StoredValue {
                address: value(&namespace, &key),
                value: projected_value,
                seq: next_seq,
            });
            next_seq += 1;
        }
    }

    ForkDestinationSnapshot {
        entries,
        scalar_values,
        next_seq,
    }
}

/// 对应 `forkSnapshotWrites`。
pub fn fork_snapshot_writes(snapshot: &ForkDestinationSnapshot) -> Vec<CommittedWrite> {
    let mut writes: Vec<CommittedWrite> = Vec::new();
    for entry in snapshot.entries.values() {
        writes.push(CommittedWrite::Entry {
            entry: entry.clone(),
        });
    }
    for stored in &snapshot.scalar_values {
        writes.push(CommittedWrite::ValueSet {
            seq: stored.seq,
            namespace: stored.address.namespace.clone(),
            key: stored.address.key.clone(),
            value: stored.value.clone(),
        });
    }
    writes.sort_by_key(|w| w.seq());
    writes
}

fn select_fork_contents(
    source_entries: &HashMap<String, Entry>,
    source_tips: &[StoredValue<Json>],
    options: &ForkOptions,
) -> (HashSet<String>, ForkCurrentStatePlan) {
    let mut entry_ids = HashSet::new();
    match options {
        ForkOptions::Tree { .. } => {
            for id in source_entries.keys() {
                entry_ids.insert(id.clone());
            }
            (entry_ids, ForkCurrentStatePlan::Tree)
        }
        ForkOptions::Branch {
            branch,
            entry_id,
            position,
            ..
        } => {
            let tip = source_tips
                .iter()
                .find(|stored| stored.address.key == *branch)
                .map(|stored| {
                    serde_json::from_value::<Option<String>>(stored.value.clone()).unwrap_or(None)
                });
            let plan = select_branch_fork(
                branch,
                tip,
                entry_id.clone(),
                *position,
                |entry_id| {
                    source_entries
                        .get(entry_id)
                        .map(|e| e.base().parent_id.clone())
                },
                |entry_id| {
                    entry_ids.insert(entry_id.to_string());
                },
            );
            (entry_ids, plan)
        }
    }
}

fn validate_fork_source_snapshot(
    source: &ForkSourceSnapshot,
    source_entries: &HashMap<String, Entry>,
    source_tips: &[StoredValue<Json>],
    options: &ForkOptions,
) {
    let source_tip_keys: HashSet<&str> =
        source_tips.iter().map(|s| s.address.key.as_str()).collect();
    let lane_config_ns = lane_config("").namespace;
    let lane_state_ns = lane_state("").namespace;

    for stored in &source.scalar_values {
        if (stored.address.namespace == lane_config_ns || stored.address.namespace == lane_state_ns)
            && !source_tip_keys.contains(stored.address.key.as_str())
        {
            panic!(
                "Source session branch {:?} is missing branch.tip",
                stored.address.key
            );
        }
    }

    for tip in source_tips {
        let configuration = find_stored_value(
            &source.scalar_values,
            &lane_config(&tip.address.key).erased(),
        );
        let state = find_stored_value(
            &source.scalar_values,
            &lane_state(&tip.address.key).erased(),
        );
        if configuration.is_some() != state.is_some() {
            panic!(
                "Source session branch {:?} has incomplete lane state",
                tip.address.key
            );
        }
        if let ForkOptions::Branch { branch, .. } = options
            && tip.address.key == *branch
            && configuration.is_none()
        {
            panic!("Source branch {branch:?} is not a configured AgentLane");
        }
        let tip_value: Option<String> = serde_json::from_value(tip.value.clone()).unwrap_or(None);
        if (source.entries_complete != Some(false) || matches!(options, ForkOptions::Tree { .. }))
            && tip_value.is_some()
            && !source_entries.contains_key(tip_value.as_ref().unwrap())
        {
            panic!(
                "Source session branch {:?} has an unknown tip",
                tip.address.key
            );
        }
    }
}
