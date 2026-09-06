//! Rust 翻译自 packages/agent/src/harness/session/context.ts

use std::collections::BTreeMap;

use pi_ai::StopReason;

use crate::harness::context::Context;
use crate::harness::messages::{create_branch_summary_message, create_compaction_summary_message};
use crate::harness::session::types::{Entry, EntryProjector};
use crate::types::AgentMessage;

/// 对应 `buildContextEntries`：保留最后一个 compaction 及其后的 entries。
pub fn build_context_entries(path_entries: &[Entry]) -> Vec<Entry> {
    let mut compaction_index = None;
    for (index, entry) in path_entries.iter().enumerate().rev() {
        if matches!(entry, Entry::Compaction(_)) {
            compaction_index = Some(index);
            break;
        }
    }
    match compaction_index {
        None => path_entries.to_vec(),
        Some(index) => path_entries[index..].to_vec(),
    }
}

fn is_context_message(message: &AgentMessage) -> bool {
    match message {
        AgentMessage::Assistant(a) => {
            a.stop_reason != StopReason::Error
                && a.stop_reason != StopReason::Aborted
                && a.stop_reason != StopReason::Deferred
        }
        _ => true,
    }
}

/// 对应 `sessionEntryToContextMessages`。
pub fn session_entry_to_context_messages(entry: &Entry) -> Vec<AgentMessage> {
    match entry {
        Entry::Message(e) => {
            if is_context_message(&e.message) {
                vec![e.message.clone()]
            } else {
                Vec::new()
            }
        }
        Entry::Compaction(e) => {
            let mut messages = vec![AgentMessage::CompactionSummary(
                create_compaction_summary_message(
                    e.summary.clone(),
                    e.tokens_before,
                    e.base.timestamp,
                ),
            )];
            messages.extend(
                e.retained_tail
                    .iter()
                    .filter(|m| is_context_message(m))
                    .cloned(),
            );
            messages
        }
        Entry::BranchSummary(e) => {
            if e.summary.is_empty() {
                Vec::new()
            } else {
                vec![AgentMessage::BranchSummary(create_branch_summary_message(
                    e.summary.clone(),
                    e.from_id.clone(),
                    e.base.timestamp,
                ))]
            }
        }
        Entry::Custom(_) => Vec::new(),
    }
}

/// 对应 `buildSessionContext`。
pub async fn build_session_context(
    path_entries: &[Entry],
    entry_projectors: Option<&BTreeMap<String, EntryProjector>>,
    context: &Context,
) -> Vec<AgentMessage> {
    let entries = build_context_entries(path_entries);
    let mut messages = Vec::new();
    for entry in &entries {
        if let Entry::Custom(custom) = entry {
            if let Some(projector) = entry_projectors.and_then(|p| p.get(&custom.custom_type))
                && let Some(projected) = projector(custom, context).await
            {
                messages.extend(projected);
            }
            continue;
        }
        messages.extend(session_entry_to_context_messages(entry));
    }
    messages
}
