//! Rust 翻译自 packages/agent/src/harness/session/jsonl/storage.ts（v4 核心，legacy-v3 迁移简化）
//!
//! 基于 InMemoryStorageState 的 JSONL 文件 Storage。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::Value as Json;
use tokio::sync::Mutex;

use crate::harness::context::Context;
use crate::harness::session::commit::CommittedWrite;
use crate::harness::session::in_memory_storage_state::InMemoryStorageState;
use crate::harness::session::jsonl::codec::parse_jsonl_session_header;
use crate::harness::session::jsonl::types::{JSONL_STORAGE_VERSION, JsonlStorageHeader};
use crate::harness::session::types::{
    CommitResult, Entry, EntryScan, EntryStructure, SessionStats, Storage, StorageBranchScan,
    UsageRow, UsageScan, Write,
};
use crate::harness::session::values::{
    ListElement, ListReadOptions, StoredValue, Value, ValueList,
};
use crate::harness::types::FileSystem;

struct JsonlInner {
    storage_state: InMemoryStorageState,
    state: StorageState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StorageState {
    Open,
    Closed,
}

/// 对应 `JsonlStorageOptions`。
pub struct JsonlStorageOptions {
    pub file_system: Arc<dyn FileSystem>,
    pub path: String,
}

fn serialize_storage(header: &JsonlStorageHeader, transactions: &[Vec<CommittedWrite>]) -> String {
    let mut lines = vec![serde_json::to_string(header).unwrap_or_else(|_| "{}".to_string())];
    for transaction in transactions {
        let value: Json = match transaction.len() {
            0 => Json::Null,
            1 => serde_json::to_value(&transaction[0]).unwrap_or(Json::Null),
            _ => serde_json::to_value(transaction).unwrap_or(Json::Null),
        };
        lines.push(serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()));
    }
    format!("{}\n", lines.join("\n"))
}

fn parse_transaction(line: &str) -> Result<Vec<CommittedWrite>, String> {
    let value: Json = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if value.is_array() {
        serde_json::from_value(value).map_err(|e| e.to_string())
    } else {
        let write: CommittedWrite = serde_json::from_value(value).map_err(|e| e.to_string())?;
        Ok(vec![write])
    }
}

fn split_complete_lines(content: &str) -> (Vec<String>, bool) {
    if content.ends_with('\n') {
        let body = content.strip_suffix('\n').unwrap_or(content);
        return (body.split('\n').map(String::from).collect(), false);
    }
    match content.rfind('\n') {
        None => (Vec::new(), true),
        Some(last) => (
            content[..last].split('\n').map(String::from).collect(),
            true,
        ),
    }
}

/// 对应 `JsonlStorage`。
pub struct JsonlStorage {
    options: JsonlStorageOptions,
    #[allow(dead_code)]
    header: JsonlStorageHeader,
    inner: Arc<Mutex<JsonlInner>>,
}

impl JsonlStorage {
    pub async fn create(
        options: JsonlStorageOptions,
        header: JsonlStorageHeader,
        initial_writes: Vec<Write>,
        context: &Context,
    ) -> Result<Self, String> {
        let mut storage_state = InMemoryStorageState::new();
        let timestamp = header.created_at;
        let prepared = storage_state.prepare_commit(&initial_writes, timestamp);
        let transactions = if prepared.writes.is_empty() {
            Vec::new()
        } else {
            vec![prepared.writes.clone()]
        };
        let content = serialize_storage(&header, &transactions);
        options
            .file_system
            .write_file(&options.path, content.as_bytes(), context.abort_signal())
            .await
            .map_err(|e| {
                format!(
                    "Failed to write JSONL storage {}: {}",
                    options.path, e.message
                )
            })?;
        storage_state.apply_validated(&prepared.writes);
        Ok(Self {
            options,
            header,
            inner: Arc::new(Mutex::new(JsonlInner {
                storage_state,
                state: StorageState::Open,
            })),
        })
    }

    pub async fn open(options: JsonlStorageOptions, context: &Context) -> Result<Self, String> {
        let content = options
            .file_system
            .read_text_file(&options.path, context.abort_signal())
            .await
            .map_err(|e| {
                format!(
                    "Failed to read JSONL storage {}: {}",
                    options.path, e.message
                )
            })?;
        let (lines, torn) = split_complete_lines(&content);
        if lines.first().map(|s| s.as_str()).unwrap_or("").is_empty() {
            return Err(format!(
                "Invalid JSONL storage {}: missing header",
                options.path
            ));
        }
        let parsed = parse_jsonl_session_header(&lines[0])?;
        let header = match parsed {
            crate::harness::session::jsonl::codec::JsonlParsedSessionHeader::V4 { header } => {
                header
            }
            crate::harness::session::jsonl::codec::JsonlParsedSessionHeader::V3Legacy {
                ..
            } => {
                return Err("Legacy v3 JSONL migration is not yet implemented".to_string());
            }
        };
        if header.storage_version != JSONL_STORAGE_VERSION {
            return Err(format!(
                "Session {} uses unsupported storage version {}",
                header.id, header.storage_version
            ));
        }
        let mut storage_state = InMemoryStorageState::new();
        for (index, line) in lines.iter().enumerate().skip(1) {
            let writes = parse_transaction(line).map_err(|e| {
                format!(
                    "Invalid JSONL storage {}: line {} {e}",
                    options.path,
                    index + 1
                )
            })?;
            storage_state.validate_committed(&writes).map_err(|e| {
                format!(
                    "Invalid JSONL storage {}: line {} {e}",
                    options.path,
                    index + 1
                )
            })?;
            storage_state.apply_validated(&writes);
        }
        if let Some(next_seq) = header.next_seq {
            storage_state.advance_next_seq(next_seq);
        }
        if torn {
            let _ = options
                .file_system
                .write_file(
                    &options.path,
                    format!("{}\n", lines.join("\n")).as_bytes(),
                    context.abort_signal(),
                )
                .await;
        }
        Ok(Self {
            options,
            header,
            inner: Arc::new(Mutex::new(JsonlInner {
                storage_state,
                state: StorageState::Open,
            })),
        })
    }

    fn assert_open(&self, inner: &JsonlInner) -> Result<(), String> {
        if inner.state != StorageState::Open {
            return Err("JsonlStorage is closed".to_string());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Storage for JsonlStorage {
    async fn commit(&self, writes: Vec<Write>, context: &Context) -> Result<CommitResult, String> {
        let mut inner = self.inner.lock().await;
        self.assert_open(&inner)?;
        let timestamp = pi_ai::utils::uuid::now_ms() as u64;
        let prepared = inner.storage_state.prepare_commit(&writes, timestamp);
        if !prepared.writes.is_empty() {
            let line = serde_json::to_string(&prepared.writes).map_err(|e| e.to_string())?;
            self.options
                .file_system
                .append_file(
                    &self.options.path,
                    format!("{line}\n").as_bytes(),
                    context.abort_signal(),
                )
                .await
                .map_err(|e| {
                    format!(
                        "Failed to append JSONL storage {}: {}",
                        self.options.path, e.message
                    )
                })?;
        }
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

#[allow(unused)]
fn _unused(_: &StdMutex<()>) {}
