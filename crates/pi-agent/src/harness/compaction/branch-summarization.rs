//! Rust 翻译自 packages/agent/src/harness/compaction/branch-summarization.ts
//!
//! 分支摘要：收集分支 entries、准备摘要输入、调用 LLM 生成摘要。

use std::collections::HashSet;

use pi_ai::{
    AbortSignal, ContentTextInput, Context, Message, Model, Models, RetryCallbacks, RetryPolicy,
    SimpleStreamOptions, StopReason, TextContent, TextKind, TextOrImageContent, Usage, UserContent,
    UserMessage, content_text,
};

use crate::harness::compaction::compaction::{
    SUMMARIZATION_SYSTEM_PROMPT, complete_simple_with_retries, estimate_tokens,
};
use crate::harness::compaction::utils::{
    FileOperations, compute_file_lists, create_file_ops, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};
use crate::harness::messages::{
    convert_to_llm, create_branch_summary_message, create_compaction_summary_message,
};
use crate::harness::session::Session;
use crate::harness::session::types::{Entry, SessionError, SessionErrorCode, SessionTree};
use crate::harness::types::{BranchSummaryError, BranchSummaryErrorCode};
use crate::types::AgentMessage;

/// 对应 `BranchSummaryResult`
#[derive(Debug, Clone)]
pub struct BranchSummaryResult {
    pub summary: String,
    pub usage: Option<Usage>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// 对应 `BranchSummaryDetails`：生成的分支摘要 entry 上存储的文件操作详情。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// 对应 `BranchPreparation`
#[derive(Debug, Clone, Default)]
pub struct BranchPreparation {
    pub messages: Vec<AgentMessage>,
    pub file_ops: FileOperations,
    pub total_tokens: u64,
}

/// 对应 `CollectEntriesResult`
#[derive(Debug, Clone)]
pub struct CollectEntriesResult {
    pub entries: Vec<Entry>,
    pub common_ancestor_id: Option<String>,
}

/// 对应 `GenerateBranchSummaryOptions`
pub struct GenerateBranchSummaryOptions<'a> {
    pub models: &'a Models,
    pub model: &'a Model,
    pub signal: &'a AbortSignal,
    pub custom_instructions: Option<&'a str>,
    pub replace_instructions: bool,
    /// 为 prompt 与模型输出预留的 token。默认 16384。
    pub reserve_tokens: u64,
    pub retry: Option<&'a RetryPolicy>,
    pub callbacks: Option<&'a RetryCallbacks>,
}

/// 对应 `collectEntriesForBranchSummary`：收集导航到另一 session tree entry 前应摘要的 entries。
pub async fn collect_entries_for_branch_summary(
    session: &Session,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> Result<CollectEntriesResult, SessionError> {
    let Some(old_leaf_id) = old_leaf_id else {
        return Ok(CollectEntriesResult {
            entries: Vec::new(),
            common_ancestor_id: None,
        });
    };

    let old_path = path_to_root(session, old_leaf_id).await?;
    let old_ids: HashSet<String> = old_path.iter().map(|e| e.id().to_string()).collect();
    let target_path = path_to_root(session, target_id).await?;

    let mut common_ancestor_id: Option<String> = None;
    for entry in &target_path {
        if old_ids.contains(entry.id()) {
            common_ancestor_id = Some(entry.id().to_string());
            break;
        }
    }

    let mut entries: Vec<Entry> = Vec::new();
    let mut current = Some(old_leaf_id.to_string());
    while let Some(id) = current {
        if Some(&id) == common_ancestor_id.as_ref() {
            break;
        }
        let entry = session.get_entry(&id).await?.ok_or_else(|| {
            SessionError::new(
                SessionErrorCode::InvalidEntry,
                format!("Entry {id} not found"),
            )
        })?;
        current = entry_parent_id(&entry);
        entries.push(entry);
    }
    entries.reverse();

    Ok(CollectEntriesResult {
        entries,
        common_ancestor_id,
    })
}

/// 从 `start_id` 沿 parent_id 回溯到 root，返回路径（start → root 顺序）。
async fn path_to_root(session: &Session, start_id: &str) -> Result<Vec<Entry>, SessionError> {
    let mut path = Vec::new();
    let mut current = Some(start_id.to_string());
    while let Some(id) = current {
        let Some(entry) = session.get_entry(&id).await? else {
            break;
        };
        current = entry_parent_id(&entry);
        path.push(entry);
    }
    Ok(path)
}

fn entry_parent_id(entry: &Entry) -> Option<String> {
    match entry {
        Entry::Message(e) => e.base.parent_id.clone(),
        Entry::ModelChange(e) => e.base.parent_id.clone(),
        Entry::ThinkingLevelChange(e) => e.base.parent_id.clone(),
        Entry::ActiveToolsChange(e) => e.base.parent_id.clone(),
        Entry::Compaction(e) => e.base.parent_id.clone(),
        Entry::BranchSummary(e) => e.base.parent_id.clone(),
        Entry::Custom(e) => e.base.parent_id.clone(),
    }
}

/// 对应 `getMessageFromEntry`
fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message(e) => {
            if matches!(&e.message, AgentMessage::ToolResult(_)) {
                return None;
            }
            Some(e.message.clone())
        }
        Entry::BranchSummary(e) => Some(AgentMessage::BranchSummary(
            create_branch_summary_message(e.summary.clone(), e.from_id.clone(), e.base.timestamp),
        )),
        Entry::Compaction(e) => Some(AgentMessage::CompactionSummary(
            create_compaction_summary_message(e.summary.clone(), e.tokens_before, e.base.timestamp),
        )),
        _ => None,
    }
}

/// 对应 `prepareBranchEntries`
pub fn prepare_branch_entries(entries: &[Entry], token_budget: u64) -> BranchPreparation {
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut file_ops = create_file_ops();
    let mut total_tokens: u64 = 0;

    for entry in entries {
        if let Entry::BranchSummary(e) = entry
            && let Some(details) = &e.details
        {
            if let Some(read) = details.get("readFiles").and_then(|v| v.as_array()) {
                for f in read {
                    if let Some(s) = f.as_str() {
                        file_ops.read.insert(s.to_string());
                    }
                }
            }
            if let Some(modified) = details.get("modifiedFiles").and_then(|v| v.as_array()) {
                for f in modified {
                    if let Some(s) = f.as_str() {
                        file_ops.edited.insert(s.to_string());
                    }
                }
            }
        }
    }

    for entry in entries.iter().rev() {
        let Some(message) = get_message_from_entry(entry) else {
            continue;
        };
        extract_file_ops_from_message(&message, &mut file_ops);

        let tokens = estimate_tokens(&message);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            if matches!(entry, Entry::Compaction(_) | Entry::BranchSummary(_))
                && total_tokens < (token_budget as f64 * 0.9) as u64
            {
                messages.insert(0, message);
                total_tokens += tokens;
            }
            break;
        }

        messages.insert(0, message);
        total_tokens += tokens;
    }

    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

/// 对应 `BRANCH_SUMMARY_PREAMBLE`
pub const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

/// 对应 `BRANCH_SUMMARY_PROMPT`
pub const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// 对应 `generateBranchSummary`：为被弃用的分支 entries 生成摘要。
pub async fn generate_branch_summary(
    entries: &[Entry],
    options: &GenerateBranchSummaryOptions<'_>,
) -> Result<BranchSummaryResult, BranchSummaryError> {
    let context_window = if options.model.context_window > 0 {
        options.model.context_window
    } else {
        128_000
    };
    let token_budget = context_window.saturating_sub(options.reserve_tokens);

    let preparation = prepare_branch_entries(entries, token_budget);

    if preparation.messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: "No content to summarize".to_string(),
            usage: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        });
    }

    let llm_messages = convert_to_llm(preparation.messages.clone());
    let conversation_text = serialize_conversation(&llm_messages);

    let instructions = if options.replace_instructions {
        options.custom_instructions.unwrap_or("").to_string()
    } else if let Some(ci) = options.custom_instructions {
        format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {ci}")
    } else {
        BRANCH_SUMMARY_PROMPT.to_string()
    };

    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");

    let summarization_messages = vec![Message::User(UserMessage {
        content: UserContent::Blocks(vec![TextOrImageContent::Text(TextContent {
            kind: TextKind,
            text: prompt_text,
            text_signature: None,
        })]),
        timestamp: pi_ai::utils::uuid::now_ms() as u64,
    })];

    let mut completion_options = SimpleStreamOptions::default();
    completion_options.stream.max_tokens = Some(2048);
    completion_options.stream.request.signal = Some(options.signal.clone());

    let response = complete_simple_with_retries(
        options.models,
        options.model,
        &Context {
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
            messages: summarization_messages,
            tools: None,
        },
        &completion_options,
        options.retry,
        options.callbacks,
    )
    .await;

    if response.stop_reason == StopReason::Aborted {
        return Err(BranchSummaryError::new(
            BranchSummaryErrorCode::Aborted,
            response
                .error_message
                .unwrap_or_else(|| "Branch summary aborted".to_string()),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(BranchSummaryError::new(
            BranchSummaryErrorCode::SummarizationFailed,
            format!(
                "Branch summary failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_string())
            ),
        ));
    }

    let summary_text = content_text(ContentTextInput::Blocks(&response.content), "");
    let summary = if summary_text.is_empty() {
        "No summary generated".to_string()
    } else {
        format!("{BRANCH_SUMMARY_PREAMBLE}{summary_text}")
    };

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let summary_with_files = format!(
        "{summary}{}",
        format_file_operations(&read_files, &modified_files)
    );

    Ok(BranchSummaryResult {
        summary: summary_with_files,
        usage: Some(response.usage),
        read_files,
        modified_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::session::InMemorySessionRepo;
    use crate::harness::session::types::SessionTree;

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::User(pi_ai::UserMessage {
            content: pi_ai::UserContent::Text(text.to_string()),
            timestamp: 0,
        })
    }

    #[tokio::test]
    async fn collects_entries_between_old_leaf_and_target() {
        let repo = InMemorySessionRepo::new();
        let session = repo.create(None).await.unwrap();
        let id1 = session.append_message(user_msg("a")).await.unwrap();
        let id2 = session.append_message(user_msg("b")).await.unwrap();
        let id3 = session.append_message(user_msg("c")).await.unwrap();

        let result = collect_entries_for_branch_summary(&session, Some(&id3), &id1)
            .await
            .unwrap();
        assert_eq!(result.common_ancestor_id.as_deref(), Some(id1.as_str()));
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.entries[0].id(), id2);
        assert_eq!(result.entries[1].id(), id3);
    }

    #[tokio::test]
    async fn collects_nothing_when_no_old_leaf() {
        let repo = InMemorySessionRepo::new();
        let session = repo.create(None).await.unwrap();
        let result = collect_entries_for_branch_summary(&session, None, "x")
            .await
            .unwrap();
        assert!(result.entries.is_empty());
        assert!(result.common_ancestor_id.is_none());
    }
}
