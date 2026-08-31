//! Rust 翻译自 packages/agent/src/harness/compaction/branch-summarization.ts
//!
//! 注：`generateBranchSummary` 依赖 LLM 流式调用（`completeSimpleWithRetries`），
//! 此处保留 prepare/collect 与 prompt 常量，LLM 调用后续按需补。

use crate::harness::compaction::compaction::estimate_tokens;
use crate::harness::compaction::utils::{
    FileOperations, compute_file_lists, create_file_ops, extract_file_ops_from_message,
};
use crate::harness::messages::{create_branch_summary_message, create_compaction_summary_message};
use crate::harness::session::types::Entry;
use crate::types::AgentMessage;

/// 对应 `BranchSummaryResult`
#[derive(Debug, Clone, Default)]
pub struct BranchSummaryResult {
    pub summary: String,
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
    let mut total_tokens = 0u64;

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

// 保留引用避免告警。
#[allow(unused)]
fn _unused(_: Vec<String>) {
    let _ = compute_file_lists;
}
