//! Rust 翻译自 packages/ai/src/utils/text.ts

use crate::types::ContentBlock;

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
