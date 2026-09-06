//! Rust 翻译自 packages/agent/src/harness/tools/read.ts

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pi_ai::{ImageContent, ImageKind, TextContent, TextKind, TextOrImageContent};

use crate::harness::context::{BACKGROUND_CONTEXT, Context, with_abort_signal};
use crate::harness::tools::image::{detect_supported_image_mime_type, encode_base64};
use crate::harness::tools::path_utils::resolve_read_tool_path;
use crate::harness::types::ExecutionEnv;
use crate::harness::utils::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationOptions, format_size, truncate_head,
};
use crate::types::{AgentTool, AgentToolResult};

/// 对应 `ReadImageProcessorResult`。
pub enum ReadImageProcessorResult {
    Ok {
        data: String,
        mime_type: String,
        hints: Vec<String>,
    },
    Err {
        message: String,
    },
}

/// 对应 `ReadImageProcessor`。
pub type ReadImageProcessor = Arc<
    dyn Fn(
            Vec<u8>,
            String,
            bool,
            Context,
        ) -> Pin<Box<dyn Future<Output = ReadImageProcessorResult> + Send>>
        + Send
        + Sync,
>;

/// 对应 `ReadToolOptions`。
#[derive(Default, Clone)]
pub struct ReadToolOptions {
    /// 是否让注入的图片处理器自动 resize。默认 true。
    pub auto_resize_images: Option<bool>,
    /// 可选的图片转换/resize 实现。
    pub image_processor: Option<ReadImageProcessor>,
}

/// 对应 `createReadTool`
pub fn create_read_tool(env: Arc<dyn ExecutionEnv>, options: ReadToolOptions) -> AgentTool {
    AgentTool {
        label: "read".to_string(),
        tool: pi_ai::Tool {
            name: "read".to_string(),
            description: format!(
                "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files.",
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
            let options = options.clone();
            Box::pin(async move {
                let path = params
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let offset = params.get("offset").and_then(|v| v.as_u64());
                let limit = params.get("limit").and_then(|v| v.as_u64());
                let context = match signal {
                    Some(s) => with_abort_signal(&s, &BACKGROUND_CONTEXT),
                    None => (*BACKGROUND_CONTEXT).clone(),
                };
                read_file(&env, &path, offset, limit, &options, &context).await
            })
        }),
        execution_mode: None,
        replay: None,
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
    options: &ReadToolOptions,
    context: &Context,
) -> AgentToolResult {
    let absolute = resolve_read_tool_path(env, path, context).await;
    let bytes =
        crate::harness::result::get_or_throw(env.read_binary_file(&absolute, context).await);

    // 图片：基于字节内容检测 mime type（对应 `detectSupportedImageMimeType`）。
    if let Some(mime_type) = detect_supported_image_mime_type(&bytes) {
        if let Some(processor) = &options.image_processor {
            let processed = processor(
                bytes.clone(),
                mime_type.to_string(),
                options.auto_resize_images.unwrap_or(true),
                context.clone(),
            )
            .await;
            return match processed {
                ReadImageProcessorResult::Ok {
                    data,
                    mime_type: processed_mime,
                    hints,
                } => AgentToolResult {
                    content: vec![
                        text_content(if hints.is_empty() {
                            format!("Read image file [{processed_mime}]")
                        } else {
                            format!("Read image file [{processed_mime}]\n{}", hints.join("\n"))
                        }),
                        TextOrImageContent::Image(ImageContent {
                            kind: ImageKind,
                            data,
                            mime_type: processed_mime,
                        }),
                    ],
                    details: serde_json::Value::Null,
                    usage: None,
                    added_tool_names: None,
                    terminate: false,
                },
                ReadImageProcessorResult::Err { message } => AgentToolResult {
                    content: vec![text_content(format!(
                        "Read image file [{mime_type}]\n{message}"
                    ))],
                    details: serde_json::Value::Null,
                    usage: None,
                    added_tool_names: None,
                    terminate: false,
                },
            };
        }
        if mime_type == "image/bmp" {
            return AgentToolResult {
                content: vec![text_content(
                    "Read image file [image/bmp]\n[Image omitted: configure an imageProcessor to convert BMP images.]"
                        .to_string(),
                )],
                details: serde_json::Value::Null,
                usage: None,
                added_tool_names: None,
                terminate: false,
            };
        }
        return AgentToolResult {
            content: vec![
                text_content(format!("Read image file [{mime_type}]")),
                TextOrImageContent::Image(ImageContent {
                    kind: ImageKind,
                    data: encode_base64(&bytes),
                    mime_type: mime_type.to_string(),
                }),
            ],
            details: serde_json::Value::Null,
            usage: None,
            added_tool_names: None,
            terminate: false,
        };
    }

    let text = String::from_utf8_lossy(&bytes).to_string();
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total_lines = all_lines.len();

    let start_line = offset.map(|o| o.saturating_sub(1) as usize).unwrap_or(0);
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

    let (selected_content, user_limited_lines): (String, Option<usize>) = match limit {
        Some(l) => {
            let end = (start_line + l as usize).min(total_lines);
            (
                all_lines[start_line..end].join("\n"),
                Some(end - start_line),
            )
        }
        None => (all_lines[start_line..].join("\n"), None),
    };

    let truncation = truncate_head(&selected_content, TruncationOptions::default());

    let output = if truncation.first_line_exceeds_limit {
        let first_line_size = format_size(all_lines[start_line].len());
        format!(
            "[Line {start_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_display}p' {path} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(DEFAULT_MAX_BYTES)
        )
    } else if truncation.truncated {
        let end_display = start_display + truncation.output_lines.saturating_sub(1);
        let next_offset = end_display + 1;
        let mut out = truncation.content.clone();
        if truncation.truncated_by == Some("lines") {
            out.push_str(&format!(
                "\n\n[Showing lines {start_display}-{end_display} of {total_lines}. Use offset={next_offset} to continue.]"
            ));
        } else {
            out.push_str(&format!(
                "\n\n[Showing lines {start_display}-{end_display} of {total_lines} ({} limit). Use offset={next_offset} to continue.]",
                format_size(DEFAULT_MAX_BYTES)
            ));
        }
        out
    } else if let Some(user_limited) = user_limited_lines
        && start_line + user_limited < total_lines
    {
        let remaining = total_lines - (start_line + user_limited);
        let next_offset = start_line + user_limited + 1;
        format!(
            "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
            truncation.content
        )
    } else {
        truncation.content
    };

    AgentToolResult {
        content: vec![text_content(output)],
        details: serde_json::Value::Null,
        usage: None,
        added_tool_names: None,
        terminate: false,
    }
}
