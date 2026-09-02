//! Rust 翻译自 agent 核心测试（faux provider 驱动 agent loop + 工具执行）。

use std::sync::{Arc, Mutex};

use pi_agent::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentOptions, AgentTool,
    AgentToolResult, ToolExecutionMode, agent_loop::AgentEventSink, run_agent_loop,
};
use pi_ai::{
    ContentBlock, StopReason, StreamFunction, TextOrImageContent, Tool, ToolCallKind,
    faux_assistant_message, faux_provider, faux_text, faux_tool_call,
};

fn identity_convert(messages: Vec<AgentMessage>) -> Vec<pi_ai::Message> {
    pi_agent::harness::messages::convert_to_llm(messages)
}

fn make_config(model: pi_ai::Model) -> AgentLoopConfig {
    AgentLoopConfig {
        model,
        stream: Default::default(),
        convert_to_llm: Arc::new(identity_convert),
        transform_context: None,
        get_api_key: None,
        should_stop_after_turn: None,
        prepare_next_turn: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        before_tool_call: None,
        after_tool_call: None,
        tool_execution: ToolExecutionMode::Parallel,
    }
}

fn faux_stream_fn(handle: Arc<pi_ai::providers::faux::FauxProviderHandle>) -> StreamFunction {
    Arc::new(move |model, context, options| handle.provider.stream_simple(model, context, options))
}

fn sink() -> (AgentEventSink, Arc<Mutex<Vec<AgentEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let emit: AgentEventSink = {
        let events = events.clone();
        Arc::new(move |e| {
            let events = events.clone();
            Box::pin(async move {
                events.lock().unwrap().push(e);
            })
        })
    };
    (emit, events)
}

#[tokio::test]
async fn agent_loop_streams_text_response() {
    let handle = faux_provider(Vec::new());
    let model = handle.provider.get_models().into_iter().next().unwrap();
    handle.set_responses(vec![faux_assistant_message(
        vec![faux_text("hello world")],
        StopReason::Stop,
    )]);
    let stream_fn = faux_stream_fn(Arc::new(handle));

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: None,
    };
    let prompts = vec![AgentMessage::User(pi_ai::UserMessage {
        content: pi_ai::UserContent::Text("hi".into()),
        timestamp: 0,
    })];
    let (emit, _events) = sink();

    let messages =
        run_agent_loop(prompts, context, make_config(model), None, emit, stream_fn).await;

    // transcript 应包含 assistant 消息。
    let assistant = messages
        .iter()
        .find_map(|m| match m {
            AgentMessage::Assistant(a) => Some(a),
            _ => None,
        })
        .expect("expected assistant message");
    let text: String = assistant
        .content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello world");
    assert_eq!(assistant.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn agent_loop_executes_tool_call() {
    let handle = faux_provider(Vec::new());
    let model = handle.provider.get_models().into_iter().next().unwrap();
    // 第一响应：请求工具调用；第二响应：工具结果后的最终回复。
    handle.set_responses(vec![
        faux_assistant_message(
            vec![faux_tool_call(
                "double",
                serde_json::json!({ "n": 21 }),
                None,
            )],
            StopReason::ToolUse,
        ),
        faux_assistant_message(vec![faux_text("done")], StopReason::Stop),
    ]);
    let stream_fn = faux_stream_fn(Arc::new(handle));

    let tool = AgentTool {
        label: "double".into(),
        tool: Tool {
            name: "double".into(),
            description: "double a number".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } },
                "required": ["n"]
            }),
            constrained_sampling: None,
        },
        execute: Arc::new(|_id, params, _signal, _on_update| {
            Box::pin(async move {
                let n = params.get("n").and_then(|v| v.as_i64()).unwrap_or(0);
                AgentToolResult {
                    content: vec![TextOrImageContent::Text(pi_ai::TextContent {
                        kind: pi_ai::TextKind,
                        text: (n * 2).to_string(),
                        text_signature: None,
                    })],
                    details: serde_json::json!({ "doubled": n * 2 }),
                    usage: None,
                    added_tool_names: None,
                    terminate: false,
                }
            })
        }),
        execution_mode: None,
    };

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![tool]),
    };
    let prompts = vec![AgentMessage::User(pi_ai::UserMessage {
        content: pi_ai::UserContent::Text("double 21".into()),
        timestamp: 0,
    })];
    let (emit, _events) = sink();

    let messages =
        run_agent_loop(prompts, context, make_config(model), None, emit, stream_fn).await;

    // 应包含 toolResult 消息（工具执行结果）。
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, AgentMessage::ToolResult(r) if r.tool_name == "double"))
    );
}

#[tokio::test]
async fn agent_class_runs_prompt() {
    let handle = faux_provider(Vec::new());
    let model = handle.provider.get_models().into_iter().next().unwrap();
    handle.set_responses(vec![faux_assistant_message(
        vec![faux_text("hi back")],
        StopReason::Stop,
    )]);

    pi_agent::set_default_stream_fn(Some(faux_stream_fn(Arc::new(handle))));

    let agent = pi_agent::Agent::new(AgentOptions {
        model: Some(model),
        ..Default::default()
    });
    agent.prompt_text("hello").await;

    let state = agent.state();
    assert!(
        state
            .messages
            .iter()
            .any(|m| matches!(m, AgentMessage::Assistant(_)))
    );
    assert!(!state.is_streaming);
}

#[allow(dead_code)]
fn _assert_send<T: Send + Sync>() {}

#[test]
fn agent_types_are_send_sync() {
    _assert_send::<pi_agent::Agent>();
    _assert_send::<AgentTool>();
    _assert_send::<AgentEvent>();
    _assert_send::<ToolCallKind>();
}

/// 回归：openai-responses 流无 `start` 事件时，agent-loop 不应把 user 消息覆盖成
/// assistant partial，也不应把 assistant 重复写入 context（曾导致 duplicate call_id）。
#[tokio::test]
async fn stream_without_start_event_keeps_context_intact() {
    use pi_ai::{
        AssistantMessage, AssistantMessageEvent, TerminalStopReason, ToolCall, UserContent,
        UserMessage, create_assistant_message_event_stream, default_usage,
    };

    let model = faux_provider(Vec::new())
        .provider
        .get_models()
        .into_iter()
        .next()
        .unwrap();

    let call_count = Arc::new(Mutex::new(0u32));
    let stream_fn: StreamFunction = {
        let call_count = call_count.clone();
        Arc::new(move |model, _context, _options| {
            let stream = create_assistant_message_event_stream();
            let producer = stream.clone();
            let model = model.clone();
            let call_count = call_count.clone();
            tokio::spawn(async move {
                let mut count = call_count.lock().unwrap();
                *count += 1;
                let is_first = *count == 1;
                drop(count);

                let base = |content| AssistantMessage {
                    content,
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.id.clone(),
                    response_model: None,
                    response_id: None,
                    usage: default_usage(),
                    stop_reason: if is_first {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    },
                    deferred: None,
                    error_message: None,
                    raw_stop_reason: None,
                    end_turn: None,
                    timestamp: 1,
                };

                if is_first {
                    let tool_call = ToolCall {
                        kind: ToolCallKind,
                        id: "call_00_X|fc_1".into(),
                        name: "double".into(),
                        arguments: serde_json::json!({ "n": 21 }),
                        thought_signature: None,
                        namespace: None,
                    };
                    let partial = base(vec![ContentBlock::ToolCall(tool_call.clone())]);
                    // 无 start 事件（对齐 openai-responses 流）。
                    producer.push(AssistantMessageEvent::ToolCallStart {
                        content_index: 0,
                        partial: partial.clone(),
                    });
                    producer.push(AssistantMessageEvent::ToolCallEnd {
                        content_index: 0,
                        tool_call,
                        partial: partial.clone(),
                    });
                    producer.push(AssistantMessageEvent::Done {
                        reason: TerminalStopReason::ToolUse,
                        message: partial.clone(),
                    });
                    producer.end(Some(partial));
                } else {
                    let partial = base(vec![ContentBlock::Text(pi_ai::TextContent {
                        kind: pi_ai::TextKind,
                        text: "done".into(),
                        text_signature: None,
                    })]);
                    producer.push(AssistantMessageEvent::TextStart {
                        content_index: 0,
                        partial: partial.clone(),
                    });
                    producer.push(AssistantMessageEvent::TextEnd {
                        content_index: 0,
                        content: "done".into(),
                        partial: partial.clone(),
                    });
                    producer.push(AssistantMessageEvent::Done {
                        reason: TerminalStopReason::Stop,
                        message: partial.clone(),
                    });
                    producer.end(Some(partial));
                }
            });
            stream
        })
    };

    let tool = AgentTool {
        label: "double".into(),
        tool: Tool {
            name: "double".into(),
            description: "double a number".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } },
                "required": ["n"]
            }),
            constrained_sampling: None,
        },
        execute: Arc::new(|_id, params, _signal, _on_update| {
            Box::pin(async move {
                let n = params.get("n").and_then(|v| v.as_i64()).unwrap_or(0);
                AgentToolResult {
                    content: vec![TextOrImageContent::Text(pi_ai::TextContent {
                        kind: pi_ai::TextKind,
                        text: (n * 2).to_string(),
                        text_signature: None,
                    })],
                    details: serde_json::json!({ "doubled": n * 2 }),
                    usage: None,
                    added_tool_names: None,
                    terminate: false,
                }
            })
        }),
        execution_mode: None,
    };

    // 捕获每轮 LLM 调用时的 context 消息。
    let captured: Arc<Mutex<Vec<Vec<AgentMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_convert = {
        let captured = captured.clone();
        Arc::new(move |messages: Vec<AgentMessage>| {
            captured.lock().unwrap().push(messages.clone());
            pi_agent::harness::messages::convert_to_llm(messages)
        })
    };

    let mut config = make_config(model.clone());
    config.convert_to_llm = captured_convert;

    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Some(vec![tool]),
    };
    let prompts = vec![AgentMessage::User(UserMessage {
        content: UserContent::Text("double 21".into()),
        timestamp: 0,
    })];
    let (emit, _events) = sink();

    let _messages = run_agent_loop(prompts, context, config, None, emit, stream_fn).await;

    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 2, "应有两轮 LLM 调用");
    let second = &calls[1];
    let user_count = second
        .iter()
        .filter(|m| matches!(m, AgentMessage::User(_)))
        .count();
    let assistant_count = second
        .iter()
        .filter(|m| matches!(m, AgentMessage::Assistant(_)))
        .count();
    let tool_call_count: usize = second
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Assistant(a) => Some(
                a.content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolCall(_)))
                    .count(),
            ),
            _ => None,
        })
        .sum();
    assert_eq!(user_count, 1, "user 消息不应被 assistant partial 覆盖");
    assert_eq!(assistant_count, 1, "assistant 不应重复写入 context");
    assert_eq!(
        tool_call_count, 1,
        "toolCall 不应重复（duplicate call_id 回归）"
    );
}
