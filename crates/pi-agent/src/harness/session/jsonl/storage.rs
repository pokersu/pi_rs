//! Rust 翻译自 packages/agent/src/harness/session/jsonl/storage.ts（核心）
//!
//! JSONL 文件存储：`SessionState` + 逐行追加 mutation 到 `.jsonl` 文件。

use std::sync::{Arc, Mutex};

use crate::harness::session::jsonl::codec::{
    decode_entry, decode_record, encode_entry, encode_record,
};
use crate::harness::session::jsonl::types::JsonlV4Header;
use crate::harness::session::state::{SessionMutation, SessionState};
use crate::harness::session::types::{
    Entry, EntryQuery, ForkOptions, LanePointer, LaneRecord, LogItem, OperationStartedRecord,
    RecordQuery, SessionError, SessionErrorCode, SessionMetadata, SessionStats, SessionStorage,
};

/// 对应 `JsonlSessionStorage`
pub struct JsonlSessionStorage {
    fs: Arc<dyn crate::harness::types::FileSystem>,
    path: String,
    metadata: SessionMetadata,
    state: Mutex<SessionState>,
}

impl JsonlSessionStorage {
    /// 对应 `load`
    pub async fn load(
        fs: Arc<dyn crate::harness::types::FileSystem>,
        path: &str,
    ) -> Result<Self, SessionError> {
        let content = fs.read_text_file(path, None).await.map_err(|e| {
            SessionError::new(
                crate::harness::session::types::SessionErrorCode::Storage,
                e.message,
            )
        })?;
        let physical_lines: Vec<&str> = content.split('\n').collect();
        // 第一行是 header。
        let header: crate::harness::session::jsonl::types::JsonlV4Header =
            if let Some(first) = physical_lines.first() {
                serde_json::from_str(first).map_err(|e| {
                    SessionError::new(
                        crate::harness::session::types::SessionErrorCode::InvalidEntry,
                        e.to_string(),
                    )
                })?
            } else {
                return Err(SessionError::new(
                    crate::harness::session::types::SessionErrorCode::InvalidEntry,
                    "missing header",
                ));
            };
        let metadata = SessionMetadata {
            id: header.id.clone(),
            created_at: header.created_at,
            parent_session_id: header.parent_session_id.clone(),
        };
        let storage = Self {
            fs,
            path: path.to_string(),
            metadata,
            state: Mutex::new(SessionState::new()),
        };

        for line in physical_lines.iter().skip(1) {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = decode_entry(line) {
                let mut state = storage.state.lock().unwrap();
                let seq = state.next_sequence();
                state.apply_mutation(SessionMutation::Entry {
                    lane: Some("main".to_string()),
                    entry,
                });
                let _ = seq;
            } else if let Ok(record) = decode_record(line) {
                let mut state = storage.state.lock().unwrap();
                state.apply_mutation(SessionMutation::Record { record });
            }
        }
        Ok(storage)
    }

    /// 对应 `fork`：复制 source 的 fork mutations 到新文件。
    pub async fn fork(
        &self,
        path: &str,
        header: &JsonlV4Header,
        options: &ForkOptions,
    ) -> Result<Self, SessionError> {
        let header_line = serde_json::to_string(header).map_err(|e| {
            SessionError::new(
                crate::harness::session::types::SessionErrorCode::InvalidEntry,
                e.to_string(),
            )
        })?;
        self.fs
            .write_file(path, format!("{header_line}\n").as_bytes(), None)
            .await
            .map_err(|e| SessionError::new(SessionErrorCode::Storage, e.message))?;

        let storage = Self {
            fs: self.fs.clone(),
            path: path.to_string(),
            metadata: SessionMetadata {
                id: header.id.clone(),
                created_at: header.created_at,
                parent_session_id: header.parent_session_id.clone(),
            },
            state: Mutex::new(SessionState::new()),
        };
        let mutations = self.state.lock().unwrap().create_fork_mutations(options);
        for mutation in mutations {
            storage.append_mutation(&mutation).await?;
            storage.state.lock().unwrap().apply_mutation(mutation);
        }
        Ok(storage)
    }

    async fn append_mutation(&self, mutation: &SessionMutation) -> Result<(), SessionError> {
        let line = match mutation {
            SessionMutation::Entry { entry, .. } => encode_entry(entry),
            SessionMutation::Record { record } => encode_record(record),
            _ => return Ok(()),
        };
        let content = format!("{line}\n");
        self.fs
            .append_file(&self.path, content.as_bytes(), None)
            .await
            .map_err(|e| {
                SessionError::new(
                    crate::harness::session::types::SessionErrorCode::Storage,
                    e.message,
                )
            })?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionStorage for JsonlSessionStorage {
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
        let mutation = SessionMutation::Lane {
            seq,
            lane: lane.to_string(),
            leaf_id: at.map(|s| s.to_string()),
        };
        state.apply_mutation(mutation);
        Ok(())
    }

    async fn move_lane(&self, lane: &str, to: Option<&str>) -> Result<(), SessionError> {
        let mut state = self.state.lock().unwrap();
        state.require_lane(lane);
        state.validate_target(to);
        let seq = state.next_sequence();
        let mutation = SessionMutation::Lane {
            seq,
            lane: lane.to_string(),
            leaf_id: to.map(|s| s.to_string()),
        };
        state.apply_mutation(mutation);
        Ok(())
    }

    async fn append_entry(&self, entry: Entry, lane: &str) -> Result<Entry, SessionError> {
        let (parent, seq) = {
            let state = self.state.lock().unwrap();
            let parent = state.require_lane(lane);
            (parent, state.next_sequence())
        };
        let entry = fill_entry_base(entry, parent, seq);
        let mutation = SessionMutation::Entry {
            lane: Some(lane.to_string()),
            entry: entry.clone(),
        };
        self.append_mutation(&mutation).await?;
        self.state.lock().unwrap().apply_mutation(mutation);
        Ok(entry)
    }

    async fn append_record(&self, record: LaneRecord) -> Result<LaneRecord, SessionError> {
        let seq = {
            let state = self.state.lock().unwrap();
            state.next_sequence()
        };
        let record = fill_record_base(record, seq);
        let mutation = SessionMutation::Record {
            record: record.clone(),
        };
        self.append_mutation(&mutation).await?;
        self.state.lock().unwrap().apply_mutation(mutation);
        Ok(record)
    }

    async fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError> {
        Ok(self.state.lock().unwrap().get_entry(id))
    }

    async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        Ok(self.state.lock().unwrap().find_entries(query))
    }

    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        start: &str,
        stop_at_type: Option<&str>,
        stop_at_id: Option<&str>,
    ) -> Result<Vec<Entry>, SessionError> {
        self.state
            .lock()
            .unwrap()
            .find_entries_on_branch(query, start, stop_at_type, stop_at_id)
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
        state.apply_mutation(SessionMutation::FactName {
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
        state.apply_mutation(SessionMutation::FactLabel {
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
