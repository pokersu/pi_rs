//! 错误流辅助（对应 TS 的 `lazyStream` + `createErrorMessage` 的失败路径）。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::{
    AssistantMessage, AssistantMessageEvent, ErrorStopReason, Model, StopReason, Usage, UsageCost,
};
use crate::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 对应 `DEFAULT_USAGE`（零值 usage）。
pub fn default_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: UsageCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    }
}

/// 对应 `createErrorMessage`
pub fn create_error_message(
    error: &str,
    api: &str,
    provider: &str,
    model_id: &str,
) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: api.to_string(),
        provider: provider.to_string(),
        model: model_id.to_string(),
        response_model: None,
        response_id: None,
        usage: default_usage(),
        stop_reason: StopReason::Error,
        deferred: None,
        error_message: Some(error.to_string()),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
    }
}

/// 构造一个立即编码错误的流（对应 TS 中 dispatch 失败时的 lazyStream 错误）。
pub fn stream_error(model: &Model, message: String) -> AssistantMessageEventStream {
    let stream = create_assistant_message_event_stream();
    let error = create_error_message(&message, &model.api, &model.provider, &model.id);
    stream.push(AssistantMessageEvent::Error {
        reason: ErrorStopReason::Error,
        error: error.clone(),
    });
    stream.end(Some(error));
    stream
}
