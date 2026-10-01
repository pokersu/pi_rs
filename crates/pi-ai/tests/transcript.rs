//! transcript 工具层的测试（对应上游 `utils/transcript.ts`）。

use indexmap::IndexMap;

use pi_ai::{
    Context, Message, SystemContent, SystemMessage, Tool, ToolReference, TranscriptContext,
    collapse_system_messages, create_initial_system_message, get_current_system_message,
    get_current_system_prompt, get_current_tools, get_declared_tools, get_initial_system_message,
    get_tool_state_changes, has_non_additive_tool_changes, has_tool_redefinitions,
    normalize_context, resolve_transcript, resolve_transcript_tools,
    without_initial_system_message,
};

fn tool(name: &str, description: &str) -> Tool {
    Tool {
        name: name.to_string(),
        description: description.to_string(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        constrained_sampling: None,
    }
}

fn system(
    content: &str,
    sections: Option<IndexMap<String, Option<String>>>,
    tools_added: Option<Vec<Tool>>,
    tools_removed: Option<Vec<ToolReference>>,
) -> Message {
    Message::System(SystemMessage {
        content: SystemContent::Text(content.to_string()),
        sections,
        tools_added,
        tools_removed,
        timestamp: 1,
    })
}

fn user(text: &str) -> Message {
    Message::User(pi_ai::UserMessage {
        content: pi_ai::UserContent::Text(text.to_string()),
        timestamp: 2,
    })
}

#[test]
fn create_initial_system_message_handles_empty_inputs() {
    assert!(create_initial_system_message(None, None).is_none());
    assert!(create_initial_system_message(Some(""), Some(&[])).is_none());

    let prompt_only = create_initial_system_message(Some("hello"), None).expect("prompt");
    assert_eq!(prompt_only.content, SystemContent::Text("hello".into()));
    assert!(prompt_only.tools_added.is_none());

    let tools = vec![tool("read", "read a file")];
    let tools_only = create_initial_system_message(None, Some(&tools)).expect("tools");
    assert_eq!(tools_only.content, SystemContent::Text(String::new()));
    assert_eq!(tools_only.tools_added.as_deref(), Some(tools.as_slice()));
}

#[test]
fn normalize_context_folds_prompt_and_tools_into_leading_message() {
    let context = Context {
        system_prompt: Some("base".into()),
        messages: vec![user("hi")],
        tools: Some(vec![tool("read", "read a file")]),
    };

    let transcript = normalize_context(&context);
    assert_eq!(transcript.messages.len(), 2);
    match &transcript.messages[0] {
        Message::System(system) => {
            assert_eq!(system.content, SystemContent::Text("base".into()));
            assert_eq!(system.tools_added.as_ref().map(Vec::len), Some(1));
        }
        other => panic!("expected leading system message, got {other:?}"),
    }
    assert_eq!(transcript.messages[1].role(), "user");

    // 没有 prompt/tools 时不插入空 system 消息。
    let empty = normalize_context(&Context {
        system_prompt: None,
        messages: vec![user("hi")],
        tools: None,
    });
    assert_eq!(empty.messages.len(), 1);
}

#[test]
fn get_initial_system_message_and_without_initial() {
    let messages = vec![system("base", None, None, None), user("hi")];
    assert!(get_initial_system_message(&messages).is_some());

    let stripped = without_initial_system_message(messages.clone());
    assert_eq!(stripped.len(), 1);
    assert_eq!(stripped[0].role(), "user");

    // 非 system 开头时原样返回。
    let no_system = vec![user("hi")];
    assert!(get_initial_system_message(&no_system).is_none());
    assert_eq!(without_initial_system_message(no_system).len(), 1);
}

#[test]
fn get_current_tools_applies_additions_and_removals_in_order() {
    let messages = vec![
        system(
            "base",
            None,
            Some(vec![
                tool("read", "read a file"),
                tool("write", "write a file"),
            ]),
            None,
        ),
        system(
            "update",
            None,
            Some(vec![tool("read", "read a file (v2)")]),
            Some(vec![ToolReference {
                name: "write".into(),
            }]),
        ),
    ];

    let tools = get_current_tools(&messages);
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    // write 被移除；read 就地更新（保持原位置）。
    assert_eq!(names, vec!["read"]);
    assert_eq!(tools[0].description, "read a file (v2)");
}

#[test]
fn get_current_system_message_replays_content_sections_and_tools() {
    let mut first_sections = IndexMap::new();
    first_sections.insert("style".to_string(), Some("terse".to_string()));

    let mut later_sections = IndexMap::new();
    later_sections.insert("style".to_string(), Some("verbose".to_string()));
    later_sections.insert("dropped".to_string(), None);

    let messages = vec![
        system(
            "base",
            Some(first_sections),
            Some(vec![tool("read", "read a file")]),
            None,
        ),
        system("extra", Some(later_sections), None, None),
    ];

    let current = get_current_system_message(&messages).expect("current system message");
    assert_eq!(current.content, SystemContent::Text("base\n\nextra".into()));
    let sections = current.sections.expect("sections");
    assert_eq!(
        sections.get("style"),
        Some(&Some("verbose".to_string())),
        "后声明的分节覆盖先前的"
    );
    assert!(!sections.contains_key("dropped"), "null 删除该分节");
    assert_eq!(current.tools_added.as_ref().map(Vec::len), Some(1));

    assert_eq!(
        get_current_system_prompt(&messages),
        "base\n\nextra\n\nverbose"
    );
}

#[test]
fn collapse_system_messages_keeps_only_replayed_head() {
    let messages = vec![
        system("base", None, Some(vec![tool("read", "read")]), None),
        user("hi"),
        system("mid", None, Some(vec![tool("write", "write")]), None),
    ];

    let collapsed = collapse_system_messages(TranscriptContext { messages });
    let roles: Vec<&str> = collapsed.messages.iter().map(Message::role).collect();
    assert_eq!(roles, vec!["system", "user"]);

    match &collapsed.messages[0] {
        Message::System(system) => {
            assert_eq!(system.content, SystemContent::Text("base\n\nmid".into()));
            let tools = system.tools_added.as_ref().expect("tools");
            assert_eq!(tools.len(), 2, "折叠后保留完整当前工具集");
        }
        other => panic!("expected collapsed system message, got {other:?}"),
    }
}

#[test]
fn resolve_transcript_respects_support_flag() {
    let messages = vec![
        system("base", None, None, None),
        system("mid", None, None, None),
    ];
    let context = TranscriptContext {
        messages: messages.clone(),
    };

    let kept = resolve_transcript(context.clone(), Some(true));
    assert_eq!(kept.messages.len(), 2);

    let collapsed = resolve_transcript(context, None);
    assert_eq!(collapsed.messages.len(), 1);
}

#[test]
fn tool_state_changes_detect_add_remove_and_redeclare() {
    let previous = vec![tool("read", "read"), tool("write", "write")];
    let current = vec![
        tool("read", "read"),
        tool("write", "write v2"),
        tool("bash", "bash"),
    ];

    let changes = get_tool_state_changes(&previous, &current);
    let added: Vec<&str> = changes
        .tools_added
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    let removed: Vec<&str> = changes
        .tools_removed
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    // write 定义变化 = 一次移除 + 一次添加。
    assert_eq!(added, vec!["write", "bash"]);
    assert_eq!(removed, vec!["write"]);
}

#[test]
fn tool_history_flags() {
    // 纯追加：无移除、无重定义。
    let additive = vec![
        system("base", None, Some(vec![tool("read", "read")]), None),
        system("mid", None, Some(vec![tool("write", "write")]), None),
    ];
    assert!(!has_non_additive_tool_changes(&additive));
    assert!(!has_tool_redefinitions(&additive));
    assert_eq!(get_declared_tools(&additive).len(), 2);

    // 移除 → 非追加。
    let with_removal = vec![system(
        "base",
        None,
        None,
        Some(vec![ToolReference {
            name: "read".into(),
        }]),
    )];
    assert!(has_non_additive_tool_changes(&with_removal));

    // 同名不同定义 → 重定义。
    let redefined = vec![
        system("base", None, Some(vec![tool("read", "read")]), None),
        system("mid", None, Some(vec![tool("read", "read v2")]), None),
    ];
    assert!(has_tool_redefinitions(&redefined));
    assert!(has_non_additive_tool_changes(&redefined));
}

#[test]
fn resolve_transcript_tools_splits_request_and_in_place_additions() {
    let additive = vec![
        system("base", None, Some(vec![tool("read", "read")]), None),
        system("mid", None, Some(vec![tool("write", "write")]), None),
    ];

    let anchored = resolve_transcript_tools(&additive, true);
    assert!(anchored.anchors_additions);
    assert_eq!(anchored.request_tools.len(), 1, "顶层字段只带初始工具");
    assert_eq!(anchored.request_tools[0].name, "read");

    let fallback = resolve_transcript_tools(&additive, false);
    assert!(!fallback.anchors_additions);
    assert_eq!(fallback.request_tools.len(), 2, "回退时发送完整当前工具集");

    // 非追加历史 → 即使支持也必须回退。
    let non_additive = vec![system(
        "base",
        None,
        Some(vec![tool("read", "read")]),
        Some(vec![ToolReference {
            name: "read".into(),
        }]),
    )];
    let forced_fallback = resolve_transcript_tools(&non_additive, true);
    assert!(!forced_fallback.anchors_additions);
}
