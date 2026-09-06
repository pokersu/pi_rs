//! Rust 翻译自 packages/agent/src/harness/utils/adaptive-publisher.ts
//!
//! 自适应发布器：发布最新状态而不排队中间突变。空闲后的首个 dirty 状态立即发布，
//! 每次发布按其编码大小换取成比例的延迟，并保证最终发布（尾随 timer）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

/// 对应 `AdaptivePublisherOptions` 的回调集合（trait 化）。
pub trait AdaptivePublisherSink<TValue, TUpdate>: Send + Sync {
    fn snapshot(&self) -> TValue;
    fn update(&self, previous: Option<&TValue>, current: &TValue) -> Option<TUpdate>;
    fn measure(&self, update: &TUpdate) -> usize;
    fn publish(&self, update: TUpdate);
    fn on_error(&self, error: String);
}

struct Inner<TValue, TUpdate> {
    sink: Arc<dyn AdaptivePublisherSink<TValue, TUpdate>>,
    min_interval_ms: u64,
    target_bytes_per_second: u64,
    published: Option<TValue>,
    dirty: bool,
    next_emit_at: Instant,
    timer: Option<JoinHandle<()>>,
    disposed: bool,
}

/// 对应 `AdaptivePublisher`。
pub struct AdaptivePublisher<TValue, TUpdate> {
    inner: Arc<Mutex<Inner<TValue, TUpdate>>>,
}

impl<TValue, TUpdate> Clone for AdaptivePublisher<TValue, TUpdate> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct PreparedUpdate<TValue, TUpdate> {
    sink: Arc<dyn AdaptivePublisherSink<TValue, TUpdate>>,
    update: TUpdate,
}

impl<TValue: Clone, TUpdate: Clone> AdaptivePublisher<TValue, TUpdate>
where
    TValue: Send + Sync + 'static,
    TUpdate: Send + Sync + 'static,
{
    pub fn new(
        sink: Arc<dyn AdaptivePublisherSink<TValue, TUpdate>>,
        min_interval_ms: u64,
        target_bytes_per_second: u64,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                sink,
                min_interval_ms,
                target_bytes_per_second,
                published: None,
                dirty: false,
                next_emit_at: Instant::now(),
                timer: None,
                disposed: false,
            })),
        }
    }

    pub fn mark_dirty(&self) {
        let flush_now = {
            let mut inner = self.inner.lock().unwrap();
            if inner.disposed {
                return;
            }
            inner.dirty = true;
            let now = Instant::now();
            if now >= inner.next_emit_at {
                true
            } else {
                let wait = inner.next_emit_at - now;
                drop(inner);
                self.arm_timer(wait);
                false
            }
        };
        if flush_now {
            self.flush(true);
        }
    }

    pub fn flush(&self, force: bool) {
        let prepared = {
            let mut inner = self.inner.lock().unwrap();
            if inner.disposed || !inner.dirty {
                return;
            }
            let now = Instant::now();
            if !force && now < inner.next_emit_at {
                let wait = inner.next_emit_at - now;
                drop(inner);
                self.arm_timer(wait);
                return;
            }
            Self::flush_locked(&mut inner, force)
        };
        // publish 在锁外调用，避免消费者重入导致死锁。
        if let Some(prepared) = prepared {
            prepared.sink.publish(prepared.update);
        }
    }

    pub fn dispose(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(timer) = inner.timer.take() {
            timer.abort();
        }
        inner.disposed = true;
    }

    fn arm_timer(&self, wait: Duration) {
        {
            let inner = self.inner.lock().unwrap();
            if inner.timer.is_some() || inner.disposed {
                return;
            }
        }
        let inner_arc = Arc::clone(&self.inner);
        let timer = tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let prepared = {
                let mut inner = inner_arc.lock().unwrap();
                inner.timer = None;
                Self::flush_locked(&mut inner, true)
            };
            if let Some(prepared) = prepared {
                prepared.sink.publish(prepared.update);
            }
        });
        self.inner.lock().unwrap().timer = Some(timer);
    }

    fn flush_locked(
        inner: &mut Inner<TValue, TUpdate>,
        force: bool,
    ) -> Option<PreparedUpdate<TValue, TUpdate>> {
        let now = Instant::now();
        if !force && now < inner.next_emit_at {
            return None;
        }
        if let Some(timer) = inner.timer.take() {
            timer.abort();
        }
        let current = inner.sink.snapshot();
        let Some(update) = inner.sink.update(inner.published.as_ref(), &current) else {
            inner.published = Some(current);
            inner.dirty = false;
            return None;
        };
        let encoded_bytes = inner.sink.measure(&update);
        inner.published = Some(current);
        inner.dirty = false;
        inner.next_emit_at = now
            + Duration::from_millis(
                inner
                    .min_interval_ms
                    .max((encoded_bytes as u64 * 1000) / inner.target_bytes_per_second),
            );
        Some(PreparedUpdate {
            sink: Arc::clone(&inner.sink),
            update,
        })
    }
}
