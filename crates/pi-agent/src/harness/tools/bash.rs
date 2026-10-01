//! Rust 翻译自 packages/agent/src/harness/tools/bash.ts（含流式 onUpdate + 100ms 节流）

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_ai::{TextContent, TextKind, TextOrImageContent};

use crate::harness::context::{BACKGROUND_CONTEXT, Context, with_abort_signal};
use crate::harness::types::{
    ExecutionEnv, ExecutionErrorCode, ShellExecOptions, ShellOutputCaptureOptions,
    ShellOutputLimits, ShellOutputRetention, ShellOutputUpdate, ShellOutputUpdateCallback,
};
use crate::harness::utils::truncate::format_size;
use crate::types::{AgentTool, AgentToolResult};

const DEFAULT_MAX_LINES: usize = 2000;
const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// 对应 `BASH_UPDATE_THROTTLE_MS`
const BASH_UPDATE_THROTTLE_MS: u64 = 100;

/// 对应 `BashExecution`。
pub struct BashExecution {
    pub command: String,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub inherit_env: bool,
}

/// 对应 `BashPrepare`。
pub type BashPrepare = Arc<
    dyn Fn(&mut BashExecution, &Context) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

/// 对应 `BashToolOptions`。
#[derive(Default, Clone)]
pub struct BashToolOptions {
    pub command_prefix: Option<String>,
    pub prepare: Option<BashPrepare>,
}

/// 对应 `createBashTool`
pub fn create_bash_tool(env: Arc<dyn ExecutionEnv>, options: BashToolOptions) -> AgentTool {
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
            let options = options.clone();
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

                let mut execution = BashExecution {
                    command: options
                        .command_prefix
                        .as_ref()
                        .map(|prefix| format!("{prefix}\n{command}"))
                        .unwrap_or(command),
                    cwd: env.cwd().to_string(),
                    env: BTreeMap::new(),
                    inherit_env: true,
                };
                if let Some(prepare) = &options.prepare {
                    prepare(&mut execution, &context).await;
                }

                let exec_result = env
                    .exec(
                        &execution.command,
                        ShellExecOptions {
                            cwd: Some(execution.cwd.clone()),
                            env: if execution.env.is_empty() {
                                None
                            } else {
                                Some(execution.env.clone())
                            },
                            inherit_env: execution.inherit_env,
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
                        },
                        &context,
                    )
                    .await;

                let mut output = accumulated.lock().unwrap().clone();

                // 对齐上游：截断时补 `BashToolDetails` 并在输出尾部追加提示。
                let mut details = serde_json::Value::Null;
                if let Ok(result) = &exec_result
                    && result.truncation.truncated
                {
                    let truncation = &result.truncation;
                    let mut details_obj = serde_json::Map::new();
                    details_obj.insert(
                        "truncation".to_string(),
                        serde_json::to_value(truncation).unwrap_or(serde_json::Value::Null),
                    );
                    if let Some(spill) = &result.spill_path {
                        details_obj.insert(
                            "fullOutputPath".to_string(),
                            serde_json::Value::String(spill.clone()),
                        );
                    }
                    details = serde_json::Value::Object(details_obj);

                    let start_line = truncation
                        .total_lines
                        .saturating_sub(truncation.output_lines)
                        + 1;
                    let end_line = truncation.total_lines;
                    let spill = result.spill_path.clone().unwrap_or_default();
                    if truncation.last_line_partial {
                        let last_line_size =
                            format_size(result.last_line_bytes.unwrap_or(truncation.output_bytes));
                        output.push_str(&format!(
                            "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {spill}]",
                            format_size(truncation.output_bytes)
                        ));
                    } else if truncation.truncated_by.as_deref() == Some("lines") {
                        output.push_str(&format!(
                            "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {spill}]",
                            truncation.total_lines
                        ));
                    } else {
                        output.push_str(&format!(
                            "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {spill}]",
                            truncation.total_lines,
                            format_size(DEFAULT_MAX_BYTES)
                        ));
                    }
                }

                match exec_result {
                    Err(error) => {
                        let status = if error.code == ExecutionErrorCode::Timeout {
                            format!(
                                "Command timed out after {} seconds",
                                timeout.unwrap_or_default()
                            )
                        } else if error.code == ExecutionErrorCode::Aborted {
                            "Command aborted".to_string()
                        } else {
                            error.message.clone()
                        };
                        panic!(
                            "{}",
                            if output.is_empty() {
                                status
                            } else {
                                format!("{output}\n\n{status}")
                            }
                        );
                    }
                    Ok(result) => {
                        if result.exit_code != 0 {
                            panic!(
                                "{}",
                                if output.is_empty() {
                                    format!("Command exited with code {}", result.exit_code)
                                } else {
                                    format!(
                                        "{output}\n\nCommand exited with code {}",
                                        result.exit_code
                                    )
                                }
                            );
                        }
                    }
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
                    details,
                    usage: None,
                    added_tool_names: None,
                    terminate: false,
                    is_error: false,
                    structured_content: None,
                }
            })
        }),
        execution_mode: None,
        prepare_arguments: None,
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
        is_error: false,
        structured_content: None,
    }
}
