//! Rust 翻译自 packages/agent/src/harness/session/fork.ts

use std::collections::{HashMap, HashSet};

use serde_json::Value as Json;

use crate::harness::session::commit::CommittedWrite;
use crate::harness::session::fork_policy::{ForkScope, classify_fork_address};
use crate::harness::session::types::{Entry, ForkOptions, ForkPosition, LaneState};
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

    let (entry_ids, destination_tips) =
        select_fork_contents(&source_entries, &source_tips, options);
    let mut entries = HashMap::new();
    for id in &entry_ids {
        entries.insert(id.clone(), source_entries.get(id).unwrap().clone());
    }

    let mut scalar_values: Vec<StoredValue<Json>> = Vec::new();
    let mut next_seq = entries.values().map(|e| e.base().seq).max().unwrap_or(0) + 1;
    let mut store = |address: &Value<Json>, stored_value: Json| {
        scalar_values.push(StoredValue {
            address: value(&address.namespace, &address.key),
            value: stored_value,
            seq: next_seq,
        });
        next_seq += 1;
    };

    for (branch, tip_id) in &destination_tips {
        let configuration = find_stored_value(&source.scalar_values, &lane_config(branch).erased());
        store(
            &branch_tip(branch).erased(),
            serde_json::to_value(tip_id).unwrap_or(Json::Null),
        );
        if let Some(configuration) = configuration {
            store(
                &lane_config(branch).erased(),
                serde_json::to_value(configuration.value).unwrap_or(Json::Null),
            );
            store(
                &lane_state(branch).erased(),
                serde_json::to_value(LaneState {
                    current_operation_id: None,
                    last_operation_id: None,
                    inbox: Vec::new(),
                })
                .unwrap_or(Json::Null),
            );
        }
    }

    for stored in &source.scalar_values {
        let scope = match options {
            ForkOptions::Tree { .. } => ForkScope::Tree,
            ForkOptions::Branch { .. } => ForkScope::Branch,
        };
        match classify_fork_address(
            &stored.address.namespace,
            &stored.address.key,
            scope,
            |entry_id| entry_ids.contains(entry_id),
        ) {
            crate::harness::session::fork_policy::ForkDisposition::Copy => {
                store(&stored.address, stored.value.clone());
            }
            crate::harness::session::fork_policy::ForkDisposition::Exclude
            | crate::harness::session::fork_policy::ForkDisposition::Reconstruct => {}
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
) -> (HashSet<String>, HashMap<String, Option<String>>) {
    let mut entry_ids = HashSet::new();
    let mut destination_tips: HashMap<String, Option<String>> = HashMap::new();

    match options {
        ForkOptions::Tree { .. } => {
            for id in source_entries.keys() {
                entry_ids.insert(id.clone());
            }
            for stored in source_tips {
                let tip: Option<String> =
                    serde_json::from_value(stored.value.clone()).unwrap_or(None);
                destination_tips.insert(stored.address.key.clone(), tip);
            }
        }
        ForkOptions::Branch {
            branch,
            entry_id,
            position,
            ..
        } => {
            let source_tip = source_tips
                .iter()
                .find(|stored| stored.address.key == *branch)
                .ok_or_else(|| format!("Unknown source branch: {branch}"))
                .unwrap();
            let source_tip_value: Option<String> =
                serde_json::from_value(source_tip.value.clone()).unwrap_or(None);
            let requested = entry_id.clone().or(source_tip_value.clone());
            let mut found = requested.is_none();
            let mut tip_id: Option<String> = None;
            let mut current = source_tip_value;
            while let Some(entry_id) = current {
                let entry = source_entries
                    .get(&entry_id)
                    .ok_or_else(|| format!("Corrupt source branch: missing parent {entry_id}"))
                    .unwrap();
                if Some(entry.base().id.clone()) == requested {
                    found = true;
                    let position_before = position == &Some(ForkPosition::Before);
                    tip_id = if position_before {
                        entry.base().parent_id.clone()
                    } else {
                        Some(entry.base().id.clone())
                    };
                    if !position_before {
                        entry_ids.insert(entry.base().id.clone());
                    }
                } else if found {
                    entry_ids.insert(entry.base().id.clone());
                }
                current = entry.base().parent_id.clone();
            }
            if !found {
                panic!("Fork entry {requested:?} is not on source branch {branch:?}");
            }
            destination_tips.insert(branch.clone(), tip_id);
        }
    }

    (entry_ids, destination_tips)
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
