//! Rust 翻译自 packages/ai/src/auth/types.ts
//!
//! 认证体系的类型层：凭据（`Credential`）、凭据存储（`CredentialStore`）、
//! 认证上下文（`AuthContext`）、登录交互（`AuthInteraction`）、
//! API key 认证（`ApiKeyAuth`）与 OAuth 认证（`OAuthAuth`）。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::types::{AbortSignal, ProviderEnv, ProviderHeaders};
use crate::utils::abort::BoxError;

/// 对应 `Credential["type"] = "api_key" | "oauth"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

/// 对应 `ModelAuth`：单次模型请求的认证信息。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuth {
    pub api_key: Option<String>,
    pub headers: Option<ProviderHeaders>,
    pub base_url: Option<String>,
}

/// 对应 `ApiKeyCredential`（不含 `type` 判别字段，由外层 `Credential` 提供）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyCredential {
    pub key: Option<String>,
    pub env: Option<ProviderEnv>,
}

/// 对应 `OAuthCredential`。
///
/// TS 中带 index signature `[key: string]: unknown`，允许附加 `scope`、
/// `accountId`、`availableModelIds`、`enterpriseUrl`、`gatewayConfig` 等
/// provider 特有字段；Rust 用 `extra` 映射承载这些字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredential {
    pub refresh: String,
    pub access: String,
    pub expires: u64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl OAuthCredential {
    /// 对应 `credential.enterpriseUrl`（GitHub Copilot）。
    pub fn enterprise_url(&self) -> Option<&str> {
        self.extra.get("enterpriseUrl")?.as_str()
    }

    /// 对应 `credential.accountId`（OpenAI Codex）。
    pub fn account_id(&self) -> Option<&str> {
        self.extra.get("accountId")?.as_str()
    }

    /// 对应 `credential.availableModelIds`（GitHub Copilot）。
    pub fn available_model_ids(&self) -> Option<Vec<String>> {
        self.extra
            .get("availableModelIds")?
            .as_array()?
            .iter()
            .map(|v| v.as_str().map(String::from))
            .collect()
    }
}

/// 对应 `Credential = ApiKeyCredential | OAuthCredential`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

impl Credential {
    /// 对应 `credential.type`。
    pub fn kind(&self) -> CredentialKind {
        match self {
            Credential::ApiKey(_) => CredentialKind::ApiKey,
            Credential::OAuth(_) => CredentialKind::OAuth,
        }
    }
}

/// 对应 `CredentialInfo`：不含密钥的凭据元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub kind: CredentialKind,
}

/// 对应 `AuthOperationOptions`。
#[derive(Debug, Clone, Default)]
pub struct AuthOperationOptions {
    pub signal: Option<AbortSignal>,
}

/// 对应 `CredentialStore.modify` 的回调：读当前凭据，返回新的凭据（或 None 保持不变）。
pub type CredentialModifyFn = Box<
    dyn FnOnce(Option<Credential>) -> Pin<Box<dyn Future<Output = Result<Option<Credential>, BoxError>> + Send>>
        + Send,
>;

/// 对应 `CredentialStore`：按 `Provider.id` 键控的凭据存储。
#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    /// 对应 `read(providerId, options?)`。缺失条目返回 `Ok(None)`。
    async fn read(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, BoxError>;

    /// 对应 `list(options?)`。
    async fn list(&self, options: Option<&AuthOperationOptions>) -> Result<Vec<CredentialInfo>, BoxError>;

    /// 对应 `modify(providerId, fn, options?)`：唯一的写路径，按 provider 串行化。
    async fn modify(
        &self,
        provider_id: &str,
        fn_: CredentialModifyFn,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, BoxError>;

    /// 对应 `delete(providerId, options?)`：移除凭据（登出）。
    async fn delete(&self, provider_id: &str, options: Option<&AuthOperationOptions>)
        -> Result<(), BoxError>;
}

/// 对应 `AuthContext`：认证解析所需的环境访问，可注入以便测试与浏览器。
#[async_trait::async_trait]
pub trait AuthContext: Send + Sync {
    /// 对应 `env(name)`。
    async fn env(&self, name: &str) -> Option<String>;
    /// 对应 `fileExists(path)`。支持前导 `~`；浏览器中恒为 false。
    async fn file_exists(&self, path: &str) -> bool;
}

/// 对应 `AuthResult`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthResult {
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
    pub source: Option<String>,
}

/// 对应 `AuthType = "api_key" | "oauth"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

/// 对应 `AuthCheck`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthCheck {
    pub source: Option<String>,
    #[serde(rename = "type")]
    pub kind: AuthType,
}

/// 对应 `AuthInfoLink`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthInfoLink {
    pub url: String,
    pub label: Option<String>,
}

/// 对应 `Select` 提示的选项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSelectOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// 对应 `AuthPrompt` 的判别部分（不含 `signal`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthPromptKind {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<AuthSelectOption>,
    },
    ManualCode {
        message: String,
        placeholder: Option<String>,
    },
}

/// 对应 `AuthPrompt`：登录期间展示给用户的提示，`signal` 允许流程在外部事件
/// 完成该步骤时取消挂起的提示。
#[derive(Debug, Clone, PartialEq)]
pub struct AuthPrompt {
    pub signal: Option<AbortSignal>,
    pub kind: AuthPromptKind,
}

impl AuthPrompt {
    pub fn text(message: impl Into<String>) -> Self {
        Self {
            signal: None,
            kind: AuthPromptKind::Text {
                message: message.into(),
                placeholder: None,
            },
        }
    }

    pub fn secret(message: impl Into<String>) -> Self {
        Self {
            signal: None,
            kind: AuthPromptKind::Secret {
                message: message.into(),
                placeholder: None,
            },
        }
    }

    pub fn select(message: impl Into<String>, options: Vec<AuthSelectOption>) -> Self {
        Self {
            signal: None,
            kind: AuthPromptKind::Select {
                message: message.into(),
                options,
            },
        }
    }

    pub fn manual_code(message: impl Into<String>, placeholder: Option<String>) -> Self {
        Self {
            signal: None,
            kind: AuthPromptKind::ManualCode {
                message: message.into(),
                placeholder,
            },
        }
    }

    pub fn with_signal(mut self, signal: AbortSignal) -> Self {
        self.signal = Some(signal);
        self
    }
}

/// 对应 `AuthEvent`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AuthEvent {
    Info {
        message: String,
        links: Option<Vec<AuthInfoLink>>,
    },
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        interval_seconds: Option<u64>,
        expires_in_seconds: Option<u64>,
    },
    Progress {
        message: String,
    },
}

/// 对应 `AuthInteraction`：同时服务 api-key 与 OAuth 流程的登录交互回调。
#[async_trait::async_trait]
pub trait AuthInteraction: Send + Sync {
    /// 对应 `signal`。
    fn signal(&self) -> Option<&AbortSignal>;

    /// 对应 `prompt(prompt)`。取消/中止时返回错误。
    async fn prompt(&self, prompt: AuthPrompt) -> Result<String, BoxError>;

    /// 对应 `notify(event)`。
    fn notify(&self, event: AuthEvent);
}

/// 对应 `ProviderAuthInteraction = AuthInteraction & { signal: AbortSignal }`。
#[derive(Clone)]
pub struct ProviderAuthInteraction<'a> {
    pub signal: AbortSignal,
    pub interaction: &'a dyn AuthInteraction,
}

impl<'a> ProviderAuthInteraction<'a> {
    pub async fn prompt(&self, prompt: AuthPrompt) -> Result<String, BoxError> {
        self.interaction.prompt(prompt).await
    }

    pub fn notify(&self, event: AuthEvent) {
        self.interaction.notify(event);
    }
}

/// 对应 `ApiKeyAuth.check` 的输入。
pub struct ApiKeyCheckInput<'a> {
    pub ctx: &'a dyn AuthContext,
    pub credential: Option<&'a ApiKeyCredential>,
    pub signal: &'a AbortSignal,
}

/// 对应 `ApiKeyAuth.resolve` 的输入。
pub struct ApiKeyResolveInput<'a> {
    pub ctx: &'a dyn AuthContext,
    pub credential: Option<&'a ApiKeyCredential>,
    pub signal: &'a AbortSignal,
}

/// 对应 `ApiKeyAuth`。
#[async_trait::async_trait]
pub trait ApiKeyAuth: Send + Sync {
    /// 对应 `name`（显示名，如 "Anthropic API key"）。
    fn name(&self) -> &str;

    /// 对应 `login?`。缺失表示 ambient-only；默认实现返回错误。
    async fn login(
        &self,
        _interaction: &ProviderAuthInteraction<'_>,
    ) -> Result<ApiKeyCredential, BoxError> {
        Err("login not supported for this provider".into())
    }

    /// 对应 `check?`。缺失时 `Models` 通过解析 auth 检查可用性；默认返回 `Ok(None)`。
    async fn check(&self, _input: &ApiKeyCheckInput<'_>) -> Result<Option<AuthCheck>, BoxError> {
        Ok(None)
    }

    /// 对应 `resolve(input)`。
    async fn resolve(&self, input: &ApiKeyResolveInput<'_>) -> Result<Option<AuthResult>, BoxError>;
}

/// 对应 `OAuthAuth`。
#[async_trait::async_trait]
pub trait OAuthAuth: Send + Sync {
    /// 对应 `name`。
    fn name(&self) -> &str;

    /// 对应 `isSubscription?`。
    fn is_subscription(&self) -> Option<bool> {
        None
    }

    /// 对应 `loginLabel?`。
    fn login_label(&self) -> Option<&str> {
        None
    }

    /// 对应 `login(interaction)`。
    async fn login(&self, interaction: &ProviderAuthInteraction<'_>)
        -> Result<OAuthCredential, BoxError>;

    /// 对应 `refresh(credential, signal)`：交换 refresh token。失败时返回错误
    /// （invalid_grant 等）。
    async fn refresh(
        &self,
        credential: &OAuthCredential,
        signal: &AbortSignal,
    ) -> Result<OAuthCredential, BoxError>;

    /// 对应 `toAuth(credential)`：从有效凭据无副作用地派生请求认证。
    async fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, BoxError>;
}

/// 对应 `ProviderAuth`。`apiKey`/`oauth` 至少其一存在。
pub struct ProviderAuth {
    pub api_key: Option<Arc<dyn ApiKeyAuth>>,
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}
