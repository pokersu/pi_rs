//! Rust 翻译自 packages/agent/src/harness/session/jsonl/fork.ts（v4 两阶段流式 fork，legacy-v3 未复刻）
//!
//! 索引源 JSONL → 选择 fork plan → 流式投影 selected entries + current state → 原子发布。

use std::collections::{HashMap, HashSet};

use serde_json::Value as Json;

use crate::harness::context::Context;
use crate::harness::session::commit::CommittedWrite;
use crate::harness::session::fork_policy::{
    ForkCurrentStatePlan, project_fork_current_state_write,
};
use crate::harness::session::jsonl::codec::JsonlParsedSessionHeader;
use crate::harness::session::jsonl::io::{
    file_value, parse_jsonl_transaction, publish_jsonl, read_jsonl_header,
};
use crate::harness::session::jsonl::types::{JSONL_STORAGE_VERSION, JsonlStorageHeader};
use crate::harness::session::types::ForkOptions;
use crate::harness::types::{FileSystem, TextLineReader};

fn physical_key(namespace: &str, key: &str) -> String {
    format!("{namespace}\u{0}{key}")
}

fn reaches_fork_boundary(
    writes: &[CommittedWrite],
    stop_before_seq: Option<u64>,
) -> Result<bool, String> {
    let Some(stop_before_seq) = stop_before_seq else {
        return Ok(false);
    };
    if writes.is_empty() {
        return Ok(false);
    }
    let first = writes.first().unwrap().seq();
    let last = writes.last().unwrap().seq();
    if first >= stop_before_seq {
        return Ok(true);
    }
    if last >= stop_before_seq {
        return Err(format!(
            "JSONL transaction crosses fork sequence boundary {stop_before_seq}"
        ));
    }
    Ok(false)
}

/// 读取 header 之后的完整 transactions，绝不在序列边界拆分 transaction。
async fn read_jsonl_fork_transactions(
    reader: &dyn TextLineReader,
    path: &str,
    stop_before_seq: Option<u64>,
    context: &Context,
) -> Result<Vec<Vec<CommittedWrite>>, String> {
    let mut transactions = Vec::new();
    loop {
        let line = file_value(
            reader.read_line(context).await,
            &format!("Failed to read JSONL fork source {path}"),
        );
        let Some(line) = line else {
            break;
        };
        if !line.terminated {
            break;
        }
        let writes = parse_jsonl_transaction(&line.text)?;
        if reaches_fork_boundary(&writes, stop_before_seq)? {
            break;
        }
        transactions.push(writes);
    }
    Ok(transactions)
}

struct JsonlForkIndex {
    current_scalar_seqs: HashMap<String, u64>,
    branch_tips: HashMap<String, Option<String>>,
    first_surviving_list_seqs: HashMap<String, u64>,
    entry_parents: HashMap<String, Option<String>>,
    copied_entry_ids: HashSet<String>,
    lane_configs: HashSet<String>,
    lane_states: HashSet<String>,
}

impl JsonlForkIndex {
    fn new() -> Self {
        Self {
            current_scalar_seqs: HashMap::new(),
            branch_tips: HashMap::new(),
            first_surviving_list_seqs: HashMap::new(),
            entry_parents: HashMap::new(),
            copied_entry_ids: HashSet::new(),
            lane_configs: HashSet::new(),
            lane_states: HashSet::new(),
        }
    }

    fn apply_entry(&mut self, id: &str, parent_id: Option<String>) {
        self.entry_parents.insert(id.to_string(), parent_id);
    }

    fn apply_writes(&mut self, writes: &[CommittedWrite]) {
        for write in writes {
            match write {
                CommittedWrite::Entry { entry } => {
                    self.apply_entry(&entry.base().id, entry.base().parent_id.clone());
                }
                CommittedWrite::ValueSet {
                    seq,
                    namespace,
                    key,
                    value,
                } => {
                    let pkey = physical_key(namespace, key);
                    self.current_scalar_seqs.insert(pkey, *seq);
                    self.apply_lane_value(namespace, key, Some(value));
                }
                CommittedWrite::ValueDelete { namespace, key, .. } => {
                    let pkey = physical_key(namespace, key);
                    self.current_scalar_seqs.remove(&pkey);
                    self.apply_lane_value(namespace, key, None);
                }
                CommittedWrite::ListAppend {
                    seq,
                    namespace,
                    key,
                    ..
                } => {
                    let pkey = physical_key(namespace, key);
                    self.first_surviving_list_seqs.entry(pkey).or_insert(*seq);
                }
                CommittedWrite::ListDelete { namespace, key, .. } => {
                    let pkey = physical_key(namespace, key);
                    self.first_surviving_list_seqs.remove(&pkey);
                }
                CommittedWrite::Usage { .. } => {}
            }
        }
    }

    fn apply_lane_value(&mut self, namespace: &str, key: &str, value: Option<&Json>) {
        let present = value.is_some();
        match namespace {
            "pi.branch.tip" => {
                if let Some(value) = value {
                    self.branch_tips.insert(
                        key.to_string(),
                        serde_json::from_value(value.clone()).unwrap_or(None),
                    );
                } else {
                    self.branch_tips.remove(key);
                }
            }
            "pi.lane.config" => {
                if present {
                    self.lane_configs.insert(key.to_string());
                } else {
                    self.lane_configs.remove(key);
                }
            }
            "pi.lane.state" => {
                if present {
                    self.lane_states.insert(key.to_string());
                } else {
                    self.lane_states.remove(key);
                }
            }
            _ => {}
        }
    }

    fn get_branch_tip(&self, branch: &str) -> Option<Option<String>> {
        self.branch_tips.get(branch).cloned()
    }

    fn has_complete_lane(&self, branch: &str) -> bool {
        self.lane_configs.contains(branch) && self.lane_states.contains(branch)
    }

    fn get_current_scalar_seq(&self, namespace: &str, key: &str) -> Option<u64> {
        self.current_scalar_seqs
            .get(&physical_key(namespace, key))
            .copied()
    }

    fn is_surviving_list_element(&self, namespace: &str, key: &str, seq: u64) -> bool {
        self.first_surviving_list_seqs
            .get(&physical_key(namespace, key))
            .map(|first| seq >= *first)
            .unwrap_or(false)
    }

    fn get_parent(&self, entry_id: &str) -> Option<Option<String>> {
        self.entry_parents.get(entry_id).cloned()
    }

    fn select_entry(&mut self, entry_id: &str) {
        self.copied_entry_ids.insert(entry_id.to_string());
    }

    fn is_entry_selected(&self, entry_id: &str) -> bool {
        self.copied_entry_ids.contains(entry_id)
    }
}

/// 验证 source lanes 并返回 fork plan（无文件 I/O）。
fn select_jsonl_fork(index: &mut JsonlForkIndex, options: &ForkOptions) -> ForkCurrentStatePlan {
    match options {
        ForkOptions::Tree { .. } => ForkCurrentStatePlan::Tree,
        ForkOptions::Branch {
            branch,
            entry_id,
            position,
            ..
        } => {
            // 手动遍历（避免 select_branch_fork 闭包对 index 同时 & 和 &mut 的借用冲突）。
            let Some(tip) = index.get_branch_tip(branch) else {
                panic!("Unknown source branch: {branch:?}");
            };
            let requested = entry_id.clone().or(tip.clone());
            let mut found = requested.is_none();
            let mut destination_tip: Option<String> = None;
            let mut current = tip;
            while let Some(id) = current {
                let Some(parent_id) = index.get_parent(&id) else {
                    panic!("Corrupt source branch: missing parent {id}");
                };
                if Some(id.clone()) == requested {
                    found = true;
                    destination_tip = if position
                        == &Some(crate::harness::session::types::ForkPosition::Before)
                    {
                        parent_id.clone()
                    } else {
                        Some(id.clone())
                    };
                    if position != &Some(crate::harness::session::types::ForkPosition::Before) {
                        index.select_entry(&id);
                    }
                } else if found {
                    index.select_entry(&id);
                }
                current = parent_id;
            }
            if !found {
                panic!("Fork entry {requested:?} is not on source branch {branch:?}");
            }
            if !index.has_complete_lane(branch) {
                panic!("Source branch {branch:?} is not a configured AgentLane");
            }
            ForkCurrentStatePlan::Branch {
                branch: branch.clone(),
                destination_tip,
            }
        }
    }
}

/// 投影/过滤单个 write：selected entries + current scalar/list state。
fn project_jsonl_fork_write(
    write: &CommittedWrite,
    index: &JsonlForkIndex,
    plan: &ForkCurrentStatePlan,
    is_entry_copied: impl Fn(&str) -> bool,
) -> Option<CommittedWrite> {
    match write {
        CommittedWrite::Entry { entry } => {
            if is_entry_copied(&entry.base().id) {
                Some(write.clone())
            } else {
                None
            }
        }
        CommittedWrite::ValueSet {
            seq,
            namespace,
            key,
            value,
        } => {
            if index.get_current_scalar_seq(namespace, key) != Some(*seq) {
                return None;
            }
            let projected = project_fork_current_state_write(write, plan, &is_entry_copied)?;
            let _ = value;
            Some(projected)
        }
        CommittedWrite::ListAppend {
            seq,
            namespace,
            key,
            ..
        } => {
            if !index.is_surviving_list_element(namespace, key, *seq) {
                return None;
            }
            project_fork_current_state_write(write, plan, &is_entry_copied)
        }
        CommittedWrite::Usage { .. }
        | CommittedWrite::ValueDelete { .. }
        | CommittedWrite::ListDelete { .. } => None,
    }
}

/// 对应 `JsonlForkInput`（legacy-v3 未复刻）。
pub struct JsonlForkSourceMetadata {
    pub id: String,
    pub cwd: String,
    pub path: String,
}

pub enum JsonlForkInput {
    Open {
        metadata: JsonlForkSourceMetadata,
        next_seq: u64,
    },
    Closed {
        metadata: JsonlForkSourceMetadata,
    },
}

async fn read_jsonl_fork_header(
    reader: &dyn TextLineReader,
    source: &JsonlForkSourceMetadata,
    context: &Context,
) -> Result<JsonlStorageHeader, String> {
    let parsed = read_jsonl_header(reader, &source.path, context).await?;
    let JsonlParsedSessionHeader::V4 { header } = parsed else {
        return Err(format!(
            "Invalid JSONL storage {}: expected format 4 header",
            source.path
        ));
    };
    if header.id != source.id || header.cwd != source.cwd {
        return Err(format!(
            "Session identity does not match header: {}",
            source.id
        ));
    }
    if header.storage_version != JSONL_STORAGE_VERSION {
        return Err(format!(
            "Session {} uses unsupported storage version {}",
            source.id, header.storage_version
        ));
    }
    Ok(header)
}

/// 索引 fork 输入：折叠完整 transactions，返回索引 + nextSeq 高水位。不做 destination 写入。
async fn index_fork_input(
    input: &JsonlForkInput,
    file_system: &dyn FileSystem,
    context: &Context,
) -> Result<(JsonlForkIndex, u64), String> {
    let mut index = JsonlForkIndex::new();
    let metadata = match input {
        JsonlForkInput::Open { metadata, .. } | JsonlForkInput::Closed { metadata } => metadata,
    };
    let reader = file_value(
        file_system
            .open_text_line_reader(&metadata.path, context)
            .await,
        &format!("Failed to open JSONL fork source {}", metadata.path),
    );
    let reader_ref: &dyn TextLineReader = reader.as_ref();
    let stop_before_seq = match input {
        JsonlForkInput::Open { next_seq, .. } => Some(*next_seq),
        JsonlForkInput::Closed { .. } => None,
    };
    let header = read_jsonl_fork_header(reader_ref, metadata, context).await?;
    let transactions =
        read_jsonl_fork_transactions(reader_ref, &metadata.path, stop_before_seq, context).await?;
    let mut highest_complete_seq = 0;
    for writes in &transactions {
        index.apply_writes(writes);
        if let Some(last) = writes.last() {
            highest_complete_seq = last.seq();
        }
    }
    let next_seq = match input {
        JsonlForkInput::Open { next_seq, .. } => *next_seq,
        JsonlForkInput::Closed { .. } => header.next_seq.unwrap_or(1).max(highest_complete_seq + 1),
    };
    Ok((index, next_seq))
}

/// 复刻 `runJsonlFork`：索引 → 选择 → 流式投影并原子发布（不打开 destination session）。
pub async fn run_jsonl_fork(
    input: JsonlForkInput,
    file_system: &dyn FileSystem,
    destination_path: &str,
    destination_header: JsonlStorageHeader,
    options: &ForkOptions,
    context: &Context,
) -> Result<(), String> {
    let (mut index, next_seq) = index_fork_input(&input, file_system, context).await?;
    let plan = select_jsonl_fork(&mut index, options);
    let metadata = match &input {
        JsonlForkInput::Open { metadata, .. } | JsonlForkInput::Closed { metadata } => metadata,
    };

    // 重新读源，流式投影（读侧不保留 entry payload）。
    let reader = file_value(
        file_system
            .open_text_line_reader(&metadata.path, context)
            .await,
        &format!("Failed to open JSONL fork source {}", metadata.path),
    );
    let reader_ref: &dyn TextLineReader = reader.as_ref();
    let stop_before_seq = match &input {
        JsonlForkInput::Open { next_seq, .. } => Some(*next_seq),
        JsonlForkInput::Closed { .. } => None,
    };
    read_jsonl_fork_header(reader_ref, metadata, context).await?;
    let transactions =
        read_jsonl_fork_transactions(reader_ref, &metadata.path, stop_before_seq, context).await?;
    drop(reader);

    // 投影到收集的 writes（单 write 一个 transaction，对齐原版每次 append([projected])）。
    let is_entry_copied = |entry_id: &str| match &plan {
        ForkCurrentStatePlan::Tree => true,
        ForkCurrentStatePlan::Branch { .. } => index.is_entry_selected(entry_id),
    };
    let mut projected_writes: Vec<CommittedWrite> = Vec::new();
    for writes in &transactions {
        for write in writes {
            if let Some(projected) =
                project_jsonl_fork_write(write, &index, &plan, |entry_id| is_entry_copied(entry_id))
            {
                projected_writes.push(projected);
            }
        }
    }

    let header = JsonlStorageHeader {
        next_seq: Some(next_seq),
        ..destination_header
    };
    publish_jsonl(file_system, destination_path, &header, context, |append| {
        for write in &projected_writes {
            append(std::slice::from_ref(write));
        }
        Ok(())
    })
    .await
}
