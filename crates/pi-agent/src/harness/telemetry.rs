//! Rust 翻译自 packages/agent/src/harness/telemetry.ts
//!
//! agent 的 telemetry schema 定义与便捷 span 启动函数。TS 中 schema 仅用于编译期
//! 类型推断与文档生成；Rust 中以 `serde_json::Value` 保留 schema 数据，`start_ai_span`/
//! `start_harness_span` 委托底层 `TelemetryContext::start_span`。

use std::future::Future;
use std::sync::Arc;

use pi_telemetry::{SpanAttributes, SpanOptions, TelemetryContext, TelemetrySpan};

/// 对应 `HOOK_NAMES`
pub const HOOK_NAMES: [&str; 11] = [
    "before_run",
    "before_resume",
    "before_run_end",
    "transform_context",
    "before_request",
    "before_payload",
    "after_response",
    "before_tool",
    "after_tool",
    "before_compaction",
    "before_navigation",
];

/// 对应 `EVENT_TYPES`
pub const EVENT_TYPES: [&str; 29] = [
    "run_start",
    "run_resume",
    "run_suspend",
    "run_abort",
    "run_end",
    "fault",
    "handler_error",
    "turn_start",
    "turn_end",
    "retry_scheduled",
    "retry_start",
    "retry_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_start",
    "tool_update",
    "tool_end",
    "entry_added",
    "write_pending",
    "queue_update",
    "fact_update",
    "config_update",
    "compaction_start",
    "compaction_end",
    "navigation_start",
    "navigation_end",
    "lane_created",
    "usage",
];

/// 对应 `startAiSpan`
pub async fn start_ai_span<C, F, Fut, T>(
    telemetry_context: &C,
    name: &str,
    attributes: SpanAttributes,
    callback: F,
) -> T
where
    C: TelemetryContext,
    F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send,
    Fut: Future<Output = T> + Send,
    T: Send,
{
    telemetry_context
        .start_span(
            SpanOptions {
                name: name.to_string(),
                attributes: Some(attributes),
            },
            callback,
        )
        .await
}

/// 对应 `startHarnessSpan`
pub async fn start_harness_span<C, F, Fut, T>(
    telemetry_context: &C,
    name: &str,
    attributes: SpanAttributes,
    callback: F,
) -> T
where
    C: TelemetryContext,
    F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send,
    Fut: Future<Output = T> + Send,
    T: Send,
{
    telemetry_context
        .start_span(
            SpanOptions {
                name: name.to_string(),
                attributes: Some(attributes),
            },
            callback,
        )
        .await
}
