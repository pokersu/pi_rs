//! Rust 翻译自 packages/agent/src/harness/runtime/drive/retry.ts

use std::time::Duration;

use pi_ai::AbortSignal;

/// 对应 `retryDelay`。
pub fn retry_delay(base_delay_ms: u64, attempt: u32) -> u64 {
    base_delay_ms.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)))
}

/// 对应 `retryNotBefore`。
pub fn retry_not_before(base_delay_ms: u64, attempt: u32, now: u64) -> u64 {
    now.saturating_add(retry_delay(base_delay_ms, attempt))
}

/// 对应 `waitUntil`：等待到 `not_before` 或 abort。
pub async fn wait_until(not_before: u64, signal: &AbortSignal) -> Result<(), ()> {
    let now = pi_ai::utils::uuid::now_ms() as u64;
    if not_before <= now {
        return Ok(());
    }
    tokio::select! {
        _ = signal.cancelled() => Err(()),
        _ = tokio::time::sleep(Duration::from_millis(not_before - now)) => Ok(()),
    }
}
