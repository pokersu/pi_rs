//! Rust 翻译自 packages/telemetry/src/noop.ts

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::{
    ErasedSpanCallback, ErasedSpanFuture, SpanAttributes, SpanOptions, SpanStatus,
    TelemetryContext, TelemetrySpan,
};

/// 对应 TS 的 `noopTelemetrySpan`：实现 `TelemetrySpan` 的空操作单例。
#[derive(Debug, Default)]
pub struct NoopTelemetrySpan;

impl TelemetryContext for NoopTelemetrySpan {
    fn start_span<'a, F, Fut, T>(
        &'a self,
        _options: SpanOptions,
        callback: F,
    ) -> Pin<Box<dyn Future<Output = T> + Send + 'a>>
    where
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut + Send + 'a,
        Fut: Future<Output = T> + Send + 'a,
        T: Send + 'a,
    {
        // 对应 TS 的 startNoopSpan：直接以 noop span 调用 callback。
        Box::pin(async move { callback(Arc::new(NoopTelemetrySpan)).await })
    }
}

impl TelemetrySpan for NoopTelemetrySpan {
    fn add_event(&self, _name: &str, _attributes: Option<SpanAttributes>) {}

    fn set_attributes(&self, _attributes: SpanAttributes) {}

    fn set_status(&self, _status: SpanStatus) {}

    fn start_child_span<'a>(
        &'a self,
        _options: SpanOptions,
        callback: ErasedSpanCallback<'a>,
    ) -> ErasedSpanFuture<'a> {
        Box::pin(async move { callback(Arc::new(NoopTelemetrySpan)).await })
    }
}

/// 对应 TS 的 `NOOP_TELEMETRY_CONTEXT`：应用未提供 telemetry 上下文时共享的空操作单例。
pub static NOOP_TELEMETRY_CONTEXT: NoopTelemetrySpan = NoopTelemetrySpan;
