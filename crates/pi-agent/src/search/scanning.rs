//! Rust 翻译自 packages/agent/src/search/scanning.ts
//!
//! session 条目扫描搜索。注：TS 的 `AsyncIterable` 在 Rust 中简化为返回 `Vec`。

use std::sync::Arc;

use crate::harness::session::types::{
    Entry, EntryOrder, EntryQuery, SessionMetadata, SessionStorage,
};
use crate::search::index::{SessionSearch, SessionSearchOptions};

/// 对应 `SessionSearchCandidate`
#[derive(Debug, Clone)]
pub struct SessionSearchCandidate {
    pub entry_id: String,
    pub seq: u64,
    pub kind: String,
    pub timestamp: u64,
    pub text: String,
    pub fields: Option<serde_json::Value>,
}

/// 对应 `ScanningReadable`（Rust 中用 `Arc<dyn SessionStorage>`）。
pub type ScanningReadable = Arc<dyn SessionStorage>;

/// 对应 `ScanningSearchTextProjector`
pub type ScanningSearchTextProjector =
    Arc<dyn Fn(&SessionMetadata, &Entry, Option<&str>) -> String + Send + Sync>;

/// 对应 `ScanningReadableOptions`
#[derive(Clone)]
pub struct ScanningReadableOptions {
    pub project_text: Option<ScanningSearchTextProjector>,
    pub page_size: Option<usize>,
}

/// 对应 `ScanningSessionSearchHit`
#[derive(Debug, Clone)]
pub struct ScanningSessionSearchHit {
    pub session_id: String,
    pub entry_id: String,
    pub timestamp: u64,
    pub snippet: String,
}

/// 对应 `ScanningSessionSearchOptions`
#[allow(clippy::type_complexity)]
#[derive(Clone)]
pub struct ScanningSessionSearchOptions {
    pub project_text: Option<ScanningSearchTextProjector>,
    pub page_size: Option<usize>,
    pub match_fn: Option<Arc<dyn Fn(&str, &SessionSearchCandidate) -> bool + Send + Sync>>,
}

fn default_search_text(_metadata: &SessionMetadata, entry: &Entry, label: Option<&str>) -> String {
    let json = serde_json::to_string(entry).unwrap_or_default();
    match label {
        None => json,
        Some(label) => format!("{json} {label}"),
    }
}

/// 对应 `scanReadableEntries`
async fn scan_readable_entries(
    readable: &Arc<dyn SessionStorage>,
    metadata: &SessionMetadata,
    options: &ScanningReadableOptions,
    entry_types: Option<&Vec<String>>,
    limit: Option<usize>,
) -> Vec<SessionSearchCandidate> {
    let project_text = options.project_text.clone();
    let page_size = limit.unwrap_or_else(|| options.page_size.unwrap_or(100));
    let mut after_seq = 0u64;
    let mut result = Vec::new();

    loop {
        let query = EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            limit: Some(page_size),
            cursor: Some(crate::harness::session::types::EntryCursor { after_seq }),
            kind: None,
            custom_type: None,
        };
        let entries = readable.find_entries(&query).await.unwrap_or_default();
        if entries.is_empty() {
            break;
        }
        for entry in &entries {
            if let Some(types) = entry_types
                && !types.contains(&entry_type_str(entry).to_string())
            {
                continue;
            }
            let label = readable.get_label(entry.id()).await.unwrap_or(None);
            let text = match &project_text {
                Some(p) => p(metadata, entry, label.as_deref()),
                None => default_search_text(metadata, entry, label.as_deref()),
            };
            result.push(SessionSearchCandidate {
                entry_id: entry.id().to_string(),
                seq: entry_seq(entry),
                kind: entry_type_str(entry).to_string(),
                timestamp: entry_timestamp(entry),
                text,
                fields: label.map(|l| serde_json::json!({ "label": l })),
            });
        }
        after_seq = entries.last().map(entry_seq).unwrap_or(after_seq);
        if entries.len() < page_size {
            break;
        }
    }

    result
}

/// 对应 `scanningEntries`
pub async fn scanning_entries(
    readable: Arc<dyn SessionStorage>,
    options: &ScanningReadableOptions,
) -> Vec<SessionSearchCandidate> {
    let metadata = readable
        .get_metadata()
        .await
        .unwrap_or_else(|_| SessionMetadata {
            id: String::new(),
            created_at: 0,
            parent_session_id: None,
        });
    scan_readable_entries(&readable, &metadata, options, None, None).await
}

fn default_match(query_text: &str, candidate: &SessionSearchCandidate) -> bool {
    candidate.text.to_lowercase().contains(query_text)
}

/// 对应 `createScanningSessionSearch`
pub fn create_scanning_session_search(
    source: Arc<dyn SessionStorage>,
    options: ScanningSessionSearchOptions,
) -> Arc<dyn SessionSearch<ScanningSessionSearchHit>> {
    Arc::new(ScanningSearch { source, options })
}

struct ScanningSearch {
    source: Arc<dyn SessionStorage>,
    options: ScanningSessionSearchOptions,
}

impl SessionSearch<ScanningSessionSearchHit> for ScanningSearch {
    fn search(
        &self,
        text: &str,
        search_options: &SessionSearchOptions,
    ) -> Vec<ScanningSessionSearchHit> {
        let normalized = text.trim().to_lowercase();
        if normalized.is_empty() {
            return Vec::new();
        }
        if search_options.limit.map(|l| l == 0).unwrap_or(false) {
            return Vec::new();
        }
        if search_options
            .entry_types
            .as_ref()
            .map(|t| t.is_empty())
            .unwrap_or(false)
        {
            return Vec::new();
        }

        let metadata = match futures::executor::block_on(self.source.get_metadata()) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        let readable_options = ScanningReadableOptions {
            project_text: self.options.project_text.clone(),
            page_size: self.options.page_size,
        };
        let candidates = futures::executor::block_on(scan_readable_entries(
            &self.source,
            &metadata,
            &readable_options,
            search_options.entry_types.as_ref(),
            search_options.limit,
        ));

        let mut hits = Vec::new();
        for candidate in candidates {
            let matches = match &self.options.match_fn {
                Some(m) => m(&normalized, &candidate),
                None => default_match(&normalized, &candidate),
            };
            if !matches {
                continue;
            }
            hits.push(ScanningSessionSearchHit {
                session_id: metadata.id.clone(),
                entry_id: candidate.entry_id,
                timestamp: candidate.timestamp,
                snippet: candidate.text,
            });
            if let Some(limit) = search_options.limit
                && hits.len() >= limit
            {
                break;
            }
        }
        hits
    }
}

fn entry_type_str(entry: &Entry) -> &'static str {
    match entry {
        Entry::Message(_) => "message",
        Entry::ModelChange(_) => "model_change",
        Entry::ThinkingLevelChange(_) => "thinking_level_change",
        Entry::ActiveToolsChange(_) => "active_tools_change",
        Entry::Compaction(_) => "compaction",
        Entry::BranchSummary(_) => "branch_summary",
        Entry::Custom(_) => "custom",
    }
}

fn entry_seq(entry: &Entry) -> u64 {
    match entry {
        Entry::Message(e) => e.base.seq,
        Entry::ModelChange(e) => e.base.seq,
        Entry::ThinkingLevelChange(e) => e.base.seq,
        Entry::ActiveToolsChange(e) => e.base.seq,
        Entry::Compaction(e) => e.base.seq,
        Entry::BranchSummary(e) => e.base.seq,
        Entry::Custom(e) => e.base.seq,
    }
}

fn entry_timestamp(entry: &Entry) -> u64 {
    match entry {
        Entry::Message(e) => e.base.timestamp,
        Entry::ModelChange(e) => e.base.timestamp,
        Entry::ThinkingLevelChange(e) => e.base.timestamp,
        Entry::ActiveToolsChange(e) => e.base.timestamp,
        Entry::Compaction(e) => e.base.timestamp,
        Entry::BranchSummary(e) => e.base.timestamp,
        Entry::Custom(e) => e.base.timestamp,
    }
}
