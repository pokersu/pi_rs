//! 对应 `harness/types.ts`：harness 的类型契约。
//!
//! # 本阶段的切分（P5b）
//!
//! 上游 `types.ts` 同时包含**数据类型**、**扩展契约**与**顶层编排接口**。Rust 侧按返工风险切开：
//!
//! - **已落地**：数据模型（提交、工具结果、Agent 状态/解析结果、策略与设置、视图与探查）与
//!   **扩展契约**（`ToolRegistration`、`PromptSection`、`HookApi`、三类内置任务 Hook、
//!   `ToolExecutionApi`、`ConversationHandle`、`Submission`）—— 它们是对外 API，形状由上游固定。
//! - **留到 P5h**：`Harness`、`Conversation`、`ConversationWatch`、`Registry` / `RegistryReader`、
//!   `HarnessOptions`、`ConversationInit` / `ConversationCreateOptions` —— 它们的方法面直接建立在
//!   scheduler / 内置文档 / 组装之上，现在定形会随实现返工（见 `UPSTREAM-SYNC-v1.1.0.md` 的 P5b 说明）。
//!
//! # 与上游的差异（语言机制所致）
//!
//! - **`undefined` vs `null`**：上游用「字段缺席 = 不变、`null` = 清除、值 = 替换」三态；
//!   Rust 用 [`FieldChange`] 显式表达。
//! - **泛型擦除**：`T extends JsonValue` / `Static<TSchema>` / `TaskId<R>` 在 Rust 侧擦除为
//!   [`JsonValue`] / [`TaskId`]（与 P2 的既有惯例一致）。
//! - **`Partial<Hooks>`**：上游允许只实现部分 hook；Rust 的 trait 方法**都有默认实现**
//!   （返回「不干预」），达到同样效果。
//! - **重载 → 显式方法名**：`memo(name, ctx)` / `memo(name, candidate, ctx)` → `memo` / `memo_or`。
//! - **泛型方法进不了 dyn trait**：`ToolExecutionApi::commit` 接收 [`CommitOperation`]
//!   （boxed 闭包 + HRTB），与 P4 的 `SessionImpl::commit` 同一手法。

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::chord::context::Context;
use crate::chord::state::AttachedReplicatedState;
use crate::documents::AnyDocToken;
use crate::env::{ExecutionEnv, ShellOutputSkip, ShellOutputWindow};
use crate::harness::output::{OutputChunk, OutputLimits};
use crate::harness::usage::UsageState;
use crate::session::transaction::{AnyTaskRecord, Transaction};
use crate::session::{DocumentObserver, DocumentReader, Session, SessionError};
use crate::types::{
    ConversationId, ConversationOwnership, ConversationRecord, Cursor, EntryDraft, EntryId,
    EntryQuery, EntryRecord, JsonObject, Page, SubmissionId, SubmissionRecord, Task, TaskId,
    TaskOptions, WatchHandle,
};

/// 对应 `ModelRef`：经 pi-ai `Models` 解析的 provider 与模型 ID。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    /// Provider 名。
    pub provider: String,
    /// 模型 ID。
    pub model_id: String,
}

/// 对应 `UserInput = UserMessage["content"]`。
pub type UserInput = pi_ai::UserContent;

/// 对应 `undefined` / `null` / 值 的三态字段（上游可空字段的通用语义）。
///
/// 上游：字段缺席表示「不改变」，`null` 表示「清除」，给值表示「替换」。
#[derive(Debug, Clone, PartialEq, Default)]
pub enum FieldChange<T> {
    /// 字段缺席：不改变。
    #[default]
    Unchanged,
    /// `null`：清除。
    Clear,
    /// 给定值：替换。
    Set(T),
}

impl<T> FieldChange<T> {
    /// 是否要求改动。
    pub fn is_unchanged(&self) -> bool {
        matches!(self, FieldChange::Unchanged)
    }

    /// 取出要写入的值（清除时得到 `None`）；不改变时为 `None`。
    pub fn into_value(self) -> Option<Option<T>> {
        match self {
            FieldChange::Unchanged => None,
            FieldChange::Clear => Some(None),
            FieldChange::Set(value) => Some(Some(value)),
        }
    }
}

// ─── 提交 ────────────────────────────────────────────────────────────────────

/// 对应 `whenBusy: "steer" | "followUp" | "reject"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WhenBusy {
    /// 插入当前运行。
    Steer,
    /// 当前运行结束后执行。
    FollowUp,
    /// 直接以 `ConversationBusy` 拒绝。
    Reject,
}

/// 对应 `SubmissionDraft`：宿主提交——可能启动一次运行的用户输入，或一次被动条目写入。
#[derive(Debug, Clone, PartialEq)]
pub enum SubmissionDraft {
    /// `type: "input"`。
    Input(InputSubmissionDraft),
    /// `type: "write"`。
    Write(WriteSubmissionDraft),
}

/// 对应 `SubmissionDraft` 的 `input` 分支。
#[derive(Debug, Clone, PartialEq)]
pub struct InputSubmissionDraft {
    /// 去重键。
    pub request_id: Option<String>,
    /// 用户输入。
    pub content: UserInput,
    /// 会话繁忙时的处理方式。
    pub when_busy: Option<WhenBusy>,
}

/// 对应 `SubmissionDraft` 的 `write` 分支。
#[derive(Debug, Clone, PartialEq)]
pub struct WriteSubmissionDraft {
    /// 去重键。
    pub request_id: Option<String>,
    /// 要写入的条目。
    pub entry: EntryDraft,
}

/// 对应 `SettledSubmissionRecord`：终态（`done` / `unanswered`）的提交记录。
#[derive(Debug, Clone, PartialEq)]
pub struct SettledSubmissionRecord(SubmissionRecord);

impl SettledSubmissionRecord {
    /// 由已确认处于终态的记录构造。
    ///
    /// 上游用交叉类型保证；Rust 在构造点校验，越界时 panic（调用方保证语义）。
    pub fn new(record: SubmissionRecord) -> Self {
        assert!(
            matches!(
                record.status(),
                crate::types::SubmissionStatus::Done | crate::types::SubmissionStatus::Unanswered
            ),
            "SettledSubmissionRecord 要求终态提交"
        );
        Self(record)
    }

    /// 记录本身。
    pub fn record(&self) -> &SubmissionRecord {
        &self.0
    }

    /// 取出记录。
    pub fn into_record(self) -> SubmissionRecord {
        self.0
    }
}

/// 对应 `SettledTask<R>`：已终态的任务记录。
#[derive(Debug, Clone, PartialEq)]
pub struct SettledTask {
    /// 任务记录（`state.status` 为 `terminal`）。
    pub record: AnyTaskRecord,
}

/// 对应 `Submission.abort()` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionAbort {
    /// `aborted`。
    Aborted,
    /// `already_placed`。
    AlreadyPlaced,
    /// `settled`。
    Settled,
}

/// 对应 `Submission`：一个已持久接纳的提交的等待句柄。
#[async_trait::async_trait]
pub trait Submission: Send + Sync {
    /// 提交 ID。
    fn id(&self) -> SubmissionId;

    /// 对应 `status(context)`。
    async fn status(&self, context: Arc<dyn Context>) -> Result<SubmissionRecord, SessionError>;

    /// 对应 `wait(context)`。
    async fn wait(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<SettledSubmissionRecord, SessionError>;

    /// 对应 `abort(context)`。
    async fn abort(&self, context: Arc<dyn Context>) -> Result<SubmissionAbort, SessionError>;
}

/// 对应 `ConversationAbortOptions`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConversationAbortOptions {
    /// 是否跨越后台边界（对应 `background`）。
    pub background: Option<bool>,
}

/// 对应 `ConversationHandle`：调用期绑定的会话操作。
///
/// 调用结束后一切操作都会拒绝；被动条目改用普通事务写入。
#[async_trait::async_trait]
pub trait ConversationHandle: Send + Sync {
    /// 会话 ID。
    fn id(&self) -> ConversationId;

    /// 对应 `submit(submission, context)`。
    async fn submit(
        &self,
        submission: InputSubmissionDraft,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Submission>, SessionError>;

    /// 对应 `abort(context, options?)`。
    async fn abort(
        &self,
        context: Arc<dyn Context>,
        options: Option<ConversationAbortOptions>,
    ) -> Result<(), SessionError>;

    /// 对应 `waitForIdle(context)`。
    async fn wait_for_idle(&self, context: Arc<dyn Context>) -> Result<(), SessionError>;
}

// ─── 工具 ────────────────────────────────────────────────────────────────────

/// 对应 `ToolControl`：工具结果请求的后续控制。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolControl {
    /// 追加提供的工具名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_tools: Option<Vec<String>>,
    /// 对应 `terminate: true`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    /// 交接到的目标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

/// 对应 `ToolDiagnostic.severity`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolDiagnosticSeverity {
    /// `info`。
    Info,
    /// `warn`。
    Warn,
    /// `error`。
    Error,
}

/// 对应 `ToolDiagnostic`：给模型与 UI 的调用备注，从不进入工具数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDiagnostic {
    /// 严重程度。
    pub severity: ToolDiagnosticSeverity,
    /// 消息。
    pub message: String,
    /// 可选的诊断码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// 对应 `ToolExecutionResult`。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolExecutionResult {
    /// 省略时用 `output()` 的文本作为内容。
    pub content: Option<Vec<pi_ai::TextOrImageContent>>,
    /// 是否为错误结果。
    pub is_error: Option<bool>,
    /// 省略时用最后一次 `details()` 的值。
    pub details: Option<JsonValue>,
    /// 追加到 `api.diagnostic()` 记录之后的诊断。
    pub diagnostics: Vec<ToolDiagnostic>,
    /// 本次执行的耗费。
    pub usage: Option<pi_ai::Usage>,
    /// 后续控制。
    pub control: Option<ToolControl>,
}

/// 对应 `ToolExecutionMode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    /// 一轮内同时执行。
    #[default]
    Parallel,
    /// 按调用顺序逐个执行。
    Sequential,
}

/// 对应 `ToolRegistration.replay`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Replay {
    /// 中断后可以重跑。
    Safe,
    /// 默认：不得重跑。
    #[default]
    Unsafe,
}

/// 对应 `QueueMode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    /// 全部。
    All,
    /// 每次一个。
    OneAtATime,
}

/// 对应 `ToolRegistration`：注册表中可执行的工具。
///
/// 只有 pi-ai `Tool` 的字段进入转录；`args` 在执行前按 `parameters` 校验。
#[async_trait::async_trait]
pub trait ToolRegistration: Send + Sync {
    /// 工具名。
    fn name(&self) -> &str;

    /// 描述。
    fn description(&self) -> &str;

    /// JSON Schema 形式的参数定义（对应 `Tool.parameters`）。
    fn parameters(&self) -> &JsonValue;

    /// 对应 `replay`；默认 `unsafe`。
    fn replay(&self) -> Replay {
        Replay::Unsafe
    }

    /// 对应 `executionMode`；默认用设置里的 `toolExecution`。
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    /// 对应 `prepareArguments`：校验前修正模型常犯的参数错误。
    ///
    /// 必须是纯函数且不得改动入参；结果仍会按 `parameters` 校验。
    fn prepare_arguments(&self, args: JsonValue) -> Result<JsonValue, SessionError> {
        Ok(args)
    }

    /// 对应 `outputLimits`。
    fn output_limits(&self) -> Option<OutputLimits> {
        None
    }

    /// 对应 `execute(args, api, context)`。
    async fn execute(
        &self,
        args: JsonValue,
        api: Arc<dyn ToolExecutionApi>,
        context: Arc<dyn Context>,
    ) -> Result<ToolExecutionResult, SessionError>;
}

/// 对应 `api.commit(change, context)` 的 boxed 闭包参数。
///
/// 上游 `commit<T>` 是泛型方法；Rust 的 dyn trait 无法承载泛型方法，因此结果擦除为 [`JsonValue`]。
pub type CommitOperation<'a> = Box<
    dyn for<'tx> FnOnce(&'tx Transaction) -> BoxFuture<'tx, Result<JsonValue, SessionError>>
        + Send
        + 'a,
>;

/// 对应 `ToolExecutionApi.createTask` 的 `Omit<TaskOptions, "conversationId">`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskCreationOptions {
    /// 任务拥有者。
    pub ownership: crate::types::TaskOwnership,
    /// 是否后台任务（仅会话拥有的任务）。
    pub background: Option<bool>,
}

impl Default for TaskCreationOptions {
    fn default() -> Self {
        Self {
            ownership: crate::types::TaskOwnership::Conversation,
            background: None,
        }
    }
}

impl TaskCreationOptions {
    /// 由完整选项去掉会话字段得到。
    pub fn from_options(options: TaskOptions) -> Self {
        Self {
            ownership: options.ownership,
            background: options.background,
        }
    }

    /// 补会上话字段，得到完整选项。
    pub fn with_conversation(self, conversation_id: ConversationId) -> TaskOptions {
        TaskOptions {
            ownership: self.ownership,
            conversation_id: Some(conversation_id),
            background: self.background,
        }
    }
}

/// 对应 `ToolExecutionApi`：一次工具调用可用的操作。
///
/// 上游是普通对象，因此包装函数可以直接展开它；Rust 用 trait（调用结束后一切操作拒绝）。
#[async_trait::async_trait]
pub trait ToolExecutionApi: DocumentObserver + DocumentReader + Send + Sync {
    /// 本次工具调用的任务。
    fn task_id(&self) -> TaskId;

    /// 调用发生的会话。
    fn conversation_id(&self) -> ConversationId;

    /// 本次调用 ID。
    fn call_id(&self) -> &str;

    /// 工具任务所在阶段的注册表快照。
    fn registry(&self) -> &RegistrySnapshot;

    /// 对应 `agent(context)`：调用会话的 agent（按工具任务阶段的解析结果）。
    async fn agent(&self, context: Arc<dyn Context>) -> Result<Agent, SessionError>;

    /// 对应 `models`：pi-ai 目录、凭据与请求变换。
    fn models(&self) -> Arc<pi_ai::Models>;

    /// 对应 `env`：本次调用的执行环境；无环境时为 `None`。
    fn env(&self) -> Option<Arc<dyn ExecutionEnv>>;

    /// 对应 `output(chunk, skipped?)`：追加运行输出。
    ///
    /// 结果省略 `content` 时它成为内容。
    fn output(&self, chunk: OutputChunk<'_>, skipped: Option<ShellOutputSkip>);

    /// 对应 `outputWindow`：本调用保留的输出尾部与进度提交节奏。
    fn output_window(&self) -> Option<ShellOutputWindow>;

    /// 对应 `diagnostic(diagnostic)`。
    fn diagnostic(&self, diagnostic: ToolDiagnostic);

    /// 对应 `details(value, context)`：替换运行中的细节。
    async fn details(
        &self,
        value: JsonValue,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError>;

    /// 对应 `commit(change, context)`。
    async fn commit(
        &self,
        change: CommitOperation<'static>,
        context: Arc<dyn Context>,
    ) -> Result<JsonValue, SessionError>;

    /// 对应 `memo(name, context)`：读取一个记忆值。
    async fn memo(
        &self,
        name: &str,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonValue>, SessionError>;

    /// 对应 `memo(name, candidate, context)`：不存在时写入候选值。
    async fn memo_or(
        &self,
        name: &str,
        candidate: JsonValue,
        context: Arc<dyn Context>,
    ) -> Result<JsonValue, SessionError>;

    /// 对应 `createTask(task, input, options, context)`。
    async fn create_task(
        &self,
        task: &Task,
        input: JsonValue,
        options: TaskCreationOptions,
        context: Arc<dyn Context>,
    ) -> Result<TaskId, SessionError>;

    /// 对应 `getTask(id, context)`。
    async fn get_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<Option<AnyTaskRecord>, SessionError>;

    /// 对应 `waitForTask(id, context)`。
    async fn wait_for_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<SettledTask, SessionError>;

    /// 对应 `conversation(id, context)`。
    async fn conversation(
        &self,
        id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn ConversationHandle>>, SessionError>;
}

// ─── 提示章节 ────────────────────────────────────────────────────────────────

/// 对应 `PromptInput`：一次请求准备时系统提示章节渲染的输入。
pub struct PromptInput {
    /// 会话 ID。
    pub conversation_id: ConversationId,
    /// 本次请求的解析结果；`agent.tools` 是本次提供的工具。
    pub agent: Agent,
    /// 本次准备构建的执行环境；无环境时为 `None`。
    pub env: Option<Arc<dyn ExecutionEnv>>,
    /// 重放活动转录后已经生效的章节。
    pub shown: BTreeMap<String, String>,
    /// 已提交文档读取。
    pub read: Arc<dyn DocumentReader>,
}

/// 对应 `PromptSection`：一个系统提示章节；agent 的章节在每次请求前按序渲染。
#[async_trait::async_trait]
pub trait PromptSection: Send + Sync {
    /// 章节键。
    fn key(&self) -> &str;

    /// 对应 `tag`；默认 `true`（把文本包成 `<key>\n...\n</key>`）。
    fn is_tagged(&self) -> bool {
        true
    }

    /// 对应 `render(input, context)`。
    async fn render(
        &self,
        input: &PromptInput,
        context: Arc<dyn Context>,
    ) -> Result<Option<String>, SessionError>;
}

// ─── 扩展与注册表 ────────────────────────────────────────────────────────────

/// 对应 `HookRegistration`：按任务名匹配的 hook 处理器。
pub struct HookRegistration {
    /// 目标任务名。
    pub task: String,
    /// 处理器集合。
    pub handlers: HookHandlers,
}

/// 对应 `HooksOf<K>` 的处理器集合。
///
/// 上游按任务定义的泛型推导处理器类型；Rust 用枚举列出内置任务的 hook 契约。
#[derive(Clone)]
pub enum HookHandlers {
    /// 内置 generation 任务的 hook。
    Generation(Arc<dyn GenerationHooks>),
    /// 内置 tool 任务的 hook。
    Tool(Arc<dyn ToolHooks>),
    /// 内置 compaction 任务的 hook。
    Compaction(Arc<dyn CompactionHooks>),
}

/// 对应 `wrapTool()` / `wrapSection()` 的包装器函数。
///
/// 包装必须是纯函数。
pub type ToolWrapper =
    Arc<dyn Fn(Arc<dyn ToolRegistration>) -> Arc<dyn ToolRegistration> + Send + Sync + 'static>;

/// 对应 `wrapSection()` 的章节包装器函数。
pub type SectionWrapper =
    Arc<dyn Fn(Arc<dyn PromptSection>) -> Arc<dyn PromptSection> + Send + Sync + 'static>;

/// 对应 `wrapTool()` / `wrapSection()` 产生的包装器。
///
/// 包装必须是纯函数。
pub enum Wrap {
    /// 按工具名包装。
    Tool {
        /// 目标工具名。
        tool: String,
        /// 包装函数。
        wrap: ToolWrapper,
    },
    /// 按章节键包装。
    Section {
        /// 目标章节键。
        section: String,
        /// 包装函数。
        wrap: SectionWrapper,
    },
}

impl Clone for Wrap {
    fn clone(&self) -> Self {
        match self {
            Wrap::Tool { tool, wrap } => Wrap::Tool {
                tool: tool.clone(),
                wrap: Arc::clone(wrap),
            },
            Wrap::Section { section, wrap } => Wrap::Section {
                section: section.clone(),
                wrap: Arc::clone(wrap),
            },
        }
    }
}

/// 对应 `Extension`：一段具名代码；装入注册表后由会话按名选择。
#[derive(Clone)]
pub struct Extension {
    /// 扩展名。
    pub name: String,
    /// 提供的工具。
    pub tools: Vec<Arc<dyn ToolRegistration>>,
    /// 提供的章节。
    pub sections: Vec<Arc<dyn PromptSection>>,
    /// 提供的 hook。
    pub hooks: Vec<Arc<HookRegistration>>,
    /// 在该扩展被选中处应用的包装（按序）。
    pub wraps: Vec<Wrap>,
    /// 为每个任务按名解析的任务定义（无论会话是否选择该扩展）。
    pub tasks: Vec<Arc<Task>>,
}

impl std::fmt::Debug for Extension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extension")
            .field("name", &self.name)
            .field("tools", &self.tools.len())
            .field("sections", &self.sections.len())
            .field("hooks", &self.hooks.len())
            .field("wraps", &self.wraps.len())
            .field("tasks", &self.tasks.len())
            .finish()
    }
}

/// 对应 `RegistrySnapshot<Tool>`：某个已发布注册表状态的不可变视图。
#[derive(Clone, Default)]
pub struct RegistrySnapshot {
    installed: Arc<Vec<Arc<Extension>>>,
    tasks: Arc<Vec<Arc<Task>>>,
}

impl std::fmt::Debug for RegistrySnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrySnapshot")
            .field(
                "installed",
                &self
                    .installed
                    .iter()
                    .map(|extension| extension.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field(
                "tasks",
                &self
                    .tasks
                    .iter()
                    .map(|task| task.definition().name())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl RegistrySnapshot {
    /// 由已安装的扩展与全部任务定义构造。
    pub fn new(installed: Vec<Arc<Extension>>, tasks: Vec<Arc<Task>>) -> Self {
        Self {
            installed: Arc::new(installed),
            tasks: Arc::new(tasks),
        }
    }

    /// 对应 `installed()`。
    pub fn installed(&self) -> &[Arc<Extension>] {
        &self.installed
    }

    /// 对应 `extension(name)`。
    pub fn extension(&self, name: &str) -> Option<&Arc<Extension>> {
        self.installed
            .iter()
            .find(|extension| extension.name == name)
    }

    /// 对应 `tools()`：按安装顺序的「扩展 + 工具」；同名工具可跨扩展重复。
    pub fn tools(&self) -> Vec<(&Arc<Extension>, &Arc<dyn ToolRegistration>)> {
        self.installed
            .iter()
            .flat_map(|extension| extension.tools.iter().map(move |tool| (extension, tool)))
            .collect()
    }

    /// 对应 `sections()`。
    pub fn sections(&self) -> Vec<(&Arc<Extension>, &Arc<dyn PromptSection>)> {
        self.installed
            .iter()
            .flat_map(|extension| {
                extension
                    .sections
                    .iter()
                    .map(move |section| (extension, section))
            })
            .collect()
    }

    /// 对应 `tasks()`：内置与已安装的任务定义。
    pub fn tasks(&self) -> &[Arc<Task>] {
        &self.tasks
    }

    /// 对应 `task(name)`。
    pub fn task(&self, name: &str) -> Option<&Arc<Task>> {
        self.tasks
            .iter()
            .find(|task| task.definition().name() == name)
    }
}

/// 对应 `RegistryReader<Tool>`：注册表的只读视图与变更订阅。
///
/// 上游把 `Registry` / `RegistryReader` 定义在 `harness/types.ts`；Rust 侧先落地调度器依赖的
/// 只读面（`snapshot` / `subscribe`），`Registry` 的写面（`install` / `uninstall`）留待 registry 落地。
pub trait RegistryReader: Send + Sync {
    /// 对应 `snapshot()`：当前已发布的不可变状态。
    fn snapshot(&self) -> RegistrySnapshot;

    /// 对应 `subscribe(listener)`：注册一个在状态发布时调用的监听器，返回取消订阅函数。
    fn subscribe(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Box<dyn Fn() + Send + Sync>;
}

// ─── Agent ───────────────────────────────────────────────────────────────────

/// 对应 `AgentState`：一个会话存储的选择（存名字而非对象）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentState {
    /// 模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// 思考等级。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<pi_ai::ModelThinkingLevel>,
    /// 扩展选择。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<ExtensionStateSelection>,
    /// 工具过滤。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolStateSelection>,
    /// 每个扩展章节之后渲染的 `instructions` 章节。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// 环境文件系统内的工作目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// 对应 `AgentState.extensions`：数组＝精确选择；对象＝编辑宿主默认选择。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtensionStateSelection {
    /// 恰好按序选择这些扩展名。
    Names(Vec<String>),
    /// 在宿主默认选择上增删。
    Edit {
        /// 追加。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        add: Option<Vec<String>>,
        /// 移除。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remove: Option<Vec<String>>,
    },
}

/// 对应 `AgentState.tools`：数组＝恰好这些；对象＝移除这些。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolStateSelection {
    /// 恰好按序提供这些工具名。
    Names(Vec<String>),
    /// 从选中扩展里移除这些工具名。
    Remove(Vec<String>),
}

/// 对应 `AgentChange`：对 `pi.agent` 的一次改动；给定字段替换、`null` 清除、缺席不变。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentChange {
    /// 模型。
    pub model: FieldChange<ModelRef>,
    /// 思考等级。
    pub thinking_level: FieldChange<pi_ai::ModelThinkingLevel>,
    /// 扩展选择。
    pub extensions: FieldChange<ExtensionChangeSelection>,
    /// 工具过滤。
    pub tools: FieldChange<ToolChangeSelection>,
    /// 指令。
    pub instructions: FieldChange<String>,
    /// 工作目录。
    pub cwd: FieldChange<String>,
}

/// 对应 `AgentChange.extensions` 的值形态。
#[derive(Clone)]
pub enum ExtensionChangeSelection {
    /// 恰好这些扩展（按序）。
    Exactly(Vec<Arc<Extension>>),
    /// 在宿主默认选择上增删。
    Edit {
        /// 追加；`None` 表示未提供（对应上游 `add?: Extension[]`）。
        add: Option<Vec<Arc<Extension>>>,
        /// 移除；`None` 表示未提供。
        remove: Option<Vec<Arc<Extension>>>,
    },
}

impl std::fmt::Debug for ExtensionChangeSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtensionChangeSelection::Exactly(items) => {
                f.debug_tuple("Exactly").field(&items.len()).finish()
            }
            ExtensionChangeSelection::Edit { add, remove } => f
                .debug_struct("Edit")
                .field("add", &add.as_ref().map(Vec::len))
                .field("remove", &remove.as_ref().map(Vec::len))
                .finish(),
        }
    }
}

impl PartialEq for ExtensionChangeSelection {
    fn eq(&self, other: &Self) -> bool {
        fn same(left: &Option<Vec<Arc<Extension>>>, right: &Option<Vec<Arc<Extension>>>) -> bool {
            match (left, right) {
                (None, None) => true,
                (Some(left), Some(right)) => {
                    left.len() == right.len()
                        && left
                            .iter()
                            .zip(right)
                            .all(|(left, right)| left.name == right.name)
                }
                _ => false,
            }
        }
        match (self, other) {
            (ExtensionChangeSelection::Exactly(left), ExtensionChangeSelection::Exactly(right)) => {
                let left = Some(left.clone());
                let right = Some(right.clone());
                same(&left, &right)
            }
            (
                ExtensionChangeSelection::Edit {
                    add: left_add,
                    remove: left_remove,
                },
                ExtensionChangeSelection::Edit {
                    add: right_add,
                    remove: right_remove,
                },
            ) => same(left_add, right_add) && same(left_remove, right_remove),
            _ => false,
        }
    }
}

/// 对应 `AgentChange.tools` 的值形态。
pub enum ToolChangeSelection {
    /// 恰好这些工具（按序）。
    Exactly(Vec<Arc<dyn ToolRegistration>>),
    /// 从选中扩展里移除这些工具。
    Remove(Vec<Arc<dyn ToolRegistration>>),
}

impl Clone for ToolChangeSelection {
    fn clone(&self) -> Self {
        match self {
            ToolChangeSelection::Exactly(items) => {
                ToolChangeSelection::Exactly(items.iter().map(Arc::clone).collect())
            }
            ToolChangeSelection::Remove(items) => {
                ToolChangeSelection::Remove(items.iter().map(Arc::clone).collect())
            }
        }
    }
}

impl std::fmt::Debug for ToolChangeSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolChangeSelection::Exactly(items) => {
                f.debug_tuple("Exactly").field(&items.len()).finish()
            }
            ToolChangeSelection::Remove(items) => {
                f.debug_tuple("Remove").field(&items.len()).finish()
            }
        }
    }
}

impl PartialEq for ToolChangeSelection {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ToolChangeSelection::Exactly(left), ToolChangeSelection::Exactly(right))
            | (ToolChangeSelection::Remove(left), ToolChangeSelection::Remove(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| left.name() == right.name())
            }
            _ => false,
        }
    }
}

/// 对应 `Agent<Tool>`：一个会话的 agent 经注册表快照与设置解析后的结果。
#[derive(Clone)]
pub struct Agent {
    /// 模型；未设置时为 `None`。
    pub model: Option<ModelRef>,
    /// 思考等级。
    pub thinking_level: pi_ai::ModelThinkingLevel,
    /// 选中的扩展。
    pub extensions: Vec<Arc<Extension>>,
    /// 本次请求提供的工具（按序）。
    pub tools: Vec<Arc<dyn ToolRegistration>>,
    /// 扩展章节，其后是设置了的 `instructions`。
    pub sections: Vec<Arc<dyn PromptSection>>,
    /// 指令。
    pub instructions: Option<String>,
    /// 工作目录。
    pub cwd: Option<String>,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            model: None,
            thinking_level: pi_ai::ModelThinkingLevel::Off,
            extensions: Vec::new(),
            tools: Vec::new(),
            sections: Vec::new(),
            instructions: None,
            cwd: None,
        }
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field(
                "extensions",
                &self
                    .extensions
                    .iter()
                    .map(|extension| extension.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field(
                "tools",
                &self
                    .tools
                    .iter()
                    .map(|tool| tool.name())
                    .collect::<Vec<_>>(),
            )
            .field("sections", &self.sections.len())
            .field("instructions", &self.instructions)
            .field("cwd", &self.cwd)
            .finish()
    }
}

// ─── 策略与设置 ──────────────────────────────────────────────────────────────

/// 对应 `ConversationStreamOptions`：精选的 pi-ai 请求选项。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationStreamOptions {
    /// 传输方式。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<pi_ai::Transport>,
    /// 超时（毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// 单次请求内的 provider/SDK 重试次数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    /// 最大重试间隔（毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
    /// 额外请求头。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// 元数据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
    /// 缓存保留策略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<pi_ai::CacheRetention>,
    /// 延迟执行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredOptions>,
}

/// 对应 `deferred?: boolean | { window?: "15m" | "1h" | "24h" }`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeferredOptions {
    /// 布尔开关。
    Enabled(bool),
    /// 窗口设置。
    Window {
        /// 延迟窗口。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<DeferredWindow>,
    },
}

/// 对应 `window` 的字面量联合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferredWindow {
    /// `15m`。
    #[serde(rename = "15m")]
    QuarterHour,
    /// `1h`。
    #[serde(rename = "1h")]
    Hour,
    /// `24h`。
    #[serde(rename = "24h")]
    Day,
}

/// 对应 `ConversationRetryPolicy`：durable 的生成尝试重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRetryPolicy {
    /// 是否启用。
    pub enabled: bool,
    /// 最大重试次数。
    pub max_retries: u32,
    /// 基础延迟（毫秒）。
    pub base_delay_ms: u64,
    /// 最大 agent 延迟（毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<u64>,
}

/// 对应 `DEFAULT_RETRY_POLICY`（上游定义在 `agent.ts`）。
pub const DEFAULT_RETRY_POLICY: ConversationRetryPolicy = ConversationRetryPolicy {
    enabled: true,
    max_retries: 3,
    base_delay_ms: 2_000,
    max_agent_delay_ms: Some(60_000),
};

impl Default for ConversationRetryPolicy {
    fn default() -> Self {
        DEFAULT_RETRY_POLICY
    }
}

/// 对应 `CompactionPolicy`：自动压缩阈值；手动压缩忽略 `enabled`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPolicy {
    /// 阈值与溢出压缩。
    pub enabled: bool,
    /// 为回答留出的空间：超过 `contextWindow - reserveTokens` 时阻塞压缩。
    pub reserve_tokens: u64,
    /// 摘要原样保留的近期上下文规模。
    pub keep_recent_tokens: u64,
    /// 后台压缩比阻塞阈值低多少时启动；`0` 表示禁用。
    pub background_tokens: u64,
}

/// 对应 `DEFAULT_COMPACTION_POLICY`（上游定义在 `agent.ts`）。
pub const DEFAULT_COMPACTION_POLICY: CompactionPolicy = CompactionPolicy {
    enabled: true,
    reserve_tokens: 16_384,
    keep_recent_tokens: 20_000,
    background_tokens: 32_768,
};

impl Default for CompactionPolicy {
    fn default() -> Self {
        DEFAULT_COMPACTION_POLICY
    }
}

/// 对应 `ProgressPolicy`：运行中进度提交的频率。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPolicy {
    /// 正在生成的答案两次提交之间的最小间隔（毫秒）。
    pub partial_interval_ms: u64,
    /// 运行中工具输出两次提交之间的最小间隔（毫秒）。
    pub output_interval_ms: u64,
}

/// 对应 `DEFAULT_PROGRESS_POLICY`（上游定义在 `agent.ts`）。
pub const DEFAULT_PROGRESS_POLICY: ProgressPolicy = ProgressPolicy {
    partial_interval_ms: 100,
    output_interval_ms: 100,
};

impl Default for ProgressPolicy {
    fn default() -> Self {
        DEFAULT_PROGRESS_POLICY
    }
}

/// 对应 `CompactionReason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    /// `manual`。
    Manual,
    /// `threshold`。
    Threshold,
    /// `overflow`。
    Overflow,
}

/// 对应 `CompactionResult`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    /// 阻塞式压缩摘要的条目。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<EntryId>,
    /// 会话自有压缩摘要写入的提交。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_id: Option<SubmissionId>,
}

/// 对应 `HarnessSettings`：harness 级运行策略；缺省字段取内置默认。
#[derive(Clone, Default)]
pub struct HarnessSettings {
    /// 默认扩展选择；缺席表示全部已安装扩展（按安装顺序）。
    pub extensions: Option<Vec<Arc<Extension>>>,
    /// 流选项。
    pub stream: Option<ConversationStreamOptions>,
    /// 重试策略（部分字段）。
    pub retry: Option<PartialRetryPolicy>,
    /// 压缩策略（部分字段）。
    pub compaction: Option<PartialCompactionPolicy>,
    /// 进度策略（部分字段）。
    pub progress: Option<PartialProgressPolicy>,
    /// 默认工具执行模式。
    pub tool_execution: Option<ToolExecutionMode>,
    /// steering 队列模式。
    pub steering_mode: Option<QueueMode>,
    /// follow-up 队列模式。
    pub follow_up_mode: Option<QueueMode>,
    /// 空闲会话保留上次上下文读取的时长（毫秒）；`0` 表示一旦空闲即丢弃。
    pub context_retention_ms: Option<u64>,
}

/// 对应 `HarnessSettings.retry`（`Partial<ConversationRetryPolicy>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartialRetryPolicy {
    /// 是否启用。
    pub enabled: Option<bool>,
    /// 最大重试次数。
    pub max_retries: Option<u32>,
    /// 基础延迟（毫秒）。
    pub base_delay_ms: Option<u64>,
    /// 最大 agent 延迟（毫秒）。
    pub max_agent_delay_ms: Option<u64>,
}

/// 对应 `HarnessSettings.compaction`（`Partial<CompactionPolicy>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartialCompactionPolicy {
    /// 是否启用。
    pub enabled: Option<bool>,
    /// 保留 token。
    pub reserve_tokens: Option<u64>,
    /// 保留的近期 token。
    pub keep_recent_tokens: Option<u64>,
    /// 后台压缩阈值差。
    pub background_tokens: Option<u64>,
}

/// 对应 `HarnessSettings.progress`（`Partial<ProgressPolicy>`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartialProgressPolicy {
    /// 部分提交间隔。
    pub partial_interval_ms: Option<u64>,
    /// 输出提交间隔。
    pub output_interval_ms: Option<u64>,
}

/// 对应 `DEFAULT_CONTEXT_RETENTION_MS`（上游定义在 `agent.ts`）：十分钟。
pub const DEFAULT_CONTEXT_RETENTION_MS: u64 = 600_000;

/// 对应 `Settings`：解析后的设置，每个字段都覆盖了内置默认。
#[derive(Clone)]
pub struct Settings {
    /// 缺席表示全部已安装扩展（按安装顺序）。
    pub extensions: Option<Vec<Arc<Extension>>>,
    /// 流选项。
    pub stream: ConversationStreamOptions,
    /// 重试策略。
    pub retry: ConversationRetryPolicy,
    /// 压缩策略。
    pub compaction: CompactionPolicy,
    /// 进度策略。
    pub progress: ProgressPolicy,
    /// 工具执行模式。
    pub tool_execution: ToolExecutionMode,
    /// steering 队列模式。
    pub steering_mode: QueueMode,
    /// follow-up 队列模式。
    pub follow_up_mode: QueueMode,
    /// 上下文保留时长（毫秒）。
    pub context_retention_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            extensions: None,
            stream: ConversationStreamOptions::default(),
            retry: DEFAULT_RETRY_POLICY,
            compaction: DEFAULT_COMPACTION_POLICY,
            progress: DEFAULT_PROGRESS_POLICY,
            tool_execution: ToolExecutionMode::Parallel,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            context_retention_ms: DEFAULT_CONTEXT_RETENTION_MS,
        }
    }
}

// ─── 环境、视图与探查 ────────────────────────────────────────────────────────

/// 对应 `EnvTarget`：`HarnessOptions.env` 为一个会话构建环境时的输入。
#[derive(Clone)]
pub struct EnvTarget {
    /// 会话 ID。
    pub conversation_id: ConversationId,
    /// 会话 agent 的 `cwd`。
    pub cwd: Option<String>,
    /// 已提交文档读取。
    pub read: Arc<dyn DocumentReader>,
}

/// 对应 `ContextView`：原始活动转录与派生的模型上下文。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextView {
    /// 最新的适用 head 标记。
    pub head: Option<EntryRecord>,
    /// 原始活动条目：head 标记，其后是自其 head 到末尾的非 head 条目。
    pub entries: Vec<EntryRecord>,
    /// 每个条目在编辑与排除停止原因之后、工具结果排序之前的模型消息。
    pub contributions: Vec<Vec<pi_ai::Message>>,
    /// 下一次 provider 请求的模型上下文。
    pub messages: Vec<pi_ai::Message>,
}

/// 对应 `TaskInspection`：一个活动任务与当前注册表下调度器将采取的行动。
#[derive(Debug, Clone)]
pub struct TaskInspection {
    /// 任务记录。
    pub record: AnyTaskRecord,
    /// 调度视角的状态。
    pub state: TaskInspectionState,
}

/// 对应 `TaskInspection.state`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskInspectionState {
    /// 有一次调用正在进行。
    Running,
    /// 下一轮调度会保留它；定义更新且有 `migrate` 时 `migrates`。
    Ready {
        /// 是否会发生迁移。
        migrates: bool,
    },
    /// 等待这些活动任务。
    Waiting {
        /// 依赖的任务。
        on: Vec<TaskId>,
    },
    /// 结果保留到其自有普通工作排空。
    Completing,
    /// 没有注册定义能接手；中止它会把任务结算为 `orphaned`。
    Blocked {
        /// 阻塞原因。
        reason: TaskBlockedReason,
    },
}

/// 对应 `TaskInspectionState::Blocked.reason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskBlockedReason {
    /// `missing_task`。
    MissingTask,
    /// `task_too_old`。
    TaskTooOld,
    /// `migration_failed`。
    MigrationFailed,
}

/// 对应 `HarnessInspection`：活动工作的时间点视图。
#[derive(Debug, Clone)]
pub struct HarnessInspection {
    /// 调度状态。
    pub scheduling: SchedulingState,
    /// 未完成任务。
    pub tasks: Vec<TaskInspection>,
    /// queued 与 placed 的提交（按 ID 顺序）。
    pub submissions: Vec<SubmissionRecord>,
}

/// 对应 `HarnessInspection.scheduling`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulingState {
    /// 已暂停。
    Paused,
    /// 运行中。
    Running,
    /// 关闭中。
    Closing,
}

// ─── Hook 契约 ───────────────────────────────────────────────────────────────

/// 对应 `HookResult<T>`：`T | undefined | Promise<T | undefined>`。
pub type HookResult<T> = Result<Option<T>, SessionError>;

/// 对应 `HookApi`：hook 可用的能力——已提交读取与所属任务的记忆（hook 与任务共享）。
#[async_trait::async_trait]
pub trait HookApi: DocumentReader + Send + Sync {
    /// 发起 hook 的任务。
    fn task_id(&self) -> TaskId;

    /// 调用发生的会话。
    fn conversation_id(&self) -> ConversationId;

    /// 对应 `models`。
    fn models(&self) -> Arc<pi_ai::Models>;

    /// 对应 `memo(name, context)`。
    async fn memo(
        &self,
        name: &str,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonValue>, SessionError>;

    /// 对应 `memo(name, candidate, context)`。
    async fn memo_or(
        &self,
        name: &str,
        candidate: JsonValue,
        context: Arc<dyn Context>,
    ) -> Result<JsonValue, SessionError>;
}

/// 对应 `GenerationHooks.beforeRequest` 的请求形状。
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationRequest {
    /// 本次请求的消息。
    pub messages: Vec<pi_ai::Message>,
}

/// 对应 `GenerationHooks.onYield` 的继续结果。
#[derive(Debug, Clone, PartialEq)]
pub struct GenerationContinuation {
    /// 追加的用户消息。
    pub user_input: UserInput,
}

/// 对应 `GenerationHooks`：内置 generation 任务的 hook。
///
/// 上游用 `Partial<>` 允许只实现部分；Rust 的每个方法都有默认实现（返回「不干预」）。
#[async_trait::async_trait]
pub trait GenerationHooks: Send + Sync {
    /// 每次请求尝试之前（含恢复）运行；结果只用于该次请求。
    async fn before_request(
        &self,
        _request: &GenerationRequest,
        _api: Arc<dyn HookApi>,
        _context: Arc<dyn Context>,
    ) -> HookResult<GenerationRequest> {
        Ok(None)
    }

    /// 每个终止的 provider 消息，在分类之前。
    async fn after_response(
        &self,
        _message: pi_ai::AssistantMessage,
        _api: Arc<dyn HookApi>,
        _context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        Ok(())
    }

    /// 一次最终回答；第一个返回 `continue` 的 hook 追加用户消息并继续该轮。
    async fn on_yield(
        &self,
        _answer: pi_ai::AssistantMessage,
        _api: Arc<dyn HookApi>,
        _context: Arc<dyn Context>,
    ) -> HookResult<GenerationContinuation> {
        Ok(None)
    }

    /// 一轮的每个工具都已终态之后；`results` 是该轮按调用顺序的结果条目。
    async fn after_tools(
        &self,
        _assistant: EntryId,
        _results: &[EntryId],
        _api: Arc<dyn HookApi>,
        _context: Arc<dyn Context>,
    ) -> Result<(), SessionError> {
        Ok(())
    }
}

/// 对应 `ToolHooks.beforeTool` 的结果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BeforeToolResult {
    /// 替换本次调用的参数。
    pub arguments: Option<JsonObject>,
    /// 第一个 `block` 胜出。
    pub block: Option<String>,
}

/// 对应 `ToolHooks`：内置 tool 任务的 hook。
#[async_trait::async_trait]
pub trait ToolHooks: Send + Sync {
    /// 记录意图之前；第一个 `block` 胜出，否则 `arguments` 替换调用参数。抛错即阻塞。
    async fn before_tool(
        &self,
        _call: &pi_ai::ToolCall,
        _api: Arc<dyn HookApi>,
        _context: Arc<dyn Context>,
    ) -> HookResult<BeforeToolResult> {
        Ok(None)
    }

    /// 执行之后、结果条目之前；替换结果。
    async fn after_tool(
        &self,
        _call: &pi_ai::ToolCall,
        _result: ToolExecutionResult,
        _api: Arc<dyn HookApi>,
        _context: Arc<dyn Context>,
    ) -> HookResult<ToolExecutionResult> {
        Ok(None)
    }
}

/// 对应 `CompactionHooks.beforeCompact` 的输入。
#[derive(Debug, Clone)]
pub struct BeforeCompact {
    /// 压缩原因。
    pub reason: CompactionReason,
    /// 摘要将替换的活动条目（head 标记在前）。
    pub entries: Vec<EntryRecord>,
    /// 它们的模型上下文（摘要器的输入）。
    pub messages: Vec<pi_ai::Message>,
    /// 原样保留的第一个条目。
    pub first_kept: EntryId,
    /// 附加指令。
    pub instructions: Option<String>,
}

/// 对应 `CompactionHooks.beforeCompact` 的结果：拒绝或给出摘要。
#[derive(Debug, Clone, PartialEq)]
pub enum CompactDecision {
    /// 放弃本次压缩。
    Decline,
    /// 使用给定摘要。
    Summary(String),
}

/// 对应 `CompactionHooks`：内置 compaction 任务的 hook。
#[async_trait::async_trait]
pub trait CompactionHooks: Send + Sync {
    /// 范围选定之后、摘要之前；第一个决定胜出。
    async fn before_compact(
        &self,
        _compaction: &BeforeCompact,
        _api: Arc<dyn HookApi>,
        _context: Arc<dyn Context>,
    ) -> HookResult<CompactDecision> {
        Ok(None)
    }
}

/// 便于阅读：`ToolExecutionApi.registry` 的原始泛型（上游 `RegistrySnapshot<Tool>`）。
#[allow(dead_code)]
type ToolRegistrySnapshot = RegistrySnapshot;

// ─── 任务运行时契约 ──────────────────────────────────────────────────────────
//
// 上游把这些定义在 durable `types.ts`；Rust 侧放在这里，以免底层 `types.rs` 反向依赖 harness 的
// `Agent` / `Settings` / `RegistrySnapshot` 等。

/// 对应 `RunningTask<I, S, R>`：一个调用保留的活动任务记录（`state` 必为 `running`）。
#[derive(Debug, Clone, PartialEq)]
pub struct RunningTask(pub AnyTaskRecord);

impl RunningTask {
    /// 由已确认处于 `running` 的记录构造；形状不符时 panic（调用方保证）。
    pub fn new(record: AnyTaskRecord) -> Self {
        assert_eq!(
            record.state.status(),
            crate::types::TaskStatus::Running,
            "RunningTask 要求 running 记录"
        );
        Self(record)
    }

    /// 记录本身。
    pub fn record(&self) -> &AnyTaskRecord {
        &self.0
    }
}

/// 对应 `NextTaskState<S, R>`：任务为自己提交的下一状态（替换 checkpoint、等待，或结果）。
///
/// 返回的 `terminal` 会在任务下属的普通工作仍然活动时被存成 `completing`（spec §5.5）。
#[derive(Debug, Clone, PartialEq)]
pub enum NextTaskState {
    /// 继续运行。
    Running {
        /// 替换后的 checkpoint。
        checkpoint: JsonValue,
    },
    /// 挂起等待。
    Waiting {
        /// 替换后的 checkpoint。
        checkpoint: JsonValue,
        /// 所等待的任务。
        on: Vec<TaskId>,
        /// 汇合策略。
        policy: crate::types::JoinPolicy,
    },
    /// 结束。
    Terminal {
        /// 结果。
        outcome: crate::types::TaskOutcome<JsonValue>,
    },
}

/// 对应 `runtime.commit(change, context)` 的 boxed 闭包参数。
///
/// `RunningTask` 按值传递（上游 `change(tx, current)` 里的 `current` 是引用；Rust 侧把它 clone 后
/// move 进闭包，future 只借用 `tx`，从而满足 [`SessionImpl::commit_with`] 的 HRTB）。
pub type TaskCommit<'a> = Box<
    dyn for<'tx> FnOnce(
            &'tx crate::session::transaction::Transaction,
            RunningTask,
        ) -> BoxFuture<'tx, Result<Option<NextTaskState>, SessionError>>
        + Send
        + 'a,
>;

/// 对应 `HookRunner<H>`：把一个具名 hook 分发给每个匹配的注册处理器。
///
/// 上游是泛型 `each<K extends keyof H>(name, invoke)`；Rust 无法在 dyn trait 上表达「按 name 变化的
/// 处理器类型」，因此改为交出处理器集合，由各内置任务自己分发（见 P5g 的 generation / tool / compaction）。
pub trait HookRunner: Send + Sync {
    /// 本运行期的 hook 处理器（按扩展顺序）。
    fn handlers(&self) -> Vec<Arc<HookHandlers>>;
}

/// 对应 `TaskRuntime<I, S, R, H>`：一次任务调用的操作。
///
/// 每个操作在调用结束后都会被拒绝；通过它获取的观察在调用结束时停止。
#[async_trait::async_trait]
pub trait TaskRuntime: DocumentObserver + DocumentReader + Send + Sync {
    /// 任务 ID。
    fn task_id(&self) -> TaskId;

    /// 任务的会话。
    fn conversation_id(&self) -> ConversationId;

    /// 被 `abortTask()`、Harness 关闭或调用结束时中止的信号。
    fn signal(&self) -> pi_ai::AbortSignal;

    /// 当前阶段的注册表快照；在每个阶段边界刷新。
    fn registry(&self) -> RegistrySnapshot;

    /// 任务会话的 agent，每个阶段至多解析一次、首次使用时解析，并在该阶段固定。
    async fn agent(&self, context: Arc<dyn Context>) -> Result<Agent, SessionError>;

    /// 对应 `settings`：每次访问重新解析。
    fn settings(&self) -> Settings;

    /// 对应 `models`。
    fn models(&self) -> Arc<pi_ai::Models>;

    /// 为任务会话调用 `HarnessOptions.env`；以它的错误拒绝。
    async fn env(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn ExecutionEnv>>, SessionError>;

    /// 本任务名的 hook 运行器。
    fn hooks(&self) -> Arc<dyn HookRunner>;

    /// 对应 `commit(change, context)`：在重读任务后在 Session 线上提交。
    ///
    /// 任务终态、调用已结束、Harness 正在关闭，或（运行调用中）任务带有 abort 标记时拒绝。
    /// 返回的状态在同一提交里替换任务状态；什么都不返回则保持不变。
    async fn commit(
        &self,
        change: TaskCommit<'static>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError>;

    /// 对应 `memo(name, context)`：读一个持久记忆。
    async fn memo(
        &self,
        name: &str,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonValue>, SessionError>;

    /// 对应 `memo(name, candidate, context)`：不存在时存入候选值，返回持久的胜者。
    async fn memo_or(
        &self,
        name: &str,
        candidate: JsonValue,
        context: Arc<dyn Context>,
    ) -> Result<JsonValue, SessionError>;

    /// 对应 `getTask(id, context)`：已提交的任务记录。
    async fn get_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<Option<AnyTaskRecord>, SessionError>;

    /// 对应 `waitForTask(id, context)`：等待终态回执；调用结束时拒绝。
    async fn wait_for_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<SettledTask, SessionError>;

    /// 对应 `outcomes(ids, context)`：按序返回终态任务的结果。
    async fn outcomes(
        &self,
        ids: &[TaskId],
        context: Arc<dyn Context>,
    ) -> Result<Vec<crate::types::TaskOutcome<JsonValue>>, SessionError>;

    /// 对应 `conversation(id, context)`：调用期绑定的会话句柄。
    async fn conversation(
        &self,
        id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn ConversationHandle>>, SessionError>;

    /// 对应 `entry(id, context)`：任务会话可见的已提交条目。
    async fn entry(
        &self,
        id: EntryId,
        context: Arc<dyn Context>,
    ) -> Result<Option<EntryRecord>, SessionError>;

    /// 对应 `context(conversationId, context, options?)`：已提交的原始活动转录与模型上下文。
    async fn context_view(
        &self,
        conversation_id: ConversationId,
        context: Arc<dyn Context>,
        at: Option<EntryId>,
    ) -> Result<ContextView, SessionError>;

    /// 对应 `now()`：Harness 时钟。
    fn now(&self) -> u64;

    /// 对应 `report(error)`：把一个非致命失败转给 `HarnessOptions.onReport`。
    fn report(&self, error: SessionError);

    /// 对应 `sleep(until, context)`：等 Harness 时钟到达 `until`。
    async fn sleep(&self, until: u64, context: Arc<dyn Context>) -> Result<(), SessionError>;
}

/// 任务运行期即是一套 hook 能力：把 `Arc<dyn TaskRuntime>` 包装成 [`HookApi`]。
///
/// `TaskRuntime` 的方法面覆盖 [`HookApi`] 的全部要求，但两个 trait object 之间没有子 trait 关系，
/// Rust 无法直接 upcast；这里用委托包装桥接。
pub struct RuntimeHookApi {
    runtime: Arc<dyn TaskRuntime>,
}

impl RuntimeHookApi {
    /// 由任务运行期构造。
    pub fn new(runtime: Arc<dyn TaskRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait::async_trait]
impl DocumentReader for RuntimeHookApi {
    async fn snapshot(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        self.runtime.snapshot(token, owner, key, context).await
    }

    async fn snapshot_as_of(
        &self,
        token: &dyn AnyDocToken,
        owner: u64,
        key: Option<String>,
        at: EntryId,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError> {
        self.runtime
            .snapshot_as_of(token, owner, key, at, context)
            .await
    }
}

#[async_trait::async_trait]
impl HookApi for RuntimeHookApi {
    fn task_id(&self) -> TaskId {
        self.runtime.task_id()
    }

    fn conversation_id(&self) -> ConversationId {
        self.runtime.conversation_id()
    }

    fn models(&self) -> Arc<pi_ai::Models> {
        self.runtime.models()
    }

    async fn memo(
        &self,
        name: &str,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonValue>, SessionError> {
        self.runtime.memo(name, context).await
    }

    async fn memo_or(
        &self,
        name: &str,
        candidate: JsonValue,
        context: Arc<dyn Context>,
    ) -> Result<JsonValue, SessionError> {
        self.runtime.memo_or(name, candidate, context).await
    }
}

// ─── 顶层编排接口（P5h） ──────────────────────────────────────────────────────

/// 对应 `ConversationWatch`：一个会话的结构视图的串行精确帧观察。
///
/// 泛型 `ConversationView` 在 Rust 侧退化为 [`JsonValue`]（与 [`WatchHandle`] 一致）。
pub type ConversationWatch = Arc<dyn WatchHandle>;

/// 对应 `ConversationInit`：在创建提交内、创建钩子与 `agent` 变更之后运行。
pub type ConversationInit = Arc<
    dyn Fn(&Transaction, ConversationId) -> BoxFuture<'static, Result<(), SessionError>>
        + Send
        + Sync,
>;

/// 对应 `HarnessOptions.conversationCreated`：每次创建或分叉会话的提交里，在内置文档之后运行。
pub type ConversationCreated = Arc<
    dyn Fn(&Transaction, ConversationRecord) -> BoxFuture<'static, Result<(), SessionError>>
        + Send
        + Sync,
>;

/// 对应 `HarnessOptions.env`：为一次使用构建会话环境。
pub type EnvBuilder = Arc<
    dyn Fn(
            EnvTarget,
            Arc<dyn Context>,
        ) -> BoxFuture<'static, Result<Option<Arc<dyn ExecutionEnv>>, SessionError>>
        + Send
        + Sync,
>;

/// 对应 `ConversationCreateOptions`。
#[derive(Clone)]
pub struct ConversationCreateOptions {
    /// 拥有关系。
    pub ownership: ConversationOwnership,
    /// 在创建提交内、创建钩子副本之后、`init` 之前应用的 agent 变更。
    pub agent: Option<AgentChange>,
    /// 在创建提交内、`agent` 之后运行的初始化。
    pub init: Option<ConversationInit>,
}

/// 对应 `HarnessOptions`：打开一个 Harness 所需的宿主配置。
pub struct HarnessOptions {
    /// pi-ai 模型访问（generation 使用）。
    pub models: Arc<pi_ai::Models>,
    /// 注册表只读视图。
    pub registry: Arc<dyn RegistryReader>,
    /// Harness 级运行策略；缺省字段取内置默认。
    pub settings: Option<HarnessSettings>,
    /// 为一次使用构建会话环境；无环境时为 `None`。
    pub env: Option<EnvBuilder>,
    /// 每次创建或分叉会话的提交里，在内置 `pi.*` 文档之后运行。
    pub conversation_created: Option<ConversationCreated>,
    /// Harness 时钟；缺省取系统墙钟。
    pub now: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    /// 接收不使调用失败的上报错误。
    pub on_report: Option<Arc<dyn Fn(SessionError) + Send + Sync>>,
}

/// 对应 `Harness.abortSubmission()` 的返回。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessAbortSubmission {
    /// `aborted`。
    Aborted,
    /// `already_placed`。
    AlreadyPlaced,
    /// `settled`。
    Settled,
    /// `not_found`。
    NotFound,
}

/// 对应 `Harness.abortTask()` 的返回。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAbort {
    /// `marked`。
    Marked,
    /// `terminal`。
    Terminal,
}

/// 对应 `Conversation`：绑定到返回它的 Harness 的一个会话的无状态句柄，按 `id` 比较。
#[async_trait::async_trait]
pub trait Conversation: Send + Sync {
    /// 会话 ID。
    fn id(&self) -> ConversationId;

    /// 以当前注册表快照与设置解析 agent。
    async fn agent(&self, context: Arc<dyn Context>) -> Result<Agent, SessionError>;

    /// 在自己的提交里调用 `configure()`。
    async fn configure(
        &self,
        change: AgentChange,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError>;

    /// 持久接纳用户输入或一次被动条目写入。
    async fn submit(
        &self,
        submission: SubmissionDraft,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Submission>, SessionError>;

    /// 写入一个 `pi.reset` 条目，开始新上下文；`handoff` 给定时作为用户消息携带。
    async fn reset(
        &self,
        handoff: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<(), SessionError>;

    /// 接纳一个手动压缩任务并返回其 ID。
    async fn compact(
        &self,
        instructions: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<TaskId<CompactionResult>, SessionError>;

    /// 会话提交，`tx.createTask()` 默认指向这个会话。
    async fn commit(
        &self,
        change: CommitOperation<'static>,
        context: Arc<dyn Context>,
    ) -> Result<JsonValue, SessionError>;

    /// 已提交的原始活动转录与模型上下文。
    async fn context(
        &self,
        context: Arc<dyn Context>,
        at: Option<EntryId>,
    ) -> Result<ContextView, SessionError>;

    /// 会话的分叉感知历史，`query.order` 非 `ascending` 时最新在前。
    async fn entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: Arc<dyn Context>,
    ) -> Result<Page<EntryRecord, Cursor>, SessionError>;

    /// 在 `at` 处分叉。
    async fn fork(
        &self,
        at: EntryId,
        options: ConversationCreateOptions,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Conversation>, SessionError>;

    /// 撤回排队输入、中止常规归属作用域、等待其空闲。
    async fn abort(
        &self,
        context: Arc<dyn Context>,
        options: ConversationAbortOptions,
    ) -> Result<(), SessionError>;

    /// 常规归属作用域无存活非后台任务时 resolve。
    async fn wait_for_idle(&self, context: Arc<dyn Context>) -> Result<(), SessionError>;

    /// 结构视图，作为一次性只读 chord 状态。
    async fn view_state(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<AttachedReplicatedState, SessionError>;

    /// 结构视图，作为有界待投递帧的序列化精确帧观察。
    async fn watch(&self, context: Arc<dyn Context>) -> Result<ConversationWatch, SessionError>;
}

/// 对应 `Harness`：一个会话上的 durable agent harness。
#[async_trait::async_trait]
pub trait Harness: Session {
    /// 启用任务调度；幂等，关闭后抛错。
    fn resume(&self);

    /// 返回保留的根会话；缺席时在单个提交里以 `agent` 与 `init` 创建。
    async fn root(
        &self,
        context: Arc<dyn Context>,
        agent: Option<AgentChange>,
        init: Option<ConversationInit>,
    ) -> Result<Arc<dyn Conversation>, SessionError>;

    /// 已存在的会话句柄，或 `None`。
    async fn conversation(
        &self,
        id: ConversationId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn Conversation>>, SessionError>;

    /// 创建一个独立会话。
    async fn create_conversation(
        &self,
        options: ConversationCreateOptions,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn Conversation>, SessionError>;

    /// 任务记录，或 `None`。
    async fn get_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<Option<AnyTaskRecord>, SessionError>;

    /// 存活任务与未结清提交。
    async fn inspect(&self, context: Arc<dyn Context>) -> Result<HarnessInspection, SessionError>;

    /// 重新取得一个提交，例如重新打开之后。
    async fn submission(
        &self,
        id: SubmissionId,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<dyn Submission>>, SessionError>;

    /// 未知提交或另一个会话的提交返回 `not_found`。
    async fn abort_submission(
        &self,
        id: SubmissionId,
        context: Arc<dyn Context>,
        conversation_id: Option<ConversationId>,
    ) -> Result<HarnessAbortSubmission, SessionError>;

    /// 提交中止标记、信号并加入活动的 run 调用，再调度中止调用。
    async fn abort_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<TaskAbort, SessionError>;

    /// 以终态回执 resolve；取消 `context` 只取消这次等待。
    async fn wait_for_task(
        &self,
        id: TaskId,
        context: Arc<dyn Context>,
    ) -> Result<SettledTask, SessionError>;

    /// 每个无主会话的常规归属作用域无存活非后台任务时 resolve。
    async fn wait_for_idle(&self, context: Arc<dyn Context>) -> Result<(), SessionError>;

    /// 会话总量：每个会话 `pi.usage` 之和。
    async fn usage(&self, context: Arc<dyn Context>) -> Result<UsageState, SessionError>;

    /// 每个存活任务与其拥有边、状态与拥有的会话。
    async fn task_graph(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<AttachedReplicatedState, SessionError>;

    /// 任务图的序列化精确帧观察。
    async fn watch_task_graph(
        &self,
        context: Arc<dyn Context>,
    ) -> Result<Arc<dyn WatchHandle>, SessionError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_change_carries_the_three_states() {
        let unchanged: FieldChange<String> = FieldChange::default();
        assert!(unchanged.is_unchanged());
        assert_eq!(unchanged.into_value(), None);

        let cleared: FieldChange<String> = FieldChange::Clear;
        assert_eq!(cleared.into_value(), Some(None));

        let set: FieldChange<String> = FieldChange::Set("x".to_string());
        assert_eq!(set.into_value(), Some(Some("x".to_string())));
    }

    #[test]
    fn agent_state_round_trips_names_not_objects() {
        let state = AgentState {
            model: Some(ModelRef {
                provider: "deepseek".to_string(),
                model_id: "deepseek-chat".to_string(),
            }),
            extensions: Some(ExtensionStateSelection::Edit {
                add: Some(vec!["extra".to_string()]),
                remove: None,
            }),
            tools: Some(ToolStateSelection::Names(vec!["read".to_string()])),
            cwd: Some("/work".to_string()),
            ..AgentState::default()
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["model"]["provider"], serde_json::json!("deepseek"));
        assert_eq!(json["model"]["modelId"], serde_json::json!("deepseek-chat"));
        assert_eq!(json["extensions"]["add"][0], serde_json::json!("extra"));
        assert_eq!(json["tools"][0], serde_json::json!("read"));
        assert_eq!(json["cwd"], serde_json::json!("/work"));
        let decoded: AgentState = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, state);
    }

    #[test]
    fn agent_state_omits_unset_fields() {
        let json = serde_json::to_value(AgentState::default()).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn settled_submission_rejects_a_non_terminal_record() {
        let record = SubmissionRecord::Input {
            identity: crate::types::SubmissionIdentity {
                id: SubmissionId::new(1),
                conversation_id: ConversationId::new(1),
                request_id: None,
            },
            status: crate::types::InputSubmissionStatus::Queued,
        };
        let result = std::panic::catch_unwind(|| SettledSubmissionRecord::new(record));
        assert!(result.is_err(), "queued 提交不得成为终态记录");
    }

    #[test]
    fn settings_defaults_match_the_upstream_constants() {
        // 上游 `agent.ts` 的 DEFAULT_* 常量；改这里即表示有意偏离。
        let settings = Settings::default();
        assert_eq!(
            settings.retry,
            ConversationRetryPolicy {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 2_000,
                max_agent_delay_ms: Some(60_000),
            }
        );
        assert_eq!(
            settings.compaction,
            CompactionPolicy {
                enabled: true,
                reserve_tokens: 16_384,
                keep_recent_tokens: 20_000,
                background_tokens: 32_768,
            }
        );
        assert_eq!(
            settings.progress,
            ProgressPolicy {
                partial_interval_ms: 100,
                output_interval_ms: 100,
            }
        );
        assert_eq!(settings.tool_execution, ToolExecutionMode::Parallel);
        assert_eq!(settings.steering_mode, QueueMode::OneAtATime);
        assert_eq!(settings.follow_up_mode, QueueMode::OneAtATime);
        assert_eq!(settings.context_retention_ms, 600_000);
        assert!(settings.extensions.is_none());
    }

    #[test]
    fn registry_snapshot_finds_by_name() {
        let extension = Arc::new(Extension {
            name: "demo".to_string(),
            tools: Vec::new(),
            sections: Vec::new(),
            hooks: Vec::new(),
            wraps: Vec::new(),
            tasks: Vec::new(),
        });
        let snapshot = RegistrySnapshot::new(vec![Arc::clone(&extension)], Vec::new());
        assert_eq!(snapshot.installed().len(), 1);
        assert!(snapshot.extension("demo").is_some());
        assert!(snapshot.extension("missing").is_none());
        assert!(snapshot.tasks().is_empty());
    }
}
