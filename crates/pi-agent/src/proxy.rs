//! Rust 翻译自 packages/agent/src/proxy.ts
//!
//! 通过服务器代理的流函数：服务器剥离 partial 字段以降低带宽，客户端重建完整事件。

use futures::StreamExt;
use pi_ai::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, ContentBlock, Context, ErrorStopReason,
    Model, SimpleStreamOptions, StopReason, TerminalStopReason, TextContent, TextKind,
    ThinkingContent, ThinkingKind, ToolCall, Usage,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::types::StreamFn;
use pi_ai::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};
use pi_ai::utils::json_parse::parse_streaming_json;

/// 对应 `ProxyAssistantMessageEvent`（服务器端剥离 partial 字段的精简事件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ProxyAssistantMessageEvent {
    Start,
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content_signature: Option<String>,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content_signature: Option<String>,
    },
    ToolcallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    ToolcallDelta {
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        content_index: usize,
        tool_call: ToolCall,
    },
    Done {
        reason: TerminalStopReason,
        usage: Usage,
    },
    Error {
        reason: ErrorStopReason,
        error_message: Option<String>,
        usage: Usage,
    },
}

/// 对应 `processProxyEvent`：把精简事件重建为完整 `AssistantMessageEvent`，并更新 partial。
fn process_proxy_event(
    proxy_event: &ProxyAssistantMessageEvent,
    partial: &mut AssistantMessage,
) -> Option<AssistantMessageEvent> {
    match proxy_event {
        ProxyAssistantMessageEvent::Start => Some(AssistantMessageEvent::Start {
            partial: partial.clone(),
        }),

        ProxyAssistantMessageEvent::TextStart { content_index } => {
            set_content(
                partial,
                *content_index,
                ContentBlock::Text(TextContent {
                    kind: TextKind,
                    text: String::new(),
                    text_signature: None,
                }),
            );
            Some(AssistantMessageEvent::TextStart {
                content_index: *content_index,
                partial: partial.clone(),
            })
        }
        ProxyAssistantMessageEvent::TextDelta {
            content_index,
            delta,
        } => {
            if let ContentBlock::Text(t) = &mut partial.content[*content_index] {
                t.text.push_str(delta);
            } else {
                panic!("Received text_delta for non-text content");
            }
            Some(AssistantMessageEvent::TextDelta {
                content_index: *content_index,
                delta: delta.clone(),
                partial: partial.clone(),
            })
        }
        ProxyAssistantMessageEvent::TextEnd {
            content_index,
            content_signature,
        } => {
            let text = match &partial.content[*content_index] {
                ContentBlock::Text(t) => t.text.clone(),
                _ => panic!("Received text_end for non-text content"),
            };
            if let ContentBlock::Text(t) = &mut partial.content[*content_index] {
                t.text_signature = content_signature.clone();
            }
            Some(AssistantMessageEvent::TextEnd {
                content_index: *content_index,
                content: text,
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::ThinkingStart { content_index } => {
            set_content(
                partial,
                *content_index,
                ContentBlock::Thinking(ThinkingContent {
                    kind: ThinkingKind,
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                }),
            );
            Some(AssistantMessageEvent::ThinkingStart {
                content_index: *content_index,
                partial: partial.clone(),
            })
        }
        ProxyAssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
        } => {
            if let ContentBlock::Thinking(t) = &mut partial.content[*content_index] {
                t.thinking.push_str(delta);
            } else {
                panic!("Received thinking_delta for non-thinking content");
            }
            Some(AssistantMessageEvent::ThinkingDelta {
                content_index: *content_index,
                delta: delta.clone(),
                partial: partial.clone(),
            })
        }
        ProxyAssistantMessageEvent::ThinkingEnd {
            content_index,
            content_signature,
        } => {
            let thinking = match &partial.content[*content_index] {
                ContentBlock::Thinking(t) => t.thinking.clone(),
                _ => panic!("Received thinking_end for non-thinking content"),
            };
            if let ContentBlock::Thinking(t) = &mut partial.content[*content_index] {
                t.thinking_signature = content_signature.clone();
            }
            Some(AssistantMessageEvent::ThinkingEnd {
                content_index: *content_index,
                content: thinking,
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::ToolcallStart {
            content_index,
            id,
            tool_name,
        } => {
            set_content(
                partial,
                *content_index,
                ContentBlock::ToolCall(ToolCall {
                    kind: pi_ai::ToolCallKind,
                    id: id.clone(),
                    name: tool_name.clone(),
                    arguments: serde_json::Value::Object(Default::default()),
                    thought_signature: None,
                    namespace: None,
                }),
            );
            Some(AssistantMessageEvent::ToolCallStart {
                content_index: *content_index,
                partial: partial.clone(),
            })
        }
        ProxyAssistantMessageEvent::ToolcallDelta {
            content_index,
            delta,
        } => {
            if let ContentBlock::ToolCall(tc) = &mut partial.content[*content_index] {
                let accumulated = tc.arguments.to_string() + delta;
                tc.arguments = parse_streaming_json(Some(&accumulated));
            } else {
                panic!("Received toolcall_delta for non-toolCall content");
            }
            Some(AssistantMessageEvent::ToolCallDelta {
                content_index: *content_index,
                delta: delta.clone(),
                partial: partial.clone(),
            })
        }
        ProxyAssistantMessageEvent::ToolcallEnd {
            content_index,
            tool_call,
        } => {
            if let ContentBlock::ToolCall(tc) = &mut partial.content[*content_index] {
                *tc = tool_call.clone();
            }
            Some(AssistantMessageEvent::ToolCallEnd {
                content_index: *content_index,
                tool_call: tool_call.clone(),
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::Done { reason, usage } => {
            partial.stop_reason = terminal_to_stop(*reason);
            partial.usage = usage.clone();
            Some(AssistantMessageEvent::Done {
                reason: *reason,
                message: partial.clone(),
            })
        }
        ProxyAssistantMessageEvent::Error {
            reason,
            error_message,
            usage,
        } => {
            partial.stop_reason = if *reason == ErrorStopReason::Aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            partial.error_message = error_message.clone();
            partial.usage = usage.clone();
            Some(AssistantMessageEvent::Error {
                reason: *reason,
                error: partial.clone(),
            })
        }
    }
}

fn terminal_to_stop(reason: TerminalStopReason) -> StopReason {
    match reason {
        TerminalStopReason::Stop => StopReason::Stop,
        TerminalStopReason::Length => StopReason::Length,
        TerminalStopReason::ToolUse => StopReason::ToolUse,
        TerminalStopReason::Deferred => StopReason::Deferred,
    }
}

fn set_content(partial: &mut AssistantMessage, index: usize, content: ContentBlock) {
    if partial.content.len() <= index {
        partial.content.resize(
            index + 1,
            ContentBlock::Text(TextContent {
                kind: TextKind,
                text: String::new(),
                text_signature: None,
            }),
        );
    }
    partial.content[index] = content;
}

/// 对应 `streamProxy`：把请求转发给代理服务器并重建事件流。
pub fn stream_proxy(
    model: Model,
    context: Context,
    proxy_url: String,
    auth_token: String,
) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let producer = stream.clone();
    tokio::spawn(async move {
        let result = proxy_request(&model, &context, &proxy_url, &auth_token, &producer).await;
        if let Err(message) = result {
            let mut partial = partial_message(&model);
            partial.stop_reason = StopReason::Error;
            partial.error_message = Some(message.clone());
            producer.push(AssistantMessageEvent::Error {
                reason: ErrorStopReason::Error,
                error: partial.clone(),
            });
            producer.end(Some(partial));
        }
    });
    stream
}

fn partial_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        usage: pi_ai::utils::error_stream::default_usage(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: pi_ai::utils::uuid::now_ms() as u64,
    }
}

async fn proxy_request(
    model: &Model,
    context: &Context,
    proxy_url: &str,
    auth_token: &str,
    stream: &AssistantMessageEventStream,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "model": model, "context": context });
    let response = client
        .post(format!("{}/api/stream", proxy_url.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {auth_token}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        return Err(format!("Proxy error: {}", response.status()));
    }

    let mut partial = partial_message(model);
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
            if data.is_empty() {
                continue;
            }
            let proxy_event: ProxyAssistantMessageEvent =
                serde_json::from_str(data).map_err(|e| e.to_string())?;
            if let Some(event) = process_proxy_event(&proxy_event, &mut partial) {
                stream.push(event);
            }
        }
    }

    stream.end(None);
    Ok(())
}

/// 对应以代理 stream 作为 `StreamFn` 的便捷构造。
pub fn proxy_stream_fn(proxy_url: String, auth_token: String) -> StreamFn {
    Arc::new(move |model, context, _options| {
        stream_proxy(
            model.clone(),
            context.clone(),
            proxy_url.clone(),
            auth_token.clone(),
        )
    })
}

// 保留 `SimpleStreamOptions`/`AbortSignal` 引用以避免未使用告警。
#[allow(unused)]
fn _unused(_: Option<SimpleStreamOptions>, _: Option<AbortSignal>) {}
