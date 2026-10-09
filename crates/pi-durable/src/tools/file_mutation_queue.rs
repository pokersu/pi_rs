//! 对应 `tools/file-mutation-queue.ts`：按文件串行化 `edit` / `write` 变更。

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};

use crate::chord::context::Context;
use crate::env::{ExecutionEnv, FileErrorCode};
use crate::session::SessionError;

/// 每个文件（文件系统 id + 规范路径）的变更链尾，进程内共享。
static QUEUES: LazyLock<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

async fn mutation_key(
    env: &dyn ExecutionEnv,
    path: &str,
    context: &dyn Context,
) -> Result<String, SessionError> {
    let absolute_path = env.absolute_path(path, context).await?;
    let canonical = canonical(env, &absolute_path, context).await?;
    Ok(format!("{}\0{canonical}", env.id()))
}

/// 对应 `canonical`：规范路径；尚不存在的文件用其规范父目录 + 名字。
async fn canonical(
    env: &dyn ExecutionEnv,
    absolute_path: &str,
    context: &dyn Context,
) -> Result<String, SessionError> {
    match env.canonical_path(absolute_path, context).await {
        Ok(value) => Ok(value),
        Err(error) if error.code == FileErrorCode::NotSupported => Ok(absolute_path.to_string()),
        Err(error) if error.code == FileErrorCode::NotFound => {
            let parent = env.join_path(&[absolute_path, ".."], context).await?;
            if parent == absolute_path || !absolute_path.starts_with(&parent) {
                return Ok(absolute_path.to_string());
            }
            let name = absolute_path
                [parent.len() + usize::from(!(parent.ends_with('/') || parent.ends_with('\\')))..]
                .to_string();
            let canonical_parent = Box::pin(canonical(env, &parent, context)).await?;
            Ok(env.join_path(&[&canonical_parent, &name], context).await?)
        }
        Err(error) => Err(SessionError::from(error)),
    }
}

/// 对应 `withFileMutationQueue`：串行化同一文件的 `edit` / `write` 变更。
pub async fn with_file_mutation_queue<T, F, Fut>(
    env: &dyn ExecutionEnv,
    path: &str,
    f: F,
    context: &dyn Context,
) -> Result<T, SessionError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, SessionError>>,
{
    let key = mutation_key(env, path, context).await?;
    let lock = {
        let mut queues = QUEUES.lock().expect("mutation queues");
        Arc::clone(
            queues
                .entry(key)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    };
    let _guard = lock.lock().await;
    f().await
}
