//! Rust 翻译自 packages/agent/src/harness/session/jsonl/io.ts
//!
//! JSONL IO 工具：header 读取、transaction 解析/序列化、原子发布。

use serde_json::Value as Json;

use crate::harness::context::Context;
use crate::harness::session::commit::CommittedWrite;
use crate::harness::session::jsonl::codec::{JsonlParsedSessionHeader, parse_jsonl_session_header};
use crate::harness::session::jsonl::types::JsonlStorageHeader;
use crate::harness::types::{FileError, FileSystem, TextLineReader};

/// 对应 `fileValue`：失败时抛出（Rust panic）。
pub fn file_value<T>(result: Result<T, FileError>, action: &str) -> T {
    result.unwrap_or_else(|e| panic!("{action}: {}", e.message))
}

/// 对应 `readJsonlHeader`：读取首行并解析 v4/legacy-v3 header。
pub async fn read_jsonl_header(
    reader: &dyn TextLineReader,
    path: &str,
    context: &Context,
) -> Result<JsonlParsedSessionHeader, String> {
    let line = file_value(
        reader.read_line(context).await,
        &format!("Failed to read JSONL storage {path}"),
    );
    let Some(line) = line else {
        return Err(format!("Invalid JSONL storage {path}: missing header"));
    };
    if !line.terminated || line.text.is_empty() {
        return Err(format!("Invalid JSONL storage {path}: missing header"));
    }
    parse_jsonl_session_header(&line.text)
        .map_err(|e| format!("Invalid JSONL storage {path}: invalid header: {e}"))
}

/// 对应 `parseJsonlTransaction`：解析一行 transaction（单 write 或数组）。
pub fn parse_jsonl_transaction(line: &str) -> Result<Vec<CommittedWrite>, String> {
    let value: Json = serde_json::from_str(line)
        .map_err(|e| format!("Invalid JSONL transaction: not valid JSON: {e}"))?;
    if value.is_array() {
        serde_json::from_value(value).map_err(|e| e.to_string())
    } else {
        let write: CommittedWrite = serde_json::from_value(value).map_err(|e| e.to_string())?;
        Ok(vec![write])
    }
}

/// 对应 `serializeJsonlTransaction`：单 write 序列化为对象，否则序列化为数组。
pub fn serialize_jsonl_transaction(writes: &[CommittedWrite]) -> String {
    if writes.len() == 1 {
        serde_json::to_string(&writes[0]).unwrap_or_else(|_| "null".to_string())
    } else {
        serde_json::to_string(writes).unwrap_or_else(|_| "null".to_string())
    }
}

/// 对应 `publishFileAtomically`：先写 temp 再 rename，失败时清理 temp。
///
/// `write_content` 同步收集要发布的内容行；`append` 是返回 `()` 的行收集回调。
pub async fn publish_file_atomically(
    file_system: &dyn FileSystem,
    destination_path: &str,
    context: &Context,
    write_content: impl FnOnce(&mut dyn FnMut(&str)) -> Result<(), String>,
) -> Result<(), String> {
    let temp_path = format!("{destination_path}.tmp");
    let mut lines: Vec<String> = Vec::new();
    write_content(&mut |content: &str| {
        lines.push(content.to_string());
    })?;
    let content = lines.concat();
    file_system
        .write_file(&temp_path, content.as_bytes(), context)
        .await
        .map_err(|e| {
            format!(
                "Failed to stage JSONL storage {destination_path}: {}",
                e.message
            )
        })?;
    if let Err(e) = file_system
        .rename_file(&temp_path, destination_path, context)
        .await
    {
        let _ = file_system.remove(&temp_path, true, true, context).await;
        return Err(format!(
            "Failed to publish JSONL storage {destination_path}: {}",
            e.message
        ));
    }
    Ok(())
}

/// 对应 `publishJsonl`：发布 header + 完整 transactions（原子）。
pub async fn publish_jsonl(
    file_system: &dyn FileSystem,
    destination_path: &str,
    header: &JsonlStorageHeader,
    context: &Context,
    write_transactions: impl FnOnce(&mut dyn FnMut(&[CommittedWrite])) -> Result<(), String>,
) -> Result<(), String> {
    let header_line = serde_json::to_string(header).map_err(|e| e.to_string())?;
    publish_file_atomically(file_system, destination_path, context, |append| {
        append(&format!("{header_line}\n"));
        write_transactions(&mut |writes| {
            append(&format!("{}\n", serialize_jsonl_transaction(writes)));
        })
    })
    .await
}
