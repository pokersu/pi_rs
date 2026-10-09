//! Rust 翻译自 packages/durable（`@earendil-works/pi-durable`，v1.1.0）。
//!
//! 上游 v1.0.0 把 harness 从 `pi-agent` 移出后，改由本包承载 durable conversation /
//! task / document runtime。与旧 harness（lane + drive 状态机）架构不同，本包是
//! **document/transaction + task-graph** 模型，无法从旧实现搬运。
//!
//! 模块与移植计划见 `UPSTREAM-SYNC-v1.1.0.md`：
//!
//! - `chord`（`delta` 不可变状态 apply + `context` + 基础类型）
//! - 基础层（types / documents / entries / errors / ids / tasks / truncate）
//! - `storage`（抽象 + memory + jsonl + sqlite）
//! - `session`（session / transaction / observation / forks）
//! - `harness` 核心（define / registry / events / task-graph / scheduler / harness / 内置 Task）
//! - `tools` + `env`
//! - `testing` / conformance
//!
//! **当前状态：chord 子集、基础层与 storage 三后端（memory / jsonl / sqlite）、session 层
//! （P1–P4）以及 harness 的基础工具（P5a）已落地；harness 的其余部分待做。**

pub mod chord;
pub mod documents;
pub mod entries;
pub mod env;
pub mod errors;
pub mod harness;
pub mod ids;
pub mod session;
pub mod storage;
pub mod tasks;
pub mod testing;
pub mod tools;
pub mod truncate;
pub mod types;
