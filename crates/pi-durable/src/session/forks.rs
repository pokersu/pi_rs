//! 对应 `src/session/forks.ts`：分叉时选择并拷贝会话文档。

use std::collections::HashSet;

use crate::chord::context::Context;
use crate::documents::address_id;
use crate::session::SessionError;
use crate::types::{
    ConversationFork, ConversationHistory, ConversationId, Cursor, DocumentAddress,
    DocumentCopySource, DocumentCreate, DocumentId, DocumentPoint, DocumentQuery, DocumentScope,
    EntryId, Storage,
};

/// 对应 `SCAN_PAGE_SIZE`。
const SCAN_PAGE_SIZE: usize = 256;

/// 对应 `ForkPolicy`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkPolicy {
    /// 在其截断点初始化。
    AsOf,
    /// 从当前源状态初始化。
    Current,
}

impl ForkPolicy {
    /// 该策略对应的记录 `fork` 值。
    fn as_fork(self) -> ConversationFork {
        match self {
            ForkPolicy::AsOf => ConversationFork::AsOf,
            ForkPolicy::Current => ConversationFork::Current,
        }
    }
}

/// 对应 `ForkDocumentCopy`：一次分叉要创建的无定义文档拷贝。
#[derive(Debug, Clone, PartialEq)]
pub struct ForkDocumentCopy {
    /// 要创建的记录。
    pub record: DocumentCreate,
    /// 拷贝源。
    pub source: DocumentCopySource,
}

/// 对应 `prepareForkDocumentCopies`：选出一次分叉要拷贝的全部持久会话文档。
///
/// 先按父条目所在提交点取 `asOf` 策略的文档，再取父会话的 `current` 策略文档；
/// 两步共享同一份 `copiedAddresses`，因此同一目标地址被两个源选中时报错。
pub async fn prepare_fork_document_copies(
    storage: &dyn Storage,
    parent_conversation_id: ConversationId,
    at: EntryId,
    child_conversation_id: ConversationId,
    context: &dyn Context,
) -> Result<Vec<ForkDocumentCopy>, SessionError> {
    let stored = storage
        .entry_in_conversation(parent_conversation_id, at, context)
        .await?
        .ok_or_else(|| {
            SessionError::Message(format!(
                "Entry {at} is not visible from conversation {parent_conversation_id}"
            ))
        })?;
    let (entry, commit_seq) = stored;

    let mut copies = Vec::new();
    let mut copied_addresses = HashSet::new();
    collect_copies(
        storage,
        DocumentScope::Conversation {
            conversation_id: entry.conversation_id,
        },
        DocumentPoint::At(commit_seq),
        ForkPolicy::AsOf,
        child_conversation_id,
        &mut copies,
        &mut copied_addresses,
        context,
    )
    .await?;
    collect_copies(
        storage,
        DocumentScope::Conversation {
            conversation_id: parent_conversation_id,
        },
        DocumentPoint::Current,
        ForkPolicy::Current,
        child_conversation_id,
        &mut copies,
        &mut copied_addresses,
        context,
    )
    .await?;
    Ok(copies)
}

/// 对应 `collectCopies`：扫描一个作用域，选出符合策略的文档并生成拷贝。
#[allow(clippy::too_many_arguments)]
async fn collect_copies(
    storage: &dyn Storage,
    scope: DocumentScope,
    at: DocumentPoint,
    policy: ForkPolicy,
    child_conversation_id: ConversationId,
    copies: &mut Vec<ForkDocumentCopy>,
    copied_addresses: &mut HashSet<String>,
    context: &dyn Context,
) -> Result<(), SessionError> {
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = storage
            .scan_documents(
                DocumentQuery {
                    scope,
                    at,
                    kind: None,
                },
                SCAN_PAGE_SIZE,
                cursor.clone(),
                context,
            )
            .await?;
        for source in page.items {
            if !matches!(source.scope, DocumentScope::Conversation { .. }) {
                continue;
            }
            // 上游按记录顶层的 `fork` 过滤。
            if source.fork != Some(policy.as_fork()) {
                continue;
            }
            let id = DocumentId::new(storage.mint_id().await);
            // 上游：`source.history === "latest"` 保持 `latest`，否则一律 `rewindable`；
            // `fork` 直接沿用源记录。
            let history = if source.history == Some(ConversationHistory::Latest) {
                ConversationHistory::Latest
            } else {
                ConversationHistory::Rewindable
            };
            let record = DocumentCreate {
                id,
                kind: source.kind.clone(),
                key: source.key.clone(),
                history: Some(history),
                fork: source.fork,
                scope: DocumentScope::Conversation {
                    conversation_id: child_conversation_id,
                },
            };
            let copy_address = address_id(&DocumentAddress {
                kind: record.kind.clone(),
                scope: record.scope,
                key: record.key.clone(),
            });
            if !copied_addresses.insert(copy_address) {
                let member = match &record.key {
                    None => record.kind.clone(),
                    Some(key) => format!("{}/{key}", record.kind),
                };
                return Err(SessionError::Message(format!(
                    "Fork selects multiple source documents for {member}"
                )));
            }
            copies.push(ForkDocumentCopy {
                record,
                source: DocumentCopySource { id: source.id, at },
            });
        }
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(())
}
