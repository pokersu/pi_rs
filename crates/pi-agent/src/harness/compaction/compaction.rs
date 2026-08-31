//! Rust 翻译自 packages/agent/src/harness/compaction/compaction.ts（核心：token 估计与切点查找）
//!
//! 注：`prepareCompaction`/`generateSummary` 依赖 LLM 流式调用，后续按需补。

use pi_ai::{AssistantMessage, Usage};

use crate::harness::session::types::Entry;
use crate::types::AgentMessage;

/// 对应 `CompactionSettings`
#[derive(Debug, Clone)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

/// 对应 `DEFAULT_COMPACTION_SETTINGS`
pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 8192,
    keep_recent_tokens: 32768,
};

/// 对应 `calculateContextTokens`
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    usage
        .total_tokens
        .max(usage.input + usage.output + usage.cache_read + usage.cache_write)
}

fn get_assistant_usage(msg: &AgentMessage) -> Option<Usage> {
    let AgentMessage::Assistant(assistant) = msg else {
        return None;
    };
    if assistant.stop_reason == pi_ai::StopReason::Aborted
        || assistant.stop_reason == pi_ai::StopReason::Error
    {
        return None;
    }
    if calculate_context_tokens(&assistant.usage) > 0 {
        return Some(assistant.usage.clone());
    }
    None
}

/// 对应 `getLastAssistantUsage`
pub fn get_last_assistant_usage(entries: &[Entry]) -> Option<Usage> {
    for entry in entries.iter().rev() {
        if let Entry::Message(e) = entry
            && let Some(usage) = get_assistant_usage(&e.message)
        {
            return Some(usage);
        }
    }
    None
}

/// 对应 `ContextUsageEstimate`
#[derive(Debug, Clone, Default)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}

/// 对应 `estimateContextTokens`
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let usage_info = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, m)| get_assistant_usage(m).map(|u| (u, i)));

    let Some((usage, index)) = usage_info else {
        let mut estimated = 0;
        for message in messages {
            estimated += estimate_tokens(message);
        }
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: 0,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(&usage);
    let mut trailing_tokens = 0;
    for message in &messages[index + 1..] {
        trailing_tokens += estimate_tokens(message);
    }
    ContextUsageEstimate {
        tokens: usage_tokens + trailing_tokens,
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

/// 对应 `shouldCompact`
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// 对应 `estimateTokens`
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    let chars = match message {
        AgentMessage::User(u) => match &u.content {
            pi_ai::UserContent::Text(t) => t.len(),
            pi_ai::UserContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    pi_ai::TextOrImageContent::Text(t) => t.text.len(),
                    pi_ai::TextOrImageContent::Image(_) => ESTIMATED_IMAGE_CHARS,
                })
                .sum(),
        },
        AgentMessage::Assistant(a) => a
            .content
            .iter()
            .map(|b| match b {
                pi_ai::ContentBlock::Text(t) => t.text.len(),
                pi_ai::ContentBlock::Thinking(t) => t.thinking.len(),
                pi_ai::ContentBlock::ToolCall(tc) => tc.name.len() + tc.arguments.to_string().len(),
                pi_ai::ContentBlock::Image(_) => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
        AgentMessage::ToolResult(r) => r
            .content
            .iter()
            .map(|b| match b {
                pi_ai::TextOrImageContent::Text(t) => t.text.len(),
                pi_ai::TextOrImageContent::Image(_) => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
        AgentMessage::BashExecution(b) => b.command.len() + b.output.len(),
        AgentMessage::Custom(c) => match &c.content {
            crate::types::CustomMessageContent::Text(t) => t.len(),
            crate::types::CustomMessageContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    pi_ai::TextOrImageContent::Text(t) => t.text.len(),
                    pi_ai::TextOrImageContent::Image(_) => ESTIMATED_IMAGE_CHARS,
                })
                .sum(),
        },
        AgentMessage::BranchSummary(b) => b.summary.len(),
        AgentMessage::CompactionSummary(s) => s.summary.len(),
    };
    ((chars as f64) / 4.0).ceil() as u64
}

fn is_valid_cut_point(entry: &Entry) -> bool {
    match entry {
        Entry::Message(e) => match &e.message {
            AgentMessage::User(_)
            | AgentMessage::Assistant(_)
            | AgentMessage::BashExecution(_)
            | AgentMessage::Custom(_)
            | AgentMessage::BranchSummary(_)
            | AgentMessage::CompactionSummary(_) => true,
            AgentMessage::ToolResult(_) => false,
        },
        Entry::BranchSummary(_) => true,
        _ => false,
    }
}

/// 对应 `findTurnStartIndex`
pub fn find_turn_start_index(entries: &[Entry], entry_index: usize, start_index: usize) -> i64 {
    for i in (start_index..=entry_index).rev() {
        let entry = &entries[i];
        if matches!(entry, Entry::BranchSummary(_)) {
            return i as i64;
        }
        if let Entry::Message(e) = entry
            && matches!(
                &e.message,
                AgentMessage::User(_) | AgentMessage::BashExecution(_)
            )
        {
            return i as i64;
        }
    }
    -1
}

/// 对应 `CutPointResult`
#[derive(Debug, Clone)]
pub struct CutPointResult {
    pub first_kept_entry_index: usize,
    pub turn_start_index: i64,
    pub is_split_turn: bool,
}

/// 对应 `findCutPoint`
#[allow(clippy::needless_range_loop)]
pub fn find_cut_point(
    entries: &[Entry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u64,
) -> CutPointResult {
    let mut cut_points: Vec<usize> = Vec::new();
    for i in start_index..end_index {
        if is_valid_cut_point(&entries[i]) {
            cut_points.push(i);
        }
    }

    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: -1,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens = 0u64;
    let mut cut_index = cut_points[0];

    for i in (start_index..end_index).rev() {
        if !matches!(entries[i], Entry::Message(_)) {
            continue;
        }
        let message_tokens = entry_tokens(&entries[i]);
        accumulated_tokens += message_tokens;
        if accumulated_tokens >= keep_recent_tokens {
            for &cp in &cut_points {
                if cp >= i {
                    cut_index = cp;
                    break;
                }
            }
            break;
        }
    }

    while cut_index > start_index {
        let prev = &entries[cut_index - 1];
        if matches!(prev, Entry::Compaction(_)) || matches!(prev, Entry::Message(_)) {
            break;
        }
        cut_index -= 1;
    }

    let cut_entry = &entries[cut_index];
    let is_user_message =
        matches!(cut_entry, Entry::Message(e) if matches!(&e.message, AgentMessage::User(_)));
    let turn_start_index = if is_user_message {
        -1
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !is_user_message && turn_start_index != -1,
    }
}

fn entry_tokens(entry: &Entry) -> u64 {
    match entry {
        Entry::Message(e) => estimate_tokens(&e.message),
        _ => 0,
    }
}

/// 对应 `SUMMARIZATION_SYSTEM_PROMPT`
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.";

// 保留 AssistantMessage 引用。
#[allow(unused)]
fn _unused(_: AssistantMessage) {}
