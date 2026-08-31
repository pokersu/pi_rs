//! Rust 翻译自 packages/ai/src/providers/deepseek.ts + deepseek.models.ts（简化：硬编码常用模型）。

use std::sync::Arc;

use crate::api::openai_completions::openai_completions_stream;
use crate::models::{CreateProviderOptions, Provider, create_provider};
use crate::types::{InputModality, Model, ModelCost, ModelCostRates};

fn deepseek_model(
    id: &str,
    name: &str,
    reasoning: bool,
    context_window: u64,
    max_tokens: u64,
    input_cost: f64,
    output_cost: f64,
) -> Model {
    Model {
        id: id.to_string(),
        name: name.to_string(),
        api: "openai-completions".to_string(),
        provider: "deepseek".to_string(),
        base_url: "https://api.deepseek.com".to_string(),
        reasoning,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost {
            rates: ModelCostRates {
                input: input_cost,
                output: output_cost,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: None,
        },
        context_window,
        max_tokens,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

/// 对应 `deepseekProvider()`
pub fn deepseek_provider() -> Arc<dyn Provider> {
    let models = vec![
        deepseek_model(
            "deepseek-chat",
            "DeepSeek Chat",
            false,
            64_000,
            8_192,
            0.27,
            1.1,
        ),
        deepseek_model(
            "deepseek-reasoner",
            "DeepSeek Reasoner",
            true,
            64_000,
            8_192,
            0.55,
            2.19,
        ),
    ];
    let stream =
        openai_completions_stream("https://api.deepseek.com".to_string(), "DEEPSEEK_API_KEY");
    create_provider(CreateProviderOptions {
        id: "deepseek".to_string(),
        name: Some("DeepSeek".to_string()),
        base_url: Some("https://api.deepseek.com".to_string()),
        models,
        stream,
    })
}
