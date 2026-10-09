//! 对应 `src/env/index.ts`：可移植的文件系统与 Shell 能力契约。
//!
//! 上游用 `Result<TValue, TError>` 的标签联合表示「失败而不抛出」；Rust 直接用标准库
//! `Result<T, E>`，因此 `ok`/`err`/`getOrThrow`/`getOrUndefined` 这几个辅助函数**不需要移植**。

use serde::{Deserialize, Serialize};

use crate::chord::context::Context;

// ─── 文件错误 ────────────────────────────────────────────────────────────────

/// 对应 `FileKind`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    /// 普通文件。
    File,
    /// 目录。
    Directory,
    /// 符号链接。
    Symlink,
}

/// 对应 `FileErrorCode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileErrorCode {
    /// 操作被取消。
    Aborted,
    /// 路径不存在。
    NotFound,
    /// 权限不足。
    PermissionDenied,
    /// 期望目录但不是。
    NotDirectory,
    /// 期望文件但是目录。
    IsDirectory,
    /// 参数或状态非法。
    Invalid,
    /// 后端不支持该操作。
    NotSupported,
    /// 其他。
    Unknown,
}

/// 对应 `FileError`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileError {
    /// 错误码。
    pub code: FileErrorCode,
    /// 错误消息。
    pub message: String,
    /// 相关路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl FileError {
    /// 对应 `new FileError(code, message, path?)`。
    pub fn new(code: FileErrorCode, message: impl Into<String>, path: Option<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path,
        }
    }
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {path}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for FileError {}

// ─── 执行错误 ────────────────────────────────────────────────────────────────

/// 对应 `ExecutionErrorCode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionErrorCode {
    /// 被取消。
    Aborted,
    /// 超时。
    Timeout,
    /// Shell 不可用。
    ShellUnavailable,
    /// 进程启动失败。
    SpawnError,
    /// 回调失败。
    CallbackError,
    /// 其他。
    Unknown,
}

/// 对应 `ExecutionError`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionError {
    /// 错误码。
    pub code: ExecutionErrorCode,
    /// 错误消息。
    pub message: String,
    /// 超时或被取消、且输出已越过 spill 阈值时的溢出文件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spill_path: Option<String>,
}

impl ExecutionError {
    /// 对应 `new ExecutionError(code, message, cause?)`。
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            spill_path: None,
        }
    }
}

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ExecutionError {}

// ─── 文件信息与读取 ──────────────────────────────────────────────────────────

/// 对应 `FileInfo`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    /// 文件名。
    pub name: String,
    /// 完整路径。
    pub path: String,
    /// 类型。
    pub kind: FileKind,
    /// 字节大小。
    pub size: u64,
    /// 修改时间（毫秒）。
    pub mtime_ms: f64,
}

/// 对应 `TextLine`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextLine {
    /// 行文本（不含结尾换行）。
    pub text: String,
    /// 该行是否以换行结束。
    pub terminated: bool,
}

/// 对应 `TextLineReader`。
#[async_trait::async_trait]
pub trait TextLineReader: Send + Sync {
    /// 读下一行；`None` 表示已到末尾。
    async fn read_line(&self, context: &dyn Context) -> Result<Option<TextLine>, FileError>;

    /// 关闭读取器。
    async fn close(&self, context: &dyn Context);
}

/// 对应 `LineScan`：一次遍历得到的行定位结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineScan {
    /// 整个文件的换行字节数（文件有 `newlines + 1` 行）。
    pub newlines: usize,
    /// 所选行的字节范围起点。
    pub start: usize,
    /// 所选行的字节范围终点（不含结尾换行）。
    pub end: usize,
    /// 首个所选行的结束位置。
    pub first_line_end: usize,
    /// 最后一个所选行的起始位置。
    pub last_line_start: usize,
    /// 所选范围解码后的 UTF-8 字节数。
    pub selected_bytes: usize,
    /// 首行的 UTF-8 字节数。
    pub first_line_bytes: usize,
}

/// 对应 `BinaryReader`：对一个已打开常规文件的位置读取。
#[async_trait::async_trait]
pub trait BinaryReader: Send + Sync {
    /// 已打开文件的元数据（而非其路径现在指向的对象）。
    async fn info(&self, context: &dyn Context) -> Result<FileInfo, FileError>;

    /// 从 `offset` 起最多 `length` 字节。
    async fn read(
        &self,
        offset: u64,
        length: usize,
        context: &dyn Context,
    ) -> Result<Vec<u8>, FileError>;

    /// 单次遍历定位 `[start_line, end_line)` 行（`end_line` 缺省为到末尾，0 基）。
    async fn scan_lines(
        &self,
        start_line: usize,
        end_line: Option<usize>,
        context: &dyn Context,
    ) -> Result<LineScan, FileError>;

    /// 关闭读取器。
    async fn close(&self, context: &dyn Context);
}

// ─── 监听 ────────────────────────────────────────────────────────────────────

/// 对应 `WatchTarget`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    /// 目标路径（可以不存在；创建它即为一次变更）。
    pub path: String,
    /// 是否递归监听目录下的全部内容。
    pub recursive: Option<bool>,
    /// 监听时要排除的条目。
    pub exclude: Option<WatchExclude>,
}

/// 对应 `WatchTarget.exclude`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchExclude {
    /// 是否排除以 `.` 开头的条目。
    pub hidden: Option<bool>,
    /// 额外排除的名字。
    pub names: Option<Vec<String>>,
}

/// 对应 `WatchChange`。
#[derive(Debug, Clone, PartialEq)]
pub enum WatchChange {
    /// 这些路径处或其下可能发生了变化。
    Paths(Vec<String>),
    /// 覆盖范围一度不确定，需要重新扫描。
    Overflow,
    /// 监听器已停止。
    Error(FileError),
}

/// 对应 `FileWatcher.mode`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    /// 变化约在两秒内上报。
    Native,
    /// 环境通过快照对比发现变化（网络/FUSE 文件系统）；两次快照之间被撤销的变化可能遗漏。
    Polling,
}

/// 对应 `FileWatcher`。
#[async_trait::async_trait]
pub trait FileWatcher: Send + Sync {
    /// 对应 `mode`。
    fn mode(&self) -> WatchMode;

    /// 对应 `close`：停止监听；幂等。
    async fn close(&self, context: &dyn Context);
}

// ─── 目录读取 ────────────────────────────────────────────────────────────────

/// 对应 `DirReader`：目录条目的分页读取。
#[async_trait::async_trait]
pub trait DirReader: Send + Sync {
    /// 读取下一页（最多 `max_entries` 条），`done` 标记结束。
    async fn next(
        &self,
        max_entries: usize,
        context: &dyn Context,
    ) -> Result<(Vec<FileInfo>, bool), FileError>;

    /// 关闭读取器。
    async fn close(&self, context: &dyn Context);
}

// ─── 文件系统 ────────────────────────────────────────────────────────────────

/// 对应 `FileSystem`：可移植的文件系统能力。操作返回失败而不是抛出。
///
/// # 与上游的差异
///
/// 上游把 `{ maxLines? }` / `{ noFollow? }` / `{ recursive? }` 等选项包成对象字面量；
/// Rust 用 `Option<T>` 参数（单字段选项）或者具名结构体（多字段）。
#[async_trait::async_trait]
pub trait FileSystem: Send + Sync {
    /// 对应 `id`：文件命名空间标识。
    fn id(&self) -> &str;

    /// 对应 `cwd`。
    fn cwd(&self) -> String;

    /// 对应 `absolutePath`。
    async fn absolute_path(&self, path: &str, context: &dyn Context) -> Result<String, FileError>;

    /// 对应 `joinPath`。
    async fn join_path(&self, parts: &[&str], context: &dyn Context) -> Result<String, FileError>;

    /// 对应 `readTextFile`。
    async fn read_text_file(&self, path: &str, context: &dyn Context) -> Result<String, FileError>;

    /// 对应 `openTextLineReader`。
    async fn open_text_line_reader(
        &self,
        path: &str,
        context: &dyn Context,
    ) -> Result<Box<dyn TextLineReader>, FileError>;

    /// 对应 `readTextLines`（`maxLines` 为 `None` 时读全部）。
    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        context: &dyn Context,
    ) -> Result<Vec<String>, FileError>;

    /// 对应 `readBinaryFile`。
    async fn read_binary_file(
        &self,
        path: &str,
        context: &dyn Context,
    ) -> Result<Vec<u8>, FileError>;

    /// 对应 `openBinaryReader`（`noFollow` 为 `None` 时跟随符号链接）。
    async fn open_binary_reader(
        &self,
        path: &str,
        no_follow: Option<bool>,
        context: &dyn Context,
    ) -> Result<Box<dyn BinaryReader>, FileError>;

    /// 对应 `writeFile`。
    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        context: &dyn Context,
    ) -> Result<(), FileError>;

    /// 对应 `appendFile`。
    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        context: &dyn Context,
    ) -> Result<(), FileError>;

    /// 对应 `truncateFile`：把文件截断或扩展到恰好 `size` 字节。
    async fn truncate_file(
        &self,
        path: &str,
        size: u64,
        context: &dyn Context,
    ) -> Result<(), FileError>;

    /// 对应 `flushFile`：冲刷文件内容与检索所需的元数据。
    async fn flush_file(&self, path: &str, context: &dyn Context) -> Result<(), FileError>;

    /// 对应 `renameFile`。
    async fn rename_file(
        &self,
        source_path: &str,
        destination_path: &str,
        context: &dyn Context,
    ) -> Result<(), FileError>;

    /// 对应 `fileInfo`。
    async fn file_info(&self, path: &str, context: &dyn Context) -> Result<FileInfo, FileError>;

    /// 对应 `listDir`。
    async fn list_dir(&self, path: &str, context: &dyn Context)
    -> Result<Vec<FileInfo>, FileError>;

    /// 对应 `openDirReader`。
    async fn open_dir_reader(
        &self,
        path: &str,
        context: &dyn Context,
    ) -> Result<Box<dyn DirReader>, FileError>;

    /// 对应 `watch`：报告文件与目录的变化。
    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: Box<dyn Fn(WatchChange) + Send + Sync>,
        context: &dyn Context,
    ) -> Result<Box<dyn FileWatcher>, FileError>;

    /// 对应 `canonicalPath`。
    async fn canonical_path(&self, path: &str, context: &dyn Context) -> Result<String, FileError>;

    /// 对应 `exists`。
    async fn exists(&self, path: &str, context: &dyn Context) -> Result<bool, FileError>;

    /// 对应 `createDir`（`recursive` 为 `None` 时不递归）。
    async fn create_dir(
        &self,
        path: &str,
        recursive: Option<bool>,
        context: &dyn Context,
    ) -> Result<(), FileError>;

    /// 对应 `remove`。
    async fn remove(
        &self,
        path: &str,
        options: RemoveOptions,
        context: &dyn Context,
    ) -> Result<(), FileError>;

    /// 对应 `createTempDir`。
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        context: &dyn Context,
    ) -> Result<String, FileError>;

    /// 对应 `createTempFile`。
    async fn create_temp_file(
        &self,
        prefix: Option<&str>,
        suffix: Option<&str>,
        context: &dyn Context,
    ) -> Result<String, FileError>;

    /// 对应 `cleanup`。
    async fn cleanup(&self, context: &dyn Context);
}

/// 对应 `remove(path, { recursive?, force? })`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoveOptions {
    /// 递归删除目录。
    pub recursive: Option<bool>,
    /// 目标不存在时不报错。
    pub force: Option<bool>,
}

// ─── Shell ───────────────────────────────────────────────────────────────────

/// 对应 `exec(command: string | readonly string[])`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    /// 经环境的 shell 运行。
    Shell(String),
    /// 不经 shell 直接以 `command[0]` 运行，其余为参数。
    Argv(Vec<String>),
}

/// 对应 `ShellSpillOptions`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellSpillOptions {
    /// 超过该字节数即溢出。
    pub after_bytes: usize,
    /// 超过该行数即溢出。
    pub after_lines: usize,
}

/// 对应 `ShellExecResult`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellExecResult {
    /// 退出码。
    pub exit_code: i32,
    /// 超过 spill 阈值时保存完整原始输出的临时文件。
    pub spill_path: Option<String>,
}

/// 对应 `ShellOutputWindow`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShellOutputWindow {
    /// 末尾保留的 UTF-8 字节数。
    pub max_bytes: usize,
    /// 末尾保留的行数。
    pub max_lines: usize,
    /// 调用方两次采样之间的最小间隔（毫秒）。
    pub min_interval_ms: u64,
    /// 按该速率对采样大小计费。
    pub bytes_per_second: f64,
}

/// 对应 `ShellOutputSkip`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellOutputSkip {
    /// 被省略文本的 UTF-8 字节数。
    pub bytes: usize,
    /// 被省略文本中的换行数。
    pub newlines: usize,
    /// 被省略文本是否以换行结束。
    pub ends_with_newline: bool,
}

/// 对应 `ShellOutputInfo`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutputInfo {
    /// 来源流。
    pub stream: ShellOutputStream,
    /// 紧邻本块之前被省略的输出（仅在使用 `window` 时出现）。
    pub skipped: Option<ShellOutputSkip>,
}

/// 对应 `stream: "stdout" | "stderr"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellOutputStream {
    /// 标准输出。
    Stdout,
    /// 标准错误。
    Stderr,
}

/// 对应 `onOutput` 回调。
pub type ShellOutputCallback = Box<dyn Fn(&str, &dyn Context, &ShellOutputInfo) + Send + Sync>;

/// 对应 `Shell`。
#[async_trait::async_trait]
pub trait Shell: Send + Sync {
    /// 运行命令。
    async fn exec(
        &self,
        command: ShellCommand,
        options: ShellExecOptions,
        context: &dyn Context,
    ) -> Result<ShellExecResult, ExecutionError>;

    /// 杀掉本环境仍在运行的每个命令（用于拥有者关闭，而非单次请求）。
    async fn cleanup(&self, context: &dyn Context);
}

/// 对应 `ShellExecOptions`。
#[derive(Default)]
pub struct ShellExecOptions {
    /// 工作目录。
    pub cwd: Option<String>,
    /// 额外环境变量。
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// 是否继承父进程环境。
    pub inherit_env: Option<bool>,
    /// 超时（秒）。
    pub timeout: Option<f64>,
    /// 每个解码块的到达回调。
    pub on_output: Option<ShellOutputCallback>,
    /// 溢出设置。
    pub spill: Option<ShellSpillOptions>,
    /// 调用方只保留的末尾窗口。
    pub window: Option<ShellOutputWindow>,
}

/// 对应 `ExecutionEnv = FileSystem & Shell`。
pub trait ExecutionEnv: FileSystem + Shell {}

impl<T: FileSystem + Shell> ExecutionEnv for T {}
