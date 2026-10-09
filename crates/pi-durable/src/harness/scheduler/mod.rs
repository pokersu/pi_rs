//! 对应 `harness/scheduler.ts`：durable 任务调度器。
//!
//! 上游是 1435 行的单一类。Rust 侧按可独立验证的边界拆分：
//!
//! - [`ownership`]（✅ 已落地）：所有权树的遍历与派生逻辑——纯逻辑，可单独测试
//! - [`state`]（✅ 已落地）：状态表（`#live` / `#edges` / `#settled` / `#contexts` / 标志位）
//!   与提交监听 `#observe`——状态表实现在 `OwnerLookup` 之上，因此遍历函数直接作用于它
//! - [`reserve`]（✅ 已落地）：预留决策——`#waitingOn` / `#fit` / `#resolve` / `#inspectTask`
//! - 执行层（待落地）：`open` / `abort` / `waitForTask` / `waitForIdle` / `abortConversation` /
//!   `#reconcile` / `#finalize` / `#reserve`（I/O 部分）/ `#drain` / `#run` / `#step` /
//!   `#terminate` / `#commitState` / `#validateWait` / `#runtime`
//!
//! 执行层依赖上述三个模块与 P5f-2 落地的运行时契约（`TaskRuntime` / `RunningTask` / `NextTaskState`）。

pub mod exec;
pub mod expiry;
pub mod ownership;
pub mod reserve;
pub mod state;

pub use exec::{
    AbortResult, Invocation, InvocationBinding, InvocationMode, SchedulerOutcome, TaskScheduler,
    TaskSchedulerOptions,
};
