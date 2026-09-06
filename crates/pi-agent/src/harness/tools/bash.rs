//! Rust 翻译自 packages/agent/src/harness/tools/bash.ts（含流式 onUpdate + 100ms 节流）

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_ai::{TextContent, TextKind, TextOrImageContent};

use crate::harness::context::{BACKGROUND_CONTEXT, Context, with_abort_signal};
use crate::harness::result::get_or_throw;
use crate::harness::types::{
    ExecutionEnv, ShellExecOptions, ShellOutputCaptureOptions, ShellOutputLimits,
    ShellOutputRetention, ShellOutputUpdate, ShellOutputUpdateCallback,
};
use crate::types::{AgentTool, AgentToolResult};

const DEFAULT_MAX_LINES: usize = 2000;
const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// 对应 `BASH_UPDATE_THROTTLE_MS`
const BASH_UPDATE_THROTTLE_MS: u64 = 100;

/// 对应 `createBashTool`
pub fn create_bash_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool {
        label: "bash".to_string(),
        tool: pi_ai::Tool {
            name: "bash".to_string(),
            description: format!(
                "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Optionally provide a timeout in seconds.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command to execute" },
                    "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
                },
                "required": ["command"]
            }),
            constrained_sampling: None,
        },
        execute: Arc::new(move |_id, params, signal, on_update| {
            let env = env.clone();
            Box::pin(async move {
                let command = params
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let timeout = params.get("timeout").and_then(|v| v.as_f64());
                if let Some(t) = timeout
                    && (!t.is_finite() || t <= 0.0)
                {
                    panic!("Invalid timeout: must be a finite number of seconds");
                }

                let context = match signal {
                    Some(s) => with_abort_signal(&s, &BACKGROUND_CONTEXT),
                    None => (*BACKGROUND_CONTEXT).clone(),
                };

                let accumulated = Arc::new(Mutex::new(String::new()));
                let acc_for_cb = Arc::clone(&accumulated);
                let last_update = Arc::new(Mutex::new(Instant::now()));
                let on_update_shared = Arc::new(Mutex::new(on_update));

                let on_update_cb: ShellOutputUpdateCallback = {
                    let last_update = Arc::clone(&last_update);
                    let on_update_shared = Arc::clone(&on_update_shared);
                    Box::new(move |update: &ShellOutputUpdate, _ctx: &Context| {
                        let text = match update {
                            ShellOutputUpdate::Replace { output } => output.text.clone(),
                            ShellOutputUpdate::Append { text, .. }
                            | ShellOutputUpdate::Slide { text, .. } => text.clone(),
                            ShellOutputUpdate::Metadata { .. } => String::new(),
                        };
                        if text.is_empty() {
                            return;
                        }
                        let current = {
                            let mut acc = acc_for_cb.lock().unwrap();
                            if matches!(update, ShellOutputUpdate::Replace { .. }) {
                                *acc = text;
                            } else {
                                acc.push_str(&text);
                            }
                            acc.clone()
                        };
                        let callback = on_update_shared.lock().unwrap();
                        if let Some(callback) = callback.as_ref() {
                            let mut last = last_update.lock().unwrap();
                            if last.elapsed() >= Duration::from_millis(BASH_UPDATE_THROTTLE_MS) {
                                *last = Instant::now();
                                drop(last);
                                callback(text_update(&current));
                            }
                        }
                    })
                };

                let result = get_or_throw(
                    env.exec(
                        &command,
                        ShellExecOptions {
                            cwd: Some(env.cwd().to_string()),
                            timeout,
                            capture: Some(ShellOutputCaptureOptions {
                                limits: ShellOutputLimits {
                                    max_bytes: DEFAULT_MAX_BYTES,
                                    max_lines: DEFAULT_MAX_LINES,
                                    retain: Some(ShellOutputRetention::Tail),
                                },
                                spill: Some(true),
                            }),
                            on_update: Some(on_update_cb),
                            ..Default::default()
                        },
                        &context,
                    )
                    .await,
                );

                let output = accumulated.lock().unwrap().clone();
                if result.exit_code != 0 {
                    panic!("Command exited with code {}\n{}", result.exit_code, output);
                }

                let output = if output.is_empty() {
                    "(no output)".to_string()
                } else {
                    output
                };

                AgentToolResult {
                    content: vec![TextOrImageContent::Text(TextContent {
                        kind: TextKind,
                        text: output,
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
        replay: None,
    }
}

fn text_update(text: &str) -> AgentToolResult {
    AgentToolResult {
        content: vec![TextOrImageContent::Text(TextContent {
            kind: TextKind,
            text: text.to_string(),
            text_signature: None,
        })],
        details: serde_json::Value::Null,
        usage: None,
        added_tool_names: None,
        terminate: false,
    }
}
