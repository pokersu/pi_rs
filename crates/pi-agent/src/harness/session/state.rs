//! Rust 翻译自 packages/agent/src/harness/session/state.ts
//!
//! session 核心状态管理：entries/records/lanes/log/stats 与 mutation 应用。

use std::collections::{HashMap, HashSet};

use crate::harness::session::types::{
    Entry, EntryOrder, EntryQuery, ForkOptions, ForkPosition, LanePointer, LaneRecord, LogItem,
    OperationStartedRecord, RecordQuery, SessionError, SessionStats,
};

/// 对应 `SessionMutation`
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum SessionMutation {
    Entry {
        lane: Option<String>,
        entry: Entry,
    },
    Record {
        record: LaneRecord,
    },
    Lane {
        seq: u64,
        lane: String,
        leaf_id: Option<String>,
    },
    FactName {
        seq: u64,
        name: Option<String>,
    },
    FactLabel {
        seq: u64,
        target_id: String,
        label: Option<String>,
    },
}

impl SessionMutation {
    fn seq(&self) -> u64 {
        match self {
            SessionMutation::Entry { entry, .. } => entry_seq(entry),
            SessionMutation::Record { record } => record_seq(record),
            SessionMutation::Lane { seq, .. }
            | SessionMutation::FactName { seq, .. }
            | SessionMutation::FactLabel { seq, .. } => *seq,
        }
    }
}

fn entry_seq(entry: &Entry) -> u64 {
    match entry {
        Entry::Message(e) => e.base.seq,
        Entry::ModelChange(e) => e.base.seq,
        Entry::ThinkingLevelChange(e) => e.base.seq,
        Entry::ActiveToolsChange(e) => e.base.seq,
        Entry::Compaction(e) => e.base.seq,
        Entry::BranchSummary(e) => e.base.seq,
        Entry::Custom(e) => e.base.seq,
    }
}

fn record_seq(record: &LaneRecord) -> u64 {
    match record {
        LaneRecord::OperationStarted(r) => r.base.seq,
        LaneRecord::AbortRequested(r) => r.base.seq,
        LaneRecord::OperationFinished(r) => r.base.seq,
        LaneRecord::StepAttempt(r) => r.base.seq,
        LaneRecord::ToolStarted(r) => r.base.seq,
        LaneRecord::QueueEnqueued(r) => r.base.seq,
        LaneRecord::QueueCancelled(r) => r.base.seq,
        LaneRecord::WriteDeferred(r) => r.base.seq,
        LaneRecord::Usage(r) => r.base.seq,
    }
}

/// 对应 `SessionState`
#[derive(Debug, Default)]
pub struct SessionState {
    sequence: u64,
    used_ids: HashSet<String>,
    entries: Vec<Entry>,
    entries_by_id: HashMap<String, Entry>,
    records: Vec<LaneRecord>,
    open_operations_by_lane: HashMap<String, HashMap<String, OperationStartedRecord>>,
    lanes: HashMap<String, Option<String>>,
    log: Vec<LogItem>,
    stats: SessionStats,
    name: Option<String>,
    labels: HashMap<String, String>,
}

impl SessionState {
    pub fn new() -> Self {
        let mut lanes = HashMap::new();
        lanes.insert("main".to_string(), None);
        Self {
            lanes,
            ..Default::default()
        }
    }

    /// 对应 `nextSequence`
    pub fn next_sequence(&self) -> u64 {
        self.sequence + 1
    }

    /// 对应 `getLanes`
    pub fn get_lanes(&self) -> Vec<LanePointer> {
        self.lanes
            .iter()
            .map(|(lane, leaf)| LanePointer {
                lane: lane.clone(),
                leaf_id: leaf.clone(),
            })
            .collect()
    }

    /// 对应 `requireLane`
    pub fn require_lane(&self, lane: &str) -> Option<String> {
        self.lanes
            .get(lane)
            .cloned()
            .ok_or_else(|| {
                SessionError::new(
                    crate::harness::session::types::SessionErrorCode::InvalidLane,
                    format!("Lane not found: {lane}"),
                )
            })
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// 对应 `validateNewLane`
    pub fn validate_new_lane(&self, lane: &str) {
        if self.lanes.contains_key(lane) {
            panic!(
                "{}",
                SessionError::new(
                    crate::harness::session::types::SessionErrorCode::AlreadyExists,
                    format!("Lane already exists: {lane}")
                )
            );
        }
    }

    /// 对应 `validateTarget`
    pub fn validate_target(&self, target_id: Option<&str>) {
        if let Some(id) = target_id
            && !self.entries_by_id.contains_key(id)
        {
            panic!(
                "{}",
                SessionError::new(
                    crate::harness::session::types::SessionErrorCode::NotFound,
                    format!("Entry not found: {id}")
                )
            );
        }
    }

    /// 对应 `validateUnusedId`
    pub fn validate_unused_id(&self, id: &str) {
        if self.used_ids.contains(id) {
            panic!(
                "{}",
                SessionError::new(
                    crate::harness::session::types::SessionErrorCode::AlreadyExists,
                    format!("Session id already exists: {id}")
                )
            );
        }
    }

    /// 对应 `applyMutation`
    pub fn apply_mutation(&mut self, mutation: SessionMutation) {
        let seq = mutation.seq();
        if seq != self.sequence + 1 {
            panic!(
                "{}",
                SessionError::new(
                    crate::harness::session::types::SessionErrorCode::InvalidEntry,
                    format!("Invalid session mutation: has non-consecutive seq {seq}")
                )
            );
        }

        match mutation {
            SessionMutation::Entry { lane, entry } => {
                let id = entry.id().to_string();
                if self.used_ids.contains(&id) {
                    panic!("duplicate id {id}");
                }
                if let Some(lane) = &lane {
                    let leaf = self
                        .lanes
                        .get(lane)
                        .cloned()
                        .unwrap_or_else(|| panic!("missing lane {lane}"));
                    if entry_parent_id(&entry) != leaf {
                        panic!("does not chain to the lane leaf");
                    }
                }
                self.used_ids.insert(id.clone());
                self.entries_by_id.insert(id.clone(), entry.clone());
                self.entries.push(entry.clone());
                self.sequence += 1;
                if let Entry::Message(_) = &entry {
                    self.stats.message_count += 1;
                }
                self.log.push(LogItem::Entry {
                    seq: self.sequence,
                    entry,
                });
                if let Some(lane) = &lane {
                    self.lanes.insert(
                        lane.clone(),
                        Some(entry_id_of(self.entries.last().unwrap())),
                    );
                }
            }
            SessionMutation::Record { record } => {
                let id = record_id(&record).to_string();
                if self.used_ids.contains(&id) {
                    panic!("duplicate id {id}");
                }
                self.used_ids.insert(id.clone());
                if let LaneRecord::OperationStarted(started) = &record {
                    self.open_operations_by_lane
                        .entry(started.base.lane.clone())
                        .or_default()
                        .insert(started.base.id.clone(), started.clone());
                }
                self.records.push(record.clone());
                self.sequence += 1;
                self.log.push(LogItem::Record {
                    seq: self.sequence,
                    record,
                });
            }
            SessionMutation::Lane { seq, lane, leaf_id } => {
                self.lanes.insert(lane.clone(), leaf_id.clone());
                self.sequence = seq;
                self.log.push(LogItem::Lane { seq, lane, leaf_id });
            }
            SessionMutation::FactName { seq, name } => {
                self.name = name.clone();
                self.sequence = seq;
                self.log.push(LogItem::FactName { seq, name });
            }
            SessionMutation::FactLabel {
                seq,
                target_id,
                label,
            } => {
                if let Some(l) = label.clone() {
                    self.labels.insert(target_id.clone(), l);
                } else {
                    self.labels.remove(&target_id);
                }
                self.sequence = seq;
                self.log.push(LogItem::FactLabel {
                    seq,
                    target_id,
                    label,
                });
            }
        }
    }

    /// 对应 `getEntry`
    pub fn get_entry(&self, id: &str) -> Option<Entry> {
        self.entries_by_id.get(id).cloned()
    }

    /// 对应 `findEntries`
    pub fn find_entries(&self, query: &EntryQuery) -> Vec<Entry> {
        let mut result: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| self.matches_entry_query(e, query))
            .cloned()
            .collect();
        match query.order {
            Some(EntryOrder::OldestFirst) => {}
            _ => result.reverse(),
        }
        if let Some(limit) = query.limit {
            result.truncate(limit);
        }
        result
    }

    /// 对应 `findEntriesOnBranch`：从 `start` 沿 parent 链向根扫描。
    pub fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        start: &str,
        stop_at_type: Option<&str>,
        stop_at_id: Option<&str>,
    ) -> Result<Vec<Entry>, SessionError> {
        let mut results: Vec<Entry> = Vec::new();
        match query.order {
            Some(EntryOrder::OldestFirst) => {
                let path = self.walk_to_root(start, None, None)?;
                for entry in path.iter().rev() {
                    let reached_bound = stop_at_id.map(|sid| entry.id() == sid).unwrap_or(false)
                        || stop_at_type
                            .map(|st| entry_type(entry) == st)
                            .unwrap_or(false);
                    if self.matches_entry_query(entry, query) {
                        results.push(entry.clone());
                    }
                    if reached_bound || query.limit.map(|l| results.len() >= l).unwrap_or(false) {
                        break;
                    }
                }
            }
            _ => {
                let path = self.walk_to_root(start, stop_at_type, stop_at_id)?;
                for entry in &path {
                    if self.matches_entry_query(entry, query) {
                        results.push(entry.clone());
                    }
                    if query.limit.map(|l| results.len() >= l).unwrap_or(false) {
                        break;
                    }
                }
            }
        }
        Ok(results)
    }

    /// 对应 `walkToRoot`：从 `start` 沿 parent 链到根（含 `start`，含 stop 边界）。
    fn walk_to_root(
        &self,
        start: &str,
        stop_at_type: Option<&str>,
        stop_at_id: Option<&str>,
    ) -> Result<Vec<Entry>, SessionError> {
        let mut result = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut current = self.entries_by_id.get(start).cloned().ok_or_else(|| {
            SessionError::new(
                crate::harness::session::types::SessionErrorCode::NotFound,
                format!("Entry not found: {start}"),
            )
        })?;
        loop {
            if visited.contains(current.id()) {
                return Err(SessionError::new(
                    crate::harness::session::types::SessionErrorCode::InvalidEntry,
                    format!("Session branch contains a cycle at {}", current.id()),
                ));
            }
            visited.insert(current.id().to_string());
            let stop = stop_at_id.map(|sid| current.id() == sid).unwrap_or(false)
                || stop_at_type
                    .map(|st| entry_type(&current) == st)
                    .unwrap_or(false);
            let parent_id = entry_parent_id(&current);
            result.push(current);
            if stop || parent_id.is_none() {
                break;
            }
            let parent_id = parent_id.unwrap();
            current = self.entries_by_id.get(&parent_id).cloned().ok_or_else(|| {
                SessionError::new(
                    crate::harness::session::types::SessionErrorCode::InvalidEntry,
                    format!("Entry not found: {parent_id}"),
                )
            })?;
        }
        Ok(result)
    }

    /// 对应 `findRecords`
    pub fn find_records(&self, query: &RecordQuery) -> Vec<LaneRecord> {
        let mut result: Vec<LaneRecord> = self
            .records
            .iter()
            .filter(|r| self.matches_record_query(r, query))
            .cloned()
            .collect();
        match query.order {
            Some(EntryOrder::OldestFirst) => {}
            _ => result.reverse(),
        }
        if let Some(limit) = query.limit {
            result.truncate(limit);
        }
        result
    }

    /// 对应 `findOpenOperations`
    pub fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Vec<OperationStartedRecord> {
        let map = self.open_operations_by_lane.get(lane);
        let mut result: Vec<OperationStartedRecord> = map
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        result.reverse();
        if let Some(limit) = limit {
            result.truncate(limit);
        }
        result
    }

    /// 对应 `getLog`
    pub fn get_log(&self, after_seq: Option<u64>, limit: Option<usize>) -> Vec<LogItem> {
        let mut result: Vec<LogItem> = self
            .log
            .iter()
            .filter(|item| after_seq.map(|s| log_item_seq(item) > s).unwrap_or(true))
            .cloned()
            .collect();
        if let Some(limit) = limit {
            result.truncate(limit);
        }
        result
    }

    /// 对应 `getName`
    pub fn get_name(&self) -> Option<String> {
        self.name.clone()
    }

    /// 对应 `getLabel`
    pub fn get_label(&self, id: &str) -> Option<String> {
        self.labels.get(id).cloned()
    }

    /// 对应 `getStats`
    pub fn get_stats(&self) -> SessionStats {
        self.stats.clone()
    }

    /// 对应 `createForkMutations`：复制分支/tree 的 entries、lanes 与 facts。
    pub fn create_fork_mutations(&self, options: &ForkOptions) -> Vec<SessionMutation> {
        let (copied_entries, fork_lanes) = match options {
            ForkOptions::Tree => (
                self.find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                }),
                self.get_lanes(),
            ),
            ForkOptions::Branch { entry_id, position } => {
                let selected_entry_id = entry_id
                    .clone()
                    .or_else(|| self.lanes.get("main").and_then(|leaf| leaf.clone()));
                let mut target_id: Option<String> = None;
                if let Some(selected_entry_id) = selected_entry_id {
                    let entry = self.get_entry(&selected_entry_id);
                    if entry
                        .as_ref()
                        .map(|e| entry_type(e) != "message")
                        .unwrap_or(true)
                    {
                        panic!(
                            "{}",
                            SessionError::new(
                                crate::harness::session::types::SessionErrorCode::InvalidForkTarget,
                                format!("Fork target is not a message entry: {selected_entry_id}"),
                            )
                        );
                    }
                    let entry = entry.unwrap();
                    let position = position.unwrap_or(if entry_id.is_none() {
                        ForkPosition::At
                    } else {
                        ForkPosition::Before
                    });
                    target_id = match position {
                        ForkPosition::At => Some(entry.id().to_string()),
                        ForkPosition::Before => entry_parent_id(&entry),
                    };
                }
                let copied = match &target_id {
                    None => Vec::new(),
                    Some(target) => self
                        .find_entries_on_branch(
                            &EntryQuery {
                                order: Some(EntryOrder::OldestFirst),
                                ..Default::default()
                            },
                            target,
                            None,
                            None,
                        )
                        .unwrap_or_default(),
                };
                (
                    copied,
                    vec![LanePointer {
                        lane: "main".to_string(),
                        leaf_id: target_id,
                    }],
                )
            }
        };

        let mut mutations: Vec<SessionMutation> = Vec::new();
        let mut sequence: u64 = 1;
        for source_entry in &copied_entries {
            let mut entry = source_entry.clone();
            set_entry_seq(&mut entry, sequence);
            sequence += 1;
            mutations.push(SessionMutation::Entry { lane: None, entry });
        }
        for pointer in fork_lanes {
            mutations.push(SessionMutation::Lane {
                seq: sequence,
                lane: pointer.lane,
                leaf_id: pointer.leaf_id,
            });
            sequence += 1;
        }
        if let Some(name) = &self.name {
            mutations.push(SessionMutation::FactName {
                seq: sequence,
                name: Some(name.clone()),
            });
            sequence += 1;
        }
        for source_entry in &copied_entries {
            if let Some(label) = self.labels.get(source_entry.id()) {
                mutations.push(SessionMutation::FactLabel {
                    seq: sequence,
                    target_id: source_entry.id().to_string(),
                    label: Some(label.clone()),
                });
                sequence += 1;
            }
        }
        mutations
    }

    fn matches_entry_query(&self, entry: &Entry, query: &EntryQuery) -> bool {
        let type_matches = query
            .kind
            .as_ref()
            .map(|k| entry_type(entry) == k.as_str())
            .unwrap_or(true);
        let custom_type_matches = query
            .custom_type
            .as_ref()
            .map(|ct| match entry {
                Entry::Custom(e) => e.custom_type == *ct,
                _ => false,
            })
            .unwrap_or(true);
        let cursor_matches = query
            .cursor
            .as_ref()
            .map(|cursor| match query.order {
                Some(EntryOrder::OldestFirst) => entry_seq(entry) > cursor.after_seq,
                _ => entry_seq(entry) < cursor.after_seq,
            })
            .unwrap_or(true);
        type_matches && custom_type_matches && cursor_matches
    }

    fn matches_record_query(&self, record: &LaneRecord, query: &RecordQuery) -> bool {
        let lane_matches = query
            .lane
            .as_ref()
            .map(|l| record_lane(record) == l.as_str())
            .unwrap_or(true);
        let type_matches = query
            .kind
            .as_ref()
            .map(|k| record_type(record) == k.as_str())
            .unwrap_or(true);
        lane_matches && type_matches
    }
}

fn entry_type(entry: &Entry) -> &'static str {
    match entry {
        Entry::Message(_) => "message",
        Entry::ModelChange(_) => "model_change",
        Entry::ThinkingLevelChange(_) => "thinking_level_change",
        Entry::ActiveToolsChange(_) => "active_tools_change",
        Entry::Compaction(_) => "compaction",
        Entry::BranchSummary(_) => "branch_summary",
        Entry::Custom(_) => "custom",
    }
}

fn entry_id_of(entry: &Entry) -> String {
    entry.id().to_string()
}

fn entry_parent_id(entry: &Entry) -> Option<String> {
    match entry {
        Entry::Message(e) => e.base.parent_id.clone(),
        Entry::ModelChange(e) => e.base.parent_id.clone(),
        Entry::ThinkingLevelChange(e) => e.base.parent_id.clone(),
        Entry::ActiveToolsChange(e) => e.base.parent_id.clone(),
        Entry::Compaction(e) => e.base.parent_id.clone(),
        Entry::BranchSummary(e) => e.base.parent_id.clone(),
        Entry::Custom(e) => e.base.parent_id.clone(),
    }
}

fn set_entry_seq(entry: &mut Entry, seq: u64) {
    match entry {
        Entry::Message(e) => e.base.seq = seq,
        Entry::ModelChange(e) => e.base.seq = seq,
        Entry::ThinkingLevelChange(e) => e.base.seq = seq,
        Entry::ActiveToolsChange(e) => e.base.seq = seq,
        Entry::Compaction(e) => e.base.seq = seq,
        Entry::BranchSummary(e) => e.base.seq = seq,
        Entry::Custom(e) => e.base.seq = seq,
    }
}

fn record_id(record: &LaneRecord) -> &str {
    match record {
        LaneRecord::OperationStarted(r) => &r.base.id,
        LaneRecord::AbortRequested(r) => &r.base.id,
        LaneRecord::OperationFinished(r) => &r.base.id,
        LaneRecord::StepAttempt(r) => &r.base.id,
        LaneRecord::ToolStarted(r) => &r.base.id,
        LaneRecord::QueueEnqueued(r) => &r.base.id,
        LaneRecord::QueueCancelled(r) => &r.base.id,
        LaneRecord::WriteDeferred(r) => &r.base.id,
        LaneRecord::Usage(r) => &r.base.id,
    }
}

fn record_lane(record: &LaneRecord) -> &str {
    match record {
        LaneRecord::OperationStarted(r) => &r.base.lane,
        LaneRecord::AbortRequested(r) => &r.base.lane,
        LaneRecord::OperationFinished(r) => &r.base.lane,
        LaneRecord::StepAttempt(r) => &r.base.lane,
        LaneRecord::ToolStarted(r) => &r.base.lane,
        LaneRecord::QueueEnqueued(r) => &r.base.lane,
        LaneRecord::QueueCancelled(r) => &r.base.lane,
        LaneRecord::WriteDeferred(r) => &r.base.lane,
        LaneRecord::Usage(r) => &r.base.lane,
    }
}

fn record_type(record: &LaneRecord) -> &'static str {
    match record {
        LaneRecord::OperationStarted(_) => "operation_started",
        LaneRecord::AbortRequested(_) => "abort_requested",
        LaneRecord::OperationFinished(_) => "operation_finished",
        LaneRecord::StepAttempt(_) => "step_attempt",
        LaneRecord::ToolStarted(_) => "tool_started",
        LaneRecord::QueueEnqueued(_) => "queue_enqueued",
        LaneRecord::QueueCancelled(_) => "queue_cancelled",
        LaneRecord::WriteDeferred(_) => "write_deferred",
        LaneRecord::Usage(_) => "usage",
    }
}

fn log_item_seq(item: &LogItem) -> u64 {
    match item {
        LogItem::Entry { seq, .. }
        | LogItem::Record { seq, .. }
        | LogItem::Lane { seq, .. }
        | LogItem::FactName { seq, .. }
        | LogItem::FactLabel { seq, .. } => *seq,
    }
}
