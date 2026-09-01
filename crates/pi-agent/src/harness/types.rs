//! Rust 翻译自 packages/agent/src/harness/types.ts
//!
//! harness 的抽象：`FileSystem`/`Shell`/`ExecutionEnv` 能力接口、错误类型、`Skill`/`PromptTemplate`。

use std::collections::BTreeMap;

use pi_ai::AbortSignal;

/// 对应 `FileKind`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

/// 对应 `FileErrorCode`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    NotSupported,
    Unknown,
}

/// 对应 `FileError`
#[derive(Debug, Clone)]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
    pub path: Option<String>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: None,
        }
    }
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for FileError {}

/// 对应 `ExecutionErrorCode`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}

/// 对应 `ExecutionError`
#[derive(Debug, Clone)]
pub struct ExecutionError {
    pub code: ExecutionErrorCode,
    pub message: String,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ExecutionError {}

/// 对应 `CompactionErrorCode`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionErrorCode {
    Aborted,
    SummarizationFailed,
}

/// 对应 `CompactionError`
#[derive(Debug, Clone)]
pub struct CompactionError {
    pub code: CompactionErrorCode,
    pub message: String,
}

impl CompactionError {
    pub fn new(code: CompactionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CompactionError {}

/// 对应 `FileInfo`
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: u64,
}

/// 对应 `FileSystem`。所有方法不抛错，失败编码进返回的 `Result`。
#[async_trait::async_trait]
pub trait FileSystem: Send + Sync {
    fn cwd(&self) -> &str;
    async fn absolute_path(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError>;
    async fn join_path(
        &self,
        parts: &[&str],
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError>;
    async fn read_text_file(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError>;
    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        signal: Option<&AbortSignal>,
    ) -> Result<Vec<String>, FileError>;
    async fn read_binary_file(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<Vec<u8>, FileError>;
    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError>;
    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError>;
    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError>;
    async fn file_info(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<FileInfo, FileError>;
    async fn list_dir(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<Vec<FileInfo>, FileError>;
    async fn canonical_path(
        &self,
        path: &str,
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError>;
    async fn exists(&self, path: &str, signal: Option<&AbortSignal>) -> Result<bool, FileError>;
    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError>;
    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        signal: Option<&AbortSignal>,
    ) -> Result<(), FileError>;
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError>;
    async fn create_temp_file(
        &self,
        prefix: Option<&str>,
        suffix: Option<&str>,
        signal: Option<&AbortSignal>,
    ) -> Result<String, FileError>;
    async fn cleanup(&self);
}

/// 对应 `ShellExecOptions`
#[derive(Debug, Clone, Default)]
pub struct ShellExecOptions {
    pub cwd: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    pub inherit_env: bool,
    pub timeout: Option<f64>,
    pub abort_signal: Option<AbortSignal>,
}

/// 对应 `Shell.exec` 的返回值
#[derive(Debug, Clone)]
pub struct ShellResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// 对应 `Shell`
#[async_trait::async_trait]
pub trait Shell: Send + Sync {
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
    ) -> Result<ShellResult, ExecutionError>;
    async fn cleanup(&self);
}

/// 对应 `ExecutionEnv = FileSystem & Shell`
pub trait ExecutionEnv: FileSystem + Shell {}

/// 对应 `Skill`
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub file_path: String,
    pub disable_model_invocation: bool,
}

/// 对应 `PromptTemplate`
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    pub name: String,
    pub description: Option<String>,
    pub content: String,
}
