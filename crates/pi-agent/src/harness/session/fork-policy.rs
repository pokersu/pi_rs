//! Rust 翻译自 packages/agent/src/harness/session/fork-policy.ts

/// 对应 `ForkDisposition`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkDisposition {
    Copy,
    Exclude,
    Reconstruct,
}

/// 对应 `ForkScope`（ForkOptions 的 scope 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkScope {
    Branch,
    Tree,
}

/// 对应 `classifyForkAddress`：决定某个 scalar/list 地址的最终 fork 动作。
pub fn classify_fork_address(
    namespace: &str,
    key: &str,
    scope: ForkScope,
    is_entry_copied: impl Fn(&str) -> bool,
) -> ForkDisposition {
    match namespace {
        "pi.session.name" => return ForkDisposition::Copy,
        "pi.entry.label" => {
            return if is_entry_copied(key) {
                ForkDisposition::Copy
            } else {
                ForkDisposition::Exclude
            };
        }
        "pi.branch.tip" | "pi.lane.config" | "pi.lane.state" => {
            return ForkDisposition::Reconstruct;
        }
        "pi.result" => return ForkDisposition::Exclude,
        _ => {}
    }
    if namespace.starts_with("pi.op.") || namespace.starts_with("pi.pending.") {
        return ForkDisposition::Exclude;
    }
    if namespace == "pi" || namespace.starts_with("pi.") {
        panic!("Unknown reserved fork namespace: {namespace}");
    }
    match scope {
        ForkScope::Tree => ForkDisposition::Copy,
        ForkScope::Branch => ForkDisposition::Exclude,
    }
}
