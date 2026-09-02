//! HTTP 工具配置加载。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use pi_agent::AgentTool;

use crate::http::config::HttpToolConfig;
use crate::http::executor::config_to_tool;

/// 配置加载错误。
#[derive(Debug)]
pub struct LoadError {
    pub message: String,
    pub path: Option<PathBuf>,
}

impl LoadError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: None,
        }
    }

    pub fn at(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: Some(path.into()),
        }
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}", path.display(), self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for LoadError {}

/// 扫描目录下所有 `.json` 文件，每个文件加载成一个 HTTP 工具。
///
/// 工具名在目录内必须唯一，重复时返回错误。
pub fn load_tools_from_dir(dir: &Path) -> Result<Vec<AgentTool>, LoadError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| LoadError::at(dir, format!("无法读取目录: {e}")))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();

    if files.is_empty() {
        return Err(LoadError::at(dir, "目录中没有 .json 配置文件"));
    }

    let mut tools = Vec::new();
    let mut seen = HashSet::new();
    for file in files {
        let tool = load_tool_file(&file)?;
        if !seen.insert(tool.name().to_string()) {
            return Err(LoadError::at(
                &file,
                format!("重复的工具名: {}", tool.name()),
            ));
        }
        tools.push(tool);
    }
    Ok(tools)
}

/// 读取单个配置文件并转成工具。
pub fn load_tool_file(path: &Path) -> Result<AgentTool, LoadError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| LoadError::at(path, format!("无法读取文件: {e}")))?;
    let config: HttpToolConfig = serde_json::from_str(&content)
        .map_err(|e| LoadError::at(path, format!("配置解析失败: {e}")))?;
    validate_config(&config).map_err(|m| LoadError::at(path, m))?;
    Ok(config_to_tool(config))
}

fn validate_config(config: &HttpToolConfig) -> Result<(), String> {
    if config.name.trim().is_empty() {
        return Err("name 不能为空".to_string());
    }
    if config.description.trim().is_empty() {
        return Err("description 不能为空".to_string());
    }
    if config.request.url.trim().is_empty() {
        return Err("request.url 不能为空".to_string());
    }
    let method = config.request.method.to_ascii_uppercase();
    if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "DELETE" | "PATCH") {
        return Err(format!("不支持的 HTTP method: {}", config.request.method));
    }
    Ok(())
}
