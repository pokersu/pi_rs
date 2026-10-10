//! 对应 `src/storage/sqlite/`：SQLite 后端。
//!
//! # 与上游的差异（schema）
//!
//! 上游用**结构化列**（每个记录类型的字段各占一列）并带 125 行 migrations，以便用 SQL 索引与
//! 谓词下推。Rust 版把记录整体存为 **JSON 列**（`records(table_name, id, seq, data)`），
//! 过滤与范围扫描在 SQL 里按 `id` 完成、其余谓词在内存里判断。
//! **数据往返与语义一致**，差的是查询下推能力（记录量很大时可再优化）。
//!
//! 另：上游的 `sqlite/cloudflare.ts`（Durable Object 上的 SQLite）在 Rust 侧无对应运行时，
//! 不移植；`migrations` 也不需要（schema 由 `open()` 直接建出）。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value as JsonValue;

use super::resolve_document_copies;
use crate::chord::context::Context;
use crate::errors::StorageRejected;
use crate::storage::scan::{ScanStart, page, scan_start};
use crate::types::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress,
    DocumentContent, DocumentId, DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope,
    EntryId, EntryQuery, EntryRecord, JsonObject, Page, ScanOrder, Seq, Storage, StorageError,
    StorageWrite, StoredDocument, SubmissionId, SubmissionQuery, SubmissionRecord, TaskId,
    TaskQuery, TaskRecord,
};

type StoredTask = TaskRecord<JsonValue, JsonValue, JsonValue>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS records (
    table_name TEXT    NOT NULL,
    id         INTEGER NOT NULL,
    seq        INTEGER NOT NULL,
    data       TEXT    NOT NULL,
    PRIMARY KEY (table_name, id)
);
CREATE TABLE IF NOT EXISTS documents (
    id        INTEGER PRIMARY KEY,
    record    TEXT NOT NULL,
    revisions TEXT NOT NULL
);
";

/// 对应 `SqliteStorage`。
pub struct SqliteStorage {
    connection: Arc<Mutex<Connection>>,
    closed: Arc<AtomicBool>,
}

impl std::fmt::Debug for SqliteStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStorage").finish_non_exhaustive()
    }
}

fn sqlite_error(error: rusqlite::Error) -> StorageError {
    StorageError::Message(format!("SQLite error: {error}"))
}

fn json_error(error: serde_json::Error) -> StorageError {
    StorageError::Message(format!("SQLite JSON error: {error}"))
}

/// 取一条写入的 `(ID, 表名)`；`document.change`/`document.retire` 不占用新 ID。
/// 与 `memory::write_identity` 保持同一套语义。
fn write_identity(write: &StorageWrite) -> Option<(u64, &'static str)> {
    match write {
        StorageWrite::Conversation(record) => Some((record.id.get(), "conversation")),
        StorageWrite::Entry(record) => Some((record.id.get(), "entry")),
        StorageWrite::Task(record) => Some((record.id.get(), "task")),
        StorageWrite::Submission(record) => Some((record.identity().id.get(), "submission")),
        StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
            Some((record.id.get(), "document"))
        }
        StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => None,
    }
}

/// 查询一个全局 ID 当前归属于哪张表（records 与 documents 共享同一 ID 空间）。
fn existing_owner(connection: &Connection, id: u64) -> Result<Option<String>, StorageError> {
    if let Some(table) = connection
        .query_row(
            "SELECT table_name FROM records WHERE id = ?1",
            params![id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sqlite_error)?
    {
        return Ok(Some(table));
    }
    let in_documents: Option<i64> = connection
        .query_row(
            "SELECT id FROM documents WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    Ok(in_documents.map(|_| "document".to_string()))
}

fn with_connection<T>(
    connection: &Arc<Mutex<Connection>>,
    operation: impl FnOnce(&mut Connection) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    let mut guard = connection.lock().expect("sqlite connection");
    operation(&mut guard)
}

impl SqliteStorage {
    /// 打开（或创建）一个 SQLite 文件。
    pub fn open(path: &str) -> Result<Self, StorageError> {
        let connection = Connection::open(path).map_err(sqlite_error)?;
        Self::from_connection(connection)
    }

    /// 打开一个内存库（测试与临时会话用）。
    pub fn open_in_memory() -> Result<Self, StorageError> {
        let connection = Connection::open_in_memory().map_err(sqlite_error)?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, StorageError> {
        connection.execute_batch(SCHEMA).map_err(sqlite_error)?;
        for key in ["next_seq", "next_id"] {
            connection
                .execute(
                    "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
                    params![key, if key == "next_id" { "2" } else { "1" }],
                )
                .map_err(sqlite_error)?;
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 对应上游 `assertOpen()`：关闭之后的任何操作都被拒绝。
    fn assert_open(&self) -> Result<(), StorageError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(StorageError::Closed);
        }
        Ok(())
    }

    /// 取连接句柄（顺带做一次开启检查）。
    fn connection_for(&self) -> Result<Arc<Mutex<Connection>>, StorageError> {
        self.assert_open()?;
        Ok(Arc::clone(&self.connection))
    }

    fn read_counter(connection: &Connection, key: &str) -> Result<u64, StorageError> {
        connection
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .map_err(sqlite_error)?
            .parse::<u64>()
            .map_err(|_| StorageError::Message(format!("invalid {key} counter")))
    }

    fn bump_counter(connection: &Connection, key: &str) -> Result<u64, StorageError> {
        let value = Self::read_counter(connection, key)?;
        connection
            .execute(
                "UPDATE meta SET value = ?2 WHERE key = ?1",
                params![key, (value + 1).to_string()],
            )
            .map_err(sqlite_error)?;
        Ok(value)
    }

    /// 应用一批写入（与 memory 后端相同的语义；在单个事务里完成）。
    fn apply(
        connection: &mut Connection,
        writes: &[StorageWrite],
        seq: Seq,
    ) -> Result<(), StorageError> {
        let transaction = connection.transaction().map_err(sqlite_error)?;

        // 展开 document.copy（物化源 → create；校验源本批未改、源可读、记录一致）。
        let resolved = resolve_document_copies(writes, |id, at| {
            materialize_stored_document(&transaction, id, at)
        })?;
        let writes = resolved.as_slice();

        // 全局 ID 归属：与 memory 后端同一套规则（不可变创建跨表唯一，task/submission 同表可更新）。
        let mut claimed: BTreeMap<u64, &'static str> = BTreeMap::new();
        for write in writes {
            let Some((id, table)) = write_identity(write) else {
                continue;
            };
            let existing = existing_owner(&transaction, id)?;
            let earlier = claimed.get(&id).copied();
            match table {
                // conversation / entry / document 是「不可变创建」：任何已占用都拒绝。
                "conversation" | "entry" | "document" => {
                    if let Some(existing) = existing {
                        return Err(StorageError::Rejected(StorageRejected::new(format!(
                            "ID {id} already belongs to {existing}"
                        ))));
                    }
                    if earlier.is_some() {
                        return Err(StorageError::Rejected(StorageRejected::new(format!(
                            "ID {id} is written more than once"
                        ))));
                    }
                }
                // task / submission 是「最新记录替换」：同表可更新，跨表仍拒绝。
                "task" | "submission" => {
                    if let Some(existing) = existing
                        && existing != table
                    {
                        return Err(StorageError::Rejected(StorageRejected::new(format!(
                            "ID {id} already belongs to {existing}"
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
                _ => {}
            }
            claimed.insert(id, table);
        }

        for write in writes {
            match write {
                StorageWrite::Conversation(record) => {
                    put_record(&transaction, "conversation", record.id.get(), seq, record)?;
                }
                StorageWrite::Entry(record) => {
                    put_record(&transaction, "entry", record.id.get(), seq, record)?;
                }
                StorageWrite::Task(record) => {
                    put_record(&transaction, "task", record.id.get(), seq, record)?;
                }
                StorageWrite::Submission(record) => {
                    put_record(
                        &transaction,
                        "submission",
                        record.identity().id.get(),
                        seq,
                        record,
                    )?;
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
                    insert_document(
                        &transaction,
                        &doc,
                        &[StoredRevision {
                            seq: seq.get(),
                            content: DocumentContent::Base(content.clone()),
                        }],
                    )?;
                }
                StorageWrite::DocumentChange { id, content } => {
                    let (record, mut revisions) = read_document(&transaction, *id)?;
                    revisions.push(StoredRevision {
                        seq: seq.get(),
                        content: content.clone(),
                    });
                    insert_document(&transaction, &record, &revisions)?;
                }
                StorageWrite::DocumentRetire { id } => {
                    let (mut record, revisions) = read_document(&transaction, *id)?;
                    record.retired_at = Some(seq);
                    insert_document(&transaction, &record, &revisions)?;
                }
                StorageWrite::DocumentCopy { .. } => {
                    unreachable!("document.copy is resolved to document.create before apply")
                }
            }
        }

        transaction.commit().map_err(sqlite_error)
    }
}

fn put_record<T: serde::Serialize>(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    id: u64,
    seq: Seq,
    value: &T,
) -> Result<(), StorageError> {
    let data = serde_json::to_string(value).map_err(json_error)?;
    transaction
        .execute(
            "INSERT INTO records (table_name, id, seq, data) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(table_name, id) DO UPDATE SET seq = excluded.seq, data = excluded.data",
            params![table, id, seq.get(), data],
        )
        .map_err(sqlite_error)?;
    Ok(())
}

fn read_record<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    table: &str,
    id: u64,
) -> Result<Option<(T, Seq)>, StorageError> {
    let row: Option<(String, u64)> = connection
        .query_row(
            "SELECT data, seq FROM records WHERE table_name = ?1 AND id = ?2",
            params![table, id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    match row {
        None => Ok(None),
        Some((data, seq)) => Ok(Some((
            serde_json::from_str(&data).map_err(json_error)?,
            Seq::new(seq),
        ))),
    }
}

/// 对应 `visibleEntries` 的会话链：沿 fork 链向上，返回每个会话的 `(conversation_id, upper)` 段，
/// 其中 `upper` 是该段可见条目的 ID 上界（被父会话的 `parent.at` 截断）。
fn load_fork_segments(
    connection: &Connection,
    conversation_id: u64,
    max: u64,
) -> Result<Vec<(u64, u64)>, StorageError> {
    let mut segments = Vec::new();
    let mut current_id = conversation_id;
    let mut upper = max;
    loop {
        segments.push((current_id, upper));
        let Some((record, _)) =
            read_record::<ConversationRecord>(connection, "conversation", current_id)?
        else {
            break;
        };
        let Some(parent) = record.parent else {
            break;
        };
        upper = upper.min(parent.at.get());
        current_id = parent.conversation_id.get();
    }
    Ok(segments)
}

fn scan_records<T: serde::de::DeserializeOwned + Clone>(
    connection: &Connection,
    table: &str,
    start: ScanStart,
    limit: usize,
    filter: impl Fn(&T) -> bool,
    id_of: impl Fn(&T) -> u64,
) -> Result<Page<T, Cursor>, StorageError> {
    let sql = match start.order {
        ScanOrder::Ascending => {
            "SELECT id, data FROM records WHERE table_name = ?1 AND id > ?2 ORDER BY id ASC"
        }
        ScanOrder::Descending => {
            "SELECT id, data FROM records WHERE table_name = ?1 AND id < ?2 ORDER BY id DESC"
        }
    };
    let after = match start.order {
        ScanOrder::Ascending => start.after.unwrap_or(0) as i64,
        ScanOrder::Descending => start.after.unwrap_or(i64::MAX as u64) as i64,
    };

    let mut statement = connection.prepare(sql).map_err(sqlite_error)?;
    let rows = statement
        .query_map(params![table, after], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sqlite_error)?;

    let mut values = Vec::new();
    for row in rows {
        let (_id, data) = row.map_err(sqlite_error)?;
        let value: T = serde_json::from_str(&data).map_err(json_error)?;
        if filter(&value) {
            values.push(value);
        }
    }

    Ok(page(values, limit, start.order, id_of))
}

/// 对应文档在给定时间点是否存活。
fn alive_at(record: &DocumentRecord, at: DocumentPoint) -> bool {
    match at {
        DocumentPoint::Current => record.retired_at.is_none(),
        DocumentPoint::At(seq) => {
            record.created_at <= seq && record.retired_at.is_none_or(|retired| seq < retired)
        }
    }
}

/// 对应 `isCurrentOnly`：不保留历史内容的记录其图像只存活在“当前”。
fn is_current_only(record: &DocumentRecord) -> bool {
    !matches!(record.scope, DocumentScope::Conversation { .. })
        || record.history == Some(crate::types::ConversationHistory::Latest)
}

/// 一条已存文档修订：其提交序号决定它在哪个点上可见。
///
/// 上游把修订写在同一个有序数组里（每条自带 `seq`）；Rust 的 JSON 列把两者一同序列化。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct StoredRevision {
    seq: u64,
    content: DocumentContent,
}

fn insert_document(
    transaction: &rusqlite::Transaction<'_>,
    record: &DocumentRecord,
    revisions: &[StoredRevision],
) -> Result<(), StorageError> {
    let record_json = serde_json::to_string(record).map_err(json_error)?;
    let revisions_json = serde_json::to_string(revisions).map_err(json_error)?;
    transaction
        .execute(
            "INSERT INTO documents (id, record, revisions) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET record = excluded.record, revisions = excluded.revisions",
            params![record.id.get(), record_json, revisions_json],
        )
        .map_err(sqlite_error)?;
    Ok(())
}

fn read_document(
    connection: &Connection,
    id: DocumentId,
) -> Result<(DocumentRecord, Vec<StoredRevision>), StorageError> {
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT record, revisions FROM documents WHERE id = ?1",
            params![id.get()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((record, revisions)) = row else {
        return Err(StorageError::Message(format!(
            "Document {} does not exist",
            id.get()
        )));
    };
    Ok((
        serde_json::from_str(&record).map_err(json_error)?,
        serde_json::from_str(&revisions).map_err(json_error)?,
    ))
}

/// 物化文档到选定点（与 memory 后端同一套「base + delta 重放」语义）。
fn materialize_document(
    record: &DocumentRecord,
    revisions: &[StoredRevision],
    at: DocumentPoint,
) -> Result<Option<(JsonObject, u32, usize)>, StorageError> {
    if !alive_at(record, at) {
        return Ok(None);
    }
    // 非当前点的读取要求记录保留历史内容（对应上游 `isCurrentOnly` 检查）。
    if !matches!(at, DocumentPoint::Current) && is_current_only(record) {
        return Err(StorageError::Message(format!(
            "Document {} does not retain historical content",
            record.id.get()
        )));
    }

    // 对应上游 `at === "current" ? revisions : revisions.filter(r => r.seq <= at)`。
    let point_revisions: Vec<&StoredRevision> = match at {
        DocumentPoint::Current => revisions.iter().collect(),
        DocumentPoint::At(seq) => revisions
            .iter()
            .take_while(|revision| revision.seq <= seq.get())
            .collect(),
    };

    let mut base = None;
    for (index, revision) in point_revisions.iter().enumerate() {
        if matches!(revision.content, DocumentContent::Base(_)) {
            base = Some(index);
        }
    }
    let Some(base_index) = base else {
        return Err(StorageError::Message(format!(
            "Document {} is missing a required base",
            record.id.get()
        )));
    };
    let DocumentContent::Base(base_value) = &point_revisions[base_index].content else {
        unreachable!("base_index points at a base")
    };

    let mut ops_batches: Vec<Vec<crate::chord::delta::Op>> = Vec::new();
    for revision in &point_revisions[base_index + 1..] {
        match &revision.content {
            DocumentContent::Delta { version, ops } => {
                if *version != base_value.version {
                    return Err(StorageError::Message(format!(
                        "Document {} crosses a stored version boundary without a base",
                        record.id.get()
                    )));
                }
                ops_batches.push(ops.clone());
            }
            DocumentContent::Base(_) => break,
        }
    }

    let value = crate::chord::delta::apply_immutable_batches(
        Some(JsonValue::Object(base_value.value.clone())),
        ops_batches.iter().map(Vec::as_slice),
    )
    .map_err(|error| StorageError::Message(error.to_string()))?;

    match value {
        JsonValue::Object(map) => Ok(Some((
            map,
            base_value.version,
            point_revisions.len() - base_index - 1,
        ))),
        _ => Err(StorageError::Message(
            "document root must stay an object".to_string(),
        )),
    }
}

/// 物化一个文档化身到选定点（对应上游 `document(id, at)` 的存储侧物化）。
///
/// 与 [`materialize_document`] 的区别：源不存在时返回 `Ok(None)`（而非错误），
/// 供 [`resolve_document_copies`] 把「源不可读」统一映射为「copy 被拒绝」。
fn materialize_stored_document(
    connection: &Connection,
    id: DocumentId,
    at: DocumentPoint,
) -> Result<Option<StoredDocument>, StorageError> {
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT record, revisions FROM documents WHERE id = ?1",
            params![id.get()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((record_data, revisions_data)) = row else {
        return Ok(None);
    };
    let record: DocumentRecord = serde_json::from_str(&record_data).map_err(json_error)?;
    let revisions: Vec<StoredRevision> =
        serde_json::from_str(&revisions_data).map_err(json_error)?;
    let materialized = materialize_document(&record, &revisions, at)?;
    Ok(materialized.map(|(value, version, deltas)| StoredDocument {
        record,
        version,
        value,
        deltas_since_base: deltas,
    }))
}

#[async_trait::async_trait]
impl Storage for SqliteStorage {
    async fn commit(
        &self,
        writes: &[StorageWrite],
        _context: &dyn Context,
    ) -> Result<Seq, StorageError> {
        let connection = self.connection_for()?;
        let writes = writes.to_vec();
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, move |connection| {
                let seq = Seq::new(SqliteStorage::read_counter(connection, "next_seq")?);
                SqliteStorage::apply(connection, &writes, seq)?;
                SqliteStorage::bump_counter(connection, "next_seq")?;
                Ok(seq)
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn mint_id(&self) -> u64 {
        // 上游 `mintId` 会 `assertOpen()` 并抛异常；`Storage::mint_id` 的签名无法返回错误，
        // 这里用 panic 保持“关闭后不可用”的语义。
        assert!(self.assert_open().is_ok(), "SqliteStorage is closed");
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                SqliteStorage::bump_counter(connection, "next_id")
            })
        })
        .await
        .expect("sqlite mint_id task panicked")
        .expect("sqlite mint_id failed")
    }

    async fn conversation(
        &self,
        id: ConversationId,
        _context: &dyn Context,
    ) -> Result<Option<ConversationRecord>, StorageError> {
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                read_record::<ConversationRecord>(connection, "conversation", id.get())
                    .map(|found| found.map(|(record, _)| record))
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<ConversationRecord, Cursor>, StorageError> {
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                scan_records::<ConversationRecord>(
                    connection,
                    "conversation",
                    start,
                    limit,
                    |record| {
                        query
                            .owner_conversation_id
                            .is_none_or(|id| record.owner.is_some_and(|o| o.conversation_id == id))
                            && query
                                .owner_task_id
                                .is_none_or(|id| record.owner.is_some_and(|o| o.task_id == id))
                    },
                    |record| record.id.get(),
                )
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn entry(
        &self,
        id: EntryId,
        _context: &dyn Context,
    ) -> Result<Option<(EntryRecord, Seq)>, StorageError> {
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                read_record::<EntryRecord>(connection, "entry", id.get())
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn entry_in_conversation(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        _context: &dyn Context,
    ) -> Result<Option<(EntryRecord, Seq)>, StorageError> {
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                let Some((record, seq)) =
                    read_record::<EntryRecord>(connection, "entry", id.get())?
                else {
                    return Ok(None);
                };
                // 对应 `visibleEntries(conversationId, id, id)`：只有 fork 链可见的条目才算命中。
                let segments = load_fork_segments(connection, conversation_id.get(), id.get())?;
                let visible = segments.iter().any(|(cid, upper)| {
                    record.conversation_id.get() == *cid && record.id.get() <= *upper
                });
                Ok(visible.then_some((record, seq)))
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
        _context: &dyn Context,
    ) -> Result<Option<(EntryRecord, EntryId)>, StorageError> {
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                let upper_u64 = at_or_before_entry_id.map_or(u64::MAX, EntryId::get);
                let upper_i64 = at_or_before_entry_id.map_or(i64::MAX, |id| id.get() as i64);
                let segments = load_fork_segments(connection, conversation_id.get(), upper_u64)?;
                let mut statement = connection
                    .prepare(
                        "SELECT data FROM records WHERE table_name = 'entry' AND id <= ?1
                         ORDER BY id DESC",
                    )
                    .map_err(sqlite_error)?;
                let rows = statement
                    .query_map(params![upper_i64], |row| row.get::<_, String>(0))
                    .map_err(sqlite_error)?;
                for row in rows {
                    let data = row.map_err(sqlite_error)?;
                    let record: EntryRecord = serde_json::from_str(&data).map_err(json_error)?;
                    let visible = segments.iter().any(|(cid, seg_upper)| {
                        record.conversation_id.get() == *cid && record.id.get() <= *seg_upper
                    });
                    if visible && let Some(head) = record.head {
                        return Ok(Some((record, head)));
                    }
                }
                Ok(None)
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<EntryRecord, Cursor>, StorageError> {
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Descending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let min = query.min_entry_id.map_or(0, EntryId::get);
        let max = query.max_entry_id.map_or(u64::MAX, EntryId::get);
        let conversation_id = query.conversation_id.get();
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                // fork 链可见性：每条可见段由（会话，上界）描述，父会话的 `parent.at` 截断。
                let segments = load_fork_segments(connection, conversation_id, max)?;
                scan_records::<EntryRecord>(
                    connection,
                    "entry",
                    start,
                    limit,
                    move |record| {
                        segments.iter().any(|(cid, upper)| {
                            record.conversation_id.get() == *cid
                                && record.id.get() >= min
                                && record.id.get() <= *upper
                        })
                    },
                    |record| record.id.get(),
                )
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn task(
        &self,
        id: TaskId,
        _context: &dyn Context,
    ) -> Result<Option<StoredTask>, StorageError> {
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                read_record::<StoredTask>(connection, "task", id.get())
                    .map(|found| found.map(|(record, _)| record))
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<StoredTask, Cursor>, StorageError> {
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                scan_records::<StoredTask>(
                    connection,
                    "task",
                    start,
                    limit,
                    |record| {
                        query
                            .conversation_id
                            .is_none_or(|id| record.conversation_id == id)
                            && query.kind.as_deref().is_none_or(|kind| record.kind == kind)
                            && query
                                .status
                                .is_none_or(|status| record.state.status() == status)
                            && query
                                .abort_requested
                                .is_none_or(|value| record.abort_requested == value)
                            && query
                                .background
                                .is_none_or(|value| record.background == value)
                    },
                    |record| record.id.get(),
                )
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn submission(
        &self,
        id: SubmissionId,
        _context: &dyn Context,
    ) -> Result<Option<SubmissionRecord>, StorageError> {
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                read_record::<SubmissionRecord>(connection, "submission", id.get())
                    .map(|found| found.map(|(record, _)| record))
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<SubmissionRecord, Cursor>, StorageError> {
        let start = scan_start(query.order, cursor.as_ref(), ScanOrder::Ascending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                scan_records::<SubmissionRecord>(
                    connection,
                    "submission",
                    start,
                    limit,
                    |record| {
                        query
                            .conversation_id
                            .is_none_or(|id| record.identity().conversation_id == id)
                            && query.status.is_none_or(|status| record.status() == status)
                    },
                    |record| record.identity().id.get(),
                )
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        _context: &dyn Context,
    ) -> Result<Option<SubmissionRecord>, StorageError> {
        let connection = self.connection_for()?;
        let request_id = request_id.to_string();
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                let mut statement = connection
                    .prepare("SELECT data FROM records WHERE table_name = 'submission' ORDER BY id")
                    .map_err(sqlite_error)?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(sqlite_error)?;
                for row in rows {
                    let data = row.map_err(sqlite_error)?;
                    let record: SubmissionRecord =
                        serde_json::from_str(&data).map_err(json_error)?;
                    let identity = record.identity();
                    if identity.conversation_id == conversation_id
                        && identity.request_id.as_deref() == Some(request_id.as_str())
                    {
                        return Ok(Some(record));
                    }
                }
                Ok(None)
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        _context: &dyn Context,
    ) -> Result<Option<DocumentRecord>, StorageError> {
        let connection = self.connection_for()?;
        let address = address.clone();
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                let mut statement = connection
                    .prepare("SELECT record FROM documents ORDER BY id")
                    .map_err(sqlite_error)?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(sqlite_error)?;
                let mut found = None;
                for row in rows {
                    let data = row.map_err(sqlite_error)?;
                    let record: DocumentRecord = serde_json::from_str(&data).map_err(json_error)?;
                    if record.kind != address.kind
                        || record.key != address.key
                        || !same_scope(&record.scope, &address.scope)
                    {
                        continue;
                    }
                    if alive_at(&record, at) {
                        found = Some(record);
                    }
                }
                Ok(found)
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        _context: &dyn Context,
    ) -> Result<Option<StoredDocument>, StorageError> {
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                let (record, revisions) = read_document(connection, id)?;
                let materialized = materialize_document(&record, &revisions, at)?;
                Ok(materialized.map(|(value, version, deltas)| StoredDocument {
                    record,
                    version,
                    value,
                    deltas_since_base: deltas,
                }))
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
        _context: &dyn Context,
    ) -> Result<Page<DocumentRecord, Cursor>, StorageError> {
        let start = scan_start(None, cursor.as_ref(), ScanOrder::Ascending)
            .map_err(|error| StorageError::Message(error.to_string()))?;
        let connection = self.connection_for()?;
        tokio::task::spawn_blocking(move || {
            with_connection(&connection, |connection| {
                let sql = match start.order {
                    ScanOrder::Ascending => "SELECT id, record FROM documents ORDER BY id ASC",
                    ScanOrder::Descending => "SELECT id, record FROM documents ORDER BY id DESC",
                };
                let mut statement = connection.prepare(sql).map_err(sqlite_error)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(sqlite_error)?;
                let mut values = Vec::new();
                for row in rows {
                    let (id, data) = row.map_err(sqlite_error)?;
                    let id = id as u64;
                    // 游标延续其创建时的顺序：升序取 `id > after`，降序取 `id < after`。
                    let after_ok = match start.order {
                        ScanOrder::Ascending => start.after.is_none_or(|after| id > after),
                        ScanOrder::Descending => start.after.is_none_or(|after| id < after),
                    };
                    if !after_ok {
                        continue;
                    }
                    let record: DocumentRecord = serde_json::from_str(&data).map_err(json_error)?;
                    if !same_scope(&record.scope, &query.scope) {
                        continue;
                    }
                    if query
                        .kind
                        .as_deref()
                        .is_some_and(|kind| record.kind != kind)
                    {
                        continue;
                    }
                    if alive_at(&record, query.at) {
                        values.push(record);
                    }
                }
                Ok(page(values, limit, start.order, |record| record.id.get()))
            })
        })
        .await
        .map_err(|error| StorageError::Message(error.to_string()))?
    }

    async fn close(&self, _context: &dyn Context) -> Result<(), StorageError> {
        // rusqlite 的 `Connection::close` 会消费连接，无法从 `Arc<Mutex<_>>` 中移出；连接在
        // 最后一个 `Arc` 释放时关闭。这里只置关闭标记（对应上游 `close()` 的幂等语义：
        // 重复调用同样返回成功）。
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn same_scope(left: &DocumentScope, right: &DocumentScope) -> bool {
    match (left, right) {
        (DocumentScope::Session, DocumentScope::Session) => true,
        (DocumentScope::Task { task_id: left }, DocumentScope::Task { task_id: right }) => {
            left == right
        }
        (
            DocumentScope::Conversation {
                conversation_id: left,
                ..
            },
            DocumentScope::Conversation {
                conversation_id: right,
                ..
            },
        ) => left == right,
        _ => false,
    }
}
