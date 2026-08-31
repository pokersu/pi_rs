//! Rust 翻译自 packages/agent/src/harness/session/jsonl/repo.ts（核心）
//!
//! JSONL 文件目录中的 session 仓库：create/open/list/delete/fork。

use std::sync::Arc;

use crate::harness::session::jsonl::storage::JsonlSessionStorage;
use crate::harness::session::jsonl::types::{
    JsonlSessionCreateOptions, JsonlSessionListOptions, JsonlSessionMetadata, JsonlV4Header,
};
use crate::harness::session::session::Session;
use crate::harness::session::types::{SessionError, SessionErrorCode};

/// 对应 `JsonlSessionRepo`
pub struct JsonlSessionRepo {
    fs: Arc<dyn crate::harness::types::FileSystem>,
    sessions_root: String,
}

impl JsonlSessionRepo {
    pub fn new(fs: Arc<dyn crate::harness::types::FileSystem>, sessions_root: String) -> Self {
        Self { fs, sessions_root }
    }

    /// 对应 `create`
    pub async fn create(
        &self,
        options: JsonlSessionCreateOptions,
    ) -> Result<Session, SessionError> {
        let id = pi_ai::uuidv7();
        let cwd = self
            .fs
            .absolute_path(&options.cwd, None)
            .await
            .map_err(|e| SessionError::new(SessionErrorCode::Storage, e.message))?;
        let created_at = pi_ai::utils::uuid::now_ms() as u64;
        let session_directory = self.session_directory(&cwd).await?;
        self.fs
            .create_dir(&session_directory, true, None)
            .await
            .map_err(|e| SessionError::new(SessionErrorCode::Storage, e.message))?;
        let path = format!(
            "{}/{}",
            session_directory.trim_end_matches('/'),
            session_file_name(created_at, &id)
        );
        let header = JsonlV4Header {
            kind: "header".to_string(),
            version: 4,
            id: id.clone(),
            created_at,
            cwd: cwd.clone(),
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: options.metadata,
        };
        self.fs
            .write_file(
                &path,
                serde_json::to_string(&header).unwrap().as_bytes(),
                None,
            )
            .await
            .map_err(|e| SessionError::new(SessionErrorCode::Storage, e.message))?;
        let storage = Arc::new(JsonlSessionStorage::load(self.fs.clone(), &path).await?);
        Ok(Session::new(storage))
    }

    /// 对应 `open`
    pub async fn open(&self, metadata: JsonlSessionMetadata) -> Result<Session, SessionError> {
        let storage = Arc::new(JsonlSessionStorage::load(self.fs.clone(), &metadata.path).await?);
        Ok(Session::new(storage))
    }

    /// 对应 `list`
    pub async fn list(
        &self,
        _options: JsonlSessionListOptions,
    ) -> Result<Vec<JsonlSessionMetadata>, SessionError> {
        // 简化：返回空列表（元数据扫描后续按需补）。
        Ok(Vec::new())
    }

    /// 对应 `delete`
    pub async fn delete(&self, metadata: JsonlSessionMetadata) -> Result<(), SessionError> {
        self.fs
            .remove(&metadata.path, false, true, None)
            .await
            .map_err(|e| SessionError::new(SessionErrorCode::Storage, e.message))
    }

    async fn session_directory(&self, cwd: &str) -> Result<String, SessionError> {
        let root = self
            .fs
            .absolute_path(&self.sessions_root, None)
            .await
            .map_err(|e| SessionError::new(SessionErrorCode::Storage, e.message))?;
        Ok(format!(
            "{}/{}",
            root.trim_end_matches('/'),
            jsonl_session_directory_name(cwd)
        ))
    }
}

fn session_file_name(created_at: u64, id: &str) -> String {
    format!("{created_at}_{id}.jsonl")
}

fn jsonl_session_directory_name(cwd: &str) -> String {
    // cwd 编码为目录名（简化：去掉路径分隔符）。
    cwd.replace(['/', '\\'], "_")
}
