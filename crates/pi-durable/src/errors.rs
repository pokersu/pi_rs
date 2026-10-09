//! 对应 `src/errors.ts`。

use crate::types::ConversationId;

/// 对应 `ReadAfterWrite`：事务在首次表写入后又读表。
///
/// 上游提示：在写入之前先读齐所需的行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadAfterWrite {
    /// 触发该错误的方法名（对应 `Tx.${method}()`）。
    pub method: String,
}

impl ReadAfterWrite {
    /// 对应 `new ReadAfterWrite(method)`。
    pub fn new(method: impl Into<String>) -> Self {
        Self {
            method: method.into(),
        }
    }
}

impl std::fmt::Display for ReadAfterWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Tx.{}() cannot read tables after the first table write",
            self.method
        )
    }
}

impl std::error::Error for ReadAfterWrite {}

/// 对应 `StorageRejected`：存储在任何持久化效果之前拒绝了批次。
///
/// 拥有该批次的 Session 可以安全地继续。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRejected {
    /// 拒绝原因。
    pub message: String,
}

impl StorageRejected {
    /// 对应 `new StorageRejected(message, options?)`。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for StorageRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for StorageRejected {}

/// 对应 `ConversationBusy`：提交到达了忙碌的会话，未被接纳。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationBusy {
    /// 忙碌的会话 ID。
    pub conversation_id: ConversationId,
}

impl ConversationBusy {
    /// 对应 `new ConversationBusy(conversationId)`。
    pub fn new(conversation_id: ConversationId) -> Self {
        Self { conversation_id }
    }
}

impl std::fmt::Display for ConversationBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Conversation {} is busy", self.conversation_id)
    }
}

impl std::error::Error for ConversationBusy {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_after_write_message_matches_upstream() {
        assert_eq!(
            ReadAfterWrite::new("scanEntries").to_string(),
            "Tx.scanEntries() cannot read tables after the first table write",
        );
    }

    #[test]
    fn storage_rejected_preserves_message() {
        assert_eq!(StorageRejected::new("rejected").to_string(), "rejected");
    }

    #[test]
    fn conversation_busy_includes_id() {
        assert_eq!(
            ConversationBusy::new(ConversationId::new(7)).to_string(),
            "Conversation 7 is busy",
        );
    }
}
