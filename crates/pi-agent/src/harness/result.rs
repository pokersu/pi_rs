//! Rust 翻译自 packages/agent/src/harness/result.ts
//!
//! 注：TS 的 `Result<T, E>` 判别联合、`TaggedError`/`matchError` 在 Rust 中由标准
//! `Result<T, E>` 与 `enum` 错误类型自然取代，此处仅保留少量辅助函数。

/// 对应 `ok` / `err`：Rust 标准 `Ok`/`Err` 直接替代。
pub use std::result::Result::{Err, Ok};

/// 对应 `getOrThrow`：取出成功值，失败则 panic。
pub fn get_or_throw<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("getOrThrow called on error: {error:?}"),
    }
}

/// 对应 `getOrUndefined`。
pub fn get_or_undefined<T, E>(result: Result<T, E>) -> Option<T> {
    result.ok()
}
