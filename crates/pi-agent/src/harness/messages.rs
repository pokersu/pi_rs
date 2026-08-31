//! Rust 翻译自 packages/agent/src/harness/messages.ts

use pi_ai::{Message, TextContent, TextKind, TextOrImageContent, UserMessage};

use crate::types::{
    AgentMessage, BranchSummaryMessage, CompactionSummaryMessage, CustomMessage,
    CustomMessageContent,
};

/// 对应 `COMPACTION_SUMMARY_PREFIX`
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// 对应 `COMPACTION_SUMMARY_SUFFIX`
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// 对应 `BRANCH_SUMMARY_PREFIX`
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
/// 对应 `BRANCH_SUMMARY_SUFFIX`
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// 对应 `bashExecutionToText`
pub fn bash_execution_to_text(msg: &crate::types::BashExecutionMessage) -> String {
    let mut text = format!("Ran `{}`\n", msg.command);
    if !msg.output.is_empty() {
        text += &format!("```\n{}\n```", msg.output);
    } else {
        text += "(no output)";
    }
    if msg.cancelled {
        text += "\n\n(command cancelled)";
    } else if let Some(code) = msg.exit_code
        && code != 0
    {
        text += &format!("\n\nCommand exited with code {code}");
    }
    if msg.truncated
        && let Some(path) = &msg.full_output_path
    {
        text += &format!("\n\n[Output truncated. Full output: {path}]");
    }
    text
}

/// 对应 `createBranchSummaryMessage`
pub fn create_branch_summary_message(
    summary: String,
    from_id: String,
    timestamp: u64,
) -> BranchSummaryMessage {
    BranchSummaryMessage {
        summary,
        from_id,
        timestamp,
    }
}

/// 对应 `createCompactionSummaryMessage`
pub fn create_compaction_summary_message(
    summary: String,
    tokens_before: u64,
    timestamp: u64,
) -> CompactionSummaryMessage {
    CompactionSummaryMessage {
        summary,
        tokens_before,
        timestamp,
    }
}

/// 对应 `createCustomMessage`
pub fn create_custom_message(
    custom_type: String,
    content: CustomMessageContent,
    display: bool,
    details: Option<serde_json::Value>,
    timestamp: u64,
) -> CustomMessage {
    CustomMessage {
        custom_type,
        content,
        display,
        details,
        timestamp,
    }
}

fn text_block(text: String) -> Vec<TextOrImageContent> {
    vec![TextOrImageContent::Text(TextContent {
        kind: TextKind,
        text,
        text_signature: None,
    })]
}

/// 对应 `convertToLlm`：把 `AgentMessage[]` 转成 LLM 可见的 `Message[]`。
pub fn convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    messages
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::BashExecution(b) => {
                if b.exclude_from_context {
                    return None;
                }
                Some(Message::User(UserMessage {
                    content: pi_ai::UserContent::Blocks(text_block(bash_execution_to_text(&b))),
                    timestamp: b.timestamp,
                }))
            }
            AgentMessage::Custom(c) => {
                let content = match c.content {
                    CustomMessageContent::Text(t) => pi_ai::UserContent::Text(t),
                    CustomMessageContent::Blocks(blocks) => pi_ai::UserContent::Blocks(blocks),
                };
                Some(Message::User(UserMessage {
                    content,
                    timestamp: c.timestamp,
                }))
            }
            AgentMessage::BranchSummary(b) => Some(Message::User(UserMessage {
                content: pi_ai::UserContent::Text(format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    b.summary
                )),
                timestamp: b.timestamp,
            })),
            AgentMessage::CompactionSummary(s) => Some(Message::User(UserMessage {
                content: pi_ai::UserContent::Text(format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    s.summary
                )),
                timestamp: s.timestamp,
            })),
            AgentMessage::User(u) => Some(Message::User(u)),
            AgentMessage::Assistant(a) => Some(Message::Assistant(a)),
            AgentMessage::ToolResult(r) => Some(Message::ToolResult(r)),
        })
        .collect()
}
