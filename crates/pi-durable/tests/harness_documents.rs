//! `harness` 内置文档（`agent` / `usage` / `inbox` / `live`）的端到端测试。
//!
//! P5c 的这些函数是「事务内改写文档」的行为，上游模块依赖 `@earendil-works/chord` 的 Proxy 草稿、
//! 无法在无依赖的 Node 环境里加载做对照，因此这里通过真实的 `Session` + `Transaction` 提交来验证
//! 语义：写入的形状、累加结果、条目放置与提交结算。

use std::sync::Arc;

use pi_ai::{Usage, UsageCost};
use pi_durable::chord::context::Context;
use pi_durable::chord::delta::PathSegment;
use pi_durable::harness::agent::{
    AGENT_DOC, add_tools, configure, resolve_agent, resolve_settings,
};
use pi_durable::harness::inbox::{
    BoundaryAt, INBOX_DOC, InboxInputMode, InboxItem, QueueModes, apply_boundary, prepare_boundary,
    withdraw_queued_inputs,
};
use pi_durable::harness::live::{
    LIVE_DOC, LiveRun, ToolSlot, ToolSlotStatus, end_run, live_state, tool_slot_index,
};
use pi_durable::harness::types::{AgentChange, FieldChange, ModelRef, Settings};
use pi_durable::harness::usage::{USAGE_DOC, UsageBucket, record_usage};
use pi_durable::session::SessionImpl;
use pi_durable::session::create_session;
use pi_durable::storage::MemoryStorage;
use pi_durable::types::{
    ConversationId, DocAccess, InputSubmissionStatus, JsonObject, Storage, SubmissionCreate,
    SubmissionId, SubmissionSettlement, SubmissionStatus, TaskId,
};
use serde_json::{Value as JsonValue, json};

fn context() -> Arc<dyn Context> {
    Arc::new(pi_durable::chord::context::EmptyContext::new("[test]"))
}

fn path(key: &str) -> Vec<PathSegment> {
    vec![PathSegment::Key(key.to_string())]
}

/// 开一个带根会话（ID = 1）的 Session，并返回存储以便直接查提交记录。
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

fn access() -> DocAccess {
    DocAccess {
        owner: Some(conversation().get()),
        key: None,
    }
}

// ─── agent ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn configure_writes_and_clears_agent_state() {
    let (session, _storage) = open_session().await;

    session
        .commit(
            |tx| {
                Box::pin(async move {
                    configure(
                        tx,
                        conversation(),
                        &AgentChange {
                            model: FieldChange::Set(ModelRef {
                                provider: "deepseek".to_string(),
                                model_id: "deepseek-chat".to_string(),
                            }),
                            thinking_level: FieldChange::Set(pi_ai::ModelThinkingLevel::High),
                            instructions: FieldChange::Set("be terse".to_string()),
                            cwd: FieldChange::Set("/work".to_string()),
                            ..AgentChange::default()
                        },
                    )
                    .await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("configure");

    let state = session
        .snapshot(&*AGENT_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.agent");
    assert_eq!(state["model"]["provider"], json!("deepseek"));
    assert_eq!(state["model"]["modelId"], json!("deepseek-chat"));
    assert_eq!(state["thinkingLevel"], json!("high"));
    assert_eq!(state["instructions"], json!("be terse"));
    assert_eq!(state["cwd"], json!("/work"));

    // `null`（Clear）清除字段，`Unchanged` 什么也不做。
    session
        .commit(
            |tx| {
                Box::pin(async move {
                    configure(
                        tx,
                        conversation(),
                        &AgentChange {
                            instructions: FieldChange::Clear,
                            cwd: FieldChange::Unchanged,
                            ..AgentChange::default()
                        },
                    )
                    .await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("configure clear");

    let state = session
        .snapshot(&*AGENT_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.agent");
    assert!(state.get("instructions").is_none(), "instructions 应被清除");
    assert_eq!(state["cwd"], json!("/work"), "未提供的字段不变");
}

#[tokio::test]
async fn add_tools_extends_an_array_and_edits_a_remove_object() {
    let (session, _storage) = open_session().await;

    // 数组形态：追加缺失的名字。
    session
        .commit(
            |tx| {
                Box::pin(async move {
                    configure(
                        tx,
                        conversation(),
                        &AgentChange {
                            tools: FieldChange::Set(
                                pi_durable::harness::types::ToolChangeSelection::Exactly(vec![
                                    Arc::new(NamedTool("read")),
                                ]),
                            ),
                            ..AgentChange::default()
                        },
                    )
                    .await?;
                    add_tools(
                        tx,
                        conversation(),
                        &["read".to_string(), "bash".to_string()],
                    )
                    .await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("configure + addTools");

    let state = session
        .snapshot(&*AGENT_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.agent");
    assert_eq!(
        state["tools"],
        json!(["read", "bash"]),
        "已存在的 read 不重复追加，新的 bash 追加到末尾"
    );

    // 对象形态：只要被移除的名字与新增的名字有交集才写入。
    session
        .commit(
            |tx| {
                Box::pin(async move {
                    let draft = tx.doc(&*AGENT_DOC, access(), None).await?;
                    draft
                        .set(path("tools"), json!({"remove": ["read", "bash"]}))
                        .map_err(|error| {
                            pi_durable::session::SessionError::Message(error.to_string())
                        })?;
                    add_tools(tx, conversation(), &["bash".to_string()]).await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("remove object");

    let state = session
        .snapshot(&*AGENT_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.agent");
    assert_eq!(state["tools"], json!({"remove": ["read"]}));
}

#[tokio::test]
async fn add_tools_writes_nothing_without_a_stored_filter() {
    let (session, _storage) = open_session().await;
    session
        .commit(
            |tx| {
                Box::pin(async move {
                    add_tools(tx, conversation(), &["read".to_string()]).await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("addTools without a filter");

    let state = session
        .snapshot(&*AGENT_DOC, Some(1), None, context())
        .await
        .expect("snapshot");
    // `tx.doc` 会创建文档（`initial()` 为空对象），但不得写入 `tools`。
    let state = state.unwrap_or_default();
    assert!(
        state.get("tools").is_none(),
        "未设置 tools 的会话不写 tools（已提供全部工具）"
    );
}

#[tokio::test]
async fn resolve_agent_reads_the_stored_state_back() {
    let (session, _storage) = open_session().await;
    session
        .commit(
            |tx| {
                Box::pin(async move {
                    configure(
                        tx,
                        conversation(),
                        &AgentChange {
                            instructions: FieldChange::Set("stored".to_string()),
                            tools: FieldChange::Set(
                                pi_durable::harness::types::ToolChangeSelection::Exactly(vec![
                                    Arc::new(NamedTool("read")),
                                ]),
                            ),
                            ..AgentChange::default()
                        },
                    )
                    .await?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("configure");

    let stored = session
        .snapshot(&*AGENT_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.agent");
    let state: pi_durable::harness::types::AgentState =
        serde_json::from_value(JsonValue::Object(stored)).expect("agent state");

    let snapshot = pi_durable::harness::types::RegistrySnapshot::default();
    let agent = resolve_agent(Some(&state), &snapshot, &Settings::default(), &|_| {});
    assert_eq!(agent.instructions.as_deref(), Some("stored"));
    assert_eq!(agent.thinking_level, pi_ai::ModelThinkingLevel::Off);
    assert_eq!(agent.sections.len(), 1, "instructions 渲染为章节");
    assert!(agent.tools.is_empty(), "注册表里没有 read 工具");
}

// ─── usage ───────────────────────────────────────────────────────────────────

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output,
        cost: UsageCost {
            input: 0.5,
            output: 1.5,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 2.0,
        },
    }
}

#[tokio::test]
async fn record_usage_accumulates_models_and_tools_buckets() {
    let (session, _storage) = open_session().await;

    for (bucket, key, amount) in [
        (UsageBucket::Models, "deepseek/deepseek-chat", 1_u64),
        (UsageBucket::Models, "deepseek/deepseek-chat", 2_u64),
        (UsageBucket::Tools, "bash", 4_u64),
    ] {
        session
            .commit(
                move |tx| {
                    Box::pin(async move {
                        record_usage(tx, conversation(), bucket, key, &usage(amount, amount))
                            .await?;
                        Ok(())
                    })
                },
                context(),
            )
            .await
            .expect("recordUsage");
    }

    let state = session
        .snapshot(&*USAGE_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.usage");
    assert_eq!(state["models"]["deepseek/deepseek-chat"]["input"], json!(3));
    assert_eq!(
        state["models"]["deepseek/deepseek-chat"]["output"],
        json!(3)
    );
    assert_eq!(
        state["models"]["deepseek/deepseek-chat"]["totalTokens"],
        json!(6)
    );
    assert_eq!(state["tools"]["bash"]["input"], json!(4));
    assert_eq!(state["tools"]["bash"]["cost"]["total"], json!(2.0));
    assert!(
        state["models"]["deepseek/deepseek-chat"]
            .get("reasoning")
            .is_none(),
        "未报告的可选计数不写入"
    );
}

// ─── inbox ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn apply_boundary_places_writes_and_selected_user_inputs() {
    let (session, storage) = open_session().await;
    let (write_id, steer_id, follow_id) = session
        .commit(
            |tx| {
                Box::pin(async move {
                    let mut entry = JsonObject::new();
                    entry.insert("kind".to_string(), json!("pi.custom"));
                    entry.insert("value".to_string(), json!(7));

                    // 先造记录，再按记录 ID 写队列（ID 由存储铸造）。
                    let write_record = tx
                        .create_submission(SubmissionCreate::Write {
                            conversation_id: conversation(),
                            request_id: None,
                            status: pi_durable::types::WriteSubmissionStatus::Queued,
                        })
                        .await?;
                    let steer_record = tx
                        .create_submission(SubmissionCreate::Input {
                            conversation_id: conversation(),
                            request_id: None,
                            status: InputSubmissionStatus::Queued,
                        })
                        .await?;
                    let follow_record = tx
                        .create_submission(SubmissionCreate::Input {
                            conversation_id: conversation(),
                            request_id: None,
                            status: InputSubmissionStatus::Queued,
                        })
                        .await?;
                    let write_id = write_record.identity().id;
                    let steer_id = steer_record.identity().id;
                    let follow_id = follow_record.identity().id;

                    let items = vec![
                        InboxItem::Write {
                            id: write_id,
                            entry,
                        }
                        .to_json(),
                        InboxItem::Input {
                            id: steer_id,
                            mode: InboxInputMode::Steer,
                            content: json!("steer one"),
                        }
                        .to_json(),
                        InboxItem::Input {
                            id: follow_id,
                            mode: InboxInputMode::FollowUp,
                            content: json!("follow up"),
                        }
                        .to_json(),
                    ];
                    let inbox = tx.doc(&*INBOX_DOC, access(), None).await?;
                    inbox
                        .set(path("items"), JsonValue::Array(items))
                        .map_err(|error| {
                            pi_durable::session::SessionError::Message(error.to_string())
                        })?;
                    Ok((write_id, steer_id, follow_id))
                })
            },
            context(),
        )
        .await
        .expect("queue");

    // 边界在自己的提交里准备：表读必须早于该提交的首次表写。
    let boundary_result = session
        .commit(
            |tx| {
                Box::pin(async move {
                    let modes = QueueModes {
                        steering_mode: pi_durable::harness::types::QueueMode::OneAtATime,
                        follow_up_mode: pi_durable::harness::types::QueueMode::OneAtATime,
                    };
                    let mut boundary = prepare_boundary(tx, conversation(), modes).await?;
                    apply_boundary(tx, &mut boundary, BoundaryAt::Final, 1_700).await
                })
            },
            context(),
        )
        .await
        .expect("applyBoundary");

    assert_eq!(boundary_result.users, vec![steer_id, follow_id]);
    assert!(!boundary_result.reset);

    // 队列清空。
    let inbox = session
        .snapshot(&*INBOX_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.inbox");
    assert_eq!(inbox["items"], json!([]));

    // 写入与用户条目都已放置（放置把 queued 的 write 变成 `done`、queued 的 input 变成 `placed`）。
    for (id, expected) in [
        (write_id, SubmissionStatus::Done),
        (steer_id, SubmissionStatus::Placed),
        (follow_id, SubmissionStatus::Placed),
    ] {
        let record = storage
            .submission(id, context().as_ref())
            .await
            .expect("submission lookup")
            .expect("submission");
        assert_eq!(record.status(), expected, "提交 {id:?} 应已放置");
    }
}

#[tokio::test]
async fn apply_boundary_places_only_one_input_per_mode_with_one_at_a_time() {
    let (session, _storage) = open_session().await;
    let (first, second) = session
        .commit(
            |tx| {
                Box::pin(async move {
                    let a = tx
                        .create_submission(SubmissionCreate::Input {
                            conversation_id: conversation(),
                            request_id: None,
                            status: InputSubmissionStatus::Queued,
                        })
                        .await?;
                    let b = tx
                        .create_submission(SubmissionCreate::Input {
                            conversation_id: conversation(),
                            request_id: None,
                            status: InputSubmissionStatus::Queued,
                        })
                        .await?;
                    let first = a.identity().id;
                    let second = b.identity().id;
                    let items = vec![
                        InboxItem::Input {
                            id: first,
                            mode: InboxInputMode::Steer,
                            content: json!("one"),
                        }
                        .to_json(),
                        InboxItem::Input {
                            id: second,
                            mode: InboxInputMode::Steer,
                            content: json!("two"),
                        }
                        .to_json(),
                    ];
                    let inbox = tx.doc(&*INBOX_DOC, access(), None).await?;
                    inbox
                        .set(path("items"), JsonValue::Array(items))
                        .map_err(|error| {
                            pi_durable::session::SessionError::Message(error.to_string())
                        })?;
                    Ok((first, second))
                })
            },
            context(),
        )
        .await
        .expect("queue");

    let result = session
        .commit(
            |tx| {
                Box::pin(async move {
                    let modes = QueueModes {
                        steering_mode: pi_durable::harness::types::QueueMode::OneAtATime,
                        follow_up_mode: pi_durable::harness::types::QueueMode::OneAtATime,
                    };
                    let mut boundary = prepare_boundary(tx, conversation(), modes).await?;
                    // `postTools` 边界不放置 follow-up；steer 只放第一个。
                    apply_boundary(tx, &mut boundary, BoundaryAt::PostTools, 1).await
                })
            },
            context(),
        )
        .await
        .expect("applyBoundary");

    assert_eq!(result.users, vec![first], "one-at-a-time 只放第一个 steer");
    let inbox = session
        .snapshot(&*INBOX_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.inbox");
    assert_eq!(
        inbox["items"].as_array().expect("items").len(),
        1,
        "未选中的条目留在队列里"
    );
    assert_eq!(inbox["items"][0]["id"], json!(second.get()));
}

#[tokio::test]
async fn withdraw_queued_inputs_settles_only_inputs() {
    let (session, storage) = open_session().await;
    let (write_id, steer_id) = session
        .commit(
            |tx| {
                Box::pin(async move {
                    let write = tx
                        .create_submission(SubmissionCreate::Write {
                            conversation_id: conversation(),
                            request_id: None,
                            status: pi_durable::types::WriteSubmissionStatus::Queued,
                        })
                        .await?;
                    let steer = tx
                        .create_submission(SubmissionCreate::Input {
                            conversation_id: conversation(),
                            request_id: None,
                            status: InputSubmissionStatus::Queued,
                        })
                        .await?;
                    let write_id = write.identity().id;
                    let steer_id = steer.identity().id;
                    let mut entry = JsonObject::new();
                    entry.insert("kind".to_string(), json!("pi.custom"));
                    let items = vec![
                        InboxItem::Write {
                            id: write_id,
                            entry,
                        }
                        .to_json(),
                        InboxItem::Input {
                            id: steer_id,
                            mode: InboxInputMode::Steer,
                            content: json!("queued"),
                        }
                        .to_json(),
                    ];
                    let inbox = tx.doc(&*INBOX_DOC, access(), None).await?;
                    inbox
                        .set(path("items"), JsonValue::Array(items))
                        .map_err(|error| {
                            pi_durable::session::SessionError::Message(error.to_string())
                        })?;
                    withdraw_queued_inputs(tx, conversation()).await?;
                    Ok((write_id, steer_id))
                })
            },
            context(),
        )
        .await
        .expect("withdraw");

    let inbox = session
        .snapshot(&*INBOX_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.inbox");
    assert_eq!(inbox["items"].as_array().expect("items").len(), 1);
    assert_eq!(inbox["items"][0]["id"], json!(write_id.get()), "写保留");

    let steer = storage
        .submission(steer_id, context().as_ref())
        .await
        .expect("lookup")
        .expect("submission");
    assert_eq!(steer.status(), SubmissionStatus::Unanswered);
    let write = storage
        .submission(write_id, context().as_ref())
        .await
        .expect("lookup")
        .expect("submission");
    assert_eq!(write.status(), SubmissionStatus::Queued, "写不被结算");
}

// ─── live ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn end_run_settles_inputs_and_drops_presentation() {
    let (session, storage) = open_session().await;

    let input = session
        .commit(
            |tx| {
                Box::pin(async move {
                    let record = tx
                        .create_submission(SubmissionCreate::Input {
                            conversation_id: conversation(),
                            request_id: None,
                            status: InputSubmissionStatus::Queued,
                        })
                        .await?;
                    let input = record.identity().id;

                    let live = tx.doc(&*LIVE_DOC, access(), None).await?;
                    let run = LiveRun {
                        task_id: TaskId::new(5),
                        inputs: vec![input],
                    };
                    live.set(path("run"), serde_json::to_value(&run).expect("run"))
                        .map_err(|error| {
                            pi_durable::session::SessionError::Message(error.to_string())
                        })?;
                    live.set(
                        path("generation"),
                        serde_json::to_value(pi_durable::harness::live::LiveGeneration {
                            attempt: 1,
                            message: None,
                            retry: None,
                            deferred: None,
                        })
                        .expect("generation"),
                    )
                    .map_err(|error| {
                        pi_durable::session::SessionError::Message(error.to_string())
                    })?;
                    live.set(
                        path("tools"),
                        serde_json::to_value(vec![ToolSlot {
                            call_id: "call-1".to_string(),
                            name: "bash".to_string(),
                            task_id: Some(TaskId::new(6)),
                            status: ToolSlotStatus::Running,
                            output: Some("partial".to_string()),
                            dropped_bytes: Some(0),
                            dropped_lines: Some(0),
                            details: None,
                            diagnostics: None,
                            entry: None,
                        }])
                        .expect("tools"),
                    )
                    .map_err(|error| {
                        pi_durable::session::SessionError::Message(error.to_string())
                    })?;

                    end_run(
                        tx,
                        &live,
                        TaskId::new(5),
                        SubmissionSettlement::Unanswered {
                            reason: "aborted".to_string(),
                            detail: None,
                        },
                    )?;
                    Ok(input)
                })
            },
            context(),
        )
        .await
        .expect("endRun");

    let live = session
        .snapshot(&*LIVE_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.live");
    assert!(live.get("run").is_none());
    assert!(live.get("generation").is_none());
    assert!(live.get("tools").is_none());

    let record = storage
        .submission(input, context().as_ref())
        .await
        .expect("lookup")
        .expect("submission");
    assert_eq!(record.status(), SubmissionStatus::Unanswered);
}

#[tokio::test]
async fn end_run_leaves_a_run_owned_by_another_task_alone() {
    let (session, _storage) = open_session().await;
    session
        .commit(
            |tx| {
                Box::pin(async move {
                    let live = tx.doc(&*LIVE_DOC, access(), None).await?;
                    let run = LiveRun {
                        task_id: TaskId::new(42),
                        inputs: vec![SubmissionId::new(9)],
                    };
                    live.set(path("run"), serde_json::to_value(&run).expect("run"))
                        .map_err(|error| {
                            pi_durable::session::SessionError::Message(error.to_string())
                        })?;
                    live.set(path("generation"), json!({"attempt": 3}))
                        .map_err(|error| {
                            pi_durable::session::SessionError::Message(error.to_string())
                        })?;

                    // 别的任务结束：`run` 保留，但呈现仍会被清掉。
                    end_run(
                        tx,
                        &live,
                        TaskId::new(7),
                        SubmissionSettlement::Unanswered {
                            reason: "aborted".to_string(),
                            detail: None,
                        },
                    )?;
                    Ok(())
                })
            },
            context(),
        )
        .await
        .expect("endRun for another task");

    let live = session
        .snapshot(&*LIVE_DOC, Some(1), None, context())
        .await
        .expect("snapshot")
        .expect("pi.live");
    assert_eq!(live["run"]["taskId"], json!(42), "别的任务的运行不受影响");
    assert!(live.get("generation").is_none());
}

#[tokio::test]
async fn tool_slot_lookup_follows_task_ids() {
    let (session, _storage) = open_session().await;
    let (index, missing) = session
        .commit(
            |tx| {
                Box::pin(async move {
                    let live = tx.doc(&*LIVE_DOC, access(), None).await?;
                    let slots = vec![
                        ToolSlot {
                            call_id: "a".to_string(),
                            name: "read".to_string(),
                            task_id: Some(TaskId::new(1)),
                            status: ToolSlotStatus::Pending,
                            output: None,
                            dropped_bytes: None,
                            dropped_lines: None,
                            details: None,
                            diagnostics: None,
                            entry: None,
                        },
                        ToolSlot {
                            call_id: "b".to_string(),
                            name: "bash".to_string(),
                            task_id: Some(TaskId::new(2)),
                            status: ToolSlotStatus::Running,
                            output: None,
                            dropped_bytes: None,
                            dropped_lines: None,
                            details: None,
                            diagnostics: None,
                            entry: None,
                        },
                    ];
                    live.set(path("tools"), serde_json::to_value(&slots).expect("tools"))
                        .map_err(|error| {
                            pi_durable::session::SessionError::Message(error.to_string())
                        })?;
                    let index = tool_slot_index(&live, TaskId::new(2));
                    let missing = tool_slot_index(&live, TaskId::new(9));
                    let state = live_state(&live);
                    assert_eq!(state.tools.as_ref().expect("tools").len(), 2);
                    Ok((index, missing))
                })
            },
            context(),
        )
        .await
        .expect("toolSlot");

    assert_eq!(index, Some(1));
    assert_eq!(missing, None, "不在当前轮的槽位返回 None");
}

#[tokio::test]
async fn resolve_settings_is_used_for_queue_modes() {
    let settings = resolve_settings(None);
    let modes = QueueModes::from_settings(&settings);
    assert_eq!(
        modes.steering_mode,
        pi_durable::harness::types::QueueMode::OneAtATime
    );
}

// ─── 辅助 ────────────────────────────────────────────────────────────────────

struct NamedTool(&'static str);

#[async_trait::async_trait]
impl pi_durable::harness::types::ToolRegistration for NamedTool {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "test tool"
    }

    fn parameters(&self) -> &JsonValue {
        static PARAMETERS: std::sync::LazyLock<JsonValue> =
            std::sync::LazyLock::new(|| json!({"type": "object"}));
        &PARAMETERS
    }

    async fn execute(
        &self,
        _args: JsonValue,
        _api: Arc<dyn pi_durable::harness::types::ToolExecutionApi>,
        _context: Arc<dyn Context>,
    ) -> Result<pi_durable::harness::types::ToolExecutionResult, pi_durable::session::SessionError>
    {
        Ok(pi_durable::harness::types::ToolExecutionResult::default())
    }
}
