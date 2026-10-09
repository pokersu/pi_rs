//! 对应 chord `src/services/state.ts` 的响应式状态（`ReplicatedState` 族）。
//!
//! 提供「一个不可变修订 + 若干订阅者」的发布订阅：订阅按序收到 `hydrate`/`update` 投递。
//!
//! # 与上游的差异
//!
//! 1. **同步/异步回调统一为异步**。上游监听器可以是同步函数或返回 Promise；Rust 统一为返回
//!    `Future` 的闭包 —— 同步逻辑用 `Box::pin(async { … })` 包裹即可。
//! 2. **unsubscribe 走 RAII**。上游 `subscribe` 返回 `() => void` 需显式调用；这里返回
//!    [`Subscription`]，`Drop` 即取消（同时保留幂等的 [`Subscription::unsubscribe`]）。
//! 3. **`MutableReplicatedState`（`replicatedState(initial)`）**：`change` 的
//!    `Draft` 用 [`Change`] 显式编辑 API 表达（`set`/`delete`/`append`/`truncate`/`splice`/`move_items`），
//!    与上游 Proxy 式 `Draft` 语义等价。
//! 4. **`state-codec`（`ServiceStateEncoder`/`ServiceStateDecoder`）**：见同模块 [`crate::chord::state_codec`]，
//!    依赖 delta 的 `Encoder`/`Decoder`（跨帧路径 interning 压缩）与 `Service*`/`Wire*` 订阅线类型。
//!
//! 投递队列语义与上游一致：单个订阅者的回调**串行**执行；最多 100 条待投递，溢出时清空队列
//! （若尚未开始投递则保留首条 hydration），因此更新序列可能跳号；回调失败被隔离上报，投递继续。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::Value as JsonValue;

use super::context::{BACKGROUND_CONTEXT, Context};
use super::delta::{DeltaError, Op, apply_immutable};
use super::tracker::{Change, Tracker};

/// 单个订阅者最多排队多少条投递（对应上游的 `100`）。
pub const MAX_PENDING_DELIVERIES: usize = 100;

/// 对应 `ReplicatedStateDelivery`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicatedStateDelivery {
    /// 对应 `kind`。
    pub kind: DeliveryKind,
    /// 对应 `sequence`。
    pub sequence: u64,
}

/// 对应 `kind: "hydrate" | "update"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    /// 订阅建立时的首次投递。
    Hydrate,
    /// 后续修订。
    Update,
}

/// 对应 `StateListener`：`(value, context, delivery) -> void | Promise<void>`。
pub type StateListener = Arc<
    dyn Fn(Arc<JsonValue>, Arc<dyn Context>, ReplicatedStateDelivery) -> BoxFuture<'static, ()>
        + Send
        + Sync,
>;

/// 错误上报回调（对应 `onError`）。
pub type StateErrorReporter = Arc<dyn Fn(String) + Send + Sync>;

fn default_reporter() -> StateErrorReporter {
    Arc::new(|message| eprintln!("[chord:replicated-state] {message}"))
}

/// 对应 `subscribe` 返回的取消函数。丢弃即取消（上游需显式调用）。
pub struct Subscription {
    cancel: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Subscription {
    /// 显式取消（幂等）。
    pub fn unsubscribe(mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

/// 对应 `ReplicatedState<T>`。
pub trait ReplicatedState: Send + Sync {
    /// 对应 `value`（`None` 表示尚未 hydration）。
    fn value(&self) -> Option<Arc<JsonValue>>;

    /// 对应 `subscribe`。
    fn subscribe(&self, listener: StateListener) -> Subscription;
}

// ─── 投递与订阅者 ────────────────────────────────────────────────────────────

struct Delivery {
    value: Arc<JsonValue>,
    context: Arc<dyn Context>,
    delivery: ReplicatedStateDelivery,
}

struct SubscriberState {
    pending: VecDeque<Delivery>,
    started: bool,
    closed: bool,
}

/// 对应 `StateSubscriber`：一个订阅者独立的投递队列与串行执行。
pub(crate) struct StateSubscriber {
    listener: StateListener,
    report_error: StateErrorReporter,
    state: Mutex<SubscriberState>,
}

impl StateSubscriber {
    fn new(listener: StateListener, report_error: StateErrorReporter) -> Self {
        Self {
            listener,
            report_error,
            state: Mutex::new(SubscriberState {
                pending: VecDeque::new(),
                started: false,
                closed: false,
            }),
        }
    }

    /// 对应 `push`：入队；队列满时按上游语义丢弃（保留 hydration）。
    fn push(&self, delivery: Delivery) {
        let mut state = self.state.lock().expect("subscriber state");
        if state.closed {
            return;
        }
        if state.pending.len() == MAX_PENDING_DELIVERIES {
            // 尚未开始投递时（冷副本可能在自己 hydration 前重入），保留首条 hydration。
            let hydration = if state.started {
                None
            } else {
                state.pending.front().map(|first| Delivery {
                    value: Arc::clone(&first.value),
                    context: Arc::clone(&first.context),
                    delivery: first.delivery,
                })
            };
            state.pending.clear();
            if let Some(hydration) = hydration {
                state.pending.push_back(hydration);
            }
        }
        state.pending.push_back(delivery);
    }

    /// 对应 `drain`：串行执行队列；每个回调单独隔离失败。
    ///
    /// 同时只有一个 drain 在跑（`self_weak` 由调用方以同样的 `Arc` 保证）。
    pub(crate) async fn drain(self: &Arc<Self>) {
        loop {
            let frame = {
                let mut state = self.state.lock().expect("subscriber state");
                if state.closed {
                    return;
                }
                match state.pending.pop_front() {
                    Some(frame) => {
                        state.started = true;
                        frame
                    }
                    None => return,
                }
            };

            let listener = Arc::clone(&self.listener);
            let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async {
                listener(frame.value, frame.context, frame.delivery).await;
            }))
            .await;
            if let Err(payload) = outcome {
                (self.report_error)(panic_message(&payload));
            }
        }
    }

    /// 对应 `close`：标记关闭并丢弃待投递内容。
    fn close(&self) {
        let mut state = self.state.lock().expect("subscriber state");
        state.closed = true;
        state.pending.clear();
    }

    /// 对应 `clear`：丢弃待投递内容但保持订阅打开。
    fn clear(&self) {
        self.state.lock().expect("subscriber state").pending.clear();
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "listener panicked".to_string())
}

/// 对应 `serviceDeliveryContext()`。
pub fn service_delivery_context() -> Arc<dyn Context> {
    Arc::clone(&BACKGROUND_CONTEXT)
}

// ─── 发布者 ──────────────────────────────────────────────────────────────────

/// 对应 `SourceListener`：`(ops, sequence, context)`。
pub type SourceListener = Arc<dyn Fn(Vec<Op>, u64, Arc<dyn Context>) + Send + Sync>;

type SubscriberList = Arc<Mutex<Vec<(Arc<StateSubscriber>, u64)>>>;
type SourceListenerList = Arc<Mutex<Vec<SourceListener>>>;

struct Publication {
    value: Arc<JsonValue>,
    ops: Vec<Op>,
    sequence: u64,
    context: Arc<dyn Context>,
}

/// 对应 `ReplicatedStatePublisher`：维护本地发布顺序。
pub struct ReplicatedStatePublisher {
    listeners: SubscriberList,
    source_listeners: SourceListenerList,
    publications: Mutex<VecDeque<Publication>>,
    value: Mutex<Arc<JsonValue>>,
    sequence: Mutex<u64>,
    delivering: Mutex<bool>,
    report_error: StateErrorReporter,
}

impl ReplicatedStatePublisher {
    /// 对应 `constructor(initial, reportError?)`。
    pub fn new(initial: JsonValue, report_error: Option<StateErrorReporter>) -> Self {
        Self {
            listeners: Arc::new(Mutex::new(Vec::new())),
            source_listeners: Arc::new(Mutex::new(Vec::new())),
            publications: Mutex::new(VecDeque::new()),
            value: Mutex::new(Arc::new(initial)),
            sequence: Mutex::new(0),
            delivering: Mutex::new(false),
            report_error: report_error.unwrap_or_else(default_reporter),
        }
    }

    /// 对应 `value`。
    pub fn value(&self) -> Arc<JsonValue> {
        Arc::clone(&self.value.lock().expect("publisher value"))
    }

    /// 对应 `snapshot()`：`(value, sequence)`。
    pub fn snapshot(&self) -> (Arc<JsonValue>, u64) {
        (
            self.value(),
            *self.sequence.lock().expect("publisher sequence"),
        )
    }

    /// 对应 `subscribe`：立即投递一次 `hydrate`。
    pub fn subscribe(&self, listener: StateListener) -> Subscription {
        let (value, sequence) = self.snapshot();
        let subscriber = Arc::new(StateSubscriber::new(
            listener,
            Arc::clone(&self.report_error),
        ));
        self.listeners
            .lock()
            .expect("publisher listeners")
            .push((Arc::clone(&subscriber), sequence));
        subscriber.push(Delivery {
            value,
            context: service_delivery_context(),
            delivery: ReplicatedStateDelivery {
                kind: DeliveryKind::Hydrate,
                sequence,
            },
        });
        spawn_drain(Arc::clone(&subscriber));

        let listeners = Arc::clone(&self.listeners);
        let cancel: Box<dyn FnOnce() + Send + Sync> = Box::new(move || {
            subscriber.close();
            if let Ok(mut guard) = listeners.lock() {
                guard.retain(|(candidate, _)| !Arc::ptr_eq(candidate, &subscriber));
            }
        });
        Subscription {
            cancel: Some(cancel),
        }
    }

    /// 当前订阅者数量（诊断与测试用）。
    pub fn subscriber_count(&self) -> usize {
        self.listeners.lock().expect("publisher listeners").len()
    }

    /// 对应 `subscribeSource`。
    pub fn subscribe_source(&self, listener: SourceListener) -> Subscription {
        self.source_listeners
            .lock()
            .expect("source listeners")
            .push(Arc::clone(&listener));
        let source_listeners = Arc::clone(&self.source_listeners);
        let cancel: Box<dyn FnOnce() + Send + Sync> = Box::new(move || {
            if let Ok(mut guard) = source_listeners.lock() {
                guard.retain(|candidate| !Arc::ptr_eq(candidate, &listener));
            }
        });
        Subscription {
            cancel: Some(cancel),
        }
    }

    /// 对应 `publish`：记录一次修订并分发给订阅者。
    ///
    /// 上游此处返回各 source listener 的失败集合；Rust 侧失败经 `report_error` 上报，
    /// 因此不额外返回。重入的 `publish` 只入队，由外层循环继续排空。
    pub fn publish(&self, value: JsonValue, ops: Vec<Op>, context: Arc<dyn Context>) {
        {
            let mut current = self.value.lock().expect("publisher value");
            *current = Arc::new(value.clone());
        }
        let sequence = {
            let mut sequence = self.sequence.lock().expect("publisher sequence");
            *sequence += 1;
            *sequence
        };
        self.publications
            .lock()
            .expect("publications")
            .push_back(Publication {
                value: Arc::new(value),
                ops,
                sequence,
                context,
            });

        let mut delivering = self.delivering.lock().expect("delivering");
        if *delivering {
            return;
        }
        *delivering = true;

        loop {
            let publication = self.publications.lock().expect("publications").pop_front();
            let Some(publication) = publication else {
                break;
            };

            let source_listeners = self
                .source_listeners
                .lock()
                .expect("source listeners")
                .clone();
            for listener in source_listeners {
                listener(
                    publication.ops.clone(),
                    publication.sequence,
                    Arc::clone(&publication.context),
                );
            }

            let listeners = self.listeners.lock().expect("publisher listeners").clone();
            for (subscriber, hydrated_sequence) in listeners {
                if publication.sequence <= hydrated_sequence {
                    continue;
                }
                subscriber.push(Delivery {
                    value: Arc::clone(&publication.value),
                    context: Arc::clone(&publication.context),
                    delivery: ReplicatedStateDelivery {
                        kind: DeliveryKind::Update,
                        sequence: publication.sequence,
                    },
                });
                spawn_drain(subscriber);
            }
        }

        *delivering = false;
    }
}

fn spawn_drain(subscriber: Arc<StateSubscriber>) {
    // `drain` 的并发保护由 `SubscriberState` 的队列消费本身保证：
    // 多次 spawn 只会有序地消费各自取到的帧。这里 fire-and-forget 以对齐上游「不阻塞 publish」。
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move { subscriber.drain().await });
    }
}

// ─── 源契约 ──────────────────────────────────────────────────────────────────

/// 对应 `ReplicatedStateSourceFrame<T>`。
#[derive(Clone)]
pub struct ReplicatedStateSourceFrame {
    /// 对应 `cursor`：必须等于上一帧 + 1。
    pub cursor: u64,
    /// 该次提交产生的不可变值。
    pub value: JsonValue,
    /// 从上一修订到本修订的操作批次。
    pub ops: Vec<Op>,
    /// 提交时的上下文。
    pub context: Arc<dyn Context>,
}

/// 对应 `ReplicatedStateSourceAttachment<T>`。
pub trait ReplicatedStateSourceAttachment: Send + Sync {
    /// 对应 `snapshot`：`(value, cursor)`。
    fn snapshot(&self) -> (JsonValue, u64);

    /// 对应 `activate`：安装唯一监听者并排空已缓冲的帧（单次使用）。
    fn activate(&self, listener: Arc<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>);

    /// 对应 `dispose`（必须幂等）。
    fn dispose(&self);
}

/// 对应 `ReplicatedStateSource<T>`。
pub trait ReplicatedStateSource: Send + Sync {
    /// 对应 `attach`：原子地捕获快照并注册后续帧的缓冲。
    fn attach(&self) -> Box<dyn ReplicatedStateSourceAttachment>;
}

// ─── Attached 状态 ───────────────────────────────────────────────────────────

/// 对应 `AttachedReplicatedState<T>`：由一个源 attachment 支撑的只读发布状态。
pub struct AttachedReplicatedState {
    publisher: Arc<ReplicatedStatePublisher>,
    attachment: Arc<dyn ReplicatedStateSourceAttachment>,
    cursor: Arc<Mutex<u64>>,
    disposed: Arc<AtomicBool>,
    report_error: StateErrorReporter,
    /// 持有自省适配的强引用，保证注册表里的 `Weak` 可 `upgrade`。
    #[allow(dead_code)]
    internals: Arc<AttachedInternals>,
    internals_key: usize,
}

impl AttachedReplicatedState {
    /// 对应 `new AttachedReplicatedStateImpl(attachment, options)`（构造时不做 activate）。
    pub fn new(
        attachment: Box<dyn ReplicatedStateSourceAttachment>,
        on_error: Option<StateErrorReporter>,
    ) -> Self {
        let (value, cursor) = attachment.snapshot();
        let report_error = on_error.unwrap_or_else(default_reporter);
        let reporter = Arc::clone(&report_error);
        let publisher = Arc::new(ReplicatedStatePublisher::new(
            value,
            Some(Arc::new(move |message| reporter(message))),
        ));
        let internals_key = next_internals_key();
        let internals = Arc::new(AttachedInternals {
            publisher: Arc::clone(&publisher),
        });
        register_replicated_state_internals(internals_key, internals.clone());
        Self {
            publisher,
            attachment: Arc::from(attachment),
            cursor: Arc::new(Mutex::new(cursor)),
            disposed: Arc::new(AtomicBool::new(false)),
            report_error,
            internals,
            internals_key,
        }
    }

    /// 当前游标（测试与诊断用）。
    pub fn cursor(&self) -> u64 {
        *self.cursor.lock().expect("cursor")
    }

    /// 对应 `activate`：把源帧接到内部 publisher。
    pub fn activate(&self) {
        let publisher = Arc::clone(&self.publisher);
        let attachment = Arc::clone(&self.attachment);
        let cursor = Arc::clone(&self.cursor);
        let disposed = Arc::clone(&self.disposed);
        let report_error = Arc::clone(&self.report_error);

        let listener: Arc<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync> = Arc::new(
            move |frame: ReplicatedStateSourceFrame| {
                if disposed.load(Ordering::Relaxed) {
                    return;
                }
                let expected = *cursor.lock().expect("cursor") + 1;
                if frame.cursor != expected {
                    // 对应 `#fail`：标记已释放、释放 attachment（幂等），再上报。
                    disposed.store(true, Ordering::Relaxed);
                    attachment.dispose();
                    report_error(format!(
                        "Replicated state source cursor has a gap: expected {expected}, received {}",
                        frame.cursor
                    ));
                    return;
                }
                *cursor.lock().expect("cursor") = frame.cursor;
                publisher.publish(frame.value, frame.ops, frame.context);
            },
        );
        self.attachment.activate(listener);
    }

    /// 对应 `dispose`（幂等）。
    pub fn dispose(&self) {
        if self.disposed.swap(true, Ordering::Relaxed) {
            return;
        }
        self.attachment.dispose();
    }
}

impl ReplicatedState for AttachedReplicatedState {
    fn value(&self) -> Option<Arc<JsonValue>> {
        Some(self.publisher.value())
    }

    fn subscribe(&self, listener: StateListener) -> Subscription {
        self.publisher.subscribe(listener)
    }
}

impl Drop for AttachedReplicatedState {
    fn drop(&mut self) {
        self.dispose();
        unregister_replicated_state_internals(self.internals_key);
    }
}

/// 对应 `attachReplicatedStateSource` / `replicatedState(source)`。
///
/// 对应上游的 `attach()` → 构造 → `activate()` 流程。
pub fn attach_replicated_state_source(
    source: &dyn ReplicatedStateSource,
    on_error: Option<StateErrorReporter>,
) -> AttachedReplicatedState {
    let attachment = source.attach();
    let state = AttachedReplicatedState::new(attachment, on_error);
    state.activate();
    state
}

// ─── 可变状态（MutableReplicatedState） ────────────────────────────────────────

/// 对应 `MutableReplicatedState<T>`：本地可变更的响应式状态。
pub trait MutableReplicatedState: ReplicatedState {
    /// 对应 `change(context, mutate)`：原子提交一次同步变更。
    fn change(&self, context: Arc<dyn Context>, mutate: Box<dyn FnOnce(&mut Change) + Send>);
    /// 对应 `replace(context, value)`：原子替换整值。
    fn replace(&self, context: Arc<dyn Context>, value: JsonValue);
}

/// 对应 `MutableReplicatedStateImpl`。
pub struct MutableReplicatedStateImpl {
    tracker: Mutex<Tracker>,
    publisher: ReplicatedStatePublisher,
    changing: AtomicBool,
}

impl MutableReplicatedStateImpl {
    fn new(initial: JsonValue) -> Self {
        let tracker = Tracker::new(initial.clone());
        let publisher = ReplicatedStatePublisher::new(initial, None);
        Self {
            tracker: Mutex::new(tracker),
            publisher,
            changing: AtomicBool::new(false),
        }
    }

    /// 对应 `value` getter。
    pub fn value(&self) -> Arc<JsonValue> {
        Arc::new(self.tracker.lock().expect("tracker").value().clone())
    }
}

impl ReplicatedState for MutableReplicatedStateImpl {
    fn value(&self) -> Option<Arc<JsonValue>> {
        Some(self.value())
    }

    fn subscribe(&self, listener: StateListener) -> Subscription {
        self.publisher.subscribe(listener)
    }
}

impl MutableReplicatedState for MutableReplicatedStateImpl {
    fn change(&self, context: Arc<dyn Context>, mutate: Box<dyn FnOnce(&mut Change) + Send>) {
        if self.changing.swap(true, Ordering::SeqCst) {
            panic!("Replicated state cannot be changed reentrantly from a change callback");
        }
        let prepared = {
            let tracker = self.tracker.lock().expect("tracker");
            let mut change = tracker.begin_change();
            mutate(&mut change);
            change.prepare()
        };
        self.changing.store(false, Ordering::SeqCst);

        let ops_empty = prepared.ops().is_empty();
        let value = prepared.value().clone();
        let ops = prepared.ops().to_vec();
        self.tracker.lock().expect("tracker").adopt(prepared);
        if ops_empty {
            return;
        }
        self.publisher.publish(value, ops, context);
    }

    fn replace(&self, context: Arc<dyn Context>, value: JsonValue) {
        if self.changing.load(Ordering::SeqCst) {
            panic!("Replicated state cannot be replaced from a change callback");
        }
        let prepared = self.tracker.lock().expect("tracker").prepare_replace(value);
        let ops_empty = prepared.ops().is_empty();
        let value = prepared.value().clone();
        let ops = prepared.ops().to_vec();
        self.tracker.lock().expect("tracker").adopt(prepared);
        if ops_empty {
            return;
        }
        self.publisher.publish(value, ops, context);
    }
}

impl ReplicatedStateInternals for MutableReplicatedStateImpl {
    fn snapshot(&self) -> (JsonValue, u64) {
        let (value, sequence) = self.publisher.snapshot();
        ((*value).clone(), sequence)
    }

    fn subscribe_source(&self, listener: SourceListener) -> Subscription {
        self.publisher.subscribe_source(listener)
    }
}

/// 对应 `replicatedState(initial)`。
pub fn replicated_state(initial: JsonValue) -> Arc<MutableReplicatedStateImpl> {
    let state = Arc::new(MutableReplicatedStateImpl::new(initial));
    let internals: Arc<dyn ReplicatedStateInternals> = state.clone();
    register_replicated_state_internals(Arc::as_ptr(&state) as usize, internals);
    state
}

// ─── 冷副本（ReplicatedStateReplica） ──────────────────────────────────────────

/// 对应 `ReplicatedStateReplica`：服务消费者在完整快照到达前的只读冷状态。
pub struct ReplicatedStateReplica {
    listeners: Arc<Mutex<Vec<Arc<StateSubscriber>>>>,
    value: Mutex<Option<Arc<JsonValue>>>,
    sequence: Mutex<Option<u64>>,
    report_error: StateErrorReporter,
}

impl ReplicatedStateReplica {
    /// 对应 `constructor(reportError)`。
    pub fn new(report_error: StateErrorReporter) -> Self {
        Self {
            listeners: Arc::new(Mutex::new(Vec::new())),
            value: Mutex::new(None),
            sequence: Mutex::new(None),
            report_error,
        }
    }

    /// 对应 `hydrate`：以基操作批建立首个快照。
    pub fn hydrate(
        &self,
        sequence: u64,
        ops: &[Op],
        context: Arc<dyn Context>,
    ) -> Result<(), DeltaError> {
        let next = match apply_immutable(None, ops) {
            Ok(value) => value,
            Err(error) => {
                self.clear();
                return Err(error);
            }
        };
        if !matches!(ops.first(), Some(Op::Replace(_))) {
            self.clear();
            return Err(DeltaError::InvalidOp);
        }
        *self.sequence.lock().expect("sequence") = Some(sequence);
        *self.value.lock().expect("value") = Some(Arc::new(next));
        self.deliver_all(context, DeliveryKind::Hydrate, sequence);
        Ok(())
    }

    /// 对应 `update`：应用一次增量更新。
    pub fn update(
        &self,
        sequence: u64,
        ops: &[Op],
        context: Arc<dyn Context>,
    ) -> Result<(), DeltaError> {
        let current = self.value.lock().expect("value").clone();
        let current_seq = *self.sequence.lock().expect("sequence");
        let (Some(current), Some(current_seq)) = (current, current_seq) else {
            return Err(DeltaError::InvalidOp);
        };
        if sequence != current_seq + 1 {
            self.clear();
            return Err(DeltaError::InvalidOp);
        }
        let next = match apply_immutable(Some((*current).clone()), ops) {
            Ok(value) => value,
            Err(error) => {
                self.clear();
                return Err(error);
            }
        };
        *self.sequence.lock().expect("sequence") = Some(sequence);
        *self.value.lock().expect("value") = Some(Arc::new(next));
        self.deliver_all(context, DeliveryKind::Update, sequence);
        Ok(())
    }

    /// 对应 `clear`。
    pub fn clear(&self) {
        *self.value.lock().expect("value") = None;
        *self.sequence.lock().expect("sequence") = None;
        for subscriber in self.listeners.lock().expect("listeners").iter() {
            subscriber.clear();
        }
    }

    fn deliver_all(&self, context: Arc<dyn Context>, kind: DeliveryKind, sequence: u64) {
        let value = self.value.lock().expect("value").clone();
        let Some(value) = value else {
            return;
        };
        let listeners = self.listeners.lock().expect("listeners").clone();
        for subscriber in &listeners {
            subscriber.push(Delivery {
                value: Arc::clone(&value),
                context: Arc::clone(&context),
                delivery: ReplicatedStateDelivery { kind, sequence },
            });
        }
        for subscriber in listeners {
            spawn_drain(subscriber);
        }
    }
}

impl ReplicatedState for ReplicatedStateReplica {
    fn value(&self) -> Option<Arc<JsonValue>> {
        self.value.lock().expect("value").clone()
    }

    fn subscribe(&self, listener: StateListener) -> Subscription {
        let subscriber = Arc::new(StateSubscriber::new(
            listener,
            Arc::clone(&self.report_error),
        ));
        self.listeners
            .lock()
            .expect("listeners")
            .push(Arc::clone(&subscriber));
        if let Some(value) = self.value.lock().expect("value").clone()
            && let Some(sequence) = *self.sequence.lock().expect("sequence")
        {
            subscriber.push(Delivery {
                value,
                context: service_delivery_context(),
                delivery: ReplicatedStateDelivery {
                    kind: DeliveryKind::Hydrate,
                    sequence,
                },
            });
            spawn_drain(Arc::clone(&subscriber));
        }
        let listeners = Arc::clone(&self.listeners);
        let cancel: Box<dyn FnOnce() + Send + Sync> = Box::new(move || {
            subscriber.close();
            if let Ok(mut guard) = listeners.lock() {
                guard.retain(|candidate| !Arc::ptr_eq(candidate, &subscriber));
            }
        });
        Subscription {
            cancel: Some(cancel),
        }
    }
}

// ─── 内部自省注册表（state-internals） ────────────────────────────────────────

/// 对应 `ReplicatedStateInternals`。
pub trait ReplicatedStateInternals: Send + Sync {
    /// 对应 `snapshot()`：`(value, sequence)`。
    fn snapshot(&self) -> (JsonValue, u64);
    /// 对应 `subscribe(listener)`（source 层）。
    fn subscribe_source(&self, listener: SourceListener) -> Subscription;
}

static INTERNALS: std::sync::OnceLock<
    Mutex<HashMap<usize, std::sync::Weak<dyn ReplicatedStateInternals>>>,
> = std::sync::OnceLock::new();

fn internals_registry()
-> &'static Mutex<HashMap<usize, std::sync::Weak<dyn ReplicatedStateInternals>>> {
    INTERNALS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 对应 `registerReplicatedStateInternals`（上游用 WeakMap，此处存弱引用）。
pub fn register_replicated_state_internals(
    key: usize,
    internals: Arc<dyn ReplicatedStateInternals>,
) {
    internals_registry()
        .lock()
        .expect("internals")
        .insert(key, Arc::downgrade(&internals));
}

/// 对应上游 WeakMap 的弱引用自动清理（Rust 侧可显式移除）。
pub fn unregister_replicated_state_internals(key: usize) {
    internals_registry().lock().expect("internals").remove(&key);
}

/// 对应 `getReplicatedStateInternals`。
pub fn get_replicated_state_internals(key: usize) -> Option<Arc<dyn ReplicatedStateInternals>> {
    internals_registry()
        .lock()
        .expect("internals")
        .get(&key)
        .and_then(|weak| weak.upgrade())
}

static NEXT_INTERNALS_ID: AtomicU64 = AtomicU64::new(1);

fn next_internals_key() -> usize {
    NEXT_INTERNALS_ID.fetch_add(1, Ordering::Relaxed) as usize
}

/// `AttachedReplicatedState` 的自省适配：与 `ReplicatedStateInternals` 解耦（值类型不直接进入注册表）。
struct AttachedInternals {
    publisher: Arc<ReplicatedStatePublisher>,
}

impl ReplicatedStateInternals for AttachedInternals {
    fn snapshot(&self) -> (JsonValue, u64) {
        let (value, sequence) = self.publisher.snapshot();
        ((*value).clone(), sequence)
    }

    fn subscribe_source(&self, listener: SourceListener) -> Subscription {
        self.publisher.subscribe_source(listener)
    }
}
