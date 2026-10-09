//! 对应 `testing/storage-benchmark.ts`：Storage 实现的确定性性能基准。
//!
//! 与上游相同：通过 **public `Storage` 契约** 播种确定性的代表性数据，再以固定读数 / 写数
//! 断言数据集结构。播种本身构造了大量边界数据（rewindable 历史、家族重放、fork 深度、
//! 过滤任务、混合写入），因此也是对各后端实现的有效正确性验证。
//!
//! 语言机制映射：`Promise.all` → 顺序 `mint_id`；`Object.fromEntries` → `HashMap`；
//! 数字 ID 品牌 → `Id::new`。benchmark 的 `run` / `expected` 用闭包 + [`BoxFuture`] 表达。

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Value as JsonValue, json};

use crate::chord::context::{BACKGROUND_CONTEXT, Context};
use crate::chord::delta::{Op, PathSegment};
use crate::types::{
    ConversationFork, ConversationHistory, ConversationId, ConversationParent, ConversationRecord,
    DocumentAddress, DocumentBase, DocumentContent, DocumentCreate, DocumentId, DocumentPoint,
    DocumentScope, EntryId, EntryQuery, EntryRecord, JsonObject, ROOT_CONVERSATION_ID, Seq,
    Storage, StorageWrite, SubmissionId, SubmissionIdentity, SubmissionRecord, TaskId, TaskOutcome,
    TaskQuery, TaskRecord, TaskState, TaskStatus, WriteSubmissionStatus,
};

/// 对应 `StorageBenchmarkScale`。
#[derive(Debug, Clone, Copy)]
pub struct StorageBenchmarkScale {
    pub name: &'static str,
    pub entry_count: usize,
    pub task_count: usize,
    pub document_count: usize,
}

/// 对应 `STORAGE_MEMORY_SCALES`。
pub const STORAGE_MEMORY_SCALES: &[StorageBenchmarkScale] = &[
    StorageBenchmarkScale {
        name: "1k",
        entry_count: 1_000,
        task_count: 200,
        document_count: 200,
    },
    StorageBenchmarkScale {
        name: "10k",
        entry_count: 10_000,
        task_count: 2_000,
        document_count: 2_000,
    },
];

/// 对应 `TIMING_SCALE`。
pub const TIMING_SCALE: StorageBenchmarkScale = StorageBenchmarkScale {
    name: "timing",
    entry_count: 1_000,
    task_count: 300,
    document_count: 300,
};

/// 对应 `REPLAY_TAILS`。
const REPLAY_TAILS: [usize; 4] = [0, 16, 128, 1_024];
/// 对应 `HISTORY_SEGMENT_LENGTH`。
const HISTORY_SEGMENT_LENGTH: usize = 128;
/// 对应 `FORK_DEPTH`。
const FORK_DEPTH: usize = 8;
/// 对应 `ENTRIES_PER_FORK`。
const ENTRIES_PER_FORK: usize = 32;
/// 对应 `BATCH_SIZE`。
const BATCH_SIZE: usize = 100;

/// 对应 `storageBenchmarkPrimaryRecordCount`。
pub fn storage_benchmark_primary_record_count(scale: &StorageBenchmarkScale) -> usize {
    1 + scale.entry_count
        + scale.task_count
        + scale.document_count
        + REPLAY_TAILS.len()
        + 1
        + FORK_DEPTH * (1 + ENTRIES_PER_FORK)
}

type StoredTask = TaskRecord<JsonValue, JsonValue, JsonValue>;

/// 对应 `StorageBenchmarkDataset`。
#[derive(Debug, Clone)]
pub struct StorageBenchmarkDataset {
    pub first_entry_id: EntryId,
    pub filtered_task_count: usize,
    pub exact_document_id: DocumentId,
    pub exact_document_key: String,
    pub replay_document_ids: HashMap<usize, DocumentId>,
    pub historical_document_id: DocumentId,
    pub ancient_at: Seq,
    pub recent_at: Seq,
    pub deepest_conversation_id: ConversationId,
    pub ancestor_head_entry_id: EntryId,
}

/// 把 `serde_json::Value` 对象转成 [`JsonObject`]。
fn obj(value: JsonValue) -> JsonObject {
    value.as_object().cloned().unwrap_or_default()
}

/// 对应 `task`。
fn task(id: u64, index: usize) -> StoredTask {
    let status = index % 3; // 0=pending, 1=running, 2=terminal
    let kind = if index.is_multiple_of(4) {
        "benchmark.filtered"
    } else {
        "benchmark.other"
    };
    let common = TaskRecord {
        id: TaskId::<JsonValue>::new(id),
        conversation_id: ROOT_CONVERSATION_ID,
        kind: kind.to_string(),
        version: 1,
        input: json!({ "index": index }),
        owner: None,
        background: index.is_multiple_of(5),
        abort_requested: index.is_multiple_of(7),
        started_at: None,
        ended_at: None,
        state: TaskState::Pending {
            checkpoint: json!(null),
        },
        memos: None,
    };
    let checkpoint = json!({ "index": index, "payload": "x".repeat(64) });
    if status == 2 {
        TaskRecord {
            state: TaskState::Terminal {
                outcome: TaskOutcome::Completed {
                    result: json!({ "index": index }),
                },
            },
            ..common
        }
    } else if status == 1 {
        TaskRecord {
            state: TaskState::Running { checkpoint },
            ..common
        }
    } else {
        TaskRecord {
            state: TaskState::Pending { checkpoint },
            ..common
        }
    }
}

fn conversation(id: ConversationId) -> ConversationRecord {
    ConversationRecord {
        id,
        parent: None,
        owner: None,
    }
}

fn entry(
    id: EntryId,
    conversation_id: ConversationId,
    kind: &str,
    data: JsonValue,
    head: Option<EntryId>,
) -> EntryRecord {
    EntryRecord {
        id,
        conversation_id,
        kind: kind.to_string(),
        model: None,
        data: Some(data),
        head,
        edits: None,
        by_task_id: None,
    }
}

/// 对应 `seedStorageBenchmark`：只经 public `Storage` 契约播种确定性代表性数据。
pub async fn seed_storage_benchmark(
    storage: &dyn Storage,
    scale: StorageBenchmarkScale,
    context: &dyn Context,
) -> StorageBenchmarkDataset {
    storage
        .commit(
            &[StorageWrite::Conversation(conversation(
                ROOT_CONVERSATION_ID,
            ))],
            context,
        )
        .await
        .expect("seed root conversation");

    let mut first_entry_id: Option<EntryId> = None;
    let mut start = 0;
    while start < scale.entry_count {
        let end = (start + BATCH_SIZE).min(scale.entry_count);
        let mut writes = Vec::new();
        for index in start..end {
            let id = EntryId::new(storage.mint_id().await);
            if index == 0 {
                first_entry_id = Some(id);
            }
            let head = if index == 0 { Some(id) } else { None };
            writes.push(StorageWrite::Entry(entry(
                id,
                ROOT_CONVERSATION_ID,
                "benchmark.entry",
                json!({ "index": index, "text": format!("entry-{index}-{}", "x".repeat(96)) }),
                head,
            )));
        }
        storage
            .commit(&writes, context)
            .await
            .expect("seed entries");
        start = end;
    }

    let mut start = 0;
    while start < scale.task_count {
        let end = (start + BATCH_SIZE).min(scale.task_count);
        let mut writes = Vec::new();
        for index in start..end {
            let id = storage.mint_id().await;
            writes.push(StorageWrite::Task(task(id, index)));
        }
        storage.commit(&writes, context).await.expect("seed tasks");
        start = end;
    }

    let mut exact_document_id: Option<DocumentId> = None;
    let mut start = 0;
    while start < scale.document_count {
        let end = (start + BATCH_SIZE).min(scale.document_count);
        let mut writes = Vec::new();
        for index in start..end {
            let id = DocumentId::new(storage.mint_id().await);
            exact_document_id = Some(id);
            writes.push(StorageWrite::DocumentCreate {
                record: DocumentCreate {
                    id,
                    kind: "benchmark.family".to_string(),
                    key: Some(format!("key-{index}")),
                    history: None,
                    fork: None,
                    scope: DocumentScope::Session,
                },
                content: DocumentBase {
                    version: 1,
                    value: obj(json!({ "index": index, "text": "x".repeat(128) })),
                },
            });
        }
        storage
            .commit(&writes, context)
            .await
            .expect("seed documents");
        start = end;
    }

    let mut replay_entries: Vec<(usize, DocumentId)> = Vec::new();
    for &tail in REPLAY_TAILS.iter() {
        replay_entries.push((tail, DocumentId::new(storage.mint_id().await)));
    }
    let replay_writes: Vec<StorageWrite> = replay_entries
        .iter()
        .map(|&(tail, id)| StorageWrite::DocumentCreate {
            record: DocumentCreate {
                id,
                kind: "benchmark.replay".to_string(),
                key: Some(tail.to_string()),
                history: Some(ConversationHistory::Rewindable),
                fork: Some(ConversationFork::AsOf),
                scope: DocumentScope::Conversation {
                    conversation_id: ROOT_CONVERSATION_ID,
                },
            },
            content: DocumentBase {
                version: 1,
                value: obj(json!({ "count": 0, "text": "x".repeat(64) })),
            },
        })
        .collect();
    storage
        .commit(&replay_writes, context)
        .await
        .expect("seed replay documents");

    let max_tail = *REPLAY_TAILS.last().unwrap();
    for count in 1..=max_tail {
        let writes: Vec<StorageWrite> = replay_entries
            .iter()
            .filter(|&&(tail, _)| count <= tail)
            .map(|&(_, id)| StorageWrite::DocumentChange {
                id,
                content: DocumentContent::Delta {
                    version: 1,
                    ops: vec![Op::Set(
                        vec![PathSegment::Key("count".to_string())],
                        json!(count),
                    )],
                },
            })
            .collect();
        storage
            .commit(&writes, context)
            .await
            .expect("seed replay changes");
    }

    let historical_document_id = DocumentId::new(storage.mint_id().await);
    let historical_record = DocumentCreate {
        id: historical_document_id,
        kind: "benchmark.history".to_string(),
        key: None,
        history: Some(ConversationHistory::Rewindable),
        fork: Some(ConversationFork::AsOf),
        scope: DocumentScope::Conversation {
            conversation_id: ROOT_CONVERSATION_ID,
        },
    };
    storage
        .commit(
            &[StorageWrite::DocumentCreate {
                record: historical_record,
                content: DocumentBase {
                    version: 1,
                    value: obj(json!({ "count": 0 })),
                },
            }],
            context,
        )
        .await
        .expect("seed historical document");

    let mut ancient_at = Seq::new(0);
    for count in 1..=HISTORY_SEGMENT_LENGTH {
        ancient_at = storage
            .commit(
                &[StorageWrite::DocumentChange {
                    id: historical_document_id,
                    content: DocumentContent::Delta {
                        version: 1,
                        ops: vec![Op::Set(
                            vec![PathSegment::Key("count".to_string())],
                            json!(count),
                        )],
                    },
                }],
                context,
            )
            .await
            .expect("seed historical deltas");
    }
    storage
        .commit(
            &[StorageWrite::DocumentChange {
                id: historical_document_id,
                content: DocumentContent::Base(DocumentBase {
                    version: 1,
                    value: obj(json!({ "count": HISTORY_SEGMENT_LENGTH })),
                }),
            }],
            context,
        )
        .await
        .expect("seed historical base");

    let mut recent_at = ancient_at;
    for count in (HISTORY_SEGMENT_LENGTH + 1)..=(HISTORY_SEGMENT_LENGTH * 2) {
        recent_at = storage
            .commit(
                &[StorageWrite::DocumentChange {
                    id: historical_document_id,
                    content: DocumentContent::Delta {
                        version: 1,
                        ops: vec![Op::Set(
                            vec![PathSegment::Key("count".to_string())],
                            json!(count),
                        )],
                    },
                }],
                context,
            )
            .await
            .expect("seed recent deltas");
    }

    let first_entry_id = first_entry_id.expect("Benchmark scale must create entries");
    let mut parent_conversation_id = ROOT_CONVERSATION_ID.get();
    let mut parent_at = first_entry_id.get();
    let mut deepest_conversation_id = ROOT_CONVERSATION_ID.get();
    for depth in 0..FORK_DEPTH {
        let conversation_id = storage.mint_id().await;
        storage
            .commit(
                &[StorageWrite::Conversation(ConversationRecord {
                    id: ConversationId::new(conversation_id),
                    parent: Some(ConversationParent {
                        conversation_id: ConversationId::new(parent_conversation_id),
                        at: EntryId::new(parent_at),
                    }),
                    owner: None,
                })],
                context,
            )
            .await
            .expect("seed fork conversation");

        let mut ids = Vec::new();
        for _ in 0..ENTRIES_PER_FORK {
            ids.push(storage.mint_id().await);
        }
        let writes: Vec<StorageWrite> = ids
            .iter()
            .enumerate()
            .map(|(index, &id)| {
                StorageWrite::Entry(entry(
                    EntryId::new(id),
                    ConversationId::new(conversation_id),
                    "benchmark.fork",
                    json!({ "depth": depth, "index": index }),
                    None,
                ))
            })
            .collect();
        storage
            .commit(&writes, context)
            .await
            .expect("seed fork entries");

        parent_conversation_id = conversation_id;
        parent_at = *ids.last().unwrap();
        deepest_conversation_id = conversation_id;
    }

    let exact_document_id = exact_document_id.expect("Benchmark scale must create documents");
    let replay_document_ids: HashMap<usize, DocumentId> = replay_entries.into_iter().collect();

    StorageBenchmarkDataset {
        first_entry_id,
        filtered_task_count: 50.min(scale.task_count.div_ceil(60)),
        exact_document_id,
        exact_document_key: format!("key-{}", scale.document_count - 1),
        replay_document_ids,
        historical_document_id,
        ancient_at,
        recent_at,
        deepest_conversation_id: ConversationId::new(deepest_conversation_id),
        ancestor_head_entry_id: first_entry_id,
    }
}

/// 对应 `seedStorageWriteBenchmark`。
pub async fn seed_storage_write_benchmark(storage: &dyn Storage, context: &dyn Context) {
    storage
        .commit(
            &[StorageWrite::Conversation(conversation(
                ROOT_CONVERSATION_ID,
            ))],
            context,
        )
        .await
        .expect("seed root conversation");

    let mut writes = Vec::new();
    for index in 0..100 {
        writes.push(StorageWrite::Entry(entry(
            EntryId::new(storage.mint_id().await),
            ROOT_CONVERSATION_ID,
            "benchmark.baseline",
            json!({ "index": index }),
            None,
        )));
    }
    storage
        .commit(&writes, context)
        .await
        .expect("seed baseline entries");
}

async fn document_count_value(
    storage: &dyn Storage,
    id: DocumentId,
    at: DocumentPoint,
    context: &dyn Context,
) -> u64 {
    storage
        .document(id, at, context)
        .await
        .expect("document")
        .map(|doc| {
            doc.value
                .get("count")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

// ─── 读取基准 ────────────────────────────────────────────────────────────────

/// 对应 `StorageReadBenchmark.run`。
pub type StorageReadBenchmarkRun = Arc<
    dyn for<'a> Fn(
            &'a dyn Storage,
            &'a StorageBenchmarkDataset,
            &'a dyn Context,
        ) -> BoxFuture<'a, u64>
        + Send
        + Sync,
>;

/// 对应 `StorageReadBenchmark`。
pub struct StorageReadBenchmark {
    pub name: &'static str,
    pub run: StorageReadBenchmarkRun,
    pub expected: Arc<dyn Fn(&StorageBenchmarkDataset) -> u64 + Send + Sync>,
}

/// 对应 `STORAGE_READ_BENCHMARKS`。
pub fn storage_read_benchmarks() -> Vec<StorageReadBenchmark> {
    let mut benchmarks = vec![
        StorageReadBenchmark {
            name: "exact entry lookup",
            run: Arc::new(|s, d, c| {
                Box::pin(async move {
                    s.entry(d.first_entry_id, c)
                        .await
                        .expect("entry")
                        .map(|(r, _)| r.id.get())
                        .unwrap_or(u64::MAX)
                })
            }),
            expected: Arc::new(|d| d.first_entry_id.get()),
        },
        StorageReadBenchmark {
            name: "entry page scan (100)",
            run: Arc::new(|s, _d, c| {
                Box::pin(async move {
                    s.scan_entries(
                        EntryQuery {
                            conversation_id: ROOT_CONVERSATION_ID,
                            min_entry_id: None,
                            max_entry_id: None,
                            order: None,
                        },
                        100,
                        None,
                        c,
                    )
                    .await
                    .expect("scan entries")
                    .items
                    .len() as u64
                })
            }),
            expected: Arc::new(|_| 100),
        },
        StorageReadBenchmark {
            name: "filtered task scan (50)",
            run: Arc::new(|s, _d, c| {
                Box::pin(async move {
                    s.scan_tasks(
                        TaskQuery {
                            conversation_id: None,
                            kind: Some("benchmark.filtered".to_string()),
                            status: Some(TaskStatus::Pending),
                            abort_requested: None,
                            background: Some(true),
                            order: None,
                        },
                        50,
                        None,
                        c,
                    )
                    .await
                    .expect("scan tasks")
                    .items
                    .len() as u64
                })
            }),
            expected: Arc::new(|d| d.filtered_task_count as u64),
        },
        StorageReadBenchmark {
            name: "exact document address among many",
            run: Arc::new(|s, d, c| {
                Box::pin(async move {
                    s.find_document(
                        &DocumentAddress {
                            kind: "benchmark.family".to_string(),
                            scope: DocumentScope::Session,
                            key: Some(d.exact_document_key.clone()),
                        },
                        DocumentPoint::Current,
                        c,
                    )
                    .await
                    .expect("find document")
                    .map(|r| r.id.get())
                    .unwrap_or(u64::MAX)
                })
            }),
            expected: Arc::new(|d| d.exact_document_id.get()),
        },
        StorageReadBenchmark {
            name: "ancient historical read before newer base",
            run: Arc::new(|s, d, c| {
                Box::pin(async move {
                    document_count_value(
                        s,
                        d.historical_document_id,
                        DocumentPoint::At(d.ancient_at),
                        c,
                    )
                    .await
                })
            }),
            expected: Arc::new(|_| HISTORY_SEGMENT_LENGTH as u64),
        },
        StorageReadBenchmark {
            name: "recent historical read after newer base",
            run: Arc::new(|s, d, c| {
                Box::pin(async move {
                    document_count_value(
                        s,
                        d.historical_document_id,
                        DocumentPoint::At(d.recent_at),
                        c,
                    )
                    .await
                })
            }),
            expected: Arc::new(|_| (HISTORY_SEGMENT_LENGTH * 2) as u64),
        },
        StorageReadBenchmark {
            name: "fork-depth history scan (100)",
            run: Arc::new(|s, d, c| {
                Box::pin(async move {
                    s.scan_entries(
                        EntryQuery {
                            conversation_id: d.deepest_conversation_id,
                            min_entry_id: None,
                            max_entry_id: None,
                            order: None,
                        },
                        100,
                        None,
                        c,
                    )
                    .await
                    .expect("scan fork entries")
                    .items
                    .len() as u64
                })
            }),
            expected: Arc::new(|_| 100),
        },
        StorageReadBenchmark {
            name: "fork-depth head lookup",
            run: Arc::new(|s, d, c| {
                Box::pin(async move {
                    s.find_latest_head_marker(d.deepest_conversation_id, None, c)
                        .await
                        .expect("head marker")
                        .map(|(record, _)| record.id.get())
                        .unwrap_or(u64::MAX)
                })
            }),
            expected: Arc::new(|d| d.ancestor_head_entry_id.get()),
        },
    ];

    for tail in REPLAY_TAILS {
        benchmarks.push(StorageReadBenchmark {
            name: Box::leak(format!("document replay tail ({tail})").into_boxed_str()),
            run: Arc::new(move |s, d, c| {
                Box::pin(async move {
                    let id = d.replay_document_ids[&tail];
                    document_count_value(s, id, DocumentPoint::Current, c).await
                })
            }),
            expected: Arc::new(move |_| tail as u64),
        });
    }
    benchmarks
}

// ─── 写入基准 ────────────────────────────────────────────────────────────────

/// 对应 `StorageWriteBenchmark.run`。
pub type StorageWriteBenchmarkRun =
    Arc<dyn for<'a> Fn(&'a dyn Storage, &'a dyn Context) -> BoxFuture<'a, u64> + Send + Sync>;

/// 对应 `StorageWriteBenchmark`。
pub struct StorageWriteBenchmark {
    pub name: &'static str,
    pub expected: u64,
    pub run: StorageWriteBenchmarkRun,
}

/// 对应 `STORAGE_WRITE_BENCHMARKS`。
pub fn storage_write_benchmarks() -> Vec<StorageWriteBenchmark> {
    vec![
        StorageWriteBenchmark {
            name: "commit one entry",
            expected: 1,
            run: Arc::new(|s, c| {
                Box::pin(async move {
                    let id = EntryId::new(s.mint_id().await);
                    s.commit(
                        &[StorageWrite::Entry(entry(
                            id,
                            ROOT_CONVERSATION_ID,
                            "benchmark.write",
                            json!({ "text": "x".repeat(128) }),
                            None,
                        ))],
                        c,
                    )
                    .await
                    .expect("write one entry");
                    1
                })
            }),
        },
        StorageWriteBenchmark {
            name: "commit 100 entries",
            expected: 100,
            run: Arc::new(|s, c| {
                Box::pin(async move {
                    let mut writes = Vec::new();
                    for index in 0..100 {
                        let id = EntryId::new(s.mint_id().await);
                        writes.push(StorageWrite::Entry(entry(
                            id,
                            ROOT_CONVERSATION_ID,
                            "benchmark.write",
                            json!({ "index": index, "text": "x".repeat(128) }),
                            None,
                        )));
                    }
                    let count = writes.len() as u64;
                    s.commit(&writes, c).await.expect("write 100 entries");
                    count
                })
            }),
        },
        StorageWriteBenchmark {
            name: "commit mixed entry/task/submission/document",
            expected: 4,
            run: Arc::new(|s, c| {
                Box::pin(async move {
                    let entry_id = s.mint_id().await;
                    let task_id = s.mint_id().await;
                    let submission_id = s.mint_id().await;
                    let document_id = s.mint_id().await;
                    let writes = [
                        StorageWrite::Entry(entry(
                            EntryId::new(entry_id),
                            ROOT_CONVERSATION_ID,
                            "benchmark.mixed",
                            json!(null),
                            None,
                        )),
                        StorageWrite::Task(task(task_id, task_id as usize)),
                        StorageWrite::Submission(SubmissionRecord::Write {
                            identity: SubmissionIdentity {
                                id: SubmissionId::new(submission_id),
                                conversation_id: ROOT_CONVERSATION_ID,
                                request_id: Some(format!("benchmark-{submission_id}")),
                            },
                            status: WriteSubmissionStatus::Done {
                                entry: EntryId::new(entry_id),
                            },
                        }),
                        StorageWrite::DocumentCreate {
                            record: DocumentCreate {
                                id: DocumentId::new(document_id),
                                kind: "benchmark.mixed".to_string(),
                                key: Some(document_id.to_string()),
                                history: None,
                                fork: None,
                                scope: DocumentScope::Session,
                            },
                            content: DocumentBase {
                                version: 1,
                                value: obj(json!({ "entryId": entry_id, "taskId": task_id })),
                            },
                        },
                    ];
                    let count = writes.len() as u64;
                    s.commit(&writes, c).await.expect("write mixed");
                    count
                })
            }),
        },
    ]
}

/// 供外部直接使用的上下文（对应上游传 `BACKGROUND_CONTEXT` 的调用点）。
pub fn background_context() -> &'static dyn Context {
    BACKGROUND_CONTEXT.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MemoryStorage;

    #[tokio::test]
    async fn read_benchmarks_against_memory_storage() {
        let storage = MemoryStorage::new();
        let context = background_context();
        let dataset = seed_storage_benchmark(&storage, TIMING_SCALE, context).await;

        assert_eq!(
            dataset.filtered_task_count, 5,
            "300 tasks → ceil(300/60)=5 filtered+pending+background"
        );
        assert_eq!(dataset.replay_document_ids.len(), REPLAY_TAILS.len());

        for benchmark in storage_read_benchmarks() {
            let actual = (benchmark.run)(&storage, &dataset, context).await;
            let expected = (benchmark.expected)(&dataset);
            assert_eq!(actual, expected, "read benchmark `{}`", benchmark.name);
        }
    }

    #[tokio::test]
    async fn write_benchmarks_against_memory_storage() {
        let storage = MemoryStorage::new();
        let context = background_context();
        seed_storage_write_benchmark(&storage, context).await;

        for benchmark in storage_write_benchmarks() {
            let actual = (benchmark.run)(&storage, context).await;
            assert_eq!(
                actual, benchmark.expected,
                "write benchmark `{}`",
                benchmark.name
            );
        }
    }
}
