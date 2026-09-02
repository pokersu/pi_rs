//! Rust 翻译自 packages/ai/src/utils/estimate.ts
//!
//! 估算 context tokens（用于 max-tokens 收缩等场景）。

use std::collections::HashSet;

use crate::types::{
    ContentBlock, Context, Message, StopReason, TextOrImageContent, Tool, Usage, UserContent,
};

/// 对应 `ContextUsageEstimate`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    /// 估算的总 context tokens。
    pub tokens: u64,
    /// 最近一次可用的 assistant usage 块报告的 tokens。
    pub usage_tokens: u64,
    /// 最近一次可用 assistant usage 块之后的估算 tokens。
    pub trailing_tokens: u64,
    /// 提供 usage 的可应用消息索引，不存在时为 None。
    pub last_usage_index: Option<usize>,
}

const CHARS_PER_TOKEN: u64 = 4;
const ESTIMATED_IMAGE_CHARS: u64 = 4800;

/// 对应 `calculateContextTokens(usage)`
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn safe_json_stringify<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_string())
}

fn estimate_blocks_chars(blocks: &[TextOrImageContent]) -> u64 {
    blocks
        .iter()
        .map(|block| match block {
            TextOrImageContent::Text(text) => text.text.len() as u64,
            TextOrImageContent::Image(_) => ESTIMATED_IMAGE_CHARS,
        })
        .sum()
}

/// 对应 `estimateTextTokens(text)`
pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(CHARS_PER_TOKEN)
}

/// 对应 `estimateTextAndImageContentTokens(content)`（数组形式）。
pub fn estimate_text_and_image_content_tokens(blocks: &[TextOrImageContent]) -> u64 {
    estimate_blocks_chars(blocks).div_ceil(CHARS_PER_TOKEN)
}

/// 对应 `estimateMessageTokens(message)`
pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => estimate_text_tokens(text),
            UserContent::Blocks(blocks) => estimate_text_and_image_content_tokens(blocks),
        },
        Message::ToolResult(result) => estimate_text_and_image_content_tokens(&result.content),
        Message::Assistant(assistant) => {
            let mut chars = 0u64;
            for block in &assistant.content {
                match block {
                    ContentBlock::Text(text) => chars += text.text.len() as u64,
                    ContentBlock::Thinking(thinking) => chars += thinking.thinking.len() as u64,
                    ContentBlock::Image(_) => chars += ESTIMATED_IMAGE_CHARS,
                    ContentBlock::ToolCall(call) => {
                        chars += call.name.len() as u64
                            + safe_json_stringify(&call.arguments).len() as u64;
                    }
                }
            }
            chars.div_ceil(CHARS_PER_TOKEN)
        }
    }
}

fn message_timestamp(message: &Message) -> u64 {
    match message {
        Message::User(user) => user.timestamp,
        Message::Assistant(assistant) => assistant.timestamp,
        Message::ToolResult(result) => result.timestamp,
    }
}

fn get_last_assistant_usage_info(messages: &[Message]) -> Option<(Usage, usize)> {
    let mut latest_prefix_timestamp: u64 = 0;
    let mut usage_info: Option<(Usage, usize)> = None;

    for (i, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            // 该响应之后插入了更新的前缀消息（例如 compaction 摘要），其 usage 无法描述当前前缀。
            let usage_applies_to_prefix = assistant.timestamp >= latest_prefix_timestamp;
            if usage_applies_to_prefix
                && assistant.stop_reason != StopReason::Aborted
                && assistant.stop_reason != StopReason::Error
                && calculate_context_tokens(&assistant.usage) > 0
            {
                usage_info = Some((assistant.usage.clone(), i));
            }
        }
        latest_prefix_timestamp = latest_prefix_timestamp.max(message_timestamp(message));
    }

    usage_info
}

fn estimate_messages(messages: &[Message]) -> ContextUsageEstimate {
    if let Some((usage, index)) = get_last_assistant_usage_info(messages) {
        let usage_tokens = calculate_context_tokens(&usage);
        let mut trailing_tokens = 0u64;
        for message in &messages[index + 1..] {
            trailing_tokens += estimate_message_tokens(message);
        }
        return ContextUsageEstimate {
            tokens: usage_tokens + trailing_tokens,
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(index),
        };
    }

    let mut tokens = 0u64;
    for message in messages {
        tokens += estimate_message_tokens(message);
    }
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

fn estimate_tools_tokens(tools: &[Tool]) -> u64 {
    if tools.is_empty() {
        return 0;
    }
    estimate_text_tokens(&safe_json_stringify(tools))
}

/// 对应 `estimateContextTokens` 的入参（`Context | readonly Message[]`）。
pub enum ContextInput<'a> {
    Context(&'a Context),
    Messages(&'a [Message]),
}

/// 对应 `estimateContextTokens(context)`
pub fn estimate_context_tokens(input: ContextInput<'_>) -> ContextUsageEstimate {
    match input {
        ContextInput::Messages(messages) => estimate_messages(messages),
        ContextInput::Context(context) => {
            let estimate = estimate_messages(&context.messages);
            if let Some(last_usage_index) = estimate.last_usage_index {
                let mut added_names: HashSet<String> = HashSet::new();
                for message in &context.messages[last_usage_index + 1..] {
                    if let Message::ToolResult(result) = message
                        && let Some(names) = &result.added_tool_names
                    {
                        added_names.extend(names.iter().cloned());
                    }
                }
                let added_tools: Vec<Tool> = context
                    .tools
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter(|tool| added_names.contains(&tool.name))
                    .cloned()
                    .collect();
                let added_tool_tokens = estimate_tools_tokens(&added_tools);
                return ContextUsageEstimate {
                    tokens: estimate.tokens + added_tool_tokens,
                    usage_tokens: estimate.usage_tokens,
                    trailing_tokens: estimate.trailing_tokens + added_tool_tokens,
                    last_usage_index: estimate.last_usage_index,
                };
            }

            let prefix_tokens = context
                .system_prompt
                .as_deref()
                .map(estimate_text_tokens)
                .unwrap_or(0)
                + estimate_tools_tokens(context.tools.as_deref().unwrap_or(&[]));
            ContextUsageEstimate {
                tokens: estimate.tokens + prefix_tokens,
                usage_tokens: estimate.usage_tokens,
                trailing_tokens: estimate.trailing_tokens + prefix_tokens,
                last_usage_index: estimate.last_usage_index,
            }
        }
    }
}
