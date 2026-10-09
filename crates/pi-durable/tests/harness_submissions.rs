//! `harness::submissions` 的端到端测试：接纳、排队、拒绝、撤回与等待。
//!
//! `startRun` 由 generation 层注入（P5g）。大多数用例注入一个记录型桩（把 `pi.live.run` 写进同一提交，
//! 这正是「繁忙」判定的依据）；文件末尾另有端到端用例接上真实的 `make_start_run`。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_durable::chord::context::Context;
use pi_durable::harness::submissions::{StartRun, Submissions, SubmissionsOptions};
use pi_durable::harness::types::{
    InputSubmissionDraft, SubmissionAbort, SubmissionDraft, WhenBusy, WriteSubmissionDraft,
};
use pi_durable::harness::util::Waiters;
use pi_durable::session::{SessionError, SessionImpl, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::types::{
    ConversationId, EntryDraft, JsonObject, Storage, SubmissionId, SubmissionStatus,
};
use serde_json::{Value as JsonValue, json};

fn context() -> Arc<dyn Context> {
    Arc::new(pi_durable::chord::context::EmptyContext::new("[test]"))
}

/// 记录每次启动的输入，并把 `pi.live.run` 写进同一提交（使后续提交看到「繁忙」）。
#[derive(Clone, Default)]
struct RunLog {
    runs: Arc<Mutex<Vec<Vec<SubmissionId>>>>,
}

impl RunLog {
    fn started(&self) -> Vec<Vec<SubmissionId>> {
        self.runs.lock().expect("runs").clone()
    }

    fn start_run(&self) -> StartRun {
        let runs = Arc::clone(&self.runs);
        Arc::new(
            move |tx: &pi_durable::session::transaction::Transaction,
                  conversation_id: ConversationId,
                  live: &pi_durable::types::Draft,
                  inputs: Vec<SubmissionId>| {
                runs.lock().expect("runs").push(inputs.clone());
                let future: BoxFuture<'_, Result<(), SessionError>> = Box::pin(async move {
                    let run = json!({
                        "taskId": 1,
                        "inputs": inputs.iter().map(|id| id.get()).collect::<Vec<_>>(),
                    });
                    live.set(
                        vec![pi_durable::chord::delta::PathSegment::Key(
                            "run".to_string(),
                        )],
                        run,
                    )
                    .map_err(|error| SessionError::Message(error.to_string()))?;
                    let _ = (tx, conversation_id);
                    Ok(())
                });
                future
            },
        )
    }
}

async fn open_with(
    start_run: StartRun,
) -> (Arc<SessionImpl>, Arc<MemoryStorage>, Arc<Submissions>) {
    let storage = Arc::new(MemoryStorage::new());
    let session = create_session(Arc::clone(&storage) as Arc<dyn Storage>, None);
    session
        .commit(
            |tx| {
                Box::pin(async move {
                    tx.create_root_conversation().await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("root conversation");

    let submissions = Submissions::new(SubmissionsOptions {
        session: Arc::clone(&session),
        storage: Arc::clone(&storage) as Arc<dyn Storage>,
        now: Arc::new(|| 1_700),
        queue_modes: Arc::new(|| pi_durable::harness::inbox::QueueModes {
            steering_mode: pi_durable::harness::types::QueueMode::OneAtATime,
            follow_up_mode: pi_durable::harness::types::QueueMode::OneAtATime,
        }),
        resume: Arc::new(|| {}),
        start_run,
    });
    (session, storage, submissions)
}

/// 用记录型桩启动的调度器（多数测试用）。
async fn open() -> (
    Arc<SessionImpl>,
    Arc<MemoryStorage>,
    Arc<Submissions>,
    RunLog,
) {
    let log = RunLog::default();
    let (session, storage, submissions) = open_with(log.start_run()).await;
    (session, storage, submissions, log)
}

fn conversation() -> ConversationId {
    ConversationId::new(1)
}

fn input(content: &str) -> SubmissionDraft {
    SubmissionDraft::Input(InputSubmissionDraft {
        request_id: None,
        content: pi_ai::UserContent::Text(content.to_string()),
        when_busy: None,
    })
}

fn write_entry(value: i64) -> SubmissionDraft {
    let mut data = JsonObject::new();
    data.insert("value".to_string(), JsonValue::from(value));
    SubmissionDraft::Write(WriteSubmissionDraft {
        request_id: None,
        entry: EntryDraft {
            kind: "pi.custom".to_string(),
            model: None,
            data: Some(JsonValue::Object(data)),
            head: None,
            edits: None,
        },
    })
}

#[tokio::test]
async fn idle_input_is_placed_and_starts_a_run() {
    let (_session, storage, submissions, log) = open().await;
    let submission = submissions
        .submit(conversation(), input("hello"), context())
        .await
        .expect("submit");
    let id = submission.id();
    assert_eq!(
        submissions
            .status(id, context())
            .await
            .expect("status")
            .status(),
        SubmissionStatus::Placed,
        "空闲输入直接放置"
    );
    assert_eq!(log.started(), vec![vec![id]], "恰好启动一次运行");
    assert!(storage.submission(id, context().as_ref()).await.is_ok());
}

#[tokio::test]
async fn idle_write_is_appended_and_settled_done() {
    let (session, _storage, submissions, log) = open().await;
    let submission = submissions
        .submit(conversation(), write_entry(7), context())
        .await
        .expect("submit");
    let id = submission.id();
    assert_eq!(
        submissions
            .status(id, context())
            .await
            .expect("status")
            .status(),
        SubmissionStatus::Done,
        "空闲写入直接追加并结清"
    );
    assert!(log.started().is_empty(), "被动写入不启动运行");

    // 被动条目确实写进了 transcript。
    let entries = session
        .read_on_line(|| async { Ok::<_, SessionError>(1) })
        .await
        .expect("read");
    assert_eq!(entries, 1);
}

#[tokio::test]
async fn a_busy_conversation_queues_the_input() {
    let (_session, _storage, submissions, log) = open().await;
    // 第一次提交让会话忙起来。
    let first = submissions
        .submit(conversation(), input("first"), context())
        .await
        .expect("first");
    assert_eq!(log.started().len(), 1);

    let second = submissions
        .submit(conversation(), input("second"), context())
        .await
        .expect("second");
    assert_eq!(
        submissions
            .status(second.id(), context())
            .await
            .expect("status")
            .status(),
        SubmissionStatus::Queued,
        "繁忙时排队"
    );
    assert_eq!(log.started().len(), 1, "排队不启动运行");
    assert_ne!(first.id(), second.id());
}

#[tokio::test]
async fn reject_when_busy_fails_without_writing() {
    let (_session, _storage, submissions, _log) = open().await;
    submissions
        .submit(conversation(), input("first"), context())
        .await
        .expect("first");

    let rejected = submissions
        .submit(
            conversation(),
            SubmissionDraft::Input(InputSubmissionDraft {
                request_id: None,
                content: pi_ai::UserContent::Text("second".to_string()),
                when_busy: Some(WhenBusy::Reject),
            }),
            context(),
        )
        .await;
    let error = match rejected {
        Ok(_) => panic!("繁忙且 whenBusy=reject 应拒绝"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("is busy"),
        "错误消息应说明会话繁忙：{error}"
    );
}

#[tokio::test]
async fn a_known_request_id_returns_the_existing_submission() {
    let (_session, _storage, submissions, log) = open().await;
    let draft = SubmissionDraft::Input(InputSubmissionDraft {
        request_id: Some("req-1".to_string()),
        content: pi_ai::UserContent::Text("hello".to_string()),
        when_busy: None,
    });
    let first = submissions
        .submit(conversation(), draft.clone(), context())
        .await
        .expect("first");
    let second = submissions
        .submit(conversation(), draft, context())
        .await
        .expect("second");
    assert_eq!(first.id(), second.id(), "同一 request ID 复用提交");
    assert_eq!(log.started().len(), 1, "不重复启动运行");
}

#[tokio::test]
async fn abort_withdraws_a_queued_submission() {
    let (session, _storage, submissions, _log) = open().await;
    submissions
        .submit(conversation(), input("first"), context())
        .await
        .expect("first");
    let queued = submissions
        .submit(conversation(), input("second"), context())
        .await
        .expect("second");

    let result = submissions
        .abort(queued.id(), context(), Some(conversation()))
        .await
        .expect("abort");
    assert_eq!(result, Some(SubmissionAbort::Aborted));
    assert_eq!(
        submissions
            .status(queued.id(), context())
            .await
            .expect("status")
            .status(),
        SubmissionStatus::Unanswered
    );

    // 已放置的输入报告 already_placed，不改变记录。
    let placed_id = session
        .commit(
            |tx| {
                Box::pin(async move {
                    let entry = tx
                        .append_entry(
                            None,
                            conversation(),
                            pi_durable::harness::inbox::user_entry_draft(
                                JsonValue::from("third"),
                                1,
                            ),
                        )
                        .await?;
                    let record = tx
                        .create_submission(pi_durable::types::SubmissionCreate::Input {
                            conversation_id: conversation(),
                            request_id: None,
                            status: pi_durable::types::InputSubmissionStatus::Placed {
                                entry: entry.id,
                            },
                        })
                        .await?;
                    Ok(record.identity().id)
                })
            },
            context(),
        )
        .await
        .expect("placed submission");
    assert_eq!(
        submissions
            .abort(placed_id, context(), Some(conversation()))
            .await
            .expect("abort placed"),
        Some(SubmissionAbort::AlreadyPlaced)
    );
}

#[tokio::test]
async fn abort_of_another_conversation_reports_not_found() {
    let (_session, _storage, submissions, _log) = open().await;
    let submission = submissions
        .submit(conversation(), input("hello"), context())
        .await
        .expect("submit");
    assert_eq!(
        submissions
            .abort(submission.id(), context(), Some(ConversationId::new(99)))
            .await
            .expect("abort"),
        None
    );
}

#[tokio::test]
async fn wait_settles_when_the_submission_does() {
    let (_session, _storage, submissions, _log) = open().await;
    // 一个排队中的提交，稍后由另一次提交结清。
    submissions
        .submit(conversation(), input("first"), context())
        .await
        .expect("first");
    let queued = submissions
        .submit(conversation(), input("second"), context())
        .await
        .expect("queue");

    let handle = Arc::clone(&submissions);
    let id = queued.id();
    let waiter = tokio::spawn(async move { handle.wait(id, context()).await });

    // 结清那条排队提交（模拟运行把它应答掉）。
    tokio::time::sleep(Duration::from_millis(10)).await;
    submissions
        .abort(id, context(), None)
        .await
        .expect("abort settles it");

    let settled = waiter.await.expect("join").expect("settled");
    assert!(matches!(
        settled.record().status(),
        SubmissionStatus::Unanswered
    ));
}

#[tokio::test]
async fn wait_of_an_already_settled_submission_resolves_immediately() {
    let (_session, _storage, submissions, _log) = open().await;
    let submission = submissions
        .submit(conversation(), write_entry(1), context())
        .await
        .expect("submit");
    let settled = submissions
        .wait(submission.id(), context())
        .await
        .expect("wait");
    assert_eq!(settled.record().status(), SubmissionStatus::Done);
}

#[tokio::test]
async fn wait_of_an_unknown_submission_fails() {
    let (_session, _storage, submissions, _log) = open().await;
    let error = submissions
        .wait(SubmissionId::new(999), context())
        .await
        .expect_err("unknown submission");
    assert_eq!(error.to_string(), "Submission 999 does not exist");
}

#[tokio::test]
async fn close_rejects_pending_waiters() {
    let (session, _storage, submissions, _log) = open().await;
    submissions
        .submit(conversation(), input("first"), context())
        .await
        .expect("first");
    let queued = submissions
        .submit(conversation(), input("second"), context())
        .await
        .expect("queue");

    let handle = Arc::clone(&submissions);
    let id = queued.id();
    let waiter = tokio::spawn(async move { handle.wait(id, context()).await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    session.close(context()).await.expect("close");
    let error = waiter.await.expect("join").expect_err("closed");
    assert_eq!(error.to_string(), "Harness is closed");
}

#[tokio::test]
async fn waiters_of_other_keys_are_unaffected() {
    // `Waiters` 的键隔离（与 harness::util 的单元测试互补，这里走提交路径的键）。
    let waiters: Arc<Waiters<SubmissionId, u32>> = Arc::new(Waiters::new());
    let context = context();
    let resolver = {
        let waiters = Arc::clone(&waiters);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waiters.resolve(&SubmissionId::new(1), 5);
        })
    };
    let value = waiters
        .add(SubmissionId::new(1), Arc::clone(&context))
        .await
        .expect("resolved");
    resolver.await.expect("join");
    assert_eq!(value, 5);
    assert!(waiters.keys().is_empty());
}

/// 最小 generation 定义：本测试只关心 `startRun` 建出的任务与 `pi.live.run`，
/// 因此 `generate` 阶段不需要处理器（对应上游 `GenerationTask` 的骨架）。
struct Generation;

impl pi_durable::types::TaskDefinitionSpec for Generation {
    fn name(&self) -> &str {
        "pi.generation"
    }

    fn version(&self) -> u32 {
        1
    }

    fn initial(&self, _input: &JsonValue) -> JsonValue {
        json!({"phase": "generate"})
    }

    fn phases(&self) -> &[&'static str] {
        &["generate"]
    }
}

/// 端到端：`Submissions` 的启动钩子接上真实的 `startRun`（P5g-2 首项）。
/// 被放置的输入应当建出一个由该会话拥有的 generation 任务，并把运行写进 `pi.live`。
#[tokio::test]
async fn a_placed_input_starts_a_real_generation_run() {
    let start_run = pi_durable::harness::generation::make_start_run(Arc::new(move || {
        Arc::new(pi_durable::types::Task::new(Arc::new(Generation)))
    }));
    let (session, _storage, submissions) = open_with(start_run).await;

    let submission = submissions
        .submit(conversation(), input("hello"), context())
        .await
        .expect("submit");
    let id = submission.id();

    let state = session
        .commit(
            |tx| {
                Box::pin(async move {
                    let live = tx
                        .doc(
                            &*pi_durable::harness::live::LIVE_DOC,
                            pi_durable::types::DocAccess {
                                owner: Some(1),
                                key: None,
                            },
                            None,
                        )
                        .await?;
                    Ok(pi_durable::harness::live::live_state(&live))
                })
            },
            context(),
        )
        .await
        .expect("live");

    let run = state.run.expect("run 已写入");
    assert_eq!(run.inputs, vec![id], "运行携带这次提交");

    // 任务已持久化，且由该会话拥有（另起一次提交读取：事务不允许写后读）。
    let task_id = run.task_id.get();
    let record = session
        .commit(
            |tx| Box::pin(async move { tx.task(pi_durable::types::TaskId::new(task_id)).await }),
            context(),
        )
        .await
        .expect("task")
        .expect("任务已持久化");
    assert_eq!(record.kind, "pi.generation");
    assert_eq!(record.conversation_id, conversation());
}
