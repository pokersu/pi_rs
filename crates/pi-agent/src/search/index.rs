//! Rust 翻译自 packages/agent/src/search/index.ts
//!
//! 会话搜索服务接口（原版 v0.85 起仅保留接口契约，具体实现由后端提供）。

/// 对应 `SearchQuery`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchQuery {
    pub text: String,
    pub limit: Option<usize>,
}

/// 对应 `SessionSearchHit`。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub score: Option<f64>,
    pub top: Option<SessionSearchTop>,
}

/// 对应 `top` 字段。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSearchTop {
    pub entry_id: String,
    pub snippet: Option<String>,
    pub timestamp: u64,
}

/// 对应 `EntrySearchHit`。
#[derive(Debug, Clone, PartialEq)]
pub struct EntrySearchHit {
    pub session_id: String,
    pub entry_id: String,
    pub timestamp: u64,
    pub snippet: Option<String>,
    pub score: Option<f64>,
}

/// 对应 `SessionSearchService`。
#[async_trait::async_trait]
pub trait SessionSearchService: Send + Sync {
    async fn search_sessions(&self, query: SearchQuery) -> Vec<SessionSearchHit>;
    async fn search_entries(&self, query: SearchQuery) -> Vec<EntrySearchHit>;
    async fn sync(&self);
    fn notify(&self, session_id: &str);
    async fn remove(&self, session_id: &str);
    async fn close(&self);
}
