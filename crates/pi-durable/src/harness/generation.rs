//! 对应 `harness/generation.ts` 的运行控制部分。
//!
//! 上游 `generation.ts` 是 681 行的 generation 任务；这里先落地其中**不依赖该任务定义**的四件事：
//! `startRun`（上游第 664 行）、私有的 `createGeneration`（第 674 行）、`handOver`（第 679 行），
//! 以及纯逻辑的 `thresholdCompaction`（第 306 行）。
//! `startRun` 做成接受 generation 任务定义的工厂 [`make_start_run`]：上游直接引用模块级常量
//! `GenerationTask`，而 Rust 侧该定义要到 P5g-2 才落地，工厂让运行控制可以先行接线、随后绑定。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游 `startRun` 是 `live.run = { taskId, inputs }`，就地赋一个对象；Rust 对 `pi.live` 草稿
//!   做一次 `set` 写入同样的形状——`run` 整个子树被替换，语义相同。
//! - 上游 `createGeneration` 返回 `TaskId<GenerationResult>`；运行控制路径从不使用该结果类型，
//!   而 Rust 的 `TaskId` 不携带结果类型参数（结果类型只出现在 `TaskRecord` 里），因此这里是无参的。
//! - `thresholdCompaction` 里上游用浮点减法得到 `blocking` / `background` 阈值；Rust 的
//!   `CompactionPolicy` 字段是 `u64`，因此用 `i128` 做差，以免上下文窗口小于保留量时下溢。
//!
//! # 待落地（P5g-2）
//!
//! `GenerationTask` 本身：上游第 1–663 行（phase 处理器、重试、溢出压缩、部分消息节流等）。

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use pi_ai::utils::overflow::is_context_overflow;
use pi_ai::utils::retry::{is_retryable_assistant_error, retry_delay_ms};
use pi_ai::utils::transcript::get_current_tools;
use pi_ai::{
    AssistantMessage, ContentBlock, DeferredHandle, Message, Model, ModelThinkingLevel,
    SimpleStreamOptions, Tool, ToolCall, UserContent, UserMessage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::chord::context::Context;
use crate::chord::delta::PathSegment;
use crate::entries::{ASSISTANT_ENTRY, RESET_ENTRY, SYSTEM_ENTRY, USER_ENTRY};
use crate::harness::agent::add_tools;
use crate::harness::compaction::{CreateCompaction, estimate_context, select_cut};
use crate::harness::inbox::{BoundaryAt, QueueModes, apply_boundary, prepare_boundary};
use crate::harness::live::{
    DeferredPoll, LIVE_DOC, LiveGeneration, LiveState, ToolSlot, end_run, live_state,
};
use crate::harness::prompt::{plan_system_entries, render_sections, replay_sections};
use crate::harness::provider::ensure_provider_session_id;
use crate::harness::submissions::StartRun;
use crate::harness::tool::{ToolTaskInput, ToolTaskResult, append_tool_result, harness_error};
use crate::harness::types::{
    CompactionPolicy, ContextView, ConversationStreamOptions, GenerationRequest, HookApi,
    HookHandlers, ModelRef, NextTaskState, PromptInput, RunningTask, RuntimeHookApi, TaskRuntime,
    ToolExecutionMode,
};
use crate::harness::usage::{UsageBucket, record_usage};
use crate::session::SessionError;
use crate::session::transaction::Transaction;
use crate::types::{
    ConversationId, DocAccess, Draft, EntryDraft, EntryHead, EntryId, SubmissionId,
    SubmissionSettlement, Task, TaskDefinitionSpec, TaskId, TaskOptions, TaskOutcome,
    TaskOutcomeError, TaskOwnership, TaskState,
};

/// 对应 `startRun(tx, conversationId, live, inputs)`：启动一次运行。
///
/// 上游用 generation 任务定义；这里由调用方传入，返回的函数即 [`StartRun`]。
/// 对应 `startRun(tx, conversationId, live, inputs)`：启动一次运行。
///
/// 上游直接引用模块级 `GenerationTask`；Rust 侧 generation 任务定义与 `start_run` 相互依赖，
/// 因此这里接收一个**惰性工厂**（返回 generation 任务定义），由组装层用 `OnceLock` 打破循环。
pub fn make_start_run(generation_task: Arc<dyn Fn() -> Arc<Task> + Send + Sync>) -> StartRun {
    Arc::new(
        move |tx: &Transaction,
              conversation_id: ConversationId,
              live: &Draft,
              inputs: Vec<SubmissionId>| {
            let task = (generation_task)();
            Box::pin(async move {
                let task_id = create_generation(tx, conversation_id, &task).await?;
                live.set(
                    vec![PathSegment::Key("run".to_string())],
                    serde_json::json!({
                        "taskId": task_id.get(),
                        "inputs": inputs.iter().map(|id| id.get()).collect::<Vec<_>>(),
                    }),
                )
                .map_err(|error| SessionError::Message(error.to_string()))?;
                Ok(())
            })
        },
    )
}

/// 对应 `createGeneration(tx, conversationId)`：一个由其会话拥有的 generation 任务。
async fn create_generation(
    tx: &Transaction,
    conversation_id: ConversationId,
    task: &Task,
) -> Result<crate::types::TaskId, SessionError> {
    tx.create_task(
        task,
        JsonValue::Object(Default::default()),
        TaskOptions {
            ownership: TaskOwnership::Conversation,
            conversation_id: Some(conversation_id),
            background: None,
        },
    )
    .await
}

/// 对应 `handOver(live, from, to)`：把运行控制从 `from` 交给 `to`；运行的输入随之移动。
pub fn hand_over(
    live: &Draft,
    from: crate::types::TaskId,
    to: crate::types::TaskId,
) -> Result<(), SessionError> {
    let state = crate::harness::live::live_state(live);
    let Some(run) = state.run else {
        return Ok(());
    };
    if run.task_id != from {
        return Ok(());
    }
    live.set(
        vec![
            PathSegment::Key("run".to_string()),
            PathSegment::Key("taskId".to_string()),
        ],
        serde_json::Value::from(to.get()),
    )
    .map_err(|error| SessionError::Message(error.to_string()))?;
    Ok(())
}

/// 对应 `thresholdCompaction` 的结果：越过了哪一道自动压缩的阈值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdOver {
    /// 超过 `contextWindow - reserveTokens`。
    Blocking,
    /// 超过后台阈值（比阻塞阈值低 `backgroundTokens`）。
    Background,
}

/// 对应 `thresholdCompaction(view, planned, contextWindow, policy)`（spec §8.3）：
/// 在发请求之前就该开始的阈值压缩——`blocking` 高于 `contextWindow - reserveTokens`，
/// `background` 高于后台阈值，且**只有选段能找到切点时才返回**。
///
/// 调用方只在当前没有已列出的压缩时启动后台压缩。
pub fn threshold_compaction(
    view: &ContextView,
    planned: &[EntryDraft],
    context_window: u64,
    policy: CompactionPolicy,
) -> Option<ThresholdOver> {
    if !policy.enabled || context_window == 0 {
        return None;
    }
    let extra: Vec<Message> = planned
        .iter()
        .flat_map(|entry| entry.model.clone().unwrap_or_default())
        .collect();
    // 上游用浮点减法；这里用 i128 是为了上下文窗口小于保留量时不发生无符号下溢。
    let tokens = i128::from(estimate_context(view, &extra));
    let window = i128::from(context_window);
    let blocking = window - i128::from(policy.reserve_tokens);
    let background = blocking - i128::from(policy.background_tokens);
    let over = if tokens > blocking {
        Some(ThresholdOver::Blocking)
    } else if policy.background_tokens > 0 && tokens > background {
        Some(ThresholdOver::Background)
    } else {
        None
    };
    if over.is_none() || select_cut(view, policy.keep_recent_tokens).is_none() {
        return None;
    }
    over
}

// ─── GenerationTask 本体 ──────────────────────────────────────────────────────

/// 对应 `GenerationInput`：空输入。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationInput;

/// 对应 `GenerationCheckpoint`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum GenerationCheckpoint {
    /// `{ phase: "prepare" }`。
    Prepare {
        /// 尝试次数。
        attempt: u32,
        /// 本 generation 等待的阻塞压缩。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// 启动 `compacted` 的溢出错误文本。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overflow: Option<String>,
    },
    /// `{ phase: "request" }`。
    Request {
        /// 尝试次数。
        attempt: u32,
        /// 阻塞压缩。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// 模型。
        model: ModelRef,
        /// thinking level。
        thinking_level: ModelThinkingLevel,
        /// 请求选项。
        stream_options: ConversationStreamOptions,
        /// 请求包含的最新条目。
        cutoff: EntryId,
    },
    /// `{ phase: "retry" }`。
    Retry {
        /// 尝试次数。
        attempt: u32,
        /// 阻塞压缩。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// 重试时刻。
        until: u64,
    },
    /// `{ phase: "poll" }`。
    Poll {
        /// 尝试次数。
        attempt: u32,
        /// 阻塞压缩。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// 模型。
        model: ModelRef,
        /// 请求包含的最新条目。
        cutoff: EntryId,
        /// provider 端延迟句柄。
        handle: DeferredHandle,
        /// 轮询时刻。
        poll_at: u64,
    },
    /// `{ phase: "tools" }`。
    Tools {
        /// 工具调用的回答。
        assistant: EntryId,
        /// 已创建的工具任务（按调用顺序）。
        tools: Vec<TaskId>,
        /// 串行轮尚未启动的调用（按调用顺序）。
        pending: Vec<String>,
    },
}

/// 对应 `GenerationResult`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationResult {
    /// 回答条目。
    pub entry_id: EntryId,
}

/// 分类需要的请求上下文。
#[derive(Clone)]
struct Request {
    attempt: u32,
    compacted: Option<TaskId>,
    model: ModelRef,
    cutoff: EntryId,
    messages: Option<Vec<Message>>,
    poll_at: Option<u64>,
}

const DEFAULT_POLL_AFTER_MS: u64 = 5000;

/// 对应 `GenerationTask`：内置 generation 任务。
struct GenerationTaskDefinition {
    start_run: StartRun,
    create_compaction: CreateCompaction,
    tool_task: Arc<Task>,
    generation_task: Arc<dyn Fn() -> Arc<Task> + Send + Sync>,
}

/// 对应 `GenerationTask` 工厂：`start_run` / `create_compaction` / `tool_task` / `generation_task` 由组装层提供。
pub fn make_generation_task(
    start_run: StartRun,
    create_compaction: CreateCompaction,
    tool_task: Arc<Task>,
    generation_task: Arc<dyn Fn() -> Arc<Task> + Send + Sync>,
) -> Arc<Task> {
    Arc::new(Task::new(Arc::new(GenerationTaskDefinition {
        start_run,
        create_compaction,
        tool_task,
        generation_task,
    })))
}

fn session_error(error: impl std::fmt::Display) -> SessionError {
    SessionError::Message(error.to_string())
}

fn doc_access(conversation_id: ConversationId) -> DocAccess {
    DocAccess {
        owner: Some(conversation_id.get()),
        key: None,
    }
}

fn checkpoint_json(task: &RunningTask) -> JsonValue {
    match &task.0.state {
        TaskState::Running { checkpoint } => checkpoint.clone(),
        _ => JsonValue::Null,
    }
}

/// `ToolRegistration` → pi-ai `Tool`。
fn to_pi_tool(tool: &Arc<dyn crate::harness::types::ToolRegistration>) -> Tool {
    Tool {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters: tool.parameters().clone(),
        constrained_sampling: None,
    }
}

fn event_partial(event: &pi_ai::AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        pi_ai::AssistantMessageEvent::Start { partial }
        | pi_ai::AssistantMessageEvent::TextStart { partial, .. }
        | pi_ai::AssistantMessageEvent::TextDelta { partial, .. }
        | pi_ai::AssistantMessageEvent::TextEnd { partial, .. }
        | pi_ai::AssistantMessageEvent::ThinkingStart { partial, .. }
        | pi_ai::AssistantMessageEvent::ThinkingDelta { partial, .. }
        | pi_ai::AssistantMessageEvent::ThinkingEnd { partial, .. }
        | pi_ai::AssistantMessageEvent::ToolCallStart { partial, .. }
        | pi_ai::AssistantMessageEvent::ToolCallDelta { partial, .. }
        | pi_ai::AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        _ => None,
    }
}

impl GenerationTaskDefinition {
    /// 对应 `failNoModel(runtime, ref, context)`。
    async fn fail_no_model(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        reference: Option<&ModelRef>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let message = match reference {
            None => "No model is configured".to_string(),
            Some(reference) => format!(
                "Model {}/{} is not available",
                reference.provider, reference.model_id
            ),
        };
        let runtime = Arc::clone(runtime);
        let runtime_for_closure = Arc::clone(&runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_closure);
                    let message = message.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        end_run(
                            tx,
                            &live,
                            runtime.task_id(),
                            SubmissionSettlement::Unanswered {
                                reason: "no_model".to_string(),
                                detail: None,
                            },
                        )
                        .map_err(session_error)?;
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Failed {
                                error: TaskOutcomeError {
                                    message,
                                    detail: Some(serde_json::json!({ "reason": "no_model" })),
                                },
                                result: None,
                            },
                        }))
                    })
                }),
                context,
            )
            .await
    }

    /// 对应 `failModelError(runtime, text, context)`。
    async fn fail_model_error(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        text: String,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let runtime = Arc::clone(runtime);
        let runtime_for_closure = Arc::clone(&runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_closure);
                    let text = text.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        end_run(
                            tx,
                            &live,
                            runtime.task_id(),
                            SubmissionSettlement::Unanswered {
                                reason: "model_error".to_string(),
                                detail: Some(serde_json::json!(text)),
                            },
                        )
                        .map_err(session_error)?;
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Failed {
                                error: TaskOutcomeError {
                                    message: text,
                                    detail: Some(serde_json::json!({ "reason": "model_error" })),
                                },
                                result: None,
                            },
                        }))
                    })
                }),
                context,
            )
            .await
    }

    /// 对应 `prepare` phase。
    async fn run_prepare(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let conversation_id = runtime.conversation_id();
        let agent = runtime.agent(Arc::clone(&context)).await?;
        let settings = runtime.settings();
        let reference = agent.model.clone();
        let resolved = reference
            .as_ref()
            .and_then(|r| runtime.models().get_model(&r.provider, &r.model_id));
        if reference.is_none() || resolved.is_none() {
            return self
                .fail_no_model(runtime, reference.as_ref(), context)
                .await;
        }
        let resolved = resolved.expect("resolved");
        let checkpoint: GenerationCheckpoint =
            serde_json::from_value(checkpoint_json(task)).map_err(session_error)?;
        let GenerationCheckpoint::Prepare {
            attempt,
            compacted,
            overflow,
        } = checkpoint
        else {
            return Err(SessionError::Message(
                "pi.generation prepare phase has no attempt".to_string(),
            ));
        };
        if let (Some(compacted), Some(overflow)) = (compacted, overflow) {
            let outcomes = runtime.outcomes(&[compacted], Arc::clone(&context)).await?;
            let outcome = outcomes.first();
            let completed = outcome.is_some_and(|outcome| {
                matches!(outcome, TaskOutcome::Completed { result } if result.get("entryId").is_some())
            });
            if !completed {
                return self.fail_model_error(runtime, overflow, context).await;
            }
        }
        let view = runtime
            .context_view(conversation_id, Arc::clone(&context), None)
            .await?;
        let shown = replay_sections(&view.messages);
        let report = |error: SessionError| runtime.report(error);
        let mut env = None;
        match runtime.env(Arc::clone(&context)).await {
            Ok(value) => env = value,
            Err(error) => {
                if context
                    .abort_signal()
                    .is_some_and(|signal| signal.aborted())
                {
                    return Err(error);
                }
                report(error);
            }
        }
        let input = PromptInput {
            conversation_id,
            agent: agent.clone(),
            env,
            shown: shown
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            read: runtime.clone(),
        };
        let desired = render_sections(&agent.sections, &input, &shown, &report, &context).await?;
        let pi_tools: Vec<Tool> = agent.tools.iter().map(to_pi_tool).collect();
        let entries = plan_system_entries(&view, &desired, &pi_tools, runtime.now());
        let threshold = if compacted.is_none() {
            threshold_compaction(
                &view,
                &entries,
                resolved.context_window,
                settings.compaction,
            )
        } else {
            None
        };
        if threshold == Some(ThresholdOver::Blocking) {
            let create_compaction = Arc::clone(&self.create_compaction);
            let runtime_for_commit = Arc::clone(runtime);
            runtime
                .commit(
                    Box::new(move |tx, _current| {
                        let runtime = Arc::clone(&runtime_for_commit);
                        let create_compaction = Arc::clone(&create_compaction);
                        Box::pin(async move {
                            let child = create_compaction(
                                tx,
                                runtime.conversation_id(),
                                crate::harness::compaction::CompactionInput {
                                    reason: crate::harness::types::CompactionReason::Threshold,
                                    instructions: None,
                                },
                                Some(runtime.task_id()),
                            )
                            .await?;
                            let checkpoint = GenerationCheckpoint::Prepare {
                                attempt,
                                compacted: Some(child),
                                overflow: None,
                            };
                            Ok(Some(NextTaskState::Waiting {
                                checkpoint: serde_json::to_value(&checkpoint)
                                    .map_err(session_error)?,
                                on: vec![child],
                                policy: crate::types::JoinPolicy::AllSettled,
                            }))
                        })
                    }),
                    Arc::clone(&context),
                )
                .await?;
            return Ok(());
        }
        let runtime_for_commit = Arc::clone(runtime);
        let create_compaction = Arc::clone(&self.create_compaction);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let _runtime = Arc::clone(&runtime_for_commit);
                    let create_compaction = Arc::clone(&create_compaction);
                    let entries = entries.clone();
                    Box::pin(async move {
                        let mut cutoff = tx
                            .scan_entries(
                                crate::types::EntryQuery {
                                    conversation_id,
                                    min_entry_id: None,
                                    max_entry_id: None,
                                    order: None,
                                },
                                1,
                                None,
                            )
                            .await?
                            .items
                            .first()
                            .map(|entry| entry.id);
                        for entry in &entries {
                            cutoff = Some(
                                tx.append_entry(
                                    Some(SYSTEM_ENTRY.kind().to_string()),
                                    conversation_id,
                                    entry.clone(),
                                )
                                .await?
                                .id,
                            );
                        }
                        let Some(cutoff) = cutoff else {
                            return Err(SessionError::Message(format!(
                                "Conversation {conversation_id} has no entries to send"
                            )));
                        };
                        if threshold == Some(ThresholdOver::Background) {
                            let live = tx
                                .doc(&*LIVE_DOC, doc_access(conversation_id), None)
                                .await
                                .map_err(SessionError::Doc)?;
                            if live_state(&live).compactions.is_none() {
                                create_compaction(
                                    tx,
                                    conversation_id,
                                    crate::harness::compaction::CompactionInput {
                                        reason: crate::harness::types::CompactionReason::Threshold,
                                        instructions: None,
                                    },
                                    None,
                                )
                                .await?;
                            }
                        }
                        let checkpoint = GenerationCheckpoint::Request {
                            attempt,
                            compacted,
                            model: reference.clone().expect("reference"),
                            thinking_level: agent.thinking_level,
                            stream_options: settings.stream,
                            cutoff,
                        };
                        Ok(Some(NextTaskState::Running {
                            checkpoint: serde_json::to_value(&checkpoint).map_err(session_error)?,
                        }))
                    })
                }),
                Arc::clone(&context),
            )
            .await
    }

    /// 对应 `request` phase。
    async fn run_request(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let checkpoint: GenerationCheckpoint =
            serde_json::from_value(checkpoint_json(task)).map_err(session_error)?;
        let GenerationCheckpoint::Request {
            attempt,
            compacted,
            model: reference,
            thinking_level,
            stream_options,
            cutoff,
        } = checkpoint
        else {
            return Err(SessionError::Message(
                "pi.generation request phase has no request".to_string(),
            ));
        };
        let conversation_id = runtime.conversation_id();
        let runtime_for_commit = Arc::clone(runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_commit);
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        Self::convert_partial_static(tx, &live, runtime.conversation_id()).await?;
                        let generation = LiveGeneration {
                            attempt,
                            message: None,
                            retry: None,
                            deferred: None,
                        };
                        live.set(
                            vec![PathSegment::Key("generation".to_string())],
                            serde_json::to_value(&generation).map_err(session_error)?,
                        )
                        .map_err(session_error)?;
                        Ok(None)
                    })
                }),
                Arc::clone(&context),
            )
            .await?;
        let Some(model) = runtime
            .models()
            .get_model(&reference.provider, &reference.model_id)
        else {
            return self.fail_no_model(runtime, Some(&reference), context).await;
        };
        let view = runtime
            .context_view(conversation_id, Arc::clone(&context), Some(cutoff))
            .await?;
        let mut messages = view.messages.clone();
        for handlers in runtime.hooks().handlers() {
            if let HookHandlers::Generation(hook) = handlers.as_ref() {
                let api: Arc<dyn HookApi> = Arc::new(RuntimeHookApi::new(Arc::clone(runtime)));
                if let Some(replaced) = hook
                    .before_request(
                        &GenerationRequest {
                            messages: messages.clone(),
                        },
                        api,
                        Arc::clone(&context),
                    )
                    .await?
                {
                    messages = replaced.messages;
                }
            }
        }
        let mut options = crate::harness::compaction::stream_options(&stream_options);
        options.stream.request.signal = Some(runtime.signal());
        options.stream.session_id =
            Some(ensure_provider_session_id(runtime, Arc::clone(&context)).await?);
        options.reasoning = crate::harness::compaction::thinking_level(thinking_level);
        let message = Self::stream_response(
            runtime,
            &model,
            &messages,
            options,
            attempt,
            Arc::clone(&context),
        )
        .await?;
        let request = Request {
            attempt,
            compacted,
            model: reference,
            cutoff,
            messages: Some(view.messages.clone()),
            poll_at: None,
        };
        self.classify(runtime, request, message, context).await
    }

    /// 对应 `retry` phase。
    async fn run_retry(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let checkpoint: GenerationCheckpoint =
            serde_json::from_value(checkpoint_json(task)).map_err(session_error)?;
        let GenerationCheckpoint::Retry {
            attempt,
            compacted,
            until,
        } = checkpoint
        else {
            return Err(SessionError::Message(
                "pi.generation retry phase has no until".to_string(),
            ));
        };
        runtime.sleep(until, Arc::clone(&context)).await?;
        let runtime_for_commit = Arc::clone(runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_commit);
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        let generation = LiveGeneration {
                            attempt: attempt + 1,
                            message: None,
                            retry: None,
                            deferred: None,
                        };
                        live.set(
                            vec![PathSegment::Key("generation".to_string())],
                            serde_json::to_value(&generation).map_err(session_error)?,
                        )
                        .map_err(session_error)?;
                        let checkpoint = GenerationCheckpoint::Prepare {
                            attempt: attempt + 1,
                            compacted,
                            overflow: None,
                        };
                        Ok(Some(NextTaskState::Running {
                            checkpoint: serde_json::to_value(&checkpoint).map_err(session_error)?,
                        }))
                    })
                }),
                Arc::clone(&context),
            )
            .await
    }

    /// 对应 `poll` phase。
    async fn run_poll(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let checkpoint: GenerationCheckpoint =
            serde_json::from_value(checkpoint_json(task)).map_err(session_error)?;
        let GenerationCheckpoint::Poll {
            attempt,
            compacted,
            model: reference,
            cutoff,
            handle,
            poll_at,
        } = checkpoint
        else {
            return Err(SessionError::Message(
                "pi.generation poll phase has no handle".to_string(),
            ));
        };
        let Some(model) = runtime
            .models()
            .get_model(&reference.provider, &reference.model_id)
        else {
            return self.fail_no_model(runtime, Some(&reference), context).await;
        };
        runtime.sleep(poll_at, Arc::clone(&context)).await?;
        let message = runtime
            .models()
            .fetch_deferred(
                &model,
                &handle,
                Some(&pi_ai::DeferredFetchOptions {
                    request: pi_ai::ProviderRequestOptions {
                        signal: Some(runtime.signal()),
                        api_key: None,
                        headers: None,
                        timeout_ms: None,
                        max_retries: None,
                        max_retry_delay_ms: None,
                        on_payload: None,
                        on_response: None,
                    },
                    wait: None,
                }),
            )
            .await;
        let request = Request {
            attempt,
            compacted,
            model: reference,
            cutoff,
            messages: None,
            poll_at: Some(poll_at),
        };
        self.classify(runtime, request, message, context).await
    }

    /// 对应 `tools` phase。
    async fn run_tools(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let checkpoint: GenerationCheckpoint =
            serde_json::from_value(checkpoint_json(task)).map_err(session_error)?;
        let GenerationCheckpoint::Tools {
            assistant,
            tools,
            pending,
        } = checkpoint
        else {
            return Err(SessionError::Message(
                "pi.generation tools phase has no assistant".to_string(),
            ));
        };
        let next = pending.first().cloned();
        let Some(next) = next else {
            return self
                .finish_tool_round(runtime, assistant, &tools, context)
                .await;
        };
        let rest: Vec<String> = pending.iter().skip(1).cloned().collect();
        let tool_task = Arc::clone(&self.tool_task);
        let runtime_for_commit = Arc::clone(runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_commit);
                    let tool_task = Arc::clone(&tool_task);
                    let assistant = assistant;
                    let next = next.clone();
                    let rest = rest.clone();
                    let tools = tools.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        let input = ToolTaskInput {
                            assistant,
                            call_id: next.clone(),
                        };
                        let task_id = tx
                            .create_task(
                                &tool_task,
                                serde_json::to_value(&input).map_err(session_error)?,
                                TaskOptions {
                                    ownership: TaskOwnership::Task {
                                        task_id: runtime.task_id(),
                                    },
                                    conversation_id: None,
                                    background: None,
                                },
                            )
                            .await?;
                        let state = live_state(&live);
                        if let Some(index) = state.tools.as_ref().and_then(|slots| {
                            slots
                                .iter()
                                .position(|slot| slot.call_id == next && slot.task_id.is_none())
                        }) {
                            live.set(
                                vec![
                                    PathSegment::Key("tools".to_string()),
                                    PathSegment::Index(index),
                                    PathSegment::Key("taskId".to_string()),
                                ],
                                JsonValue::from(task_id.get()),
                            )
                            .map_err(session_error)?;
                        }
                        let mut new_tools = tools;
                        new_tools.push(task_id);
                        let checkpoint = GenerationCheckpoint::Tools {
                            assistant,
                            tools: new_tools.clone(),
                            pending: rest,
                        };
                        Ok(Some(NextTaskState::Waiting {
                            checkpoint: serde_json::to_value(&checkpoint).map_err(session_error)?,
                            on: vec![task_id],
                            policy: crate::types::JoinPolicy::AllSettled,
                        }))
                    })
                }),
                Arc::clone(&context),
            )
            .await
    }

    /// 对应 `streamResponse(...)`：流式请求 + 部分消息节流提交。
    async fn stream_response(
        runtime: &Arc<dyn TaskRuntime>,
        model: &Model,
        messages: &[Message],
        options: SimpleStreamOptions,
        attempt: u32,
        context: Arc<dyn Context>,
    ) -> Result<AssistantMessage, SessionError> {
        let interval = runtime.settings().progress.partial_interval_ms;
        let runtime = Arc::clone(runtime);
        let pending: Arc<Mutex<Option<AssistantMessage>>> = Arc::new(Mutex::new(None));
        let flush_signal = pi_ai::AbortSignal::new();
        let flush_task = {
            let pending = Arc::clone(&pending);
            let runtime = Arc::clone(&runtime);
            let context = Arc::clone(&context);
            let signal = flush_signal.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(interval)) => {}
                        _ = signal.cancelled() => return,
                    }
                    let partial = { pending.lock().expect("pending").take() };
                    let Some(partial) = partial else {
                        continue;
                    };
                    let runtime = Arc::clone(&runtime);
                    let runtime_for_commit = Arc::clone(&runtime);
                    let context = Arc::clone(&context);
                    if let Err(error) = runtime
                        .commit(
                            Box::new(move |tx, _current| {
                                let runtime = Arc::clone(&runtime_for_commit);
                                let partial = partial.clone();
                                Box::pin(async move {
                                    let live = tx
                                        .doc(
                                            &*LIVE_DOC,
                                            doc_access(runtime.conversation_id()),
                                            None,
                                        )
                                        .await
                                        .map_err(SessionError::Doc)?;
                                    let mut generation =
                                        live_state(&live).generation.unwrap_or(LiveGeneration {
                                            attempt,
                                            message: None,
                                            retry: None,
                                            deferred: None,
                                        });
                                    generation.message = Some(partial);
                                    live.set(
                                        vec![PathSegment::Key("generation".to_string())],
                                        serde_json::to_value(&generation).map_err(session_error)?,
                                    )
                                    .map_err(session_error)?;
                                    Ok(None)
                                })
                            }),
                            Arc::clone(&context),
                        )
                        .await
                        && !runtime.signal().aborted()
                    {
                        runtime.report(error);
                    }
                }
            })
        };
        let events = runtime.models().stream_simple(
            model,
            &pi_ai::Context {
                system_prompt: None,
                messages: messages.to_vec(),
                tools: None,
            },
            Some(&options),
        );
        let mut stream = events.clone();
        while let Some(event) = stream.next().await {
            match &event {
                pi_ai::AssistantMessageEvent::Done { .. }
                | pi_ai::AssistantMessageEvent::Error { .. } => continue,
                _ => {}
            }
            let Some(partial) = event_partial(&event) else {
                continue;
            };
            if partial.content.is_empty() {
                continue;
            }
            *pending.lock().expect("pending") = Some(partial.clone());
        }
        let message = events.result().await;
        flush_signal.abort();
        let _ = flush_task.await;
        Ok(message)
    }

    /// 对应 `classify(...)`。
    async fn classify(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        request: Request,
        message: AssistantMessage,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        runtime
            .signal()
            .throw_if_aborted()
            .map_err(SessionError::Aborted)?;
        let conversation_id = runtime.conversation_id();
        if message.stop_reason == pi_ai::StopReason::Deferred && message.deferred.is_some() {
            let handle = message.deferred.clone().expect("deferred");
            let poll_at = (runtime.now() + (handle.poll_after_ms.unwrap_or(DEFAULT_POLL_AFTER_MS)))
                .max(request.poll_at.map(|at| at + 1).unwrap_or(0));
            let runtime_for_commit = Arc::clone(runtime);
            runtime
                .commit(
                    Box::new(move |tx, _current| {
                        let runtime = Arc::clone(&runtime_for_commit);
                        let handle = handle.clone();
                        Box::pin(async move {
                            let live = tx
                                .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                                .await
                                .map_err(SessionError::Doc)?;
                            let generation = LiveGeneration {
                                attempt: request.attempt,
                                message: None,
                                retry: None,
                                deferred: Some(DeferredPoll { poll_at }),
                            };
                            live.set(
                                vec![PathSegment::Key("generation".to_string())],
                                serde_json::to_value(&generation).map_err(session_error)?,
                            )
                            .map_err(session_error)?;
                            let checkpoint = GenerationCheckpoint::Poll {
                                attempt: request.attempt,
                                compacted: request.compacted,
                                model: request.model.clone(),
                                cutoff: request.cutoff,
                                handle,
                                poll_at,
                            };
                            Ok(Some(NextTaskState::Running {
                                checkpoint: serde_json::to_value(&checkpoint)
                                    .map_err(session_error)?,
                            }))
                        })
                    }),
                    Arc::clone(&context),
                )
                .await?;
            return Ok(());
        }
        for handlers in runtime.hooks().handlers() {
            if let HookHandlers::Generation(hook) = handlers.as_ref() {
                let api: Arc<dyn HookApi> = Arc::new(RuntimeHookApi::new(Arc::clone(runtime)));
                hook.after_response(message.clone(), api, Arc::clone(&context))
                    .await?;
            }
        }
        let calls: Vec<ToolCall> = message
            .content
            .iter()
            .filter_map(|content| match content {
                ContentBlock::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect();
        if message.stop_reason == pi_ai::StopReason::ToolUse && !calls.is_empty() {
            return self
                .start_tool_round(runtime, request, message, &calls, context)
                .await;
        }
        if matches!(
            message.stop_reason,
            pi_ai::StopReason::Stop | pi_ai::StopReason::Length | pi_ai::StopReason::ToolUse
        ) {
            return self.answer(runtime, message, context).await;
        }
        let settings = runtime.settings();
        let overflow =
            message.stop_reason == pi_ai::StopReason::Error && is_context_overflow(&message, None);
        if overflow && request.compacted.is_none() && settings.compaction.enabled {
            let policy = settings.compaction;
            let view = runtime
                .context_view(conversation_id, Arc::clone(&context), Some(request.cutoff))
                .await?;
            if select_cut(&view, policy.keep_recent_tokens).is_some() {
                let text = message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Context overflow".to_string());
                let create_compaction = Arc::clone(&self.create_compaction);
                let runtime_for_commit = Arc::clone(runtime);
                runtime
                    .commit(
                        Box::new(move |tx, _current| {
                            let runtime = Arc::clone(&runtime_for_commit);
                            let create_compaction = Arc::clone(&create_compaction);
                            let message = message.clone();
                            Box::pin(async move {
                                let live = tx
                                    .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                                    .await
                                    .map_err(SessionError::Doc)?;
                                Self::append_assistant_static(
                                    tx,
                                    runtime.conversation_id(),
                                    message,
                                )
                                .await?;
                                live.delete(vec![PathSegment::Key("generation".to_string())])
                                    .map_err(session_error)?;
                                let child = create_compaction(
                                    tx,
                                    runtime.conversation_id(),
                                    crate::harness::compaction::CompactionInput {
                                        reason: crate::harness::types::CompactionReason::Overflow,
                                        instructions: None,
                                    },
                                    Some(runtime.task_id()),
                                )
                                .await?;
                                let checkpoint = GenerationCheckpoint::Prepare {
                                    attempt: request.attempt,
                                    compacted: Some(child),
                                    overflow: Some(text.clone()),
                                };
                                Ok(Some(NextTaskState::Waiting {
                                    checkpoint: serde_json::to_value(&checkpoint)
                                        .map_err(session_error)?,
                                    on: vec![child],
                                    policy: crate::types::JoinPolicy::AllSettled,
                                }))
                            })
                        }),
                        Arc::clone(&context),
                    )
                    .await?;
                return Ok(());
            }
        }
        let policy = settings.retry;
        let retry = message.stop_reason == pi_ai::StopReason::Error
            && !overflow
            && is_retryable_assistant_error(&message)
            && policy.enabled
            && request.attempt <= policy.max_retries;
        let until = if retry {
            runtime.now()
                + retry_delay_ms(
                    policy.base_delay_ms,
                    policy.max_agent_delay_ms,
                    request.attempt as u64,
                )
        } else {
            0
        };
        let runtime_for_commit = Arc::clone(runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_commit);
                    let message = message.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        Self::append_assistant_static(
                            tx,
                            runtime.conversation_id(),
                            message.clone(),
                        )
                        .await?;
                        if retry {
                            let generation = LiveGeneration {
                                attempt: request.attempt,
                                message: None,
                                retry: Some(crate::harness::live::RetryBackoff {
                                    at: until,
                                    error: message.error_message.clone().unwrap_or_default(),
                                }),
                                deferred: None,
                            };
                            live.set(
                                vec![PathSegment::Key("generation".to_string())],
                                serde_json::to_value(&generation).map_err(session_error)?,
                            )
                            .map_err(session_error)?;
                            let checkpoint = GenerationCheckpoint::Retry {
                                attempt: request.attempt,
                                compacted: request.compacted,
                                until,
                            };
                            return Ok(Some(NextTaskState::Running {
                                checkpoint: serde_json::to_value(&checkpoint)
                                    .map_err(session_error)?,
                            }));
                        }
                        let text = message.error_message.clone().unwrap_or_else(|| {
                            format!(
                                "Model response ended with stop reason {:?}",
                                message.stop_reason
                            )
                        });
                        end_run(
                            tx,
                            &live,
                            runtime.task_id(),
                            SubmissionSettlement::Unanswered {
                                reason: "model_error".to_string(),
                                detail: Some(serde_json::json!(text)),
                            },
                        )
                        .map_err(session_error)?;
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Failed {
                                error: TaskOutcomeError {
                                    message: text,
                                    detail: Some(serde_json::json!({ "reason": "model_error" })),
                                },
                                result: None,
                            },
                        }))
                    })
                }),
                Arc::clone(&context),
            )
            .await
    }

    /// 对应 `answer(...)`。
    async fn answer(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        message: AssistantMessage,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let mut continuation: Option<UserContent> = None;
        for handlers in runtime.hooks().handlers() {
            if let HookHandlers::Generation(hook) = handlers.as_ref()
                && continuation.is_none()
            {
                let api: Arc<dyn HookApi> = Arc::new(RuntimeHookApi::new(Arc::clone(runtime)));
                if let Some(result) = hook
                    .on_yield(message.clone(), api, Arc::clone(&context))
                    .await?
                {
                    continuation = Some(result.user_input);
                }
            }
        }
        let conversation_id = runtime.conversation_id();
        let runtime_for_commit = Arc::clone(runtime);
        let generation_task = Arc::clone(&self.generation_task);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_commit);
                    let generation_task = Arc::clone(&generation_task);
                    let message = message.clone();
                    Box::pin(async move {
                        let boundary = prepare_boundary(
                            tx,
                            conversation_id,
                            QueueModes::from_settings(&runtime.settings()),
                        )
                        .await?;
                        let mut boundary = boundary;
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(conversation_id), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        let entry =
                            Self::append_assistant_static(tx, conversation_id, message.clone())
                                .await?;
                        let result = NextTaskState::Terminal {
                            outcome: TaskOutcome::Completed {
                                result: serde_json::to_value(GenerationResult {
                                    entry_id: entry.id,
                                })
                                .map_err(session_error)?,
                            },
                        };
                        let boundary_result =
                            apply_boundary(tx, &mut boundary, BoundaryAt::Final, runtime.now())
                                .await?;
                        if let Some(cont) = continuation.clone()
                            && boundary_result.users.is_empty()
                            && !boundary_result.reset
                        {
                            let user = UserMessage {
                                content: cont,
                                timestamp: runtime.now(),
                            };
                            tx.append_entry(
                                Some(USER_ENTRY.kind().to_string()),
                                conversation_id,
                                EntryDraft {
                                    kind: USER_ENTRY.kind().to_string(),
                                    model: Some(vec![Message::User(user)]),
                                    data: None,
                                    head: None,
                                    edits: None,
                                },
                            )
                            .await?;
                            let next = create_generation_task_id(
                                tx,
                                conversation_id,
                                generation_task.as_ref(),
                            )
                            .await?;
                            hand_over(&live, runtime.task_id(), next).map_err(session_error)?;
                            live.delete(vec![PathSegment::Key("generation".to_string())])
                                .map_err(session_error)?;
                            return Ok(Some(result));
                        }
                        end_run(
                            tx,
                            &live,
                            runtime.task_id(),
                            SubmissionSettlement::Done { answer: entry.id },
                        )
                        .map_err(session_error)?;
                        Ok(Some(result))
                    })
                }),
                Arc::clone(&context),
            )
            .await
    }

    /// 对应 `startToolRound(...)`。
    async fn start_tool_round(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        request: Request,
        message: AssistantMessage,
        calls: &[ToolCall],
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let conversation_id = runtime.conversation_id();
        let messages = match &request.messages {
            Some(messages) => messages.clone(),
            None => {
                runtime
                    .context_view(conversation_id, Arc::clone(&context), Some(request.cutoff))
                    .await?
                    .messages
            }
        };
        let offered: std::collections::BTreeSet<String> = get_current_tools(&messages)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let tools = runtime.agent(Arc::clone(&context)).await?.tools;
        let sequential = runtime.settings().tool_execution == ToolExecutionMode::Sequential
            || calls.iter().any(|call| {
                offered.contains(&call.name)
                    && tools
                        .iter()
                        .find(|tool| tool.name() == call.name)
                        .is_some_and(|tool| {
                            tool.execution_mode() == Some(ToolExecutionMode::Sequential)
                        })
            });
        let tool_task = Arc::clone(&self.tool_task);
        let calls_vec = calls.to_vec();
        let offered_set = offered.clone();
        let runtime_for_commit = Arc::clone(runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_commit);
                    let tool_task = Arc::clone(&tool_task);
                    let calls = calls_vec.clone();
                    let offered = offered_set.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        let entry = Self::append_assistant_static(
                            tx,
                            runtime.conversation_id(),
                            message.clone(),
                        )
                        .await?;
                        let mut slots: Vec<ToolSlot> = Vec::new();
                        let mut task_ids: Vec<TaskId> = Vec::new();
                        let mut pending: Vec<String> = Vec::new();
                        for call in &calls {
                            if !offered.contains(&call.name) {
                                let unavailable = harness_error(
                                    "tool_unavailable",
                                    &format!("Tool {} is not available", call.name),
                                );
                                let result = append_tool_result(
                                    tx,
                                    runtime.conversation_id(),
                                    call,
                                    &unavailable,
                                    runtime.now(),
                                    None,
                                )
                                .await?;
                                slots.push(ToolSlot {
                                    call_id: call.id.clone(),
                                    name: call.name.clone(),
                                    task_id: None,
                                    status: crate::harness::live::ToolSlotStatus::Done,
                                    output: None,
                                    dropped_bytes: None,
                                    dropped_lines: None,
                                    details: None,
                                    diagnostics: None,
                                    entry: Some(result.id),
                                });
                                continue;
                            }
                            if sequential && !task_ids.is_empty() {
                                pending.push(call.id.clone());
                                slots.push(ToolSlot {
                                    call_id: call.id.clone(),
                                    name: call.name.clone(),
                                    task_id: None,
                                    status: crate::harness::live::ToolSlotStatus::Pending,
                                    output: None,
                                    dropped_bytes: None,
                                    dropped_lines: None,
                                    details: None,
                                    diagnostics: None,
                                    entry: None,
                                });
                                continue;
                            }
                            let input = ToolTaskInput {
                                assistant: entry.id,
                                call_id: call.id.clone(),
                            };
                            let task_id = tx
                                .create_task(
                                    &tool_task,
                                    serde_json::to_value(&input).map_err(session_error)?,
                                    TaskOptions {
                                        ownership: TaskOwnership::Task {
                                            task_id: runtime.task_id(),
                                        },
                                        conversation_id: None,
                                        background: None,
                                    },
                                )
                                .await?;
                            task_ids.push(task_id);
                            slots.push(ToolSlot {
                                call_id: call.id.clone(),
                                name: call.name.clone(),
                                task_id: Some(task_id),
                                status: crate::harness::live::ToolSlotStatus::Pending,
                                output: None,
                                dropped_bytes: None,
                                dropped_lines: None,
                                details: None,
                                diagnostics: None,
                                entry: None,
                            });
                        }
                        live.delete(vec![PathSegment::Key("generation".to_string())])
                            .map_err(session_error)?;
                        live.set(
                            vec![PathSegment::Key("tools".to_string())],
                            serde_json::to_value(&slots).map_err(session_error)?,
                        )
                        .map_err(session_error)?;
                        let checkpoint = GenerationCheckpoint::Tools {
                            assistant: entry.id,
                            tools: task_ids.clone(),
                            pending,
                        };
                        Ok(Some(NextTaskState::Waiting {
                            checkpoint: serde_json::to_value(&checkpoint).map_err(session_error)?,
                            on: task_ids,
                            policy: crate::types::JoinPolicy::AllSettled,
                        }))
                    })
                }),
                Arc::clone(&context),
            )
            .await
    }

    /// 对应 `finishToolRound(...)`。
    async fn finish_tool_round(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        assistant: EntryId,
        tools: &[TaskId],
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let conversation_id = runtime.conversation_id();
        let outcomes = runtime.outcomes(tools, Arc::clone(&context)).await?;
        let controls: std::collections::BTreeMap<
            TaskId,
            Option<crate::harness::types::ToolControl>,
        > = tools
            .iter()
            .zip(outcomes.iter())
            .map(|(id, outcome)| {
                let control = match outcome {
                    TaskOutcome::Completed { result } => {
                        serde_json::from_value::<ToolTaskResult>(result.clone())
                            .ok()
                            .and_then(|result| result.control)
                    }
                    _ => None,
                };
                (*id, control)
            })
            .collect();
        let live_value = runtime
            .snapshot(
                &*LIVE_DOC,
                Some(conversation_id.get()),
                None,
                Arc::clone(&context),
            )
            .await?;
        let state: LiveState = live_value
            .map(|value| serde_json::from_value(JsonValue::Object(value)).unwrap_or_default())
            .unwrap_or_default();
        let slots = state.tools.unwrap_or_default();
        let results: Vec<EntryId> = slots.iter().filter_map(|slot| slot.entry).collect();
        for handlers in runtime.hooks().handlers() {
            if let HookHandlers::Generation(hook) = handlers.as_ref() {
                let api: Arc<dyn HookApi> = Arc::new(RuntimeHookApi::new(Arc::clone(runtime)));
                hook.after_tools(assistant, &results, api, Arc::clone(&context))
                    .await?;
            }
        }
        let terminate = !slots.is_empty()
            && slots.iter().all(|slot| {
                slot.task_id.is_some_and(|task_id| {
                    controls
                        .get(&task_id)
                        .and_then(|control| control.as_ref())
                        .and_then(|control| control.terminate)
                        == Some(true)
                })
            });
        let added: Vec<String> = controls
            .values()
            .filter_map(|control| control.as_ref())
            .flat_map(|control| control.add_tools.clone().unwrap_or_default())
            .collect();
        let handoff = controls
            .values()
            .filter_map(|control| control.as_ref())
            .filter_map(|control| control.handoff.clone())
            .next_back();
        let runtime_for_commit = Arc::clone(runtime);
        let start_run = Arc::clone(&self.start_run);
        let generation_task = Arc::clone(&self.generation_task);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_commit);
                    let start_run = Arc::clone(&start_run);
                    let generation_task = Arc::clone(&generation_task);
                    Box::pin(async move {
                        let mut boundary = prepare_boundary(
                            tx,
                            conversation_id,
                            QueueModes::from_settings(&runtime.settings()),
                        )
                        .await?;
                        if !added.is_empty() {
                            add_tools(tx, conversation_id, &added).await?;
                        }
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(conversation_id), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        let now = runtime.now();
                        if terminate || handoff.is_some() {
                            if let Some(handoff) = handoff.clone() {
                                let user = UserMessage {
                                    content: UserContent::Text(handoff),
                                    timestamp: now,
                                };
                                let entry = tx
                                    .append_entry(
                                        Some(RESET_ENTRY.kind().to_string()),
                                        conversation_id,
                                        EntryDraft {
                                            kind: RESET_ENTRY.kind().to_string(),
                                            head: Some(EntryHead::SelfEntry),
                                            model: Some(vec![Message::User(user)]),
                                            data: None,
                                            edits: None,
                                        },
                                    )
                                    .await?;
                                boundary.head = Some(entry.id);
                            }
                            let boundary_result =
                                apply_boundary(tx, &mut boundary, BoundaryAt::Final, now).await?;
                            end_run(
                                tx,
                                &live,
                                runtime.task_id(),
                                SubmissionSettlement::Done { answer: assistant },
                            )
                            .map_err(session_error)?;
                            if !boundary_result.users.is_empty() {
                                start_run(tx, conversation_id, &live, boundary_result.users)
                                    .await?;
                            }
                        } else {
                            let boundary_result =
                                apply_boundary(tx, &mut boundary, BoundaryAt::PostTools, now)
                                    .await?;
                            if boundary_result.reset {
                                end_run(
                                    tx,
                                    &live,
                                    runtime.task_id(),
                                    SubmissionSettlement::Unanswered {
                                        reason: "reset".to_string(),
                                        detail: None,
                                    },
                                )
                                .map_err(session_error)?;
                                if !boundary_result.users.is_empty() {
                                    start_run(tx, conversation_id, &live, boundary_result.users)
                                        .await?;
                                }
                            } else {
                                live.delete(vec![PathSegment::Key("tools".to_string())])
                                    .map_err(session_error)?;
                                let state = live_state(&live);
                                if state
                                    .run
                                    .as_ref()
                                    .is_some_and(|run| run.task_id == runtime.task_id())
                                {
                                    let mut run = state.run.expect("run");
                                    run.inputs.extend(boundary_result.users.clone());
                                    live.set(
                                        vec![PathSegment::Key("run".to_string())],
                                        serde_json::to_value(&run).map_err(session_error)?,
                                    )
                                    .map_err(session_error)?;
                                }
                                let next = create_generation_task_id(
                                    tx,
                                    conversation_id,
                                    generation_task.as_ref(),
                                )
                                .await?;
                                hand_over(&live, runtime.task_id(), next).map_err(session_error)?;
                            }
                        }
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Completed {
                                result: serde_json::to_value(GenerationResult {
                                    entry_id: assistant,
                                })
                                .map_err(session_error)?,
                            },
                        }))
                    })
                }),
                Arc::clone(&context),
            )
            .await
    }
}

/// 静态桥接：`convert_partial` 需要 `&self`，这里提供无 self 的版本。
impl GenerationTaskDefinition {
    async fn convert_partial_static(
        tx: &Transaction,
        live: &Draft,
        conversation_id: ConversationId,
    ) -> Result<(), SessionError> {
        convert_partial(tx, live, conversation_id).await
    }

    async fn append_assistant_static(
        tx: &Transaction,
        conversation_id: ConversationId,
        message: AssistantMessage,
    ) -> Result<crate::types::EntryRecord, SessionError> {
        record_usage(
            tx,
            conversation_id,
            UsageBucket::Models,
            &format!("{}/{}", message.provider, message.model),
            &message.usage,
        )
        .await?;
        tx.append_entry(
            None,
            conversation_id,
            EntryDraft {
                kind: ASSISTANT_ENTRY.kind().to_string(),
                model: Some(vec![Message::Assistant(message)]),
                data: None,
                head: None,
                edits: None,
            },
        )
        .await
    }
}

/// 对应 `appendAssistant` 的 free 函数。
async fn append_assistant(
    tx: &Transaction,
    conversation_id: ConversationId,
    message: AssistantMessage,
) -> Result<crate::types::EntryRecord, SessionError> {
    record_usage(
        tx,
        conversation_id,
        UsageBucket::Models,
        &format!("{}/{}", message.provider, message.model),
        &message.usage,
    )
    .await?;
    tx.append_entry(
        None,
        conversation_id,
        EntryDraft {
            kind: ASSISTANT_ENTRY.kind().to_string(),
            model: Some(vec![Message::Assistant(message)]),
            data: None,
            head: None,
            edits: None,
        },
    )
    .await
}

/// 对应 `convertPartial`：把中断/中止/故障/孤儿化尝试留下的已提交 partial 追加为 aborted assistant 条目；
/// 调用方随后替换或移除 `generation`。
pub async fn convert_partial(
    tx: &Transaction,
    live: &Draft,
    conversation_id: ConversationId,
) -> Result<(), SessionError> {
    let state = live_state(live);
    let Some(partial) = state.generation.and_then(|g| g.message) else {
        return Ok(());
    };
    let mut message = partial;
    message.stop_reason = pi_ai::StopReason::Aborted;
    append_assistant(tx, conversation_id, message).await?;
    Ok(())
}

/// 对应 `createGeneration` 的 task id 版（answer/finishToolRound 用）。
async fn create_generation_task_id(
    tx: &Transaction,
    conversation_id: ConversationId,
    generation_task: &(dyn Fn() -> Arc<Task> + Send + Sync),
) -> Result<TaskId, SessionError> {
    let task = generation_task();
    create_generation(tx, conversation_id, &task).await
}

/// 对应 `readCalls(runtime, assistant, callIds, context)`：assistant 条目里 `callIds` 的调用。
async fn read_calls(
    runtime: &Arc<dyn TaskRuntime>,
    assistant: EntryId,
    call_ids: &[String],
    context: Arc<dyn Context>,
) -> Result<Vec<ToolCall>, SessionError> {
    let entry = runtime.entry(assistant, context).await?;
    let message = entry.and_then(|entry| entry.model.into_iter().flatten().next());
    let calls = match message {
        Some(Message::Assistant(assistant)) => assistant
            .content
            .into_iter()
            .filter_map(|content| match content {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    Ok(call_ids
        .iter()
        .filter_map(|id| calls.iter().find(|call| &call.id == id).cloned())
        .collect())
}

#[async_trait::async_trait]
impl TaskDefinitionSpec for GenerationTaskDefinition {
    fn name(&self) -> &str {
        "pi.generation"
    }

    fn version(&self) -> u32 {
        1
    }

    fn initial(&self, _input: &JsonValue) -> JsonValue {
        serde_json::to_value(GenerationCheckpoint::Prepare {
            attempt: 1,
            compacted: None,
            overflow: None,
        })
        .expect("prepare serialises")
    }

    fn phases(&self) -> &[&'static str] {
        &["prepare", "request", "retry", "poll", "tools"]
    }

    async fn run_phase(
        &self,
        phase: &str,
        task: RunningTask,
        runtime: Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        match phase {
            "prepare" => self.run_prepare(&task, &runtime, context).await,
            "request" => self.run_request(&task, &runtime, context).await,
            "retry" => self.run_retry(&task, &runtime, context).await,
            "poll" => self.run_poll(&task, &runtime, context).await,
            "tools" => self.run_tools(&task, &runtime, context).await,
            _ => Err(SessionError::Message(format!(
                "Task pi.generation has no phase handler for {phase}"
            ))),
        }
    }

    async fn abort(
        &self,
        task: RunningTask,
        runtime: Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let checkpoint: GenerationCheckpoint =
            serde_json::from_value(checkpoint_json(&task)).map_err(session_error)?;
        if let GenerationCheckpoint::Poll {
            model: reference,
            handle,
            ..
        } = &checkpoint
            && let Some(model) = runtime
                .models()
                .get_model(&reference.provider, &reference.model_id)
        {
            match runtime
                .models()
                .cancel_deferred(
                    &model,
                    handle,
                    Some(&pi_ai::DeferredCancelOptions {
                        signal: Some(runtime.signal()),
                        api_key: None,
                        headers: None,
                        timeout_ms: None,
                        max_retries: None,
                        max_retry_delay_ms: None,
                        on_payload: None,
                        on_response: None,
                    }),
                )
                .await
            {
                Ok(()) => {}
                Err(error) => runtime.report(SessionError::Message(error)),
            }
        }
        let _conversation_id = runtime.conversation_id();
        let unstarted = if let GenerationCheckpoint::Tools {
            assistant, pending, ..
        } = &checkpoint
        {
            read_calls(&runtime, *assistant, pending, Arc::clone(&context)).await?
        } else {
            Vec::new()
        };
        let runtime_for_commit = Arc::clone(&runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_commit);
                    let unstarted = unstarted.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        Self::convert_partial_static(tx, &live, runtime.conversation_id()).await?;
                        for call in &unstarted {
                            let result = harness_error(
                                "aborted",
                                &format!("Tool {} was aborted", call.name),
                            );
                            append_tool_result(
                                tx,
                                runtime.conversation_id(),
                                call,
                                &result,
                                runtime.now(),
                                None,
                            )
                            .await?;
                        }
                        end_run(
                            tx,
                            &live,
                            runtime.task_id(),
                            SubmissionSettlement::Unanswered {
                                reason: "aborted".to_string(),
                                detail: None,
                            },
                        )
                        .map_err(session_error)?;
                        Ok(Some(NextTaskState::Terminal {
                            outcome: TaskOutcome::Aborted {
                                reason: None,
                                result: None,
                            },
                        }))
                    })
                }),
                context,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::live::LIVE_DOC;
    use crate::session::{SessionImpl, create_session};
    use crate::storage::MemoryStorage;
    use crate::types::{DocAccess, Storage, TaskDefinitionSpec};
    use pi_ai::UserContent;
    use serde_json::json;

    fn context() -> Arc<dyn crate::chord::context::Context> {
        Arc::new(crate::chord::context::EmptyContext::new("[test]"))
    }

    struct Generation;

    impl TaskDefinitionSpec for Generation {
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

    async fn open() -> (Arc<SessionImpl>, Arc<MemoryStorage>) {
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

    fn access() -> DocAccess {
        DocAccess {
            owner: Some(1),
            key: None,
        }
    }

    #[tokio::test]
    async fn start_run_creates_a_generation_and_writes_the_run() {
        let (session, _storage) = open().await;
        let start_run = make_start_run(Arc::new(move || Arc::new(Task::new(Arc::new(Generation)))));

        let state = session
            .commit(
                |tx| {
                    Box::pin(async move {
                        let live = tx.doc(&*LIVE_DOC, access(), None).await?;
                        start_run(
                            tx,
                            ConversationId::new(1),
                            &live,
                            vec![crate::types::SubmissionId::new(5)],
                        )
                        .await?;
                        Ok(crate::harness::live::live_state(&live))
                    })
                },
                context(),
            )
            .await
            .expect("start run");

        let run = state.run.expect("run 已写入");
        assert_eq!(run.inputs, vec![crate::types::SubmissionId::new(5)]);
        assert!(run.task_id.get() > 0);

        // 任务确实被持久化，且由该会话拥有（对应上游 `ownership: { kind: "conversation" }`）。
        // 另起一次提交读取：事务不允许「写后读」。
        let task_id = run.task_id.get();
        let record = session
            .commit(
                |tx| {
                    Box::pin(async move {
                        Ok(tx
                            .task(crate::types::TaskId::new(task_id))
                            .await?
                            .expect("任务已持久化"))
                    })
                },
                context(),
            )
            .await
            .expect("read task");
        assert_eq!(record.id.get(), task_id);
        assert_eq!(record.kind, "pi.generation");
        assert_eq!(record.conversation_id, ConversationId::new(1));
        assert_eq!(record.owner, None, "会话拥有的任务没有父任务");
    }

    #[tokio::test]
    async fn hand_over_moves_the_run_only_when_it_matches() {
        let (session, _storage) = open().await;
        let (mismatched, matched) = session
            .commit(
                |tx| {
                    Box::pin(async move {
                        let live = tx.doc(&*LIVE_DOC, access(), None).await?;
                        live.set(
                            vec![PathSegment::Key("run".to_string())],
                            json!({"taskId": 3, "inputs": [7]}),
                        )
                        .map_err(|error| SessionError::Message(error.to_string()))?;

                        // 不匹配的 from：什么都不做。
                        hand_over(
                            &live,
                            crate::types::TaskId::new(9),
                            crate::types::TaskId::new(11),
                        )
                        .expect("no-op handover");
                        let mismatched = crate::harness::live::live_state(&live)
                            .run
                            .expect("run")
                            .task_id;

                        // 匹配：taskId 换掉，inputs 保留。
                        hand_over(
                            &live,
                            crate::types::TaskId::new(3),
                            crate::types::TaskId::new(11),
                        )
                        .expect("handover");
                        let matched = crate::harness::live::live_state(&live).run.expect("run");

                        Ok((mismatched, matched))
                    })
                },
                context(),
            )
            .await
            .expect("handover");

        assert_eq!(mismatched, crate::types::TaskId::new(3));
        assert_eq!(matched.task_id, crate::types::TaskId::new(11));
        assert_eq!(matched.inputs, vec![crate::types::SubmissionId::new(7)]);
    }

    fn user_message(text: &str) -> Message {
        Message::User(pi_ai::UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    fn system_draft(text: &str) -> EntryDraft {
        EntryDraft {
            kind: "pi.system".to_string(),
            model: Some(vec![user_message(text)]),
            data: None,
            head: None,
            edits: None,
        }
    }

    /// 两条非空贡献：`selectCut` 能找到切点。
    fn cuttable_view() -> ContextView {
        ContextView {
            head: None,
            entries: Vec::new(),
            contributions: vec![vec![user_message("old")], vec![user_message("new")]],
            messages: Vec::new(),
        }
    }

    fn policy() -> CompactionPolicy {
        CompactionPolicy {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 0,
            background_tokens: 50,
        }
    }

    #[test]
    fn threshold_compaction_picks_the_crossed_threshold() {
        let view = cuttable_view();
        let planned = vec![system_draft("hello world")];
        let extra = vec![user_message("hello world")];
        let tokens = i128::from(estimate_context(&view, &extra));

        // 越过阻塞阈值：tokens > window - reserveTokens。
        let window = u64::try_from(tokens + 90).expect("window");
        assert_eq!(
            threshold_compaction(&view, &planned, window, policy()),
            Some(ThresholdOver::Blocking)
        );

        // 只越过后台阈值（比阻塞阈值低 50）：background < tokens <= blocking。
        let window = u64::try_from(tokens + 110).expect("window");
        assert_eq!(
            threshold_compaction(&view, &planned, window, policy()),
            Some(ThresholdOver::Background)
        );

        // 两道阈值都没越过。
        let window = u64::try_from(tokens + 200).expect("window");
        assert_eq!(
            threshold_compaction(&view, &planned, window, policy()),
            None
        );
    }

    #[test]
    fn threshold_compaction_needs_a_cut_and_an_enabled_policy() {
        let planned = vec![system_draft("hello world")];
        let extra = vec![user_message("hello world")];
        let tokens = i128::from(estimate_context(&cuttable_view(), &extra));
        let window = u64::try_from(tokens + 90).expect("window");

        // 阈值越过，但候选里找不到切点（只有一条贡献）——上游同样不启动压缩。
        let one_entry = ContextView {
            head: None,
            entries: Vec::new(),
            contributions: vec![vec![user_message("only")]],
            messages: Vec::new(),
        };
        assert_eq!(
            threshold_compaction(&one_entry, &planned, window, policy()),
            None
        );

        // 策略关闭。
        let disabled = CompactionPolicy {
            enabled: false,
            ..policy()
        };
        assert_eq!(
            threshold_compaction(&cuttable_view(), &planned, window, disabled),
            None
        );

        // 上下文窗口未知。
        assert_eq!(
            threshold_compaction(&cuttable_view(), &planned, 0, policy()),
            None
        );

        // 后台阈值 0 表示禁用后台压缩：只越过它不算数。
        let no_background = CompactionPolicy {
            background_tokens: 0,
            ..policy()
        };
        let window = u64::try_from(tokens + 110).expect("window");
        assert_eq!(
            threshold_compaction(&cuttable_view(), &planned, window, no_background),
            None
        );
    }

    #[test]
    fn generation_task_metadata_matches_upstream() {
        let start_run: StartRun =
            Arc::new(|_tx, _cid, _live, _inputs| Box::pin(async move { Ok(()) }));
        let create_compaction: CreateCompaction = Arc::new(|_tx, _cid, _input, _owner| {
            Box::pin(async move { Ok(crate::types::TaskId::new(1)) })
        });
        let tool_task = crate::harness::tool::make_tool_task();
        let generation_task = make_generation_task(
            start_run,
            create_compaction,
            tool_task,
            Arc::new(move || Arc::new(Task::new(Arc::new(Generation)))),
        );
        assert_eq!(generation_task.definition().name(), "pi.generation");
        assert_eq!(generation_task.definition().version(), 1);
        assert_eq!(
            generation_task.definition().phases(),
            &["prepare", "request", "retry", "poll", "tools"]
        );
        assert_eq!(
            generation_task.definition().initial(&JsonValue::Null),
            json!({"phase": "prepare", "attempt": 1})
        );
    }
}
