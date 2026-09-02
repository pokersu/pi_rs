//! Rust 翻译自 packages/ai/src/utils/provider-retry.ts
//!
//! 复刻 OpenAI/Anthropic SDK 的 provider 请求重试策略，退避睡眠可被中断。

use std::time::Duration;

use reqwest::header::HeaderMap;

use crate::types::AbortSignal;

pub const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

/// 对应 `ProviderRetryOptions`
#[derive(Debug, Clone, Default)]
pub struct ProviderRetryOptions {
    pub max_retries: u64,
    pub max_retry_delay_ms: Option<u64>,
    pub signal: Option<AbortSignal>,
}

/// 对应 `ProviderError`（携带 HTTP status 与 headers 的 provider 错误）。
#[derive(Debug, Clone)]
pub struct ProviderError {
    pub status: Option<u16>,
    pub headers: HeaderMap,
    pub message: String,
}

/// 对应 `retryProviderRequest` 的失败结果。
///
/// 原版用 `isProviderError` 区分「可重试分类的 provider 错误」与「其他错误」；Rust 用
/// 枚举变体表达这一区分。
#[derive(Debug, Clone)]
pub enum ProviderRequestError {
    Provider(ProviderError),
    Other(String),
}

/// 对应 `isRetryableProviderError(error)`。
pub fn is_retryable_provider_error(error: &ProviderError) -> bool {
    if let Some(should_retry) = error
        .headers
        .get("x-should-retry")
        .and_then(|v| v.to_str().ok())
    {
        if should_retry == "true" {
            return true;
        }
        if should_retry == "false" {
            return false;
        }
    }

    match error.status {
        None => true,
        Some(408 | 409 | 429) => true,
        Some(status) => status >= 500,
    }
}

fn validate_server_retry_delay_ms(
    delay_ms: f64,
    max_retry_delay_ms: Option<u64>,
    provider_error_message: &str,
) -> Result<u64, String> {
    let max_delay_ms = max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max_delay_ms > 0 && delay_ms > max_delay_ms as f64 {
        return Err(format!(
            "Server requested {}s retry delay (max: {}s). {provider_error_message}",
            (delay_ms / 1000.0).ceil() as u64,
            (max_delay_ms as f64 / 1000.0).ceil() as u64,
        ));
    }
    Ok(delay_ms as u64)
}

/// 对应 `getRetryDelayMs(error, retryIndex, maxRetryDelayMs)`。
pub fn get_retry_delay_ms(
    error: &ProviderError,
    retry_index: u64,
    max_retry_delay_ms: Option<u64>,
) -> Result<u64, String> {
    if let Some(retry_after_ms) = error
        .headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        && let Ok(value) = retry_after_ms.parse::<f64>()
    {
        return validate_server_retry_delay_ms(value, max_retry_delay_ms, &error.message);
    }

    if let Some(retry_after) = error
        .headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        && let Ok(seconds) = retry_after.parse::<f64>()
    {
        let delay_ms = seconds * 1000.0;
        return validate_server_retry_delay_ms(delay_ms, max_retry_delay_ms, &error.message);
    }
    // HTTP-date 形式的 `retry-after` 需要日期解析，Rust 无内置等价物；此处忽略该
    // header，走指数退避（原版用 `Date.parse`，属合理简化）。

    let exponential_delay = (0.5 * 2f64.powi(retry_index.min(4) as i32)) * 1000.0;
    let jitter = 1.0 - rand::random::<f64>() * 0.25;
    Ok((exponential_delay * jitter) as u64)
}

/// 对应 `retryProviderRequest(request, options)`。
pub async fn retry_provider_request<T, F, Fut>(
    mut request: F,
    options: &ProviderRetryOptions,
) -> Result<T, ProviderRequestError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderRequestError>>,
{
    let max_retries = options.max_retries;
    let mut retries_remaining = max_retries;

    loop {
        match request().await {
            Ok(value) => return Ok(value),
            Err(ProviderRequestError::Other(message)) => {
                return Err(ProviderRequestError::Other(message));
            }
            Err(ProviderRequestError::Provider(error)) => {
                if let Some(signal) = &options.signal
                    && signal.aborted()
                {
                    return Err(ProviderRequestError::Other("Request aborted".to_string()));
                }
                if retries_remaining == 0 || !is_retryable_provider_error(&error) {
                    return Err(ProviderRequestError::Provider(error));
                }

                let retry_index = max_retries - retries_remaining;
                retries_remaining -= 1;
                let delay_ms =
                    match get_retry_delay_ms(&error, retry_index, options.max_retry_delay_ms) {
                        Ok(delay_ms) => delay_ms,
                        Err(message) => return Err(ProviderRequestError::Other(message)),
                    };

                match &options.signal {
                    Some(signal) => {
                        if crate::utils::sleep::sleep(delay_ms, signal).await.is_err() {
                            return Err(ProviderRequestError::Other("Request aborted".to_string()));
                        }
                    }
                    None => {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                }
            }
        }
    }
}
