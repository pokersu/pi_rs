//! Rust 翻译自 packages/ai/src/api/transform-messages.ts
//!
//! 跨 provider 的消息规范化（图片降级、thinking 处理、toolCall ID 规范化、孤儿
//! toolCall 合成、error/aborted 消息过滤）。

use std::collections::{HashMap, HashSet};

use crate::types::{
    AssistantMessage, ContentBlock, InputModality, Message, Model, StopReason, TextContent,
    TextKind, TextOrImageContent, ToolCall, ToolResultMessage, UserContent,
};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

/// 对应 `normalizeToolCallId` 回调类型。
pub type NormalizeToolCallId = dyn Fn(&str, &Model, &AssistantMessage) -> String + Send + Sync;

fn replace_images_with_placeholder(
    blocks: &[TextOrImageContent],
    placeholder: &str,
) -> Vec<TextOrImageContent> {
    let mut result: Vec<TextOrImageContent> = Vec::new();
    let mut previous_was_placeholder = false;

    for block in blocks {
        match block {
            TextOrImageContent::Image(_) => {
                if !previous_was_placeholder {
                    result.push(TextOrImageContent::Text(TextContent {
                        kind: TextKind,
                        text: placeholder.to_string(),
                        text_signature: None,
                    }));
                }
                previous_was_placeholder = true;
            }
            TextOrImageContent::Text(text) => {
                let is_placeholder = text.text == placeholder;
                result.push(TextOrImageContent::Text(text.clone()));
                previous_was_placeholder = is_placeholder;
            }
        }
    }

    result
}

fn downgrade_unsupported_images(messages: &[Message], model: &Model) -> Vec<Message> {
    if model.input.contains(&InputModality::Image) {
        return messages.to_vec();
    }

    messages
        .iter()
        .map(|msg| match msg {
            Message::User(user) => {
                if let UserContent::Blocks(blocks) = &user.content {
                    let mut cloned = user.clone();
                    cloned.content = UserContent::Blocks(replace_images_with_placeholder(
                        blocks,
                        NON_VISION_USER_IMAGE_PLACEHOLDER,
                    ));
                    Message::User(cloned)
                } else {
                    msg.clone()
                }
            }
            Message::ToolResult(result) => {
                let mut cloned = result.clone();
                cloned.content = replace_images_with_placeholder(
                    &cloned.content,
                    NON_VISION_TOOL_IMAGE_PLACEHOLDER,
                );
                Message::ToolResult(cloned)
            }
            _ => msg.clone(),
        })
        .collect()
}

fn transform_assistant_content(
    assistant: &AssistantMessage,
    model: &Model,
    tool_call_id_map: &mut HashMap<String, String>,
    normalize_tool_call_id: Option<&NormalizeToolCallId>,
) -> Vec<ContentBlock> {
    let is_same_model = assistant.provider == model.provider
        && assistant.api == model.api
        && assistant.model == model.id;

    let mut transformed: Vec<ContentBlock> = Vec::new();
    for block in &assistant.content {
        match block {
            ContentBlock::Thinking(thinking) => {
                if thinking.redacted.unwrap_or(false) {
                    if is_same_model {
                        transformed.push(block.clone());
                    }
                    continue;
                }
                if is_same_model && thinking.thinking_signature.is_some() {
                    transformed.push(block.clone());
                    continue;
                }
                if thinking.thinking.trim().is_empty() {
                    continue;
                }
                if is_same_model {
                    transformed.push(block.clone());
                } else {
                    transformed.push(ContentBlock::Text(TextContent {
                        kind: TextKind,
                        text: thinking.thinking.clone(),
                        text_signature: None,
                    }));
                }
            }
            ContentBlock::Text(text) => {
                if is_same_model {
                    transformed.push(block.clone());
                } else {
                    transformed.push(ContentBlock::Text(TextContent {
                        kind: TextKind,
                        text: text.text.clone(),
                        text_signature: None,
                    }));
                }
            }
            ContentBlock::ToolCall(tool_call) => {
                let mut normalized = tool_call.clone();
                if !is_same_model && normalized.thought_signature.is_some() {
                    normalized.thought_signature = None;
                }
                if !is_same_model && let Some(normalize) = normalize_tool_call_id {
                    let normalized_id = normalize(&tool_call.id, model, assistant);
                    if normalized_id != tool_call.id {
                        tool_call_id_map.insert(tool_call.id.clone(), normalized_id.clone());
                        normalized.id = normalized_id;
                    }
                }
                transformed.push(ContentBlock::ToolCall(normalized));
            }
            ContentBlock::Image(_) => {
                // assistant content 不含 image（原版类型约束），保留。
                transformed.push(block.clone());
            }
        }
    }
    transformed
}

fn insert_synthetic_tool_results(
    result: &mut Vec<Message>,
    pending_tool_calls: &mut Vec<ToolCall>,
    existing_tool_result_ids: &mut HashSet<String>,
) {
    if pending_tool_calls.is_empty() {
        return;
    }
    for tool_call in pending_tool_calls.iter() {
        if !existing_tool_result_ids.contains(&tool_call.id) {
            result.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                content: vec![TextOrImageContent::Text(TextContent {
                    kind: TextKind,
                    text: "No result provided".to_string(),
                    text_signature: None,
                })],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: true,
                timestamp: now_ms(),
            }));
        }
    }
    pending_tool_calls.clear();
    existing_tool_result_ids.clear();
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 对应 `transformMessages(messages, model, normalizeToolCallId?)`
pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<&NormalizeToolCallId>,
) -> Vec<Message> {
    // Rust 的类型系统已保证 content 非 null，故省略原版的 null 归一化步骤。
    let image_aware = downgrade_unsupported_images(messages, model);

    let mut tool_call_id_map: HashMap<String, String> = HashMap::new();
    let transformed: Vec<Message> = image_aware
        .iter()
        .map(|msg| match msg {
            Message::User(_) => msg.clone(),
            Message::ToolResult(result) => {
                if let Some(normalized_id) = tool_call_id_map.get(&result.tool_call_id)
                    && normalized_id != &result.tool_call_id
                {
                    let mut cloned = result.clone();
                    cloned.tool_call_id = normalized_id.clone();
                    Message::ToolResult(cloned)
                } else {
                    msg.clone()
                }
            }
            Message::Assistant(assistant) => {
                let mut cloned = assistant.clone();
                cloned.content = transform_assistant_content(
                    assistant,
                    model,
                    &mut tool_call_id_map,
                    normalize_tool_call_id,
                );
                Message::Assistant(cloned)
            }
        })
        .collect();

    // 第二遍：为孤儿 toolCall 插入合成空 toolResult，并跳过 error/aborted assistant。
    let mut result: Vec<Message> = Vec::new();
    let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
    let mut existing_tool_result_ids: HashSet<String> = HashSet::new();

    for msg in &transformed {
        match msg {
            Message::Assistant(assistant) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                if assistant.stop_reason == StopReason::Error
                    || assistant.stop_reason == StopReason::Aborted
                {
                    continue;
                }
                let tool_calls: Vec<ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall(tool_call) => Some(tool_call.clone()),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    pending_tool_calls = tool_calls;
                    existing_tool_result_ids = HashSet::new();
                }
                result.push(msg.clone());
            }
            Message::ToolResult(tool_result) => {
                existing_tool_result_ids.insert(tool_result.tool_call_id.clone());
                result.push(msg.clone());
            }
            Message::User(_) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                result.push(msg.clone());
            }
        }
    }
    insert_synthetic_tool_results(
        &mut result,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
    );

    result
}
