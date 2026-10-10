//! 对应 `harness/tool.ts`：内置工具任务（`pi.tool`）。
//!
//! `call` handler 一次性完成「解析 → 校验 → beforeTool → 记录意图 → 执行 → afterTool → 结算」，
//! 使解析与结算之间没有分离；`execute` 只在恢复后到达，按 replay 规则决定重跑或失败。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游 `api.commit<T>` / `api.createTask<I, S, R, H>` 是泛型方法；Rust 的 dyn trait 无法承载，
//!   结果分别擦除为 [`CommitOperation`] / [`TaskCreationOptions`]（见 `harness/types.rs`）。
//! - 上游 `copyJson(value, { omitUndefinedProperties: true })` 去掉 `undefined` 键；Rust 的 JSON 没有
//!   `undefined`，[`ToolDiagnostic`] / [`ToolControl`] 的 `Option` 字段用 `skip_serializing_if` 等价覆盖。
//! - 上游 `assignJson(slot, "details", …)` 就地写 chord 草稿；Rust 侧 `slot` 是 `pi.live` 的 [`Draft`]，
//!   改由 `live.set(path, value)` / `live.splice(...)` 逐字段写入。
//! - 工具执行耗时 `performance.now()` → Rust 用 `std::time::Instant` 取单调时钟。

use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pi_ai::utils::validation::validate_tool_arguments;
use pi_ai::{
    ContentBlock, Message, TextKind, TextOrImageContent, ToolCall, ToolCallKind, ToolResultMessage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::chord::context::{Context, await_with_context};
use crate::chord::delta::PathSegment;
use crate::entries::{
    DiagnosticSeverity, TOOL_RESULT_ENTRY, ToolDiagnostic as EntryDiagnostic, ToolResultData,
};
use crate::env::{ExecutionEnv, ShellOutputSkip, ShellOutputWindow};
use crate::harness::live::{
    LIVE_DOC, ToolSlot, clear_progress, finish_slot, live_state, tool_slot_index,
};
use crate::harness::output::{
    OutputBuffer, OutputChunk, OutputLimits, PROGRESS_BYTES_PER_SECOND, Progress, Retain,
    bound_output,
};
use crate::harness::types::{
    Agent, CommitOperation, ConversationHandle, HookHandlers, NextTaskState, RegistrySnapshot,
    Replay, RunningTask, SettledTask, TaskCreationOptions, TaskRuntime, ToolControl,
    ToolDiagnostic, ToolDiagnosticSeverity, ToolExecutionApi, ToolExecutionResult,
    ToolRegistration,
};
use crate::harness::usage::{UsageBucket, record_usage};
use crate::session::SessionError;
use crate::session::transaction::Transaction;
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, utf8_byte_length};
use crate::types::{
    ConversationId, DocAccess, EntryDraft, EntryId, EntryRecord, JsonObject, Task,
    TaskDefinitionSpec, TaskId, TaskOutcome, TaskState,
};

/// 对应 `ToolTaskInput`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTaskInput {
    /// 发出该调用的 assistant 条目。
    pub assistant: EntryId,
    /// 调用 ID。
    pub call_id: String,
}

/// 对应 `ToolTaskCheckpoint`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum ToolTaskCheckpoint {
    /// `{ phase: "call" }`。
    Call,
    /// `{ phase: "execute"; arguments; replay }`：执行前的持久意图。
    Execute {
        /// 最终参数。
        arguments: JsonObject,
        /// 重放策略。
        replay: Replay,
    },
}

/// 对应 `ToolTaskResult`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTaskResult {
    /// 结果条目。
    pub entry_id: EntryId,
    /// 后续控制。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ToolControl>,
}

/// 参数，或它们为什么无效。
enum Checked {
    Args(JsonObject),
    Error(String),
}

/// 一次运行中工具通过 api 上报的内容。
struct Reported {
    output: OutputBuffer,
    diagnostics: Vec<ToolDiagnostic>,
    details: Option<JsonValue>,
}

/// 工具任务如何结束；结果条目总是追加。
#[derive(Clone)]
enum Ending {
    Completed,
    Aborted,
    Failed { message: String },
}

const COMPLETED: Ending = Ending::Completed;

/// 对应 `ToolTask`：内置工具任务。
struct ToolTaskDefinition;

/// 对应 `ToolTask` 工厂（无状态，不需要宿主依赖）。
pub fn make_tool_task() -> Arc<Task> {
    Arc::new(Task::new(Arc::new(ToolTaskDefinition)))
}

fn session_error(error: impl std::fmt::Display) -> SessionError {
    SessionError::Message(error.to_string())
}

fn doc_access(conversation_id: ConversationId) -> DocAccess {
    DocAccess {
        owner: Some(conversation_id.get()),
        key: None,
    }
}

fn checkpoint_json(task: &RunningTask) -> JsonValue {
    match &task.0.state {
        TaskState::Running { checkpoint } => checkpoint.clone(),
        _ => JsonValue::Null,
    }
}

/// `pi.live.tools[index].key` 的路径。
fn slot_path(index: usize, key: &str) -> Vec<PathSegment> {
    vec![
        PathSegment::Key("tools".to_string()),
        PathSegment::Index(index),
        PathSegment::Key(key.to_string()),
    ]
}

/// 对应 `errorText(error)`。
fn error_text(error: &SessionError) -> String {
    error.to_string()
}

/// 对应 TS 的 catch 分支：构造 `tool_error` 结果 + `Tool {name} threw` failed 结算。
fn tool_threw(call: &ToolCall, error: &SessionError) -> (ToolExecutionResult, Ending) {
    (
        ToolExecutionResult {
            content: None,
            is_error: Some(true),
            details: None,
            diagnostics: vec![tool_diagnostic(
                ToolDiagnosticSeverity::Error,
                "tool_error",
                &error_text(error),
            )],
            usage: None,
            control: None,
        },
        Ending::Failed {
            message: format!("Tool {} threw", call.name),
        },
    )
}

/// 对应 `readCall(runtime, input, context)`：assistant 条目里 `callId` 的调用。
async fn read_call(
    runtime: &Arc<dyn TaskRuntime>,
    input: &ToolTaskInput,
    context: Arc<dyn Context>,
) -> Result<ToolCall, SessionError> {
    let entry = runtime.entry(input.assistant, context).await?;
    let message = entry.and_then(|entry| entry.model.into_iter().flatten().next());
    let call = match message {
        Some(Message::Assistant(assistant)) => assistant
            .content
            .into_iter()
            .find(|content| matches!(content, ContentBlock::ToolCall(call) if call.id == input.call_id))
            .and_then(|content| match content {
                ContentBlock::ToolCall(call) => Some(call),
                _ => None,
            }),
        _ => None,
    };
    call.ok_or_else(|| {
        SessionError::Message(format!(
            "Entry {} has no tool call {}",
            input.assistant, input.call_id
        ))
    })
}

/// 对应 `prepare(tool, args)`：按工具的 `prepareArguments` 修复参数。
fn prepare(tool: &Arc<dyn ToolRegistration>, args: &JsonObject) -> Checked {
    match tool.prepare_arguments(JsonValue::Object(args.clone())) {
        Ok(JsonValue::Object(args)) => Checked::Args(args),
        Ok(_) => Checked::Error("prepareArguments returned a non-object".to_string()),
        Err(error) => Checked::Error(error_text(&error)),
    }
}

/// 对应 `validate(tool, call, args)`：按实现 schema 校验并强制转换。
fn validate(tool: &Arc<dyn ToolRegistration>, call: &ToolCall, args: JsonObject) -> Checked {
    let pi_tool = pi_ai::Tool {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        parameters: tool.parameters().clone(),
        constrained_sampling: None,
    };
    let call_with_args = ToolCall {
        kind: ToolCallKind,
        id: call.id.clone(),
        name: call.name.clone(),
        arguments: JsonValue::Object(args),
        thought_signature: None,
        namespace: None,
    };
    match validate_tool_arguments(&pi_tool, &call_with_args) {
        Ok(JsonValue::Object(args)) => Checked::Args(args),
        Ok(_) => Checked::Error("validated arguments are not an object".to_string()),
        Err(error) => Checked::Error(error),
    }
}

/// 对应 `invalid(message)`。
fn invalid(message: String) -> ToolExecutionResult {
    harness_error("invalid_arguments", &message)
}

/// 对应 `harnessError(code, message)`：Harness 自己写的错误结果。
pub fn harness_error(code: &str, message: &str) -> ToolExecutionResult {
    ToolExecutionResult {
        content: Some(Vec::new()),
        is_error: Some(true),
        details: None,
        diagnostics: vec![tool_diagnostic(
            ToolDiagnosticSeverity::Error,
            code,
            message,
        )],
        usage: None,
        control: None,
    }
}

fn tool_diagnostic(severity: ToolDiagnosticSeverity, code: &str, message: &str) -> ToolDiagnostic {
    ToolDiagnostic {
        severity,
        message: message.to_string(),
        code: Some(code.to_string()),
    }
}

/// 对应 `truncated(dropped, retain?)`：Harness 的截断诊断。
fn truncated(dropped_lines: usize, dropped_bytes: usize, retain: Option<Retain>) -> ToolDiagnostic {
    let kept = match retain {
        None => String::new(),
        Some(Retain::Head) => " to its beginning".to_string(),
        Some(Retain::Tail) => " to its end".to_string(),
    };
    ToolDiagnostic {
        severity: ToolDiagnosticSeverity::Warn,
        code: Some("truncated".to_string()),
        message: format!(
            "Output truncated{kept}: {dropped_lines} lines, {dropped_bytes} bytes dropped"
        ),
    }
}

fn to_entry_diagnostic(diagnostic: &ToolDiagnostic) -> EntryDiagnostic {
    EntryDiagnostic {
        severity: match diagnostic.severity {
            ToolDiagnosticSeverity::Info => DiagnosticSeverity::Info,
            ToolDiagnosticSeverity::Warn => DiagnosticSeverity::Warn,
            ToolDiagnosticSeverity::Error => DiagnosticSeverity::Error,
        },
        message: diagnostic.message.clone(),
        code: diagnostic.code.clone(),
    }
}

/// 对应 `renderDiagnostics(diagnostics)`。
fn render_diagnostics(diagnostics: &[ToolDiagnostic]) -> String {
    let lines = diagnostics
        .iter()
        .map(|diagnostic| {
            format!(
                "[{}] {}",
                match diagnostic.severity {
                    ToolDiagnosticSeverity::Info => "info",
                    ToolDiagnosticSeverity::Warn => "warn",
                    ToolDiagnosticSeverity::Error => "error",
                },
                diagnostic.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("<harness>\n{lines}\n</harness>")
}

/// 对应 `appendToolResult(tx, conversationId, call, result, timestamp, durationMs?)`。
pub async fn append_tool_result(
    tx: &Transaction,
    conversation_id: ConversationId,
    call: &ToolCall,
    result: &ToolExecutionResult,
    timestamp: u64,
    duration_ms: Option<u64>,
) -> Result<EntryRecord, SessionError> {
    let diagnostics = result.diagnostics.clone();
    let mut content = result.content.clone().unwrap_or_default();
    if !diagnostics.is_empty() {
        content.push(TextOrImageContent::Text(pi_ai::TextContent {
            kind: TextKind,
            text: render_diagnostics(&diagnostics),
            text_signature: None,
        }));
    }
    let message = ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content,
        details: result.details.clone(),
        usage: result.usage.clone(),
        added_tool_names: None,
        is_error: result.is_error.unwrap_or(false),
        timestamp,
        duration_ms,
    };
    if let Some(usage) = &result.usage {
        record_usage(tx, conversation_id, UsageBucket::Tools, &call.name, usage).await?;
    }
    let data = ToolResultData {
        diagnostics: diagnostics.iter().map(to_entry_diagnostic).collect(),
    };
    tx.append_entry(
        None,
        conversation_id,
        EntryDraft {
            kind: TOOL_RESULT_ENTRY.kind().to_string(),
            model: Some(vec![Message::ToolResult(message)]),
            data: Some(serde_json::to_value(&data).expect("tool result data serialises")),
            head: None,
            edits: None,
        },
    )
    .await
}

/// 对应 `boundContent(content, limits)`。
fn bound_content(
    content: Vec<TextOrImageContent>,
    limits: &OutputLimits,
) -> (Vec<TextOrImageContent>, usize, usize) {
    let texts: Vec<usize> = content
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            TextOrImageContent::Text(_) => Some(index),
            _ => None,
        })
        .collect();
    let joined = content
        .iter()
        .filter_map(|item| match item {
            TextOrImageContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let bounded = bound_output(&joined, limits);
    if bounded.dropped_bytes == 0 {
        return (content, 0, 0);
    }
    let keep = match limits.retain {
        Retain::Head => texts.first().copied(),
        Retain::Tail => texts.last().copied(),
    };
    let mut result = Vec::new();
    for (index, item) in content.into_iter().enumerate() {
        match item {
            TextOrImageContent::Image(image) => result.push(TextOrImageContent::Image(image)),
            TextOrImageContent::Text(mut text) if Some(index) == keep => {
                text.text = bounded.text.clone();
                result.push(TextOrImageContent::Text(text));
            }
            // 对应上游 `boundContent`：除 keep 外的文本项被丢弃，结果只剩一个裁剪后的文本项。
            TextOrImageContent::Text(_) => {}
        }
    }
    (result, bounded.dropped_bytes, bounded.dropped_lines)
}

/// 对应 `fromSlot(slot, code, message)`：从槽位持久化部分输出重建错误结果。
fn from_slot(slot: Option<&ToolSlot>, code: &str, message: &str) -> ToolExecutionResult {
    let mut diagnostics = slot
        .and_then(|slot| slot.diagnostics.clone())
        .unwrap_or_default();
    let dropped_bytes = slot.and_then(|slot| slot.dropped_bytes).unwrap_or(0);
    if dropped_bytes > 0 {
        let dropped_lines = slot.and_then(|slot| slot.dropped_lines).unwrap_or(0);
        diagnostics.push(truncated(
            dropped_lines as usize,
            dropped_bytes as usize,
            None,
        ));
    }
    diagnostics.push(tool_diagnostic(
        ToolDiagnosticSeverity::Error,
        code,
        message,
    ));
    let content = match slot.and_then(|slot| slot.output.clone()) {
        Some(output) if !output.is_empty() => {
            vec![TextOrImageContent::Text(pi_ai::TextContent {
                kind: TextKind,
                text: output,
                text_signature: None,
            })]
        }
        _ => Vec::new(),
    };
    ToolExecutionResult {
        content: Some(content),
        is_error: Some(true),
        details: slot.and_then(|slot| slot.details.clone()),
        diagnostics,
        usage: None,
        control: None,
    }
}

impl ToolTaskDefinition {
    /// 对应 `settle(runtime, call, ending, build, context, durationMs?)`。
    async fn settle(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        call: &ToolCall,
        ending: Ending,
        build: impl Fn(Option<&ToolSlot>) -> ToolExecutionResult + Send + 'static,
        context: Arc<dyn Context>,
        duration_ms: Option<u64>,
    ) -> Result<(), SessionError> {
        let runtime = Arc::clone(runtime);
        let runtime_for_closure = Arc::clone(&runtime);
        let call = call.clone();
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_closure);
                    let call = call.clone();
                    let ending = ending.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        let index = tool_slot_index(&live, runtime.task_id());
                        let slot = index.and_then(|index| {
                            live_state(&live)
                                .tools
                                .and_then(|tools| tools.get(index).cloned())
                        });
                        let result = build(slot.as_ref());
                        let entry = append_tool_result(
                            tx,
                            runtime.conversation_id(),
                            &call,
                            &result,
                            runtime.now(),
                            duration_ms,
                        )
                        .await?;
                        if let Some(index) = index {
                            finish_slot(&live, index, Some(entry.id)).map_err(session_error)?;
                        }
                        let entry_id = entry.id;
                        match ending {
                            Ending::Aborted => Ok(Some(NextTaskState::Terminal {
                                outcome: TaskOutcome::Aborted {
                                    reason: None,
                                    result: Some(
                                        serde_json::to_value(&ToolTaskResult {
                                            entry_id,
                                            control: None,
                                        })
                                        .expect("tool task result serialises"),
                                    ),
                                },
                            })),
                            Ending::Failed { message } => Ok(Some(NextTaskState::Terminal {
                                outcome: TaskOutcome::Failed {
                                    error: crate::types::TaskOutcomeError {
                                        message,
                                        detail: None,
                                    },
                                    result: Some(
                                        serde_json::to_value(&ToolTaskResult {
                                            entry_id,
                                            control: None,
                                        })
                                        .expect("tool task result serialises"),
                                    ),
                                },
                            })),
                            Ending::Completed => {
                                let control = result.control.clone();
                                Ok(Some(NextTaskState::Terminal {
                                    outcome: TaskOutcome::Completed {
                                        result: serde_json::to_value(&ToolTaskResult {
                                            entry_id,
                                            control,
                                        })
                                        .expect("tool task result serialises"),
                                    },
                                }))
                            }
                        }
                    })
                }),
                context,
            )
            .await
    }

    /// 对应 `run(runtime, call, tool, args, context)`：用解析出的实现执行，然后结算结果。
    async fn run(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        call: &ToolCall,
        tool: &Arc<dyn ToolRegistration>,
        args: JsonObject,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let limits = OutputLimits {
            max_bytes: tool
                .output_limits()
                .map(|limits| limits.max_bytes)
                .unwrap_or(DEFAULT_MAX_BYTES),
            max_lines: tool
                .output_limits()
                .map(|limits| limits.max_lines)
                .unwrap_or(DEFAULT_MAX_LINES),
            retain: tool
                .output_limits()
                .map(|limits| limits.retain)
                .unwrap_or(Retain::Head),
        };
        let reported = Arc::new(Mutex::new(Reported {
            output: OutputBuffer::new(limits),
            diagnostics: Vec::new(),
            details: None,
        }));
        let progress = self.publish_progress(runtime, &reported, &limits, &context);
        let ended = Arc::new(AtomicBool::new(false));

        let result: ToolExecutionResult;
        let mut ending = COMPLETED;
        let mut duration_ms: Option<u64> = None;

        // 对应上游：env 构建在 try 内，失败也走 tool_error + failed 结算（而非 ? 传播）。
        match runtime.env(Arc::clone(&context)).await {
            Ok(env) => {
                let started_at = Instant::now();
                let tool_api: Arc<dyn ToolExecutionApi> = Arc::new(ToolApi {
                    runtime: Arc::clone(runtime),
                    call_id: call.id.clone(),
                    reported: Arc::clone(&reported),
                    progress: Arc::clone(&progress),
                    ended: Arc::clone(&ended),
                    registry: runtime.registry(),
                    env,
                    output_window: if limits.retain == Retain::Tail {
                        Some(ShellOutputWindow {
                            max_bytes: limits.max_bytes,
                            max_lines: limits.max_lines,
                            min_interval_ms: runtime.settings().progress.output_interval_ms,
                            bytes_per_second: PROGRESS_BYTES_PER_SECOND as f64,
                        })
                    } else {
                        None
                    },
                });
                match tool
                    .execute(
                        JsonValue::Object(args),
                        Arc::clone(&tool_api) as Arc<dyn ToolExecutionApi>,
                        Arc::clone(&context),
                    )
                    .await
                {
                    Ok(executed) => {
                        result = executed;
                    }
                    Err(error) => {
                        if runtime.signal().aborted() {
                            ended.store(true, Ordering::SeqCst);
                            let pending = progress.stop().await;
                            for waiter in pending {
                                waiter.reject(error.clone());
                            }
                            return Err(error);
                        }
                        let (error_result, error_ending) = tool_threw(call, &error);
                        result = error_result;
                        ending = error_ending;
                    }
                }
                duration_ms = Some(started_at.elapsed().as_millis() as u64);
            }
            Err(error) => {
                if runtime.signal().aborted() {
                    ended.store(true, Ordering::SeqCst);
                    let pending = progress.stop().await;
                    for waiter in pending {
                        waiter.reject(error.clone());
                    }
                    return Err(error);
                }
                let (error_result, error_ending) = tool_threw(call, &error);
                result = error_result;
                ending = error_ending;
            }
        }
        ended.store(true, Ordering::SeqCst);
        {
            reported.lock().expect("reported").output.end();
        }
        let pending = progress.stop().await;
        let settled = self
            .final_result(runtime, call, result, &reported, &limits, &context)
            .await?;
        self.settle(
            runtime,
            call,
            ending,
            move |_slot| settled.clone(),
            context,
            duration_ms,
        )
        .await?;
        for waiter in pending {
            waiter.resolve();
        }
        Ok(())
    }

    /// 对应 `publishProgress(runtime, reported, context)`：节流提交工具上报的内容到 `pi.live.tools` 槽位。
    fn publish_progress(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        reported: &Arc<Mutex<Reported>>,
        _limits: &OutputLimits,
        context: &Arc<dyn Context>,
    ) -> Arc<Progress> {
        let runtime = Arc::clone(runtime);
        let reported = Arc::clone(reported);
        let context = Arc::clone(context);
        let runtime_for_error = Arc::clone(&runtime);
        let output_interval_ms = runtime.settings().progress.output_interval_ms;
        let written: Arc<Mutex<(String, Option<JsonValue>, usize)>> =
            Arc::new(Mutex::new((String::new(), None, 0)));
        let write: crate::harness::output::ProgressWrite =
            Arc::new(move || -> BoxFuture<'static, Result<usize, SessionError>> {
                let runtime = Arc::clone(&runtime);
                let reported = Arc::clone(&reported);
                let written = Arc::clone(&written);
                let context = Arc::clone(&context);
                Box::pin(async move {
                    let (snapshot, details, added, details_changed, new_written, prev_text) = {
                        let mut reported = reported.lock().expect("reported");
                        let snapshot = reported.output.snapshot();
                        let details = reported.details.clone();
                        let diagnostics_len = reported.diagnostics.len();
                        let (prev_text, prev_details, prev_diagnostics) =
                            written.lock().expect("written").clone();
                        let added = reported.diagnostics[prev_diagnostics..].to_vec();
                        let details_changed = details != prev_details;
                        let new_written = (snapshot.text.clone(), details.clone(), diagnostics_len);
                        (
                            snapshot,
                            details,
                            added,
                            details_changed,
                            new_written,
                            prev_text,
                        )
                    };
                    let bytes = if snapshot.text != prev_text {
                        let shared = if snapshot.text.starts_with(&prev_text) {
                            prev_text.len()
                        } else {
                            crate::chord::delta::overlap(&prev_text, &snapshot.text, 65_536, 64, 8)
                        };
                        utf8_byte_length(&snapshot.text[shared..])
                    } else {
                        0
                    };
                    let runtime_for_commit = Arc::clone(&runtime);
                    runtime
                        .commit(
                            Box::new(move |tx, _current| {
                                let runtime = Arc::clone(&runtime_for_commit);
                                let snapshot = snapshot.clone();
                                let details = details.clone();
                                let added = added.clone();
                                let details_changed = details_changed;
                                Box::pin(async move {
                                    let live = tx
                                        .doc(
                                            &*LIVE_DOC,
                                            doc_access(runtime.conversation_id()),
                                            None,
                                        )
                                        .await
                                        .map_err(SessionError::Doc)?;
                                    let Some(index) = tool_slot_index(&live, runtime.task_id())
                                    else {
                                        return Ok(None);
                                    };
                                    let slot = live_state(&live)
                                        .tools
                                        .and_then(|tools| tools.get(index).cloned());
                                    if slot
                                        .as_ref()
                                        .and_then(|slot| slot.output.clone())
                                        .unwrap_or_default()
                                        != snapshot.text
                                    {
                                        live.set(
                                            slot_path(index, "output"),
                                            JsonValue::String(snapshot.text.clone()),
                                        )
                                        .map_err(session_error)?;
                                    }
                                    if snapshot.dropped_bytes > 0 {
                                        live.set(
                                            slot_path(index, "droppedBytes"),
                                            JsonValue::from(snapshot.dropped_bytes as u64),
                                        )
                                        .map_err(session_error)?;
                                    }
                                    if snapshot.dropped_lines > 0 {
                                        live.set(
                                            slot_path(index, "droppedLines"),
                                            JsonValue::from(snapshot.dropped_lines as u64),
                                        )
                                        .map_err(session_error)?;
                                    }
                                    if details_changed && let Some(details) = &details {
                                        live.set(slot_path(index, "details"), details.clone())
                                            .map_err(session_error)?;
                                    }
                                    if !added.is_empty() {
                                        let existing = slot
                                            .as_ref()
                                            .and_then(|slot| slot.diagnostics.clone())
                                            .map(|diagnostics| diagnostics.len())
                                            .unwrap_or(0);
                                        if existing == 0 {
                                            live.set(
                                                slot_path(index, "diagnostics"),
                                                JsonValue::Array(Vec::new()),
                                            )
                                            .map_err(session_error)?;
                                        }
                                        let mut at = existing;
                                        for diagnostic in &added {
                                            live.splice(
                                                slot_path(index, "diagnostics"),
                                                at,
                                                0,
                                                vec![
                                                    serde_json::to_value(diagnostic)
                                                        .expect("diagnostic serialises"),
                                                ],
                                            )
                                            .map_err(session_error)?;
                                            at += 1;
                                        }
                                    }
                                    Ok(None)
                                })
                            }),
                            Arc::clone(&context),
                        )
                        .await?;
                    *written.lock().expect("written") = new_written;
                    Ok(bytes)
                })
            });
        let on_error: crate::harness::output::ProgressErrorReporter = {
            let runtime = Arc::clone(&runtime_for_error);
            Arc::new(move |error| {
                if !runtime.signal().aborted() {
                    runtime.report(error);
                }
            })
        };
        Progress::new(write, on_error, output_interval_ms)
    }

    /// 对应 `finalResult(...)`：合并运行结果、afterTool 与显式文本界限。
    async fn final_result(
        &self,
        runtime: &Arc<dyn TaskRuntime>,
        call: &ToolCall,
        result: ToolExecutionResult,
        reported: &Arc<Mutex<Reported>>,
        limits: &OutputLimits,
        context: &Arc<dyn Context>,
    ) -> Result<ToolExecutionResult, SessionError> {
        let mut harness_diagnostics: Vec<ToolDiagnostic> = Vec::new();
        let retained = if result.content.is_none() {
            Some(reported.lock().expect("reported").output.snapshot())
        } else {
            None
        };
        let content = match &retained {
            Some(retained) if retained.text.is_empty() => Vec::new(),
            Some(retained) => vec![TextOrImageContent::Text(pi_ai::TextContent {
                kind: TextKind,
                text: retained.text.clone(),
                text_signature: None,
            })],
            None => result.content.clone().unwrap_or_default(),
        };
        let mut final_result = ToolExecutionResult {
            content: Some(content.clone()),
            is_error: result.is_error,
            details: if result.details.is_none() {
                reported.lock().expect("reported").details.clone()
            } else {
                result.details.clone()
            },
            diagnostics: {
                let mut diagnostics = reported.lock().expect("reported").diagnostics.clone();
                diagnostics.extend(result.diagnostics.clone());
                diagnostics
            },
            usage: result.usage.clone(),
            control: result.control.clone(),
        };
        for handlers in runtime.hooks().handlers() {
            if let HookHandlers::Tool(hook) = handlers.as_ref() {
                let api: Arc<dyn crate::harness::types::HookApi> = Arc::new(
                    crate::harness::types::RuntimeHookApi::new(Arc::clone(runtime)),
                );
                if let Some(replaced) = hook
                    .after_tool(call, final_result.clone(), api, Arc::clone(context))
                    .await?
                {
                    final_result = replaced;
                }
            }
        }
        if let Some(retained) = &retained
            && retained.dropped_bytes > 0
            && final_result.content.as_ref() == Some(&content)
        {
            harness_diagnostics.push(truncated(
                retained.dropped_lines,
                retained.dropped_bytes,
                Some(limits.retain),
            ));
        }
        let bounded = bound_content(final_result.content.clone().unwrap_or_default(), limits);
        if bounded.1 > 0 {
            harness_diagnostics.push(truncated(bounded.2, bounded.1, Some(limits.retain)));
        }
        final_result.content = Some(bounded.0);
        final_result.diagnostics.extend(harness_diagnostics);
        Ok(final_result)
    }

    /// 对应 `call` phase。
    async fn run_call(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let input: ToolTaskInput =
            serde_json::from_value(task.0.input.clone()).map_err(session_error)?;
        let call = read_call(runtime, &input, Arc::clone(&context)).await?;
        let agent = runtime.agent(Arc::clone(&context)).await?;
        let tool = agent
            .tools
            .iter()
            .find(|tool| tool.name() == call.name)
            .cloned();
        let Some(tool) = tool else {
            let error = harness_error(
                "tool_unavailable",
                &format!("Tool {} is not available", call.name),
            );
            return self
                .settle(
                    runtime,
                    &call,
                    COMPLETED,
                    move |_slot| error.clone(),
                    context,
                    None,
                )
                .await;
        };
        let checked = match prepare(
            &tool,
            call.arguments.as_object().unwrap_or(&JsonObject::new()),
        ) {
            Checked::Error(error) => {
                let error = invalid(error);
                return self
                    .settle(
                        runtime,
                        &call,
                        COMPLETED,
                        move |_slot| error.clone(),
                        context,
                        None,
                    )
                    .await;
            }
            Checked::Args(args) => match validate(&tool, &call, args) {
                Checked::Error(error) => {
                    let error = invalid(error);
                    return self
                        .settle(
                            runtime,
                            &call,
                            COMPLETED,
                            move |_slot| error.clone(),
                            context,
                            None,
                        )
                        .await;
                }
                Checked::Args(args) => args,
            },
        };
        let mut args = checked;
        let mut block: Option<String> = None;
        for handlers in runtime.hooks().handlers() {
            if let HookHandlers::Tool(hook) = handlers.as_ref() {
                if block.is_some() {
                    continue;
                }
                let api: Arc<dyn crate::harness::types::HookApi> = Arc::new(
                    crate::harness::types::RuntimeHookApi::new(Arc::clone(runtime)),
                );
                match hook.before_tool(&call, api, Arc::clone(&context)).await {
                    Ok(Some(decision)) => {
                        if let Some(reason) = decision.block {
                            block = Some(reason);
                        } else if let Some(arguments) = decision.arguments {
                            args = arguments;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if runtime.signal().aborted() {
                            return Err(error);
                        }
                        block = Some(error_text(&error));
                    }
                }
            }
        }
        if let Some(block) = block {
            let blocked = harness_error("blocked", &format!("Tool call blocked: {block}"));
            return self
                .settle(
                    runtime,
                    &call,
                    COMPLETED,
                    move |_slot| blocked.clone(),
                    context,
                    None,
                )
                .await;
        }
        let validated = match validate(&tool, &call, args) {
            Checked::Error(error) => {
                let error = invalid(error);
                return self
                    .settle(
                        runtime,
                        &call,
                        COMPLETED,
                        move |_slot| error.clone(),
                        context,
                        None,
                    )
                    .await;
            }
            Checked::Args(args) => args,
        };
        let final_args = validated;
        let replay = tool.replay();
        let checkpoint = ToolTaskCheckpoint::Execute {
            arguments: final_args.clone(),
            replay,
        };
        let runtime_for_commit = Arc::clone(runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let runtime = Arc::clone(&runtime_for_commit);
                    let checkpoint = checkpoint.clone();
                    Box::pin(async move {
                        let live = tx
                            .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                            .await
                            .map_err(SessionError::Doc)?;
                        if let Some(index) = tool_slot_index(&live, runtime.task_id()) {
                            live.set(
                                slot_path(index, "status"),
                                JsonValue::String("running".to_string()),
                            )
                            .map_err(session_error)?;
                        }
                        Ok(Some(NextTaskState::Running {
                            checkpoint: serde_json::to_value(&checkpoint).map_err(session_error)?,
                        }))
                    })
                }),
                Arc::clone(&context),
            )
            .await?;
        self.run(runtime, &call, &tool, final_args, Arc::clone(&context))
            .await
    }

    /// 对应 `execute` phase：恢复后按 replay 规则重跑或失败。
    async fn run_execute(
        &self,
        task: &RunningTask,
        runtime: &Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let checkpoint: ToolTaskCheckpoint =
            serde_json::from_value(checkpoint_json(task)).map_err(session_error)?;
        let ToolTaskCheckpoint::Execute { arguments, replay } = checkpoint else {
            return Err(SessionError::Message(
                "pi.tool execute phase has no intent".to_string(),
            ));
        };
        let input: ToolTaskInput =
            serde_json::from_value(task.0.input.clone()).map_err(session_error)?;
        let call = read_call(runtime, &input, Arc::clone(&context)).await?;
        let agent = runtime.agent(Arc::clone(&context)).await?;
        let tool = agent
            .tools
            .iter()
            .find(|tool| tool.name() == call.name)
            .cloned();
        if replay == Replay::Safe
            && tool
                .as_ref()
                .is_some_and(|tool| tool.replay() == Replay::Safe)
        {
            let tool = tool.expect("tool");
            let runtime_for_commit = Arc::clone(runtime);
            runtime
                .commit(
                    Box::new(move |tx, _current| {
                        let runtime = Arc::clone(&runtime_for_commit);
                        Box::pin(async move {
                            let live = tx
                                .doc(&*LIVE_DOC, doc_access(runtime.conversation_id()), None)
                                .await
                                .map_err(SessionError::Doc)?;
                            if let Some(index) = tool_slot_index(&live, runtime.task_id()) {
                                clear_progress(&live, index).map_err(session_error)?;
                            }
                            Ok(None)
                        })
                    }),
                    Arc::clone(&context),
                )
                .await?;
            return self
                .run(runtime, &call, &tool, arguments, Arc::clone(&context))
                .await;
        }
        let message = format!(
            "Tool {} was interrupted and may have partially run",
            call.name
        );
        self.settle(
            runtime,
            &call,
            Ending::Failed {
                message: message.clone(),
            },
            move |slot| from_slot(slot, "interrupted", &message),
            context,
            None,
        )
        .await
    }
}

/// 对应 `run` 里构造的 `ToolExecutionApi`。
struct ToolApi {
    runtime: Arc<dyn TaskRuntime>,
    call_id: String,
    reported: Arc<Mutex<Reported>>,
    progress: Arc<Progress>,
    ended: Arc<AtomicBool>,
    registry: RegistrySnapshot,
    env: Option<Arc<dyn ExecutionEnv>>,
    output_window: Option<ShellOutputWindow>,
}

#[async_trait::async_trait]
impl ToolExecutionApi for ToolApi {
    fn task_id(&self) -> TaskId {
        self.runtime.task_id()
    }

    fn conversation_id(&self) -> ConversationId {
        self.runtime.conversation_id()
    }

    fn call_id(&self) -> &str {
        &self.call_id
    }

    fn registry(&self) -> &RegistrySnapshot {
        &self.registry
    }

    async fn agent(&self, context: Arc<dyn Context>) -> Result<Agent, SessionError> {
        self.runtime.agent(context).await
    }

    fn models(&self) -> Arc<pi_ai::Models> {
        self.runtime.models()
    }

    fn env(&self) -> Option<Arc<dyn ExecutionEnv>> {
        self.env.clone()
    }

    fn output(&self, chunk: OutputChunk<'_>, skipped: Option<ShellOutputSkip>) {
        assert!(
            !self.ended.load(Ordering::SeqCst),
            "Tool call {} has settled",
            self.call_id
        );
        let accepted = {
            let mut reported = self.reported.lock().expect("reported");
            reported.output.push(chunk, skipped)
        };
        if accepted {
            self.progress.mark();
        }
    }

    fn output_window(&self) -> Option<ShellOutputWindow> {
        self.output_window
    }

    fn diagnostic(&self, diagnostic: ToolDiagnostic) {
        assert!(
            !self.ended.load(Ordering::SeqCst),
            "Tool call {} has settled",
            self.call_id
        );
        self.reported
            .lock()
            .expect("reported")
            .diagnostics
            .push(diagnostic);
        self.progress.mark();
    }

    async fn details(
        &self,
        value: JsonValue,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        assert!(
            !self.ended.load(Ordering::SeqCst),
            "Tool call {} has settled",
            self.call_id
        );
        self.reported.lock().expect("reported").details = Some(value);
        let committed = self.progress.mark_and_wait();
        await_with_context(committed, context.as_ref())
            .await
            .map_err(|_| SessionError::Aborted(pi_ai::AbortError))?
    }

    async fn commit(
        &self,
        change: CommitOperation<'static>,
        context: Arc<dyn Context>,
    ) -> Result<JsonValue, SessionError> {
        let result: Arc<Mutex<Option<JsonValue>>> = Arc::new(Mutex::new(None));
        let result_for_closure = Arc::clone(&result);
        let runtime = Arc::clone(&self.runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let result_for_closure = Arc::clone(&result_for_closure);
                    Box::pin(async move {
                        let value = change(tx).await?;
                        *result_for_closure.lock().expect("result") = Some(value);
                        Ok(None)
                    })
                }),
                context,
            )
            .await?;
        Ok(result
            .lock()
            .expect("result")
            .clone()
            .unwrap_or(JsonValue::Null))
    }

    async fn memo(
        &self,
        name: &str,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonValue>, SessionError> {
        self.runtime.memo(name, context).await
    }

    async fn memo_or(
        &self,
        name: &str,
        candidate: JsonValue,
        context: Arc<dyn Context>,
    ) -> Result<JsonValue, SessionError> {
        self.runtime.memo_or(name, candidate, context).await
    }

    async fn create_task(
        &self,
        task: &Task,
        input: JsonValue,
        options: TaskCreationOptions,
        context: Arc<dyn Context>,
    ) -> Result<TaskId, SessionError> {
        let id: Arc<Mutex<Option<TaskId>>> = Arc::new(Mutex::new(None));
        let id_for_closure = Arc::clone(&id);
        let task = task.clone();
        let conversation_id = self.runtime.conversation_id();
        let runtime = Arc::clone(&self.runtime);
        runtime
            .commit(
                Box::new(move |tx, _current| {
                    let id_for_closure = Arc::clone(&id_for_closure);
                    let task = task.clone();
                    let options = options.with_conversation(conversation_id);
                    let input = input.clone();
                    Box::pin(async move {
                        let task_id = tx.create_task(&task, input, options).await?;
                        *id_for_closure.lock().expect("id") = Some(task_id);
                        Ok(None)
                    })
                }),
                context,
            )
            .await?;
        id.lock()
            .expect("id")
            .ok_or_else(|| SessionError::Message("createTask did not produce an id".to_string()))
    }

    async fn get_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<Option<crate::types::TaskRecord<JsonValue, JsonValue, JsonValue>>, SessionError>
    {
        self.runtime.get_task(id, context).await
    }

    async fn wait_for_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<SettledTask, SessionError> {
        self.runtime.wait_for_task(id, context).await
    }

    async fn conversation(
        &self,
        id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn ConversationHandle>>, SessionError> {
        self.runtime.conversation(id, context).await
    }
}

#[async_trait::async_trait]
impl crate::session::DocumentReader for ToolApi {
    async fn snapshot(
        &self,
        token: &dyn crate::documents::AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        self.runtime.snapshot(token, owner, key, context).await
    }

    async fn snapshot_as_of(
        &self,
        token: &dyn crate::documents::AnyDocToken,
        owner: u64,
        key: Option<String>,
        at: EntryId,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        self.runtime
            .snapshot_as_of(token, owner, key, at, context)
            .await
    }
}

#[async_trait::async_trait]
impl crate::session::DocumentObserver for ToolApi {
    async fn watch_doc(
        &self,
        token: &dyn crate::documents::AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<crate::session::DocumentWatch>>, SessionError> {
        self.runtime.watch_doc(token, owner, key, context).await
    }
}

#[async_trait::async_trait]
impl TaskDefinitionSpec for ToolTaskDefinition {
    fn name(&self) -> &str {
        "pi.tool"
    }

    fn version(&self) -> u32 {
        1
    }

    fn initial(&self, _input: &JsonValue) -> JsonValue {
        serde_json::to_value(ToolTaskCheckpoint::Call).expect("call serialises")
    }

    fn phases(&self) -> &[&'static str] {
        &["call", "execute"]
    }

    async fn run_phase(
        &self,
        phase: &str,
        task: RunningTask,
        runtime: Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        match phase {
            "call" => self.run_call(&task, &runtime, context).await,
            "execute" => self.run_execute(&task, &runtime, context).await,
            _ => Err(SessionError::Message(format!(
                "Task pi.tool has no phase handler for {phase}"
            ))),
        }
    }

    async fn abort(
        &self,
        task: RunningTask,
        runtime: Arc<dyn TaskRuntime>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        let input: ToolTaskInput =
            serde_json::from_value(task.0.input.clone()).map_err(session_error)?;
        let call = read_call(&runtime, &input, Arc::clone(&context)).await?;
        let message = format!("Tool {} was aborted", call.name);
        self.settle(
            &runtime,
            &call,
            Ending::Aborted,
            move |slot| from_slot(slot, "aborted", &message),
            context,
            None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_task_metadata_matches_upstream() {
        let task = make_tool_task();
        assert_eq!(task.definition().name(), "pi.tool");
        assert_eq!(task.definition().version(), 1);
        assert_eq!(task.definition().phases(), &["call", "execute"]);
        assert_eq!(
            task.definition().initial(&JsonValue::Null),
            json!({"phase": "call"})
        );
    }

    #[test]
    fn tool_task_checkpoint_round_trips() {
        let execute = ToolTaskCheckpoint::Execute {
            arguments: serde_json::Map::from_iter([("x".to_string(), json!(1))]),
            replay: Replay::Safe,
        };
        let json = serde_json::to_value(&execute).expect("serialises");
        assert_eq!(json["phase"], json!("execute"));
        assert_eq!(json["replay"], json!("safe"));
        assert_eq!(
            serde_json::from_value::<ToolTaskCheckpoint>(json).expect("round trips"),
            execute
        );
    }

    #[test]
    fn harness_error_carries_an_error_diagnostic() {
        let result = harness_error("blocked", "Tool call blocked: nope");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.content, Some(Vec::new()));
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code.as_deref(), Some("blocked"));
        assert_eq!(
            result.diagnostics[0].severity,
            ToolDiagnosticSeverity::Error
        );
    }

    #[test]
    fn render_diagnostics_wraps_in_harness_tags() {
        let rendered = render_diagnostics(&[
            tool_diagnostic(ToolDiagnosticSeverity::Warn, "truncated", "cut"),
            tool_diagnostic(ToolDiagnosticSeverity::Error, "tool_error", "boom"),
        ]);
        assert_eq!(rendered, "<harness>\n[warn] cut\n[error] boom\n</harness>");
    }
}
