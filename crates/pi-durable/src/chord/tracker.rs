//! 对应 chord `src/delta/tracker.ts` 的 `Tracker` / `Change` / `Prepared`。
//!
//! 负责把「对一份 JSON 值的一组编辑」变成可提交的 [`Op`] 序列，语义与上游一致：
//! `Prepared::value` 恒等于「把 `ops` 应用到 `base`」的结果。
//!
//! # 与上游的差异（语言机制所致，非简化）
//!
//! 1. **Proxy → 显式编辑方法**。上游 `Change::state` 是一个 `Proxy`，调用方像改普通对象
//!    一样改它，tracker 透明捕获读写并生成 Op。Rust 没有 Proxy，因此改为显式方法
//!    （[`Change::set`] / [`Change::delete`] / [`Change::append`] / [`Change::truncate`] /
//!    [`Change::splice`] / [`Change::move_items`]），产生的 Op 序列与最终值不变。
//! 2. **未移植 piece-tree 与密集区域启发式**。那是上游为超大数组降低 Op 数量的**性能优化**，
//!    不影响结果。本实现每次编辑产生一个 Op；Op 数超过 [`MAX_DELTA_OPERATIONS`] 时
//!    按上游语义退化为一次整值替换。
//! 3. **错误时机**。上游在 Proxy 上删除不存在的键也会先记录，直到 `apply` 才报错；
//!    这里在编辑时即通过 `apply_in_place` 校验并返回 [`DeltaError`]。

use serde_json::Value as JsonValue;

use super::delta::{DeltaError, Op, Path, apply_in_place};

/// 对应 `MAX_DELTA_OPERATIONS`：超过则退化为整值替换。
pub const MAX_DELTA_OPERATIONS: usize = 4_096;

/// 对应 `Prepared<T>`：一次已定稿的变更。
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    base: JsonValue,
    value: JsonValue,
    ops: Vec<Op>,
    base_revision: u64,
}

impl Prepared {
    /// 变更所基于的值（对应 `base`）。
    pub fn base(&self) -> &JsonValue {
        &self.base
    }

    /// 变更后的值（对应 `value`），恒等于 `base` 应用 `ops` 的结果。
    pub fn value(&self) -> &JsonValue {
        &self.value
    }

    /// 变更的操作序列（对应 `ops`）。
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// 对应 `baseRevision`。
    pub fn base_revision(&self) -> u64 {
        self.base_revision
    }

    /// 取出变更后的值。
    pub fn into_value(self) -> JsonValue {
        self.value
    }

    /// 取出「值 + 操作序列」。
    pub fn into_parts(self) -> (JsonValue, Vec<Op>) {
        (self.value, self.ops)
    }
}

/// 对应 `Change<T>`；`state`（Proxy）以显式编辑方法替代。
///
/// 每次编辑都立即作用于内部副本（因此后续读取能看到未提交的改动），同时追加一条 [`Op`]。
pub struct Change {
    base: JsonValue,
    current: JsonValue,
    ops: Vec<Op>,
    base_revision: u64,
    settled: bool,
}

impl Change {
    fn record(&mut self, op: Op) -> Result<(), DeltaError> {
        if self.settled {
            return Err(DeltaError::InvalidOp);
        }
        apply_in_place(&mut self.current, std::slice::from_ref(&op))?;
        self.ops.push(op);
        Ok(())
    }

    /// 当前值（含尚未提交的改动）。
    pub fn state(&self) -> &JsonValue {
        &self.current
    }

    /// 已累积的操作数。
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// 是否尚无改动。
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// 对应上游 `state[key] = value`（含末段为数组下标的形态）。
    pub fn set(&mut self, path: Path, value: JsonValue) -> Result<(), DeltaError> {
        self.record(Op::Set(path, value))
    }

    /// 对应上游 `delete state[key]`。
    pub fn delete(&mut self, path: Path) -> Result<(), DeltaError> {
        self.record(Op::Delete(path))
    }

    /// 对应上游 `state[key] += text` 的字符串拼接。
    pub fn append(&mut self, path: Path, text: impl Into<String>) -> Result<(), DeltaError> {
        self.record(Op::Append(path, text.into()))
    }

    /// 对应上游对字符串的截断（JS `slice(count)` 语义）。
    pub fn truncate(&mut self, path: Path, count: usize) -> Result<(), DeltaError> {
        self.record(Op::Truncate(path, count))
    }

    /// 对应上游的 `splice`（含 `push`/`pop`/`shift`/`unshift` 等数组变更）。
    pub fn splice(
        &mut self,
        path: Path,
        index: usize,
        delete_count: usize,
        items: Vec<JsonValue>,
    ) -> Result<(), DeltaError> {
        self.record(Op::Splice(path, index, delete_count, items))
    }

    /// 对应上游的数组重排（`reverse`/`sort`/`copyWithin` 等的归一化形态）。
    pub fn move_items(&mut self, path: Path, permutation: Vec<usize>) -> Result<(), DeltaError> {
        self.record(Op::Move(path, permutation))
    }

    /// 对应 `Change::prepare`：定稿并产出 [`Prepared`]。
    ///
    /// Op 数超过 [`MAX_DELTA_OPERATIONS`] 时按上游语义退化为一次整值替换。
    pub fn prepare(mut self) -> Prepared {
        self.settled = true;
        let ops = if self.ops.len() > MAX_DELTA_OPERATIONS {
            vec![Op::Replace(self.current.clone())]
        } else {
            std::mem::take(&mut self.ops)
        };
        Prepared {
            base: self.base,
            value: self.current,
            ops,
            base_revision: self.base_revision,
        }
    }

    /// 对应 `Change::abort`：丢弃本次变更。
    pub fn abort(mut self) {
        self.settled = true;
        self.ops.clear();
    }
}

/// 对应 `Tracker<T>`。
#[derive(Debug, Clone)]
pub struct Tracker {
    value: JsonValue,
    revision: u64,
}

impl Tracker {
    /// 对应 `track(initial)`。
    pub fn new(initial: JsonValue) -> Self {
        Self {
            value: initial,
            revision: 0,
        }
    }

    /// 当前已提交的值。
    pub fn value(&self) -> &JsonValue {
        &self.value
    }

    /// 对应 `revision`：每次 [`Tracker::adopt`] 递增。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// 对应 `beginChange`。
    pub fn begin_change(&self) -> Change {
        Change {
            base: self.value.clone(),
            current: self.value.clone(),
            ops: Vec::new(),
            base_revision: self.revision,
            settled: false,
        }
    }

    /// 对应 `prepareReplace`：不经过编辑，直接替换整值。
    ///
    /// 与当前值相同时产出空 op 列表（对应上游的 `replacementNoop`），避免无谓写入。
    pub fn prepare_replace(&self, value: JsonValue) -> Prepared {
        let ops = if value == self.value {
            Vec::new()
        } else {
            vec![Op::Replace(value.clone())]
        };
        Prepared {
            base: self.value.clone(),
            value,
            ops,
            base_revision: self.revision,
        }
    }

    /// 对应 `adopt`：把已定稿的变更提交为新的当前值，并使 `revision` 递增。
    pub fn adopt(&mut self, prepared: Prepared) {
        self.value = prepared.value;
        self.revision += 1;
    }
}

/// 对应 `track(initial)`。
pub fn track(initial: JsonValue) -> Tracker {
    Tracker::new(initial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(name: &str) -> super::super::delta::PathSegment {
        super::super::delta::PathSegment::Key(name.to_string())
    }

    #[test]
    fn prepared_value_equals_base_plus_ops() {
        let tracker = track(json!({ "a": 1, "list": [1, 2] }));
        let mut change = tracker.begin_change();
        change.set(vec![key("a")], json!(2)).unwrap();
        change
            .splice(vec![key("list")], 1, 0, vec![json!(9)])
            .unwrap();
        let prepared = change.prepare();

        let replayed =
            super::super::delta::apply_immutable(Some(prepared.base().clone()), prepared.ops())
                .unwrap();
        assert_eq!(
            &replayed,
            prepared.value(),
            "value 必须等于 base 应用 ops 的结果",
        );
        assert_eq!(prepared.value(), &json!({ "a": 2, "list": [1, 9, 2] }));
    }

    #[test]
    fn change_state_sees_uncommitted_edits() {
        let tracker = track(json!({ "a": 1 }));
        let mut change = tracker.begin_change();
        change.set(vec![key("a")], json!(5)).unwrap();
        assert_eq!(change.state(), &json!({ "a": 5 }));
        assert_eq!(
            tracker.value(),
            &json!({ "a": 1 }),
            "未 adopt 前 tracker 不变"
        );
    }

    #[test]
    fn adopt_commits_and_bumps_revision() {
        let mut tracker = track(json!({ "a": 1 }));
        let mut change = tracker.begin_change();
        change.set(vec![key("a")], json!(2)).unwrap();
        let prepared = change.prepare();
        assert_eq!(prepared.base_revision(), 0);

        tracker.adopt(prepared);

        assert_eq!(tracker.value(), &json!({ "a": 2 }));
        assert_eq!(tracker.revision(), 1);
    }

    #[test]
    fn abort_discards_edits() {
        let tracker = track(json!({ "a": 1 }));
        let mut change = tracker.begin_change();
        change.set(vec![key("a")], json!(2)).unwrap();
        change.abort();
        assert_eq!(tracker.value(), &json!({ "a": 1 }));
        assert_eq!(tracker.revision(), 0);
    }

    #[test]
    fn prepare_replace_reports_noop_when_unchanged() {
        let tracker = track(json!({ "a": 1 }));
        let same = tracker.prepare_replace(json!({ "a": 1 }));
        assert!(same.ops().is_empty(), "同值替换应产出空 op 列表");

        let different = tracker.prepare_replace(json!({ "a": 2 }));
        assert_eq!(different.ops(), &[Op::Replace(json!({ "a": 2 }))]);
        assert_eq!(different.value(), &json!({ "a": 2 }));
    }

    #[test]
    fn invalid_path_is_reported_at_edit_time() {
        let tracker = track(json!({ "a": 1 }));
        let mut change = tracker.begin_change();
        let error = change
            .set(vec![key("missing"), key("deep")], json!(1))
            .unwrap_err();
        assert_eq!(error, DeltaError::Path);
    }

    #[test]
    fn oversized_change_falls_back_to_replacement() {
        let tracker = track(json!({ "text": "" }));
        let mut change = tracker.begin_change();
        for _ in 0..=MAX_DELTA_OPERATIONS {
            change.append(vec![key("text")], "x").unwrap();
        }
        assert_eq!(change.len(), MAX_DELTA_OPERATIONS + 1);

        let prepared = change.prepare();

        assert_eq!(prepared.ops().len(), 1, "超过上限时应退化为单条 Replace");
        assert!(matches!(prepared.ops()[0], Op::Replace(_)));
        assert_eq!(
            prepared.value()["text"].as_str().unwrap().len(),
            MAX_DELTA_OPERATIONS + 1,
        );
    }
}
