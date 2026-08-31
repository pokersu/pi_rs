//! Rust 翻译自 packages/agent/src/harness/session/context.ts

use crate::harness::messages::{create_branch_summary_message, create_compaction_summary_message};
use crate::harness::session::types::Entry;
use crate::types::AgentMessage;

/// 对应 `SessionContext`
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub provider: Option<String>,
    pub model_id: Option<String>,
    pub active_tool_names: Option<Vec<String>>,
}

/// 对应 `deriveSessionContextState`
fn derive_session_context_state(
    path_entries: &[Entry],
) -> (String, Option<String>, Option<String>, Option<Vec<String>>) {
    let mut thinking_level = "off".to_string();
    let mut provider: Option<String> = None;
    let mut model_id: Option<String> = None;
    let mut active_tool_names: Option<Vec<String>> = None;

    for entry in path_entries {
        match entry {
            Entry::ThinkingLevelChange(e) => {
                thinking_level = e.thinking_level.clone();
            }
            Entry::ModelChange(e) => {
                provider = Some(e.provider.clone());
                model_id = Some(e.model_id.clone());
            }
            Entry::Message(e) => {
                if let AgentMessage::Assistant(a) = &e.message {
                    provider = Some(a.provider.clone());
                    model_id = Some(a.model.clone());
                }
            }
            Entry::ActiveToolsChange(e) => {
                active_tool_names = Some(e.active_tool_names.clone());
            }
            _ => {}
        }
    }

    (thinking_level, provider, model_id, active_tool_names)
}

/// 对应 `defaultContextEntryTransform`
pub fn default_context_entry_transform(path_entries: &[Entry]) -> Vec<Entry> {
    let mut compaction_index: Option<usize> = None;
    for index in (0..path_entries.len()).rev() {
        if matches!(path_entries[index], Entry::Compaction(_)) {
            compaction_index = Some(index);
            break;
        }
    }
    match compaction_index {
        None => path_entries.to_vec(),
        Some(index) => {
            let mut result = vec![path_entries[index].clone()];
            result.extend_from_slice(&path_entries[index + 1..]);
            result
        }
    }
}

/// 对应 `sessionEntryToContextMessages`
pub fn session_entry_to_context_messages(entry: &Entry) -> Vec<AgentMessage> {
    match entry {
        Entry::Message(e) => {
            if let AgentMessage::Assistant(a) = &e.message
                && a.stop_reason == pi_ai::StopReason::Deferred
            {
                return Vec::new();
            }
            vec![e.message.clone()]
        }
        Entry::Compaction(e) => {
            let mut result = vec![AgentMessage::CompactionSummary(
                create_compaction_summary_message(
                    e.summary.clone(),
                    e.tokens_before,
                    e.base.timestamp,
                ),
            )];
            result.extend(e.retained_tail.clone());
            result
        }
        Entry::BranchSummary(e) if !e.summary.is_empty() => {
            vec![AgentMessage::BranchSummary(create_branch_summary_message(
                e.summary.clone(),
                e.from_id.clone(),
                e.base.timestamp,
            ))]
        }
        _ => Vec::new(),
    }
}

/// 对应 `buildSessionContext`
pub fn build_session_context(path_entries: &[Entry]) -> SessionContext {
    let (thinking_level, provider, model_id, active_tool_names) =
        derive_session_context_state(path_entries);
    let context_entries = default_context_entry_transform(path_entries);
    let messages: Vec<AgentMessage> = context_entries
        .iter()
        .flat_map(session_entry_to_context_messages)
        .collect();
    SessionContext {
        messages,
        thinking_level,
        provider,
        model_id,
        active_tool_names,
    }
}
