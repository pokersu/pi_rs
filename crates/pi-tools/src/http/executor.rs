//! 把 HTTP 工具配置转换为 `AgentTool` 并执行请求。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use pi_agent::{AgentTool, AgentToolResult};
use pi_ai::{TextContent, TextKind, TextOrImageContent};

use crate::http::config::{AuthConfig, HttpToolConfig};

/// 响应体截断上限（字符）。
const MAX_BODY_CHARS: usize = 8 * 1024;

/// 把 HTTP 工具配置转换为 `AgentTool`。
pub fn config_to_tool(config: HttpToolConfig) -> AgentTool {
    let label = config.name.clone();
    let name = config.name.clone();
    let description = config.description.clone();
    let parameters = config.parameters.clone();
    AgentTool {
        label,
        tool: pi_ai::Tool {
            name,
            description,
            parameters,
            constrained_sampling: None,
        },
        execute: Arc::new(move |_id, params, _signal, _on_update| {
            let config = config.clone();
            Box::pin(async move { execute_http(&config, &params).await })
        }),
        execution_mode: None,
    }
}

async fn execute_http(config: &HttpToolConfig, params: &serde_json::Value) -> AgentToolResult {
    let params_obj: serde_json::Map<String, serde_json::Value> =
        params.as_object().cloned().unwrap_or_default();

    // 1. 填充 URL 占位符（path 参数）。
    let placeholders = extract_placeholders(&config.request.url);
    let url = fill_url(&config.request.url, &params_obj).unwrap_or_else(|e| panic!("{e}"));

    // 2. 按 method 决定剩余参数的绑定位置。
    let method = config.request.method.to_ascii_uppercase();
    let body_method = matches!(method.as_str(), "POST" | "PUT" | "PATCH");

    let remaining: serde_json::Map<String, serde_json::Value> = params_obj
        .iter()
        .filter(|(k, _)| !placeholders.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let mut headers: BTreeMap<String, String> = config.request.headers.clone();
    let mut query_params: Vec<(String, String)> = Vec::new();

    // 3. 认证。
    if let Some(auth) = &config.auth {
        apply_auth(auth, &mut headers, &mut query_params);
    }

    // 默认 User-Agent：GitHub 等 API 要求请求带 UA，配置未指定时自动补上。
    ensure_user_agent(&mut headers);

    let mut final_url = url;
    if body_method {
        // 剩余参数作为 JSON body（query 只含 auth 注入的 query token）。
    } else {
        for (k, v) in &remaining {
            query_params.push((k.clone(), value_to_string(v)));
        }
    }

    if !query_params.is_empty() {
        let qs = query_params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
            .collect::<Vec<_>>()
            .join("&");
        final_url = format!("{final_url}?{qs}");
    }

    // 4. 发请求。
    let reqwest_method =
        reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let client = reqwest::Client::new();
    let mut req = client.request(reqwest_method, &final_url);
    for (k, v) in &headers {
        req = req.header(k, v);
    }
    if body_method {
        req = req.json(&serde_json::Value::Object(remaining));
    }
    let resp = req
        .timeout(Duration::from_secs(config.timeout_secs))
        .send()
        .await
        .unwrap_or_else(|e| panic!("请求失败: {e}"));

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let body = truncate_chars(&body, MAX_BODY_CHARS);

    if status.is_success() {
        AgentToolResult {
            content: vec![TextOrImageContent::Text(TextContent {
                kind: TextKind,
                text: format!("HTTP {}\n\n{body}", status.as_u16()),
                text_signature: None,
            })],
            details: serde_json::json!({ "status": status.as_u16() }),
            usage: None,
            added_tool_names: None,
            terminate: false,
        }
    } else {
        panic!("HTTP {}: {body}", status.as_u16());
    }
}

fn apply_auth(
    auth: &AuthConfig,
    headers: &mut BTreeMap<String, String>,
    query_params: &mut Vec<(String, String)>,
) {
    match auth {
        AuthConfig::Bearer { token_env } => {
            let token = env_token(token_env);
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        }
        AuthConfig::Header {
            header_name,
            token_env,
        } => {
            let token = env_token(token_env);
            headers.insert(header_name.clone(), token);
        }
        AuthConfig::Basic {
            username_env,
            password_env,
        } => {
            let username = env_token(username_env);
            let password = env_token(password_env);
            let encoded = STANDARD.encode(format!("{username}:{password}"));
            headers.insert("Authorization".to_string(), format!("Basic {encoded}"));
        }
        AuthConfig::QueryToken {
            param_name,
            token_env,
        } => {
            let token = env_token(token_env);
            query_params.push((param_name.clone(), token));
        }
    }
}

fn env_token(name: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => panic!("缺少环境变量: {name}"),
    }
}

/// 提取 URL 模板中的 `{name}` 占位符（按出现顺序，去重）。
pub fn extract_placeholders(url: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut chars = url.chars();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut name = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                name.push(c);
            }
            if !name.is_empty() && !result.contains(&name) {
                result.push(name);
            }
        }
    }
    result
}

/// 用参数填充 URL 占位符（path 段做 percent-encoding）。
pub fn fill_url(
    url: &str,
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    let mut result = String::new();
    let mut chars = url.chars();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut name = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                name.push(c);
            }
            let value = params
                .get(name.as_str())
                .ok_or_else(|| format!("缺少 path 参数: {name}"))?;
            result.push_str(&urlencode(&value_to_string(value)));
        } else {
            result.push(c);
        }
    }
    Ok(result)
}

/// 把 JSON 值转为字符串（用于 query/path 参数）。
fn value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 对 query/path 参数做 percent-encoding（保留 unreserved 字符）。
fn urlencode(s: &str) -> String {
    let mut result = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char)
            }
            _ => result.push_str(&format!("%{b:02X}")),
        }
    }
    result
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated}\n\n[... 响应已截断 ...]")
}

/// 若 headers 中没有 User-Agent（不区分大小写），补一个默认值。
fn ensure_user_agent(headers: &mut BTreeMap<String, String>) {
    if !headers.keys().any(|k| k.eq_ignore_ascii_case("user-agent")) {
        headers.insert("User-Agent".to_string(), "pi-tools".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_default_user_agent_when_missing() {
        let mut headers = BTreeMap::new();
        ensure_user_agent(&mut headers);
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some("pi-tools")
        );
    }

    #[test]
    fn keeps_existing_user_agent() {
        let mut headers = BTreeMap::from([("User-Agent".to_string(), "custom".to_string())]);
        ensure_user_agent(&mut headers);
        assert_eq!(
            headers.get("User-Agent").map(String::as_str),
            Some("custom")
        );
    }

    #[test]
    fn detects_user_agent_case_insensitively() {
        let mut headers = BTreeMap::from([("user-agent".to_string(), "x".to_string())]);
        ensure_user_agent(&mut headers);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("user-agent").map(String::as_str), Some("x"));
    }
}
