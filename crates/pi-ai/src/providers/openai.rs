//! Rust 翻译自 packages/ai/src/providers/openai.ts + openai.models.ts（简化：硬编码常用模型）。

use std::sync::Arc;

use crate::api::openai_completions::openai_completions_stream;
use crate::auth::helpers::env_api_key_auth;
use crate::auth::types::ProviderAuth;
use crate::models::{CreateProviderOptions, Provider, create_provider};
use crate::types::{InputModality, Model, ModelCost, ModelCostRates};

fn openai_model(
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
        provider: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        reasoning,
        thinking_level_map: None,
        input: vec![InputModality::Text, InputModality::Image],
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

/// 对应 `openaiProvider()`
pub fn openai_provider() -> Arc<dyn Provider> {
    let models = vec![
        openai_model("gpt-4o", "GPT-4o", false, 128_000, 16_384, 2.5, 10.0),
        openai_model(
            "gpt-4o-mini",
            "GPT-4o mini",
            false,
            128_000,
            16_384,
            0.15,
            0.6,
        ),
    ];
    let stream = openai_completions_stream("https://api.openai.com/v1".to_string());
    create_provider(CreateProviderOptions {
        id: "openai".to_string(),
        name: Some("OpenAI".to_string()),
        base_url: Some("https://api.openai.com/v1".to_string()),
        auth: ProviderAuth {
            api_key: Some(env_api_key_auth(
                "OpenAI API key",
                vec!["OPENAI_API_KEY".to_string()],
            )),
            oauth: None,
        },
        models,
        stream,
    })
}
