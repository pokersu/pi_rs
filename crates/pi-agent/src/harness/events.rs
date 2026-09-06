//! Rust 翻译自 packages/agent/src/harness/events.ts

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use futures::FutureExt;
use futures::future::BoxFuture;

use crate::harness::agent_harness::{HarnessEventListener, WatchHandle};
use crate::harness::context::Context;
use crate::harness::harness_event::{HandlerErrorKind, HarnessEvent};

/// 对应 `ResnapshotCapture<T>`：`(context, markBoundary) => Promise<T>`。
pub type ResnapshotCapture<T> =
    Arc<dyn Fn(Context, Arc<dyn Fn() + Send + Sync>) -> BoxFuture<'static, T> + Send + Sync>;

/// 对应 lane.ts 的 `WatchHandler`：安装一个 lane 快照 watcher。
pub type WatchHandler<T> = Arc<
    dyn Fn(
            Option<T>,
            Arc<dyn Fn(&HarnessEvent) -> bool + Send + Sync>,
            Context,
            Option<ResnapshotCapture<T>>,
        ) -> Arc<BufferedEventWatcher<T>>
        + Send
        + Sync,
>;

/// 对应 `installWatcher` 内部的 `capture`：`(context) => Promise<T>`。
type ResnapshotCallback<T> = Arc<dyn Fn(Context) -> BoxFuture<'static, T> + Send + Sync>;

type OnErrorHandler =
    Arc<dyn Fn(String, HarnessEvent, Context) -> BoxFuture<'static, ()> + Send + Sync>;

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "event handler panicked".to_string()
    }
}

struct EventBusState {
    listeners: Mutex<HashMap<&'static str, Vec<HarnessEventListener>>>,
    watch_listeners: Mutex<Vec<HarnessEventListener>>,
    delivery_tail: Arc<tokio::sync::Mutex<()>>,
    closed_error: Mutex<Option<String>>,
}

impl EventBusState {
    fn snapshot_recipients(&self, event: &HarnessEvent) -> Vec<HarnessEventListener> {
        let mut recipients = Vec::new();
        if let Some(list) = self.listeners.lock().unwrap().get(event.event_type()) {
            recipients.extend(list.iter().cloned());
        }
        recipients.extend(self.watch_listeners.lock().unwrap().iter().cloned());
        recipients
    }

    async fn deliver(
        &self,
        event: &HarnessEvent,
        recipients: &[HarnessEventListener],
        report_errors: bool,
        context: &Context,
    ) {
        for listener in recipients {
            let future = listener(event.clone(), context.clone());
            let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
            if let Err(payload) = result {
                if !report_errors || event.event_type() == "handler_error" {
                    continue;
                }
                let handler_error = HarnessEvent::HandlerError {
                    error: panic_message(payload),
                    stack: None,
                    kind: HandlerErrorKind::Event {
                        event: event.event_type().to_string(),
                    },
                    lane: event.lane().map(|s| s.to_string()),
                    recovery: None,
                };
                let handler_recipients = self.snapshot_recipients(&handler_error);
                for handler_listener in &handler_recipients {
                    let future = handler_listener(handler_error.clone(), context.clone());
                    let _ = std::panic::AssertUnwindSafe(future).catch_unwind().await;
                }
            }
        }
    }
}

/// 对应 `HarnessEventBus`。
pub struct HarnessEventBus {
    state: Arc<EventBusState>,
}

impl Default for HarnessEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessEventBus {
    pub fn new() -> Self {
        Self {
            state: Arc::new(EventBusState {
                listeners: Mutex::new(HashMap::new()),
                watch_listeners: Mutex::new(Vec::new()),
                delivery_tail: Arc::new(tokio::sync::Mutex::new(())),
                closed_error: Mutex::new(None),
            }),
        }
    }

    /// 对应 `on`
    pub fn on(&self, event_type: &'static str, listener: HarnessEventListener) {
        let mut listeners = self.state.listeners.lock().unwrap();
        listeners.entry(event_type).or_default().push(listener);
    }

    /// 对应 `emit`
    pub fn emit(&self, event: HarnessEvent, context: Context) -> BoxFuture<'static, ()> {
        self.emit_batch(vec![event], context)
    }

    /// 对应 `emitBatch`
    pub fn emit_batch(
        &self,
        events: Vec<HarnessEvent>,
        context: Context,
    ) -> BoxFuture<'static, ()> {
        if self.state.closed_error.lock().unwrap().is_some() || events.is_empty() {
            return Box::pin(async {});
        }
        let bound: Vec<_> = events
            .into_iter()
            .map(|event| {
                let recipients = self.state.snapshot_recipients(&event);
                (event, recipients)
            })
            .collect();
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let _guard = state.delivery_tail.lock().await;
            for (event, recipients) in bound {
                state.deliver(&event, &recipients, true, &context).await;
            }
        })
    }

    /// 对应 `watch`
    pub fn watch<T>(
        &self,
        snapshot: Option<T>,
        filter: Arc<dyn Fn(&HarnessEvent) -> bool + Send + Sync>,
        _context: Context,
        resnapshot: Option<ResnapshotCapture<T>>,
    ) -> Arc<BufferedEventWatcher<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        if self.state.closed_error.lock().unwrap().is_some() {
            panic!("HarnessEventBus is closed");
        }
        self.install_watcher(snapshot, filter, resnapshot)
    }

    /// 对应 `close`
    pub fn close(&self, error: String) {
        *self.state.closed_error.lock().unwrap() = Some(error);
        self.state.listeners.lock().unwrap().clear();
        self.state.watch_listeners.lock().unwrap().clear();
    }

    fn install_watcher<T>(
        &self,
        snapshot: Option<T>,
        filter: Arc<dyn Fn(&HarnessEvent) -> bool + Send + Sync>,
        resnapshot: Option<ResnapshotCapture<T>>,
    ) -> Arc<BufferedEventWatcher<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        let state = Arc::clone(&self.state);

        let error_state = Arc::clone(&state);
        let on_error: OnErrorHandler = Arc::new(move |message, event, ctx| {
            if event.event_type() == "handler_error" {
                return Box::pin(async {});
            }
            let state = Arc::clone(&error_state);
            Box::pin(async move {
                let handler_error = HarnessEvent::HandlerError {
                    error: message,
                    stack: None,
                    kind: HandlerErrorKind::Event {
                        event: event.event_type().to_string(),
                    },
                    lane: event.lane().map(|s| s.to_string()),
                    recovery: None,
                };
                let recipients = state.snapshot_recipients(&handler_error);
                state
                    .deliver(&handler_error, &recipients, false, &ctx)
                    .await;
            })
        });

        let watcher = Arc::new_cyclic(|weak| BufferedEventWatcher {
            snapshot: Arc::new(Mutex::new(snapshot)),
            resnapshot_callback: Arc::new(Mutex::new(None)),
            on_error,
            buffer: Mutex::new(Vec::new()),
            listener: Arc::new(Mutex::new(None)),
            unsubscribe_callback: Mutex::new(None),
            delivery_tail: Arc::new(tokio::sync::Mutex::new(())),
            epoch: Arc::new(Mutex::new(0)),
            state: Arc::new(Mutex::new(WatcherState::Buffering)),
            resnapshot_state: Arc::new(Mutex::new(None)),
            self_weak: Arc::new(Mutex::new(Some(weak.clone()))),
        });

        if let Some(user_resnapshot) = resnapshot {
            let watcher_weak = Arc::downgrade(&watcher);
            let delivery_tail = Arc::clone(&state.delivery_tail);
            let capture: ResnapshotCallback<T> = Arc::new(move |context| {
                let user_resnapshot = Arc::clone(&user_resnapshot);
                let watcher_weak = watcher_weak.clone();
                let delivery_tail = Arc::clone(&delivery_tail);
                Box::pin(async move {
                    let marked = Arc::new(Mutex::new(false));
                    let marked_inner = Arc::clone(&marked);
                    let watcher_weak_inner = watcher_weak.clone();
                    let delivery_tail_inner = Arc::clone(&delivery_tail);
                    let mark_boundary: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                        let mut marked = marked_inner.lock().unwrap();
                        if *marked {
                            panic!("Resnapshot boundary was already marked");
                        }
                        *marked = true;
                        let watcher_weak = watcher_weak_inner.clone();
                        let tail = Arc::clone(&delivery_tail_inner);
                        tokio::spawn(async move {
                            let _guard = tail.lock().await;
                            if let Some(watcher) = watcher_weak.upgrade() {
                                watcher.mark_resnapshot_boundary();
                            }
                        });
                    });
                    let next = user_resnapshot(context, mark_boundary).await;
                    if !*marked.lock().unwrap() {
                        panic!("Resnapshot capture did not mark its boundary");
                    }
                    next
                })
            });
            *watcher.resnapshot_callback.lock().unwrap() = Some(capture);
        }

        let watcher_for_listener = Arc::clone(&watcher);
        let filter_for_listener = Arc::clone(&filter);
        let watch_listener: HarnessEventListener = Arc::new(move |event, ctx| {
            let watcher = Arc::clone(&watcher_for_listener);
            let filter = Arc::clone(&filter_for_listener);
            Box::pin(async move {
                if filter(&event) {
                    watcher.push(event, ctx);
                }
            })
        });
        state
            .watch_listeners
            .lock()
            .unwrap()
            .push(watch_listener.clone());
        let unsub_state = Arc::clone(&state);
        let unsub: Box<dyn Fn() + Send + Sync> = Box::new(move || {
            unsub_state
                .watch_listeners
                .lock()
                .unwrap()
                .retain(|l| !Arc::ptr_eq(l, &watch_listener));
        });
        *watcher.unsubscribe_callback.lock().unwrap() = Some(unsub);

        watcher
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatcherState {
    Buffering,
    Started,
    Unsubscribed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResnapshotPhase {
    Dropping,
    Holding,
}

struct ResnapshotState {
    phase: Mutex<ResnapshotPhase>,
    held: Mutex<Vec<(HarnessEvent, Context)>>,
    reached: Arc<tokio::sync::Notify>,
}

/// 对应 `BufferedEventWatcher<T>`。
pub struct BufferedEventWatcher<T> {
    snapshot: Arc<Mutex<Option<T>>>,
    resnapshot_callback: Arc<Mutex<Option<ResnapshotCallback<T>>>>,
    on_error: OnErrorHandler,
    buffer: Mutex<Vec<(HarnessEvent, Context, u64)>>,
    listener: Arc<Mutex<Option<HarnessEventListener>>>,
    unsubscribe_callback: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    delivery_tail: Arc<tokio::sync::Mutex<()>>,
    epoch: Arc<Mutex<u64>>,
    state: Arc<Mutex<WatcherState>>,
    resnapshot_state: Arc<Mutex<Option<Arc<ResnapshotState>>>>,
    self_weak: Arc<Mutex<Option<Weak<Self>>>>,
}

impl<T: Clone + Send + Sync + 'static> BufferedEventWatcher<T> {
    /// 对应 `setSnapshot`
    pub fn set_snapshot(&self, snapshot: T) {
        *self.snapshot.lock().unwrap() = Some(snapshot);
    }

    /// 对应 `markResnapshotBoundary`
    pub fn mark_resnapshot_boundary(&self) {
        let guard = self.resnapshot_state.lock().unwrap();
        let Some(resnapshot) = guard.as_ref() else {
            return;
        };
        let mut phase = resnapshot.phase.lock().unwrap();
        if *phase != ResnapshotPhase::Dropping {
            return;
        }
        *phase = ResnapshotPhase::Holding;
        drop(phase);
        resnapshot.reached.notify_waiters();
    }

    /// 对应 `push`
    pub fn push(&self, event: HarnessEvent, context: Context) {
        if *self.state.lock().unwrap() == WatcherState::Unsubscribed {
            return;
        }
        {
            let guard = self.resnapshot_state.lock().unwrap();
            if let Some(resnapshot) = guard.as_ref() {
                let phase = resnapshot.phase.lock().unwrap();
                match *phase {
                    ResnapshotPhase::Dropping => return,
                    ResnapshotPhase::Holding => {
                        drop(phase);
                        resnapshot.held.lock().unwrap().push((event, context));
                        return;
                    }
                }
            }
        }
        if *self.state.lock().unwrap() == WatcherState::Buffering {
            let epoch = *self.epoch.lock().unwrap();
            self.buffer.lock().unwrap().push((event, context, epoch));
            return;
        }
        let epoch = *self.epoch.lock().unwrap();
        self.enqueue(event, context, epoch);
    }

    fn enqueue(&self, event: HarnessEvent, context: Context, epoch: u64) {
        let listener = self.listener.lock().unwrap().clone();
        let Some(listener) = listener else {
            return;
        };
        let tail = Arc::clone(&self.delivery_tail);
        let state = Arc::clone(&self.state);
        let current_epoch = Arc::clone(&self.epoch);
        let on_error = Arc::clone(&self.on_error);
        tokio::spawn(async move {
            let _guard = tail.lock().await;
            if *state.lock().unwrap() == WatcherState::Started
                && epoch == *current_epoch.lock().unwrap()
            {
                let future = listener(event.clone(), context.clone());
                let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
                if let Err(payload) = result {
                    let message = panic_message(payload);
                    (on_error)(message, event, context).await;
                }
            }
        });
    }
}

impl<T: Clone + Send + Sync + 'static> WatchHandle<T> for BufferedEventWatcher<T> {
    fn snapshot(&self) -> T {
        self.snapshot
            .lock()
            .unwrap()
            .clone()
            .expect("WatchHandle snapshot is not set")
    }

    fn start(&self, listener: HarnessEventListener) {
        let mut state = self.state.lock().unwrap();
        if *state != WatcherState::Buffering {
            panic!("WatchHandle.start() may be called only once");
        }
        *state = WatcherState::Started;
        drop(state);
        *self.listener.lock().unwrap() = Some(listener);
        let buffered = std::mem::take(&mut *self.buffer.lock().unwrap());
        for (event, context, epoch) in buffered {
            self.enqueue(event, context, epoch);
        }
    }

    fn resnapshot<'a>(
        &'a self,
        context: &'a Context,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send + 'a>> {
        let context = context.clone();
        let snapshot = Arc::clone(&self.snapshot);
        let resnapshot_callback = Arc::clone(&self.resnapshot_callback);
        let resnapshot_state = Arc::clone(&self.resnapshot_state);
        let epoch = Arc::clone(&self.epoch);
        let state = Arc::clone(&self.state);
        let self_weak = self.self_weak.lock().unwrap().clone();
        Box::pin(async move {
            if *state.lock().unwrap() == WatcherState::Unsubscribed {
                return Err("WatchHandle is unsubscribed".to_string());
            }
            let callback = resnapshot_callback.lock().unwrap().clone();
            let Some(callback) = callback else {
                return Err("WatchHandle does not support resnapshot".to_string());
            };
            if resnapshot_state.lock().unwrap().is_some() {
                return Err("WatchHandle resnapshot is already in progress".to_string());
            }
            let reached = Arc::new(tokio::sync::Notify::new());
            let resnapshot = Arc::new(ResnapshotState {
                phase: Mutex::new(ResnapshotPhase::Dropping),
                held: Mutex::new(Vec::new()),
                reached: Arc::clone(&reached),
            });
            *epoch.lock().unwrap() += 1;
            *resnapshot_state.lock().unwrap() = Some(Arc::clone(&resnapshot));

            let result = std::panic::AssertUnwindSafe(callback(context.clone()))
                .catch_unwind()
                .await;

            match result {
                Ok(next) => {
                    reached.notified().await;
                    let held = std::mem::take(&mut *resnapshot.held.lock().unwrap());
                    *resnapshot_state.lock().unwrap() = None;
                    *snapshot.lock().unwrap() = Some(next);
                    for (event, ctx) in held {
                        if let Some(watcher) = self_weak.as_ref().and_then(|w| w.upgrade()) {
                            watcher.push(event, ctx);
                        }
                    }
                    snapshot
                        .lock()
                        .unwrap()
                        .clone()
                        .ok_or_else(|| "WatchHandle snapshot is not set".to_string())
                }
                Err(payload) => {
                    let held = std::mem::take(&mut *resnapshot.held.lock().unwrap());
                    *resnapshot_state.lock().unwrap() = None;
                    for (event, ctx) in held {
                        if let Some(watcher) = self_weak.as_ref().and_then(|w| w.upgrade()) {
                            watcher.push(event, ctx);
                        }
                    }
                    Err(panic_message(payload))
                }
            }
        })
    }

    fn unsubscribe(&self) {
        let mut state = self.state.lock().unwrap();
        if *state == WatcherState::Unsubscribed {
            return;
        }
        *state = WatcherState::Unsubscribed;
        drop(state);
        *self.buffer.lock().unwrap() = Vec::new();
        *self.listener.lock().unwrap() = None;
        if let Some(callback) = self.unsubscribe_callback.lock().unwrap().take() {
            callback();
        }
    }
}
