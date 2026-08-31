//! Rust 翻译自 packages/agent/src/harness/session/jsonl/codec.ts（简化）
//!
//! JSONL 编解码：Entry/LaneRecord 与 JSON 字符串互转。

use crate::harness::session::types::{Entry, LaneRecord};

/// 对应 encode（Entry → JSON 字符串）。
pub fn encode_entry(entry: &Entry) -> String {
    serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string())
}

/// 对应 encode（LaneRecord → JSON 字符串）。
pub fn encode_record(record: &LaneRecord) -> String {
    serde_json::to_string(record).unwrap_or_else(|_| "{}".to_string())
}

/// 对应 decode（JSON 字符串 → Entry）。
pub fn decode_entry(line: &str) -> Result<Entry, String> {
    serde_json::from_str(line).map_err(|e| e.to_string())
}

/// 对应 decode（JSON 字符串 → LaneRecord）。
pub fn decode_record(line: &str) -> Result<LaneRecord, String> {
    serde_json::from_str(line).map_err(|e| e.to_string())
}
