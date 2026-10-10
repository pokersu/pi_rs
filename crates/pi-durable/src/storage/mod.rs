//! 对应 `src/storage/`：会话记录的持久化后端。
//!
//! 三个后端共享 [`Storage`](crate::types::Storage) 契约与 [`scan`] 的扫描起点/游标/分页：
//! [`memory`]（参考实现）、[`jsonl`]（两阶段写 + 重放）、[`sqlite`]（SQL 实现）。

pub mod jsonl;
pub mod memory;
pub mod scan;
pub mod sqlite;

pub use jsonl::{JsonlStorage, JsonlStorageOptions};
pub use memory::MemoryStorage;
pub use scan::{ScanError, ScanStart, next_cursor, scan_start};
pub use sqlite::SqliteStorage;

use std::collections::BTreeSet;

use crate::errors::StorageRejected;
use crate::types::{
    DocumentBase, DocumentCopySource, DocumentCreate, DocumentId, DocumentPoint, DocumentScope,
    StorageError, StorageWrite, StoredDocument,
};

/// 对应上游 `MemoryStorage.resolveDocumentCopies`：把 `document.copy` 展开为 `document.create`。
///
/// 上游在 `commit` 的校验阶段（`prepareCommit`）先把 copy 物化源文档、校验一致性，再转成 create；
/// 三个后端共用这一套逻辑，各自传入「物化源文档」的闭包（返回 `None` 表示源不可读）。
///
/// 三项校验（按上游顺序）：
/// 1. 源在本批被改 → `Fork source document {id} is changed in the copy batch`
/// 2. 源不可读 → `Fork source document {id} cannot be read`
/// 3. 记录不一致（scope 非会话 / kind / key / history / fork）→ `Fork source document {id} does not match the copied record`
///
/// 非 `StorageRejected` 的失败统一包装为 `Document copy {id} was rejected`（上游 `{ cause }` 语义由错误链表达）。
pub(crate) fn resolve_document_copies(
    writes: &[StorageWrite],
    mut materialize: impl FnMut(
        DocumentId,
        DocumentPoint,
    ) -> Result<Option<StoredDocument>, StorageError>,
) -> Result<Vec<StorageWrite>, StorageError> {
    if !writes
        .iter()
        .any(|write| matches!(write, StorageWrite::DocumentCopy { .. }))
    {
        return Ok(writes.to_vec());
    }
    // 本批被改的文档 ID（create/copy 的 record.id，change/retire 的 id）。
    let mut changed: BTreeSet<u64> = BTreeSet::new();
    for write in writes {
        match write {
            StorageWrite::DocumentCreate { record, .. }
            | StorageWrite::DocumentCopy { record, .. } => {
                changed.insert(record.id.get());
            }
            StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => {
                changed.insert(id.get());
            }
            _ => {}
        }
    }
    let mut resolved = Vec::with_capacity(writes.len());
    for write in writes {
        let StorageWrite::DocumentCopy { record, source } = write else {
            resolved.push(write.clone());
            continue;
        };
        let create = match resolve_one_copy(record, *source, &changed, &mut materialize) {
            Ok(create) => create,
            Err(StorageError::Rejected(rejection)) => {
                return Err(StorageError::Rejected(rejection));
            }
            Err(_cause) => {
                return Err(StorageError::Rejected(StorageRejected::new(format!(
                    "Document copy {} was rejected",
                    record.id.get()
                ))));
            }
        };
        resolved.push(create);
    }
    Ok(resolved)
}

fn resolve_one_copy(
    record: &DocumentCreate,
    source: DocumentCopySource,
    changed: &BTreeSet<u64>,
    materialize: &mut impl FnMut(
        DocumentId,
        DocumentPoint,
    ) -> Result<Option<StoredDocument>, StorageError>,
) -> Result<StorageWrite, StorageError> {
    if changed.contains(&source.id.get()) {
        return Err(StorageError::Message(format!(
            "Fork source document {} is changed in the copy batch",
            source.id.get()
        )));
    }
    let stored = materialize(source.id, source.at)?.ok_or_else(|| {
        StorageError::Message(format!(
            "Fork source document {} cannot be read",
            source.id.get()
        ))
    })?;
    if !matches!(stored.record.scope, DocumentScope::Conversation { .. })
        || !matches!(record.scope, DocumentScope::Conversation { .. })
        || stored.record.kind != record.kind
        || stored.record.key != record.key
        || stored.record.history != record.history
        || stored.record.fork != record.fork
    {
        return Err(StorageError::Message(format!(
            "Fork source document {} does not match the copied record",
            source.id.get()
        )));
    }
    Ok(StorageWrite::DocumentCreate {
        record: record.clone(),
        content: DocumentBase {
            version: stored.version,
            value: stored.value,
        },
    })
}
