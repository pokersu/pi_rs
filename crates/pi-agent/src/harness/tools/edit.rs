//! Rust 翻译自 packages/agent/src/harness/tools/edit.ts

use std::sync::Arc;

use pi_ai::{AbortSignal, TextContent, TextKind, TextOrImageContent};

use crate::harness::result::get_or_throw;
use crate::harness::tools::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, strip_bom,
};
use crate::harness::tools::file_mutation_queue::with_file_mutation_queue;
use crate::harness::tools::path_utils::resolve_tool_path;
use crate::harness::types::ExecutionEnv;
use crate::types::{AgentTool, AgentToolResult};

fn validate_edit_input(input: &serde_json::Value) -> (String, Vec<Edit>) {
    let path = input
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let edits = input
        .get("edits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if edits.is_empty() {
        panic!("Edit tool input is invalid. edits must contain at least one replacement.");
    }
    let edits: Vec<Edit> = edits
        .iter()
        .map(|e| Edit {
            old_text: e
                .get("oldText")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            new_text: e
                .get("newText")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect();
    (path, edits)
}

/// 对应 `createEditTool`
pub fn create_edit_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool {
		label: "edit".to_string(),
		tool: pi_ai::Tool {
			name: "edit".to_string(),
			description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits.".to_string(),
			parameters: serde_json::json!({
				"type": "object",
				"properties": {
					"path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
					"edits": {
						"type": "array",
						"description": "One or more targeted replacements.",
						"items": {
							"type": "object",
							"properties": {
								"oldText": { "type": "string" },
								"newText": { "type": "string" }
							},
							"required": ["oldText", "newText"]
						}
					}
				},
				"required": ["path", "edits"]
			}),
			constrained_sampling: None,
		},
		execute: Arc::new(move |_id, params, signal, _on_update| {
			let env = env.clone();
			Box::pin(async move {
				let (path, edits) = validate_edit_input(&params);
				let absolute = resolve_tool_path(&env, &path, signal.as_ref()).await;
				let absolute_for_closure = absolute.clone();
				let env_for_closure = env.clone();
				with_file_mutation_queue(&env, &absolute, move || {
					let env = env_for_closure;
					let path = path.clone();
					let edits = edits.clone();
					let absolute = absolute_for_closure;
					Box::pin(async move {
						if signal.as_ref().map(|s| s.aborted()).unwrap_or(false) {
							panic!("Operation aborted");
						}
						let info = get_or_throw(env.file_info(&absolute, signal.as_ref()).await);
						if info.kind != crate::harness::types::FileKind::File
							&& info.kind != crate::harness::types::FileKind::Symlink
						{
							panic!("Could not edit file: {path}. Path is not a file.");
						}

						let read_result = get_or_throw(env.read_text_file(&absolute, signal.as_ref()).await);
						if signal.as_ref().map(|s| s.aborted()).unwrap_or(false) {
							panic!("Operation aborted");
						}

						let (bom, content) = strip_bom(&read_result);
						let original_ending = detect_line_ending(&content);
						let normalized_content = normalize_to_lf(&content);
						let applied = apply_edits_to_normalized_content(&normalized_content, &edits, &path);
						if signal.as_ref().map(|s| s.aborted()).unwrap_or(false) {
							panic!("Operation aborted");
						}

						let final_content = format!("{}{}", bom, restore_line_endings(&applied.new_content, original_ending));
						get_or_throw(env.write_file(&absolute, final_content.as_bytes(), signal.as_ref()).await);
						if signal.as_ref().map(|s| s.aborted()).unwrap_or(false) {
							panic!("Operation aborted");
						}

						let (diff, first_changed_line) =
							generate_diff_string(&applied.base_content, &applied.new_content, 4);
						AgentToolResult {
							content: vec![TextOrImageContent::Text(TextContent {
								kind: TextKind,
								text: format!("Successfully replaced {} block(s) in {path}.", edits.len()),
								text_signature: None,
							})],
							details: serde_json::json!({
								"diff": diff,
								"patch": generate_unified_patch(&path, &applied.base_content, &applied.new_content, 4),
								"firstChangedLine": first_changed_line,
							}),
							usage: None,
							added_tool_names: None,
							terminate: false,
						}
					})
				})
				.await
			})
		}),
		execution_mode: None,
	}
}

// `AbortSignal` 在签名中作为参数类型出现，此引用避免未使用告警。
#[allow(unused)]
fn _unused_signal(_: Option<AbortSignal>) {}
