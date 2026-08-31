//! Rust 翻译自 packages/ai/src/utils/event-stream.ts
//!
//! 通用事件流：生产者 `push` 事件、消费者 async 迭代、最终结果通过 `result()` 获取。
//! 内部用 `Arc` 共享状态，`EventStream` 可 clone：clone 出的句柄只用于 `push`/`end`，
//! 原始句柄用于消费。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use futures::channel::{mpsc, oneshot};
use futures::future::Shared;
use futures::{FutureExt, Stream};

use crate::types::{AssistantMessage, AssistantMessageEvent};

struct EventStreamInner<T, R> {
    sender: mpsc::UnboundedSender<T>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<T>>>,
    final_tx: Mutex<Option<oneshot::Sender<R>>>,
    final_rx: Shared<oneshot::Receiver<R>>,
    is_complete: fn(&T) -> bool,
    extract_result: fn(&T) -> R,
    done: AtomicBool,
}

/// 对应 TS 的 `EventStream<T, R = T>`（`AsyncIterable<T>`）。
pub struct EventStream<T, R> {
    inner: Arc<EventStreamInner<T, R>>,
}

impl<T, R> Clone for EventStream<T, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T, R> EventStream<T, R>
where
    T: Send + 'static,
    R: Clone + Send + Sync + 'static,
{
    /// 对应 `constructor(isComplete, extractResult)`
    pub fn new(is_complete: fn(&T) -> bool, extract_result: fn(&T) -> R) -> Self {
        let (sender, receiver) = mpsc::unbounded();
        let (final_tx, final_rx) = oneshot::channel();
        Self {
            inner: Arc::new(EventStreamInner {
                sender,
                receiver: Mutex::new(Some(receiver)),
                final_tx: Mutex::new(Some(final_tx)),
                final_rx: final_rx.shared(),
                is_complete,
                extract_result,
                done: AtomicBool::new(false),
            }),
        }
    }

    /// 对应 `push(event)`
    pub fn push(&self, event: T) {
        if self.inner.done.load(Ordering::Relaxed) {
            return;
        }

        if (self.inner.is_complete)(&event) {
            self.inner.done.store(true, Ordering::Relaxed);
            if let Some(tx) = self.inner.final_tx.lock().unwrap().take() {
                let _ = tx.send((self.inner.extract_result)(&event));
            }
        }

        let _ = self.inner.sender.unbounded_send(event);
    }

    /// 对应 `end(result?)`
    pub fn end(&self, result: Option<R>) {
        self.inner.done.store(true, Ordering::Relaxed);
        if let Some(r) = result
            && let Some(tx) = self.inner.final_tx.lock().unwrap().take()
        {
            let _ = tx.send(r);
        }
        self.inner.sender.close_channel();
    }

    /// 对应 `result(): Promise<R>`。返回可多次 await 的 future。
    pub fn result(&self) -> impl Future<Output = R> + '_ {
        let rx = self.inner.final_rx.clone();
        async move { rx.await.expect("event stream ended without a final result") }
    }
}

impl<T, R> Stream for EventStream<T, R>
where
    T: Send + 'static,
    R: Clone + Send + Sync + 'static,
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<T>> {
        let mut guard = self.inner.receiver.lock().unwrap();
        let receiver = guard.as_mut().expect("event stream receiver unavailable");
        Pin::new(receiver).poll_next(cx)
    }
}

/// 对应 TS 的 `AssistantMessageEventStream`。
pub type AssistantMessageEventStream = EventStream<AssistantMessageEvent, AssistantMessage>;

/// 对应 TS 的 `createAssistantMessageEventStream()` 工厂。
pub fn create_assistant_message_event_stream() -> AssistantMessageEventStream {
    EventStream::new(
        |event| {
            matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            )
        },
        |event| match event {
            AssistantMessageEvent::Done { message, .. } => message.clone(),
            AssistantMessageEvent::Error { error, .. } => error.clone(),
            _ => panic!("Unexpected event type for final result"),
        },
    )
}
