//! Rust 翻译自 packages/ai/src/utils/（目录）
//!
//! 文件名保持与 TS 原版一致（连字符），模块名按 Rust 惯例使用下划线。

pub mod abort;
pub mod diagnostics;
#[path = "error-stream.rs"]
pub mod error_stream;
#[path = "event-stream.rs"]
pub mod event_stream;
#[path = "json-parse.rs"]
pub mod json_parse;
pub mod text;
pub mod uuid;
pub mod validation;
