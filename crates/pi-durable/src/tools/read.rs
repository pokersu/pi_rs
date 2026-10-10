//! 对应 `tools/read.ts`：读取文本文件（含截断与续读诊断）。

use std::sync::{Arc, LazyLock};

use serde_json::{Value as JsonValue, json};

use crate::chord::context::Context;
use crate::env::decode::{RangeDecoder, starts_with_bom};
use crate::env::{BinaryReader, FileInfo, LineScan};
use crate::harness::define::define_tool;
use crate::harness::output::character_end;
use crate::harness::types::{
    ToolDiagnostic, ToolDiagnosticSeverity, ToolExecutionApi, ToolExecutionResult, ToolRegistration,
};
use crate::session::SessionError;
use crate::tools::env::require_env;
use crate::tools::image::{ByteSource, detect_supported_image_mime_type_of};
use crate::tools::path_utils::resolve_read_tool_path;
use crate::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationOptions, TruncationResult, format_size,
    truncate_head_of, utf8_byte_length,
};

const READ_CHUNK: usize = 64 * 1024;

static SCHEMA: LazyLock<JsonValue> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "number", "description": "Maximum number of lines to read" }
        },
        "required": ["path"]
    })
});

/// 对应 `ReadToolDetails`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadToolDetails {
    /// 显示文本是如何被裁剪的；文本本身是结果内容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationDetails>,
}

/// `TruncationResult` 去掉 `content`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationDetails {
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_by: Option<String>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    /// 生效的行数上限（对应 `TruncationResult.maxLines`）。
    pub max_lines: usize,
    /// 生效的字节上限（对应 `TruncationResult.maxBytes`）。
    pub max_bytes: usize,
}

fn truncation_details(
    result: &TruncationResult,
    output_bytes: usize,
    output_lines: usize,
) -> TruncationDetails {
    TruncationDetails {
        truncated: result.truncated,
        truncated_by: result.truncated_by.map(|by| match by {
            crate::truncate::TruncatedBy::Lines => "lines".to_string(),
            crate::truncate::TruncatedBy::Bytes => "bytes".to_string(),
        }),
        total_lines: result.total_lines,
        total_bytes: result.total_bytes,
        output_lines,
        output_bytes,
        last_line_partial: result.last_line_partial,
        first_line_exceeds_limit: result.first_line_exceeds_limit,
        max_lines: result.max_lines,
        max_bytes: result.max_bytes,
    }
}

/// `Array.prototype.slice` 对下标的转换：NaN 是 0，其他向零截断。
fn slice_index(value: f64) -> i64 {
    if value.is_nan() {
        0
    } else {
        value.trunc() as i64
    }
}

/// 解码字节区间 `[start, end)` 的开头，作为整个文件的一部分解码。
async fn read_head(
    reader: &dyn BinaryReader,
    start: usize,
    end: usize,
    skip_bom: bool,
    context: &dyn Context,
) -> Result<String, SessionError> {
    let mut decoder = RangeDecoder::new();
    let mut text = String::new();
    let mut newlines = 0usize;
    let mut position = if skip_bom && start == 0 { 3 } else { start };
    while position < end {
        let bytes = reader
            .read(position as u64, (end - position).min(READ_CHUNK), context)
            .await?;
        if bytes.is_empty() {
            break;
        }
        position += bytes.len();
        let decoded = decoder.decode(&bytes);
        newlines += decoded.matches('\n').count();
        text.push_str(&decoded);
        if newlines >= DEFAULT_MAX_LINES || utf8_byte_length(&text) > DEFAULT_MAX_BYTES + 1 {
            return Ok(text);
        }
    }
    text.push_str(&decoder.flush());
    Ok(text)
}

struct ReadTool;

#[async_trait::async_trait]
impl ToolRegistration for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a text file. Output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
    }

    fn parameters(&self) -> &JsonValue {
        &SCHEMA
    }

    async fn execute(
        &self,
        args: JsonValue,
        api: Arc<dyn ToolExecutionApi>,
        context: Arc<dyn Context>,
    ) -> Result<ToolExecutionResult, SessionError> {
        let path = args["path"].as_str().unwrap_or_default().to_string();
        let offset = args["offset"].as_f64();
        let limit = args["limit"].as_f64();
        let env = require_env(api.as_ref())?;
        let absolute_path = resolve_read_tool_path(env.as_ref(), &path, context.as_ref()).await?;
        let reader = env
            .open_binary_reader(&absolute_path, None, context.as_ref())
            .await?;
        // 并发写者可能在扫描与读取之间改动文件：追加（增长的日志）保持扫描字节不变；
        // 缩小或被原地重写的文件会再读一次。
        let result = async {
            for attempt in 0..2 {
                let before = reader.info(context.as_ref()).await?;
                let result = read_text(
                    reader.as_ref(),
                    &before,
                    &path,
                    offset,
                    limit,
                    context.as_ref(),
                )
                .await?;
                let after = reader.info(context.as_ref()).await?;
                if after.size > before.size
                    || (after.size == before.size && after.mtime_ms == before.mtime_ms)
                {
                    return Ok(result);
                }
                if attempt == 1 {
                    return Err(SessionError::Message(format!(
                        "{path} changed while it was read"
                    )));
                }
            }
            unreachable!("attempt loop")
        }
        .await;
        reader.close(context.as_ref()).await;
        result
    }
}

/// 打开文件的读取结果。
async fn read_text(
    reader: &dyn BinaryReader,
    info: &FileInfo,
    path: &str,
    offset: Option<f64>,
    limit: Option<f64>,
    context: &dyn Context,
) -> Result<ToolExecutionResult, SessionError> {
    let mime_type = detect_supported_image_mime_type_of(&ByteSource {
        size: info.size,
        reader,
        context,
    })
    .await?;
    if let Some(mime_type) = mime_type {
        return Ok(ToolExecutionResult {
            content: Some(Vec::new()),
            is_error: Some(true),
            diagnostics: vec![ToolDiagnostic {
                severity: ToolDiagnosticSeverity::Error,
                code: Some("unsupported_image".to_string()),
                message: format!(
                    "{path} is an image ({mime_type}); reading images is not supported"
                ),
            }],
            ..Default::default()
        });
    }

    let start_line = offset.map_or(0.0, |offset| (offset - 1.0).max(0.0));
    let start_line_display = (start_line + 1.0) as i64;
    let slice_start = slice_index(start_line);
    let scan_start = slice_start.max(0) as usize;
    let requested_end =
        limit.map(|limit| (scan_start as i64 + 1).max(slice_index(start_line + limit)));
    let scan_end = requested_end.and_then(|end| (end >= 0).then_some(end as usize));

    let scan_of = |end_line: Option<usize>| reader.scan_lines(scan_start, end_line, context);
    let mut scan: LineScan = scan_of(scan_end).await?;
    let total_file_lines = scan.newlines + 1;
    if start_line >= total_file_lines as f64 {
        return Err(SessionError::Message(format!(
            "Offset {} is beyond end of file ({} lines total)",
            offset.unwrap_or(0.0) as i64,
            total_file_lines
        )));
    }

    let mut user_limited_lines: Option<usize> = None;
    let mut selected_line_count = total_file_lines - slice_start.max(0) as usize;
    if let Some(limit) = limit {
        let end_line = (start_line + limit).min(total_file_lines as f64);
        user_limited_lines = Some((end_line - start_line) as usize);
        let relative_end = slice_index(end_line);
        let slice_end = if relative_end < 0 {
            (total_file_lines as i64 + relative_end).max(0)
        } else {
            relative_end
        };
        selected_line_count = (slice_end - slice_start.max(0)).max(0) as usize;
        if selected_line_count > 0 && relative_end < 0 {
            scan = scan_of(Some(slice_end as usize)).await?;
        }
    }
    let empty = selected_line_count == 0;
    let ends_with_newline =
        !empty && scan.last_line_start == scan.end && scan.last_line_start > scan.start;
    let totals = (
        if empty || scan.selected_bytes == 0 {
            0
        } else {
            selected_line_count - usize::from(ends_with_newline)
        },
        if empty { 0 } else { scan.selected_bytes },
    );
    let first_bytes = reader.read(0, 3, context).await?;
    let head = if empty {
        String::new()
    } else {
        read_head(
            reader,
            scan.start,
            scan.end,
            starts_with_bom(&first_bytes),
            context,
        )
        .await?
    };

    let truncation = truncate_head_of(&head, totals, TruncationOptions::default());
    let mut diagnostics: Vec<ToolDiagnostic> = Vec::new();
    let mut output_text = truncation.content.clone();
    let mut details: Option<ReadToolDetails> = None;
    if truncation.first_line_exceeds_limit {
        let integral = start_line.fract() == 0.0;
        let line_bytes = if integral {
            head.split('\n').next().unwrap_or("").as_bytes().to_vec()
        } else {
            Vec::new()
        };
        let line_size = if integral { scan.first_line_bytes } else { 0 };
        let end = character_end(&line_bytes, DEFAULT_MAX_BYTES);
        output_text = String::from_utf8_lossy(&line_bytes[..end]).into_owned();
        diagnostics.push(ToolDiagnostic {
            severity: ToolDiagnosticSeverity::Warn,
            code: Some("truncated".to_string()),
            message: format!(
                "Line {start_line_display} is {}, exceeds the {} limit; showing its first {}. Use bash: sed -n '{start_line_display}p' {path} | tail -c +{}",
                format_size(line_size),
                format_size(DEFAULT_MAX_BYTES),
                format_size(end),
                end + 1
            ),
        });
        details = Some(ReadToolDetails {
            truncation: Some(truncation_details(&truncation, end, 1)),
        });
    } else if truncation.truncated {
        let end_line_display = start_line_display + truncation.output_lines as i64 - 1;
        let next_offset = end_line_display + 1;
        let limit_text = match truncation.truncated_by {
            Some(crate::truncate::TruncatedBy::Bytes) => {
                format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES))
            }
            _ => String::new(),
        };
        diagnostics.push(ToolDiagnostic {
            severity: ToolDiagnosticSeverity::Info,
            code: Some("truncated".to_string()),
            message: format!(
                "Showing lines {start_line_display}-{end_line_display} of {total_file_lines}{limit_text}. Use offset={next_offset} to continue."
            ),
        });
        details = Some(ReadToolDetails {
            truncation: Some(truncation_details(
                &truncation,
                truncation.output_bytes,
                truncation.output_lines,
            )),
        });
    } else if let Some(user_limited_lines) = user_limited_lines
        && start_line as usize + user_limited_lines < total_file_lines
    {
        let remaining = total_file_lines - (start_line as usize + user_limited_lines);
        let next_offset = start_line as usize + user_limited_lines + 1;
        diagnostics.push(ToolDiagnostic {
            severity: ToolDiagnosticSeverity::Info,
            code: None,
            message: format!(
                "{remaining} more lines in file. Use offset={next_offset} to continue."
            ),
        });
    }

    Ok(ToolExecutionResult {
        content: if output_text.is_empty() {
            Some(Vec::new())
        } else {
            Some(vec![pi_ai::TextOrImageContent::Text(pi_ai::TextContent {
                kind: pi_ai::TextKind,
                text: output_text,
                text_signature: None,
            })])
        },
        details: details
            .map(|details| serde_json::to_value(details).expect("read details serialise")),
        diagnostics,
        ..Default::default()
    })
}

/// 对应 `createReadTool()`。
pub fn create_read_tool() -> Arc<dyn ToolRegistration> {
    define_tool(ReadTool)
}
