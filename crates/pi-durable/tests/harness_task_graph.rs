//! `harness::task_graph` 的端到端测试：真实提交 → 挂载构建与帧推进。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_durable::chord::context::Context;
use pi_durable::chord::state::ReplicatedState;
use pi_durable::harness::task_graph::TaskGraphView;
use pi_durable::session::{SessionImpl, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::types::{
    ConversationId, Storage, Task, TaskDefinitionSpec, TaskOptions, TaskOwnership, WatchEnd,
    WatchHandle, WatchReason,
};
use serde_json::{Value as JsonValue, json};

fn context() -> Arc<dyn Context> {
    Arc::new(pi_durable::chord::context::EmptyContext::new("[test]"))
}

struct DemoTask;

impl TaskDefinitionSpec for DemoTask {
    fn name(&self) -> &str {
        "demo.task"
    }

    fn version(&self) -> u32 {
        1
    }

    fn initial(&self, _input: &JsonValue) -> JsonValue {
        json!({"phase": "run"})
    }

    fn phases(&self) -> &[&'static str] {
        &["run"]
    }
}

fn task() -> Task {
    Task::new(Arc::new(DemoTask))
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

async fn create_task(session: &Arc<SessionImpl>) -> pi_durable::types::TaskId {
    let task = task();
    session
        .commit(
            move |tx| {
                Box::pin(async move {
                    tx.create_task(
                        &task,
                        json!({"prompt": "hi"}),
                        TaskOptions {
                            ownership: TaskOwnership::Conversation,
                            conversation_id: Some(ConversationId::new(1)),
                            background: None,
                        },
                    )
                    .await
                })
            },
            context(),
        )
        .await
        .expect("create task")
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

fn tasks_of(value: &JsonValue) -> &serde_json::Map<String, JsonValue> {
    value["tasks"].as_object().expect("tasks")
}

#[tokio::test]
async fn state_mounts_an_empty_graph() {
    let (session, storage) = open_session().await;
    let view = TaskGraphView::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );
    let state = view.state(context()).await.expect("state");
    let value = state.value().expect("hydrated");
    assert_eq!(tasks_of(&value).len(), 0, "没有活动任务");
}

#[tokio::test]
async fn state_reflects_created_tasks() {
    let (session, storage) = open_session().await;
    let view = TaskGraphView::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );
    let state = view.state(context()).await.expect("state");

    let latest = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&latest);
    let _subscription = state.subscribe(Arc::new(move |value, _context, _delivery| {
        *sink.lock().expect("latest") = Some((*value).clone());
        Box::pin(async {})
    }));

    let id = create_task(&session).await;
    let key = id.get().to_string();
    wait_for({
        let latest = Arc::clone(&latest);
        let key = key.clone();
        move || {
            latest
                .lock()
                .expect("latest")
                .as_ref()
                .is_some_and(|value| tasks_of(value).contains_key(&key))
        }
    })
    .await;

    let value = latest.lock().expect("latest").clone().expect("frame");
    let node = &value["tasks"][&key];
    assert_eq!(node["kind"], json!("demo.task"));
    assert_eq!(node["conversationId"], json!(1));
    assert_eq!(node["state"]["status"], json!("pending"));
    assert_eq!(
        node["state"]["phase"],
        json!("run"),
        "阶段来自任务的 initial()"
    );
    assert_eq!(node["conversations"], json!([]));
    assert_eq!(node["background"], json!(false));
}

#[tokio::test]
async fn state_carries_the_owned_conversations_of_a_task() {
    let (session, storage) = open_session().await;
    let view = TaskGraphView::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );

    let id = create_task(&session).await;
    // 创建一个由该任务拥有的会话。
    session
        .commit(
            move |tx| {
                Box::pin(async move {
                    tx.create_conversation(pi_durable::types::ConversationOwnership::Task {
                        task_id: id,
                    })
                    .await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("owned conversation");

    let state = view.state(context()).await.expect("state");
    let value = state.value().expect("hydrated");
    let node = &value["tasks"][&id.get().to_string()];
    let conversations = node["conversations"].as_array().expect("conversations");
    assert_eq!(conversations.len(), 1, "任务拥有它的会话");
    assert_eq!(node["owner"], JsonValue::Null, "会话拥有的任务没有 owner");
}

#[tokio::test]
async fn state_advances_when_a_task_becomes_terminal() {
    let (session, storage) = open_session().await;
    let view = TaskGraphView::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );

    let id = create_task(&session).await;
    let state = view.state(context()).await.expect("state");
    let before = state.value().expect("hydrated");
    assert!(
        tasks_of(&before).contains_key(&id.get().to_string()),
        "任务在图中"
    );

    let latest = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&latest);
    let _subscription = state.subscribe(Arc::new(move |value, _context, _delivery| {
        *sink.lock().expect("latest") = Some((*value).clone());
        Box::pin(async {})
    }));

    // 把一个已存在的任务替换为终态记录：它应当离开图。
    let mut record = session
        .read_on_line({
            let storage = Arc::clone(&storage);
            move || {
                let storage = Arc::clone(&storage);
                async move {
                    storage
                        .task(id, context().as_ref())
                        .await
                        .map_err(pi_durable::session::SessionError::from)
                }
            }
        })
        .await
        .expect("read task")
        .expect("task exists");
    record.state = pi_durable::types::TaskState::Terminal {
        outcome: pi_durable::types::TaskOutcome::Completed {
            result: JsonValue::Null,
        },
    };
    session
        .commit(
            move |tx| {
                let record = record.clone();
                Box::pin(async move {
                    tx.set_task(record);
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("set terminal task");

    wait_for({
        let latest = Arc::clone(&latest);
        let key = id.get().to_string();
        move || {
            latest
                .lock()
                .expect("latest")
                .as_ref()
                .is_some_and(|value| !tasks_of(value).contains_key(&key))
        }
    })
    .await;
}

#[tokio::test]
async fn watch_delivers_frames_and_stops() {
    let (session, storage) = open_session().await;
    let view = TaskGraphView::new(
        Arc::clone(&session),
        Arc::clone(&storage) as Arc<dyn Storage>,
    );
    let watch = view.watch(context()).await.expect("watch");

    let frames = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    watch.start(Arc::new(move |value, _ops, _context| {
        let sink = Arc::clone(&sink);
        Box::pin(async move {
            sink.lock().expect("frames").push(value);
        })
    }));

    create_task(&session).await;
    wait_for(|| !frames.lock().expect("frames").is_empty()).await;

    let end = watch.stop().await.expect("stop");
    assert_eq!(end, WatchEnd::Reason(WatchReason::Stopped));
}
