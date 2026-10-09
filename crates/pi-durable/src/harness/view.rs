//! 对应 `harness/view.ts`：一个会话活动转录与内置文档的结构化挂载（spec §9.3）。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - **挂载值以 JSON 保存**：上游直接把 `ConversationView` 对象交给 chord，并通过 Proxy 观察者
//!   就地推进；Rust 的挂载值以 [`JsonValue`] 保存（与 `task-graph.ts` 同一手法），
//!   `ConversationView` 是它的反序列化视图。
//! - **`freezeJson` 豁免**：Rust 的值是 owned，无共享可变。
//! - **`conversationViews(harness)`（`WeakMap<object, …>`）推迟到 P5h**：它需要 Harness 对象作键，
//!   Harness 落地时改为在 Harness 上直接持有 [`ConversationViews`]。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::chord::context::{Context, without_abort_signal};
use crate::chord::delta::{Op, Path, PathSegment, apply_immutable};
use crate::chord::state::{AttachedReplicatedState, attach_replicated_state_source};
use crate::session::observation::{CommittedStateSource, CommittedWatch};
use crate::session::{SessionError, SessionImpl};
use crate::types::{
    CommitPublication, ConversationId, ConversationRecord, DocToken, DocumentId, EntryRecord,
    JsonObject, Storage,
};

/// 对应 `ConversationView`：一个会话活动转录与内置文档的结构化挂载。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationView {
    /// 会话记录。
    pub conversation: ConversationRecord,
    /// 原始活动条目，与 `ContextView.entries` 相同：head 标记，其后是自其 head 起的非 head 条目。
    pub entries: Vec<EntryRecord>,
    /// 按 kind 索引的内置会话文档；缺席的文档就是缺席。
    pub docs: BTreeMap<String, JsonObject>,
}

/// 对应 `ViewObserver`：接收挂载的每个新修订，以及 Session 的关闭。
pub trait ViewObserver: Send + Sync {
    /// 每个新修订。
    fn advance(&self, value: JsonValue, ops: &[Op], context: Arc<dyn Context>) {
        let _ = (value, ops, context);
    }

    /// 每个发布，在挂载取用它之后；`ops` 是挂载的，可能为空。
    fn publication(
        &self,
        before: &JsonValue,
        after: &JsonValue,
        ops: &[Op],
        publication: &CommitPublication,
        context: Arc<dyn Context>,
    ) {
        let _ = (before, after, ops, publication, context);
    }

    /// Session 关闭。
    fn close_session(&self);
}

impl ViewObserver for CommittedStateSource {
    fn advance(&self, value: JsonValue, ops: &[Op], context: Arc<dyn Context>) {
        CommittedStateSource::advance(self, value, ops.to_vec(), context);
    }

    fn close_session(&self) {
        CommittedStateSource::close_session(self);
    }
}

impl ViewObserver for CommittedWatch {
    fn advance(&self, value: JsonValue, ops: &[Op], context: Arc<dyn Context>) {
        CommittedWatch::advance(self, value, ops.to_vec(), context);
    }

    fn close_session(&self) {
        CommittedWatch::close_session(self);
    }
}

/// 与 [`crate::session::session`] 同构的幂等释放槽（对应上游的 `release` 闭包）。
///
/// 出现在 [`ConversationViews::attach`] 的返回类型里，因此是公开的；内部方法不对外。
type ReleaseAction = Box<dyn FnOnce() + Send + Sync>;

/// 挂载观察者的释放句柄；丢弃或调用 `run` 都会执行一次释放。
#[derive(Clone)]
pub struct ReleaseSlot(Arc<Mutex<Option<ReleaseAction>>>);

impl ReleaseSlot {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    pub(crate) fn set(&self, action: ReleaseAction) {
        let previous = self.0.lock().expect("release").replace(action);
        drop(previous);
    }

    fn run(&self) {
        let action = self.0.lock().expect("release").take();
        if let Some(action) = action {
            action();
        }
    }
}

/// 一个挂载：其当前修订、它展示的文档化身，以及它的观察者。
struct Mount {
    value: Mutex<JsonValue>,
    /// 每个 kind 的已挂载化身与定义版本；另一个化身或版本会整体替换。
    docs: Mutex<BTreeMap<String, DocIncarnation>>,
    observers: Mutex<Vec<Arc<dyn ViewObserver>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DocIncarnation {
    id: DocumentId,
    version: u32,
}

/// Harness 的会话视图挂载：每个会话至多一个，由第一个观察者建立、随最后一个观察者丢弃。
pub struct ConversationViews {
    session: Arc<SessionImpl>,
    storage: Arc<dyn Storage>,
    mounts: Mutex<BTreeMap<ConversationId, Arc<Mount>>>,
    closed: AtomicBool,
    /// `subscribe_commits` / `subscribe_close` 返回的 RAII 句柄必须活到视图存续期；
    /// 丢弃它们会立即取消失订阅。
    commit_subscription: Mutex<Option<crate::session::SessionSubscription>>,
    close_subscription: Mutex<Option<crate::session::SessionSubscription>>,
    self_ref: std::sync::OnceLock<Weak<ConversationViews>>,
}

impl ConversationViews {
    /// 对应 `new ConversationViews(session, storage)`：立即订阅提交与关闭。
    pub fn new(session: Arc<SessionImpl>, storage: Arc<dyn Storage>) -> Arc<Self> {
        let views = Arc::new(Self {
            session: Arc::clone(&session),
            storage,
            mounts: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
            commit_subscription: Mutex::new(None),
            close_subscription: Mutex::new(None),
            self_ref: std::sync::OnceLock::new(),
        });
        let _ = views.self_ref.set(Arc::downgrade(&views));

        let weak = Arc::downgrade(&views);
        let commit_subscription =
            session.subscribe_commits(Arc::new(move |publication, context| {
                let Some(views) = weak.upgrade() else {
                    return;
                };
                let mounts: Vec<(ConversationId, Arc<Mount>)> = views
                    .mounts
                    .lock()
                    .expect("mounts")
                    .iter()
                    .map(|(id, mount)| (*id, Arc::clone(mount)))
                    .collect();
                for (id, mount) in mounts {
                    advance(id, &mount, publication, context);
                }
            }));

        let weak = Arc::downgrade(&views);
        let close_subscription = session.subscribe_close(Arc::new(move || {
            let Some(views) = weak.upgrade() else {
                return;
            };
            views.closed.store(true, Ordering::Release);
            let mounts: Vec<Arc<Mount>> = views
                .mounts
                .lock()
                .expect("mounts")
                .values()
                .cloned()
                .collect();
            for mount in mounts {
                let observers: Vec<Arc<dyn ViewObserver>> =
                    mount.observers.lock().expect("observers").clone();
                for observer in observers {
                    observer.close_session();
                }
            }
            views.mounts.lock().expect("mounts").clear();
        }));
        *views.commit_subscription.lock().expect("subscription") = commit_subscription.ok();
        *views.close_subscription.lock().expect("subscription") = close_subscription.ok();
        views
    }

    /// 对应 `state(id, context)`：视图的可释放只读 chord 状态。
    pub async fn state(
        &self,
        id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<AttachedReplicatedState, SessionError> {
        let (source, _detach) = self
            .attach(
                id,
                |value, handle, _storage| {
                    Box::pin(
                        async move { Ok(CommittedStateSource::new(value, handle.into_release())) },
                    )
                },
                context,
            )
            .await?;
        Ok(attach_replicated_state_source(source.as_ref(), None))
    }

    /// 对应 `watch(id, context)`：视图的串行精确帧观察；取消 `context` 会停止它。
    pub async fn watch(
        &self,
        id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<Arc<CommittedWatch>, SessionError> {
        let signal = context.abort_signal();
        let (watch, _detach) = self
            .attach(
                id,
                |value, handle, _storage| {
                    Box::pin(
                        async move { Ok(CommittedWatch::new(value, handle.into_release(), None)) },
                    )
                },
                Arc::clone(&context),
            )
            .await?;
        if signal.as_ref().is_some_and(|signal| signal.aborted()) {
            watch.cancel();
            return Err(SessionError::Aborted(pi_ai::AbortError));
        }
        if let Some(signal) = signal {
            watch.observe_cancellation(signal);
        }
        Ok(watch)
    }

    /// 对应 `attach(id, create, context)`：在 Session 线上原子地注册一个从当前修订创建的观察者。
    ///
    /// 它会看到之后的每个发布，且看不到更早的。`create` 仍可在线上读取已提交存储。
    pub async fn attach<O, F>(
        &self,
        id: ConversationId,
        create: F,
        context: Arc<dyn Context>,
    ) -> Result<(Arc<O>, ReleaseSlot), SessionError>
    where
        O: ViewObserver + 'static,
        F: FnOnce(
            JsonValue,
            ReleaseHandle,
            Arc<dyn Storage>,
        ) -> BoxFuture<'static, Result<O, SessionError>>,
    {
        let session = Arc::clone(&self.session);
        let views = self
            .self_ref
            .get()
            .and_then(Weak::upgrade)
            .expect("views self reference");
        let storage = Arc::clone(&self.storage);
        let id_for_job = id;
        session
            .read_on_line(move || {
                let views = Arc::clone(&views);
                let storage = Arc::clone(&storage);
                async move {
                    // 先取出已有挂载并释放锁：`std::sync::Mutex` 不可重入，
                    // 而构建分支还要再次写入 `mounts`。
                    let existing = views
                        .mounts
                        .lock()
                        .expect("mounts")
                        .get(&id_for_job)
                        .cloned();
                    let mount = match existing {
                        Some(mount) => mount,
                        None => {
                            let mount = Arc::new(views.build(id_for_job, &context).await?);
                            views
                                .mounts
                                .lock()
                                .expect("mounts")
                                .insert(id_for_job, Arc::clone(&mount));
                            mount
                        }
                    };

                    let slot = ReleaseSlot::new();
                    let handle = ReleaseHandle { slot: slot.clone() };
                    let value = mount.value.lock().expect("value").clone();
                    let observer = Arc::new(create(value, handle, Arc::clone(&storage)).await?);

                    // 挂载构建期间可能已经开始关闭或取消；此时不注册任何东西。
                    if views.closed.load(Ordering::Acquire) {
                        return Err(crate::harness::util::closed_error());
                    }
                    if let Some(signal) = context.abort_signal() {
                        signal.throw_if_aborted()?;
                    }

                    mount
                        .observers
                        .lock()
                        .expect("observers")
                        .push(Arc::clone(&observer) as Arc<dyn ViewObserver>);

                    let mount_for_release = Arc::clone(&mount);
                    let observer_for_release: Arc<dyn ViewObserver> =
                        Arc::clone(&observer) as Arc<dyn ViewObserver>;
                    let views_for_release = Arc::downgrade(&views);
                    slot.set(Box::new(move || {
                        mount_for_release
                            .observers
                            .lock()
                            .expect("observers")
                            .retain(|candidate| !Arc::ptr_eq(candidate, &observer_for_release));
                        if mount_for_release
                            .observers
                            .lock()
                            .expect("observers")
                            .is_empty()
                            && let Some(views) = views_for_release.upgrade()
                        {
                            let mut mounts = views.mounts.lock().expect("mounts");
                            if mounts
                                .get(&id_for_job)
                                .is_some_and(|current| Arc::ptr_eq(current, &mount_for_release))
                            {
                                mounts.remove(&id_for_job);
                            }
                        }
                    }));
                    Ok((observer, slot))
                }
            })
            .await
    }

    /// 对应 `#build(id, context)`。
    async fn build(
        &self,
        id: ConversationId,
        context: &Arc<dyn Context>,
    ) -> Result<Mount, SessionError> {
        let storage = self.storage.as_ref();
        let Some(conversation) = storage.conversation(id, context.as_ref()).await? else {
            return Err(SessionError::Message(format!(
                "Conversation {id} does not exist"
            )));
        };
        let bounds =
            crate::harness::context::capture_context_bounds(storage, id, context.as_ref(), None)
                .await?;
        let entries =
            crate::harness::context::active_entries(storage, id, bounds.as_ref(), context.as_ref())
                .await?;
        let mut docs: BTreeMap<String, JsonObject> = BTreeMap::new();
        let mut incarnations: BTreeMap<String, DocIncarnation> = BTreeMap::new();
        for token in mounted_docs() {
            let Some(loaded) = self
                .session
                .conversation_document_on_line(&**token, id, Arc::clone(context))
                .await?
            else {
                continue;
            };
            let kind = token.definition().kind().to_string();
            docs.insert(kind.clone(), loaded.value);
            incarnations.insert(
                kind,
                DocIncarnation {
                    id: loaded.record.id,
                    version: loaded.version,
                },
            );
        }
        let view = ConversationView {
            conversation,
            entries,
            docs,
        };
        Ok(Mount {
            value: Mutex::new(serde_json::to_value(view).expect("view serialises")),
            docs: Mutex::new(incarnations),
            observers: Mutex::new(Vec::new()),
        })
    }
}

/// 对应 `attach` 里交给 `create` 的 `release`。
#[derive(Clone)]
pub struct ReleaseHandle {
    slot: ReleaseSlot,
}

impl ReleaseHandle {
    /// 由挂载内部构造（对应 `attach` 把 `release` 交给 `create`）。
    pub(crate) fn from_slot(slot: ReleaseSlot) -> Self {
        Self { slot }
    }

    /// 对应的释放闭包（供 `CommittedStateSource` / `CommittedWatch` 使用）。
    pub fn into_release(self) -> impl FnOnce() + Send + Sync + 'static {
        let slot = self.slot;
        move || slot.run()
    }
}

/// 对应 `MOUNTED`：被挂载的内置会话文档。
fn mounted_docs() -> [&'static std::sync::LazyLock<DocToken>; 5] {
    [
        &crate::harness::agent::AGENT_DOC,
        &crate::harness::live::LIVE_DOC,
        &crate::harness::inbox::INBOX_DOC,
        &crate::harness::provider::PROVIDER_DOC,
        &crate::harness::usage::USAGE_DOC,
    ]
}

/// 对应 `MOUNTED_KINDS`。
fn is_mounted_kind(kind: &str) -> bool {
    mounted_docs()
        .iter()
        .any(|token| token.definition().kind() == kind)
}

/// 对应 `advance(id, mount, publication, context)`：从一个发布派生挂载操作、应用并交给每个观察者。
fn advance(
    id: ConversationId,
    mount: &Mount,
    publication: &CommitPublication,
    context: &Arc<dyn Context>,
) {
    let mut doc_ops: Vec<Op> = Vec::new();
    let mut entry_ops: Vec<Op> = Vec::new();
    let mut entries: Vec<EntryRecord> = match serde_json::from_value::<ConversationView>(
        mount.value.lock().expect("value").clone(),
    ) {
        Ok(view) => view.entries,
        Err(_) => Vec::new(),
    };

    for change in &publication.changes {
        match change {
            crate::types::CommitChange::Table(crate::types::TableCommitChange::Entry(entry)) => {
                if entry.conversation_id != id {
                    continue;
                }
                let value = serde_json::to_value(entry).expect("entry serialises");
                let entries_path = vec![PathSegment::Key("entries".to_string())];
                match entry.head {
                    None => {
                        // 条目写入按 ID 顺序发布。
                        entry_ops.push(Op::Splice(entries_path, entries.len(), 0, vec![value]));
                        entries.push(entry.clone());
                    }
                    Some(target) => {
                        // head 标记保有其 head 起的非 head 条目（它们总是后缀），并放到最前。
                        let kept = entries
                            .iter()
                            .position(|candidate| {
                                candidate.head.is_none() && candidate.id >= target
                            })
                            .unwrap_or(entries.len());
                        entry_ops.push(Op::Splice(entries_path, 0, kept, vec![value]));
                        let mut next = vec![entry.clone()];
                        next.extend(entries[kept..].iter().cloned());
                        entries = next;
                    }
                }
            }
            crate::types::CommitChange::Document(change) => {
                handle_document_change(id, mount, change, &mut doc_ops);
            }
            _ => {}
        }
    }

    let before = mount.value.lock().expect("value").clone();
    let mut ops = Vec::with_capacity(doc_ops.len() + entry_ops.len());
    ops.extend(doc_ops.iter().cloned());
    ops.extend(entry_ops.iter().cloned());
    let frame_context = without_abort_signal(Arc::clone(context));
    if !ops.is_empty() {
        let after = {
            let mut value = if doc_ops.is_empty() {
                before.clone()
            } else {
                apply_immutable(Some(before.clone()), &doc_ops).unwrap_or(before.clone())
            };
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "entries".to_string(),
                    serde_json::to_value(&entries).expect("entries serialise"),
                );
            }
            *mount.value.lock().expect("value") = value.clone();
            value
        };
        let observers: Vec<Arc<dyn ViewObserver>> =
            mount.observers.lock().expect("observers").clone();
        for observer in &observers {
            observer.advance(after.clone(), &ops, Arc::clone(&frame_context));
        }
        let after = mount.value.lock().expect("value").clone();
        for observer in &observers {
            observer.publication(
                &before,
                &after,
                &ops,
                publication,
                Arc::clone(&frame_context),
            );
        }
        return;
    }
    let after = mount.value.lock().expect("value").clone();
    let observers: Vec<Arc<dyn ViewObserver>> = mount.observers.lock().expect("observers").clone();
    for observer in &observers {
        observer.publication(
            &before,
            &after,
            &ops,
            publication,
            Arc::clone(&frame_context),
        );
    }
}

fn handle_document_change(
    id: ConversationId,
    mount: &Mount,
    change: &crate::types::DocumentCommitChange,
    doc_ops: &mut Vec<Op>,
) {
    let crate::types::DocumentCommitChange::Document {
        record,
        conversation_id,
        version,
        value,
        ops,
    } = change
    else {
        return;
    };
    if *conversation_id != Some(id) {
        return;
    }
    let kind = record.kind.clone();
    if !is_mounted_kind(&kind) || record.key.is_some() {
        return;
    }
    let path: Path = vec![
        PathSegment::Key("docs".to_string()),
        PathSegment::Key(kind.clone()),
    ];
    let mounted = mount.docs.lock().expect("docs").get(&kind).copied();
    let matches_incarnation = mounted.is_some_and(|mounted| mounted.id == record.id);
    let matches_version =
        mounted.is_some_and(|mounted| mounted.version == version.unwrap_or_default());
    if value.is_none() {
        if !matches_incarnation {
            return;
        }
        mount.docs.lock().expect("docs").remove(&kind);
        doc_ops.push(Op::Delete(path));
    } else if matches_incarnation && matches_version {
        for op in ops {
            doc_ops.push(prefixed(op, &path));
        }
    } else {
        mount.docs.lock().expect("docs").insert(
            kind,
            DocIncarnation {
                id: record.id,
                version: version.unwrap_or_default(),
            },
        );
        doc_ops.push(Op::Set(
            path,
            value
                .as_ref()
                .map(|object| JsonValue::Object(object.clone()))
                .unwrap_or(JsonValue::Null),
        ));
    }
}

/// 对应 `prefixed(op, prefix)`：`op` 移到 `prefix` 之下；根替换变成对前缀的一次 set。
fn prefixed(op: &Op, prefix: &[PathSegment]) -> Op {
    let at = |path: &Path| -> Path {
        prefix
            .iter()
            .cloned()
            .chain(path.iter().cloned())
            .collect::<Path>()
    };
    match op {
        Op::Replace(value) => Op::Set(prefix.to_vec(), value.clone()),
        Op::Set(path, value) => Op::Set(at(path), value.clone()),
        Op::Delete(path) => Op::Delete(at(path)),
        Op::Append(path, text) => Op::Append(at(path), text.clone()),
        Op::Truncate(path, count) => Op::Truncate(at(path), *count),
        Op::Splice(path, index, delete_count, items) => {
            Op::Splice(at(path), *index, *delete_count, items.clone())
        }
        Op::Move(path, permutation) => Op::Move(at(path), permutation.clone()),
    }
}

/// 便于阅读：`attach` 的 `create` 回调返回类型（上游允许 `O | Promise<O>`）。
#[allow(dead_code)]
type CreateResult<O> = BoxFuture<'static, Result<O, SessionError>>;

#[allow(dead_code)]
fn _unused_context_marker(_: &dyn Context) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConversationParent, Seq};

    fn conversation_record() -> ConversationRecord {
        ConversationRecord {
            id: ConversationId::new(1),
            parent: Some(ConversationParent {
                conversation_id: ConversationId::new(2),
                at: crate::types::EntryId::new(3),
            }),
            owner: None,
        }
    }

    fn entry(id: u64, head: Option<u64>) -> EntryRecord {
        EntryRecord {
            id: crate::types::EntryId::new(id),
            conversation_id: ConversationId::new(1),
            kind: "pi.user".to_string(),
            model: None,
            data: None,
            head: head.map(crate::types::EntryId::new),
            edits: None,
            by_task_id: None,
        }
    }

    #[test]
    fn view_serialises_with_camel_case_and_survives_a_round_trip() {
        let mut docs = BTreeMap::new();
        let mut agent = JsonObject::new();
        agent.insert("cwd".to_string(), JsonValue::from("/work"));
        docs.insert("pi.agent".to_string(), agent);
        let view = ConversationView {
            conversation: conversation_record(),
            entries: vec![entry(1, None), entry(2, Some(1))],
            docs,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["conversation"]["id"], serde_json::json!(1));
        assert_eq!(json["docs"]["pi.agent"]["cwd"], serde_json::json!("/work"));
        assert_eq!(json["entries"][1]["head"], serde_json::json!(1));
        assert_eq!(
            serde_json::from_value::<ConversationView>(json).unwrap(),
            view
        );
    }

    #[test]
    fn prefixed_moves_every_op_kind_under_the_prefix() {
        let prefix: Path = vec![
            PathSegment::Key("docs".to_string()),
            PathSegment::Key("pi.agent".to_string()),
        ];
        let path: Path = vec![PathSegment::Key("cwd".to_string())];

        assert_eq!(
            prefixed(&Op::Replace(serde_json::json!(1)), &prefix),
            Op::Set(prefix.clone(), serde_json::json!(1)),
            "根替换变成前缀上的 set"
        );
        assert_eq!(
            prefixed(&Op::Set(path.clone(), serde_json::json!(2)), &prefix),
            Op::Set(
                vec![
                    PathSegment::Key("docs".to_string()),
                    PathSegment::Key("pi.agent".to_string()),
                    PathSegment::Key("cwd".to_string())
                ],
                serde_json::json!(2)
            )
        );
        assert_eq!(
            prefixed(&Op::Delete(path.clone()), &prefix),
            Op::Delete(vec![
                PathSegment::Key("docs".to_string()),
                PathSegment::Key("pi.agent".to_string()),
                PathSegment::Key("cwd".to_string())
            ])
        );
        assert_eq!(
            prefixed(
                &Op::Splice(path.clone(), 1, 2, vec![serde_json::json!(3)]),
                &prefix
            )
            .clone(),
            Op::Splice(
                vec![
                    PathSegment::Key("docs".to_string()),
                    PathSegment::Key("pi.agent".to_string()),
                    PathSegment::Key("cwd".to_string())
                ],
                1,
                2,
                vec![serde_json::json!(3)]
            )
        );
        assert_eq!(
            prefixed(&Op::Append(path.clone(), "x".to_string()), &prefix),
            Op::Append(
                vec![
                    PathSegment::Key("docs".to_string()),
                    PathSegment::Key("pi.agent".to_string()),
                    PathSegment::Key("cwd".to_string())
                ],
                "x".to_string()
            )
        );
        assert_eq!(
            prefixed(&Op::Truncate(path.clone(), 4), &prefix),
            Op::Truncate(
                vec![
                    PathSegment::Key("docs".to_string()),
                    PathSegment::Key("pi.agent".to_string()),
                    PathSegment::Key("cwd".to_string())
                ],
                4
            )
        );
        assert_eq!(
            prefixed(&Op::Move(path.clone(), vec![1, 0]), &prefix),
            Op::Move(
                vec![
                    PathSegment::Key("docs".to_string()),
                    PathSegment::Key("pi.agent".to_string()),
                    PathSegment::Key("cwd".to_string())
                ],
                vec![1, 0]
            )
        );
    }

    #[test]
    fn mounted_kinds_are_exactly_the_five_built_ins() {
        for kind in ["pi.agent", "pi.live", "pi.inbox", "pi.provider", "pi.usage"] {
            assert!(is_mounted_kind(kind), "{kind} 应被挂载");
        }
        assert!(!is_mounted_kind("pi.other"));
    }

    #[test]
    fn release_slot_runs_once() {
        let runs = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let slot = ReleaseSlot::new();
        let counter = Arc::clone(&runs);
        slot.set(Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        slot.run();
        slot.run();
        assert_eq!(runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn conversation_records_carry_their_parent_and_sequence() {
        // 让 `Seq` 的引用固定下来（视图值会序列化创建序号）。
        let record = conversation_record();
        assert_eq!(record.id, ConversationId::new(1));
        assert!(record.parent.is_some());
        assert_eq!(Seq::new(1).get(), 1);
    }
}
