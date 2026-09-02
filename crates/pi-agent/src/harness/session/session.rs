//! Rust 翻译自 packages/agent/src/harness/session/session.ts
//!
//! `Session` 是 `SessionStorage` 的薄封装（`SessionTree` 视图），`SessionView` 是
//! `Session.view(lane)` 返回的 lane 视图。

use std::sync::Arc;

use crate::harness::session::types::{
    BranchBounds, CustomEntry, Entry, EntryBase, EntryQuery, LanePointer, MessageEntry,
    SessionError, SessionStats, SessionStorage, SessionTree,
};
use crate::types::AgentMessage;

/// 对应 `Session`（固定表示 `main` lane 的树视图）。
pub struct Session {
    storage: Arc<dyn SessionStorage>,
}

/// 对应 `Session.view(lane)` 返回的 lane 视图。
pub struct SessionView {
    storage: Arc<dyn SessionStorage>,
    lane: String,
}

async fn append_message_to_lane(
    storage: &dyn SessionStorage,
    lane: &str,
    message: AgentMessage,
) -> Result<String, SessionError> {
    let id = pi_ai::uuidv7();
    let entry = Entry::Message(MessageEntry {
        base: EntryBase {
            kind: "message".to_string(),
            id: id.clone(),
            seq: 0,
            parent_id: None,
            timestamp: pi_ai::utils::uuid::now_ms() as u64,
        },
        message,
        terminate: None,
    });
    storage.append_entry(entry, lane).await?;
    Ok(id)
}

async fn append_custom_entry_to_lane(
    storage: &dyn SessionStorage,
    lane: &str,
    custom_type: &str,
    data: Option<serde_json::Value>,
) -> Result<String, SessionError> {
    let id = pi_ai::uuidv7();
    let entry = Entry::Custom(CustomEntry {
        base: EntryBase {
            kind: "custom".to_string(),
            id: id.clone(),
            seq: 0,
            parent_id: None,
            timestamp: pi_ai::utils::uuid::now_ms() as u64,
        },
        custom_type: custom_type.to_string(),
        data,
    });
    storage.append_entry(entry, lane).await?;
    Ok(id)
}

async fn resolve_branch_start(
    storage: &dyn SessionStorage,
    lane: &str,
    start: Option<&str>,
) -> Result<Option<String>, SessionError> {
    if let Some(start) = start {
        return Ok(Some(start.to_string()));
    }
    let lanes = storage.get_lanes().await?;
    Ok(lanes
        .iter()
        .find(|l| l.lane == lane)
        .and_then(|l| l.leaf_id.clone()))
}

async fn query_branch_entries(
    storage: &dyn SessionStorage,
    lane: &str,
    query: &EntryQuery,
    bounds: &BranchBounds,
) -> Result<Vec<Entry>, SessionError> {
    let start = resolve_branch_start(storage, lane, bounds.start.as_deref()).await?;
    let Some(start) = start else {
        return Ok(Vec::new());
    };
    storage
        .find_entries_on_branch(
            query,
            &start,
            bounds.stop_at_type.as_deref(),
            bounds.stop_at_id.as_deref(),
        )
        .await
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

    /// 对应 `view(lane)`：返回该 lane 的树视图。
    pub fn view(&self, lane: &str) -> Arc<dyn SessionTree> {
        Arc::new(SessionView {
            storage: self.storage.clone(),
            lane: lane.to_string(),
        })
    }

    pub async fn append_message_to_lane(
        &self,
        lane: &str,
        message: AgentMessage,
    ) -> Result<String, SessionError> {
        append_message_to_lane(self.storage.as_ref(), lane, message).await
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

    async fn find_entry(&self, query: &EntryQuery) -> Result<Option<Entry>, SessionError> {
        let mut query = query.clone();
        query.limit = Some(1);
        Ok(self.storage.find_entries(&query).await?.into_iter().next())
    }

    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        query_branch_entries(self.storage.as_ref(), "main", query, bounds).await
    }

    async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Option<Entry>, SessionError> {
        let mut query = query.clone();
        query.limit = Some(1);
        Ok(
            query_branch_entries(self.storage.as_ref(), "main", &query, bounds)
                .await?
                .into_iter()
                .next(),
        )
    }

    async fn append_message(&self, message: AgentMessage) -> Result<String, SessionError> {
        append_message_to_lane(self.storage.as_ref(), "main", message).await
    }

    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        append_custom_entry_to_lane(self.storage.as_ref(), "main", custom_type, data).await
    }
}

#[async_trait::async_trait]
impl SessionTree for SessionView {
    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        let lanes = self.storage.get_lanes().await?;
        Ok(lanes
            .iter()
            .find(|l| l.lane == self.lane)
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

    async fn find_entry(&self, query: &EntryQuery) -> Result<Option<Entry>, SessionError> {
        let mut query = query.clone();
        query.limit = Some(1);
        Ok(self.storage.find_entries(&query).await?.into_iter().next())
    }

    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Vec<Entry>, SessionError> {
        query_branch_entries(self.storage.as_ref(), &self.lane, query, bounds).await
    }

    async fn find_entry_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
    ) -> Result<Option<Entry>, SessionError> {
        let mut query = query.clone();
        query.limit = Some(1);
        Ok(
            query_branch_entries(self.storage.as_ref(), &self.lane, &query, bounds)
                .await?
                .into_iter()
                .next(),
        )
    }

    async fn append_message(&self, message: AgentMessage) -> Result<String, SessionError> {
        append_message_to_lane(self.storage.as_ref(), &self.lane, message).await
    }

    async fn append_custom_entry(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, SessionError> {
        append_custom_entry_to_lane(self.storage.as_ref(), &self.lane, custom_type, data).await
    }
}

// 保留 LanePointer 引用。
#[allow(unused)]
fn _unused(_: LanePointer) {}
