//! Rust 翻译自 packages/ai/src/auth/credential-store.ts

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::auth::types::{
    AuthOperationOptions, Credential, CredentialInfo, CredentialModifyFn, CredentialStore,
};
use crate::types::AbortSignal;
use crate::utils::abort::{BoxError, operation_signal, race_with_abort_signal};

struct Inner {
    credentials: Mutex<BTreeMap<String, Credential>>,
    chains: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

/// 对应 `InMemoryCredentialStore`：默认的内存凭据存储，按 `Provider.id` 键控，
/// 每个 provider 一个凭据。写操作通过 per-provider 串行队列实现。
pub struct InMemoryCredentialStore {
    inner: Arc<Inner>,
}

impl Default for InMemoryCredentialStore {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                credentials: Mutex::new(BTreeMap::new()),
                chains: Mutex::new(BTreeMap::new()),
            }),
        }
    }
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 对应 `enqueue`：按 provider id 串行化任务，活动工作完成前不释放链。
    async fn enqueue<T, F, Fut>(
        &self,
        provider_id: &str,
        signal: &AbortSignal,
        task: F,
    ) -> Result<T, BoxError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, BoxError>>,
    {
        let lock = {
            let mut chains = self.inner.chains.lock().await;
            chains
                .entry(provider_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let operation = async move {
            let _guard = lock.lock().await;
            signal
                .throw_if_aborted()
                .map_err(|e| Box::new(e) as BoxError)?;
            task().await
        };
        race_with_abort_signal(operation, signal).await
    }
}

#[async_trait::async_trait]
impl CredentialStore for InMemoryCredentialStore {
    /// 对应 `read(providerId, options?)`。
    async fn read(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, BoxError> {
        if let Some(signal) = options.and_then(|o| o.signal.as_ref()) {
            signal
                .throw_if_aborted()
                .map_err(|e| Box::new(e) as BoxError)?;
        }
        Ok(self
            .inner
            .credentials
            .lock()
            .await
            .get(provider_id)
            .cloned())
    }

    /// 对应 `list(options?)`。
    async fn list(
        &self,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Vec<CredentialInfo>, BoxError> {
        if let Some(signal) = options.and_then(|o| o.signal.as_ref()) {
            signal
                .throw_if_aborted()
                .map_err(|e| Box::new(e) as BoxError)?;
        }
        let credentials = self.inner.credentials.lock().await;
        Ok(credentials
            .iter()
            .map(|(provider_id, credential)| CredentialInfo {
                provider_id: provider_id.clone(),
                kind: credential.kind(),
            })
            .collect())
    }

    /// 对应 `modify(providerId, fn, options?)`。
    async fn modify(
        &self,
        provider_id: &str,
        fn_: CredentialModifyFn,
        options: Option<&AuthOperationOptions>,
    ) -> Result<Option<Credential>, BoxError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        let signal_for_task = signal.clone();
        let inner = self.inner.clone();
        let pid = provider_id.to_string();
        self.enqueue(provider_id, &signal, move || {
            let inner = inner.clone();
            let pid = pid.clone();
            let signal = signal_for_task.clone();
            async move {
                let current = inner.credentials.lock().await.get(&pid).cloned();
                let next = fn_(current.clone()).await?;
                signal
                    .throw_if_aborted()
                    .map_err(|e| Box::new(e) as BoxError)?;
                if let Some(next_cred) = &next {
                    inner
                        .credentials
                        .lock()
                        .await
                        .insert(pid.clone(), next_cred.clone());
                }
                Ok(next.or(current))
            }
        })
        .await
    }

    /// 对应 `delete(providerId, options?)`。
    async fn delete(
        &self,
        provider_id: &str,
        options: Option<&AuthOperationOptions>,
    ) -> Result<(), BoxError> {
        let signal = operation_signal(options.and_then(|o| o.signal.as_ref()));
        let inner = self.inner.clone();
        let pid = provider_id.to_string();
        self.enqueue(provider_id, &signal, move || {
            let inner = inner.clone();
            let pid = pid.clone();
            async move {
                inner.credentials.lock().await.remove(&pid);
                Ok(())
            }
        })
        .await
    }
}
