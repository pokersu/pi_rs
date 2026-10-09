//! 对应 `src/session/`：会话内核。
//!
//! - [`forks`]：分叉时选择并拷贝会话文档（`forks.ts`）
//! - [`observation`]：已提交状态的观察桥（`observation.ts`）
//! - [`transaction`]：一次提交回调的事务（`transaction.ts`）
//! - [`session`]：会话内核与提交线（`session.ts`）

pub mod forks;
pub mod observation;
// 与上游 `session/session.ts` 的文件名对齐；`session::session` 是刻意的映射。
#[allow(clippy::module_inception)]
pub mod session;
pub mod transaction;

pub use session::{
    ConversationDocumentOnLine, DefaultSessionHooks, DocumentState, DocumentWatch, Session,
    SessionHooks, SessionImpl, SessionOptions, SessionSubscription, create_session, default_now,
};

use std::sync::Arc;

use crate::chord::context::Context;
use crate::documents::AnyDocToken;
use crate::session::observation::CommittedWatch;
use crate::types::{EntryId, JsonObject};

/// 对应 durable `types.ts` 的 `DocumentReader = Pick<Session, "snapshot" | "snapshotAsOf">`。
///
/// 已提交文档读取；`harness` 的 `ToolExecutionApi` / `HookApi` / `PromptInput` 都建立在这一面之上。
#[async_trait::async_trait]
pub trait DocumentReader: Send + Sync {
    /// 对应 `snapshot(token, ...)`。
    async fn snapshot(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError>;

    /// 对应 `snapshotAsOf(token, conversationId, at, context)`。
    async fn snapshot_as_of(
        &self,
        token: &dyn AnyDocToken,
        owner: u64,
        key: Option<String>,
        at: EntryId,
        context: Arc<dyn Context>,
    ) -> Result<Option<JsonObject>, SessionError>;
}

/// 对应 durable `types.ts` 的 `DocumentObserver`：非创建式的文档观察获取。
#[async_trait::async_trait]
pub trait DocumentObserver: Send + Sync {
    /// 对应 `watchDoc(token, ...)`。
    async fn watch_doc(
        &self,
        token: &dyn AnyDocToken,
        owner: Option<u64>,
        key: Option<String>,
        context: Arc<dyn Context>,
    ) -> Result<Option<Arc<CommittedWatch>>, SessionError>;
}

/// session 层操作错误。
///
/// 上游用 `Error` / `TypeError` / `ReadAfterWrite` 抛出；Rust 收敛为枚举，消息文本与上游一致。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionError {
    /// 对应上游 `new Error(message)`。
    Message(String),
    /// 对应上游 `new Error(message, { cause })`（`#poison` 的包装错误）。
    MessageWithCause {
        /// 错误消息。
        message: String,
        /// 原始错误的文本形态。
        cause: String,
    },
    /// 对应上游 `new TypeError(message)`。
    Type(String),
    /// 对应 `new ReadAfterWrite(method)`。
    ReadAfterWrite(crate::errors::ReadAfterWrite),
    /// 对应 `new StorageRejected(message)`。
    Rejected(crate::errors::StorageRejected),
    /// 存储层错误。
    Storage(crate::types::StorageError),
    /// 文档定义与校验错误。
    Doc(crate::documents::DocError),
    /// 对应 `cancellationError(signal)`（`DOMException("AbortError")`）。
    Aborted(pi_ai::AbortError),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Message(message) | SessionError::Type(message) => write!(f, "{message}"),
            SessionError::MessageWithCause { message, cause } => {
                write!(f, "{message} ({cause})")
            }
            SessionError::ReadAfterWrite(error) => write!(f, "{error}"),
            SessionError::Rejected(error) => write!(f, "{error}"),
            SessionError::Storage(error) => write!(f, "{error}"),
            SessionError::Doc(error) => write!(f, "{error}"),
            SessionError::Aborted(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<crate::types::StorageError> for SessionError {
    fn from(error: crate::types::StorageError) -> Self {
        SessionError::Storage(error)
    }
}

impl From<crate::documents::DocError> for SessionError {
    fn from(error: crate::documents::DocError) -> Self {
        SessionError::Doc(error)
    }
}

impl From<crate::errors::StorageRejected> for SessionError {
    fn from(error: crate::errors::StorageRejected) -> Self {
        SessionError::Rejected(error)
    }
}

impl From<crate::env::FileError> for SessionError {
    fn from(error: crate::env::FileError) -> Self {
        SessionError::Message(error.to_string())
    }
}

impl From<crate::env::ExecutionError> for SessionError {
    fn from(error: crate::env::ExecutionError) -> Self {
        SessionError::Message(error.to_string())
    }
}

impl From<pi_ai::AbortError> for SessionError {
    fn from(error: pi_ai::AbortError) -> Self {
        SessionError::Aborted(error)
    }
}
