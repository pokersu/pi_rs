//! 对应 `harness/inbox.ts`：一个会话等待边界的提交队列。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游的 `InboxItem` 判别联合用 `mode` 字段区分；Rust 用 enum（`mode: "write"` 的条目
//!   携带普通 JSON 形式的 {@link EntryDraft}）。
//! - 上游对 Proxy 草稿做 `items.splice(...)`；Rust 用 [`Draft::splice`]，且用
//!   [`Draft::value`] 取快照来读取条目。因为上游在遍历结束后才删除条目，快照读取是安全的。
//! - `isStale` 的 `typeof entry.head === "number"` → Rust 的 [`EntryHead::Entry`] 变体。

use serde_json::Value as JsonValue;

use crate::chord::delta::{Path, PathSegment};
use crate::documents::define_doc;
use crate::entries::USER_ENTRY;
use crate::harness::types::{QueueMode, Settings};
use crate::session::SessionError;
use crate::session::transaction::Transaction;
use crate::types::{
    ConversationFork, ConversationHistory, ConversationId, DocAccess, DocDefinitionSpec, DocToken,
    DocumentSemantics, EntryDraft, EntryHead, EntryId, JsonObject, SubmissionId,
};

/// 对应 `InboxItem`：一个排队中的提交。
#[derive(Debug, Clone, PartialEq)]
pub enum InboxItem {
    /// `mode: "steer" | "followUp"`：一次运行的用户输入。
    Input {
        /// 提交 ID。
        id: SubmissionId,
        /// 排队模式。
        mode: InboxInputMode,
        /// `UserInput` 的 JSON 形式。
        content: JsonValue,
    },
    /// `mode: "write"`：一次被动条目写入（`entry` 是普通 JSON 形式的 `EntryDraft`）。
    Write {
        /// 提交 ID。
        id: SubmissionId,
        /// 条目草稿。
        entry: JsonObject,
    },
}

/// 对应 `mode: "steer" | "followUp"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxInputMode {
    /// `steer`。
    Steer,
    /// `followUp`。
    FollowUp,
}

impl InboxInputMode {
    /// JSON 里的字面量。
    pub fn as_str(self) -> &'static str {
        match self {
            InboxInputMode::Steer => "steer",
            InboxInputMode::FollowUp => "followUp",
        }
    }

    /// 从 JSON 字面量解析。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "steer" => Some(InboxInputMode::Steer),
            "followUp" => Some(InboxInputMode::FollowUp),
            _ => None,
        }
    }
}

impl InboxItem {
    /// 提交 ID。
    pub fn id(&self) -> SubmissionId {
        match self {
            InboxItem::Input { id, .. } | InboxItem::Write { id, .. } => *id,
        }
    }

    /// 是否为输入条目（`steer` / `followUp`）。
    pub fn is_input(&self) -> bool {
        matches!(self, InboxItem::Input { .. })
    }

    /// 从存储的 JSON 解析；形状不符时返回 `None`。
    pub fn from_json(value: &JsonValue) -> Option<Self> {
        let object = value.as_object()?;
        let id = SubmissionId::new(object.get("id")?.as_u64()?);
        match object.get("mode")?.as_str()? {
            "write" => Some(InboxItem::Write {
                id,
                entry: object.get("entry")?.as_object()?.clone(),
            }),
            mode => Some(InboxItem::Input {
                id,
                mode: InboxInputMode::parse(mode)?,
                content: object.get("content")?.clone(),
            }),
        }
    }

    /// 转为存储的 JSON。
    pub fn to_json(&self) -> JsonValue {
        match self {
            InboxItem::Input { id, mode, content } => serde_json::json!({
                "id": id.get(),
                "mode": mode.as_str(),
                "content": content,
            }),
            InboxItem::Write { id, entry } => serde_json::json!({
                "id": id.get(),
                "mode": "write",
                "entry": JsonValue::Object(entry.clone()),
            }),
        }
    }
}

/// 对应 `InboxState`：按 ID 顺序排队的条目。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct InboxState {
    /// 排队条目。
    pub items: Vec<JsonValue>,
}

struct InboxDefinition;

impl DocDefinitionSpec for InboxDefinition {
    fn kind(&self) -> &str {
        "pi.inbox"
    }

    fn version(&self) -> u32 {
        1
    }

    fn semantics(&self) -> DocumentSemantics {
        DocumentSemantics::Conversation {
            history: ConversationHistory::Latest,
            fork: ConversationFork::Initial,
        }
    }

    fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
        let mut object = JsonObject::new();
        object.insert("items".to_string(), JsonValue::Array(Vec::new()));
        object
    }

    fn checkpoint_when(
        &self,
        value: &JsonObject,
        _ops: &[crate::chord::delta::Op],
        _info: &crate::types::CheckpointInfo,
    ) -> bool {
        // 上游 `checkpointWhen: (value) => value.items.length === 0`。
        value
            .get("items")
            .and_then(JsonValue::as_array)
            .is_none_or(|items| items.is_empty())
    }
}

/// 对应 `InboxDoc`：内置队列文档。
pub static INBOX_DOC: std::sync::LazyLock<DocToken> = std::sync::LazyLock::new(|| {
    define_doc(std::sync::Arc::new(InboxDefinition)).expect("pi.inbox")
});

/// 对应 `QueueModes = Pick<Settings, "steeringMode" | "followUpMode">`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueModes {
    /// steering 队列模式。
    pub steering_mode: QueueMode,
    /// follow-up 队列模式。
    pub follow_up_mode: QueueMode,
}

impl QueueModes {
    /// 从解析后的设置取这两项。
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            steering_mode: settings.steering_mode,
            follow_up_mode: settings.follow_up_mode,
        }
    }
}

/// 对应 `Boundary`：一个边界在提交的首次表写入之前读到的东西。
pub struct Boundary {
    /// 会话 ID。
    pub conversation_id: ConversationId,
    /// 队列草稿。
    pub inbox: crate::types::Draft,
    /// steering 队列模式。
    pub steering_mode: QueueMode,
    /// follow-up 队列模式。
    pub follow_up_mode: QueueMode,
    /// 活动区间的起点（最新 head 标记的 `head`）；被本次提交写入的 head 前推。
    pub head: Option<EntryId>,
}

/// 对应 `BoundaryResult`：按 ID 顺序选中的用户条目，以及是否放置了 `head: "self"` 的写入（一次 reset）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundaryResult {
    /// 被放置的用户提交。
    pub users: Vec<SubmissionId>,
    /// 是否放置了 reset 写入。
    pub reset: bool,
}

/// 对应 `prepareBoundary(tx, conversationId, modes)`。
///
/// 表读取必须早于本次提交的首次表写入，因此调用方在提交开始时准备边界。
pub async fn prepare_boundary(
    tx: &Transaction,
    conversation_id: ConversationId,
    modes: QueueModes,
) -> Result<Boundary, SessionError> {
    let head = tx
        .latest_head_marker(conversation_id)
        .await?
        .map(|(_, head)| head);
    let inbox = tx
        .doc(
            &*INBOX_DOC,
            DocAccess {
                owner: Some(conversation_id.get()),
                key: None,
            },
            None,
        )
        .await?;
    Ok(Boundary {
        conversation_id,
        inbox,
        steering_mode: modes.steering_mode,
        follow_up_mode: modes.follow_up_mode,
        head,
    })
}

/// 对应 `applyBoundary(tx, boundary, at, now)`：放置边界选中的排队条目。
///
/// 每次写入、第一个（或全部）steer，以及在 `final` 时的第一个（或全部）follow-up 都会被放置。
/// 被选中的 reset 会把 `postTools` 边界变成 `final`。写入先放、用户条目随后，各自按 ID 顺序，
/// 因此排在 reset 之前的用户条目会在新上下文里运行。head 指向活动区间之前的写入（包括本提交内
/// 更早开始的区间）视为过期。选中与过期的条目按位置移除。
pub async fn apply_boundary(
    tx: &Transaction,
    boundary: &mut Boundary,
    at: BoundaryAt,
    now: u64,
) -> Result<BoundaryResult, SessionError> {
    let conversation_id = boundary.conversation_id;
    let snapshot = boundary.inbox.value();
    let raw_items: Vec<JsonValue> = snapshot
        .get("items")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();
    let items: Vec<InboxItem> = raw_items.iter().filter_map(InboxItem::from_json).collect();

    let reset = items.iter().any(|item| match item {
        InboxItem::Write { entry, .. } => entry.get("head") == Some(&JsonValue::from("self")),
        InboxItem::Input { .. } => false,
    });
    let final_boundary = at == BoundaryAt::Final || reset;

    let pick = |mode: InboxInputMode, queue_mode: QueueMode| -> Vec<usize> {
        let indexes: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| matches!(item, InboxItem::Input { mode: item_mode, .. } if *item_mode == mode))
            .map(|(index, _)| index)
            .collect();
        match queue_mode {
            QueueMode::All => indexes,
            QueueMode::OneAtATime => indexes.into_iter().take(1).collect(),
        }
    };

    let writes: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item, InboxItem::Write { .. }))
        .map(|(index, _)| index)
        .collect();
    let mut users: Vec<usize> = pick(InboxInputMode::Steer, boundary.steering_mode);
    if final_boundary {
        users.extend(pick(InboxInputMode::FollowUp, boundary.follow_up_mode));
    }
    users.sort_unstable();

    // `appendEntry()` 会复制草稿的值；条目只在此之后移除。
    for index in &writes {
        let InboxItem::Write { id, entry } = &items[*index] else {
            continue;
        };
        let draft = entry_draft_from_json(entry);
        if is_stale(boundary, &draft) {
            tx.settle_submission(
                *id,
                crate::types::SubmissionSettlement::Unanswered {
                    reason: "stale".to_string(),
                    detail: None,
                },
            );
            continue;
        }
        let entry = tx
            .append_entry(None, conversation_id, draft.clone())
            .await?;
        if let Some(head) = &draft.head {
            boundary.head = Some(match head {
                EntryHead::SelfEntry => entry.id,
                EntryHead::Entry(entry_id) => *entry_id,
            });
        }
        tx.place_submission(*id, entry.id);
    }

    let mut placed = Vec::new();
    for index in &users {
        let InboxItem::Input { id, content, .. } = &items[*index] else {
            continue;
        };
        let entry = tx
            .append_entry(
                None,
                conversation_id,
                user_entry_draft(content.clone(), now),
            )
            .await?;
        tx.place_submission(*id, entry.id);
        placed.push(*id);
    }

    let mut removed: Vec<usize> = writes;
    removed.extend(users);
    removed.sort_unstable_by(|left, right| right.cmp(left));
    for index in removed {
        boundary
            .inbox
            .splice(path("items"), index, 1, Vec::new())
            .map_err(delta_error)?;
    }
    Ok(BoundaryResult {
        users: placed,
        reset,
    })
}

/// 对应 `applyBoundary` 的 `at` 参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryAt {
    /// `postTools`。
    PostTools,
    /// `final`。
    Final,
}

/// 边界与提交共用的用户条目形状：`model` 为一条 `UserMessage`。
pub fn user_entry_draft(content: JsonValue, now: u64) -> EntryDraft {
    let message = serde_json::json!({
        "role": "user",
        "content": content,
        "timestamp": now,
    });
    EntryDraft {
        kind: USER_ENTRY.kind().to_string(),
        model: serde_json::from_value(message)
            .ok()
            .map(|message| vec![message]),
        data: None,
        head: None,
        edits: None,
    }
}

/// 对应 `isStale(boundary, entry)`：head 指向活动区间之前的条目，放置它会带回已切掉的历史。
pub fn is_stale(boundary: &Boundary, entry: &EntryDraft) -> bool {
    match entry.head {
        Some(EntryHead::Entry(entry_id)) => boundary
            .head
            .is_some_and(|head| entry_id.get() < head.get()),
        _ => false,
    }
}

/// 对应 `removeInboxItem(tx, conversationId, id)`：移除一个已撤回提交的条目；调用方负责结算该提交。
pub async fn remove_inbox_item(
    tx: &Transaction,
    conversation_id: ConversationId,
    id: SubmissionId,
) -> Result<(), SessionError> {
    let inbox = inbox_draft(tx, conversation_id).await?;
    let items = read_items(&inbox);
    let Some(index) = items.iter().position(|item| item.id() == id) else {
        return Ok(());
    };
    inbox
        .splice(path("items"), index, 1, Vec::new())
        .map_err(delta_error)?;
    Ok(())
}

/// 对应 `withdrawQueuedInputs(tx, conversationId)`：撤回一个会话的全部排队输入。
///
/// 每个都结算为 `unanswered` / `aborted` 并离开队列；排队的写入保留待后续放置。
pub async fn withdraw_queued_inputs(
    tx: &Transaction,
    conversation_id: ConversationId,
) -> Result<(), SessionError> {
    let inbox = inbox_draft(tx, conversation_id).await?;
    let items = read_items(&inbox);
    for index in (0..items.len()).rev() {
        let item = &items[index];
        if !item.is_input() {
            continue;
        }
        tx.settle_submission(
            item.id(),
            crate::types::SubmissionSettlement::Unanswered {
                reason: "aborted".to_string(),
                detail: None,
            },
        );
        inbox
            .splice(path("items"), index, 1, Vec::new())
            .map_err(delta_error)?;
    }
    Ok(())
}

async fn inbox_draft(
    tx: &Transaction,
    conversation_id: ConversationId,
) -> Result<crate::types::Draft, SessionError> {
    Ok(tx
        .doc(
            &*INBOX_DOC,
            DocAccess {
                owner: Some(conversation_id.get()),
                key: None,
            },
            None,
        )
        .await?)
}

fn read_items(inbox: &crate::types::Draft) -> Vec<InboxItem> {
    inbox
        .value()
        .get("items")
        .and_then(JsonValue::as_array)
        .map(|items| items.iter().filter_map(InboxItem::from_json).collect())
        .unwrap_or_default()
}

/// 把 {@link EntryDraft} 的 JSON 形式还原为草稿（`head` 的 `"self"` 字面量一并还原）。
fn entry_draft_from_json(entry: &JsonObject) -> EntryDraft {
    let head = match entry.get("head") {
        Some(JsonValue::String(value)) if value == "self" => Some(EntryHead::SelfEntry),
        Some(JsonValue::Number(number)) => number
            .as_u64()
            .map(|value| EntryHead::Entry(EntryId::new(value))),
        _ => None,
    };
    EntryDraft {
        kind: entry
            .get("kind")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string(),
        model: entry
            .get("model")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
        data: entry.get("data").cloned(),
        head,
        edits: entry
            .get("edits")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
    }
}

fn path(key: &str) -> Path {
    vec![PathSegment::Key(key.to_string())]
}

fn delta_error(error: crate::chord::delta::DeltaError) -> SessionError {
    SessionError::Message(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_kind_and_checkpoint_rule_match_upstream() {
        let definition = INBOX_DOC.definition();
        assert_eq!(definition.kind(), "pi.inbox");
        assert_eq!(definition.initial(None)["items"], serde_json::json!([]));

        let empty = definition.initial(None);
        assert!(definition.checkpoint_when(
            &empty,
            &[],
            &crate::types::CheckpointInfo {
                deltas_since_base: 3
            }
        ));

        let mut non_empty = JsonObject::new();
        non_empty.insert(
            "items".to_string(),
            serde_json::json!([{"id": 1, "mode": "steer", "content": "hi"}]),
        );
        assert!(!definition.checkpoint_when(
            &non_empty,
            &[],
            &crate::types::CheckpointInfo {
                deltas_since_base: 1
            }
        ));
    }

    #[test]
    fn items_round_trip_through_their_stored_json() {
        let input = InboxItem::Input {
            id: SubmissionId::new(7),
            mode: InboxInputMode::FollowUp,
            content: serde_json::json!("hello"),
        };
        let json = input.to_json();
        assert_eq!(json["mode"], serde_json::json!("followUp"));
        assert_eq!(InboxItem::from_json(&json), Some(input));

        let mut entry = JsonObject::new();
        entry.insert("kind".to_string(), serde_json::json!("pi.user"));
        let write = InboxItem::Write {
            id: SubmissionId::new(8),
            entry,
        };
        assert_eq!(InboxItem::from_json(&write.to_json()), Some(write));
    }

    #[test]
    fn malformed_items_are_rejected() {
        assert_eq!(InboxItem::from_json(&serde_json::json!({})), None);
        assert_eq!(
            InboxItem::from_json(&serde_json::json!({"id": 1, "mode": "unknown", "content": "x"})),
            None
        );
        assert_eq!(
            InboxItem::from_json(&serde_json::json!({"id": 1, "mode": "write"})),
            None
        );
    }

    #[test]
    fn is_stale_only_triggers_for_heads_before_the_active_range() {
        let boundary = Boundary {
            conversation_id: ConversationId::new(1),
            inbox: crate::types::Draft::new(
                crate::chord::tracker::track(JsonValue::Null).begin_change(),
            ),
            steering_mode: QueueMode::All,
            follow_up_mode: QueueMode::All,
            head: Some(EntryId::new(10)),
        };
        let draft_with = |head: Option<EntryHead>| EntryDraft {
            kind: "pi.user".to_string(),
            model: None,
            data: None,
            head,
            edits: None,
        };
        assert!(is_stale(
            &boundary,
            &draft_with(Some(EntryHead::Entry(EntryId::new(9))))
        ));
        assert!(!is_stale(
            &boundary,
            &draft_with(Some(EntryHead::Entry(EntryId::new(10))))
        ));
        assert!(!is_stale(
            &boundary,
            &draft_with(Some(EntryHead::SelfEntry))
        ));
        assert!(!is_stale(&boundary, &draft_with(None)));
    }

    #[test]
    fn queue_modes_come_from_settings() {
        let settings = Settings::default();
        let modes = QueueModes::from_settings(&settings);
        assert_eq!(modes.steering_mode, QueueMode::OneAtATime);
        assert_eq!(modes.follow_up_mode, QueueMode::OneAtATime);
    }

    #[test]
    fn entry_draft_head_survives_the_json_round_trip() {
        let mut entry = JsonObject::new();
        entry.insert("kind".to_string(), serde_json::json!("pi.user"));
        entry.insert("head".to_string(), serde_json::json!("self"));
        assert_eq!(
            entry_draft_from_json(&entry).head,
            Some(EntryHead::SelfEntry)
        );

        entry.insert("head".to_string(), serde_json::json!(42));
        assert_eq!(
            entry_draft_from_json(&entry).head,
            Some(EntryHead::Entry(EntryId::new(42)))
        );
    }
}
