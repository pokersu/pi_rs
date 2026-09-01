//! Rust 翻译自 packages/agent/src/harness/compaction/compaction.ts（完整）
//!
//! token 估计、切点查找、compaction 准备与 summary 生成（含 LLM 调用与重试）。

use pi_ai::{
    AbortSignal, AssistantMessage, CacheRetention, ContentTextInput, Context, Message, Model,
    Models, RetryCallbacks, RetryPolicy, SimpleStreamOptions, StopReason, TextContent, TextKind,
    TextOrImageContent, Usage, UsageCost, UserContent, UserMessage, content_text,
    retry_assistant_call, uuidv7,
};

use crate::harness::compaction::utils::{
    FileOperations, compute_file_lists, create_file_ops, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};
use crate::harness::messages::{
    convert_to_llm, create_branch_summary_message, create_compaction_summary_message,
};
use crate::harness::session::context::build_session_context;
use crate::harness::session::types::{Entry, EntryBase, MessageEntry};
use crate::harness::types::{CompactionError, CompactionErrorCode};
use crate::types::{AgentMessage, ThinkingLevel, to_ai_thinking_level};

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
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

/// 对应 `SUMMARIZATION_PROMPT`
const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// 对应 `UPDATE_SUMMARIZATION_PROMPT`
const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// 对应 `TURN_PREFIX_SUMMARIZATION_PROMPT`
const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix."#;

/// 对应 `CompactionDetails`：生成的 compaction entry 上存储的文件操作详情。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

fn message_timestamp(message: &AgentMessage) -> u64 {
    match message {
        AgentMessage::User(m) => m.timestamp,
        AgentMessage::Assistant(m) => m.timestamp,
        AgentMessage::ToolResult(m) => m.timestamp,
        AgentMessage::BashExecution(m) => m.timestamp,
        AgentMessage::Custom(m) => m.timestamp,
        AgentMessage::BranchSummary(m) => m.timestamp,
        AgentMessage::CompactionSummary(m) => m.timestamp,
    }
}

/// 对应 `extractFileOperations`
fn extract_file_operations(
    messages: &[AgentMessage],
    entries: &[Entry],
    prev_compaction_index: i64,
) -> FileOperations {
    let mut file_ops = create_file_ops();
    if prev_compaction_index >= 0
        && let Entry::Compaction(prev) = &entries[prev_compaction_index as usize]
        && let Some(details_json) = &prev.details
        && let Ok(details) = serde_json::from_value::<CompactionDetails>(details_json.clone())
    {
        for f in &details.read_files {
            file_ops.read.insert(f.clone());
        }
        for f in &details.modified_files {
            file_ops.edited.insert(f.clone());
        }
    }
    for msg in messages {
        extract_file_ops_from_message(msg, &mut file_ops);
    }
    file_ops
}

/// 对应 `getMessageFromEntry`
fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message(e) => Some(e.message.clone()),
        Entry::BranchSummary(b) => Some(AgentMessage::BranchSummary(
            create_branch_summary_message(b.summary.clone(), b.from_id.clone(), b.base.timestamp),
        )),
        Entry::Compaction(c) => Some(AgentMessage::CompactionSummary(
            create_compaction_summary_message(c.summary.clone(), c.tokens_before, c.base.timestamp),
        )),
        _ => None,
    }
}

/// 对应 `getMessageFromEntryForCompaction`
fn get_message_from_entry_for_compaction(entry: &Entry) -> Option<AgentMessage> {
    if matches!(entry, Entry::Compaction(_)) {
        return None;
    }
    get_message_from_entry(entry)
}

/// 对应 `CompactResult`：生成后待持久化为 compaction entry 的数据。
#[derive(Debug, Clone)]
pub struct CompactResult {
    pub summary: String,
    pub tokens_before: u64,
    pub usage: Option<Usage>,
    pub retained_tail: Vec<AgentMessage>,
    pub details: CompactionDetails,
}

/// 对应 `generateSummaryWithUsage` 的返回值 `{ text, usage }`。
#[derive(Debug, Clone)]
pub struct SummaryWithUsage {
    pub text: String,
    pub usage: Usage,
}

/// 对应 `combineUsage`
fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write_1h: match (first.cache_write_1h, second.cache_write_1h) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        },
        reasoning: match (first.reasoning, second.reasoning) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        },
        total_tokens: first.total_tokens + second.total_tokens,
        cost: UsageCost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
        },
    }
}

/// 对应 `completeSimpleWithRetries`：summary 是独立请求，隔离路由并避免不可复用的缓存写入。
pub async fn complete_simple_with_retries(
    models: &Models,
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks>,
) -> AssistantMessage {
    let mut request_options = options.clone();
    request_options.stream.cache_retention = Some(CacheRetention::None);
    request_options.stream.session_id = Some(uuidv7());
    let signal = request_options.stream.request.signal.clone();
    retry_assistant_call(
        || {
            models.complete_simple(
                model.clone(),
                context.clone(),
                Some(request_options.clone()),
            )
        },
        retry,
        signal.as_ref(),
        callbacks,
    )
    .await
}

/// 对应 `generateSummary`
#[allow(clippy::too_many_arguments)]
pub async fn generate_summary(
    current_messages: Vec<AgentMessage>,
    models: &Models,
    model: &Model,
    reserve_tokens: u64,
    signal: Option<&AbortSignal>,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    thinking_level: Option<ThinkingLevel>,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks>,
) -> Result<String, CompactionError> {
    generate_summary_with_usage(
        current_messages,
        models,
        model,
        reserve_tokens,
        signal,
        custom_instructions,
        previous_summary,
        thinking_level,
        retry,
        callbacks,
    )
    .await
    .map(|r| r.text)
}

/// 对应 `generateSummaryWithUsage`：生成或更新会话 summary 并返回 provider usage。
#[allow(clippy::too_many_arguments)]
pub async fn generate_summary_with_usage(
    current_messages: Vec<AgentMessage>,
    models: &Models,
    model: &Model,
    reserve_tokens: u64,
    signal: Option<&AbortSignal>,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
    thinking_level: Option<ThinkingLevel>,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks>,
) -> Result<SummaryWithUsage, CompactionError> {
    let budget = ((reserve_tokens as f64) * 0.8).floor() as u64;
    let max_tokens = if model.max_tokens > 0 {
        budget.min(model.max_tokens)
    } else {
        budget
    };

    let mut base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    }
    .to_string();
    if let Some(instructions) = custom_instructions {
        base_prompt = format!("{base_prompt}\n\nAdditional focus: {instructions}");
    }

    let llm_messages = convert_to_llm(current_messages);
    let conversation_text = serialize_conversation(&llm_messages);
    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(summary) = previous_summary {
        prompt_text.push_str(&format!(
            "<previous-summary>\n{summary}\n</previous-summary>\n\n"
        ));
    }
    prompt_text.push_str(&base_prompt);

    let summarization_messages = vec![Message::User(UserMessage {
        content: UserContent::Blocks(vec![TextOrImageContent::Text(TextContent {
            kind: TextKind,
            text: prompt_text,
            text_signature: None,
        })]),
        timestamp: pi_ai::utils::uuid::now_ms() as u64,
    })];

    let mut completion_options = SimpleStreamOptions::default();
    completion_options.stream.max_tokens = Some(max_tokens);
    completion_options.stream.request.signal = signal.cloned();
    if model.reasoning {
        completion_options.reasoning = thinking_level.and_then(to_ai_thinking_level);
    }

    let response = complete_simple_with_retries(
        models,
        model,
        &Context {
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
            messages: summarization_messages,
            tools: None,
        },
        &completion_options,
        retry,
        callbacks,
    )
    .await;

    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::new(
            CompactionErrorCode::Aborted,
            response
                .error_message
                .unwrap_or_else(|| "Summarization aborted".to_string()),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(CompactionError::new(
            CompactionErrorCode::SummarizationFailed,
            format!(
                "Summarization failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_string())
            ),
        ));
    }

    let text = content_text(ContentTextInput::Blocks(&response.content), "");
    Ok(SummaryWithUsage {
        text,
        usage: response.usage,
    })
}

/// 对应 `CompactionPreparation`：一次 compaction 运行的准备输入。
#[derive(Debug, Clone)]
pub struct CompactionPreparation {
    pub messages_to_summarize: Vec<AgentMessage>,
    pub turn_prefix_messages: Vec<AgentMessage>,
    pub retained_tail: Vec<AgentMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: CompactionSettings,
}

/// 对应 `prepareCompaction`：准备 session entries 用于 compaction，不适用时返回 `None`。
pub fn prepare_compaction(
    path_entries: &[Entry],
    settings: &CompactionSettings,
) -> Result<Option<CompactionPreparation>, CompactionError> {
    if path_entries.is_empty() || matches!(path_entries.last(), Some(Entry::Compaction(_))) {
        return Ok(None);
    }

    let mut prev_compaction_index: i64 = -1;
    for i in (0..path_entries.len()).rev() {
        if matches!(path_entries[i], Entry::Compaction(_)) {
            prev_compaction_index = i as i64;
            break;
        }
    }

    let mut previous_summary: Option<String> = None;
    let mut compactable_entries: Vec<Entry> = path_entries.to_vec();
    if prev_compaction_index >= 0
        && let Entry::Compaction(prev) = &path_entries[prev_compaction_index as usize]
    {
        previous_summary = Some(prev.summary.clone());
        let mut virtual_retained: Vec<Entry> = prev
            .retained_tail
            .iter()
            .enumerate()
            .map(|(index, message)| {
                Entry::Message(MessageEntry {
                    base: EntryBase {
                        kind: "message".to_string(),
                        id: format!("{}:retained:{index}", prev.base.id),
                        seq: prev.base.seq,
                        parent_id: if index == 0 {
                            Some(prev.base.id.clone())
                        } else {
                            Some(format!("{}:retained:{}", prev.base.id, index - 1))
                        },
                        timestamp: message_timestamp(message),
                    },
                    message: message.clone(),
                    terminate: None,
                })
            })
            .collect();
        virtual_retained.extend_from_slice(&path_entries[(prev_compaction_index as usize + 1)..]);
        compactable_entries = virtual_retained;
    }
    let boundary_end = compactable_entries.len();

    let tokens_before =
        estimate_context_tokens(&build_session_context(path_entries).messages).tokens;

    let cut_point = find_cut_point(
        &compactable_entries,
        0,
        boundary_end,
        settings.keep_recent_tokens,
    );
    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index as usize
    } else {
        cut_point.first_kept_entry_index
    };

    let mut messages_to_summarize = Vec::new();
    for entry in &compactable_entries[..history_end] {
        if let Some(msg) = get_message_from_entry_for_compaction(entry) {
            messages_to_summarize.push(msg);
        }
    }

    let mut turn_prefix_messages = Vec::new();
    if cut_point.is_split_turn {
        for entry in &compactable_entries
            [cut_point.turn_start_index as usize..cut_point.first_kept_entry_index]
        {
            if let Some(msg) = get_message_from_entry_for_compaction(entry) {
                turn_prefix_messages.push(msg);
            }
        }
    }

    let mut retained_tail = Vec::new();
    for entry in &compactable_entries[cut_point.first_kept_entry_index..boundary_end] {
        if let Some(msg) = get_message_from_entry_for_compaction(entry) {
            retained_tail.push(msg);
        }
    }

    let mut file_ops =
        extract_file_operations(&messages_to_summarize, path_entries, prev_compaction_index);
    if cut_point.is_split_turn {
        for msg in &turn_prefix_messages {
            extract_file_ops_from_message(msg, &mut file_ops);
        }
    }

    Ok(Some(CompactionPreparation {
        messages_to_summarize,
        turn_prefix_messages,
        retained_tail,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings: settings.clone(),
    }))
}

/// 对应 `compact`：从准备的 session 历史生成 compaction summary 数据。
#[allow(clippy::too_many_arguments)]
pub async fn compact(
    preparation: &CompactionPreparation,
    models: &Models,
    model: &Model,
    custom_instructions: Option<&str>,
    signal: Option<&AbortSignal>,
    thinking_level: Option<ThinkingLevel>,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks>,
) -> Result<CompactResult, CompactionError> {
    let (summary, summary_usage) =
        if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
            let mut history_text = "No prior history.".to_string();
            let mut history_usage: Option<Usage> = None;
            if !preparation.messages_to_summarize.is_empty() {
                let history_result = generate_summary_with_usage(
                    preparation.messages_to_summarize.clone(),
                    models,
                    model,
                    preparation.settings.reserve_tokens,
                    signal,
                    custom_instructions,
                    preparation.previous_summary.as_deref(),
                    thinking_level,
                    retry,
                    callbacks,
                )
                .await?;
                history_text = history_result.text;
                history_usage = Some(history_result.usage);
            }
            let turn_prefix_result = generate_turn_prefix_summary(
                &preparation.turn_prefix_messages,
                models,
                model,
                preparation.settings.reserve_tokens,
                signal,
                thinking_level,
                retry,
                callbacks,
            )
            .await?;
            let summary = format!(
                "{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
                turn_prefix_result.text
            );
            let usage = match history_usage {
                Some(history) => combine_usage(&history, &turn_prefix_result.usage),
                None => turn_prefix_result.usage,
            };
            (summary, usage)
        } else {
            let summary_result = generate_summary_with_usage(
                preparation.messages_to_summarize.clone(),
                models,
                model,
                preparation.settings.reserve_tokens,
                signal,
                custom_instructions,
                preparation.previous_summary.as_deref(),
                thinking_level,
                retry,
                callbacks,
            )
            .await?;
            (summary_result.text, summary_result.usage)
        };

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let mut summary_with_files = summary;
    summary_with_files.push_str(&format_file_operations(&read_files, &modified_files));

    Ok(CompactResult {
        summary: summary_with_files,
        tokens_before: preparation.tokens_before,
        usage: Some(summary_usage),
        retained_tail: preparation.retained_tail.clone(),
        details: CompactionDetails {
            read_files,
            modified_files,
        },
    })
}

/// 对应 `generateTurnPrefixSummary`
#[allow(clippy::too_many_arguments)]
async fn generate_turn_prefix_summary(
    messages: &[AgentMessage],
    models: &Models,
    model: &Model,
    reserve_tokens: u64,
    signal: Option<&AbortSignal>,
    thinking_level: Option<ThinkingLevel>,
    retry: Option<&RetryPolicy>,
    callbacks: Option<&RetryCallbacks>,
) -> Result<SummaryWithUsage, CompactionError> {
    let budget = ((reserve_tokens as f64) * 0.5).floor() as u64;
    let max_tokens = if model.max_tokens > 0 {
        budget.min(model.max_tokens)
    } else {
        budget
    };

    let llm_messages = convert_to_llm(messages.to_vec());
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );

    let summarization_messages = vec![Message::User(UserMessage {
        content: UserContent::Blocks(vec![TextOrImageContent::Text(TextContent {
            kind: TextKind,
            text: prompt_text,
            text_signature: None,
        })]),
        timestamp: pi_ai::utils::uuid::now_ms() as u64,
    })];

    let mut completion_options = SimpleStreamOptions::default();
    completion_options.stream.max_tokens = Some(max_tokens);
    completion_options.stream.request.signal = signal.cloned();
    if model.reasoning {
        completion_options.reasoning = thinking_level.and_then(to_ai_thinking_level);
    }

    let response = complete_simple_with_retries(
        models,
        model,
        &Context {
            system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
            messages: summarization_messages,
            tools: None,
        },
        &completion_options,
        retry,
        callbacks,
    )
    .await;

    if response.stop_reason == StopReason::Aborted {
        return Err(CompactionError::new(
            CompactionErrorCode::Aborted,
            response
                .error_message
                .unwrap_or_else(|| "Turn prefix summarization aborted".to_string()),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(CompactionError::new(
            CompactionErrorCode::SummarizationFailed,
            format!(
                "Turn prefix summarization failed: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "Unknown error".to_string())
            ),
        ));
    }

    Ok(SummaryWithUsage {
        text: content_text(ContentTextInput::Blocks(&response.content), ""),
        usage: response.usage,
    })
}
