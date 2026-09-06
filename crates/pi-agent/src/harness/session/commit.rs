//! Rust 翻译自 packages/agent/src/harness/session/commit.ts

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::harness::session::types::{
    CommitResult, Entry, NewEntry, SessionStats, UsageRow, Write,
};
use crate::harness::session::values::{ListWrite, ValueWrite};

/// 对应 `CommittedWrite`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum CommittedWrite {
    Entry {
        entry: Entry,
    },
    Usage {
        row: UsageRow,
    },
    ValueSet {
        seq: u64,
        namespace: String,
        key: String,
        value: Json,
    },
    ValueDelete {
        seq: u64,
        namespace: String,
        key: String,
    },
    ListAppend {
        seq: u64,
        namespace: String,
        key: String,
        value: Json,
    },
    ListDelete {
        seq: u64,
        namespace: String,
        key: String,
    },
}

impl CommittedWrite {
    pub fn seq(&self) -> u64 {
        match self {
            CommittedWrite::Entry { entry } => entry.base().seq,
            CommittedWrite::Usage { row } => row.seq,
            CommittedWrite::ValueSet { seq, .. }
            | CommittedWrite::ValueDelete { seq, .. }
            | CommittedWrite::ListAppend { seq, .. }
            | CommittedWrite::ListDelete { seq, .. } => *seq,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            CommittedWrite::Entry { .. } => "entry",
            CommittedWrite::Usage { .. } => "usage",
            CommittedWrite::ValueSet { .. } | CommittedWrite::ValueDelete { .. } => "value",
            CommittedWrite::ListAppend { .. } | CommittedWrite::ListDelete { .. } => "list",
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            CommittedWrite::Entry { entry } => Some(&entry.base().id),
            CommittedWrite::Usage { row } => Some(&row.id),
            _ => None,
        }
    }
}

/// 对应 `PreparedCommit`。
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedCommit {
    pub writes: Vec<CommittedWrite>,
    pub first_seq: u64,
    pub seqs: Vec<u64>,
    pub timestamp: u64,
}

/// 对应 `materializeCommittedEntry`：把 NewEntry 物化为带 seq/timestamp 的 Entry。
pub fn materialize_committed_entry(entry: &NewEntry, seq: u64, timestamp: u64) -> Entry {
    entry.clone().materialize(seq, timestamp)
}

/// 对应 `insertEntry`。
pub fn insert_entry(entry: NewEntry) -> Write {
    Write::Entry(crate::harness::session::types::EntryWrite {
        kind: crate::harness::session::types::WriteKind::Entry,
        entry,
    })
}

/// 对应 `insertUsage`。
pub fn insert_usage(row: UsageRow) -> Write {
    Write::Usage(crate::harness::session::types::UsageWrite {
        kind: crate::harness::session::types::WriteKind::Usage,
        row,
    })
}

/// 对应 `commitWrite`。
pub fn commit_write(write: &Write, seq: u64, timestamp: u64) -> CommittedWrite {
    match write {
        Write::Entry(entry_write) => CommittedWrite::Entry {
            entry: entry_write.entry.clone().materialize(seq, timestamp),
        },
        Write::Usage(usage_write) => {
            let mut row = usage_write.row.clone();
            row.seq = seq;
            CommittedWrite::Usage { row }
        }
        Write::Value(value_write) => match value_write {
            ValueWrite::Set(w) => CommittedWrite::ValueSet {
                seq,
                namespace: w.namespace.clone(),
                key: w.key.clone(),
                value: w.value.clone(),
            },
            ValueWrite::Delete(w) => CommittedWrite::ValueDelete {
                seq,
                namespace: w.namespace.clone(),
                key: w.key.clone(),
            },
        },
        Write::List(list_write) => match list_write {
            ListWrite::Append(w) => CommittedWrite::ListAppend {
                seq,
                namespace: w.namespace.clone(),
                key: w.key.clone(),
                value: w.value.clone(),
            },
            ListWrite::Delete(w) => CommittedWrite::ListDelete {
                seq,
                namespace: w.namespace.clone(),
                key: w.key.clone(),
            },
        },
    }
}

/// 对应 `prepareStorageCommit`。
pub fn prepare_storage_commit(writes: &[Write], first_seq: u64, timestamp: u64) -> PreparedCommit {
    let committed: Vec<CommittedWrite> = writes
        .iter()
        .enumerate()
        .map(|(index, write)| commit_write(write, first_seq + index as u64, timestamp))
        .collect();
    PreparedCommit {
        seqs: committed.iter().map(|w| w.seq()).collect(),
        writes: committed,
        first_seq,
        timestamp,
    }
}

/// 对应 `CommitValidationState`。
pub trait CommitValidationState {
    fn has_entry_or_usage_id(&self, id: &str) -> bool;
    fn has_entry_id(&self, id: &str) -> bool;
}

/// 对应 `validateCommittedWrites`。
pub fn validate_committed_writes(
    writes: &[CommittedWrite],
    first_seq: u64,
    state: &dyn CommitValidationState,
) -> Result<(), String> {
    let mut previous_seq = first_seq.saturating_sub(1);
    let mut transaction_ids = HashSet::new();
    let mut transaction_entry_ids = HashSet::new();
    for write in writes {
        if write.seq() <= previous_seq {
            return Err(format!("Non-monotonic storage sequence: {}", write.seq()));
        }
        previous_seq = write.seq();
        let Some(id) = write.id() else {
            continue;
        };
        if state.has_entry_or_usage_id(id) || transaction_ids.contains(id) {
            return Err(format!("Duplicate entry or usage id: {id}"));
        }
        if let CommittedWrite::Entry { entry } = write
            && let Some(parent) = &entry.base().parent_id
            && !state.has_entry_id(parent)
            && !transaction_entry_ids.contains(parent)
        {
            return Err(format!("Missing parent entry: {parent}"));
        }
        transaction_ids.insert(id.to_string());
        if matches!(write, CommittedWrite::Entry { .. }) {
            transaction_entry_ids.insert(id.to_string());
        }
    }
    Ok(())
}

/// 未使用抑制：CommitResult/SessionStats 在结果构造处使用。
#[allow(unused)]
fn _result_shape(_: &CommitResult) -> &SessionStats {
    unimplemented!()
}
