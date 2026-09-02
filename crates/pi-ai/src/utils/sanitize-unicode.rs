//! Rust 翻译自 packages/ai/src/utils/sanitize-unicode.ts
//!
//! 移除未配对的 Unicode 代理字符。

/// 对应 `sanitizeSurrogates(text)`。
///
/// Rust 的 `String` 是 UTF-8，无法包含未配对的 UTF-16 代理字符（它们不是合法
/// Unicode 标量值），因此本函数为恒等映射，仅为保持与原版一致的 API 而保留。
pub fn sanitize_surrogates(text: &str) -> String {
    text.to_string()
}
