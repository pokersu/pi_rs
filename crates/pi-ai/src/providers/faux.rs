//! Rust 翻译自 packages/ai/src/providers/faux.ts（核心，省略 deferred/token 速率控制）
//!
//! Faux 是测试用 provider：把脚本化的 `AssistantMessage` 按完整的事件序列
//! （start → text/thinking/toolcall 增量 → done）回放，不依赖真实 HTTP。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::models::{CreateProviderOptions, Provider, create_provider};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, ErrorStopReason, Model, StopReason,
    StreamFunction, TerminalStopReason, TextContent, TextKind, ThinkingContent, ThinkingKind,
    ToolCall,
};
use crate::utils::error_stream::{create_error_message, default_usage};
use crate::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};

const DEFAULT_API: &str = "faux";
const DEFAULT_PROVIDER: &str = "faux";
const DEFAULT_MODEL_ID: &str = "faux-1";
const DEFAULT_MODEL_NAME: &str = "Faux Model";
const DEFAULT_BASE_URL: &str = "http://localhost:0";

/// 对应 `fauxText`
pub fn faux_text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        kind: TextKind,
        text: text.to_string(),
        text_signature: None,
    })
}

/// 对应 `fauxThinking`
pub fn faux_thinking(thinking: &str) -> ContentBlock {
    ContentBlock::Thinking(ThinkingContent {
        kind: ThinkingKind,
        thinking: thinking.to_string(),
        thinking_signature: None,
        redacted: None,
    })
}

/// 对应 `fauxToolCall`
pub fn faux_tool_call(name: &str, arguments: serde_json::Value, id: Option<&str>) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        kind: crate::types::ToolCallKind,
        id: id
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("tool-{}", crate::utils::uuid::uuidv7())),
        name: name.to_string(),
        arguments,
        thought_signature: None,
        namespace: None,
    })
}

/// 对应 `fauxAssistantMessage`（简化：直接接收 content blocks）。
pub fn faux_assistant_message(
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: DEFAULT_API.to_string(),
        provider: DEFAULT_PROVIDER.to_string(),
        model: DEFAULT_MODEL_ID.to_string(),
        response_model: None,
        response_id: None,
        usage: default_usage(),
        stop_reason,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: crate::utils::uuid::now_ms() as u64,
    }
}

/// 对应 `FauxProviderState`（简化）。
#[derive(Debug, Default)]
pub struct FauxProviderState {
    pub call_count: u64,
}

/// 对应 `streamWithDeltas` 的简化版：按块一次性回放事件序列。
pub fn stream_with_deltas(stream: &AssistantMessageEventStream, message: &AssistantMessage) {
    let mut partial = message.clone();
    partial.content = Vec::new();
    partial.stop_reason = StopReason::Pending;

    stream.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });

    for (index, block) in message.content.iter().enumerate() {
        match block {
            ContentBlock::Text(text) => {
                partial.content.push(ContentBlock::Text(TextContent {
                    kind: TextKind,
                    text: String::new(),
                    text_signature: None,
                }));
                stream.push(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                stream.push(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta: text.text.clone(),
                    partial: partial.clone(),
                });
                if let ContentBlock::Text(t) = &mut partial.content[index] {
                    t.text = text.text.clone();
                }
                stream.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content: text.text.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::Thinking(thinking) => {
                partial
                    .content
                    .push(ContentBlock::Thinking(ThinkingContent {
                        kind: ThinkingKind,
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }));
                stream.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                stream.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta: thinking.thinking.clone(),
                    partial: partial.clone(),
                });
                if let ContentBlock::Thinking(t) = &mut partial.content[index] {
                    t.thinking = thinking.thinking.clone();
                }
                stream.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content: thinking.thinking.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::ToolCall(tool_call) => {
                partial.content.push(ContentBlock::ToolCall(ToolCall {
                    kind: crate::types::ToolCallKind,
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    arguments: serde_json::Value::Object(Default::default()),
                    thought_signature: None,
                    namespace: None,
                }));
                stream.push(AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial: partial.clone(),
                });
                stream.push(AssistantMessageEvent::ToolCallDelta {
                    content_index: index,
                    delta: tool_call.arguments.to_string(),
                    partial: partial.clone(),
                });
                if let ContentBlock::ToolCall(t) = &mut partial.content[index] {
                    t.arguments = tool_call.arguments.clone();
                }
                stream.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: tool_call.clone(),
                    partial: partial.clone(),
                });
            }
            ContentBlock::Image(_) => {}
        }
    }

    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            stream.push(AssistantMessageEvent::Error {
                reason: if message.stop_reason == StopReason::Aborted {
                    ErrorStopReason::Aborted
                } else {
                    ErrorStopReason::Error
                },
                error: message.clone(),
            });
            stream.end(Some(message.clone()));
        }
        _ => {
            let reason = match message.stop_reason {
                StopReason::Length => TerminalStopReason::Length,
                StopReason::ToolUse => TerminalStopReason::ToolUse,
                StopReason::Deferred => TerminalStopReason::Deferred,
                _ => TerminalStopReason::Stop,
            };
            stream.push(AssistantMessageEvent::Done {
                reason,
                message: message.clone(),
            });
            stream.end(Some(message.clone()));
        }
    }
}

/// 对应 `FauxProviderHandle`。
pub struct FauxProviderHandle {
    pub provider: Arc<dyn Provider>,
    pub state: Arc<Mutex<FauxProviderState>>,
    set_responses: Arc<dyn Fn(Vec<AssistantMessage>) + Send + Sync>,
    get_pending_response_count: Arc<dyn Fn() -> usize + Send + Sync>,
}

impl FauxProviderHandle {
    pub fn set_responses(&self, responses: Vec<AssistantMessage>) {
        (self.set_responses)(responses);
    }

    pub fn get_pending_response_count(&self) -> usize {
        (self.get_pending_response_count)()
    }
}

/// 对应 `fauxProvider`：构造一个测试 provider，默认单模型 `faux-1`。
pub fn faux_provider(models: Vec<Model>) -> FauxProviderHandle {
    let models = if models.is_empty() {
        vec![Model {
            id: DEFAULT_MODEL_ID.to_string(),
            name: DEFAULT_MODEL_NAME.to_string(),
            api: DEFAULT_API.to_string(),
            provider: DEFAULT_PROVIDER.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![
                crate::types::InputModality::Text,
                crate::types::InputModality::Image,
            ],
            cost: crate::types::ModelCost {
                rates: crate::types::ModelCostRates {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                tiers: None,
            },
            context_window: 128_000,
            max_tokens: 4096,
            sampling_params: None,
            headers: None,
            compat: None,
        }]
    } else {
        models
    };

    let pending: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::new(Mutex::new(VecDeque::new()));
    let state: Arc<Mutex<FauxProviderState>> = Arc::new(Mutex::new(FauxProviderState::default()));

    let stream: StreamFunction = {
        let pending = pending.clone();
        let state = state.clone();
        Arc::new(move |model, _context, _options| {
            let outer = create_assistant_message_event_stream();
            let producer = outer.clone();
            let step = pending.lock().unwrap().pop_front();
            state.lock().unwrap().call_count += 1;
            let model = model.clone();
            tokio::spawn(async move {
                match step {
                    None => {
                        let error = create_error_message(
                            "No more faux responses queued",
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
                    Some(message) => {
                        stream_with_deltas(&producer, &message);
                    }
                }
            });
            outer
        })
    };

    let provider = create_provider(CreateProviderOptions {
        id: DEFAULT_PROVIDER.to_string(),
        name: Some("Faux".to_string()),
        base_url: Some(DEFAULT_BASE_URL.to_string()),
        models: models.clone(),
        stream,
    });

    let set_responses: Arc<dyn Fn(Vec<AssistantMessage>) + Send + Sync> = {
        let pending = pending.clone();
        Arc::new(move |responses| {
            *pending.lock().unwrap() = responses.into_iter().collect();
        })
    };
    let get_pending_response_count: Arc<dyn Fn() -> usize + Send + Sync> = {
        let pending = pending.clone();
        Arc::new(move || pending.lock().unwrap().len())
    };

    FauxProviderHandle {
        provider,
        state,
        set_responses,
        get_pending_response_count,
    }
}

/// 便于直接通过 `Models` 使用 faux 的最小入口（返回 provider + 默认模型）。
pub fn faux_default_model() -> Model {
    let handle = faux_provider(Vec::new());
    handle.provider.get_models().into_iter().next().unwrap()
}
