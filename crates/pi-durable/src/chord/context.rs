//! 对应 chord `src/context/index.ts` 与 `types.ts` 的 `Context`。
//!
//! # 与上游的差异
//!
//! chord 的 `Context` 有两部分能力：`abortSignal` 与泛型上下文值
//! （`ContextKey<T>` + `value<T>(key)`，用 `Symbol` 做身份）。
//! **`pi-durable` 只使用 `abortSignal`**（全仓无任何 `context.value(...)` 调用），
//! 因此这里只实现前者；`ContextKey` / `value` 留待有真实消费方时再补。

use std::future::Future;
use std::sync::{Arc, LazyLock};

use pi_ai::{AbortError, AbortSignal};

/// 对应 `Context`（仅保留 durable 实际使用的 `abortSignal` 部分）。
pub trait Context: Send + Sync {
    /// 对应 `context.abortSignal`。
    fn abort_signal(&self) -> Option<AbortSignal>;

    /// 对应 `toString()`。
    fn describe(&self) -> String;
}

/// 对应 `EmptyContext`：不携带任何值。
pub struct EmptyContext {
    name: String,
}

impl EmptyContext {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl Context for EmptyContext {
    fn abort_signal(&self) -> Option<AbortSignal> {
        None
    }

    fn describe(&self) -> String {
        self.name.clone()
    }
}

/// 对应上游 `ContextValue` 中「仅覆盖 abort signal」的形态。
struct AbortSignalContext {
    parent: Arc<dyn Context>,
    signal: Option<AbortSignal>,
}

impl Context for AbortSignalContext {
    fn abort_signal(&self) -> Option<AbortSignal> {
        self.signal.clone()
    }

    fn describe(&self) -> String {
        self.parent.describe()
    }
}

/// 对应 `BACKGROUND_CONTEXT`。
pub static BACKGROUND_CONTEXT: LazyLock<Arc<dyn Context>> =
    LazyLock::new(|| Arc::new(EmptyContext::new("[Context BACKGROUND_CONTEXT]")));

/// 对应 `TODO_CONTEXT`。
pub static TODO_CONTEXT: LazyLock<Arc<dyn Context>> =
    LazyLock::new(|| Arc::new(EmptyContext::new("[Context TODO_CONTEXT]")));

/// 对应 `withAbortSignal`：派生出「父 signal 与给定 signal 任一取消即取消」的上下文。
///
/// 父上下文保持不变。
pub fn with_abort_signal(signal: AbortSignal, parent: Arc<dyn Context>) -> Arc<dyn Context> {
    let combined = match parent.abort_signal() {
        None => signal,
        Some(parent_signal) => AbortSignal::any(&[parent_signal, signal]),
    };
    Arc::new(AbortSignalContext {
        parent,
        signal: Some(combined),
    })
}

/// 对应 `withoutAbortSignal`：保留其余值但去掉调用方取消能力（仅用于强制清理路径）。
pub fn without_abort_signal(parent: Arc<dyn Context>) -> Arc<dyn Context> {
    Arc::new(AbortSignalContext {
        parent,
        signal: None,
    })
}

/// 对应 `withCancel`：派生出可独立取消的子上下文。
pub fn with_cancel(parent: Arc<dyn Context>) -> (Arc<dyn Context>, impl Fn() + Send + Sync) {
    let controller = AbortSignal::new();
    let context = with_abort_signal(controller.clone(), parent);
    let cancel = move || controller.abort();
    (context, cancel)
}

/// 对应 `awaitWithContext`：等待 promise，直到 settle 或本次调用被取消。
///
/// 取消只让**本等待者**失败，不会取消底层 future。
pub async fn await_with_context<T, F>(future: F, context: &dyn Context) -> Result<T, AbortError>
where
    F: Future<Output = T>,
{
    let Some(signal) = context.abort_signal() else {
        return Ok(future.await);
    };
    if signal.aborted() {
        return Err(AbortError);
    }
    tokio::select! {
        value = future => Ok(value),
        () = signal.cancelled() => Err(AbortError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_context_has_no_abort_signal() {
        assert!(BACKGROUND_CONTEXT.abort_signal().is_none());
        assert_eq!(
            BACKGROUND_CONTEXT.describe(),
            "[Context BACKGROUND_CONTEXT]"
        );
    }

    #[test]
    fn with_abort_signal_exposes_signal_and_composes_with_parent() {
        let signal = AbortSignal::new();
        let context = with_abort_signal(signal.clone(), Arc::clone(&BACKGROUND_CONTEXT));
        let exposed = context.abort_signal().expect("signal");
        assert!(!exposed.aborted());

        signal.abort();
        assert!(
            context.abort_signal().unwrap().aborted(),
            "取消应传播到上下文"
        );
    }

    #[tokio::test]
    async fn with_abort_signal_combines_parent_signal() {
        let parent_signal = AbortSignal::new();
        let parent = with_abort_signal(parent_signal.clone(), Arc::clone(&BACKGROUND_CONTEXT));
        let child_signal = AbortSignal::new();
        let child = with_abort_signal(child_signal, parent);

        parent_signal.abort();
        // 注：`AbortSignal::any` 的传播基于 `tokio::spawn`（上游为同步事件监听），
        // 因此取消不是同步可见的，这里让出一次调度。
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            child.abort_signal().unwrap().aborted(),
            "父 signal 取消应使子上下文取消",
        );
    }

    #[test]
    fn without_abort_signal_drops_cancellation() {
        let signal = AbortSignal::new();
        let context = with_abort_signal(signal, Arc::clone(&BACKGROUND_CONTEXT));
        let detached = without_abort_signal(context);
        assert!(detached.abort_signal().is_none());
    }

    #[tokio::test]
    async fn await_with_context_passes_through_without_signal() {
        let value = await_with_context(async { 7 }, BACKGROUND_CONTEXT.as_ref())
            .await
            .expect("no signal -> no cancellation");
        assert_eq!(value, 7);
    }

    #[tokio::test]
    async fn await_with_context_rejects_when_already_aborted() {
        let signal = AbortSignal::new();
        signal.abort();
        let context = with_abort_signal(signal, Arc::clone(&BACKGROUND_CONTEXT));

        let result = await_with_context(async { 7 }, context.as_ref()).await;
        assert!(result.is_err());
    }
}
