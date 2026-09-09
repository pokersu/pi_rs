//! Rust 翻译自 packages/agent/src/harness/session/jsonl/repo.ts（核心）

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use crate::harness::context::Context;
use crate::harness::session::jsonl::codec::{JsonlParsedSessionHeader, parse_jsonl_session_header};
use crate::harness::session::jsonl::fork::{
    JsonlForkInput, JsonlForkSourceMetadata, run_jsonl_fork,
};
use crate::harness::session::jsonl::storage::{JsonlStorage, JsonlStorageOptions};
use crate::harness::session::jsonl::types::{
    JSONL_STORAGE_VERSION, JsonlSessionCreateOptions, JsonlSessionMetadata, JsonlStorageHeader,
};
use crate::harness::session::session::StorageBackedSession;
use crate::harness::session::types::{
    ForkOptions, Session, SessionCreateOptions, SessionMetadata, SessionRepo,
};
use crate::harness::types::{FileKind, FileSystem};

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
            .absolute_path(&self.sessions_root_input, context)
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
            .join_path(&[&root, &session_directory_name(&metadata.cwd)], context)
            .await
            .map_err(|e| e.message)?;
        self.file_system
            .join_path(
                &[
                    &directory,
                    &session_file_name(metadata.base.created_at, &metadata.base.id),
                ],
                context,
            )
            .await
            .map_err(|e| e.message)
    }

    /// 对应 `readSessionMetadata`：读取文件首行 header，构造 `SessionMetadata`。
    async fn read_session_metadata(
        &self,
        file: &crate::harness::types::FileInfo,
        context: &Context,
    ) -> Result<Option<SessionMetadata>, String> {
        let lines = self
            .file_system
            .read_text_lines(&file.path, Some(1), context)
            .await
            .map_err(|e| e.message)?;
        let Some(first) = lines.first() else {
            return Ok(None);
        };
        let parsed = match parse_jsonl_session_header(first) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(None),
        };
        let JsonlParsedSessionHeader::V4 { header } = parsed else {
            // V3Legacy 需要 legacy-v3 迁移（未复刻），跳过。
            return Ok(None);
        };
        Ok(Some(SessionMetadata {
            id: header.id,
            created_at: header.created_at,
            storage_version: header.storage_version,
            cwd: Some(header.cwd),
            parent_session_id: header.parent_session_id,
            legacy_parent_session_path: header.legacy_parent_session_path,
        }))
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
                context,
            )
            .await
            .map_err(|e| e.message)?;
        let _ = self.file_system.create_dir(&directory, true, context).await;
        let path = self
            .file_system
            .join_path(&[&directory, &session_file_name(created_at, &id)], context)
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

    async fn list(&self, context: &Context) -> Result<Vec<SessionMetadata>, String> {
        if *self.closed.lock().unwrap() {
            return Err("JsonlSessionRepo is closed".to_string());
        }
        let root = self.root(context).await?;
        let exists = self
            .file_system
            .exists(&root, context)
            .await
            .map_err(|e| e.message)?;
        if !exists {
            return Ok(Vec::new());
        }
        let directories = self
            .file_system
            .list_dir(&root, context)
            .await
            .map_err(|e| e.message)?;
        let mut metadata: Vec<SessionMetadata> = Vec::new();
        for directory in directories {
            if directory.kind != FileKind::Directory {
                continue;
            }
            let files = self
                .file_system
                .list_dir(&directory.path, context)
                .await
                .map_err(|e| e.message)?;
            for file in files {
                if file.kind == FileKind::Directory || !file.name.ends_with(".jsonl") {
                    continue;
                }
                if let Some(discovered) = self.read_session_metadata(&file, context).await? {
                    metadata.push(discovered);
                }
            }
        }
        metadata.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then(left.id.cmp(&right.id))
                .then(left.cwd.cmp(&right.cwd))
        });
        Ok(metadata)
    }

    async fn delete(&self, metadata: SessionMetadata, context: &Context) -> Result<(), String> {
        self.open_sessions.lock().unwrap().remove(&metadata.id);
        if let Some(cwd) = &metadata.cwd {
            let root = self.root(context).await?;
            let directory = self
                .file_system
                .join_path(&[&root, &session_directory_name(cwd)], context)
                .await
                .map_err(|e| e.message)?;
            let path = self
                .file_system
                .join_path(
                    &[
                        &directory,
                        &session_file_name(metadata.created_at, &metadata.id),
                    ],
                    context,
                )
                .await
                .map_err(|e| e.message)?;
            let _ = self.file_system.remove(&path, false, false, context).await;
        }
        Ok(())
    }

    async fn fork(
        &self,
        source: SessionMetadata,
        options: ForkOptions,
        context: &Context,
    ) -> Result<Arc<dyn Session>, String> {
        if *self.closed.lock().unwrap() {
            return Err("JsonlSessionRepo is closed".to_string());
        }
        let id = match &options {
            ForkOptions::Branch { id, .. } | ForkOptions::Tree { id } => id.clone(),
        }
        .unwrap_or_else(pi_ai::uuidv7);
        let cwd = source.cwd.clone().unwrap_or_else(|| ".".to_string());
        let created_at = pi_ai::utils::uuid::now_ms() as u64;
        let source_storage = self.open_sessions.lock().unwrap().get(&source.id).cloned();

        let source_root = self.root(context).await?;
        let source_directory = self
            .file_system
            .join_path(&[&source_root, &session_directory_name(&cwd)], context)
            .await
            .map_err(|e| e.message)?;
        let source_path = self
            .file_system
            .join_path(
                &[
                    &source_directory,
                    &session_file_name(source.created_at, &source.id),
                ],
                context,
            )
            .await
            .map_err(|e| e.message)?;
        let metadata = JsonlForkSourceMetadata {
            id: source.id.clone(),
            cwd: cwd.clone(),
            path: source_path,
        };

        let input = match source_storage {
            Some(storage) => {
                let next_seq = storage.capture_fork_next_seq().await?;
                JsonlForkInput::Open { metadata, next_seq }
            }
            None => {
                let lines = self
                    .file_system
                    .read_text_lines(&metadata.path, Some(1), context)
                    .await
                    .map_err(|e| e.message)?;
                if let Some(first) = lines.first()
                    && matches!(
                        parse_jsonl_session_header(first),
                        Ok(JsonlParsedSessionHeader::V3Legacy { .. })
                    )
                {
                    return Err(
                        "Cannot fork a legacy v3 JSONL session; legacy v3 migration is not implemented"
                            .to_string(),
                    );
                }
                JsonlForkInput::Closed { metadata }
            }
        };

        let root = self.root(context).await?;
        let directory = self
            .file_system
            .join_path(&[&root, &session_directory_name(&cwd)], context)
            .await
            .map_err(|e| e.message)?;
        let _ = self.file_system.create_dir(&directory, true, context).await;
        let path = self
            .file_system
            .join_path(&[&directory, &session_file_name(created_at, &id)], context)
            .await
            .map_err(|e| e.message)?;
        let header = JsonlStorageHeader {
            v: 4,
            kind: "header".to_string(),
            id: id.clone(),
            storage_version: JSONL_STORAGE_VERSION,
            created_at,
            cwd: cwd.clone(),
            parent_session_id: Some(source.id.clone()),
            legacy_parent_session_path: None,
            next_seq: None,
        };
        run_jsonl_fork(
            input,
            self.file_system.as_ref(),
            &path,
            header,
            &options,
            context,
        )
        .await?;

        let storage = Arc::new(
            JsonlStorage::open(
                JsonlStorageOptions {
                    file_system: Arc::clone(&self.file_system),
                    path: path.clone(),
                },
                context,
            )
            .await?,
        );
        let metadata = SessionMetadata {
            id,
            created_at,
            storage_version: JSONL_STORAGE_VERSION,
            cwd: Some(cwd),
            parent_session_id: Some(source.id.clone()),
            legacy_parent_session_path: None,
        };
        self.open_sessions
            .lock()
            .unwrap()
            .insert(metadata.id.clone(), Arc::clone(&storage));
        Ok(Arc::new(StorageBackedSession::new(metadata, storage, None)))
    }
}
