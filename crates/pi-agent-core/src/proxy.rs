//! Rust 翻译自 packages/agent/src/proxy.ts
//!
//! 通过服务器代理的流函数：服务器剥离 partial 字段以降低带宽，客户端重建完整事件。

use std::collections::HashMap;

use futures::StreamExt;
use pi_ai::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, CacheRetention, ContentBlock, Context,
    ErrorStopReason, Model, StopReason, TerminalStopReason, TextContent, TextKind, ThinkingBudgets,
    ThinkingContent, ThinkingKind, ThinkingLevel, ToolCall, Transport, Usage,
};
use serde::{Deserialize, Serialize};

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
        #[serde(default)]
        provider_thinking_level: Option<String>,
    },
    Error {
        reason: ErrorStopReason,
        error_message: Option<String>,
        usage: Usage,
        #[serde(default)]
        provider_thinking_level: Option<String>,
    },
}

/// 对应 `processProxyEvent`：把精简事件重建为完整 `AssistantMessageEvent`，并更新 partial。
fn process_proxy_event(
    proxy_event: &ProxyAssistantMessageEvent,
    partial: &mut AssistantMessage,
    partial_json: &mut HashMap<usize, String>,
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
            // 对应上游：toolcall_start 初始化原始 JSON 文本累积。
            partial_json.insert(*content_index, String::new());
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
                // 对应上游：在原始 partialJson 文本上累积，而非在已解析参数的再序列化上累积。
                let raw = partial_json.entry(*content_index).or_default();
                raw.push_str(delta);
                tc.arguments = parse_streaming_json(Some(raw));
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
            // 对应上游：toolcall_end 删除 scratch 缓冲。
            partial_json.remove(content_index);
            Some(AssistantMessageEvent::ToolCallEnd {
                content_index: *content_index,
                tool_call: tool_call.clone(),
                partial: partial.clone(),
            })
        }

        ProxyAssistantMessageEvent::Done {
            reason,
            usage,
            provider_thinking_level,
        } => {
            partial.stop_reason = terminal_to_stop(*reason);
            partial.usage = usage.clone();
            if let Some(level) = provider_thinking_level {
                partial.provider_thinking_level = Some(level.clone());
            }
            Some(AssistantMessageEvent::Done {
                reason: *reason,
                message: partial.clone(),
            })
        }
        ProxyAssistantMessageEvent::Error {
            reason,
            error_message,
            usage,
            provider_thinking_level,
        } => {
            partial.stop_reason = if *reason == ErrorStopReason::Aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            partial.error_message = error_message.clone();
            partial.usage = usage.clone();
            if let Some(level) = provider_thinking_level {
                partial.provider_thinking_level = Some(level.clone());
            }
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

/// 对应 `ProxyStreamOptions`：含可序列化流式选项 + 代理地址/令牌。
#[derive(Debug, Clone, Default)]
pub struct ProxyStreamOptions {
    pub signal: Option<AbortSignal>,
    pub auth_token: String,
    pub proxy_url: String,
    pub temperature: Option<f64>,
    pub sampling_params: Option<serde_json::Value>,
    pub max_tokens: Option<u64>,
    pub reasoning: Option<ThinkingLevel>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub headers: Option<std::collections::BTreeMap<String, Option<String>>>,
    pub metadata: Option<serde_json::Value>,
    pub transport: Option<Transport>,
    pub thinking_budgets: Option<ThinkingBudgets>,
    pub max_retry_delay_ms: Option<u64>,
}

/// 对应 `streamProxy`：把请求转发给代理服务器并重建事件流。
pub fn stream_proxy(
    model: Model,
    context: Context,
    options: ProxyStreamOptions,
) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let producer = stream.clone();
    tokio::spawn(async move {
        let mut partial = partial_message(&model);
        let result = proxy_request(&model, &context, &options, &producer, &mut partial).await;
        if let Err(message) = result {
            // 对应上游：错误/中止时把已累积内容的 partial 作为 error 事件发出，而非新建空 partial。
            let reason = if options
                .signal
                .as_ref()
                .map(|s| s.aborted())
                .unwrap_or(false)
            {
                ErrorStopReason::Aborted
            } else {
                ErrorStopReason::Error
            };
            partial.stop_reason = if reason == ErrorStopReason::Aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            partial.error_message = Some(message);
            producer.push(AssistantMessageEvent::Error {
                reason,
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
        provider_thinking_level: None,
        thinking_level: None,
        usage: pi_ai::utils::error_stream::default_usage(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: pi_ai::utils::uuid::now_ms() as u64,
        duration_ms: None,
    }
}

async fn proxy_request(
    model: &Model,
    context: &Context,
    options: &ProxyStreamOptions,
    stream: &AssistantMessageEventStream,
    partial: &mut AssistantMessage,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    // 对应 `buildProxyRequestOptions`：提取可序列化选项。
    let options_body = serde_json::json!({
        "temperature": options.temperature,
        "samplingParams": options.sampling_params,
        "maxTokens": options.max_tokens,
        "reasoning": options.reasoning,
        "cacheRetention": options.cache_retention,
        "sessionId": options.session_id,
        "headers": options.headers,
        "metadata": options.metadata,
        "transport": options.transport,
        "thinkingBudgets": options.thinking_budgets,
        "maxRetryDelayMs": options.max_retry_delay_ms,
    });
    let body = serde_json::json!({ "model": model, "context": context, "options": options_body });
    let response = client
        .post(format!(
            "{}/api/stream",
            options.proxy_url.trim_end_matches('/')
        ))
        .header("Authorization", format!("Bearer {}", options.auth_token))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !response.status().is_success() {
        // 对应上游：非 2xx 时优先读响应体 `{ error }`，否则回退到 `status statusText`。
        let status = response.status();
        let status_text = status.canonical_reason().unwrap_or("").to_string();
        let mut error_message = format!("Proxy error: {} {}", status, status_text);
        error_message = error_message.trim_end().to_string();
        if let Ok(text) = response.text().await
            && let Ok(data) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(err) = data.get("error").and_then(|v| v.as_str())
        {
            error_message = format!("Proxy error: {}", err);
        }
        return Err(error_message);
    }

    let mut partial_json: HashMap<usize, String> = HashMap::new();
    let mut byte_stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut saw_terminal_event = false;

    loop {
        // 对应上游的 `reader.cancel` + 逐块 `signal.aborted` 检查：
        // 有 signal 时用 select 在读取间隙响应 abort。
        let chunk = if let Some(signal) = &options.signal {
            tokio::select! {
                _ = signal.cancelled() => {
                    return Err("Request aborted by user".to_string());
                }
                result = byte_stream.next() => result,
            }
        } else {
            byte_stream.next().await
        };
        let Some(chunk) = chunk else {
            break;
        };
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
            // 对应上游：协议违规（类型不符的增量事件）抛 Error，被外层 catch 捕获 → 发 error 事件。
            // 这里用 catch_unwind 捕获 panic，提取消息后作为 Err 返回，让流正常结束。
            let event = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                process_proxy_event(&proxy_event, partial, &mut partial_json)
            })) {
                Ok(event) => event,
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("Proxy protocol violation");
                    return Err(message.to_string());
                }
            };
            if let Some(event) = event {
                if matches!(
                    event,
                    AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
                ) {
                    saw_terminal_event = true;
                }
                stream.push(event);
            }
        }
    }

    if options
        .signal
        .as_ref()
        .map(|s| s.aborted())
        .unwrap_or(false)
    {
        return Err("Request aborted by user".to_string());
    }

    // 最后一条事件可能不以换行结尾：flush 剩余 buffer。
    if !buffer.is_empty() {
        let line = buffer.trim();
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty() {
                let proxy_event: ProxyAssistantMessageEvent =
                    serde_json::from_str(data).map_err(|e| e.to_string())?;
                if let Some(event) = process_proxy_event(&proxy_event, partial, &mut partial_json) {
                    if matches!(
                        event,
                        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
                    ) {
                        saw_terminal_event = true;
                    }
                    stream.push(event);
                }
            }
        }
    }

    // 对应上游：干净 EOF 但无 done/error 终结事件时，视为服务器中途丢连接。
    if !saw_terminal_event {
        partial.stop_reason = StopReason::Error;
        partial.error_message =
            Some("Connection closed by proxy server before the response completed".to_string());
        stream.push(AssistantMessageEvent::Error {
            reason: ErrorStopReason::Error,
            error: partial.clone(),
        });
    }

    stream.end(None);
    Ok(())
}
