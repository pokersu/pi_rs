//! Rust 翻译自 packages/agent/src/harness/runtime/transcript.ts
//!
//! lane transcript 的投影与队列读取纯函数。

use serde_json::{Value as Json, json};

use crate::harness::agent_harness::{HarnessEvent, LaneQueuedItem};
use crate::harness::context::Context;
use crate::harness::runtime::types::{ContinueOperationResult, Drive, Lane, OperationCommand};
use crate::harness::session::commit::materialize_committed_entry;
use crate::harness::session::context::build_session_context;
use crate::harness::session::session::SessionInvariantError;
use crate::harness::session::types::{
    BranchScan, CommitResult, Entry, EntryType, InboxItem, NewEntry, OperationState, PendingEntry,
    ScanOrder, SessionReader, StorageBranchScan,
};
use crate::harness::session::values::pending_entry;
use crate::types::AgentMessage;

/// 对应 `chainEntries`：把 items 链成带 parentId 的 JSON 对象。
pub fn chain_entries<T: Clone + Into<Json>>(
    mut parent_id: Option<String>,
    items: &[T],
) -> Vec<Json> {
    let mut result = Vec::new();
    for item in items {
        let mut entry = item.clone().into();
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("parentId".to_string(), json!(parent_id));
        }
        if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
            parent_id = Some(id.to_string());
        }
        result.push(entry);
    }
    result
}

/// 对应 `entryLifecycleEvents`。
pub fn entry_lifecycle_events(
    entry: &Entry,
    lane: &str,
    run_id: Option<&str>,
) -> Vec<HarnessEvent> {
    match entry {
        Entry::Message(e) => {
            let message = e.message.clone();
            let entry_id = e.base.id.clone();
            vec![
                HarnessEvent::MessageStart {
                    lane: lane.to_string(),
                    run_id: run_id.map(|s| s.to_string()),
                    message: message.clone(),
                    recovery: None,
                },
                HarnessEvent::MessageEnd {
                    lane: lane.to_string(),
                    run_id: run_id.map(|s| s.to_string()),
                    message: message.clone(),
                    entry_id: Some(entry_id.clone()),
                    recovery: None,
                },
                HarnessEvent::EntryAdded {
                    lane: lane.to_string(),
                    entry: entry.clone(),
                    recovery: None,
                },
            ]
        }
        _ => vec![HarnessEvent::EntryAdded {
            lane: lane.to_string(),
            entry: entry.clone(),
            recovery: None,
        }],
    }
}

/// 对应 `committedEntryEvents`。
pub fn committed_entry_events(
    entries: &[NewEntry],
    commit: &CommitResult,
    lane: &str,
    run_id: Option<&str>,
    first_write_index: usize,
) -> Vec<HarnessEvent> {
    let mut events = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let seq = commit
            .seqs
            .get(first_write_index + index)
            .copied()
            .unwrap_or(0);
        let materialized = materialize_committed_entry(entry, seq, commit.timestamp);
        events.extend(entry_lifecycle_events(&materialized, lane, run_id));
    }
    events
}

/// 对应 `readBoundedEntries`。
pub async fn read_bounded_entries<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    capability: &OperationState,
) -> Result<ContinueOperationResult<Vec<Entry>>, String> {
    let context = drive.context.clone();
    lane.continue_operation(
        capability,
        Box::new(move |state, _current, _meta, reader| {
            let context = context.clone();
            Box::pin(async move {
                let Some(tip_id) = state.tip_id else {
                    panic!(
                        "{}",
                        SessionInvariantError::new("Run operation has no Branch tip")
                    );
                };
                let query = StorageBranchScan {
                    scan: BranchScan {
                        start: Some(tip_id.clone()),
                        stop_at_type: Some(EntryType::Compaction),
                        order: Some(ScanOrder::NewestFirst),
                        ..Default::default()
                    },
                    start: tip_id,
                };
                let mut entries = reader
                    .scan_branch(query, &context)
                    .await
                    .expect("scan_branch failed");
                entries.reverse();
                OperationCommand::Return { result: entries }
            })
        }),
        &drive.context,
    )
    .await
}

/// 对应 `readBoundedContext`。
pub async fn read_bounded_context<L: Lane + ?Sized>(
    lane: &L,
    drive: &Drive,
    capability: &OperationState,
) -> Result<ContinueOperationResult<Vec<AgentMessage>>, String> {
    let entries = read_bounded_entries(lane, drive, capability).await?;
    let ContinueOperationResult::Result { value } = entries else {
        return Ok(ContinueOperationResult::CancelRequested);
    };
    let entry_projectors = lane.read_config().entry_projectors;
    let messages = build_session_context(&value, Some(&entry_projectors), &drive.context).await;
    Ok(ContinueOperationResult::Result { value: messages })
}

/// 对应 `readLaneQueues`。
pub async fn read_lane_queues(
    reader: &dyn SessionReader,
    inbox: &[InboxItem],
    context: &Context,
) -> Result<Vec<LaneQueuedItem>, String> {
    let mut queues = Vec::new();
    for item in inbox {
        let stored = reader
            .get_value(&pending_entry(&item.entry_id).erased(), context)
            .await?;
        let stored = stored.ok_or_else(|| {
            SessionInvariantError::new(format!(
                "Pending {:?} entry {} is missing its payload",
                item.kind, item.entry_id
            ))
            .to_string()
        })?;
        let pending: PendingEntry =
            serde_json::from_value(stored.value.clone()).map_err(|e| e.to_string())?;
        match pending {
            PendingEntry::Message { payload } => queues.push(LaneQueuedItem::Message {
                entry_id: item.entry_id.clone(),
                kind: item.kind,
                message: payload,
            }),
            PendingEntry::Custom {
                custom_type,
                payload,
            } => {
                if item.kind != crate::harness::session::types::InboxItemKind::Write {
                    return Err(SessionInvariantError::new(format!(
                        "Pending {:?} entry {} is not a message",
                        item.kind, item.entry_id
                    ))
                    .to_string());
                }
                queues.push(LaneQueuedItem::Custom {
                    entry_id: item.entry_id.clone(),
                    kind: item.kind,
                    custom_type,
                    data: payload,
                });
            }
        }
    }
    Ok(queues)
}

/// 对应 `readPendingMessages`。
pub async fn read_pending_messages(
    reader: &dyn SessionReader,
    ids: &[String],
    description: &str,
    context: &Context,
) -> Result<Vec<(String, AgentMessage)>, String> {
    let mut messages = Vec::new();
    for entry_id in ids {
        let value = reader
            .get_value(&pending_entry(entry_id).erased(), context)
            .await?;
        let value = value.ok_or_else(|| {
            SessionInvariantError::new(format!(
                "{description} {entry_id} is missing its message payload"
            ))
            .to_string()
        })?;
        let pending: PendingEntry =
            serde_json::from_value(value.value).map_err(|e| e.to_string())?;
        let PendingEntry::Message { payload } = pending else {
            return Err(SessionInvariantError::new(format!(
                "{description} {entry_id} is missing its message payload"
            ))
            .to_string());
        };
        messages.push((entry_id.clone(), payload));
    }
    Ok(messages)
}
