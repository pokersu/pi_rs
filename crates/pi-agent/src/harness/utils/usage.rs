//! Rust 翻译自 packages/agent/src/harness/utils/usage.ts

use pi_ai::{Usage, UsageCost};

/// 对应 `emptyUsage`。
pub fn empty_usage() -> Usage {
    Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: UsageCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    }
}

/// 对应 `addUsage`。
pub fn add_usage(left: &Usage, right: &Usage) -> Usage {
    Usage {
        input: left.input + right.input,
        output: left.output + right.output,
        cache_read: left.cache_read + right.cache_read,
        cache_write: left.cache_write + right.cache_write,
        cache_write_1h: match (left.cache_write_1h, right.cache_write_1h) {
            (None, None) => None,
            (l, r) => Some(l.unwrap_or(0) + r.unwrap_or(0)),
        },
        reasoning: match (left.reasoning, right.reasoning) {
            (None, None) => None,
            (l, r) => Some(l.unwrap_or(0) + r.unwrap_or(0)),
        },
        total_tokens: left.total_tokens + right.total_tokens,
        cost: UsageCost {
            input: left.cost.input + right.cost.input,
            output: left.cost.output + right.cost.output,
            cache_read: left.cost.cache_read + right.cost.cache_read,
            cache_write: left.cost.cache_write + right.cost.cache_write,
            total: left.cost.total + right.cost.total,
        },
    }
}
