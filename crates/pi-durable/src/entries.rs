//! 对应 `src/entries.ts`：内置条目种类。
//!
//! `defineEntry` 产出一个带 kind 与窄化守卫的条目类型 token；下列六个常量是 durable
//! 自己写入 transcript 的条目种类。
//!
//! 注：TS 的 `is()` 是类型守卫（`entry is TypedEntry<D>`）；Rust 无法在 trait/方法上表达
//! 类型窄化，因此 [`DefinedEntry::is`] 返回 `bool`，需要数据时再用 [`crate::types::EntryRecord::typed`] 解析。

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

use crate::types::EntryRecord;

/// 对应 `ToolDiagnostic`（定义在 `harness/types.ts`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDiagnostic {
    /// 严重级别。
    pub severity: DiagnosticSeverity,
    /// 诊断消息。
    pub message: String,
    /// 可选错误码。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// 对应 `severity: "info" | "warn" | "error"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    /// 信息。
    Info,
    /// 警告。
    Warn,
    /// 错误。
    Error,
}

/// 对应 `CompactionReason`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    /// 手动触发。
    Manual,
    /// 达到阈值。
    Threshold,
    /// 上下文溢出。
    Overflow,
}

/// `ToolResultEntry` 的 `data` 形态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultData {
    /// 结构化诊断（可能为空）。
    pub diagnostics: Vec<ToolDiagnostic>,
}

/// `CompactionEntry` 的 `data` 形态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionData {
    /// 触发原因。
    pub reason: CompactionReason,
}

/// 对应 `Entry<D>`：由 [`define_entry`] 产出的条目种类 token。
pub struct DefinedEntry<D = ()> {
    kind: &'static str,
    _marker: PhantomData<fn() -> D>,
}

impl<D> DefinedEntry<D> {
    /// 对应 `kind`。
    pub const fn kind(&self) -> &'static str {
        self.kind
    }

    /// 对应 `is(entry)`：判断记录是否属于本种类。
    pub fn is(&self, entry: Option<&EntryRecord>) -> bool {
        entry.is_some_and(|candidate| candidate.kind == self.kind)
    }
}

impl<D> Clone for DefinedEntry<D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D> Copy for DefinedEntry<D> {}

impl<D> std::fmt::Debug for DefinedEntry<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EntryKind({})", self.kind)
    }
}

/// 对应 `defineEntry`：定义一个带 kind 的类型化条目种类。
pub const fn define_entry<D>(kind: &'static str) -> DefinedEntry<D> {
    assert!(!kind.is_empty(), "Entry kind must be a non-empty string");
    DefinedEntry {
        kind,
        _marker: PhantomData,
    }
}

/// 对应 `UserEntry`：用户输入，`model` 为 `[UserMessage]`，由提交写入。
pub const USER_ENTRY: DefinedEntry = define_entry("pi.user");
/// 对应 `AssistantEntry`：provider 结果，`model` 为 `[AssistantMessage]`，由 generation 写入。
pub const ASSISTANT_ENTRY: DefinedEntry = define_entry("pi.assistant");
/// 对应 `SystemEntry`：位置性提示与工具变更，`model` 为内容为空的 `[SystemMessage]`。
pub const SYSTEM_ENTRY: DefinedEntry = define_entry("pi.system");
/// 对应 `ToolResultEntry`：`data` 持有结构化诊断（可能为空）。
pub const TOOL_RESULT_ENTRY: DefinedEntry<ToolResultData> = define_entry("pi.tool-result");
/// 对应 `ResetEntry`：新上下文的起点，`head` 恒为 `"self"`。
pub const RESET_ENTRY: DefinedEntry = define_entry("pi.reset");
/// 对应 `CompactionEntry`：压缩摘要，`head` 为首个保留条目。
pub const COMPACTION_ENTRY: DefinedEntry<CompactionData> = define_entry("pi.compaction");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ConversationId, EntryId};

    fn record(kind: &str) -> EntryRecord {
        EntryRecord {
            id: EntryId::new(1),
            conversation_id: ConversationId::new(1),
            kind: kind.to_string(),
            model: None,
            data: None,
            head: None,
            edits: None,
            by_task_id: None,
        }
    }

    #[test]
    fn builtin_kinds_match_upstream_strings() {
        assert_eq!(USER_ENTRY.kind(), "pi.user");
        assert_eq!(ASSISTANT_ENTRY.kind(), "pi.assistant");
        assert_eq!(SYSTEM_ENTRY.kind(), "pi.system");
        assert_eq!(TOOL_RESULT_ENTRY.kind(), "pi.tool-result");
        assert_eq!(RESET_ENTRY.kind(), "pi.reset");
        assert_eq!(COMPACTION_ENTRY.kind(), "pi.compaction");
    }

    #[test]
    fn is_narrows_by_kind() {
        let entry = record("pi.user");
        assert!(USER_ENTRY.is(Some(&entry)));
        assert!(!ASSISTANT_ENTRY.is(Some(&entry)));
        assert!(!USER_ENTRY.is(None));
    }

    #[test]
    fn typed_data_round_trips_through_json() {
        let mut entry = record("pi.tool-result");
        entry.data = Some(serde_json::json!({
            "diagnostics": [{ "severity": "warn", "message": "careful", "code": "W1" }]
        }));

        let typed: crate::types::TypedEntry<ToolResultData> = entry.typed().unwrap();
        assert_eq!(typed.data.diagnostics.len(), 1);
        assert_eq!(typed.data.diagnostics[0].severity, DiagnosticSeverity::Warn);
        assert_eq!(typed.data.diagnostics[0].code.as_deref(), Some("W1"));
    }

    #[test]
    fn compaction_reason_serialises_lowercase() {
        let data = CompactionData {
            reason: CompactionReason::Threshold,
        };
        assert_eq!(
            serde_json::to_value(&data).unwrap(),
            serde_json::json!({ "reason": "threshold" }),
        );
    }
}
