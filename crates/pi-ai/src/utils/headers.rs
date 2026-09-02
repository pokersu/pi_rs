//! Rust 翻译自 packages/ai/src/utils/headers.ts

use std::collections::BTreeMap;

use reqwest::header::HeaderMap;

use crate::types::ProviderHeaders;

/// 对应 `headersToRecord(headers)`：将 `reqwest` 的 `HeaderMap` 转为字符串映射。
pub fn headers_to_record(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

/// 对应 `providerHeadersToRecord(headers)`：过滤 null 值；空映射返回 `None`。
pub fn provider_headers_to_record(
    headers: Option<&ProviderHeaders>,
) -> Option<BTreeMap<String, String>> {
    let headers = headers?;
    let result: BTreeMap<String, String> = headers
        .iter()
        .filter_map(|(name, value)| value.as_ref().map(|v| (name.clone(), v.clone())))
        .collect();
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}
