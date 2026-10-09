//! 对应 `harness/usage.ts`：一个会话自身花费的账本。
//!
//! `pi.usage` 按 `provider/modelId` 记录 assistant 条目与摘要尝试，按工具名记录工具结果
//! （工具的花费没有模型身份）。
//!
//! # 与上游的差异
//!
//! - 上游用 `Record<string, JsonRepresentation<Usage>>`（普通对象）；Rust 用 [`JsonObject`]。
//!   因此「只认自有键」检查（`Object.hasOwn`）在 Rust 侧天然成立，`__proto__` 一类的原型投毒
//!   也不存在——相关保护省略。
//! - `copyJson(usage, { omitUndefinedProperties: true })` → [`usage_to_json`] 在序列化后剔除
//!   `null` 成员（pi-ai 的 `Usage` 把可选计数序列化为 `null`，而上游那里是 `undefined`）。

use std::sync::Arc;
use std::sync::LazyLock;

use pi_ai::{Usage, UsageCost};
use serde_json::Value as JsonValue;

use crate::documents::define_doc;
use crate::session::SessionError;
use crate::session::transaction::Transaction;
use crate::types::{
    ConversationFork, ConversationHistory, DocAccess, DocDefinitionSpec, DocToken,
    DocumentSemantics, JsonObject,
};

/// 对应 `UsageState`：一个会话的花费账本。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct UsageState {
    /// assistant 条目与摘要尝试，键为 `provider/modelId`。
    #[serde(default)]
    pub models: JsonObject,
    /// 工具结果，键为工具名。
    #[serde(default)]
    pub tools: JsonObject,
}

/// 对应 `keyof UsageState`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageBucket {
    /// `models`。
    Models,
    /// `tools`。
    Tools,
}

impl UsageBucket {
    /// 状态里的键名。
    pub fn as_key(self) -> &'static str {
        match self {
            UsageBucket::Models => "models",
            UsageBucket::Tools => "tools",
        }
    }

    /// 两个桶，按上游 `addUsageState` 的遍历顺序。
    pub fn all() -> [UsageBucket; 2] {
        [UsageBucket::Models, UsageBucket::Tools]
    }
}

struct UsageDefinition;

impl DocDefinitionSpec for UsageDefinition {
    fn kind(&self) -> &str {
        "pi.usage"
    }

    fn version(&self) -> u32 {
        1
    }

    fn semantics(&self) -> DocumentSemantics {
        DocumentSemantics::Conversation {
            history: ConversationHistory::Latest,
            fork: ConversationFork::Initial,
        }
    }

    fn initial(&self, _seed: Option<&JsonValue>) -> JsonObject {
        let mut models = JsonObject::new();
        let mut tools = JsonObject::new();
        models.clear();
        tools.clear();
        let mut object = JsonObject::new();
        object.insert("models".to_string(), JsonValue::Object(models));
        object.insert("tools".to_string(), JsonValue::Object(tools));
        object
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

/// 对应 `UsageDoc`：内置花费账本文档。
pub static USAGE_DOC: LazyLock<DocToken> =
    LazyLock::new(|| define_doc(Arc::new(UsageDefinition)).expect("pi.usage"));

/// 对应 `recordUsage(tx, conversationId, bucket, key, usage)`：在记录响应的同一个提交里累加花费。
pub async fn record_usage(
    tx: &Transaction,
    conversation_id: crate::types::ConversationId,
    bucket: UsageBucket,
    key: &str,
    usage: &Usage,
) -> Result<(), SessionError> {
    let draft = tx
        .doc(
            &*USAGE_DOC,
            DocAccess {
                owner: Some(conversation_id.get()),
                key: None,
            },
            None,
        )
        .await?;
    let mut value = draft.value();
    let bucket_key = bucket.as_key();
    let Some(buckets) = value.as_object_mut() else {
        return Err(SessionError::Message(
            "pi.usage is not an object".to_string(),
        ));
    };
    let Some(bucket_object) = buckets
        .get_mut(bucket_key)
        .and_then(JsonValue::as_object_mut)
    else {
        return Err(SessionError::Message(format!(
            "pi.usage.{bucket_key} is not an object"
        )));
    };
    match bucket_object.get_mut(key) {
        Some(total) => add_usage_json(total, usage)?,
        None => {
            bucket_object.insert(key.to_string(), usage_to_json(usage));
        }
    }
    let bucket_value = JsonValue::Object(bucket_object.clone());
    draft
        .set(path(bucket_key), bucket_value)
        .map_err(delta_error)?;
    Ok(())
}

/// 对应 `addUsage(total, usage)`：把 `usage` 的每个计数加到 `total` 上。
///
/// 可选计数只要任一侧报告过就参与累加。
pub fn add_usage_json(total: &mut JsonValue, usage: &Usage) -> Result<(), SessionError> {
    let Some(object) = total.as_object_mut() else {
        return Err(SessionError::Message(
            "usage total is not an object".to_string(),
        ));
    };
    add_counter(object, "input", usage.input);
    add_counter(object, "output", usage.output);
    add_counter(object, "cacheRead", usage.cache_read);
    add_counter(object, "cacheWrite", usage.cache_write);
    add_counter(object, "totalTokens", usage.total_tokens);
    add_optional_counter(object, "cacheWrite1h", usage.cache_write_1h);
    add_optional_counter(object, "reasoning", usage.reasoning);
    match object.get_mut("cost").and_then(JsonValue::as_object_mut) {
        Some(cost) => add_cost(cost, &usage.cost),
        None => {
            object.insert("cost".to_string(), cost_to_json(&usage.cost));
        }
    }
    Ok(())
}

/// 对应 `addUsageState(sum, state)`：把 `state` 的每个桶加进 `sum`。
pub fn add_usage_state(sum: &mut UsageState, state: &UsageState) {
    for bucket in UsageBucket::all() {
        let source = match bucket {
            UsageBucket::Models => &state.models,
            UsageBucket::Tools => &state.tools,
        };
        let target = match bucket {
            UsageBucket::Models => &mut sum.models,
            UsageBucket::Tools => &mut sum.tools,
        };
        for (key, usage) in source {
            match target.get_mut(key) {
                Some(total) => {
                    // 两侧都来自本模块的写入，因此形状可信；损坏时保持原值（上游会就地抛错）。
                    let _ = add_usage_json(total, &json_to_usage(usage));
                }
                None => {
                    target.insert(key.clone(), usage.clone());
                }
            }
        }
    }
}

/// 对应 `copyJson(usage, { omitUndefinedProperties: true })`：把 `Usage` 序列化为账本里的 JSON。
///
/// pi-ai 把可选计数序列化为 `null`（上游是 `undefined`），这里剔除 `null` 成员以对齐上游形状。
pub fn usage_to_json(usage: &Usage) -> JsonValue {
    crate::harness::json::to_json_without_nulls(usage)
}

fn cost_to_json(cost: &UsageCost) -> JsonValue {
    crate::harness::json::to_json_without_nulls(cost)
}

fn add_counter(object: &mut JsonObject, key: &str, delta: u64) {
    let current = object.get(key).and_then(JsonValue::as_u64).unwrap_or(0);
    object.insert(key.to_string(), JsonValue::from(current + delta));
}

fn add_optional_counter(object: &mut JsonObject, key: &str, delta: Option<u64>) {
    let Some(delta) = delta else {
        return;
    };
    add_counter(object, key, delta);
}

fn add_cost(cost: &mut JsonObject, delta: &UsageCost) {
    for (key, value) in [
        ("input", delta.input),
        ("output", delta.output),
        ("cacheRead", delta.cache_read),
        ("cacheWrite", delta.cache_write),
        ("total", delta.total),
    ] {
        let current = cost.get(key).and_then(JsonValue::as_f64).unwrap_or(0.0);
        cost.insert(key.to_string(), JsonValue::from(current + value));
    }
}

/// 把账本里的 JSON 还原为 `Usage`（仅用于内部合并；形态由本模块保证）。
fn json_to_usage(value: &JsonValue) -> Usage {
    serde_json::from_value(value.clone()).unwrap_or(Usage {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: 0,
        cost: UsageCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    })
}

fn path(key: &str) -> Vec<crate::chord::delta::PathSegment> {
    vec![crate::chord::delta::PathSegment::Key(key.to_string())]
}

fn delta_error(error: crate::chord::delta::DeltaError) -> SessionError {
    SessionError::Message(error.to_string())
}

/// 便于阅读：上游 `JsonRepresentation<Usage>` 对应本模块的 JSON 形态。
#[allow(dead_code)]
type UsageRepresentation = JsonValue;

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: input + output,
            cost: UsageCost {
                input: 0.1,
                output: 0.2,
                cache_read: 0.0,
                cache_write: 0.0,
                total: 0.3,
            },
        }
    }

    #[test]
    fn doc_kind_and_initial_shape_match_upstream() {
        let definition = USAGE_DOC.definition();
        assert_eq!(definition.kind(), "pi.usage");
        let initial = definition.initial(None);
        assert_eq!(initial["models"], serde_json::json!({}));
        assert_eq!(initial["tools"], serde_json::json!({}));
    }

    #[test]
    fn omitted_optional_counters_are_dropped_from_the_stored_shape() {
        let json = usage_to_json(&usage(1, 2));
        assert_eq!(json["input"], serde_json::json!(1));
        assert_eq!(json["totalTokens"], serde_json::json!(3));
        assert_eq!(json["cost"]["total"], serde_json::json!(0.3));
        assert!(json.get("cacheWrite1h").is_none(), "undefined 成员不得写入");
        assert!(json.get("reasoning").is_none());
    }

    #[test]
    fn adding_accumulates_every_counter() {
        let mut total = usage_to_json(&usage(1, 2));
        add_usage_json(&mut total, &usage(10, 20)).unwrap();
        assert_eq!(total["input"], serde_json::json!(11));
        assert_eq!(total["output"], serde_json::json!(22));
        assert_eq!(total["totalTokens"], serde_json::json!(33));
        assert!((total["cost"]["total"].as_f64().unwrap() - 0.6).abs() < 1e-9);
    }

    #[test]
    fn optional_counters_appear_once_either_side_reports_them() {
        let mut total = usage_to_json(&usage(0, 0));
        assert!(total.get("reasoning").is_none());

        let mut reported = usage(0, 0);
        reported.reasoning = Some(5);
        add_usage_json(&mut total, &reported).unwrap();
        assert_eq!(total["reasoning"], serde_json::json!(5));

        let mut again = usage(0, 0);
        again.reasoning = Some(7);
        add_usage_json(&mut total, &again).unwrap();
        assert_eq!(total["reasoning"], serde_json::json!(12));
    }

    #[test]
    fn usage_state_merge_adds_matching_keys_and_copies_new_ones() {
        let mut sum = UsageState::default();
        let mut first = UsageState::default();
        first
            .models
            .insert("p/m".to_string(), usage_to_json(&usage(1, 1)));
        first
            .tools
            .insert("bash".to_string(), usage_to_json(&usage(2, 2)));

        add_usage_state(&mut sum, &first);
        assert_eq!(sum.models["p/m"]["input"], serde_json::json!(1));
        assert_eq!(sum.tools["bash"]["input"], serde_json::json!(2));

        add_usage_state(&mut sum, &first);
        assert_eq!(sum.models["p/m"]["input"], serde_json::json!(2));
        assert_eq!(sum.tools["bash"]["input"], serde_json::json!(4));

        let mut second = UsageState::default();
        second
            .tools
            .insert("read".to_string(), usage_to_json(&usage(9, 0)));
        add_usage_state(&mut sum, &second);
        assert_eq!(sum.tools["read"]["input"], serde_json::json!(9));
        assert_eq!(
            sum.tools["bash"]["input"],
            serde_json::json!(4),
            "旧桶不受影响"
        );
    }

    #[test]
    fn bucket_keys_match_upstream_field_names() {
        assert_eq!(UsageBucket::Models.as_key(), "models");
        assert_eq!(UsageBucket::Tools.as_key(), "tools");
    }
}
