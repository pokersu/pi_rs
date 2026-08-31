//! Rust 翻译自 packages/agent/src/harness/session/jsonl/types.ts

use serde::{Deserialize, Serialize};

use crate::harness::session::types::SessionMetadata;

/// 对应 `JsonlSessionRepoOptions`
#[derive(Debug, Clone)]
pub struct JsonlSessionRepoOptions {
    pub sessions_root: String,
}

/// 对应 `JsonlSessionMetadata`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonlSessionMetadata {
    #[serde(flatten)]
    pub base: SessionMetadata,
    pub cwd: String,
    pub path: String,
    pub modified_at: u64,
    pub source_format: u32,
    pub legacy_parent_session_path: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// 对应 `JsonlSessionCreateOptions`
#[derive(Debug, Clone, Default)]
pub struct JsonlSessionCreateOptions {
    pub cwd: String,
    pub metadata: Option<serde_json::Value>,
}

/// 对应 `JsonlSessionListOptions`
#[derive(Debug, Clone, Default)]
pub struct JsonlSessionListOptions {
    pub cwd: Option<String>,
}

/// 对应 `JsonlV4Header`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonlV4Header {
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub created_at: u64,
    pub cwd: String,
    pub parent_session_id: Option<String>,
    pub legacy_parent_session_path: Option<String>,
    pub metadata: Option<serde_json::Value>,
}
