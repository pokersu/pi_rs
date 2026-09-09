//! Rust 翻译自 packages/agent/src/harness/config.ts

use std::collections::HashSet;

use pi_ai::utils::retry::RetryPolicy;

use crate::harness::compaction::compaction::CompactionSettings;

/// 对应 `DEFAULT_RETRY_POLICY`。
pub const DEFAULT_RETRY_POLICY: RetryPolicy = RetryPolicy {
    enabled: true,
    max_retries: 3,
    base_delay_ms: 1_000,
    max_agent_delay_ms: Some(pi_ai::utils::retry::DEFAULT_MAX_AGENT_RETRY_DELAY_MS),
};

/// 对应 `validateToolNames`。
pub fn validate_tool_names(tools: &[impl AsRef<str>]) {
    let mut names = HashSet::new();
    for tool in tools {
        let name = tool.as_ref();
        if !names.insert(name.to_string()) {
            panic!("Duplicate tool name: {name:?}");
        }
    }
}

/// 对应 `validateRetryPolicy`。
pub fn validate_retry_policy(policy: &RetryPolicy) {
    if policy.max_retries == u64::MAX
        || policy.base_delay_ms == u64::MAX
        || policy.max_agent_delay_ms == Some(u64::MAX)
    {
        panic!("Retry policy values must be finite non-negative safe integers");
    }
}

/// 对应 `validateCompactionSettings`。
pub fn validate_compaction_settings(settings: &CompactionSettings) {
    if settings.reserve_tokens == u64::MAX || settings.keep_recent_tokens == u64::MAX {
        panic!("Compaction token counts must be finite non-negative safe integers");
    }
}
