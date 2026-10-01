//! Rust 翻译自 packages/ai/src/utils/text.ts

use crate::types::{ContentBlock, SystemContent, SystemMessage};

/// 对应 `contentText` 的输入（`string | readonly Content[]`）。
pub enum ContentTextInput<'a> {
    Str(&'a str),
    Blocks(&'a [ContentBlock]),
}

/// 对应 `contentText(content, separator = "\n")`：从消息内容中提取并拼接文本。
pub fn content_text(content: ContentTextInput<'_>, separator: &str) -> String {
    match content {
        ContentTextInput::Str(s) => s.to_string(),
        ContentTextInput::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(separator),
    }
}

/// `SystemContent` 的文本投影（对应 `contentText(systemMessage.content)`）。
pub fn system_content_text(content: &SystemContent) -> String {
    match content {
        SystemContent::Text(text) => text.clone(),
        SystemContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// 对应 `getSystemMessageText`：把 system 消息渲染成一段文本（用于首条/逐步升级场景）。
pub fn get_system_message_text(message: &SystemMessage) -> String {
    let mut parts: Vec<String> = vec![system_content_text(&message.content)];
    if let Some(sections) = &message.sections {
        for text in sections.values().flatten() {
            parts.push(text.clone());
        }
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 对应 `renderSystemMessageUpdate`：把中途插入的 system 消息渲染成带分节名的更新文本。
///
/// 该包装仅存在于请求时，版本间可能变化（与上游注释一致）。
pub fn render_system_message_update(message: &SystemMessage) -> String {
    let mut parts: Vec<String> = Vec::new();
    let text = system_content_text(&message.content);
    if !text.is_empty() {
        parts.push(text);
    }
    if let Some(sections) = &message.sections {
        for (name, value) in sections {
            parts.push(match value {
                None => format!("Removed system prompt section \"{name}\"."),
                Some(value) => format!("Updated system prompt section \"{name}\":\n\n{value}"),
            });
        }
    }
    parts.join("\n\n")
}
