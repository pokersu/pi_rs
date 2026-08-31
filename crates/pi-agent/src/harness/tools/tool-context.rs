//! Rust 翻译自 packages/agent/src/harness/tools/tool-context.ts

use std::sync::Arc;

use crate::harness::types::ExecutionEnv;

/// 对应 `ExecutionToolContext`：内置执行工具所需的文件系统与 shell 上下文。
#[derive(Clone)]
pub struct ExecutionToolContext {
    pub env: Arc<dyn ExecutionEnv>,
}
