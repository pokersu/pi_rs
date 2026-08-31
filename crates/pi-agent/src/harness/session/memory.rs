//! Rust 翻译自 packages/agent/src/harness/session/memory.ts

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::harness::session::session::Session;
use crate::harness::session::state::SessionState;
use crate::harness::session::types::{
    Entry, EntryQuery, LanePointer, LaneRecord, LogItem, OperationStartedRecord, RecordQuery,
    SessionError, SessionErrorCode, SessionMetadata, SessionStats, SessionStorage,
};

/// 对应 `InMemorySessionStorage`
pub struct InMemorySessionStorage {
    metadata: SessionMetadata,
    state: Mutex<SessionState>,
}

impl InMemorySessionStorage {
    pub fn new(metadata: SessionMetadata) -> Self {
        Self {
            metadata,
            state: Mutex::new(SessionState::new()),
        }
    }
}

#[async_trait::async_trait]
impl SessionStorage for InMemorySessionStorage {
    async fn get_metadata(&self) -> Result<SessionMetadata, SessionError> {
        Ok(self.metadata.clone())
    }

    async fn get_lanes(&self) -> Result<Vec<LanePointer>, SessionError> {
        Ok(self.state.lock().unwrap().get_lanes())
    }

    async fn create_lane(&self, lane: &str, at: Option<&str>) -> Result<(), SessionError> {
        let mut state = self.state.lock().unwrap();
        state.validate_new_lane(lane);
        state.validate_target(at);
        let seq = state.next_sequence();
        state.apply_mutation(crate::harness::session::state::SessionMutation::Lane {
            seq,
            lane: lane.to_string(),
            leaf_id: at.map(|s| s.to_string()),
        });
        Ok(())
    }

    async fn move_lane(&self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        let mut state = self.state.lock().unwrap();
        state.require_lane(lane);
        state.validate_target(to);
        let seq = state.next_sequence();
        state.apply_mutation(crate::harness::session::state::SessionMutation::Lane {
            seq,
            lane: lane.to_string(),
            leaf_id: to.map(|s| s.to_string()),
        });
        Ok(())
    }

    async fn append_entry(&self, entry: Entry, lane: &str) -> Result<Entry, SessionError> {
        let mut state = self.state.lock().unwrap();
        let parent = state.require_lane(lane);
        state.validate_unused_id(entry.id());
        // 填充 parentId/seq/timestamp（对应 TS 的 storage-assigned 字段）。
        let entry = fill_entry_base(entry, parent, state.next_sequence());
        state.apply_mutation(crate::harness::session::state::SessionMutation::Entry {
            lane: Some(lane.to_string()),
            entry: entry.clone(),
        });
        Ok(entry)
    }

    async fn append_record(&self, record: LaneRecord) -> Result<LaneRecord, SessionError> {
        let mut state = self.state.lock().unwrap();
        state.require_lane(record_lane_of(&record));
        state.validate_unused_id(record_id_of(&record));
        let record = fill_record_base(record, state.next_sequence());
        state.apply_mutation(crate::harness::session::state::SessionMutation::Record {
            record: record.clone(),
        });
        Ok(record)
    }

    async fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError> {
        Ok(self.state.lock().unwrap().get_entry(id))
    }

    async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        Ok(self.state.lock().unwrap().find_entries(query))
    }

    async fn find_records(&self, query: &RecordQuery) -> Result<Vec<LaneRecord>, SessionError> {
        Ok(self.state.lock().unwrap().find_records(query))
    }

    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> Result<Vec<OperationStartedRecord>, SessionError> {
        Ok(self.state.lock().unwrap().find_open_operations(lane, limit))
    }

    async fn get_log(
        &self,
        after_seq: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Vec<LogItem>, SessionError> {
        Ok(self.state.lock().unwrap().get_log(after_seq, limit))
    }

    async fn get_name(&self) -> Result<Option<String>, SessionError> {
        Ok(self.state.lock().unwrap().get_name())
    }

    async fn set_name(&self, name: Option<&str>) -> Result<(), SessionError> {
        let mut state = self.state.lock().unwrap();
        let seq = state.next_sequence();
        state.apply_mutation(crate::harness::session::state::SessionMutation::FactName {
            seq,
            name: name.map(|s| s.to_string()),
        });
        Ok(())
    }

    async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        Ok(self.state.lock().unwrap().get_label(id))
    }

    async fn set_label(&self, id: &str, label: Option<&str>) -> Result<(), SessionError> {
        let mut state = self.state.lock().unwrap();
        state.validate_target(Some(id));
        let seq = state.next_sequence();
        state.apply_mutation(crate::harness::session::state::SessionMutation::FactLabel {
            seq,
            target_id: id.to_string(),
            label: label.map(|s| s.to_string()),
        });
        Ok(())
    }

    async fn get_stats(&self) -> Result<SessionStats, SessionError> {
        Ok(self.state.lock().unwrap().get_stats())
    }
}

fn fill_entry_base(entry: Entry, parent_id: Option<String>, seq: u64) -> Entry {
    let ts = pi_ai::utils::uuid::now_ms() as u64;
    match entry {
        Entry::Message(mut e) => {
            e.base.parent_id = parent_id;
            e.base.seq = seq;
            e.base.timestamp = ts;
            Entry::Message(e)
        }
        Entry::ModelChange(mut e) => {
            e.base.parent_id = parent_id;
            e.base.seq = seq;
            e.base.timestamp = ts;
            Entry::ModelChange(e)
        }
        Entry::ThinkingLevelChange(mut e) => {
            e.base.parent_id = parent_id;
            e.base.seq = seq;
            e.base.timestamp = ts;
            Entry::ThinkingLevelChange(e)
        }
        Entry::ActiveToolsChange(mut e) => {
            e.base.parent_id = parent_id;
            e.base.seq = seq;
            e.base.timestamp = ts;
            Entry::ActiveToolsChange(e)
        }
        Entry::Compaction(mut e) => {
            e.base.parent_id = parent_id;
            e.base.seq = seq;
            e.base.timestamp = ts;
            Entry::Compaction(e)
        }
        Entry::BranchSummary(mut e) => {
            e.base.parent_id = parent_id;
            e.base.seq = seq;
            e.base.timestamp = ts;
            Entry::BranchSummary(e)
        }
        Entry::Custom(mut e) => {
            e.base.parent_id = parent_id;
            e.base.seq = seq;
            e.base.timestamp = ts;
            Entry::Custom(e)
        }
    }
}

fn fill_record_base(record: LaneRecord, seq: u64) -> LaneRecord {
    let ts = pi_ai::utils::uuid::now_ms() as u64;
    macro_rules! set {
        ($r:expr) => {{
            let mut r = $r;
            r.base.seq = seq;
            r.base.timestamp = ts;
            r
        }};
    }
    match record {
        LaneRecord::OperationStarted(r) => LaneRecord::OperationStarted(set!(r)),
        LaneRecord::AbortRequested(r) => LaneRecord::AbortRequested(set!(r)),
        LaneRecord::OperationFinished(r) => LaneRecord::OperationFinished(set!(r)),
        LaneRecord::StepAttempt(r) => LaneRecord::StepAttempt(set!(r)),
        LaneRecord::ToolStarted(r) => LaneRecord::ToolStarted(set!(r)),
        LaneRecord::QueueEnqueued(r) => LaneRecord::QueueEnqueued(set!(r)),
        LaneRecord::QueueCancelled(r) => LaneRecord::QueueCancelled(set!(r)),
        LaneRecord::WriteDeferred(r) => LaneRecord::WriteDeferred(set!(r)),
        LaneRecord::Usage(r) => LaneRecord::Usage(set!(r)),
    }
}

fn record_lane_of(record: &LaneRecord) -> &str {
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

fn record_id_of(record: &LaneRecord) -> &str {
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

/// 对应 `InMemorySessionRepo`
#[derive(Default)]
pub struct InMemorySessionRepo {
    sessions: Mutex<HashMap<String, Arc<InMemorySessionStorage>>>,
}

impl InMemorySessionRepo {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn create(&self, id: Option<&str>) -> Result<Session, SessionError> {
        let id = id.map(|s| s.to_string()).unwrap_or_else(pi_ai::uuidv7);
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.contains_key(&id) {
            return Err(SessionError::new(
                SessionErrorCode::AlreadyExists,
                format!("Session already exists: {id}"),
            ));
        }
        let storage = Arc::new(InMemorySessionStorage::new(SessionMetadata {
            id: id.clone(),
            created_at: pi_ai::utils::uuid::now_ms() as u64,
            parent_session_id: None,
        }));
        sessions.insert(id, storage.clone());
        Ok(Session::new(storage))
    }

    pub async fn open(&self, metadata: SessionMetadata) -> Result<Session, SessionError> {
        let sessions = self.sessions.lock().unwrap();
        let storage = sessions.get(&metadata.id).cloned().ok_or_else(|| {
            SessionError::new(
                SessionErrorCode::NotFound,
                format!("Session not found: {}", metadata.id),
            )
        })?;
        Ok(Session::new(storage))
    }

    pub async fn list(&self) -> Result<Vec<SessionMetadata>, SessionError> {
        let storages: Vec<Arc<InMemorySessionStorage>> =
            self.sessions.lock().unwrap().values().cloned().collect();
        let mut result = Vec::new();
        for storage in storages {
            result.push(storage.get_metadata().await?);
        }
        Ok(result)
    }

    pub async fn delete(&self, metadata: SessionMetadata) -> Result<(), SessionError> {
        self.sessions.lock().unwrap().remove(&metadata.id);
        Ok(())
    }
}
