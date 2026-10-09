//! 对应 `src/session/session.ts`：会话内核与提交线。
//!
//! 只有已提交状态可被观察。每个提交回调、准备、存储结算、采纳与发布入队都在**持有提交线**时运行；
//! 监听器稍后运行。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - **继承 → 钩子 trait**：上游 `HarnessImpl extends SessionImpl` 并覆盖 `conversationCreated` /
//!   `beforeClose`；Rust 用 [`SessionHooks`]（默认 [`DefaultSessionHooks`] 为空实现）。
//! - **Promise 链 → 公平异步互斥**：上游用 `#tail` Promise 链串行提交；Rust 用
//!   `tokio::sync::Mutex`（FIFO 公平锁），等待顺序等于调用顺序，job 失败不影响后续 job。
//! - **可重复 await 的 `close()`**：上游 `#closing` 是 Promise；Rust 用
//!   `futures::future::Shared` + `BoxFuture`，多次 `close` 共享同一结果。
//! - **`subscribeCommits` / `subscribeClose` 的取消函数 → RAII**：返回 [`SessionSubscription`]，
//!   其 `Drop` 即取消（保留幂等语义）。
//! - **重载 + rest args → 显式参数**：上游靠 `resolveAddress(definition, ...args)` 的参数个数推断
//!   `conversationId` / `key` / `context`；Rust 显式接收 `owner` / `key` / `context`
//!   （与 [`crate::documents::resolve_address`] 一致），`next_argument` 不再参与。
//! - **`Session` 契约的泛型部分**：`commit` / `commitWith` / `readOnLine` /
//!   `conversationDocumentOnLine` 是泛型方法，无法进入 dyn-safe 的 [`Session`] trait，
//!   保留为 [`SessionImpl`] 的固有方法（上游 `DocumentReader = Pick<Session, ...>` 的用途由 trait 覆盖）。
//! - `DOMException("AbortError")` → [`crate::session::SessionError::Aborted`]。

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use futures::future::{BoxFuture, FutureExt, Shared};
use serde_json::Value as JsonValue;

use crate::chord::context::{Context, await_with_context, without_abort_signal};
use crate::chord::delta::Op;
use crate::chord::state::{AttachedReplicatedState, attach_replicated_state_source};
use crate::chord::tracker::track;
use crate::documents::{
    AnyDocToken, check_record_scope, check_record_version, materialize_document, resolve_address,
};
use crate::session::SessionError;
use crate::session::observation::{
    CommittedStateSource, CommittedWatch, observed_to_json, retirement_operations,
};
use crate::session::transaction::{LoadedDocument, Transaction, TransactionHost, TransactionScope};
use crate::types::{
    CommitChange, CommitPublication, ConversationId, ConversationRecord, DocDefinitionSpec,
    DocumentAddress, DocumentCommitChange, DocumentId, DocumentPoint, DocumentScope, EntryId,
    JsonObject, Seq, Storage, StorageWrite,
};

/// 对应 `WatchHandle<T>` 的文档观察（Rust 侧的具体实现）。
pub type DocumentWatch = CommittedWatch;

/// 对应 `DocumentState<T> = AttachedReplicatedState<Readonly<T> | null>`。
pub type DocumentState = AttachedReplicatedState;

/// 对应 `MAX_...`：不适用；监听器以插入序号为键，保证 FIFO 调用顺序。
type CommitListeners = BTreeMap<u64, CommitListener>;
type CloseListeners = BTreeMap<u64, CloseListener>;

/// 对应 `subscribeCommits` 的监听者签名：`(publication, context) => void`。
pub type CommitListener = Arc<dyn Fn(&CommitPublication, &Arc<dyn Context>) + Send + Sync>;

/// 对应 `subscribeClose` 的监听者签名：`() => void`。
pub type CloseListener = Arc<dyn Fn() + Send + Sync>;

static NEXT_LISTENER_ID: AtomicU64 = AtomicU64::new(1);

/// 对应 `createSession(storage, options?)`。
pub fn create_session(
    storage: Arc<dyn Storage>,
    options: Option<SessionOptions>,
) -> Arc<SessionImpl> {
    SessionImpl::create(storage, options.unwrap_or_default())
}

/// 对应 `createSession` 的 `options`。
#[derive(Clone, Default)]
pub struct SessionOptions {
    /// 任务生命周期时间的墙钟；缺省 [`default_now`]（对应 `Date.now`）。
    pub now: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

/// 对应 `Date.now`：Unix 毫秒。
pub fn default_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// 对应 `SessionImpl` 的两个可覆盖钩子。
///
/// 上游靠子类覆盖（`HarnessImpl extends SessionImpl`）；Rust 用 trait 注入。
#[async_trait::async_trait]
pub trait SessionHooks: Send + Sync {
    /// 在每个创建或分叉会话的事务内、会话记录暂存之后运行。
    /// 普通 Session 不暂存任何东西；harness 在此暂存其内置文档。
    async fn conversation_created(
        &self,
        tx: &Transaction,
        record: &ConversationRecord,
    ) -> Result<(), SessionError>;

    /// 在 close 封闭准入之后、提交线关闭 Storage 之前运行；**不得 reject**。
    async fn before_close(&self) -> Result<(), SessionError>;
}

/// 对应 `SessionImpl` 的默认钩子（两者都是空实现）。
pub struct DefaultSessionHooks;

#[async_trait::async_trait]
impl SessionHooks for DefaultSessionHooks {
    async fn conversation_created(
        &self,
        _tx: &Transaction,
        _record: &ConversationRecord,
    ) -> Result<(), SessionError> {
        Ok(())
    }

    async fn before_close(&self) -> Result<(), SessionError> {
        Ok(())
    }
}

/// 对应 `conversationDocumentOnLine` 的结果：一个会话文档的当前化身与值。
#[derive(Clone)]
pub struct ConversationDocumentOnLine {
    /// 化身记录。
    pub record: crate::types::DocumentRecord,
    /// 跟踪值形态对应的定义版本。
    pub version: u32,
    /// 已提交值。
    pub value: JsonObject,
}

/// 对应 `Session` 契约中可在 dyn 上表达的部分。
///
/// 上游 `Session extends DocumentObserver` 并从 `DocumentReader` 取 `snapshot` / `snapshotAsOf`；
/// Rust 用 supertrait 表达同样的继承。泛型的 `commit` / `commitWith` / `readOnLine` /
/// `conversationDocumentOnLine` 留在 [`SessionImpl`] 的固有方法上（见模块文档）。
#[async_trait::async_trait]
pub trait Session:
    Send + Sync + crate::session::DocumentObserver + crate::session::DocumentReader
{
    /// 对应 `close(context)`。
    async fn close(&self, context: Arc<dyn Context>) -> Result<(), SessionError>;

    /// 对应 `subscribeCommits(listener)`。
    fn subscribe_commits(
        &self,
        listener: CommitListener,
    ) -> Result<SessionSubscription, SessionError>;

    /// 对应 `subscribeClose(listener)`。
    fn subscribe_close(&self, listener: CloseListener)
    -> Result<SessionSubscription, SessionError>;

    /// 对应 `documentState(token, ...)`。
    async fn document_state(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<DocumentState>, SessionError>;
}

/// 对应 `#closing`：首次 `close()` 同步构造、可重复等待的关闭过程。
type ClosingFuture = Shared<BoxFuture<'static, Result<(), SessionError>>>;

/// 对应 `subscribeCommits` / `subscribeClose` 返回的取消函数（Rust 用 RAII）。
pub struct SessionSubscription {
    session: Weak<SessionImpl>,
    kind: SubscriptionKind,
    id: u64,
    active: AtomicBool,
}

#[derive(Clone, Copy)]
enum SubscriptionKind {
    Commit,
    Close,
}

impl SessionSubscription {
    /// 幂等取消（`Drop` 之外也可显式调用）。
    pub fn unsubscribe(&self) {
        if self.active.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(session) = self.session.upgrade() else {
            return;
        };
        match self.kind {
            SubscriptionKind::Commit => {
                session
                    .commit_listeners
                    .lock()
                    .expect("listeners")
                    .remove(&self.id);
            }
            SubscriptionKind::Close => {
                session
                    .close_listeners
                    .lock()
                    .expect("listeners")
                    .remove(&self.id);
            }
        }
    }
}

impl Drop for SessionSubscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// 对应 `SessionImpl`：一条提交线、已加载文档的 tracker 缓存，以及已提交发布。
pub struct SessionImpl {
    storage: Arc<dyn Storage>,
    hooks: Arc<dyn SessionHooks>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    documents: Mutex<BTreeMap<String, LoadedDocument>>,
    commit_listeners: Mutex<CommitListeners>,
    close_listeners: Mutex<CloseListeners>,
    /// 提交线（对应 `#tail`）：公平 FIFO 异步互斥。
    line: tokio::sync::Mutex<()>,
    /// 对应 `#closing`：首次 `close()` 同步构造、可重复等待。
    closing: Mutex<Option<ClosingFuture>>,
    closing_started: AtomicBool,
    /// 对应 `#poison`。
    poison: Mutex<Option<SessionError>>,
    self_ref: OnceLock<Weak<SessionImpl>>,
}

impl SessionImpl {
    /// 对应 `new SessionImpl(storage, now)`；请用 [`create_session`] 以获得自引用。
    pub fn create(storage: Arc<dyn Storage>, options: SessionOptions) -> Arc<Self> {
        Self::create_with_hooks(storage, Arc::new(DefaultSessionHooks), options)
    }

    /// 带自定义钩子的构造（对应上游子类化 `SessionImpl`）。
    pub fn create_with_hooks(
        storage: Arc<dyn Storage>,
        hooks: Arc<dyn SessionHooks>,
        options: SessionOptions,
    ) -> Arc<Self> {
        let session = Arc::new(Self {
            storage,
            hooks,
            now: options.now.unwrap_or_else(|| Arc::new(default_now)),
            documents: Mutex::new(BTreeMap::new()),
            commit_listeners: Mutex::new(BTreeMap::new()),
            close_listeners: Mutex::new(BTreeMap::new()),
            line: tokio::sync::Mutex::new(()),
            closing: Mutex::new(None),
            closing_started: AtomicBool::new(false),
            poison: Mutex::new(None),
            self_ref: OnceLock::new(),
        });
        let _ = session.self_ref.set(Arc::downgrade(&session));
        session
    }

    // ─── 公开 API ────────────────────────────────────────────────────────────

    /// 对应 `commit(change, context)`。
    pub async fn commit<T, F>(
        &self,
        change: F,
        context: Arc<dyn Context>,
    ) -> Result<T, SessionError>
    where
        F: for<'a> FnOnce(&'a Transaction) -> BoxFuture<'a, Result<T, SessionError>>,
    {
        self.commit_with(change, context, None).await
    }

    /// 对应 `commitWith(change, context, scope?)`：暴露具体事务及其内部操作
    /// （保留 ID 的根引导、任务替换）。`scope` 设置 `tx.createTask()` 的默认会话，
    /// 以及追加条目所归属的任务。
    pub async fn commit_with<T, F>(
        &self,
        change: F,
        context: Arc<dyn Context>,
        scope: Option<TransactionScope>,
    ) -> Result<T, SessionError>
    where
        F: for<'a> FnOnce(&'a Transaction) -> BoxFuture<'a, Result<T, SessionError>>,
    {
        self.assert_usable()?;
        let session = self.self_arc();
        let job_session = Arc::clone(&session);
        let job_context = Arc::clone(&context);
        let scope = scope.unwrap_or_default();
        session
            .enqueue(async move { job_session.run_commit(change, job_context, scope).await })
            .await
    }

    /// 对应 `readOnLine(job)`：在提交线上运行只读作业，使多读派生观察到同一已提交状态。
    pub async fn read_on_line<T, F, Fut>(&self, job: F) -> Result<T, SessionError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, SessionError>>,
    {
        self.assert_usable()?;
        let session = self.self_arc();
        let job_session = Arc::clone(&session);
        session
            .enqueue(async move {
                job_session.assert_healthy()?;
                job().await
            })
            .await
    }

    /// 对应 `conversationDocumentOnLine(token, conversationId, context)`：供**已在提交线上**的
    /// 作业读取一个会话文档的当前化身与值（缺席为 `None`）。
    pub async fn conversation_document_on_line(
        &self,
        token: &dyn AnyDocToken,
        conversation_id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<Option<ConversationDocumentOnLine>, SessionError> {
        let definition = Arc::clone(token.definition());
        let resolved = resolve_address(definition.as_ref(), Some(conversation_id.get()), None)?;
        let loaded = self
            .load_document(
                &definition,
                &resolved.id,
                &resolved.address,
                context.as_ref(),
            )
            .await?;
        let Some(loaded) = loaded else {
            return Ok(None);
        };
        check_record_scope(definition.as_ref(), &loaded.record)?;
        check_record_version(definition.as_ref(), &loaded.record, loaded.stored_version)?;
        Ok(Some(ConversationDocumentOnLine {
            version: loaded.value_version,
            record: loaded.record,
            value: json_object(loaded.tracker.lock().expect("tracker").value().clone()),
        }))
    }

    /// 对应 `snapshot(token, ...)`：一个已提交文档的当前不可变值。
    pub async fn snapshot(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        self.assert_usable()?;
        let definition = Arc::clone(token.definition());
        let resolved = resolve_address(definition.as_ref(), owner, key)?;
        let cached = self
            .documents
            .lock()
            .expect("documents")
            .get(&resolved.id)
            .cloned();
        let loaded = match cached {
            Some(loaded) if loaded.value_version == definition.version() => Some(loaded),
            _ => {
                let session = self.self_arc();
                let job_session = Arc::clone(&session);
                let job_definition = Arc::clone(&definition);
                session
                    .enqueue(async move {
                        job_session.assert_healthy()?;
                        job_session
                            .load_document(
                                &job_definition,
                                &resolved.id,
                                &resolved.address,
                                context.as_ref(),
                            )
                            .await
                    })
                    .await?
            }
        };
        let Some(loaded) = loaded else {
            return Ok(None);
        };
        check_record_scope(definition.as_ref(), &loaded.record)?;
        check_record_version(definition.as_ref(), &loaded.record, loaded.stored_version)?;
        Ok(Some(json_object(
            loaded.tracker.lock().expect("tracker").value().clone(),
        )))
    }

    /// 对应 `documentState(token, ...)`：绑定到一个已提交化身的可释放只读 chord 状态。
    pub async fn document_state(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<DocumentState>, SessionError> {
        let definition = Arc::clone(token.definition());
        let resolved = resolve_address(definition.as_ref(), owner, key)?;
        let session = self.self_arc();
        let job_session = Arc::clone(&session);
        let job_definition = Arc::clone(&definition);
        session
            .enqueue(async move {
                job_session.assert_healthy()?;
                let loaded = job_session
                    .load_document(
                        &job_definition,
                        &resolved.id,
                        &resolved.address,
                        context.as_ref(),
                    )
                    .await?;
                let Some(loaded) = loaded else {
                    return Ok(None);
                };
                let (source, _detach) = job_session.attach_state_source(
                    Arc::clone(&job_definition),
                    loaded,
                    context,
                )?;
                Ok(Some(attach_replicated_state_source(&*source, None)))
            })
            .await
    }

    /// 对应 `watchDoc(token, ...)`：非创建式的文档观察获取。
    pub async fn watch_doc(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<DocumentWatch>>, SessionError> {
        self.assert_usable()?;
        let definition = Arc::clone(token.definition());
        let resolved = resolve_address(definition.as_ref(), owner, key)?;
        let signal = context.abort_signal();
        let cancelled = Arc::new(AtomicBool::new(
            signal.as_ref().is_some_and(|signal| signal.aborted()),
        ));

        let attach_cancelled = Arc::clone(&cancelled);
        let attach_signal = signal.clone();
        let attach_context = Arc::clone(&context);
        let session = self.self_arc();
        let job_session = Arc::clone(&session);
        let attach_definition = Arc::clone(&definition);
        let watch = session
            .enqueue(async move {
                job_session.assert_healthy()?;
                if attach_cancelled.load(Ordering::Acquire) {
                    return Err(cancellation_error(attach_signal.as_ref()));
                }
                let loaded = job_session
                    .load_document(
                        &attach_definition,
                        &resolved.id,
                        &resolved.address,
                        attach_context.as_ref(),
                    )
                    .await?;
                job_session.assert_healthy()?;
                if attach_cancelled.load(Ordering::Acquire) {
                    return Err(cancellation_error(attach_signal.as_ref()));
                }
                let Some(loaded) = loaded else {
                    return Ok(None);
                };
                let (watch, _detach) = job_session.attach_watch(
                    Arc::clone(&attach_definition),
                    loaded,
                    Arc::clone(&attach_context),
                )?;
                Ok(Some(watch))
            })
            .await?;

        let Some(watch) = watch else {
            return Ok(None);
        };
        if cancelled.load(Ordering::Acquire) {
            watch.cancel();
            return Err(cancellation_error(signal.as_ref()));
        }
        if let Some(signal) = signal {
            watch.observe_cancellation(signal);
        }
        Ok(Some(watch))
    }

    /// 对应 `snapshotAsOf(token, conversationId, at, context)`：某个历史条目点上的文档值。
    pub async fn snapshot_as_of(
        &self,
        token: &dyn AnyDocToken,
        owner: u64,
        key: Option<String>,
        at: EntryId,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        self.assert_usable()?;
        let definition = Arc::clone(token.definition());
        let resolved = resolve_address(definition.as_ref(), Some(owner), key)?;
        let DocumentScope::Conversation { conversation_id } = resolved.address.scope else {
            return Err(SessionError::Type(
                "Session.snapshotAsOf() requires a conversation document".to_string(),
            ));
        };
        let session = self.self_arc();
        let job_session = Arc::clone(&session);
        let job_definition = Arc::clone(&definition);
        session
            .enqueue(async move {
                job_session.assert_healthy()?;
                let Some((stored_entry, commit_seq)) = job_session
                    .storage
                    .entry_in_conversation(conversation_id, at, context.as_ref())
                    .await?
                else {
                    return Err(SessionError::Message(format!(
                        "Entry {at} is not visible from conversation {conversation_id}"
                    )));
                };
                let address = DocumentAddress {
                    scope: DocumentScope::Conversation {
                        conversation_id: stored_entry.conversation_id,
                    },
                    ..resolved.address
                };
                let Some(record) = job_session
                    .storage
                    .find_document(&address, DocumentPoint::At(commit_seq), context.as_ref())
                    .await?
                else {
                    return Ok(None);
                };
                let Some(stored) = job_session
                    .storage
                    .document(record.id, DocumentPoint::At(commit_seq), context.as_ref())
                    .await?
                else {
                    return Err(SessionError::Message(format!(
                        "Historical document {} ({}) cannot be read",
                        record.id, record.kind
                    )));
                };
                Ok(Some(materialize_document(
                    job_definition.as_ref(),
                    &stored,
                )?))
            })
            .await
    }

    /// 对应 `close(context)`：封闭准入、结算已受理提交，然后关闭存储。
    pub fn close(&self, context: Arc<dyn Context>) -> BoxFuture<'static, Result<(), SessionError>> {
        let shared = self.ensure_closing(&context);
        async move {
            match await_with_context(shared, context.as_ref()).await {
                Ok(result) => result,
                Err(error) => Err(SessionError::Aborted(error)),
            }
        }
        .boxed()
    }

    /// 对应 `subscribeCommits(listener)`：注册一个同步的采纳后监听器。
    ///
    /// 它不得抛错、阻塞或调用 Session 操作。
    pub fn subscribe_commits(
        &self,
        listener: CommitListener,
    ) -> Result<SessionSubscription, SessionError> {
        self.assert_usable()?;
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::Relaxed);
        self.commit_listeners
            .lock()
            .expect("listeners")
            .insert(id, listener);
        Ok(SessionSubscription {
            session: self.self_weak(),
            kind: SubscriptionKind::Commit,
            id,
            active: AtomicBool::new(false),
        })
    }

    /// 对应 `subscribeClose(listener)`：注册一个在 close 开始时同步调用的监听器。
    ///
    /// 它不得抛错、阻塞或调用 Session 操作。
    pub fn subscribe_close(
        &self,
        listener: CloseListener,
    ) -> Result<SessionSubscription, SessionError> {
        self.assert_usable()?;
        let id = NEXT_LISTENER_ID.fetch_add(1, Ordering::Relaxed);
        self.close_listeners
            .lock()
            .expect("listeners")
            .insert(id, listener);
        Ok(SessionSubscription {
            session: self.self_weak(),
            kind: SubscriptionKind::Close,
            id,
            active: AtomicBool::new(false),
        })
    }

    /// 对应 `unloadDocuments()`：在提交线上丢弃全部已加载 tracker；之后访问会从存储冷加载。
    pub async fn unload_documents(&self) -> Result<(), SessionError> {
        self.enqueue(async {
            self.documents.lock().expect("documents").clear();
        })
        .await;
        Ok(())
    }

    // ─── 内部：提交线 ────────────────────────────────────────────────────────

    async fn enqueue<T>(&self, job: impl Future<Output = T>) -> T {
        let _guard = self.line.lock().await;
        job.await
    }

    async fn run_commit<T, F>(
        self: &Arc<Self>,
        change: F,
        context: Arc<dyn Context>,
        scope: TransactionScope,
    ) -> Result<T, SessionError>
    where
        F: for<'a> FnOnce(&'a Transaction) -> BoxFuture<'a, Result<T, SessionError>>,
    {
        self.assert_healthy()?;
        if let Some(signal) = context.abort_signal() {
            signal.throw_if_aborted()?;
        }
        let host: Arc<dyn TransactionHost> = Arc::clone(self) as Arc<dyn TransactionHost>;
        let tx = Transaction::new(host, Arc::clone(&context), scope);
        let result = match change(&tx).await {
            Ok(result) => result,
            Err(error) => {
                tx.settle_failure();
                return Err(error);
            }
        };
        let writes = tx.settle_success().await?;
        if writes.is_empty() {
            tx.discard();
            return Ok(result);
        }
        let settlement_context = without_abort_signal(Arc::clone(&context));
        // 一旦被受理，调用方取消就不再中断存储结算。
        let seq = match self
            .storage
            .commit(&writes, settlement_context.as_ref())
            .await
        {
            Ok(seq) => seq,
            Err(error) => {
                tx.discard();
                // 回调错误不会到达这里；只有 StorageRejected 保证没有任何批次效果被提交。
                let error = SessionError::from(error);
                if !matches!(error, SessionError::Rejected(_)) {
                    self.poison(error.clone());
                }
                return Err(error);
            }
        };
        let documents = match tx.adopt(seq) {
            Ok(documents) => documents,
            Err(error) => {
                // 存储已提交；失败的采纳会让内存落后于持久状态。
                self.poison(error.clone());
                return Err(error);
            }
        };
        self.publish(seq, &writes, &documents, &context);
        Ok(result)
    }

    /// 对应 `#publish(seq, writes, documents, context)`。
    fn publish(
        &self,
        seq: Seq,
        writes: &[StorageWrite],
        documents: &[DocumentCommitChange],
        context: &Arc<dyn Context>,
    ) {
        let listeners: Vec<CommitListener> = self
            .commit_listeners
            .lock()
            .expect("listeners")
            .values()
            .cloned()
            .collect();
        if listeners.is_empty() {
            return;
        }
        let mut changes: Vec<CommitChange> = Vec::new();
        for write in writes {
            if let Some(change) = write.as_table_change() {
                changes.push(CommitChange::Table(change));
            }
        }
        for document in documents {
            changes.push(CommitChange::Document(document.clone()));
        }
        let publication = CommitPublication { seq, changes };
        for listener in listeners {
            listener(&publication, context);
        }
    }

    /// 对应 `close()` 中「首次调用才构造 `#closing`」的部分。
    fn ensure_closing(&self, context: &Arc<dyn Context>) -> ClosingFuture {
        let mut closing = self.closing.lock().expect("closing");
        if let Some(shared) = closing.as_ref() {
            return shared.clone();
        }
        // 先封闭准入，之后才轮到任何其它工作。
        self.closing_started.store(true, Ordering::Release);
        let cleanup = without_abort_signal(Arc::clone(context));
        let session = self.self_arc();
        let future: BoxFuture<'static, Result<(), SessionError>> = async move {
            session.hooks.before_close().await?;
            let cleanup_session = Arc::clone(&session);
            session
                .enqueue(async move {
                    cleanup_session
                        .commit_listeners
                        .lock()
                        .expect("listeners")
                        .clear();
                    cleanup_session.documents.lock().expect("documents").clear();
                    cleanup_session
                        .storage
                        .close(cleanup.as_ref())
                        .await
                        .map_err(SessionError::from)
                })
                .await
        }
        .boxed();
        let shared = future.shared();
        *closing = Some(shared.clone());
        drop(closing);

        // 观察者在提交线上停摆之前先同步收到 close。
        let listeners: Vec<CloseListener> = {
            let mut close_listeners = self.close_listeners.lock().expect("listeners");
            std::mem::take(&mut *close_listeners)
                .into_values()
                .collect()
        };
        for listener in listeners {
            listener();
        }
        shared
    }

    fn self_arc(&self) -> Arc<SessionImpl> {
        self.self_ref
            .get()
            .and_then(Weak::upgrade)
            .expect("session self reference must be initialized by create()")
    }

    fn self_weak(&self) -> Weak<SessionImpl> {
        self.self_ref
            .get()
            .cloned()
            .expect("session self reference must be initialized by create()")
    }

    fn assert_usable(&self) -> Result<(), SessionError> {
        if self.closing_started.load(Ordering::Acquire) {
            return Err(SessionError::Message("Session is closed".to_string()));
        }
        self.assert_healthy()
    }

    fn assert_healthy(&self) -> Result<(), SessionError> {
        let poison = self.poison.lock().expect("poison");
        if let Some(error) = poison.as_ref() {
            return Err(SessionError::MessageWithCause {
                message:
                    "Session is poisoned by a failed commit after storage admission; reopen it"
                        .to_string(),
                cause: error.to_string(),
            });
        }
        Ok(())
    }

    fn poison(&self, error: SessionError) {
        let mut poison = self.poison.lock().expect("poison");
        if poison.is_none() {
            *poison = Some(error);
        }
    }

    // ─── 内部：文档 ──────────────────────────────────────────────────────────

    /// 对应 `#loadDocument(definition, addressId, address, context)`。
    async fn load_document(
        &self,
        definition: &Arc<dyn DocDefinitionSpec>,
        address_id: &str,
        address: &DocumentAddress,
        context: &dyn Context,
    ) -> Result<Option<LoadedDocument>, SessionError> {
        let cached = self
            .documents
            .lock()
            .expect("documents")
            .get(address_id)
            .cloned();
        // 一个 tracker 只服务其值物化所依据的定义版本；其他版本重新从存储加载。
        if let Some(loaded) = &cached
            && loaded.value_version == definition.version()
        {
            return Ok(Some(loaded.clone()));
        }
        if cached.is_some() {
            self.documents.lock().expect("documents").remove(address_id);
        }
        let Some(record) = self
            .storage
            .find_document(address, DocumentPoint::Current, context)
            .await?
        else {
            return Ok(None);
        };
        let Some(stored) = self
            .storage
            .document(record.id, DocumentPoint::Current, context)
            .await?
        else {
            return Err(SessionError::Message(format!(
                "Current document {} ({}) cannot be read",
                record.id, record.kind
            )));
        };
        let value = materialize_document(definition.as_ref(), &stored)?;
        let loaded = LoadedDocument {
            address_id: address_id.to_string(),
            record: stored.record,
            stored_version: stored.version,
            value_version: definition.version(),
            deltas_since_base: stored.deltas_since_base,
            tracker: Arc::new(Mutex::new(track(JsonValue::Object(value)))),
        };
        self.documents
            .lock()
            .expect("documents")
            .insert(address_id.to_string(), loaded.clone());
        Ok(Some(loaded))
    }

    /// 对应 `#attachDocument(..., create = CommittedStateSource)`。
    fn attach_state_source(
        &self,
        definition: Arc<dyn crate::types::DocDefinitionSpec>,
        loaded: LoadedDocument,
        context: Arc<dyn Context>,
    ) -> Result<(Arc<CommittedStateSource>, DetachSlot), SessionError> {
        check_record_scope(definition.as_ref(), &loaded.record)?;
        check_record_version(definition.as_ref(), &loaded.record, loaded.stored_version)?;
        let slot = DetachSlot::new();
        let release_slot = slot.clone();
        let source = Arc::new(CommittedStateSource::new(
            loaded.tracker.lock().expect("tracker").value().clone(),
            move || release_slot.run(),
        ));
        let observed_version = Arc::new(Mutex::new(loaded.value_version));
        let record_id = loaded.record.id;
        let observed_source = Arc::clone(&source);
        let observed_context = Arc::clone(&context);
        let commit_subscription = self.subscribe_commits(Arc::new(move |publication, _| {
            for change in &publication.changes {
                let CommitChange::Document(change) = change else {
                    continue;
                };
                let DocumentCommitChange::Document { record, .. } = change else {
                    continue;
                };
                if record.id != record_id {
                    continue;
                }
                // 文档状态的帧不携带调用方取消；watch 由自身的取消观察负责。
                let frame_context = without_abort_signal(Arc::clone(&observed_context));
                let ops = observed_operations(&observed_version, change);
                // 仅迁移的 base 对新版本的观察者不改变任何东西。
                if ops.is_empty() {
                    continue;
                }
                observed_source.advance(document_change_value(change), ops, frame_context);
            }
        }))?;
        let closed_source = Arc::clone(&source);
        let close_subscription =
            self.subscribe_close(Arc::new(move || closed_source.close_session()))?;
        slot.set(Box::new(move || {
            drop(commit_subscription);
            drop(close_subscription);
        }));
        Ok((source, slot))
    }

    /// 对应 `#attachDocument(..., create = CommittedWatch)`。
    fn attach_watch(
        &self,
        definition: Arc<dyn crate::types::DocDefinitionSpec>,
        loaded: LoadedDocument,
        context: Arc<dyn Context>,
    ) -> Result<(Arc<CommittedWatch>, DetachSlot), SessionError> {
        check_record_scope(definition.as_ref(), &loaded.record)?;
        check_record_version(definition.as_ref(), &loaded.record, loaded.stored_version)?;
        let slot = DetachSlot::new();
        let release_slot = slot.clone();
        let watch = Arc::new(CommittedWatch::new(
            loaded.tracker.lock().expect("tracker").value().clone(),
            move || release_slot.run(),
            None,
        ));
        let observed_version = Arc::new(Mutex::new(loaded.value_version));
        let record_id = loaded.record.id;
        let observed_watch = Arc::clone(&watch);
        let observed_context = Arc::clone(&context);
        let commit_subscription = self.subscribe_commits(Arc::new(move |publication, _| {
            for change in &publication.changes {
                let CommitChange::Document(change) = change else {
                    continue;
                };
                let DocumentCommitChange::Document { record, .. } = change else {
                    continue;
                };
                if record.id != record_id {
                    continue;
                }
                let ops = observed_operations(&observed_version, change);
                if ops.is_empty() {
                    continue;
                }
                observed_watch.advance(
                    document_change_value(change),
                    ops,
                    Arc::clone(&observed_context),
                );
            }
        }))?;
        let closed_watch = Arc::clone(&watch);
        let close_subscription =
            self.subscribe_close(Arc::new(move || closed_watch.close_session()))?;
        slot.set(Box::new(move || {
            drop(commit_subscription);
            drop(close_subscription);
        }));
        Ok((watch, slot))
    }

    /// 对应 `#assertUsable()` 的可用性探针（测试与 harness 用）。
    pub fn is_closing(&self) -> bool {
        self.closing_started.load(Ordering::Acquire)
    }

    /// 对应 `#poison` 的当前错误（诊断用）。
    pub fn poison_error(&self) -> Option<SessionError> {
        self.poison.lock().expect("poison").clone()
    }

    /// 提交线空闲等待（测试用）：把一次无操作作业排到队尾。
    pub async fn drain_line(&self) -> Result<(), SessionError> {
        self.enqueue(async {}).await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl TransactionHost for SessionImpl {
    fn storage(&self) -> &dyn Storage {
        self.storage.as_ref()
    }

    fn now(&self) -> u64 {
        (self.now)()
    }

    fn cached(&self, address_id: &str) -> Option<LoadedDocument> {
        self.documents
            .lock()
            .expect("documents")
            .get(address_id)
            .cloned()
    }

    async fn load(
        &self,
        definition: &Arc<dyn DocDefinitionSpec>,
        address_id: &str,
        address: &DocumentAddress,
        context: &dyn Context,
    ) -> Result<Option<LoadedDocument>, SessionError> {
        self.load_document(definition, address_id, address, context)
            .await
    }

    fn install(&self, document: LoadedDocument) {
        self.documents
            .lock()
            .expect("documents")
            .insert(document.address_id.clone(), document);
    }

    fn evict(&self, address_id: &str, record_id: DocumentId) {
        let mut documents = self.documents.lock().expect("documents");
        if documents.get(address_id).map(|loaded| loaded.record.id) == Some(record_id) {
            documents.remove(address_id);
        }
    }

    async fn conversation_created(
        &self,
        tx: &Transaction,
        record: &ConversationRecord,
    ) -> Result<(), SessionError> {
        self.hooks.conversation_created(tx, record).await
    }
}

#[async_trait::async_trait]
impl Session for SessionImpl {
    async fn close(&self, context: Arc<dyn Context>) -> Result<(), SessionError> {
        SessionImpl::close(self, context).await
    }

    fn subscribe_commits(
        &self,
        listener: CommitListener,
    ) -> Result<SessionSubscription, SessionError> {
        SessionImpl::subscribe_commits(self, listener)
    }

    fn subscribe_close(
        &self,
        listener: CloseListener,
    ) -> Result<SessionSubscription, SessionError> {
        SessionImpl::subscribe_close(self, listener)
    }

    async fn document_state(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<DocumentState>, SessionError> {
        SessionImpl::document_state(self, token, owner, key, context).await
    }
}

#[async_trait::async_trait]
impl crate::session::DocumentReader for SessionImpl {
    async fn snapshot(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        SessionImpl::snapshot(self, token, owner, key, context).await
    }

    async fn snapshot_as_of(
        &self,
        token: &dyn AnyDocToken,
        owner: u64,
        key: Option<String>,
        at: EntryId,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        SessionImpl::snapshot_as_of(self, token, owner, key, at, context).await
    }
}

#[async_trait::async_trait]
impl crate::session::DocumentObserver for SessionImpl {
    async fn watch_doc(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<DocumentWatch>>, SessionError> {
        SessionImpl::watch_doc(self, token, owner, key, context).await
    }
}

/// 对应 `observedOperations(observed, change)`。
///
/// 在另一个定义版本下被水合的观察者持有不同形态的值，因此对它而言该次提交就是一次根替换。
fn observed_operations(observed: &Arc<Mutex<u32>>, change: &DocumentCommitChange) -> Vec<Op> {
    let DocumentCommitChange::Document {
        version,
        value,
        ops,
        ..
    } = change
    else {
        return Vec::new();
    };
    let Some(value) = value else {
        return retirement_operations();
    };
    let mut observed_version = observed.lock().expect("observed version");
    if let Some(version) = version
        && *version == *observed_version
    {
        return ops.clone();
    }
    if let Some(version) = version {
        *observed_version = *version;
    }
    vec![Op::Replace(JsonValue::Object(value.clone()))]
}

/// 把一次文档提交变更的 `value` 转成 chord 帧值（`None` → `null`）。
fn document_change_value(change: &DocumentCommitChange) -> JsonValue {
    match change {
        DocumentCommitChange::Document { value, .. } => observed_to_json(value),
        DocumentCommitChange::DocumentCopy { .. } => JsonValue::Null,
    }
}

/// 对应 `cancellationError(signal)`：Rust 的 `AbortSignal` 不携带 reason，统一为 `AbortError`。
fn cancellation_error(_signal: Option<&pi_ai::AbortSignal>) -> SessionError {
    SessionError::Aborted(pi_ai::AbortError)
}

/// 一个待执行的 detach 动作（对应上游 `detach` 闭包）。
type DetachAction = Box<dyn FnOnce() + Send + Sync>;

/// 一个幂等的 detach 槽（对应上游 `detach` 闭包）。
#[derive(Clone)]
struct DetachSlot(Arc<Mutex<Option<DetachAction>>>);

impl DetachSlot {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    fn set(&self, action: DetachAction) {
        let previous = self.0.lock().expect("detach").replace(action);
        drop(previous);
    }

    fn run(&self) {
        let action = self.0.lock().expect("detach").take();
        if let Some(action) = action {
            action();
        }
    }
}

fn json_object(value: JsonValue) -> JsonObject {
    match value {
        JsonValue::Object(object) => object,
        _ => JsonObject::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_slot_runs_once() {
        let runs = Arc::new(AtomicU64::new(0));
        let slot = DetachSlot::new();
        let counter = Arc::clone(&runs);
        slot.set(Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        slot.run();
        slot.run();
        assert_eq!(runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn detach_slot_set_replaces_pending_action() {
        let runs = Arc::new(AtomicU64::new(0));
        let slot = DetachSlot::new();
        let first = Arc::clone(&runs);
        slot.set(Box::new(move || {
            first.fetch_add(1, Ordering::Relaxed);
        }));
        let second = Arc::clone(&runs);
        slot.set(Box::new(move || {
            second.fetch_add(10, Ordering::Relaxed);
        }));
        slot.run();
        assert_eq!(runs.load(Ordering::Relaxed), 10, "被替换的动作不得运行");
    }
}
