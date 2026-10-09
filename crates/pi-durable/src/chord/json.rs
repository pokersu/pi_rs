//! 对应 chord `src/json.ts`。

pub use serde_json::Value as JsonValue;

/// 对应 `copyJson`。
///
/// 上游实现做严格 JSON 校验：拒绝非有限数、循环引用、稀疏数组与带额外键的数组。
/// Rust 侧输入类型已经是 `serde_json::Value`，这些约束由类型与 serde 解析共同保证，
/// 因此这里等价于深拷贝。
pub fn copy_json(value: &JsonValue) -> JsonValue {
    value.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn copy_json_produces_independent_value() {
        let original = json!({ "a": [1, 2, { "b": "c" }] });
        let mut copied = copy_json(&original);
        copied["a"][2]["b"] = json!("changed");

        assert_eq!(original["a"][2]["b"], json!("c"), "拷贝不应影响原值");
        assert_eq!(copied["a"][2]["b"], json!("changed"));
    }
}
