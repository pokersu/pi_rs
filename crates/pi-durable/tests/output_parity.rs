//! `harness::output` 与上游 `harness/output.ts` 的对照测试。
//!
//! 用例与期望值由 `tools.d/parity/output-parity.mjs` 用 Node 直接加载上游 TypeScript 生成：
//!
//! ```bash
//! node --experimental-strip-types tools.d/parity/output-parity.mjs
//! ```
//!
//! 覆盖 `sanitizeOutput`、`boundOutput`、`characterEnd` 与 `OutputBuffer` 的手写边界用例
//! （含 UTF-8 跨块、BOM、跳过、head 满、tail 丢弃）与确定性伪随机用例；期望抛错的用例
//! 要求 Rust 侧同样 panic。

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use pi_durable::env::ShellOutputSkip;
use pi_durable::harness::output::{
    OutputBuffer, OutputChunk, OutputLimits, Retain, bound_output, character_end, sanitize_output,
};
use serde_json::{Value as JsonValue, json};

const CASES: &str = include_str!("../../../tools.d/parity/output-cases.json");
const EXPECTED: &str = include_str!("../../../tools.d/parity/output-expected.json");

#[test]
fn matches_the_upstream_output_module() {
    let cases: Vec<JsonValue> = serde_json::from_str(CASES).expect("parse cases");
    let expected: Vec<JsonValue> = serde_json::from_str(EXPECTED).expect("parse expected");
    let expected: BTreeMap<String, JsonValue> = expected
        .into_iter()
        .map(|entry| {
            (
                entry["name"].as_str().expect("name").to_string(),
                entry.clone(),
            )
        })
        .collect();

    assert!(!cases.is_empty(), "对照用例不得为空");
    let mut mismatches = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().expect("name").to_string();
        let expectation = expected
            .get(&name)
            .unwrap_or_else(|| panic!("缺少期望值：{name}"));
        let should_throw = !expectation["ok"].as_bool().expect("ok");
        let outcome = catch_unwind(AssertUnwindSafe(|| run_case(case)));
        if should_throw {
            if outcome.is_ok() {
                mismatches.push(format!("{name}: 上游抛错，Rust 未抛错"));
            }
            continue;
        }
        let expected_value = expectation["value"].clone();
        match outcome {
            Ok(actual) if actual == expected_value => {}
            Ok(actual) => mismatches.push(format!("{name}: 期望 {expected_value}，实际 {actual}")),
            Err(_) => mismatches.push(format!("{name}: Rust 抛错，上游返回 {expected_value}")),
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} 个用例与上游不一致：\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

fn run_case(case: &JsonValue) -> JsonValue {
    match case["kind"].as_str().expect("kind") {
        "sanitize" => json!({ "text": sanitize_output(case["text"].as_str().expect("text")) }),
        "bound" => {
            let slice = bound_output(
                case["text"].as_str().expect("text"),
                &read_limits(&case["limits"]),
            );
            json!({
                "text": slice.text,
                "bytes": slice.bytes,
                "droppedBytes": slice.dropped_bytes,
                "droppedLines": slice.dropped_lines,
            })
        }
        "characterEnd" => {
            let bytes = read_bytes(&case["bytes"]);
            let index = case["index"].as_u64().expect("index") as usize;
            json!({ "index": character_end(&bytes, index) })
        }
        "buffer" => {
            let mut buffer = OutputBuffer::new(read_limits(&case["limits"]));
            let mut pushes = Vec::new();
            for chunk in case["chunks"].as_array().expect("chunks") {
                match chunk["kind"].as_str().expect("chunk kind") {
                    "flush" => {
                        buffer.end();
                        pushes.push(json!(true));
                    }
                    "text" => {
                        let accepted = buffer.push(
                            OutputChunk::Text(chunk["text"].as_str().expect("text")),
                            read_skip(chunk),
                        );
                        pushes.push(json!(accepted));
                    }
                    "bytes" => {
                        let bytes = read_bytes(&chunk["bytes"]);
                        let accepted = buffer.push(OutputChunk::Bytes(&bytes), read_skip(chunk));
                        pushes.push(json!(accepted));
                    }
                    other => panic!("未知的块类型：{other}"),
                }
            }
            let out = buffer.snapshot();
            json!({
                "pushes": pushes,
                "storedBytes": buffer.stored_bytes(),
                "text": out.text,
                "droppedBytes": out.dropped_bytes,
                "droppedLines": out.dropped_lines,
            })
        }
        other => panic!("未知的用例类型：{other}"),
    }
}

fn read_limits(value: &JsonValue) -> OutputLimits {
    OutputLimits {
        max_bytes: value["maxBytes"].as_u64().expect("maxBytes") as usize,
        max_lines: value["maxLines"].as_u64().expect("maxLines") as usize,
        retain: match value["retain"].as_str().expect("retain") {
            "head" => Retain::Head,
            "tail" => Retain::Tail,
            other => panic!("未知的保留策略：{other}"),
        },
    }
}

fn read_skip(chunk: &JsonValue) -> Option<ShellOutputSkip> {
    let skip = chunk.get("skip")?;
    if skip.is_null() {
        return None;
    }
    Some(ShellOutputSkip {
        bytes: skip["bytes"].as_u64().expect("bytes") as usize,
        newlines: skip["newlines"].as_u64().expect("newlines") as usize,
        ends_with_newline: skip["endsWithNewline"].as_bool().expect("endsWithNewline"),
    })
}

fn read_bytes(value: &JsonValue) -> Vec<u8> {
    value
        .as_array()
        .expect("bytes")
        .iter()
        .map(|byte| byte.as_u64().expect("byte") as u8)
        .collect()
}
