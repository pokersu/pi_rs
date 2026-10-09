//! 对应 `src/session/observation.ts`：会话到 chord 的已提交状态桥。
//!
//! 两个组件：
//!
//! - [`CommittedStateSource`]：对应 `CommittedStateSource`，把已提交化身（或会话视图）暴露为
//!   chord 的 `ReplicatedStateSource`；`null` 值使其退役。
//! - [`CommittedWatch`]：对应 `CommittedWatch`，对一个化身做串行、精确帧、有界待投递的观察。
//!
//! # 与上游的差异
//!
//! - 上游泛型 `T` 在 Rust 侧退化为 [`JsonValue`]（与 chord 的 `AttachedReplicatedState` 一致）。
//! - `queueMicrotask` → `tokio::spawn`（无运行时则同步执行）；`Promise` → `oneshot` +
//!   `futures::future::Shared`；`AbortSignal.addEventListener` → `cancelled()` + `select!`。

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use futures::future::Shared;
use futures::{FutureExt, channel::oneshot};
use serde_json::Value as JsonValue;

use crate::chord::context::{Context, without_abort_signal};
use crate::chord::delta::Op;
use crate::chord::state::{
    ReplicatedStateSource, ReplicatedStateSourceAttachment, ReplicatedStateSourceFrame,
};
use crate::types::{ClosedWatch, JsonObject, WatchEnd, WatchHandle, WatchListener, WatchReason};

/// 对应 `ObservedDocumentValue`：已提交的文档值；`null`（这里为 `None`）表示退役。
pub type ObservedDocumentValue = Option<JsonObject>;

/// 对应 `MAX_PENDING_WATCH_FRAMES`：一个不可用观察者之后保留的精确已提交帧上限。
const MAX_PENDING_WATCH_FRAMES: usize = 100;

/// 对应 `RETIREMENT_OPERATIONS`：退役化身的规范终止更新。
pub fn retirement_operations() -> Vec<Op> {
    vec![Op::Replace(JsonValue::Null)]
}

/// 已提交值 → chord 帧值（`None` 为 `null`）。
pub fn observed_to_json(value: &ObservedDocumentValue) -> JsonValue {
    match value {
        Some(object) => JsonValue::Object(object.clone()),
        None => JsonValue::Null,
    }
}

/// chord 帧值 → 已提交值。文档值始终是对象或 `null`；其他 JSON 形态只会来自上游契约的破坏。
pub fn json_to_observed(value: &JsonValue) -> ObservedDocumentValue {
    match value {
        JsonValue::Null => None,
        JsonValue::Object(object) => Some(object.clone()),
        _ => None,
    }
}

// ─── 状态源 ──────────────────────────────────────────────────────────────────

static NEXT_ATTACHMENT_ID: AtomicU64 = AtomicU64::new(1);

/// 对应 `activate` 的监听者签名。
type FrameListener = Arc<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>;

struct SourceState {
    attachments: Vec<Arc<SessionSourceAttachment>>,
    release: Option<Box<dyn FnOnce() + Send + Sync>>,
    value: Option<JsonValue>,
    cursor: u64,
    retired: bool,
    closed: bool,
}

/// 对应 `CommittedStateSource`：由一个已提交化身（或会话视图）独占持有的状态源。
pub struct CommittedStateSource {
    state: Arc<Mutex<SourceState>>,
}

impl CommittedStateSource {
    /// 对应 `new CommittedStateSource(value, release)`。
    pub fn new(value: JsonValue, release: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            state: Arc::new(Mutex::new(SourceState {
                attachments: Vec::new(),
                release: Some(Box::new(release)),
                value: Some(value),
                cursor: 0,
                retired: false,
                closed: false,
            })),
        }
    }

    /// 对应 `advance(value, ops, context)`：把一帧已提交变更广播给全部 attachment。
    pub fn advance(&self, value: JsonValue, ops: Vec<Op>, context: Arc<dyn Context>) {
        let (frame, attachments) = {
            let mut state = self.state.lock().expect("state source");
            if state.closed || state.retired {
                return;
            }
            let retired = value.is_null();
            state.value = Some(value.clone());
            state.cursor += 1;
            if retired {
                state.retired = true;
            }
            (
                ReplicatedStateSourceFrame {
                    cursor: state.cursor,
                    value,
                    ops,
                    context,
                },
                state.attachments.clone(),
            )
        };
        for attachment in attachments {
            attachment.publish(frame.clone());
        }
    }

    /// 对应 `closeSession()`：释放全部 attachment 并结束该源。
    pub fn close_session(&self) {
        let attachments = {
            let mut state = self.state.lock().expect("state source");
            if state.closed {
                return;
            }
            std::mem::take(&mut state.attachments)
        };
        for attachment in attachments {
            attachment.dispose();
        }
        finish_disposal(&self.state);
    }
}

impl ReplicatedStateSource for CommittedStateSource {
    fn attach(&self) -> Box<dyn ReplicatedStateSourceAttachment> {
        let attachment = {
            let mut state = self.state.lock().expect("state source");
            assert!(!state.closed, "State source is closed");
            let value = state.value.clone().expect("value present while open");
            let cursor = state.cursor;
            let id = NEXT_ATTACHMENT_ID.fetch_add(1, Ordering::Relaxed);
            let weak = Arc::downgrade(&self.state);
            let release = Box::new(move || release_attachment(&weak, id));
            let attachment = Arc::new(SessionSourceAttachment::new(id, value, cursor, release));
            state.attachments.push(Arc::clone(&attachment));
            attachment
        };
        Box::new(AttachmentHandle { attachment })
    }
}

/// attachment 的 release 回调：移除自身；若已无 attachment 则结束该源。
fn release_attachment(state: &Weak<Mutex<SourceState>>, id: u64) {
    let Some(state) = state.upgrade() else {
        return;
    };
    let empty = {
        let mut state = state.lock().expect("state source");
        state.attachments.retain(|attachment| attachment.id != id);
        state.attachments.is_empty()
    };
    if empty {
        finish_disposal(&state);
    }
}

/// 对应 `#finishDisposal()`：标记关闭、丢弃值、释放外部持有。
fn finish_disposal(state: &Arc<Mutex<SourceState>>) {
    let release = {
        let mut state = state.lock().expect("state source");
        if state.closed {
            return;
        }
        state.closed = true;
        state.value = None;
        state.release.take()
    };
    if let Some(release) = release {
        release();
    }
}

/// 对应 `SessionSourceAttachment`：一个 attachment 的有界缓冲与唯一监听者。
struct SessionSourceAttachment {
    id: u64,
    snapshot: (JsonValue, u64),
    release: Mutex<Option<Box<dyn FnOnce() + Send + Sync>>>,
    frames: Mutex<VecDeque<ReplicatedStateSourceFrame>>,
    listener: Mutex<Option<FrameListener>>,
    activated: AtomicBool,
    disposed: AtomicBool,
    scheduled: AtomicBool,
    delivering: AtomicBool,
}

impl SessionSourceAttachment {
    fn new(
        id: u64,
        value: JsonValue,
        cursor: u64,
        release: Box<dyn FnOnce() + Send + Sync>,
    ) -> Self {
        Self {
            id,
            snapshot: (value, cursor),
            release: Mutex::new(Some(release)),
            frames: Mutex::new(VecDeque::new()),
            listener: Mutex::new(None),
            activated: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            scheduled: AtomicBool::new(false),
            delivering: AtomicBool::new(false),
        }
    }

    /// 对应 `activate(listener)`：安装唯一监听者并同步排空已缓冲的帧。
    fn activate(&self, listener: FrameListener) {
        assert!(
            !self.activated.swap(true, Ordering::SeqCst),
            "State attachment is already active"
        );
        assert!(
            !self.disposed.load(Ordering::SeqCst),
            "State attachment is disposed"
        );
        *self.listener.lock().expect("listener") = Some(listener);
        self.drain();
    }

    /// 对应 `publish(frame)`：入队；已激活时调度一次排空。
    fn publish(self: &Arc<Self>, frame: ReplicatedStateSourceFrame) {
        if self.disposed.load(Ordering::SeqCst) {
            return;
        }
        self.frames.lock().expect("frames").push_back(frame);
        if !self.activated.load(Ordering::SeqCst) {
            return;
        }
        if self.delivering.load(Ordering::SeqCst) || self.scheduled.load(Ordering::SeqCst) {
            return;
        }
        if self.scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = Arc::clone(self);
        let fallback = Arc::clone(&this);
        let job = async move {
            this.scheduled.store(false, Ordering::SeqCst);
            if this.disposed.load(Ordering::SeqCst) {
                return;
            }
            this.drain();
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(job);
            }
            // 无 tokio 运行时（同步测试）时退化为同步排空：语义与微任务一致，
            // 只是不再把调用推迟到当前栈之后。
            Err(_) => {
                fallback.scheduled.store(false, Ordering::SeqCst);
                fallback.drain();
            }
        }
    }

    /// 对应 `dispose()`（幂等）。
    fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.frames.lock().expect("frames").clear();
        *self.listener.lock().expect("listener") = None;
        let release = self.release.lock().expect("release").take();
        if let Some(release) = release {
            release();
        }
    }

    /// 对应 `#drain()`：串行投递全部缓冲帧；监听者失败隔离到本 attachment。
    fn drain(&self) {
        let Some(listener) = self.listener.lock().expect("listener").clone() else {
            return;
        };
        if self.delivering.swap(true, Ordering::SeqCst) {
            return;
        }
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            while !self.disposed.load(Ordering::SeqCst) {
                let frame = self.frames.lock().expect("frames").pop_front();
                let Some(frame) = frame else {
                    break;
                };
                listener(frame);
            }
        }));
        self.delivering.store(false, Ordering::SeqCst);
        if result.is_err() {
            // chord 的帧监听者会兜住源契约失败；这里把意外的直接监听者失败隔离到本 attachment。
            self.dispose();
        }
    }
}

/// 对应 `attach()` 返回的 attachment 句柄。
struct AttachmentHandle {
    attachment: Arc<SessionSourceAttachment>,
}

impl ReplicatedStateSourceAttachment for AttachmentHandle {
    fn snapshot(&self) -> (JsonValue, u64) {
        self.attachment.snapshot.clone()
    }

    fn activate(&self, listener: FrameListener) {
        self.attachment.activate(listener);
    }

    fn dispose(&self) {
        self.attachment.dispose();
    }
}

// ─── 观察句柄 ────────────────────────────────────────────────────────────────

/// 对应 `WatchFrame<T>`：待投递的一帧。
struct WatchFrame {
    value: JsonValue,
    ops: Vec<Op>,
    context: Arc<dyn Context>,
}

struct WatchState {
    value: JsonValue,
    pending: VecDeque<WatchFrame>,
    listener: Option<WatchListener>,
    started: bool,
    scheduled: bool,
    running: bool,
    detached: bool,
    retired: bool,
    end: Option<WatchEnd>,
    resolved: bool,
    detach: Option<Box<dyn FnOnce() + Send + Sync>>,
    replace: Option<Arc<dyn Fn() -> JsonValue + Send + Sync>>,
    resolve_closed: Option<oneshot::Sender<WatchEnd>>,
}

/// 对应 `CommittedWatch`：绑定到一个文档化身或会话视图的串行精确帧观察。
pub struct CommittedWatch {
    state: Arc<Mutex<WatchState>>,
    closed: Shared<oneshot::Receiver<WatchEnd>>,
}

impl CommittedWatch {
    /// 对应 `new CommittedWatch(value, detach, replace?)`。
    pub fn new(
        value: JsonValue,
        detach: impl FnOnce() + Send + Sync + 'static,
        replace: Option<Arc<dyn Fn() -> JsonValue + Send + Sync>>,
    ) -> Self {
        let (sender, receiver) = oneshot::channel();
        Self {
            state: Arc::new(Mutex::new(WatchState {
                value,
                pending: VecDeque::new(),
                listener: None,
                started: false,
                scheduled: false,
                running: false,
                detached: false,
                retired: false,
                end: None,
                resolved: false,
                detach: Some(Box::new(detach)),
                replace,
                resolve_closed: Some(sender),
            })),
            closed: receiver.shared(),
        }
    }

    /// 对应 `advance(value, ops, context)`。
    pub fn advance(&self, value: JsonValue, ops: Vec<Op>, context: Arc<dyn Context>) {
        let schedule = {
            let mut state = self.state.lock().expect("watch");
            if state.end.is_some() || state.retired {
                return;
            }
            if value.is_null() {
                state.retired = true;
            }
            if state.pending.len() >= MAX_PENDING_WATCH_FRAMES {
                state.pending.clear();
                let replacement = state
                    .replace
                    .as_ref()
                    .map_or_else(|| value.clone(), |replace| replace());
                state.pending.push_back(WatchFrame {
                    ops: vec![Op::Replace(replacement.clone())],
                    value: replacement,
                    context: Arc::clone(&context),
                });
            } else {
                state.pending.push_back(WatchFrame {
                    value,
                    ops,
                    context,
                });
            }
            state.started && state.end.is_none()
        };
        if schedule {
            schedule_watch(Arc::clone(&self.state));
        }
    }

    /// 对应 `observeCancellation(signal)`。
    pub fn observe_cancellation(&self, signal: pi_ai::AbortSignal) {
        {
            let state = self.state.lock().expect("watch");
            assert!(
                state.end.is_none(),
                "Watch cancellation is already installed"
            );
        }
        if signal.aborted() {
            terminate_watch(&self.state, WatchEnd::Reason(WatchReason::Cancelled));
            return;
        }
        let Some(handle) = tokio::runtime::Handle::try_current().ok() else {
            return;
        };
        let state = Arc::clone(&self.state);
        let closed = self.closed.clone();
        handle.spawn(async move {
            tokio::select! {
                () = signal.cancelled() => {
                    terminate_watch(&state, WatchEnd::Reason(WatchReason::Cancelled));
                }
                _ = closed => {}
            }
        });
    }

    /// 对应 `cancel()`。
    pub fn cancel(&self) {
        terminate_watch(&self.state, WatchEnd::Reason(WatchReason::Cancelled));
    }

    /// 对应 `closeSession()`。
    pub fn close_session(&self) {
        terminate_watch(&self.state, WatchEnd::Reason(WatchReason::SessionClosed));
    }
}

impl WatchHandle for CommittedWatch {
    fn value(&self) -> JsonValue {
        self.state.lock().expect("watch").value.clone()
    }

    fn start(&self, listener: WatchListener) {
        let schedule = {
            let mut state = self.state.lock().expect("watch");
            assert!(!state.started, "Watch is already started");
            assert!(state.end.is_none(), "Watch is stopped");
            state.started = true;
            state.listener = Some(listener);
            !state.pending.is_empty()
        };
        if schedule {
            schedule_watch(Arc::clone(&self.state));
        }
    }

    fn stop(&self) -> ClosedWatch {
        terminate_watch(&self.state, WatchEnd::Reason(WatchReason::Stopped));
        self.closed()
    }

    fn closed(&self) -> ClosedWatch {
        self.closed.clone()
    }
}

/// 对应 `#schedule()`：确保同一时刻只有一次排空在跑。
fn schedule_watch(state: Arc<Mutex<WatchState>>) {
    {
        let mut state = state.lock().expect("watch");
        if state.scheduled || state.running || state.end.is_some() {
            return;
        }
        state.scheduled = true;
    }
    let job = async move {
        state.lock().expect("watch").scheduled = false;
        drain_watch(Arc::clone(&state)).await;
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(job);
        }
        Err(_) => {
            futures::executor::block_on(job);
        }
    }
}

/// 对应 `#drain()`：串行投递；监听者失败或值退役时终止。
async fn drain_watch(state: Arc<Mutex<WatchState>>) {
    {
        let mut guard = state.lock().expect("watch");
        if guard.running || guard.end.is_some() || !guard.started {
            drop(guard);
            finish_if_ready(&state);
            return;
        }
        guard.running = true;
    }
    loop {
        let (frame, listener) = {
            let mut guard = state.lock().expect("watch");
            if guard.end.is_some() {
                break;
            }
            let Some(frame) = guard.pending.pop_front() else {
                break;
            };
            guard.value = frame.value.clone();
            let listener = guard.listener.clone();
            (frame, listener)
        };
        let Some(listener) = listener else {
            break;
        };
        let delivery_context = without_abort_signal(Arc::clone(&frame.context));
        let future = listener(frame.value.clone(), frame.ops.clone(), delivery_context);
        if AssertUnwindSafe(future).catch_unwind().await.is_err() {
            terminate_watch(
                &state,
                WatchEnd::ListenerError("watch listener failed".to_string()),
            );
            break;
        }
        if frame.value.is_null() {
            terminate_watch(&state, WatchEnd::Reason(WatchReason::Retired));
            break;
        }
    }
    state.lock().expect("watch").running = false;
    finish_if_ready(&state);
}

/// 对应 `#terminate(end)`。
fn terminate_watch(state: &Arc<Mutex<WatchState>>, end: WatchEnd) {
    let detach = {
        let mut guard = state.lock().expect("watch");
        if guard.end.is_some() {
            return;
        }
        guard.end = Some(end);
        guard.pending.clear();
        if guard.detached {
            None
        } else {
            guard.detached = true;
            guard.detach.take()
        }
    };
    if let Some(detach) = detach {
        detach();
    }
    finish_if_ready(state);
}

/// 对应 `#finishIfReady()`。
fn finish_if_ready(state: &Arc<Mutex<WatchState>>) {
    let (end, sender) = {
        let mut guard = state.lock().expect("watch");
        if guard.resolved {
            return;
        }
        let Some(end) = guard.end.clone() else {
            return;
        };
        guard.resolved = true;
        (end, guard.resolve_closed.take())
    };
    if let Some(sender) = sender {
        let _ = sender.send(end);
    }
}
