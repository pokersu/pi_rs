//! 对应 `harness/context.ts`：一个会话的原始活动转录与派生的模型上下文。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - **`freezeJson` 豁免**：上游在 `MemoryStorage` 的语义上再冻结一遍值，防止后来读取者看到变动；
//!   Rust 的记录是 owned/`Arc` 值，不存在共享可变，故无对应物。
//! - `Omit<EntryQuery, "conversationId">` → [`EntryRange`]。
//! - `idFromNumber<EntryId>(n)` → [`EntryId::new`]。

use std::collections::BTreeSet;
use std::sync::Arc;

use pi_ai::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ToolCall, ToolResultMessage,
};
use serde_json::json;

use crate::chord::context::Context;
use crate::harness::types::ContextView;
use crate::harness::util::scan_all;
use crate::session::{SessionError, SessionImpl};
use crate::types::{ContextEdit, Cursor, EntryId, EntryQuery, EntryRecord, Storage};

/// 对应 `SCAN_PAGE_SIZE`。
const SCAN_PAGE_SIZE: usize = 256;

/// 对应 `MISSING_RESULT_TEXT`。
const MISSING_RESULT_TEXT: &str =
    "Tool result unavailable: history ends before this call completed.";

/// 对应 `EXCLUDED_STOP_REASONS`。
fn excluded_stop_reason(reason: &StopReason) -> bool {
    matches!(
        reason,
        StopReason::Aborted | StopReason::Error | StopReason::Deferred
    )
}

/// 对应 `ContextBounds`：固定一个已提交上下文区间的 head 标记与最新可见条目。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextBounds {
    /// 带 `head` 的 head 标记条目。
    pub head: Option<EntryRecord>,
    /// 区间的最新条目（含）。
    pub tail: EntryId,
}

/// 对应 `Omit<EntryQuery, "conversationId">`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntryRange {
    /// 可返回的最旧条目 ID（含）。
    pub min_entry_id: Option<EntryId>,
    /// 可返回的最新条目 ID（含）。
    pub max_entry_id: Option<EntryId>,
}

impl EntryRange {
    fn query(self, conversation_id: crate::types::ConversationId) -> EntryQuery {
        EntryQuery {
            conversation_id,
            min_entry_id: self.min_entry_id,
            max_entry_id: self.max_entry_id,
            order: None,
        }
    }
}

/// 对应 `captureContextBounds`：用两次 O(1) 读取固定当前上下文的边界，或截断在可见条目 `at` 的边界。
///
/// 在 Session 线上运行；位于或早于 `tail` 的条目不可变，因此之后可以离线扫描它们。
pub async fn capture_context_bounds(
    storage: &dyn Storage,
    conversation_id: crate::types::ConversationId,
    context: &dyn Context,
    at: Option<EntryId>,
) -> Result<Option<ContextBounds>, SessionError> {
    let tail = match at {
        None => {
            let page = storage
                .scan_entries(
                    EntryQuery {
                        conversation_id,
                        min_entry_id: None,
                        max_entry_id: None,
                        order: None,
                    },
                    1,
                    None,
                    context,
                )
                .await?;
            match page.items.first() {
                Some(entry) => entry.id,
                None => return Ok(None),
            }
        }
        Some(at) => {
            if storage
                .entry_in_conversation(conversation_id, at, context)
                .await?
                .is_none()
            {
                return Err(SessionError::Message(format!(
                    "Entry {at} is not visible from conversation {conversation_id}"
                )));
            }
            at
        }
    };
    let head = storage
        .find_latest_head_marker(conversation_id, Some(tail), context)
        .await?
        .map(|(entry, _)| entry);
    Ok(Some(ContextBounds { head, tail }))
}

/// 对应 `ContextRange`：一次上下文读取的可见条目与派生视图。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextRange {
    /// 区间边界。
    pub bounds: ContextBounds,
    /// 旧到新的可见条目。
    pub entries: Vec<EntryRecord>,
    /// 派生视图。
    pub view: ContextView,
    /// `entries` 中编辑的目标。
    pub edited: BTreeSet<EntryId>,
    /// 最后一条 assistant 消息之前的已结算消息。
    pub settled: Vec<Message>,
    /// 从最后一条 assistant 消息起、尚未排序工具结果的消息。
    pub open: Vec<Message>,
}

/// 对应 `readContext()`：在一个会话的已提交边界内派生活动转录与模型上下文。
pub async fn read_context(
    session: &SessionImpl,
    storage: &dyn Storage,
    conversation_id: crate::types::ConversationId,
    context: Arc<dyn Context>,
    at: Option<EntryId>,
) -> Result<ContextView, SessionError> {
    let storage_for_bounds = storage;
    let job_context = Arc::clone(&context);
    let bounds = session
        .read_on_line(move || {
            let job_context = Arc::clone(&job_context);
            async move {
                capture_context_bounds(
                    storage_for_bounds,
                    conversation_id,
                    job_context.as_ref(),
                    at,
                )
                .await
            }
        })
        .await?;
    derive_context(storage, conversation_id, bounds.as_ref(), context.as_ref()).await
}

/// 对应 `readContextFrom()`：复用 `previous` 的上下文读取。
///
/// 同一个 head 标记下只扫描其 tail 之后的条目。返回视图与下一次读取要传入的区间。
pub async fn read_context_from(
    session: &SessionImpl,
    storage: &dyn Storage,
    conversation_id: crate::types::ConversationId,
    context: Arc<dyn Context>,
    at: Option<EntryId>,
    previous: Option<ContextRange>,
) -> Result<(ContextView, Option<ContextRange>), SessionError> {
    let job_context = Arc::clone(&context);
    let bounds = session
        .read_on_line(move || {
            let job_context = Arc::clone(&job_context);
            async move {
                capture_context_bounds(storage, conversation_id, job_context.as_ref(), at).await
            }
        })
        .await?;
    let Some(bounds) = bounds else {
        return Ok((empty_view(), None));
    };

    let range = match previous {
        None => derive_range(
            &bounds,
            scan_range(
                storage,
                conversation_id,
                range_query(&bounds),
                context.as_ref(),
            )
            .await?,
        ),
        Some(previous) => {
            let same_head = previous.bounds.head.as_ref().map(|head| head.id)
                == bounds.head.as_ref().map(|head| head.id);
            if !same_head {
                derive_range(
                    &bounds,
                    scan_range(
                        storage,
                        conversation_id,
                        range_query(&bounds),
                        context.as_ref(),
                    )
                    .await?,
                )
            } else if previous.bounds.tail == bounds.tail {
                previous
            } else if bounds.tail < previous.bounds.tail {
                let entries: Vec<EntryRecord> = previous
                    .entries
                    .iter()
                    .filter(|entry| entry.id <= bounds.tail)
                    .cloned()
                    .collect();
                derive_range(&bounds, entries)
            } else {
                let min_entry_id = EntryId::new(previous.bounds.tail.get() + 1);
                let added = scan_range(
                    storage,
                    conversation_id,
                    EntryRange {
                        min_entry_id: Some(min_entry_id),
                        max_entry_id: Some(bounds.tail),
                    },
                    context.as_ref(),
                )
                .await?;
                extend_range(&previous, &bounds, added)
            }
        }
    };
    // 区间会活过本次读取，因此返回的视图持有自己的数组。
    let view = ContextView {
        head: range.view.head.clone(),
        entries: range.view.entries.clone(),
        contributions: range.view.contributions.clone(),
        messages: range.view.messages.clone(),
    };
    Ok((view, Some(range)))
}

/// 对应 `deriveContext()`。
pub async fn derive_context(
    storage: &dyn Storage,
    conversation_id: crate::types::ConversationId,
    bounds: Option<&ContextBounds>,
    context: &dyn Context,
) -> Result<ContextView, SessionError> {
    let Some(bounds) = bounds else {
        return Ok(empty_view());
    };
    let entries = scan_range(storage, conversation_id, range_query(bounds), context).await?;
    Ok(derive_range(bounds, entries).view)
}

/// 对应 `activeEntries()`：已提交边界内的原始活动条目，不派生模型上下文。
pub async fn active_entries(
    storage: &dyn Storage,
    conversation_id: crate::types::ConversationId,
    bounds: Option<&ContextBounds>,
    context: &dyn Context,
) -> Result<Vec<EntryRecord>, SessionError> {
    let Some(bounds) = bounds else {
        return Ok(Vec::new());
    };
    let entries = scan_range(storage, conversation_id, range_query(bounds), context).await?;
    Ok(select_active(bounds.head.as_ref(), &entries))
}

fn empty_view() -> ContextView {
    ContextView {
        head: None,
        entries: Vec::new(),
        contributions: Vec::new(),
        messages: Vec::new(),
    }
}

/// 对应 `deriveRange()`：从 `bounds` 内扫描到的条目派生上下文视图。
fn derive_range(bounds: &ContextBounds, entries: Vec<EntryRecord>) -> ContextRange {
    let mut edits: std::collections::BTreeMap<EntryId, ContextEdit> = Default::default();
    // 区间内每个条目的编辑都算数，包括 `select_active()` 会丢掉的更早 head 标记。
    for entry in &entries {
        for edit in entry.edits.iter().flatten() {
            edits.insert(edit_target(edit), edit.clone());
        }
    }
    let active = select_active(bounds.head.as_ref(), &entries);
    let contributions: Vec<Vec<Message>> = active
        .iter()
        .map(|entry| contribute(entry, edits.get(&entry.id)))
        .collect();
    let open_before: Vec<Message> = contributions.iter().flatten().cloned().collect();
    let (settled, open) = settle(&[], &open_before);
    let mut messages = settled.clone();
    messages.extend(order_tool_results(&open));
    let messages = lead_with_system(messages);
    let view = ContextView {
        head: bounds.head.clone(),
        entries: active,
        contributions,
        messages,
    };
    ContextRange {
        bounds: bounds.clone(),
        entries,
        view,
        edited: edits.into_keys().collect(),
        settled,
        open,
    }
}

/// 对应 `extendRange()`：在同一个 head 标记下用 tail 之后的条目扩展 `previous`。
fn extend_range(
    previous: &ContextRange,
    bounds: &ContextBounds,
    added: Vec<EntryRecord>,
) -> ContextRange {
    let mut entries = previous.entries.clone();
    entries.extend(added.iter().cloned());
    // 新增的编辑可能改变更早的条目，因此整段重新派生。
    let needs_rebuild = added.iter().any(|entry| {
        entry.edits.is_some() || entry.head.is_some() || previous.edited.contains(&entry.id)
    });
    if needs_rebuild {
        return derive_range(bounds, entries);
    }
    let contributions: Vec<Vec<Message>> =
        added.iter().map(|entry| contribute(entry, None)).collect();
    let mut open_before = previous.open.clone();
    open_before.extend(contributions.iter().flatten().cloned());
    let (settled, open) = settle(&previous.settled, &open_before);
    let mut messages = settled.clone();
    messages.extend(order_tool_results(&open));
    let messages = lead_with_system(messages);
    let view = ContextView {
        head: bounds.head.clone(),
        entries: {
            let mut entries = previous.view.entries.clone();
            entries.extend(added.iter().cloned());
            entries
        },
        contributions: {
            let mut contributions_out = previous.view.contributions.clone();
            contributions_out.extend(contributions);
            contributions_out
        },
        messages,
    };
    ContextRange {
        bounds: bounds.clone(),
        entries,
        view,
        edited: previous.edited.clone(),
        settled,
        open,
    }
}

/// 对应 `leadWithSystem()`：把只有用户消息在前的系统消息移到最前。
///
/// 一次运行的输入在 generation 渲染系统提示之前就已提交，因此转录（或压缩/重置之后的区间）会以
/// 用户消息开头、基线系统消息在后。provider 只把**开头**的系统消息当作初始提示与工具集；否则后来的
/// 工具变更会重写请求的工具列表并使整个提示缓存失效。
fn lead_with_system(messages: Vec<Message>) -> Vec<Message> {
    let Some(index) = messages.iter().position(|message| message.role() != "user") else {
        return messages;
    };
    if index == 0 || messages[index].role() != "system" {
        return messages;
    }
    let mut ordered = Vec::with_capacity(messages.len());
    ordered.push(messages[index].clone());
    ordered.extend(messages[..index].iter().cloned());
    ordered.extend(messages[index + 1..].iter().cloned());
    ordered
}

/// 对应 `contribute()`：一个活动条目在其编辑与排除停止原因之后的模型消息。
fn contribute(entry: &EntryRecord, edit: Option<&ContextEdit>) -> Vec<Message> {
    match edit {
        Some(ContextEdit::Omit { .. }) => Vec::new(),
        Some(ContextEdit::Replace { messages, .. }) => messages
            .iter()
            .filter(|message| !is_excluded_assistant(message))
            .cloned()
            .collect(),
        None => entry
            .model
            .as_ref()
            .map(|messages| {
                messages
                    .iter()
                    .filter(|message| !is_excluded_assistant(message))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn is_excluded_assistant(message: &Message) -> bool {
    match message {
        Message::Assistant(assistant) => excluded_stop_reason(&assistant.stop_reason),
        _ => false,
    }
}

/// 对应 `settle()`：把 `open` 中最后一条 assistant 消息之前的已排序消息移入 `settled`。
fn settle(settled: &[Message], open: &[Message]) -> (Vec<Message>, Vec<Message>) {
    let last = open
        .iter()
        .rposition(|message| message.role() == "assistant");
    match last {
        Some(last) if last > 0 => {
            let mut next_settled = settled.to_vec();
            next_settled.extend(order_tool_results(&open[..last]));
            (next_settled, open[last..].to_vec())
        }
        _ => (settled.to_vec(), open.to_vec()),
    }
}

/// 对应 `rangeQuery()`：`bounds` 的可见区间——从 head 标记的 `head`（或转录起点）到 tail。
fn range_query(bounds: &ContextBounds) -> EntryRange {
    match &bounds.head {
        None => EntryRange {
            min_entry_id: None,
            max_entry_id: Some(bounds.tail),
        },
        Some(head) => EntryRange {
            min_entry_id: head.head,
            max_entry_id: Some(bounds.tail),
        },
    }
}

/// 对应 `scanRange()`：区间内的可见条目，旧到新。
async fn scan_range(
    storage: &dyn Storage,
    conversation_id: crate::types::ConversationId,
    range: EntryRange,
    context: &dyn Context,
) -> Result<Vec<EntryRecord>, SessionError> {
    let mut entries: Vec<EntryRecord> = scan_all(|cursor: Option<Cursor>| {
        let query = range.query(conversation_id);
        async move {
            storage
                .scan_entries(query, SCAN_PAGE_SIZE, cursor, context)
                .await
        }
    })
    .await
    .map_err(SessionError::from)?;
    entries.reverse();
    Ok(entries)
}

/// 对应 `selectActive()`：head 标记后跟区间的非 head 条目；没有标记时就是整个区间。
fn select_active(head: Option<&EntryRecord>, range: &[EntryRecord]) -> Vec<EntryRecord> {
    match head {
        None => range.to_vec(),
        Some(head) => {
            let mut active = vec![head.clone()];
            active.extend(range.iter().filter(|entry| entry.head.is_none()).cloned());
            active
        }
    }
}

/// 对应 `orderToolResults()`：把每个 assistant 的工具结果按调用顺序紧随其后。
///
/// 结果取自下一条 assistant 之前的消息；缺失的结果被合成，未匹配的结果被丢弃。
pub fn order_tool_results(messages: &[Message]) -> Vec<Message> {
    let mut ordered: Vec<Message> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if matches!(message, Message::ToolResult(_)) {
            continue;
        }
        ordered.push(message.clone());
        let Message::Assistant(assistant) = message else {
            continue;
        };
        let calls: Vec<&ToolCall> = assistant
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            continue;
        }
        // 下一条 assistant 之前的工具结果，按 toolCallId 首见。
        let mut results: std::collections::BTreeMap<&str, usize> = Default::default();
        for (next, candidate) in messages.iter().enumerate().skip(index + 1) {
            if matches!(candidate, Message::Assistant(_)) {
                break;
            }
            if let Message::ToolResult(result) = candidate
                && !results.contains_key(result.tool_call_id.as_str())
            {
                results.insert(result.tool_call_id.as_str(), next);
            }
        }
        for call in calls {
            match results.get(call.id.as_str()) {
                Some(result_index) => ordered.push(messages[*result_index].clone()),
                None => ordered.push(missing_result(call, assistant.timestamp)),
            }
        }
    }
    ordered
}

/// 对应 `missingResult()`：为未完成其结果的调用合成一个错误结果。
fn missing_result(call: &ToolCall, timestamp: u64) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: vec![pi_ai::TextOrImageContent::Text(TextContent {
            kind: pi_ai::TextKind,
            text: MISSING_RESULT_TEXT.to_string(),
            text_signature: None,
        })],
        details: Some(json!({"reason": "missing_result"})),
        usage: None,
        added_tool_names: None,
        is_error: true,
        timestamp,
        duration_ms: None,
    })
}

fn edit_target(edit: &ContextEdit) -> EntryId {
    match edit {
        ContextEdit::Omit { target } | ContextEdit::Replace { target, .. } => *target,
    }
}

/// 便于阅读：`ContextRange.settled` / `open` 的上游类型。
#[allow(dead_code)]
type AssistantContent = AssistantMessage;

/// 便于阅读：`readContextFrom` 的 `previous` 参数形态。
#[allow(dead_code)]
type PreviousRange = ContextRange;

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{ImageContent, ThinkingContent, UserContent, UserMessage};

    fn user(text: &str, timestamp: u64) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp,
        })
    }

    fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason, timestamp: u64) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            api: "openai-completions".to_string(),
            provider: "p".to_string(),
            model: "m".to_string(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: pi_ai::default_usage(),
            stop_reason,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp,
            duration_ms: None,
        })
    }

    fn tool_call(id: &str, name: &str) -> ContentBlock {
        ContentBlock::ToolCall(ToolCall {
            kind: pi_ai::ToolCallKind,
            id: id.to_string(),
            name: name.to_string(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        })
    }

    fn tool_result(id: &str, timestamp: u64) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.to_string(),
            tool_name: "t".to_string(),
            content: vec![pi_ai::TextOrImageContent::Text(TextContent {
                kind: pi_ai::TextKind,
                text: "done".to_string(),
                text_signature: None,
            })],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp,
            duration_ms: None,
        })
    }

    #[test]
    fn order_tool_results_places_results_after_their_assistant() {
        let messages = vec![
            user("hi", 1),
            assistant(
                vec![tool_call("a", "read"), tool_call("b", "bash")],
                StopReason::ToolUse,
                2,
            ),
            tool_result("b", 4),
            tool_result("a", 3),
        ];
        let ordered = order_tool_results(&messages);
        let ids: Vec<String> = ordered
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result.tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["a", "b"], "按调用顺序，而不是结果到达顺序");
    }

    #[test]
    fn order_tool_results_synthesizes_missing_results() {
        let messages = vec![assistant(
            vec![tool_call("a", "read"), tool_call("b", "bash")],
            StopReason::ToolUse,
            7,
        )];
        let ordered = order_tool_results(&messages);
        assert_eq!(ordered.len(), 3);
        let Message::ToolResult(result) = &ordered[2] else {
            panic!("expected a synthesized result");
        };
        assert_eq!(result.tool_call_id, "b");
        assert!(result.is_error);
        assert_eq!(result.timestamp, 7, "合成结果沿用 assistant 的时间戳");
        assert_eq!(result.details, Some(json!({"reason": "missing_result"})));
    }

    #[test]
    fn order_tool_results_drops_unmatched_results() {
        let messages = vec![
            assistant(vec![tool_call("a", "read")], StopReason::ToolUse, 1),
            tool_result("a", 2),
            tool_result("orphan", 3),
        ];
        let ordered = order_tool_results(&messages);
        assert_eq!(ordered.len(), 2, "没有对应调用的结果被丢弃");
    }

    #[test]
    fn lead_with_system_moves_a_trailing_system_message_to_the_front() {
        let messages = vec![user("hi", 1), system("baseline", 2), user("again", 3)];
        let ordered = lead_with_system(messages);
        assert_eq!(ordered[0].role(), "system");
        assert_eq!(ordered[1].role(), "user");
        assert_eq!(ordered[2].role(), "user");
    }

    #[test]
    fn lead_with_system_leaves_a_leading_system_message_alone() {
        let messages = vec![system("baseline", 1), user("hi", 2)];
        let ordered = lead_with_system(messages.clone());
        assert_eq!(ordered, messages);
    }

    fn system(text: &str, timestamp: u64) -> Message {
        Message::System(pi_ai::SystemMessage {
            content: pi_ai::SystemContent::Text(text.to_string()),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp,
        })
    }

    #[test]
    fn contribute_drops_excluded_stop_reasons() {
        let mut entry = EntryRecord {
            id: EntryId::new(1),
            conversation_id: crate::types::ConversationId::new(1),
            kind: "pi.assistant".to_string(),
            model: Some(vec![
                assistant(vec![], StopReason::Stop, 1),
                assistant(vec![], StopReason::Aborted, 2),
                assistant(vec![], StopReason::Error, 3),
                assistant(vec![], StopReason::Deferred, 4),
            ]),
            data: None,
            head: None,
            edits: None,
            by_task_id: None,
        };
        assert_eq!(contribute(&entry, None).len(), 1, "只有 stop 保留");

        entry.model = Some(vec![user("hi", 1)]);
        assert_eq!(contribute(&entry, None).len(), 1);

        let omitted = ContextEdit::Omit {
            target: EntryId::new(1),
        };
        assert!(contribute(&entry, Some(&omitted)).is_empty());

        let replaced = ContextEdit::Replace {
            target: EntryId::new(1),
            messages: vec![user("replacement", 9)],
        };
        let contributed = contribute(&entry, Some(&replaced));
        assert_eq!(contributed.len(), 1);
        assert_eq!(contributed[0], user("replacement", 9));
    }

    #[test]
    fn settle_keeps_the_tail_from_the_last_assistant() {
        let settled: Vec<Message> = vec![user("old", 1)];
        let open = vec![
            assistant(vec![tool_call("a", "t")], StopReason::ToolUse, 2),
            tool_result("a", 3),
            assistant(vec![], StopReason::Stop, 4),
            tool_result("stray", 5),
        ];
        let (next_settled, next_open) = settle(&settled, &open);
        assert_eq!(next_settled.len(), 3, "旧 settled + 已排序的前半段");
        assert_eq!(next_open.len(), 2, "从最后一条 assistant 起");
        assert_eq!(next_open[0], open[2]);
    }

    #[test]
    fn settle_is_a_noop_without_a_preceding_assistant() {
        let open = vec![user("hi", 1)];
        let (settled, next_open) = settle(&[], &open);
        assert!(settled.is_empty());
        assert_eq!(next_open, open);
    }

    #[test]
    fn range_query_starts_at_the_head_marker() {
        let bounds = ContextBounds {
            head: Some(EntryRecord {
                id: EntryId::new(5),
                conversation_id: crate::types::ConversationId::new(1),
                kind: "pi.user".to_string(),
                model: None,
                data: None,
                head: Some(EntryId::new(2)),
                edits: None,
                by_task_id: None,
            }),
            tail: EntryId::new(9),
        };
        let range = range_query(&bounds);
        assert_eq!(range.min_entry_id, Some(EntryId::new(2)));
        assert_eq!(range.max_entry_id, Some(EntryId::new(9)));

        let without = ContextBounds {
            head: None,
            tail: EntryId::new(9),
        };
        assert_eq!(range_query(&without).min_entry_id, None);
    }

    #[test]
    fn select_active_puts_the_head_marker_first() {
        let head = EntryRecord {
            id: EntryId::new(5),
            conversation_id: crate::types::ConversationId::new(1),
            kind: "pi.user".to_string(),
            model: None,
            data: None,
            head: Some(EntryId::new(2)),
            edits: None,
            by_task_id: None,
        };
        let mut older = head.clone();
        older.id = EntryId::new(2);
        older.head = None;
        let mut skipped = head.clone();
        skipped.id = EntryId::new(3);
        let mut newer = head.clone();
        newer.id = EntryId::new(6);
        newer.head = None;

        let active = select_active(Some(&head), &[older.clone(), skipped, newer.clone()]);
        assert_eq!(
            active
                .iter()
                .map(|entry| entry.id.get())
                .collect::<Vec<_>>(),
            vec![5, 2, 6],
            "head 标记在前，其余非 head 条目保留"
        );

        let all = select_active(None, &[older, newer]);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn unused_types_are_exercised() {
        // 让 `ThinkingContent` / `ImageContent` 的引用不至于被误删（与 pi-ai 的类型面保持一致）。
        let _ = std::mem::size_of::<ThinkingContent>();
        let _ = std::mem::size_of::<ImageContent>();
    }
}
