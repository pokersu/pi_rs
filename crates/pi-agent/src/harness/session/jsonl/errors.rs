//! Rust 翻译自 packages/agent/src/harness/session/jsonl/errors.ts

use crate::harness::session::types::{SessionError, SessionErrorCode};

/// 对应 `JsonlDecodeError`
#[derive(Debug)]
pub struct JsonlDecodeError {
    pub kind: &'static str,
    pub message: String,
}

impl std::fmt::Display for JsonlDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for JsonlDecodeError {}

/// 对应 `fileResult`
pub fn file_result<T, E: std::fmt::Debug>(result: Result<T, E>, message: &str) -> T {
    match result {
        Ok(value) => value,
        Err(_) => panic!("{}", SessionError::new(SessionErrorCode::Storage, message)),
    }
}

/// 对应 `invalidFile`
pub fn invalid_file(path: &str, line: usize, message: &str) -> SessionError {
    SessionError::new(
        SessionErrorCode::InvalidEntry,
        format!("Invalid JSONL v4 session {path}: line {line} {message}"),
    )
}
