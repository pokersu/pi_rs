//! 认证管理方法（checkAuth/getAuth/login/logout/getAvailable）的行为测试。

use std::collections::BTreeMap;
use std::sync::Arc;

use pi_ai::utils::abort::BoxError;
use pi_ai::{
    AuthContext, AuthEvent, AuthInteraction, AuthPrompt, AuthType, Credential,
    InMemoryCredentialStore, Models, openai_provider,
};

/// 测试用认证上下文：从内存映射读环境变量，避免 `set_var` 的并发问题。
struct TestAuthContext {
    keys: BTreeMap<String, String>,
}

#[async_trait::async_trait]
impl AuthContext for TestAuthContext {
    async fn env(&self, name: &str) -> Option<String> {
        self.keys.get(name).cloned()
    }

    async fn file_exists(&self, _path: &str) -> bool {
        false
    }
}

/// 测试用登录交互：secret 提示固定返回 "logged-key"。
struct TestInteraction;

#[async_trait::async_trait]
impl AuthInteraction for TestInteraction {
    fn signal(&self) -> Option<&pi_ai::AbortSignal> {
        None
    }

    async fn prompt(&self, _prompt: AuthPrompt) -> Result<String, BoxError> {
        Ok("logged-key".to_string())
    }

    fn notify(&self, _event: AuthEvent) {}
}

fn models_with(keys: BTreeMap<String, String>) -> Models {
    let store = Arc::new(InMemoryCredentialStore::new());
    let ctx = Arc::new(TestAuthContext { keys });
    let models = Models::with_auth(store, ctx);
    models.set_provider(openai_provider());
    models
}

#[tokio::test]
async fn check_auth_none_when_unconfigured() {
    let models = models_with(BTreeMap::new());
    let check = models.check_auth("openai", None).await.unwrap();
    assert!(check.is_none());
}

#[tokio::test]
async fn check_auth_resolves_env_key() {
    let mut keys = BTreeMap::new();
    keys.insert("OPENAI_API_KEY".to_string(), "k".to_string());
    let models = models_with(keys);
    let check = models.check_auth("openai", None).await.unwrap().unwrap();
    assert_eq!(check.kind, AuthType::ApiKey);
    assert_eq!(check.source.as_deref(), Some("OPENAI_API_KEY"));
}

#[tokio::test]
async fn get_auth_resolves_env_key() {
    let mut keys = BTreeMap::new();
    keys.insert("OPENAI_API_KEY".to_string(), "k".to_string());
    let models = models_with(keys);
    let auth = models
        .get_auth_for_provider("openai", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(auth.auth.api_key.as_deref(), Some("k"));
}

#[tokio::test]
async fn login_persists_and_logout_removes() {
    let models = models_with(BTreeMap::new());
    assert!(models.check_auth("openai", None).await.unwrap().is_none());

    let credential = models
        .login("openai", AuthType::ApiKey, &TestInteraction)
        .await
        .unwrap();
    assert!(matches!(credential, Credential::ApiKey(_)));

    let check = models.check_auth("openai", None).await.unwrap().unwrap();
    assert_eq!(check.source.as_deref(), Some("stored credential"));

    models.logout("openai", None).await.unwrap();
    assert!(models.check_auth("openai", None).await.unwrap().is_none());
}

#[tokio::test]
async fn get_available_only_includes_configured() {
    let models = models_with(BTreeMap::new());
    let available = models.get_available(None, None).await.unwrap();
    assert!(available.is_empty());

    let mut keys = BTreeMap::new();
    keys.insert("OPENAI_API_KEY".to_string(), "k".to_string());
    let models = models_with(keys);
    let available = models.get_available(None, None).await.unwrap();
    assert!(!available.is_empty());
}
