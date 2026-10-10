//! pi-tools：可复用的 agent 工具集合。
//!
//! 工具以 [`ToolRegistration`] 形态提供，供 `pi-durable` harness 的扩展（`Extension`）装载。
//!
//! - `now`：返回当前 Unix 时间戳。
//! - `http`：从 JSON 配置目录加载声明式 HTTP API 工具。

pub mod http;
pub mod now;

pub use http::{load_tool_file, load_tools_from_dir};
pub use now::create_now_tool;

use std::sync::Arc;

use pi_durable::harness::define::define_extension;
use pi_durable::harness::types::{Extension, ToolRegistration};

/// 把 pi-tools 的工具打包成一个 [`Extension`]：始终包含 `now`，可附加 HTTP 工具。
///
/// 用法：
/// ```ignore
/// let http_tools = pi_tools::load_tools_from_dir(Path::new("./tools")).unwrap_or_default();
/// let extension = pi_tools::extension(http_tools);
/// registry.install(Arc::new(extension.clone()));
/// ```
pub fn extension(http_tools: Vec<Arc<dyn ToolRegistration>>) -> Extension {
    let mut tools = vec![create_now_tool()];
    tools.extend(http_tools);
    define_extension(Extension {
        name: "pi-tools".to_string(),
        tools,
        sections: Vec::new(),
        hooks: Vec::new(),
        wraps: Vec::new(),
        tasks: Vec::new(),
    })
}
