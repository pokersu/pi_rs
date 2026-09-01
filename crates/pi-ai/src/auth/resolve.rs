//! Rust 翻译自 packages/ai/src/auth/resolve.ts
//!
//! 认证解析逻辑，供 `Models` 与 `ImagesModels` 集合共享。
//! 已存储的凭据拥有该 provider：只有在没有任何存储时才查询 ambient/env。
//! refresh 失败后不做静默 env 回退。

use std::sync::Arc;
use std::time::Duration;

use crate::auth::types::{
    ApiKeyAuth, ApiKeyCredential, AuthContext, AuthOperationOptions, AuthResult, Credential,
    CredentialStore, OAuthAuth, OAuthCredential, ProviderAuth,
};
use crate::types::{AbortSignal, ProviderEnv};
use crate::utils::abort::{BoxError, operation_signal, race_with_abort_signal};
use crate::utils::diagnostics::format_thrown_value;

/// 对应 `ModelsErrorCode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelsErrorCode {
    ModelSource,
    ModelValidation,
    Provider,
    Stream,
    Auth,
    OAuth,
}

impl std::fmt::Display for ModelsErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ModelsErrorCode::ModelSource => "model_source",
            ModelsErrorCode::ModelValidation => "model_validation",
            ModelsErrorCode::Provider => "provider",
            ModelsErrorCode::Stream => "stream",
            ModelsErrorCode::Auth => "auth",
            ModelsErrorCode::OAuth => "oauth",
        };
        f.write_str(s)
    }
}

/// 对应 `AuthResolutionOverrides`。
#[derive(Debug, Clone, Default)]
pub struct AuthResolutionOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    /// 要求剩余的 OAuth token 有效期；默认五分钟。
    pub min_oauth_validity_ms: Option<u64>,
    pub signal: Option<AbortSignal>,
}

/// 对应 `ModelsError`。
#[derive(Debug)]
pub struct ModelsError {
    pub code: ModelsErrorCode,
    pub message: String,
    pub cause: Option<BoxError>,
}

impl ModelsError {
    /// 对应 `new ModelsError(code, message, options?)`。
    pub fn new(code: ModelsErrorCode, message: impl Into<String>, cause: Option<BoxError>) -> Self {
        let message = message.into();
        let message = with_cause_detail(&message, cause.as_ref());
        Self {
            code,
            message,
            cause,
        }
    }
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ModelsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_deref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

/// 对应 `withCauseDetail`：调用方只展示 `error.message`，故把底层原因拼进其中。
fn with_cause_detail(message: &str, cause: Option<&BoxError>) -> String {
    let Some(cause) = cause else {
        return message.to_string();
    };
    let detail = format_thrown_value(cause.as_ref()).trim().to_string();
    if detail.is_empty() || message.contains(&detail) {
        message.to_string()
    } else {
        format!("{message}: {detail}")
    }
}

/// 对应 `resolveProviderAuth` 的 provider 参数 `{ id, auth }`。
pub struct ProviderAuthRef<'a> {
    pub id: &'a str,
    pub auth: &'a ProviderAuth,
}

/// 对应 `resolveProviderAuth(provider, credentials, authContext, overrides?)`。
pub async fn resolve_provider_auth(
    provider: &ProviderAuthRef<'_>,
    credentials: &dyn CredentialStore,
    auth_context: &dyn AuthContext,
    overrides: Option<&AuthResolutionOverrides>,
) -> Result<Option<AuthResult>, BoxError> {
    let signal = operation_signal(overrides.and_then(|o| o.signal.as_ref()));
    race_with_abort_signal(
        resolve_provider_auth_with_signal(provider, credentials, auth_context, overrides, &signal),
        &signal,
    )
    .await
}

async fn resolve_provider_auth_with_signal(
    provider: &ProviderAuthRef<'_>,
    credentials: &dyn CredentialStore,
    auth_context: &dyn AuthContext,
    overrides: Option<&AuthResolutionOverrides>,
    signal: &AbortSignal,
) -> Result<Option<AuthResult>, BoxError> {
    signal
        .throw_if_aborted()
        .map_err(|e| Box::new(e) as BoxError)?;
    let request_auth_context: Box<dyn AuthContext> =
        if let Some(env) = overrides.and_then(|o| o.env.clone()) {
            Box::new(OverlayEnvAuthContext {
                base: auth_context,
                env,
            })
        } else {
            Box::new(BorrowedAuthContext(auth_context))
        };

    if let Some(api_key) = overrides.and_then(|o| o.api_key.clone())
        && let Some(api_key_auth) = provider.auth.api_key.as_deref()
    {
        let credential = ApiKeyCredential {
            key: Some(api_key),
            env: overrides.and_then(|o| o.env.clone()),
        };
        return resolve_api_key(
            request_auth_context.as_ref(),
            api_key_auth,
            provider.id,
            Some(&credential),
            signal,
        )
        .await;
    }

    let stored = read_credential(credentials, provider.id, signal).await?;
    if let Some(stored) = stored {
        match stored {
            Credential::OAuth(oauth_credential) => {
                if let Some(oauth) = &provider.auth.oauth {
                    return resolve_stored_oauth(
                        credentials,
                        provider.id,
                        oauth.clone(),
                        oauth_credential,
                        signal,
                        overrides.and_then(|o| o.min_oauth_validity_ms),
                    )
                    .await;
                }
                Ok(None)
            }
            Credential::ApiKey(api_key_credential) => {
                if let Some(api_key_auth) = provider.auth.api_key.as_deref() {
                    let credential = if let Some(env) = overrides.and_then(|o| o.env.as_ref()) {
                        let mut merged = api_key_credential.env.clone().unwrap_or_default();
                        merged.extend(env.clone());
                        ApiKeyCredential {
                            key: api_key_credential.key.clone(),
                            env: Some(merged),
                        }
                    } else {
                        api_key_credential
                    };
                    return resolve_api_key(
                        request_auth_context.as_ref(),
                        api_key_auth,
                        provider.id,
                        Some(&credential),
                        signal,
                    )
                    .await;
                }
                Ok(None)
            }
        }
    } else {
        // Ambient（env vars、AWS profiles、ADC files）。
        if let Some(api_key_auth) = provider.auth.api_key.as_deref() {
            return resolve_api_key(
                request_auth_context.as_ref(),
                api_key_auth,
                provider.id,
                None,
                signal,
            )
            .await;
        }
        Ok(None)
    }
}

/// 对应 `overlayEnvAuthContext`。
struct OverlayEnvAuthContext<'a> {
    base: &'a dyn AuthContext,
    env: ProviderEnv,
}

#[async_trait::async_trait]
impl AuthContext for OverlayEnvAuthContext<'_> {
    async fn env(&self, name: &str) -> Option<String> {
        if let Some(v) = self.env.get(name) {
            Some(v.clone())
        } else {
            self.base.env(name).await
        }
    }

    async fn file_exists(&self, path: &str) -> bool {
        self.base.file_exists(path).await
    }
}

/// 借用的 `AuthContext` 适配器（用于无 env override 时统一到 `Box<dyn AuthContext>`）。
struct BorrowedAuthContext<'a>(&'a dyn AuthContext);

#[async_trait::async_trait]
impl AuthContext for BorrowedAuthContext<'_> {
    async fn env(&self, name: &str) -> Option<String> {
        self.0.env(name).await
    }

    async fn file_exists(&self, path: &str) -> bool {
        self.0.file_exists(path).await
    }
}

const DEFAULT_OAUTH_MINIMUM_VALIDITY_MS: u64 = 5 * 60 * 1000;
const DEFAULT_OAUTH_REFRESH_TIMEOUT_MS: u64 = 15_000;

fn now_ms() -> u64 {
    crate::utils::uuid::now_ms() as u64
}

/// 对应 `resolveStoredOAuth`：双重检查锁定——剩余有效期不足五分钟的 token 加锁、
/// 在锁下复查过期、全局刷新一次、并在释放前持久化轮换后的凭据。
async fn resolve_stored_oauth(
    credentials: &dyn CredentialStore,
    provider_id: &str,
    oauth: Arc<dyn OAuthAuth>,
    stored: OAuthCredential,
    signal: &AbortSignal,
    min_oauth_validity_ms: Option<u64>,
) -> Result<Option<AuthResult>, BoxError> {
    let minimum_validity_ms =
        DEFAULT_OAUTH_MINIMUM_VALIDITY_MS.max(min_oauth_validity_ms.unwrap_or(0));
    let expires_soon =
        |credential: &OAuthCredential| now_ms() + minimum_validity_ms >= credential.expires;
    let mut credential = stored;

    if expires_soon(&credential) {
        // 乐观检查判为过期；权威检查在锁下进行。
        let provider_id_owned = provider_id.to_string();
        let signal_owned = signal.clone();
        let oauth_owned = oauth.clone();
        let post = match credentials
            .modify(
                provider_id,
                Box::new(move |current: Option<Credential>| {
                    let provider_id = provider_id_owned.clone();
                    let signal = signal_owned.clone();
                    let oauth = oauth_owned.clone();
                    Box::pin(async move {
                        let Some(Credential::OAuth(current_oauth)) = current else {
                            return Ok(None); // 期间已登出
                        };
                        if now_ms() + minimum_validity_ms < current_oauth.expires {
                            return Ok(None); // 其他进程/请求已刷新
                        }
                        let refresh_signal = AbortSignal::any(&[
                            signal.clone(),
                            AbortSignal::timeout(Duration::from_millis(
                                DEFAULT_OAUTH_REFRESH_TIMEOUT_MS,
                            )),
                        ]);
                        match oauth.refresh(&current_oauth, &refresh_signal).await {
                            Ok(credential) => Ok(Some(Credential::OAuth(credential))),
                            Err(error) => Err(Box::new(ModelsError::new(
                                ModelsErrorCode::OAuth,
                                format!("OAuth refresh failed for {provider_id}"),
                                Some(error),
                            )) as BoxError),
                        }
                    })
                }),
                Some(&AuthOperationOptions {
                    signal: Some(signal.clone()),
                }),
            )
            .await
        {
            Ok(post) => post,
            Err(error) => {
                if error.downcast_ref::<ModelsError>().is_some() {
                    return Err(error);
                }
                return Err(Box::new(ModelsError::new(
                    ModelsErrorCode::Auth,
                    format!("Credential store modify failed for {provider_id}"),
                    Some(error),
                )) as BoxError);
            }
        };

        let Some(Credential::OAuth(post_oauth)) = post else {
            return Ok(None); // 期间已登出
        };
        credential = post_oauth;
        // 正常五分钟窗口触发刷新，但不构成 provider 契约；显式调用方（如
        // bearer-token 导出）确实要求刷新后满足所要求的最小有效期。
        if min_oauth_validity_ms.is_some() && expires_soon(&credential) {
            return Err(Box::new(ModelsError::new(
                ModelsErrorCode::OAuth,
                format!("OAuth refresh returned a token that expires too soon for {provider_id}"),
                None,
            )) as BoxError);
        }
    }

    match oauth.to_auth(&credential).await {
        Ok(auth) => Ok(Some(AuthResult {
            auth,
            env: None,
            source: Some("OAuth".to_string()),
        })),
        Err(error) => Err(Box::new(ModelsError::new(
            ModelsErrorCode::OAuth,
            format!("OAuth auth derivation failed for {provider_id}"),
            Some(error),
        )) as BoxError),
    }
}

/// 对应 `resolveApiKey`。
async fn resolve_api_key(
    auth_context: &dyn AuthContext,
    api_key: &dyn ApiKeyAuth,
    provider_id: &str,
    credential: Option<&ApiKeyCredential>,
    signal: &AbortSignal,
) -> Result<Option<AuthResult>, BoxError> {
    let input = crate::auth::types::ApiKeyResolveInput {
        ctx: auth_context,
        credential,
        signal,
    };
    api_key.resolve(&input).await.map_err(|error| {
        Box::new(ModelsError::new(
            ModelsErrorCode::Auth,
            format!("API key auth failed for provider {provider_id}"),
            Some(error),
        )) as BoxError
    })
}

/// 对应 `readCredential`。
async fn read_credential(
    credentials: &dyn CredentialStore,
    provider_id: &str,
    signal: &AbortSignal,
) -> Result<Option<Credential>, BoxError> {
    credentials
        .read(
            provider_id,
            Some(&AuthOperationOptions {
                signal: Some(signal.clone()),
            }),
        )
        .await
        .map_err(|error| {
            Box::new(ModelsError::new(
                ModelsErrorCode::Auth,
                format!("Credential store read failed for {provider_id}"),
                Some(error),
            )) as BoxError
        })
}
