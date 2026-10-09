//! 对应 `src/harness/`：durable 之上的 agent harness。
//!
//! harness 是 v1.1.0 架构下会话编排、任务图与工具执行的所在层。本模块按依赖自底向上分阶段落地，
//! 计划与逐项差异见根目录 `UPSTREAM-SYNC-v1.1.0.md` 的 P5。
//!
//! 已落地：
//!
//! - [`json`]（P5a）：`assignJson` 的逐叶写入（对应 `harness/json.ts`）
//! - [`output`]（P5a）：输出消毒、按行/字节裁剪、有界运行输出缓冲（对应 `harness/output.ts`）
//! - [`util`]（P5a）：分页扫描与关闭错误（对应 `harness/util.ts` 的 `scanAll` / `closedError`）
//! - [`types`]（P5b）：harness 的数据模型、扩展契约与顶层编排接口（对应 `harness/types.ts`）
//! - [`define`]（P5b）：扩展 / 工具 / 章节 / hook / 包装的 DSL（对应 `harness/define.ts`）
//! - [`agent`] / [`provider`] / [`usage`] / [`inbox`] / [`live`]（P5c）：内置文档与其事务内操作
//! - [`view`] / [`context`]（P5d）：会话视图挂载与模型上下文（对应 `harness/view.ts` / `context.ts`）
//! - [`events`]（P5e）：agent 事件流与 `watchEvents`（对应 `harness/events.ts`）
//! - [`scheduler`]（P5f）：任务调度器的所有权 / 保留 / 过期 / 执行（对应 `harness/scheduler.ts`）
//! - [`generation`] / [`compaction`] / [`tool`] / [`registry`]（P5g）：三个内置 Task 与注册表写面
//! - [`harness`]（P5h）：`Harness` / `Conversation` 组装与 `open` 工厂（对应 `harness/harness.ts`）
//!
//! 各阶段有意推迟的部分在对应模块的文档里注明。

pub mod agent;
pub mod compaction;
pub mod context;
pub mod define;
pub mod events;
pub mod generation;
// 与上游 `harness/harness.ts` 的文件名对齐；`harness::harness` 是刻意的映射。
#[allow(clippy::module_inception)]
pub mod harness;
pub mod inbox;
pub mod json;
pub mod live;
pub mod output;
pub mod prompt;
pub mod provider;
pub mod registry;
pub mod scheduler;
pub mod submissions;
pub mod task_graph;
pub mod tool;
pub mod types;
pub mod usage;
pub mod util;
pub mod view;
