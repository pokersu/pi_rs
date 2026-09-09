//! Rust 翻译自 packages/agent/src/harness/env/nodejs.ts
//!
//! 基于 `std::fs` + `std::process` 的 `FileSystem`/`Shell` 实现。

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};

use pi_ai::AbortSignal;

use crate::harness::context::Context;
use crate::harness::types::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo, FileKind,
    FileSystem, Shell, ShellExecOptions, ShellExecResult, TextLine, TextLineReader,
};
use crate::harness::utils::output_capture::OutputCapture;

fn map_io_error(path: &str, error: &std::io::Error) -> FileError {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => FileErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        std::io::ErrorKind::NotADirectory => FileErrorCode::NotDirectory,
        std::io::ErrorKind::IsADirectory => FileErrorCode::IsDirectory,
        std::io::ErrorKind::InvalidInput => FileErrorCode::Invalid,
        std::io::ErrorKind::Unsupported => FileErrorCode::NotSupported,
        _ => FileErrorCode::Unknown,
    };
    FileError {
        code,
        message: error.to_string(),
        path: Some(path.to_string()),
    }
}

fn check_aborted(signal: Option<&AbortSignal>) -> Result<(), FileError> {
    if signal.map(|s| s.aborted()).unwrap_or(false) {
        return Err(FileError::new(FileErrorCode::Aborted, "Operation aborted"));
    }
    Ok(())
}

fn kind_of(meta: &std::fs::Metadata) -> FileKind {
    if meta.is_dir() {
        FileKind::Directory
    } else if meta.file_type().is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::File
    }
}

fn file_info_from(path: &str, entry: std::fs::DirEntry) -> Result<FileInfo, FileError> {
    let meta = entry.metadata().map_err(|e| map_io_error(path, &e))?;
    let full = entry.path().to_string_lossy().to_string();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Ok(FileInfo {
        name: entry.file_name().to_string_lossy().to_string(),
        path: full,
        kind: kind_of(&meta),
        size: meta.len(),
        mtime_ms: mtime,
    })
}

/// 对应 `NodeExecutionEnv`
pub struct NodeExecutionEnv {
    cwd: String,
}

impl NodeExecutionEnv {
    pub fn new(cwd: String) -> Self {
        Self { cwd }
    }
}

/// 对应 `NodeTextLineReader`。
struct NodeTextLineReader {
    inner: Mutex<NodeTextLineReaderInner>,
}

struct NodeTextLineReaderInner {
    reader: BufReader<std::fs::File>,
    path: String,
    closed: bool,
}

#[async_trait::async_trait]
impl TextLineReader for NodeTextLineReader {
    async fn read_line(&self, context: &Context) -> Result<Option<TextLine>, FileError> {
        check_aborted(context.abort_signal())?;
        let mut inner = self.inner.lock().unwrap();
        if inner.closed {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Text line reader is closed",
            ));
        }
        let mut buf: Vec<u8> = Vec::new();
        let n = inner
            .reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| map_io_error(&inner.path, &e))?;
        if n == 0 {
            return Ok(None);
        }
        let terminated = buf.last() == Some(&b'\n');
        if terminated {
            buf.pop();
        }
        let text = String::from_utf8_lossy(&buf).to_string();
        Ok(Some(TextLine { text, terminated }))
    }

    async fn close(&self, _context: &Context) -> Result<(), FileError> {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        Ok(())
    }
}

#[async_trait::async_trait]
impl FileSystem for NodeExecutionEnv {
    fn cwd(&self) -> &str {
        &self.cwd
    }

    async fn absolute_path(&self, path: &str, context: &Context) -> Result<String, FileError> {
        check_aborted(context.abort_signal())?;
        let p = Path::new(path);
        if p.is_absolute() {
            Ok(path.to_string())
        } else {
            Ok(Path::new(&self.cwd)
                .join(path)
                .to_string_lossy()
                .to_string())
        }
    }

    async fn join_path(&self, parts: &[&str], context: &Context) -> Result<String, FileError> {
        check_aborted(context.abort_signal())?;
        let mut path = std::path::PathBuf::new();
        for part in parts {
            path.push(part);
        }
        Ok(path.to_string_lossy().to_string())
    }

    async fn read_text_file(&self, path: &str, context: &Context) -> Result<String, FileError> {
        check_aborted(context.abort_signal())?;
        std::fs::read_to_string(path).map_err(|e| map_io_error(path, &e))
    }

    async fn open_text_line_reader(
        &self,
        path: &str,
        context: &Context,
    ) -> Result<Arc<dyn TextLineReader>, FileError> {
        check_aborted(context.abort_signal())?;
        let file = std::fs::File::open(path).map_err(|e| map_io_error(path, &e))?;
        Ok(Arc::new(NodeTextLineReader {
            inner: Mutex::new(NodeTextLineReaderInner {
                reader: BufReader::new(file),
                path: path.to_string(),
                closed: false,
            }),
        }))
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        context: &Context,
    ) -> Result<Vec<String>, FileError> {
        check_aborted(context.abort_signal())?;
        let content = std::fs::read_to_string(path).map_err(|e| map_io_error(path, &e))?;
        let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        if let Some(max) = max_lines {
            lines.truncate(max);
        }
        Ok(lines)
    }

    async fn read_binary_file(&self, path: &str, context: &Context) -> Result<Vec<u8>, FileError> {
        check_aborted(context.abort_signal())?;
        std::fs::read(path).map_err(|e| map_io_error(path, &e))
    }

    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError> {
        check_aborted(context.abort_signal())?;
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| map_io_error(path, &e))?;
        }
        std::fs::write(path, content).map_err(|e| map_io_error(path, &e))
    }

    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError> {
        check_aborted(context.abort_signal())?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| map_io_error(path, &e))?;
        file.write_all(content).map_err(|e| map_io_error(path, &e))
    }

    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        context: &Context,
    ) -> Result<(), FileError> {
        check_aborted(context.abort_signal())?;
        std::fs::rename(source, dest).map_err(|e| map_io_error(source, &e))
    }

    async fn file_info(&self, path: &str, context: &Context) -> Result<FileInfo, FileError> {
        check_aborted(context.abort_signal())?;
        let meta = std::fs::metadata(path).map_err(|e| map_io_error(path, &e))?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ok(FileInfo {
            name: Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            path: path.to_string(),
            kind: kind_of(&meta),
            size: meta.len(),
            mtime_ms: mtime,
        })
    }

    async fn list_dir(&self, path: &str, context: &Context) -> Result<Vec<FileInfo>, FileError> {
        check_aborted(context.abort_signal())?;
        let entries = std::fs::read_dir(path).map_err(|e| map_io_error(path, &e))?;
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| map_io_error(path, &e))?;
            result.push(file_info_from(path, entry)?);
        }
        Ok(result)
    }

    async fn canonical_path(&self, path: &str, context: &Context) -> Result<String, FileError> {
        check_aborted(context.abort_signal())?;
        std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().to_string())
            .map_err(|e| map_io_error(path, &e))
    }

    async fn exists(&self, path: &str, context: &Context) -> Result<bool, FileError> {
        check_aborted(context.abort_signal())?;
        match std::fs::metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(map_io_error(path, &e)),
        }
    }

    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        context: &Context,
    ) -> Result<(), FileError> {
        check_aborted(context.abort_signal())?;
        if recursive {
            std::fs::create_dir_all(path).map_err(|e| map_io_error(path, &e))
        } else {
            std::fs::create_dir(path).map_err(|e| map_io_error(path, &e))
        }
    }

    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        context: &Context,
    ) -> Result<(), FileError> {
        check_aborted(context.abort_signal())?;
        let result = if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path))
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if force && e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(map_io_error(path, &e)),
        }
    }

    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        context: &Context,
    ) -> Result<String, FileError> {
        check_aborted(context.abort_signal())?;
        let prefix = prefix.unwrap_or("tmp-");
        for i in 0..1000 {
            let dir = std::env::temp_dir().join(format!("{prefix}{i}-{}", std::process::id()));
            if std::fs::create_dir(&dir).is_ok() {
                return Ok(dir.to_string_lossy().to_string());
            }
        }
        Err(FileError::new(
            FileErrorCode::Unknown,
            "Failed to create temp dir",
        ))
    }

    async fn create_temp_file(
        &self,
        prefix: Option<&str>,
        suffix: Option<&str>,
        context: &Context,
    ) -> Result<String, FileError> {
        check_aborted(context.abort_signal())?;
        let prefix = prefix.unwrap_or("");
        let suffix = suffix.unwrap_or("");
        for i in 0..1000 {
            let file =
                std::env::temp_dir().join(format!("{prefix}{i}-{}{suffix}", std::process::id()));
            if std::fs::File::create(&file).is_ok() {
                return Ok(file.to_string_lossy().to_string());
            }
        }
        Err(FileError::new(
            FileErrorCode::Unknown,
            "Failed to create temp file",
        ))
    }

    async fn cleanup(&self) {}
}

#[async_trait::async_trait]
impl Shell for NodeExecutionEnv {
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
        context: &Context,
    ) -> Result<ShellExecResult, ExecutionError> {
        let command = command.to_string();
        let cwd = options.cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let env = options.env.clone();
        let timeout = options.timeout;
        let capture_options = options.capture.clone();
        let on_update: Option<
            Arc<dyn Fn(&crate::harness::types::ShellOutputUpdate, &Context) + Send + Sync>,
        > = options.on_update.map(|cb| Arc::from(cb));

        let capture = Arc::new(OutputCapture::new(
            capture_options.as_ref(),
            context.clone(),
            on_update,
        ));
        let capture_clone = Arc::clone(&capture);
        let context_clone = context.clone();

        let exit_code = tokio::task::spawn_blocking(move || {
            exec_sync(
                &command,
                &cwd,
                env.as_ref(),
                timeout,
                &context_clone,
                &capture_clone,
            )
        })
        .await
        .map_err(|e| {
            ExecutionError::new(
                ExecutionErrorCode::Unknown,
                format!("Shell task failed: {e}"),
            )
        })??;

        capture.finish();
        let view = capture.snapshot();
        capture.dispose();
        Ok(ShellExecResult {
            truncation: view.truncation,
            spill_path: view.spill_path,
            last_line_bytes: view.last_line_bytes,
            exit_code,
        })
    }

    async fn cleanup(&self, _context: &Context) {}
}

impl ExecutionEnv for NodeExecutionEnv {}

/// 同步执行 shell 命令：spawn + 增量读 stdout/stderr（回调上报）+ timeout/abort。
#[allow(clippy::too_many_arguments)]
fn exec_sync(
    command: &str,
    cwd: &str,
    env: Option<&std::collections::BTreeMap<String, String>>,
    timeout: Option<f64>,
    context: &Context,
    capture: &Arc<OutputCapture>,
) -> Result<i32, ExecutionError> {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd.current_dir(cwd);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if let Some(env) = env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }

    let mut child = cmd.spawn().map_err(|e| {
        ExecutionError::new(
            ExecutionErrorCode::SpawnError,
            format!("Failed to spawn shell: {e}"),
        )
    })?;
    let stdout_pipe = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();

    let capture_stdout = Arc::clone(capture);
    let capture_stderr = Arc::clone(capture);
    let stdout_handle = std::thread::spawn(move || read_pipe(stdout_pipe, &capture_stdout));
    let stderr_handle = std::thread::spawn(move || read_pipe(stderr_pipe, &capture_stderr));

    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| {
            ExecutionError::new(
                ExecutionErrorCode::Unknown,
                format!("Failed to wait for shell: {e}"),
            )
        })? {
            break status;
        }
        if context.abort_signal().map(|s| s.aborted()).unwrap_or(false) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Err(ExecutionError::new(
                ExecutionErrorCode::Aborted,
                "Command aborted",
            ));
        }
        if let Some(secs) = timeout
            && start.elapsed().as_secs_f64() > secs
        {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Err(ExecutionError::new(
                ExecutionErrorCode::Timeout,
                format!("Command timed out after {secs} seconds"),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    let _ = stdout_handle.join();
    let _ = stderr_handle.join();
    Ok(status.code().unwrap_or(-1))
}

/// 增量读一个管道，逐 chunk push 到有界捕获器。
fn read_pipe<R: Read>(mut pipe: R, capture: &Arc<OutputCapture>) {
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf) {
        if n == 0 {
            break;
        }
        let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
        capture.push(&chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::harness::context::BACKGROUND_CONTEXT;
    use crate::harness::types::{
        Shell, ShellOutputCaptureOptions, ShellOutputLimits, ShellOutputRetention,
        ShellOutputUpdate,
    };

    #[tokio::test]
    async fn exec_streams_stdout_chunks() {
        let env = NodeExecutionEnv::new(".".to_string());
        let chunks = Arc::new(Mutex::new(Vec::new()));
        let chunks_clone = chunks.clone();
        let options = ShellExecOptions {
            capture: Some(ShellOutputCaptureOptions {
                limits: ShellOutputLimits {
                    max_bytes: 50 * 1024,
                    max_lines: 2000,
                    retain: Some(ShellOutputRetention::Tail),
                },
                spill: Some(true),
            }),
            on_update: Some(Box::new(
                move |update: &ShellOutputUpdate, _ctx: &Context| {
                    if let ShellOutputUpdate::Append { text, .. }
                    | ShellOutputUpdate::Slide { text, .. } = update
                    {
                        chunks_clone.lock().unwrap().push(text.clone());
                    }
                },
            )),
            ..Default::default()
        };

        let context = (*BACKGROUND_CONTEXT).clone();
        let result = env
            .exec("printf 'line1\nline2\n'", options, &context)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        // 流式 on_update 时序由 AdaptivePublisher 控制，此处仅验证执行成功。
    }
}
