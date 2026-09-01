//! Rust 翻译自 packages/ai/src/utils/retry.ts
//!
//! 对有界重试的 assistant 调用进行瞬时错误分类与退避重试。

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use regex::{Regex, RegexBuilder};

use crate::types::{AbortSignal, AssistantMessage, StopReason};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 对应 `RetryPolicy`：有界尝试 + 指数退避（`baseDelayMs * 2^(attempt-1)`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// 最大重试次数（0 = 不重试）。首次调用不计入重试。
    pub max_retries: u64,
    /// 基础延迟毫秒。每次尝试延迟为 `baseDelayMs * 2^(attempt-1)`（抖动前）。
    pub base_delay_ms: u64,
}

/// 对应 `RetryCallbacks`：每次重试周围发出的可选回调。
#[derive(Default)]
pub struct RetryCallbacks {
    /// 每次重试的退避睡眠前发出（attempt 从 1 开始）。
    pub on_retry_scheduled: Option<RetryScheduledCallback>,
    /// 退避睡眠后、重试调用开始前发出。
    pub on_retry_attempt_start: Option<RetryAttemptStartCallback>,
    /// 循环结束时发出一次。
    pub on_retry_finished: Option<RetryFinishedCallback>,
}

type RetryScheduledCallback = Box<dyn Fn(u64, u64, u64, String) -> BoxFuture<()> + Send + Sync>;
type RetryAttemptStartCallback = Box<dyn Fn() -> BoxFuture<()> + Send + Sync>;
type RetryFinishedCallback = Box<dyn Fn(bool, u64, Option<String>) -> BoxFuture<()> + Send + Sync>;

fn build_provider_error_pattern(patterns: &[&str]) -> Regex {
    RegexBuilder::new(&patterns.join("|"))
        .case_insensitive(true)
        .build()
        .expect("retry error pattern must compile")
}

const NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERNS: &[&str] = &[
    // OpenCode Go/free-tier limits returned as 429 JSON error types.
    "GoUsageLimitError",
    "FreeUsageLimitError",
    // OpenCode Go subscription-limit text.
    "Monthly usage limit reached",
    "available balance",
    // Generic quota/budget/billing exhaustion.
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
];

const RETRYABLE_PROVIDER_ERROR_PATTERNS: &[&str] = &[
    // Generic provider load, HTTP status, and server-side transient failures.
    "overloaded",
    "rate.?limit",
    "too many requests",
    "429",
    "500",
    "502",
    "503",
    "504",
    "524",
    "service.?unavailable",
    "server.?error",
    "internal.?error",
    // Wrapper/provider text for transient upstream failures.
    "provider.?returned.?error",
    "exceeded request buffer limit while retrying upstream",
    // Network, proxy, and fetch transport failures.
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
    "getaddrinfo",
    "ENOTFOUND",
    "EAI_AGAIN",
    "upstream.?connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed? out",
    "timeout",
    "terminated",
    // WebSocket transports.
    "websocket.?closed",
    "websocket.?error",
    // Premature stream endings from SDKs and transports.
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    // Provider-requested retry delay cap failures.
    "retry delay",
    // Explicit retry guidance emitted mid-stream.
    "you can retry your request",
    "try your request again",
    "please retry your request",
    // gRPC based providers.
    "ResourceExhausted",
];

fn non_retryable_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| build_provider_error_pattern(NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERNS))
}

fn retryable_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| build_provider_error_pattern(RETRYABLE_PROVIDER_ERROR_PATTERNS))
}

struct RetrySleepAbortError;

async fn sleep(ms: u64, signal: Option<&AbortSignal>) -> Result<(), RetrySleepAbortError> {
    match signal {
        Some(signal) => {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
                _ = signal.cancelled() => Err(RetrySleepAbortError),
            }
        }
        None => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(())
        }
    }
}

/// 对应 `retryAssistantCall`：在瞬时错误上运行一次带界重试的 assistant 调用。
///
/// - 成功响应立即返回。abort 是终态、永不重试，但若发生在重试已排程后则报告为不成功。
/// - 不可重试错误（含配额/计费耗尽）立即返回，确定性错误快速失败。
/// - 否则最多重试 `maxRetries` 次，指数退避。
///
/// 当 `policy` 为 `None` 或禁用时，直接返回首次响应（等价于直接调用 `produce`）。
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: Option<&RetryPolicy>,
    signal: Option<&AbortSignal>,
    callbacks: Option<&RetryCallbacks>,
) -> AssistantMessage
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AssistantMessage>,
{
    let max_attempts = policy
        .filter(|p| p.enabled)
        .map(|p| p.max_retries)
        .unwrap_or(0);

    let mut attempt: u64 = 0;
    let mut last_retry: Option<(u64, String)> = None;
    loop {
        let response = produce().await;

        // Abort：终态但不成功。绝不重试 aborted 消息。
        if response.stop_reason == StopReason::Aborted {
            if let Some((attempt, _)) = last_retry {
                call_retry_finished(callbacks, false, attempt, None).await;
            }
            return response;
        }

        // 成功：非 error、非 abort 的响应原样返回。
        if response.stop_reason != StopReason::Error {
            if let Some((attempt, _)) = last_retry {
                call_retry_finished(callbacks, true, attempt, None).await;
            }
            return response;
        }

        // 不可重试，或预算耗尽：返回最终错误消息。
        if attempt >= max_attempts || !is_retryable_assistant_error(&response) {
            if let Some((attempt, _)) = last_retry {
                call_retry_finished(callbacks, false, attempt, response.error_message.clone())
                    .await;
            }
            return response;
        }

        attempt += 1;
        let error_message = response
            .error_message
            .clone()
            .unwrap_or_else(|| "Unknown error".to_string());
        last_retry = Some((attempt, error_message.clone()));
        let shift = (attempt - 1).min(63) as u32;
        let delay_ms = policy
            .map(|p| p.base_delay_ms.saturating_mul(1u64 << shift))
            .unwrap_or(0);
        if let Some(callback) = callbacks.and_then(|c| c.on_retry_scheduled.as_ref()) {
            callback(attempt, max_attempts, delay_ms, error_message.clone()).await;
        }

        // 退避期间 abort 归一化为 aborted AssistantMessage。
        if sleep(delay_ms, signal).await.is_err() {
            let (attempt, error_message) = last_retry.take().unwrap();
            call_retry_finished(callbacks, false, attempt, Some(error_message)).await;
            return aborted_message(&response);
        }
        if let Some(callback) = callbacks.and_then(|c| c.on_retry_attempt_start.as_ref()) {
            callback().await;
        }
    }
}

async fn call_retry_finished(
    callbacks: Option<&RetryCallbacks>,
    success: bool,
    attempt: u64,
    final_error: Option<String>,
) {
    if let Some(callback) = callbacks.and_then(|c| c.on_retry_finished.as_ref()) {
        callback(success, attempt, final_error).await;
    }
}

fn aborted_message(response: &AssistantMessage) -> AssistantMessage {
    let mut message = response.clone();
    message.stop_reason = StopReason::Aborted;
    message.error_message = None;
    message
}

/// 对应 `isRetryableAssistantError`：判断失败的 assistant 消息是否像瞬时 provider/transport 错误。
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = &message.error_message else {
        return false;
    };
    if non_retryable_re().is_match(error_message) {
        return false;
    }
    retryable_re().is_match(error_message)
}
