//! 对应 `testing/storage-conformance.ts`：Storage 实现的参数化一致性测试（核心子集）。
//!
//! 同一组 case 对每个后端（memory / jsonl / sqlite）各跑一遍，验证行为一致。
//! 断言用 Rust 原生 `assert!`（等价于上游的 `strictEqual` / `deepEqual` / `greaterThan` / `rejects`）。

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Value as JsonValue, json};

use crate::chord::context::Context;
use crate::types::{
    ConversationId, ConversationRecord, EntryId, EntryQuery, EntryRecord, InputSubmissionStatus,
    ROOT_CONVERSATION_ID, ScanOrder, Storage, StorageError, StorageWrite, SubmissionId,
    SubmissionIdentity, SubmissionRecord, TaskId, TaskRecord, TaskState,
};

/// 一个一致性测试 case：名称 + 以某后端运行的函数。
pub type StorageConformanceCase = (
    &'static str,
    Arc<dyn for<'a> Fn(&'a dyn Storage, &'a dyn Context) -> BoxFuture<'a, ()> + Send + Sync>,
);

type StoredTask = TaskRecord<JsonValue, JsonValue, JsonValue>;

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

fn pending_task(id: u64, conversation_id: u64) -> StoredTask {
    TaskRecord {
        id: TaskId::new(id),
        conversation_id: ConversationId::new(conversation_id),
        kind: "test.task".to_string(),
        version: 1,
        input: json!({ "value": id }),
        owner: None,
        background: false,
        abort_requested: false,
        started_at: None,
        ended_at: None,
        state: TaskState::Pending {
            checkpoint: json!({ "phase": "ready" }),
        },
        memos: None,
    }
}

fn input_submission(id: u64, conversation_id: u64, request_id: &str) -> SubmissionRecord {
    SubmissionRecord::Input {
        identity: SubmissionIdentity {
            id: SubmissionId::new(id),
            conversation_id: ConversationId::new(conversation_id),
            request_id: Some(request_id.to_string()),
        },
        status: InputSubmissionStatus::Queued,
    }
}

async fn create_root(storage: &dyn Storage, context: &dyn Context) {
    storage
        .commit(&[StorageWrite::Conversation(conversation(1))], context)
        .await
        .expect("root conversation");
}

fn assert_closed(result: Result<impl std::fmt::Debug, StorageError>) {
    assert!(
        matches!(result, Err(StorageError::Closed)),
        "expected closed error, got {result:?}"
    );
}

/// 对应 `createStorageConformance`：核心一致性 case 集合。
pub fn storage_conformance_cases() -> Vec<StorageConformanceCase> {
    vec![
        (
            "reserves ID 1 for the immutable root conversation",
            Arc::new(|storage, context| {
                Box::pin(async move {
                    assert_eq!(storage.mint_id().await, 2);
                    create_root(storage, context).await;
                    let root = storage
                        .conversation(ROOT_CONVERSATION_ID, context)
                        .await
                        .expect("conversation")
                        .expect("root exists");
                    assert_eq!(root.id, ROOT_CONVERSATION_ID);
                    // 再次提交 root 被拒。
                    let result = storage
                        .commit(&[StorageWrite::Conversation(conversation(1))], context)
                        .await;
                    assert!(result.is_err(), "重复 root 应被拒");
                })
            }),
        ),
        (
            "commits mixed table writes atomically and rolls back on failure",
            Arc::new(|storage, context| {
                Box::pin(async move {
                    create_root(storage, context).await;
                    let entry_id = storage.mint_id().await;
                    let task_id = storage.mint_id().await;
                    let submission_id = storage.mint_id().await;
                    let task = pending_task(task_id, 1);
                    let submission = input_submission(submission_id, 1, "request-1");
                    let initial_seq = storage
                        .commit(
                            &[
                                StorageWrite::Entry(entry(entry_id, 1, "pi.user")),
                                StorageWrite::Task(task.clone()),
                                StorageWrite::Submission(submission.clone()),
                            ],
                            context,
                        )
                        .await
                        .expect("commit");

                    assert_eq!(
                        storage
                            .entry(EntryId::new(entry_id), context)
                            .await
                            .unwrap()
                            .unwrap()
                            .0
                            .kind,
                        "pi.user"
                    );
                    assert_eq!(
                        storage
                            .task(TaskId::new(task_id), context)
                            .await
                            .unwrap()
                            .unwrap(),
                        task
                    );
                    assert_eq!(
                        storage
                            .submission(SubmissionId::new(submission_id), context)
                            .await
                            .unwrap()
                            .unwrap(),
                        submission
                    );

                    // 失败提交回滚全部。
                    let transient = storage.mint_id().await;
                    let result = storage
                        .commit(
                            &[
                                StorageWrite::Task(pending_task(task_id, 1)),
                                StorageWrite::Submission(input_submission(
                                    submission_id,
                                    1,
                                    "request-1",
                                )),
                                StorageWrite::Entry(entry(transient, 1, "pi.assistant")),
                                StorageWrite::Conversation(conversation(1)),
                            ],
                            context,
                        )
                        .await;
                    assert!(result.is_err(), "重复 root 应导致整批回滚");

                    assert_eq!(
                        storage
                            .task(TaskId::new(task_id), context)
                            .await
                            .unwrap()
                            .unwrap(),
                        task
                    );
                    assert_eq!(
                        storage
                            .submission(SubmissionId::new(submission_id), context)
                            .await
                            .unwrap()
                            .unwrap(),
                        submission
                    );
                    assert!(
                        storage
                            .entry(EntryId::new(transient), context)
                            .await
                            .unwrap()
                            .is_none()
                    );
                    let after = storage
                        .commit(
                            &[StorageWrite::Entry(entry(
                                storage.mint_id().await,
                                1,
                                "after",
                            ))],
                            context,
                        )
                        .await
                        .unwrap();
                    assert!(after > initial_seq);
                })
            }),
        ),
        (
            "detaches retained writes and every returned record",
            Arc::new(|storage, context| {
                Box::pin(async move {
                    create_root(storage, context).await;
                    let entry_id = storage.mint_id().await;
                    let task_id = storage.mint_id().await;
                    let submission_id = storage.mint_id().await;
                    let mut stored_entry = entry(entry_id, 1, "note");
                    stored_entry.data = Some(json!({ "nested": [1, 2] }));
                    let mut stored_task = pending_task(task_id, 1);
                    stored_task.state = TaskState::Pending {
                        checkpoint: json!({ "phase": "ready", "nested": { "count": 1 } }),
                    };
                    storage
                        .commit(
                            &[
                                StorageWrite::Entry(stored_entry),
                                StorageWrite::Task(stored_task),
                                StorageWrite::Submission(input_submission(
                                    submission_id,
                                    1,
                                    "request-1",
                                )),
                            ],
                            context,
                        )
                        .await
                        .expect("commit");

                    let read = storage
                        .entry(EntryId::new(entry_id), context)
                        .await
                        .unwrap()
                        .unwrap()
                        .0;
                    assert_eq!(read.data, Some(json!({ "nested": [1, 2] })));
                    let read_task = storage
                        .task(TaskId::new(task_id), context)
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        read_task.state,
                        TaskState::Pending {
                            checkpoint: json!({ "phase": "ready", "nested": { "count": 1 } })
                        }
                    );
                })
            }),
        ),
        (
            "indexes entries committed out of ID order",
            Arc::new(|storage, context| {
                Box::pin(async move {
                    create_root(storage, context).await;
                    storage
                        .commit(
                            &[
                                StorageWrite::Entry(entry(30, 1, "message")),
                                StorageWrite::Entry(entry(10, 1, "message")),
                                StorageWrite::Entry(entry(20, 1, "marker")),
                            ],
                            context,
                        )
                        .await
                        .expect("commit");

                    let query = EntryQuery {
                        conversation_id: ConversationId::new(1),
                        min_entry_id: None,
                        max_entry_id: None,
                        order: None,
                    };
                    let ids: Vec<u64> = storage
                        .scan_entries(query, 10, None, context)
                        .await
                        .unwrap()
                        .items
                        .iter()
                        .map(|entry| entry.id.get())
                        .collect();
                    assert_eq!(ids, vec![30, 20, 10], "默认降序按 ID");
                })
            }),
        ),
        (
            "scans entries in either order and pages by cursor",
            Arc::new(|storage, context| {
                Box::pin(async move {
                    create_root(storage, context).await;
                    let mut ids = Vec::new();
                    for _ in 0..5 {
                        let id = storage.mint_id().await;
                        ids.push(id);
                        storage
                            .commit(&[StorageWrite::Entry(entry(id, 1, "message"))], context)
                            .await
                            .expect("commit");
                    }
                    let query = EntryQuery {
                        conversation_id: ConversationId::new(1),
                        min_entry_id: None,
                        max_entry_id: None,
                        order: Some(ScanOrder::Ascending),
                    };
                    let first = storage.scan_entries(query, 2, None, context).await.unwrap();
                    assert_eq!(
                        first.items.iter().map(|e| e.id.get()).collect::<Vec<_>>(),
                        ids[..2].to_vec()
                    );
                    let second = storage
                        .scan_entries(query, 2, first.next, context)
                        .await
                        .unwrap();
                    assert_eq!(
                        second.items.iter().map(|e| e.id.get()).collect::<Vec<_>>(),
                        ids[2..4].to_vec()
                    );
                })
            }),
        ),
        (
            "replaces complete task records and pages filtered task scans",
            Arc::new(|storage, context| {
                Box::pin(async move {
                    create_root(storage, context).await;
                    let task_id = storage.mint_id().await;
                    storage
                        .commit(&[StorageWrite::Task(pending_task(task_id, 1))], context)
                        .await
                        .expect("commit");

                    let mut replaced = pending_task(task_id, 1);
                    replaced.state = TaskState::Terminal {
                        outcome: crate::types::TaskOutcome::Completed {
                            result: json!(null),
                        },
                    };
                    storage
                        .commit(&[StorageWrite::Task(replaced.clone())], context)
                        .await
                        .expect("replace");

                    let read = storage
                        .task(TaskId::new(task_id), context)
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(read, replaced);
                    assert_eq!(read.state.status(), crate::types::TaskStatus::Terminal);
                })
            }),
        ),
        (
            "rejects every operation after close",
            Arc::new(|storage, context| {
                Box::pin(async move {
                    create_root(storage, context).await;
                    let _ = storage.close(context).await;
                    assert_closed(storage.conversation(ROOT_CONVERSATION_ID, context).await);
                    assert_closed(storage.entry(EntryId::new(2), context).await);
                    assert_closed(
                        storage
                            .commit(&[StorageWrite::Entry(entry(2, 1, "message"))], context)
                            .await,
                    );
                })
            }),
        ),
    ]
}
