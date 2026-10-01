//! Rust 翻译自 packages/agent/src/harness/tools/edit.ts

use std::sync::Arc;

use pi_ai::{AbortSignal, TextContent, TextKind, TextOrImageContent};

use crate::harness::context::{BACKGROUND_CONTEXT, with_abort_signal};
use crate::harness::result::get_or_throw;
use crate::harness::tools::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, strip_bom,
};
use crate::harness::tools::file_mutation_queue::with_file_mutation_queue;
use crate::harness::tools::path_utils::resolve_tool_path;
use crate::harness::types::ExecutionEnv;
use crate::types::{AgentTool, AgentToolResult};

/// 对应 `isSingleEditInput`：判断值是否为单个 `{ oldText, newText }` 对象。
fn is_single_edit_input(value: &serde_json::Value) -> bool {
    let Some(edit) = value.as_object() else {
        return false;
    };
    edit.get("oldText").is_some_and(|v| v.is_string())
        && edit.get("newText").is_some_and(|v| v.is_string())
}

/// 对应 `prepareEditArguments`：把模型给出的原始参数规范化为 `{ path, edits: [...] }`。
///
/// 处理三种常见形态：`edits` 为 JSON 字符串、`edits` 为单个 edit 对象、legacy 顶层
/// `oldText`/`newText`。解析失败时保持原样，交由后续校验报错（与上游一致）。
fn prepare_edit_arguments(input: serde_json::Value) -> serde_json::Value {
    let Some(map) = input.as_object() else {
        return input;
    };
    let mut args = map.clone();

    if let Some(edits) = args.get("edits") {
        if let Some(text) = edits.as_str() {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
                if parsed.is_array() {
                    args.insert("edits".to_string(), parsed);
                } else if is_single_edit_input(&parsed) {
                    args.insert("edits".to_string(), serde_json::Value::Array(vec![parsed]));
                }
            }
        } else if is_single_edit_input(edits) {
            let single = edits.clone();
            args.insert("edits".to_string(), serde_json::Value::Array(vec![single]));
        }
    }

    let has_legacy = args.get("oldText").is_some_and(|v| v.is_string())
        && args.get("newText").is_some_and(|v| v.is_string());
    if !has_legacy {
        return serde_json::Value::Object(args);
    }

    let mut edits = match args.get("edits") {
        Some(serde_json::Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    let old_text = args.remove("oldText").unwrap_or(serde_json::Value::Null);
    let new_text = args.remove("newText").unwrap_or(serde_json::Value::Null);
    edits.push(serde_json::json!({ "oldText": old_text, "newText": new_text }));
    args.insert("edits".to_string(), serde_json::Value::Array(edits));
    serde_json::Value::Object(args)
}

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
				let context = match signal {
					Some(s) => with_abort_signal(&s, &BACKGROUND_CONTEXT),
					None => (*BACKGROUND_CONTEXT).clone(),
				};
				let (path, edits) = validate_edit_input(&params);
				let absolute = resolve_tool_path(&env, &path, &context).await;
				let absolute_for_closure = absolute.clone();
				let env_for_closure = env.clone();
				let context_for_closure = context.clone();
				with_file_mutation_queue(&env, &absolute, &context, move || {
					let context = context_for_closure;
					let env = env_for_closure;
					let path = path.clone();
					let edits = edits.clone();
					let absolute = absolute_for_closure;
					Box::pin(async move {
						if context.abort_signal().map(|s| s.aborted()).unwrap_or(false) {
							panic!("Operation aborted");
						}
						let info = get_or_throw(env.file_info(&absolute, &context).await);
						if info.kind != crate::harness::types::FileKind::File
							&& info.kind != crate::harness::types::FileKind::Symlink
						{
							panic!("Could not edit file: {path}. Path is not a file.");
						}

						let read_result = get_or_throw(env.read_text_file(&absolute, &context).await);
						if context.abort_signal().map(|s| s.aborted()).unwrap_or(false) {
							panic!("Operation aborted");
						}

						let (bom, content) = strip_bom(&read_result);
						let original_ending = detect_line_ending(&content);
						let normalized_content = normalize_to_lf(&content);
						let applied = apply_edits_to_normalized_content(&normalized_content, &edits, &path);
						if context.abort_signal().map(|s| s.aborted()).unwrap_or(false) {
							panic!("Operation aborted");
						}

						let final_content = format!("{}{}", bom, restore_line_endings(&applied.new_content, original_ending));
						get_or_throw(env.write_file(&absolute, final_content.as_bytes(), &context).await);
						if context.abort_signal().map(|s| s.aborted()).unwrap_or(false) {
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
							is_error: false,
							structured_content: None,
						}
					})
				})
				.await
			})
		}),
		prepare_arguments: Some(Arc::new(prepare_edit_arguments)),
		execution_mode: None,
		replay: None,
	}
}

// `AbortSignal` 在签名中作为参数类型出现，此引用避免未使用告警。
#[allow(unused)]
fn _unused_signal(_: Option<AbortSignal>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prepare_edit_arguments_parses_json_string_array() {
        let input = json!({ "path": "a.txt", "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]" });
        let out = prepare_edit_arguments(input);
        assert_eq!(out["edits"], json!([{ "oldText": "a", "newText": "b" }]));
    }

    #[test]
    fn prepare_edit_arguments_parses_json_string_single_object() {
        let input = json!({ "path": "a.txt", "edits": "{\"oldText\":\"a\",\"newText\":\"b\"}" });
        let out = prepare_edit_arguments(input);
        assert_eq!(out["edits"], json!([{ "oldText": "a", "newText": "b" }]));
    }

    #[test]
    fn prepare_edit_arguments_wraps_single_object() {
        let input = json!({ "path": "a.txt", "edits": { "oldText": "a", "newText": "b" } });
        let out = prepare_edit_arguments(input);
        assert_eq!(out["edits"], json!([{ "oldText": "a", "newText": "b" }]));
    }

    #[test]
    fn prepare_edit_arguments_folds_legacy_top_level_fields() {
        let input = json!({ "path": "a.txt", "oldText": "a", "newText": "b" });
        let out = prepare_edit_arguments(input);
        assert_eq!(out["edits"], json!([{ "oldText": "a", "newText": "b" }]));
        assert!(out.get("oldText").is_none());
        assert!(out.get("newText").is_none());
    }

    #[test]
    fn prepare_edit_arguments_appends_legacy_after_existing_edits() {
        let input = json!({
            "path": "a.txt",
            "edits": [{ "oldText": "x", "newText": "y" }],
            "oldText": "a",
            "newText": "b"
        });
        let out = prepare_edit_arguments(input);
        assert_eq!(
            out["edits"],
            json!([
                { "oldText": "x", "newText": "y" },
                { "oldText": "a", "newText": "b" }
            ])
        );
    }

    #[test]
    fn prepare_edit_arguments_keeps_unparsable_string() {
        let input = json!({ "path": "a.txt", "edits": "not json" });
        let out = prepare_edit_arguments(input);
        assert_eq!(out["edits"], json!("not json"));
    }

    #[test]
    fn prepare_edit_arguments_passes_through_non_object() {
        assert_eq!(prepare_edit_arguments(json!("x")), json!("x"));
        assert_eq!(prepare_edit_arguments(json!(null)), json!(null));
    }

    #[test]
    fn prepare_edit_arguments_is_idempotent_for_normal_input() {
        let input = json!({
            "path": "a.txt",
            "edits": [{ "oldText": "a", "newText": "b" }]
        });
        let once = prepare_edit_arguments(input.clone());
        assert_eq!(once, input);
        assert_eq!(prepare_edit_arguments(once.clone()), once);
    }
}
