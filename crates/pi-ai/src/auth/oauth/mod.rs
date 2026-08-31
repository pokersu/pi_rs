//! Rust 翻译自 packages/ai/src/auth/oauth/（目录）
//!
//! 仅包含 OAuth 通用工具；各厂商的 OAuth 流程（anthropic/github-copilot/
//! openrouter/xai/kimi-coding/openai-codex/radius）按需求后续补充。

#[path = "device-code.rs"]
pub mod device_code;
#[path = "oauth-page.rs"]
pub mod oauth_page;
pub mod pkce;
