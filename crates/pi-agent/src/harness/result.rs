//! Rust 翻译自 packages/agent/src/harness/result.ts
//!
//! `Result<TValue, TError>` 判别联合由 Rust 标准 `Result<T, E>` 取代；此处复刻
//! `TaggedError` 带标签错误机制与 `matchError` 分派。

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

/// 对应 `TaggedErrorValue<Tag>`：带 `_tag` 标签的错误，可 JSON 序列化。
pub trait TaggedError: std::error::Error {
    /// 对应 `_tag`。
    fn tag(&self) -> &'static str;

    /// 对应 `toJSON()`：序列化为 `{ "_tag": tag, ...fields }`。
    fn to_json(&self) -> serde_json::Value;
}

/// 对应 `ErrorMatchers<TError, TValue>`：按 tag 分派的匹配器映射。
pub type ErrorMatchers<'a, T, E> = std::collections::BTreeMap<&'a str, Box<dyn Fn(&E) -> T + 'a>>;

/// 对应 `matchError(error, matchers)`：按 `tag` 分派到匹配器；无匹配返回 `None`。
pub fn match_error<T, E: TaggedError>(
    error: &E,
    matchers: &ErrorMatchers<'_, T, E>,
) -> Option<T> {
    matchers.get(error.tag()).map(|matcher| matcher(error))
}

/// 对应 `TaggedError(tag)`：生成一个带 `_tag` 标签与字段的错误结构体。
///
/// 要求字段列表中包含 `message: String`（对应原版 `props.message`）。
#[macro_export]
macro_rules! tagged_error {
    ($name:ident, $tag:literal, { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone)]
        pub struct $name {
            $(pub $field: $ty,)*
        }

        impl $name {
            pub fn new($($field: $ty,)*) -> Self {
                Self { $($field,)* }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.message)
            }
        }

        impl std::error::Error for $name {}

        impl $crate::harness::result::TaggedError for $name {
            fn tag(&self) -> &'static str {
                $tag
            }

            fn to_json(&self) -> serde_json::Value {
                serde_json::json!({
                    "_tag": $tag,
                    $(stringify!($field): self.$field,)*
                })
            }
        }
    };
}
