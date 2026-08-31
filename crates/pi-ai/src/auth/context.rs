//! Rust 翻译自 packages/ai/src/auth/context.ts

use crate::auth::types::AuthContext;

struct DefaultProviderAuthContext;

#[async_trait::async_trait]
impl AuthContext for DefaultProviderAuthContext {
    /// 对应 `env(name)`：从进程环境变量读取，浏览器中为 `undefined`。
    async fn env(&self, name: &str) -> Option<String> {
        let value = std::env::var(name).ok()?;
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    }

    /// 对应 `fileExists(path)`：检查文件是否存在，支持前导 `~`。
    async fn file_exists(&self, path: &str) -> bool {
        let resolved = match path.strip_prefix('~') {
            Some(rest) => {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}{rest}")
            }
            None => path.to_string(),
        };
        std::path::Path::new(&resolved).exists()
    }
}

/// 对应 `defaultProviderAuthContext()`。
pub fn default_provider_auth_context() -> Box<dyn AuthContext> {
    Box::new(DefaultProviderAuthContext)
}
