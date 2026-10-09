//! 对应 `harness/json.ts`：JSON 容器的逐叶赋值。
//!
//! chord 会把一次容器赋值记录成一次整体 `set`，只有把字符串叶重新赋成更长的字符串时才会产生 append。
//! 因此直接写整个局部对象会让每次 flush 都存储并发布完整消息；逐叶写入只产生真正变化的叶。

use serde_json::Value as JsonValue;

use crate::types::JsonObject;

/// 对应 `JsonContainer` 中的键：对象成员名或数组下标。
#[derive(Debug, Clone, Copy)]
pub enum JsonKey<'a> {
    /// 对象成员。
    Name(&'a str),
    /// 数组下标。
    Index(usize),
}

/// 对应 `assignJson(target, key, value)`：把 `value` 逐叶写入 `target[key]`。
///
/// 两侧都是对象时按上游语义合并：先删除 `value` 没有的成员，再逐个递归赋值；
/// 两侧都是数组且当前长度不超过目标长度时按下标递归，多出的元素追加；
/// 其余情况在值确实不同时整体替换。
pub fn assign_json(target: &mut JsonValue, key: JsonKey<'_>, value: JsonValue) {
    match slot(target, key) {
        Some(current) => assign_into(current, value),
        None => insert_new(target, key, value),
    }
}

/// 取出可写槽位；键与容器形态不符或下标越界时为 `None`。
fn slot<'a>(target: &'a mut JsonValue, key: JsonKey<'_>) -> Option<&'a mut JsonValue> {
    match (target, key) {
        (JsonValue::Object(object), JsonKey::Name(name)) => object.get_mut(name),
        (JsonValue::Array(items), JsonKey::Index(index)) => items.get_mut(index),
        _ => None,
    }
}

/// 槽位不存在：对象插入新成员，数组追加（对应 JS 数组赋值的扩展语义）。
fn insert_new(target: &mut JsonValue, key: JsonKey<'_>, value: JsonValue) {
    match (target, key) {
        (JsonValue::Object(object), JsonKey::Name(name)) => {
            object.insert(name.to_string(), value);
        }
        (JsonValue::Array(items), JsonKey::Index(_)) => items.push(value),
        _ => {}
    }
}

/// 对应 `assignJson` 的递归主体：把 `value` 写进已存在的 `current`。
fn assign_into(current: &mut JsonValue, value: JsonValue) {
    match (&mut *current, value) {
        (JsonValue::Object(target), JsonValue::Object(source)) => {
            let keep: std::collections::BTreeSet<String> = source.keys().cloned().collect();
            target.retain(|name, _| keep.contains(name));
            for (name, child) in source {
                match target.get_mut(&name) {
                    Some(existing) => assign_into(existing, child),
                    None => {
                        target.insert(name, child);
                    }
                }
            }
        }
        (JsonValue::Array(target), JsonValue::Array(source)) if target.len() <= source.len() => {
            for (index, child) in source.into_iter().enumerate() {
                match target.get_mut(index) {
                    Some(existing) => assign_into(existing, child),
                    None => target.push(child),
                }
            }
        }
        (target, value) => {
            if *target != value {
                *target = value;
            }
        }
    }
}

/// 对应 `isRecord`：JSON 对象（排除数组与 null）。
pub fn is_record(value: &JsonValue) -> bool {
    value.is_object()
}

/// 对应 `copyJson(value, { omitUndefinedProperties: true })` 的效果：剔除 `null` 成员。
///
/// pi-ai 与 durable 的可选字段序列化为 `null`，而上游那里是 `undefined`；两者在存储形状上需要一致。
pub fn omit_null_members(value: &mut JsonValue) {
    match value {
        JsonValue::Object(object) => {
            object.retain(|_, member| !member.is_null());
            for member in object.values_mut() {
                omit_null_members(member);
            }
        }
        JsonValue::Array(items) => {
            for item in items {
                omit_null_members(item);
            }
        }
        _ => {}
    }
}

/// 序列化一个值并剔除 `null` 成员。
pub fn to_json_without_nulls<T: serde::Serialize>(value: &T) -> JsonValue {
    let mut json = serde_json::to_value(value).expect("value serialises");
    omit_null_members(&mut json);
    json
}

/// 便捷入口：在对象上按名字赋值。
pub fn assign_json_name(target: &mut JsonObject, name: &str, value: JsonValue) {
    let mut root = JsonValue::Object(std::mem::take(target));
    assign_json(&mut root, JsonKey::Name(name), value);
    if let JsonValue::Object(object) = root {
        *target = object;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assigns_a_new_key() {
        let mut target = json!({});
        assign_json(&mut target, JsonKey::Name("a"), json!(1));
        assert_eq!(target, json!({"a": 1}));
    }

    #[test]
    fn merges_nested_objects_leaf_by_leaf() {
        let mut target = json!({"message": {"role": "assistant", "text": "hi"}});
        assign_json(
            &mut target,
            JsonKey::Name("message"),
            json!({"role": "assistant", "text": "hi there"}),
        );
        assert_eq!(
            target,
            json!({"message": {"role": "assistant", "text": "hi there"}})
        );
    }

    #[test]
    fn removes_members_absent_from_the_value() {
        let mut target = json!({"message": {"role": "assistant", "stale": true}});
        assign_json(
            &mut target,
            JsonKey::Name("message"),
            json!({"role": "assistant"}),
        );
        assert_eq!(target, json!({"message": {"role": "assistant"}}));
    }

    #[test]
    fn appends_grown_arrays_and_merges_shared_items() {
        let mut target = json!({"items": [{"text": "a"}]});
        assign_json(
            &mut target,
            JsonKey::Name("items"),
            json!([{"text": "a+"}, {"text": "b"}]),
        );
        assert_eq!(target, json!({"items": [{"text": "a+"}, {"text": "b"}]}));
    }

    #[test]
    fn replaces_when_the_array_shrinks() {
        let mut target = json!({"items": [1, 2, 3]});
        assign_json(&mut target, JsonKey::Name("items"), json!([9]));
        assert_eq!(target, json!({"items": [9]}));
    }

    #[test]
    fn replaces_scalars_only_when_they_differ() {
        let mut target = json!({"a": {"b": 1}});
        assign_json(&mut target, JsonKey::Name("a"), json!({"b": 1}));
        assert_eq!(target, json!({"a": {"b": 1}}));
    }

    #[test]
    fn writes_into_arrays_by_index() {
        let mut target = json!([{"a": 1}]);
        assign_json(&mut target, JsonKey::Index(0), json!({"a": 2}));
        assert_eq!(target, json!([{"a": 2}]));
    }

    #[test]
    fn object_helper_round_trips() {
        let mut target = JsonObject::new();
        target.insert("count".to_string(), json!(0));
        assign_json_name(&mut target, "count", json!(1));
        assert_eq!(target.get("count"), Some(&json!(1)));
    }
}
