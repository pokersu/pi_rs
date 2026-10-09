//! 对应 chord `src/delta/index.ts` 的 `Op` / `apply` 系列与 `src/delta/apply-immutable-trusted.ts`。
//!
//! `Op` 是「不可变状态的最小变更」描述；上游以 JSON 数组元组表示，内存/线上/磁盘同形。
//! 本模块实现其中最被 `pi-durable` 使用的部分：
//!
//! - 类型：`PathSegment`（= `Seg`）、`Path`、`WireOp`
//! - 构造与读取：`apply`（原地）、`apply_immutable`、`apply_immutable_batches`（写时复制）
//! - wire 编解码：`encoder()`/`decoder()`（跨批次的路径 interning 压缩）
//!
//! 未移植（留待后续阶段）：`assertValidOp`/`assertValidWireOp` 的完整校验、`diff`（Op 的生产端）。
//!
//! # 与上游的差异
//!
//! - 上游以异常报错，这里返回 [`DeltaError`]。
//! - 上游用 `Object.defineProperty` 绕开原型链上的 setter、并显式拒绝 `__proto__` 等保留键；
//!   Rust 的 `serde_json::Map` 没有原型链，天然免疫，因此不设保留键检查。
//! - 上游 `applyImmutable` 用 `WeakSet` 做路径复制去重；这里每次沿路径重建容器，
//!   语义等价（不改动输入），实现更直接。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize, de, ser::SerializeSeq};
use serde_json::Value as JsonValue;

/// 对应 `Seg`：对象键或数组下标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

/// 对应 `Path`。
pub type Path = Vec<PathSegment>;

/// 对应 `Op`（JSON 元组形式见各变体注释）。
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// `["r", value]`：整值替换（唯一可作用于根的「写」操作）。
    Replace(JsonValue),
    /// `["s", path, value]`
    Set(Path, JsonValue),
    /// `["d", path]`
    Delete(Path),
    /// `["a", path, text]`：字符串拼接。
    Append(Path, String),
    /// `["t", path, count]`：JS `String.prototype.slice(count)` 语义，
    /// 保留从第 `count` 个 UTF-16 码元起的内容。
    Truncate(Path, usize),
    /// `["p", path, index, deleteCount, items]`：数组替换。
    Splice(Path, usize, usize, Vec<JsonValue>),
    /// `["m", path, permutation]`：按排列重排数组，`new[i] = old[permutation[i]]`。
    Move(Path, Vec<usize>),
}

/// 对应上游的 `PathError` / `UnsafePathError` / `assertValidOp` 失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaError {
    /// 路径不存在或结构不符（`PathError`）。
    Path,
    /// 数组上出现非数字段，或对象上出现数字段（`UnsafePathError`）。
    UnsafePath(PathSegment),
    /// 需要非空路径的操作收到空路径（`s`/`d`/`a`/`t` 不可作用于根）。
    EmptyPath,
    /// op 元组结构非法。
    InvalidOp,
}

impl std::fmt::Display for DeltaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeltaError::Path => write!(f, "path does not resolve"),
            DeltaError::UnsafePath(segment) => write!(f, "unsafe path segment: {segment:?}"),
            DeltaError::EmptyPath => write!(f, "operation requires a non-empty path"),
            DeltaError::InvalidOp => write!(f, "invalid operation"),
        }
    }
}

impl std::error::Error for DeltaError {}

// ─── Serde（按上游的 JSON 元组形式） ─────────────────────────────────────────

impl Serialize for Op {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Op::Replace(value) => {
                let mut seq = serializer.serialize_seq(Some(2))?;
                seq.serialize_element("r")?;
                seq.serialize_element(value)?;
                seq.end()
            }
            Op::Set(path, value) => {
                let mut seq = serializer.serialize_seq(Some(3))?;
                seq.serialize_element("s")?;
                seq.serialize_element(path)?;
                seq.serialize_element(value)?;
                seq.end()
            }
            Op::Delete(path) => {
                let mut seq = serializer.serialize_seq(Some(2))?;
                seq.serialize_element("d")?;
                seq.serialize_element(path)?;
                seq.end()
            }
            Op::Append(path, text) => {
                let mut seq = serializer.serialize_seq(Some(3))?;
                seq.serialize_element("a")?;
                seq.serialize_element(path)?;
                seq.serialize_element(text)?;
                seq.end()
            }
            Op::Truncate(path, count) => {
                let mut seq = serializer.serialize_seq(Some(3))?;
                seq.serialize_element("t")?;
                seq.serialize_element(path)?;
                seq.serialize_element(count)?;
                seq.end()
            }
            Op::Splice(path, index, delete_count, items) => {
                let mut seq = serializer.serialize_seq(Some(5))?;
                seq.serialize_element("p")?;
                seq.serialize_element(path)?;
                seq.serialize_element(index)?;
                seq.serialize_element(delete_count)?;
                seq.serialize_element(items)?;
                seq.end()
            }
            Op::Move(path, permutation) => {
                let mut seq = serializer.serialize_seq(Some(3))?;
                seq.serialize_element("m")?;
                seq.serialize_element(path)?;
                seq.serialize_element(permutation)?;
                seq.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Op {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Vec::<JsonValue>::deserialize(deserializer)?;
        let tag = raw
            .first()
            .and_then(JsonValue::as_str)
            .ok_or_else(|| de::Error::custom("op 缺少字符串 tag"))?;

        let segment = |index: usize| -> Result<&JsonValue, D::Error> {
            raw.get(index)
                .ok_or_else(|| de::Error::custom(format!("op `{tag}` 缺少第 {index} 项")))
        };
        let parse_path = |index: usize| -> Result<Path, D::Error> {
            serde_json::from_value(segment(index)?.clone())
                .map_err(|e| de::Error::custom(format!("op `{tag}` 路径非法: {e}")))
        };
        let parse_usize = |index: usize| -> Result<usize, D::Error> {
            segment(index)?
                .as_u64()
                .map(|n| n as usize)
                .ok_or_else(|| de::Error::custom(format!("op `{tag}` 第 {index} 项应为非负整数")))
        };

        match tag {
            "r" => Ok(Op::Replace(segment(1)?.clone())),
            "s" => Ok(Op::Set(parse_path(1)?, segment(2)?.clone())),
            "d" => Ok(Op::Delete(parse_path(1)?)),
            "a" => {
                let text = segment(2)?
                    .as_str()
                    .ok_or_else(|| de::Error::custom("op `a` 第 2 项应为字符串"))?;
                Ok(Op::Append(parse_path(1)?, text.to_string()))
            }
            "t" => Ok(Op::Truncate(parse_path(1)?, parse_usize(2)?)),
            "p" => {
                let items = segment(4)?
                    .as_array()
                    .ok_or_else(|| de::Error::custom("op `p` 第 4 项应为数组"))?
                    .clone();
                Ok(Op::Splice(
                    parse_path(1)?,
                    parse_usize(2)?,
                    parse_usize(3)?,
                    items,
                ))
            }
            "m" => {
                let permutation = segment(2)?
                    .as_array()
                    .ok_or_else(|| de::Error::custom("op `m` 第 2 项应为数组"))?
                    .iter()
                    .map(|item| {
                        item.as_u64()
                            .map(|n| n as usize)
                            .ok_or_else(|| de::Error::custom("op `m` 排列项应为非负整数"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Op::Move(parse_path(1)?, permutation))
            }
            other => Err(de::Error::custom(format!("未知 op tag: {other}"))),
        }
    }
}

// ─── 路径解析 ────────────────────────────────────────────────────────────────

fn resolve_mut<'a>(
    mut node: &'a mut JsonValue,
    path: &[PathSegment],
) -> Result<&'a mut JsonValue, DeltaError> {
    for segment in path {
        node = match segment {
            PathSegment::Key(key) => node
                .as_object_mut()
                .ok_or(DeltaError::Path)?
                .get_mut(key)
                .ok_or(DeltaError::Path)?,
            PathSegment::Index(index) => node
                .as_array_mut()
                .ok_or(DeltaError::UnsafePath(segment.clone()))?
                .get_mut(*index)
                .ok_or(DeltaError::Path)?,
        };
    }
    Ok(node)
}

/// 取「父容器 + 末段」，用于需要写入/删除末段的操作。
fn resolve_parent_mut<'a>(
    root: &'a mut JsonValue,
    path: &'a [PathSegment],
) -> Result<(&'a mut JsonValue, &'a PathSegment), DeltaError> {
    let (last, parent_path) = path.split_last().ok_or(DeltaError::EmptyPath)?;
    let parent = resolve_mut(root, parent_path)?;
    Ok((parent, last))
}

/// 取末段对应的可变槽位（要求已存在）。
fn slot_mut<'a>(
    parent: &'a mut JsonValue,
    segment: &PathSegment,
) -> Result<&'a mut JsonValue, DeltaError> {
    match segment {
        PathSegment::Key(key) => parent
            .as_object_mut()
            .ok_or(DeltaError::Path)?
            .get_mut(key)
            .ok_or(DeltaError::Path),
        PathSegment::Index(index) => parent
            .as_array_mut()
            .ok_or(DeltaError::UnsafePath(segment.clone()))?
            .get_mut(*index)
            .ok_or(DeltaError::Path),
    }
}

/// 对应 JS `String.prototype.slice(n)`：从第 `n` 个 **UTF-16 码元**开始截到末尾。
///
/// 不能用 `char_indices`：JS 以 UTF-16 计长，含增补平面字符（emoji 等）时两者含义不同。
fn slice_utf16_from(value: &str, from: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    let start = from.min(units.len());
    String::from_utf16_lossy(&units[start..])
}

// ─── apply ───────────────────────────────────────────────────────────────────

fn apply_one(root: &mut JsonValue, op: &Op) -> Result<(), DeltaError> {
    match op {
        Op::Replace(value) => {
            *root = value.clone();
            Ok(())
        }
        Op::Splice(path, index, delete_count, items) => {
            let array = resolve_mut(root, path)?
                .as_array_mut()
                .ok_or(DeltaError::Path)?;
            let start = (*index).min(array.len());
            let removed = (*delete_count).min(array.len() - start);
            array.splice(start..start + removed, items.iter().cloned());
            Ok(())
        }
        Op::Move(path, permutation) => {
            let array = resolve_mut(root, path)?
                .as_array_mut()
                .ok_or(DeltaError::Path)?;
            if array.len() != permutation.len() {
                return Err(DeltaError::Path);
            }
            let previous = array.clone();
            for (position, source) in permutation.iter().enumerate() {
                array[position] = previous.get(*source).cloned().ok_or(DeltaError::Path)?;
            }
            Ok(())
        }
        Op::Set(path, value) => {
            let (parent, last) = resolve_parent_mut(root, path)?;
            match last {
                PathSegment::Key(key) => {
                    parent
                        .as_object_mut()
                        .ok_or(DeltaError::Path)?
                        .insert(key.clone(), value.clone());
                }
                PathSegment::Index(index) => {
                    let slot = parent
                        .as_array_mut()
                        .ok_or(DeltaError::UnsafePath(last.clone()))?
                        .get_mut(*index)
                        .ok_or(DeltaError::Path)?;
                    *slot = value.clone();
                }
            }
            Ok(())
        }
        Op::Delete(path) => {
            let (parent, last) = resolve_parent_mut(root, path)?;
            match last {
                PathSegment::Key(key) => {
                    if let Some(map) = parent.as_object_mut() {
                        map.remove(key);
                        Ok(())
                    } else {
                        Err(DeltaError::Path)
                    }
                }
                PathSegment::Index(index) => {
                    let array = parent
                        .as_array_mut()
                        .ok_or(DeltaError::UnsafePath(last.clone()))?;
                    if *index >= array.len() {
                        return Err(DeltaError::Path);
                    }
                    array.remove(*index);
                    Ok(())
                }
            }
        }
        Op::Append(path, text) => {
            let (parent, last) = resolve_parent_mut(root, path)?;
            let slot = slot_mut(parent, last)?;
            let current = slot.as_str().ok_or(DeltaError::Path)?;
            *slot = JsonValue::String(format!("{current}{text}"));
            Ok(())
        }
        Op::Truncate(path, count) => {
            let (parent, last) = resolve_parent_mut(root, path)?;
            let slot = slot_mut(parent, last)?;
            let current = slot.as_str().ok_or(DeltaError::Path)?;
            *slot = JsonValue::String(slice_utf16_from(current, *count));
            Ok(())
        }
    }
}

/// 对应 `overlap`：`a` 的最长后缀同时也是 `b` 的前缀长度。
///
/// 用于文本窗口的增量重叠检测（如工具输出的滚动窗口）。按 **UTF-16 码元**计算，
/// 与 JS 的 `String.prototype.indexOf`/`slice` 一致。
///
/// 上游先用长探针（命中少）再回退到 1 字符；候选数超过 `max_candidates` 时放弃
/// （返回 0，结果为「更大但不会错」的窗口）。
pub fn overlap(a: &str, b: &str, scan: usize, probe: usize, max_candidates: usize) -> usize {
    let a: Vec<u16> = a.encode_utf16().collect();
    let b: Vec<u16> = b.encode_utf16().collect();
    if a.is_empty() || b.is_empty() || scan == 0 {
        return 0;
    }
    let tail = if a.len() > scan {
        &a[a.len() - scan..]
    } else {
        &a[..]
    };

    for head_length in [probe.min(b.len()), 1] {
        if head_length == 0 {
            continue;
        }
        let head = &b[..head_length];
        let mut tried = 0usize;
        for start in 0..=tail.len().saturating_sub(head_length) {
            // 探针比 tail 还长时 `indexOf` 会直接落空（这里等价于跳过）。
            let Some(window) = tail.get(start..start + head_length) else {
                break;
            };
            if window != head {
                continue;
            }
            tried += 1;
            if tried > max_candidates {
                break;
            }
            let length = tail.len() - start;
            if length <= b.len() && tail[start..] == b[..length] {
                return length;
            }
        }
        if head_length == 1 {
            break;
        }
    }
    0
}

/// 对应 `apply`：在给定值上原地应用操作序列。
///
/// 上游注释明确「`r` 的 value 是被 adopt 而非 copy」；这里的 `apply` 同样 clone 输入，
/// 因此调用方拿到的返回值与传入值相互独立（Rust 无共享可变引用语义）。
pub fn apply(target: Option<JsonValue>, ops: &[Op]) -> Result<JsonValue, DeltaError> {
    let mut root = target.unwrap_or(JsonValue::Null);
    apply_in_place(&mut root, ops)?;
    Ok(root)
}

/// 在已有值上原地应用操作（[`apply`] 的非克隆变体）。
pub fn apply_in_place(root: &mut JsonValue, ops: &[Op]) -> Result<(), DeltaError> {
    for op in ops {
        apply_one(root, op)?;
    }
    Ok(())
}

/// 沿路径重建容器（写时复制），返回与输入共享其余子树的新根。
fn copy_along(root: &JsonValue, path: &[PathSegment]) -> Result<JsonValue, DeltaError> {
    let Some((first, rest)) = path.split_first() else {
        return Ok(root.clone());
    };
    match first {
        PathSegment::Key(key) => {
            let map = root.as_object().ok_or(DeltaError::Path)?;
            let child = map.get(key).ok_or(DeltaError::Path)?;
            let copied = copy_along(child, rest)?;
            let mut next = map.clone();
            next.insert(key.clone(), copied);
            Ok(JsonValue::Object(next))
        }
        PathSegment::Index(index) => {
            let array = root
                .as_array()
                .ok_or(DeltaError::UnsafePath(first.clone()))?;
            let child = array.get(*index).ok_or(DeltaError::Path)?;
            let copied = copy_along(child, rest)?;
            let mut next = array.clone();
            next[*index] = copied;
            Ok(JsonValue::Array(next))
        }
    }
}

/// 操作需要复制的路径：`p`/`m` 作用于路径本身（可为根），其余作用于其父。
fn copy_target_path(op: &Op) -> &[PathSegment] {
    match op {
        Op::Replace(_) => &[],
        Op::Splice(path, ..) | Op::Move(path, ..) => path,
        Op::Set(path, _) | Op::Delete(path) | Op::Append(path, _) | Op::Truncate(path, _) => {
            path.split_last().map(|(_, head)| head).unwrap_or(&[])
        }
    }
}

/// 对应 `applyImmutable`：应用一批操作，且不修改传入的旧值。
pub fn apply_immutable(target: Option<JsonValue>, ops: &[Op]) -> Result<JsonValue, DeltaError> {
    apply_immutable_batches(target, std::iter::once(ops))
}

/// 对应 `applyImmutableBatches`：把多批操作当作一次「只保留最终结果」的重放。
pub fn apply_immutable_batches<'a, I>(
    target: Option<JsonValue>,
    batches: I,
) -> Result<JsonValue, DeltaError>
where
    I: IntoIterator<Item = &'a [Op]>,
{
    let mut root = target.unwrap_or(JsonValue::Null);
    for batch in batches {
        for op in batch {
            if let Op::Replace(value) = op {
                root = value.clone();
                continue;
            }
            root = copy_along(&root, copy_target_path(op))?;
            apply_one(&mut root, op)?;
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(name: &str) -> PathSegment {
        PathSegment::Key(name.to_string())
    }

    fn index(value: usize) -> PathSegment {
        PathSegment::Index(value)
    }

    #[test]
    fn replace_swaps_the_whole_value() {
        let result = apply(Some(json!({ "a": 1 })), &[Op::Replace(json!([1, 2]))]).unwrap();
        assert_eq!(result, json!([1, 2]));
    }

    #[test]
    fn set_updates_nested_object_key() {
        let ops = [Op::Set(vec![key("a"), key("b")], json!("new"))];
        let result = apply(Some(json!({ "a": { "b": "old", "c": 1 } })), &ops).unwrap();
        assert_eq!(result, json!({ "a": { "b": "new", "c": 1 } }));
    }

    #[test]
    fn set_rejects_out_of_range_index() {
        let ops = [Op::Set(vec![key("list"), index(5)], json!(1))];
        let error = apply(Some(json!({ "list": [1, 2] })), &ops).unwrap_err();
        assert_eq!(error, DeltaError::Path);
    }

    #[test]
    fn delete_removes_object_key_and_array_element() {
        let ops = [
            Op::Delete(vec![key("a")]),
            Op::Delete(vec![key("list"), index(0)]),
        ];
        let result = apply(Some(json!({ "a": 1, "list": [10, 20] })), &ops).unwrap();
        assert_eq!(result, json!({ "list": [20] }));
    }

    /// `t` 是 `String.prototype.slice(n)` 语义：从第 n 个码元截到末尾。
    #[test]
    fn append_and_truncate_operate_on_strings() {
        let ops = [
            Op::Append(vec![key("s")], "-tail".to_string()),
            Op::Truncate(vec![key("s")], 4),
        ];
        let result = apply(Some(json!({ "s": "head" })), &ops).unwrap();
        assert_eq!(
            result["s"],
            json!("-tail"),
            "`head-tail`.slice(4) 应为 `-tail`"
        );
    }

    #[test]
    fn truncate_uses_utf16_code_units() {
        // "😀" 占 2 个 UTF-16 码元（1 个 char）：[0]=高代理 [1]=低代理 [2]='a' [3]='b' [4]='c'
        // 因此 slice(3) === "bc"；若按 char 计数会错得 "abc"。
        let result = apply(
            Some(json!({ "s": "😀abc" })),
            &[Op::Truncate(vec![key("s")], 3)],
        )
        .unwrap();
        assert_eq!(result["s"], json!("bc"));

        let result = apply(
            Some(json!({ "s": "😀abc" })),
            &[Op::Truncate(vec![key("s")], 2)],
        )
        .unwrap();
        assert_eq!(result["s"], json!("abc"));
    }

    #[test]
    fn splice_replaces_array_window() {
        let ops = [Op::Splice(
            vec![key("list")],
            1,
            2,
            vec![json!("x"), json!("y")],
        )];
        let result = apply(Some(json!({ "list": [1, 2, 3, 4] })), &ops).unwrap();
        assert_eq!(result["list"], json!([1, "x", "y", 4]));
    }

    #[test]
    fn move_reorders_array_by_permutation() {
        let ops = [Op::Move(vec![key("list")], vec![2, 0, 1])];
        let result = apply(Some(json!({ "list": ["a", "b", "c"] })), &ops).unwrap();
        assert_eq!(result["list"], json!(["c", "a", "b"]));
    }

    #[test]
    fn move_rejects_length_mismatch() {
        let ops = [Op::Move(vec![key("list")], vec![0])];
        assert_eq!(
            apply(Some(json!({ "list": [1, 2] })), &ops).unwrap_err(),
            DeltaError::Path,
        );
    }

    #[test]
    fn root_splice_is_allowed() {
        let ops = [Op::Splice(Vec::new(), 1, 1, vec![json!("z")])];
        let result = apply(Some(json!(["a", "b", "c"])), &ops).unwrap();
        assert_eq!(result, json!(["a", "z", "c"]));
    }

    #[test]
    fn set_on_root_is_rejected() {
        let ops = [Op::Set(Vec::new(), json!(1))];
        assert_eq!(
            apply(Some(json!({ "a": 1 })), &ops).unwrap_err(),
            DeltaError::EmptyPath,
        );
    }

    #[test]
    fn apply_immutable_leaves_input_untouched() {
        let original = json!({ "a": { "b": "old" } });
        let ops = [Op::Set(vec![key("a"), key("b")], json!("new"))];

        let result = apply_immutable(Some(original.clone()), &ops).unwrap();

        assert_eq!(original["a"]["b"], json!("old"), "输入必须保持不变");
        assert_eq!(result["a"]["b"], json!("new"));
    }

    #[test]
    fn apply_immutable_batches_replays_in_order() {
        let batch_one = [Op::Set(vec![key("a")], json!(1))];
        let batch_two = [
            Op::Set(vec![key("a")], json!(2)),
            Op::Set(vec![key("b")], json!(3)),
        ];

        let result = apply_immutable_batches(
            Some(json!({ "a": 0 })),
            [batch_one.as_slice(), batch_two.as_slice()],
        )
        .unwrap();

        assert_eq!(result, json!({ "a": 2, "b": 3 }));
    }

    #[test]
    fn overlap_finds_longest_suffix_prefix() {
        assert_eq!(overlap("abcabc", "abcdef", 64, 64, 8), 3, "\"abc\" 重叠");
        assert_eq!(overlap("xyz", "abc", 64, 64, 8), 0);
        assert_eq!(overlap("", "abc", 64, 64, 8), 0);
        assert_eq!(overlap("abc", "", 64, 64, 8), 0);
        assert_eq!(overlap("abc", "abc", 0, 64, 8), 0, "scan 为 0 时不探测");
    }

    #[test]
    fn overlap_respects_scan_window() {
        // scan 限制 `a` 侧参与比对的尾部长度：
        // scan=2 → tail="ef"，与 "defghi" 的任何前缀都不重叠 → 0
        assert_eq!(overlap("abcdef", "defghi", 2, 64, 8), 0);
        // scan=3 → tail="def"，恰好是 "defghi" 的前缀 → 3
        assert_eq!(overlap("abcdef", "defghi", 3, 64, 8), 3);
    }

    #[test]
    fn op_serialises_as_upstream_tuples() {
        let cases = [
            (Op::Replace(json!({ "a": 1 })), json!(["r", { "a": 1 }])),
            (
                Op::Set(vec![key("a"), index(0)], json!("v")),
                json!(["s", ["a", 0], "v"]),
            ),
            (Op::Delete(vec![key("a")]), json!(["d", ["a"]])),
            (
                Op::Append(vec![key("a")], "x".to_string()),
                json!(["a", ["a"], "x"]),
            ),
            (Op::Truncate(vec![key("a")], 3), json!(["t", ["a"], 3])),
            (
                Op::Splice(vec![key("a")], 1, 2, vec![json!("z")]),
                json!(["p", ["a"], 1, 2, ["z"]]),
            ),
            (
                Op::Move(vec![key("a")], vec![1, 0]),
                json!(["m", ["a"], [1, 0]]),
            ),
        ];

        for (op, expected) in cases {
            let encoded = serde_json::to_value(&op).unwrap();
            assert_eq!(encoded, expected, "序列化形态应与上游元组一致");

            let decoded: Op = serde_json::from_value(expected).unwrap();
            assert_eq!(decoded, op, "反序列化应还原同一个 op");
        }
    }

    #[test]
    fn wire_roundtrip_preserves_ops() {
        let ops = vec![
            Op::Replace(json!({ "root": true })),
            Op::Set(vec![key("a")], json!(1)),
            Op::Set(vec![key("a")], json!(2)),
            Op::Append(vec![key("text")], "x".to_string()),
            Op::Splice(vec![key("items"), index(0)], 0, 0, vec![json!(1), json!(2)]),
            Op::Move(vec![key("items")], vec![1, 0]),
            Op::Delete(vec![key("gone")]),
            Op::Truncate(vec![key("text")], 2),
            Op::Set(vec![key("a")], json!(3)),
        ];
        let mut encoder = encoder();
        let wire = encoder.encode(&ops);
        let mut decoder = decoder();
        let decoded = decoder.decode(&wire).unwrap();
        assert_eq!(decoded, ops, "wire 往返应还原原始 op");
    }

    #[test]
    fn wire_encoder_interns_on_second_use() {
        // 不连续的同路径才触发 interning；连续同路径走 short form。
        let ops = vec![
            Op::Set(vec![key("a")], json!(1)),
            Op::Set(vec![key("b")], json!(2)),
            Op::Set(vec![key("a")], json!(3)),
            Op::Set(vec![key("b")], json!(4)),
            Op::Set(vec![key("a")], json!(5)),
        ];
        let mut encoder = encoder();
        let wire = encoder.encode(&ops);
        // a/b 首次内联、a 第二次定义 id=0、b 第二次定义 id=1、a 第三次引用 id=0。
        assert_eq!(wire[0], json!(["s", ["a"], 1]));
        assert_eq!(wire[1], json!(["s", ["b"], 2]));
        assert_eq!(wire[2], json!(["#", 0, ["a"]]));
        assert_eq!(wire[3], json!(["s", 0, 3]));
        assert_eq!(wire[4], json!(["#", 1, ["b"]]));
        assert_eq!(wire[5], json!(["s", 1, 4]));
        assert_eq!(wire[6], json!(["s", 0, 5]));
    }

    #[test]
    fn wire_encoder_short_forms_consecutive_paths() {
        let ops = vec![
            Op::Set(vec![key("a")], json!(1)),
            Op::Set(vec![key("a")], json!(2)),
            Op::Set(vec![key("a")], json!(3)),
        ];
        let mut encoder = encoder();
        let wire = encoder.encode(&ops);
        assert_eq!(wire[0], json!(["s", ["a"], 1]));
        assert_eq!(wire[1], json!(["s", 2]));
        assert_eq!(wire[2], json!(["s", 3]));
    }
}

// ─── Wire 编解码（路径 interning） ──────────────────────────────────────────

/// 对应 `WireOp`：线上压缩 op（JSON 元组）。与上游一致，用 JSON 数组表示。
pub type WireOp = JsonValue;

/// 对应 `pathKey`：路径的规范化键（JSON 字符串）。
fn path_key(path: &Path) -> String {
    serde_json::to_string(path).unwrap_or_default()
}

/// 对应 `assertSafePath`：拒绝原型链保留段。
fn assert_safe_path(path: &Path) -> Result<(), DeltaError> {
    for segment in path {
        if let PathSegment::Key(key) = segment
            && matches!(key.as_str(), "__proto__" | "constructor" | "prototype")
        {
            return Err(DeltaError::UnsafePath(segment.clone()));
        }
    }
    Ok(())
}

/// 提取 op 的路径（`Replace` 除外）。
fn wire_op_path(op: &Op) -> &Path {
    match op {
        Op::Set(path, _)
        | Op::Delete(path)
        | Op::Append(path, _)
        | Op::Truncate(path, _)
        | Op::Splice(path, ..)
        | Op::Move(path, _) => path,
        Op::Replace(_) => unreachable!("replace 无路径"),
    }
}

/// 对应 `Encoder`：跨批次路径 interning（首次内联、第二次定义 id、同路径省略）。
#[derive(Default)]
pub struct Encoder {
    seen: HashSet<String>,
    ids: HashMap<String, u64>,
    next_id: u64,
}

impl Encoder {
    /// 对应 `encoder()`。
    pub fn new() -> Self {
        Self::default()
    }

    /// 对应 `encode(ops)`。
    pub fn encode(&mut self, ops: &[Op]) -> Vec<WireOp> {
        let mut out = Vec::new();
        let mut previous: Option<String> = None;
        for op in ops {
            if let Op::Replace(value) = op {
                out.push(serde_json::json!(["r", value]));
                // 基批是恢复点：其后必须自包含。
                self.seen.clear();
                self.ids.clear();
                self.next_id = 0;
                previous = None;
                continue;
            }

            let path = wire_op_path(op).clone();
            let key = path_key(&path);

            // 与上一 op 同路径：省略 ref。
            if previous.as_deref() == Some(key.as_str()) {
                match op {
                    Op::Set(_, value) => out.push(serde_json::json!(["s", value])),
                    Op::Delete(_) => out.push(serde_json::json!(["d"])),
                    Op::Append(_, text) => out.push(serde_json::json!(["a", text])),
                    Op::Truncate(_, count) => out.push(serde_json::json!(["t", count])),
                    Op::Splice(_, index, remove, items) => {
                        out.push(serde_json::json!(["p", index, remove, items]))
                    }
                    Op::Move(_, permutation) => out.push(serde_json::json!(["m", permutation])),
                    Op::Replace(_) => unreachable!("replace 已提前处理"),
                }
                continue;
            }

            let reference = if let Some(id) = self.ids.get(&key) {
                serde_json::json!(*id)
            } else if self.seen.contains(&key) {
                let id = self.next_id;
                self.next_id += 1;
                self.ids.insert(key.clone(), id);
                out.push(serde_json::json!(["#", id, path]));
                serde_json::json!(id)
            } else {
                self.seen.insert(key.clone());
                serde_json::json!(path)
            };

            match op {
                Op::Set(_, value) => out.push(serde_json::json!(["s", reference, value])),
                Op::Delete(_) => out.push(serde_json::json!(["d", reference])),
                Op::Append(_, text) => out.push(serde_json::json!(["a", reference, text])),
                Op::Truncate(_, count) => out.push(serde_json::json!(["t", reference, count])),
                Op::Splice(_, index, remove, items) => {
                    out.push(serde_json::json!(["p", reference, index, remove, items]))
                }
                Op::Move(_, permutation) => {
                    out.push(serde_json::json!(["m", reference, permutation]))
                }
                Op::Replace(_) => unreachable!("replace 已提前处理"),
            }
            previous = Some(key);
        }
        out
    }
}

/// 对应 `encoder()` 工厂。
pub fn encoder() -> Encoder {
    Encoder::new()
}

/// 对应 `Decoder`：还原压缩 wire op 为 `Op`。
#[derive(Default)]
pub struct Decoder {
    paths: HashMap<u64, Path>,
}

impl Decoder {
    /// 对应 `decoder()`。
    pub fn new() -> Self {
        Self::default()
    }

    /// 对应 `decode(wire)`。
    pub fn decode(&mut self, wire: &[WireOp]) -> Result<Vec<Op>, DeltaError> {
        let mut out = Vec::new();
        let mut previous: Option<Path> = None;
        for wire_op in wire {
            let arr = wire_op.as_array().ok_or(DeltaError::InvalidOp)?;
            let verb = arr
                .first()
                .and_then(|v| v.as_str())
                .ok_or(DeltaError::InvalidOp)?;

            if verb == "#" {
                let id = arr
                    .get(1)
                    .and_then(|v| v.as_u64())
                    .ok_or(DeltaError::InvalidOp)?;
                let path = json_to_path(arr.get(2).ok_or(DeltaError::InvalidOp)?)?;
                assert_safe_path(&path)?;
                self.paths.insert(id, path);
                continue;
            }

            if verb == "r" {
                out.push(Op::Replace(
                    arr.get(1).cloned().ok_or(DeltaError::InvalidOp)?,
                ));
                self.paths.clear();
                previous = None;
                continue;
            }

            // 短形态（省略 ref）按 arity 判断。
            let short = match verb {
                "d" => arr.len() == 1,
                "p" => arr.len() == 4,
                _ => arr.len() == 2,
            };

            let path = if short {
                previous.clone().ok_or(DeltaError::Path)?
            } else {
                let reference = arr.get(1).ok_or(DeltaError::InvalidOp)?;
                let path = if reference.is_number() {
                    let id = reference.as_u64().ok_or(DeltaError::InvalidOp)?;
                    self.paths.get(&id).cloned().ok_or(DeltaError::Path)?
                } else {
                    json_to_path(reference)?
                };
                previous = Some(path.clone());
                path
            };

            if verb != "p" && verb != "m" && path.is_empty() {
                return Err(DeltaError::EmptyPath);
            }

            match verb {
                "s" => {
                    let value = if short { arr.get(1) } else { arr.get(2) }
                        .cloned()
                        .ok_or(DeltaError::InvalidOp)?;
                    out.push(Op::Set(path, value));
                }
                "d" => out.push(Op::Delete(path)),
                "a" => {
                    let text = if short { arr.get(1) } else { arr.get(2) }
                        .and_then(|v| v.as_str())
                        .ok_or(DeltaError::InvalidOp)?
                        .to_string();
                    out.push(Op::Append(path, text));
                }
                "t" => {
                    let count = if short { arr.get(1) } else { arr.get(2) }
                        .and_then(|v| v.as_u64())
                        .ok_or(DeltaError::InvalidOp)? as usize;
                    out.push(Op::Truncate(path, count));
                }
                "p" => {
                    let (index, remove, items) = if short {
                        (
                            arr.get(1).and_then(|v| v.as_u64()),
                            arr.get(2).and_then(|v| v.as_u64()),
                            arr.get(3).and_then(|v| v.as_array()),
                        )
                    } else {
                        (
                            arr.get(2).and_then(|v| v.as_u64()),
                            arr.get(3).and_then(|v| v.as_u64()),
                            arr.get(4).and_then(|v| v.as_array()),
                        )
                    };
                    let index = index.ok_or(DeltaError::InvalidOp)? as usize;
                    let remove = remove.ok_or(DeltaError::InvalidOp)? as usize;
                    let items = items.ok_or(DeltaError::InvalidOp)?.clone();
                    out.push(Op::Splice(path, index, remove, items));
                }
                "m" => {
                    let permutation = if short { arr.get(1) } else { arr.get(2) }
                        .and_then(|v| v.as_array())
                        .ok_or(DeltaError::InvalidOp)?;
                    let permutation = permutation
                        .iter()
                        .map(|v| v.as_u64().map(|n| n as usize))
                        .collect::<Option<Vec<_>>>()
                        .ok_or(DeltaError::InvalidOp)?;
                    out.push(Op::Move(path, permutation));
                }
                _ => return Err(DeltaError::InvalidOp),
            }
        }
        Ok(out)
    }
}

/// 对应 `decoder()` 工厂。
pub fn decoder() -> Decoder {
    Decoder::new()
}

/// 从 JSON 数组还原 `Path`。
fn json_to_path(value: &JsonValue) -> Result<Path, DeltaError> {
    let arr = value.as_array().ok_or(DeltaError::InvalidOp)?;
    let mut path = Vec::with_capacity(arr.len());
    for segment in arr {
        if let Some(key) = segment.as_str() {
            path.push(PathSegment::Key(key.to_string()));
        } else if let Some(index) = segment.as_u64() {
            path.push(PathSegment::Index(index as usize));
        } else {
            return Err(DeltaError::InvalidOp);
        }
    }
    Ok(path)
}
