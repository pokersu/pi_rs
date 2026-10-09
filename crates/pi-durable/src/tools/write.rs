//! 对应 `tools/write.ts`：写入文件内容。

use std::sync::{Arc, LazyLock};

use serde_json::{Value as JsonValue, json};

use crate::chord::context::Context;
use crate::harness::define::define_tool;
use crate::harness::types::{ToolExecutionApi, ToolExecutionResult, ToolRegistration};
use crate::session::SessionError;
use crate::tools::env::require_env;
use crate::tools::file_mutation_queue::with_file_mutation_queue;
use crate::tools::path_utils::resolve_tool_path;

static SCHEMA: LazyLock<JsonValue> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
            "content": { "type": "string", "description": "Content to write to the file" }
        },
        "required": ["path", "content"]
    })
});

struct WriteTool;

#[async_trait::async_trait]
impl ToolRegistration for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
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
        let path = args["path"].as_str().unwrap_or_default();
        let content = args["content"].as_str().unwrap_or_default();
        let env = require_env(api.as_ref())?;
        let absolute_path = resolve_tool_path(env.as_ref(), path, context.as_ref()).await?;
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
                env.write_file(&absolute_path, content.as_bytes(), context.as_ref())
                    .await?;
                if context
                    .abort_signal()
                    .is_some_and(|signal| signal.aborted())
                {
                    return Err(SessionError::Message("Operation aborted".to_string()));
                }
                Ok(ToolExecutionResult {
                    content: Some(vec![pi_ai::TextOrImageContent::Text(pi_ai::TextContent {
                        kind: pi_ai::TextKind,
                        text: format!("Successfully wrote to {path}"),
                        text_signature: None,
                    })]),
                    ..Default::default()
                })
            },
            context.as_ref(),
        )
        .await
    }
}

/// 对应 `createWriteTool()`。
pub fn create_write_tool() -> Arc<dyn ToolRegistration> {
    define_tool(WriteTool)
}
