//! 对应 `src/storage/jsonl/storage.ts` 的存储层。
//!
//! 以 `main.jsonl` 追加提交标记、以 sidecar 文件承载大对象；内存态复用 [`MemoryStorage`]。
//!
//! # 与上游的差异
//!
//! - 上游的 sidecar **回收**（`planReclamations` / `reclaimSidecars`）是空间优化：当 sidecar 只剩
//!   最后一条仍被引用的记录时，用 `.reclaim` 文件原子替换。Rust 版暂不实现回收（只追加），
//!   语义不受影响（读取路径只看 marker 引用的那一条）。
//! - 上游有 `JsonlStoragePoisonedError`：解析失败后把存储标记为「必须重开」。Rust 用
//!   [`StorageError::Message`] 携带同样文案，并置 `poisoned` 标志拒绝后续操作。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as JsonValue;

use crate::chord::context::Context;
use crate::env::FileSystem;
use crate::storage::jsonl::codec::{
    EncodedCommit, MAIN_FILE, MainMarker, MainOperation, SidecarKind, SidecarPayload,
    SidecarRecord, StoredTask, encode_commit, sidecar_file_name,
};
use crate::storage::memory::MemoryStorage;
use crate::types::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress,
    DocumentContent, DocumentCreate, DocumentId, DocumentPoint, DocumentQuery, EntryId, EntryQuery,
    EntryRecord, Page, Seq, Storage, StorageError, StorageWrite, StoredDocument, SubmissionId,
    SubmissionQuery, SubmissionRecord, TaskId, TaskQuery, TaskRecord,
};

/// 打开 JSONL 存储时的选项（对应 `JsonlStorageOptions`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonlStorageOptions {
    /// 是否在追加主标记前冲刷每个受影响的 sidecar。
    pub fsync: bool,
}

/// 对应 `JsonlStorage`。
pub struct JsonlStorage {
    file_system: Arc<dyn FileSystem>,
    directory: String,
    main_path: String,
    options: JsonlStorageOptions,
    memory: MemoryStorage,
    next_seq: std::sync::Mutex<u64>,
    poisoned: std::sync::atomic::AtomicBool,
    closed: std::sync::atomic::AtomicBool,
}

fn corrupted(message: impl Into<String>) -> StorageError {
    StorageError::Message(format!("JsonlCorruptionError: {}", message.into()))
}

fn poisoned_error() -> StorageError {
    StorageError::Message("JSONL storage is poisoned and must be reopened".to_string())
}

fn file_error(action: &str, error: crate::env::FileError) -> StorageError {
    StorageError::Message(format!("JSONL {action} failed: {}", error.message))
}

impl std::fmt::Debug for JsonlStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlStorage")
            .field("directory", &self.directory)
            .field("fsync", &self.options.fsync)
            .finish_non_exhaustive()
    }
}

impl JsonlStorage {
    /// 对应 `open()`：读取 `main.jsonl` 与 sidecar，重建内存态。
    pub async fn open(
        file_system: Arc<dyn FileSystem>,
        directory: impl Into<String>,
        options: JsonlStorageOptions,
        context: &dyn Context,
    ) -> Result<Self, StorageError> {
        let directory = directory.into();
        let main_path = format!("{}/{}", directory.trim_end_matches('/'), MAIN_FILE);
        let memory = MemoryStorage::new();
        let mut next_seq = 1u64;

        if file_system
            .exists(&main_path, context)
            .await
            .unwrap_or(false)
        {
            let content = file_system
                .read_text_file(&main_path, context)
                .await
                .map_err(|error| file_error("read", error))?;

            for (index, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let marker: MainMarker = serde_json::from_str(line).map_err(|_| {
                    corrupted(format!(
                        "Malformed complete commit marker at line {}",
                        index + 1
                    ))
                })?;
                let writes = resolve_marker(&*file_system, &directory, &marker, context).await?;
                memory
                    .commit(&writes, context)
                    .await
                    .map_err(|error| StorageError::Message(error.to_string()))?;
                next_seq = marker.seq.get() + 1;
            }
        }

        Ok(Self {
            file_system,
            directory,
            main_path,
            options,
            memory,
            next_seq: std::sync::Mutex::new(next_seq),
            poisoned: std::sync::atomic::AtomicBool::new(false),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn assert_usable(&self) -> Result<(), StorageError> {
        if self.poisoned.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(poisoned_error());
        }
        if self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(StorageError::Closed);
        }
        Ok(())
    }

    /// 对应 `commit()`：预校验 → 编码 → 写 sidecar → 写主标记 → 应用到内存态。
    async fn write_commit(
        &self,
        writes: &[StorageWrite],
        context: &dyn Context,
    ) -> Result<Seq, StorageError> {
        self.assert_usable()?;
        // 先预校验：Rejected（如重复 ID）在任何持久化前返回，不写盘也不 poison。
        self.memory.validate_writes(writes)?;
        let seq = {
            let mut guard = self.next_seq.lock().expect("next_seq");
            let seq = Seq::new(*guard);
            *guard += 1;
            seq
        };

        let EncodedCommit { marker, sidecars } =
            encode_commit(seq, writes).map_err(|error| StorageError::Message(error.to_string()))?;

        // 两阶段：先 sidecar，后主标记 —— 主标记出现即代表整次提交可见。
        for (file, content) in &sidecars {
            let path = format!("{}/{}", self.directory.trim_end_matches('/'), file);
            self.file_system
                .append_file(&path, content.as_bytes(), context)
                .await
                .map_err(|error| file_error("append sidecar", error))?;
            if self.options.fsync {
                self.file_system
                    .flush_file(&path, context)
                    .await
                    .map_err(|error| file_error("flush sidecar", error))?;
            }
        }

        self.file_system
            .append_file(&self.main_path, marker.as_bytes(), context)
            .await
            .map_err(|error| file_error("append", error))?;
        if self.options.fsync {
            self.file_system
                .flush_file(&self.main_path, context)
                .await
                .map_err(|error| file_error("flush", error))?;
        }

        self.memory
            .commit(writes, context)
            .await
            .map_err(|error| self.record_failure(error))?;
        Ok(seq)
    }

    /// 内存态拒绝时把存储标记为 poisoned（磁盘与内存已分叉）。
    fn record_failure(&self, error: StorageError) -> StorageError {
        match error {
            StorageError::Closed => StorageError::Closed,
            other => {
                self.poisoned
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                other
            }
        }
    }
}

/// 按 `(seq, ordinal)` 定位 sidecar 记录。
///
/// sidecar 文件是**跨提交追加**的，而 `ordinal` 只在单次提交内递增；
/// 因此必须用 marker 自己的 `seq` 一起定位（对应上游的 `sidecarKey = [file, seq, ordinal]`）。
fn find_sidecar_record<'a>(
    records: &'a [SidecarRecord],
    seq: Seq,
    ordinal: usize,
    file: &str,
) -> Result<&'a SidecarRecord, StorageError> {
    records
        .iter()
        .find(|record| record.seq == seq && record.ordinal == ordinal)
        .ok_or_else(|| {
            corrupted(format!(
                "Missing sidecar record {file}#{}@{}",
                ordinal,
                seq.get()
            ))
        })
}

/// 把一行 marker 还原成可应用到内存态的写入（必要时读回 sidecar）。
async fn resolve_marker(
    file_system: &dyn FileSystem,
    directory: &str,
    marker: &MainMarker,
    context: &dyn Context,
) -> Result<Vec<StorageWrite>, StorageError> {
    let mut writes = Vec::with_capacity(marker.writes.len());
    let mut sidecar_cache: BTreeMap<String, Vec<SidecarRecord>> = BTreeMap::new();

    for operation in &marker.writes {
        match operation {
            MainOperation::Conversation { value } => {
                writes.push(StorageWrite::Conversation(*value));
            }
            MainOperation::Entry { value } => {
                writes.push(StorageWrite::Entry(value.clone()));
            }
            MainOperation::Submission { value } => {
                writes.push(StorageWrite::Submission(value.clone()));
            }
            MainOperation::Task { value } => {
                writes.push(StorageWrite::Task(value.clone()));
            }
            MainOperation::TaskSidecar { id, ordinal } => {
                let file = sidecar_file_name(SidecarKind::Task, id.get());
                let records =
                    load_sidecar(file_system, directory, &file, &mut sidecar_cache, context)
                        .await?;
                let record = find_sidecar_record(&records, marker.seq, *ordinal, &file)?;
                let SidecarPayload::Task { value } = &record.payload else {
                    return Err(corrupted(format!(
                        "Sidecar {file} does not hold a task record"
                    )));
                };
                writes.push(StorageWrite::Task((**value).clone()));
            }
            MainOperation::DocumentCreate { record, ordinal } => {
                let file = sidecar_file_name(SidecarKind::Doc, record.id.get());
                let records =
                    load_sidecar(file_system, directory, &file, &mut sidecar_cache, context)
                        .await?;
                let entry = find_sidecar_record(&records, marker.seq, *ordinal, &file)?;
                let SidecarPayload::Document { content, .. } = &entry.payload else {
                    return Err(corrupted(format!(
                        "Sidecar {file} does not hold a document"
                    )));
                };
                let DocumentContent::Base(base) = content else {
                    return Err(corrupted(format!(
                        "document.create must reference a base, {file} holds a delta"
                    )));
                };
                writes.push(StorageWrite::DocumentCreate {
                    record: record.clone(),
                    content: base.clone(),
                });
            }
            MainOperation::DocumentChange { id, ordinal } => {
                let file = sidecar_file_name(SidecarKind::Doc, id.get());
                let records =
                    load_sidecar(file_system, directory, &file, &mut sidecar_cache, context)
                        .await?;
                let entry = find_sidecar_record(&records, marker.seq, *ordinal, &file)?;
                let SidecarPayload::Document { content, .. } = &entry.payload else {
                    return Err(corrupted(format!(
                        "Sidecar {file} does not hold a document"
                    )));
                };
                writes.push(StorageWrite::DocumentChange {
                    id: *id,
                    content: content.clone(),
                });
            }
            MainOperation::DocumentRetire { id } => {
                writes.push(StorageWrite::DocumentRetire { id: *id });
            }
        }
    }
    Ok(writes)
}

async fn load_sidecar(
    file_system: &dyn FileSystem,
    directory: &str,
    file: &str,
    cache: &mut BTreeMap<String, Vec<SidecarRecord>>,
    context: &dyn Context,
) -> Result<Vec<SidecarRecord>, StorageError> {
    if let Some(records) = cache.get(file) {
        return Ok(records.clone());
    }
    let path = format!("{}/{}", directory.trim_end_matches('/'), file);
    let content = file_system
        .read_text_file(&path, context)
        .await
        .map_err(|error| file_error("read sidecar", error))?;
    let mut records = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: SidecarRecord = serde_json::from_str(line)
            .map_err(|_| corrupted(format!("Malformed sidecar record in {file}")))?;
        records.push(record);
    }
    cache.insert(file.to_string(), records.clone());
    Ok(records)
}

fn delegated<T>(result: Result<T, StorageError>) -> Result<T, StorageError> {
    result.map_err(|error| match error {
        // Closed 是有语义的错误，保留；其余折叠成 Message。
        StorageError::Closed => StorageError::Closed,
        other => StorageError::Message(other.to_string()),
    })
}

#[async_trait::async_trait]
impl Storage for JsonlStorage {
    async fn commit(
        &self,
        writes: &[StorageWrite],
        context: &dyn Context,
    ) -> Result<Seq, StorageError> {
        self.write_commit(writes, context).await
    }

    async fn mint_id(&self) -> u64 {
        self.memory.mint_id().await
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: &dyn Context,
    ) -> Result<Option<ConversationRecord>, StorageError> {
        delegated(self.memory.conversation(id, context).await)
    }

    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn Context,
    ) -> Result<Page<ConversationRecord, Cursor>, StorageError> {
        delegated(
            self.memory
                .scan_conversations(query, limit, cursor, context)
                .await,
        )
    }

    async fn entry(
        &self,
        id: EntryId,
        context: &dyn Context,
    ) -> Result<Option<(EntryRecord, Seq)>, StorageError> {
        delegated(self.memory.entry(id, context).await)
    }

    async fn entry_in_conversation(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &dyn Context,
    ) -> Result<Option<(EntryRecord, Seq)>, StorageError> {
        delegated(
            self.memory
                .entry_in_conversation(conversation_id, id, context)
                .await,
        )
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
        context: &dyn Context,
    ) -> Result<Option<(EntryRecord, EntryId)>, StorageError> {
        delegated(
            self.memory
                .find_latest_head_marker(conversation_id, at_or_before_entry_id, context)
                .await,
        )
    }

    async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn Context,
    ) -> Result<Page<EntryRecord, Cursor>, StorageError> {
        delegated(
            self.memory
                .scan_entries(query, limit, cursor, context)
                .await,
        )
    }

    async fn task(
        &self,
        id: TaskId,
        context: &dyn Context,
    ) -> Result<Option<TaskRecord<JsonValue, JsonValue, JsonValue>>, StorageError> {
        delegated(self.memory.task(id, context).await)
    }

    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn Context,
    ) -> Result<Page<StoredTask, Cursor>, StorageError> {
        delegated(self.memory.scan_tasks(query, limit, cursor, context).await)
    }

    async fn submission(
        &self,
        id: SubmissionId,
        context: &dyn Context,
    ) -> Result<Option<SubmissionRecord>, StorageError> {
        delegated(self.memory.submission(id, context).await)
    }

    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn Context,
    ) -> Result<Page<SubmissionRecord, Cursor>, StorageError> {
        delegated(
            self.memory
                .scan_submissions(query, limit, cursor, context)
                .await,
        )
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &dyn Context,
    ) -> Result<Option<SubmissionRecord>, StorageError> {
        delegated(
            self.memory
                .submission_by_request(conversation_id, request_id, context)
                .await,
        )
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        context: &dyn Context,
    ) -> Result<Option<crate::types::DocumentRecord>, StorageError> {
        delegated(self.memory.find_document(address, at, context).await)
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &dyn Context,
    ) -> Result<Option<StoredDocument>, StorageError> {
        delegated(self.memory.document(id, at, context).await)
    }

    async fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn Context,
    ) -> Result<Page<crate::types::DocumentRecord, Cursor>, StorageError> {
        delegated(
            self.memory
                .scan_documents(query, limit, cursor, context)
                .await,
        )
    }

    async fn close(&self, context: &dyn Context) -> Result<(), StorageError> {
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.memory.close(context).await
    }
}

// 供实现 `StorageWrite::DocumentCopy` 的调用方使用（该分支需先展开为 create）。
#[allow(dead_code)]
fn unresolved_copy(record: &DocumentCreate) -> StorageError {
    StorageError::Message(format!(
        "document.copy {} must be resolved to document.create before encoding",
        record.id
    ))
}
