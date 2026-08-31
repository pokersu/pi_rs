//! Rust 翻译自 packages/ai/src/auth/oauth/device-code.ts
//!
//! RFC 8628 设备授权轮询流程。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::types::AbortSignal;
use crate::utils::abort::BoxError;

const CANCEL_MESSAGE: &str = "Login cancelled";
const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
// RFC 8628 section 3.2：授权服务器省略 `interval` 时客户端必须使用 5 秒。
const MINIMUM_INTERVAL_MS: u64 = 1000;
const DEFAULT_POLL_INTERVAL_SECONDS: u64 = 5;
// RFC 8628 section 3.5：`slow_down` 意味着轮询间隔须增加 5 秒。
const SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5000;

/// 对应 `OAuthDeviceCodePollResult<T>`。
pub enum OAuthDeviceCodePollResult<T> {
    Pending,
    SlowDown { interval_seconds: Option<u64> },
    Failed { message: String },
    Complete { value: T },
}

/// 对应 `OAuthDeviceCodePollOptions<T>`。
/// 对应 `OAuthDeviceCodePollOptions<T>.poll`。
pub type OAuthDeviceCodePollFn<T> = Box<
    dyn Fn()
        -> Pin<Box<dyn Future<Output = Result<OAuthDeviceCodePollResult<T>, BoxError>> + Send>>
        + Send,
>;

pub struct OAuthDeviceCodePollOptions<T> {
    pub interval_seconds: Option<u64>,
    pub expires_in_seconds: Option<u64>,
    pub wait_before_first_poll: Option<bool>,
    pub poll: OAuthDeviceCodePollFn<T>,
    pub signal: AbortSignal,
}

fn now_ms() -> u64 {
    crate::utils::uuid::now_ms() as u64
}

/// 对应 `abortableSleep(ms, signal, cancelMessage)`。
pub async fn abortable_sleep(
    ms: u64,
    signal: &AbortSignal,
    cancel_message: &str,
) -> Result<(), BoxError> {
    if signal.aborted() {
        return Err(cancel_message.to_string().into());
    }
    tokio::select! {
        _ = signal.cancelled() => Err(cancel_message.to_string().into()),
        _ = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
    }
}

/// 对应 `pollOAuthDeviceCodeFlow<T>(options)`。
pub async fn poll_oauth_device_code_flow<T>(
    options: OAuthDeviceCodePollOptions<T>,
) -> Result<T, BoxError> {
    let deadline_ms = options
        .expires_in_seconds
        .map(|s| now_ms().saturating_add(s.saturating_mul(1000)))
        .unwrap_or(u64::MAX);
    let mut interval_ms = MINIMUM_INTERVAL_MS
        .max(options.interval_seconds.unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS).saturating_mul(1000));

    let mut slow_down_responses = 0u64;
    if options.wait_before_first_poll.unwrap_or(false) {
        let remaining_ms = deadline_ms.saturating_sub(now_ms());
        if remaining_ms > 0 {
            abortable_sleep(interval_ms.min(remaining_ms), &options.signal, CANCEL_MESSAGE).await?;
        }
    }

    while now_ms() < deadline_ms {
        if options.signal.aborted() {
            return Err(CANCEL_MESSAGE.to_string().into());
        }

        let result = (options.poll)().await?;
        match result {
            OAuthDeviceCodePollResult::Complete { value } => return Ok(value),
            OAuthDeviceCodePollResult::Failed { message } => return Err(message.into()),
            OAuthDeviceCodePollResult::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                // 使用服务器提供的 interval（GitHub 在新要求的最小值里报告 `interval`）；
                // 只信任客户端记录的值会在 WSL/VM 时钟漂移下过早轮询。否则按 RFC 8628
                // section 3.5 增加 5 秒。
                interval_ms = match interval_seconds {
                    Some(seconds) if seconds > 0 => {
                        MINIMUM_INTERVAL_MS.max(seconds.saturating_mul(1000))
                    }
                    _ => MINIMUM_INTERVAL_MS.max(interval_ms + SLOW_DOWN_INTERVAL_INCREMENT_MS),
                };
            }
            OAuthDeviceCodePollResult::Pending => {}
        }

        let remaining_ms = deadline_ms.saturating_sub(now_ms());
        if remaining_ms == 0 {
            break;
        }

        abortable_sleep(interval_ms.min(remaining_ms), &options.signal, CANCEL_MESSAGE).await?;
    }

    Err(if slow_down_responses > 0 {
        SLOW_DOWN_TIMEOUT_MESSAGE
    } else {
        TIMEOUT_MESSAGE
    }
    .to_string()
    .into())
}
