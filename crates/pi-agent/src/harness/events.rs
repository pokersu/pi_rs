//! Rust 翻译自 packages/agent/src/harness/events.ts

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 对应 `RunStartEvent`
#[derive(Debug, Clone, PartialEq)]
pub struct RunStartEvent {
    pub lane: String,
    pub run_id: String,
}

/// 对应 `RunEndEvent`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Aborted,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunEndEvent {
    pub lane: String,
    pub run_id: String,
    pub outcome: RunOutcome,
    pub leaf_id: String,
}

/// 对应 `HarnessEvent`
#[derive(Debug, Clone, PartialEq)]
pub enum HarnessEvent {
    RunStart(RunStartEvent),
    RunEnd(RunEndEvent),
}

impl HarnessEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            HarnessEvent::RunStart(_) => "run_start",
            HarnessEvent::RunEnd(_) => "run_end",
        }
    }
}

/// 对应 `HarnessEventListener`
pub type HarnessEventListener = Arc<dyn Fn(HarnessEvent) + Send + Sync>;

/// 对应 `Events` trait + `HarnessEventBus`。
pub struct HarnessEventBus {
    listeners: Mutex<HashMap<&'static str, Vec<HarnessEventListener>>>,
    watch_listeners: Mutex<Vec<HarnessEventListener>>,
}

impl Default for HarnessEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessEventBus {
    pub fn new() -> Self {
        Self {
            listeners: Mutex::new(HashMap::new()),
            watch_listeners: Mutex::new(Vec::new()),
        }
    }

    /// 对应 `on`
    pub fn on(&self, event_type: &'static str, listener: HarnessEventListener) {
        let mut listeners = self.listeners.lock().unwrap();
        listeners.entry(event_type).or_default().push(listener);
    }

    /// 对应 `emit`
    pub fn emit(&self, event: HarnessEvent) {
        let event_type = event.event_type();
        let listeners = self.listeners.lock().unwrap();
        if let Some(set) = listeners.get(event_type) {
            for listener in set {
                listener(event.clone());
            }
        }
        let watch = self.watch_listeners.lock().unwrap();
        for listener in watch.iter() {
            listener(event.clone());
        }
    }

    /// 对应 `watch`（简化：返回快照 + 订阅）。
    pub fn watch<TSnapshot>(
        &self,
        capture_snapshot: impl FnOnce() -> TSnapshot,
    ) -> (TSnapshot, HarnessEventListener, impl FnOnce()) {
        let listener: HarnessEventListener = Arc::new(|_| {});
        self.watch_listeners.lock().unwrap().push(listener.clone());
        let snapshot = capture_snapshot();
        (snapshot, listener, move || {})
    }
}
