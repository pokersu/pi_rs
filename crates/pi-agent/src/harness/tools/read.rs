//! Rust 翻译自 packages/agent/src/harness/tools/read.ts（简化：图片仅返回提示）

use std::sync::Arc;

use pi_ai::{AbortSignal, TextContent, TextKind, TextOrImageContent};

use crate::harness::types::ExecutionEnv;
use crate::types::{AgentTool, AgentToolResult};

const DEFAULT_MAX_LINES: usize = 2000;
const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// 对应 `createReadTool`
pub fn create_read_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool {
        label: "read".to_string(),
        tool: pi_ai::Tool {
            name: "read".to_string(),
            description: format!(
                "Read the contents of a file. Supports text files. Output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                    "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                    "limit": { "type": "number", "description": "Maximum number of lines to read" }
                },
                "required": ["path"]
            }),
            constrained_sampling: None,
        },
        execute: Arc::new(move |_id, params, signal, _on_update| {
            let env = env.clone();
            Box::pin(async move {
                let path = params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let offset = params.get("offset").and_then(|v| v.as_u64());
                let limit = params.get("limit").and_then(|v| v.as_u64());
                read_file(&env, &path, offset, limit, signal).await
            })
        }),
        execution_mode: None,
    }
}

fn text_content(text: String) -> TextOrImageContent {
    TextOrImageContent::Text(TextContent {
        kind: TextKind,
        text,
        text_signature: None,
    })
}

async fn read_file(
    env: &Arc<dyn ExecutionEnv>,
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    signal: Option<AbortSignal>,
) -> AgentToolResult {
    let absolute =
        crate::harness::result::get_or_throw(env.absolute_path(path, signal.as_ref()).await);

    // 按扩展名检测图片，简化处理：返回提示。
    let lower = path.to_lowercase();
    let is_image = lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".bmp");
    if is_image {
        return AgentToolResult {
            content: vec![text_content(format!(
                "Read image file [{path}]\n[Image omitted: image processing not enabled in this Rust port.]"
            ))],
            details: serde_json::Value::Null,
            usage: None,
            added_tool_names: None,
            terminate: false,
        };
    }

    let bytes = crate::harness::result::get_or_throw(
        env.read_binary_file(&absolute, signal.as_ref()).await,
    );
    let text = String::from_utf8_lossy(&bytes).to_string();
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total_lines = all_lines.len();

    let start_line = offset.map(|o| (o.saturating_sub(1)) as usize).unwrap_or(0);
    let start_display = start_line + 1;
    if start_line >= total_lines {
        return AgentToolResult {
            content: vec![text_content(format!(
                "Offset {} is beyond end of file ({total_lines} lines total)",
                offset.unwrap_or(0)
            ))],
            details: serde_json::Value::Null,
            usage: None,
            added_tool_names: None,
            terminate: false,
        };
    }

    let end_line = match limit {
        Some(l) => (start_line + l as usize).min(total_lines),
        None => total_lines,
    };
    let selected: Vec<&str> = all_lines[start_line..end_line].to_vec();
    let selected_text = selected.join("\n");

    // 截断到字节限制。
    let mut output = selected_text.clone();
    let mut truncated_by_lines = false;
    if selected.len() > DEFAULT_MAX_LINES {
        output = selected[..DEFAULT_MAX_LINES].join("\n");
        truncated_by_lines = true;
    }
    if output.len() > DEFAULT_MAX_BYTES {
        output = output[..DEFAULT_MAX_BYTES].to_string();
        truncated_by_lines = false;
    }

    if truncated_by_lines || output.len() < selected_text.len() {
        let shown_lines = output.split('\n').count();
        let end_display = start_display + shown_lines - 1;
        let next_offset = end_display + 1;
        output.push_str(&format!(
			"\n\n[Showing lines {start_display}-{end_display} of {total_lines}. Use offset={next_offset} to continue.]"
		));
    }

    AgentToolResult {
        content: vec![text_content(output)],
        details: serde_json::Value::Null,
        usage: None,
        added_tool_names: None,
        terminate: false,
    }
}
