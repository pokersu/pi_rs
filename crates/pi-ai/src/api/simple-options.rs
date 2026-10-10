//! Rust 翻译自 packages/ai/src/api/simple-options.ts
//!
//! 构建 provider 基础流式选项（max-tokens 收缩 + thinking 预算）。

use crate::types::{
    Context, Model, ModelThinkingLevel, ProviderRequestOptions, SamplingParams,
    SimpleStreamOptions, StreamOptions, ThinkingBudgets, ThinkingLevel,
};

const CONTEXT_SAFETY_TOKENS: u64 = 4096;
const MIN_MAX_TOKENS: u64 = 1;

/// 对应 `clampMaxTokensToContext(model, context, maxTokens)`
pub fn clamp_max_tokens_to_context(model: &Model, context: &Context, max_tokens: u64) -> u64 {
    if model.context_window == 0 {
        return MIN_MAX_TOKENS.max(max_tokens);
    }
    let estimate = crate::utils::estimate::estimate_context_tokens(
        crate::utils::estimate::ContextInput::Context(context),
    )
    .tokens;
    let available = model
        .context_window
        .saturating_sub(estimate)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    max_tokens.min(MIN_MAX_TOKENS.max(available))
}

fn model_thinking_level(reasoning: Option<ThinkingLevel>) -> ModelThinkingLevel {
    match reasoning {
        None => ModelThinkingLevel::Off,
        Some(level) => match level {
            ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
            ThinkingLevel::Low => ModelThinkingLevel::Low,
            ThinkingLevel::Medium => ModelThinkingLevel::Medium,
            ThinkingLevel::High => ModelThinkingLevel::High,
            ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
            ThinkingLevel::Max => ModelThinkingLevel::Max,
        },
    }
}

/// 对应 `resolveSamplingParams(model, thinkingLevel, requestParams?)`：
/// 按有效 thinking level 合并模型默认、分层覆盖与请求参数。
pub fn resolve_sampling_params(
    model: &Model,
    thinking_level: ModelThinkingLevel,
    request_params: Option<&SamplingParams>,
) -> Option<SamplingParams> {
    let effective = crate::models::clamp_thinking_level(model, thinking_level);
    let thinking_level_params = model
        .sampling_params_by_thinking_level
        .as_ref()
        .and_then(|map| map.get(&effective));
    let has_any = model.sampling_params.is_some()
        || thinking_level_params.is_some()
        || request_params.is_some();
    if !has_any {
        return None;
    }
    let mut merged = serde_json::Map::new();
    for params in [
        model.sampling_params.as_ref(),
        thinking_level_params,
        request_params,
    ]
    .into_iter()
    .flatten()
    {
        if let Some(object) = params.as_object() {
            merged.extend(object.clone());
        }
    }
    Some(serde_json::Value::Object(merged))
}

/// 对应 `buildBaseOptions(model, context, options?, apiKey?)`
pub fn build_base_options(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
    api_key: Option<&str>,
) -> StreamOptions {
    let sampling_params = resolve_sampling_params(
        model,
        model_thinking_level(options.and_then(|o| o.reasoning)),
        options.and_then(|o| o.stream.sampling_params.as_ref()),
    );

    StreamOptions {
        request: ProviderRequestOptions {
            signal: options.and_then(|o| o.stream.request.signal.clone()),
            api_key: api_key
                .map(|k| k.to_string())
                .or_else(|| options.and_then(|o| o.stream.request.api_key.clone())),
            headers: options.and_then(|o| o.stream.request.headers.clone()),
            timeout_ms: options.and_then(|o| o.stream.request.timeout_ms),
            max_retries: options.and_then(|o| o.stream.request.max_retries),
            max_retry_delay_ms: options.and_then(|o| o.stream.request.max_retry_delay_ms),
            on_payload: options.and_then(|o| o.stream.request.on_payload.clone()),
            on_response: options.and_then(|o| o.stream.request.on_response.clone()),
            env: options.and_then(|o| o.stream.request.env.clone()),
        },
        temperature: options.and_then(|o| o.stream.temperature),
        sampling_params,
        max_tokens: Some(clamp_max_tokens_to_context(
            model,
            context,
            options
                .and_then(|o| o.stream.max_tokens)
                .unwrap_or(model.max_tokens),
        )),
        transport: options.and_then(|o| o.stream.transport),
        cache_retention: options.and_then(|o| o.stream.cache_retention),
        session_id: options.and_then(|o| o.stream.session_id.clone()),
        websocket_connect_timeout_ms: options.and_then(|o| o.stream.websocket_connect_timeout_ms),
        metadata: options.and_then(|o| o.stream.metadata.clone()),
        on_provider_stream_event: options.and_then(|o| o.stream.on_provider_stream_event.clone()),
    }
}

/// 对应 `MIN_ANSWER_TOKENS`：thinking 预算与响应共享上限时，始终为回答保留的 tokens。
pub const MIN_ANSWER_TOKENS: u64 = 1024;

/// 对应 `DEFAULT_THINKING_BUDGETS`
pub fn default_thinking_budgets() -> ThinkingBudgets {
    ThinkingBudgets {
        minimal: Some(1024),
        low: Some(2048),
        medium: Some(8192),
        high: Some(16384),
    }
}

/// 对应 `clampReasoning(effort)`
pub fn clamp_reasoning(effort: Option<ThinkingLevel>) -> Option<ThinkingLevel> {
    match effort {
        Some(ThinkingLevel::Xhigh) | Some(ThinkingLevel::Max) => Some(ThinkingLevel::High),
        other => other,
    }
}

/// 对应 `thinkingBudgetForLevel(reasoningLevel, customBudgets?)`
pub fn thinking_budget_for_level(
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> u64 {
    let level =
        clamp_reasoning(Some(reasoning_level)).expect("clampReasoning always returns a level");
    match level {
        ThinkingLevel::Minimal => custom_budgets.and_then(|b| b.minimal).unwrap_or(1024),
        ThinkingLevel::Low => custom_budgets.and_then(|b| b.low).unwrap_or(2048),
        ThinkingLevel::Medium => custom_budgets.and_then(|b| b.medium).unwrap_or(8192),
        ThinkingLevel::High => custom_budgets.and_then(|b| b.high).unwrap_or(16384),
        ThinkingLevel::Xhigh | ThinkingLevel::Max => {
            unreachable!("clampReasoning folds xhigh/max into high")
        }
    }
}

/// 对应 `clampThinkingBudgetToAnswerRoom(thinkingBudget, ceiling)`
pub fn clamp_thinking_budget_to_answer_room(thinking_budget: u64, ceiling: u64) -> u64 {
    thinking_budget.min(ceiling.saturating_sub(MIN_ANSWER_TOKENS))
}

/// 对应 `adjustMaxTokensForThinking(...)` 的返回值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdjustedMaxTokens {
    pub max_tokens: u64,
    pub thinking_budget: u64,
}

/// 对应 `adjustMaxTokensForThinking(baseMaxTokens, modelMaxTokens, reasoningLevel, customBudgets?)`
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&ThinkingBudgets>,
) -> AdjustedMaxTokens {
    let mut thinking_budget = thinking_budget_for_level(reasoning_level, custom_budgets);
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => base.saturating_add(thinking_budget).min(model_max_tokens),
    };
    if max_tokens <= thinking_budget {
        thinking_budget = clamp_thinking_budget_to_answer_room(thinking_budget, max_tokens);
    }
    AdjustedMaxTokens {
        max_tokens,
        thinking_budget,
    }
}
