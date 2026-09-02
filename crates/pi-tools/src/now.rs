//! `now` 工具：返回当前 UTC Unix 时间戳。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use pi_agent::{AgentTool, AgentToolResult};
use pi_ai::{TextContent, TextKind, TextOrImageContent};

/// 构造 `now` 工具：返回当前 UTC Unix 时间戳（秒）。
pub fn create_now_tool() -> AgentTool {
    AgentTool {
        label: "now".to_string(),
        tool: pi_ai::Tool {
            name: "now".to_string(),
            description:
                "Return the current time as a Unix timestamp (seconds since 1970-01-01 UTC)."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            constrained_sampling: None,
        },
        execute: Arc::new(|_id, _params, _signal, _on_update| {
            Box::pin(async move {
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                AgentToolResult {
                    content: vec![TextOrImageContent::Text(TextContent {
                        kind: TextKind,
                        text: format!(
                            "{secs} (Unix timestamp, seconds since 1970-01-01T00:00:00Z)"
                        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_tool_returns_content() {
        let tool = create_now_tool();
        assert_eq!(tool.name(), "now");

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on((tool.execute)(
            "id".to_string(),
            serde_json::json!({}),
            None,
            None,
        ));
        assert_eq!(result.content.len(), 1);
        assert!(matches!(
            &result.content[0],
            TextOrImageContent::Text(t) if t.text.contains("Unix timestamp")
        ));
    }
}
