//! 对应 `harness/agent.ts`：`pi.agent` 文档、设置解析与 agent 解析。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - 上游 `applyChange` / `addTools` / `createAgent` 直接改写 Proxy 草稿；Rust 用 [`Draft`] 的显式
//!   方法（先读 [`Draft::value`] 得到快照，再 `set` / `delete`）。
//! - `Object.assign(agent, copyJson(owner))` 的「逐键覆盖、不删除」语义 → 逐键 `set`。
//! - `agentSections.push({ key: INSTRUCTIONS_KEY, render: () => instructions })` → 用
//!   [`crate::harness::define::section`] 构造等价章节。

use std::sync::Arc;
use std::sync::LazyLock;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::chord::delta::{DeltaError, Path, PathSegment};
use crate::documents::define_doc;
use crate::harness::define::{SectionOptions, section};
use crate::harness::types::{
    Agent, AgentChange, AgentState, Extension, ExtensionChangeSelection, FieldChange,
    HarnessSettings, HookHandlers, ModelRef, PromptSection, RegistrySnapshot, Settings,
    ToolChangeSelection, ToolRegistration,
};
use crate::session::SessionError;
use crate::session::transaction::Transaction;
use crate::types::{
    ConversationFork, ConversationHistory, ConversationId, ConversationRecord, DocAccess,
    DocDefinitionSpec, DocToken, DocumentSemantics, JsonObject,
};

struct AgentDefinition;

impl DocDefinitionSpec for AgentDefinition {
    fn kind(&self) -> &str {
        "pi.agent"
    }

    fn version(&self) -> u32 {
        1
    }

    fn semantics(&self) -> DocumentSemantics {
        DocumentSemantics::Conversation {
            // 可回溯：分叉从自身分叉点上的 agent 开始。
            history: ConversationHistory::Rewindable,
            fork: ConversationFork::AsOf,
        }
    }

    fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
        JsonObject::new()
    }

    fn checkpoint_when(
        &self,
        _value: &JsonObject,
        _ops: &[crate::chord::delta::Op],
        _info: &crate::types::CheckpointInfo,
    ) -> bool {
        // 上游 `checkpointWhen: () => true`。
        true
    }
}

/// 对应 `AgentDoc`：内置 agent 文档。
pub static AGENT_DOC: LazyLock<DocToken> =
    LazyLock::new(|| define_doc(Arc::new(AgentDefinition)).expect("pi.agent"));

/// 对应 `INSTRUCTIONS_KEY`：agent `instructions` 的保留章节键。
pub const INSTRUCTIONS_KEY: &str = "instructions";

/// 对应 `resolveSettings(settings)`：把宿主设置解析到内置默认之上（对象字段逐项合并）。
pub fn resolve_settings(settings: Option<&HarnessSettings>) -> Settings {
    let defaults = Settings::default();
    let Some(settings) = settings else {
        return defaults;
    };
    Settings {
        extensions: settings.extensions.clone(),
        stream: settings.stream.clone().unwrap_or(defaults.stream),
        retry: merge_retry(settings.retry.as_ref(), defaults.retry),
        compaction: merge_compaction(settings.compaction.as_ref(), defaults.compaction),
        // 逐字段解析，因此显式写下的间隔会覆盖默认值。
        progress: crate::harness::types::ProgressPolicy {
            partial_interval_ms: settings
                .progress
                .as_ref()
                .and_then(|progress| progress.partial_interval_ms)
                .unwrap_or(defaults.progress.partial_interval_ms),
            output_interval_ms: settings
                .progress
                .as_ref()
                .and_then(|progress| progress.output_interval_ms)
                .unwrap_or(defaults.progress.output_interval_ms),
        },
        tool_execution: settings.tool_execution.unwrap_or(defaults.tool_execution),
        steering_mode: settings.steering_mode.unwrap_or(defaults.steering_mode),
        follow_up_mode: settings.follow_up_mode.unwrap_or(defaults.follow_up_mode),
        context_retention_ms: settings
            .context_retention_ms
            .unwrap_or(defaults.context_retention_ms),
    }
}

fn merge_retry(
    partial: Option<&crate::harness::types::PartialRetryPolicy>,
    defaults: crate::harness::types::ConversationRetryPolicy,
) -> crate::harness::types::ConversationRetryPolicy {
    let Some(partial) = partial else {
        return defaults;
    };
    crate::harness::types::ConversationRetryPolicy {
        enabled: partial.enabled.unwrap_or(defaults.enabled),
        max_retries: partial.max_retries.unwrap_or(defaults.max_retries),
        base_delay_ms: partial.base_delay_ms.unwrap_or(defaults.base_delay_ms),
        max_agent_delay_ms: partial.max_agent_delay_ms.or(defaults.max_agent_delay_ms),
    }
}

fn merge_compaction(
    partial: Option<&crate::harness::types::PartialCompactionPolicy>,
    defaults: crate::harness::types::CompactionPolicy,
) -> crate::harness::types::CompactionPolicy {
    let Some(partial) = partial else {
        return defaults;
    };
    crate::harness::types::CompactionPolicy {
        enabled: partial.enabled.unwrap_or(defaults.enabled),
        reserve_tokens: partial.reserve_tokens.unwrap_or(defaults.reserve_tokens),
        keep_recent_tokens: partial
            .keep_recent_tokens
            .unwrap_or(defaults.keep_recent_tokens),
        background_tokens: partial
            .background_tokens
            .unwrap_or(defaults.background_tokens),
    }
}

/// 对应 `configure(tx, conversationId, change)`：对 `pi.agent` 应用一次改动。
pub async fn configure(
    tx: &Transaction,
    conversation_id: ConversationId,
    change: &AgentChange,
) -> Result<(), SessionError> {
    let state = agent_draft(tx, conversation_id).await?;
    apply_change(&state, change)
}

/// 对应 `addTools(tx, conversationId, added)`：工具轮的 `addTools`。
///
/// 数组形态会追加缺失的名字；`{ remove }` 形态会移除这些名字；未设置 tools 的会话已经提供全部工具，
/// 因此不写入任何东西。
pub async fn add_tools(
    tx: &Transaction,
    conversation_id: ConversationId,
    added: &[String],
) -> Result<(), SessionError> {
    let state = agent_draft(tx, conversation_id).await?;
    let value = state.value();
    let Some(tools) = value.get("tools") else {
        return Ok(());
    };
    match tools {
        JsonValue::Array(existing) => {
            let mut names: Vec<String> = existing
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect();
            for name in added {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
            let array: Vec<JsonValue> = names.into_iter().map(JsonValue::from).collect();
            state
                .set(path("tools"), JsonValue::Array(array))
                .map_err(delta_error)?;
        }
        JsonValue::Object(object) => {
            let remove = object
                .get("remove")
                .and_then(JsonValue::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if remove.iter().any(|name| added.contains(name)) {
                let kept: Vec<JsonValue> = remove
                    .into_iter()
                    .filter(|name| !added.contains(name))
                    .map(JsonValue::from)
                    .collect();
                state
                    .set(path("tools"), serde_json::json!({ "remove": kept }))
                    .map_err(delta_error)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// 对应 `applyChange(state, change)`：给定字段替换、`null` 清除、缺席不变。
pub fn apply_change(state: &crate::types::Draft, change: &AgentChange) -> Result<(), SessionError> {
    apply_json_field(state, "model", &json_field(&change.model))?;
    apply_json_field(state, "thinkingLevel", &json_field(&change.thinking_level))?;

    let extensions = match &change.extensions {
        FieldChange::Unchanged => FieldChange::Unchanged,
        FieldChange::Clear => FieldChange::Clear,
        FieldChange::Set(ExtensionChangeSelection::Exactly(items)) => {
            FieldChange::Set(names_json(items.iter().map(|item| item.name.as_str())))
        }
        FieldChange::Set(ExtensionChangeSelection::Edit { add, remove }) => {
            let mut object = JsonObject::new();
            if let Some(add) = add {
                object.insert(
                    "add".to_string(),
                    names_json(add.iter().map(|item| item.name.as_str())),
                );
            }
            if let Some(remove) = remove {
                object.insert(
                    "remove".to_string(),
                    names_json(remove.iter().map(|item| item.name.as_str())),
                );
            }
            FieldChange::Set(JsonValue::Object(object))
        }
    };
    apply_json_field(state, "extensions", &extensions)?;

    let tools = match &change.tools {
        FieldChange::Unchanged => FieldChange::Unchanged,
        FieldChange::Clear => FieldChange::Clear,
        FieldChange::Set(ToolChangeSelection::Exactly(items)) => {
            FieldChange::Set(names_json(items.iter().map(|item| item.name())))
        }
        FieldChange::Set(ToolChangeSelection::Remove(items)) => FieldChange::Set(
            serde_json::json!({ "remove": names_json(items.iter().map(|item| item.name())) }),
        ),
    };
    apply_json_field(state, "tools", &tools)?;

    apply_json_field(state, "instructions", &json_field(&change.instructions))?;
    apply_json_field(state, "cwd", &json_field(&change.cwd))?;
    Ok(())
}

/// 对应 `createAgent(tx, conversation)`：每个创建或分叉会话的提交里的内置部分。
///
/// 分叉保留其 `asOf` 副本；任务拥有的新会话复制其拥有者任务会话的已存 agent；新的无主会话从空开始。
pub async fn create_agent(
    tx: &Transaction,
    conversation: &ConversationRecord,
) -> Result<(), SessionError> {
    if conversation.parent.is_some() {
        return Ok(());
    }
    let agent = agent_draft(tx, conversation.id).await?;
    let Some(owner) = conversation.owner else {
        return Ok(());
    };
    let owner_agent = agent_draft(tx, owner.conversation_id).await?;
    // `Object.assign` 语义：逐键覆盖，不删除目标已有的键。
    let source = owner_agent.value();
    if let Some(object) = source.as_object() {
        for (name, value) in object {
            agent.set(path(name), value.clone()).map_err(delta_error)?;
        }
    }
    Ok(())
}

/// 对应 `agentHooks(agent, taskName)`：选中扩展里匹配任务名的处理器，按扩展顺序。
pub fn agent_hooks<'a>(agent: &'a Agent, task_name: &str) -> Vec<&'a HookHandlers> {
    let mut handlers = Vec::new();
    for extension in &agent.extensions {
        for hook in &extension.hooks {
            if hook.task == task_name {
                handlers.push(&hook.handlers);
            }
        }
    }
    handlers
}

/// 对应 `resolveAgent(state, snapshot, settings, report)`。
///
/// 抛错或改名的包装会丢弃其目标并被上报；没有目标的包装什么也不做。
pub fn resolve_agent(
    state: Option<&AgentState>,
    snapshot: &RegistrySnapshot,
    settings: &Settings,
    report: &dyn Fn(String),
) -> Agent {
    let extensions = select_extensions(
        state.and_then(|state| state.extensions.as_ref()),
        snapshot,
        settings,
    );

    // 后安装的同名工具/章节覆盖先前的（对应上游 `Map.set`）。
    let mut composed: IndexMap<String, Arc<dyn ToolRegistration>> = IndexMap::new();
    for extension in &extensions {
        for tool in &extension.tools {
            composed.insert(tool.name().to_string(), Arc::clone(tool));
        }
    }
    let mut sections: IndexMap<String, Arc<dyn PromptSection>> = IndexMap::new();
    for extension in &extensions {
        for section in &extension.sections {
            sections.insert(section.key().to_string(), Arc::clone(section));
        }
    }
    for extension in &extensions {
        for wrap in &extension.wraps {
            match wrap {
                crate::harness::types::Wrap::Tool { tool, wrap } => {
                    apply_wrap(
                        &mut composed,
                        tool,
                        |item| wrap(Arc::clone(item)),
                        |item| item.name().to_string(),
                        report,
                    );
                }
                crate::harness::types::Wrap::Section { section, wrap } => {
                    apply_wrap(
                        &mut sections,
                        section,
                        |item| wrap(Arc::clone(item)),
                        |item| item.key().to_string(),
                        report,
                    );
                }
            }
        }
    }

    let filter = state.and_then(|state| state.tools.as_ref());
    let tools: Vec<Arc<dyn ToolRegistration>> = match filter {
        None => composed.values().map(Arc::clone).collect(),
        Some(crate::harness::types::ToolStateSelection::Names(names)) => {
            let mut tools = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for name in names {
                if !seen.insert(name.clone()) {
                    continue;
                }
                if let Some(tool) = composed.get(name) {
                    tools.push(Arc::clone(tool));
                }
            }
            tools
        }
        Some(crate::harness::types::ToolStateSelection::Remove(removed)) => {
            let removed: std::collections::BTreeSet<&String> = removed.iter().collect();
            composed
                .values()
                .filter(|tool| !removed.contains(&tool.name().to_string()))
                .map(Arc::clone)
                .collect()
        }
    };

    let instructions = state.and_then(|state| state.instructions.clone());
    let mut agent_sections: Vec<Arc<dyn PromptSection>> =
        sections.values().map(Arc::clone).collect();
    if let Some(instructions) = instructions.clone() {
        agent_sections.push(section(
            INSTRUCTIONS_KEY,
            Arc::new(move |_input, _context| {
                let text = instructions.clone();
                Box::pin(async move { Ok(Some(text)) })
            }),
            Some(SectionOptions { tag: Some(true) }),
        ));
    }

    Agent {
        model: state.and_then(|state| state.model.clone()),
        thinking_level: state
            .and_then(|state| state.thinking_level)
            .unwrap_or(pi_ai::ModelThinkingLevel::Off),
        extensions,
        tools,
        sections: agent_sections,
        instructions,
        cwd: state.and_then(|state| state.cwd.clone()),
    }
}

/// 对应 `selectExtensions(stored, snapshot, settings)`：已存储的数组，或用 `{ add, remove }` 编辑过的默认选择。
fn select_extensions(
    stored: Option<&crate::harness::types::ExtensionStateSelection>,
    snapshot: &RegistrySnapshot,
    settings: &Settings,
) -> Vec<Arc<Extension>> {
    let selected: Vec<String> = match stored {
        Some(crate::harness::types::ExtensionStateSelection::Names(names)) => names.clone(),
        other => {
            let base: Vec<String> = match &settings.extensions {
                Some(extensions) => extensions.iter().map(|item| item.name.clone()).collect(),
                None => snapshot
                    .installed()
                    .iter()
                    .map(|item| item.name.clone())
                    .collect(),
            };
            let (add, remove) = match other {
                Some(crate::harness::types::ExtensionStateSelection::Edit { add, remove }) => (
                    add.clone().unwrap_or_default(),
                    remove.clone().unwrap_or_default(),
                ),
                _ => (Vec::new(), Vec::new()),
            };
            let removed: std::collections::BTreeSet<String> = remove.into_iter().collect();
            base.into_iter()
                .chain(add)
                .filter(|name| !removed.contains(name))
                .collect()
        }
    };

    let mut extensions = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for name in selected {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(extension) = snapshot.extension(&name) {
            extensions.push(Arc::clone(extension));
        }
    }
    extensions
}

/// 对应 `applyWrap(items, target, wrap, nameOf, report)`。
fn apply_wrap<T: Clone>(
    items: &mut IndexMap<String, T>,
    target: &str,
    wrap: impl Fn(&T) -> T,
    name_of: impl Fn(&T) -> String,
    report: &dyn Fn(String),
) {
    let Some(item) = items.get(target) else {
        return;
    };
    let wrapped = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wrap(item))) {
        Ok(wrapped) => wrapped,
        Err(payload) => {
            items.shift_remove(target);
            report(panic_message(payload));
            return;
        }
    };
    let name = name_of(&wrapped);
    if name != target {
        items.shift_remove(target);
        report(format!("Wrapper renamed {target} to {name}"));
        return;
    }
    items.insert(target.to_string(), wrapped);
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_string(),
            Err(_) => "wrapper panicked".to_string(),
        },
    }
}

async fn agent_draft(
    tx: &Transaction,
    conversation_id: ConversationId,
) -> Result<crate::types::Draft, SessionError> {
    Ok(tx
        .doc(
            &*AGENT_DOC,
            DocAccess {
                owner: Some(conversation_id.get()),
                key: None,
            },
            None,
        )
        .await?)
}

fn apply_json_field(
    state: &crate::types::Draft,
    key: &str,
    field: &FieldChange<JsonValue>,
) -> Result<(), SessionError> {
    match field {
        FieldChange::Unchanged => Ok(()),
        FieldChange::Clear => state.delete(path(key)).map_err(delta_error),
        FieldChange::Set(value) => state.set(path(key), value.clone()).map_err(delta_error),
    }
}

fn json_field<T: Serialize>(field: &FieldChange<T>) -> FieldChange<JsonValue> {
    match field {
        FieldChange::Unchanged => FieldChange::Unchanged,
        FieldChange::Clear => FieldChange::Clear,
        FieldChange::Set(value) => {
            FieldChange::Set(serde_json::to_value(value).expect("field serialises"))
        }
    }
}

fn names_json<'a>(names: impl Iterator<Item = &'a str>) -> JsonValue {
    JsonValue::Array(names.map(JsonValue::from).collect())
}

fn path(key: &str) -> Path {
    vec![PathSegment::Key(key.to_string())]
}

fn delta_error(error: DeltaError) -> SessionError {
    SessionError::Message(error.to_string())
}

/// 便于阅读：上游 `AgentState["model"]` 的字段类型。
#[allow(dead_code)]
type AgentModel = ModelRef;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::types::{ExtensionStateSelection, ToolExecutionMode, ToolStateSelection};

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl ToolRegistration for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn parameters(&self) -> &JsonValue {
            static PARAMETERS: std::sync::LazyLock<JsonValue> =
                std::sync::LazyLock::new(|| serde_json::json!({"type": "object"}));
            &PARAMETERS
        }

        async fn execute(
            &self,
            _args: JsonValue,
            _api: Arc<dyn crate::harness::types::ToolExecutionApi>,
            _context: Arc<dyn crate::chord::context::Context>,
        ) -> Result<crate::harness::types::ToolExecutionResult, SessionError> {
            Ok(crate::harness::types::ToolExecutionResult::default())
        }
    }

    fn extension(name: &str, tools: Vec<Arc<dyn ToolRegistration>>) -> Arc<Extension> {
        Arc::new(Extension {
            name: name.to_string(),
            tools,
            sections: Vec::new(),
            hooks: Vec::new(),
            wraps: Vec::new(),
            tasks: Vec::new(),
        })
    }

    #[test]
    fn doc_kind_and_semantics_match_upstream() {
        let definition = AGENT_DOC.definition();
        assert_eq!(definition.kind(), "pi.agent");
        assert_eq!(definition.version(), 1);
        assert_eq!(
            definition.semantics(),
            DocumentSemantics::Conversation {
                history: ConversationHistory::Rewindable,
                fork: ConversationFork::AsOf,
            }
        );
        assert_eq!(definition.initial(None), JsonObject::new());
    }

    #[test]
    fn resolve_settings_merges_partial_fields_over_defaults() {
        let settings = resolve_settings(None);
        assert_eq!(settings.compaction.reserve_tokens, 16_384);
        assert_eq!(
            settings.steering_mode,
            crate::harness::types::QueueMode::OneAtATime
        );

        let partial = HarnessSettings {
            retry: Some(crate::harness::types::PartialRetryPolicy {
                max_retries: Some(9),
                ..Default::default()
            }),
            progress: Some(crate::harness::types::PartialProgressPolicy {
                partial_interval_ms: Some(7),
                output_interval_ms: None,
            }),
            tool_execution: Some(ToolExecutionMode::Sequential),
            ..HarnessSettings::default()
        };
        let resolved = resolve_settings(Some(&partial));
        assert_eq!(resolved.retry.max_retries, 9, "显式字段覆盖默认");
        assert_eq!(resolved.retry.base_delay_ms, 2_000, "未给字段取默认");
        assert_eq!(resolved.progress.partial_interval_ms, 7);
        assert_eq!(
            resolved.progress.output_interval_ms, 100,
            "显式缺席的间隔保留默认"
        );
        assert_eq!(resolved.tool_execution, ToolExecutionMode::Sequential);
        assert_eq!(resolved.context_retention_ms, 600_000);
    }

    #[test]
    fn select_extensions_uses_the_stored_array_when_present() {
        let snapshot = RegistrySnapshot::new(
            vec![
                extension("a", Vec::new()),
                extension("b", Vec::new()),
                extension("c", Vec::new()),
            ],
            Vec::new(),
        );
        let settings = Settings::default();
        let stored = ExtensionStateSelection::Names(vec!["c".to_string(), "a".to_string()]);
        let selected = select_extensions(Some(&stored), &snapshot, &settings);
        assert_eq!(
            selected
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["c", "a"]
        );
    }

    #[test]
    fn select_extensions_edits_the_default_selection() {
        let snapshot = RegistrySnapshot::new(
            vec![
                extension("a", Vec::new()),
                extension("b", Vec::new()),
                extension("c", Vec::new()),
            ],
            Vec::new(),
        );
        let settings = Settings::default();
        let stored = ExtensionStateSelection::Edit {
            add: Some(vec!["c".to_string()]),
            remove: Some(vec!["a".to_string()]),
        };
        let selected = select_extensions(Some(&stored), &snapshot, &settings);
        assert_eq!(
            selected
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"],
            "默认选择去掉 a，再追加 c"
        );
    }

    #[test]
    fn select_extensions_drops_unknown_names_and_duplicates() {
        let snapshot = RegistrySnapshot::new(vec![extension("a", Vec::new())], Vec::new());
        let stored = ExtensionStateSelection::Names(vec![
            "a".to_string(),
            "missing".to_string(),
            "a".to_string(),
        ]);
        let selected = select_extensions(Some(&stored), &snapshot, &Settings::default());
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn resolve_agent_composes_tools_and_filters_them() {
        let first = extension("first", vec![Arc::new(NamedTool("read"))]);
        let second = extension("second", vec![Arc::new(NamedTool("bash"))]);
        let snapshot = RegistrySnapshot::new(vec![first, second], Vec::new());

        let full = resolve_agent(None, &snapshot, &Settings::default(), &|_| {});
        assert_eq!(
            full.tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>(),
            vec!["read", "bash"]
        );

        let filtered = resolve_agent(
            Some(&AgentState {
                tools: Some(ToolStateSelection::Names(vec!["bash".to_string()])),
                ..AgentState::default()
            }),
            &snapshot,
            &Settings::default(),
            &|_| {},
        );
        assert_eq!(
            filtered
                .tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>(),
            vec!["bash"]
        );

        let removed = resolve_agent(
            Some(&AgentState {
                tools: Some(ToolStateSelection::Remove(vec!["read".to_string()])),
                ..AgentState::default()
            }),
            &snapshot,
            &Settings::default(),
            &|_| {},
        );
        assert_eq!(
            removed
                .tools
                .iter()
                .map(|tool| tool.name())
                .collect::<Vec<_>>(),
            vec!["bash"]
        );
    }

    #[test]
    fn resolve_agent_defaults_thinking_level_to_off() {
        let agent = resolve_agent(
            None,
            &RegistrySnapshot::default(),
            &Settings::default(),
            &|_| {},
        );
        assert_eq!(agent.thinking_level, pi_ai::ModelThinkingLevel::Off);
        assert!(agent.sections.is_empty());
        assert!(agent.model.is_none());
    }

    #[test]
    fn resolve_agent_appends_the_instructions_section() {
        let state = AgentState {
            instructions: Some("be terse".to_string()),
            ..AgentState::default()
        };
        let agent = resolve_agent(
            Some(&state),
            &RegistrySnapshot::default(),
            &Settings::default(),
            &|_| {},
        );
        assert_eq!(agent.sections.len(), 1);
        assert_eq!(agent.sections[0].key(), INSTRUCTIONS_KEY);
        assert!(agent.sections[0].is_tagged());
        assert_eq!(agent.instructions.as_deref(), Some("be terse"));
    }

    #[test]
    fn apply_wrap_drops_a_renaming_wrapper_and_reports() {
        let mut items: IndexMap<String, Arc<dyn ToolRegistration>> = IndexMap::new();
        items.insert("read".to_string(), Arc::new(NamedTool("read")));
        let reported = std::sync::Mutex::new(Vec::new());
        apply_wrap(
            &mut items,
            "read",
            |_| Arc::new(NamedTool("renamed")) as Arc<dyn ToolRegistration>,
            |item| item.name().to_string(),
            &|message| reported.lock().expect("reported").push(message),
        );
        assert!(items.is_empty(), "改名的包装会丢弃目标");
        assert_eq!(
            reported.lock().expect("reported")[0],
            "Wrapper renamed read to renamed"
        );
    }

    #[test]
    fn apply_wrap_ignores_a_missing_target() {
        let mut items: IndexMap<String, Arc<dyn ToolRegistration>> = IndexMap::new();
        let reported = std::sync::Mutex::new(Vec::new());
        apply_wrap(
            &mut items,
            "missing",
            |item| Arc::clone(item),
            |item| item.name().to_string(),
            &|message| reported.lock().expect("reported").push(message),
        );
        assert!(reported.lock().expect("reported").is_empty());
    }

    #[test]
    fn agent_hooks_follow_extension_order() {
        struct NoopGenerationHooks;
        impl crate::harness::types::GenerationHooks for NoopGenerationHooks {}

        let hooks_for = |name: &str| {
            Arc::new(Extension {
                name: name.to_string(),
                tools: Vec::new(),
                sections: Vec::new(),
                hooks: vec![Arc::new(crate::harness::types::HookRegistration {
                    task: "pi.generation".to_string(),
                    handlers: HookHandlers::Generation(Arc::new(NoopGenerationHooks)),
                })],
                wraps: Vec::new(),
                tasks: Vec::new(),
            })
        };

        let agent = Agent {
            extensions: vec![hooks_for("first"), hooks_for("second")],
            ..Agent::default()
        };
        assert_eq!(agent_hooks(&agent, "pi.generation").len(), 2);
        assert!(agent_hooks(&agent, "other").is_empty());
    }
}
