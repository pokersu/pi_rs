//! system 消息在 agent 层的语义测试。
//!
//! 对照上游：
//! - `convertToLlm` 对 `system` 与 user/assistant/toolResult 一样原样透传
//! - `estimateTokens` / `findValidCutPoints` / `serializeConversation` 未列 system 分支
//!   → system 不参与压缩预算、不可作为切点、不进摘要文本

use pi_agent::AgentMessage;
use pi_agent::harness::compaction::compaction::estimate_tokens;
use pi_agent::harness::messages::convert_to_llm;
use pi_ai::{Message, SystemContent, SystemMessage};

fn system_message(content: &str) -> AgentMessage {
    AgentMessage::System(SystemMessage {
        content: SystemContent::Text(content.to_string()),
        sections: None,
        tools_added: None,
        tools_removed: None,
        timestamp: 7,
    })
}

#[test]
fn convert_to_llm_passes_system_messages_through() {
    let converted = convert_to_llm(vec![system_message("base prompt")]);

    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0].role(), "system");
    match &converted[0] {
        Message::System(system) => {
            assert_eq!(system.content, SystemContent::Text("base prompt".into()));
            assert_eq!(system.timestamp, 7);
        }
        other => panic!("expected system message, got {other:?}"),
    }
}

#[test]
fn convert_to_llm_keeps_system_messages_in_order() {
    let converted = convert_to_llm(vec![
        system_message("base"),
        AgentMessage::User(pi_ai::UserMessage {
            content: pi_ai::UserContent::Text("hi".into()),
            timestamp: 8,
        }),
        system_message("update"),
    ]);

    let roles: Vec<&str> = converted.iter().map(Message::role).collect();
    assert_eq!(roles, vec!["system", "user", "system"]);
}

#[test]
fn compaction_ignores_system_messages() {
    // 上游 `estimateTokens` 未列 system 分支 → 估算为 0（不参与压缩预算）。
    assert_eq!(
        estimate_tokens(&system_message("a very long base prompt")),
        0
    );
}
