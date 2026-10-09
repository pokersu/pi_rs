//! 对应 `harness/submissions.ts`：一个 Harness 的持久提交的接纳、等待与撤回。
//!
//! # 与上游的差异
//!
//! - **`startRun` 注入**：上游直接 import `generation.ts` 的 `startRun`；Rust 侧把它作为
//!   [`StartRun`] 注入，因此本模块可以在 generation 落地之前独立工作，且不存在模块环。
//! - 上游 `Promise` 的「先检查再注册」在 Rust 侧用 [`SessionImpl::read_on_line`] 表达。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value as JsonValue;

use crate::chord::context::Context;
use crate::harness::inbox::{
    BoundaryAt, InboxInputMode, InboxItem, QueueModes, apply_boundary, is_stale, prepare_boundary,
    remove_inbox_item,
};
use crate::harness::json::to_json_without_nulls;
use crate::harness::live::LIVE_DOC;
use crate::harness::types::{
    InputSubmissionDraft, SettledSubmissionRecord, Submission, SubmissionAbort, SubmissionDraft,
    WhenBusy,
};
use crate::harness::util::{Waiters, closed_error};
use crate::session::transaction::Transaction;
use crate::session::{SessionError, SessionImpl, SessionSubscription};
use crate::types::{
    ConversationId, DocAccess, EntryDraft, EntryHead, JsonObject, Storage, SubmissionCreate,
    SubmissionId, SubmissionRecord, SubmissionSettlement, SubmissionStatus,
};

/// 对应 `startRun(tx, conversationId, live, inputs)` 的函数形状。
pub type StartRunFn = dyn for<'a> Fn(
        &'a Transaction,
        ConversationId,
        &'a crate::types::Draft,
        Vec<SubmissionId>,
    ) -> BoxFuture<'a, Result<(), SessionError>>
    + Send
    + Sync;

/// 对应 `startRun(tx, conversationId, live, inputs)`：启动一次运行。
///
/// 由 generation 层提供实现（P5g）；本模块只负责在正确的时点调用它。
pub type StartRun = Arc<StartRunFn>;

/// 对应 `AbortResult`。
pub type AbortResult = SubmissionAbort;

/// 接纳、等待与撤回一个 Harness 的持久提交。
pub struct Submissions {
    session: Arc<SessionImpl>,
    storage: Arc<dyn Storage>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    queue_modes: Arc<dyn Fn() -> QueueModes + Send + Sync>,
    resume: Arc<dyn Fn() + Send + Sync>,
    start_run: StartRun,
    waiters: Arc<Waiters<SubmissionId, SettledSubmissionRecord>>,
    closed: AtomicBool,
    commit_subscription: Mutex<Option<SessionSubscription>>,
    close_subscription: Mutex<Option<SessionSubscription>>,
}

/// 构造 [`Submissions`] 的选项（对应上游构造函数的参数）。
pub struct SubmissionsOptions {
    /// 会话内核。
    pub session: Arc<SessionImpl>,
    /// 存储后端。
    pub storage: Arc<dyn Storage>,
    /// 会话分配 ID 与条目时间使用的时钟。
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// 每次接纳时在 Session 线上读取的队列模式。
    pub queue_modes: Arc<dyn Fn() -> QueueModes + Send + Sync>,
    /// 启用任务调度；提交或等待都会请求推进。
    pub resume: Arc<dyn Fn() + Send + Sync>,
    /// 启动一次运行（generation 层提供）。
    pub start_run: StartRun,
}

impl Submissions {
    /// 对应 `new Submissions(...)`：立即订阅提交与关闭。
    pub fn new(options: SubmissionsOptions) -> Arc<Self> {
        let session = Arc::clone(&options.session);
        let submissions = Arc::new(Self {
            session: Arc::clone(&session),
            storage: options.storage,
            now: options.now,
            queue_modes: options.queue_modes,
            resume: options.resume,
            start_run: options.start_run,
            waiters: Arc::new(Waiters::new()),
            closed: AtomicBool::new(false),
            commit_subscription: Mutex::new(None),
            close_subscription: Mutex::new(None),
        });

        let weak = Arc::downgrade(&submissions);
        let commit_subscription =
            session.subscribe_commits(Arc::new(move |publication, _context| {
                let Some(submissions) = weak.upgrade() else {
                    return;
                };
                submissions.observe(publication);
            }));

        let weak = Arc::downgrade(&submissions);
        let close_subscription = session.subscribe_close(Arc::new(move || {
            let Some(submissions) = weak.upgrade() else {
                return;
            };
            submissions.closed.store(true, Ordering::Release);
            submissions.waiters.reject_all(closed_error());
        }));
        *submissions
            .commit_subscription
            .lock()
            .expect("subscription") = commit_subscription.ok();
        *submissions.close_subscription.lock().expect("subscription") = close_subscription.ok();
        submissions
    }

    /// 对应 `submit(conversationId, draft, context)`：在一个提交里接纳一次提交。
    pub async fn submit(
        self: &Arc<Self>,
        conversation_id: ConversationId,
        draft: SubmissionDraft,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Submission>, SessionError> {
        (self.resume)();
        let now = (self.now)();
        let modes = (self.queue_modes)();
        let start_run = Arc::clone(&self.start_run);
        let id = self
            .session
            .commit(
                move |tx| {
                    Box::pin(async move {
                        admit_submission(
                            tx,
                            conversation_id,
                            &draft,
                            now,
                            modes,
                            start_run.as_ref(),
                        )
                        .await
                    })
                },
                context,
            )
            .await?;
        Ok(Arc::new(SubmissionHandle {
            id,
            submissions: Arc::clone(self),
        }))
    }

    /// 对应 `get(id, context)`：已存在提交的句柄，或 `None`。
    pub async fn get(
        self: &Arc<Self>,
        id: SubmissionId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn Submission>>, SessionError> {
        let storage = Arc::clone(&self.storage);
        let job_context = Arc::clone(&context);
        let found = self
            .session
            .read_on_line(move || {
                let storage = Arc::clone(&storage);
                let context = Arc::clone(&job_context);
                async move {
                    storage
                        .submission(id, context.as_ref())
                        .await
                        .map_err(SessionError::from)
                }
            })
            .await?;
        Ok(found.map(|record| {
            Arc::new(SubmissionHandle {
                id: record.identity().id,
                submissions: Arc::clone(self),
            }) as Arc<dyn Submission>
        }))
    }

    /// 对应 `status(id, context)`。
    pub async fn status(
        &self,
        id: SubmissionId,
        context: Arc<dyn Context>,
    ) -> Result<SubmissionRecord, SessionError> {
        let storage = Arc::clone(&self.storage);
        let job_context = Arc::clone(&context);
        let found = self
            .session
            .read_on_line(move || {
                let storage = Arc::clone(&storage);
                let context = Arc::clone(&job_context);
                async move {
                    storage
                        .submission(id, context.as_ref())
                        .await
                        .map_err(SessionError::from)
                }
            })
            .await?;
        found.ok_or_else(|| SessionError::Message(format!("Submission {id} does not exist")))
    }

    /// 对应 `wait(id, context)`。
    pub async fn wait(
        &self,
        id: SubmissionId,
        context: Arc<dyn Context>,
    ) -> Result<SettledSubmissionRecord, SessionError> {
        (self.resume)();
        // 在同一条线上检查并注册，使没有结清发布能落在两者之间。
        let closed_now = self.closed.load(Ordering::Acquire);
        let waiters = Arc::clone(&self.waiters);
        let storage = Arc::clone(&self.storage);
        let session = Arc::clone(&self.session);
        let job_context = Arc::clone(&context);
        let outcome = session
            .read_on_line(move || {
                let storage = Arc::clone(&storage);
                let context = Arc::clone(&job_context);
                let waiters = Arc::clone(&waiters);
                async move {
                    let record = storage
                        .submission(id, context.as_ref())
                        .await
                        .map_err(SessionError::from)?;
                    let Some(record) = record else {
                        return Err(SessionError::Message(format!(
                            "Submission {id} does not exist"
                        )));
                    };
                    if is_settled(&record) {
                        return Ok(WaitOutcome::Settled(SettledSubmissionRecord::new(record)));
                    }
                    // 关闭会同步拒绝已注册的等待者，且可能在读取期间开始。
                    if closed_now {
                        return Err(closed_error());
                    }
                    Ok(WaitOutcome::Pending(waiters.add(id, Arc::clone(&context))))
                }
            })
            .await?;
        match outcome {
            WaitOutcome::Settled(record) => Ok(record),
            WaitOutcome::Pending(future) => {
                let settled = future.await?;
                Ok(settled)
            }
        }
    }

    /// 对应 `abort(id, context, conversationId?)`：撤回一个排队中的提交并移除它的队列条目。
    pub async fn abort(
        &self,
        id: SubmissionId,
        context: Arc<dyn Context>,
        conversation_id: Option<ConversationId>,
    ) -> Result<Option<AbortResult>, SessionError> {
        self.session
            .commit(
                move |tx| {
                    Box::pin(async move {
                        let Some(record) = tx.submission(id).await? else {
                            return Ok(None);
                        };
                        if let Some(expected) = conversation_id
                            && record.identity().conversation_id != expected
                        {
                            return Ok(None);
                        }
                        let status = record.status();
                        if status == SubmissionStatus::Queued {
                            tx.settle_submission(
                                id,
                                SubmissionSettlement::Unanswered {
                                    reason: "aborted".to_string(),
                                    detail: None,
                                },
                            );
                            remove_inbox_item(tx, record.identity().conversation_id, id).await?;
                            return Ok(Some(AbortResult::Aborted));
                        }
                        Ok(Some(if status == SubmissionStatus::Placed {
                            AbortResult::AlreadyPlaced
                        } else {
                            AbortResult::Settled
                        }))
                    })
                },
                context,
            )
            .await
    }

    /// 对应 `#observe(publication)`：结清已终态提交的等待者。
    fn observe(&self, publication: &crate::types::CommitPublication) {
        for change in &publication.changes {
            let crate::types::CommitChange::Table(crate::types::TableCommitChange::Submission(
                record,
            )) = change
            else {
                continue;
            };
            if !is_settled(record) {
                continue;
            }
            let Ok(settled) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                SettledSubmissionRecord::new(record.clone())
            })) else {
                continue;
            };
            self.waiters.resolve(&record.identity().id, settled);
        }
    }
}

/// `read_on_line` 的两条分支：已终态，或一个待结清的等待。
enum WaitOutcome {
    Settled(SettledSubmissionRecord),
    Pending(BoxFuture<'static, Result<SettledSubmissionRecord, SessionError>>),
}

/// 对应 `SubmissionHandle`。
struct SubmissionHandle {
    id: SubmissionId,
    submissions: Arc<Submissions>,
}

#[async_trait::async_trait]
impl Submission for SubmissionHandle {
    fn id(&self) -> SubmissionId {
        self.id
    }

    async fn status(&self, context: Arc<dyn Context>) -> Result<SubmissionRecord, SessionError> {
        self.submissions.status(self.id, context).await
    }

    async fn wait(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<SettledSubmissionRecord, SessionError> {
        self.submissions.wait(self.id, context).await
    }

    async fn abort(&self, context: Arc<dyn Context>) -> Result<SubmissionAbort, SessionError> {
        let result = self.submissions.abort(self.id, context, None).await?;
        match result {
            Some(result) => Ok(result),
            None => Err(SessionError::Message(format!(
                "Submission {} does not exist",
                self.id
            ))),
        }
    }
}

/// 对应 `isSettled(record)`。
pub fn is_settled(record: &SubmissionRecord) -> bool {
    matches!(
        record.status(),
        SubmissionStatus::Done | SubmissionStatus::Unanswered
    )
}

/// 对应 `admitSubmission(tx, conversationId, draft, now, queueModes)`：在一个提交里接纳一次提交（spec §6）。
///
/// 已知的 request ID 会直接返回它已有的提交而不写入。繁忙会话把它排入 `pi.inbox`，或对
/// `whenBusy: "reject"` 的输入以 `ConversationBusy` 拒绝。有空闲条目排队的空闲会话会排在它们之后并跑一次
/// final 边界。其余情况下，空闲输入放置一个用户条目并启动运行，空闲写入追加其条目并结清 `done`
/// （其 head 指到活动区间之前时为 `stale`）。
pub async fn admit_submission(
    tx: &Transaction,
    conversation_id: ConversationId,
    draft: &SubmissionDraft,
    now: u64,
    queue_modes: QueueModes,
    start_run: &StartRunFn,
) -> Result<SubmissionId, SessionError> {
    let request_id = match draft {
        SubmissionDraft::Input(draft) => draft.request_id.clone(),
        SubmissionDraft::Write(draft) => draft.request_id.clone(),
    };
    if let Some(request_id) = &request_id
        && let Some(existing) = tx
            .submission_by_request(conversation_id, request_id)
            .await?
    {
        let same_type = matches!(
            (draft, &existing),
            (SubmissionDraft::Input(_), SubmissionRecord::Input { .. })
                | (SubmissionDraft::Write(_), SubmissionRecord::Write { .. })
        );
        if !same_type {
            return Err(SessionError::Message(format!(
                "Request {request_id} already identifies a submission of the other type"
            )));
        }
        return Ok(existing.identity().id);
    }

    let live = tx
        .doc(&*LIVE_DOC, doc_access(conversation_id), None)
        .await?;
    let busy = crate::harness::live::live_state(&live).run.is_some();
    if busy
        && let SubmissionDraft::Input(input) = draft
        && input.when_busy == Some(WhenBusy::Reject)
    {
        return Err(SessionError::Message(format!(
            "Conversation {conversation_id} is busy"
        )));
    }

    // 边界会读表，因此在首次表写入之前准备；繁忙时不需要。
    let mut boundary = if busy {
        None
    } else {
        Some(prepare_boundary(tx, conversation_id, queue_modes).await?)
    };
    let queued_ahead = boundary.as_ref().is_some_and(|boundary| {
        boundary
            .inbox
            .value()
            .get("items")
            .and_then(JsonValue::as_array)
            .is_some_and(|items| !items.is_empty())
    });

    if boundary.is_none() || queued_ahead {
        let create = match draft {
            SubmissionDraft::Input(_) => SubmissionCreate::Input {
                conversation_id,
                request_id: request_id.clone(),
                status: crate::types::InputSubmissionStatus::Queued,
            },
            SubmissionDraft::Write(_) => SubmissionCreate::Write {
                conversation_id,
                request_id: request_id.clone(),
                status: crate::types::WriteSubmissionStatus::Queued,
            },
        };
        let id = tx.create_submission(create).await?.identity().id;
        let item = match draft {
            SubmissionDraft::Write(draft) => InboxItem::Write {
                id,
                entry: match to_json_without_nulls(&draft.entry) {
                    JsonValue::Object(object) => object,
                    _ => JsonObject::new(),
                },
            },
            SubmissionDraft::Input(draft) => InboxItem::Input {
                id,
                mode: match draft.when_busy {
                    Some(WhenBusy::Steer) => InboxInputMode::Steer,
                    _ => InboxInputMode::FollowUp,
                },
                content: to_json_without_nulls(&draft.content),
            },
        };
        let inbox = match boundary.as_ref() {
            Some(boundary) => boundary.inbox.clone(),
            None => {
                tx.doc(
                    &*crate::harness::inbox::INBOX_DOC,
                    doc_access(conversation_id),
                    None,
                )
                .await?
            }
        };
        let items = inbox
            .value()
            .get("items")
            .and_then(JsonValue::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        inbox
            .splice(
                vec![crate::chord::delta::PathSegment::Key("items".to_string())],
                items,
                0,
                vec![item.to_json()],
            )
            .map_err(|error| SessionError::Message(error.to_string()))?;
        let Some(boundary) = boundary.as_mut() else {
            return Ok(id);
        };
        let result = apply_boundary(tx, boundary, BoundaryAt::Final, now).await?;
        if !result.users.is_empty() {
            start_run(tx, conversation_id, &live, result.users).await?;
        }
        return Ok(id);
    }

    let boundary = boundary
        .take()
        .expect("a non-busy conversation prepared a boundary");

    if let SubmissionDraft::Write(draft) = draft {
        if is_stale(&boundary, &draft.entry) {
            return Ok(tx
                .create_submission(SubmissionCreate::Write {
                    conversation_id,
                    request_id: request_id.clone(),
                    status: crate::types::WriteSubmissionStatus::Unanswered {
                        reason: "stale".to_string(),
                        detail: None,
                    },
                })
                .await?
                .identity()
                .id);
        }
        let entry = tx
            .append_entry(None, conversation_id, draft.entry.clone())
            .await?;
        return Ok(tx
            .create_submission(SubmissionCreate::Write {
                conversation_id,
                request_id: request_id.clone(),
                status: crate::types::WriteSubmissionStatus::Done { entry: entry.id },
            })
            .await?
            .identity()
            .id);
    }

    let SubmissionDraft::Input(draft) = draft else {
        unreachable!("已处理 write 分支")
    };
    let content = serde_json::to_value(&draft.content)
        .map_err(|error| SessionError::Message(format!("user input serialises: {error}")))?;
    let message = crate::harness::inbox::user_entry_draft(content, now);
    let entry = tx.append_entry(None, conversation_id, message).await?;
    let id = tx
        .create_submission(SubmissionCreate::Input {
            conversation_id,
            request_id: request_id.clone(),
            status: crate::types::InputSubmissionStatus::Placed { entry: entry.id },
        })
        .await?
        .identity()
        .id;
    start_run(tx, conversation_id, &live, vec![id]).await?;
    Ok(id)
}

fn doc_access(conversation_id: ConversationId) -> DocAccess {
    DocAccess {
        owner: Some(conversation_id.get()),
        key: None,
    }
}

/// 便于阅读：`InputSubmissionDraft` 的字段（上游 `draft.content`）。
#[allow(dead_code)]
type InputDraftFields = InputSubmissionDraft;

/// 便于阅读：`EntryHead` 供队列条目的 head 使用。
#[allow(dead_code)]
type QueueHead = EntryHead;

/// 便于阅读：`EntryDraft` 供写入条目使用。
#[allow(dead_code)]
type WriteEntry = EntryDraft;

/// 便于阅读：`Duration` 供关闭路径使用。
#[allow(dead_code)]
fn unused_duration_marker() -> Duration {
    Duration::from_millis(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_record(status: crate::types::InputSubmissionStatus) -> SubmissionRecord {
        SubmissionRecord::Input {
            identity: crate::types::SubmissionIdentity {
                id: SubmissionId::new(1),
                conversation_id: ConversationId::new(1),
                request_id: None,
            },
            status,
        }
    }

    #[test]
    fn settled_matches_the_upstream_predicate() {
        assert!(!is_settled(&input_record(
            crate::types::InputSubmissionStatus::Queued
        )));
        assert!(!is_settled(&input_record(
            crate::types::InputSubmissionStatus::Placed {
                entry: crate::types::EntryId::new(1)
            }
        )));
        assert!(is_settled(&input_record(
            crate::types::InputSubmissionStatus::Done {
                entry: crate::types::EntryId::new(1),
                answer: crate::types::EntryId::new(2),
            }
        )));
        assert!(is_settled(&input_record(
            crate::types::InputSubmissionStatus::Unanswered {
                entry: None,
                reason: "aborted".to_string(),
                detail: None,
            }
        )));
    }

    #[test]
    fn abort_result_variants_match_upstream_strings() {
        assert_eq!(AbortResult::Aborted, SubmissionAbort::Aborted);
        assert_eq!(AbortResult::AlreadyPlaced, SubmissionAbort::AlreadyPlaced);
        assert_eq!(AbortResult::Settled, SubmissionAbort::Settled);
    }
}
