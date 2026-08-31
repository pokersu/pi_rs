//! Rust 翻译自 packages/agent/src/harness/tools/write.ts

use std::sync::Arc;

use pi_ai::{AbortSignal, TextContent, TextKind, TextOrImageContent};

use crate::harness::result::get_or_throw;
use crate::harness::types::ExecutionEnv;
use crate::types::{AgentTool, AgentToolResult};

/// 对应 `createWriteTool`
pub fn create_write_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool {
		label: "write".to_string(),
		tool: pi_ai::Tool {
			name: "write".to_string(),
			description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_string(),
			parameters: serde_json::json!({
				"type": "object",
				"properties": {
					"path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
					"content": { "type": "string", "description": "Content to write to the file" }
				},
				"required": ["path", "content"]
			}),
			constrained_sampling: None,
		},
		execute: Arc::new(move |_id, params, signal, _on_update| {
			let env = env.clone();
			Box::pin(async move {
				let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
				let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
				let absolute = get_or_throw(env.absolute_path(&path, signal.as_ref()).await);
				get_or_throw(env.write_file(&absolute, content.as_bytes(), signal.as_ref()).await);
				if signal.as_ref().map(|s| s.aborted()).unwrap_or(false) {
					panic!("Operation aborted");
				}
				AgentToolResult {
					content: vec![TextOrImageContent::Text(TextContent {
						kind: TextKind,
						text: format!("Successfully wrote {} bytes to {path}", content.len()),
						text_signature: None,
					})],
					details: serde_json::Value::Null,
					usage: None,
					added_tool_names: None,
					terminate: false,
				}
			})
		}),
		execution_mode: None,
	}
}

// `AbortSignal` 在签名中作为参数类型出现，此引用避免未使用告警。
#[allow(unused)]
fn _unused_signal(_: Option<AbortSignal>) {}
