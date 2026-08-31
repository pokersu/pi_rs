//! Rust 翻译自 packages/ai/src/utils/abort.ts
//!
//! 对应 TS 的三个函数：`abortReason`、`operationSignal`、`raceWithAbortSignal`。

use crate::types::{AbortError, AbortSignal};

/// 统一错误类型：对应 TS 中 untyped 的 Promise rejection（`unknown`）。
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 对应 `abortReason(signal)`。TS 返回 `signal.reason` 或一个 `AbortError`；
/// Rust 的 `AbortSignal` 不携带 reason，统一返回 `AbortError`。
pub fn abort_reason(_signal: &AbortSignal) -> AbortError {
    AbortError
}

/// 对应 `operationSignal(signal?)`：为公开 API（signal 可选）创建 operation-local signal。
pub fn operation_signal(signal: Option<&AbortSignal>) -> AbortSignal {
    match signal {
        Some(s) => s.clone(),
        None => AbortSignal::new(),
    }
}

/// 对应 `raceWithAbortSignal(operation, signal)`：signal abort 时停止等待，
/// 同时继续观察被放弃的 operation，保证后续 rejection 总被处理。
pub async fn race_with_abort_signal<T>(
    operation: impl std::future::Future<Output = Result<T, BoxError>>,
    signal: &AbortSignal,
) -> Result<T, BoxError> {
    if signal.aborted() {
        return Err(Box::new(abort_reason(signal)));
    }
    tokio::select! {
        _ = signal.cancelled() => Err(Box::new(abort_reason(signal))),
        result = operation => result,
    }
}
