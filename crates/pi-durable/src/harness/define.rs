//! 对应 `harness/define.ts`：扩展 / 工具 / 章节 / hook / 包装的 DSL 构造器。
//!
//! 上游这些函数多是**恒等函数**，存在只为让 TypeScript 推导泛型；Rust 侧保留了同样的
//! 调用形状（`define_tool` 顺带完成装箱），以便扩展作者写出的代码与上游一一对应。

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::chord::context::Context;
use crate::harness::types::{
    Extension, HookHandlers, HookRegistration, PromptInput, PromptSection, SectionWrapper,
    ToolRegistration, ToolWrapper, Wrap,
};
use crate::session::SessionError;
use crate::types::Task;

/// 对应 `section()` 的 `options`。
#[derive(Debug, Clone, Copy, Default)]
pub struct SectionOptions {
    /// 对应 `tag`：默认 `true`。
    pub tag: Option<bool>,
}

/// 对应 `section(key, render, options?)` 的渲染函数。
pub type SectionRenderer = Arc<
    dyn for<'a> Fn(
            &'a PromptInput,
            Arc<dyn Context>,
        ) -> BoxFuture<'a, Result<Option<String>, SessionError>>
        + Send
        + Sync,
>;

/// 对应 `section()` 返回的具体章节。
pub struct RenderSection {
    key: String,
    tag: bool,
    render: SectionRenderer,
}

impl RenderSection {
    /// 由 `section()` 构造。
    pub fn new(key: impl Into<String>, render: SectionRenderer, options: SectionOptions) -> Self {
        Self {
            key: key.into(),
            tag: options.tag.unwrap_or(true),
            render,
        }
    }
}

#[async_trait::async_trait]
impl PromptSection for RenderSection {
    fn key(&self) -> &str {
        &self.key
    }

    fn is_tagged(&self) -> bool {
        self.tag
    }

    async fn render(
        &self,
        input: &PromptInput,
        context: Arc<dyn Context>,
    ) -> Result<Option<String>, SessionError> {
        (self.render)(input, context).await
    }
}

/// 对应 `defineExtension(extension)`：给扩展定型。
pub fn define_extension(extension: Extension) -> Extension {
    extension
}

/// 对应 `defineTool(tool)`：给工具定型并装箱为注册表条目。
///
/// 上游的恒等函数由 TS 泛型推导需要；Rust 侧顺带完成 `Arc<dyn ToolRegistration>` 装箱。
pub fn define_tool<T: ToolRegistration + 'static>(tool: T) -> Arc<dyn ToolRegistration> {
    Arc::new(tool)
}

/// 对应 `section(key, render, options?)`：一个提示章节；除非 `tag` 为 `false`，否则加标签。
pub fn section(
    key: impl Into<String>,
    render: SectionRenderer,
    options: Option<SectionOptions>,
) -> Arc<dyn PromptSection> {
    Arc::new(RenderSection::new(key, render, options.unwrap_or_default()))
}

/// 对应 `hook(task, handlers)`：匹配 `task` 名字的任务的 hook 处理器。
pub fn hook(task: &Task, handlers: HookHandlers) -> Arc<HookRegistration> {
    Arc::new(HookRegistration {
        task: task.definition().name().to_string(),
        handlers,
    })
}

/// 对应 `wrapTool(tool, wrapper)`：在包装扩展被选中的地方包装与 `tool` 同名的工具。
pub fn wrap_tool(tool: &Arc<dyn ToolRegistration>, wrapper: ToolWrapper) -> Wrap {
    Wrap::Tool {
        tool: tool.name().to_string(),
        wrap: wrapper,
    }
}

/// 对应 `wrapSection(key, wrapper)`：在包装扩展被选中的地方包装章节 `key`。
pub fn wrap_section(key: impl Into<String>, wrapper: SectionWrapper) -> Wrap {
    Wrap::Section {
        section: key.into(),
        wrap: wrapper,
    }
}

/// 便于阅读：`HooksOf<K>` 在 Rust 侧的列表示意（上游按任务定义推导第四个泛型）。
#[allow(dead_code)]
type HooksOfPlaceholder = HookHandlers;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::types::{GenerationHooks, ToolExecutionMode};
    use crate::types::{TaskDefinitionSpec, TaskOwnership};
    use serde_json::Value as JsonValue;

    struct EchoTool;

    #[async_trait::async_trait]
    impl ToolRegistration for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echo the arguments back"
        }

        fn parameters(&self) -> &JsonValue {
            static PARAMETERS: std::sync::LazyLock<JsonValue> =
                std::sync::LazyLock::new(|| serde_json::json!({"type": "object"}));
            &PARAMETERS
        }

        fn execution_mode(&self) -> Option<ToolExecutionMode> {
            Some(ToolExecutionMode::Sequential)
        }

        async fn execute(
            &self,
            _args: JsonValue,
            _api: Arc<dyn crate::harness::types::ToolExecutionApi>,
            _context: Arc<dyn Context>,
        ) -> Result<crate::harness::types::ToolExecutionResult, SessionError> {
            Ok(crate::harness::types::ToolExecutionResult::default())
        }
    }

    struct DemoTask;

    impl TaskDefinitionSpec for DemoTask {
        fn name(&self) -> &str {
            "demo.task"
        }

        fn version(&self) -> u32 {
            1
        }

        fn initial(&self, _input: &JsonValue) -> JsonValue {
            JsonValue::Null
        }

        fn phases(&self) -> &[&'static str] {
            &["run"]
        }
    }

    /// 一个只实现部分 hook 的处理器（上游 `Partial<GenerationHooks>`）。
    struct NoopGenerationHooks;

    impl GenerationHooks for NoopGenerationHooks {}

    #[test]
    fn define_tool_boxes_and_preserves_metadata() {
        let tool: Arc<dyn ToolRegistration> = define_tool(EchoTool);
        assert_eq!(tool.name(), "echo");
        assert_eq!(tool.execution_mode(), Some(ToolExecutionMode::Sequential));
        assert_eq!(
            tool.parameters(),
            &serde_json::json!({"type": "object"}),
            "默认 parameters 直接透传"
        );
        assert_eq!(tool.replay(), crate::harness::types::Replay::Unsafe);
        assert_eq!(
            tool.prepare_arguments(serde_json::json!(1)).unwrap(),
            serde_json::json!(1)
        );
    }

    #[test]
    fn section_defaults_to_tagged() {
        let tagged = section(
            "notes",
            Arc::new(|_input, _context| Box::pin(async { Ok(None) })),
            None,
        );
        assert_eq!(tagged.key(), "notes");
        assert!(tagged.is_tagged());

        let untagged = section(
            "raw",
            Arc::new(|_input, _context| Box::pin(async { Ok(None) })),
            Some(SectionOptions { tag: Some(false) }),
        );
        assert!(!untagged.is_tagged());
    }

    #[tokio::test]
    async fn section_renders_through_the_trait_object() {
        let renderer = section(
            "greeting",
            Arc::new(|_input, _context| Box::pin(async { Ok(Some("hello".to_string())) })),
            None,
        );
        let input = PromptInput {
            conversation_id: crate::types::ConversationId::new(1),
            agent: crate::harness::types::Agent::default(),
            env: None,
            shown: std::collections::BTreeMap::new(),
            read: Arc::new(NoopReader),
        };
        let value = renderer
            .render(&input, crate::chord::context::BACKGROUND_CONTEXT.clone())
            .await;
        assert_eq!(value.unwrap(), Some("hello".to_string()));
    }

    struct NoopReader;

    #[async_trait::async_trait]
    impl crate::session::DocumentReader for NoopReader {
        async fn snapshot(
            &self,
            _token: &dyn crate::documents::AnyDocToken,
            _owner: Option<u64>,
            _key: Option<String>,
            _context: Arc<dyn Context>,
        ) -> Result<Option<crate::types::JsonObject>, SessionError> {
            Ok(None)
        }

        async fn snapshot_as_of(
            &self,
            _token: &dyn crate::documents::AnyDocToken,
            _owner: u64,
            _key: Option<String>,
            _at: crate::types::EntryId,
            _context: Arc<dyn Context>,
        ) -> Result<Option<crate::types::JsonObject>, SessionError> {
            Ok(None)
        }
    }

    #[test]
    fn hook_binds_the_task_name() {
        let task = Task::new(Arc::new(DemoTask));
        let registration = hook(
            &task,
            HookHandlers::Generation(Arc::new(NoopGenerationHooks)),
        );
        assert_eq!(registration.task, "demo.task");
        assert!(matches!(registration.handlers, HookHandlers::Generation(_)));
    }

    #[test]
    fn wrap_tool_uses_the_tool_name() {
        let tool = define_tool(EchoTool);
        let wrapped = wrap_tool(&tool, Arc::new(|tool| tool));
        match wrapped {
            Wrap::Tool { tool, .. } => assert_eq!(tool, "echo"),
            _ => panic!("expected a tool wrap"),
        }
    }

    #[test]
    fn wrap_section_uses_the_section_key() {
        let wrapped = wrap_section("notes", Arc::new(|section| section));
        match wrapped {
            Wrap::Section { section, .. } => assert_eq!(section, "notes"),
            _ => panic!("expected a section wrap"),
        }
    }

    #[test]
    fn ownership_default_matches_conversation_owned_tasks() {
        let options = crate::harness::types::TaskCreationOptions::default();
        assert_eq!(options.ownership, TaskOwnership::Conversation);
    }
}
