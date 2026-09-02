//! Rust 翻译自 packages/agent/src/agent-loop.ts
//!
//! 底层 agent 循环：始终以 `AgentMessage` 工作，仅在 LLM 调用边界转换为 `Message[]`。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::{FutureExt, StreamExt};
use pi_ai::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, ContentBlock, Context, EventStream,
    StopReason, TextContent, TextKind, TextOrImageContent, ToolCall, ToolResultMessage,
    validate_tool_arguments,
};

use crate::types::{
    AfterToolCallContext, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool,
    AgentToolResult, BeforeToolCallContext, ShouldStopAfterTurnContext, StreamFn,
    ToolExecutionMode,
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

/// 对应 `FinalizedToolCallOutcome`
#[derive(Clone)]
struct FinalizedToolCallOutcome {
    tool_call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
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
    let mut last_completed_turn: Option<ShouldStopAfterTurnContext> = None;
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

            // 注入 pending 消息。
            if !pending_messages.is_empty() {
                let drained: Vec<AgentMessage> = std::mem::take(&mut pending_messages);
                for message in drained {
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

            emit(AgentEvent::TurnEnd {
                message: AgentMessage::Assistant(message.clone()),
                tool_results: tool_results.clone(),
            })
            .await;

            last_completed_turn = Some(ShouldStopAfterTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: current_context.clone(),
                new_messages: new_messages.clone(),
            });

            if let Some(should_stop) = &config.should_stop_after_turn
                && should_stop(last_completed_turn.as_ref().unwrap()).await
            {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return;
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
            config,
            signal,
        )
        .await;
        let finalized = match preparation {
            PreparedToolCall::Immediate { result, is_error } => FinalizedToolCallOutcome {
                tool_call: tool_call.clone(),
                result,
                is_error,
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
                    config,
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
            config,
            signal,
        )
        .await;
        let fut: BoxFuture = match preparation {
            PreparedToolCall::Immediate { result, is_error } => {
                let finalized = FinalizedToolCallOutcome {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
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
                let config = config.clone();
                let signal = signal.cloned();
                let emit = emit.clone();
                Box::pin(async move {
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
                        &config,
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
fn prepare_tool_call_arguments(_tool: &AgentTool, tool_call: &ToolCall) -> ToolCall {
    tool_call.clone()
}

/// 对应 `prepareToolCall`
async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
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

    if let Some(before) = &config.before_tool_call {
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
) -> AgentToolResult {
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
                    partial_result: partial.details.clone(),
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
            return create_error_tool_result(&panic_message(&payload));
        }
    };
    accepting_updates.store(false, std::sync::atomic::Ordering::Relaxed);

    // 冲刷 update 事件。
    let events = update_events.lock().unwrap().clone();
    for event in events {
        emit(event).await;
    }

    result
}

/// 对应 `finalizeExecutedToolCall`
async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    args: &serde_json::Value,
    executed: AgentToolResult,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
) -> FinalizedToolCallOutcome {
    let mut result = executed;
    let mut is_error = false;

    if let Some(after) = &config.after_tool_call {
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
            result.content = after_result.content.unwrap_or(result.content);
            result.details = after_result.details.unwrap_or(result.details);
            result.usage = after_result.usage.or(result.usage);
            result.terminate = after_result.terminate.unwrap_or(result.terminate);
            is_error = after_result.is_error.unwrap_or(is_error);
        }
    }

    FinalizedToolCallOutcome {
        tool_call: tool_call.clone(),
        result,
        is_error,
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
