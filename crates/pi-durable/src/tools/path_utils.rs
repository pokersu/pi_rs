//! 对应 `tools/path-utils.ts`：工具路径的归一化与读取变体解析。

use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use crate::chord::context::Context;
use crate::env::ExecutionEnv;
use crate::session::SessionError;

const NARROW_NO_BREAK_SPACE: &str = "\u{202F}";

static UNICODE_SPACES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("[\u{00A0}\u{2000}-\u{200A}\u{202F}\u{205F}\u{3000}]").expect("unicode spaces")
});

static AMPM_DOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(" (?<ampm>AM|PM)\\.").expect("am/pm"));

fn normalize_tool_path(path: &str) -> String {
    let normalized = UNICODE_SPACES.replace_all(path, " ");
    normalized
        .strip_prefix('@')
        .map_or_else(|| normalized.to_string(), str::to_string)
}

/// 对应 `resolveToolPath`。
pub async fn resolve_tool_path(
    env: &dyn ExecutionEnv,
    path: &str,
    context: &dyn Context,
) -> Result<String, SessionError> {
    Ok(env
        .absolute_path(&normalize_tool_path(path), context)
        .await?)
}

/// 对应 `resolveReadToolPath`：尝试 macOS 变体，找到存在的那个；否则返回归一化路径。
pub async fn resolve_read_tool_path(
    env: &dyn ExecutionEnv,
    path: &str,
    context: &dyn Context,
) -> Result<String, SessionError> {
    let resolved = resolve_tool_path(env, path, context).await?;
    let nfd = resolved.nfd().collect::<String>();
    let replacement = format!("{NARROW_NO_BREAK_SPACE}$ampm.");
    let variants = [
        resolved.clone(),
        AMPM_DOT
            .replace_all(&resolved, replacement.as_str())
            .to_string(),
        nfd.clone(),
        resolved.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ];
    for variant in variants {
        if env.exists(&variant, context).await? {
            return Ok(variant);
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_leading_at_and_unicode_spaces() {
        assert_eq!(normalize_tool_path("@/a\u{00A0}b"), "/a b");
        assert_eq!(normalize_tool_path("/plain"), "/plain");
    }
}
