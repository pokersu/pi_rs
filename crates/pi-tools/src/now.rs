//! `now` 工具：返回当前 UTC Unix 时间戳。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value as JsonValue, json};

use pi_ai::{TextContent, TextKind, TextOrImageContent};
use pi_durable::chord::context::Context;
use pi_durable::harness::define::define_tool;
use pi_durable::harness::types::{ToolExecutionApi, ToolExecutionResult, ToolRegistration};
use pi_durable::session::SessionError;

struct NowTool;

#[async_trait::async_trait]
impl ToolRegistration for NowTool {
    fn name(&self) -> &str {
        "now"
    }

    fn description(&self) -> &str {
        "Return the current time as a Unix timestamp (seconds since 1970-01-01 UTC)."
    }

    fn parameters(&self) -> &JsonValue {
        static SCHEMA: std::sync::LazyLock<JsonValue> = std::sync::LazyLock::new(|| {
            json!({
                "type": "object",
                "properties": {}
            })
        });
        &SCHEMA
    }

    async fn execute(
        &self,
        _args: JsonValue,
        _api: Arc<dyn ToolExecutionApi>,
        _context: Arc<dyn Context>,
    ) -> Result<ToolExecutionResult, SessionError> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Ok(ToolExecutionResult {
            content: Some(vec![TextOrImageContent::Text(TextContent {
                kind: TextKind,
                text: format!("{secs} (Unix timestamp, seconds since 1970-01-01T00:00:00Z)"),
                text_signature: None,
            })]),
            ..Default::default()
        })
    }
}

/// 构造 `now` 工具：返回当前 UTC Unix 时间戳（秒）。
pub fn create_now_tool() -> Arc<dyn ToolRegistration> {
    define_tool(NowTool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_tool_returns_name() {
        let tool = create_now_tool();
        assert_eq!(tool.name(), "now");
    }
}
