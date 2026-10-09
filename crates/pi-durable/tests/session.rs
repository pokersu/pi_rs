//! `session::session`（对应 `src/session/session.ts`）的集成测试。
//!
//! 覆盖：提交线的串行化与发布、快照与历史快照、`documentState` / `watchDoc` 的已提交帧、
//! 取消与关闭、`unloadDocuments` 的冷加载，以及失败提交不污染会话。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::AbortSignal;
use pi_durable::chord::context::{BACKGROUND_CONTEXT, Context, with_abort_signal};
use pi_durable::chord::delta::{Path, PathSegment};
use pi_durable::chord::state::ReplicatedState;
use pi_durable::documents::define_doc;
use pi_durable::session::{SessionError, SessionImpl, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::types::{
    ConversationFork, ConversationHistory, ConversationId, ConversationOwnership, DocAccess,
    DocDefinitionSpec, DocToken, DocumentSemantics, EntryDraft, EntryId, JsonObject, WatchEnd,
    WatchHandle, WatchReason,
};
use serde_json::{Value as JsonValue, json};

fn context() -> Arc<dyn Context> {
    (*BACKGROUND_CONTEXT).clone()
}

fn path(key: &str) -> Path {
    vec![PathSegment::Key(key.to_string())]
}

fn doc_error(error: pi_durable::chord::delta::DeltaError) -> SessionError {
    SessionError::Message(error.to_string())
}

fn object(value: JsonValue) -> JsonObject {
    value.as_object().expect("object").clone()
}

/// 会话级计数文档（对应一个普通 `defineDoc` 单例）。
struct Counter;

impl DocDefinitionSpec for Counter {
    fn kind(&self) -> &str {
        "test.counter"
    }

    fn version(&self) -> u32 {
        1
    }

    fn semantics(&self) -> DocumentSemantics {
        DocumentSemantics::Session
    }

    fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
        JsonObject::new()
    }
}

/// 会话文档（可回溯），用于 `snapshotAsOf`。
struct Note;

impl DocDefinitionSpec for Note {
    fn kind(&self) -> &str {
        "test.note"
    }

    fn version(&self) -> u32 {
        1
    }

    fn semantics(&self) -> DocumentSemantics {
        DocumentSemantics::Conversation {
            history: ConversationHistory::Rewindable,
            fork: ConversationFork::Current,
        }
    }

    fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
        JsonObject::new()
    }
}

fn counter_token() -> DocToken {
    define_doc(Arc::new(Counter)).expect("define counter")
}

fn note_token() -> DocToken {
    define_doc(Arc::new(Note)).expect("define note")
}

/// 开一个带根会话的 Session（会话文档的地址以会话为 owner）。
async fn open_session() -> (Arc<SessionImpl>, DocToken) {
    let session = create_session(Arc::new(MemoryStorage::new()), None);
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
    (session, counter_token())
}

async fn write_counter(session: &Arc<SessionImpl>, token: &DocToken, value: u64) {
    let token = token.clone();
    session
        .commit(
            move |tx| {
                let token = token.clone();
                Box::pin(async move {
                    let draft = tx.doc(&token, DocAccess::default(), None).await?;
                    draft.set(path("count"), json!(value)).map_err(doc_error)?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("write counter");
}

/// 读-改-写一次计数器，用于验证提交线的串行化。
async fn bump(session: &Arc<SessionImpl>, token: &DocToken) -> Result<(), SessionError> {
    let token = DocToken::clone(token);
    session
        .commit(
            move |tx| {
                let token = DocToken::clone(&token);
                Box::pin(async move {
                    let draft = tx.doc(&token, DocAccess::default(), None).await?;
                    let current = draft
                        .value()
                        .get("count")
                        .and_then(JsonValue::as_u64)
                        .unwrap_or(0);
                    draft
                        .set(path("count"), json!(current + 1))
                        .map_err(doc_error)?;
                    Ok(())
                })
            },
            context(),
        )
        .await
}

async fn wait_for(predicate: impl Fn() -> bool) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("条件在超时前未满足");
}

#[tokio::test]
async fn commit_publishes_table_changes_to_listeners() {
    let (session, _token) = open_session().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let _subscription = session
        .subscribe_commits(Arc::new(move |publication, _context| {
            sink.lock().expect("sink").push(publication.clone());
        }))
        .expect("subscribe");

    session
        .commit(
            |tx| {
                Box::pin(async move {
                    tx.create_conversation(ConversationOwnership::Ownerless)
                        .await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("commit");

    let seen = seen.lock().expect("sink");
    assert_eq!(seen.len(), 1);
    assert!(seen[0].seq.get() > 0);
    assert!(!seen[0].changes.is_empty(), "会话记录必须出现在发布里");
}

#[tokio::test]
async fn unsubscribe_stops_delivery() {
    let (session, _token) = open_session().await;
    let count = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&count);
    let subscription = session
        .subscribe_commits(Arc::new(move |_publication, _context| {
            counter.fetch_add(1, Ordering::Relaxed);
        }))
        .expect("subscribe");
    subscription.unsubscribe();

    session
        .commit(
            |tx| {
                Box::pin(async move {
                    tx.create_conversation(ConversationOwnership::Ownerless)
                        .await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("commit");
    assert_eq!(count.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn mutation_line_serialises_concurrent_commits() {
    let (session, token) = open_session().await;
    write_counter(&session, &token, 0).await;

    let (first, second) = tokio::join!(bump(&session, &token), bump(&session, &token));
    first.expect("first commit");
    second.expect("second commit");

    let value = session
        .snapshot(&token, None, None, context())
        .await
        .expect("snapshot")
        .expect("present");
    assert_eq!(
        value.get("count").and_then(JsonValue::as_u64),
        Some(2),
        "提交线必须串行化：第二次提交要看到第一次的结果"
    );
}

#[tokio::test]
async fn snapshot_reads_a_committed_document() {
    let (session, token) = open_session().await;
    write_counter(&session, &token, 7).await;
    let value = session
        .snapshot(&token, None, None, context())
        .await
        .expect("snapshot");
    assert_eq!(value, Some(object(json!({"count": 7}))));
}

#[tokio::test]
async fn unload_documents_cold_loads_from_storage() {
    let (session, token) = open_session().await;
    write_counter(&session, &token, 3).await;
    session.unload_documents().await.expect("unload");
    let value = session
        .snapshot(&token, None, None, context())
        .await
        .expect("snapshot");
    assert_eq!(value, Some(object(json!({"count": 3}))));
}

#[tokio::test]
async fn document_state_receives_committed_frames() {
    let (session, token) = open_session().await;
    write_counter(&session, &token, 1).await;

    let state = session
        .document_state(&token, None, None, context())
        .await
        .expect("documentState")
        .expect("present");
    let latest = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&latest);
    let _subscription = state.subscribe(Arc::new(move |value, _context, _delivery| {
        *sink.lock().expect("latest") = Some((*value).clone());
        Box::pin(async {})
    }));

    write_counter(&session, &token, 2).await;
    wait_for(|| {
        latest
            .lock()
            .expect("latest")
            .as_ref()
            .and_then(|value| value.get("count"))
            .and_then(JsonValue::as_u64)
            == Some(2)
    })
    .await;
}

#[tokio::test]
async fn watch_doc_delivers_frames_then_stops() {
    let (session, token) = open_session().await;
    write_counter(&session, &token, 1).await;

    let watch = session
        .watch_doc(&token, None, None, context())
        .await
        .expect("watchDoc")
        .expect("present");
    assert_eq!(watch.value(), json!({"count": 1}));

    let frames = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    watch.start(Arc::new(move |value, _ops, _context| {
        let sink = Arc::clone(&sink);
        Box::pin(async move {
            sink.lock().expect("frames").push(value);
        })
    }));

    write_counter(&session, &token, 2).await;
    wait_for(|| !frames.lock().expect("frames").is_empty()).await;
    assert_eq!(frames.lock().expect("frames")[0], json!({"count": 2}));

    let end = watch.stop().await.expect("stop");
    assert_eq!(end, WatchEnd::Reason(WatchReason::Stopped));
}

#[tokio::test]
async fn watch_doc_observes_caller_cancellation() {
    let (session, token) = open_session().await;
    write_counter(&session, &token, 1).await;

    let signal = AbortSignal::new();
    let watch = session
        .watch_doc(
            &token,
            None,
            None,
            with_abort_signal(signal.clone(), context()),
        )
        .await
        .expect("watchDoc")
        .expect("present");
    signal.abort();
    let end = watch.closed().await.expect("closed");
    assert_eq!(end, WatchEnd::Reason(WatchReason::Cancelled));
}

/// 写一个会话文档的新修订，并追加一个条目作为该修订的标记。
async fn write_note(
    session: &Arc<SessionImpl>,
    token: &DocToken,
    value: &'static str,
) -> Result<EntryId, SessionError> {
    let token = token.clone();
    session
        .commit(
            move |tx| {
                let token = token.clone();
                Box::pin(async move {
                    let draft = tx
                        .doc(
                            &token,
                            DocAccess {
                                owner: Some(1),
                                key: None,
                            },
                            None,
                        )
                        .await?;
                    draft.set(path("title"), json!(value)).map_err(doc_error)?;
                    let entry = tx
                        .append_entry(
                            None,
                            ConversationId::new(1),
                            EntryDraft {
                                kind: "test.entry".to_string(),
                                model: None,
                                data: Some(json!({"value": value})),
                                head: None,
                                edits: None,
                            },
                        )
                        .await?;
                    Ok(entry.id)
                })
            },
            context(),
        )
        .await
}

#[tokio::test]
async fn snapshot_as_of_reads_a_historical_revision() {
    let session = create_session(Arc::new(MemoryStorage::new()), None);
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

    let token = note_token();
    let marker = write_note(&session, &token, "v1")
        .await
        .expect("first revision");
    write_note(&session, &token, "v2")
        .await
        .expect("second revision");

    let current = session
        .snapshot(&token, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("present");
    assert_eq!(current.get("title"), Some(&json!("v2")));

    let historical = session
        .snapshot_as_of(&token, 1, None, marker, context())
        .await
        .expect("snapshotAsOf")
        .expect("present");
    assert_eq!(historical.get("title"), Some(&json!("v1")));
}

#[tokio::test]
async fn snapshot_as_of_rejects_non_conversation_documents() {
    let (session, token) = open_session().await;
    let error = session
        .snapshot_as_of(&token, 1, None, EntryId::new(1), context())
        .await
        .expect_err("session documents have no history");
    assert_eq!(
        error.to_string(),
        "Session.snapshotAsOf() requires a conversation document"
    );
}

#[tokio::test]
async fn failed_commit_leaves_the_session_usable() {
    let (session, token) = open_session().await;
    let error = session
        .commit(
            |_tx| Box::pin(async move { Err::<(), _>(SessionError::Message("boom".to_string())) }),
            context(),
        )
        .await
        .expect_err("callback failure");
    assert_eq!(error.to_string(), "boom");
    assert!(session.poison_error().is_none(), "回调失败不应污染会话");

    write_counter(&session, &token, 4).await;
    let value = session
        .snapshot(&token, None, None, context())
        .await
        .expect("snapshot");
    assert_eq!(value, Some(object(json!({"count": 4}))));
}

#[tokio::test]
async fn read_on_line_runs_on_the_mutation_line() {
    let (session, _token) = open_session().await;
    let value: u32 = session
        .read_on_line(|| async { Ok(42u32) })
        .await
        .expect("readOnLine");
    assert_eq!(value, 42);
}

#[tokio::test]
async fn close_seals_admission_and_notifies_listeners() {
    let (session, token) = open_session().await;
    let closes = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&closes);
    let _subscription = session
        .subscribe_close(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }))
        .expect("subscribeClose");

    session.close(context()).await.expect("close");
    assert_eq!(closes.load(Ordering::Relaxed), 1);

    let error = session
        .snapshot(&token, None, None, context())
        .await
        .expect_err("closed session");
    assert_eq!(error.to_string(), "Session is closed");

    let error = session
        .commit(
            |_tx| Box::pin(async move { Ok::<(), SessionError>(()) }),
            context(),
        )
        .await
        .expect_err("closed session");
    assert_eq!(error.to_string(), "Session is closed");

    // 幂等：重复关闭仍然成功。
    session.close(context()).await.expect("second close");
}

#[tokio::test]
async fn close_uses_the_first_callers_cleanup_context() {
    let (session, _token) = open_session().await;
    // 调用方取消只中断“等待”，不得阻止存储关闭（close 会剥掉取消能力）。
    let signal = AbortSignal::new();
    signal.abort();
    let result = session.close(with_abort_signal(signal, context())).await;
    assert!(
        matches!(result, Err(SessionError::Aborted(_))),
        "已取消的调用方中断等待：{result:?}"
    );

    session
        .close(context())
        .await
        .expect("close completes despite the first caller's cancellation");
    assert!(session.is_closing());
}
