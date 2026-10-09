//! `storage::jsonl`（对应 `src/storage/jsonl/storage.ts`）的集成测试。

use std::sync::Arc;

use pi_durable::chord::context::{BACKGROUND_CONTEXT, Context};
use pi_durable::chord::delta::PathSegment;
use pi_durable::env::{FileSystem, InMemoryFileSystem};
use pi_durable::storage::JsonlStorage;
use pi_durable::types::{
    ConversationFork, ConversationHistory, ConversationId, ConversationRecord, DocumentBase,
    DocumentContent, DocumentCreate, DocumentId, DocumentPoint, DocumentScope, EntryId,
    EntryRecord, JsonObject, Storage, StorageWrite, TaskId, TaskOutcome, TaskRecord, TaskState,
};
use serde_json::json;

fn context() -> &'static dyn Context {
    BACKGROUND_CONTEXT.as_ref()
}

fn conversation(id: u64) -> ConversationRecord {
    ConversationRecord {
        id: ConversationId::new(id),
        parent: None,
        owner: None,
    }
}

fn entry(id: u64, conversation_id: u64) -> EntryRecord {
    EntryRecord {
        id: EntryId::new(id),
        conversation_id: ConversationId::new(conversation_id),
        kind: "pi.user".to_string(),
        model: None,
        data: None,
        head: None,
        edits: None,
        by_task_id: None,
    }
}

fn task(
    id: u64,
    terminal: bool,
) -> TaskRecord<serde_json::Value, serde_json::Value, serde_json::Value> {
    let state = if terminal {
        TaskState::Terminal {
            outcome: TaskOutcome::Completed {
                result: json!("ok"),
            },
        }
    } else {
        TaskState::Pending {
            checkpoint: json!({ "phase": "start" }),
        }
    };
    TaskRecord {
        id: TaskId::new(id),
        conversation_id: ConversationId::new(1),
        kind: "demo".to_string(),
        version: 1,
        input: json!({ "seed": id }),
        owner: None,
        background: false,
        abort_requested: false,
        started_at: None,
        ended_at: None,
        state,
        memos: None,
    }
}

fn fs() -> Arc<dyn FileSystem> {
    Arc::new(InMemoryFileSystem::new())
}

#[tokio::test]
async fn open_on_missing_main_file_starts_empty() {
    let storage = JsonlStorage::open(fs(), "/sessions", Default::default(), context())
        .await
        .expect("空目录应可打开");
    assert!(
        storage
            .conversation(ConversationId::new(1), context())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn commit_writes_main_file_and_reopen_replays_it() {
    let file_system = fs();
    {
        let storage = JsonlStorage::open(
            Arc::clone(&file_system),
            "/sessions",
            Default::default(),
            context(),
        )
        .await
        .unwrap();
        let seq = storage
            .commit(
                &[
                    StorageWrite::Conversation(conversation(1)),
                    StorageWrite::Entry(entry(101, 1)),
                ],
                context(),
            )
            .await
            .unwrap();
        assert_eq!(seq.get(), 1);
    }

    // 主文件应存在且含一行 marker。
    let main = file_system
        .read_text_file("/sessions/main.jsonl", context())
        .await
        .unwrap();
    assert_eq!(main.lines().count(), 1);
    assert!(main.contains("\"type\":\"commit\""));

    // 重开后数据仍在（replay）。
    let reopened = JsonlStorage::open(
        Arc::clone(&file_system),
        "/sessions",
        Default::default(),
        context(),
    )
    .await
    .unwrap();
    assert!(
        reopened
            .conversation(ConversationId::new(1), context())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        reopened
            .entry(EntryId::new(101), context())
            .await
            .unwrap()
            .is_some()
    );

    // 序号在重开后继续递增。
    let seq = reopened
        .commit(&[StorageWrite::Entry(entry(102, 1))], context())
        .await
        .unwrap();
    assert_eq!(seq.get(), 2);
}

#[tokio::test]
async fn live_task_uses_sidecar_and_round_trips() {
    let file_system = fs();
    {
        let storage = JsonlStorage::open(
            Arc::clone(&file_system),
            "/sessions",
            Default::default(),
            context(),
        )
        .await
        .unwrap();
        storage
            .commit(&[StorageWrite::Task(task(7, false))], context())
            .await
            .unwrap();
    }

    // 非终态任务走 sidecar：主文件里只有引用，内容在 task-7.jsonl。
    let main = file_system
        .read_text_file("/sessions/main.jsonl", context())
        .await
        .unwrap();
    assert!(main.contains("\"type\":\"task.sidecar\""));
    assert!(!main.contains("\"pi.task\""));

    let sidecar = file_system
        .read_text_file("/sessions/task-7.jsonl", context())
        .await
        .unwrap();
    assert_eq!(sidecar.lines().count(), 1);
    assert!(sidecar.contains("\"type\":\"task\""));

    let reopened = JsonlStorage::open(
        Arc::clone(&file_system),
        "/sessions",
        Default::default(),
        context(),
    )
    .await
    .unwrap();
    let restored = reopened
        .task(TaskId::new(7), context())
        .await
        .unwrap()
        .expect("任务应被 replay");
    assert_eq!(restored.input, json!({ "seed": 7 }));
}

#[tokio::test]
async fn terminal_task_stays_in_main_file() {
    let file_system = fs();
    let storage = JsonlStorage::open(
        Arc::clone(&file_system),
        "/sessions",
        Default::default(),
        context(),
    )
    .await
    .unwrap();
    storage
        .commit(&[StorageWrite::Task(task(9, true))], context())
        .await
        .unwrap();

    let main = file_system
        .read_text_file("/sessions/main.jsonl", context())
        .await
        .unwrap();
    assert!(main.contains("\"type\":\"task\""));
    assert!(
        !file_system
            .exists("/sessions/task-9.jsonl", context())
            .await
            .unwrap(),
        "终态任务不应产生 sidecar"
    );
}

#[tokio::test]
async fn document_sidecar_round_trips_with_delta() {
    let file_system = fs();
    {
        let storage = JsonlStorage::open(
            Arc::clone(&file_system),
            "/sessions",
            Default::default(),
            context(),
        )
        .await
        .unwrap();

        let mut value = JsonObject::new();
        value.insert("count".to_string(), json!(0));
        storage
            .commit(
                &[StorageWrite::DocumentCreate {
                    record: DocumentCreate {
                        id: DocumentId::new(3),
                        kind: "demo.session".to_string(),
                        key: None,
                        history: None,
                        fork: None,
                        scope: DocumentScope::Session,
                    },
                    content: DocumentBase { version: 1, value },
                }],
                context(),
            )
            .await
            .unwrap();

        storage
            .commit(
                &[StorageWrite::DocumentChange {
                    id: DocumentId::new(3),
                    content: DocumentContent::Delta {
                        version: 1,
                        ops: vec![pi_durable::chord::delta::Op::Set(
                            vec![PathSegment::Key("count".to_string())],
                            json!(5),
                        )],
                    },
                }],
                context(),
            )
            .await
            .unwrap();
    }

    let reopened = JsonlStorage::open(
        Arc::clone(&file_system),
        "/sessions",
        Default::default(),
        context(),
    )
    .await
    .unwrap();
    let stored = reopened
        .document(DocumentId::new(3), DocumentPoint::Current, context())
        .await
        .unwrap()
        .expect("文档应被 replay 并物化");
    assert_eq!(stored.value.get("count"), Some(&json!(5)));
    assert_eq!(stored.deltas_since_base, 1);
}

#[tokio::test]
async fn malformed_marker_is_reported_as_corruption() {
    let memory = Arc::new(InMemoryFileSystem::new());
    memory.put("/sessions/main.jsonl", "{not json}\n");
    let file_system: Arc<dyn FileSystem> = memory;

    let error = JsonlStorage::open(
        Arc::clone(&file_system),
        "/sessions",
        Default::default(),
        context(),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Malformed complete commit marker"),
        "{error}"
    );
}

#[tokio::test]
async fn close_rejects_later_commits() {
    let storage = JsonlStorage::open(fs(), "/sessions", Default::default(), context())
        .await
        .unwrap();
    storage.close(context()).await.unwrap();

    let error = storage
        .commit(&[StorageWrite::Conversation(conversation(1))], context())
        .await
        .unwrap_err();
    assert!(matches!(error, pi_durable::types::StorageError::Closed));
}

#[tokio::test]
async fn document_reads_retain_each_point_in_time_value_after_reopen() {
    let file_system = fs();
    let mut value = JsonObject::new();
    value.insert("count".to_string(), json!(0));

    let (created, changed) = {
        let storage = JsonlStorage::open(
            Arc::clone(&file_system),
            "/sessions",
            Default::default(),
            context(),
        )
        .await
        .unwrap();

        let created = storage
            .commit(
                &[StorageWrite::DocumentCreate {
                    record: DocumentCreate {
                        id: DocumentId::new(3),
                        kind: "demo.session".to_string(),
                        key: None,
                        history: Some(ConversationHistory::Rewindable),
                        fork: Some(ConversationFork::Current),
                        scope: DocumentScope::Conversation {
                            conversation_id: ConversationId::new(1),
                        },
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

        let changed = storage
            .commit(
                &[StorageWrite::DocumentChange {
                    id: DocumentId::new(3),
                    content: DocumentContent::Delta {
                        version: 1,
                        ops: vec![pi_durable::chord::delta::Op::Set(
                            vec![PathSegment::Key("count".to_string())],
                            json!(5),
                        )],
                    },
                }],
                context(),
            )
            .await
            .unwrap();
        (created, changed)
    };

    let reopened = JsonlStorage::open(
        Arc::clone(&file_system),
        "/sessions",
        Default::default(),
        context(),
    )
    .await
    .unwrap();

    let at_base = reopened
        .document(DocumentId::new(3), DocumentPoint::At(created), context())
        .await
        .unwrap()
        .expect("base 点应可读");
    assert_eq!(at_base.value.get("count"), Some(&json!(0)));

    let at_delta = reopened
        .document(DocumentId::new(3), DocumentPoint::At(changed), context())
        .await
        .unwrap()
        .expect("delta 点应可读");
    assert_eq!(at_delta.value.get("count"), Some(&json!(5)));
}
