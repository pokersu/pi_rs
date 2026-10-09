//! 对应 `tools/`：可复用的内置工具（read / write / edit / bash）与它们的辅助。
//!
//! 这些工具建立在 [`ExecutionEnv`]（文件系统 + Shell 契约）与 [`ToolRegistration`] 之上；
//! 具体环境（node 文件系统 / Shell）由 `env/` 的 node 实现提供（P6）。

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod env;
pub mod file_mutation_queue;
pub mod image;
pub mod path_utils;
pub mod read;
pub mod write;

pub use bash::{BashToolOptions, create_bash_tool, create_powershell_tool};
pub use edit::create_edit_tool;
pub use read::create_read_tool;
pub use write::create_write_tool;

use std::sync::LazyLock;

use crate::harness::define::define_extension;
use crate::harness::types::Extension;

/// 对应 `CodingTools`：`read`、`write`、`edit`、`bash`；不自动安装。
pub static CODING_TOOLS: LazyLock<Extension> = LazyLock::new(|| {
    define_extension(Extension {
        name: "coding-tools".to_string(),
        tools: vec![
            create_read_tool(),
            create_write_tool(),
            create_edit_tool(),
            create_bash_tool(None),
        ],
        sections: Vec::new(),
        hooks: Vec::new(),
        wraps: Vec::new(),
        tasks: Vec::new(),
    })
});
