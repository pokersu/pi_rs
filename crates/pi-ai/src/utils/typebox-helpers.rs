//! Rust 翻译自 packages/ai/src/utils/typebox-helpers.ts
//!
//! TypeBox 是 JS 专属的 schema 库，这里用 `serde_json::Value` 直接构造等价的
//! JSON Schema 节点，保持 API 语义对齐。

use serde_json::{Value, json};

/// 对应 `StringEnum(values, options?)` 的选项。
#[derive(Debug, Clone, Default)]
pub struct StringEnumOptions {
    pub description: Option<String>,
    pub default: Option<String>,
}

/// 对应 `StringEnum(values, options?)`：构造 `{ type: "string", enum, ... }`。
pub fn string_enum(values: &[&str], options: Option<StringEnumOptions>) -> Value {
    let mut schema = json!({
        "type": "string",
        "enum": values,
    });
    if let Some(options) = options {
        if let Some(description) = options.description {
            schema["description"] = json!(description);
        }
        if let Some(default) = options.default {
            schema["default"] = json!(default);
        }
    }
    schema
}
