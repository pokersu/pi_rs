//! Rust 翻译自 packages/agent/src/harness/compaction/utils.ts

use std::collections::HashSet;

use pi_ai::utils::text::{ContentTextInput, content_text};
use pi_ai::{ContentBlock, Message};

use crate::types::AgentMessage;

/// 对应 `FileOperations`
#[derive(Debug, Clone, Default)]
pub struct FileOperations {
    pub read: HashSet<String>,
    pub written: HashSet<String>,
    pub edited: HashSet<String>,
}

/// 对应 `createFileOps`
pub fn create_file_ops() -> FileOperations {
    FileOperations::default()
}

/// 对应 `extractFileOpsFromMessage`
pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    let AgentMessage::Assistant(assistant) = message else {
        return;
    };
    for block in &assistant.content {
        let ContentBlock::ToolCall(tool_call) = block else {
            continue;
        };
        let Some(path) = tool_call.arguments.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        match tool_call.name.as_str() {
            "read" => {
                file_ops.read.insert(path.to_string());
            }
            "write" => {
                file_ops.written.insert(path.to_string());
            }
            "edit" => {
                file_ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

/// 对应 `computeFileLists`
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: HashSet<String> = file_ops.edited.union(&file_ops.written).cloned().collect();
    let mut read_only: Vec<String> = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(*f))
        .cloned()
        .collect();
    read_only.sort();
    let mut modified_files: Vec<String> = modified.into_iter().collect();
    modified_files.sort();
    (read_only, modified_files)
}

/// 对应 `formatFileOperations`
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

const TOOL_RESULT_MAX_CHARS: usize = 2000;

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let truncated_chars = text.len() - max_chars;
    format!(
        "{}\n\n[... {truncated_chars} more characters truncated]",
        &text[..max_chars]
    )
}

/// 对应 `serializeConversation`
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        match msg {
            Message::User(user) => {
                let content = match &user.content {
                    pi_ai::UserContent::Text(t) => t.clone(),
                    pi_ai::UserContent::Blocks(blocks) => content_text(
                        ContentTextInput::Blocks(
                            &blocks
                                .iter()
                                .map(|b| match b {
                                    pi_ai::TextOrImageContent::Text(t) => {
                                        ContentBlock::Text(t.clone())
                                    }
                                    pi_ai::TextOrImageContent::Image(_) => {
                                        ContentBlock::Text(pi_ai::TextContent {
                                            kind: pi_ai::TextKind,
                                            text: String::new(),
                                            text_signature: None,
                                        })
                                    }
                                })
                                .collect::<Vec<_>>(),
                        ),
                        "",
                    ),
                };
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking_parts = Vec::new();
                let mut tool_calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Thinking(t) => thinking_parts.push(t.thinking.clone()),
                        ContentBlock::ToolCall(tc) => {
                            let args_str = tc
                                .arguments
                                .as_object()
                                .map(|o| {
                                    o.iter()
                                        .map(|(k, v)| format!("{k}={v}"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            tool_calls.push(format!("{}({args_str})", tc.name));
                        }
                        _ => {}
                    }
                }
                if !thinking_parts.is_empty() {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        thinking_parts.join("\n")
                    ));
                }
                if assistant
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text(_)))
                {
                    parts.push(format!(
                        "[Assistant]: {}",
                        content_text(ContentTextInput::Blocks(&assistant.content), "")
                    ));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let content = content_text(
                    ContentTextInput::Blocks(
                        &result
                            .content
                            .iter()
                            .map(|b| match b {
                                pi_ai::TextOrImageContent::Text(t) => ContentBlock::Text(t.clone()),
                                pi_ai::TextOrImageContent::Image(_) => {
                                    ContentBlock::Text(pi_ai::TextContent {
                                        kind: pi_ai::TextKind,
                                        text: String::new(),
                                        text_signature: None,
                                    })
                                }
                            })
                            .collect::<Vec<_>>(),
                    ),
                    "",
                );
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }

    parts.join("\n\n")
}
