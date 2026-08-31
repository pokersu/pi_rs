//! Rust 翻译自 packages/agent/src/harness/tools/file-mutation-queue.ts
//!
//! 注：TS 用 `WeakMap<ExecutionEnv, ...>` 按 env 实例隔离队列；Rust 中按规范化路径
//! 在全局表中串行化（同一路径的 mutation 互斥）。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::harness::result::get_or_throw;
use crate::harness::types::ExecutionEnv;

type Queue = Arc<tokio::sync::Mutex<()>>;

static QUEUES: std::sync::OnceLock<Mutex<HashMap<String, Queue>>> = std::sync::OnceLock::new();

fn queues() -> &'static Mutex<HashMap<String, Queue>> {
    QUEUES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 对应 `withFileMutationQueue`
pub async fn with_file_mutation_queue<T, F>(env: &Arc<dyn ExecutionEnv>, path: &str, f: F) -> T
where
    F: FnOnce() -> Pin<Box<dyn Future<Output = T> + Send>> + Send,
    T: Send,
{
    let absolute = get_or_throw(env.absolute_path(path, None).await);
    let key = match env.canonical_path(&absolute, None).await {
        Ok(p) => p,
        Err(_) => absolute,
    };

    let queue = {
        let mut map = queues().lock().unwrap();
        map.get(&key).cloned().unwrap_or_else(|| {
            let q = Arc::new(tokio::sync::Mutex::new(()));
            map.insert(key.clone(), q.clone());
            q
        })
    };

    let _guard = queue.lock().await;
    f().await
}
