//! Rust 翻译自 packages/agent/src/search/index.ts

use crate::harness::session::types::Entry;

pub use crate::search::scanning::{
    ScanningReadableOptions, ScanningSearchTextProjector, ScanningSessionSearchHit,
    ScanningSessionSearchOptions, SessionSearchCandidate, create_scanning_session_search,
    scanning_entries,
};

/// 对应 `SessionSearchOptions`
#[derive(Debug, Clone, Default)]
pub struct SessionSearchOptions {
    pub entry_types: Option<Vec<String>>,
    pub limit: Option<usize>,
    pub signal: Option<pi_ai::AbortSignal>,
}

/// 对应 `SessionSearchHit`
#[derive(Debug, Clone)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub entry_id: String,
}

/// 对应 `SessionSearch`
pub trait SessionSearch<T: Clone + Send = SessionSearchHit>: Send + Sync {
    fn search(&self, text: &str, options: &SessionSearchOptions) -> Vec<T>;
}

// 保留 Entry 引用。
#[allow(unused)]
fn _unused(_: Entry) {}
