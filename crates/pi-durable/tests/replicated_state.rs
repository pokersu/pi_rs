//! `chord::state`（对应 chord `src/services/state.ts`）的集成测试。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_durable::chord::state::{
    AttachedReplicatedState, DeliveryKind, ReplicatedState, ReplicatedStateDelivery,
    ReplicatedStatePublisher, ReplicatedStateSource, ReplicatedStateSourceAttachment,
    ReplicatedStateSourceFrame, StateListener, attach_replicated_state_source,
    service_delivery_context,
};
use serde_json::{Value as JsonValue, json};

fn collector() -> (
    StateListener,
    tokio::sync::mpsc::UnboundedReceiver<(JsonValue, ReplicatedStateDelivery)>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let listener: StateListener = Arc::new(move |value, _context, delivery| {
        let tx = tx.clone();
        let value = (*value).clone();
        Box::pin(async move {
            let _ = tx.send((value, delivery));
        })
    });
    (listener, rx)
}

type FrameListener = Arc<dyn Fn(ReplicatedStateSourceFrame) + Send + Sync>;

struct FakeInner {
    value: JsonValue,
    listener: Mutex<Option<FrameListener>>,
    disposed: AtomicUsize,
}

impl FakeInner {
    fn new(value: JsonValue) -> Arc<Self> {
        Arc::new(Self {
            value,
            listener: Mutex::new(None),
            disposed: AtomicUsize::new(0),
        })
    }

    fn emit(&self, frame: ReplicatedStateSourceFrame) {
        let listener = self.listener.lock().unwrap().clone();
        if let Some(listener) = listener {
            listener(frame);
        }
    }
}

#[derive(Clone)]
struct FakeAttachment(Arc<FakeInner>);

impl ReplicatedStateSourceAttachment for FakeAttachment {
    fn snapshot(&self) -> (JsonValue, u64) {
        (self.0.value.clone(), 0)
    }

    fn activate(&self, listener: FrameListener) {
        *self.0.listener.lock().unwrap() = Some(listener);
    }

    fn dispose(&self) {
        self.0.disposed.fetch_add(1, Ordering::Relaxed);
    }
}

struct FakeSource(Arc<FakeInner>);

impl ReplicatedStateSource for FakeSource {
    fn attach(&self) -> Box<dyn ReplicatedStateSourceAttachment> {
        Box::new(FakeAttachment(Arc::clone(&self.0)))
    }
}

fn frame(cursor: u64, value: JsonValue) -> ReplicatedStateSourceFrame {
    ReplicatedStateSourceFrame {
        cursor,
        value,
        ops: Vec::new(),
        context: service_delivery_context(),
    }
}

#[tokio::test]
async fn delivers_hydration_then_updates() {
    let inner = FakeInner::new(json!({ "n": 0 }));
    let state = attach_replicated_state_source(&FakeSource(Arc::clone(&inner)), None);

    let (listener, mut rx) = collector();
    let _subscription = state.subscribe(listener);

    let (value, delivery) = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("hydration 应在超时前送达")
        .expect("channel open");
    assert_eq!(delivery.kind, DeliveryKind::Hydrate);
    assert_eq!(value, json!({ "n": 0 }));

    inner.emit(frame(1, json!({ "n": 1 })));

    let (value, delivery) = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("update 应在超时前送达")
        .expect("channel open");
    assert_eq!(delivery.kind, DeliveryKind::Update);
    assert_eq!(delivery.sequence, 1);
    assert_eq!(value, json!({ "n": 1 }));
}

#[tokio::test]
async fn cursor_gap_is_reported_and_disposes() {
    let inner = FakeInner::new(json!(null));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&errors);

    let state = attach_replicated_state_source(
        &FakeSource(Arc::clone(&inner)),
        Some(Arc::new(move |message| {
            collected.lock().unwrap().push(message)
        })),
    );

    // 期望游标 1，实际给 5 → 契约失败。
    inner.emit(frame(5, json!(1)));

    let messages = errors.lock().unwrap().clone();
    assert_eq!(messages.len(), 1, "应上报一次游标缺口");
    assert!(messages[0].contains("cursor has a gap"), "{}", messages[0]);
    assert_eq!(
        inner.disposed.load(Ordering::Relaxed),
        1,
        "契约失败应释放 attachment（且只释放一次）"
    );
    let _ = state;
}

#[tokio::test]
async fn dispose_is_idempotent() {
    let inner = FakeInner::new(json!(0));
    let state = AttachedReplicatedState::new(Box::new(FakeAttachment(Arc::clone(&inner))), None);
    state.activate();

    state.dispose();
    state.dispose();

    assert_eq!(inner.disposed.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn publisher_overflow_keeps_latest_value() {
    let publisher = ReplicatedStatePublisher::new(json!(0), None);
    let (listener, mut rx) = collector();
    let _subscription = publisher.subscribe(listener);

    let (value, _) = rx.recv().await.expect("hydration");
    assert_eq!(value, json!(0));

    let total = 150u64;
    for index in 1..=total {
        publisher.publish(json!(index), Vec::new(), service_delivery_context());
    }

    // 逐步读到队列排空，最后一条必须是最终发布的值（溢出可能跳过中间帧）。
    let mut last = None;
    while let Ok(Some((value, _))) =
        tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
    {
        last = Some(value);
    }
    assert_eq!(last, Some(json!(total)), "溢出后仍应收到最新值");
}

#[tokio::test]
async fn unsubscribe_stops_delivery() {
    let publisher = ReplicatedStatePublisher::new(json!(0), None);
    let (listener, mut rx) = collector();
    let subscription = publisher.subscribe(listener);

    let _ = rx.recv().await.expect("hydration");
    assert_eq!(publisher.subscriber_count(), 1);
    subscription.unsubscribe();
    assert_eq!(publisher.subscriber_count(), 0, "取消后应从订阅者列表移除");

    publisher.publish(json!(1), Vec::new(), service_delivery_context());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let result = rx.try_recv();
    assert!(result.is_err(), "取消订阅后不应再收到投递");
}
