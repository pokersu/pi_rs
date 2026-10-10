//! 对应 `src/storage/memory.ts`：内存参考实现。
//!
//! # 与上游的差异
//!
//! - 上游维护「Map + 有序 ID 数组 + 二分查找」的索引结构（为扫描性能）。Rust 用 `BTreeMap`
//!   天然有序，扫描直接做区间迭代 + 过滤，**语义一致**但少了手写二分的复杂度。
//! - 上游的 `prepareCommit()` 分两阶段（先校验并冻结、再由调用方 `apply()`），用于把「校验」与
//!   「生效」分离给上层。Rust 版在 `commit()` 内一步完成（校验失败则不产生任何效果），
//!   等价于「任何持久化效果之前拒绝」。
//! - 上游 clone/freeze 是为了对齐序列化后端的所有权边界。Rust 的 `#[derive(Clone)]` 已提供
//!   同样的隔离，无需 freeze。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde_json::Value as JsonValue;

use super::resolve_document_copies;
use crate::chord::context::Context;
use crate::chord::delta::{Op, apply_immutable_batches};
use crate::errors::StorageRejected;
use crate::storage::scan::{ScanStart, page, scan_start};
use crate::types::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress,
    DocumentContent, DocumentCreate, DocumentId, DocumentPoint, DocumentQuery, DocumentRecord,
    DocumentScope, EntryId, EntryQuery, EntryRecord, JsonObject, Page, ScanOrder, Seq, Storage,
    StorageError, StorageWrite, StoredDocument, SubmissionId, SubmissionQuery, SubmissionRecord,
    SubmissionStatus, TaskId, TaskQuery, TaskRecord, TaskStatus,
};

type StoredTask = TaskRecord<JsonValue, JsonValue, JsonValue>;

/// 存储的表名（用于全局 ID 归属校验）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableName {
    Conversation,
    Entry,
    Task,
    Submission,
    Document,
}

impl TableName {
    /// 对应上游错误消息里的表名。
    fn as_str(self) -> &'static str {
        match self {
            TableName::Conversation => "conversation",
            TableName::Entry => "entry",
            TableName::Task => "task",
            TableName::Submission => "submission",
            TableName::Document => "document",
        }
    }
}

struct DocumentRevision {
    content: DocumentContent,
    seq: Seq,
}

struct StoredDocumentState {
    record: DocumentRecord,
    revisions: Vec<DocumentRevision>,
}

#[derive(Default)]
struct AddressIndex {
    ids: Vec<DocumentId>,
    current: Option<DocumentId>,
}

#[derive(Default)]
struct MemoryState {
    record_types: BTreeMap<u64, TableName>,
    conversations: BTreeMap<u64, ConversationRecord>,
    entries: BTreeMap<u64, EntryRecord>,
    entry_commit_seqs: BTreeMap<u64, Seq>,
    tasks: BTreeMap<u64, StoredTask>,
    submissions: BTreeMap<u64, SubmissionRecord>,
    documents: BTreeMap<u64, StoredDocumentState>,
    conversation_ids_by_owner_conversation: BTreeMap<u64, BTreeSet<u64>>,
    conversation_ids_by_owner_task: BTreeMap<u64, BTreeSet<u64>>,
    entry_ids_by_conversation: BTreeMap<u64, BTreeSet<u64>>,
    submission_ids_by_request: BTreeMap<(u64, String), u64>,
    submission_ids_by_status: BTreeMap<SubmissionStatus, BTreeSet<u64>>,
    task_ids_by_status: BTreeMap<TaskStatus, BTreeSet<u64>>,
    document_addresses: BTreeMap<String, AddressIndex>,
    document_ids_by_scope: BTreeMap<String, BTreeSet<u64>>,
}

struct MemoryInner {
    state: MemoryState,
    next_id: u64,
    next_seq: u64,
    closed: bool,
}

/// 对应 `MemoryStorage`。
pub struct MemoryStorage {
    inner: Mutex<MemoryInner>,
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStorage {
    /// 对应 `constructor`：ID 从 2 开始（1 是根会话），序号从 1 开始。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MemoryInner {
                state: MemoryState::default(),
                next_id: 2,
                next_seq: 1,
                closed: false,
            }),
        }
    }

    /// 预校验一批写入（不应用、不推进序号）：供 jsonl 等先写盘的后端在持久化前检查。
    pub fn validate_writes(&self, writes: &[StorageWrite]) -> Result<(), StorageError> {
        let inner = self.inner.lock().expect("memory storage");
        validate_writes(&inner, writes)
    }

    /// 展开 `document.copy` 为 `document.create`（物化源文档为 base）。
    ///
    /// 供 jsonl 等「先编码后应用」的后端在写盘前调用，也供 `commit` 内部复用；
    /// 三项校验（源本批未改 / 源可读 / 记录一致）见 [`resolve_document_copies`]。
    pub fn resolve_document_copies(
        &self,
        writes: &[StorageWrite],
    ) -> Result<Vec<StorageWrite>, StorageError> {
        let inner = self.inner.lock().expect("memory storage");
        resolve_document_copies(writes, |id, at| {
            materialize_document_state(&inner.state, id, at)
        })
    }

    fn assert_open(&self) -> Result<(), StorageError> {
        if self.inner.lock().expect("memory storage").closed {
            return Err(StorageError::Closed);
        }
        Ok(())
    }
}

// ─── 作用域 / 地址键 ─────────────────────────────────────────────────────────

/// 对应 `scopeKey`。
fn scope_key(scope: &DocumentScope) -> String {
    match scope {
        DocumentScope::Session => serde_json::to_string(&serde_json::json!(["session"])).unwrap(),
        DocumentScope::Conversation {
            conversation_id, ..
        } => serde_json::to_string(&serde_json::json!(["conversation", conversation_id.get()]))
            .unwrap(),
        DocumentScope::Task { task_id } => {
            serde_json::to_string(&serde_json::json!(["task", task_id.get()])).unwrap()
        }
    }
}

/// 对应 `addressKey`。
fn address_key(kind: &str, scope: &DocumentScope, key: Option<&str>) -> String {
    let key_part = match key {
        None => serde_json::json!(["singleton"]),
        Some(key) => serde_json::json!(["family", key]),
    };
    serde_json::to_string(&serde_json::json!([kind, scope_key(scope), key_part])).unwrap()
}

/// 对应 `isAliveAt`。
fn is_alive_at(record: &DocumentRecord, at: DocumentPoint) -> bool {
    match at {
        DocumentPoint::Current => record.retired_at.is_none(),
        DocumentPoint::At(seq) => {
            record.created_at <= seq && record.retired_at.is_none_or(|retired| seq < retired)
        }
    }
}

/// 对应 `isCurrentOnly`。
fn is_current_only(record: &DocumentRecord) -> bool {
    !matches!(record.scope, DocumentScope::Conversation { .. })
        || record.history == Some(crate::types::ConversationHistory::Latest)
}

/// 对应 `scanIndexes`：按扫描顺序收集区间内的键。
fn scan_keys(keys: &BTreeSet<u64>, start: ScanStart) -> Vec<u64> {
    match start.order {
        ScanOrder::Ascending => keys
            .range(start.after.map_or(0u64.., |after| after + 1..))
            .copied()
            .collect(),
        ScanOrder::Descending => keys
            .range(..start.after.unwrap_or(u64::MAX))
            .rev()
            .copied()
            .collect(),
    }
}

/// 对应 `visibleEntries`：fork 链上从指定会话向上、`[min, max]` 内可见的条目 ID（降序）。
///
/// 每个会话的条目被其父会话的 `parent.at` 截断：分叉只继承父会话在该条目（含）之前的历史。
fn visible_entry_ids(state: &MemoryState, conversation_id: u64, min: u64, max: u64) -> Vec<u64> {
    let mut result = Vec::new();
    let mut current_id = conversation_id;
    let mut upper = max;
    loop {
        if let Some(ids) = state.entry_ids_by_conversation.get(&current_id) {
            for &id in ids.range(..=upper).rev() {
                if id < min {
                    break;
                }
                result.push(id);
            }
        }
        let Some(conversation) = state.conversations.get(&current_id) else {
            break;
        };
        let Some(parent) = conversation.parent else {
            break;
        };
        upper = upper.min(parent.at.get());
        if upper < min {
            break;
        }
        current_id = parent.conversation_id.get();
    }
    result
}

/// 对应 `visibleEntriesAscending`：同上，升序（从根会话段向前）。
fn visible_entry_ids_ascending(
    state: &MemoryState,
    conversation_id: u64,
    min: u64,
    max: u64,
) -> Vec<u64> {
    let mut segments: Vec<(u64, u64)> = Vec::new();
    let mut current_id = conversation_id;
    let mut upper = max;
    loop {
        segments.push((current_id, upper));
        let Some(conversation) = state.conversations.get(&current_id) else {
            break;
        };
        let Some(parent) = conversation.parent else {
            break;
        };
        upper = upper.min(parent.at.get());
        if upper < min {
            break;
        }
        current_id = parent.conversation_id.get();
    }
    let mut result = Vec::new();
    for (cid, seg_upper) in segments.into_iter().rev() {
        if let Some(ids) = state.entry_ids_by_conversation.get(&cid) {
            for &id in ids.range(min..=seg_upper) {
                result.push(id);
            }
        }
    }
    result
}

// ─── 写入应用 ────────────────────────────────────────────────────────────────

/// 取一条写入的 `(ID, 表名)`；`document.change`/`document.retire` 不占用新 ID。
fn write_identity(write: &StorageWrite) -> Option<(u64, TableName)> {
    match write {
        StorageWrite::Conversation(record) => Some((record.id.get(), TableName::Conversation)),
        StorageWrite::Entry(record) => Some((record.id.get(), TableName::Entry)),
        StorageWrite::Task(record) => Some((record.id.get(), TableName::Task)),
        StorageWrite::Submission(record) => {
            Some((record.identity().id.get(), TableName::Submission))
        }
        StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
            Some((record.id.get(), TableName::Document))
        }
        StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => None,
    }
}

/// 预校验一批写入：只读现有状态 + 本批局部集合，失败不污染任何状态。
fn validate_writes(inner: &MemoryInner, writes: &[StorageWrite]) -> Result<(), StorageError> {
    let mut claimed: BTreeMap<u64, TableName> = BTreeMap::new();
    for write in writes {
        let Some((id, table)) = write_identity(write) else {
            continue;
        };
        let existing = inner.state.record_types.get(&id).copied();
        let earlier = claimed.get(&id).copied();
        match table {
            // conversation / entry / document 是「不可变创建」：任何已占用都拒绝。
            TableName::Conversation | TableName::Entry | TableName::Document => {
                if let Some(existing) = existing {
                    return Err(StorageError::Rejected(StorageRejected::new(format!(
                        "ID {id} already belongs to {}",
                        existing.as_str()
                    ))));
                }
                if earlier.is_some() {
                    return Err(StorageError::Rejected(StorageRejected::new(format!(
                        "ID {id} is written more than once"
                    ))));
                }
            }
            // task / submission 是「最新记录替换」：同表可更新，跨表仍拒绝。
            TableName::Task | TableName::Submission => {
                if let Some(existing) = existing
                    && existing != table
                {
                    return Err(StorageError::Rejected(StorageRejected::new(format!(
                        "ID {id} already belongs to {}",
                        existing.as_str()
                    ))));
                }
                if let Some(earlier) = earlier
                    && earlier != table
                {
                    return Err(StorageError::Rejected(StorageRejected::new(format!(
                        "ID {id} is written as two record types"
                    ))));
                }
            }
        }
        claimed.insert(id, table);
    }
    Ok(())
}

/// 对应上游 `DocumentAction`：同一文档在一批提交内的 create/change/retire 组合。
#[derive(Default)]
struct DocumentAction {
    create: Option<DocumentCreate>,
    content: Option<DocumentContent>,
    retire: bool,
}

fn content_version(content: &DocumentContent) -> u32 {
    match content {
        DocumentContent::Base(base) => base.version,
        DocumentContent::Delta { version, .. } => *version,
    }
}

/// 对应上游 `prepareDocumentActions`：聚合一批写入里每个文档的 create/change/retire。
fn prepare_document_actions(
    writes: &[StorageWrite],
) -> Result<BTreeMap<u64, DocumentAction>, StorageError> {
    let mut actions: BTreeMap<u64, DocumentAction> = BTreeMap::new();
    for write in writes {
        match write {
            StorageWrite::DocumentCreate { record, content } => {
                let action = actions.entry(record.id.get()).or_default();
                if action.create.is_some() || action.content.is_some() {
                    return Err(StorageError::Message(format!(
                        "Document {} has more than one content command",
                        record.id.get()
                    )));
                }
                action.create = Some(record.clone());
                action.content = Some(DocumentContent::Base(content.clone()));
            }
            StorageWrite::DocumentChange { id, content } => {
                let action = actions.entry(id.get()).or_default();
                if action.content.is_some() {
                    return Err(StorageError::Message(format!(
                        "Document {} has more than one content command",
                        id.get()
                    )));
                }
                action.content = Some(content.clone());
            }
            StorageWrite::DocumentRetire { id } => {
                let action = actions.entry(id.get()).or_default();
                if action.retire {
                    return Err(StorageError::Message(format!(
                        "Document {} is retired more than once",
                        id.get()
                    )));
                }
                action.retire = true;
            }
            _ => {}
        }
    }
    Ok(actions)
}

/// 对应上游 `checkDocumentActions`：文档语义校验（在应用前执行，失败不污染任何状态）。
fn check_document_actions(
    state: &MemoryState,
    actions: &BTreeMap<u64, DocumentAction>,
) -> Result<(), StorageError> {
    let mut live_counts: BTreeMap<String, i64> = BTreeMap::new();
    for (id, action) in actions {
        let existing = state.documents.get(id);
        if action.create.is_none() && existing.is_none() {
            return Err(StorageError::Message(format!("Unknown document: {id}")));
        }
        if action.create.is_some() && existing.is_some() {
            return Err(StorageError::Message(format!(
                "Document {id} already exists"
            )));
        }
        if existing.is_some_and(|entry| entry.record.retired_at.is_some()) {
            return Err(StorageError::Message(format!("Document {id} is retired")));
        }
        let previous = existing.and_then(|entry| entry.revisions.last());
        if let Some(DocumentContent::Delta { version, .. }) = &action.content {
            let Some(previous) = previous else {
                return Err(StorageError::Message(format!(
                    "Document {id} delta has no base"
                )));
            };
            if content_version(&previous.content) != *version {
                return Err(StorageError::Message(format!(
                    "Document {id} version transition requires a base"
                )));
            }
        }

        // 同一地址不能有超过一个当前化身。
        let key = match &action.create {
            Some(create) => address_key(&create.kind, &create.scope, create.key.as_deref()),
            None => {
                let record = &existing.expect("checked above").record;
                address_key(&record.kind, &record.scope, record.key.as_deref())
            }
        };
        let current_id = state
            .document_addresses
            .get(&key)
            .and_then(|index| index.current);
        let live = live_counts
            .entry(key)
            .or_insert(if current_id.is_none() { 0 } else { 1 });
        if action.retire && current_id == Some(DocumentId::new(*id)) {
            *live -= 1;
        }
        if action.create.is_some() && !action.retire {
            *live += 1;
        }
    }
    for live in live_counts.values() {
        if *live > 1 {
            return Err(StorageError::Message(
                "Document address already has a current incarnation".to_string(),
            ));
        }
    }
    Ok(())
}

fn apply_writes(
    inner: &mut MemoryInner,
    writes: &[StorageWrite],
    seq: Seq,
) -> Result<(), StorageError> {
    // 0) 展开 document.copy（物化源 → create；校验源本批未改、源可读、记录一致）。
    let resolved = {
        let state = &inner.state;
        resolve_document_copies(writes, |id, at| materialize_document_state(state, id, at))?
    };
    let writes = resolved.as_slice();

    // 1) 校验阶段：失败不污染任何状态。
    validate_writes(inner, writes)?;
    let document_actions = prepare_document_actions(writes)?;
    check_document_actions(&inner.state, &document_actions)?;

    // 2) 登记 ID 归属（仅在校验全部通过后）。
    let claimed: BTreeMap<u64, TableName> = writes.iter().filter_map(write_identity).collect();
    for (id, table) in &claimed {
        inner.state.record_types.insert(*id, *table);
    }

    let state = &mut inner.state;
    for write in writes {
        match write {
            StorageWrite::Conversation(record) => {
                state.conversations.insert(record.id.get(), *record);
                if let Some(owner) = record.owner {
                    state
                        .conversation_ids_by_owner_conversation
                        .entry(owner.conversation_id.get())
                        .or_default()
                        .insert(record.id.get());
                    state
                        .conversation_ids_by_owner_task
                        .entry(owner.task_id.get())
                        .or_default()
                        .insert(record.id.get());
                }
            }
            StorageWrite::Entry(record) => {
                state.entries.insert(record.id.get(), record.clone());
                state.entry_commit_seqs.insert(record.id.get(), seq);
                state
                    .entry_ids_by_conversation
                    .entry(record.conversation_id.get())
                    .or_default()
                    .insert(record.id.get());
            }
            StorageWrite::Task(record) => {
                if let Some(previous) = state.tasks.get(&record.id.get())
                    && let Some(set) = state.task_ids_by_status.get_mut(&previous.state.status())
                {
                    set.remove(&record.id.get());
                }
                state.tasks.insert(record.id.get(), record.clone());
                state
                    .task_ids_by_status
                    .entry(record.state.status())
                    .or_default()
                    .insert(record.id.get());
            }
            StorageWrite::Submission(record) => {
                let id = record.identity().id.get();
                // 更新时先移除旧的 status 索引与 requestId 映射（对齐上游）。
                if let Some(previous) = state.submissions.get(&id) {
                    if previous.status() != record.status()
                        && let Some(set) =
                            state.submission_ids_by_status.get_mut(&previous.status())
                    {
                        set.remove(&id);
                    }
                    if let Some(previous_request_id) = &previous.identity().request_id {
                        let key = (
                            previous.identity().conversation_id.get(),
                            previous_request_id.clone(),
                        );
                        if state.submission_ids_by_request.get(&key) == Some(&id) {
                            state.submission_ids_by_request.remove(&key);
                        }
                    }
                }
                state.submissions.insert(id, record.clone());
                state
                    .submission_ids_by_status
                    .entry(record.status())
                    .or_default()
                    .insert(id);
                if let Some(request_id) = &record.identity().request_id {
                    state.submission_ids_by_request.insert(
                        (record.identity().conversation_id.get(), request_id.clone()),
                        id,
                    );
                }
            }
            StorageWrite::DocumentCreate { record, content } => {
                let doc = DocumentRecord {
                    id: record.id,
                    kind: record.kind.clone(),
                    key: record.key.clone(),
                    created_at: seq,
                    retired_at: None,
                    history: record.history,
                    fork: record.fork,
                    scope: record.scope,
                };
                state.documents.insert(
                    record.id.get(),
                    StoredDocumentState {
                        record: doc.clone(),
                        revisions: vec![DocumentRevision {
                            content: DocumentContent::Base(content.clone()),
                            seq,
                        }],
                    },
                );
                index_document(state, &doc);
            }
            StorageWrite::DocumentCopy { .. } => {
                unreachable!("document.copy is resolved to document.create before apply")
            }
            StorageWrite::DocumentChange { id, content } => {
                let entry = state.documents.get_mut(&id.get()).ok_or_else(|| {
                    StorageError::Rejected(StorageRejected::new(format!(
                        "Document {} does not exist",
                        id.get()
                    )))
                })?;
                entry.revisions.push(DocumentRevision {
                    content: content.clone(),
                    seq,
                });
            }
            StorageWrite::DocumentRetire { id } => {
                let entry = state.documents.get_mut(&id.get()).ok_or_else(|| {
                    StorageError::Rejected(StorageRejected::new(format!(
                        "Document {} does not exist",
                        id.get()
                    )))
                })?;
                entry.record.retired_at = Some(seq);
                let key = address_key(
                    &entry.record.kind,
                    &entry.record.scope,
                    entry.record.key.as_deref(),
                );
                if let Some(index) = state.document_addresses.get_mut(&key)
                    && index.current == Some(entry.record.id)
                {
                    index.current = None;
                }
            }
        }
    }

    // 推进 next_id（对应上游每个 record 分支的 nextId = max(nextId, id + 1)）。
    for (id, _) in writes.iter().filter_map(write_identity) {
        inner.next_id = inner.next_id.max(id + 1);
    }
    Ok(())
}

fn index_document(state: &mut MemoryState, record: &DocumentRecord) {
    let key = address_key(&record.kind, &record.scope, record.key.as_deref());
    let index = state.document_addresses.entry(key).or_default();
    if !index.ids.contains(&record.id) {
        index.ids.push(record.id);
    }
    index.current = Some(record.id);
    state
        .document_ids_by_scope
        .entry(scope_key(&record.scope))
        .or_default()
        .insert(record.id.get());
}

/// 物化一个文档化身到选定点，返回 `(value, version)`。
fn materialize(
    state: &MemoryState,
    id: DocumentId,
    at: DocumentPoint,
) -> Result<(JsonObject, u32, usize), StorageError> {
    let Some(stored) = state.documents.get(&id.get()) else {
        return Err(StorageError::Message(format!(
            "Document {} does not exist",
            id.get()
        )));
    };

    // 对应上游 `at === "current" ? revisions : revisions.filter(r => r.seq <= at)`。
    let revisions: Vec<&DocumentRevision> = match at {
        DocumentPoint::Current => stored.revisions.iter().collect(),
        DocumentPoint::At(seq) => stored
            .revisions
            .iter()
            .take_while(|revision| revision.seq <= seq)
            .collect(),
    };

    // 选定点之前（含）的最新 base，然后重放其后（且仍在选定点之前）的增量。
    let mut base_index = None;
    for (index, revision) in revisions.iter().enumerate() {
        if matches!(revision.content, DocumentContent::Base(_)) {
            base_index = Some(index);
        }
    }
    let Some(base_index) = base_index else {
        return Err(StorageError::Message(format!(
            "Document {} is missing a required base",
            id.get()
        )));
    };

    let DocumentContent::Base(base) = &revisions[base_index].content else {
        unreachable!("base_index points at a base revision")
    };
    let version = base.version;
    let mut batches: Vec<Vec<Op>> = Vec::new();
    for revision in &revisions[base_index + 1..] {
        match &revision.content {
            DocumentContent::Delta {
                version: delta_version,
                ops,
            } => {
                if *delta_version != version {
                    return Err(StorageError::Message(format!(
                        "Document {} crosses a stored version boundary without a base",
                        id.get()
                    )));
                }
                batches.push(ops.clone());
            }
            DocumentContent::Base(_) => break,
        }
    }

    let value = apply_immutable_batches(
        Some(JsonValue::Object(base.value.clone())),
        batches.iter().map(Vec::as_slice),
    )
    .map_err(|error| StorageError::Message(error.to_string()))?;

    match value {
        JsonValue::Object(map) => Ok((map, version, revisions.len() - base_index - 1)),
        _ => Err(StorageError::Message(
            "document root must stay an object".to_string(),
        )),
    }
}

/// 物化文档到选定点，含 `isCurrentOnly` / `isAliveAt` 检查（对应上游 `materializeDocument`）。
fn materialize_document_state(
    state: &MemoryState,
    id: DocumentId,
    at: DocumentPoint,
) -> Result<Option<StoredDocument>, StorageError> {
    let Some(stored) = state.documents.get(&id.get()) else {
        return Ok(None);
    };
    // 非当前点的读取要求记录保留历史内容（对应上游 `isCurrentOnly` 检查）。
    if !matches!(at, DocumentPoint::Current) && is_current_only(&stored.record) {
        return Err(StorageError::Message(format!(
            "Document {} does not retain historical content",
            id.get()
        )));
    }
    if !is_alive_at(&stored.record, at) {
        return Ok(None);
    }
    let (value, version, deltas_since_base) = materialize(state, id, at)?;
    Ok(Some(StoredDocument {
        record: stored.record.clone(),
        version,
        value,
        deltas_since_base,
    }))
}

// ─── Storage 实现 ────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl Storage for MemoryStorage {
    async fn commit(
        &self,
        writes: &[StorageWrite],
        _context: &dyn Context,
    ) -> Result<Seq, StorageError> {
        let mut inner = self.inner.lock().expect("memory storage");
        if inner.closed {
            return Err(StorageError::Closed);
        }
        let seq = Seq::new(inner.next_seq);
        apply_writes(&mut inner, writes, seq)?;
        inner.next_seq += 1;
        Ok(seq)
    }

    async fn mint_id(&self) -> u64 {
        // 上游 `mintId` 会 `assertOpen()` 并抛异常；`Storage::mint_id` 的签名无法返回错误，
        // 这里用 panic 保持“关闭后不可用”的语义。
        let mut inner = self.inner.lock().expect("memory storage");
        assert!(!inner.closed, "MemoryStorage is closed");
        let id = inner.next_id;
        inner.next_id += 1;
        id
    }

    async fn conversation(
        &self,
        id: ConversationId,
        _context: &dyn Context,
    ) -> Result<Option<ConversationRecord>, StorageError> {
        self.assert_open()?;
        Ok(self
            .inner
            .lock()
            .expect("memory storage")
            .state
            .conversations
            .get(&id.get())
            .copied())
    }

    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<ConversationRecord, Cursor>, StorageError> {
        self.assert_open()?;
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let inner = self.inner.lock().expect("memory storage");
        let state = &inner.state;

        let keys: BTreeSet<u64> = if let Some(owner_conversation) = query.owner_conversation_id {
            state
                .conversation_ids_by_owner_conversation
                .get(&owner_conversation.get())
                .cloned()
                .unwrap_or_default()
        } else if let Some(owner_task) = query.owner_task_id {
            state
                .conversation_ids_by_owner_task
                .get(&owner_task.get())
                .cloned()
                .unwrap_or_default()
        } else {
            state.conversations.keys().copied().collect()
        };

        let values: Vec<ConversationRecord> = scan_keys(&keys, start)
            .into_iter()
            .filter_map(|key| state.conversations.get(&key).copied())
            .collect();
        Ok(page(values, limit, start.order, |record| record.id.get()))
    }

    async fn entry(
        &self,
        id: EntryId,
        _context: &dyn Context,
    ) -> Result<Option<(EntryRecord, Seq)>, StorageError> {
        self.assert_open()?;
        let inner = self.inner.lock().expect("memory storage");
        Ok(inner
            .state
            .entries
            .get(&id.get())
            .cloned()
            .zip(inner.state.entry_commit_seqs.get(&id.get()).copied()))
    }

    async fn entry_in_conversation(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        _context: &dyn Context,
    ) -> Result<Option<(EntryRecord, Seq)>, StorageError> {
        self.assert_open()?;
        let inner = self.inner.lock().expect("memory storage");
        // 对应 `visibleEntries(conversationId, id, id)`：只有 fork 链可见的条目才算命中。
        let visible =
            !visible_entry_ids(&inner.state, conversation_id.get(), id.get(), id.get()).is_empty();
        if !visible {
            return Ok(None);
        }
        Ok(inner
            .state
            .entries
            .get(&id.get())
            .cloned()
            .zip(inner.state.entry_commit_seqs.get(&id.get()).copied()))
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
        _context: &dyn Context,
    ) -> Result<Option<(EntryRecord, EntryId)>, StorageError> {
        self.assert_open()?;
        let inner = self.inner.lock().expect("memory storage");
        // 对应 `findLatestHeadMarker`：沿 fork 链向上，找最新的带 `head` 的条目。
        let mut current_id = conversation_id.get();
        let mut upper = at_or_before_entry_id.map_or(u64::MAX, EntryId::get);
        loop {
            if let Some(ids) = inner.state.entry_ids_by_conversation.get(&current_id) {
                for &key in ids.range(..=upper).rev() {
                    if let Some(record) = inner.state.entries.get(&key)
                        && let Some(head) = record.head
                    {
                        return Ok(Some((record.clone(), head)));
                    }
                }
            }
            let Some(conversation) = inner.state.conversations.get(&current_id) else {
                return Ok(None);
            };
            let Some(parent) = conversation.parent else {
                return Ok(None);
            };
            upper = upper.min(parent.at.get());
            current_id = parent.conversation_id.get();
        }
    }

    async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<EntryRecord, Cursor>, StorageError> {
        self.assert_open()?;
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Descending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let inner = self.inner.lock().expect("memory storage");
        let state = &inner.state;
        let mut min = query.min_entry_id.map_or(0, EntryId::get);
        let mut max = query.max_entry_id.map_or(u64::MAX, EntryId::get);
        // 游标收窄「扫描离开的那一侧」的边界（对应 `scanEntries`）。
        match (start.order, start.after) {
            (ScanOrder::Descending, Some(after)) => max = max.min(after - 1),
            (ScanOrder::Ascending, Some(after)) => min = min.max(after + 1),
            _ => {}
        }
        let ids = match start.order {
            ScanOrder::Ascending => {
                visible_entry_ids_ascending(state, query.conversation_id.get(), min, max)
            }
            ScanOrder::Descending => {
                visible_entry_ids(state, query.conversation_id.get(), min, max)
            }
        };
        let values: Vec<EntryRecord> = ids
            .into_iter()
            .filter_map(|key| state.entries.get(&key).cloned())
            .collect();
        Ok(page(values, limit, start.order, |record| record.id.get()))
    }

    async fn task(
        &self,
        id: TaskId,
        _context: &dyn Context,
    ) -> Result<Option<StoredTask>, StorageError> {
        self.assert_open()?;
        Ok(self
            .inner
            .lock()
            .expect("memory storage")
            .state
            .tasks
            .get(&id.get())
            .cloned())
    }

    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<StoredTask, Cursor>, StorageError> {
        self.assert_open()?;
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let inner = self.inner.lock().expect("memory storage");
        let state = &inner.state;
        let keys: BTreeSet<u64> = match query.status {
            Some(status) => state
                .task_ids_by_status
                .get(&status)
                .cloned()
                .unwrap_or_default(),
            None => state.tasks.keys().copied().collect(),
        };
        let values: Vec<StoredTask> = scan_keys(&keys, start)
            .into_iter()
            .filter_map(|key| state.tasks.get(&key).cloned())
            .filter(|record| {
                query
                    .conversation_id
                    .is_none_or(|id| record.conversation_id == id)
                    && query.kind.as_deref().is_none_or(|kind| record.kind == kind)
                    && query
                        .abort_requested
                        .is_none_or(|v| record.abort_requested == v)
                    && query.background.is_none_or(|v| record.background == v)
            })
            .collect();
        Ok(page(values, limit, start.order, |record| record.id.get()))
    }

    async fn submission(
        &self,
        id: SubmissionId,
        _context: &dyn Context,
    ) -> Result<Option<SubmissionRecord>, StorageError> {
        self.assert_open()?;
        Ok(self
            .inner
            .lock()
            .expect("memory storage")
            .state
            .submissions
            .get(&id.get())
            .cloned())
    }

    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<SubmissionRecord, Cursor>, StorageError> {
        self.assert_open()?;
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let inner = self.inner.lock().expect("memory storage");
        let state = &inner.state;
        let keys: BTreeSet<u64> = match query.status {
            Some(status) => state
                .submission_ids_by_status
                .get(&status)
                .cloned()
                .unwrap_or_default(),
            None => state.submissions.keys().copied().collect(),
        };
        let values: Vec<SubmissionRecord> = scan_keys(&keys, start)
            .into_iter()
            .filter_map(|key| state.submissions.get(&key).cloned())
            .filter(|record| {
                query
                    .conversation_id
                    .is_none_or(|id| record.identity().conversation_id == id)
            })
            .collect();
        Ok(page(values, limit, start.order, |record| {
            record.identity().id.get()
        }))
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        _context: &dyn Context,
    ) -> Result<Option<SubmissionRecord>, StorageError> {
        self.assert_open()?;
        let inner = self.inner.lock().expect("memory storage");
        let id = inner
            .state
            .submission_ids_by_request
            .get(&(conversation_id.get(), request_id.to_string()))
            .copied();
        Ok(id.and_then(|id| inner.state.submissions.get(&id).cloned()))
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        _context: &dyn Context,
    ) -> Result<Option<DocumentRecord>, StorageError> {
        self.assert_open()?;
        let inner = self.inner.lock().expect("memory storage");
        let key = address_key(&address.kind, &address.scope, address.key.as_deref());
        let Some(index) = inner.state.document_addresses.get(&key) else {
            return Ok(None);
        };
        let found = index
            .ids
            .iter()
            .rev()
            .filter_map(|id| inner.state.documents.get(&id.get()))
            .find(|stored| is_alive_at(&stored.record, at));
        Ok(found.map(|stored| stored.record.clone()))
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        _context: &dyn Context,
    ) -> Result<Option<StoredDocument>, StorageError> {
        self.assert_open()?;
        let inner = self.inner.lock().expect("memory storage");
        materialize_document_state(&inner.state, id, at)
    }

    async fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<DocumentRecord, Cursor>, StorageError> {
        self.assert_open()?;
        let start = scan_start(None, cursor.as_ref(), ScanOrder::Ascending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let inner = self.inner.lock().expect("memory storage");
        let state = &inner.state;
        let keys = state
            .document_ids_by_scope
            .get(&scope_key(&query.scope))
            .cloned()
            .unwrap_or_default();
        let values: Vec<DocumentRecord> = scan_keys(&keys, start)
            .into_iter()
            .filter_map(|key| state.documents.get(&key))
            .filter(|stored| {
                is_alive_at(&stored.record, query.at)
                    && query
                        .kind
                        .as_deref()
                        .is_none_or(|kind| stored.record.kind == kind)
            })
            .map(|stored| stored.record.clone())
            .collect();
        Ok(page(values, limit, start.order, |record| record.id.get()))
    }

    async fn close(&self, _context: &dyn Context) -> Result<(), StorageError> {
        self.inner.lock().expect("memory storage").closed = true;
        Ok(())
    }
}
