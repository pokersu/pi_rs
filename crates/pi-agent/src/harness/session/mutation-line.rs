//! Rust 翻译自 packages/agent/src/harness/session/mutation-line.ts
//!
//! 串行化一个 Session 的完整 read-modify-write 任务，并提供 sealed 屏障。

use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Mutex;

struct Inner {
    lock: Mutex<()>,
    sealed_error: StdMutex<Option<String>>,
}

/// 对应 `MutationLine`。
#[derive(Clone)]
pub struct MutationLine {
    inner: Arc<Inner>,
}

impl Default for MutationLine {
    fn default() -> Self {
        Self::new()
    }
}

impl MutationLine {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                lock: Mutex::new(()),
                sealed_error: StdMutex::new(None),
            }),
        }
    }

    /// 对应 `run(operation)`：串行执行；sealed 后拒绝。
    pub async fn run<T, F, Fut>(&self, operation: F) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _guard = self.inner.lock.lock().await;
        if let Some(error) = self.inner.sealed_error.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(operation().await)
    }

    /// 对应 `seal(error)`：设置 sealed error 并等待排空。
    pub async fn seal(&self, error: String) {
        {
            let mut sealed = self.inner.sealed_error.lock().unwrap();
            if sealed.is_none() {
                *sealed = Some(error);
            }
        }
        let _guard = self.inner.lock.lock().await;
    }
}
