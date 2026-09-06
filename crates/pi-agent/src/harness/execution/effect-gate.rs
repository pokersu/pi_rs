//! Rust 翻译自 packages/agent/src/harness/execution/effect-gate.ts
//!
//! 一次 drive pass 的效果门（procedure-facing admit + owner-facing abort/close）。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pi_ai::AbortSignal;

/// 对应 `cancellation: Promise<void>`：可重复等待的取消完成句柄。
/// Rust 中 Promise 为单消费 Future，故用「每次调用返回新 Future」的工厂函数表示。
pub type Cancellation = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// 对应 `AbortRequested`：取消抢先效果准入时的预期控制流错误。
#[derive(Clone)]
pub struct AbortRequested {
    pub cancellation: Cancellation,
}

impl std::fmt::Debug for AbortRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbortRequested").finish_non_exhaustive()
    }
}

impl std::fmt::Display for AbortRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Abort requested")
    }
}

impl std::error::Error for AbortRequested {}

/// 对应 `Gate.admit` 在 `check()` 阶段抛出的两类错误。
#[derive(Debug)]
pub enum GateError {
    AbortRequested(AbortRequested),
    Closed(String),
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateError::AbortRequested(_) => write!(f, "Abort requested"),
            GateError::Closed(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for GateError {}

enum GateState {
    Open,
    Aborting { cancellation: Cancellation },
    Closed { error: String },
}

/// 对应 `Gate`：procedure 面向的同步准入能力。
#[derive(Clone)]
pub struct Gate {
    pub signal: AbortSignal,
    state: Arc<Mutex<GateState>>,
}

/// 对应 `GateControl`：owner 面向的生命周期控制。
#[derive(Clone)]
pub struct GateControl {
    signal: AbortSignal,
    state: Arc<Mutex<GateState>>,
}

impl Gate {
    /// 对应 `admit`：状态检查后同步执行 `invoke`。
    ///
    /// 检查失败返回 `Err`（对应原版的 throw `AbortRequested` / `error`）。
    pub fn admit<T>(&self, invoke: impl FnOnce() -> T) -> Result<T, GateError> {
        {
            let state = self.state.lock().unwrap();
            match &*state {
                GateState::Open => {}
                GateState::Aborting { cancellation } => {
                    return Err(GateError::AbortRequested(AbortRequested {
                        cancellation: cancellation.clone(),
                    }));
                }
                GateState::Closed { error } => {
                    return Err(GateError::Closed(error.clone()));
                }
            }
        }
        Ok(invoke())
    }
}

impl GateControl {
    /// 对应 `beginAbort`。
    pub fn begin_abort(&self, cancellation: Cancellation) {
        let mut state = self.state.lock().unwrap();
        if matches!(&*state, GateState::Open) {
            *state = GateState::Aborting { cancellation };
        }
    }

    /// 对应 `signalAbort`。
    pub fn signal_abort(&self) {
        let state = self.state.lock().unwrap();
        if matches!(&*state, GateState::Aborting { .. }) && !self.signal.aborted() {
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
