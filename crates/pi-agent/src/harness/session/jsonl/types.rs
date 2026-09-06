//! Rust 翻译自 packages/agent/src/harness/session/jsonl/types.ts

use serde::{Deserialize, Serialize};

use crate::harness::session::types::{SessionCreateOptions, SessionMetadata};

/// 对应 `JSONL_FORMAT_VERSION`。
pub const JSONL_FORMAT_VERSION: u32 = 4;
/// 对应 `JSONL_STORAGE_VERSION`。
pub const JSONL_STORAGE_VERSION: u32 = 1;

/// 对应 `JsonlStorageHeader`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonlStorageHeader {
    pub v: u32,
    pub kind: String,
    pub id: String,
    pub storage_version: u32,
    pub created_at: u64,
    pub cwd: String,
    pub parent_session_id: Option<String>,
    pub legacy_parent_session_path: Option<String>,
    pub next_seq: Option<u64>,
}

/// 对应 `JsonlSessionMetadata`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonlSessionMetadata {
    #[serde(flatten)]
    pub base: SessionMetadata,
    pub cwd: String,
    pub path: String,
    pub modified_at: u64,
}

/// 对应 `JsonlSessionCreateOptions`。
#[derive(Debug, Clone, Default)]
pub struct JsonlSessionCreateOptions {
    pub base: SessionCreateOptions,
    pub cwd: String,
}

/// 对应 `JsonlSessionListOptions`。
#[derive(Debug, Clone, Default)]
pub struct JsonlSessionListOptions {
    pub cwd: Option<String>,
}
