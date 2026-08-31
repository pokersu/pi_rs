//! Rust 翻译自 packages/ai/src/utils/json-parse.ts
//!
//! 注：TS 的 `parseStreamingJson` 依赖 `partial-json` 包做不完整 JSON 的补全解析；
//! Rust 中先以 `repair_json` + 标准解析近似（失败返回空对象），精确的 partial parse
//! 后续按需补充。

use serde::de::DeserializeOwned;

const VALID_JSON_ESCAPES: &[char] = &['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

fn is_control_character(c: char) -> bool {
    (c as u32) <= 0x1f
}

fn escape_control_character(c: char) -> String {
    match c {
        '\u{8}' => "\\b".to_string(),
        '\u{c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        _ => format!("\\u{:04x}", c as u32),
    }
}

/// 对应 `repairJson`：修复畸形 JSON 字符串字面量。
pub fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut repaired = String::new();
    let mut in_string = false;
    let mut index = 0;

    while index < chars.len() {
        let c = chars[index];

        if !in_string {
            repaired.push(c);
            if c == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }

        if c == '"' {
            repaired.push(c);
            in_string = false;
            index += 1;
            continue;
        }

        if c == '\\' {
            if index + 1 >= chars.len() {
                repaired.push_str("\\\\");
                index += 1;
                continue;
            }

            let next = chars[index + 1];
            if next == 'u'
                && let Some(digits) = chars.get(index + 2..index + 6)
                && digits.len() == 4
                && digits.iter().all(|d| d.is_ascii_hexdigit())
            {
                let digits: String = digits.iter().collect();
                repaired.push_str(&format!("\\u{digits}"));
                index += 6;
                continue;
            }

            if VALID_JSON_ESCAPES.contains(&next) {
                repaired.push('\\');
                repaired.push(next);
                index += 2;
                continue;
            }

            repaired.push_str("\\\\");
            index += 1;
            continue;
        }

        if is_control_character(c) {
            repaired.push_str(&escape_control_character(c));
        } else {
            repaired.push(c);
        }
        index += 1;
    }

    repaired
}

/// 对应 `parseJsonWithRepair`：先标准解析，失败时修复后再解析。
pub fn parse_json_with_repair<T: DeserializeOwned>(json: &str) -> Result<T, serde_json::Error> {
    match serde_json::from_str(json) {
        Ok(value) => Ok(value),
        Err(first_error) => {
            let repaired = repair_json(json);
            if repaired != json {
                serde_json::from_str(&repaired)
            } else {
                Err(first_error)
            }
        }
    }
}

/// 对应 `parseStreamingJson`：解析可能不完整的流式 JSON，失败返回空对象。
pub fn parse_streaming_json(partial_json: Option<&str>) -> serde_json::Value {
    let empty = || serde_json::Value::Object(Default::default());
    let Some(partial_json) = partial_json else {
        return empty();
    };
    if partial_json.trim().is_empty() {
        return empty();
    }
    parse_json_with_repair::<serde_json::Value>(partial_json).unwrap_or_else(|_| empty())
}
