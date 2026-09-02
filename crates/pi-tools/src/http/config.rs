//! HTTP 工具配置类型。

use std::collections::BTreeMap;

use serde::Deserialize;

/// 认证配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    /// `Authorization: Bearer <token>`，token 从环境变量读取。
    Bearer { token_env: String },
    /// 自定义 header 名承载 token。
    Header {
        header_name: String,
        token_env: String,
    },
    /// HTTP Basic 认证。
    Basic {
        username_env: String,
        password_env: String,
    },
    /// token 作为 query 参数。
    QueryToken {
        param_name: String,
        token_env: String,
    },
}

/// HTTP 请求模板。
#[derive(Debug, Clone, Deserialize)]
pub struct HttpRequestConfig {
    pub method: String,
    /// URL 模板，`{name}` 为 path 参数占位符。
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// HTTP 工具配置。
#[derive(Debug, Clone, Deserialize)]
pub struct HttpToolConfig {
    pub name: String,
    pub description: String,
    /// 工具参数 JSON Schema。
    pub parameters: serde_json::Value,
    pub request: HttpRequestConfig,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    30
}
