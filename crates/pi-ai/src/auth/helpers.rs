//! Rust 翻译自 packages/ai/src/auth/helpers.ts

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::auth::types::{
    ApiKeyAuth, ApiKeyCredential, ApiKeyResolveInput, AuthPrompt, AuthResult, ModelAuth, OAuthAuth,
    OAuthCredential, ProviderAuthInteraction,
};
use crate::types::AbortSignal;
use crate::utils::abort::BoxError;

/// 对应 `envApiKeyAuth`：标准 api-key 认证，提示输入 key，任一已设置的
/// env var 命中即解析成功。
struct EnvApiKeyAuth {
    name: String,
    env_vars: Vec<String>,
}

#[async_trait::async_trait]
impl ApiKeyAuth for EnvApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    async fn login(
        &self,
        interaction: &ProviderAuthInteraction<'_>,
    ) -> Result<ApiKeyCredential, BoxError> {
        interaction
            .signal
            .throw_if_aborted()
            .map_err(|e| Box::new(e) as BoxError)?;
        let key = interaction
            .prompt(AuthPrompt::secret(format!("Enter {}", self.name)))
            .await?;
        interaction
            .signal
            .throw_if_aborted()
            .map_err(|e| Box::new(e) as BoxError)?;
        Ok(ApiKeyCredential {
            key: Some(key),
            env: None,
        })
    }

    async fn resolve(&self, input: &ApiKeyResolveInput<'_>) -> Result<Option<AuthResult>, BoxError> {
        input
            .signal
            .throw_if_aborted()
            .map_err(|e| Box::new(e) as BoxError)?;
        if let Some(key) = input.credential.and_then(|c| c.key.as_ref()) {
            return Ok(Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some(key.clone()),
                    headers: None,
                    base_url: None,
                },
                env: input.credential.and_then(|c| c.env.clone()),
                source: Some("stored credential".to_string()),
            }));
        }
        for env_var in &self.env_vars {
            let value = input.ctx.env(env_var).await;
            input
                .signal
                .throw_if_aborted()
                .map_err(|e| Box::new(e) as BoxError)?;
            if let Some(value) = value {
                return Ok(Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(value),
                        headers: None,
                        base_url: None,
                    },
                    env: None,
                    source: Some(env_var.clone()),
                }));
            }
        }
        Ok(None)
    }
}

/// 对应 `envApiKeyAuth(name, envVars)`。
pub fn env_api_key_auth(name: impl Into<String>, env_vars: Vec<String>) -> Arc<dyn ApiKeyAuth> {
    Arc::new(EnvApiKeyAuth {
        name: name.into(),
        env_vars,
    })
}

/// 对应 `lazyOAuth` 的 `load: () => Promise<OAuthAuth>`。
pub type OAuthLoadFn = Box<
    dyn Fn() -> Pin<Box<dyn Future<Output = Arc<dyn OAuthAuth>> + Send>> + Send + Sync,
>;

/// 对应 `lazyOAuth` 的输入。
pub struct LazyOAuthInput {
    pub name: String,
    pub is_subscription: Option<bool>,
    pub login_label: Option<String>,
    pub load: OAuthLoadFn,
}

/// 对应 `lazyOAuth(input)`：惰性包装动态加载的 `OAuthAuth`，首次
/// `login`/`refresh`/`toAuth` 时才 `load()`，并缓存结果。
struct LazyOAuth {
    name: String,
    is_subscription: Option<bool>,
    login_label: Option<String>,
    load: OAuthLoadFn,
    loaded: tokio::sync::OnceCell<Arc<dyn OAuthAuth>>,
}

impl LazyOAuth {
    async fn loaded(&self) -> Result<Arc<dyn OAuthAuth>, BoxError> {
        self.loaded
            .get_or_try_init(|| async {
                let oauth = (self.load)().await;
                Ok::<_, BoxError>(oauth)
            })
            .await
            .map(Clone::clone)
    }
}

#[async_trait::async_trait]
impl OAuthAuth for LazyOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_subscription(&self) -> Option<bool> {
        self.is_subscription
    }

    fn login_label(&self) -> Option<&str> {
        self.login_label.as_deref()
    }

    async fn login(
        &self,
        interaction: &ProviderAuthInteraction<'_>,
    ) -> Result<OAuthCredential, BoxError> {
        self.loaded().await?.login(interaction).await
    }

    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &AbortSignal,
    ) -> Result<OAuthCredential, BoxError> {
        self.loaded().await?.refresh(credential, signal).await
    }

    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, BoxError> {
        self.loaded().await?.to_auth(credential).await
    }
}

/// 对应 `lazyOAuth(input)`。
pub fn lazy_oauth(input: LazyOAuthInput) -> Arc<dyn OAuthAuth> {
    Arc::new(LazyOAuth {
        name: input.name,
        is_subscription: input.is_subscription,
        login_label: input.login_label,
        load: input.load,
        loaded: tokio::sync::OnceCell::new(),
    })
}
