//! Rust 翻译自 packages/agent/src/harness/utils/shell-output.ts（简化：无流式 onChunk）
//!
//! 注：TS 通过 `Shell.exec` 的 `onStdout`/`onStderr` 回调流式捕获；Rust 的
//! `std::process::Command::output` 为一次性返回，故省略流式进度，保留截断与
//! 临时文件保存完整输出的核心逻辑。

use std::sync::Arc;

use crate::harness::types::{ExecutionEnv, ExecutionError, ShellExecOptions};
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
    str.chars()
        .filter(|c| {
            let code = *c as u32;
            if code == 0x09 || code == 0x0a || code == 0x0d {
                return true;
            }
            if code <= 0x1f {
                return false;
            }
            if (0xfff9..=0xfffb).contains(&code) {
                return false;
            }
            true
        })
        .collect()
}

/// 对应 `executeShellWithCapture`（简化）。
pub async fn execute_shell_with_capture(
    env: &Arc<dyn ExecutionEnv>,
    command: &str,
    options: ShellExecOptions,
) -> Result<ShellCaptureResult, ExecutionError> {
    let exec_result = env.exec(command, options.clone()).await;
    let cancelled = options
        .abort_signal
        .as_ref()
        .map(|s| s.aborted())
        .unwrap_or(false);

    let stdout = match &exec_result {
        Ok(r) => &r.stdout,
        Err(_) => "",
    };
    let stderr = match &exec_result {
        Ok(r) => &r.stderr,
        Err(_) => "",
    };
    let combined = if stdout.is_empty() {
        stderr.to_string()
    } else if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    };
    let combined = sanitize_binary_output(&combined).replace('\r', "");

    let total_bytes = combined.len();
    let total_lines = if combined.is_empty() {
        0
    } else {
        combined.split('\n').count()
    };
    let truncation = truncate_tail(&combined, Default::default());
    let truncated = total_lines > DEFAULT_MAX_LINES || total_bytes > DEFAULT_MAX_BYTES;

    // 超限时保存完整输出到临时文件。
    let mut full_output_path = None;
    if truncated
        && let Ok(temp_file) = env
            .create_temp_file(Some("bash-"), Some(".log"), None)
            .await
    {
        let _ = env.write_file(&temp_file, combined.as_bytes(), None).await;
        full_output_path = Some(temp_file);
    }

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
        last_line_bytes: combined.rsplit('\n').next().map(|l| l.len()).unwrap_or(0),
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
            if error.code == crate::harness::types::ExecutionErrorCode::Aborted || cancelled {
                Ok(ShellCaptureResult {
                    exit_code: None,
                    cancelled: true,
                    truncated,
                    progress,
                    execution_error: None,
                })
            } else {
                // returnExecutionErrors 语义：返回带捕获输出的结果。
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
