//! Rust 翻译自 packages/agent/src/harness/tools/bash.ts（含流式 onUpdate + 100ms 节流）

use std::sync::Arc;
use std::time::{Duration, Instant};

use pi_ai::{TextContent, TextKind, TextOrImageContent};

use crate::harness::result::get_or_throw;
use crate::harness::types::{ExecutionEnv, ShellChunkCallback, ShellExecOptions};
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

                // 进度 channel：读线程 -> 节流上报 task。
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();

                let progress_handle = on_update.map(|callback| {
                    tokio::spawn(async move {
                        let mut rx = rx;
                        let mut accumulated = String::new();
                        let mut last = Instant::now();
                        callback(empty_update());
                        while let Some(chunk) = rx.recv().await {
                            accumulated.push_str(&chunk);
                            if last.elapsed() >= Duration::from_millis(BASH_UPDATE_THROTTLE_MS) {
                                last = Instant::now();
                                callback(text_update(&accumulated));
                            }
                        }
                    })
                });

                let on_stdout: ShellChunkCallback = {
                    let tx = tx.clone();
                    Box::new(move |chunk: &str| {
                        let _ = tx.send(chunk.to_string());
                    })
                };
                let on_stderr: ShellChunkCallback = {
                    let tx = tx.clone();
                    Box::new(move |chunk: &str| {
                        let _ = tx.send(chunk.to_string());
                    })
                };

                let result = get_or_throw(
                    env.exec(
                        &command,
                        ShellExecOptions {
                            cwd: Some(env.cwd().to_string()),
                            timeout,
                            abort_signal: signal,
                            on_stdout: Some(on_stdout),
                            on_stderr: Some(on_stderr),
                            ..Default::default()
                        },
                    )
                    .await,
                );

                // 关闭 channel，等节流 task 结束。
                drop(tx);
                if let Some(handle) = progress_handle {
                    let _ = handle.await;
                }

                if result.exit_code != 0 {
                    panic!(
                        "Command exited with code {}\nstdout: {}\nstderr: {}",
                        result.exit_code, result.stdout, result.stderr
                    );
                }

                let mut output = if result.stdout.is_empty() {
                    result.stderr.clone()
                } else if result.stderr.is_empty() {
                    result.stdout.clone()
                } else {
                    format!("{}\n{}", result.stdout, result.stderr)
                };
                if output.is_empty() {
                    output = "(no output)".to_string();
                }

                // 截断到行数/字节限制。
                let lines: Vec<&str> = output.lines().collect();
                if lines.len() > DEFAULT_MAX_LINES {
                    let start = lines.len() - DEFAULT_MAX_LINES;
                    output = format!(
                        "[Showing last {DEFAULT_MAX_LINES} lines of {}]\n{}",
                        lines.len(),
                        lines[start..].join("\n")
                    );
                }
                if output.len() > DEFAULT_MAX_BYTES {
                    output = format!("[Truncated to {}KB]\n", DEFAULT_MAX_BYTES / 1024)
                        + &output[..DEFAULT_MAX_BYTES];
                }

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
    }
}

fn empty_update() -> AgentToolResult {
    AgentToolResult {
        content: Vec::new(),
        details: serde_json::Value::Null,
        usage: None,
        added_tool_names: None,
        terminate: false,
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
