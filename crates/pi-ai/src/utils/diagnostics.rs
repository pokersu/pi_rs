//! Rust 翻译自 packages/ai/src/utils/diagnostics.ts

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 对应 `DiagnosticErrorInfo`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticErrorInfo {
    pub name: Option<String>,
    pub message: String,
    pub stack: Option<String>,
    /// TS 中为 `string | number`；Rust 统一为字符串。
    pub code: Option<String>,
}

/// 对应 `AssistantMessageDiagnostic`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessageDiagnostic {
    #[serde(rename = "type")]
    pub kind: String,
    pub timestamp: u64,
    pub error: Option<DiagnosticErrorInfo>,
    pub details: Option<BTreeMap<String, serde_json::Value>>,
}

/// 对应 `formatThrownValue(value)`：TS 接受 `unknown`（Error / string / 其他）。
/// Rust 中用于解析 `BoxError`，取 `Display` 输出即可。
pub fn format_thrown_value(value: &dyn std::error::Error) -> String {
    let message = value.to_string();
    if !message.is_empty() {
        message
    } else {
        "Error".to_string()
    }
}

/// 对应 `extractDiagnosticError(error)`
pub fn extract_diagnostic_error(error: &(dyn std::error::Error + 'static)) -> DiagnosticErrorInfo {
    DiagnosticErrorInfo {
        name: Some(error.to_string()),
        message: error.to_string(),
        stack: None,
        code: None,
    }
}

/// 对应 `createAssistantMessageDiagnostic(type, error, details?)`
pub fn create_assistant_message_diagnostic(
    kind: String,
    error: &(dyn std::error::Error + 'static),
    details: Option<BTreeMap<String, serde_json::Value>>,
) -> AssistantMessageDiagnostic {
    AssistantMessageDiagnostic {
        kind,
        timestamp: crate::utils::uuid::now_ms() as u64,
        error: Some(extract_diagnostic_error(error)),
        details,
    }
}

/// 对应 `appendAssistantMessageDiagnostic<T extends { diagnostics? }>(message, diagnostic)`。
///
/// TS 通过结构化类型约束把 diagnostic 追加到 `message.diagnostics`；Rust 中改用 trait。
#[allow(dead_code)]
pub trait DiagnosticContainer {
    fn diagnostics_mut(&mut self) -> &mut Vec<AssistantMessageDiagnostic>;
}

/// 对应 `appendAssistantMessageDiagnostic`。
#[allow(dead_code)]
pub fn append_assistant_message_diagnostic<T: DiagnosticContainer>(
    message: &mut T,
    diagnostic: AssistantMessageDiagnostic,
) {
    message.diagnostics_mut().push(diagnostic);
}
