//! Rust 翻译自 packages/ai/src/api/constrained-sampling.ts
//!
//! 工具参数的严格 JSON Schema 与 grammar 约束采样。

use std::collections::HashMap;

use serde_json::Value;

use crate::types::{ConstrainedSamplingConfig, StrictMode, Tool};

const UNSUPPORTED_STRICT_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn is_json_schema_object(value: &Value) -> bool {
    value.is_object()
}

fn is_structured_schema(schema: &Value) -> bool {
    if !is_json_schema_object(schema) {
        return false;
    }
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    types.iter().any(|t| *t == "object" || *t == "array")
        || schema.get("properties").is_some()
        || schema.get("items").is_some()
}

fn schema_allows_null(schema: &Value) -> bool {
    if !is_json_schema_object(schema) {
        return false;
    }
    if schema.get("type").and_then(|t| t.as_str()) == Some("null")
        || schema
            .get("type")
            .and_then(|t| t.as_array())
            .map(|arr| arr.iter().any(|v| v.as_str() == Some("null")))
            .unwrap_or(false)
    {
        return true;
    }
    if schema.get("const").and_then(|v| v.as_null()).is_some()
        || schema
            .get("enum")
            .and_then(|e| e.as_array())
            .map(|arr| arr.iter().any(|v| v.is_null()))
            .unwrap_or(false)
    {
        return true;
    }
    schema
        .get("anyOf")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().any(schema_allows_null))
        .unwrap_or(false)
}

fn make_json_schema_node_strict(schema: &mut Value) -> Result<(), String> {
    if !is_json_schema_object(schema) {
        return Err("boolean schemas are unsupported".to_string());
    }
    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if schema.get(*key).is_some() {
            return Err(format!("{key} schemas are unsupported"));
        }
    }

    if let Some(any_of) = schema.get("anyOf") {
        let arr = any_of
            .as_array()
            .ok_or_else(|| "anyOf must contain at least one schema".to_string())?;
        if arr.is_empty() {
            return Err("anyOf must contain at least one schema".to_string());
        }
        for variant in arr.clone() {
            if is_structured_schema(&variant) {
                return Err("object and array unions are unsupported".to_string());
            }
            let mut variant = variant;
            make_json_schema_node_strict(&mut variant)?;
        }
    }

    if let Some(items) = schema.get("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".to_string());
        }
        let mut items = items.clone();
        make_json_schema_node_strict(&mut items)?;
    }

    let is_object_schema = schema.get("type").and_then(|t| t.as_str()) == Some("object");
    if schema.get("properties").is_some() && !is_object_schema {
        return Err("properties require type object".to_string());
    }
    if !is_object_schema {
        return Ok(());
    }
    if let Some(additional) = schema.get("additionalProperties")
        && additional != &Value::Bool(false)
    {
        return Err("schema-valued or true additionalProperties is unsupported".to_string());
    }
    if schema.get("properties").is_some() && !schema.get("properties").unwrap().is_object() {
        return Err("object properties must be a schema map".to_string());
    }
    if let Some(required) = schema.get("required")
        && (!required.is_array() || required.as_array().unwrap().iter().any(|k| !k.is_string()))
    {
        return Err("object required must be a string array".to_string());
    }

    let properties = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default();
    let property_names: Vec<String> = properties.keys().cloned().collect();
    let required: Vec<String> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if required.iter().any(|key| !property_names.contains(key)) {
        return Err("required contains an unknown property".to_string());
    }

    let mut new_properties = serde_json::Map::new();
    for (key, property) in &properties {
        let mut property = property.clone();
        make_json_schema_node_strict(&mut property)?;
        if !required.contains(key) && !schema_allows_null(&property) {
            property = serde_json::json!({ "anyOf": [property, { "type": "null" }] });
        }
        new_properties.insert(key.clone(), property);
    }

    let schema_obj = schema.as_object_mut().unwrap();
    schema_obj.insert("properties".to_string(), Value::Object(new_properties));
    schema_obj.insert(
        "required".to_string(),
        Value::Array(property_names.into_iter().map(Value::String).collect()),
    );
    schema_obj.insert("additionalProperties".to_string(), Value::Bool(false));
    Ok(())
}

/// 对应 `makeStrictJsonSchema`。
pub fn make_strict_json_schema(schema: &Value) -> Result<Value, String> {
    let mut cloned = schema.clone();
    if !is_json_schema_object(&cloned) {
        return Err("root schema must have type object".to_string());
    }
    make_json_schema_node_strict(&mut cloned)?;
    if cloned.get("type").and_then(|t| t.as_str()) != Some("object") {
        return Err("root schema must have type object".to_string());
    }
    Ok(cloned)
}

/// 对应 `getJsonSchemaToolParameters`。
pub fn get_json_schema_tool_parameters(tool: &Tool, strict: Option<bool>) -> Value {
    if strict == Some(true) {
        make_strict_json_schema(&tool.parameters).unwrap_or_else(|_| tool.parameters.clone())
    } else {
        tool.parameters.clone()
    }
}

/// 对应 `GrammarConstrainedSampling`。
#[derive(Debug, Clone)]
pub struct GrammarConstrainedSampling {
    pub format: String,
    pub definition: String,
    pub input_property: String,
}

/// 对应 `GrammarToolInputJsonBuffer`。
#[derive(Debug, Clone, Default)]
pub struct GrammarToolInputJsonBuffer {
    pub input: String,
    pub started: bool,
    pub closed: bool,
}

/// 对应 `getGrammarToolInput`。
pub fn get_grammar_tool_input(
    tool_name: &str,
    arguments: &Value,
    input_property: &str,
) -> Result<String, String> {
    let input = arguments.get(input_property);
    match input.and_then(|v| v.as_str()) {
        Some(s) => Ok(s.to_string()),
        _ => Err(format!(
            "Grammar tool call \"{tool_name}\" requires argument \"{input_property}\" to be a string."
        )),
    }
}

/// 对应 `appendGrammarToolInputJsonDelta`。
pub fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarToolInputJsonBuffer,
    input_property: &str,
    next_input: &str,
    close: bool,
) -> Result<Option<String>, String> {
    if buffer.closed {
        if close && next_input == buffer.input {
            return Ok(None);
        }
        return Err(format!(
            "grammar tool input for property \"{input_property}\" changed after it was closed"
        ));
    }
    if !next_input.starts_with(&buffer.input) {
        return Err(format!(
            "grammar tool input for property \"{input_property}\" changed non-monotonically"
        ));
    }

    let input_delta = &next_input[buffer.input.len()..];
    if !close && input_delta.is_empty() {
        return Ok(None);
    }

    let mut delta = String::new();
    if !buffer.started {
        delta.push_str(&format!("{{\"{input_property}\":\""));
        buffer.started = true;
    }
    let escaped = serde_json::to_string(input_delta).unwrap_or_else(|_| "\"\"".to_string());
    delta.push_str(&escaped[1..escaped.len() - 1]);
    buffer.input = next_input.to_string();

    if close {
        delta.push_str("\"}");
        buffer.closed = true;
    }
    Ok(Some(delta))
}

fn infer_grammar_input_property(tool: &Tool) -> Result<String, String> {
    let schema = &tool.parameters;
    if schema.get("type").and_then(|t| t.as_str()) != Some("object") {
        return Err("grammar constrained sampling requires an object parameter schema".to_string());
    }
    let required = schema
        .get("required")
        .and_then(|r| r.as_array())
        .ok_or_else(|| {
            "grammar constrained sampling requires exactly one required string property".to_string()
        })?;
    if required.len() != 1 || required[0].as_str().is_none() {
        return Err(
            "grammar constrained sampling requires exactly one required string property"
                .to_string(),
        );
    }
    let input_property = required[0].as_str().unwrap();
    let properties = schema.get("properties").and_then(|p| p.as_object());
    if properties.and_then(|p| p.get(input_property)).is_none() {
        return Err(format!(
            "grammar constrained sampling requires a properties entry for {input_property}"
        ));
    }
    if properties
        .and_then(|p| p.get(input_property))
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str())
        != Some("string")
    {
        return Err(format!(
            "grammar constrained sampling property {input_property} must have type string"
        ));
    }
    Ok(input_property.to_string())
}

/// 对应 `resolveJsonSchemaStrictSampling`。
pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<bool>, String> {
    let Some(config) = &tool.constrained_sampling else {
        return Ok(None);
    };
    let strict = match config {
        ConstrainedSamplingConfig::JsonSchema { strict } => *strict,
        ConstrainedSamplingConfig::Grammar { .. } => return Ok(None),
    };

    if supports_strict_mode {
        match make_strict_json_schema(&tool.parameters) {
            Ok(_) => return Ok(Some(true)),
            Err(error) => {
                if strict != StrictMode::Require {
                    return Ok(None);
                }
                return Err(format!(
                    "Tool \"{}\" requires JSON-schema constrained sampling, but {error}.",
                    tool.name
                ));
            }
        }
    }
    if strict == StrictMode::Require {
        return Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ));
    }
    Ok(None)
}

/// 对应 `resolveGrammarConstrainedSampling`。
pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_openai_grammar_tools: bool,
) -> Result<Option<GrammarConstrainedSampling>, String> {
    let Some(config) = &tool.constrained_sampling else {
        return Ok(None);
    };
    let variants = match config {
        ConstrainedSamplingConfig::Grammar { variants } => variants,
        ConstrainedSamplingConfig::JsonSchema { .. } => return Ok(None),
    };
    if !supports_openai_grammar_tools {
        return Ok(None);
    }

    let lark = variants.get("openai_lark").map(|s| s.as_str());
    let regex = variants.get("openai_regex").map(|s| s.as_str());
    let has_lark = lark.map(|s| !s.trim().is_empty()).unwrap_or(false);
    let has_regex = regex.map(|s| !s.trim().is_empty()).unwrap_or(false);
    if !has_lark && !has_regex {
        return Err(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
            tool.name
        ));
    }

    match infer_grammar_input_property(tool) {
        Ok(input_property) => Ok(Some(GrammarConstrainedSampling {
            format: if has_lark { "lark" } else { "regex" }.to_string(),
            definition: if has_lark {
                lark.unwrap()
            } else {
                regex.unwrap()
            }
            .to_string(),
            input_property,
        })),
        Err(message) => Err(format!(
            "Tool \"{}\" cannot use grammar constrained sampling: {message}.",
            tool.name
        )),
    }
}

/// 对应 `createGrammarToolInputProperties`。
pub fn create_grammar_tool_input_properties(
    tools: Option<&[Tool]>,
    supports_openai_grammar_tools: bool,
) -> HashMap<String, String> {
    let mut properties = HashMap::new();
    for tool in tools.unwrap_or(&[]) {
        if let Ok(Some(grammar)) =
            resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)
        {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    properties
}
