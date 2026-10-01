# 剩余复刻 TODO

> 本文件是 `UPSTREAM.md`「未完成清单（backlog）」的**详情页**，记录 pi_rs 相对上游 Pi 的尚未复刻项。
> 完整差异扫描见 `UPSTREAM-PARITY.md`（可重跑：`python3 tools.d/.parity/scan.py`）。
>
> 分类：
> - **B 类** —— 原版存在，但对 Rust 版属 legacy 迁移 / Node 平台适配 / 一致性测试，
>   不属于 harness runtime 的产品逻辑。按需取舍，不必 1:1 逐行搬。
> - **C 类** —— 类型层 / API 形态差异（**无运行时行为**，或已核实等价）。来自 2026-10-01
>   全量方法级对比；建议按需补，不影响正确性。

## 已完成（对照）

### 核心 runtime（100%）

- **drive 引擎**：40 个 leaf + `drive_operation`，含 deferred 轮询（`stream_deferred`）、`run_tools` 接入 reconcile、`cancel_deferred` best-effort。
- **lane**：command/settle/continue + accept/drive/request_abort + **31 个 agent 方法**（含 `watch`/`run_when_idle`）+ idle 管理。
- **事件系统**：强类型 29 种 `HarnessEvent` + 完整 `HarnessEventBus`/`BufferedEventWatcher`（`watch` 快照订阅，含 epoch/resnapshot boundary）。
- **reducer / restore / harness**（`Harness` 类 + `create_agent_harness`）。
- **session 存储**：memory / jsonl 主路径（含 `storage.fork`（内存 `create_fork`）、jsonl 两阶段流式 `run_jsonl_fork`、`list` 目录扫描、`capture_fork_next_seq`）。
- **compaction**（含 branch-summarization 的 LLM 生成）、hooks、execution、skills、prompt-templates。
- **telemetry**：schema 数据已补（`AI_TELEMETRY_SCHEMA` + `HARNESS_TELEMETRY_SCHEMA`，12 个 span）。
- **工具**：全部 10 个工具已复刻 —— bash（流式 `onUpdate` + `commandPrefix`/`prepare`）、read（图片 + `imageProcessor`）、edit-diff（NFKC 归一化）等。

### 逻辑对齐（已完成）

对照上游逐项核实，已修复 9 处逻辑/数据不等价（两轮：`LaneSnapshot.faulted` 语义反转 + 8 项工具/会话层）。
其中最关键的是 **`isError` 传递丢失**：`finalize_executed_tool_call` 原硬编码 `is_error = false`，
导致工具抛错被上报为成功。逐条改动明细见 git 历史；判定为「适配非缺口」的项见下方 C3/C4。

## B 类清单（平台 / 测试 / 迁移）

| # | 模块 | 原版路径 | 约行数 | 性质 | 建议 |
|---|------|----------|-------|------|------|
| B1 | Node 环境适配 | `harness/env/nodejs.ts` | 851 | Node 平台细节（findBashOnPath / WSL bash 检测 / killProcessTree） | 不搬 |
| B2 | 会话一致性测试套件 | `harness/session/testing/conformance/*` + `benchmark/*` + `gating-storage.ts` + `storage-decorator.ts` | ~2,275 | 测试基建，验证 storage/repo 契约 | 不搬（或按需补测试） |
| B3 | 旧版 JSONL 迁移 | `harness/session/jsonl/legacy-v3.ts` | 539 | 老版本 JSONL 会话格式的读取迁移 | 不搬（除非兼容旧数据） |

合计约 **3,665 行 TS**。

## C 类清单（类型层 / API 形态，2026-10-01 全量对比新增）

| # | 项 | 原版位置 | Rust 现状 | 影响 | 估算 | 建议 |
|---|----|----------|-----------|------|------|------|
| C1 | 工具入参类型 `BashToolInput` / `EditToolInput` / `ReadToolInput` / `WriteToolInput` | `harness/tools/{bash,edit,read,write}.ts` | serde 内联解析 `arguments`，无同名导出类型 | 无运行时差异，仅缺 API 形态与编译期类型安全 | 小 | 可补 |
| C2 | `harness/telemetry.ts` 的 16 个 span 类型（`AiSpanName`/`AiSpanAttributes`/`AiSpanStartAttributes`/`AiSpanEndAttributes`/`AiSpanEventName`/`AiSpanEventAttributes`/`HarnessSpan*`） | `harness/telemetry.ts` | `harness/telemetry.rs` 只有 `HOOK_NAMES`/`EVENT_TYPES` 常量表 + `agent_telemetry_schemas()` | 无运行时行为，纯类型层 | 小–中 | 可补 |
| C3 | session 具名错误 `SessionInvalidBranchError` / `SessionBranchExistsError` / `SessionPendingAssistantMessageError` / `SessionUnknownTargetError` | `harness/session/session.ts` | `Session` trait 统一 `Result<_, String>`，仅保留 `SessionInvariantError` | **已核实**：消息文本逐字对齐，上游自身无 `instanceof` 分支 → 无行为差异 | 大（需改 trait 错误类型） | 不建议 |
| C4 | `HarnessFault` / `HarnessClosed` 错误变体 | `harness/runtime/harness.ts` | lane 侧已用 `SealReason` 对齐；harness 对外统一 `HarnessError::Closed` | **已核实**：唯一 `instanceof` 消费处在 lane 层，已消化 → 无行为差异 | 小 | 不建议 |
| C5 | `session/testing/conformance` 空实现 | `conformance/{session-repo,storage}.ts` | `create_session_backend_conformance` 返回空列表；上游 1700+ 行 | 无运行时；决定 storage 契约回归能力 | 中–大 | 见 B2 |

> 另有 `pi-telemetry` 的 12 项 TS **类型级编程**（条件类型 / 映射类型 / `UnionToIntersection`）与
> `pi-ai` 的 107 项（绝大多数为 `crates/pi-ai/AGENT.md` 声明的范围外），**不列为待办**。

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

### C1 工具入参类型

上游每个内置工具都导出输入类型（如 `BashToolInput { command, timeout? }`）供调用方与工具定义共用；Rust 直接在 `execute` 里从 `serde_json::Value` 取字段。补法是给 4 个工具各加输入结构体，并让它同时驱动 `parameters` JSON schema。`*ToolDetails`（`BashToolDetails` / `ReadToolDetails`）已于 2026-10-01 补齐。

### C2 telemetry span 类型层

上游 `harness/telemetry.ts` 导出按 span 名分组的属性类型（`AiSpanStartAttributes` 等 16 个）。Rust 侧以 `agent_telemetry_schemas()` 的 JSON schema + `HOOK_NAMES`/`EVENT_TYPES` 常量表达同一信息，运行时行为一致，差的是编译期类型约束。

### C3 / C4 已核实为「适配非缺口」

两者的判定依据都写在上表「影响」列：C3 的消息文本已逐字对齐且上游无分支消费；C4 的唯一 `instanceof` 消费点（`lane.ts` 的 `faulted`）已在 lane 层用 `SealReason` 对齐。若无新需求，**建议不投入**。
