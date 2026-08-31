//! Rust 翻译自 packages/ai/src/models.ts（核心框架）
//!
//! 注：TS 原版的 auth 系统（OAuth、CredentialStore、AuthContext）体量巨大且与
//! agent 核心无关，此处简化为：provider 自行从环境变量解析 API key；`Models`
//! 只负责 provider 集合、模型查找与流式分发。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::types::{
    Context, Model, ModelThinkingLevel, SimpleStreamOptions, StreamFunction, Usage, UsageCost,
};
use crate::utils::event_stream::AssistantMessageEventStream;

/// 对应 `ModelsError`（简化为 code + message）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ModelsError {}

/// 对应 `Provider`（去掉 auth/OAuth，仅保留模型目录与流式分发）。
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn base_url(&self) -> Option<&str>;
    /// 对应 `getModels()`
    fn get_models(&self) -> Vec<Model>;
    /// 对应 `streamSimple(model, context, options)`
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream;
}

/// 对应 `CreateProviderOptions`（去掉 auth/filterModels/fetchModels）。
pub struct CreateProviderOptions {
    pub id: String,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub models: Vec<Model>,
    pub stream: StreamFunction,
}

struct ProviderImpl {
    id: String,
    name: String,
    base_url: Option<String>,
    models: Vec<Model>,
    stream: StreamFunction,
}

impl Provider for ProviderImpl {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    fn get_models(&self) -> Vec<Model> {
        self.models.clone()
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        (self.stream)(model, context, options)
    }
}

/// 对应 `createProvider`
pub fn create_provider(options: CreateProviderOptions) -> Arc<dyn Provider> {
    let id = options.id.clone();
    Arc::new(ProviderImpl {
        name: options.name.unwrap_or(id),
        id: options.id,
        base_url: options.base_url,
        models: options.models,
        stream: options.stream,
    })
}

/// 对应 `Models`（provider 集合 + 模型查找 + 流式分发，去掉 auth/login）。
pub struct Models {
    providers: Mutex<BTreeMap<String, Arc<dyn Provider>>>,
}

impl Default for Models {
    fn default() -> Self {
        Self::new()
    }
}

impl Models {
    pub fn new() -> Self {
        Self {
            providers: Mutex::new(BTreeMap::new()),
        }
    }

    /// 对应 `getProviders()`
    pub fn get_providers(&self) -> Vec<Arc<dyn Provider>> {
        self.providers.lock().unwrap().values().cloned().collect()
    }

    /// 对应 `getProvider(id)`
    pub fn get_provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.lock().unwrap().get(id).cloned()
    }

    /// 对应 `getModels(provider?)`
    pub fn get_models(&self, provider: Option<&str>) -> Vec<Model> {
        let providers = self.providers.lock().unwrap();
        match provider {
            Some(id) => providers
                .get(id)
                .map(|p| p.get_models())
                .unwrap_or_default(),
            None => providers.values().flat_map(|p| p.get_models()).collect(),
        }
    }

    /// 对应 `getModel(provider, id)`
    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        let provider = self.get_provider(provider)?;
        provider.get_models().into_iter().find(|m| m.id == id)
    }

    /// 对应 `streamSimple(model, context, options)`
    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        if let Some(provider) = self.get_provider(&model.provider) {
            return provider.stream_simple(model, context, options);
        }
        // 未知 provider：返回一个编码错误的流。

        crate::utils::error_stream::stream_error(
            model,
            format!("Unknown provider: {}", model.provider),
        )
    }

    /// 对应 `completeSimple(model, context, options)`
    pub fn complete_simple(
        &self,
        model: Model,
        context: Context,
        options: Option<SimpleStreamOptions>,
    ) -> Pin<Box<dyn Future<Output = crate::types::AssistantMessage> + Send>> {
        let stream = self.stream_simple(&model, &context, options.as_ref());
        Box::pin(async move { stream.result().await })
    }

    /// 对应 `setProvider(provider)`
    pub fn set_provider(&self, provider: Arc<dyn Provider>) {
        self.providers
            .lock()
            .unwrap()
            .insert(provider.id().to_string(), provider);
    }

    /// 对应 `deleteProvider(id)`
    pub fn delete_provider(&self, id: &str) {
        self.providers.lock().unwrap().remove(id);
    }

    /// 对应 `clearProviders()`
    pub fn clear_providers(&self) {
        self.providers.lock().unwrap().clear();
    }
}

/// 对应 `createModels`
pub fn create_models() -> Models {
    Models::new()
}

/// 对应 `hasApi`
pub fn has_api(model: &Model, api: &str) -> bool {
    model.api == api
}

/// 对应 `calculateCost`：就地更新 `usage.cost` 并返回。
pub fn calculate_cost(model: &Model, usage: &mut Usage) -> UsageCost {
    let input_tokens = usage.input;
    let mut rates = &model.cost.rates;
    let mut matched_threshold: i64 = -1;
    if let Some(tiers) = &model.cost.tiers {
        for tier in tiers {
            if input_tokens > tier.input_tokens_above
                && (tier.input_tokens_above as i64) > matched_threshold
            {
                rates = &tier.rates;
                matched_threshold = tier.input_tokens_above as i64;
            }
        }
    }

    // Anthropic 对 1h 缓存写入按基础输入价的 2 倍计费。
    let long_write = usage.cache_write_1h.unwrap_or(0);
    let short_write = usage.cache_write - long_write;
    let cost = UsageCost {
        input: (rates.input / 1_000_000.0) * usage.input as f64,
        output: (rates.output / 1_000_000.0) * usage.output as f64,
        cache_read: (rates.cache_read / 1_000_000.0) * usage.cache_read as f64,
        cache_write: (rates.cache_write * short_write as f64
            + rates.input * 2.0 * long_write as f64)
            / 1_000_000.0,
        total: 0.0,
    };
    let mut cost = cost;
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
    usage.cost = cost.clone();
    cost
}

/// 对应 `EXTENDED_THINKING_LEVELS`
const EXTENDED_THINKING_LEVELS: [ModelThinkingLevel; 7] = [
    ModelThinkingLevel::Off,
    ModelThinkingLevel::Minimal,
    ModelThinkingLevel::Low,
    ModelThinkingLevel::Medium,
    ModelThinkingLevel::High,
    ModelThinkingLevel::Xhigh,
    ModelThinkingLevel::Max,
];

/// 对应 `getSupportedThinkingLevels`
pub fn get_supported_thinking_levels(model: &Model) -> Vec<ModelThinkingLevel> {
    if !model.reasoning {
        return vec![ModelThinkingLevel::Off];
    }
    EXTENDED_THINKING_LEVELS
        .iter()
        .copied()
        .filter(|level| {
            let mapped = model.thinking_level_map.as_ref().and_then(|m| m.get(level));
            // mapped === null（值为 null 表示不支持）
            if mapped == Some(&None) {
                return false;
            }
            if *level == ModelThinkingLevel::Xhigh || *level == ModelThinkingLevel::Max {
                return mapped.is_some();
            }
            true
        })
        .collect()
}

/// 对应 `clampThinkingLevel`
pub fn clamp_thinking_level(model: &Model, level: ModelThinkingLevel) -> ModelThinkingLevel {
    let available = get_supported_thinking_levels(model);
    if available.contains(&level) {
        return level;
    }

    let Some(requested_index) = EXTENDED_THINKING_LEVELS.iter().position(|l| *l == level) else {
        return available
            .first()
            .copied()
            .unwrap_or(ModelThinkingLevel::Off);
    };

    for candidate in &EXTENDED_THINKING_LEVELS[requested_index..] {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    for candidate in EXTENDED_THINKING_LEVELS[..requested_index].iter().rev() {
        if available.contains(candidate) {
            return *candidate;
        }
    }
    available
        .first()
        .copied()
        .unwrap_or(ModelThinkingLevel::Off)
}

/// 对应 `modelsAreEqual`
pub fn models_are_equal(a: Option<&Model>, b: Option<&Model>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.id == b.id && a.provider == b.provider,
        _ => false,
    }
}
