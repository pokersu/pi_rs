//! 对应 `tools/edit.ts`：以精确文本替换编辑单个文件。

use std::sync::{Arc, LazyLock};

use serde_json::{Value as JsonValue, json};

use crate::chord::context::Context;
use crate::env::{FileError, FileKind};
use crate::harness::define::define_tool;
use crate::harness::types::{ToolExecutionApi, ToolExecutionResult, ToolRegistration};
use crate::session::SessionError;
use crate::tools::edit_diff::{
    Edit, apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, strip_bom,
};
use crate::tools::env::require_env;
use crate::tools::file_mutation_queue::with_file_mutation_queue;
use crate::tools::path_utils::resolve_tool_path;

static SCHEMA: LazyLock<JsonValue> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
            "edits": {
                "type": "array",
                "items": { "$ref": "#/definitions/replaceEdit" },
                "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."
            }
        },
        "required": ["path", "edits"],
        "definitions": {
            "replaceEdit": {
                "type": "object",
                "properties": {
                    "oldText": { "type": "string" },
                    "newText": { "type": "string" }
                },
                "required": ["oldText", "newText"]
            }
        }
    })
});

/// 对应 `EditToolDetails`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditToolDetails {
    pub diff: String,
    pub patch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<usize>,
}

struct EditTool;

fn is_single_edit_input(value: &JsonValue) -> bool {
    match value {
        JsonValue::Object(object) => {
            object.get("oldText").is_some_and(JsonValue::is_string)
                && object.get("newText").is_some_and(JsonValue::is_string)
        }
        _ => false,
    }
}

/// 对应 `prepareEditArguments`：修复模型常发的参数形状。
fn prepare_edit_arguments(input: JsonValue) -> JsonValue {
    let JsonValue::Object(mut args) = input else {
        return input;
    };
    if let Some(edits) = args.get("edits").cloned() {
        match edits {
            JsonValue::String(serialized) => {
                if let Ok(parsed) = serde_json::from_str::<JsonValue>(&serialized)
                    && (parsed.is_array() || is_single_edit_input(&parsed))
                {
                    let normalized = if parsed.is_array() {
                        parsed
                    } else {
                        json!([parsed])
                    };
                    args.insert("edits".to_string(), normalized);
                }
            }
            single if is_single_edit_input(&single) => {
                args.insert("edits".to_string(), json!([single]));
            }
            _ => {}
        }
    }

    let legacy_old = args
        .get("oldText")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let legacy_new = args
        .get("newText")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    if let (Some(old_text), Some(new_text)) = (legacy_old, legacy_new) {
        let mut edits: Vec<JsonValue> = args
            .get("edits")
            .and_then(JsonValue::as_array)
            .cloned()
            .unwrap_or_default();
        edits.push(json!({ "oldText": old_text, "newText": new_text }));
        args.remove("oldText");
        args.remove("newText");
        args.insert("edits".to_string(), JsonValue::Array(edits));
    }
    JsonValue::Object(args)
}

fn validate_edit_input(input: &JsonValue) -> Result<(String, Vec<Edit>), SessionError> {
    let edits = input["edits"].as_array().ok_or_else(|| {
        SessionError::Message(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        )
    })?;
    if edits.is_empty() {
        return Err(SessionError::Message(
            "Edit tool input is invalid. edits must contain at least one replacement.".to_string(),
        ));
    }
    let path = input["path"].as_str().unwrap_or_default().to_string();
    let edits = edits
        .iter()
        .map(|edit| Edit {
            old_text: edit["oldText"].as_str().unwrap_or_default().to_string(),
            new_text: edit["newText"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    Ok((path, edits))
}

fn edit_access_error(path: &str, error: &FileError) -> SessionError {
    let code = serde_json::to_value(error.code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    SessionError::Message(format!("Could not edit file: {path}. Error code: {code}."))
}

#[async_trait::async_trait]
impl ToolRegistration for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
    }

    fn parameters(&self) -> &JsonValue {
        &SCHEMA
    }

    fn prepare_arguments(&self, args: JsonValue) -> Result<JsonValue, SessionError> {
        Ok(prepare_edit_arguments(args))
    }

    async fn execute(
        &self,
        args: JsonValue,
        api: Arc<dyn ToolExecutionApi>,
        context: Arc<dyn Context>,
    ) -> Result<ToolExecutionResult, SessionError> {
        let (path, edits) = validate_edit_input(&args)?;
        let env = require_env(api.as_ref())?;
        let absolute_path = resolve_tool_path(env.as_ref(), &path, context.as_ref()).await?;
        with_file_mutation_queue(
            env.as_ref(),
            &absolute_path,
            || async {
                if context
                    .abort_signal()
                    .is_some_and(|signal| signal.aborted())
                {
                    return Err(SessionError::Message("Operation aborted".to_string()));
                }
                let info = match env.file_info(&absolute_path, context.as_ref()).await {
                    Ok(info) => info,
                    Err(error) => return Err(edit_access_error(&path, &error)),
                };
                if info.kind != FileKind::File && info.kind != FileKind::Symlink {
                    return Err(SessionError::Message(format!(
                        "Could not edit file: {path}. Path is not a file."
                    )));
                }

                let read_result = match env.read_text_file(&absolute_path, context.as_ref()).await {
                    Ok(text) => text,
                    Err(error) => return Err(edit_access_error(&path, &error)),
                };
                if context
                    .abort_signal()
                    .is_some_and(|signal| signal.aborted())
                {
                    return Err(SessionError::Message("Operation aborted".to_string()));
                }

                let (bom, content) = strip_bom(&read_result);
                let original_ending = detect_line_ending(content);
                let normalized_content = normalize_to_lf(content);
                let applied = apply_edits_to_normalized_content(&normalized_content, &edits, &path)
                    .map_err(SessionError::Message)?;
                if context
                    .abort_signal()
                    .is_some_and(|signal| signal.aborted())
                {
                    return Err(SessionError::Message("Operation aborted".to_string()));
                }

                let final_content = format!(
                    "{bom}{}",
                    restore_line_endings(&applied.new_content, original_ending)
                );
                if let Err(error) = env
                    .write_file(&absolute_path, final_content.as_bytes(), context.as_ref())
                    .await
                {
                    return Err(edit_access_error(&path, &error));
                }
                if context
                    .abort_signal()
                    .is_some_and(|signal| signal.aborted())
                {
                    return Err(SessionError::Message("Operation aborted".to_string()));
                }

                let (diff, first_changed_line) =
                    generate_diff_string(&applied.base_content, &applied.new_content, 4);
                let details = EditToolDetails {
                    diff,
                    patch: generate_unified_patch(
                        &path,
                        &applied.base_content,
                        &applied.new_content,
                        4,
                    ),
                    first_changed_line,
                };
                Ok(ToolExecutionResult {
                    content: Some(vec![pi_ai::TextOrImageContent::Text(pi_ai::TextContent {
                        kind: pi_ai::TextKind,
                        text: format!("Successfully replaced {} block(s) in {path}.", edits.len()),
                        text_signature: None,
                    })]),
                    details: Some(serde_json::to_value(details).expect("edit details serialise")),
                    ..Default::default()
                })
            },
            context.as_ref(),
        )
        .await
    }
}

/// 对应 `createEditTool()`。
pub fn create_edit_tool() -> Arc<dyn ToolRegistration> {
    define_tool(EditTool)
}
