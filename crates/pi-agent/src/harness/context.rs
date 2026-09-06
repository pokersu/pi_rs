//! Rust 翻译自 packages/chord/src/context/index.ts 的 Context 子集
//!（agent harness 实际使用的最小等价物，经 harness/context.ts 暴露）。
//!
//! Context 是不可变链表：`with_*` 派生新节点，`value` 沿父链查找。abort signal
//! 采用「合并」语义（父 signal 与子 signal 任一触发即 abort），`withoutAbortSignal`
//! 采用「遮蔽」语义（覆盖为 None）。

use std::any::Any;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use pi_ai::AbortError;
use pi_ai::AbortSignal;
use pi_telemetry::{ErasedTelemetryContext, NOOP_TELEMETRY_CONTEXT};

/// 对应 `ContextKey<T>`：类型化 key，token 全局唯一。
pub struct ContextKey<T: 'static> {
    token: usize,
    #[allow(dead_code)]
    description: &'static str,
    _marker: PhantomData<T>,
}

static NEXT_KEY_TOKEN: AtomicUsize = AtomicUsize::new(0);

/// 对应 `createContextKey<T>(description)`。
pub fn create_context_key<T: 'static>(description: &'static str) -> ContextKey<T> {
    ContextKey {
        token: NEXT_KEY_TOKEN.fetch_add(1, Ordering::Relaxed),
        description,
        _marker: PhantomData,
    }
}

/// 对应 `Context`：不可变链表节点。
#[derive(Clone)]
pub struct Context {
    inner: Arc<ContextNode>,
}

enum ContextNode {
    Empty(#[allow(dead_code)] &'static str),
    Value {
        parent: Context,
        key_token: usize,
        value: Arc<dyn Any + Send + Sync>,
    },
}

static ABORT_SIGNAL_KEY: LazyLock<ContextKey<Option<AbortSignal>>> =
    LazyLock::new(|| create_context_key("abortSignal"));
static TELEMETRY_KEY: LazyLock<ContextKey<Arc<dyn ErasedTelemetryContext>>> =
    LazyLock::new(|| create_context_key("pi.telemetryContext"));

/// 对应 `BACKGROUND_CONTEXT`。
pub static BACKGROUND_CONTEXT: LazyLock<Context> = LazyLock::new(|| Context {
    inner: Arc::new(ContextNode::Empty("[Context BACKGROUND_CONTEXT]")),
});

/// 对应 `TODO_CONTEXT`。
pub static TODO_CONTEXT: LazyLock<Context> = LazyLock::new(|| Context {
    inner: Arc::new(ContextNode::Empty("[Context TODO_CONTEXT]")),
});

impl Context {
    /// 对应 `context.value(key)`。
    pub fn value<T: 'static>(&self, key: &ContextKey<T>) -> Option<&T> {
        let mut node: &ContextNode = &self.inner;
        loop {
            match node {
                ContextNode::Empty(_) => return None,
                ContextNode::Value {
                    parent,
                    key_token,
                    value,
                } => {
                    if *key_token == key.token {
                        return value.downcast_ref::<T>();
                    }
                    node = &parent.inner;
                }
            }
        }
    }

    /// 对应 `context.abortSignal`。
    pub fn abort_signal(&self) -> Option<&AbortSignal> {
        self.value(&ABORT_SIGNAL_KEY).and_then(|v| v.as_ref())
    }
}

/// 对应 `withContextValue(key, value, parent)`。
pub fn with_context_value<T: Send + Sync + 'static>(
    key: &ContextKey<T>,
    value: T,
    parent: &Context,
) -> Context {
    Context {
        inner: Arc::new(ContextNode::Value {
            parent: parent.clone(),
            key_token: key.token,
            value: Arc::new(value),
        }),
    }
}

/// 对应 `withAbortSignal(signal, context)`：父 signal 与子 signal 合并。
pub fn with_abort_signal(signal: &AbortSignal, context: &Context) -> Context {
    let combined = match context.abort_signal() {
        None => signal.clone(),
        Some(parent) => AbortSignal::any(&[parent.clone(), signal.clone()]),
    };
    with_context_value(&ABORT_SIGNAL_KEY, Some(combined), context)
}

/// 对应 `withoutAbortSignal(context)`：遮蔽调用方取消（仅用于强制清理）。
pub fn without_abort_signal(context: &Context) -> Context {
    with_context_value(&ABORT_SIGNAL_KEY, None::<AbortSignal>, context)
}

/// 对应 `withCancel(context)`：派生可独立取消的子 context。
pub fn with_cancel(context: &Context) -> (Context, impl Fn() + 'static) {
    let controller = AbortSignal::new();
    let child = with_abort_signal(&controller, context);
    (child, move || controller.abort())
}

/// 对应 `awaitWithContext(promise, context)`：等待 future 直至完成或 abort。
pub async fn await_with_context<T>(
    future: impl Future<Output = T>,
    context: &Context,
) -> Result<T, AbortError> {
    let Some(signal) = context.abort_signal() else {
        return Ok(future.await);
    };
    if signal.aborted() {
        return Err(AbortError);
    }
    tokio::select! {
        _ = signal.cancelled() => Err(AbortError),
        value = future => Ok(value),
    }
}

/// 对应 `getTelemetryContext(context)`。
pub fn get_telemetry_context(context: &Context) -> Arc<dyn ErasedTelemetryContext> {
    context
        .value(&TELEMETRY_KEY)
        .cloned()
        .unwrap_or_else(|| Arc::new(NOOP_TELEMETRY_CONTEXT.clone()))
}

/// 对应 `withTelemetryContext(telemetryContext, context)`。
pub fn with_telemetry_context(
    telemetry: Arc<dyn ErasedTelemetryContext>,
    context: &Context,
) -> Context {
    with_context_value(&TELEMETRY_KEY, telemetry, context)
}
