//! Rust 翻译自 packages/agent/src/harness/session/in-memory-storage-state.ts
//!
//! 完整物化 session 状态，供 MemoryStorage 与 JsonlStorage 使用。

use std::collections::{HashMap, HashSet};

use serde_json::Value as Json;

use crate::harness::session::commit::{
    CommittedWrite, PreparedCommit, prepare_storage_commit, validate_committed_writes,
};
use crate::harness::session::fork_policy::{
    ForkCurrentStatePlan, project_fork_current_state_write, select_branch_fork,
};
use crate::harness::session::types::{
    Entry, EntryScan, EntryStructure, ForkOptions, ScanOrder, SessionStats, StorageBranchScan,
    UsageRow, UsageScan, Write,
};
use crate::harness::session::values::{
    ListElement, ListOrder, ListReadOptions, StoredValue, Value, ValueList, branch_tip,
    lane_config, lane_state, list, resolve_list_read_options, value,
};
use crate::harness::utils::usage::{add_usage, empty_usage};

struct StoredListSnapshot {
    address: ValueList<Json>,
    elements: Vec<ListElement<Json>>,
}

/// 对应 `MemoryForkPlan`。
struct MemoryForkPlan {
    plan: ForkCurrentStatePlan,
    entry_ids: HashSet<String>,
}

impl MemoryForkPlan {
    fn is_entry_copied(&self, entry_id: &str) -> bool {
        match &self.plan {
            ForkCurrentStatePlan::Tree => true,
            ForkCurrentStatePlan::Branch { .. } => self.entry_ids.contains(entry_id),
        }
    }
}

fn physical_key(namespace: &str, key: &str) -> String {
    format!("{namespace}\0{key}")
}

fn compare_keys(left: &str, right: &str) -> std::cmp::Ordering {
    let left: Vec<u32> = left.chars().map(|c| c as u32).collect();
    let right: Vec<u32> = right.chars().map(|c| c as u32).collect();
    left.cmp(&right)
}

/// 对应 `InMemoryStorageState`。
pub struct InMemoryStorageState {
    entries: HashMap<String, Entry>,
    entries_by_seq: Vec<Entry>,
    scalar_values: HashMap<String, StoredValue<Json>>,
    list_values: HashMap<String, StoredListSnapshot>,
    usage: HashMap<String, UsageRow>,
    stats: SessionStats,
    next_seq: u64,
}

impl Default for InMemoryStorageState {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryStorageState {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            entries_by_seq: Vec::new(),
            scalar_values: HashMap::new(),
            list_values: HashMap::new(),
            usage: HashMap::new(),
            stats: SessionStats {
                message_count: 0,
                usage: empty_usage(),
            },
            next_seq: 1,
        }
    }

    pub fn prepare_commit(&self, writes: &[Write], timestamp: u64) -> PreparedCommit {
        let prepared = prepare_storage_commit(writes, self.next_seq, timestamp);
        self.validate_committed(&prepared.writes)
            .expect("prepared commit failed validation");
        prepared
    }

    pub fn validate_committed(&self, writes: &[CommittedWrite]) -> Result<(), String> {
        validate_committed_writes(writes, self.next_seq, &CommitStateAdapter { state: self })
    }

    /// 对应 `applyValidated`。
    pub fn apply_validated(&mut self, writes: &[CommittedWrite]) -> SessionStats {
        for write in writes {
            match write {
                CommittedWrite::Entry { entry } => {
                    let id = entry.base().id.clone();
                    if matches!(entry, Entry::Message(_)) {
                        self.stats.message_count += 1;
                    }
                    self.entries_by_seq.push(entry.clone());
                    self.entries.insert(id, entry.clone());
                }
                CommittedWrite::Usage { row } => {
                    let id = row.id.clone();
                    self.stats.usage = add_usage(&self.stats.usage, &row.usage);
                    self.usage.insert(id, row.clone());
                }
                CommittedWrite::ValueSet {
                    seq,
                    namespace,
                    key,
                    value: v,
                } => {
                    let pkey = physical_key(namespace, key);
                    self.scalar_values.insert(
                        pkey,
                        StoredValue {
                            address: value(namespace, key),
                            value: v.clone(),
                            seq: *seq,
                        },
                    );
                }
                CommittedWrite::ValueDelete { namespace, key, .. } => {
                    self.scalar_values.remove(&physical_key(namespace, key));
                }
                CommittedWrite::ListAppend {
                    seq,
                    namespace,
                    key,
                    value: v,
                } => {
                    let pkey = physical_key(namespace, key);
                    let element = ListElement {
                        seq: *seq,
                        value: v.clone(),
                    };
                    match self.list_values.get_mut(&pkey) {
                        Some(stored) => stored.elements.push(element),
                        None => {
                            self.list_values.insert(
                                pkey,
                                StoredListSnapshot {
                                    address: list(namespace, key),
                                    elements: vec![element],
                                },
                            );
                        }
                    }
                }
                CommittedWrite::ListDelete { namespace, key, .. } => {
                    self.list_values.remove(&physical_key(namespace, key));
                }
            }
            self.next_seq = write.seq() + 1;
        }
        self.stats.clone()
    }

    pub fn advance_next_seq(&mut self, next_seq: u64) {
        assert!(
            next_seq >= 1,
            "Invalid storage sequence high-water mark: {next_seq}"
        );
        self.next_seq = self.next_seq.max(next_seq);
    }

    /// 对应 `createFork`：从 live state 直接构造 destination state。
    pub fn create_fork(&self, options: &ForkOptions) -> InMemoryStorageState {
        let plan = self.select_fork_plan(options);

        let mut destination = InMemoryStorageState::new();
        let mut message_count = 0;
        for entry in &self.entries_by_seq {
            if !plan.is_entry_copied(&entry.base().id) {
                continue;
            }
            destination
                .entries
                .insert(entry.base().id.clone(), entry.clone());
            destination.entries_by_seq.push(entry.clone());
            if matches!(entry, Entry::Message(_)) {
                message_count += 1;
            }
        }
        destination.stats.message_count = message_count;

        for stored in self.scalar_values.values() {
            let projected = project_fork_current_state_write(
                &CommittedWrite::ValueSet {
                    seq: stored.seq,
                    namespace: stored.address.namespace.clone(),
                    key: stored.address.key.clone(),
                    value: stored.value.clone(),
                },
                &plan.plan,
                |entry_id| plan.is_entry_copied(entry_id),
            );
            if let Some(projected) = projected {
                destination.apply_value_set_or_list_append(&projected);
            }
        }

        for stored in self.list_values.values() {
            for element in &stored.elements {
                let projected = project_fork_current_state_write(
                    &CommittedWrite::ListAppend {
                        seq: element.seq,
                        namespace: stored.address.namespace.clone(),
                        key: stored.address.key.clone(),
                        value: element.value.clone(),
                    },
                    &plan.plan,
                    |entry_id| plan.is_entry_copied(entry_id),
                );
                if let Some(projected) = projected {
                    destination.apply_value_set_or_list_append(&projected);
                }
            }
        }
        destination.next_seq = self.next_seq;
        destination
    }

    fn select_fork_plan(&self, options: &ForkOptions) -> MemoryForkPlan {
        match options {
            ForkOptions::Tree { .. } => MemoryForkPlan {
                plan: ForkCurrentStatePlan::Tree,
                entry_ids: HashSet::new(),
            },
            ForkOptions::Branch {
                branch,
                entry_id,
                position,
                ..
            } => {
                let tip = self.get_value(&branch_tip(branch).erased()).map(|stored| {
                    serde_json::from_value::<Option<String>>(stored.value.clone()).unwrap_or(None)
                });
                let mut entry_ids = HashSet::new();
                let plan = select_branch_fork(
                    branch,
                    tip,
                    entry_id.clone(),
                    *position,
                    |entry_id| {
                        self.entries
                            .get(entry_id)
                            .map(|e| e.base().parent_id.clone())
                    },
                    |entry_id| {
                        entry_ids.insert(entry_id.to_string());
                    },
                );
                if self.get_value(&lane_config(branch).erased()).is_none()
                    || self.get_value(&lane_state(branch).erased()).is_none()
                {
                    panic!("Source branch {branch:?} is not a configured AgentLane");
                }
                MemoryForkPlan { plan, entry_ids }
            }
        }
    }

    fn apply_value_set_or_list_append(&mut self, write: &CommittedWrite) {
        match write {
            CommittedWrite::ValueSet {
                seq,
                namespace,
                key,
                value: v,
            } => {
                let pkey = physical_key(namespace, key);
                self.scalar_values.insert(
                    pkey,
                    StoredValue {
                        address: value(namespace, key),
                        value: v.clone(),
                        seq: *seq,
                    },
                );
            }
            CommittedWrite::ListAppend {
                seq,
                namespace,
                key,
                value: v,
            } => {
                let pkey = physical_key(namespace, key);
                let element = ListElement {
                    seq: *seq,
                    value: v.clone(),
                };
                match self.list_values.get_mut(&pkey) {
                    Some(stored) => stored.elements.push(element),
                    None => {
                        self.list_values.insert(
                            pkey,
                            StoredListSnapshot {
                                address: list(namespace, key),
                                elements: vec![element],
                            },
                        );
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn get_entries(&self, ids: &[String]) -> HashMap<String, Entry> {
        let mut found = HashMap::new();
        for id in ids {
            if let Some(entry) = self.entries.get(id) {
                found.insert(id.clone(), entry.clone());
            }
        }
        found
    }

    pub fn get_value(&self, address: &Value<Json>) -> Option<StoredValue<Json>> {
        self.scalar_values
            .get(&physical_key(&address.namespace, &address.key))
            .cloned()
    }

    pub fn scan_values(&self, prefix: &Value<Json>) -> Vec<StoredValue<Json>> {
        let mut values: Vec<StoredValue<Json>> = self
            .scalar_values
            .values()
            .filter(|stored| {
                stored.address.namespace == prefix.namespace
                    && stored.address.key.starts_with(&prefix.key)
            })
            .cloned()
            .collect();
        values.sort_by(|a, b| compare_keys(&a.address.key, &b.address.key));
        values
    }

    pub fn read_list(
        &self,
        address: &ValueList<Json>,
        options: Option<ListReadOptions>,
    ) -> Vec<ListElement<Json>> {
        let resolved = resolve_list_read_options(options.unwrap_or_default());
        let elements = self
            .list_values
            .get(&physical_key(&address.namespace, &address.key))
            .map(|s| s.elements.clone())
            .unwrap_or_default();
        let filtered: Vec<ListElement<Json>> = elements
            .into_iter()
            .filter(|element| match resolved.cursor {
                None => true,
                Some(cursor) => match resolved.order {
                    ListOrder::Asc => element.seq > cursor.seq,
                    ListOrder::Desc => element.seq < cursor.seq,
                },
            })
            .collect();
        let ordered: Vec<ListElement<Json>> = match resolved.order {
            ListOrder::Asc => filtered,
            ListOrder::Desc => filtered.into_iter().rev().collect(),
        };
        ordered.into_iter().take(resolved.limit).collect()
    }

    pub fn scan_branch(&self, query: &StorageBranchScan) -> Result<Vec<Entry>, String> {
        let mut start = self
            .entries
            .get(&query.start)
            .ok_or_else(|| format!("Unknown branch start: {}", query.start))?;
        let mut path: Vec<Entry> = Vec::new();
        loop {
            let parent = start.base().parent_id.clone();
            path.push(start.clone());
            match parent {
                None => break,
                Some(parent_id) => {
                    start = self
                        .entries
                        .get(&parent_id)
                        .ok_or_else(|| "Corrupt branch: missing parent".to_string())?;
                }
            }
        }
        let scan = &query.scan;
        if scan.order == Some(ScanOrder::OldestFirst) {
            path.reverse();
        }
        let mut stopped: Vec<Entry> = Vec::new();
        for candidate in path {
            let stop = candidate.base().id == scan.stop_at_id.as_deref().unwrap_or_default()
                || Some(candidate.base().entry_type) == scan.stop_at_type;
            stopped.push(candidate);
            if stop {
                break;
            }
        }
        let filtered = stopped.into_iter().filter(|candidate| {
            (scan.entry_type.is_none() || Some(candidate.base().entry_type) == scan.entry_type)
                && (scan.custom_type.is_none()
                    || candidate.base().custom_type.as_deref() == scan.custom_type.as_deref())
                && (scan.cursor.is_none()
                    || match scan.order {
                        Some(ScanOrder::OldestFirst) => {
                            candidate.base().seq > scan.cursor.unwrap().seq
                        }
                        _ => candidate.base().seq < scan.cursor.unwrap().seq,
                    })
        });
        Ok(match scan.limit {
            None => filtered.collect(),
            Some(limit) => filtered.take(limit).collect(),
        })
    }

    pub fn scan_branch_structure(
        &self,
        query: &StorageBranchScan,
    ) -> Result<Vec<EntryStructure>, String> {
        Ok(self
            .scan_branch(query)?
            .into_iter()
            .map(|entry| {
                let base = entry.base();
                EntryStructure {
                    id: base.id.clone(),
                    parent_id: base.parent_id.clone(),
                    seq: base.seq,
                    timestamp: base.timestamp,
                    entry_type: base.entry_type,
                    custom_type: base.custom_type.clone(),
                }
            })
            .collect())
    }

    pub fn scan_entries(&self, query: &EntryScan) -> Vec<Entry> {
        let limit = query.limit.unwrap_or(usize::MAX);
        let descending = query.order == Some(ScanOrder::Desc);
        let mut result = Vec::new();
        let mut index = if descending {
            self.entries_by_seq.len()
        } else {
            0
        };
        while if descending {
            index > 0
        } else {
            index < self.entries_by_seq.len()
        } {
            if descending {
                index -= 1;
            }
            let entry = &self.entries_by_seq[index];
            if !descending {
                index += 1;
            }
            if result.len() >= limit {
                break;
            }
            let base = entry.base();
            if (query.entry_type.is_none() || Some(base.entry_type) == query.entry_type)
                && (query.custom_type.is_none()
                    || base.custom_type.as_deref() == query.custom_type.as_deref())
                && (query.from_seq.is_none() || base.seq >= query.from_seq.unwrap())
                && (query.to_seq.is_none() || base.seq <= query.to_seq.unwrap())
            {
                result.push(entry.clone());
            }
        }
        result
    }

    pub fn scan_usage(&self, query: &UsageScan) -> Vec<UsageRow> {
        let mut rows: Vec<UsageRow> = self
            .usage
            .values()
            .filter(|row| query.from_seq.is_none() || row.seq >= query.from_seq.unwrap())
            .filter(|row| query.to_seq.is_none() || row.seq <= query.to_seq.unwrap())
            .cloned()
            .collect();
        rows.sort_by(|a, b| match query.order {
            Some(ScanOrder::Desc) => b.seq.cmp(&a.seq),
            _ => a.seq.cmp(&b.seq),
        });
        match query.limit {
            None => rows,
            Some(limit) => rows.into_iter().take(limit).collect(),
        }
    }

    pub fn get_stats(&self) -> SessionStats {
        self.stats.clone()
    }

    pub fn get_next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn snapshot_entries_and_values(&self) -> (Vec<Entry>, Vec<StoredValue<Json>>) {
        let mut entries: Vec<Entry> = self.entries.values().cloned().collect();
        entries.sort_by_key(|e| e.base().seq);
        (entries, self.scalar_values.values().cloned().collect())
    }
}

struct CommitStateAdapter<'a> {
    state: &'a InMemoryStorageState,
}

impl crate::harness::session::commit::CommitValidationState for CommitStateAdapter<'_> {
    fn has_entry_or_usage_id(&self, id: &str) -> bool {
        self.state.entries.contains_key(id) || self.state.usage.contains_key(id)
    }

    fn has_entry_id(&self, id: &str) -> bool {
        self.state.entries.contains_key(id)
    }
}
