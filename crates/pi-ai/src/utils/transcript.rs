//! Rust 翻译自 packages/ai/src/utils/transcript.ts
//!
//! 把 `Context.systemPrompt` / `Context.tools` 折叠进 transcript，并支持
//! 中途插入的 system 消息（工具增删、提示分节更新）的推导与折叠。
//!
//! 上游的函数接受「任意带 role 的消息列表」（agent 层可能带自定义角色）；
//! Rust 版限定为 LLM 层的 [`Message`]，agent 层应先经 `convert_to_llm` 转换。

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::types::{Context, Message, SystemMessage, Tool, ToolReference, TranscriptContext};
use crate::utils::text::{get_system_message_text, system_content_text};

/// 对应 `createInitialSystemMessage`：构造首条 system 消息。
///
/// 两者都为空时返回 `None`（空 transcript 保持为空）。
pub fn create_initial_system_message(
    system_prompt: Option<&str>,
    tools: Option<&[Tool]>,
) -> Option<SystemMessage> {
    let has_system_prompt = system_prompt.is_some_and(|prompt| !prompt.is_empty());
    let has_tools = tools.is_some_and(|tools| !tools.is_empty());
    if !has_system_prompt && !has_tools {
        return None;
    }
    Some(SystemMessage {
        content: crate::types::SystemContent::Text(system_prompt.unwrap_or("").to_string()),
        sections: None,
        tools_added: if has_tools {
            tools.map(<[Tool]>::to_vec)
        } else {
            None
        },
        tools_removed: None,
        timestamp: 0,
    })
}

/// 对应 `normalizeContext`：把 `Context` 折叠成 [`TranscriptContext`]。
///
/// 这是唯一产出 `TranscriptContext` 的入口，所有面向 provider 的函数都应接受它的结果。
pub fn normalize_context(context: &Context) -> TranscriptContext {
    let initial =
        create_initial_system_message(context.system_prompt.as_deref(), context.tools.as_deref());
    let messages = match initial {
        Some(message) => {
            let mut messages = Vec::with_capacity(context.messages.len() + 1);
            messages.push(Message::System(message));
            messages.extend(context.messages.iter().cloned());
            messages
        }
        None => context.messages.clone(),
    };
    TranscriptContext { messages }
}

fn as_system_message(message: &Message) -> Option<&SystemMessage> {
    match message {
        Message::System(system) => Some(system),
        _ => None,
    }
}

/// 对应 `getInitialSystemMessage`：transcript 以 system 消息开头时返回它。
pub fn get_initial_system_message(messages: &[Message]) -> Option<SystemMessage> {
    messages.first().and_then(as_system_message).cloned()
}

/// 对应 `withoutInitialSystemMessage`：为「prompt 放在消息列表之外」的 API 去掉首条 system 消息。
pub fn without_initial_system_message(messages: Vec<Message>) -> Vec<Message> {
    if messages.first().and_then(as_system_message).is_some() {
        messages.into_iter().skip(1).collect()
    } else {
        messages
    }
}

/// 对应 `getCurrentTools`：按顺序应用每条 system 消息的工具增删（保序，与 JS Map 一致）。
pub fn get_current_tools(messages: &[Message]) -> Vec<Tool> {
    let mut tools: Vec<(String, Tool)> = Vec::new();
    for message in messages {
        let Some(system) = as_system_message(message) else {
            continue;
        };
        for removed in system.tools_removed.iter().flatten() {
            tools.retain(|(name, _)| name != &removed.name);
        }
        for tool in system.tools_added.iter().flatten() {
            match tools.iter_mut().find(|(name, _)| name == &tool.name) {
                Some(slot) => slot.1 = tool.clone(),
                None => tools.push((tool.name.clone(), tool.clone())),
            }
        }
    }
    tools.into_iter().map(|(_, tool)| tool).collect()
}

/// 对应 `getCurrentSystemMessage`：把所有 system 消息重放成一条「当前」system 消息。
///
/// 后续 `content` 依次追加，`sections` 按名打补丁，工具用 [`get_current_tools`] 求出。
pub fn get_current_system_message(messages: &[Message]) -> Option<SystemMessage> {
    let mut content: Vec<String> = Vec::new();
    let mut sections: IndexMap<String, Option<String>> = IndexMap::new();
    let mut timestamp: Option<u64> = None;

    for message in messages {
        let Some(system) = as_system_message(message) else {
            continue;
        };
        if timestamp.is_none() {
            timestamp = Some(system.timestamp);
        }
        let text = system_content_text(&system.content);
        if !text.is_empty() {
            content.push(text);
        }
        if let Some(map) = &system.sections {
            for (name, value) in map {
                match value {
                    None => {
                        // 与上游 `Map.delete` 一致：保序删除。
                        sections.shift_remove(name);
                    }
                    Some(value) => {
                        sections.insert(name.clone(), Some(value.clone()));
                    }
                }
            }
        }
    }

    let tools = get_current_tools(messages);
    if timestamp.is_none() && tools.is_empty() {
        return None;
    }
    Some(SystemMessage {
        content: crate::types::SystemContent::Text(content.join("\n\n")),
        sections: if sections.is_empty() {
            None
        } else {
            Some(sections)
        },
        tools_added: if tools.is_empty() { None } else { Some(tools) },
        tools_removed: None,
        timestamp: timestamp.unwrap_or(0),
    })
}

/// 对应 `getCurrentSystemPrompt`：重放所有 system 消息后的当前提示文本。
pub fn get_current_system_prompt(messages: &[Message]) -> String {
    get_current_system_message(messages)
        .map(|message| get_system_message_text(&message))
        .unwrap_or_default()
}

/// 对应 `collapseSystemMessages`：为「不支持中途 system 消息」的 API 重建 transcript。
///
/// 重放出的 system 消息置于首位，其余 system 消息全部丢弃。
pub fn collapse_system_messages(context: TranscriptContext) -> TranscriptContext {
    let head = get_current_system_message(&context.messages);
    let mut messages: Vec<Message> = context
        .messages
        .into_iter()
        .filter(|message| !matches!(message, Message::System(_)))
        .collect();
    if let Some(head) = head {
        messages.insert(0, Message::System(head));
    }
    TranscriptContext { messages }
}

/// 对应 `resolveTranscript`：模型支持中途 system 消息时原样保留，否则折叠。
pub fn resolve_transcript(
    context: TranscriptContext,
    supports_mid_convo_system_messages: Option<bool>,
) -> TranscriptContext {
    if supports_mid_convo_system_messages.unwrap_or(false) {
        context
    } else {
        collapse_system_messages(context)
    }
}

/// 对应 `toToolDeclaration`：在比较/持久化前剥掉工具的可执行与展示字段。
///
/// 上游用 JSON 往返丢弃 `undefined` 与 symbol key；Rust 的 `Tool` 是强类型且
/// `parameters` 为 `serde_json::Value`（对象键有序），因此克隆即为规范形式。
pub fn to_tool_declaration(tool: &Tool) -> Tool {
    tool.clone()
}

/// 对应 `declarationsEqual`：两个工具是否向模型声明了同一接口。
pub fn declarations_equal(left: &Tool, right: &Tool) -> bool {
    to_tool_declaration(left) == to_tool_declaration(right)
}

/// 对应 `ToolStateChanges`。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolStateChanges {
    pub tools_added: Vec<Tool>,
    pub tools_removed: Vec<ToolReference>,
}

/// 对应 `getToolStateChanges`：比较两个完整工具状态。
///
/// 定义发生变化 = 一次移除 + 一次添加。
pub fn get_tool_state_changes(previous: &[Tool], current: &[Tool]) -> ToolStateChanges {
    let previous_tools: HashMap<&str, &Tool> = previous
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();
    let current_tools: HashMap<&str, &Tool> = current
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();

    let tools_added = current
        .iter()
        .filter(|tool| match previous_tools.get(tool.name.as_str()) {
            Some(previous_tool) => !declarations_equal(previous_tool, tool),
            None => true,
        })
        .map(to_tool_declaration)
        .collect();

    let tools_removed = previous
        .iter()
        .filter(|tool| match current_tools.get(tool.name.as_str()) {
            Some(current_tool) => !declarations_equal(tool, current_tool),
            None => true,
        })
        .map(|tool| ToolReference {
            name: tool.name.clone(),
        })
        .collect();

    ToolStateChanges {
        tools_added,
        tools_removed,
    }
}

/// 对应 `getDeclaredTools`：transcript 工具状态引用到的全部定义，按首次声明顺序。
pub fn get_declared_tools(messages: &[Message]) -> Vec<Tool> {
    let mut definitions: Vec<(String, Tool)> = Vec::new();
    for message in messages {
        let Some(system) = as_system_message(message) else {
            continue;
        };
        for tool in system.tools_added.iter().flatten() {
            match definitions.iter_mut().find(|(name, _)| name == &tool.name) {
                Some(slot) => slot.1 = tool.clone(),
                None => definitions.push((tool.name.clone(), tool.clone())),
            }
        }
    }
    definitions.into_iter().map(|(_, tool)| tool).collect()
}

/// 对应 `hasToolRedefinitions`：同名工具是否以不同定义被声明过两次。
///
/// 以名字引用工具的传输层（Anthropic `tool_addition`/`tool_removal`）无法表达这种情况。
pub fn has_tool_redefinitions(messages: &[Message]) -> bool {
    let mut declared: HashMap<String, Tool> = HashMap::new();
    for message in messages {
        let Some(system) = as_system_message(message) else {
            continue;
        };
        for tool in system.tools_added.iter().flatten() {
            if let Some(previous) = declared.get(&tool.name)
                && !declarations_equal(previous, tool)
            {
                return true;
            }
            declared.insert(tool.name.clone(), tool.clone());
        }
    }
    false
}

/// 对应 `hasNonAdditiveToolChanges`：工具历史是否含「仅支持追加」的传输层无法重放的变化。
pub fn has_non_additive_tool_changes(messages: &[Message]) -> bool {
    let mut declared: HashSet<String> = HashSet::new();
    for message in messages {
        let Some(system) = as_system_message(message) else {
            continue;
        };
        if system.tools_removed.as_ref().is_some_and(|r| !r.is_empty()) {
            return true;
        }
        for tool in system.tools_added.iter().flatten() {
            if declared.contains(&tool.name) {
                return true;
            }
            declared.insert(tool.name.clone());
        }
    }
    false
}

/// 对应 `TranscriptTools`。
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptTools {
    /// 放在顶层请求字段里的工具。
    pub request_tools: Vec<Tool>,
    /// 后续 system 消息是否自带 `toolsAdded` 作为就地追加。
    /// 为 false 时 `request_tools` 已包含完整的当前工具集。
    pub anchors_additions: bool,
}

/// 对应 `resolveTranscriptTools`：把工具声明拆分到「顶层请求字段」与「就地追加」。
///
/// 只有工具历史是纯追加时，才能让首条消息带初始工具、后续消息就地加载；
/// 其余情况直接发送当前完整工具列表。
pub fn resolve_transcript_tools(
    messages: &[Message],
    supports_tool_additions: bool,
) -> TranscriptTools {
    let anchors_additions = supports_tool_additions && !has_non_additive_tool_changes(messages);
    TranscriptTools {
        request_tools: if anchors_additions {
            get_initial_system_message(messages)
                .and_then(|message| message.tools_added)
                .unwrap_or_default()
        } else {
            get_current_tools(messages)
        },
        anchors_additions,
    }
}
