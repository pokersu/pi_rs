//! Rust 翻译自 packages/ai/src/utils/error-body.ts
//!
//! 统一归一化 provider HTTP 错误对象。

pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;

/// 对应 `NormalizedProviderError`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedProviderError {
    /// HTTP 状态码（可从错误对象中提取到时）。
    pub status: Option<u16>,
    /// 原始 HTTP body，已 trim 并截断到上限。
    pub body: Option<String>,
    /// `error.message`，或对非 `Error` 抛出的 `safeJsonStringify(error)`。
    pub message: String,
    /// 当 `message` 已包含 body（无需单独追加 body）时为 true。
    pub message_carries_body: bool,
}

/// 对应 `normalizeProviderError(error)`。
///
/// 原版探测多个 JS SDK 的错误字段形状（`statusCode`/`status`/`$metadata`/`$response`）；
/// Rust 统一使用 `reqwest`，status 与 body 已由 HTTP 响应直接提供，故把探测步骤简化为
/// 显式传入，其余（trim/截断/`messageCarriesBody` 判断）逻辑保持一致。
pub fn normalize_provider_error(
    status: Option<u16>,
    body: Option<String>,
    message: String,
) -> NormalizedProviderError {
    let body = body.and_then(|body| {
        let trimmed = body.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_error_text(&trimmed, MAX_PROVIDER_ERROR_BODY_CHARS))
        }
    });
    let message_carries_body = match &body {
        None => true,
        Some(body) => message.contains(body),
    };
    NormalizedProviderError {
        status,
        body,
        message,
        message_carries_body,
    }
}

/// 对应 `formatProviderError(norm, prefix?)`。
pub fn format_provider_error(norm: &NormalizedProviderError, prefix: Option<&str>) -> String {
    if norm.message_carries_body || norm.status.is_none() || norm.body.is_none() {
        return match (prefix, norm.status) {
            (Some(prefix), Some(status)) => format!("{prefix} ({status}): {}", norm.message),
            _ => norm.message.clone(),
        };
    }
    let status = norm.status.unwrap();
    let body = norm.body.as_deref().unwrap();
    match prefix {
        Some(prefix) => format!("{prefix} ({status}): {body}"),
        None => format!("{status}: {body}"),
    }
}

/// 对应 `truncateErrorText(text, maxChars)`。
pub fn truncate_error_text(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}... [truncated {} chars]", total - max_chars)
}

/// 对应 `safeJsonStringify(value)`。
pub fn safe_json_stringify<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_string())
}
