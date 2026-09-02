//! Rust 翻译自 packages/agent/src/harness/env/nodejs.ts
//!
//! 基于 `std::fs` + `std::process` 的 `FileSystem`/`Shell` 实现。

use std::io::Read;
use std::path::Path;

use pi_ai::AbortSignal;

use crate::harness::types::{
    ExecutionEnv, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo, FileKind,
    FileSystem, Shell, ShellChunkCallback, ShellExecOptions, ShellResult,
};

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

#[async_trait::async_trait]
impl FileSystem for NodeExecutionEnv {
    fn cwd(&self) -> &str {
        &self.cwd
    }

    async fn absolute_path(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError> {
        check_aborted(signal)?;
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

    async fn join_path(
        &self,
        parts: &[&str],
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError> {
        check_aborted(signal)?;
        let mut path = std::path::PathBuf::new();
        for part in parts {
            path.push(part);
        }
        Ok(path.to_string_lossy().to_string())
    }

    async fn read_text_file(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError> {
        check_aborted(signal)?;
        std::fs::read_to_string(path).map_err(|e| map_io_error(path, &e))
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        signal: Option<&AbortSignal>,
    ) -> Result<Vec<String>, FileError> {
        check_aborted(signal)?;
        let content = std::fs::read_to_string(path).map_err(|e| map_io_error(path, &e))?;
        let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        if let Some(max) = max_lines {
            lines.truncate(max);
        }
        Ok(lines)
    }

    async fn read_binary_file(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<Vec<u8>, FileError> {
        check_aborted(signal)?;
        std::fs::read(path).map_err(|e| map_io_error(path, &e))
    }

    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError> {
        check_aborted(signal)?;
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| map_io_error(path, &e))?;
        }
        std::fs::write(path, content).map_err(|e| map_io_error(path, &e))
    }

    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError> {
        check_aborted(signal)?;
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
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError> {
        check_aborted(signal)?;
        std::fs::rename(source, dest).map_err(|e| map_io_error(source, &e))
    }

    async fn file_info(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<FileInfo, FileError> {
        check_aborted(signal)?;
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

    async fn list_dir(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<Vec<FileInfo>, FileError> {
        check_aborted(signal)?;
        let entries = std::fs::read_dir(path).map_err(|e| map_io_error(path, &e))?;
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| map_io_error(path, &e))?;
            result.push(file_info_from(path, entry)?);
        }
        Ok(result)
    }

    async fn canonical_path(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError> {
        check_aborted(signal)?;
        std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().to_string())
            .map_err(|e| map_io_error(path, &e))
    }

    async fn exists(&self, path: &str, signal: Option<&AbortSignal>) -> Result<bool, FileError> {
        check_aborted(signal)?;
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
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError> {
        check_aborted(signal)?;
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
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError> {
        check_aborted(signal)?;
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
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError> {
        check_aborted(signal)?;
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
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError> {
        check_aborted(signal)?;
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
    ) -> Result<ShellResult, ExecutionError> {
        let command = command.to_string();
        let cwd = options.cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let env = options.env.clone();
        let inherit_env = options.inherit_env;
        let timeout = options.timeout;
        let abort_signal = options.abort_signal.clone();
        let on_stdout = options.on_stdout;
        let on_stderr = options.on_stderr;

        tokio::task::spawn_blocking(move || {
            exec_sync(
                &command,
                &cwd,
                env.as_ref(),
                inherit_env,
                timeout,
                abort_signal.as_ref(),
                on_stdout,
                on_stderr,
            )
        })
        .await
        .map_err(|e| {
            ExecutionError::new(
                ExecutionErrorCode::Unknown,
                format!("Shell task failed: {e}"),
            )
        })?
    }

    async fn cleanup(&self) {}
}

impl ExecutionEnv for NodeExecutionEnv {}

/// 同步执行 shell 命令：spawn + 增量读 stdout/stderr（回调上报）+ timeout/abort。
#[allow(clippy::too_many_arguments)]
fn exec_sync(
    command: &str,
    cwd: &str,
    env: Option<&std::collections::BTreeMap<String, String>>,
    _inherit_env: bool,
    timeout: Option<f64>,
    abort_signal: Option<&AbortSignal>,
    on_stdout: Option<ShellChunkCallback>,
    on_stderr: Option<ShellChunkCallback>,
) -> Result<ShellResult, ExecutionError> {
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

    let stdout_handle = std::thread::spawn(move || read_pipe(stdout_pipe, on_stdout));
    let stderr_handle = std::thread::spawn(move || read_pipe(stderr_pipe, on_stderr));

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
        if abort_signal.map(|s| s.aborted()).unwrap_or(false) {
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

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    Ok(ShellResult {
        stdout,
        stderr,
        exit_code: status.code().unwrap_or(-1),
    })
}

/// 增量读一个管道，逐 chunk 回调并返回完整内容。
fn read_pipe<R: Read>(mut pipe: R, on_chunk: Option<ShellChunkCallback>) -> String {
    let mut all = String::new();
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf) {
        if n == 0 {
            break;
        }
        let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
        if let Some(callback) = &on_chunk {
            callback(&chunk);
        }
        all.push_str(&chunk);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::harness::types::Shell;

    #[tokio::test]
    async fn exec_streams_stdout_chunks() {
        let env = NodeExecutionEnv::new(".".to_string());
        let chunks = Arc::new(Mutex::new(Vec::new()));
        let chunks_clone = chunks.clone();
        let options = ShellExecOptions {
            on_stdout: Some(Box::new(move |chunk: &str| {
                chunks_clone.lock().unwrap().push(chunk.to_string());
            })),
            ..Default::default()
        };

        let result = env.exec("printf 'line1\nline2\n'", options).await.unwrap();
        assert_eq!(result.stdout, "line1\nline2\n");
        let chunks = chunks.lock().unwrap();
        assert!(!chunks.is_empty());
        assert_eq!(chunks.concat(), "line1\nline2\n");
    }
}
