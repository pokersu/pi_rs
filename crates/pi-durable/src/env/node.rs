//! 对应 `env/node.ts`：真实文件系统与 Shell 的 `ExecutionEnv` 实现（macOS / Linux）。
//!
//! 与上游的差异：
//! - Windows 特有分支（Git Bash / WSL / `taskkill.exe`）不在本实现范围；`getShellConfig` 简化为
//!   `/bin/bash` → `sh`。
//! - 进程树终止用 `libc::kill(-pid, SIGKILL)`（对进程组），fallback 到单进程。
//! - `ok`/`err` 的 `Result` 枚举 → Rust 的 `Result`（见 `env/index.rs`）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::chord::context::Context;
use crate::env::decode::StreamDecoder;
use crate::env::line_scan::LineScanner;
use crate::env::node_watch::NodeWatchOptions;
use crate::env::{
    BinaryReader, DirReader, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode,
    FileInfo, FileKind, FileSystem, FileWatcher, LineScan, RemoveOptions, Shell, ShellCommand,
    ShellExecOptions, ShellExecResult, ShellOutputInfo, ShellOutputStream, TextLine,
    TextLineReader, WatchChange, WatchTarget,
};

const MAX_TIMEOUT_MS: f64 = 2_147_483_647.0;
/// `BinaryReader` 的单次最大读取，避免大 `length` 分配超过文件实际大小。
const BINARY_READ_CHUNK: usize = 1024 * 1024;
const TEXT_LINE_CHUNK: usize = 64 * 1024;

/// 对应 `resolveTimeoutMs`。
fn resolve_timeout_ms(timeout: Option<f64>) -> Result<Option<u64>, ExecutionError> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Timeout,
            "Invalid timeout: must be a finite number of seconds",
        ));
    }
    let timeout_ms = timeout * 1000.0;
    if timeout_ms > MAX_TIMEOUT_MS {
        return Err(ExecutionError::new(
            ExecutionErrorCode::Timeout,
            format!(
                "Invalid timeout: maximum is {} seconds",
                MAX_TIMEOUT_MS / 1000.0
            ),
        ));
    }
    Ok(Some(timeout_ms as u64))
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 对应 `resolvePath`：处理 `~`、`file://` 与相对路径。
fn resolve_path(cwd: &str, path: &str) -> String {
    let normalized = if path == "~" {
        home_dir().to_string_lossy().into_owned()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest).to_string_lossy().into_owned()
    } else if let Some(rest) = path.strip_prefix("file://") {
        rest.to_string()
    } else {
        path.to_string()
    };
    let normalized = Path::new(&normalized);
    if normalized.is_absolute() {
        normalized.to_string_lossy().into_owned()
    } else {
        Path::new(cwd)
            .join(normalized)
            .to_string_lossy()
            .into_owned()
    }
}

fn file_kind_from_metadata(metadata: &std::fs::Metadata) -> Option<FileKind> {
    if metadata.is_file() {
        Some(FileKind::File)
    } else if metadata.is_dir() {
        Some(FileKind::Directory)
    } else if metadata.is_symlink() {
        Some(FileKind::Symlink)
    } else {
        None
    }
}

fn file_info_from_metadata(
    path: &str,
    metadata: &std::fs::Metadata,
) -> Result<FileInfo, FileError> {
    let kind = file_kind_from_metadata(metadata).ok_or_else(|| {
        FileError::new(
            FileErrorCode::Invalid,
            "Unsupported file type",
            Some(path.to_string()),
        )
    })?;
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(FileInfo {
        name,
        path: path.to_string(),
        kind,
        size: metadata.len(),
        mtime_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as f64)
            .unwrap_or(0.0),
    })
}

/// 对应 `toFileError`：把 `std::io::Error` 映射为 [`FileError`]。
fn to_file_error(error: std::io::Error, fallback_path: Option<&str>) -> FileError {
    let message = error.to_string();
    let path = fallback_path.map(str::to_string);
    let code = match error.raw_os_error() {
        Some(libc::ENOENT) => FileErrorCode::NotFound,
        Some(libc::EACCES) | Some(libc::EPERM) => FileErrorCode::PermissionDenied,
        Some(libc::ENOTDIR) => FileErrorCode::NotDirectory,
        Some(libc::EISDIR) => FileErrorCode::IsDirectory,
        Some(libc::EINVAL) => FileErrorCode::Invalid,
        _ => FileErrorCode::Unknown,
    };
    FileError::new(code, message, path)
}

/// 对应 `abortResult`：已取消时返回 `aborted` 错误。
fn aborted(path: Option<&str>) -> Option<FileError> {
    None.or_else(|| {
        path.map(|path| FileError::new(FileErrorCode::Aborted, "aborted", Some(path.to_string())))
    })
}

/// 对应 `killProcessTree`（macOS / Linux）。
fn kill_process_tree(pid: u32) {
    unsafe {
        if libc::kill(-(pid as i32), libc::SIGKILL) != 0 {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

fn closed_result<T>(what: &str, path: &str) -> Result<T, FileError> {
    Err(FileError::new(
        FileErrorCode::Invalid,
        format!("{what} is closed"),
        Some(path.to_string()),
    ))
}

fn symlink_refused(path: &str) -> FileError {
    FileError::new(
        FileErrorCode::Invalid,
        "Refusing to follow a symbolic link",
        Some(path.to_string()),
    )
}

/// 对应上游退出码计算：进程被信号终止时返回 128 + 信号编号（避免把 OOM 等误认为成功退出）。
fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

/// 对应 `NodeTextLineReader`。
struct NodeTextLineReader {
    file: std::fs::File,
    path: String,
    state: std::sync::Mutex<TextLineReaderState>,
}

struct TextLineReaderState {
    decoder: StreamDecoder,
    byte_offset: u64,
    buffered: String,
    ended: bool,
    closed: bool,
}

impl NodeTextLineReader {
    fn new(file: std::fs::File, path: String) -> Self {
        Self {
            file,
            path,
            state: std::sync::Mutex::new(TextLineReaderState {
                decoder: StreamDecoder::new(),
                byte_offset: 0,
                buffered: String::new(),
                ended: false,
                closed: false,
            }),
        }
    }

    fn read_line_sync(&self, context: &dyn Context) -> Result<Option<TextLine>, FileError> {
        let mut state = self.state.lock().expect("reader state");
        if state.closed {
            return closed_result("Text line reader", &self.path);
        }
        loop {
            if let Some(newline) = state.buffered.find('\n') {
                let text = state.buffered[..newline].to_string();
                state.buffered.drain(..newline + 1);
                return Ok(Some(TextLine {
                    text,
                    terminated: true,
                }));
            }
            if state.ended {
                return if state.buffered.is_empty() {
                    Ok(None)
                } else {
                    let text = std::mem::take(&mut state.buffered);
                    Ok(Some(TextLine {
                        text,
                        terminated: false,
                    }))
                };
            }
            if context
                .abort_signal()
                .is_some_and(|signal| signal.aborted())
            {
                return Err(FileError::new(
                    FileErrorCode::Aborted,
                    "aborted",
                    Some(self.path.clone()),
                ));
            }
            let mut chunk = [0u8; TEXT_LINE_CHUNK];
            let bytes_read = read_at(&self.file, &mut chunk, state.byte_offset)
                .map_err(|error| to_file_error(error, Some(&self.path)))?;
            if context
                .abort_signal()
                .is_some_and(|signal| signal.aborted())
            {
                return Err(FileError::new(
                    FileErrorCode::Aborted,
                    "aborted",
                    Some(self.path.clone()),
                ));
            }
            state.byte_offset += bytes_read as u64;
            if bytes_read == 0 {
                let tail = state.decoder.decode(None);
                state.buffered.push_str(&tail);
                state.ended = true;
            } else {
                let text = state.decoder.decode(Some(&chunk[..bytes_read]));
                state.buffered.push_str(&text);
            }
        }
    }
}

#[async_trait::async_trait]
impl TextLineReader for NodeTextLineReader {
    async fn read_line(&self, context: &dyn Context) -> Result<Option<TextLine>, FileError> {
        self.read_line_sync(context)
    }

    async fn close(&self, _context: &dyn Context) {
        let mut state = self.state.lock().expect("reader state");
        if !state.closed {
            state.closed = true;
            state.buffered.clear();
        }
    }
}

/// 用一个同步读辅助在给定偏移读取。
fn read_at(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buffer, offset)
}

/// 对应 `NodeBinaryReader`。
struct NodeBinaryReader {
    file: std::fs::File,
    path: String,
    closed: Mutex<bool>,
}

impl NodeBinaryReader {
    fn new(file: std::fs::File, path: String) -> Self {
        Self {
            file,
            path,
            closed: Mutex::new(false),
        }
    }
}

#[async_trait::async_trait]
impl BinaryReader for NodeBinaryReader {
    async fn info(&self, context: &dyn Context) -> Result<FileInfo, FileError> {
        if *self.closed.lock().expect("closed") {
            return closed_result("Binary reader", &self.path);
        }
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&self.path)).expect("aborted"));
        }
        let metadata = self
            .file
            .metadata()
            .map_err(|error| to_file_error(error, Some(&self.path)))?;
        file_info_from_metadata(&self.path, &metadata)
    }

    async fn read(
        &self,
        offset: u64,
        length: usize,
        context: &dyn Context,
    ) -> Result<Vec<u8>, FileError> {
        if *self.closed.lock().expect("closed") {
            return closed_result("Binary reader", &self.path);
        }
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&self.path)).expect("aborted"));
        }
        let mut total = 0usize;
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        while total < length {
            let chunk_len = (length - total).min(BINARY_READ_CHUNK);
            let mut chunk = vec![0u8; chunk_len];
            let bytes_read = read_at(&self.file, &mut chunk, offset + total as u64)
                .map_err(|error| to_file_error(error, Some(&self.path)))?;
            if context
                .abort_signal()
                .is_some_and(|signal| signal.aborted())
            {
                return Err(aborted(Some(&self.path)).expect("aborted"));
            }
            if bytes_read == 0 {
                break;
            }
            chunk.truncate(bytes_read);
            total += bytes_read;
            chunks.push(chunk);
        }
        let mut bytes = Vec::with_capacity(total);
        for chunk in chunks {
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn scan_lines(
        &self,
        start_line: usize,
        end_line: Option<usize>,
        context: &dyn Context,
    ) -> Result<LineScan, FileError> {
        if *self.closed.lock().expect("closed") {
            return closed_result("Binary reader", &self.path);
        }
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&self.path)).expect("aborted"));
        }
        let mut scanner = LineScanner::new(start_line, end_line);
        let mut chunk = vec![0u8; TEXT_LINE_CHUNK];
        let mut position = 0u64;
        loop {
            let bytes_read = read_at(&self.file, &mut chunk, position)
                .map_err(|error| to_file_error(error, Some(&self.path)))?;
            if context
                .abort_signal()
                .is_some_and(|signal| signal.aborted())
            {
                return Err(aborted(Some(&self.path)).expect("aborted"));
            }
            if bytes_read == 0 {
                return Ok(scanner.finish());
            }
            scanner.push(&chunk[..bytes_read]);
            position += bytes_read as u64;
        }
    }

    async fn close(&self, _context: &dyn Context) {
        let mut closed = self.closed.lock().expect("closed");
        if !*closed {
            *closed = true;
        }
    }
}

/// 对应 `NodeDirReader`。
struct NodeDirReader {
    entries: Mutex<std::vec::IntoIter<(PathBuf, std::fs::Metadata)>>,
    path: String,
    done: bool,
    closed: bool,
}

#[async_trait::async_trait]
impl DirReader for NodeDirReader {
    async fn next(
        &self,
        max_entries: usize,
        context: &dyn Context,
    ) -> Result<(Vec<FileInfo>, bool), FileError> {
        if self.closed {
            return closed_result("Directory reader", &self.path);
        }
        if max_entries == 0 {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "maxEntries must be a positive safe integer",
                Some(self.path.clone()),
            ));
        }
        let mut entries = Vec::new();
        let mut done = self.done;
        let mut iterator = self.entries.lock().expect("entries");
        while !done && entries.len() < max_entries {
            let Some((entry_path, metadata)) = iterator.next() else {
                done = true;
                break;
            };
            if context
                .abort_signal()
                .is_some_and(|signal| signal.aborted())
            {
                return Err(aborted(Some(&self.path)).expect("aborted"));
            }
            let path = entry_path.to_string_lossy().into_owned();
            if let Ok(info) = file_info_from_metadata(&path, &metadata) {
                entries.push(info);
            }
        }
        Ok((entries, done))
    }

    async fn close(&self, _context: &dyn Context) {}
}

/// 生成进程内唯一的临时文件/目录后缀。
fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// 对应 `createTempFile`。
fn write_temp_file(prefix: Option<&str>, suffix: Option<&str>) -> std::io::Result<PathBuf> {
    let name = format!(
        "{}{}{}",
        prefix.unwrap_or(""),
        unique_id(),
        suffix.unwrap_or("")
    );
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, "")?;
    Ok(path)
}

fn create_temp_file_impl(prefix: Option<&str>, suffix: Option<&str>) -> Result<String, FileError> {
    write_temp_file(prefix, suffix)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| to_file_error(error, None))
}

/// 对应 `NodeExecutionEnv`。
pub struct NodeExecutionEnv {
    cwd: String,
    shell_path: Option<String>,
    watch_options: NodeWatchOptions,
    active_child_pids: Mutex<HashSet<u32>>,
}

impl NodeExecutionEnv {
    /// 构造本地环境。
    pub fn new(cwd: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            shell_path: None,
            watch_options: NodeWatchOptions::default(),
            active_child_pids: Mutex::new(HashSet::new()),
        }
    }

    /// 指定自定义 Shell 路径。
    pub fn with_shell(mut self, shell_path: impl Into<String>) -> Self {
        self.shell_path = Some(shell_path.into());
        self
    }

    /// 指定监视选项（对应 `options.watch`）。
    pub fn with_watch_options(mut self, watch_options: NodeWatchOptions) -> Self {
        self.watch_options = watch_options;
        self
    }

    fn resolve(&self, path: &str) -> String {
        resolve_path(&self.cwd, path)
    }
}

#[async_trait::async_trait]
impl FileSystem for NodeExecutionEnv {
    fn id(&self) -> &str {
        "node:local"
    }

    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    async fn absolute_path(&self, path: &str, _context: &dyn Context) -> Result<String, FileError> {
        Ok(self.resolve(path))
    }

    async fn join_path(&self, parts: &[&str], _context: &dyn Context) -> Result<String, FileError> {
        let mut path = PathBuf::new();
        for part in parts {
            path.push(part);
        }
        Ok(path.to_string_lossy().into_owned())
    }

    async fn read_text_file(&self, path: &str, context: &dyn Context) -> Result<String, FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        let bytes =
            std::fs::read(&resolved).map_err(|error| to_file_error(error, Some(&resolved)))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn open_text_line_reader(
        &self,
        path: &str,
        context: &dyn Context,
    ) -> Result<Box<dyn TextLineReader>, FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        let file = std::fs::File::open(&resolved)
            .map_err(|error| to_file_error(error, Some(&resolved)))?;
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        Ok(Box::new(NodeTextLineReader::new(file, resolved)))
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        context: &dyn Context,
    ) -> Result<Vec<String>, FileError> {
        if max_lines == Some(0) {
            return Ok(Vec::new());
        }
        let reader = self.open_text_line_reader(path, context).await?;
        let mut lines = Vec::new();
        while max_lines.is_none_or(|max| lines.len() < max) {
            let Some(line) = reader.read_line(context).await? else {
                break;
            };
            lines.push(line.text);
        }
        reader.close(context).await;
        Ok(lines)
    }

    async fn read_binary_file(
        &self,
        path: &str,
        context: &dyn Context,
    ) -> Result<Vec<u8>, FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        std::fs::read(&resolved).map_err(|error| to_file_error(error, Some(&resolved)))
    }

    async fn open_binary_reader(
        &self,
        path: &str,
        no_follow: Option<bool>,
        context: &dyn Context,
    ) -> Result<Box<dyn BinaryReader>, FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        // noFollow：打开前检查最终组件是否为符号链接（对应上游 O_NOFOLLOW 的 ELOOP/EMLINK 报告）。
        if no_follow == Some(true)
            && let Ok(link_metadata) = std::fs::symlink_metadata(&resolved)
            && link_metadata.file_type().is_symlink()
        {
            return Err(symlink_refused(&resolved));
        }
        let file = std::fs::File::open(&resolved)
            .map_err(|error| to_file_error(error, Some(&resolved)))?;
        let metadata = file
            .metadata()
            .map_err(|error| to_file_error(error, Some(&resolved)))?;
        if !metadata.is_file() {
            return Err(if metadata.is_dir() {
                FileError::new(
                    FileErrorCode::IsDirectory,
                    "EISDIR: illegal operation on a directory, read",
                    Some(resolved),
                )
            } else {
                FileError::new(FileErrorCode::Invalid, "Not a regular file", Some(resolved))
            });
        }
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        Ok(Box::new(NodeBinaryReader::new(file, resolved)))
    }

    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        context: &dyn Context,
    ) -> Result<(), FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        if let Some(parent) = Path::new(&resolved).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| to_file_error(error, Some(&resolved)))?;
        }
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        std::fs::write(&resolved, content).map_err(|error| to_file_error(error, Some(&resolved)))
    }

    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        context: &dyn Context,
    ) -> Result<(), FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        if let Some(parent) = Path::new(&resolved).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| to_file_error(error, Some(&resolved)))?;
        }
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved)
            .map_err(|error| to_file_error(error, Some(&resolved)))?;
        file.write_all(content)
            .map_err(|error| to_file_error(error, Some(&resolved)))
    }

    async fn truncate_file(
        &self,
        path: &str,
        size: u64,
        context: &dyn Context,
    ) -> Result<(), FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&resolved)
            .map_err(|error| to_file_error(error, Some(&resolved)))?;
        file.set_len(size)
            .map_err(|error| to_file_error(error, Some(&resolved)))
    }

    async fn flush_file(&self, path: &str, context: &dyn Context) -> Result<(), FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&resolved)
            .map_err(|error| to_file_error(error, Some(&resolved)))?;
        file.sync_all()
            .map_err(|error| to_file_error(error, Some(&resolved)))
    }

    async fn rename_file(
        &self,
        source_path: &str,
        destination_path: &str,
        context: &dyn Context,
    ) -> Result<(), FileError> {
        let source = self.resolve(source_path);
        let destination = self.resolve(destination_path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&destination)).expect("aborted"));
        }
        std::fs::rename(&source, &destination).map_err(|error| to_file_error(error, Some(&source)))
    }

    async fn file_info(&self, path: &str, context: &dyn Context) -> Result<FileInfo, FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        let metadata = std::fs::symlink_metadata(&resolved)
            .map_err(|error| to_file_error(error, Some(&resolved)))?;
        file_info_from_metadata(&resolved, &metadata)
    }

    async fn list_dir(
        &self,
        path: &str,
        context: &dyn Context,
    ) -> Result<Vec<FileInfo>, FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        let mut infos = Vec::new();
        for entry in
            std::fs::read_dir(&resolved).map_err(|error| to_file_error(error, Some(&resolved)))?
        {
            if context
                .abort_signal()
                .is_some_and(|signal| signal.aborted())
            {
                return Err(aborted(Some(&resolved)).expect("aborted"));
            }
            let entry = entry.map_err(|error| to_file_error(error, Some(&resolved)))?;
            let entry_path = entry.path();
            let metadata = std::fs::symlink_metadata(&entry_path)
                .map_err(|error| to_file_error(error, Some(&resolved)))?;
            if let Ok(info) = file_info_from_metadata(&entry_path.to_string_lossy(), &metadata) {
                infos.push(info);
            }
        }
        Ok(infos)
    }

    async fn open_dir_reader(
        &self,
        path: &str,
        context: &dyn Context,
    ) -> Result<Box<dyn DirReader>, FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        let mut entries = Vec::new();
        for entry in
            std::fs::read_dir(&resolved).map_err(|error| to_file_error(error, Some(&resolved)))?
        {
            let entry = entry.map_err(|error| to_file_error(error, Some(&resolved)))?;
            let entry_path = entry.path();
            if let Ok(metadata) = std::fs::symlink_metadata(&entry_path) {
                entries.push((entry_path, metadata));
            }
        }
        Ok(Box::new(NodeDirReader {
            entries: Mutex::new(entries.into_iter()),
            path: resolved,
            done: false,
            closed: false,
        }))
    }

    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: Box<dyn Fn(WatchChange) + Send + Sync>,
        context: &dyn Context,
    ) -> Result<Box<dyn FileWatcher>, FileError> {
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(None).expect("aborted"));
        }
        let cwd = self.cwd.clone();
        let watch_options = self.watch_options.clone();
        let watcher = crate::env::node_watch::NodeFileWatcher::open(
            targets,
            move |path| resolve_path(&cwd, path),
            on_change,
            watch_options,
        )
        .await?;
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            watcher.close(context).await;
            return Err(aborted(None).expect("aborted"));
        }
        Ok(Box::new(watcher))
    }

    async fn canonical_path(&self, path: &str, context: &dyn Context) -> Result<String, FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        std::fs::canonicalize(&resolved)
            .map(|path| path.to_string_lossy().into_owned())
            .map_err(|error| to_file_error(error, Some(&resolved)))
    }

    async fn exists(&self, path: &str, context: &dyn Context) -> Result<bool, FileError> {
        match self.file_info(path, context).await {
            Ok(_) => Ok(true),
            Err(error) if error.code == FileErrorCode::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn create_dir(
        &self,
        path: &str,
        recursive: Option<bool>,
        context: &dyn Context,
    ) -> Result<(), FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        let result = if recursive.unwrap_or(true) {
            std::fs::create_dir_all(&resolved)
        } else {
            std::fs::create_dir(&resolved)
        };
        result.map_err(|error| to_file_error(error, Some(&resolved)))
    }

    async fn remove(
        &self,
        path: &str,
        options: RemoveOptions,
        context: &dyn Context,
    ) -> Result<(), FileError> {
        let resolved = self.resolve(path);
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(Some(&resolved)).expect("aborted"));
        }
        let result = if options.recursive == Some(true) {
            std::fs::remove_dir_all(&resolved)
        } else {
            let metadata = std::fs::symlink_metadata(&resolved);
            match metadata {
                Ok(metadata) if metadata.is_dir() => std::fs::remove_dir(&resolved),
                _ => std::fs::remove_file(&resolved),
            }
        };
        match result {
            Ok(()) => Ok(()),
            Err(error)
                if options.force == Some(true) && error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(())
            }
            Err(error) => Err(to_file_error(error, Some(&resolved))),
        }
    }

    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        context: &dyn Context,
    ) -> Result<String, FileError> {
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(None).expect("aborted"));
        }
        let mut builder = tempfile::Builder::new();
        builder.prefix(prefix.unwrap_or("tmp-"));
        builder
            .tempdir()
            .map(|dir| {
                let path = dir.keep();
                path.to_string_lossy().into_owned()
            })
            .map_err(|error| to_file_error(error, None))
    }

    async fn create_temp_file(
        &self,
        prefix: Option<&str>,
        suffix: Option<&str>,
        context: &dyn Context,
    ) -> Result<String, FileError> {
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(aborted(None).expect("aborted"));
        }
        create_temp_file_impl(prefix, suffix)
    }

    async fn cleanup(&self, _context: &dyn Context) {
        let pids: Vec<u32> = self
            .active_child_pids
            .lock()
            .expect("pids")
            .drain()
            .collect();
        for pid in pids {
            kill_process_tree(pid);
        }
    }
}

#[async_trait::async_trait]
impl Shell for NodeExecutionEnv {
    async fn exec(
        &self,
        command: ShellCommand,
        options: ShellExecOptions,
        context: &dyn Context,
    ) -> Result<ShellExecResult, ExecutionError> {
        if context
            .abort_signal()
            .is_some_and(|signal| signal.aborted())
        {
            return Err(ExecutionError::new(ExecutionErrorCode::Aborted, "aborted"));
        }
        let timeout_ms = resolve_timeout_ms(options.timeout)?;
        let cwd = options
            .cwd
            .as_deref()
            .map_or_else(|| self.cwd.clone(), |cwd| self.resolve(cwd));

        // 字符串走 Shell；argv 数组直接运行程序。
        let (program, args): (String, Vec<String>) = match command {
            ShellCommand::Shell(command) => {
                let shell = self
                    .shell_path
                    .clone()
                    .unwrap_or_else(|| "/bin/bash".to_string());
                (shell, vec!["-c".to_string(), command])
            }
            ShellCommand::Argv(argv) => {
                let Some((first, rest)) = argv.split_first() else {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::SpawnError,
                        "Empty argv: no program to run",
                    ));
                };
                (first.clone(), rest.to_vec())
            }
        };

        // 工作目录必须存在。
        if !Path::new(&cwd).is_dir() {
            return Err(ExecutionError::new(
                ExecutionErrorCode::SpawnError,
                format!("Working directory does not exist: {cwd}\nCannot execute bash commands."),
            ));
        }

        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&args);
        cmd.current_dir(&cwd);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // Unix：让子进程成为新进程组 leader，以便按组终止。
        cmd.process_group(0);
        // 环境：默认继承，`env` 覆盖。
        if let Some(extra) = &options.env {
            for (key, value) in extra {
                cmd.env(key, value);
            }
        }
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::SpawnError,
                    error.to_string(),
                ));
            }
        };
        if let Some(pid) = child.id() {
            self.active_child_pids.lock().expect("pids").insert(pid);
        }

        let signal = context.abort_signal();
        let mut timed_out = false;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let mut stdout_decoder = StreamDecoder::new();
        let mut stderr_decoder = StreamDecoder::new();
        let mut collected: Vec<u8> = Vec::new();
        let mut seen_bytes = 0usize;
        let mut seen_newlines = 0usize;

        let spill = options.spill;
        let mut spill_path: Option<String> = None;

        let on_output = options.on_output;

        let emit =
            |text: String, stream: ShellOutputStream, info: Option<crate::env::ShellOutputSkip>| {
                if let Some(on_output) = &on_output {
                    on_output(
                        &text,
                        context,
                        &ShellOutputInfo {
                            stream,
                            skipped: info,
                        },
                    );
                }
            };

        // 读取两个流直到结束或取消/超时。
        let drain = async {
            match (stdout, stderr) {
                (Some(mut stdout), Some(mut stderr)) => {
                    use tokio::io::AsyncReadExt;
                    let mut stdout_buf = [0u8; 8192];
                    let mut stderr_buf = [0u8; 8192];
                    loop {
                        tokio::select! {
                            read = stdout.read(&mut stdout_buf) => {
                                match read {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        let text = stdout_decoder.decode(Some(&stdout_buf[..n]));
                                        collected.extend_from_slice(&stdout_buf[..n]);
                                        seen_bytes += n;
                                        seen_newlines += stdout_buf[..n].iter().filter(|&&b| b == b'\n').count();
                                        emit(text, ShellOutputStream::Stdout, None);
                                    }
                                    Err(_) => break,
                                }
                            }
                            read = stderr.read(&mut stderr_buf) => {
                                match read {
                                    Ok(0) => {}
                                    Ok(n) => {
                                        let text = stderr_decoder.decode(Some(&stderr_buf[..n]));
                                        collected.extend_from_slice(&stderr_buf[..n]);
                                        seen_bytes += n;
                                        seen_newlines += stderr_buf[..n].iter().filter(|&&b| b == b'\n').count();
                                        emit(text, ShellOutputStream::Stderr, None);
                                    }
                                    Err(_) => {}
                                }
                            }
                        }
                    }
                    // flush 两个 decoder 的尾部。
                    emit(stdout_decoder.decode(None), ShellOutputStream::Stdout, None);
                    emit(stderr_decoder.decode(None), ShellOutputStream::Stderr, None);
                }
                (Some(mut stdout), None) => {
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 8192];
                    loop {
                        match stdout.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let text = stdout_decoder.decode(Some(&buf[..n]));
                                collected.extend_from_slice(&buf[..n]);
                                seen_bytes += n;
                                seen_newlines += buf[..n].iter().filter(|&&b| b == b'\n').count();
                                emit(text, ShellOutputStream::Stdout, None);
                            }
                        }
                    }
                    emit(stdout_decoder.decode(None), ShellOutputStream::Stdout, None);
                }
                _ => {}
            }
        };

        let interrupt = async {
            let mut fired = false;
            if let Some(signal) = &signal {
                if signal.aborted() {
                    fired = true;
                } else {
                    signal.cancelled().await;
                    fired = true;
                }
            } else {
                std::future::pending::<()>().await;
            }
            fired
        };

        let timeout_fut = async {
            if let Some(timeout_ms) = timeout_ms {
                tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
                true
            } else {
                std::future::pending::<()>().await;
                false
            }
        };

        tokio::select! {
            _ = drain => {}
            _ = interrupt => {
                timed_out = false;
                if let Some(pid) = child.id() {
                    kill_process_tree(pid);
                }
                // 等待子进程真正退出。
                let _ = child.wait().await;
            }
            _ = timeout_fut => {
                timed_out = true;
                if let Some(pid) = child.id() {
                    kill_process_tree(pid);
                }
                let _ = child.wait().await;
            }
        }

        // 溢出：超过阈值时把完整输出写到临时文件。
        if let Some(spill) = spill {
            let lines = seen_newlines
                + usize::from(!collected.is_empty() && collected.last() != Some(&b'\n'));
            if (seen_bytes > spill.after_bytes || lines > spill.after_lines)
                && let Ok(path) = self
                    .create_temp_file(Some("pi-output-"), Some(".log"), context)
                    .await
                && std::fs::write(&path, &collected).is_ok()
            {
                spill_path = Some(path);
            }
        }

        let status = child.wait().await;
        if let Some(pid) = child.id() {
            self.active_child_pids.lock().expect("pids").remove(&pid);
        }

        if timed_out {
            return Err(ExecutionError {
                code: ExecutionErrorCode::Timeout,
                message: format!("timeout:{}", options.timeout.unwrap_or(0.0)),
                spill_path: spill_path.clone(),
            });
        }
        if let Some(signal) = &signal
            && signal.aborted()
        {
            return Err(ExecutionError {
                code: ExecutionErrorCode::Aborted,
                message: "aborted".to_string(),
                spill_path: spill_path.clone(),
            });
        }
        // 对应上游：进程被信号终止时退出码为 128 + 信号编号（避免把 OOM 等误认为成功退出）。
        let exit_code = match status {
            Ok(status) => exit_code_of(status),
            Err(_) => 1,
        };
        Ok(ShellExecResult {
            exit_code,
            spill_path,
        })
    }

    async fn cleanup(&self, _context: &dyn Context) {
        let pids: Vec<u32> = self
            .active_child_pids
            .lock()
            .expect("pids")
            .drain()
            .collect();
        for pid in pids {
            kill_process_tree(pid);
        }
    }
}
