//! Rust 翻译自 packages/ai/src/utils/node-http-proxy.ts
//!
//! 根据目标 URL 与 `no_proxy`/`{protocol}_proxy`/`all_proxy` 环境变量解析 HTTP 代理。

use crate::types::ProviderEnv;

const DEFAULT_PROXY_PORTS: &[(&str, u16)] = &[
    ("ftp", 21),
    ("gopher", 70),
    ("http", 80),
    ("https", 443),
    ("ws", 80),
    ("wss", 443),
];

fn default_proxy_port(protocol: &str) -> u16 {
    DEFAULT_PROXY_PORTS
        .iter()
        .find(|(p, _)| *p == protocol)
        .map(|(_, port)| *port)
        .unwrap_or(0)
}

fn get_proxy_env(key: &str, env: Option<&ProviderEnv>) -> String {
    let lowercase = key.to_lowercase();
    let uppercase = key.to_uppercase();
    crate::utils::provider_env::get_provider_env_value(&lowercase, env)
        .or_else(|| crate::utils::provider_env::get_provider_env_value(&uppercase, env))
        .unwrap_or_default()
}

fn should_proxy_hostname(hostname: &str, port: u16, env: Option<&ProviderEnv>) -> bool {
    let no_proxy = get_proxy_env("no_proxy", env).to_lowercase();
    if no_proxy.is_empty() {
        return true;
    }
    if no_proxy == "*" {
        return false;
    }

    no_proxy
        .split(|c: char| c == ',' || c.is_whitespace())
        .all(|proxy| {
            if proxy.is_empty() {
                return true;
            }

            // 解析 "hostname:port" 形式。
            let (proxy_hostname, proxy_port) = match proxy.rsplit_once(':') {
                Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                    (h, p.parse::<u16>().unwrap_or(0))
                }
                _ => (proxy, 0),
            };
            if proxy_port != 0 && proxy_port != port {
                return true;
            }

            if !proxy_hostname.starts_with('.') && !proxy_hostname.starts_with('*') {
                return hostname != proxy_hostname;
            }
            let suffix = proxy_hostname.trim_start_matches('*');
            !hostname.ends_with(suffix)
        })
}

fn get_proxy_for_url(target_url: &str, env: Option<&ProviderEnv>) -> String {
    let Ok(parsed) = reqwest::Url::parse(target_url) else {
        return String::new();
    };
    if parsed.scheme().is_empty() || parsed.host_str().is_none() {
        return String::new();
    }

    let protocol = parsed.scheme();
    let hostname = parsed.host_str().unwrap_or("");
    let port = parsed
        .port()
        .unwrap_or_else(|| default_proxy_port(protocol));
    if !should_proxy_hostname(hostname, port, env) {
        return String::new();
    }

    let mut proxy = get_proxy_env(&format!("{protocol}_proxy"), env);
    if proxy.is_empty() {
        proxy = get_proxy_env("all_proxy", env);
    }
    if !proxy.is_empty() && !proxy.contains("://") {
        proxy = format!("{protocol}://{proxy}");
    }
    proxy
}

/// 对应 `UNSUPPORTED_PROXY_PROTOCOL_MESSAGE`。
pub const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE: &str = "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";

/// 对应 `resolveHttpProxyUrlForTarget(targetUrl, env?)`。
pub fn resolve_http_proxy_url_for_target(
    target_url: &str,
    env: Option<&ProviderEnv>,
) -> Result<Option<reqwest::Url>, String> {
    let proxy = get_proxy_for_url(target_url, env);
    if proxy.is_empty() {
        return Ok(None);
    }

    let proxy_url = reqwest::Url::parse(&proxy)
        .map_err(|error| format!("Invalid proxy URL {proxy:?}: {error}"))?;
    if proxy_url.scheme() != "http" && proxy_url.scheme() != "https" {
        return Err(format!(
            "{UNSUPPORTED_PROXY_PROTOCOL_MESSAGE} Got {}:",
            proxy_url.scheme()
        ));
    }
    Ok(Some(proxy_url))
}
