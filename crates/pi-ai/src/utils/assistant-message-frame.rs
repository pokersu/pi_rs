//! Rust 翻译自 packages/ai/src/utils/assistant-message-frame.ts
//!
//! 将 assistant 流编码为可重放的紧凑 frame（用于 durable assistant 消息持久化），
//! 以及把 frame 序列还原回 partial message。终态结算（done/error）不包含在 frame 中，
//! 需单独持久化。
//!
//! 说明：frame 的 `type` tag 与 TS 原版逐字对齐；字段名沿用 Rust snake_case 惯例
//! （原版为 camelCase），仅用于本 crate 内部流转，不跨语言互通。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, StopReason, TextContent,
    ThinkingContent, ToolCall,
};
use crate::utils::json_parse::parse_streaming_json;

/// 对应 `AssistantMessageFrame`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AssistantMessageFrame {
    #[serde(rename = "start")]
    Start { partial: Box<AssistantMessage> },
    #[serde(rename = "text_start")]
    TextStart {
        content_index: usize,
        content: TextContent,
    },
    #[serde(rename = "text_delta")]
    TextDelta { content_index: usize, delta: String },
    #[serde(rename = "text_end")]
    TextEnd {
        content_index: usize,
        content: String,
        text_signature: Option<String>,
    },
    #[serde(rename = "thinking_start")]
    ThinkingStart {
        content_index: usize,
        content: ThinkingContent,
    },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { content_index: usize, delta: String },
    #[serde(rename = "thinking_end")]
    ThinkingEnd {
        content_index: usize,
        content: String,
        thinking_signature: Option<String>,
        redacted: Option<bool>,
    },
    #[serde(rename = "toolcall_start")]
    ToolCallStart {
        content_index: usize,
        tool_call: ToolCall,
    },
    #[serde(rename = "toolcall_checkpoint")]
    ToolCallCheckpoint { content_index: usize, json: String },
    #[serde(rename = "toolcall_delta")]
    ToolCallDelta { content_index: usize, delta: String },
    #[serde(rename = "toolcall_end")]
    ToolCallEnd {
        content_index: usize,
        id: String,
        name: String,
        arguments: Value,
        thought_signature: Option<String>,
        namespace: Option<String>,
    },
}

/// 对应 `EncoderBlockState`。
enum EncoderBlockState {
    Text {
        covered_chars: usize,
        delta_chars: usize,
    },
    Thinking {
        covered_chars: usize,
        delta_chars: usize,
    },
    ToolCall {
        caught_up: bool,
        catchup_json: String,
        snapshot_arguments: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EncoderBlockKind {
    Text,
    Thinking,
    ToolCall,
}

impl EncoderBlockKind {
    fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Thinking => "thinking",
            Self::ToolCall => "toolCall",
        }
    }
}

impl EncoderBlockState {
    fn kind(&self) -> EncoderBlockKind {
        match self {
            Self::Text { .. } => EncoderBlockKind::Text,
            Self::Thinking { .. } => EncoderBlockKind::Thinking,
            Self::ToolCall { .. } => EncoderBlockKind::ToolCall,
        }
    }
}

/// 对应 `ReducerBlockState`。
enum ReducerBlockState {
    Text { ended: bool },
    Thinking { ended: bool },
    ToolCall { ended: bool, json: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReducerBlockKind {
    Text,
    Thinking,
    ToolCall,
}

impl ReducerBlockKind {
    fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Thinking => "thinking",
            Self::ToolCall => "toolCall",
        }
    }
}

impl ReducerBlockState {
    fn kind(&self) -> ReducerBlockKind {
        match self {
            Self::Text { .. } => ReducerBlockKind::Text,
            Self::Thinking { .. } => ReducerBlockKind::Thinking,
            Self::ToolCall { .. } => ReducerBlockKind::ToolCall,
        }
    }

    fn is_ended(&self) -> bool {
        match self {
            Self::Text { ended } | Self::Thinking { ended } | Self::ToolCall { ended, .. } => {
                *ended
            }
        }
    }
}

/// 对应 `cloneTextContent`。Rust 侧 `Option` 已表达「缺失」，直接 clone。
fn clone_text_content(content: &TextContent) -> TextContent {
    content.clone()
}

/// 对应 `cloneThinkingContent`。
fn clone_thinking_content(content: &ThinkingContent) -> ThinkingContent {
    content.clone()
}

/// 对应 `cloneToolCall`。
fn clone_tool_call(tool_call: &ToolCall) -> ToolCall {
    tool_call.clone()
}

/// 对应 `cloneStartMessage`。
fn clone_start_message(message: &AssistantMessage) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: message.api.clone(),
        provider: message.provider.clone(),
        model: message.model.clone(),
        response_model: message.response_model.clone(),
        response_id: message.response_id.clone(),
        provider_thinking_level: message.provider_thinking_level.clone(),
        usage: message.usage.clone(),
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: message.timestamp,
    }
}

/// 对应 `assertContentIndex`。
fn assert_content_index(content_index: usize) {
    // Number.isSafeInteger(contentIndex) 在 Rust 的 usize 下天然满足。
    let _ = content_index;
}

/// 对应 `serializedArguments`。
fn serialized_arguments(arguments_value: &Value) -> String {
    serde_json::to_string(arguments_value).expect("Tool-call arguments are not JSON-serializable")
}

/// 对应 `EMPTY_PARSED_TOOL_ARGUMENTS`：`parseStreamingJson("")` 序列化后为空对象 `{}`。
const EMPTY_PARSED_TOOL_ARGUMENTS: &str = "{}";

/// 对应 `isJsonPrefix`。
fn is_json_prefix(snapshot: &Value, current: &Value) -> bool {
    match snapshot {
        Value::String(s) => matches!(current, Value::String(c) if c.starts_with(s)),
        Value::Array(snapshot_arr) => match current {
            Value::Array(current_arr) => {
                snapshot_arr.len() <= current_arr.len()
                    && snapshot_arr
                        .iter()
                        .zip(current_arr.iter())
                        .all(|(s, c)| is_json_prefix(s, c))
            }
            _ => false,
        },
        Value::Object(snapshot_obj) => match current {
            Value::Object(current_obj) => snapshot_obj.iter().all(|(key, value)| {
                current_obj.contains_key(key) && is_json_prefix(value, &current_obj[key])
            }),
            _ => false,
        },
        // null / bool / number：严格相等（JSON 无 NaN，== 与 Object.is 等价）。
        _ => snapshot == current,
    }
}

fn event_type_name(event: &AssistantMessageEvent) -> &'static str {
    match event {
        AssistantMessageEvent::Start { .. } => "start",
        AssistantMessageEvent::TextStart { .. } => "text_start",
        AssistantMessageEvent::TextDelta { .. } => "text_delta",
        AssistantMessageEvent::TextEnd { .. } => "text_end",
        AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
        AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
        AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
    }
}

/// 对应 `eventBlock`：从事件中取出指定 contentIndex 的内容块。
fn event_block<'a>(
    partial: &'a AssistantMessage,
    content_index: usize,
    event_type: &str,
) -> &'a ContentBlock {
    assert_content_index(content_index);
    partial.content.get(content_index).unwrap_or_else(|| {
        panic!("{event_type} event has no content block at index {content_index}")
    })
}

/// 对应 `AssistantMessageFrameEncoder`。
#[derive(Default)]
pub struct AssistantMessageFrameEncoder {
    started: bool,
    terminal: bool,
    blocks: HashMap<usize, EncoderBlockState>,
}

impl AssistantMessageFrameEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 对应 `encode(event)`。
    pub fn encode(&mut self, event: &AssistantMessageEvent) -> Option<AssistantMessageFrame> {
        if self.terminal {
            panic!(
                "Assistant message event {} follows a terminal event",
                event_type_name(event)
            );
        }

        match event {
            AssistantMessageEvent::Start { partial } => {
                if self.started {
                    panic!("Assistant message stream contains more than one start event");
                }
                self.started = true;
                Some(AssistantMessageFrame::Start {
                    partial: Box::new(clone_start_message(partial)),
                })
            }
            AssistantMessageEvent::Done { .. } => {
                if !self.started {
                    panic!("Assistant message done event appears before start");
                }
                self.terminal = true;
                None
            }
            AssistantMessageEvent::Error { .. } => {
                self.terminal = true;
                None
            }
            _ => {
                if !self.started {
                    panic!(
                        "Assistant message {} event appears before start",
                        event_type_name(event)
                    );
                }
                self.encode_content_event(event)
            }
        }
    }

    fn encode_content_event(
        &mut self,
        event: &AssistantMessageEvent,
    ) -> Option<AssistantMessageFrame> {
        match event {
            AssistantMessageEvent::TextStart {
                content_index,
                partial,
            } => {
                let block = event_block(partial, *content_index, "text_start");
                let ContentBlock::Text(content) = block else {
                    panic!("text_start event points to a non-text block at index {content_index}");
                };
                self.start_block(
                    *content_index,
                    EncoderBlockState::Text {
                        covered_chars: content.text.chars().count(),
                        delta_chars: 0,
                    },
                );
                Some(AssistantMessageFrame::TextStart {
                    content_index: *content_index,
                    content: clone_text_content(content),
                })
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => self.encode_text_delta(*content_index, delta, EncoderBlockKind::Text),
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                partial,
            } => {
                let block = event_block(partial, *content_index, "text_end");
                let ContentBlock::Text(existing) = block else {
                    panic!("text_end event points to a non-text block at index {content_index}");
                };
                self.end_block(*content_index, EncoderBlockKind::Text);
                Some(AssistantMessageFrame::TextEnd {
                    content_index: *content_index,
                    content: content.clone(),
                    text_signature: existing.text_signature.clone(),
                })
            }
            AssistantMessageEvent::ThinkingStart {
                content_index,
                partial,
            } => {
                let block = event_block(partial, *content_index, "thinking_start");
                let ContentBlock::Thinking(content) = block else {
                    panic!(
                        "thinking_start event points to a non-thinking block at index {content_index}"
                    );
                };
                self.start_block(
                    *content_index,
                    EncoderBlockState::Thinking {
                        covered_chars: content.thinking.chars().count(),
                        delta_chars: 0,
                    },
                );
                Some(AssistantMessageFrame::ThinkingStart {
                    content_index: *content_index,
                    content: clone_thinking_content(content),
                })
            }
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => self.encode_text_delta(*content_index, delta, EncoderBlockKind::Thinking),
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                partial,
            } => {
                let block = event_block(partial, *content_index, "thinking_end");
                let ContentBlock::Thinking(existing) = block else {
                    panic!(
                        "thinking_end event points to a non-thinking block at index {content_index}"
                    );
                };
                self.end_block(*content_index, EncoderBlockKind::Thinking);
                Some(AssistantMessageFrame::ThinkingEnd {
                    content_index: *content_index,
                    content: content.clone(),
                    thinking_signature: existing.thinking_signature.clone(),
                    redacted: existing.redacted,
                })
            }
            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => {
                let block = event_block(partial, *content_index, "toolcall_start");
                let ContentBlock::ToolCall(content) = block else {
                    panic!(
                        "toolcall_start event points to a non-toolCall block at index {content_index}"
                    );
                };
                let snapshot_arguments = serialized_arguments(&content.arguments);
                let caught_up = snapshot_arguments == EMPTY_PARSED_TOOL_ARGUMENTS;
                self.start_block(
                    *content_index,
                    EncoderBlockState::ToolCall {
                        caught_up,
                        catchup_json: String::new(),
                        snapshot_arguments: if caught_up {
                            String::new()
                        } else {
                            snapshot_arguments
                        },
                    },
                );
                Some(AssistantMessageFrame::ToolCallStart {
                    content_index: *content_index,
                    tool_call: clone_tool_call(content),
                })
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => {
                let state = self.block(*content_index, EncoderBlockKind::ToolCall);
                let EncoderBlockState::ToolCall {
                    caught_up,
                    catchup_json,
                    snapshot_arguments,
                } = state
                else {
                    panic!("Unreachable tool-call encoder state");
                };
                if *caught_up {
                    return if delta.is_empty() {
                        None
                    } else {
                        Some(AssistantMessageFrame::ToolCallDelta {
                            content_index: *content_index,
                            delta: delta.clone(),
                        })
                    };
                }
                catchup_json.push_str(delta);
                let arguments_value = parse_streaming_json(Some(catchup_json));
                if serialized_arguments(&arguments_value) != *snapshot_arguments {
                    // 旧 grammar 调用在 toolcall_start 已含初始入参，但其 JSON delta 流仍从空输入开始。
                    // 因此解析出的参数可能是 start 快照的扩展而非精确复现。
                    let snapshot_arguments_value = parse_streaming_json(Some(snapshot_arguments));
                    if !is_json_prefix(&snapshot_arguments_value, &arguments_value) {
                        return None;
                    }
                }
                *caught_up = true;
                snapshot_arguments.clear();
                let json = std::mem::take(catchup_json);
                if json.is_empty() {
                    None
                } else {
                    Some(AssistantMessageFrame::ToolCallCheckpoint {
                        content_index: *content_index,
                        json,
                    })
                }
            }
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            } => {
                let block = event_block(partial, *content_index, "toolcall_end");
                let ContentBlock::ToolCall(_) = block else {
                    panic!(
                        "toolcall_end event points to a non-toolCall block at index {content_index}"
                    );
                };
                self.end_block(*content_index, EncoderBlockKind::ToolCall);
                Some(AssistantMessageFrame::ToolCallEnd {
                    content_index: *content_index,
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    arguments: tool_call.arguments.clone(),
                    thought_signature: tool_call.thought_signature.clone(),
                    namespace: tool_call.namespace.clone(),
                })
            }
            _ => None,
        }
    }

    fn start_block(&mut self, content_index: usize, state: EncoderBlockState) {
        assert_content_index(content_index);
        if self.blocks.contains_key(&content_index) {
            panic!("Assistant message block {content_index} starts more than once");
        }
        self.blocks.insert(content_index, state);
    }

    fn block(&mut self, content_index: usize, kind: EncoderBlockKind) -> &mut EncoderBlockState {
        assert_content_index(content_index);
        let state = self.blocks.get_mut(&content_index).unwrap_or_else(|| {
            panic!(
                "Assistant message {} block {content_index} has not started",
                kind.name()
            )
        });
        if state.kind() != kind {
            panic!(
                "Assistant message block {content_index} is {}, not {}",
                state.kind().name(),
                kind.name()
            );
        }
        state
    }

    fn end_block(&mut self, content_index: usize, kind: EncoderBlockKind) {
        self.block(content_index, kind);
        self.blocks.remove(&content_index);
    }

    fn encode_text_delta(
        &mut self,
        content_index: usize,
        delta: &str,
        kind: EncoderBlockKind,
    ) -> Option<AssistantMessageFrame> {
        let state = self.block(content_index, kind);
        let (covered_chars, delta_chars) = match state {
            EncoderBlockState::Text {
                covered_chars,
                delta_chars,
            }
            | EncoderBlockState::Thinking {
                covered_chars,
                delta_chars,
            } => (*covered_chars, delta_chars),
            EncoderBlockState::ToolCall { .. } => panic!("Unreachable text encoder state"),
        };
        let delta_start = *delta_chars;
        *delta_chars += delta.chars().count();
        let covered = covered_chars.saturating_sub(delta_start);
        if covered >= delta.chars().count() {
            return None;
        }
        let uncovered: String = if covered == 0 {
            delta.to_string()
        } else {
            delta.chars().skip(covered).collect()
        };
        match kind {
            EncoderBlockKind::Text => Some(AssistantMessageFrame::TextDelta {
                content_index,
                delta: uncovered,
            }),
            EncoderBlockKind::Thinking => Some(AssistantMessageFrame::ThinkingDelta {
                content_index,
                delta: uncovered,
            }),
            EncoderBlockKind::ToolCall => panic!("Unreachable text encoder state"),
        }
    }
}

/// 对应 `appendBlock`。
fn append_block(
    message: &mut AssistantMessage,
    states: &mut HashMap<usize, ReducerBlockState>,
    content_index: usize,
    block: ContentBlock,
    state: ReducerBlockState,
) {
    assert_content_index(content_index);
    if content_index != message.content.len() {
        let reason = if content_index < message.content.len() {
            "already exists"
        } else {
            "would leave a gap"
        };
        panic!("Cannot start assistant message block at index {content_index}: {reason}");
    }
    message.content.push(block);
    states.insert(content_index, state);
}

/// 对应 `activeBlock`。
fn active_block<'a>(
    message: &'a mut AssistantMessage,
    states: &'a mut HashMap<usize, ReducerBlockState>,
    content_index: usize,
    expected_kind: ReducerBlockKind,
    frame_type: &str,
) -> (&'a mut ContentBlock, &'a mut ReducerBlockState) {
    assert_content_index(content_index);
    let state = states.get_mut(&content_index).unwrap_or_else(|| {
        panic!("{frame_type} frame has no started block at index {content_index}")
    });
    let block = message.content.get_mut(content_index).unwrap_or_else(|| {
        panic!("{frame_type} frame has no started block at index {content_index}")
    });
    let block_kind = match block {
        ContentBlock::Text(_) => ReducerBlockKind::Text,
        ContentBlock::Thinking(_) => ReducerBlockKind::Thinking,
        ContentBlock::Image(_) => panic!("Unreachable image block in assistant message frames"),
        ContentBlock::ToolCall(_) => ReducerBlockKind::ToolCall,
    };
    if state.kind() != expected_kind || block_kind != expected_kind {
        panic!(
            "{frame_type} frame expected {} block at index {content_index}, found {}",
            expected_kind.name(),
            block_kind.name()
        );
    }
    if state.is_ended() {
        panic!("{frame_type} frame follows the end of block at index {content_index}");
    }
    (block, state)
}

fn frame_type_name(frame: &AssistantMessageFrame) -> &'static str {
    match frame {
        AssistantMessageFrame::Start { .. } => "start",
        AssistantMessageFrame::TextStart { .. } => "text_start",
        AssistantMessageFrame::TextDelta { .. } => "text_delta",
        AssistantMessageFrame::TextEnd { .. } => "text_end",
        AssistantMessageFrame::ThinkingStart { .. } => "thinking_start",
        AssistantMessageFrame::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageFrame::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageFrame::ToolCallStart { .. } => "toolcall_start",
        AssistantMessageFrame::ToolCallCheckpoint { .. } => "toolcall_checkpoint",
        AssistantMessageFrame::ToolCallDelta { .. } => "toolcall_delta",
        AssistantMessageFrame::ToolCallEnd { .. } => "toolcall_end",
    }
}

/// 对应 `reduceAssistantMessageFrames`。
pub fn reduce_assistant_message_frames(
    frames: impl IntoIterator<Item = AssistantMessageFrame>,
) -> Option<AssistantMessage> {
    let mut message: Option<AssistantMessage> = None;
    let mut frame_before_start: Option<&'static str> = None;
    let mut states: HashMap<usize, ReducerBlockState> = HashMap::new();

    for frame in frames {
        if matches!(frame, AssistantMessageFrame::Start { .. }) {
            let AssistantMessageFrame::Start { partial } = frame else {
                unreachable!()
            };
            if message.is_some() {
                panic!("Assistant message frame sequence contains more than one start frame");
            }
            if let Some(previous) = frame_before_start {
                panic!("{previous} frame appears before the start frame");
            }
            message = Some((*partial).clone());
            continue;
        }

        let frame_type = frame_type_name(&frame);
        let Some(msg) = message.as_mut() else {
            if frame_before_start.is_none() {
                frame_before_start = Some(frame_type);
            }
            continue;
        };

        reduce_content_frame(msg, &mut states, frame);
    }

    let message = message?;
    for (content_index, state) in states {
        let ReducerBlockState::ToolCall { ended, json } = state else {
            continue;
        };
        if ended || json.is_empty() {
            continue;
        }
        let Some(ContentBlock::ToolCall(block)) = message.content.get(content_index) else {
            panic!("Unreachable tool-call frame state");
        };
        let mut block = block.clone();
        block.arguments = parse_streaming_json(Some(&json));
    }

    Some(message)
}

fn reduce_content_frame(
    message: &mut AssistantMessage,
    states: &mut HashMap<usize, ReducerBlockState>,
    frame: AssistantMessageFrame,
) {
    match frame {
        AssistantMessageFrame::TextStart {
            content_index,
            content,
        } => {
            append_block(
                message,
                states,
                content_index,
                ContentBlock::Text(content),
                ReducerBlockState::Text { ended: false },
            );
        }
        AssistantMessageFrame::TextDelta {
            content_index,
            delta,
        } => {
            let (block, _state) = active_block(
                message,
                states,
                content_index,
                ReducerBlockKind::Text,
                "text_delta",
            );
            let ContentBlock::Text(text) = block else {
                panic!("Unreachable text frame state");
            };
            text.text.push_str(&delta);
        }
        AssistantMessageFrame::TextEnd {
            content_index,
            content,
            text_signature,
        } => {
            let (block, state) = active_block(
                message,
                states,
                content_index,
                ReducerBlockKind::Text,
                "text_end",
            );
            let ContentBlock::Text(text) = block else {
                panic!("Unreachable text frame state");
            };
            text.text = content;
            text.text_signature = text_signature;
            if let ReducerBlockState::Text { ended } = state {
                *ended = true;
            }
        }
        AssistantMessageFrame::ThinkingStart {
            content_index,
            content,
        } => {
            append_block(
                message,
                states,
                content_index,
                ContentBlock::Thinking(content),
                ReducerBlockState::Thinking { ended: false },
            );
        }
        AssistantMessageFrame::ThinkingDelta {
            content_index,
            delta,
        } => {
            let (block, _state) = active_block(
                message,
                states,
                content_index,
                ReducerBlockKind::Thinking,
                "thinking_delta",
            );
            let ContentBlock::Thinking(thinking) = block else {
                panic!("Unreachable thinking frame state");
            };
            thinking.thinking.push_str(&delta);
        }
        AssistantMessageFrame::ThinkingEnd {
            content_index,
            content,
            thinking_signature,
            redacted,
        } => {
            let (block, state) = active_block(
                message,
                states,
                content_index,
                ReducerBlockKind::Thinking,
                "thinking_end",
            );
            let ContentBlock::Thinking(thinking) = block else {
                panic!("Unreachable thinking frame state");
            };
            thinking.thinking = content;
            thinking.thinking_signature = thinking_signature;
            thinking.redacted = redacted;
            if let ReducerBlockState::Thinking { ended } = state {
                *ended = true;
            }
        }
        AssistantMessageFrame::ToolCallStart {
            content_index,
            tool_call,
        } => {
            append_block(
                message,
                states,
                content_index,
                ContentBlock::ToolCall(tool_call),
                ReducerBlockState::ToolCall {
                    ended: false,
                    json: String::new(),
                },
            );
        }
        AssistantMessageFrame::ToolCallCheckpoint {
            content_index,
            json,
        } => {
            let (block, state) = active_block(
                message,
                states,
                content_index,
                ReducerBlockKind::ToolCall,
                "toolcall_checkpoint",
            );
            let (
                ContentBlock::ToolCall(tool_call),
                ReducerBlockState::ToolCall {
                    json: state_json, ..
                },
            ) = (block, state)
            else {
                panic!("Unreachable tool-call checkpoint state");
            };
            *state_json = json.clone();
            tool_call.arguments = parse_streaming_json(Some(&json));
        }
        AssistantMessageFrame::ToolCallDelta {
            content_index,
            delta,
        } => {
            let (block, state) = active_block(
                message,
                states,
                content_index,
                ReducerBlockKind::ToolCall,
                "toolcall_delta",
            );
            let (
                ContentBlock::ToolCall(_),
                ReducerBlockState::ToolCall {
                    json: state_json, ..
                },
            ) = (block, state)
            else {
                panic!("Unreachable tool-call frame state");
            };
            state_json.push_str(&delta);
        }
        AssistantMessageFrame::ToolCallEnd {
            content_index,
            id,
            name,
            arguments,
            thought_signature,
            namespace,
        } => {
            let (block, state) = active_block(
                message,
                states,
                content_index,
                ReducerBlockKind::ToolCall,
                "toolcall_end",
            );
            let (ContentBlock::ToolCall(tool_call), ReducerBlockState::ToolCall { ended, .. }) =
                (block, state)
            else {
                panic!("Unreachable tool-call frame state");
            };
            tool_call.id = id;
            tool_call.name = name;
            tool_call.arguments = arguments;
            tool_call.thought_signature = thought_signature;
            tool_call.namespace = namespace;
            *ended = true;
        }
        AssistantMessageFrame::Start { .. } => unreachable!(),
    }
}
