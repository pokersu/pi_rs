//! `harness::context` 与 `harness::view` 的端到端测试。
//!
//! 覆盖：活动转录的读取与 head 标记的裁剪、模型上下文的派生，以及会话视图挂载随提交出版物推进。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::{Message, UserContent, UserMessage};
use pi_durable::chord::context::Context;
use pi_durable::chord::state::ReplicatedState;
use pi_durable::entries::USER_ENTRY;
use pi_durable::harness::context::{read_context, read_context_from};
use pi_durable::harness::inbox::INBOX_DOC;
use pi_durable::harness::view::{ConversationView, ConversationViews};
use pi_durable::session::{SessionImpl, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::types::{
    ConversationId, DocAccess, EntryDraft, EntryHead, Storage, WatchEnd, WatchHandle, WatchReason,
};
use serde_json::json;

fn context() -> Arc<dyn Context> {
    Arc::new(pi_durable::chord::context::EmptyContext::new("[test]"))
}

fn path(key: &str) -> Vec<pi_durable::chord::delta::PathSegment> {
    vec![pi_durable::chord::delta::PathSegment::Key(key.to_string())]
}

async fn open_session() -> (Arc<SessionImpl>, Arc<MemoryStorage>) {
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
    (session, storage)
}

fn conversation() -> ConversationId {
    ConversationId::new(1)
}

fn user_message(text: &str, timestamp: u64) -> EntryDraft {
    EntryDraft {
        kind: USER_ENTRY.kind().to_string(),
        model: Some(vec![Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp,
        })]),
        data: None,
        head: None,
        edits: None,
    }
}

async fn append_user(session: &Arc<SessionImpl>, text: &str, timestamp: u64) {
    let draft = user_message(text, timestamp);
    session
        .commit(
            move |tx| {
                let draft = draft.clone();
                Box::pin(async move {
                    tx.append_entry(None, conversation(), draft).await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("append user entry");
}

// ─── context ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn read_context_returns_the_active_transcript() {
    let (session, storage) = open_session().await;
    append_user(&session, "first", 1).await;
    append_user(&session, "second", 2).await;

    let view = read_context(&session, storage.as_ref(), conversation(), context(), None)
        .await
        .expect("readContext");

    assert!(view.head.is_none(), "没有 head 标记");
    assert_eq!(view.entries.len(), 2);
    assert_eq!(view.contributions.len(), 2);
    assert_eq!(
        view.messages.len(),
        2,
        "两条用户消息都进入模型上下文：{:?}",
        view.messages
    );
    assert_eq!(view.messages[0].role(), "user");
}

#[tokio::test]
async fn read_context_cuts_at_the_newest_head_marker() {
    let (session, storage) = open_session().await;
    append_user(&session, "old one", 1).await;
    append_user(&session, "old two", 2).await;

    // 一次 reset：head 指向新条目自身，切掉更早的历史。
    session
        .commit(
            |tx| {
                Box::pin(async move {
                    tx.append_entry(
                        None,
                        conversation(),
                        EntryDraft {
                            kind: USER_ENTRY.kind().to_string(),
                            model: Some(vec![Message::User(UserMessage {
                                content: UserContent::Text("after reset".to_string()),
                                timestamp: 3,
                            })]),
                            data: None,
                            head: Some(EntryHead::SelfEntry),
                            edits: None,
                        },
                    )
                    .await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("reset entry");

    let view = read_context(&session, storage.as_ref(), conversation(), context(), None)
        .await
        .expect("readContext");

    let head = view.head.as_ref().expect("head marker");
    assert_eq!(view.entries.len(), 1, "只有 head 标记");
    assert_eq!(view.entries[0].id, head.id, "head 标记就是那一条条目");
    assert_eq!(view.messages.len(), 1);
    assert_eq!(view.head.as_ref().expect("head").head, Some(head.id));
}

#[tokio::test]
async fn read_context_from_reuses_a_previous_range() {
    let (session, storage) = open_session().await;
    append_user(&session, "one", 1).await;

    let (first, range) = read_context_from(
        &session,
        storage.as_ref(),
        conversation(),
        context(),
        None,
        None,
    )
    .await
    .expect("first read");
    assert_eq!(first.entries.len(), 1);
    let range = range.expect("range");

    append_user(&session, "two", 2).await;

    let (second, next) = read_context_from(
        &session,
        storage.as_ref(),
        conversation(),
        context(),
        None,
        Some(range),
    )
    .await
    .expect("second read");
    assert_eq!(second.entries.len(), 2, "复用后仍看到完整区间");
    assert_eq!(second.messages.len(), 2);
    assert!(next.is_some());
}

#[tokio::test]
async fn read_context_from_returns_an_empty_view_without_entries() {
    let (session, storage) = open_session().await;
    let (view, range) = read_context_from(
        &session,
        storage.as_ref(),
        conversation(),
        context(),
        None,
        None,
    )
    .await
    .expect("read");
    assert!(view.entries.is_empty());
    assert!(view.messages.is_empty());
    assert!(range.is_none(), "空转录没有区间");
}

// ─── view ────────────────────────────────────────────────────────────────────

async fn wait_for(predicate: impl Fn() -> bool) {
    for _ in 0..200 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("条件在超时前未满足");
}

fn docs_of(value: &serde_json::Value) -> &serde_json::Map<String, serde_json::Value> {
    value["docs"].as_object().expect("docs")
}

#[tokio::test]
async fn view_state_mounts_the_transcript_and_built_in_documents() {
    let (session, storage) = open_session().await;
    append_user(&session, "hello", 1).await;
    // 写入一个内置文档，让它出现在视图的 docs 里。
    session
        .commit(
            |tx| {
                Box::pin(async move {
                    let inbox = tx
                        .doc(
                            &*INBOX_DOC,
                            DocAccess {
                                owner: Some(conversation().get()),
                                key: None,
                            },
                            None,
                        )
                        .await?;
                    inbox.set(path("items"), json!([])).map_err(|error| {
                        pi_durable::session::SessionError::Message(error.to_string())
                    })?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("write inbox");

    let views = ConversationViews::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );
    let state = views
        .state(conversation(), context())
        .await
        .expect("view state");
    let value = state.value().expect("hydrated value");
    let view: ConversationView = serde_json::from_value((*value).clone()).expect("view");

    assert_eq!(view.conversation.id, conversation());
    assert_eq!(view.entries.len(), 1);
    assert_eq!(view.entries[0].kind, USER_ENTRY.kind());
    assert_eq!(
        docs_of(&value)["pi.inbox"]["items"],
        json!([]),
        "内置文档按 kind 挂载"
    );
}

#[tokio::test]
async fn view_state_advances_with_later_commits() {
    let (session, storage) = open_session().await;
    let views = ConversationViews::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );
    let state = views
        .state(conversation(), context())
        .await
        .expect("view state");

    let latest = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&latest);
    let _subscription = state.subscribe(Arc::new(move |value, _context, _delivery| {
        *sink.lock().expect("latest") = Some((*value).clone());
        Box::pin(async {})
    }));

    append_user(&session, "one", 1).await;
    append_user(&session, "two", 2).await;

    wait_for(|| {
        latest
            .lock()
            .expect("latest")
            .as_ref()
            .and_then(|value| value["entries"].as_array().map(Vec::len))
            == Some(2)
    })
    .await;

    let value = latest.lock().expect("latest").clone().expect("frame");
    assert_eq!(value["entries"].as_array().expect("entries").len(), 2);
}

#[tokio::test]
async fn view_state_shares_one_mount_between_observers() {
    let (session, storage) = open_session().await;
    let views = ConversationViews::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );
    let first = views
        .state(conversation(), context())
        .await
        .expect("first state");
    let second = views
        .state(conversation(), context())
        .await
        .expect("second state");

    // 两个观察者共享同一挂载：一次提交后两者都看到新条目。
    let first_latest = Arc::new(Mutex::new(None));
    let first_sink = Arc::clone(&first_latest);
    let _first_sub = first.subscribe(Arc::new(move |value, _context, _delivery| {
        *first_sink.lock().expect("latest") = Some((*value).clone());
        Box::pin(async {})
    }));
    let second_latest = Arc::new(Mutex::new(None));
    let second_sink = Arc::clone(&second_latest);
    let _second_sub = second.subscribe(Arc::new(move |value, _context, _delivery| {
        *second_sink.lock().expect("latest") = Some((*value).clone());
        Box::pin(async {})
    }));

    append_user(&session, "shared", 1).await;
    wait_for(|| {
        let count = |cell: &Arc<Mutex<Option<serde_json::Value>>>| {
            cell.lock()
                .expect("latest")
                .as_ref()
                .and_then(|value| value["entries"].as_array().map(Vec::len))
        };
        count(&first_latest) == Some(1) && count(&second_latest) == Some(1)
    })
    .await;
}

#[tokio::test]
async fn view_state_rejects_an_unknown_conversation() {
    let (session, storage) = open_session().await;
    let views = ConversationViews::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );
    let result = views.state(ConversationId::new(99), context()).await;
    let error = match result {
        Ok(_) => panic!("未知会话不应建立挂载"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "Conversation 99 does not exist");
}

#[tokio::test]
async fn view_watch_delivers_frames_and_stops_on_cancel() {
    let (session, storage) = open_session().await;
    let views = ConversationViews::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );
    let watch = views
        .watch(conversation(), context())
        .await
        .expect("view watch");

    let frames = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    watch.start(Arc::new(move |value, _ops, _context| {
        let sink = Arc::clone(&sink);
        Box::pin(async move {
            sink.lock().expect("frames").push(value);
        })
    }));

    append_user(&session, "one", 1).await;
    wait_for(|| !frames.lock().expect("frames").is_empty()).await;

    let end = watch.stop().await.expect("stop");
    assert_eq!(end, WatchEnd::Reason(WatchReason::Stopped));
}
