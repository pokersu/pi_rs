//! Rust 翻译自 ai 核心的类型/工具/流抽象测试。

use futures::StreamExt;
use pi_ai::{
    ContentBlock, ContentTextInput, EventStream, TextContent, TextKind, ThinkingContent,
    ThinkingKind, Tool, ToolCall, ToolCallKind, content_text,
    create_assistant_message_event_stream, parse_streaming_json, repair_json, uuidv7,
    validate_tool_arguments,
};

#[test]
fn uuidv7_generates_valid_uuid() {
    let id = uuidv7();
    assert_eq!(id.len(), 36);
    // version 7 位于第 3 组首字符，variant（10xx）位于第 4 组首字符。
    assert_eq!(id.chars().nth(14), Some('7'));
    let variant = id.chars().nth(19).unwrap();
    assert!("89ab".contains(variant), "variant char = {variant}");
}

#[test]
fn content_text_extracts_text_blocks() {
    let blocks = vec![
        ContentBlock::Text(TextContent {
            kind: TextKind,
            text: "hello".into(),
            text_signature: None,
        }),
        ContentBlock::Thinking(ThinkingContent {
            kind: ThinkingKind,
            thinking: "hmm".into(),
            thinking_signature: None,
            redacted: None,
        }),
        ContentBlock::Text(TextContent {
            kind: TextKind,
            text: "world".into(),
            text_signature: None,
        }),
    ];
    assert_eq!(
        content_text(ContentTextInput::Blocks(&blocks), "\n"),
        "hello\nworld"
    );
    assert_eq!(content_text(ContentTextInput::Str("plain"), "\n"), "plain");
}

#[test]
fn repair_json_escapes_control_characters() {
    let json = "{\"a\":\"line1\nline2\"}";
    let repaired = repair_json(json);
    let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
    assert_eq!(parsed["a"], "line1\nline2");
}

#[test]
fn parse_streaming_json_returns_empty_on_garbage() {
    assert_eq!(parse_streaming_json(None), serde_json::json!({}));
    assert_eq!(
        parse_streaming_json(Some("not json")),
        serde_json::json!({})
    );
    assert_eq!(
        parse_streaming_json(Some("{\"a\":1}")),
        serde_json::json!({ "a": 1 })
    );
}

#[test]
fn validate_tool_arguments_coerces_and_validates() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string" },
            "count": { "type": "integer" }
        },
        "required": ["path"]
    });
    let tool = Tool {
        name: "read".into(),
        description: "".into(),
        parameters: schema,
        constrained_sampling: None,
    };
    let call = ToolCall {
        kind: ToolCallKind,
        id: "1".into(),
        name: "read".into(),
        arguments: serde_json::json!({ "path": "/tmp", "count": "3" }),
        thought_signature: None,
        namespace: None,
    };
    let result = validate_tool_arguments(&tool, &call).unwrap();
    assert_eq!(result["count"], 3);
    assert_eq!(result["path"], "/tmp");
}

#[test]
fn validate_tool_arguments_rejects_missing_required() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"]
    });
    let tool = Tool {
        name: "read".into(),
        description: "".into(),
        parameters: schema,
        constrained_sampling: None,
    };
    let call = ToolCall {
        kind: ToolCallKind,
        id: "1".into(),
        name: "read".into(),
        arguments: serde_json::json!({ "count": 1 }),
        thought_signature: None,
        namespace: None,
    };
    assert!(validate_tool_arguments(&tool, &call).is_err());
}

#[tokio::test]
async fn event_stream_pushes_and_resolves_result() {
    let mut stream = EventStream::<usize, usize>::new(|e| *e == 3, |e| e * 10);
    stream.push(1);
    stream.push(2);
    stream.push(3);

    let mut collected = Vec::new();
    while let Some(event) = stream.next().await {
        collected.push(event);
        if event == 3 {
            break;
        }
    }
    assert_eq!(collected, vec![1, 2, 3]);
    assert_eq!(stream.result().await, 30);
}

#[test]
fn create_assistant_message_event_stream_smoke() {
    let stream = create_assistant_message_event_stream();
    // 未 push 完成事件时，result 不应立即就绪；这里只验证能构造即可。
    drop(stream);
}
