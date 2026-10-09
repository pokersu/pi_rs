//! 对应 `tools/env.ts`：从调用 API 取执行环境。

use std::sync::Arc;

use crate::env::ExecutionEnv;
use crate::harness::types::ToolExecutionApi;
use crate::session::SessionError;

/// 对应 `requireEnv`：本次调用的执行环境；未配置时以普通错误结果失败。
pub fn require_env(api: &dyn ToolExecutionApi) -> Result<Arc<dyn ExecutionEnv>, SessionError> {
    api.env()
        .ok_or_else(|| SessionError::Message("No execution environment is configured".to_string()))
}
