//! Rust 翻译自 packages/ai/src/models.ts（核心框架）
//!
//! provider 集合、模型查找、认证解析与流式分发。认证解析（CredentialStore /
//! AuthContext / OAuth / api-key）对齐 TS 原版：`stream_simple` 在分发前解析
//! provider 认证并把 api_key/headers/base_url 注入请求。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::StreamExt;

use crate::auth::context::default_provider_auth_context;
use crate::auth::credential_store::InMemoryCredentialStore;
use crate::auth::types::{
    ApiKeyCheckInput, AuthCheck, AuthContext, AuthInteraction, AuthOperationOptions, AuthResult,
    AuthType, Credential, CredentialModifyFn, CredentialStore, ProviderAuth,
    ProviderAuthInteraction,
};
use crate::types::{
    AbortSignal, AssistantMessageEvent, Context, ErrorStopReason, Model, ModelThinkingLevel,
    ProviderHeaders, SimpleStreamOptions, StreamFunction, Usage, UsageCost,
};
use crate::utils::abort::{BoxError, operation_signal, race_with_abort_signal};
use crate::utils::error_stream::{create_error_message, stream_error};
use crate::utils::event_stream::{
    AssistantMessageEventStream, create_assistant_message_event_stream,
};

// `ModelsError` / `ModelsErrorCode` 定义于 auth/resolve（与 TS 一致：models.ts re-export
// 自 auth/resolve.ts）。
use crate::auth::resolve::{AuthResolutionOverrides, ProviderAuthRef, resolve_provider_auth};
pub use crate::auth::resolve::{ModelsError, ModelsErrorCode};

/// 对应 `Provider`（简化：去掉 filterModels/fetchModels 等模型刷新能力，保留
/// 模型目录、认证声明与流式分发）。
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn base_url(&self) -> Option<&str>;
    /// 对应 `auth`：每个 provider 至少声明一种认证语义（api-key 或 oauth）。
    fn auth(&self) -> &ProviderAuth;
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

/// 对应 `CreateProviderOptions`（去掉 filterModels/fetchModels）。
pub struct CreateProviderOptions {
    pub id: String,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub auth: ProviderAuth,
    pub models: Vec<Model>,
    pub stream: StreamFunction,
}

struct ProviderImpl {
    id: String,
    name: String,
    base_url: Option<String>,
    auth: ProviderAuth,
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

    fn auth(&self) -> &ProviderAuth {
        &self.auth
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
        auth: options.auth,
        models: options.models,
        stream: options.stream,
    })
}

/// 对应 `Models`（provider 集合 + 模型查找 + 认证解析 + 流式分发，去掉 login/
/// refreshModels 等模型刷新能力）。
pub struct Models {
    providers: Mutex<BTreeMap<String, Arc<dyn Provider>>>,
    credentials: Arc<dyn CredentialStore>,
    auth_context: Arc<dyn AuthContext>,
}

impl Default for Models {
    fn default() -> Self {
        Self::new()
    }
}

impl Models {
    pub fn new() -> Self {
        Self::with_auth(
            Arc::new(InMemoryCredentialStore::new()),
            Arc::from(default_provider_auth_context()),
        )
    }

    /// 对应 `createModels({ credentials, authContext })`：注入自定义凭据存储与认证上下文。
    pub fn with_auth(
        credentials: Arc<dyn CredentialStore>,
        auth_context: Arc<dyn AuthContext>,
    ) -> Self {
        Self {
            providers: Mutex::new(BTreeMap::new()),
            credentials,
            auth_context,
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

    /// 对应 `streamSimple(model, context, options)`：解析 provider 认证并注入请求
    /// （api_key/headers 进 options，base_url 覆盖 model），再分发到 provider stream。
    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let Some(provider) = self.get_provider(&model.provider) else {
            return stream_error(model, format!("Unknown provider: {}", model.provider));
        };

        let credentials = self.credentials.clone();
        let auth_context = self.auth_context.clone();
        let provider_id = provider.id().to_string();
        let provider_auth = provider.auth().clone();
        let model = model.clone();
        let context = context.clone();
        let options = options.cloned();

        let stream = create_assistant_message_event_stream();
        let producer = stream.clone();

        tokio::spawn(async move {
            let overrides = AuthResolutionOverrides {
                api_key: options
                    .as_ref()
                    .and_then(|o| o.stream.request.api_key.clone()),
                env: None,
                min_oauth_validity_ms: None,
                signal: options
                    .as_ref()
                    .and_then(|o| o.stream.request.signal.clone()),
            };
            let provider_ref = ProviderAuthRef {
                id: &provider_id,
                auth: &provider_auth,
            };
            match resolve_provider_auth(
                &provider_ref,
                credentials.as_ref(),
                auth_context.as_ref(),
                Some(&overrides),
            )
            .await
            {
                Ok(Some(resolution)) => {
                    let mut request_model = model.clone();
                    if let Some(base_url) = resolution.auth.base_url.as_ref() {
                        request_model.base_url = base_url.clone();
                    }
                    let mut request_options = options.clone();
                    if let Some(opts) = request_options.as_mut() {
                        opts.stream.request.api_key = resolution.auth.api_key.clone();
                        opts.stream.request.headers = merge_headers(
                            opts.stream.request.headers.clone(),
                            resolution.auth.headers.clone(),
                        );
                    }
                    let inner =
                        provider.stream_simple(&request_model, &context, request_options.as_ref());
                    let mut inner = inner;
                    while let Some(event) = inner.next().await {
                        producer.push(event);
                    }
                }
                Ok(None) => {
                    let error = create_error_message(
                        &format!("Provider is not configured: {provider_id}"),
                        &model.api,
                        &model.provider,
                        &model.id,
                    );
                    producer.push(AssistantMessageEvent::Error {
                        reason: ErrorStopReason::Error,
                        error: error.clone(),
                    });
                    producer.end(Some(error));
                }
                Err(err) => {
                    let error = create_error_message(
                        &err.to_string(),
                        &model.api,
                        &model.provider,
                        &model.id,
                    );
                    producer.push(AssistantMessageEvent::Error {
                        reason: ErrorStopReason::Error,
                        error: error.clone(),
                    });
                    producer.end(Some(error));
                }
            }
        });

        stream
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

    /// 对应 `checkAuth(providerId, options?)`：检查 provider 是否已配置认证
    /// （OAuth 凭据存在，或 api-key 可解析），不刷新 token。
    pub async fn check_auth(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<AuthCheck>, BoxError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        race_with_abort_signal(self.check_auth_with_signal(provider_id, &signal), &signal).await
    }

    async fn check_auth_with_signal(
        &self,
        provider_id: &str,
        signal: &AbortSignal,
    ) -> Result<Option<AuthCheck>, BoxError> {
        signal
            .throw_if_aborted()
            .map_err(|e| Box::new(e) as BoxError)?;
        let Some(provider) = self.get_provider(provider_id) else {
            return Ok(None);
        };
        let provider_auth = provider.auth().clone();
        let credential = self.read_credential(provider_id, signal).await?;
        self.check_provider_auth(provider_id, &provider_auth, credential.as_ref(), signal)
            .await
    }

    /// 对应 `getAvailable(providerId?, options?)`：返回认证已就绪的 provider 的模型。
    pub async fn get_available(
        &self,
        provider_id: Option<&str>,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<Model>, BoxError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        race_with_abort_signal(
            self.get_available_with_signal(provider_id, &signal),
            &signal,
        )
        .await
    }

    async fn get_available_with_signal(
        &self,
        provider_id: Option<&str>,
        signal: &AbortSignal,
    ) -> Result<Vec<Model>, BoxError> {
        signal
            .throw_if_aborted()
            .map_err(|e| Box::new(e) as BoxError)?;
        let providers: Vec<Arc<dyn Provider>> = match provider_id {
            Some(id) => self.get_provider(id).into_iter().collect(),
            None => self.get_providers(),
        };
        let mut result = Vec::new();
        for provider in providers {
            let provider_auth = provider.auth().clone();
            let credential = self.read_credential(provider.id(), signal).await?;
            if self
                .check_provider_auth(provider.id(), &provider_auth, credential.as_ref(), signal)
                .await?
                .is_some()
            {
                result.extend(provider.get_models());
            }
        }
        Ok(result)
    }

    /// 对应 `getAuth(providerId, overrides?)`：解析 provider 认证。
    pub async fn get_auth_for_provider(
        &self,
        provider_id: &str,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, BoxError> {
        let Some(provider) = self.get_provider(provider_id) else {
            return Ok(None);
        };
        let provider_id_owned = provider.id().to_string();
        let provider_auth = provider.auth().clone();
        let provider_ref = ProviderAuthRef {
            id: &provider_id_owned,
            auth: &provider_auth,
        };
        resolve_provider_auth(
            &provider_ref,
            self.credentials.as_ref(),
            self.auth_context.as_ref(),
            overrides,
        )
        .await
    }

    /// 对应 `getAuth(model, overrides?)`：解析模型所属 provider 的认证，并合并模型的
    /// 自定义 headers（caller 覆盖默认）。
    pub async fn get_auth(
        &self,
        model: &Model,
        overrides: Option<&AuthResolutionOverrides>,
    ) -> Result<Option<AuthResult>, BoxError> {
        let Some(mut result) = self
            .get_auth_for_provider(&model.provider, overrides)
            .await?
        else {
            return Ok(None);
        };
        let model_headers = model.headers.as_ref().map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), Some(v.clone())))
                .collect::<ProviderHeaders>()
        });
        result.auth.headers = merge_headers(result.auth.headers, model_headers);
        Ok(Some(result))
    }

    /// 对应 `login(providerId, type, interaction)`：运行 provider 登录流程并持久化凭据。
    pub async fn login(
        &self,
        provider_id: &str,
        auth_type: AuthType,
        interaction: &dyn AuthInteraction,
    ) -> Result<Credential, BoxError> {
        let signal = operation_signal(interaction.signal());
        signal
            .throw_if_aborted()
            .map_err(|e| Box::new(e) as BoxError)?;

        let Some(provider) = self.get_provider(provider_id) else {
            return Err(Box::new(ModelsError::new(
                ModelsErrorCode::Provider,
                format!("Unknown provider: {provider_id}"),
                None,
            )));
        };
        let provider_name = provider.name().to_string();
        let provider_auth = provider.auth().clone();
        let provider_interaction = ProviderAuthInteraction {
            signal: signal.clone(),
            interaction,
        };

        let credential = match auth_type {
            AuthType::ApiKey => {
                let api_key = provider_auth.api_key.as_ref().ok_or_else(|| {
                    Box::new(ModelsError::new(
                        ModelsErrorCode::Auth,
                        format!("{provider_name} does not support api_key login"),
                        None,
                    )) as BoxError
                })?;
                let credential =
                    race_with_abort_signal(api_key.login(&provider_interaction), &signal)
                        .await
                        .map_err(|e| {
                            Box::new(ModelsError::new(
                                ModelsErrorCode::Auth,
                                format!("Login failed for {provider_id}"),
                                Some(e),
                            )) as BoxError
                        })?;
                Credential::ApiKey(credential)
            }
            AuthType::OAuth => {
                let oauth = provider_auth.oauth.as_ref().ok_or_else(|| {
                    Box::new(ModelsError::new(
                        ModelsErrorCode::Auth,
                        format!("{provider_name} does not support oauth login"),
                        None,
                    )) as BoxError
                })?;
                let credential =
                    race_with_abort_signal(oauth.login(&provider_interaction), &signal)
                        .await
                        .map_err(|e| {
                            Box::new(ModelsError::new(
                                ModelsErrorCode::Auth,
                                format!("Login failed for {provider_id}"),
                                Some(e),
                            )) as BoxError
                        })?;
                Credential::OAuth(credential)
            }
        };

        let stored = self
            .credentials
            .modify(
                provider_id,
                {
                    let credential = credential.clone();
                    let modify: CredentialModifyFn = Box::new(move |_current| {
                        let credential = credential.clone();
                        Box::pin(async move { Ok(Some(credential)) })
                    });
                    modify
                },
                Some(&AuthOperationOptions {
                    signal: Some(signal.clone()),
                }),
            )
            .await
            .map_err(|e| {
                Box::new(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Credential store modify failed for {provider_id}"),
                    Some(e),
                )) as BoxError
            })?;

        Ok(stored.unwrap_or(credential))
    }

    /// 对应 `logout(providerId, options?)`：删除 provider 的已存凭据。
    pub async fn logout(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<(), BoxError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        signal
            .throw_if_aborted()
            .map_err(|e| Box::new(e) as BoxError)?;
        self.credentials
            .delete(
                provider_id,
                Some(&AuthOperationOptions {
                    signal: Some(signal.clone()),
                }),
            )
            .await
            .map_err(|e| {
                Box::new(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Credential store delete failed for {provider_id}"),
                    Some(e),
                )) as BoxError
            })
    }

    /// 对应 `checkProviderAuth`：OAuth 凭据存在即视为已配置；api-key 走 check（若有
    /// 自定义实现）否则 fallback 到 resolveProviderAuth。
    async fn check_provider_auth(
        &self,
        provider_id: &str,
        provider_auth: &ProviderAuth,
        credential: Option<&Credential>,
        signal: &AbortSignal,
    ) -> Result<Option<AuthCheck>, BoxError> {
        if matches!(credential, Some(Credential::OAuth(_))) {
            return Ok(provider_auth.oauth.as_ref().map(|_| AuthCheck {
                source: Some("OAuth".to_string()),
                kind: AuthType::OAuth,
            }));
        }
        let Some(api_key) = provider_auth.api_key.as_ref() else {
            return Ok(None);
        };
        let api_key_credential = match credential {
            Some(Credential::ApiKey(c)) => Some(c),
            _ => None,
        };
        let input = ApiKeyCheckInput {
            ctx: self.auth_context.as_ref(),
            credential: api_key_credential,
            signal,
        };
        match api_key.check(&input).await {
            Ok(Some(check)) => Ok(Some(check)),
            Ok(None) => {
                // Rust 版 `check` 有默认实现返回 None；与原版「无 check 方法时
                // fallback 到 resolve」语义对齐。
                let provider_ref = ProviderAuthRef {
                    id: provider_id,
                    auth: provider_auth,
                };
                let resolution = resolve_provider_auth(
                    &provider_ref,
                    self.credentials.as_ref(),
                    self.auth_context.as_ref(),
                    Some(&AuthResolutionOverrides {
                        signal: Some(signal.clone()),
                        ..Default::default()
                    }),
                )
                .await?;
                Ok(resolution.map(|r| AuthCheck {
                    source: r.source,
                    kind: AuthType::ApiKey,
                }))
            }
            Err(error) => Err(Box::new(ModelsError::new(
                ModelsErrorCode::Auth,
                format!("API key auth check failed for provider {provider_id}"),
                Some(error),
            )) as BoxError),
        }
    }

    /// 对应 `readCredential`：从凭据存储读取 provider 凭据，缺失返回 None。
    async fn read_credential(
        &self,
        provider_id: &str,
        signal: &AbortSignal,
    ) -> Result<Option<Credential>, BoxError> {
        self.credentials
            .read(
                provider_id,
                Some(&AuthOperationOptions {
                    signal: Some(signal.clone()),
                }),
            )
            .await
            .map_err(|e| {
                Box::new(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Credential store read failed for {provider_id}"),
                    Some(e),
                )) as BoxError
            })
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

/// 对应 `mergeHeaders`：按 header 名（不区分大小写）合并，override 覆盖 base。
fn merge_headers(
    base: Option<ProviderHeaders>,
    override_: Option<ProviderHeaders>,
) -> Option<ProviderHeaders> {
    match (base, override_) {
        (None, None) => None,
        (Some(base), None) => Some(base),
        (None, Some(override_)) => Some(override_),
        (Some(mut base), Some(override_)) => {
            for (name, value) in override_ {
                let lower = name.to_lowercase();
                base.retain(|existing, _| existing.to_lowercase() != lower);
                base.insert(name, value);
            }
            Some(base)
        }
    }
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
