# 剩余复刻 TODO（B 类）

> 本文件记录 `@pi-ai/agent` 原版 harness 中**尚未复刻到 `pi-agent` 的剩余模块**。
> 这些是「B 类」：原版存在，但对 Rust 版本大多是 legacy 迁移 / Node 平台适配 / 一致性测试，
> 不属于 harness runtime 的产品逻辑。按需取舍，不必 1:1 逐行搬。

## 已完成（对照）

harness runtime 核心已 100% 复刻：

- **drive 引擎**：40 个 leaf + `drive_operation`，含 deferred 轮询（`stream_deferred`）、`run_tools` 接入 reconcile、`cancel_deferred` best-effort。
- **lane**：command/settle/continue + accept/drive/request_abort + **31 个 agent 方法**（含 `watch`/`run_when_idle`）+ idle 管理。
- **事件系统**：强类型 29 种 `HarnessEvent` + 完整 `HarnessEventBus`/`BufferedEventWatcher`（`watch` 快照订阅，含 epoch/resnapshot boundary）。
- **reducer / restore / harness**（`Harness` 类 + `create_agent_harness`）。
- **session 存储**：memory / jsonl 主路径（含 `storage.fork`（内存 `create_fork`）、jsonl 两阶段流式 `run_jsonl_fork`、`list` 目录扫描、`capture_fork_next_seq`）。
- **compaction**（含 branch-summarization 的 LLM 生成）、hooks、execution、skills、prompt-templates。
- **telemetry**：schema 数据已补（`AI_TELEMETRY_SCHEMA` + `HARNESS_TELEMETRY_SCHEMA`，12 个 span）。
- **工具**：全部 10 个工具已复刻 —— bash（流式 `onUpdate` + `commandPrefix`/`prepare`）、read（图片 + `imageProcessor`）、edit-diff（NFKC 归一化）等。

## B 类清单（剩余未复刻）

| # | 模块 | 原版路径 | 约行数 | 性质 | 建议 |
|---|------|----------|-------|------|------|
| B1 | Node 环境适配 | `harness/env/nodejs.ts` | 851 | Node 平台细节（findBashOnPath / WSL bash 检测 / killProcessTree） | 不搬 |
| B2 | 会话一致性测试套件 | `harness/session/testing/conformance/*` + `benchmark/*` + `gating-storage.ts` + `storage-decorator.ts` | ~2,275 | 测试基建，验证 storage/repo 契约 | 不搬（或按需补测试） |
| B3 | 旧版 JSONL 迁移 | `harness/session/jsonl/legacy-v3.ts` | 539 | 老版本 JSONL 会话格式的读取迁移 | 不搬（除非兼容旧数据） |

合计约 **3,665 行 TS**。（原 B4 telemetry schema、B5 工具已在前几轮补齐，见上「已完成」。）

## 逐项说明

### B1 `env/nodejs.ts`（851 行）
Node 的进程/文件/网络环境实现。Rust 已实现 `NodeExecutionEnv`（FileSystem + Shell + ExecutionEnv，含子进程 spawn + 流式 stdout/stderr + timeout/abort）。剩余差异是 Node 平台细节（`findBashOnPath`、`isLegacyWslBashPath`、`killProcessTree`），Rust 的 `std::process` 已等价覆盖。

### B2 一致性测试套件（约 2,275 行）
- `session/testing/conformance/storage.ts`（920）、`session-repo.ts`（846）
- `session/testing/benchmark/storage.ts`（143）、`session-repo.ts`（181）、`datasets.ts`（33）
- `session/testing/gating-storage.ts`（114）、`storage-decorator.ts`（71）、`instrumented-storage.ts`（21）

这些是验证 storage/session-repo 行为契约的测试代码，不是产品逻辑。Rust 侧 `session/testing/` 目前仅占位（`create_session_backend_conformance` 返回空列表），可后续按需补对应测试。

### B3 `session/jsonl/legacy-v3.ts`（539 行）
旧版本（v3）JSONL 会话格式的读取/迁移。Rust 侧 `storage.rs` 的 `V3Legacy` 分支返回 "Legacy v3 JSONL migration is not yet implemented"。仅当需要读旧格式会话文件时才需要。
