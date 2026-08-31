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
