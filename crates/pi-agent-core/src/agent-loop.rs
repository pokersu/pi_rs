//! Rust 翻译自 packages/agent/src/agent-loop.ts
//!
//! 底层 agent 循环：始终以 `AgentMessage` 工作，仅在 LLM 调用边界转换为 `Message[]`。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::{FutureExt, StreamExt};
use pi_ai::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, ContentBlock, Context, EventStream,
    StopReason, SystemContent, SystemMessage, TextContent, TextKind, TextOrImageContent, Tool,
    ToolCall, ToolResultMessage, ToolStateChanges, create_initial_system_message,
    get_current_tools, get_tool_state_changes, to_tool_declaration, validate_tool_arguments,
};

use crate::types::{
    AfterToolCallContext, AfterToolCallFn, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage,
    AgentTool, AgentToolResult, AgentTurnContext, AgentTurnDecision, BeforeToolCallContext,
    BeforeToolCallFn, PrepareRequestContext, StreamFn, ToolCallHooks, ToolExecutionMode,
    ToolUpdateSink,
};

/// 对应 `AgentEventSink`
pub type AgentEventSink =
    Arc<dyn Fn(AgentEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// 对应 `ExecutedToolCallBatch`
struct ExecutedToolCallBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

/// 对应 `PreparedToolCall` / `ImmediateToolCallOutcome`
enum PreparedToolCall {
    Prepared {
        tool_call: ToolCall,
        tool: AgentTool,
        args: serde_json::Value,
    },
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
}

/// 对应 `AgentToolCallOutcome`（上游 types.ts 公开接口）。
#[derive(Clone)]
pub struct AgentToolCallOutcome {
    pub tool_call: ToolCall,
    pub result: AgentToolResult,
    pub is_error: bool,
    /// 工具执行耗时（毫秒）；立即解绑（未真正执行）时为 `None`。
    pub duration_ms: Option<u64>,
}

/// 对应 `FinalizedToolCallOutcome`（上游 agent-loop.ts 私有别名）。
type FinalizedToolCallOutcome = AgentToolCallOutcome;

/// 对应 `ExecutedToolCallOutcome`：工具已执行但尚未跑 `afterToolCall` 钩子。
struct ExecutedToolCallOutcome {
    result: AgentToolResult,
    is_error: bool,
    duration_ms: u64,
}

/// 对应 `createAgentStream`
fn create_agent_stream() -> EventStream<AgentEvent, Vec<AgentMessage>> {
    EventStream::new(
        |event| matches!(event, AgentEvent::AgentEnd { .. }),
        |event| match event {
            AgentEvent::AgentEnd { messages } => messages.clone(),
            _ => panic!("Unexpected event type for final result"),
        },
    )
}

/// 对应 `agentLoop`：以新的 prompt 启动一个 agent 循环。
pub fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<AbortSignal>,
    stream_fn: StreamFn,
) -> EventStream<AgentEvent, Vec<AgentMessage>> {
    let stream = create_agent_stream();
    let producer = stream.clone();
    tokio::spawn(async move {
        let emit: AgentEventSink = Arc::new({
            let producer = producer.clone();
            move |event| {
                let producer = producer.clone();
                Box::pin(async move { producer.push(event) })
            }
        });
        let messages = run_agent_loop(prompts, context, config, signal, emit, stream_fn).await;
        producer.end(Some(messages));
    });
    stream
}

/// 对应 `agentLoopContinue`：从当前上下文继续，不新增消息。
pub fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<AbortSignal>,
    stream_fn: StreamFn,
) -> EventStream<AgentEvent, Vec<AgentMessage>> {
    if context.messages.is_empty() {
        panic!("Cannot continue: no messages in context");
    }
    if matches!(context.messages.last(), Some(AgentMessage::Assistant(_))) {
        panic!("Cannot continue from message role: assistant");
    }

    let stream = create_agent_stream();
    let producer = stream.clone();
    tokio::spawn(async move {
        let emit: AgentEventSink = Arc::new({
            let producer = producer.clone();
            move |event| {
                let producer = producer.clone();
                Box::pin(async move { producer.push(event) })
            }
        });
        let messages = run_agent_loop_continue(context, config, signal, emit, stream_fn).await;
        producer.end(Some(messages));
    });
    stream
}

/// 取上下文里可执行工具的 LLM 层声明（`AgentTool` → `Tool`）。
fn executable_tools(context: &AgentContext) -> Vec<Tool> {
    context
        .tools
        .as_ref()
        .map(|tools| tools.iter().map(|tool| tool.tool.clone()).collect())
        .unwrap_or_default()
}

/// 取消息列表中 system 消息声明的当前工具集（对应 `getCurrentTools`）。
///
/// 上游的 helper 接受「任意带 role 的消息列表」；Rust 侧先抽出 system 消息再交给 transcript 函数，
/// 因为那些函数只读 `role == "system"` 的项（自定义角色消息不影响结果）。
fn declared_tools(messages: &[AgentMessage]) -> Vec<Tool> {
    let system_messages: Vec<pi_ai::Message> = messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::System(system) => Some(pi_ai::Message::System(system.clone())),
            _ => None,
        })
        .collect();
    get_current_tools(&system_messages)
}

/// 对应 `withToolChanges`：复制 system 消息，工具字段替换为 `changes`（空列表则省略字段）。
fn with_tool_changes(message: &SystemMessage, changes: &ToolStateChanges) -> SystemMessage {
    SystemMessage {
        content: message.content.clone(),
        sections: message.sections.clone(),
        tools_added: if changes.tools_added.is_empty() {
            None
        } else {
            Some(changes.tools_added.clone())
        },
        tools_removed: if changes.tools_removed.is_empty() {
            None
        } else {
            Some(changes.tools_removed.clone())
        },
        timestamp: message.timestamp,
    }
}

/// 对应 `declareToolChanges`：把「可执行工具集」与 transcript 已声明工具的差异声明给模型。
///
/// `executable` 是运行时能执行的工具；transcript 的 system 消息声明模型可以调用的工具。
/// 每次请求前两者之差成为 system 消息上的 `toolsAdded` / `toolsRemoved`，保证重放 transcript
/// 后的工具集恰好等于 `executable`。若 pending 已存在 system 消息，其工具字段被当作「意图」，
/// 用「已提交 transcript 与可执行集之差」替换；否则在首个非 system 消息前插入新 system 消息。
fn declare_tool_changes(
    executable: &[Tool],
    committed: &[AgentMessage],
    pending_messages: Vec<AgentMessage>,
) -> Vec<AgentMessage> {
    let system_index = pending_messages
        .iter()
        .rposition(|message| matches!(message, AgentMessage::System(_)));
    let pending_system = system_index.and_then(|index| match &pending_messages[index] {
        AgentMessage::System(system) => Some(system.clone()),
        _ => None,
    });

    // pending 的 system 消息若带工具字段，先视为意图清空，再参与基线计算。
    let baseline: Vec<AgentMessage> = match (system_index, &pending_system) {
        (Some(index), Some(system)) => {
            let no_changes = ToolStateChanges {
                tools_added: Vec::new(),
                tools_removed: Vec::new(),
            };
            let mut cleared = pending_messages.clone();
            cleared[index] = AgentMessage::System(with_tool_changes(system, &no_changes));
            cleared
        }
        _ => pending_messages.clone(),
    };

    let mut declared_sources: Vec<AgentMessage> = committed.to_vec();
    declared_sources.extend(baseline.iter().cloned());
    let current: Vec<Tool> = executable.iter().map(to_tool_declaration).collect();
    let changes = get_tool_state_changes(&declared_tools(&declared_sources), &current);
    let unchanged = changes.tools_added.is_empty() && changes.tools_removed.is_empty();

    if let (Some(index), Some(system)) = (system_index, &pending_system) {
        let pending_has_tools = system.tools_added.as_ref().is_some_and(|t| !t.is_empty())
            || system.tools_removed.as_ref().is_some_and(|t| !t.is_empty());
        if unchanged && !pending_has_tools {
            return pending_messages;
        }
        let mut updated = baseline;
        updated[index] = AgentMessage::System(with_tool_changes(system, &changes));
        return updated;
    }

    if unchanged {
        return pending_messages;
    }

    let update = with_tool_changes(
        &SystemMessage {
            content: SystemContent::Text(String::new()),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp: pi_ai::utils::uuid::now_ms() as u64,
        },
        &changes,
    );
    let insert_index = pending_messages
        .iter()
        .position(|message| !matches!(message, AgentMessage::System(_)))
        .unwrap_or(pending_messages.len());
    let mut result = pending_messages;
    result.insert(insert_index, AgentMessage::System(update));
    result
}

/// 把 `context.system_prompt` / `context.tools` 折叠成首条 system 消息。
///
/// 对应上游在 agent 层表达的 `createInitialSystemMessage` + `normalizeContext`：
/// 折叠后 `system_prompt` 置空，使 provider 侧不会重复生成首条 system 消息。
fn fold_initial_system_message(context: &mut AgentContext) {
    if matches!(context.messages.first(), Some(AgentMessage::System(_))) {
        return;
    }
    let tools = executable_tools(context);
    let system_prompt = if context.system_prompt.is_empty() {
        None
    } else {
        Some(context.system_prompt.as_str())
    };
    let Some(initial) = create_initial_system_message(system_prompt, Some(&tools)) else {
        return;
    };
    context.messages.insert(0, AgentMessage::System(initial));
    context.system_prompt.clear();
}

/// 对应 `runAgentLoop`
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<AbortSignal>,
    emit: AgentEventSink,
    stream_fn: StreamFn,
) -> Vec<AgentMessage> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    let mut current_context = AgentContext {
        system_prompt: context.system_prompt.clone(),
        messages: [context.messages.clone(), prompts.clone()].concat(),
        tools: context.tools.clone(),
    };
    // 把 systemPrompt/tools 折叠进 transcript（首条 system 消息）。
    fold_initial_system_message(&mut current_context);

    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;
    for prompt in &prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        })
        .await;
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        })
        .await;
    }

    run_loop(
        &mut current_context,
        &mut new_messages,
        config,
        signal,
        emit,
        stream_fn,
    )
    .await;
    new_messages
}

/// 对应 `runAgentLoopContinue`
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    signal: Option<AbortSignal>,
    emit: AgentEventSink,
    stream_fn: StreamFn,
) -> Vec<AgentMessage> {
    if context.messages.is_empty() {
        panic!("Cannot continue: no messages in context");
    }
    if matches!(context.messages.last(), Some(AgentMessage::Assistant(_))) {
        panic!("Cannot continue from message role: assistant");
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    let mut current_context = context;

    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;

    run_loop(
        &mut current_context,
        &mut new_messages,
        config,
        signal,
        emit,
        stream_fn,
    )
    .await;
    new_messages
}

/// 对应 `runLoop`（主循环逻辑）。
async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    signal: Option<AbortSignal>,
    emit: AgentEventSink,
    stream_fn: StreamFn,
) {
    let mut last_completed_turn: Option<AgentTurnContext> = None;
    let mut pending_messages: Vec<AgentMessage> = Vec::new();

    // 外层循环：agent 本应停止时，若出现 follow-up 消息则继续。
    loop {
        let mut has_more_tool_calls = true;

        // 内层循环：处理 tool calls 与 steering 消息。
        while has_more_tool_calls || !pending_messages.is_empty() {
            if let Some(turn) = &last_completed_turn {
                let next_turn_snapshot = match &config.prepare_next_turn {
                    Some(f) => f(turn).await,
                    None => None,
                };
                if let Some(snapshot) = next_turn_snapshot {
                    if let Some(ctx) = snapshot.context {
                        *current_context = ctx;
                    }
                    if let Some(model) = snapshot.model {
                        config.model = model;
                    }
                    if let Some(level) = snapshot.thinking_level {
                        config.stream.reasoning = crate::types::to_ai_thinking_level(level);
                    }
                }
                // prepareNextTurn 可能长运行（例如 compaction），期间排队的 steering 消息也要拾取。
                if pending_messages.is_empty() {
                    pending_messages = match &config.get_steering_messages {
                        Some(f) => f().await,
                        None => Vec::new(),
                    };
                }
                emit(AgentEvent::TurnStart).await;
            }

            // 注入 pending 消息（先声明工具装载变化）。
            if !pending_messages.is_empty() {
                let drained: Vec<AgentMessage> = std::mem::take(&mut pending_messages);
                let executable = executable_tools(current_context);
                let declared =
                    declare_tool_changes(&executable, &current_context.messages, drained);
                for message in declared {
                    emit(AgentEvent::MessageStart {
                        message: message.clone(),
                    })
                    .await;
                    emit(AgentEvent::MessageEnd {
                        message: message.clone(),
                    })
                    .await;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            // 每次 provider 请求前（含首次）应用 `prepareRequest`。
            if let Some(prepare) = &config.prepare_request {
                let request = PrepareRequestContext {
                    context: current_context.clone(),
                    model: config.model.clone(),
                    thinking_level: config.stream.reasoning,
                };
                if let Some(update) = prepare(&request, signal.clone()).await {
                    if let Some(context) = update.context {
                        *current_context = context;
                    }
                    if let Some(model) = update.model {
                        config.model = model;
                    }
                    if let Some(level) = update.thinking_level {
                        config.stream.reasoning = crate::types::to_ai_thinking_level(level);
                    }
                }
            }

            // 流式 assistant 响应。
            let message = stream_assistant_response(
                current_context,
                &config,
                signal.as_ref(),
                &emit,
                &stream_fn,
            )
            .await;
            new_messages.push(AgentMessage::Assistant(message.clone()));

            if message.stop_reason == StopReason::Error
                || message.stop_reason == StopReason::Aborted
            {
                emit(AgentEvent::TurnEnd {
                    message: AgentMessage::Assistant(message.clone()),
                    tool_results: Vec::new(),
                })
                .await;
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return;
            }

            // 提取 tool calls。
            let tool_calls: Vec<ToolCall> = message
                .content
                .iter()
                .filter_map(|c| match c {
                    ContentBlock::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let executed_batch = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&tool_calls, &emit).await
                } else {
                    execute_tool_calls(current_context, &message, &config, signal.as_ref(), &emit)
                        .await
                };
                tool_results.extend(executed_batch.messages);
                has_more_tool_calls = !executed_batch.terminate;

                for result in &tool_results {
                    current_context
                        .messages
                        .push(AgentMessage::ToolResult(result.clone()));
                    new_messages.push(AgentMessage::ToolResult(result.clone()));
                }
            }

            last_completed_turn = Some(AgentTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: current_context.clone(),
                new_messages: new_messages.clone(),
            });

            // `finishTurn` 在 assistant 与工具结果 finalize 后、`turn_end` 之前运行。
            let turn_decision = match (&config.finish_turn, &last_completed_turn) {
                (Some(finish), Some(turn)) => finish(turn, signal.clone()).await,
                _ => None,
            };

            emit(AgentEvent::TurnEnd {
                message: AgentMessage::Assistant(message.clone()),
                tool_results: tool_results.clone(),
            })
            .await;

            // 决策在 `turn_end` 之后应用（error/aborted 已是硬退出，不会走到这里）。
            match turn_decision {
                Some(AgentTurnDecision::End) => {
                    emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await;
                    return;
                }
                // 确保再进行一次 provider 请求（工具结果/steering/follow-up 可满足它；
                // 否则用当前上下文再发一次）。
                Some(AgentTurnDecision::Continue) => {
                    has_more_tool_calls = true;
                }
                None => {}
            }

            pending_messages = match &config.get_steering_messages {
                Some(f) => f().await,
                None => Vec::new(),
            };
        }

        // agent 本应停止，检查 follow-up 消息。
        let follow_up = match &config.get_follow_up_messages {
            Some(f) => f().await,
            None => Vec::new(),
        };
        if !follow_up.is_empty() {
            pending_messages = follow_up;
            continue;
        }

        break;
    }

    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await;
}

/// 对应 `streamAssistantResponse`：流式 assistant 响应，并在 LLM 边界做消息转换。
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: &StreamFn,
) -> AssistantMessage {
    // 应用上下文转换（AgentMessage[] → AgentMessage[]）。
    let mut messages = context.messages.clone();
    if let Some(transform) = &config.transform_context {
        messages = transform(messages, signal.cloned()).await;
    }

    // 转换为 LLM 兼容消息（AgentMessage[] → Message[]）。
    let llm_messages = (config.convert_to_llm)(messages);

    // 构造 LLM 上下文。
    let llm_context = Context {
        system_prompt: if context.system_prompt.is_empty() {
            None
        } else {
            Some(context.system_prompt.clone())
        },
        messages: llm_messages,
        tools: context
            .tools
            .as_ref()
            .map(|tools| tools.iter().map(|t| t.tool.clone()).collect()),
    };

    // 解析 API key（对会过期的 token 很重要）。
    let resolved_api_key = match &config.get_api_key {
        Some(f) => f(&config.model.provider).await,
        None => None,
    };

    let mut options = config.stream.clone();
    options.stream.request.api_key = resolved_api_key;
    options.stream.request.signal = signal.cloned();

    let mut response = stream_fn(&config.model, &llm_context, Some(&options));

    let mut added_partial = false;
    let mut partial_message: Option<AssistantMessage> = None;

    while let Some(event) = response.next().await {
        let event_for_update = event.clone();
        match event {
            AssistantMessageEvent::Start { partial } => {
                partial_message = Some(partial.clone());
                context
                    .messages
                    .push(AgentMessage::Assistant(partial.clone()));
                added_partial = true;
                emit(AgentEvent::MessageStart {
                    message: AgentMessage::Assistant(partial),
                })
                .await;
            }
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
                // 对齐原版：只有收到 `start` 后才跟踪 partial；无 `start` 的流
                // （openai-responses）在此忽略 delta，避免覆盖最后一条真实消息。
                if partial_message.is_some() {
                    partial_message = Some(partial.clone());
                    if let Some(last) = context.messages.last_mut() {
                        *last = AgentMessage::Assistant(partial.clone());
                    }
                    emit(AgentEvent::MessageUpdate {
                        message: AgentMessage::Assistant(partial),
                        assistant_message_event: event_for_update,
                    })
                    .await;
                }
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                let final_message = response.result().await;
                if added_partial {
                    if let Some(last) = context.messages.last_mut() {
                        *last = AgentMessage::Assistant(final_message.clone());
                    }
                } else {
                    context
                        .messages
                        .push(AgentMessage::Assistant(final_message.clone()));
                    emit(AgentEvent::MessageStart {
                        message: AgentMessage::Assistant(final_message.clone()),
                    })
                    .await;
                }
                emit(AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(final_message.clone()),
                })
                .await;
                // 对应上游在流结果上附加 `thinkingLevel: config.reasoning ?? "off"`。
                let mut final_message = final_message;
                final_message.thinking_level = config.stream.reasoning;
                return final_message;
            }
        }
    }

    let final_message = response.result().await;
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = AgentMessage::Assistant(final_message.clone());
        }
    } else {
        context
            .messages
            .push(AgentMessage::Assistant(final_message.clone()));
        emit(AgentEvent::MessageStart {
            message: AgentMessage::Assistant(final_message.clone()),
        })
        .await;
    }
    emit(AgentEvent::MessageEnd {
        message: AgentMessage::Assistant(final_message.clone()),
    })
    .await;
    let mut final_message = final_message;
    final_message.thinking_level = config.stream.reasoning;
    final_message
}

/// 对应 `failToolCallsFromTruncatedMessage`
async fn fail_tool_calls_from_truncated_message(
    tool_calls: &[ToolCall],
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await;
        let finalized = FinalizedToolCallOutcome {
            tool_call: tool_call.clone(),
            result: create_error_tool_result(&format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
            duration_ms: None,
        };
        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }
    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

/// 对应 `executeToolCalls`（按模式分发）。
async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let tool_calls: Vec<ToolCall> = assistant_message
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect();

    let has_sequential = tool_calls.iter().any(|tc| {
        current_context
            .tools
            .as_ref()
            .and_then(|tools| tools.iter().find(|t| t.name() == tc.name))
            .map(|t| t.execution_mode == Some(ToolExecutionMode::Sequential))
            .unwrap_or(false)
    });

    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential {
        execute_tool_calls_sequential(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            signal,
            emit,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            signal,
            emit,
        )
        .await
    }
}

/// 对应 `executeToolCallsSequential`
async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let hooks = tool_call_hooks_of(config);
    let mut finalized_calls: Vec<FinalizedToolCallOutcome> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            &hooks,
            signal,
        )
        .await;
        let finalized = match preparation {
            PreparedToolCall::Immediate { result, is_error } => FinalizedToolCallOutcome {
                tool_call: tool_call.clone(),
                result,
                is_error,
                duration_ms: None,
            },
            PreparedToolCall::Prepared {
                tool_call,
                tool,
                args,
            } => {
                let executed =
                    execute_prepared_tool_call(&tool_call, &tool, &args, signal, emit).await;
                finalize_executed_tool_call(
                    current_context,
                    assistant_message,
                    &tool_call,
                    &args,
                    executed,
                    &hooks,
                    signal,
                )
                .await
            }
        };

        emit_tool_execution_end(&finalized, emit).await;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        finalized_calls.push(finalized);
        messages.push(tool_result_message);

        if signal.map(|s| s.aborted()).unwrap_or(false) {
            break;
        }
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

/// 对应 `executeToolCallsParallel`
async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> ExecutedToolCallBatch {
    type BoxFuture = Pin<Box<dyn Future<Output = FinalizedToolCallOutcome> + Send>>;
    let hooks = Arc::new(tool_call_hooks_of(config));
    let mut futures: Vec<BoxFuture> = Vec::new();

    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            &hooks,
            signal,
        )
        .await;
        let fut: BoxFuture = match preparation {
            PreparedToolCall::Immediate { result, is_error } => {
                let finalized = FinalizedToolCallOutcome {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                    duration_ms: None,
                };
                emit_tool_execution_end(&finalized, emit).await;
                Box::pin(async move { finalized })
            }
            PreparedToolCall::Prepared {
                tool_call,
                tool,
                args,
            } => {
                let context = current_context.clone();
                let assistant_message = assistant_message.clone();
                let hooks = Arc::clone(&hooks);
                let signal = signal.cloned();
                let emit = emit.clone();
                Box::pin(async move {
                    if signal.as_ref().map(|s| s.aborted()).unwrap_or(false) {
                        let finalized = FinalizedToolCallOutcome {
                            tool_call: tool_call.clone(),
                            result: create_error_tool_result("Operation aborted"),
                            is_error: true,
                            duration_ms: None,
                        };
                        emit_tool_execution_end(&finalized, &emit).await;
                        return finalized;
                    }
                    let executed = execute_prepared_tool_call(
                        &tool_call,
                        &tool,
                        &args,
                        signal.as_ref(),
                        &emit,
                    )
                    .await;
                    let finalized = finalize_executed_tool_call(
                        &context,
                        &assistant_message,
                        &tool_call,
                        &args,
                        executed,
                        &hooks,
                        signal.as_ref(),
                    )
                    .await;
                    emit_tool_execution_end(&finalized, &emit).await;
                    finalized
                })
            }
        };

        if signal.map(|s| s.aborted()).unwrap_or(false) {
            futures.push(fut);
            break;
        }
        futures.push(fut);
    }

    let finalized_calls = futures::future::join_all(futures).await;
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for finalized in &finalized_calls {
        let tool_result_message = create_tool_result_message(finalized);
        emit_tool_result_message(&tool_result_message, emit).await;
        messages.push(tool_result_message);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    }
}

/// 对应 `shouldTerminateToolBatch`
fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCallOutcome]) -> bool {
    !finalized_calls.is_empty() && finalized_calls.iter().all(|f| f.result.terminate)
}

/// 对应 `prepareToolCallArguments`（Rust 中 `prepareArguments` 暂不支持，直接返回 toolCall）。
/// 对应 `prepareToolCallArguments`。
fn prepare_tool_call_arguments(tool: &AgentTool, tool_call: &ToolCall) -> ToolCall {
    let Some(prepare) = &tool.prepare_arguments else {
        return tool_call.clone();
    };
    let prepared = prepare(tool_call.arguments.clone());
    if prepared == tool_call.arguments {
        return tool_call.clone();
    }
    ToolCall {
        arguments: prepared,
        ..tool_call.clone()
    }
}

/// 对应 `RunToolCallOptions`（`ToolCallHooks` + 单次调用上下文）。
#[derive(Clone)]
pub struct RunToolCallOptions {
    /// 该调用所解析到的工具集。
    pub tools: Vec<AgentTool>,
    /// 传给钩子的「发起该调用的」assistant 消息。
    pub assistant_message: AssistantMessage,
    /// 传给钩子的当前 agent 上下文。
    pub context: AgentContext,
    pub signal: Option<AbortSignal>,
    /// 接收工具执行中的部分结果。
    pub on_update: Option<ToolUpdateSink>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
}

/// 对应 `runToolCall`：以与模型发起的调用完全相同的步骤跑一次工具调用
/// （参数准备 → schema 校验 → `beforeToolCall` → 执行 → `afterToolCall`）。
///
/// 不发事件、不加消息。工具内部调用其他工具时用它，让钩子（例如权限检查）同样生效。
/// 工具失败（未知工具、校验失败、被阻塞、抛错）不 reject，而以 `is_error: true` 返回。
pub async fn run_tool_call(
    tool_call: &ToolCall,
    options: RunToolCallOptions,
) -> AgentToolCallOutcome {
    let mut context = options.context.clone();
    context.tools = Some(options.tools.clone());
    let hooks = ToolCallHooks {
        before_tool_call: options.before_tool_call.clone(),
        after_tool_call: options.after_tool_call.clone(),
    };

    let preparation = prepare_tool_call(
        &context,
        &options.assistant_message,
        tool_call,
        &hooks,
        options.signal.as_ref(),
    )
    .await;

    match preparation {
        PreparedToolCall::Immediate { result, is_error } => FinalizedToolCallOutcome {
            tool_call: tool_call.clone(),
            result,
            is_error,
            duration_ms: None,
        },
        PreparedToolCall::Prepared {
            tool_call: prepared,
            tool,
            args,
        } => {
            // 上游此路径「Emits no events」：把内部 tool_execution_update 转给 onUpdate 回调。
            let on_update = options.on_update.clone();
            let sink: AgentEventSink = Arc::new(move |event: AgentEvent| {
                if let AgentEvent::ToolExecutionUpdate { partial_result, .. } = &event
                    && let Some(callback) = &on_update
                {
                    callback(partial_result.clone());
                }
                Box::pin(async {})
            });
            let executed =
                execute_prepared_tool_call(&prepared, &tool, &args, options.signal.as_ref(), &sink)
                    .await;
            finalize_executed_tool_call(
                &context,
                &options.assistant_message,
                &prepared,
                &args,
                executed,
                &hooks,
                options.signal.as_ref(),
            )
            .await
        }
    }
}

/// 从 `AgentLoopConfig` 抽出工具钩子（对应上游把 `ToolCallHooks` 与配置解耦的形态）。
fn tool_call_hooks_of(config: &AgentLoopConfig) -> ToolCallHooks {
    ToolCallHooks {
        before_tool_call: config.before_tool_call.clone(),
        after_tool_call: config.after_tool_call.clone(),
    }
}

/// 对应 `prepareToolCall`
async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    hooks: &ToolCallHooks,
    signal: Option<&AbortSignal>,
) -> PreparedToolCall {
    let tool = match current_context
        .tools
        .as_ref()
        .and_then(|tools| tools.iter().find(|t| t.name() == tool_call.name))
    {
        Some(tool) => tool.clone(),
        None => {
            return PreparedToolCall::Immediate {
                result: create_error_tool_result(&format!("Tool {} not found", tool_call.name)),
                is_error: true,
            };
        }
    };

    let prepared_tool_call = prepare_tool_call_arguments(&tool, tool_call);
    let validated_args = match validate_tool_arguments(&tool.tool, &prepared_tool_call) {
        Ok(args) => args,
        Err(message) => {
            return PreparedToolCall::Immediate {
                result: create_error_tool_result(&message),
                is_error: true,
            };
        }
    };

    if let Some(before) = &hooks.before_tool_call {
        let before_result = before(
            &BeforeToolCallContext {
                assistant_message: assistant_message.clone(),
                tool_call: tool_call.clone(),
                args: validated_args.clone(),
                context: current_context.clone(),
            },
            signal.cloned(),
        )
        .await;
        if signal.map(|s| s.aborted()).unwrap_or(false) {
            return PreparedToolCall::Immediate {
                result: create_error_tool_result("Operation aborted"),
                is_error: true,
            };
        }
        if let Some(result) = before_result
            && result.block
        {
            let mut error_result = create_error_tool_result(
                result
                    .reason
                    .as_deref()
                    .unwrap_or("Tool execution was blocked"),
            );
            if result.terminate {
                error_result.terminate = true;
            }
            return PreparedToolCall::Immediate {
                result: error_result,
                is_error: true,
            };
        }
    }

    if signal.map(|s| s.aborted()).unwrap_or(false) {
        return PreparedToolCall::Immediate {
            result: create_error_tool_result("Operation aborted"),
            is_error: true,
        };
    }

    PreparedToolCall::Prepared {
        tool_call: prepared_tool_call,
        tool,
        args: validated_args,
    }
}

/// 对应 `executePreparedToolCall`
async fn execute_prepared_tool_call(
    tool_call: &ToolCall,
    tool: &AgentTool,
    args: &serde_json::Value,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> ExecutedToolCallOutcome {
    let started_at = std::time::Instant::now();
    let update_events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let accepting_updates = Arc::new(std::sync::atomic::AtomicBool::new(true));

    let on_update = {
        let update_events = update_events.clone();
        let accepting_updates = accepting_updates.clone();
        let tool_call_id = tool_call.id.clone();
        let tool_name = tool_call.name.clone();
        let args_owned = args.clone();
        Some(Box::new(move |partial: AgentToolResult| {
            if !accepting_updates.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            update_events
                .lock()
                .unwrap()
                .push(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    args: args_owned.clone(),
                    partial_result: partial.clone(),
                });
        }) as Box<dyn Fn(AgentToolResult) + Send>)
    };

    let result = match std::panic::AssertUnwindSafe((tool.execute)(
        tool_call.id.clone(),
        args.clone(),
        signal.cloned(),
        on_update,
    ))
    .catch_unwind()
    .await
    {
        Ok(result) => result,
        Err(payload) => {
            accepting_updates.store(false, std::sync::atomic::Ordering::Relaxed);
            return ExecutedToolCallOutcome {
                result: create_error_tool_result(&panic_message(&payload)),
                is_error: true,
                duration_ms: started_at.elapsed().as_millis() as u64,
            };
        }
    };
    accepting_updates.store(false, std::sync::atomic::Ordering::Relaxed);

    // 冲刷 update 事件。
    let events = update_events.lock().unwrap().clone();
    for event in events {
        emit(event).await;
    }

    // 对齐上游 `isError: result.isError === true`。
    let is_error = result.is_error;
    ExecutedToolCallOutcome {
        result,
        is_error,
        duration_ms: started_at.elapsed().as_millis() as u64,
    }
}

/// 对应 `finalizeExecutedToolCall`
async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    args: &serde_json::Value,
    executed: ExecutedToolCallOutcome,
    hooks: &ToolCallHooks,
    signal: Option<&AbortSignal>,
) -> FinalizedToolCallOutcome {
    let mut result = executed.result;
    let mut is_error = executed.is_error;
    let duration_ms = executed.duration_ms;

    if let Some(after) = &hooks.after_tool_call {
        let after_result = after(
            &AfterToolCallContext {
                assistant_message: assistant_message.clone(),
                tool_call: tool_call.clone(),
                args: args.clone(),
                result: result.clone(),
                is_error,
                context: current_context.clone(),
            },
            signal.cloned(),
        )
        .await;
        if let Some(after_result) = after_result {
            // 对齐上游：`structuredContent` 与 `content` 替换联动——
            // 若钩子只换了 content 而未给 structuredContent，则丢弃旧的（已不匹配）。
            let structured_content = after_result.structured_content.or_else(|| {
                if after_result.content.is_some() {
                    None
                } else {
                    result.structured_content.clone()
                }
            });
            result.content = after_result.content.unwrap_or(result.content);
            result.details = after_result.details.unwrap_or(result.details);
            result.usage = after_result.usage.or(result.usage);
            result.terminate = after_result.terminate.unwrap_or(result.terminate);
            result.structured_content = structured_content;
            is_error = after_result.is_error.unwrap_or(is_error);
        }
    }

    FinalizedToolCallOutcome {
        tool_call: tool_call.clone(),
        result,
        is_error,
        duration_ms: Some(duration_ms),
    }
}

/// 对应 `createErrorToolResult`
fn create_error_tool_result(message: &str) -> AgentToolResult {
    AgentToolResult {
        content: vec![TextOrImageContent::Text(TextContent {
            kind: TextKind,
            text: message.to_string(),
            text_signature: None,
        })],
        details: serde_json::Value::Object(Default::default()),
        usage: None,
        added_tool_names: None,
        terminate: false,
        is_error: true,
        structured_content: None,
    }
}

/// 对应 `emitToolExecutionEnd`
async fn emit_tool_execution_end(finalized: &FinalizedToolCallOutcome, emit: &AgentEventSink) {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: finalized.result.details.clone(),
        is_error: finalized.is_error,
    })
    .await;
}

/// 对应 `createToolResultMessage`
fn create_tool_result_message(finalized: &FinalizedToolCallOutcome) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        details: Some(finalized.result.details.clone()),
        usage: finalized.result.usage.clone(),
        added_tool_names: finalized.result.added_tool_names.clone(),
        is_error: finalized.is_error,
        timestamp: pi_ai::utils::uuid::now_ms() as u64,
        duration_ms: finalized.duration_ms,
    }
}

/// 对应 `emitToolResultMessage`
async fn emit_tool_result_message(tool_result_message: &ToolResultMessage, emit: &AgentEventSink) {
    emit(AgentEvent::MessageStart {
        message: AgentMessage::ToolResult(tool_result_message.clone()),
    })
    .await;
    emit(AgentEvent::MessageEnd {
        message: AgentMessage::ToolResult(tool_result_message.clone()),
    })
    .await;
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    "tool execution failed".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            description: format!("{name} tool"),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
            constrained_sampling: None,
        }
    }

    fn agent_tool_with_prepare(
        prepare: Option<Arc<dyn Fn(serde_json::Value) -> serde_json::Value + Send + Sync>>,
    ) -> AgentTool {
        AgentTool {
            label: "t".into(),
            tool: tool("t"),
            execute: Arc::new(|_id, _params, _signal, _on_update| {
                Box::pin(async { create_error_tool_result("unused") })
            }),
            prepare_arguments: prepare,
            execution_mode: None,
            replay: None,
        }
    }

    fn tool_call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            kind: pi_ai::ToolCallKind,
            id: "call-1".into(),
            name: "t".into(),
            arguments,
            thought_signature: None,
            namespace: None,
        }
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(pi_ai::UserMessage {
            content: pi_ai::UserContent::Text(text.into()),
            timestamp: 1,
        })
    }

    fn system_with_tools(content: &str, tools_added: Vec<Tool>) -> AgentMessage {
        AgentMessage::System(SystemMessage {
            content: SystemContent::Text(content.into()),
            sections: None,
            tools_added: Some(tools_added),
            tools_removed: None,
            timestamp: 0,
        })
    }

    #[test]
    fn declare_tool_changes_inserts_system_message_for_new_tools() {
        let declared = declare_tool_changes(&[tool("read")], &[], vec![user("hi")]);

        assert_eq!(declared.len(), 2, "应在首个非 system 消息前插入声明");
        match &declared[0] {
            AgentMessage::System(system) => {
                assert_eq!(system.content, SystemContent::Text(String::new()));
                let added = system.tools_added.as_ref().expect("tools_added");
                assert_eq!(added.len(), 1);
                assert_eq!(added[0].name, "read");
            }
            other => panic!("expected injected system message, got {other:?}"),
        }
    }

    #[test]
    fn declare_tool_changes_is_noop_when_transcript_matches() {
        let committed = vec![system_with_tools("base", vec![tool("read")])];
        let pending = vec![user("hi")];

        let declared = declare_tool_changes(&[tool("read")], &committed, pending.clone());
        assert_eq!(declared.len(), pending.len(), "无差异时不应插入消息");
    }

    #[test]
    fn prepare_tool_call_arguments_applies_prepare_hook() {
        let t = agent_tool_with_prepare(Some(Arc::new(|mut args: serde_json::Value| {
            args["normalized"] = serde_json::json!(true);
            args
        })));
        let call = tool_call(serde_json::json!({ "raw": 1 }));

        let out = prepare_tool_call_arguments(&t, &call);

        assert_eq!(
            out.arguments,
            serde_json::json!({ "raw": 1, "normalized": true })
        );
        assert_eq!(out.id, "call-1", "其余字段应保留");
    }

    #[test]
    fn prepare_tool_call_arguments_is_noop_without_hook() {
        let t = agent_tool_with_prepare(None);
        let call = tool_call(serde_json::json!({ "raw": 1 }));

        let out = prepare_tool_call_arguments(&t, &call);

        assert_eq!(out.arguments, serde_json::json!({ "raw": 1 }));
    }

    #[test]
    fn prepare_tool_call_arguments_keeps_call_when_unchanged() {
        let t = agent_tool_with_prepare(Some(Arc::new(|args: serde_json::Value| args)));
        let call = tool_call(serde_json::json!({ "raw": 1 }));

        let out = prepare_tool_call_arguments(&t, &call);

        assert_eq!(out.arguments, call.arguments);
    }

    #[test]
    fn declare_tool_changes_declares_removals() {
        let committed = vec![system_with_tools("base", vec![tool("read"), tool("write")])];

        let declared = declare_tool_changes(&[tool("read")], &committed, vec![user("hi")]);
        let injected = declared
            .iter()
            .find_map(|message| match message {
                AgentMessage::System(system) if system.tools_removed.is_some() => Some(system),
                _ => None,
            })
            .expect("应声明被移除的工具");
        let removed = injected.tools_removed.as_ref().unwrap();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "write");
    }

    #[test]
    fn declare_tool_changes_reuses_pending_system_message() {
        // pending 已有 system 消息时，工具差异写入它（而不是新增一条）。
        let pending = vec![system_with_tools("", vec![]), user("hi")];

        let declared = declare_tool_changes(&[tool("read")], &[], pending);
        assert_eq!(declared.len(), 2);
        match &declared[0] {
            AgentMessage::System(system) => {
                assert_eq!(system.tools_added.as_ref().map(Vec::len), Some(1));
            }
            other => panic!("expected pending system message to carry changes, got {other:?}"),
        }
    }

    #[test]
    fn fold_initial_system_message_creates_leading_system_message() {
        let mut context = AgentContext {
            system_prompt: "be nice".into(),
            messages: vec![user("hi")],
            tools: None,
        };

        fold_initial_system_message(&mut context);

        assert!(matches!(
            context.messages.first(),
            Some(AgentMessage::System(_))
        ));
        assert!(context.system_prompt.is_empty(), "折叠后字段应清空");
        match &context.messages[0] {
            AgentMessage::System(system) => {
                assert_eq!(system.content, SystemContent::Text("be nice".into()));
            }
            other => panic!("expected system message, got {other:?}"),
        }
    }

    #[test]
    fn fold_initial_system_message_is_idempotent() {
        let mut context = AgentContext {
            system_prompt: "be nice".into(),
            messages: vec![user("hi")],
            tools: None,
        };
        fold_initial_system_message(&mut context);
        let len_after_first = context.messages.len();

        fold_initial_system_message(&mut context);
        assert_eq!(
            context.messages.len(),
            len_after_first,
            "已有首条 system 消息时不重复插入"
        );
    }
}
