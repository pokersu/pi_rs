//! Rust 翻译自 packages/ai/src/api/openai-completions.ts（简化版）
//!
//! 支持基础 text + tool call 的 SSE 流式调用，省略 thinking/cache/compat/deferred
//! 等高级特性。openai 与 deepseek 均复用此实现（OpenAI Chat Completions 兼容）。

use std::sync::Arc;

use futures::StreamExt;
use serde_json::{Value, json};

use crate::providers::faux::stream_with_deltas;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, ErrorStopReason, Message,
    Model, SimpleStreamOptions, StopReason, StreamFunction, TextContent, TextKind, Tool, ToolCall,
};
use crate::utils::error_stream::{create_error_message, default_usage};
use crate::utils::event_stream::create_assistant_message_event_stream;
use crate::utils::json_parse::parse_streaming_json;

/// 对应 `OpenAICompletionsCompat`（completions 侧需要的子集）。
struct Compat {
    supports_developer_role: bool,
    supports_mid_convo_system_messages: bool,
    supports_mid_convo_tool_additions: bool,
}

/// 对应 `getCompat`（completions 子集）。
fn get_compat(model: &Model) -> Compat {
    let compat = model.compat.as_ref().and_then(|c| c.as_object());
    let get_bool = |key: &str, default: bool| {
        compat
            .and_then(|c| c.get(key))
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    };
    Compat {
        supports_developer_role: get_bool("supportsDeveloperRole", true),
        supports_mid_convo_system_messages: get_bool("supportsMidConvoSystemMessages", false),
        supports_mid_convo_tool_additions: get_bool("supportsMidConvoToolAdditions", false),
    }
}

/// 对应 `convertMessages`（简化：仅基础 text/image/tool_call 转换）。
fn build_messages(
    raw_messages: &[Message],
    instruction_role: &str,
    transcript_tools: &crate::utils::transcript::TranscriptTools,
) -> Vec<Value> {
    let mut messages = Vec::new();
    for (msg_index, msg) in raw_messages.iter().enumerate() {
        match msg {
            crate::types::Message::System(system) => {
                // i > 0 且传输层支持锚定追加时，就地声明 toolsAdded（Kimi 风格 system+tools）。
                if msg_index > 0 && transcript_tools.anchors_additions {
                    let added = system.tools_added.as_deref().unwrap_or(&[]);
                    if !added.is_empty() {
                        messages.push(json!({ "role": "system", "tools": build_tools(added) }));
                    }
                }
                let text = if msg_index == 0 {
                    crate::utils::text::get_system_message_text(system)
                } else {
                    crate::utils::text::render_system_message_update(system)
                };
                if !text.is_empty() {
                    messages.push(json!({
                        "role": instruction_role,
                        "content": crate::utils::sanitize_unicode::sanitize_surrogates(&text)
                    }));
                }
            }
            crate::types::Message::User(u) => match &u.content {
                crate::types::UserContent::Text(s) => {
                    messages.push(json!({ "role": "user", "content": s }));
                }
                crate::types::UserContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
						.iter()
						.map(|b| match b {
							crate::types::TextOrImageContent::Text(t) => {
								json!({ "type": "text", "text": t.text })
							}
							crate::types::TextOrImageContent::Image(i) => json!({
								"type": "image_url",
								"image_url": { "url": format!("data:{};base64,{}", i.mime_type, i.data) }
							}),
						})
						.collect();
                    if !parts.is_empty() {
                        messages.push(json!({ "role": "user", "content": parts }));
                    }
                }
            },
            crate::types::Message::Assistant(a) => {
                let text: String = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let tool_calls: Vec<Value> = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall(tc) => Some(json!({
                            "id": tc.id,
                            "type": "function",
                            "function": { "name": tc.name, "arguments": tc.arguments.to_string() }
                        })),
                        _ => None,
                    })
                    .collect();
                let mut m = json!({ "role": "assistant", "content": text });
                if !tool_calls.is_empty() {
                    m["tool_calls"] = json!(tool_calls);
                }
                messages.push(m);
            }
            crate::types::Message::ToolResult(r) => {
                let content: String = r
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        crate::types::TextOrImageContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(
                    json!({ "role": "tool", "tool_call_id": r.tool_call_id, "content": content }),
                );
            }
        }
    }
    messages
}

/// 对应 `convertTools`（简化）。
fn build_tools(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

/// 对应 `buildParams`（简化）。
fn build_body(model: &Model, context: &Context, options: Option<&SimpleStreamOptions>) -> Value {
    let compat = get_compat(model);
    // 归一化：把 `Context.systemPrompt`/`tools` 折叠进 transcript。
    let transcript = crate::utils::transcript::normalize_context(context);
    let normalized = crate::utils::transcript::resolve_transcript(
        transcript,
        Some(compat.supports_mid_convo_system_messages),
    );
    let transcript_tools = crate::utils::transcript::resolve_transcript_tools(
        &normalized.messages,
        compat.supports_mid_convo_system_messages && compat.supports_mid_convo_tool_additions,
    );
    let instruction_role = if model.reasoning && compat.supports_developer_role {
        "developer"
    } else {
        "system"
    };

    let mut body = json!({
        "model": model.id,
        "messages": build_messages(&normalized.messages, instruction_role, &transcript_tools),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !transcript_tools.request_tools.is_empty() {
        body["tools"] = json!(build_tools(&transcript_tools.request_tools));
    }
    if let Some(options) = options {
        if let Some(max_tokens) = options.stream.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(temperature) = options.stream.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(tool_choice) = &options.tool_choice {
            body["tool_choice"] = serde_json::to_value(tool_choice).unwrap();
        }
    }
    body
}

struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
}

fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::Stop,
        "length" => StopReason::Length,
        "tool_calls" => StopReason::ToolUse,
        _ => StopReason::Stop,
    }
}

async fn stream_request(
    base_url: &str,
    api_key: &str,
    model: &Model,
    body: Value,
    stream: &crate::utils::event_stream::AssistantMessageEventStream,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {text}"));
    }

    let mut text = String::new();
    let mut tool_call_accums: Vec<ToolCallAccum> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut usage = default_usage();

    let mut byte_stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(newline) = buffer.find('\n') {
            let line: String = buffer.drain(..=newline).collect();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let chunk: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;

            if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array())
                && let Some(choice) = choices.first()
            {
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                        text.push_str(content);
                    }
                    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let index =
                                tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                            while tool_call_accums.len() <= index {
                                tool_call_accums.push(ToolCallAccum {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: String::new(),
                                });
                            }
                            let accum = &mut tool_call_accums[index];
                            if let Some(id) = tc.get("id").and_then(|i| i.as_str())
                                && accum.id.is_empty()
                            {
                                accum.id = id.to_string();
                            }
                            if let Some(func) = tc.get("function") {
                                if let Some(name) = func.get("name").and_then(|n| n.as_str())
                                    && accum.name.is_empty()
                                {
                                    accum.name = name.to_string();
                                }
                                if let Some(arguments) =
                                    func.get("arguments").and_then(|a| a.as_str())
                                {
                                    accum.arguments.push_str(arguments);
                                }
                            }
                        }
                    }
                }
                if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str())
                    && !fr.is_empty()
                    && fr != "null"
                {
                    finish_reason = Some(fr.to_string());
                }
            }

            if let Some(u) = chunk.get("usage") {
                usage.input = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                usage.output = u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                usage.total_tokens = usage.input + usage.output;
            }
        }
    }

    // 组装 AssistantMessage。
    let mut content: Vec<ContentBlock> = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text(TextContent {
            kind: TextKind,
            text,
            text_signature: None,
        }));
    }
    for accum in &tool_call_accums {
        content.push(ContentBlock::ToolCall(ToolCall {
            kind: crate::types::ToolCallKind,
            id: accum.id.clone(),
            name: accum.name.clone(),
            arguments: parse_streaming_json(Some(&accum.arguments)),
            thought_signature: None,
            namespace: None,
        }));
    }

    let stop_reason = finish_reason
        .as_deref()
        .map(map_finish_reason)
        .unwrap_or(StopReason::Stop);
    let message = AssistantMessage {
        content,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        usage,
        stop_reason,
        deferred: None,
        error_message: None,
        raw_stop_reason: finish_reason,
        end_turn: None,
        timestamp: crate::utils::uuid::now_ms() as u64,
    };

    stream_with_deltas(stream, &message);
    Ok(())
}

/// 构造一个 OpenAI Chat Completions 兼容的 stream 函数。
/// api key 由上层（`Models::stream_simple`）解析并注入到 `options.stream.request.api_key`，
/// 这里不再直接读环境变量。
pub fn openai_completions_stream(base_url: String) -> StreamFunction {
    Arc::new(move |model, context, options| {
        let outer = create_assistant_message_event_stream();
        let producer = outer.clone();
        let base_url = base_url.clone();
        let model = model.clone();
        let body = build_body(&model, context, options);
        let api_key = options.and_then(|o| o.stream.request.api_key.clone());
        let provider = model.provider.clone();
        tokio::spawn(async move {
            match api_key {
                None => {
                    let error = create_error_message(
                        &format!("Missing API key for provider: {provider}"),
                        &model.api,
                        &model.provider,
                        &model.id,
                    );
                    producer.push(AssistantMessageEvent::Error {
                        reason: ErrorStopReason::Error,
                        error: error.clone(),
                    });
                    producer.end(Some(error));
                }
                Some(key) => {
                    if let Err(err) = stream_request(&base_url, &key, &model, body, &producer).await
                    {
                        let error =
                            create_error_message(&err, &model.api, &model.provider, &model.id);
                        producer.push(AssistantMessageEvent::Error {
                            reason: ErrorStopReason::Error,
                            error: error.clone(),
                        });
                        producer.end(Some(error));
                    }
                }
            }
        });
        outer
    })
}
