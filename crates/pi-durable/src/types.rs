//! durable 的契约层（对应 `src/types.ts`）。
//!
//! 本文件分阶段补齐：当前包含**基础类型**（`JsonObject`、`Id`、`Seq` 与各 ID 别名）。
//! 后续阶段继续补 `Document` / `Conversation` / `Task` / `Storage` 等契约。

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use pi_ai::Message;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::chord::delta::Op;

/// 对应 `JsonObject`。
pub type JsonObject = serde_json::Map<String, JsonValue>;

// ─── ID 品牌 ─────────────────────────────────────────────────────────────────

/// 对应 `Id<Kind, Type>`：带品牌标记的数值 ID。
///
/// TS 用 `number & { readonly __brand: Kind }` 的交叉类型实现品牌；Rust 用带
/// `PhantomData` 的 newtype —— 不同 `Kind` 的 ID 无法互相赋值，`Type` 仅作类型标记
/// （对应上游给 `TaskId<Result>` 携带结果类型）。
pub struct Id<Kind, T = ()> {
    value: u64,
    _marker: PhantomData<fn() -> (Kind, T)>,
}

impl<Kind, T> Id<Kind, T> {
    /// 对应 `idFromNumber`：在可信的分配/解码边界施加品牌。
    pub const fn new(value: u64) -> Self {
        Self {
            value,
            _marker: PhantomData,
        }
    }

    /// 取出底层数值。
    pub const fn get(self) -> u64 {
        self.value
    }
}

impl<Kind, T> Clone for Id<Kind, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Kind, T> Copy for Id<Kind, T> {}

impl<Kind, T> PartialEq for Id<Kind, T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<Kind, T> Eq for Id<Kind, T> {}

impl<Kind, T> PartialOrd for Id<Kind, T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<Kind, T> Ord for Id<Kind, T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl<Kind, T> std::hash::Hash for Id<Kind, T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<Kind, T> std::fmt::Debug for Id<Kind, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.value)
    }
}

impl<Kind, T> std::fmt::Display for Id<Kind, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.value)
    }
}

impl<Kind, T> Serialize for Id<Kind, T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.value)
    }
}

impl<'de, Kind, T> Deserialize<'de> for Id<Kind, T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::new(u64::deserialize(deserializer)?))
    }
}

/// `ConversationId` 的品牌。
pub struct ConversationKind;
/// `EntryId` 的品牌。
pub struct EntryKind;
/// `TaskId` 的品牌。
pub struct TaskKind;
/// `SubmissionId` 的品牌。
pub struct SubmissionKind;
/// `DocumentId` 的品牌。
pub struct DocumentKind;

/// 对应 `ConversationId`。
pub type ConversationId = Id<ConversationKind>;
/// 对应 `EntryId`。
pub type EntryId = Id<EntryKind>;
/// 对应 `TaskId<Result>`。
pub type TaskId<T = ()> = Id<TaskKind, T>;
/// 对应 `SubmissionId`。
pub type SubmissionId = Id<SubmissionKind>;
/// 对应 `DocumentId`。
pub type DocumentId = Id<DocumentKind>;

/// 对应 `ROOT_CONVERSATION_ID`。
pub const ROOT_CONVERSATION_ID: ConversationId = Id::new(1);

// ─── 提交序号 ────────────────────────────────────────────────────────────────

/// 对应 `Seq`：单调递增的提交序号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    /// 对应 `seqFromNumber`：在可信的存储边界施加品牌。
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// 取出底层数值。
    pub const fn get(self) -> u64 {
        self.0
    }

    /// 下一个序号（对应上游的 `seq + 1` 用法）。
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

// ─── 会话记录 ───────────────────────────────────────────────────────────────

/// 对应 `ConversationOwnership`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationOwnership {
    /// 无主（ownerless）。
    Ownerless,
    /// 由某个任务拥有。
    Task { task_id: TaskId },
}

/// 对应 `ConversationRecord.parent`：分叉来源与其（含）父条目。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationParent {
    /// 源会话。
    pub conversation_id: ConversationId,
    /// 通过它继承历史的父条目（含）。
    pub at: EntryId,
}

/// 对应 `ConversationRecord.owner`：创建边，用于归属、子树中止与空闲等待。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationOwner {
    /// 所属会话。
    pub conversation_id: ConversationId,
    /// 所属任务。
    pub task_id: TaskId,
}

/// 对应 `ConversationRecord`：一段 transcript 作用域的不可变身份与谱系。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRecord {
    /// 会话 ID。
    pub id: ConversationId,
    /// 分叉来源与其父条目。
    pub parent: Option<ConversationParent>,
    /// 创建边。
    pub owner: Option<ConversationOwner>,
}

// ─── 上下文编辑 ─────────────────────────────────────────────────────────────

/// 对应 `ContextEdit`：对某个可见条目在模型上下文中的贡献做不可变覆盖。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContextEdit {
    /// 对应 `action: "omit"`：省略该条目的模型消息。
    Omit {
        /// 被覆盖的条目。
        target: EntryId,
    },
    /// 对应 `action: "replace"`：用给定消息替换该条目的模型消息。
    Replace {
        /// 被覆盖的条目。
        target: EntryId,
        /// 替代贡献的消息。
        messages: Vec<Message>,
    },
}

// ─── 条目 ────────────────────────────────────────────────────────────────────

/// 对应 `EntryRecord`：不可变的 transcript 事件（模型面与应用面负载分开）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryRecord {
    /// 条目 ID。
    pub id: EntryId,
    /// 所属会话。
    pub conversation_id: ConversationId,
    /// 应用定义的条目判别符。
    pub kind: String,
    /// 贡献给模型上下文的消息；展示或记账类条目缺省。
    pub model: Option<Vec<Message>>,
    /// 供视图/扩展/记账逻辑使用的 JSON 负载。
    pub data: Option<JsonValue>,
    /// 本条目选中的活动上下文的首条目。
    pub head: Option<EntryId>,
    /// 对更早可见条目的仅上下文覆盖。
    pub edits: Option<Vec<ContextEdit>>,
    /// 追加本条目的任务（由 durable 工作产生时）。
    pub by_task_id: Option<TaskId>,
}

/// 对应 `EntryDraft.head`：`Self` 表示「把活动上下文起点设为新分配的条目 ID」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryHead {
    /// 显式指定条目。
    Entry(EntryId),
    /// 对应 `"self"`。
    SelfEntry,
}

/// 对应 `EntryDraft`：会话分配身份与任务归属之前提供的条目内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryDraft {
    /// 应用定义的条目判别符。
    pub kind: String,
    /// 模型消息。
    pub model: Option<Vec<Message>>,
    /// JSON 负载。
    pub data: Option<JsonValue>,
    /// 活动上下文首条目。
    pub head: Option<EntryHead>,
    /// 上下文覆盖。
    pub edits: Option<Vec<ContextEdit>>,
}

/// 对应 `TypedEntry<D>`：`data` 已窄化为 `D` 的条目视图。
///
/// TS 用 `Omit<EntryRecord, "data"> & { data: D }` 的条件类型表达；Rust 用「记录 + 已解析数据」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypedEntry<D> {
    /// 原始记录（`data` 仍为 JSON）。
    pub record: EntryRecord,
    /// 已解析的 `data`。
    pub data: D,
}

impl EntryRecord {
    /// 把 `data` 反序列化为 `D`，得到 [`TypedEntry`]。
    pub fn typed<D: serde::de::DeserializeOwned>(
        &self,
    ) -> Result<TypedEntry<D>, serde_json::Error> {
        let raw = self.data.clone().unwrap_or(JsonValue::Null);
        Ok(TypedEntry {
            record: self.clone(),
            data: serde_json::from_value(raw)?,
        })
    }
}

/// 对应 `Entry<D>`：带窄化守卫的类型化条目种类。
///
/// TS 的 `is()` 是类型守卫（`entry is TypedEntry<D>`）；Rust 无法在 trait 上表达类型窄化，
/// 因此 [`EntryToken::is`] 返回 `bool`，调用方再自行 `typed()` 解析。
///
/// （命名避开 [`EntryKind`]：后者是 `EntryId` 的品牌标记。）
pub trait EntryToken<D = ()> {
    /// 对应 `kind`。
    fn kind(&self) -> &str;

    /// 对应 `is(entry)`。
    fn is(&self, entry: Option<&EntryRecord>) -> bool {
        entry.is_some_and(|candidate| candidate.kind == self.kind())
    }
}

// ─── 任务 ────────────────────────────────────────────────────────────────────

/// 对应 `TaskOutcomeError`：持久化的 JSON 安全错误快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutcomeError {
    /// 错误消息。
    pub message: String,
    /// 可选的结构化诊断数据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<JsonValue>,
}

/// 对应 `TaskOutcome<R>`：任务进入终态时的持久化原因与可选结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskOutcome<R> {
    /// 完成。
    Completed {
        /// 结果。
        result: R,
    },
    /// 实现显式提交的预期失败。
    Failed {
        /// 错误快照。
        error: TaskOutcomeError,
        /// 可选结果。
        result: Option<R>,
    },
    /// 由任务的 abort 协议处理的显式取消。
    Aborted {
        /// 可选原因。
        reason: Option<String>,
        /// 可选结果。
        result: Option<R>,
    },
    /// 定义或迁移缺失，无法继续。
    Orphaned {
        /// 原因。
        reason: String,
    },
    /// 运行时发现的契约失败（未捕获抛错、无持久进展等）。
    Faulted {
        /// 错误快照。
        error: TaskOutcomeError,
    },
}

/// 对应 `JoinPolicy`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy {
    /// 任一失败即失败。
    FailFast,
    /// 等待全部结束。
    AllSettled,
}

/// 对应 `TaskState["status"]`：任务状态的判别符。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// 可被调度。
    Pending,
    /// 已被某个内存调用占用。
    Running,
    /// 挂起等待 `on` 中任务全部终态。
    Waiting,
    /// 结果已定，等下属工作收尾。
    Completing,
    /// 永久落定的结果回执。
    Terminal,
}

/// 对应 `TaskState<S, R>`：任务的完整持久执行状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TaskState<S, R> {
    /// 可被调度。
    Pending {
        /// 可从中恢复执行的完整持久状态。
        checkpoint: S,
    },
    /// 已被某个内存调用占用。
    Running {
        /// 可从中恢复执行的完整持久状态。
        checkpoint: S,
    },
    /// 等 `on` 中任务全部终态后从 `checkpoint` 继续。
    Waiting {
        /// 恢复点。
        checkpoint: S,
        /// 所等待的任务。
        on: Vec<TaskId>,
        /// 汇合策略。
        policy: JoinPolicy,
    },
    /// 结果已定，不再跑代码。
    Completing {
        /// 结果。
        outcome: TaskOutcome<R>,
    },
    /// 永久落定。
    Terminal {
        /// 结果。
        outcome: TaskOutcome<R>,
    },
}

impl<S, R> TaskState<S, R> {
    /// 对应上游的 `state.status` 判别符。
    pub fn status(&self) -> TaskStatus {
        match self {
            TaskState::Pending { .. } => TaskStatus::Pending,
            TaskState::Running { .. } => TaskStatus::Running,
            TaskState::Waiting { .. } => TaskStatus::Waiting,
            TaskState::Completing { .. } => TaskStatus::Completing,
            TaskState::Terminal { .. } => TaskStatus::Terminal,
        }
    }
}

/// 对应 `TaskRecord<I, S, R>`：一个持久任务状态机的完整替换记录。
///
/// 上游用「`TaskRecordBase` + （带 memos / 不带 memos）的判别联合」表达；
/// Rust 统一为 `memos: Option<...>`（`completing`/`terminal` 时约定为 `None`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord<I, S, R> {
    /// 任务 ID。
    pub id: TaskId<R>,
    /// 所属会话。
    pub conversation_id: ConversationId,
    /// 已注册的任务定义名。
    pub kind: String,
    /// 定义版本（用于迁移 live input 与 checkpoint）。
    pub version: u32,
    /// 原始任务输入。
    pub input: I,
    /// 子任务的拥有者；会话拥有的任务缺省。
    pub owner: Option<TaskId>,
    /// 是否被排除在常规空闲等待 / 会话中止 / 级联之外。
    pub background: bool,
    /// 持久的中止标记。
    pub abort_requested: bool,
    /// 首次进入 `running` 的墙钟毫秒（由 Session 盖章）。
    pub started_at: Option<u64>,
    /// 进入 `terminal` 的墙钟毫秒（由 Session 盖章）。
    pub ended_at: Option<u64>,
    /// 状态机。
    pub state: TaskState<S, R>,
    /// 任务可运行期间保留的「首写者胜」小值。
    pub memos: Option<std::collections::BTreeMap<String, JsonValue>>,
}

/// 对应 `TaskOwnership`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskOwnership {
    /// 由会话拥有。
    Conversation,
    /// 由另一任务拥有。
    Task {
        /// 拥有者任务。
        task_id: TaskId,
    },
}

// ─── 提交 ────────────────────────────────────────────────────────────────────

/// 对应 `SubmissionRecord["status"]`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionStatus {
    /// 已接纳但尚未进入 transcript。
    Queued,
    /// 已进入 transcript。
    Placed,
    /// 已成功应答。
    Done,
    /// 终结且不再可能应答。
    Unanswered,
}

/// `input` 类提交的生命周期。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputSubmissionStatus {
    /// 已接纳但尚未进入 transcript。
    Queued,
    /// 已加入 transcript，由活动运行拥有。
    Placed {
        /// 对应条目。
        entry: EntryId,
    },
    /// 已成功应答。
    Done {
        /// 输入条目。
        entry: EntryId,
        /// 应答条目。
        answer: EntryId,
    },
    /// 终结且不再可能应答。
    Unanswered {
        /// 可选条目。
        entry: Option<EntryId>,
        /// 原因。
        reason: String,
        /// 可选细节。
        detail: Option<JsonValue>,
    },
}

/// `write` 类提交的生命周期。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WriteSubmissionStatus {
    /// 已接纳但尚未追加。
    Queued,
    /// 已成功追加被动条目。
    Done {
        /// 追加的条目。
        entry: EntryId,
    },
    /// 终结且未能写入。
    Unanswered {
        /// 原因。
        reason: String,
        /// 可选细节。
        detail: Option<JsonValue>,
    },
}

/// 每种提交状态共有的身份字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionIdentity {
    /// 提交 ID。
    pub id: SubmissionId,
    /// 所属会话。
    pub conversation_id: ConversationId,
    /// 宿主提供的去重键（会话内唯一）。
    pub request_id: Option<String>,
}

/// 对应 `SubmissionRecord`：一次被接纳的用户输入或被动写入的持久生命周期。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubmissionRecord {
    /// `type: "input"`。
    Input {
        /// 身份字段。
        identity: SubmissionIdentity,
        /// 生命周期。
        status: InputSubmissionStatus,
    },
    /// `type: "write"`。
    Write {
        /// 身份字段。
        identity: SubmissionIdentity,
        /// 生命周期。
        status: WriteSubmissionStatus,
    },
}

impl SubmissionRecord {
    /// 身份字段。
    pub fn identity(&self) -> &SubmissionIdentity {
        match self {
            SubmissionRecord::Input { identity, .. } | SubmissionRecord::Write { identity, .. } => {
                identity
            }
        }
    }

    /// 判别符（对应 `SubmissionRecord["status"]` 的取值集合）。
    pub fn status(&self) -> SubmissionStatus {
        match self {
            SubmissionRecord::Input { status, .. } => match status {
                InputSubmissionStatus::Queued => SubmissionStatus::Queued,
                InputSubmissionStatus::Placed { .. } => SubmissionStatus::Placed,
                InputSubmissionStatus::Done { .. } => SubmissionStatus::Done,
                InputSubmissionStatus::Unanswered { .. } => SubmissionStatus::Unanswered,
            },
            SubmissionRecord::Write { status, .. } => match status {
                WriteSubmissionStatus::Queued => SubmissionStatus::Queued,
                WriteSubmissionStatus::Done { .. } => SubmissionStatus::Done,
                WriteSubmissionStatus::Unanswered { .. } => SubmissionStatus::Unanswered,
            },
        }
    }
}

/// 对应 `SubmissionSettlement`：为提交准备的终态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubmissionSettlement {
    /// 完成。
    Done {
        /// 应答条目。
        answer: EntryId,
    },
    /// 未应答。
    Unanswered {
        /// 原因。
        reason: String,
        /// 可选细节。
        detail: Option<JsonValue>,
    },
}

// ─── 文档 ────────────────────────────────────────────────────────────────────

/// 对应会话文档的 `history`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationHistory {
    /// 只保留当前状态。
    Latest,
    /// 保留支持 as-of 读取所需的历史。
    Rewindable,
}

/// 对应会话文档的 `fork`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationFork {
    /// 从当前源状态或定义的初始值初始化分叉。
    Current,
    /// 从初始值初始化。
    Initial,
    /// 在其截断点初始化（仅 `rewindable` 允许）。
    AsOf,
}

/// 对应 `DocumentRecord.scope`：地址与记录侧的作用域（**不含** history/fork）。
///
/// 上游把 history/fork 放在记录顶层（只有会话文档声明），因为它们描述的是保留语义而不是身份；
/// 地址相等性与扫描匹配都不看它们。定义侧的语义见 [`DocumentSemantics`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DocumentScope {
    /// 会话级单例。
    Session,
    /// 会话文档。
    Conversation {
        /// 所属会话。
        conversation_id: ConversationId,
    },
    /// 任务文档。
    Task {
        /// 所属任务。
        task_id: TaskId,
    },
}

/// 对应 `DocumentSemantics`：文档定义声明的作用域与（会话文档的）history/fork 行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentSemantics {
    /// 会话级单例。
    Session,
    /// 会话文档。
    Conversation {
        /// 历史保留策略。
        history: ConversationHistory,
        /// 分叉初始化策略。
        fork: ConversationFork,
    },
    /// 任务文档。
    Task,
}

/// 对应 `DocumentRecord`：一次「创建到退役」的文档化身记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentRecord {
    /// 化身 ID（同一逻辑文档重建时不会复用）。
    pub id: DocumentId,
    /// 稳定的文档定义 kind。
    pub kind: String,
    /// 家族成员键；单例文档缺省。
    pub key: Option<String>,
    /// 创建该化身的提交（由存储盖章）。
    pub created_at: Seq,
    /// 退役该化身的提交；仍是当前时缺省。
    pub retired_at: Option<Seq>,
    /// 对应会话文档的 `history`；非会话文档缺省（上游记录顶层字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<ConversationHistory>,
    /// 对应会话文档的 `fork`；非会话文档缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<ConversationFork>,
    /// 作用域（不含 history/fork）。
    pub scope: DocumentScope,
}

/// 对应 `DocumentCreate`：创建新 `DocumentRecord` 时提供的字段（不含存储盖章的 `createdAt`/`retiredAt`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentCreate {
    /// 化身 ID。
    pub id: DocumentId,
    /// 文档定义 kind。
    pub kind: String,
    /// 家族成员键。
    pub key: Option<String>,
    /// 对应会话文档的 `history`；非会话文档缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<ConversationHistory>,
    /// 对应会话文档的 `fork`；非会话文档缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<ConversationFork>,
    /// 作用域（不含 history/fork）。
    pub scope: DocumentScope,
}

// ─── 文档定义 ────────────────────────────────────────────────────────────────

/// 对应 `CheckpointInfo`：传给文档 `checkpointWhen` 的已存储重放状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointInfo {
    /// 最新 base 之后已存储的增量数（不含正在评估的这次变更）。
    pub deltas_since_base: usize,
}

/// 对应 `CommonDocDefinition<T>` + `DocumentSemantics`，擦除泛型后的定义形态。
///
/// TS 用带方法的对象字面量表达（`initial()` / `migrate?()` / `checkpointWhen?()`）；Rust 用 trait。
/// `T extends JsonObject` 在此统一为 [`JsonObject`]。
pub trait DocDefinitionSpec: Send + Sync {
    /// 对应 `kind`：稳定的持久化种类名（属于公开协议）。
    fn kind(&self) -> &str;

    /// 对应 `version`：存储值形态的正整数版本。
    fn version(&self) -> u32;

    /// 对应 `DocumentSemantics`：作用域与（会话文档的）history/fork 行为。
    fn semantics(&self) -> DocumentSemantics;

    /// 对应家族定义上的 `family: true`。
    fn is_family(&self) -> bool {
        false
    }

    /// 对应 `initial(seed?)`：单例忽略 `seed`，家族成员仅在缺席时用它。
    fn initial(&self, seed: Option<&JsonValue>) -> JsonObject;

    /// 对应 `migrate?(value, fromVersion)`。
    fn migrate(&self, _value: &JsonObject, _from_version: u32) -> Option<JsonObject> {
        None
    }

    /// 是否提供了 `migrate`（TS 用 `definition.migrate === undefined` 判断）。
    fn has_migrate(&self) -> bool {
        false
    }

    /// 对应 `checkpointWhen?(value, ops, info)`：返回 `true` 则把本次普通变更存为完整 base。
    fn checkpoint_when(&self, _value: &JsonObject, _ops: &[Op], _info: &CheckpointInfo) -> bool {
        false
    }
}

/// 对应 `DocToken<T, D>`（单例文档的访问令牌）。
#[derive(Clone)]
pub struct DocToken {
    definition: std::sync::Arc<dyn DocDefinitionSpec>,
}

impl DocToken {
    /// 由定义构造（对应 `defineDoc` 的返回）。
    pub fn new(definition: std::sync::Arc<dyn DocDefinitionSpec>) -> Self {
        Self { definition }
    }

    /// 对应 `token.definition`。
    pub fn definition(&self) -> &std::sync::Arc<dyn DocDefinitionSpec> {
        &self.definition
    }
}

/// 对应 `DocFamilyToken<T, I, D>`（键控家族文档的访问令牌）。
#[derive(Clone)]
pub struct DocFamilyToken {
    definition: std::sync::Arc<dyn DocDefinitionSpec>,
}

impl DocFamilyToken {
    /// 由定义构造（对应 `defineDocFamily` 的返回）。
    pub fn new(definition: std::sync::Arc<dyn DocDefinitionSpec>) -> Self {
        Self { definition }
    }

    /// 对应 `token.definition`。
    pub fn definition(&self) -> &std::sync::Arc<dyn DocDefinitionSpec> {
        &self.definition
    }
}

// ─── 任务定义 ────────────────────────────────────────────────────────────────

/// 对应 `TaskDefinition<I, S, R, H>`：可执行的持久状态机定义。
///
/// 上游的 `phases` 是「每个 phase 一个处理器」的映射；Rust 用 [`run_phase`](Self::run_phase)
/// 接收 phase 名（与 [phases](Self::phases) 声明的集合对应）。
///
/// 默认实现返回错误，因此不跑阶段的类型（例如测试桩）无需实现它；真实任务必须覆盖。
#[async_trait::async_trait]
pub trait TaskDefinitionSpec: Send + Sync {
    /// 对应 `name`：注册名，持久化在 `TaskRecord.kind`。
    fn name(&self) -> &str;

    /// 对应 `version`：与 live input / checkpoint 一同持久化。
    fn version(&self) -> u32;

    /// 对应 `initial(input)`：新建任务的第一个持久 checkpoint。
    fn initial(&self, input: &JsonValue) -> JsonValue;

    /// 对应 `phases` 的键集合（phase 名）。
    fn phases(&self) -> &[&'static str];

    /// 对应 `phases[phase]`：运行一个 checkpoint 阶段。
    ///
    /// 必须通过 `runtime.commit()` 提交改变的 checkpoint 或终态结果；没有持久进展地返回会使任务 faulted。
    async fn run_phase(
        &self,
        phase: &str,
        task: crate::harness::types::RunningTask,
        runtime: std::sync::Arc<dyn crate::harness::types::TaskRuntime>,
        context: std::sync::Arc<dyn crate::chord::context::Context>,
    ) -> Result<(), crate::session::SessionError> {
        let _ = (phase, task, runtime, context);
        Err(crate::session::SessionError::Message(format!(
            "Task {} has no phase handler for {phase}",
            self.name()
        )))
    }

    /// 对应 `abort(task, runtime, context)`：在 abort 标记之后的新调用里运行，必须提交终态结果。
    async fn abort(
        &self,
        task: crate::harness::types::RunningTask,
        runtime: std::sync::Arc<dyn crate::harness::types::TaskRuntime>,
        context: std::sync::Arc<dyn crate::chord::context::Context>,
    ) -> Result<(), crate::session::SessionError> {
        let _ = (task, runtime, context);
        Err(crate::session::SessionError::Message(format!(
            "Task {} does not implement abort",
            self.name()
        )))
    }

    /// 对应 `migrate?(input, checkpoint, fromVersion)`：把任意更旧受支持版本存下的记录转换过来；在保留时运行。
    fn migrate(
        &self,
        _input: &JsonValue,
        _checkpoint: &JsonValue,
        _from_version: u32,
    ) -> Option<(JsonValue, JsonValue)> {
        None
    }

    /// 是否提供了 `migrate`（TS 用 `definition.migrate === undefined` 判断）。
    fn has_migrate(&self) -> bool {
        false
    }

    /// 对应 `hooks?`：该任务定义的 hook 处理器映射。
    fn hooks(&self) -> Option<std::sync::Arc<dyn crate::harness::types::HookRunner>> {
        None
    }
}

/// 对应 `Task<I, S, R, H>`：类型化的可执行任务定义。
#[derive(Clone)]
pub struct Task {
    definition: std::sync::Arc<dyn TaskDefinitionSpec>,
}

impl Task {
    /// 由定义构造。
    pub fn new(definition: std::sync::Arc<dyn TaskDefinitionSpec>) -> Self {
        Self { definition }
    }

    /// 对应 `task.definition`。
    pub fn definition(&self) -> &std::sync::Arc<dyn TaskDefinitionSpec> {
        &self.definition
    }
}

/// 对应 `TaskOptions`：创建持久任务的选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOptions {
    /// 必需：任务总是声明其拥有者。
    pub ownership: TaskOwnership,
    /// 缺省为拥有者任务的会话（或事务绑定的会话）。
    pub conversation_id: Option<ConversationId>,
    /// 仅会话拥有的任务：排除在常规空闲等待 / 会话中止 / 级联之外。
    pub background: Option<bool>,
}

/// 对应 `SubmissionCreate`：会话分配 ID 之前提供的提交字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubmissionCreate {
    /// `type: "input"`。
    Input {
        /// 所属会话。
        conversation_id: ConversationId,
        /// 去重键。
        request_id: Option<String>,
        /// 初始生命周期。
        status: InputSubmissionStatus,
    },
    /// `type: "write"`。
    Write {
        /// 所属会话。
        conversation_id: ConversationId,
        /// 去重键。
        request_id: Option<String>,
        /// 初始生命周期。
        status: WriteSubmissionStatus,
    },
}

/// 对应文档访问的参数化 owner（TS 用重载参数表达）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocAccess {
    /// 会话或任务 ID（按定义的 scope 解释）。
    pub owner: Option<u64>,
    /// 家族成员键。
    pub key: Option<String>,
}

// ─── 事务草稿 ────────────────────────────────────────────────────────────────

/// 对应上游 `Draft<T>`：事务内文档的共享可变草稿。
///
/// 上游用 Proxy 让调用方像改普通对象一样写 `draft.foo = 1`；Rust 用显式方法（与
/// [`crate::chord::tracker::Change`] 一致），并用 `Arc<Mutex<Option<Change>>>` 让事务与
/// 调用方**共享同一份变更** —— 事务才能在 settle 时取出并 `prepare` 它
/// （对应上游 `change.prepare()`）。
#[derive(Clone)]
pub struct Draft {
    change: Arc<Mutex<Option<crate::chord::tracker::Change>>>,
}

impl std::fmt::Debug for Draft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Draft").finish_non_exhaustive()
    }
}

impl Draft {
    /// 由变更构造（事务内部使用）。
    pub fn new(change: crate::chord::tracker::Change) -> Self {
        Self {
            change: Arc::new(Mutex::new(Some(change))),
        }
    }

    /// 对应事务在 settle 时取回变更（取走后草稿失效）。
    pub fn take(&self) -> Option<crate::chord::tracker::Change> {
        self.change.lock().expect("draft").take()
    }

    /// 是否已被事务取走。
    pub fn is_taken(&self) -> bool {
        self.change.lock().expect("draft").is_none()
    }

    /// 当前值（对应 `draft` 的读形态）。
    pub fn value(&self) -> JsonValue {
        self.change
            .lock()
            .expect("draft")
            .as_ref()
            .map(|change| change.state().clone())
            .unwrap_or(JsonValue::Null)
    }

    /// 对应 `draft[path] = value`。
    pub fn set(
        &self,
        path: crate::chord::delta::Path,
        value: JsonValue,
    ) -> Result<(), crate::chord::delta::DeltaError> {
        self.with_change(|change| change.set(path, value))
    }

    /// 对应 `delete draft[path]`。
    pub fn delete(
        &self,
        path: crate::chord::delta::Path,
    ) -> Result<(), crate::chord::delta::DeltaError> {
        self.with_change(|change| change.delete(path))
    }

    /// 对应 `draft[path] += text`。
    pub fn append(
        &self,
        path: crate::chord::delta::Path,
        text: impl Into<String>,
    ) -> Result<(), crate::chord::delta::DeltaError> {
        self.with_change(|change| change.append(path, text))
    }

    /// 对应 `draft[path] = draft[path].slice(count)`。
    pub fn truncate(
        &self,
        path: crate::chord::delta::Path,
        count: usize,
    ) -> Result<(), crate::chord::delta::DeltaError> {
        self.with_change(|change| change.truncate(path, count))
    }

    /// 对应 `draft[path].splice(index, deleteCount, ...items)`。
    pub fn splice(
        &self,
        path: crate::chord::delta::Path,
        index: usize,
        delete_count: usize,
        items: Vec<JsonValue>,
    ) -> Result<(), crate::chord::delta::DeltaError> {
        self.with_change(|change| change.splice(path, index, delete_count, items))
    }

    /// 对应 `draft[path]` 的排列重写。
    pub fn move_items(
        &self,
        path: crate::chord::delta::Path,
        permutation: Vec<usize>,
    ) -> Result<(), crate::chord::delta::DeltaError> {
        self.with_change(|change| change.move_items(path, permutation))
    }

    fn with_change<T>(&self, edit: impl FnOnce(&mut crate::chord::tracker::Change) -> T) -> T {
        let mut guard = self.change.lock().expect("draft");
        let change = guard.as_mut().expect("draft is no longer active");
        edit(change)
    }
}

/// 对应 `Tx`：一次 Session 提交回调的事务面。
///
/// 表读取与创建结果都是可信的不可变值，可能与内部提交状态共享。
///
/// # 与上游的差异
///
/// - 上游的 `doc()` 返回 Proxy 式 `Draft<T>`（可像普通对象一样直接改写）；这里返回
///   [`crate::chord::tracker::Change`]，用显式编辑方法产生同样的 `Op`。
/// - 上游 `doc()` / `retireDoc()` 用重载参数列表区分 owner 与家族 key；Rust 用 [`DocAccess`]。
#[async_trait::async_trait]
pub trait Tx: Send + Sync {
    /// 读会话记录。
    async fn conversation(&self, id: ConversationId) -> Option<ConversationRecord>;

    /// 读条目记录。
    async fn entry(&self, id: EntryId) -> Option<EntryRecord>;

    /// 读任务记录。
    async fn task(&self, id: TaskId) -> Option<TaskRecord<JsonValue, JsonValue, JsonValue>>;

    /// 有序扫描会话。
    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> Page<ConversationRecord, Cursor>;

    /// 有序扫描条目。
    async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> Page<EntryRecord, Cursor>;

    /// 会话中带有 `head` 的最新可见条目。
    async fn latest_head_marker(
        &self,
        conversation_id: ConversationId,
    ) -> Option<(EntryRecord, EntryId)>;

    /// 有序扫描任务。
    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> Page<TaskRecord<JsonValue, JsonValue, JsonValue>, Cursor>;

    /// 按会话内唯一的 request ID 查已提交的 submission。
    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Option<SubmissionRecord>;

    /// 以显式选择的拥有关系创建会话。
    async fn create_conversation(&self, ownership: ConversationOwnership) -> ConversationRecord;

    /// 在某个具体可见条目上创建历史分叉。
    async fn fork_conversation(
        &self,
        parent_conversation_id: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
    ) -> ConversationRecord;

    /// 追加条目。
    async fn append_entry(&self, conversation_id: ConversationId, value: EntryDraft)
    -> EntryRecord;

    /// 创建任务，返回其 ID。
    async fn create_task(&self, task: &Task, input: JsonValue, options: TaskOptions) -> TaskId;

    /// 创建一条原始 submission 记录（新 ID；不施加任何接纳规则）。
    async fn create_submission(&self, create: SubmissionCreate) -> SubmissionRecord;

    /// 结算一条 queued/placed 的 submission（同步）。
    fn settle_submission(&self, id: SubmissionId, settlement: SubmissionSettlement);

    /// 把 queued 的 submission 放到某个条目上（同步）。
    fn place_submission(&self, id: SubmissionId, entry: EntryId);

    /// 取得（必要时创建）文档的共享草稿。
    async fn doc(
        &self,
        token: &dyn crate::documents::AnyDocToken,
        access: DocAccess,
        seed: Option<JsonValue>,
    ) -> Result<Draft, crate::documents::DocError>;

    /// 退役文档化身。
    async fn retire_doc(
        &self,
        token: &dyn crate::documents::AnyDocToken,
        access: DocAccess,
    ) -> Result<(), crate::documents::DocError>;
}

// ─── 分页与查询 ──────────────────────────────────────────────────────────────

/// 对应 `Page<T, C>`：一次有序扫描的结果及其可选续扫状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T, C> {
    /// 本次返回的项。
    pub items: Vec<T>,
    /// 续扫游标。
    pub next: Option<C>,
}

/// 对应 `Cursor`：由后端拥有的 JSON 续扫状态。
///
/// 游标携带扫描顺序：给定游标的扫描会延续该顺序，并在查询要求另一种 `order` 时报错。
pub type Cursor = JsonObject;

/// 对应 `ScanOrder`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanOrder {
    /// 最旧优先。
    Ascending,
    /// 最新优先。
    Descending,
}

/// 对应 `ConversationQuery`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationQuery {
    /// 按拥有它的会话过滤。
    pub owner_conversation_id: Option<ConversationId>,
    /// 按拥有它的任务过滤。
    pub owner_task_id: Option<TaskId>,
    /// 默认 `ascending`；带游标时以游标的顺序为准。
    pub order: Option<ScanOrder>,
}

/// 对应 `EntryQuery`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryQuery {
    /// 目标会话。
    pub conversation_id: ConversationId,
    /// 可返回的最旧条目 ID（含）。
    pub min_entry_id: Option<EntryId>,
    /// 可返回的最新条目 ID（含）。
    pub max_entry_id: Option<EntryId>,
    /// 默认 `descending`。
    pub order: Option<ScanOrder>,
}

/// 对应 `TaskQuery`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskQuery {
    /// 按会话过滤。
    pub conversation_id: Option<ConversationId>,
    /// 按定义名过滤。
    pub kind: Option<String>,
    /// 按状态过滤。
    pub status: Option<TaskStatus>,
    /// 按中止标记过滤。
    pub abort_requested: Option<bool>,
    /// 按 background 标记过滤。
    pub background: Option<bool>,
    /// 默认 `ascending`。
    pub order: Option<ScanOrder>,
}

/// 对应 `SubmissionQuery`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionQuery {
    /// 按会话过滤。
    pub conversation_id: Option<ConversationId>,
    /// 按状态过滤。
    pub status: Option<SubmissionStatus>,
    /// 默认 `ascending`。
    pub order: Option<ScanOrder>,
}

// ─── 文档读写 ────────────────────────────────────────────────────────────────

/// 对应 `DocumentPoint`：当前状态或某个历史提交序号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DocumentPoint {
    /// 当前。
    Current,
    /// 指定提交序号。
    At(Seq),
}

/// 对应 `DocumentAddress`：单例或某个家族成员的精确逻辑身份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentAddress {
    /// 文档定义 kind。
    pub kind: String,
    /// 作用域。
    pub scope: DocumentScope,
    /// 缺省选单例，存在则选一个家族成员。
    pub key: Option<String>,
}

/// 对应 `DocumentQuery`：在某一精确作用域、某一点上对存活文档化身的有序扫描。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentQuery {
    /// 作用域。
    pub scope: DocumentScope,
    /// 时间点。
    pub at: DocumentPoint,
    /// 按 kind 过滤。
    pub kind: Option<String>,
}

/// 对应 `DocumentContent.kind: "base"`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentBase {
    /// 定义版本。
    pub version: u32,
    /// 完整值。
    pub value: JsonObject,
}

/// 对应 `DocumentContent`：完整 checkpoint 或一批 chord 操作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DocumentContent {
    /// 完整值。
    Base(DocumentBase),
    /// 增量操作。
    Delta {
        /// 定义版本。
        version: u32,
        /// 操作序列。
        ops: Vec<Op>,
    },
}

/// 对应 `DocumentCopySource`：无定义文档拷贝所选定的精确持久源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentCopySource {
    /// 源文档化身。
    pub id: DocumentId,
    /// 时间点。
    pub at: DocumentPoint,
}

/// 对应 `StoredDocument`：在选定点上物化出的值与存储的定义版本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredDocument {
    /// 化身记录。
    pub record: DocumentRecord,
    /// 定义版本。
    pub version: u32,
    /// 物化值。
    pub value: JsonObject,
    /// 选定 base 之后重放的增量数。
    pub deltas_since_base: usize,
}

// ─── 存储写入与提交变更 ──────────────────────────────────────────────────────

/// 对应 `StorageWrite`：一次原子存储提交中的一个记录或文档变更。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StorageWrite {
    /// 会话记录。
    Conversation(ConversationRecord),
    /// 条目记录。
    Entry(EntryRecord),
    /// 任务记录。
    Task(TaskRecord<JsonValue, JsonValue, JsonValue>),
    /// 提交记录。
    Submission(SubmissionRecord),
    /// 创建文档化身。
    DocumentCreate {
        /// 创建信息。
        record: DocumentCreate,
        /// 初始内容。
        content: DocumentBase,
    },
    /// 无定义文档拷贝。
    DocumentCopy {
        /// 创建信息。
        record: DocumentCreate,
        /// 拷贝源。
        source: DocumentCopySource,
    },
    /// 变更文档内容。
    DocumentChange {
        /// 目标化身。
        id: DocumentId,
        /// 新内容。
        content: DocumentContent,
    },
    /// 退役文档化身。
    DocumentRetire {
        /// 目标化身。
        id: DocumentId,
    },
}

/// 对应 `TableCommitChange`：不带其他发布副本而提交的完整表记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TableCommitChange {
    /// 会话记录。
    Conversation(ConversationRecord),
    /// 条目记录。
    Entry(EntryRecord),
    /// 任务记录。
    Task(TaskRecord<JsonValue, JsonValue, JsonValue>),
    /// 提交记录。
    Submission(SubmissionRecord),
}

impl StorageWrite {
    /// 若该写入是纯表记录，取出对应的 [`TableCommitChange`]。
    pub fn as_table_change(&self) -> Option<TableCommitChange> {
        match self {
            StorageWrite::Conversation(value) => Some(TableCommitChange::Conversation(*value)),
            StorageWrite::Entry(value) => Some(TableCommitChange::Entry(value.clone())),
            StorageWrite::Task(value) => Some(TableCommitChange::Task(value.clone())),
            StorageWrite::Submission(value) => Some(TableCommitChange::Submission(value.clone())),
            _ => None,
        }
    }
}

/// 对应 `DocumentCommitChange`：一个文档化身已提交的变更。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DocumentCommitChange {
    /// 普通更新（或创建/退役）。
    Document {
        /// 化身记录。
        record: DocumentRecord,
        /// 拥有该文档的会话；仅 Session 文档为 `None`。
        conversation_id: Option<ConversationId>,
        /// `value` 的定义版本；退役时为 `None`。
        version: Option<u32>,
        /// 采纳的精确不可变修订；退役时为 `None`。
        value: Option<JsonObject>,
        /// 采纳的操作；创建与退役时为空。
        ops: Vec<Op>,
    },
    /// 无定义子文档初始化。
    DocumentCopy {
        /// 化身记录。
        record: DocumentRecord,
        /// 所属会话。
        conversation_id: ConversationId,
        /// 拷贝源。
        source: DocumentCopySource,
    },
}

/// 对应 `CommitChange`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommitChange {
    /// 纯表记录变更。
    Table(TableCommitChange),
    /// 文档变更。
    Document(DocumentCommitChange),
}

/// 对应 `CommitPublication`：一次成功提交产生的全部不可变变更（顺序未指定）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitPublication {
    /// 提交序号。
    pub seq: Seq,
    /// 变更集合。
    pub changes: Vec<CommitChange>,
}

// ─── 文档观察接口 ────────────────────────────────────────────────────────────

/// 对应 `WatchEnd` 的非错误终止原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchReason {
    /// 对应 `stopped`。
    Stopped,
    /// 对应 `cancelled`。
    Cancelled,
    /// 对应 `session_closed`。
    SessionClosed,
    /// 对应 `retired`。
    Retired,
}

/// 对应 `WatchEnd`：一次文档观察的终止结果。
#[derive(Debug, Clone, PartialEq)]
pub enum WatchEnd {
    /// `stopped` / `cancelled` / `session_closed` / `retired`。
    Reason(WatchReason),
    /// 对应 `listener_error`。
    ListenerError(String),
}

/// 对应 `WatchHandle.closed` 的 promise 形态（可多次等待）。
pub type ClosedWatch = futures::future::Shared<futures::channel::oneshot::Receiver<WatchEnd>>;

/// 对应 `WatchHandle` 的监听者签名：`(value, ops, context) => Promise<void>`。
pub type WatchListener = std::sync::Arc<
    dyn Fn(
            JsonValue,
            Vec<Op>,
            std::sync::Arc<dyn crate::chord::context::Context>,
        ) -> futures::future::BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// 对应 `WatchHandle<T>`：对一个不可变值的有界、串行、精确帧观察。
///
/// 泛型 `T` 在 Rust 侧退化为 [`JsonValue`]（与 chord 的 `AttachedReplicatedState` 一致）。
pub trait WatchHandle: Send + Sync {
    /// 对应 `value`：启动前为获取时的修订，之后为最近投递的修订。
    fn value(&self) -> JsonValue;

    /// 对应 `start(listener)`：安装唯一异步监听者（绝不同步调用）。
    fn start(&self, listener: WatchListener);

    /// 对应 `stop()`：幂等地停止后续回调并返回终止结果。
    fn stop(&self) -> ClosedWatch;

    /// 对应 `closed`：在观察终止时 settle；已运行的回调仍由调用方负责。
    fn closed(&self) -> ClosedWatch;
}

/// 对应存储操作可能返回的错误。
#[derive(Debug, Clone, PartialEq)]
pub enum StorageError {
    /// 对应 `StorageRejected`：批次在任何持久化效果之前被拒。
    Rejected(crate::errors::StorageRejected),
    /// 存储已关闭，后续操作必须报错。
    Closed,
    /// 其他后端错误。
    Message(String),
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::Rejected(error) => write!(f, "{error}"),
            StorageError::Closed => write!(f, "storage is closed"),
            StorageError::Message(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<crate::errors::StorageRejected> for StorageError {
    fn from(error: crate::errors::StorageRejected) -> Self {
        StorageError::Rejected(error)
    }
}

/// 对应 `Storage`：Session 记录的原子持久化边界。
///
/// 存储**信任**拥有它的 Session 提供语义上有效的记录、引用、谱系与状态迁移；
/// 实现负责原子性、全局 ID 归属、不可变的会话/条目创建、文档记录一致性，以及脱离存储的值；
/// 提交的串行化由 Session 负责。
///
/// # 与上游的差异
///
/// 上游 `entry(...)` 是重载方法（全局查 / 按会话可见性查）；Rust 拆为 [`Storage::entry`] 与
/// [`Storage::entry_in_conversation`]。
#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    /// 原子地持久化一批写入并返回其序号。
    async fn commit(
        &self,
        writes: &[StorageWrite],
        context: &dyn crate::chord::context::Context,
    ) -> Result<Seq, StorageError>;

    /// 从 Session 全局的数值 ID 命名空间返回一个新的候选 ID。
    async fn mint_id(&self) -> u64;

    /// 按精确 ID 查一个会话。
    async fn conversation(
        &self,
        id: ConversationId,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Option<ConversationRecord>, StorageError>;

    /// 按 `query.order`（默认升序）扫会话。
    async fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Page<ConversationRecord, Cursor>, StorageError>;

    /// 查一个全局条目及其持久化提交序号。
    async fn entry(
        &self,
        id: EntryId,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Option<(EntryRecord, Seq)>, StorageError>;

    /// 仅当条目在所请求会话的谱系中可见时查它。
    async fn entry_in_conversation(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Option<(EntryRecord, Seq)>, StorageError>;

    /// 返回可选含上界下最新的带 `head` 的可见条目。
    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before_entry_id: Option<EntryId>,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Option<(EntryRecord, EntryId)>, StorageError>;

    /// 按 `query.order`（默认降序）扫描含端点的可见区间。
    async fn scan_entries(
        &self,
        query: EntryQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Page<EntryRecord, Cursor>, StorageError>;

    /// 查一个任务的最新完整记录。
    async fn task(
        &self,
        id: TaskId,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Option<TaskRecord<JsonValue, JsonValue, JsonValue>>, StorageError>;

    /// 按 `query.order`（默认升序）扫描匹配全部过滤器的任务记录。
    async fn scan_tasks(
        &self,
        query: TaskQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Page<TaskRecord<JsonValue, JsonValue, JsonValue>, Cursor>, StorageError>;

    /// 查一个已接纳提交的最新完整记录。
    async fn submission(
        &self,
        id: SubmissionId,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Option<SubmissionRecord>, StorageError>;

    /// 按 `query.order`（默认升序）扫描匹配的提交记录。
    async fn scan_submissions(
        &self,
        query: SubmissionQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Page<SubmissionRecord, Cursor>, StorageError>;

    /// 按会话内唯一的宿主去重键查提交。
    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Option<SubmissionRecord>, StorageError>;

    /// 解析选定点上占据某个精确逻辑地址的化身。
    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Option<DocumentRecord>, StorageError>;

    /// 按 ID 物化某个具体化身（不跟随其地址上的替换）。
    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Option<StoredDocument>, StorageError>;

    /// 扫描在某一精确作用域、某一点上存活的化身。
    async fn scan_documents(
        &self,
        query: DocumentQuery,
        limit: usize,
        cursor: Option<Cursor>,
        context: &dyn crate::chord::context::Context,
    ) -> Result<Page<DocumentRecord, Cursor>, StorageError>;

    /// 释放后端资源；之后所有操作必须报错。
    async fn close(&self, context: &dyn crate::chord::context::Context)
    -> Result<(), StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_conversation_id_is_one() {
        assert_eq!(ROOT_CONVERSATION_ID.get(), 1);
    }

    #[test]
    fn ids_serialise_as_plain_numbers() {
        let id: EntryId = Id::new(42);
        assert_eq!(serde_json::to_value(id).unwrap(), serde_json::json!(42));
        let decoded: EntryId = serde_json::from_value(serde_json::json!(42)).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn seq_advances_and_serialises_transparently() {
        assert_eq!(Seq::new(7).next(), Seq::new(8));
        assert_eq!(
            serde_json::to_value(Seq::new(7)).unwrap(),
            serde_json::json!(7)
        );
    }
}
