//! Rust 翻译自 packages/agent/src/harness/session/memory.ts
//!
//! 基于 InMemoryStorageState 的内存 Storage 与 SessionRepo。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::Value as Json;
use tokio::sync::Mutex;

use crate::harness::context::Context;
use crate::harness::session::commit::CommittedWrite;
use crate::harness::session::in_memory_storage_state::InMemoryStorageState;
use crate::harness::session::session::StorageBackedSession;
use crate::harness::session::types::{
    CommitResult, Entry, EntryScan, EntryStructure, EntryType, ForkOptions, Session,
    SessionCreateOptions, SessionMetadata, SessionRepo, SessionStats, Storage, StorageBranchScan,
    UsageRow, UsageScan, Write,
};
use crate::harness::session::values::{
    ListElement, ListReadOptions, StoredValue, Value, ValueList,
};

const MEMORY_STORAGE_VERSION: u32 = 1;

struct MemoryStorageInner {
    storage_state: InMemoryStorageState,
    state: StorageState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorageState {
    Open,
    Closed,
}

/// 对应 `MemoryStorage`。
pub struct MemoryStorage {
    inner: Arc<Mutex<MemoryStorageInner>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MemoryStorageInner {
                storage_state: InMemoryStorageState::new(),
                state: StorageState::Open,
            })),
        }
    }

    /// 对应 `fork`：从 live state 直接构造 destination storage。
    pub fn fork(&self, options: &ForkOptions) -> Result<MemoryStorage, String> {
        let inner = self.inner.blocking_lock();
        self.assert_open(&inner)?;
        let destination = MemoryStorage::new();
        let mut dest_inner = destination.inner.blocking_lock();
        dest_inner.storage_state = inner.storage_state.create_fork(options);
        drop(dest_inner);
        Ok(destination)
    }

    fn assert_open(&self, inner: &MemoryStorageInner) -> Result<(), String> {
        if inner.state != StorageState::Open {
            return Err("MemoryStorage is closed".to_string());
        }
        Ok(())
    }
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Storage for MemoryStorage {
    async fn commit(&self, writes: Vec<Write>, _context: &Context) -> Result<CommitResult, String> {
        let mut inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        let timestamp = pi_ai::utils::uuid::now_ms() as u64;
        let prepared = inner.storage_state.prepare_commit(&writes, timestamp);
        let stats = inner.storage_state.apply_validated(&prepared.writes);
        Ok(CommitResult {
            first_seq: prepared.first_seq,
            seqs: prepared.seqs,
            timestamp: prepared.timestamp,
            stats,
        })
    }

    async fn get_entries(
        &self,
        ids: &[String],
        _context: &Context,
    ) -> Result<BTreeMap<String, Entry>, String> {
        let inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        Ok(inner.storage_state.get_entries(ids).into_iter().collect())
    }

    async fn get_value(
        &self,
        address: &Value<Json>,
        _context: &Context,
    ) -> Result<Option<StoredValue<Json>>, String> {
        let inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        Ok(inner.storage_state.get_value(address))
    }

    async fn scan_values(
        &self,
        prefix: &Value<Json>,
        _context: &Context,
    ) -> Result<Vec<StoredValue<Json>>, String> {
        let inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        Ok(inner.storage_state.scan_values(prefix))
    }

    async fn read_list(
        &self,
        address: &ValueList<Json>,
        options: Option<ListReadOptions>,
        _context: &Context,
    ) -> Result<Vec<ListElement<Json>>, String> {
        let inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        Ok(inner.storage_state.read_list(address, options))
    }

    async fn scan_branch(
        &self,
        query: StorageBranchScan,
        _context: &Context,
    ) -> Result<Vec<Entry>, String> {
        let inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        inner.storage_state.scan_branch(&query)
    }

    async fn scan_branch_structure(
        &self,
        query: StorageBranchScan,
        _context: &Context,
    ) -> Result<Vec<EntryStructure>, String> {
        let inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        inner.storage_state.scan_branch_structure(&query)
    }

    async fn scan_entries(
        &self,
        query: EntryScan,
        _context: &Context,
    ) -> Result<Vec<Entry>, String> {
        let inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        Ok(inner.storage_state.scan_entries(&query))
    }

    async fn scan_usage(
        &self,
        query: UsageScan,
        _context: &Context,
    ) -> Result<Vec<UsageRow>, String> {
        let inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        Ok(inner.storage_state.scan_usage(&query))
    }

    async fn get_stats(&self, _context: &Context) -> Result<SessionStats, String> {
        let inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        Ok(inner.storage_state.get_stats())
    }

    async fn close(&self, _context: &Context) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        inner.state = StorageState::Closed;
        Ok(())
    }
}

#[derive(Clone)]
struct MemorySessionRecord {
    metadata: SessionMetadata,
    storage: Arc<MemoryStorage>,
}

/// 对应 `MemorySessionRepo`。
pub struct MemorySessionRepo {
    sessions: StdMutex<BTreeMap<String, MemorySessionRecord>>,
    pending_ids: StdMutex<Vec<String>>,
}

impl Default for MemorySessionRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl MemorySessionRepo {
    pub fn new() -> Self {
        Self {
            sessions: StdMutex::new(BTreeMap::new()),
            pending_ids: StdMutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl SessionRepo for MemorySessionRepo {
    async fn create(
        &self,
        options: SessionCreateOptions,
        _context: &Context,
    ) -> Result<Arc<dyn Session>, String> {
        let id = options.id.unwrap_or_else(pi_ai::uuidv7);
        if self.sessions.lock().unwrap().contains_key(&id) {
            return Err(format!("Session already exists: {id}"));
        }
        self.pending_ids.lock().unwrap().push(id.clone());
        let metadata = SessionMetadata {
            id: id.clone(),
            created_at: pi_ai::utils::uuid::now_ms() as u64,
            storage_version: MEMORY_STORAGE_VERSION,
            cwd: None,
            parent_session_id: options.parent_session_id,
            legacy_parent_session_path: None,
        };
        let storage = Arc::new(MemoryStorage::new());
        let session = Arc::new(StorageBackedSession::new(
            metadata.clone(),
            storage.clone(),
            None,
        ));
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), MemorySessionRecord { metadata, storage });
        self.pending_ids.lock().unwrap().retain(|p| p != &id);
        Ok(session)
    }

    async fn open(
        &self,
        metadata: SessionMetadata,
        _context: &Context,
    ) -> Result<Arc<dyn Session>, String> {
        let record = self
            .sessions
            .lock()
            .unwrap()
            .get(&metadata.id)
            .cloned()
            .ok_or_else(|| format!("Session not found: {}", metadata.id))?;
        Ok(Arc::new(StorageBackedSession::new(
            metadata,
            record.storage.clone(),
            None,
        )))
    }

    async fn list(&self, _context: &Context) -> Result<Vec<SessionMetadata>, String> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|r| r.metadata.clone())
            .collect())
    }

    async fn delete(&self, metadata: SessionMetadata, _context: &Context) -> Result<(), String> {
        self.sessions.lock().unwrap().remove(&metadata.id);
        Ok(())
    }

    async fn fork(
        &self,
        source: SessionMetadata,
        options: crate::harness::session::types::ForkOptions,
        _context: &Context,
    ) -> Result<Arc<dyn Session>, String> {
        let record = self
            .sessions
            .lock()
            .unwrap()
            .get(&source.id)
            .cloned()
            .ok_or_else(|| format!("Session not found: {}", source.id))?;
        let storage = Arc::new(record.storage.fork(&options)?);
        let id = match &options {
            crate::harness::session::types::ForkOptions::Branch { id, .. }
            | crate::harness::session::types::ForkOptions::Tree { id } => id.clone(),
        }
        .unwrap_or_else(pi_ai::uuidv7);
        let metadata = SessionMetadata {
            id: id.clone(),
            created_at: pi_ai::utils::uuid::now_ms() as u64,
            storage_version: MEMORY_STORAGE_VERSION,
            cwd: None,
            parent_session_id: Some(source.id.clone()),
            legacy_parent_session_path: None,
        };
        self.sessions.lock().unwrap().insert(
            id.clone(),
            MemorySessionRecord {
                metadata: metadata.clone(),
                storage: storage.clone(),
            },
        );
        Ok(Arc::new(StorageBackedSession::new(metadata, storage, None)))
    }
}

// 未使用抑制。
#[allow(unused)]
fn _unused(_: &CommittedWrite) -> &EntryType {
    unimplemented!()
}
