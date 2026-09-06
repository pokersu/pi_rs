//! Rust 翻译自 packages/agent/src/harness/types.ts
//!
//! harness 的抽象：`FileSystem`/`Shell`/`ExecutionEnv` 能力接口、错误类型、`Skill`/`PromptTemplate`。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use pi_ai::{CacheRetention, Tool, Transport};

use crate::harness::context::Context;
use crate::harness::session::types::ReplayPolicy;
use crate::types::{AgentToolResult, ToolExecutionMode};

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

/// 对应 `BranchSummaryErrorCode`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchSummaryErrorCode {
    Aborted,
    SummarizationFailed,
}

/// 对应 `BranchSummaryError`
#[derive(Debug, Clone)]
pub struct BranchSummaryError {
    pub code: BranchSummaryErrorCode,
    pub message: String,
}

impl BranchSummaryError {
    pub fn new(code: BranchSummaryErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for BranchSummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BranchSummaryError {}

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
    async fn absolute_path(&self, path: &str, context: &Context) -> Result<String, FileError>;
    async fn join_path(&self, parts: &[&str], context: &Context) -> Result<String, FileError>;
    async fn read_text_file(&self, path: &str, context: &Context) -> Result<String, FileError>;
    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        context: &Context,
    ) -> Result<Vec<String>, FileError>;
    async fn read_binary_file(&self, path: &str, context: &Context) -> Result<Vec<u8>, FileError>;
    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError>;
    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        context: &Context,
    ) -> Result<(), FileError>;
    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        context: &Context,
    ) -> Result<(), FileError>;
    async fn file_info(&self, path: &str, context: &Context) -> Result<FileInfo, FileError>;
    async fn list_dir(&self, path: &str, context: &Context) -> Result<Vec<FileInfo>, FileError>;
    async fn canonical_path(&self, path: &str, context: &Context) -> Result<String, FileError>;
    async fn exists(&self, path: &str, context: &Context) -> Result<bool, FileError>;
    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        context: &Context,
    ) -> Result<(), FileError>;
    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        context: &Context,
    ) -> Result<(), FileError>;
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        context: &Context,
    ) -> Result<String, FileError>;
    async fn create_temp_file(
        &self,
        prefix: Option<&str>,
        suffix: Option<&str>,
        context: &Context,
    ) -> Result<String, FileError>;
    async fn cleanup(&self);
}

/// 对应 `ShellOutputRetention`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellOutputRetention {
    Head,
    Tail,
}

/// 对应 `ShellOutputLimits`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub retain: Option<ShellOutputRetention>,
}

/// 对应 `ShellOutputCaptureOptions`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputCaptureOptions {
    pub limits: ShellOutputLimits,
    pub spill: Option<bool>,
}

/// 对应 `ShellOutputTruncation = Omit<TruncationResult, "content">`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputTruncation {
    pub truncated: bool,
    pub truncated_by: Option<String>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

/// 对应 `ShellOutputMetadata`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputMetadata {
    pub truncation: ShellOutputTruncation,
    pub spill_path: Option<String>,
    pub last_line_bytes: Option<usize>,
}

/// 对应 `ShellOutputView`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutputView {
    pub text: String,
    pub truncation: ShellOutputTruncation,
    pub spill_path: Option<String>,
    pub last_line_bytes: Option<usize>,
}

/// 对应 `ShellOutputUpdate`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ShellOutputUpdate {
    Replace {
        output: ShellOutputView,
    },
    Append {
        text: String,
        metadata: ShellOutputMetadata,
    },
    Slide {
        #[serde(rename = "drop")]
        drop: usize,
        text: String,
        metadata: ShellOutputMetadata,
    },
    Metadata {
        metadata: ShellOutputMetadata,
    },
}

/// 对应 `ShellExecResult`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellExecResult {
    pub truncation: ShellOutputTruncation,
    pub spill_path: Option<String>,
    pub last_line_bytes: Option<usize>,
    pub exit_code: i32,
}

/// 对应 `onUpdate`：有界输出变化回调。
pub type ShellOutputUpdateCallback = Box<dyn Fn(&ShellOutputUpdate, &Context) + Send + Sync>;

/// 对应 `ShellExecOptions`
#[derive(Default)]
pub struct ShellExecOptions {
    pub cwd: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    pub inherit_env: bool,
    pub timeout: Option<f64>,
    /// 源侧有界捕获。当此字段与 `on_update` 均缺省时丢弃输出。
    pub capture: Option<ShellOutputCaptureOptions>,
    /// 有界输出变化回调。
    pub on_update: Option<ShellOutputUpdateCallback>,
}

impl Clone for ShellExecOptions {
    fn clone(&self) -> Self {
        Self {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            inherit_env: self.inherit_env,
            timeout: self.timeout,
            capture: self.capture.clone(),
            // 回调是一次性消费，clone 时不可复制。
            on_update: None,
        }
    }
}

impl std::fmt::Debug for ShellExecOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellExecOptions")
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("inherit_env", &self.inherit_env)
            .field("timeout", &self.timeout)
            .field("capture", &self.capture)
            .field("on_update", &self.on_update.is_some())
            .finish()
    }
}

/// 对应 `Shell`
#[async_trait::async_trait]
pub trait Shell: Send + Sync {
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
        context: &Context,
    ) -> Result<ShellExecResult, ExecutionError>;
    async fn cleanup(&self, context: &Context);
}

/// 对应 `ExecutionEnv = FileSystem & Shell`
pub trait ExecutionEnv: FileSystem + Shell {}

/// 对应 `Skill`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub file_path: String,
    pub disable_model_invocation: bool,
}

/// 对应 `PromptTemplate`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTemplate {
    pub name: String,
    pub description: Option<String>,
    pub content: String,
}

/// 对应 `AgentHarnessToolUpdateOptions`
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentHarnessToolUpdateOptions {
    pub checkpoint: bool,
}

/// 对应 `AgentHarnessToolUpdateCallback<TDetails>`：onUpdate 必选，带 checkpoint options。
pub type AgentHarnessToolUpdateCallback =
    Box<dyn Fn(AgentToolResult, Option<AgentHarnessToolUpdateOptions>) + Send>;

/// 对应 `AgentHarnessToolInvocation`。
#[async_trait::async_trait]
pub trait AgentHarnessToolInvocation: Send + Sync {
    fn invocation_id(&self) -> &str;
    fn operation_id(&self) -> &str;
    fn turn_id(&self) -> &str;
    async fn get_memo(&self, name: &str) -> Option<serde_json::Value>;
    async fn set_memo(&self, name: &str, value: Option<serde_json::Value>);
}

/// 对应 `AgentHarnessTool<TContext, TParameters, TDetails>`（R1）。
/// Rust 中 `toolContext` 由闭包捕获（保持内置工具 `env` 闭包模式），`TDetails` 用 JSON 值。
#[derive(Clone)]
pub struct AgentHarnessTool {
    pub label: String,
    pub tool: Tool,
    /// 对应 `prepareArguments?: (args: unknown) => Static<TParameters>`。
    pub prepare_arguments:
        Option<Arc<dyn Fn(serde_json::Value) -> serde_json::Value + Send + Sync>>,
    /// 对应 `replay?: "never" | "safe"`。
    pub replay: Option<ReplayPolicy>,
    /// 对应 `executionMode?: ToolExecutionMode`。
    pub execution_mode: Option<ToolExecutionMode>,
    pub execute: AgentHarnessToolExecuteFn,
}

impl AgentHarnessTool {
    pub fn name(&self) -> &str {
        &self.tool.name
    }
}

/// 对应 `AgentHarnessTool.execute` 签名。
pub type AgentHarnessToolExecuteFn = Arc<
    dyn Fn(
            String,
            serde_json::Value,
            AgentHarnessToolUpdateCallback,
            Arc<dyn AgentHarnessToolInvocation>,
            Context,
        ) -> Pin<Box<dyn Future<Output = AgentToolResult> + Send>>
        + Send
        + Sync,
>;

/// 对应 `AgentHarnessStreamOptions`。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHarnessStreamOptions {
    pub transport: Option<Transport>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u64>,
    pub max_retry_delay_ms: Option<u64>,
    pub headers: Option<BTreeMap<String, String>>,
    pub metadata: Option<serde_json::Value>,
    pub cache_retention: Option<CacheRetention>,
    /// `bool | { window?: "15m" | "1h" | "24h" }`。
    pub deferred: Option<serde_json::Value>,
}

/// 对应 `AgentHarnessStreamOptionsPatch`。
/// `headers`/`metadata` 的 patch 语义：内层 `None` 表示删除单个 key。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHarnessStreamOptionsPatch {
    pub transport: Option<Transport>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u64>,
    pub max_retry_delay_ms: Option<u64>,
    pub headers: Option<BTreeMap<String, Option<String>>>,
    pub metadata: Option<BTreeMap<String, Option<serde_json::Value>>>,
    pub cache_retention: Option<CacheRetention>,
    pub deferred: Option<serde_json::Value>,
}
