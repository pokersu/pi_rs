//! Rust 翻译自 packages/agent/src/harness/execution/tools.ts
//!
//! 工具执行的 phase 原语：准备、准入、执行、定稿。

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::FutureExt;
use pi_ai::{
    TextContent, TextKind, TextOrImageContent, ToolCall, ToolResultMessage, Usage,
    validate_tool_arguments,
};

use crate::harness::context::{Context, with_abort_signal};
use crate::harness::execution::effect_gate::{Gate, GateError};
use crate::harness::types::{
    AgentHarnessTool, AgentHarnessToolInvocation, AgentHarnessToolUpdateCallback,
    AgentHarnessToolUpdateOptions,
};
use crate::types::AgentToolResult;

/// 对应 `PreparedToolCall`：工具存在且参数校验通过。
#[derive(Clone)]
pub struct PreparedToolCall {
    pub tool_call: ToolCall,
    pub tool: AgentHarnessTool,
    pub args: serde_json::Value,
}

/// 对应 `ImmediateToolOutcome`：未跨外部工具效果边界的合成结果。
pub struct ImmediateToolOutcome {
    pub tool_call: ToolCall,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub terminate: bool,
}

/// 对应 `BeforeToolDecision`：before-tool hook 管线的聚合决策。
#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeToolDecision {
    pub args: Option<serde_json::Value>,
    pub block: Option<BeforeToolBlock>,
}

/// 对应 `BeforeToolDecision.block`。
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeToolBlock {
    pub reason: String,
    pub terminate: Option<bool>,
}

/// 对应 `ClearedToolCall`：已通过准入、准备发布 durable intent 并执行的调用。
#[derive(Clone)]
pub struct ClearedToolCall {
    pub tool_call: ToolCall,
    pub tool: AgentHarnessTool,
    pub args: serde_json::Value,
}

/// 对应 `ExecutedToolCall`：phase-two 工具原始输出（after-tool 打补丁前）。
pub struct ExecutedToolCall {
    pub result: AgentToolResult,
    pub is_error: bool,
}

/// 对应 `AfterToolPatch`：after-tool hook 管线的聚合补丁。
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AfterToolPatch {
    pub content: Option<Vec<TextOrImageContent>>,
    pub details: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}

/// 对应 `FinalizedToolCall`：准备成为 durable tool-result message 的最终输出。
pub struct FinalizedToolCall {
    pub tool_call: ToolCall,
    pub result: AgentToolResult,
    pub is_error: bool,
    pub terminate: bool,
}

/// `prepareToolCall` 的返回值：`PreparedToolCall | ImmediateToolOutcome`。
pub enum PreparedOrImmediate {
    Prepared(PreparedToolCall),
    Immediate(ImmediateToolOutcome),
}

/// `applyBeforeToolDecision` 的返回值：`ClearedToolCall | ImmediateToolOutcome`。
pub enum ClearedOrImmediate {
    Cleared(ClearedToolCall),
    Immediate(ImmediateToolOutcome),
}

/// 对应 `createErrorToolResult`。
fn create_error_tool_result(message: &str) -> AgentToolResult {
    AgentToolResult {
        content: vec![TextOrImageContent::Text(TextContent {
            kind: TextKind,
            text: message.to_string(),
            text_signature: None,
        })],
        details: serde_json::Value::Null,
        usage: None,
        added_tool_names: None,
        terminate: false,
    }
}

/// 对应 `immediateError`。
fn immediate_error(tool_call: &ToolCall, message: &str, terminate: bool) -> ImmediateToolOutcome {
    ImmediateToolOutcome {
        tool_call: tool_call.clone(),
        result: create_error_tool_result(message),
        is_error: true,
        terminate,
    }
}

/// 从 panic payload 提取错误消息。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Unknown tool error".to_string()
    }
}

/// 对应 `prepareToolCall`：解析工具、应用确定性参数准备、校验结果。
pub fn prepare_tool_call(call: &ToolCall, tools: &[AgentHarnessTool]) -> PreparedOrImmediate {
    let Some(tool) = tools
        .iter()
        .find(|candidate| candidate.tool.name == call.name)
    else {
        return PreparedOrImmediate::Immediate(immediate_error(
            call,
            &format!("Tool {} is unavailable", call.name),
            false,
        ));
    };

    let prepared_arguments = match &tool.prepare_arguments {
        Some(prepare) => prepare(call.arguments.clone()),
        None => call.arguments.clone(),
    };
    let prepared_call = if prepared_arguments == call.arguments {
        call.clone()
    } else {
        ToolCall {
            arguments: prepared_arguments,
            ..call.clone()
        }
    };

    match validate_tool_arguments(&tool.tool, &prepared_call) {
        Ok(args) => PreparedOrImmediate::Prepared(PreparedToolCall {
            tool_call: call.clone(),
            tool: tool.clone(),
            args,
        }),
        Err(error) => PreparedOrImmediate::Immediate(immediate_error(call, &error, false)),
    }
}

/// 对应 `applyBeforeToolDecision`：应用显式 hook 决策并重新校验替换参数。
pub fn apply_before_tool_decision(
    prepared: &PreparedToolCall,
    decision: Option<&BeforeToolDecision>,
) -> ClearedOrImmediate {
    if let Some(block) = decision.and_then(|d| d.block.as_ref()) {
        return ClearedOrImmediate::Immediate(immediate_error(
            &prepared.tool_call,
            &block.reason,
            block.terminate == Some(true),
        ));
    }

    let Some(args) = decision.and_then(|d| d.args.as_ref()) else {
        return ClearedOrImmediate::Cleared(ClearedToolCall {
            tool_call: prepared.tool_call.clone(),
            tool: prepared.tool.clone(),
            args: prepared.args.clone(),
        });
    };

    let replaced_call = ToolCall {
        arguments: args.clone(),
        ..prepared.tool_call.clone()
    };
    match validate_tool_arguments(&prepared.tool.tool, &replaced_call) {
        Ok(validated) => ClearedOrImmediate::Cleared(ClearedToolCall {
            tool_call: prepared.tool_call.clone(),
            tool: prepared.tool.clone(),
            args: validated,
        }),
        Err(error) => {
            ClearedOrImmediate::Immediate(immediate_error(&prepared.tool_call, &error, false))
        }
    }
}

/// 对应 `executeToolCall`：执行一个已准入的外部工具效果，将工具抛错转为错误输出。
pub async fn execute_tool_call(
    call: ClearedToolCall,
    gate: &Gate,
    on_update: AgentHarnessToolUpdateCallback,
    invocation: Arc<dyn AgentHarnessToolInvocation>,
    context: &Context,
) -> Result<ExecutedToolCall, GateError> {
    let accepting_updates = Arc::new(AtomicBool::new(true));
    let gate_signal = gate.signal.clone();
    let context = context.clone();

    let admitted = {
        let accepting_updates_inner = Arc::clone(&accepting_updates);
        let gate_signal = gate_signal.clone();
        let context = context.clone();
        let tool = call.tool;
        let tool_call_id = call.tool_call.id;
        let args = call.args;
        gate.admit(move || {
            let admitted_context = with_abort_signal(&gate_signal, &context);
            if let Some(signal) = admitted_context.abort_signal() {
                // 对应 `admittedContext.abortSignal?.throwIfAborted()`：同步抛 AbortError。
                signal.throw_if_aborted().expect("aborted");
            }
            let accepting_updates = accepting_updates_inner;
            async move {
                let result = (tool.execute)(
                    tool_call_id,
                    args,
                    Box::new(
                        move |partial: AgentToolResult,
                              options: Option<AgentHarnessToolUpdateOptions>| {
                            if accepting_updates.load(Ordering::SeqCst) {
                                on_update(partial, options);
                            }
                        },
                    ),
                    invocation,
                    admitted_context,
                )
                .await;
                ExecutedToolCall {
                    result,
                    is_error: false,
                }
            }
        })?
    };

    let executed = AssertUnwindSafe(admitted).catch_unwind().await;
    accepting_updates.store(false, Ordering::SeqCst);

    Ok(match executed {
        Ok(executed) => executed,
        Err(payload) => ExecutedToolCall {
            result: create_error_tool_result(&panic_message(&payload)),
            is_error: true,
        },
    })
}

/// 对应 `finalizeToolCall`：逐字段应用 after-tool 补丁。
pub fn finalize_tool_call(
    call: &ClearedToolCall,
    executed: ExecutedToolCall,
    patch: Option<AfterToolPatch>,
) -> FinalizedToolCall {
    let is_error = patch
        .as_ref()
        .and_then(|p| p.is_error)
        .unwrap_or(executed.is_error);
    let result = match patch {
        Some(p) => AgentToolResult {
            content: p.content.unwrap_or(executed.result.content),
            details: p.details.unwrap_or(executed.result.details),
            usage: p.usage.or(executed.result.usage),
            added_tool_names: executed.result.added_tool_names,
            terminate: p.terminate.unwrap_or(executed.result.terminate),
        },
        None => executed.result,
    };
    FinalizedToolCall {
        tool_call: call.tool_call.clone(),
        terminate: result.terminate,
        is_error,
        result,
    }
}

/// 对应 `toolResultFromMessage`：从 staged transcript message 重建规范工具结果。
pub fn tool_result_from_message(message: &ToolResultMessage, terminate: bool) -> AgentToolResult {
    AgentToolResult {
        content: message.content.clone(),
        details: message.details.clone().unwrap_or(serde_json::Value::Null),
        usage: message.usage.clone(),
        added_tool_names: message.added_tool_names.clone(),
        terminate,
    }
}

/// 对应 `createToolResultMessage`：将定稿输出转换为 provider 面向的 transcript message。
pub fn create_tool_result_message(call: &FinalizedToolCall) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: call.tool_call.id.clone(),
        tool_name: call.tool_call.name.clone(),
        content: call.result.content.clone(),
        details: if call.result.details.is_null() {
            None
        } else {
            Some(call.result.details.clone())
        },
        usage: call.result.usage.clone(),
        added_tool_names: match &call.result.added_tool_names {
            Some(names) if !names.is_empty() => Some(names.clone()),
            _ => None,
        },
        is_error: call.is_error,
        timestamp: pi_ai::utils::uuid::now_ms() as u64,
    }
}
