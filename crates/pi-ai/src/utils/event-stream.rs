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

    /// 流是否已结束（`done` / `end` 之后）。
    pub fn is_done(&self) -> bool {
        self.inner.done.load(Ordering::Relaxed)
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

/// 对应 TS 的 `AssistantMessageEventStream`：一个响应的最终消息带 `durationMs`（单调钟测）。
///
/// 从流的创建开始计时；除非消息已有 `durationMs` 或其 `timestamp` 早于流开始（转发别处已开始的响应）。
pub struct AssistantMessageEventStream {
    stream: EventStream<AssistantMessageEvent, AssistantMessage>,
    started_at: u64,
    started_at_monotonic: std::time::Instant,
}

impl Clone for AssistantMessageEventStream {
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
            started_at: self.started_at,
            started_at_monotonic: self.started_at_monotonic,
        }
    }
}

impl AssistantMessageEventStream {
    fn time(&self, message: &mut AssistantMessage) {
        if self.stream.is_done()
            || message.duration_ms.is_some()
            || message.timestamp < self.started_at
        {
            return;
        }
        let elapsed = self.started_at_monotonic.elapsed().as_millis() as u64;
        message.duration_ms = Some(elapsed);
    }

    /// 对应 `push(event)`：计时最终消息后投递。
    pub fn push(&self, event: AssistantMessageEvent) {
        let event = match event {
            AssistantMessageEvent::Done { reason, message } => {
                let mut message = message;
                self.time(&mut message);
                AssistantMessageEvent::Done { reason, message }
            }
            AssistantMessageEvent::Error { reason, error } => {
                let mut error = error;
                self.time(&mut error);
                AssistantMessageEvent::Error { reason, error }
            }
            other => other,
        };
        self.stream.push(event);
    }

    /// 对应 `end(result?)`：计时结果后结束。
    pub fn end(&self, result: Option<AssistantMessage>) {
        let result = result.map(|mut message| {
            self.time(&mut message);
            message
        });
        self.stream.end(result);
    }

    /// 对应 `result()`。
    pub fn result(&self) -> impl Future<Output = AssistantMessage> + '_ {
        self.stream.result()
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<AssistantMessageEvent>> {
        Pin::new(&mut self.get_mut().stream).poll_next(cx)
    }
}

/// 对应 TS 的 `createAssistantMessageEventStream()` 工厂。
pub fn create_assistant_message_event_stream() -> AssistantMessageEventStream {
    let stream = EventStream::new(
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
    );
    AssistantMessageEventStream {
        stream,
        started_at: crate::utils::uuid::now_ms() as u64,
        started_at_monotonic: std::time::Instant::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StopReason, TerminalStopReason};

    fn message(timestamp: u64) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            usage: crate::default_usage(),
            stop_reason: StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp,
            duration_ms: None,
        }
    }

    #[tokio::test]
    async fn times_the_final_done_message() {
        let stream = create_assistant_message_event_stream();
        let now = crate::utils::uuid::now_ms() as u64;
        stream.push(AssistantMessageEvent::Done {
            reason: TerminalStopReason::Stop,
            message: message(now),
        });
        let result = stream.result().await;
        assert!(result.duration_ms.is_some(), "最终消息应带 durationMs");
    }

    #[tokio::test]
    async fn does_not_time_a_forwarded_response() {
        let stream = create_assistant_message_event_stream();
        // 早于流开始的 timestamp 表示响应在别处已开始，不计时。
        stream.push(AssistantMessageEvent::Done {
            reason: TerminalStopReason::Stop,
            message: message(0),
        });
        let result = stream.result().await;
        assert_eq!(result.duration_ms, None);
    }
}
