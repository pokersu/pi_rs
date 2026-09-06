//! Rust 翻译自 packages/agent/src/harness/session/jsonl/repo.ts（核心）

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use crate::harness::context::Context;
use crate::harness::session::jsonl::storage::{JsonlStorage, JsonlStorageOptions};
use crate::harness::session::jsonl::types::{
    JSONL_STORAGE_VERSION, JsonlSessionCreateOptions, JsonlSessionMetadata, JsonlStorageHeader,
};
use crate::harness::session::session::StorageBackedSession;
use crate::harness::session::types::{
    ForkOptions, Session, SessionCreateOptions, SessionMetadata, SessionRepo,
};
use crate::harness::types::FileSystem;

fn session_directory_name(cwd: &str) -> String {
    let cleaned = cwd
        .trim_start_matches('/')
        .trim_start_matches('\\')
        .replace(['/', '\\', ':'], "-");
    format!("--{cleaned}--")
}

fn session_file_name(created_at: u64, id: &str) -> String {
    // 简化：ISO 时间戳 + id。
    format!("{created_at}_{id}.jsonl")
}

/// 对应 `JsonlSessionRepo`。
pub struct JsonlSessionRepo {
    file_system: Arc<dyn FileSystem>,
    sessions_root_input: String,
    open_sessions: StdMutex<BTreeMap<String, Arc<JsonlStorage>>>,
    closed: StdMutex<bool>,
}

impl JsonlSessionRepo {
    pub fn new(file_system: Arc<dyn FileSystem>, sessions_root: String) -> Self {
        Self {
            file_system,
            sessions_root_input: sessions_root,
            open_sessions: StdMutex::new(BTreeMap::new()),
            closed: StdMutex::new(false),
        }
    }

    async fn root(&self, context: &Context) -> Result<String, String> {
        self.file_system
            .absolute_path(&self.sessions_root_input, context.abort_signal())
            .await
            .map_err(|e| {
                format!(
                    "Failed to resolve sessions root {}: {}",
                    self.sessions_root_input, e.message
                )
            })
    }

    #[allow(dead_code)]
    async fn session_path(
        &self,
        metadata: &JsonlSessionMetadata,
        context: &Context,
    ) -> Result<String, String> {
        let root = self.root(context).await?;
        let directory = self
            .file_system
            .join_path(
                &[&root, &session_directory_name(&metadata.cwd)],
                context.abort_signal(),
            )
            .await
            .map_err(|e| e.message)?;
        self.file_system
            .join_path(
                &[
                    &directory,
                    &session_file_name(metadata.base.created_at, &metadata.base.id),
                ],
                context.abort_signal(),
            )
            .await
            .map_err(|e| e.message)
    }
}

#[async_trait::async_trait]
impl SessionRepo for JsonlSessionRepo {
    async fn create(
        &self,
        options: SessionCreateOptions,
        context: &Context,
    ) -> Result<Arc<dyn Session>, String> {
        if *self.closed.lock().unwrap() {
            return Err("JsonlSessionRepo is closed".to_string());
        }
        // JsonlSessionRepo 需要 cwd；SessionCreateOptions 无 cwd 时默认 "."。
        let create_options = JsonlSessionCreateOptions {
            base: options.clone(),
            cwd: ".".to_string(),
        };
        let id = create_options.base.id.clone().unwrap_or_else(pi_ai::uuidv7);
        let created_at = pi_ai::utils::uuid::now_ms() as u64;
        let root = self.root(context).await?;
        let directory = self
            .file_system
            .join_path(
                &[&root, &session_directory_name(&create_options.cwd)],
                context.abort_signal(),
            )
            .await
            .map_err(|e| e.message)?;
        let _ = self
            .file_system
            .create_dir(&directory, true, context.abort_signal())
            .await;
        let path = self
            .file_system
            .join_path(
                &[&directory, &session_file_name(created_at, &id)],
                context.abort_signal(),
            )
            .await
            .map_err(|e| e.message)?;

        let header = JsonlStorageHeader {
            v: 4,
            kind: "header".to_string(),
            id: id.clone(),
            storage_version: JSONL_STORAGE_VERSION,
            created_at,
            cwd: create_options.cwd.clone(),
            parent_session_id: create_options.base.parent_session_id.clone(),
            legacy_parent_session_path: None,
            next_seq: None,
        };
        let storage = Arc::new(
            JsonlStorage::create(
                JsonlStorageOptions {
                    file_system: Arc::clone(&self.file_system),
                    path: path.clone(),
                },
                header,
                Vec::new(),
                context,
            )
            .await?,
        );

        let metadata = JsonlSessionMetadata {
            base: SessionMetadata {
                id,
                created_at,
                storage_version: JSONL_STORAGE_VERSION,
                cwd: Some(create_options.cwd),
                parent_session_id: create_options.base.parent_session_id,
                legacy_parent_session_path: None,
            },
            cwd: ".".to_string(),
            path,
            modified_at: created_at,
        };
        self.open_sessions
            .lock()
            .unwrap()
            .insert(metadata.base.id.clone(), Arc::clone(&storage));
        Ok(Arc::new(StorageBackedSession::new(
            metadata.base,
            storage,
            None,
        )))
    }

    async fn open(
        &self,
        metadata: SessionMetadata,
        context: &Context,
    ) -> Result<Arc<dyn Session>, String> {
        if *self.closed.lock().unwrap() {
            return Err("JsonlSessionRepo is closed".to_string());
        }
        if let Some(storage) = self
            .open_sessions
            .lock()
            .unwrap()
            .get(&metadata.id)
            .cloned()
        {
            return Ok(Arc::new(StorageBackedSession::new(metadata, storage, None)));
        }
        let path = metadata
            .cwd
            .clone()
            .map(|cwd| {
                let root = self.sessions_root_input.clone();
                let directory = session_directory_name(&cwd);
                format!(
                    "{root}/{directory}/{}",
                    session_file_name(metadata.created_at, &metadata.id)
                )
            })
            .ok_or_else(|| "Session metadata missing cwd".to_string())?;
        let storage = Arc::new(
            JsonlStorage::open(
                JsonlStorageOptions {
                    file_system: Arc::clone(&self.file_system),
                    path,
                },
                context,
            )
            .await?,
        );
        self.open_sessions
            .lock()
            .unwrap()
            .insert(metadata.id.clone(), Arc::clone(&storage));
        Ok(Arc::new(StorageBackedSession::new(metadata, storage, None)))
    }

    async fn list(&self, _context: &Context) -> Result<Vec<SessionMetadata>, String> {
        // 简化：返回空列表（完整目录扫描后续补）。
        Ok(Vec::new())
    }

    async fn delete(&self, metadata: SessionMetadata, context: &Context) -> Result<(), String> {
        self.open_sessions.lock().unwrap().remove(&metadata.id);
        if let Some(cwd) = &metadata.cwd {
            let root = self.root(context).await?;
            let directory = self
                .file_system
                .join_path(
                    &[&root, &session_directory_name(cwd)],
                    context.abort_signal(),
                )
                .await
                .map_err(|e| e.message)?;
            let path = self
                .file_system
                .join_path(
                    &[
                        &directory,
                        &session_file_name(metadata.created_at, &metadata.id),
                    ],
                    context.abort_signal(),
                )
                .await
                .map_err(|e| e.message)?;
            let _ = self
                .file_system
                .remove(&path, false, false, context.abort_signal())
                .await;
        }
        Ok(())
    }

    async fn fork(
        &self,
        source: SessionMetadata,
        options: ForkOptions,
        context: &Context,
    ) -> Result<Arc<dyn Session>, String> {
        // 简化：内存 fork（完整 JSONL fork 后续补）。
        let source_storage = self
            .open_sessions
            .lock()
            .unwrap()
            .get(&source.id)
            .cloned()
            .ok_or_else(|| format!("Session not found: {}", source.id))?;
        let _ = source_storage;
        let _ = options;
        let _ = context;
        Err("JSONL fork is not yet implemented".to_string())
    }
}
