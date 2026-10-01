//! system 消息（mid-conversation system messages）的测试。
//!
//! 对应上游 `Message = SystemMessage | UserMessage | AssistantMessage | ToolResultMessage`
//! 与 `utils/transcript.ts` 引入的文本渲染/估算语义。

use std::collections::BTreeMap;

use pi_ai::utils::estimate::estimate_message_tokens;
use pi_ai::{
    Message, SystemContent, SystemMessage, Tool, ToolReference, get_system_message_text,
    render_system_message_update,
};

fn tool(name: &str) -> Tool {
    Tool {
        name: name.to_string(),
        description: format!("{name} tool"),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        constrained_sampling: None,
    }
}

fn system_message(content: SystemContent) -> SystemMessage {
    SystemMessage {
        content,
        sections: None,
        tools_added: None,
        tools_removed: None,
        timestamp: 1_700_000_000_000,
    }
}

#[test]
fn system_message_serde_round_trip() {
    let mut sections = BTreeMap::new();
    sections.insert("style".to_string(), Some("be concise".to_string()));
    sections.insert("obsolete".to_string(), None);

    let message = Message::System(SystemMessage {
        content: SystemContent::Text("base prompt".into()),
        sections: Some(sections),
        tools_added: Some(vec![tool("read")]),
        tools_removed: Some(vec![ToolReference {
            name: "write".into(),
        }]),
        timestamp: 12345,
    });

    let json = serde_json::to_value(&message).expect("serialize");
    assert_eq!(json["role"], "system");
    assert_eq!(json["content"], "base prompt");

    let back: Message = serde_json::from_value(json).expect("deserialize");
    assert_eq!(back, message);
    assert_eq!(back.role(), "system");
}

#[test]
fn system_content_accepts_string_and_blocks() {
    let from_string: SystemContent =
        serde_json::from_value(serde_json::json!("hello")).expect("string");
    assert_eq!(from_string, SystemContent::Text("hello".into()));

    let from_blocks: SystemContent = serde_json::from_value(serde_json::json!([
        { "type": "text", "text": "a" },
        { "type": "text", "text": "b" }
    ]))
    .expect("blocks");
    match from_blocks {
        SystemContent::Blocks(blocks) => {
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0].text, "a");
        }
        other => panic!("expected blocks, got {other:?}"),
    }
}

#[test]
fn get_system_message_text_joins_content_and_non_empty_sections() {
    let mut sections = BTreeMap::new();
    sections.insert("b".to_string(), Some("second".to_string()));
    sections.insert("dropped".to_string(), None);
    sections.insert("empty".to_string(), Some(String::new()));

    let message = SystemMessage {
        content: SystemContent::Text("base".into()),
        sections: Some(sections),
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
    };

    // 只拼接非空值，用空行分隔（`None` 与空字符串都被过滤）。
    assert_eq!(get_system_message_text(&message), "base\n\nsecond");
}

#[test]
fn render_system_message_update_frames_section_changes() {
    let mut sections = BTreeMap::new();
    sections.insert("style".to_string(), Some("terse".to_string()));
    sections.insert("old".to_string(), None);

    let message = SystemMessage {
        content: SystemContent::Text("update".into()),
        sections: Some(sections),
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
    };

    let rendered = render_system_message_update(&message);
    assert!(rendered.starts_with("update"));
    assert!(rendered.contains("Updated system prompt section \"style\":\n\nterse"));
    assert!(rendered.contains("Removed system prompt section \"old\"."));
}

#[test]
fn estimate_message_tokens_counts_system_text_and_tool_changes() {
    let text_only = Message::System(system_message(SystemContent::Text("abcd".into())));
    let with_tools = Message::System(SystemMessage {
        content: SystemContent::Text("abcd".into()),
        sections: None,
        tools_added: Some(vec![tool("read")]),
        tools_removed: Some(vec![ToolReference {
            name: "write".into(),
        }]),
        timestamp: 0,
    });

    let base = estimate_message_tokens(&text_only);
    let with_changes = estimate_message_tokens(&with_tools);
    assert!(base > 0, "system 文本应计入估算");
    assert!(
        with_changes > base,
        "toolsAdded/toolsRemoved 应追加到估算中（{with_changes} > {base}）"
    );
}
