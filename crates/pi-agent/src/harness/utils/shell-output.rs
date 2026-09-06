//! Rust 翻译自 packages/agent/src/harness/utils/shell-output.ts（兼容收集层）
//!
//! 基于新的 `Shell.exec` capture/onUpdate 的有界输出收集兼容层。

use std::sync::{Arc, Mutex};

use crate::harness::context::Context;
use crate::harness::types::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, ShellExecOptions, ShellOutputCaptureOptions,
    ShellOutputLimits, ShellOutputRetention, ShellOutputView,
};
use crate::harness::utils::output_capture::{apply_shell_output_update, sanitize_shell_output};
use crate::harness::utils::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncationResult, truncate_tail,
};

/// 对应 `ShellCaptureProgress`
#[derive(Debug, Clone)]
pub struct ShellCaptureProgress {
    pub output: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<String>,
    pub last_line_bytes: usize,
}

/// 对应 `ShellCaptureResult`
#[derive(Debug, Clone)]
pub struct ShellCaptureResult {
    pub progress: ShellCaptureProgress,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    pub execution_error: Option<ExecutionError>,
}

/// 对应 `sanitizeBinaryOutput`
pub fn sanitize_binary_output(str: &str) -> String {
    sanitize_shell_output(str).replace('\r', "")
}

/// 对应 `executeShellWithCapture`。
pub async fn execute_shell_with_capture(
    env: &Arc<dyn ExecutionEnv>,
    command: &str,
    options: ShellExecOptions,
    context: &Context,
) -> Result<ShellCaptureResult, ExecutionError> {
    let view_ref = Arc::new(Mutex::new(None::<ShellOutputView>));
    let view_for_cb = Arc::clone(&view_ref);

    let mut exec_options = options;
    exec_options.capture = Some(ShellOutputCaptureOptions {
        limits: ShellOutputLimits {
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            retain: Some(ShellOutputRetention::Tail),
        },
        spill: Some(true),
    });
    exec_options.on_update = Some(Box::new(
        move |update: &crate::harness::types::ShellOutputUpdate, _ctx: &Context| {
            let mut view = view_for_cb.lock().unwrap();
            let next = apply_shell_output_update(view.as_ref(), update);
            *view = Some(next);
        },
    ));

    let exec_result = env.exec(command, exec_options, context).await;
    let cancelled = context.abort_signal().map(|s| s.aborted()).unwrap_or(false);

    let view = view_ref.lock().unwrap().clone();
    let combined = view.as_ref().map(|v| v.text.clone()).unwrap_or_default();
    let combined = sanitize_binary_output(&combined);

    let total_bytes = combined.len();
    let total_lines = if combined.is_empty() {
        0
    } else {
        combined.split('\n').count()
    };
    let truncation = truncate_tail(&combined, Default::default());
    let truncated = total_lines > DEFAULT_MAX_LINES || total_bytes > DEFAULT_MAX_BYTES;

    let full_output_path = view.as_ref().and_then(|v| v.spill_path.clone());
    let last_line_bytes = view
        .as_ref()
        .and_then(|v| v.last_line_bytes)
        .unwrap_or_else(|| combined.rsplit('\n').next().map(|l| l.len()).unwrap_or(0));

    let progress = ShellCaptureProgress {
        output: if truncated {
            truncation.content.clone()
        } else {
            combined.clone()
        },
        truncation: TruncationResult {
            truncated,
            total_lines,
            total_bytes,
            ..truncation
        },
        full_output_path,
        last_line_bytes,
    };

    match exec_result {
        Ok(result) => Ok(ShellCaptureResult {
            exit_code: if cancelled {
                None
            } else {
                Some(result.exit_code)
            },
            cancelled,
            truncated,
            progress,
            execution_error: None,
        }),
        Err(error) => {
            if error.code == ExecutionErrorCode::Aborted || cancelled {
                Ok(ShellCaptureResult {
                    exit_code: None,
                    cancelled: true,
                    truncated,
                    progress,
                    execution_error: None,
                })
            } else {
                Ok(ShellCaptureResult {
                    exit_code: None,
                    cancelled: false,
                    truncated,
                    progress,
                    execution_error: Some(error),
                })
            }
        }
    }
}
