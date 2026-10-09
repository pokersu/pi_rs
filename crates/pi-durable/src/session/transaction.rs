//! 对应 `src/session/transaction.ts`：一次 Session 提交回调的事务。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游 `Draft<T>` 是 Proxy，Rust 用共享 [`Draft`]（`Arc<Mutex<Option<Change>>>`），
//!   事务与调用方共享同一份变更（见 `types.rs` 的说明）。
//! - 上游用 `Set<Promise>` 跟踪未完成操作，并在 `settleSuccess` 时拒绝「回调先于挂起操作结束」；
//!   Rust 的 future 是惰性的，调用方必须 `await` 才能取得结果，因此这一保护不适用。
//! - 上游的 `#write()` 在**调用时**同步标记 `hasTableWrite`；Rust 的 `async fn` 体在首次
//!   poll 时执行。实践中调用方立刻 `await`，时序差异仅在「先构造多个 future 再 await」时可见。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::Value as JsonValue;

use crate::chord::context::Context;
use crate::chord::delta::Op;
use crate::chord::tracker::{Prepared, Tracker, track};
use crate::documents::{
    AnyDocToken, DocError, ResolvedAddress, address_id, check_record_scope, check_record_version,
    document_create, materialize_document_value, resolve_address,
};
use crate::errors::ReadAfterWrite;
use crate::session::SessionError;
use crate::session::forks::prepare_fork_document_copies;
use crate::types::DocDefinitionSpec;
use crate::types::{
    ConversationId, ConversationOwnership, ConversationQuery, ConversationRecord, Cursor,
    DocumentAddress, DocumentCommitChange, DocumentCopySource, DocumentCreate, DocumentId,
    DocumentQuery, DocumentRecord, DocumentScope, Draft, EntryDraft, EntryId, EntryQuery,
    EntryRecord, JsonObject, ROOT_CONVERSATION_ID, Seq, Storage, StorageWrite, SubmissionCreate,
    SubmissionId, SubmissionRecord, SubmissionSettlement, Task, TaskId, TaskOptions, TaskQuery,
    TaskRecord,
};

/// 对应 `AnyTaskRecord`。
pub type AnyTaskRecord = TaskRecord<JsonValue, JsonValue, JsonValue>;

/// 对应 `SubmissionChange`：一次被暂存的提交变更。
#[derive(Debug, Clone, PartialEq)]
pub enum SubmissionChange {
    /// 结算。
    Settlement(SubmissionSettlement),
    /// 把 queued 提交放到某个条目上。
    Placed {
        /// 目标条目。
        entry: EntryId,
    },
}

/// 对应 `applySubmissionChange`：把一次变更应用到提交记录上。
///
/// 放置把 queued 的 input 变为 `placed`、queued 的 write 变为 `done`；只有 placed 的 input 能被应答。
/// 已结算的记录保持不变。
pub fn apply_submission_change(
    current: &SubmissionRecord,
    change: &SubmissionChange,
) -> Result<SubmissionRecord, SessionError> {
    if matches!(
        current.status(),
        crate::types::SubmissionStatus::Done | crate::types::SubmissionStatus::Unanswered
    ) {
        return Ok(current.clone());
    }
    match change {
        SubmissionChange::Placed { entry } => {
            if current.status() != crate::types::SubmissionStatus::Queued {
                return Err(SessionError::Message(format!(
                    "Submission {} is not queued",
                    current.identity().id
                )));
            }
            Ok(match current {
                SubmissionRecord::Input { identity, .. } => SubmissionRecord::Input {
                    identity: identity.clone(),
                    status: crate::types::InputSubmissionStatus::Placed { entry: *entry },
                },
                SubmissionRecord::Write { identity, .. } => SubmissionRecord::Write {
                    identity: identity.clone(),
                    status: crate::types::WriteSubmissionStatus::Done { entry: *entry },
                },
            })
        }
        SubmissionChange::Settlement(settlement) => {
            if matches!(settlement, SubmissionSettlement::Done { .. })
                && current.status() != crate::types::SubmissionStatus::Placed
            {
                return Err(SessionError::Message(format!(
                    "Submission {} is not a placed input",
                    current.identity().id
                )));
            }
            Ok(match (current, settlement) {
                (
                    SubmissionRecord::Input { identity, status },
                    SubmissionSettlement::Done { answer },
                ) => {
                    let entry = match status {
                        crate::types::InputSubmissionStatus::Placed { entry } => *entry,
                        _ => {
                            return Err(SessionError::Message(format!(
                                "Submission {} is not a placed input",
                                identity.id
                            )));
                        }
                    };
                    SubmissionRecord::Input {
                        identity: identity.clone(),
                        status: crate::types::InputSubmissionStatus::Done {
                            entry,
                            answer: *answer,
                        },
                    }
                }
                (
                    SubmissionRecord::Input { identity, status },
                    SubmissionSettlement::Unanswered { reason, detail },
                ) => {
                    let entry = match status {
                        crate::types::InputSubmissionStatus::Placed { entry } => Some(*entry),
                        crate::types::InputSubmissionStatus::Queued => None,
                        _ => None,
                    };
                    SubmissionRecord::Input {
                        identity: identity.clone(),
                        status: crate::types::InputSubmissionStatus::Unanswered {
                            entry,
                            reason: reason.clone(),
                            detail: detail.clone(),
                        },
                    }
                }
                (
                    SubmissionRecord::Write { identity, .. },
                    SubmissionSettlement::Unanswered { reason, detail },
                ) => SubmissionRecord::Write {
                    identity: identity.clone(),
                    status: crate::types::WriteSubmissionStatus::Unanswered {
                        reason: reason.clone(),
                        detail: detail.clone(),
                    },
                },
                (SubmissionRecord::Write { identity, .. }, SubmissionSettlement::Done { .. }) => {
                    return Err(SessionError::Message(format!(
                        "Submission {} is not a placed input",
                        identity.id
                    )));
                }
            })
        }
    }
}

/// 对应 `INTERNAL_SCAN_PAGE_SIZE`。
const INTERNAL_SCAN_PAGE_SIZE: usize = 256;

/// 对应 `EMPTY_OPERATIONS`。
fn empty_operations() -> Vec<Op> {
    Vec::new()
}

/// 对应 `LoadedDocument`：Session tracker 缓存拥有的一个已提交化身。
#[derive(Clone)]
pub struct LoadedDocument {
    /// 逻辑地址身份。
    pub address_id: String,
    /// 化身记录。
    pub record: DocumentRecord,
    /// 持久化的定义版本（跟踪值只在内存迁移时可能更旧）。
    pub stored_version: u32,
    /// 跟踪值形态对应的定义版本。
    pub value_version: u32,
    /// 最新 base 之后的已存增量数；采纳后前进，使下次谓词调用无需读取。
    pub deltas_since_base: usize,
    /// tracker。
    pub tracker: Arc<Mutex<Tracker>>,
}

/// 对应 `TransactionHost`：事务持有提交线期间使用的 Session 服务。
#[async_trait::async_trait]
pub trait TransactionHost: Send + Sync {
    /// 存储后端。
    fn storage(&self) -> &dyn Storage;

    /// 任务生命周期时间的墙钟。
    fn now(&self) -> u64;

    /// 返回缓存的当前化身（不加载）。
    fn cached(&self, address_id: &str) -> Option<LoadedDocument>;

    /// 返回缓存的当前化身，必要时冷加载并迁移。
    async fn load(
        &self,
        definition: &Arc<dyn DocDefinitionSpec>,
        address_id: &str,
        address: &DocumentAddress,
        context: &dyn Context,
    ) -> Result<Option<LoadedDocument>, SessionError>;

    /// 安装一个新提交的化身。
    fn install(&self, document: LoadedDocument);

    /// 若被退役的化身仍是其地址的缓存占用者，则移除它。
    fn evict(&self, address_id: &str, record_id: DocumentId);

    /// 暂存属于每个新建/分叉会话的写入（在其创建事务内）。
    async fn conversation_created(
        &self,
        tx: &Transaction,
        record: &ConversationRecord,
    ) -> Result<(), SessionError>;
}

/// 对应 `TransactionScope`：一次提交绑定的默认值。
#[derive(Debug, Clone, Copy, Default)]
pub struct TransactionScope {
    /// `tx.createTask()` 的默认会话。
    pub conversation_id: Option<ConversationId>,
    /// 本次运行时提交对应的任务；追加条目时盖 `byTaskId`。
    pub task_id: Option<TaskId>,
}

/// 对应 `TransactionTask`：本次事务触及的某个任务的已提交/候选状态。
#[derive(Default)]
struct TransactionTask {
    committed_read: Option<AnyTaskRecord>,
    write: Option<TaskWrite>,
    publication_conversation_id: Option<ConversationId>,
}

struct TaskWrite {
    kind: TaskWriteKind,
    record: AnyTaskRecord,
}

#[derive(PartialEq, Eq)]
enum TaskWriteKind {
    Create,
    Replace,
}

/// 对应 `DocumentTarget`：一个已暂存化身在存储/缓存中的来源。
enum DocumentTarget {
    Loaded(LoadedDocument),
    Created {
        record: DocumentCreate,
        version: u32,
        tracker: Arc<Mutex<Tracker>>,
    },
    ForkCopy {
        record: DocumentCreate,
        source: DocumentCopySource,
    },
    RetireOnly(DocumentRecord),
}

/// 上游 `DocumentPlan.record` 是 `DocumentCreate | DocumentRecord`。
#[derive(Clone)]
enum DocumentRecordOrCreate {
    Creating(DocumentCreate),
    Existing(DocumentRecord),
}

impl DocumentRecordOrCreate {
    fn id(&self) -> DocumentId {
        match self {
            DocumentRecordOrCreate::Creating(record) => record.id,
            DocumentRecordOrCreate::Existing(record) => record.id,
        }
    }

    fn scope(&self) -> DocumentScope {
        match self {
            DocumentRecordOrCreate::Creating(record) => record.scope,
            DocumentRecordOrCreate::Existing(record) => record.scope,
        }
    }
}

/// 对应 `DocumentEntry`：本次事务获取、创建或退役的一个化身。
struct DocumentEntry {
    address_id: String,
    address: DocumentAddress,
    definition: Option<Arc<dyn DocDefinitionSpec>>,
    draft: Option<Draft>,
    target: Option<DocumentTarget>,
    prepared: Option<Prepared>,
    retire_on_commit: bool,
}

/// 对应 `DocumentPlan.change`。
struct PlanChange {
    tracker: Arc<Mutex<Tracker>>,
    prepared: Prepared,
    version: u32,
    loaded: Option<LoadedDocument>,
    definition: Option<Arc<dyn DocDefinitionSpec>>,
}

/// 对应 `DocumentPlan`：一个已暂存化身写入并发布的内容，在存储接纳前一次性决定。
struct DocumentPlan {
    address_id: String,
    record: DocumentRecordOrCreate,
    retire: bool,
    content: Option<StorageWrite>,
    change: Option<PlanChange>,
    conversation_id: Option<ConversationId>,
}

/// 对应 `Transaction`：一次 Session 提交回调的事务。
pub struct Transaction {
    host: Arc<dyn TransactionHost>,
    context: Arc<dyn Context>,
    scope: TransactionScope,
    state: Mutex<TxState>,
}

struct TxState {
    sealed: bool,
    has_table_write: bool,
    writes: Vec<StorageWrite>,
    created_conversation_ids: BTreeSet<u64>,
    fork_source_conversation_ids: BTreeSet<u64>,
    fork_source_document_ids: BTreeSet<u64>,
    tasks_by_id: BTreeMap<u64, TransactionTask>,
    submissions: BTreeMap<u64, SubmissionRecord>,
    submission_changes: Vec<(SubmissionId, SubmissionChange)>,
    plans: Vec<DocumentPlan>,
    documents: Vec<DocumentEntry>,
    latest_document_by_address: BTreeMap<String, usize>,
}

impl Transaction {
    /// 对应 `new Transaction(host, context, scope)`。
    pub fn new(
        host: Arc<dyn TransactionHost>,
        context: Arc<dyn Context>,
        scope: TransactionScope,
    ) -> Self {
        Self {
            host,
            context,
            scope,
            state: Mutex::new(TxState {
                sealed: false,
                has_table_write: false,
                writes: Vec::new(),
                created_conversation_ids: BTreeSet::new(),
                fork_source_conversation_ids: BTreeSet::new(),
                fork_source_document_ids: BTreeSet::new(),
                tasks_by_id: BTreeMap::new(),
                submissions: BTreeMap::new(),
                submission_changes: Vec::new(),
                plans: Vec::new(),
                documents: Vec::new(),
                latest_document_by_address: BTreeMap::new(),
            }),
        }
    }

    fn context(&self) -> &dyn Context {
        self.context.as_ref()
    }

    fn host(&self) -> &dyn TransactionHost {
        self.host.as_ref()
    }

    fn storage(&self) -> &dyn Storage {
        self.host.storage()
    }

    fn assert_open(&self) -> Result<(), SessionError> {
        if self.state.lock().expect("tx").sealed {
            return Err(SessionError::Message("Transaction has settled".to_string()));
        }
        Ok(())
    }

    fn read<T>(&self, method: &str, job: impl FnOnce() -> T) -> Result<T, SessionError> {
        let state = self.state.lock().expect("tx");
        if state.sealed {
            return Err(SessionError::Message("Transaction has settled".to_string()));
        }
        if state.has_table_write {
            return Err(SessionError::ReadAfterWrite(ReadAfterWrite::new(method)));
        }
        drop(state);
        Ok(job())
    }

    fn begin_write(&self) {
        let mut state = self.state.lock().expect("tx");
        state.has_table_write = true;
    }

    // ─── 表读取 ──────────────────────────────────────────────────────────────

    /// 对应 `conversation(id)`。
    pub async fn conversation(
        &self,
        id: ConversationId,
    ) -> Result<Option<ConversationRecord>, SessionError> {
        self.read("conversation", || ())?;
        Ok(self.storage().conversation(id, self.context()).await?)
    }

    /// 对应 `entry(id)` / `entry(token, id)`。
    pub async fn entry(
        &self,
        kind: Option<&str>,
        id: EntryId,
    ) -> Result<Option<EntryRecord>, SessionError> {
        self.read("entry", || ())?;
        let stored = self.storage().entry(id, self.context()).await?;
        Ok(match stored {
            None => None,
            Some((record, _)) => match kind {
                None => Some(record),
                Some(kind) if record.kind == kind => Some(record),
                Some(_) => None,
            },
        })
    }

    /// 对应 `task(id)`。
    pub async fn task(&self, id: TaskId) -> Result<Option<AnyTaskRecord>, SessionError> {
        self.read("task", || ())?;
        self.committed_task(id).await
    }

    /// 对应 `scanConversations`。
    pub async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> Result<crate::types::Page<ConversationRecord, Cursor>, SessionError> {
        self.read("scanConversations", || ())?;
        Ok(self
            .storage()
            .scan_conversations(query, limit, cursor, self.context())
            .await?)
    }

    /// 对应 `scanEntries`。
    pub async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> Result<crate::types::Page<EntryRecord, Cursor>, SessionError> {
        self.read("scanEntries", || ())?;
        Ok(self
            .storage()
            .scan_entries(query, limit, cursor, self.context())
            .await?)
    }

    /// 对应 `latestHeadMarker`。
    pub async fn latest_head_marker(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Option<(EntryRecord, EntryId)>, SessionError> {
        self.read("latestHeadMarker", || ())?;
        Ok(self
            .storage()
            .find_latest_head_marker(conversation_id, None, self.context())
            .await?)
    }

    /// 对应 `scanTasks`。
    pub async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> Result<crate::types::Page<AnyTaskRecord, Cursor>, SessionError> {
        self.read("scanTasks", || ())?;
        Ok(self
            .storage()
            .scan_tasks(query, limit, cursor, self.context())
            .await?)
    }

    /// 对应 `submission(id)`：按 ID 查一个已接纳提交。
    pub async fn submission(
        &self,
        id: SubmissionId,
    ) -> Result<Option<SubmissionRecord>, SessionError> {
        self.read("submission", || ())?;
        Ok(self.storage().submission(id, self.context()).await?)
    }

    /// 对应 `submissionByRequest`。
    pub async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>, SessionError> {
        self.read("submissionByRequest", || ())?;
        Ok(self
            .storage()
            .submission_by_request(conversation_id, request_id, self.context())
            .await?)
    }

    // ─── 表写入 ──────────────────────────────────────────────────────────────

    /// 对应 `createConversation`。
    pub async fn create_conversation(
        &self,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord, SessionError> {
        self.begin_write();
        self.stage_conversation(None, ownership, None).await
    }

    /// 对应 `createRootConversation`。
    pub async fn create_root_conversation(&self) -> Result<ConversationRecord, SessionError> {
        self.begin_write();
        self.stage_conversation(
            None,
            ConversationOwnership::Ownerless,
            Some(ROOT_CONVERSATION_ID),
        )
        .await
    }

    /// 对应 `forkConversation`。
    pub async fn fork_conversation(
        &self,
        parent_conversation_id: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord, SessionError> {
        self.begin_write();
        self.stage_conversation(Some((parent_conversation_id, at)), ownership, None)
            .await
    }

    async fn stage_conversation(
        &self,
        parent: Option<(ConversationId, EntryId)>,
        ownership: ConversationOwnership,
        reserved_id: Option<ConversationId>,
    ) -> Result<ConversationRecord, SessionError> {
        let owner_task_id = match ownership {
            ConversationOwnership::Task { task_id } => Some(task_id),
            _ => None,
        };
        let id = match reserved_id {
            Some(id) => id,
            None => ConversationId::new(self.storage().mint_id().await),
        };
        self.assert_open()?;
        let owner = match owner_task_id {
            None => None,
            Some(task_id) => {
                let task = self.current_task(task_id).await?;
                self.assert_open()?;
                let Some(task) = task else {
                    return Err(SessionError::Message(format!(
                        "Conversation owner task {task_id} does not exist"
                    )));
                };
                Some(crate::types::ConversationOwner {
                    conversation_id: task.conversation_id,
                    task_id,
                })
            }
        };
        let record = ConversationRecord {
            id,
            parent: parent.map(|(conversation_id, at)| crate::types::ConversationParent {
                conversation_id,
                at,
            }),
            owner,
        };
        let copies = match parent {
            None => Vec::new(),
            Some((parent_conversation_id, at)) => {
                prepare_fork_document_copies(
                    self.storage(),
                    parent_conversation_id,
                    at,
                    id,
                    self.context(),
                )
                .await?
            }
        };
        self.assert_open()?;

        {
            let mut state = self.state.lock().expect("tx");
            for copy in copies {
                state.fork_source_document_ids.insert(copy.source.id.get());
                let address = DocumentAddress {
                    kind: copy.record.kind.clone(),
                    scope: copy.record.scope,
                    key: copy.record.key.clone(),
                };
                let entry = DocumentEntry {
                    address_id: address_id(&address),
                    address,
                    definition: None,
                    draft: None,
                    target: Some(DocumentTarget::ForkCopy {
                        record: copy.record,
                        source: copy.source,
                    }),
                    prepared: None,
                    retire_on_commit: false,
                };
                let index = state.documents.len();
                state
                    .latest_document_by_address
                    .insert(entry.address_id.clone(), index);
                state.documents.push(entry);
            }
            if let Some((parent_conversation_id, _)) = parent {
                state
                    .fork_source_conversation_ids
                    .insert(parent_conversation_id.get());
            }
            state.created_conversation_ids.insert(id.get());
            state.writes.push(StorageWrite::Conversation(record));
        }

        self.host.conversation_created(self, &record).await?;
        self.assert_open()?;
        Ok(record)
    }

    /// 对应 `appendEntry`。
    pub async fn append_entry(
        &self,
        kind: Option<String>,
        conversation_id: ConversationId,
        value: EntryDraft,
    ) -> Result<EntryRecord, SessionError> {
        self.begin_write();
        self.require_conversation(conversation_id).await?;
        self.assert_open()?;
        let id = EntryId::new(self.storage().mint_id().await);
        self.assert_open()?;
        let head = match value.head {
            Some(crate::types::EntryHead::SelfEntry) => Some(id),
            Some(crate::types::EntryHead::Entry(entry)) => Some(entry),
            None => None,
        };
        let record = EntryRecord {
            id,
            conversation_id,
            kind: kind.unwrap_or(value.kind),
            model: value.model,
            data: value.data,
            head,
            edits: value.edits,
            by_task_id: self.scope.task_id,
        };
        self.state
            .lock()
            .expect("tx")
            .writes
            .push(StorageWrite::Entry(record.clone()));
        Ok(record)
    }

    /// 对应 `createTask`。
    pub async fn create_task(
        &self,
        task: &Task,
        input: JsonValue,
        options: TaskOptions,
    ) -> Result<TaskId, SessionError> {
        self.begin_write();
        let ownership = options.ownership;
        let mut owner: Option<AnyTaskRecord> = None;
        if let crate::types::TaskOwnership::Task { task_id } = ownership {
            owner = self.current_task(task_id).await?;
            self.assert_open()?;
            let Some(record) = &owner else {
                return Err(SessionError::Message(format!(
                    "Task owner {task_id} does not exist"
                )));
            };
            if options.background == Some(true) {
                return Err(SessionError::Type(
                    "A child task cannot be background".to_string(),
                ));
            }
            if let Some(conversation_id) = options.conversation_id
                && conversation_id != record.conversation_id
            {
                return Err(SessionError::Message(format!(
                    "A child task lives in its owner's conversation {}",
                    record.conversation_id
                )));
            }
        }
        let conversation_id = owner
            .as_ref()
            .map(|record| record.conversation_id)
            .or(options.conversation_id)
            .or(self.scope.conversation_id)
            .ok_or_else(|| {
                SessionError::Type("Tx.createTask() requires options.conversationId".to_string())
            })?;
        self.require_conversation(conversation_id).await?;
        self.assert_open()?;
        let definition = task.definition();
        let checkpoint = definition.initial(&input);
        let id = self.storage().mint_id().await;
        self.assert_open()?;
        let record: AnyTaskRecord = TaskRecord {
            id: TaskId::new(id),
            conversation_id,
            kind: definition.name().to_string(),
            version: definition.version(),
            input,
            owner: owner.as_ref().map(|record| TaskId::new(record.id.get())),
            background: options.background.unwrap_or(false),
            abort_requested: false,
            started_at: None,
            ended_at: None,
            state: crate::types::TaskState::Pending { checkpoint },
            memos: None,
        };
        self.state.lock().expect("tx").tasks_by_id.insert(
            id,
            TransactionTask {
                write: Some(TaskWrite {
                    kind: TaskWriteKind::Create,
                    record,
                }),
                ..Default::default()
            },
        );
        Ok(TaskId::new(id))
    }

    /// 对应 `createSubmission`。
    pub async fn create_submission(
        &self,
        create: SubmissionCreate,
    ) -> Result<SubmissionRecord, SessionError> {
        self.begin_write();
        let conversation_id = match &create {
            SubmissionCreate::Input {
                conversation_id, ..
            }
            | SubmissionCreate::Write {
                conversation_id, ..
            } => *conversation_id,
        };
        self.require_conversation(conversation_id).await?;
        self.assert_open()?;
        let id = SubmissionId::new(self.storage().mint_id().await);
        self.assert_open()?;
        let record = match create {
            SubmissionCreate::Input {
                conversation_id,
                request_id,
                status,
            } => SubmissionRecord::Input {
                identity: crate::types::SubmissionIdentity {
                    id,
                    conversation_id,
                    request_id,
                },
                status,
            },
            SubmissionCreate::Write {
                conversation_id,
                request_id,
                status,
            } => SubmissionRecord::Write {
                identity: crate::types::SubmissionIdentity {
                    id,
                    conversation_id,
                    request_id,
                },
                status,
            },
        };
        self.state
            .lock()
            .expect("tx")
            .submissions
            .insert(id.get(), record.clone());
        Ok(record)
    }

    /// 对应 `settleSubmission`（同步）。
    pub fn settle_submission(&self, id: SubmissionId, settlement: SubmissionSettlement) {
        self.assert_open().expect("transaction must be open");
        let mut state = self.state.lock().expect("tx");
        state.has_table_write = true;
        state
            .submission_changes
            .push((id, SubmissionChange::Settlement(settlement)));
    }

    /// 对应 `placeSubmission`（同步）。
    pub fn place_submission(&self, id: SubmissionId, entry: EntryId) {
        self.assert_open().expect("transaction must be open");
        let mut state = self.state.lock().expect("tx");
        state.has_table_write = true;
        state
            .submission_changes
            .push((id, SubmissionChange::Placed { entry }));
    }

    /// 对应 `setTask`（同步）。
    pub fn set_task(&self, value: AnyTaskRecord) {
        self.assert_open().expect("transaction must be open");
        let mut state = self.state.lock().expect("tx");
        state.has_table_write = true;
        let entry = state.tasks_by_id.entry(value.id.get()).or_default();
        if let Some(candidate) = &entry.write
            && candidate.record.state.status() == crate::types::TaskStatus::Terminal
        {
            panic!("Task {} already has a terminal candidate", value.id);
        }
        if let Some(candidate) = &entry.write
            && candidate.record.conversation_id != value.conversation_id
        {
            panic!("Task {} cannot change conversations", value.id);
        }
        let kind = match entry.write.as_ref().map(|write| &write.kind) {
            Some(TaskWriteKind::Create) => TaskWriteKind::Create,
            _ => TaskWriteKind::Replace,
        };
        let candidate = entry.write.as_ref().map(|write| write.record.clone());
        entry.write = Some(TaskWrite {
            kind,
            record: stamp_times(self.host.now(), value, candidate.as_ref()),
        });
    }

    /// 对应 `stagedTasks`。
    pub fn staged_tasks(&self) -> Vec<AnyTaskRecord> {
        self.state
            .lock()
            .expect("tx")
            .tasks_by_id
            .values()
            .filter_map(|task| task.write.as_ref().map(|write| write.record.clone()))
            .collect()
    }

    /// 对应 `stagedConversations`。
    pub fn staged_conversations(&self) -> Vec<ConversationRecord> {
        self.state
            .lock()
            .expect("tx")
            .writes
            .iter()
            .filter_map(|write| match write {
                StorageWrite::Conversation(record) => Some(*record),
                _ => None,
            })
            .collect()
    }

    // ─── 文档 ────────────────────────────────────────────────────────────────

    /// 对应 `doc(token, ...)`。
    pub async fn doc(
        &self,
        token: &dyn AnyDocToken,
        access: crate::types::DocAccess,
        seed: Option<JsonValue>,
    ) -> Result<Draft, DocError> {
        self.assert_open().map_err(doc_error)?;
        let definition = Arc::clone(token.definition());
        let resolved = resolve_address(definition.as_ref(), access.owner, access.key)?;
        self.assert_task_documents_open(&resolved)?;

        // 已有本事务内的最新化身：复用草稿；若是分叉拷贝则读取其源。
        let latest = {
            let state = self.state.lock().expect("tx");
            state
                .latest_document_by_address
                .get(&resolved.id)
                .copied()
                .and_then(|index| {
                    state.documents.get(index).map(|entry| {
                        (
                            index,
                            entry.retire_on_commit,
                            entry.draft.clone(),
                            matches!(entry.target, Some(DocumentTarget::ForkCopy { .. })),
                        )
                    })
                })
        };
        if let Some((index, false, draft, is_fork)) = latest {
            if let Some(draft) = draft {
                return Ok(draft);
            }
            if is_fork {
                return self
                    .acquire_fork_copy(index, definition)
                    .await
                    .map_err(doc_error);
            }
        }

        let is_family = definition.is_family();
        let seed = if is_family { seed } else { None };
        let entry = DocumentEntry {
            address_id: resolved.id.clone(),
            address: resolved.address,
            definition: Some(definition),
            draft: None,
            target: None,
            prepared: None,
            retire_on_commit: false,
        };
        let index = {
            let mut state = self.state.lock().expect("tx");
            let index = state.documents.len();
            state
                .latest_document_by_address
                .insert(resolved.id.clone(), index);
            state.documents.push(entry);
            index
        };
        self.acquire(index, seed).await.map_err(doc_error)
    }

    /// 对应 `retireDoc(token, ...)`。
    pub async fn retire_doc(
        &self,
        token: &dyn AnyDocToken,
        access: crate::types::DocAccess,
    ) -> Result<(), DocError> {
        self.assert_open().map_err(doc_error)?;
        let definition = Arc::clone(token.definition());
        let resolved = resolve_address(definition.as_ref(), access.owner, access.key)?;

        let (draft, is_fork_copy) = {
            let state = self.state.lock().expect("tx");
            match state
                .latest_document_by_address
                .get(&resolved.id)
                .and_then(|index| state.documents.get(*index))
            {
                Some(entry) if entry.retire_on_commit => return Ok(()),
                Some(entry) => (
                    entry.draft.clone(),
                    matches!(entry.target, Some(DocumentTarget::ForkCopy { .. })),
                ),
                None => (None, false),
            }
        };
        if is_fork_copy {
            return Ok(());
        }
        if let Some(draft) = draft {
            // 已获取草稿的回收：先持久化其最终内容，再退役。
            let _ = draft;
            let mut state = self.state.lock().expect("tx");
            if let Some(index) = state.latest_document_by_address.get(&resolved.id).copied()
                && let Some(entry) = state.documents.get_mut(index)
            {
                entry.retire_on_commit = true;
            }
            return Ok(());
        }
        let entry = DocumentEntry {
            address_id: resolved.id.clone(),
            address: resolved.address,
            definition: Some(definition),
            draft: None,
            target: None,
            prepared: None,
            retire_on_commit: true,
        };
        let index = {
            let mut state = self.state.lock().expect("tx");
            let index = state.documents.len();
            state
                .latest_document_by_address
                .insert(resolved.id.clone(), index);
            state.documents.push(entry);
            index
        };
        self.find_retirement(index).await.map_err(doc_error)
    }

    /// 对应 `#acquireForkCopy`：读取分叉源文档，以其值建立 tracker。
    async fn acquire_fork_copy(
        &self,
        index: usize,
        definition: Arc<dyn DocDefinitionSpec>,
    ) -> Result<Draft, SessionError> {
        let (target_record, source) = {
            let state = self.state.lock().expect("tx");
            match &state.documents[index].target {
                Some(DocumentTarget::ForkCopy { record, source }) => (record.clone(), *source),
                _ => unreachable!("fork copy entry"),
            }
        };
        let stored = self
            .storage()
            .document(source.id, source.at, self.context())
            .await?;
        self.assert_open()?;
        let Some(stored) = stored else {
            return Err(SessionError::Message(format!(
                "Fork source document {} cannot be read",
                source.id
            )));
        };
        let matches_source = matches!(stored.record.scope, DocumentScope::Conversation { .. })
            && stored.record.kind == target_record.kind
            && stored.record.key == target_record.key
            && stored.record.history == target_record.history
            && stored.record.fork == target_record.fork;
        if !matches_source {
            return Err(SessionError::Message(format!(
                "Fork source document {} does not match the copied record",
                source.id
            )));
        }
        let value = materialize_document_value(
            definition.as_ref(),
            &target_record,
            stored.version,
            stored.value,
        )?;
        let version = definition.version();
        let tracker = Arc::new(Mutex::new(track(JsonValue::Object(value))));
        let change = tracker.lock().expect("tracker").begin_change();
        let draft = Draft::new(change);
        let mut state = self.state.lock().expect("tx");
        let entry = &mut state.documents[index];
        entry.definition = Some(definition);
        entry.target = Some(DocumentTarget::Created {
            record: target_record,
            version,
            tracker,
        });
        entry.draft = Some(draft.clone());
        Ok(draft)
    }

    async fn acquire(&self, index: usize, seed: Option<JsonValue>) -> Result<Draft, SessionError> {
        let (definition, address_id, address) = {
            let state = self.state.lock().expect("tx");
            let entry = &state.documents[index];
            (
                Arc::clone(entry.definition.as_ref().expect("definition")),
                entry.address_id.clone(),
                entry.address.clone(),
            )
        };
        let loaded = self
            .host()
            .load(&definition, &address_id, &address, self.context())
            .await?;
        self.assert_open()?;
        if let Some(loaded) = loaded {
            check_record_scope(definition.as_ref(), &loaded.record).map_err(SessionError::Doc)?;
            check_record_version(definition.as_ref(), &loaded.record, loaded.stored_version)
                .map_err(SessionError::Doc)?;
            let tracker = Arc::clone(&loaded.tracker);
            let change = tracker.lock().expect("tracker").begin_change();
            let draft = Draft::new(change);
            let mut state = self.state.lock().expect("tx");
            let entry = &mut state.documents[index];
            entry.target = Some(DocumentTarget::Loaded(loaded));
            entry.draft = Some(draft.clone());
            return Ok(draft);
        }
        let scope = address.scope;
        if let DocumentScope::Conversation { conversation_id } = scope {
            self.require_conversation(conversation_id).await?;
        }
        if let DocumentScope::Task { task_id } = scope {
            let task = self.current_task(task_id).await?;
            let Some(task) = task else {
                return Err(SessionError::Message(format!(
                    "Task {task_id} does not exist"
                )));
            };
            if task.state.status() == crate::types::TaskStatus::Terminal {
                return Err(SessionError::Message(format!("Task {task_id} is terminal")));
            }
        }
        self.assert_open()?;
        let value = if definition.is_family() {
            definition.initial(seed.as_ref())
        } else {
            definition.initial(None)
        };
        let id = DocumentId::new(self.storage().mint_id().await);
        self.assert_open()?;
        let tracker = Arc::new(Mutex::new(track(JsonValue::Object(value))));
        let change = tracker.lock().expect("tracker").begin_change();
        let draft = Draft::new(change);
        let mut state = self.state.lock().expect("tx");
        let entry = &mut state.documents[index];
        entry.target = Some(DocumentTarget::Created {
            record: document_create(definition.as_ref(), &entry.address, id),
            version: definition.version(),
            tracker,
        });
        entry.draft = Some(draft.clone());
        Ok(draft)
    }

    async fn find_retirement(&self, index: usize) -> Result<(), SessionError> {
        let (address_id, address, definition) = {
            let state = self.state.lock().expect("tx");
            let entry = &state.documents[index];
            (
                entry.address_id.clone(),
                entry.address.clone(),
                Arc::clone(entry.definition.as_ref().expect("definition")),
            )
        };
        let record = match self.host().cached(&address_id) {
            Some(loaded) => Some(loaded.record),
            None => {
                self.storage()
                    .find_document(
                        &address,
                        crate::types::DocumentPoint::Current,
                        self.context(),
                    )
                    .await?
            }
        };
        self.assert_open()?;
        let Some(record) = record else {
            return Ok(());
        };
        check_record_scope(definition.as_ref(), &record).map_err(SessionError::Doc)?;
        let mut state = self.state.lock().expect("tx");
        state.documents[index].target = Some(DocumentTarget::RetireOnly(record));
        Ok(())
    }

    fn assert_task_documents_open(&self, resolved: &ResolvedAddress) -> Result<(), DocError> {
        if let DocumentScope::Task { task_id } = resolved.address.scope {
            let state = self.state.lock().expect("tx");
            if let Some(task) = state.tasks_by_id.get(&task_id.get())
                && let Some(write) = &task.write
                && write.record.state.status() == crate::types::TaskStatus::Terminal
            {
                return Err(DocError::Message(format!("Task {task_id} is terminal")));
            }
        }
        Ok(())
    }

    // ─── 结算 ────────────────────────────────────────────────────────────────

    /// 对应 `settleFailure()`。
    pub fn settle_failure(&self) {
        let mut state = self.state.lock().expect("tx");
        state.sealed = true;
        for entry in &mut state.documents {
            if let Some(draft) = &entry.draft
                && let Some(change) = draft.take()
            {
                change.abort();
            }
        }
    }

    /// 对应 `settleSuccess()`：冻结全部开放变更并组装原子批次。
    pub async fn settle_success(&self) -> Result<Vec<StorageWrite>, SessionError> {
        {
            let mut state = self.state.lock().expect("tx");
            state.sealed = true;
            for entry in &mut state.documents {
                if let Some(draft) = &entry.draft
                    && let Some(change) = draft.take()
                {
                    entry.prepared = Some(change.prepare());
                }
            }
        }
        let result = self.assemble().await;
        if result.is_err() {
            self.discard();
        }
        result
    }

    /// 对应 `discard()`：存储失败或无需写入时放弃全部已准备变更
    /// （Rust 的 `Prepared` 只是已计算的操作批次，放弃即为丢弃）。
    pub fn discard(&self) {
        let mut state = self.state.lock().expect("tx");
        for entry in &mut state.documents {
            if let Some(prepared) = entry.prepared.take() {
                let _ = prepared;
            }
        }
    }

    /// 对应 `adopt(seq)`：存储成功后按指针替换采纳全部已准备变更，并描述其发布。
    pub fn adopt(&self, seq: Seq) -> Result<Vec<DocumentCommitChange>, SessionError> {
        let mut publications = Vec::new();
        let mut state = self.state.lock().expect("tx");
        let plans = std::mem::take(&mut state.plans);
        for mut plan in plans {
            let publish = publishes(&plan);
            let mut published: Option<(u32, JsonValue, Vec<Op>, bool)> = None;
            let committed = matches!(plan.record, DocumentRecordOrCreate::Existing(_));
            let mut record = match &plan.record {
                DocumentRecordOrCreate::Existing(record) => record.clone(),
                DocumentRecordOrCreate::Creating(record) => DocumentRecord {
                    id: record.id,
                    kind: record.kind.clone(),
                    key: record.key.clone(),
                    created_at: seq,
                    retired_at: None,
                    history: record.history,
                    fork: record.fork,
                    scope: record.scope,
                },
            };
            if plan.retire {
                record.retired_at = Some(seq);
            }
            let change = plan.change.take();
            if let Some(change) = change {
                let PlanChange {
                    tracker,
                    prepared,
                    version,
                    loaded,
                    definition: _,
                } = change;
                let prepared_value = prepared.value().clone();
                let ops = prepared.ops().to_vec();
                let has_loaded = loaded.is_some();
                let should_adopt = loaded.as_ref().map_or(!plan.retire, |_| !ops.is_empty());
                if should_adopt {
                    tracker.lock().expect("tracker").adopt(prepared);
                } else {
                    drop(prepared);
                }
                if let Some(loaded) = &loaded {
                    let mut updated = loaded.clone();
                    if updated.stored_version < version {
                        updated.stored_version = version;
                    }
                    if let Some(StorageWrite::DocumentChange { content, .. }) = &plan.content {
                        match content {
                            crate::types::DocumentContent::Base(_) => updated.deltas_since_base = 0,
                            crate::types::DocumentContent::Delta { .. } => {
                                updated.deltas_since_base += 1
                            }
                        }
                    }
                    self.host().install(updated);
                } else if !plan.retire {
                    self.host().install(LoadedDocument {
                        address_id: plan.address_id.clone(),
                        record: record.clone(),
                        stored_version: version,
                        value_version: version,
                        deltas_since_base: 0,
                        tracker: Arc::clone(&tracker),
                    });
                }
                published = Some((version, prepared_value, ops, has_loaded));
            }
            let conversation_id = plan.conversation_id;
            if plan.retire {
                if committed {
                    self.host().evict(&plan.address_id, record.id);
                }
                publications.push(DocumentCommitChange::Document {
                    record,
                    conversation_id,
                    version: None,
                    value: None,
                    ops: empty_operations(),
                });
            } else if let Some(StorageWrite::DocumentCopy { source, .. }) = &plan.content {
                publications.push(DocumentCommitChange::DocumentCopy {
                    record,
                    conversation_id: conversation_id.expect("copy conversation"),
                    source: *source,
                });
            } else if let Some((version, value, ops, has_loaded)) = published
                && publish
            {
                let ops = if has_loaded { ops } else { empty_operations() };
                publications.push(DocumentCommitChange::Document {
                    record,
                    conversation_id,
                    version: Some(version),
                    value: Some(json_object(&value)),
                    ops,
                });
            }
        }
        Ok(publications)
    }

    async fn assemble(&self) -> Result<Vec<StorageWrite>, SessionError> {
        let documents = {
            let mut state = self.state.lock().expect("tx");
            std::mem::take(&mut state.documents)
        };
        let mut plans = Vec::new();
        for entry in documents {
            if let Some(plan) = plan_document(entry) {
                plans.push(plan);
            }
        }
        self.reject_fork_source_writes(&plans)?;
        self.validate_owners().await?;

        // 已存在的任务替换：校验其仍存在、非终态、会话不变。
        let replaces: Vec<(TaskId, AnyTaskRecord)> = {
            let state = self.state.lock().expect("tx");
            state
                .tasks_by_id
                .values()
                .filter_map(|task| match &task.write {
                    Some(write) if write.kind == TaskWriteKind::Replace => {
                        Some((TaskId::new(write.record.id.get()), write.record.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        for (id, record) in replaces {
            let committed = self.committed_task(id).await?;
            let Some(committed) = committed else {
                return Err(SessionError::Message(format!("Task {id} does not exist")));
            };
            if committed.state.status() == crate::types::TaskStatus::Terminal {
                return Err(SessionError::Message(format!(
                    "Task {id} is already terminal"
                )));
            }
            if committed.conversation_id != record.conversation_id {
                return Err(SessionError::Message(format!(
                    "Task {id} cannot change conversations"
                )));
            }
        }

        // 终态结算会退役该任务的每个文档，包括本事务新建的。
        let terminal_task_ids: BTreeSet<u64> = {
            let state = self.state.lock().expect("tx");
            state
                .tasks_by_id
                .values()
                .filter_map(|task| task.write.as_ref())
                .filter(|write| write.record.state.status() == crate::types::TaskStatus::Terminal)
                .map(|write| write.record.id.get())
                .collect()
        };
        if !terminal_task_ids.is_empty() {
            let mut retiring = BTreeSet::new();
            for plan in &mut plans {
                if let DocumentScope::Task { task_id } = plan.record.scope()
                    && terminal_task_ids.contains(&task_id.get())
                {
                    plan.retire = true;
                    retiring.insert(plan.record.id().get());
                }
            }
            for task_id in &terminal_task_ids {
                let created_here = {
                    let state = self.state.lock().expect("tx");
                    state
                        .tasks_by_id
                        .get(task_id)
                        .and_then(|task| task.write.as_ref())
                        .is_some_and(|write| write.kind == TaskWriteKind::Create)
                };
                if created_here {
                    continue;
                }
                let mut cursor = None;
                loop {
                    let page = self
                        .storage()
                        .scan_documents(
                            DocumentQuery {
                                scope: DocumentScope::Task {
                                    task_id: TaskId::new(*task_id),
                                },
                                at: crate::types::DocumentPoint::Current,
                                kind: None,
                            },
                            INTERNAL_SCAN_PAGE_SIZE,
                            cursor.clone(),
                            self.context(),
                        )
                        .await?;
                    for record in page.items {
                        if retiring.contains(&record.id.get()) {
                            continue;
                        }
                        plans.push(DocumentPlan {
                            address_id: address_id(&DocumentAddress {
                                kind: record.kind.clone(),
                                scope: record.scope,
                                key: record.key.clone(),
                            }),
                            record: DocumentRecordOrCreate::Existing(record.clone()),
                            retire: true,
                            content: None,
                            change: None,
                            conversation_id: None,
                        });
                        retiring.insert(record.id.get());
                    }
                    match page.next {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
            }
        }

        // 在存储接纳前解析发布归属，使采纳保持同步。
        for plan in &mut plans {
            if !publishes(plan) {
                continue;
            }
            let scope = plan.record.scope();
            if let DocumentScope::Conversation { conversation_id } = scope {
                plan.conversation_id = Some(conversation_id);
            }
            if let DocumentScope::Task { task_id } = scope {
                let cached = {
                    let state = self.state.lock().expect("tx");
                    state
                        .tasks_by_id
                        .get(&task_id.get())
                        .and_then(|task| task.publication_conversation_id)
                };
                let conversation_id = match cached {
                    Some(id) => Some(id),
                    None => {
                        let current = self.current_task(task_id).await?;
                        current.map(|record| record.conversation_id)
                    }
                };
                {
                    let mut state = self.state.lock().expect("tx");
                    let entry = state.tasks_by_id.entry(task_id.get()).or_default();
                    entry.publication_conversation_id = conversation_id;
                }
                plan.conversation_id = conversation_id;
            }
        }

        // 提交变更按暂存顺序对最新候选记录解析。
        let changes = {
            let state = self.state.lock().expect("tx");
            state.submission_changes.clone()
        };
        for (id, change) in changes {
            let current = {
                let state = self.state.lock().expect("tx");
                state.submissions.get(&id.get()).cloned()
            };
            let current = match current {
                Some(record) => Some(record),
                None => self.storage().submission(id, self.context()).await?,
            };
            let Some(current) = current else {
                return Err(SessionError::Message(format!(
                    "Submission {id} does not exist"
                )));
            };
            let next = apply_submission_change(&current, &change)?;
            if next != current {
                self.state
                    .lock()
                    .expect("tx")
                    .submissions
                    .insert(id.get(), next);
            }
        }

        let mut writes = {
            let state = self.state.lock().expect("tx");
            state.writes.clone()
        };
        {
            let state = self.state.lock().expect("tx");
            for value in state.submissions.values() {
                writes.push(StorageWrite::Submission(value.clone()));
            }
            for task in state.tasks_by_id.values() {
                if let Some(write) = &task.write {
                    writes.push(StorageWrite::Task(write.record.clone()));
                }
            }
        }
        for plan in &mut plans {
            // checkpoint 谓词在全部校验之后最后运行。
            if let Some(StorageWrite::DocumentChange { content, .. }) = &plan.content
                && let crate::types::DocumentContent::Delta { version, ops } = content
                && let Some(change) = &plan.change
                && change.loaded.is_some()
            {
                let info = crate::types::CheckpointInfo {
                    deltas_since_base: change
                        .loaded
                        .as_ref()
                        .map_or(0, |loaded| loaded.deltas_since_base),
                };
                let value = json_object(change.prepared.value());
                let should_checkpoint = change
                    .definition
                    .as_ref()
                    .is_some_and(|definition| definition.checkpoint_when(&value, ops, &info));
                if should_checkpoint {
                    plan.content = Some(StorageWrite::DocumentChange {
                        id: plan.record.id(),
                        content: crate::types::DocumentContent::Base(crate::types::DocumentBase {
                            version: *version,
                            value,
                        }),
                    });
                }
            }
            if let Some(content) = &plan.content {
                writes.push(content.clone());
            }
            if plan.retire {
                writes.push(StorageWrite::DocumentRetire {
                    id: plan.record.id(),
                });
            }
        }
        self.state.lock().expect("tx").plans = plans;
        Ok(writes)
    }

    /// 对应 `#validateOwners`：新拥有的工作需要一个活的拥有者，按其最终候选判定。
    async fn validate_owners(&self) -> Result<(), SessionError> {
        let mut owners: Vec<(String, TaskId)> = Vec::new();
        {
            let state = self.state.lock().expect("tx");
            for write in &state.writes {
                if let StorageWrite::Conversation(record) = write
                    && let Some(owner) = record.owner
                {
                    owners.push(("Conversation owner task".to_string(), owner.task_id));
                }
            }
            for task in state.tasks_by_id.values() {
                if let Some(write) = &task.write
                    && write.kind == TaskWriteKind::Create
                    && let Some(owner) = write.record.owner
                {
                    owners.push(("Task owner".to_string(), owner));
                }
            }
        }
        for (what, task_id) in owners {
            let task = self.current_task(task_id).await?;
            let Some(task) = task else {
                return Err(SessionError::Message(format!(
                    "{what} {task_id} does not exist"
                )));
            };
            let status = task.state.status();
            if status == crate::types::TaskStatus::Terminal
                || status == crate::types::TaskStatus::Completing
            {
                return Err(SessionError::Message(format!(
                    "{what} {task_id} is {}",
                    status_name(status)
                )));
            }
            if task.abort_requested {
                return Err(SessionError::Message(format!(
                    "{what} {task_id} is abort-marked"
                )));
            }
        }
        Ok(())
    }

    /// 对应 `#rejectForkSourceWrites`。
    fn reject_fork_source_writes(&self, plans: &[DocumentPlan]) -> Result<(), SessionError> {
        let state = self.state.lock().expect("tx");
        for plan in plans {
            if plan.content.is_none() && !plan.retire {
                continue;
            }
            let id = plan.record.id();
            if state.fork_source_document_ids.contains(&id.get()) {
                return Err(SessionError::Message(format!(
                    "Cannot change fork source document {id} in the fork transaction"
                )));
            }
            if let DocumentScope::Conversation { conversation_id } = plan.record.scope()
                && state
                    .fork_source_conversation_ids
                    .contains(&conversation_id.get())
            {
                return Err(SessionError::Message(format!(
                    "Cannot fork conversation {conversation_id} while changing its current-policy documents"
                )));
            }
        }
        Ok(())
    }

    async fn require_conversation(&self, id: ConversationId) -> Result<(), SessionError> {
        {
            let state = self.state.lock().expect("tx");
            if state.created_conversation_ids.contains(&id.get()) {
                return Ok(());
            }
        }
        if self
            .storage()
            .conversation(id, self.context())
            .await?
            .is_none()
        {
            return Err(SessionError::Message(format!(
                "Conversation {id} does not exist"
            )));
        }
        Ok(())
    }

    /// 对应 `#currentTask`：最新候选记录，回退到已提交状态。
    async fn current_task(&self, id: TaskId) -> Result<Option<AnyTaskRecord>, SessionError> {
        {
            let state = self.state.lock().expect("tx");
            if let Some(task) = state.tasks_by_id.get(&id.get())
                && let Some(write) = &task.write
            {
                return Ok(Some(write.record.clone()));
            }
        }
        self.committed_task(id).await
    }

    /// 对应 `#committedTask`：按需读取并缓存已提交记录。
    async fn committed_task(&self, id: TaskId) -> Result<Option<AnyTaskRecord>, SessionError> {
        {
            let state = self.state.lock().expect("tx");
            if let Some(task) = state.tasks_by_id.get(&id.get())
                && let Some(record) = &task.committed_read
            {
                return Ok(Some(record.clone()));
            }
        }
        let record = self.storage().task(id, self.context()).await?;
        let mut state = self.state.lock().expect("tx");
        let entry = state.tasks_by_id.entry(id.get()).or_default();
        if let Some(record) = &record {
            entry.committed_read = Some(record.clone());
        }
        Ok(record)
    }
}

fn doc_error(error: SessionError) -> DocError {
    match error {
        SessionError::Message(message) | SessionError::Type(message) => DocError::Message(message),
        SessionError::Doc(error) => error,
        other => DocError::Message(other.to_string()),
    }
}

fn status_name(status: crate::types::TaskStatus) -> &'static str {
    match status {
        crate::types::TaskStatus::Pending => "pending",
        crate::types::TaskStatus::Running => "running",
        crate::types::TaskStatus::Waiting => "waiting",
        crate::types::TaskStatus::Completing => "completing",
        crate::types::TaskStatus::Terminal => "terminal",
    }
}

fn json_object(value: &JsonValue) -> JsonObject {
    match value {
        JsonValue::Object(object) => object.clone(),
        _ => JsonObject::new(),
    }
}

/// 对应 `#stampTimes`：生命周期时间戳（首次 `running` 记 `startedAt`，进入 `terminal` 记 `endedAt`）。
fn stamp_times(now: u64, value: AnyTaskRecord, candidate: Option<&AnyTaskRecord>) -> AnyTaskRecord {
    let status = value.state.status();
    let started_at = candidate
        .and_then(|record| record.started_at)
        .or(value.started_at)
        .or(if status == crate::types::TaskStatus::Running {
            Some(now)
        } else {
            None
        });
    let ended_at = candidate
        .and_then(|record| record.ended_at)
        .or(value.ended_at)
        .or(if status == crate::types::TaskStatus::Terminal {
            Some(now)
        } else {
            None
        });
    if started_at == value.started_at && ended_at == value.ended_at {
        return value;
    }
    AnyTaskRecord {
        started_at,
        ended_at,
        ..value
    }
}

/// 对应 `planDocument`：把一个已暂存化身规划为记录、内容写入与已准备变更。
fn plan_document(entry: DocumentEntry) -> Option<DocumentPlan> {
    let target = entry.target?;
    let retire = entry.retire_on_commit;
    let address_id = entry.address_id;
    match target {
        DocumentTarget::Created {
            record,
            version,
            tracker,
        } => {
            let prepared = entry.prepared?;
            let value = json_object(prepared.value());
            let content = StorageWrite::DocumentCreate {
                record: record.clone(),
                content: crate::types::DocumentBase { version, value },
            };
            Some(DocumentPlan {
                address_id,
                record: DocumentRecordOrCreate::Creating(record),
                retire,
                content: Some(content),
                change: Some(PlanChange {
                    tracker,
                    prepared,
                    version,
                    loaded: None,
                    definition: entry.definition,
                }),
                conversation_id: None,
            })
        }
        DocumentTarget::ForkCopy { record, source } => Some(DocumentPlan {
            address_id,
            record: DocumentRecordOrCreate::Creating(record.clone()),
            retire,
            content: Some(StorageWrite::DocumentCopy { record, source }),
            change: None,
            conversation_id: None,
        }),
        DocumentTarget::RetireOnly(record) => Some(DocumentPlan {
            address_id,
            record: DocumentRecordOrCreate::Existing(record),
            retire,
            content: None,
            change: None,
            conversation_id: None,
        }),
        DocumentTarget::Loaded(loaded) => {
            let definition = entry.definition?;
            let prepared = entry.prepared?;
            let version = definition.version();
            let id = loaded.record.id;
            let content = if loaded.stored_version < version {
                Some(StorageWrite::DocumentChange {
                    id,
                    content: crate::types::DocumentContent::Base(crate::types::DocumentBase {
                        version,
                        value: json_object(prepared.value()),
                    }),
                })
            } else if !prepared.ops().is_empty() {
                Some(StorageWrite::DocumentChange {
                    id,
                    content: crate::types::DocumentContent::Delta {
                        version,
                        ops: prepared.ops().to_vec(),
                    },
                })
            } else {
                None
            };
            Some(DocumentPlan {
                address_id,
                record: DocumentRecordOrCreate::Existing(loaded.record.clone()),
                retire,
                content,
                change: Some(PlanChange {
                    tracker: Arc::clone(&loaded.tracker),
                    prepared,
                    version,
                    loaded: Some(loaded),
                    definition: Some(definition),
                }),
                conversation_id: None,
            })
        }
    }
}

/// 对应 `publishes`：采纳是否会发布该计划。
fn publishes(plan: &DocumentPlan) -> bool {
    plan.retire
        || plan
            .change
            .as_ref()
            .is_some_and(|change| change.loaded.is_none())
        || plan.content.is_some()
}
