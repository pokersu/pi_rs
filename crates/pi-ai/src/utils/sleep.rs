//! Rust 翻译自 packages/ai/src/utils/sleep.ts
//!
//! 可中断的睡眠：abort 时提前返回。

use std::time::Duration;

use crate::types::{AbortError, AbortSignal};

/// 对应 `sleep(ms, signal)`：睡眠 `ms` 毫秒，signal abort 时返回 `Err(AbortError)`。
pub async fn sleep(ms: u64, signal: &AbortSignal) -> Result<(), AbortError> {
    if signal.aborted() {
        return Err(AbortError);
    }
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
        _ = signal.cancelled() => Err(AbortError),
    }
}
