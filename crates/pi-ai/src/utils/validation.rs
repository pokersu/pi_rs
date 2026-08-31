//! Rust 翻译自 packages/ai/src/utils/validation.ts
//!
//! TS 原版用 TypeBox（`Compile`/`Value.Convert`）做运行时校验；Rust 中用
//! `jsonschema` crate 校验，并完整保留原版的 JSON schema 强制转换（coercion）逻辑。

use std::collections::HashSet;

use serde_json::Value;

use crate::types::{Tool, ToolCall};

/// 对应 `getSchemaTypes`
fn get_schema_types(schema: &Value) -> Vec<String> {
    let Some(type_value) = schema.get("type") else {
        return Vec::new();
    };
    if let Some(s) = type_value.as_str() {
        return vec![s.to_string()];
    }
    if let Some(arr) = type_value.as_array() {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    Vec::new()
}

/// 对应 `matchesJsonType`
fn matches_json_type(value: &Value, type_name: &str) -> bool {
    match type_name {
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

/// 对应 `getSubSchemaValidator`：构造子 schema 的校验器，失败返回 None。
fn sub_schema_valid(schema: &Value, value: &Value) -> Option<bool> {
    jsonschema::validator_for(schema)
        .ok()
        .map(|validator| validator.is_valid(value))
}

/// 对应 `coercePrimitiveByType`
fn coerce_primitive_by_type(value: Value, type_name: &str) -> Value {
    match type_name {
        "number" => {
            if value.is_null() {
                return Value::from(0);
            }
            if let Some(s) = value.as_str()
                && !s.trim().is_empty()
                && let Ok(n) = s.parse::<f64>()
                && n.is_finite()
            {
                return Value::from(n);
            }
            if let Some(b) = value.as_bool() {
                return Value::from(if b { 1 } else { 0 });
            }
            value
        }
        "integer" => {
            if value.is_null() {
                return Value::from(0);
            }
            if let Some(s) = value.as_str()
                && !s.trim().is_empty()
                && let Ok(n) = s.parse::<i64>()
            {
                return Value::from(n);
            }
            if let Some(b) = value.as_bool() {
                return Value::from(if b { 1 } else { 0 });
            }
            value
        }
        "boolean" => {
            if value.is_null() {
                return Value::from(false);
            }
            if let Some(s) = value.as_str() {
                if s == "true" {
                    return Value::from(true);
                }
                if s == "false" {
                    return Value::from(false);
                }
            }
            if let Some(n) = value.as_f64() {
                if n == 1.0 {
                    return Value::from(true);
                }
                if n == 0.0 {
                    return Value::from(false);
                }
            }
            value
        }
        "string" => {
            if value.is_null() {
                return Value::from("");
            }
            if value.is_number() || value.is_boolean() {
                return Value::from(value.to_string());
            }
            value
        }
        "null" => {
            if value.as_str() == Some("")
                || value.as_f64() == Some(0.0)
                || value.as_bool() == Some(false)
            {
                return Value::Null;
            }
            value
        }
        _ => value,
    }
}

/// 对应 `applySchemaObjectCoercion`
fn apply_schema_object_coercion(value: &mut Value, schema: &Value) {
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    let defined_keys: HashSet<&str> = properties.keys().map(|k| k.as_str()).collect();

    // 对 properties 中已存在的 key 做 coercion
    for (key, property_schema) in properties {
        if let Some(v) = value.get_mut(key) {
            let coerced = coerce_with_json_schema(v.take(), property_schema);
            *v = coerced;
        }
    }

    // additionalProperties 为 object 时，对未定义 key 做 coercion
    if let Some(additional) = schema.get("additionalProperties")
        && additional.is_object()
    {
        let keys: Vec<String> = value
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        for key in keys {
            if defined_keys.contains(key.as_str()) {
                continue;
            }
            if let Some(v) = value.get_mut(&key) {
                let coerced = coerce_with_json_schema(v.take(), additional);
                *v = coerced;
            }
        }
    }
}

/// 对应 `applySchemaArrayCoercion`
fn apply_schema_array_coercion(value: &mut Value, schema: &Value) {
    let Some(items) = schema.get("items") else {
        return;
    };
    if let Some(item_schemas) = items.as_array() {
        if let Some(arr) = value.as_array_mut() {
            for (index, item) in arr.iter_mut().enumerate() {
                let Some(item_schema) = item_schemas.get(index) else {
                    continue;
                };
                let coerced = coerce_with_json_schema(item.take(), item_schema);
                *item = coerced;
            }
        }
        return;
    }
    if items.is_object()
        && let Some(arr) = value.as_array_mut()
    {
        for item in arr.iter_mut() {
            let coerced = coerce_with_json_schema(item.take(), items);
            *item = coerced;
        }
    }
}

/// 对应 `coerceWithUnionSchema`
fn coerce_with_union_schema(value: Value, schemas: &[Value]) -> Value {
    // 第一遍：已满足任一子 schema 则原样返回。
    for schema in schemas {
        if sub_schema_valid(schema, &value) == Some(true) {
            return value;
        }
    }
    // 第二遍：尝试 coercion 后校验。
    for schema in schemas {
        let candidate = value.clone();
        let coerced = coerce_with_json_schema(candidate, schema);
        if sub_schema_valid(schema, &coerced) == Some(true) {
            return coerced;
        }
    }
    value
}

/// 对应 `coerceWithJsonSchema`
fn coerce_with_json_schema(value: Value, schema: &Value) -> Value {
    let mut next_value = value;

    if let Some(all_of) = schema.get("allOf").and_then(|v| v.as_array()) {
        for nested in all_of {
            next_value = coerce_with_json_schema(next_value, nested);
        }
    }
    if let Some(any_of) = schema.get("anyOf").and_then(|v| v.as_array()) {
        next_value = coerce_with_union_schema(next_value, any_of);
    }
    if let Some(one_of) = schema.get("oneOf").and_then(|v| v.as_array()) {
        next_value = coerce_with_union_schema(next_value, one_of);
    }

    let schema_types = get_schema_types(schema);
    let matches_union_member = schema_types.len() > 1
        && schema_types
            .iter()
            .any(|t| matches_json_type(&next_value, t));
    if !schema_types.is_empty() && !matches_union_member {
        for type_name in &schema_types {
            let candidate = coerce_primitive_by_type(next_value.clone(), type_name);
            if candidate != next_value {
                next_value = candidate;
                break;
            }
        }
    }

    if schema_types.iter().any(|t| t == "object") && next_value.is_object() {
        apply_schema_object_coercion(&mut next_value, schema);
    }
    if schema_types.iter().any(|t| t == "array") && next_value.is_array() {
        apply_schema_array_coercion(&mut next_value, schema);
    }

    next_value
}

/// 对应 `normalizeOptionalNulls`
fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    if value.is_array() {
        if let Some(items) = schema.get("items") {
            if let Some(item_schemas) = items.as_array() {
                let len = item_schemas.len();
                for index in 0..len {
                    if let Some(item_schema) = item_schemas.get(index)
                        && let Some(v) = value.get_mut(index)
                    {
                        normalize_optional_nulls(v, item_schema);
                    }
                }
            } else if items.is_object()
                && let Some(arr) = value.as_array_mut()
            {
                for item in arr.iter_mut() {
                    normalize_optional_nulls(item, items);
                }
            }
        }
        return;
    }
    if !value.is_object() {
        return;
    }
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return;
    };
    let required: HashSet<String> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|r| {
            r.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let keys: Vec<String> = value
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    for key in keys {
        let Some(property_schema) = properties.get(&key) else {
            continue;
        };
        let is_null = value.get(&key).map(|v| v.is_null()).unwrap_or(false);
        if is_null
            && !required.contains(&key)
            && property_schema
                .get("$ref")
                .and_then(|r| r.as_str())
                .is_none()
        {
            let accepts_null = sub_schema_valid(property_schema, &Value::Null).unwrap_or(true);
            if !accepts_null {
                if let Some(obj) = value.as_object_mut() {
                    obj.remove(&key);
                }
                continue;
            }
        }
        if let Some(v) = value.get_mut(&key) {
            normalize_optional_nulls(v, property_schema);
        }
    }
}

/// 对应 `formatValidationPath`
fn format_validation_path(instance_path: &str) -> String {
    let path = instance_path.trim_start_matches('/').replace('/', ".");
    if path.is_empty() {
        "root".to_string()
    } else {
        path
    }
}

/// 对应 `validateToolCall`：按名称查找工具并校验参数。
pub fn validate_tool_call(
    tools: &[Tool],
    tool_call: &ToolCall,
) -> Result<serde_json::Value, String> {
    let tool = tools
        .iter()
        .find(|t| t.name == tool_call.name)
        .ok_or_else(|| format!("Tool \"{}\" not found", tool_call.name))?;
    validate_tool_arguments(tool, tool_call)
}

/// 对应 `validateToolArguments`：校验（并可能强制转换）工具调用参数。
pub fn validate_tool_arguments(
    tool: &Tool,
    tool_call: &ToolCall,
) -> Result<serde_json::Value, String> {
    let mut args = tool_call.arguments.clone();
    normalize_optional_nulls(&mut args, &tool.parameters);

    // 非 TypeBox schema 的 coercion（Rust 中统一执行 coercion）。
    let coerced = coerce_with_json_schema(args.clone(), &tool.parameters);
    if coerced != args {
        args = coerced;
    }

    let validator = jsonschema::validator_for(&tool.parameters)
        .map_err(|e| format!("Failed to compile tool schema: {e}"))?;

    if validator.is_valid(&args) {
        return Ok(args);
    }

    let errors: Vec<String> = validator
        .iter_errors(&args)
        .map(|e| {
            format!(
                "  - {}: {e}",
                format_validation_path(&e.instance_path.to_string())
            )
        })
        .collect();
    let joined = if errors.is_empty() {
        "Unknown validation error".to_string()
    } else {
        errors.join("\n")
    };

    let message = format!(
        "Validation failed for tool \"{}\":\n{}\n\nReceived arguments:\n{}",
        tool_call.name,
        joined,
        serde_json::to_string_pretty(&tool_call.arguments).unwrap_or_else(|_| "{}".to_string())
    );

    Err(message)
}
