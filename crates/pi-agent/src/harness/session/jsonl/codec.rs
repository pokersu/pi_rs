//! Rust 翻译自 packages/agent/src/harness/session/jsonl/codec.ts
//!
//! JSONL session header 解析（v4 与 legacy v3 识别）。

use serde_json::Value as Json;

use crate::harness::session::jsonl::types::{JSONL_FORMAT_VERSION, JsonlStorageHeader};

/// 对应 `LegacyV3SessionHeader`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyV3SessionHeader {
    #[serde(rename = "type")]
    pub header_type: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub parent_session: Option<String>,
}

fn is_record(value: &Json) -> bool {
    value.is_object()
}

/// 对应 `isLegacyV3SessionHeader`。
pub fn is_legacy_v3_session_header(value: &Json) -> bool {
    if !is_record(value) {
        return false;
    }
    value.get("type").and_then(|v| v.as_str()) == Some("session")
        && value.get("version").and_then(|v| v.as_u64()) == Some(3)
        && value.get("id").and_then(|v| v.as_str()).is_some()
        && value.get("cwd").and_then(|v| v.as_str()).is_some()
        && value.get("timestamp").and_then(|v| v.as_str()).is_some()
}

/// 对应 `isJsonlStorageHeader`。
pub fn is_jsonl_storage_header(value: &Json) -> bool {
    if !is_record(value) {
        return false;
    }
    value.get("kind").and_then(|v| v.as_str()) == Some("header")
        && value.get("v").and_then(|v| v.as_u64()) == Some(JSONL_FORMAT_VERSION as u64)
        && value.get("id").and_then(|v| v.as_str()).is_some()
        && value.get("cwd").and_then(|v| v.as_str()).is_some()
        && value
            .get("storageVersion")
            .and_then(|v| v.as_u64())
            .is_some()
        && value.get("createdAt").and_then(|v| v.as_u64()).is_some()
}

/// 对应 `JsonlParsedSessionHeader`。
#[derive(Debug, Clone)]
pub enum JsonlParsedSessionHeader {
    V4 { header: JsonlStorageHeader },
    V3Legacy { header: LegacyV3SessionHeader },
}

/// 对应 `parseJsonlSessionHeader`。
pub fn parse_jsonl_session_header(line: &str) -> Result<JsonlParsedSessionHeader, String> {
    let value: Json = serde_json::from_str(line)
        .map_err(|e| format!("Invalid JSONL session header: not valid JSON: {e}"))?;
    if is_jsonl_storage_header(&value) {
        let header: JsonlStorageHeader =
            serde_json::from_value(value).map_err(|e| e.to_string())?;
        return Ok(JsonlParsedSessionHeader::V4 { header });
    }
    if is_legacy_v3_session_header(&value) {
        let header: LegacyV3SessionHeader =
            serde_json::from_value(value).map_err(|e| e.to_string())?;
        return Ok(JsonlParsedSessionHeader::V3Legacy { header });
    }
    Err("Unsupported JSONL session header".to_string())
}
