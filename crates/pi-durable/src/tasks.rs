//! 对应 `src/tasks.ts`：定义可执行任务。

use std::sync::Arc;

use crate::types::{Task, TaskDefinitionSpec};

/// 对应 `defineTask`：定义可执行任务，并注册到 registry 以便 Harness 运行。
///
/// 上游接受 `TaskDefinition<I, S, R, H>` 字面量；Rust 接受实现了 [`TaskDefinitionSpec`] 的定义对象。
pub fn define_task(definition: Arc<dyn TaskDefinitionSpec>) -> Task {
    Task::new(definition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value as JsonValue, json};

    struct Counter;

    impl TaskDefinitionSpec for Counter {
        fn name(&self) -> &str {
            "counter"
        }

        fn version(&self) -> u32 {
            1
        }

        fn initial(&self, input: &JsonValue) -> JsonValue {
            json!({ "phase": "start", "value": input })
        }

        fn phases(&self) -> &[&'static str] {
            &["start"]
        }
    }

    #[test]
    fn define_task_exposes_definition() {
        let task = define_task(Arc::new(Counter));
        let definition = task.definition();
        assert_eq!(definition.name(), "counter");
        assert_eq!(definition.version(), 1);
        assert_eq!(definition.phases(), &["start"]);
        assert_eq!(
            definition.initial(&json!(7)),
            json!({ "phase": "start", "value": 7 })
        );
    }
}
