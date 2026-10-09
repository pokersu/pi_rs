//! 对应 `harness/util.ts`：harness 内部的小工具。
//!
//! [`Waiters`]：按 key 挂起等待，每个只能被 `resolve` / `reject_all` / 取消结清一次。
//! [`scan_all`]：分页扫描直至游标穷尽。

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::channel::oneshot;
use futures::future::BoxFuture;

use crate::chord::context::Context;
use crate::session::SessionError;
use crate::types::{Cursor, Page};

/// 对应 `scanAll`：按页取完一次扫描的全部条目。
///
/// 上游让异常冒泡；Rust 用泛型错误参数保留同样的传播，且不绑定具体错误类型。
pub async fn scan_all<T, E, F, Fut>(mut scan: F) -> Result<Vec<T>, E>
where
    F: FnMut(Option<Cursor>) -> Fut,
    Fut: Future<Output = Result<Page<T, Cursor>, E>>,
{
    let mut items = Vec::new();
    let mut cursor = None;
    loop {
        let page = scan(cursor).await?;
        items.extend(page.items);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(items),
        }
    }
}

/// 对应 `closedError()`：harness 已关闭。
pub fn closed_error() -> SessionError {
    SessionError::Message("Harness is closed".to_string())
}

static NEXT_WAITER_ID: AtomicU64 = AtomicU64::new(1);

/// 对应 `Waiters<K, T>`：按 key 挂起的等待。
///
/// 每个等待只结清一次：通过 [`Waiters::resolve`]、[`Waiters::reject_all`]，或它自己的 `context` 被取消。
pub struct Waiters<K, T> {
    sets: Mutex<BTreeMap<K, Vec<Slot<T>>>>,
}

struct Slot<T> {
    id: u64,
    sender: oneshot::Sender<Result<T, SessionError>>,
}

impl<K: Ord + Clone + Send + 'static, T: Clone + Send + 'static> Default for Waiters<K, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Ord + Clone + Send + 'static, T: Clone + Send + 'static> Waiters<K, T> {
    /// 对应 `new Waiters()`。
    pub fn new() -> Self {
        Self {
            sets: Mutex::new(BTreeMap::new()),
        }
    }

    /// 对应 `add(key, context)`：挂起一个等待。
    ///
    /// 返回的 future 拥有 [`Arc`]，因此可以跨出当前的 Session 线（`'static`）。
    /// 与上游一致：**入队在 `add` 时立即发生**，所以 `add` 之后、`await` 之前的 `resolve` 不会丢等待者；
    /// `context` 被取消时该等待以 [`SessionError::Aborted`] 结清（并离开集合）。
    pub fn add(
        self: &Arc<Self>,
        key: K,
        context: Arc<dyn Context>,
    ) -> BoxFuture<'static, Result<T, SessionError>> {
        let waiters = Arc::clone(self);
        let signal = context.abort_signal();
        if signal.as_ref().is_some_and(|signal| signal.aborted()) {
            return Box::pin(async move { Err(SessionError::Aborted(pi_ai::AbortError)) });
        }
        let id = NEXT_WAITER_ID.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        {
            let mut sets = self.sets.lock().expect("waiters");
            sets.entry(key.clone())
                .or_default()
                .push(Slot { id, sender });
        }
        Box::pin(async move {
            let Some(signal) = signal else {
                return receiver.await.map_err(|_| closed_error())?;
            };
            tokio::select! {
                value = receiver => value.map_err(|_| closed_error())?,
                () = signal.cancelled() => {
                    waiters.forget(&key, id);
                    Err(SessionError::Aborted(pi_ai::AbortError))
                }
            }
        })
    }

    /// 移除一个取消的等待（对应上游 `onAbort` 里的集合清理）。
    fn forget(&self, key: &K, id: u64) {
        let mut sets = self.sets.lock().expect("waiters");
        if let Some(slots) = sets.get_mut(key) {
            slots.retain(|slot| slot.id != id);
            if slots.is_empty() {
                sets.remove(key);
            }
        }
    }

    /// 对应 `keys()`：当前有等待者的键。
    pub fn keys(&self) -> Vec<K> {
        self.sets.lock().expect("waiters").keys().cloned().collect()
    }

    /// 对应 `resolve(key, value)`：结清该键下的全部等待。
    pub fn resolve(&self, key: &K, value: T) {
        let slots = self
            .sets
            .lock()
            .expect("waiters")
            .remove(key)
            .unwrap_or_default();
        for slot in slots {
            let _ = slot.sender.send(Ok(value.clone()));
        }
    }

    /// 对应 `rejectAll(error)`：以错误结清全部等待。
    pub fn reject_all(&self, error: SessionError) {
        let sets = std::mem::take(&mut *self.sets.lock().expect("waiters"));
        for (_, slots) in sets {
            for slot in slots {
                let _ = slot.sender.send(Err(error.clone()));
            }
        }
    }
}

/// 便于阅读：未使用变量的占位（保持导入面稳定）。
#[allow(dead_code)]
fn unused_arc_marker(_: Arc<()>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EntryId, JsonObject};
    use serde_json::json;

    fn cursor_after(marker: u64) -> Cursor {
        let mut map = JsonObject::new();
        map.insert("after".to_string(), json!(marker));
        map
    }

    fn page(items: Vec<EntryId>, next: Option<Cursor>) -> Page<EntryId, Cursor> {
        Page { items, next }
    }

    #[tokio::test]
    async fn scan_all_follows_every_cursor() {
        let mut calls = 0;
        let items = scan_all(|cursor: Option<Cursor>| {
            calls += 1;
            let next = match cursor {
                None => Some(cursor_after(2)),
                Some(_) => None,
            };
            async move {
                Ok::<_, SessionError>(match next {
                    Some(cursor) => page(vec![EntryId::new(1), EntryId::new(2)], Some(cursor)),
                    None => page(vec![EntryId::new(3)], None),
                })
            }
        })
        .await
        .expect("scan");
        assert_eq!(calls, 2);
        assert_eq!(
            items.iter().map(|id| id.get()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[tokio::test]
    async fn scan_all_propagates_errors() {
        let error = scan_all::<EntryId, _, _, _>(|_| async {
            Err::<Page<EntryId, Cursor>, _>(SessionError::Message("boom".to_string()))
        })
        .await
        .expect_err("error");
        assert_eq!(error.to_string(), "boom");
    }

    #[test]
    fn closed_error_matches_upstream_message() {
        assert_eq!(closed_error().to_string(), "Harness is closed");
    }

    #[tokio::test]
    async fn waiters_resolve_every_waiter_of_their_key() {
        let waiters: Arc<Waiters<String, u32>> = Arc::new(Waiters::new());
        let context = (*crate::chord::context::BACKGROUND_CONTEXT).clone();

        let resolver = {
            let waiters = Arc::clone(&waiters);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                waiters.resolve(&"k".to_string(), 7);
            })
        };
        let (first, second) = tokio::join!(
            waiters.add("k".to_string(), Arc::clone(&context)),
            waiters.add("k".to_string(), Arc::clone(&context))
        );
        resolver.await.expect("resolver");
        assert_eq!(first.expect("first"), 7);
        assert_eq!(second.expect("second"), 7);
        assert!(waiters.keys().is_empty(), "结清后该键不再有等待者");
    }

    #[tokio::test]
    async fn waiters_leave_other_keys_alone() {
        let waiters: Arc<Waiters<String, u32>> = Arc::new(Waiters::new());
        let context = (*crate::chord::context::BACKGROUND_CONTEXT).clone();

        let resolver = {
            let waiters = Arc::clone(&waiters);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                waiters.resolve(&"a".to_string(), 1);
            })
        };
        let other = {
            let waiters = Arc::clone(&waiters);
            let context = Arc::clone(&context);
            tokio::spawn(async move { waiters.add("b".to_string(), Arc::clone(&context)).await })
        };
        let resolved = waiters.add("a".to_string(), Arc::clone(&context)).await;
        resolver.await.expect("resolver");
        assert_eq!(resolved.expect("a"), 1);

        assert_eq!(waiters.keys(), vec!["b".to_string()], "另一个键仍挂起");
        waiters.reject_all(SessionError::Message("done".to_string()));
        let rejected = other.await.expect("join");
        assert_eq!(rejected.expect_err("rejected").to_string(), "done");
    }

    #[tokio::test]
    async fn waiters_settle_on_cancellation_and_leave_the_set() {
        let waiters: Arc<Waiters<String, u32>> = Arc::new(Waiters::new());
        let signal = pi_ai::AbortSignal::new();
        let context = crate::chord::context::with_abort_signal(
            signal.clone(),
            (*crate::chord::context::BACKGROUND_CONTEXT).clone(),
        );

        let canceller = {
            let signal = signal.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                signal.abort();
            })
        };
        let result = waiters.add("k".to_string(), Arc::clone(&context)).await;
        canceller.await.expect("canceller");
        assert!(matches!(
            result.expect_err("cancelled"),
            SessionError::Aborted(_)
        ));
        assert!(waiters.keys().is_empty(), "取消的等待离开集合");
    }

    #[tokio::test]
    async fn waiters_reject_an_already_aborted_context() {
        let waiters: Arc<Waiters<String, u32>> = Arc::new(Waiters::new());
        let signal = pi_ai::AbortSignal::new();
        signal.abort();
        let context = crate::chord::context::with_abort_signal(
            signal,
            (*crate::chord::context::BACKGROUND_CONTEXT).clone(),
        );
        let result = waiters.add("k".to_string(), Arc::clone(&context)).await;
        assert!(matches!(
            result.expect_err("aborted"),
            SessionError::Aborted(_)
        ));
        assert!(waiters.keys().is_empty(), "未注册任何等待者");
    }
}
