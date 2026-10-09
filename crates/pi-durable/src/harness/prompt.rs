//! 对应 `harness/prompt.ts`：系统提示章节的重放、渲染与 `pi.system` 条目的规划。
//!
//! 这里全是纯逻辑（除 `render_sections` 需要调用章节的异步渲染），因此可以脱离 scheduler 单测。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游的 `Record<string, string | null>` → Rust 用 [`SectionPatch`]（`Value::Null` 表示删除）。
//! - `section.tag === false ? text : wrap` → [`PromptSection::is_tagged`]。
//! - `TypedEntryDraft<never>` → [`EntryDraft`]（`model` 为一条 `SystemMessage`）。

use std::collections::BTreeSet;
use std::sync::Arc;

use indexmap::IndexMap;

use pi_ai::utils::transcript::{declarations_equal, get_current_tools, to_tool_declaration};
use pi_ai::{Message, SystemContent, SystemMessage, Tool, ToolReference};
use serde_json::Value as JsonValue;

use crate::chord::context::Context;
use crate::entries::SYSTEM_ENTRY;
use crate::harness::types::{ContextView, PromptInput, PromptSection};
use crate::session::SessionError;
use crate::types::{ContextEdit, EntryDraft};

/// 对应 `sections` 的一次补丁：键到值，缺省（`null`）表示删除。
///
/// 顺序是语义的一部分（`planSections` 会比较重放顺序），因此用保持插入顺序的 [`IndexMap`]。
pub type SectionPatch = IndexMap<String, Option<String>>;

/// 对应 `ToolChanges`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolChanges {
    /// 失效的工具。
    pub tools_removed: Vec<ToolReference>,
    /// 变为可用的工具。
    pub tools_added: Vec<Tool>,
}

impl ToolChanges {
    /// 是否没有任何改动。
    pub fn is_empty(&self) -> bool {
        self.tools_removed.is_empty() && self.tools_added.is_empty()
    }
}

/// 对应 `replaySections(messages)`：按序重放系统消息后生效的章节。
///
/// 就地设置、`null` 删除、再次添加则追加到末尾。
pub fn replay_sections(messages: &[Message]) -> IndexMap<String, String> {
    let mut shown = IndexMap::new();
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        let Some(sections) = &system.sections else {
            continue;
        };
        for (key, value) in sections {
            match value {
                Some(value) => {
                    shown.insert(key.clone(), value.clone());
                }
                None => {
                    shown.shift_remove(key);
                }
            }
        }
    }
    shown
}

/// 对应 `renderSections(sections, input, shown, report, context)`：按序渲染 agent 的章节。
///
/// `None` 省略一个章节；带标签的文本被包成 `<key>\n...\n</key>`。抛错的章节保留其已显示文本（若有）并被
/// 上报；`context` 被取消之后抛出的错误会向外传播。
pub async fn render_sections(
    sections: &[Arc<dyn PromptSection>],
    input: &PromptInput,
    shown: &IndexMap<String, String>,
    report: &(dyn Fn(SessionError) + Send + Sync),
    context: &Arc<dyn Context>,
) -> Result<IndexMap<String, String>, SessionError> {
    let mut desired = IndexMap::new();
    for section in sections {
        let rendered = section.render(input, Arc::clone(context)).await;
        let text = match rendered {
            Ok(text) => text,
            Err(error) => {
                if context
                    .abort_signal()
                    .is_some_and(|signal| signal.aborted())
                {
                    return Err(error);
                }
                report(error);
                if let Some(kept) = shown.get(section.key()) {
                    desired.insert(section.key().to_string(), kept.clone());
                }
                continue;
            }
        };
        let Some(text) = text else {
            continue;
        };
        desired.insert(
            section.key().to_string(),
            if section.is_tagged() {
                format!("<{}>\n{}\n</{}>", section.key(), text, section.key())
            } else {
                text
            },
        );
    }
    Ok(desired)
}

/// 对应 `planSystemEntries(view, desired, tools, timestamp)`：规划让 `view` 重放后的章节与工具等于
/// `desired` 与 `tools`（值与顺序都相同）的 `pi.system` 条目。
///
/// - head 标记且上下文中其后没有 `pi.system` 条目：一条完整 baseline，省略此前保留的每个 `pi.system` 条目；
///   即使它复述了重放值也会写入。
/// - 否则，当最小章节补丁会留下不同顺序时：移除每个已显示章节，再按序重新添加每个期望章节。
/// - 否则就是改动值与 `null` 删除的最小补丁，或者什么都不写。
///
/// 工具变更搭载在最后一条规划出的条目上，或单独一条。
pub fn plan_system_entries(
    view: &ContextView,
    desired: &IndexMap<String, String>,
    tools: &[Tool],
    timestamp: u64,
) -> Vec<EntryDraft> {
    let head = view.head.as_ref();
    if let Some(head) = head
        && !view
            .entries
            .iter()
            .any(|entry| SYSTEM_ENTRY.is(Some(entry)) && entry.id > head.id)
    {
        let edits: Vec<ContextEdit> = view
            .entries
            .iter()
            .filter(|entry| SYSTEM_ENTRY.is(Some(entry)))
            .map(|entry| ContextEdit::Omit { target: entry.id })
            .collect();
        let baseline = system_entry(
            Some(
                desired
                    .iter()
                    .map(|(key, value)| (key.clone(), Some(value.clone())))
                    .collect(),
            ),
            Some(ToolChanges {
                tools_removed: Vec::new(),
                tools_added: tools.iter().map(to_tool_declaration).collect(),
            }),
            timestamp,
        );
        return vec![if edits.is_empty() {
            baseline
        } else {
            let mut with_edits = baseline;
            with_edits.edits = Some(edits);
            with_edits
        }];
    }

    let sections = plan_sections(&replay_sections(&view.messages), desired);
    let changes = plan_tools(&get_current_tools(&view.messages), tools);
    if changes.is_empty() {
        return sections
            .into_iter()
            .map(|patch| system_entry(Some(patch), None, timestamp))
            .collect();
    }
    if sections.is_empty() {
        return vec![system_entry(None, Some(changes), timestamp)];
    }
    let last = sections.len() - 1;
    sections
        .into_iter()
        .enumerate()
        .map(|(index, patch)| {
            system_entry(
                Some(patch),
                if index == last {
                    Some(changes.clone())
                } else {
                    None
                },
                timestamp,
            )
        })
        .collect()
}

/// 对应 `planTools(offered, desired)`：从 `offered` 到 `desired` 的工具变更。
///
/// 声明改变的工具有效地「先移除再重新添加」。重放会保留既有工具的位置并追加新增；若这无法得到期望顺序，
/// 则移除每个已提供工具并按序重新添加每个期望工具。
pub fn plan_tools(offered: &[Tool], desired: &[Tool]) -> ToolChanges {
    let wanted: IndexMap<&str, &Tool> = desired
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();
    let kept: Vec<&Tool> = offered
        .iter()
        .filter(|tool| {
            wanted
                .get(tool.name.as_str())
                .is_some_and(|next| declarations_equal(tool, next))
        })
        .collect();
    let kept_names: BTreeSet<&str> = kept.iter().map(|tool| tool.name.as_str()).collect();
    let added: Vec<&Tool> = desired
        .iter()
        .filter(|tool| !kept_names.contains(tool.name.as_str()))
        .collect();

    let replayed: Vec<&Tool> = kept.iter().copied().chain(added.iter().copied()).collect();
    let order_differs = replayed.iter().enumerate().any(|(index, tool)| {
        Some(tool.name.as_str()) != desired.get(index).map(|tool| tool.name.as_str())
    });
    if order_differs {
        return ToolChanges {
            tools_removed: offered
                .iter()
                .map(|tool| ToolReference {
                    name: tool.name.clone(),
                })
                .collect(),
            tools_added: desired.iter().map(to_tool_declaration).collect(),
        };
    }
    ToolChanges {
        tools_removed: offered
            .iter()
            .filter(|tool| !kept_names.contains(tool.name.as_str()))
            .map(|tool| ToolReference {
                name: tool.name.clone(),
            })
            .collect(),
        tools_added: added.iter().map(|tool| to_tool_declaration(tool)).collect(),
    }
}

/// 对应 `planSections(shown, desired)`：章节补丁——无、最小补丁，或顺序不同时的一对「全删 + 全加」。
pub fn plan_sections(
    shown: &IndexMap<String, String>,
    desired: &IndexMap<String, String>,
) -> Vec<SectionPatch> {
    let patched_order: Vec<&String> = shown
        .keys()
        .filter(|key| desired.contains_key(*key))
        .chain(desired.keys().filter(|key| !shown.contains_key(*key)))
        .collect();
    let desired_order: Vec<&String> = desired.keys().collect();
    if patched_order
        .iter()
        .zip(&desired_order)
        .any(|(left, right)| left != right)
        || patched_order.len() != desired_order.len()
    {
        let mut remove_all = SectionPatch::new();
        for key in shown.keys() {
            remove_all.insert(key.clone(), None);
        }
        let mut add_all = SectionPatch::new();
        for (key, value) in desired {
            add_all.insert(key.clone(), Some(value.clone()));
        }
        return vec![remove_all, add_all];
    }

    let mut patch = SectionPatch::new();
    for (key, value) in shown {
        match desired.get(key) {
            Some(next) if next == value => {}
            Some(next) => {
                patch.insert(key.clone(), Some(next.clone()));
            }
            None => {
                patch.insert(key.clone(), None);
            }
        }
    }
    for (key, value) in desired {
        if !shown.contains_key(key) {
            patch.insert(key.clone(), Some(value.clone()));
        }
    }
    if patch.is_empty() {
        Vec::new()
    } else {
        vec![patch]
    }
}

/// 对应 `systemEntry(sections, tools, timestamp)`：一条 `pi.system` 条目草稿。
fn system_entry(
    sections: Option<SectionPatch>,
    tools: Option<ToolChanges>,
    timestamp: u64,
) -> EntryDraft {
    let sections = sections.map(|sections| sections.into_iter().collect::<IndexMap<_, _>>());
    let message = SystemMessage {
        content: SystemContent::Text(String::new()),
        sections,
        tools_removed: tools
            .as_ref()
            .filter(|tools| !tools.tools_removed.is_empty())
            .map(|tools| tools.tools_removed.clone()),
        tools_added: tools
            .as_ref()
            .filter(|tools| !tools.tools_added.is_empty())
            .map(|tools| tools.tools_added.clone()),
        timestamp,
    };
    EntryDraft {
        kind: SYSTEM_ENTRY.kind().to_string(),
        model: Some(vec![Message::System(message)]),
        data: None,
        head: None,
        edits: None,
    }
}

/// 便于阅读：上游 `sections` 的 `null` 在 Rust 侧由 [`Option`] 表达。
#[allow(dead_code)]
fn section_patch_json(patch: &SectionPatch) -> JsonValue {
    let mut object = serde_json::Map::new();
    for (key, value) in patch {
        object.insert(
            key.clone(),
            value
                .clone()
                .map(JsonValue::from)
                .unwrap_or(JsonValue::Null),
        );
    }
    JsonValue::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::{SystemMessage, UserContent, UserMessage};

    fn system(sections: Vec<(&str, Option<&str>)>, timestamp: u64) -> Message {
        let mut map = indexmap::IndexMap::new();
        for (key, value) in sections {
            map.insert(key.to_string(), value.map(str::to_string));
        }
        Message::System(SystemMessage {
            content: SystemContent::Text(String::new()),
            sections: Some(map),
            tools_added: None,
            tools_removed: None,
            timestamp,
        })
    }

    fn user(text: &str, timestamp: u64) -> Message {
        Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp,
        })
    }

    fn tool(name: &str, description: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: description.to_string(),
            parameters: serde_json::json!({"type": "object"}),
            constrained_sampling: None,
        }
    }

    fn sections(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn replay_sections_sets_deletes_and_appends() {
        let messages = vec![
            system(vec![("a", Some("1")), ("b", Some("2"))], 1),
            system(vec![("b", None)], 2),
            system(vec![("b", Some("3"))], 3),
        ];
        let shown = replay_sections(&messages);
        assert_eq!(shown.get("a").map(String::as_str), Some("1"));
        assert_eq!(shown.get("b").map(String::as_str), Some("3"));
    }

    #[test]
    fn replay_sections_ignores_messages_without_sections() {
        let messages = vec![user("hi", 1), system(vec![], 2)];
        assert!(replay_sections(&messages).is_empty());
    }

    #[test]
    fn plan_tools_keeps_matching_declarations_in_place() {
        let offered = vec![tool("a", "A"), tool("b", "B")];
        let desired = vec![tool("a", "A"), tool("c", "C")];
        let changes = plan_tools(&offered, &desired);
        assert_eq!(
            changes
                .tools_removed
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
        assert_eq!(
            changes
                .tools_added
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["c"]
        );
    }

    #[test]
    fn plan_tools_removes_and_re_adds_a_changed_declaration() {
        let offered = vec![tool("a", "old")];
        let desired = vec![tool("a", "new")];
        let changes = plan_tools(&offered, &desired);
        assert_eq!(changes.tools_removed.len(), 1);
        assert_eq!(changes.tools_added.len(), 1);
        assert_eq!(changes.tools_added[0].description, "new");
    }

    #[test]
    fn plan_tools_rebuilds_everything_when_the_order_would_differ() {
        let offered = vec![tool("a", "A"), tool("b", "B")];
        let desired = vec![tool("b", "B"), tool("a", "A")];
        let changes = plan_tools(&offered, &desired);
        assert_eq!(changes.tools_removed.len(), 2, "顺序不同即全部重建");
        assert_eq!(changes.tools_added.len(), 2);
        assert_eq!(
            changes
                .tools_added
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    #[test]
    fn plan_tools_is_empty_when_nothing_changes() {
        let offered = vec![tool("a", "A")];
        assert!(plan_tools(&offered, &offered).is_empty());
    }

    #[test]
    fn plan_sections_produces_a_minimal_patch() {
        let shown = sections(&[("a", "1"), ("b", "2")]);
        let desired = sections(&[("a", "1"), ("b", "3"), ("c", "4")]);
        let patches = plan_sections(&shown, &desired);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].get("b"), Some(&Some("3".to_string())));
        assert_eq!(patches[0].get("c"), Some(&Some("4".to_string())));
        assert_eq!(patches[0].get("a"), None, "未变化的章节不入补丁");
    }

    #[test]
    fn plan_sections_deletes_a_removed_one_with_null() {
        let shown = sections(&[("a", "1"), ("b", "2")]);
        let desired = sections(&[("a", "1")]);
        let patches = plan_sections(&shown, &desired);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].get("b"), Some(&None));
    }

    #[test]
    fn plan_sections_rebuilds_when_the_order_would_differ() {
        let shown = sections(&[("a", "1"), ("b", "2")]);
        let desired = sections(&[("b", "2"), ("a", "1")]);
        let patches = plan_sections(&shown, &desired);
        assert_eq!(patches.len(), 2, "先全删再全加");
        assert_eq!(patches[0].get("a"), Some(&None));
        assert_eq!(patches[0].get("b"), Some(&None));
        assert_eq!(patches[1].get("b"), Some(&Some("2".to_string())));
        assert_eq!(patches[1].get("a"), Some(&Some("1".to_string())));
    }

    #[test]
    fn plan_sections_is_empty_without_changes() {
        let shown = sections(&[("a", "1")]);
        assert!(plan_sections(&shown, &shown).is_empty());
    }

    #[test]
    fn system_entry_carries_sections_and_tools() {
        let mut patch = SectionPatch::new();
        patch.insert("a".to_string(), Some("1".to_string()));
        let draft = system_entry(Some(patch), None, 7);
        assert_eq!(draft.kind, "pi.system");
        let Some(Message::System(message)) = draft.model.as_ref().and_then(|model| model.first())
        else {
            panic!("expected a system message");
        };
        assert_eq!(message.sections.as_ref().expect("sections").len(), 1);
        assert!(message.tools_added.is_none(), "空变更不写 toolsAdded");
        assert_eq!(message.timestamp, 7);
    }

    #[test]
    fn system_entry_omits_null_tool_lists() {
        let draft = system_entry(
            None,
            Some(ToolChanges {
                tools_removed: Vec::new(),
                tools_added: vec![tool("a", "A")],
            }),
            1,
        );
        let Some(Message::System(message)) = draft.model.as_ref().and_then(|model| model.first())
        else {
            panic!("expected a system message");
        };
        assert!(message.tools_removed.is_none());
        assert_eq!(message.tools_added.as_ref().expect("added").len(), 1);
        assert!(message.sections.is_none());
    }
}
