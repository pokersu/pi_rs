//! 声明式 HTTP 工具：从配置目录加载 HTTP API 为 agent 工具。

pub mod config;
pub mod executor;
pub mod loader;

pub use config::{AuthConfig, HttpRequestConfig, HttpToolConfig};
pub use executor::{config_to_tool, extract_placeholders, fill_url};
pub use loader::{LoadError, load_tool_file, load_tools_from_dir};
