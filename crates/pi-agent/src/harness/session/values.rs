//! Rust 翻译自 packages/agent/src/harness/session/values.ts
//!
//! 类型化 value/list 存储地址与写操作。配置、lane 状态、operation 状态、pending 等
//! 均通过 value/list 地址持久化，替代旧的 model_change/thinking_level/active_tools entry。

use std::marker::PhantomData;

use pi_ai::utils::assistant_message_frame::AssistantMessageFrame;
use serde_json::Value as Json;

use crate::harness::session::types::{
    DurableStructuralPreparation, LaneConfiguration, LaneState, OperationMeta,
    OperationResultRecord, OperationState, PendingEntry,
};
use crate::types::AgentToolResult;

/// 对应 `StoredAddressBase`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAddressBase {
    pub namespace: String,
    pub key: String,
    pub kind: StoredAddressKind,
}

/// 对应 `kind: "value" | "list"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredAddressKind {
    Value,
    List,
}

/// 对应 `Value<T>`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value<T> {
    pub namespace: String,
    pub key: String,
    pub kind: StoredAddressKind,
    _marker: PhantomData<T>,
}

/// 对应 `ValueList<T>`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueList<T> {
    pub namespace: String,
    pub key: String,
    pub kind: StoredAddressKind,
    _marker: PhantomData<T>,
}

impl<T> Value<T> {
    /// 地址擦除：类型参数不影响运行时地址。
    pub fn erased(&self) -> Value<Json> {
        Value {
            namespace: self.namespace.clone(),
            key: self.key.clone(),
            kind: self.kind,
            _marker: PhantomData,
        }
    }
}

impl Value<Json> {
    /// 从已擦除地址重建强类型地址。
    pub fn retype<T>(&self) -> Value<T> {
        Value {
            namespace: self.namespace.clone(),
            key: self.key.clone(),
            kind: self.kind,
            _marker: PhantomData,
        }
    }
}

impl<T> ValueList<T> {
    /// 地址擦除。
    pub fn erased(&self) -> ValueList<Json> {
        ValueList {
            namespace: self.namespace.clone(),
            key: self.key.clone(),
            kind: self.kind,
            _marker: PhantomData,
        }
    }
}

impl ValueList<Json> {
    /// 从已擦除地址重建强类型地址。
    pub fn retype<T>(&self) -> ValueList<T> {
        ValueList {
            namespace: self.namespace.clone(),
            key: self.key.clone(),
            kind: self.kind,
            _marker: PhantomData,
        }
    }
}

/// 对应 `StoredValue<T>`。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredValue<T> {
    pub address: Value<T>,
    pub value: T,
    pub seq: u64,
}

/// 对应 `ListElement<T>`。
#[derive(Debug, Clone, PartialEq)]
pub struct ListElement<T> {
    pub seq: u64,
    pub value: T,
}

/// 对应 `ListCursor`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListCursor {
    pub seq: u64,
}

/// 对应 `ListReadOptions`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListReadOptions {
    pub cursor: Option<ListCursor>,
    pub order: Option<ListOrder>,
    pub limit: Option<usize>,
}

/// 对应 `order: "asc" | "desc"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOrder {
    Asc,
    Desc,
}

/// 对应 `ResolvedListReadOptions`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedListReadOptions {
    pub cursor: Option<ListCursor>,
    pub order: ListOrder,
    pub limit: usize,
}

/// 对应 `ValueSetWrite`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValueSetWrite {
    pub namespace: String,
    pub key: String,
    pub value: Json,
}

/// 对应 `ValueDeleteWrite`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValueDeleteWrite {
    pub namespace: String,
    pub key: String,
}

/// 对应 `ListAppendWrite`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListAppendWrite {
    pub namespace: String,
    pub key: String,
    pub value: Json,
}

/// 对应 `ListDeleteWrite`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListDeleteWrite {
    pub namespace: String,
    pub key: String,
}

/// 对应 `ValueWrite`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ValueWrite {
    Set(ValueSetWrite),
    Delete(ValueDeleteWrite),
}

/// 对应 `ListWrite`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ListWrite {
    Append(ListAppendWrite),
    Delete(ListDeleteWrite),
}

fn validate_address(namespace: &str, key: &str) {
    assert!(!namespace.is_empty(), "Value namespace must not be empty");
    assert!(
        !namespace.contains('\0'),
        "Value namespace must not contain \\u0000"
    );
    assert!(!key.contains('\0'), "Value key must not contain \\u0000");
}

/// 对应 `value<T>(namespace, key)`。
pub fn value<T>(namespace: &str, key: &str) -> Value<T> {
    validate_address(namespace, key);
    Value {
        namespace: namespace.to_string(),
        key: key.to_string(),
        kind: StoredAddressKind::Value,
        _marker: PhantomData,
    }
}

/// 对应 `list<T>(namespace, key)`。
pub fn list<T>(namespace: &str, key: &str) -> ValueList<T> {
    validate_address(namespace, key);
    ValueList {
        namespace: namespace.to_string(),
        key: key.to_string(),
        kind: StoredAddressKind::List,
        _marker: PhantomData,
    }
}

/// 对应 `setValue`。
pub fn set_value<T>(address: &Value<T>, next: Json) -> ValueWrite {
    ValueWrite::Set(ValueSetWrite {
        namespace: address.namespace.clone(),
        key: address.key.clone(),
        value: next,
    })
}

/// 对应 `deleteValue`。
pub fn delete_value<T>(address: &Value<T>) -> ValueWrite {
    ValueWrite::Delete(ValueDeleteWrite {
        namespace: address.namespace.clone(),
        key: address.key.clone(),
    })
}

/// 对应 `appendList`。
pub fn append_list<T>(address: &ValueList<T>, element: Json) -> ListWrite {
    ListWrite::Append(ListAppendWrite {
        namespace: address.namespace.clone(),
        key: address.key.clone(),
        value: element,
    })
}

/// 对应 `deleteList`。
pub fn delete_list<T>(address: &ValueList<T>) -> ListWrite {
    ListWrite::Delete(ListDeleteWrite {
        namespace: address.namespace.clone(),
        key: address.key.clone(),
    })
}

/// 对应 `resolveListReadOptions`。
pub fn resolve_list_read_options(options: ListReadOptions) -> ResolvedListReadOptions {
    let requested_limit = options.limit.unwrap_or(1_000);
    assert!(
        requested_limit > 0,
        "List read limit must be a positive integer"
    );
    ResolvedListReadOptions {
        cursor: options.cursor,
        order: options.order.unwrap_or(ListOrder::Asc),
        limit: requested_limit.min(10_000),
    }
}

// 预定义地址。类型与原版 `values.ts` 对齐：强类型地址（`Value<T>`）与读写操作的
// 类型参数在编译期约束，运行时仅依赖 namespace/key。

/// 对应 `branchTip`（`string | null`）。
pub fn branch_tip(branch: &str) -> Value<Option<String>> {
    value("pi.branch.tip", branch)
}

/// 对应 `branchTipInventoryPrefix`：扫描全部 branch tip 的 prefix。
pub fn branch_tip_inventory_prefix() -> Value<Option<String>> {
    value("pi.branch.tip", "")
}

/// 对应 `laneConfig`。
pub fn lane_config(lane: &str) -> Value<LaneConfiguration> {
    value("pi.lane.config", lane)
}

/// 对应 `laneState`。
pub fn lane_state(lane: &str) -> Value<LaneState> {
    value("pi.lane.state", lane)
}

/// 对应 `operationResult`。
pub fn operation_result(operation_id: &str) -> Value<OperationResultRecord> {
    value("pi.result", operation_id)
}

/// 对应 `operationMeta`。
pub fn operation_meta(operation_id: &str) -> Value<OperationMeta> {
    value("pi.op.meta", operation_id)
}

/// 对应 `operationState`。
pub fn operation_state(operation_id: &str) -> Value<OperationState> {
    value("pi.op.state", operation_id)
}

/// 对应 `operationToolMemo`。
pub fn operation_tool_memo(operation_id: &str, invocation_id: &str, name: &str) -> Value<Json> {
    value(
        "pi.op.tool_memo",
        &format!("{operation_id}:{invocation_id}:{name}"),
    )
}

/// 对应 `operationToolArgs`。
pub fn operation_tool_args(operation_id: &str, step_id: &str, source_index: usize) -> Value<Json> {
    value(
        "pi.op.tool_args",
        &format!("{operation_id}:{step_id}:{source_index}"),
    )
}

/// 对应 `operationToolArgsPrefix`。
pub fn operation_tool_args_prefix(operation_id: &str, step_id: Option<&str>) -> Value<Json> {
    let key = match step_id {
        None => format!("{operation_id}:"),
        Some(step_id) => format!("{operation_id}:{step_id}:"),
    };
    value("pi.op.tool_args", &key)
}

/// 对应 `operationToolMemoPrefix`。
pub fn operation_tool_memo_prefix(operation_id: &str, invocation_id: Option<&str>) -> Value<Json> {
    let key = match invocation_id {
        None => format!("{operation_id}:"),
        Some(invocation_id) => format!("{operation_id}:{invocation_id}:"),
    };
    value("pi.op.tool_memo", &key)
}

/// 对应 `operationPreparationPrefix`。
pub fn operation_preparation_prefix(operation_id: &str) -> Value<DurableStructuralPreparation> {
    value("pi.op.preparation", &format!("{operation_id}:"))
}

/// 对应 `operationPreparation`。
pub fn operation_preparation(
    operation_id: &str,
    task_id: &str,
) -> Value<DurableStructuralPreparation> {
    value("pi.op.preparation", &format!("{operation_id}:{task_id}"))
}

/// 对应 `pendingToolOutputPrefix`。
pub fn pending_tool_output_prefix(operation_id: &str) -> Value<AgentToolResult> {
    value("pi.pending.tool_output", &format!("{operation_id}:"))
}

/// 对应 `pendingToolOutput`。
pub fn pending_tool_output(operation_id: &str, invocation_id: &str) -> Value<AgentToolResult> {
    value(
        "pi.pending.tool_output",
        &format!("{operation_id}:{invocation_id}"),
    )
}

/// 对应 `pendingEntry`。
pub fn pending_entry(entry_id: &str) -> Value<PendingEntry> {
    value("pi.pending.entry", entry_id)
}

/// 对应 `pendingAssistantFrames`。
pub fn pending_assistant_frames(
    operation_id: &str,
    response_entry_id: &str,
) -> ValueList<AssistantMessageFrame> {
    list(
        "pi.pending.assistant_frame",
        &format!("{operation_id}:{response_entry_id}"),
    )
}

/// 对应 `sessionName`。
pub fn session_name() -> Value<String> {
    value("pi.session.name", "")
}

/// 对应 `entryLabel`。
pub fn entry_label(entry_id: &str) -> Value<String> {
    value("pi.entry.label", entry_id)
}
