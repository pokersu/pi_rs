//! Rust 翻译自 packages/ai/src/index.ts
//!
//! 统一多 provider LLM API：核心类型、流抽象、工具校验、provider 框架、认证。

pub mod api;
pub mod auth;
pub mod models;
pub mod providers;
pub mod types;
pub mod utils;

pub use auth::context::default_provider_auth_context;
pub use auth::credential_store::InMemoryCredentialStore;
pub use auth::helpers::{env_api_key_auth, lazy_oauth};
pub use auth::resolve::{
    AuthResolutionOverrides, ModelsError, ModelsErrorCode, ProviderAuthRef, resolve_provider_auth,
};
pub use auth::types::*;
pub use models::{
    CreateProviderOptions, Models, Provider, calculate_cost, clamp_thinking_level, create_models,
    create_provider, get_supported_thinking_levels, has_api, models_are_equal,
};
pub use providers::deepseek::deepseek_provider;
pub use providers::faux::{
    faux_assistant_message, faux_default_model, faux_provider, faux_text, faux_tool_call,
};
pub use providers::openai::openai_provider;
pub use types::*;
pub use utils::error_stream::{create_error_message, default_usage, stream_error};
pub use utils::event_stream::{
    AssistantMessageEventStream, EventStream, create_assistant_message_event_stream,
};
pub use utils::json_parse::{parse_json_with_repair, parse_streaming_json, repair_json};
pub use utils::overflow::{get_overflow_patterns, is_context_overflow, is_recoverable_length};
pub use utils::retry::{
    RetryCallbacks, RetryPolicy, is_retryable_assistant_error, retry_assistant_call,
};
pub use utils::text::{ContentTextInput, content_text};
pub use utils::typebox_helpers::{StringEnumOptions, string_enum};
pub use utils::uuid::uuidv7;
pub use utils::validation::{validate_tool_arguments, validate_tool_call};
