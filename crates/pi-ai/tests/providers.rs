//! Rust 翻译自 ai 的 provider/models 层测试（faux stream + models 分发 + 成本计算）。

use futures::StreamExt;
use pi_ai::{
    AssistantMessageEvent, Context, InputModality, Model, ModelCost, ModelCostRates, StopReason,
    Usage, UsageCost, calculate_cost, create_models, faux_assistant_message, faux_provider,
    faux_text,
};

fn default_faux_model() -> Model {
    faux_provider(Vec::new())
        .provider
        .get_models()
        .into_iter()
        .next()
        .unwrap()
}

#[tokio::test]
async fn faux_streams_scripted_response() {
    let handle = faux_provider(Vec::new());
    let model = handle.provider.get_models().into_iter().next().unwrap();
    handle.set_responses(vec![faux_assistant_message(
        vec![faux_text("hello")],
        StopReason::Stop,
    )]);

    let context = Context {
        system_prompt: None,
        messages: Vec::new(),
        tools: None,
    };
    let mut stream = handle.provider.stream_simple(&model, &context, None);

    let mut text = String::new();
    let mut final_message = None;
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
            AssistantMessageEvent::Done { message, .. } => final_message = Some(message),
            _ => {}
        }
    }

    assert_eq!(text, "hello");
    let final_message = final_message.expect("expected done event");
    assert_eq!(final_message.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn faux_streams_error_when_no_responses() {
    let handle = faux_provider(Vec::new());
    let model = handle.provider.get_models().into_iter().next().unwrap();

    let context = Context {
        system_prompt: None,
        messages: Vec::new(),
        tools: None,
    };
    let mut stream = handle.provider.stream_simple(&model, &context, None);

    let mut error_message = None;
    while let Some(event) = stream.next().await {
        if let AssistantMessageEvent::Error { error, .. } = event {
            error_message = error.error_message;
        }
    }

    assert!(error_message.is_some());
}

#[test]
fn models_dispatch_lookup() {
    let models = create_models();
    let handle = faux_provider(Vec::new());
    let model = default_faux_model();
    models.set_provider(handle.provider.clone());

    assert!(models.get_provider("faux").is_some());
    assert!(models.get_model("faux", &model.id).is_some());
    assert!(models.get_model("unknown", "x").is_none());
}

#[test]
fn calculate_cost_applies_rates() {
    let model = Model {
        id: "m".into(),
        name: "m".into(),
        api: "faux".into(),
        provider: "faux".into(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 2.0,
                output: 8.0,
                cache_read: 0.5,
                cache_write: 2.0,
            },
            tiers: None,
        },
        context_window: 100_000,
        max_tokens: 4096,
        sampling_params: None,
        headers: None,
        compat: None,
    };

    let mut usage = Usage {
        input: 1_000_000,
        output: 1_000_000,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 2_000_000,
        cost: UsageCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    };

    let cost = calculate_cost(&model, &mut usage);
    // 每百万 token 2.0 / 8.0 美元
    assert!((cost.input - 2.0).abs() < 1e-9);
    assert!((cost.output - 8.0).abs() < 1e-9);
    assert!((cost.total - 10.0).abs() < 1e-9);
}
