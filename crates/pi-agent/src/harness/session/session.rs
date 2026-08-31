//! Rust 翻译自 packages/agent/src/harness/session/session.ts
//!
//! `Session` 是 `SessionStorage` 的薄封装（`SessionTree` 视图）。

use std::sync::Arc;

use crate::harness::session::types::{
    Entry, EntryQuery, LanePointer, SessionError, SessionStats, SessionStorage, SessionTree,
};
use crate::types::AgentMessage;

/// 对应 `Session`
pub struct Session {
    storage: Arc<dyn SessionStorage>,
}

impl Session {
    pub fn new(storage: Arc<dyn SessionStorage>) -> Self {
        Self { storage }
    }

    pub async fn get_metadata(
        &self,
    ) -> Result<crate::harness::session::types::SessionMetadata, SessionError> {
        self.storage.get_metadata().await
    }

    pub async fn append_message_to_lane(
        &self,
        lane: &str,
        message: AgentMessage,
    ) -> Result<String, SessionError> {
        let id = pi_ai::uuidv7();
        let entry = Entry::Message(crate::harness::session::types::MessageEntry {
            base: crate::harness::session::types::EntryBase {
                kind: "message".to_string(),
                id: id.clone(),
                seq: 0,
                parent_id: None,
                timestamp: pi_ai::utils::uuid::now_ms() as u64,
            },
            message,
            terminate: None,
        });
        self.storage.append_entry(entry, lane).await?;
        Ok(id)
    }
}

#[async_trait::async_trait]
impl SessionTree for Session {
    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        let lanes = self.storage.get_lanes().await?;
        Ok(lanes
            .iter()
            .find(|l| l.lane == "main")
            .and_then(|l| l.leaf_id.clone()))
    }

    async fn get_entry(&self, id: &str) -> Result<Option<Entry>, SessionError> {
        self.storage.get_entry(id).await
    }

    async fn get_stats(&self) -> Result<SessionStats, SessionError> {
        self.storage.get_stats().await
    }

    async fn get_name(&self) -> Result<Option<String>, SessionError> {
        self.storage.get_name().await
    }

    async fn set_name(&self, name: Option<&str>) -> Result<(), SessionError> {
        self.storage.set_name(name).await
    }

    async fn get_label(&self, target_id: &str) -> Result<Option<String>, SessionError> {
        self.storage.get_label(target_id).await
    }

    async fn set_label(&self, target_id: &str, label: Option<&str>) -> Result<(), SessionError> {
        self.storage.set_label(target_id, label).await
    }

    async fn find_entries(&self, query: &EntryQuery) -> Result<Vec<Entry>, SessionError> {
        self.storage.find_entries(query).await
    }

    async fn append_message(&self, message: AgentMessage) -> Result<String, SessionError> {
        self.append_message_to_lane("main", message).await
    }

    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        let id = pi_ai::uuidv7();
        let entry = Entry::Custom(crate::harness::session::types::CustomEntry {
            base: crate::harness::session::types::EntryBase {
                kind: "custom".to_string(),
                id: id.clone(),
                seq: 0,
                parent_id: None,
                timestamp: pi_ai::utils::uuid::now_ms() as u64,
            },
            custom_type: custom_type.to_string(),
            data,
        });
        self.storage.append_entry(entry, "main").await?;
        Ok(id)
    }
}

// 保留 LanePointer 引用。
#[allow(unused)]
fn _unused(_: LanePointer) {}
