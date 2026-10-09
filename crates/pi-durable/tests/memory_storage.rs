//! `storage::memory`（对应 `src/storage/memory.ts`）的集成测试。

use pi_durable::chord::context::BACKGROUND_CONTEXT;
use pi_durable::chord::delta::Op;
use pi_durable::storage::MemoryStorage;
use pi_durable::types::{
    ConversationFork, ConversationHistory, ConversationId, ConversationOwnership,
    ConversationQuery, ConversationRecord, DocumentAddress, DocumentBase, DocumentContent,
    DocumentCreate, DocumentId, DocumentPoint, DocumentScope, EntryDraft, EntryId, EntryQuery,
    EntryRecord, JsonObject, Storage, StorageError, StorageWrite, TaskOutcome, TaskRecord,
    TaskState, TaskStatus,
};
use serde_json::json;

fn context() -> &'static dyn pi_durable::chord::context::Context {
    BACKGROUND_CONTEXT.as_ref()
}

fn conversation(id: u64) -> ConversationRecord {
    ConversationRecord {
        id: ConversationId::new(id),
        parent: None,
        owner: None,
    }
}

fn entry(id: u64, conversation_id: u64, kind: &str) -> EntryRecord {
    EntryRecord {
        id: EntryId::new(id),
        conversation_id: ConversationId::new(conversation_id),
        kind: kind.to_string(),
        model: None,
        data: None,
        head: None,
        edits: None,
        by_task_id: None,
    }
}

fn session_doc_scope() -> DocumentScope {
    DocumentScope::Session
}

#[tokio::test]
async fn commit_persists_records_and_advances_sequence() {
    let storage = MemoryStorage::new();

    let seq = storage
        .commit(
            &[
                StorageWrite::Conversation(conversation(1)),
                StorageWrite::Entry(entry(101, 1, "pi.user")),
            ],
            context(),
        )
        .await
        .unwrap();
    assert_eq!(seq.get(), 1);

    let next = storage
        .commit(
            &[StorageWrite::Entry(entry(2, 1, "pi.assistant"))],
            context(),
        )
        .await
        .unwrap();
    assert_eq!(next.get(), 2, "序号应严格递增");

    let (record, commit_seq) = storage
        .entry(EntryId::new(101), context())
        .await
        .unwrap()
        .expect("entry");
    assert_eq!(record.kind, "pi.user");
    assert_eq!(commit_seq.get(), 1);
}

#[tokio::test]
async fn duplicate_id_across_tables_is_rejected_without_effect() {
    let storage = MemoryStorage::new();
    storage
        .commit(&[StorageWrite::Conversation(conversation(7))], context())
        .await
        .unwrap();

    // 同一个 ID 出现在另一张表 → 整批拒绝。
    let error = storage
        .commit(&[StorageWrite::Entry(entry(7, 7, "pi.user"))], context())
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Rejected(_)), "{error:?}");

    // 拒绝后不应留下任何痕迹。
    assert!(
        storage
            .entry(EntryId::new(7), context())
            .await
            .unwrap()
            .is_none()
    );
    let seq = storage
        .commit(&[StorageWrite::Entry(entry(8, 7, "pi.user"))], context())
        .await
        .unwrap();
    assert_eq!(seq.get(), 2, "被拒的提交不消耗序号");
}

#[tokio::test]
async fn scan_entries_respects_bounds_and_order() {
    let storage = MemoryStorage::new();
    storage
        .commit(
            &[
                StorageWrite::Conversation(conversation(1)),
                StorageWrite::Entry(entry(101, 1, "pi.user")),
                StorageWrite::Entry(entry(102, 1, "pi.assistant")),
                StorageWrite::Entry(entry(103, 1, "pi.tool-result")),
            ],
            context(),
        )
        .await
        .unwrap();

    // 默认降序（最新优先）。
    let page = storage
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::new(1),
                min_entry_id: None,
                max_entry_id: None,
                order: None,
            },
            10,
            None,
            context(),
        )
        .await
        .unwrap();
    let ids: Vec<u64> = page.items.iter().map(|record| record.id.get()).collect();
    assert_eq!(ids, vec![103, 102, 101]);

    // 升序 + 上界。
    let page = storage
        .scan_entries(
            EntryQuery {
                conversation_id: ConversationId::new(1),
                min_entry_id: None,
                max_entry_id: Some(EntryId::new(102)),
                order: Some(pi_durable::types::ScanOrder::Ascending),
            },
            10,
            None,
            context(),
        )
        .await
        .unwrap();
    let ids: Vec<u64> = page.items.iter().map(|record| record.id.get()).collect();
    assert_eq!(ids, vec![101, 102]);
}

#[tokio::test]
async fn scan_pagination_returns_cursor_that_continues_the_order() {
    let storage = MemoryStorage::new();
    storage
        .commit(
            &[
                StorageWrite::Conversation(conversation(1)),
                StorageWrite::Entry(entry(101, 1, "k")),
                StorageWrite::Entry(entry(102, 1, "k")),
                StorageWrite::Entry(entry(103, 1, "k")),
            ],
            context(),
        )
        .await
        .unwrap();

    let query = EntryQuery {
        conversation_id: ConversationId::new(1),
        min_entry_id: None,
        max_entry_id: None,
        order: Some(pi_durable::types::ScanOrder::Ascending),
    };
    let first = storage
        .scan_entries(query, 2, None, context())
        .await
        .unwrap();
    let ids: Vec<u64> = first.items.iter().map(|record| record.id.get()).collect();
    assert_eq!(ids, vec![101, 102]);
    let cursor = first.next.expect("page 1 应有续扫游标");

    let second = storage
        .scan_entries(query, 2, Some(cursor), context())
        .await
        .unwrap();
    let ids: Vec<u64> = second.items.iter().map(|record| record.id.get()).collect();
    assert_eq!(ids, vec![103]);
    assert!(second.next.is_none());
}

#[tokio::test]
async fn mint_id_never_reuses_and_starts_after_root() {
    let storage = MemoryStorage::new();
    assert_eq!(storage.mint_id().await, 2);
    assert_eq!(storage.mint_id().await, 3);
}

#[tokio::test]
async fn scan_conversations_filters_by_owner() {
    let storage = MemoryStorage::new();
    let owned = ConversationRecord {
        id: ConversationId::new(5),
        parent: None,
        owner: Some(pi_durable::types::ConversationOwner {
            conversation_id: ConversationId::new(1),
            task_id: pi_durable::types::TaskId::new(9),
        }),
    };
    storage
        .commit(
            &[
                StorageWrite::Conversation(conversation(1)),
                StorageWrite::Conversation(owned),
            ],
            context(),
        )
        .await
        .unwrap();

    let page = storage
        .scan_conversations(
            ConversationQuery {
                owner_conversation_id: None,
                owner_task_id: Some(pi_durable::types::TaskId::new(9)),
                order: None,
            },
            10,
            None,
            context(),
        )
        .await
        .unwrap();
    let ids: Vec<u64> = page.items.iter().map(|record| record.id.get()).collect();
    assert_eq!(ids, vec![5]);
}

#[tokio::test]
async fn document_create_change_and_materialize() {
    let storage = MemoryStorage::new();
    let mut value = JsonObject::new();
    value.insert("count".to_string(), json!(0));

    storage
        .commit(
            &[StorageWrite::DocumentCreate {
                record: DocumentCreate {
                    id: DocumentId::new(1),
                    kind: "demo.session".to_string(),
                    key: None,
                    history: None,
                    fork: None,
                    scope: session_doc_scope(),
                },
                content: DocumentBase {
                    version: 1,
                    value: value.clone(),
                },
            }],
            context(),
        )
        .await
        .unwrap();

    let address = DocumentAddress {
        kind: "demo.session".to_string(),
        scope: session_doc_scope(),
        key: None,
    };
    assert!(
        storage
            .find_document(&address, DocumentPoint::Current, context())
            .await
            .unwrap()
            .is_some()
    );

    // 增量更新。
    storage
        .commit(
            &[StorageWrite::DocumentChange {
                id: DocumentId::new(1),
                content: DocumentContent::Delta {
                    version: 1,
                    ops: vec![Op::Set(
                        vec![pi_durable::chord::delta::PathSegment::Key(
                            "count".to_string(),
                        )],
                        json!(1),
                    )],
                },
            }],
            context(),
        )
        .await
        .unwrap();

    let stored = storage
        .document(DocumentId::new(1), DocumentPoint::Current, context())
        .await
        .unwrap()
        .expect("document");
    assert_eq!(stored.value.get("count"), Some(&json!(1)));
    assert_eq!(stored.deltas_since_base, 1);

    // 退役后当前点不再可见。
    storage
        .commit(
            &[StorageWrite::DocumentRetire {
                id: DocumentId::new(1),
            }],
            context(),
        )
        .await
        .unwrap();
    assert!(
        storage
            .find_document(&address, DocumentPoint::Current, context())
            .await
            .unwrap()
            .is_none(),
        "退役后的化身在 current 点不可见"
    );
}

#[tokio::test]
async fn task_state_transitions_update_status_index() {
    let storage = MemoryStorage::new();
    let task = TaskRecord::<serde_json::Value, serde_json::Value, serde_json::Value> {
        id: pi_durable::types::TaskId::new(1),
        conversation_id: ConversationId::new(1),
        kind: "demo".to_string(),
        version: 1,
        input: json!({}),
        owner: None,
        background: false,
        abort_requested: false,
        started_at: None,
        ended_at: None,
        state: TaskState::Pending {
            checkpoint: json!({ "phase": "start" }),
        },
        memos: None,
    };

    storage
        .commit(&[StorageWrite::Task(task.clone())], context())
        .await
        .unwrap();

    let mut terminal = task.clone();
    terminal.state = TaskState::Terminal {
        outcome: TaskOutcome::Completed {
            result: json!({ "ok": true }),
        },
    };
    storage
        .commit(&[StorageWrite::Task(terminal)], context())
        .await
        .unwrap();

    let pending = storage
        .scan_tasks(
            pi_durable::types::TaskQuery {
                conversation_id: None,
                kind: None,
                status: Some(TaskStatus::Pending),
                abort_requested: None,
                background: None,
                order: None,
            },
            10,
            None,
            context(),
        )
        .await
        .unwrap();
    assert!(pending.items.is_empty(), "状态索引应移除旧的 pending");

    let terminal_page = storage
        .scan_tasks(
            pi_durable::types::TaskQuery {
                conversation_id: None,
                kind: None,
                status: Some(TaskStatus::Terminal),
                abort_requested: None,
                background: None,
                order: None,
            },
            10,
            None,
            context(),
        )
        .await
        .unwrap();
    assert_eq!(terminal_page.items.len(), 1);
}

#[tokio::test]
async fn close_makes_later_operations_fail() {
    let storage = MemoryStorage::new();
    storage.close(context()).await.unwrap();

    let error = storage
        .commit(&[StorageWrite::Conversation(conversation(1))], context())
        .await
        .unwrap_err();
    assert_eq!(error, StorageError::Closed);
}

// 保持与上游 `ConversationOwnership` 的对照（本测试未直接使用，仅确保导出存在）。
#[allow(dead_code)]
fn _ownership_exports(_: ConversationOwnership, _: ConversationHistory, _: ConversationFork) {}

// 保证 `EntryDraft` 与文档查询类型可被外部构造。
#[allow(dead_code)]
fn _draft_shapes(_: EntryDraft, _: pi_durable::types::DocumentQuery) {}
