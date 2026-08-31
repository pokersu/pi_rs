//! Rust 翻译自 packages/agent/src/harness/tools/path-utils.ts

use std::sync::Arc;

use pi_ai::AbortSignal;

use crate::harness::result::get_or_throw;
use crate::harness::types::ExecutionEnv;

fn normalize_tool_path(path: &str) -> String {
    let normalized: String = path
        .chars()
        .map(|c| match c {
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect();
    if let Some(stripped) = normalized.strip_prefix('@') {
        stripped.to_string()
    } else {
        normalized
    }
}

/// 对应 `resolveToolPath`
pub async fn resolve_tool_path(
    env: &Arc<dyn ExecutionEnv>,
    path: &str,
    signal: Option<&AbortSignal>,
) -> String {
    get_or_throw(env.absolute_path(&normalize_tool_path(path), signal).await)
}

/// 对应 `resolveReadToolPath`
pub async fn resolve_read_tool_path(
    env: &Arc<dyn ExecutionEnv>,
    path: &str,
    signal: Option<&AbortSignal>,
) -> String {
    let resolved = resolve_tool_path(env, path, signal).await;
    let variants = vec![
        resolved.clone(),
        resolved
            .replace(" AM.", "\u{202F}AM.")
            .replace(" PM.", "\u{202F}PM."),
        resolved.replace('\'', "\u{2019}"),
    ];
    for variant in variants {
        if get_or_throw(env.exists(&variant, signal).await) {
            return variant;
        }
    }
    resolved
}
