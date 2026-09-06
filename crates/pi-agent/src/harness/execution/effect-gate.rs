//! Rust 翻译自 packages/agent/src/harness/execution/effect-gate.ts
//!
//! 一次 drive pass 的效果门（procedure-facing admit + owner-facing abort/close）。

use std::sync::{Arc, Mutex};

use pi_ai::AbortSignal;

/// 对应 `AbortRequested`。
#[derive(Debug)]
pub struct AbortRequested;

impl std::fmt::Display for AbortRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Abort requested")
    }
}
impl std::error::Error for AbortRequested {}

enum GateState {
    Open,
    Aborting,
    Closed { error: String },
}

/// 对应 `Gate`。
#[derive(Clone)]
pub struct Gate {
    pub signal: AbortSignal,
    state: Arc<Mutex<GateState>>,
}

/// 对应 `GateControl`。
pub struct GateControl {
    signal: AbortSignal,
    state: Arc<Mutex<GateState>>,
}

impl Gate {
    /// 对应 `admit`：状态检查后同步执行。
    pub fn admit<T>(&self, invoke: impl FnOnce() -> T) -> T {
        {
            let state = self.state.lock().unwrap();
            match &*state {
                GateState::Open => {}
                GateState::Aborting => panic!("{}", AbortRequested),
                GateState::Closed { error } => panic!("{error}"),
            }
        }
        invoke()
    }
}

impl GateControl {
    /// 对应 `beginAbort`。
    pub fn begin_abort(&self) {
        let mut state = self.state.lock().unwrap();
        if matches!(&*state, GateState::Open) {
            *state = GateState::Aborting;
        }
    }

    /// 对应 `signalAbort`。
    pub fn signal_abort(&self) {
        let state = self.state.lock().unwrap();
        if matches!(&*state, GateState::Aborting) && !self.signal.aborted() {
            self.signal.abort();
        }
    }

    /// 对应 `close`。
    pub fn close(&self, error: String) {
        let mut state = self.state.lock().unwrap();
        if matches!(&*state, GateState::Closed { .. }) {
            return;
        }
        *state = GateState::Closed { error };
        if !self.signal.aborted() {
            self.signal.abort();
        }
    }
}

/// 对应 `createGate`。
pub fn create_gate() -> (Gate, GateControl) {
    let state = Arc::new(Mutex::new(GateState::Open));
    let signal = AbortSignal::new();
    (
        Gate {
            signal: signal.clone(),
            state: Arc::clone(&state),
        },
        GateControl { signal, state },
    )
}
