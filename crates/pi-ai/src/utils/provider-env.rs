//! Rust 翻译自 packages/ai/src/utils/provider-env.ts

use crate::types::ProviderEnv;

/// 对应 `getProviderEnvValue(name, env?)`：从 scoped env → 进程 env 依次解析。
///
/// 原版的 Bun sandbox fallback（读取 `/proc/self/environ` 以绕过 Bun 编译二进制在
/// Linux sandbox 里暴露空 `process.env` 的问题）是 Bun 特化的；Rust 的
/// `std::env::var` 直接读取进程环境，无需该 fallback。
pub fn get_provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if let Some(value) = env.and_then(|e| e.get(name)) {
        return Some(value.clone());
    }
    std::env::var(name).ok()
}
