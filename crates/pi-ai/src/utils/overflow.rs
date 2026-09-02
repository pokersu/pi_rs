//! Rust 翻译自 packages/ai/src/utils/overflow.ts
//!
//! 检测各 provider 的 context overflow 错误。

use std::sync::OnceLock;

use regex::{Regex, RegexBuilder};

use crate::types::{AssistantMessage, StopReason};

const OVERFLOW_PATTERNS: &[&str] = &[
    r"prompt is too long",
    r"request_too_large",
    r"input is too long for requested model",
    r"exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"input token count.*exceeds the maximum",
    r"maximum prompt length is \d+",
    r"reduce the length of the messages",
    r"maximum context length is \d+ tokens",
    r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
    r"exceeds the limit of \d+",
    r"exceeds the available context size",
    r"greater than the context length",
    r"context window exceeds limit",
    r"exceeded model token limit",
    r"too large for model with \d+ maximum context length",
    r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    r"model_context_window_exceeded",
    r"prompt too long; exceeded (?:max )?context length",
    r"range of input length should be",
    r"context[_ ]length[_ ]exceeded",
    r"too many tokens",
    r"token limit exceeded",
    r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)",
];

const NON_OVERFLOW_PATTERNS: &[&str] = &[
    r"^(Throttling error|Service unavailable)",
    r"rate limit",
    r"too many requests",
];

fn build_case_insensitive(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .expect("overflow pattern must compile")
}

fn overflow_regexes() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        OVERFLOW_PATTERNS
            .iter()
            .map(|p| build_case_insensitive(p))
            .collect()
    })
}

fn non_overflow_regexes() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        NON_OVERFLOW_PATTERNS
            .iter()
            .map(|p| build_case_insensitive(p))
            .collect()
    })
}

/// 对应 `isContextOverflow(message, contextWindow?)`：判断 assistant 消息是否指示 context overflow。
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    // Case 1：检查错误消息模式。
    if message.stop_reason == StopReason::Error
        && let Some(error_message) = &message.error_message
    {
        let is_non_overflow = non_overflow_regexes()
            .iter()
            .any(|p| p.is_match(error_message));
        if !is_non_overflow && overflow_regexes().iter().any(|p| p.is_match(error_message)) {
            return true;
        }
    }

    if let Some(context_window) = context_window {
        // Case 2：静默 overflow（z.ai 风格）——成功但 usage 超过 context。
        if message.stop_reason == StopReason::Stop {
            let input_tokens = message.usage.input + message.usage.cache_read;
            if input_tokens > context_window {
                return true;
            }
        }

        // Case 3：length-stop overflow（Xiaomi MiMo 风格）——输入填满 context，输出为 0。
        if message.stop_reason == StopReason::Length && message.usage.output == 0 {
            let input_tokens = message.usage.input + message.usage.cache_read;
            if input_tokens as f64 >= context_window as f64 * 0.99 {
                return true;
            }
        }
    }

    false
}

/// 对应 `isRecoverableLength(message, desiredMaxOutput)`。
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && message.usage.output < desired_max_output
}

/// 对应 `getOverflowPatterns()`（测试用途）。
pub fn get_overflow_patterns() -> Vec<Regex> {
    overflow_regexes().to_vec()
}
