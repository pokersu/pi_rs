//! 对应 `src/storage/jsonl/storage.ts` 的编解码部分。
//!
//! JSONL 后端把每次提交写成「主文件里的一行 commit marker」，把体积大的 task / document 记录
//! 另存到 sidecar 文件（`task-{id}.jsonl` / `doc-{id}.jsonl`），marker 里只留 `ordinal` 引用。
//!
//! 本模块只做**纯数据编解码**（不碰文件系统），便于独立验证。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::types::{
    ConversationRecord, DocumentContent, DocumentCreate, DocumentId, EntryRecord, Seq,
    StorageWrite, SubmissionRecord, TaskId, TaskRecord,
};

/// 对应 `FORMAT_VERSION`。
pub const FORMAT_VERSION: u32 = 1;

/// 主文件的固定名（对应 `MAIN_FILE`）。
pub const MAIN_FILE: &str = "main.jsonl";

/// 回收文件的固定后缀（对应 `RECLAIM_SUFFIX`）。
pub const RECLAIM_SUFFIX: &str = ".reclaim";

/// 持久化的任务记录形态。
pub type StoredTask = TaskRecord<JsonValue, JsonValue, JsonValue>;

/// 对应 `MarkerKind`（`type: "commit"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkerKind {
    /// 提交标记。
    Commit,
}

/// 对应 `SidecarRecord.kind`（`type: "record"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    /// sidecar 记录。
    Record,
}

/// 对应 `MainOperation`：主文件里一条提交的操作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MainOperation {
    /// `conversation` 写入。
    #[serde(rename = "conversation")]
    Conversation {
        /// 记录。
        value: ConversationRecord,
    },
    /// `entry` 写入。
    #[serde(rename = "entry")]
    Entry {
        /// 记录。
        value: EntryRecord,
    },
    /// `submission` 写入。
    #[serde(rename = "submission")]
    Submission {
        /// 记录。
        value: SubmissionRecord,
    },
    /// 终态 task 记录（直接落在主文件）。
    #[serde(rename = "task")]
    Task {
        /// 记录。
        value: StoredTask,
    },
    /// 非终态 task：记录在 sidecar，主文件只留引用。
    #[serde(rename = "task.sidecar")]
    TaskSidecar {
        /// 任务 ID（携带结果类型，与 [`StoredTask`] 一致）。
        id: TaskId<JsonValue>,
        /// 该 sidecar 内的序号。
        ordinal: usize,
    },
    /// 文档创建（内容在 sidecar）。
    #[serde(rename = "document.create")]
    DocumentCreate {
        /// 创建信息。
        record: DocumentCreate,
        /// sidecar 内序号。
        ordinal: usize,
    },
    /// 文档变更（内容在 sidecar）。
    #[serde(rename = "document.change")]
    DocumentChange {
        /// 化身 ID。
        id: DocumentId,
        /// sidecar 内序号。
        ordinal: usize,
    },
    /// 文档退役（无 sidecar）。
    #[serde(rename = "document.retire")]
    DocumentRetire {
        /// 化身 ID。
        id: DocumentId,
    },
}

/// 对应 `MainMarker`：主文件里的一行提交标记。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MainMarker {
    /// 格式版本。
    pub format: u32,
    /// 固定为 `"commit"`。
    #[serde(rename = "type")]
    pub kind: MarkerKind,
    /// 提交序号。
    pub seq: Seq,
    /// 本次提交的操作。
    pub writes: Vec<MainOperation>,
}

/// 对应 `SidecarPayload`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SidecarPayload {
    /// 任务记录。
    #[serde(rename = "task")]
    Task {
        /// 记录（Box 以避免与 Document 变体的体积差异过大）。
        value: Box<StoredTask>,
    },
    /// 文档内容。
    #[serde(rename = "document")]
    Document {
        /// 化身 ID。
        id: DocumentId,
        /// 内容。
        content: DocumentContent,
    },
}

/// 对应 `SidecarRecord`：sidecar 文件里的一行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SidecarRecord {
    /// 格式版本。
    pub format: u32,
    /// 固定为 `"record"`。
    #[serde(rename = "type")]
    pub kind: RecordKind,
    /// 写入该记录的提交序号。
    pub seq: Seq,
    /// 本次提交内的序号。
    pub ordinal: usize,
    /// 负载。
    pub payload: SidecarPayload,
}

/// 对应 `EncodedCommit`：一次提交的编码结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedCommit {
    /// 主文件要追加的一行（含结尾换行）。
    pub marker: String,
    /// 文件名 → 要追加的内容（每个文件可能是多行）。
    pub sidecars: BTreeMap<String, String>,
}

/// sidecar 的类别（对应文件名前缀）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarKind {
    /// 文档。
    Doc,
    /// 任务。
    Task,
}

impl SidecarKind {
    fn prefix(self) -> &'static str {
        match self {
            SidecarKind::Doc => "doc",
            SidecarKind::Task => "task",
        }
    }
}

/// 对应 `sidecarFileName`。
pub fn sidecar_file_name(kind: SidecarKind, id: u64) -> String {
    format!("{}-{id}.jsonl", kind.prefix())
}

/// 对应 `isSidecarFileName`：`^(?:doc|task)-(?:0|[1-9]\d*)\.jsonl$`。
///
/// 手写解析而非引入正则：语义单一，且避免为此增加依赖。
pub fn is_sidecar_file_name(name: &str) -> bool {
    is_sidecar_like(name, "")
}

/// 对应 `isReclaimFileName`：同上但带 `.reclaim` 后缀。
pub fn is_reclaim_file_name(name: &str) -> bool {
    is_sidecar_like(name, RECLAIM_SUFFIX)
}

fn is_sidecar_like(name: &str, suffix: &str) -> bool {
    // 先从外向内剥离：`doc-1.jsonl.reclaim` 先去 `.reclaim`，再去 `.jsonl`。
    let Some(rest) = name.strip_suffix(suffix) else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(".jsonl") else {
        return false;
    };
    let Some(id) = rest
        .strip_prefix("doc-")
        .or_else(|| rest.strip_prefix("task-"))
    else {
        return false;
    };
    if id.is_empty() {
        return false;
    }
    // 无前导零的十进制数字（`0` 单独允许）。
    if id.len() > 1 && id.starts_with('0') {
        return false;
    }
    id.bytes().all(|byte| byte.is_ascii_digit())
}

/// 对应 `isCurrentOnly`：单例或 latest-only 文档不需要保留历史。
pub fn is_current_only(record: &DocumentCreate) -> bool {
    !matches!(
        record.scope,
        crate::types::DocumentScope::Conversation { .. }
    ) || record.history == Some(crate::types::ConversationHistory::Latest)
}

fn json_line<T: Serialize>(value: &T) -> String {
    let mut line = serde_json::to_string(value).expect("JSONL 记录必须可序列化");
    line.push('\n');
    line
}

/// 对应 `encodeCommit` 可能返回的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// `document.copy` 必须在编码前由 `resolveDocumentCopies()` 展开为 `document.create`
    /// （值已从源物化）。
    UnresolvedDocumentCopy(DocumentId),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::UnresolvedDocumentCopy(id) => write!(
                f,
                "document.copy {id} must be resolved to document.create before encoding"
            ),
        }
    }
}

impl std::error::Error for EncodeError {}

/// 对应 `encodeCommit`：把一次提交编码成主文件行 + sidecar 追加内容。
///
/// 调用方必须先展开 `document.copy`（上游在 `commit()` 开头调 `resolveDocumentCopies()`），
/// 否则返回 [`EncodeError::UnresolvedDocumentCopy`]。
pub fn encode_commit(seq: Seq, writes: &[StorageWrite]) -> Result<EncodedCommit, EncodeError> {
    let mut main_writes: Vec<MainOperation> = Vec::new();
    let mut records: BTreeMap<String, Vec<SidecarRecord>> = BTreeMap::new();
    let mut next_ordinal = 0usize;

    let mut add_sidecar = |file: String, payload: SidecarPayload| -> usize {
        let ordinal = next_ordinal;
        next_ordinal += 1;
        records.entry(file).or_default().push(SidecarRecord {
            format: FORMAT_VERSION,
            kind: RecordKind::Record,
            seq,
            ordinal,
            payload,
        });
        ordinal
    };

    for write in writes {
        match write {
            StorageWrite::Conversation(value) => {
                main_writes.push(MainOperation::Conversation { value: *value });
            }
            StorageWrite::Entry(value) => {
                main_writes.push(MainOperation::Entry {
                    value: value.clone(),
                });
            }
            StorageWrite::Submission(value) => {
                main_writes.push(MainOperation::Submission {
                    value: value.clone(),
                });
            }
            StorageWrite::Task(value) => {
                if value.state.status() == crate::types::TaskStatus::Terminal {
                    // 终态任务体积不再增长，直接落在主文件。
                    main_writes.push(MainOperation::Task {
                        value: value.clone(),
                    });
                } else {
                    let ordinal = add_sidecar(
                        sidecar_file_name(SidecarKind::Task, value.id.get()),
                        SidecarPayload::Task {
                            value: Box::new(value.clone()),
                        },
                    );
                    main_writes.push(MainOperation::TaskSidecar {
                        id: value.id,
                        ordinal,
                    });
                }
            }
            StorageWrite::DocumentCreate { record, content } => {
                let ordinal = add_sidecar(
                    sidecar_file_name(SidecarKind::Doc, record.id.get()),
                    SidecarPayload::Document {
                        id: record.id,
                        content: DocumentContent::Base(content.clone()),
                    },
                );
                main_writes.push(MainOperation::DocumentCreate {
                    record: record.clone(),
                    ordinal,
                });
            }
            StorageWrite::DocumentCopy { record, .. } => {
                // 上游在 `commit()` 开头调 `resolveDocumentCopies()` 把它展开为 `document.create`；
                // 编码阶段不应再看到它。
                return Err(EncodeError::UnresolvedDocumentCopy(record.id));
            }
            StorageWrite::DocumentChange { id, content } => {
                let ordinal = add_sidecar(
                    sidecar_file_name(SidecarKind::Doc, id.get()),
                    SidecarPayload::Document {
                        id: *id,
                        content: content.clone(),
                    },
                );
                main_writes.push(MainOperation::DocumentChange { id: *id, ordinal });
            }
            StorageWrite::DocumentRetire { id } => {
                main_writes.push(MainOperation::DocumentRetire { id: *id });
            }
        }
    }

    let sidecars: BTreeMap<String, String> = records
        .into_iter()
        .map(|(file, file_records)| {
            let content: String = file_records.iter().map(json_line).collect();
            (file, content)
        })
        .collect();

    let marker = MainMarker {
        format: FORMAT_VERSION,
        kind: MarkerKind::Commit,
        seq,
        writes: main_writes,
    };

    Ok(EncodedCommit {
        marker: json_line(&marker),
        sidecars,
    })
}

/// 对应 `sidecarKey`：`[file, seq, ordinal]` 的 JSON 字符串。
pub fn sidecar_key(file: &str, seq: Seq, ordinal: usize) -> String {
    serde_json::to_string(&JsonValue::Array(vec![
        JsonValue::String(file.to_string()),
        JsonValue::from(seq.get()),
        JsonValue::from(ordinal as u64),
    ]))
    .expect("sidecar key 必须可序列化")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ConversationId, DocumentId, EntryId, SubmissionId, TaskOutcome, TaskState, TaskStatus,
    };
    use serde_json::json;

    fn encode(seq: Seq, writes: &[StorageWrite]) -> EncodedCommit {
        encode_commit(seq, writes).expect("编码应成功")
    }

    fn conversation(id: u64) -> ConversationRecord {
        ConversationRecord {
            id: ConversationId::new(id),
            parent: None,
            owner: None,
        }
    }

    fn entry(id: u64) -> EntryRecord {
        EntryRecord {
            id: EntryId::new(id),
            conversation_id: ConversationId::new(1),
            kind: "pi.user".to_string(),
            model: None,
            data: None,
            head: None,
            edits: None,
            by_task_id: None,
        }
    }

    fn task(id: u64, status: TaskStatus) -> StoredTask {
        let state = match status {
            TaskStatus::Terminal => TaskState::Terminal {
                outcome: TaskOutcome::Completed {
                    result: json!(null),
                },
            },
            _ => TaskState::Pending {
                checkpoint: json!({ "phase": "start" }),
            },
        };
        TaskRecord {
            id: TaskId::new(id),
            conversation_id: ConversationId::new(1),
            kind: "demo".to_string(),
            version: 1,
            input: json!({}),
            owner: None,
            background: false,
            abort_requested: false,
            started_at: None,
            ended_at: None,
            state,
            memos: None,
        }
    }

    fn submission(id: u64) -> SubmissionRecord {
        SubmissionRecord::Write {
            identity: crate::types::SubmissionIdentity {
                id: SubmissionId::new(id),
                conversation_id: ConversationId::new(1),
                request_id: None,
            },
            status: crate::types::WriteSubmissionStatus::Queued,
        }
    }

    #[test]
    fn marker_carries_small_records_directly() {
        let encoded = encode(
            Seq::new(1),
            &[
                StorageWrite::Conversation(conversation(1)),
                StorageWrite::Entry(entry(2)),
                StorageWrite::Submission(submission(3)),
            ],
        );

        assert!(encoded.sidecars.is_empty(), "这些记录不需要 sidecar");
        let decoded: MainMarker = serde_json::from_str(encoded.marker.trim_end()).unwrap();
        assert_eq!(decoded.format, FORMAT_VERSION);
        assert_eq!(decoded.kind, MarkerKind::Commit);
        assert_eq!(decoded.seq, Seq::new(1));
        assert_eq!(decoded.writes.len(), 3);
        assert!(matches!(
            decoded.writes[0],
            MainOperation::Conversation { .. }
        ));
    }

    #[test]
    fn terminal_task_goes_to_main_and_live_task_to_sidecar() {
        let encoded = encode(
            Seq::new(2),
            &[
                StorageWrite::Task(task(10, TaskStatus::Pending)),
                StorageWrite::Task(task(11, TaskStatus::Terminal)),
            ],
        );

        // 只有非终态任务产生 sidecar。
        assert_eq!(encoded.sidecars.len(), 1);
        assert!(encoded.sidecars.contains_key("task-10.jsonl"));

        let decoded: MainMarker = serde_json::from_str(encoded.marker.trim_end()).unwrap();
        assert_eq!(decoded.writes.len(), 2);
        match &decoded.writes[0] {
            MainOperation::TaskSidecar { id, ordinal } => {
                assert_eq!(id.get(), 10);
                assert_eq!(*ordinal, 0);
            }
            other => panic!("expected task.sidecar, got {other:?}"),
        }
        assert!(matches!(decoded.writes[1], MainOperation::Task { .. }));
    }

    #[test]
    fn document_writes_use_sidecar_files() {
        let mut value = crate::types::JsonObject::new();
        value.insert("n".to_string(), json!(1));

        let encoded = encode(
            Seq::new(3),
            &[
                StorageWrite::DocumentCreate {
                    record: DocumentCreate {
                        id: DocumentId::new(7),
                        kind: "demo.session".to_string(),
                        key: None,
                        history: None,
                        fork: None,
                        scope: crate::types::DocumentScope::Session,
                    },
                    content: crate::types::DocumentBase { version: 1, value },
                },
                StorageWrite::DocumentRetire {
                    id: DocumentId::new(8),
                },
            ],
        );

        assert!(encoded.sidecars.contains_key("doc-7.jsonl"));
        let sidecar = encoded.sidecars.get("doc-7.jsonl").unwrap();
        let record: SidecarRecord = serde_json::from_str(sidecar.trim_end()).unwrap();
        assert_eq!(record.format, FORMAT_VERSION);
        assert_eq!(record.kind, RecordKind::Record);
        assert_eq!(record.ordinal, 0);
        assert!(matches!(record.payload, SidecarPayload::Document { .. }));

        let decoded: MainMarker = serde_json::from_str(encoded.marker.trim_end()).unwrap();
        assert!(matches!(
            decoded.writes[0],
            MainOperation::DocumentCreate { .. }
        ));
        assert!(matches!(
            decoded.writes[1],
            MainOperation::DocumentRetire { .. }
        ));
    }

    #[test]
    fn ordinals_increase_across_sidecars() {
        let encoded = encode(
            Seq::new(4),
            &[
                StorageWrite::Task(task(1, TaskStatus::Pending)),
                StorageWrite::Task(task(2, TaskStatus::Pending)),
            ],
        );
        let first: SidecarRecord =
            serde_json::from_str(encoded.sidecars["task-1.jsonl"].trim_end()).unwrap();
        let second: SidecarRecord =
            serde_json::from_str(encoded.sidecars["task-2.jsonl"].trim_end()).unwrap();
        assert_eq!(first.ordinal, 0);
        assert_eq!(second.ordinal, 1, "ordinal 在整次提交内全局递增");
    }

    #[test]
    fn marker_json_uses_upstream_field_names() {
        let encoded = encode(Seq::new(5), &[StorageWrite::Conversation(conversation(1))]);
        let value: JsonValue = serde_json::from_str(encoded.marker.trim_end()).unwrap();
        assert_eq!(value["type"], json!("commit"));
        assert_eq!(value["format"], json!(1));
        assert_eq!(value["writes"][0]["type"], json!("conversation"));
    }

    #[test]
    fn sidecar_file_names_are_validated() {
        assert!(is_sidecar_file_name("doc-1.jsonl"));
        assert!(is_sidecar_file_name("task-0.jsonl"));
        assert!(is_sidecar_file_name("task-1234567.jsonl"));
        assert!(!is_sidecar_file_name("task-01.jsonl"), "不允许前导零");
        assert!(!is_sidecar_file_name("task-.jsonl"));
        assert!(!is_sidecar_file_name("other-1.jsonl"));
        assert!(!is_sidecar_file_name("task-1.json"));

        assert!(is_reclaim_file_name("doc-1.jsonl.reclaim"));
        assert!(!is_reclaim_file_name("doc-1.jsonl"));
    }

    #[test]
    fn sidecar_key_matches_upstream_shape() {
        assert_eq!(
            sidecar_key("doc-1.jsonl", Seq::new(2), 3),
            r#"["doc-1.jsonl",2,3]"#,
        );
    }
}
