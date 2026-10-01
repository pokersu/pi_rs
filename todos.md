# 剩余复刻 TODO

> 本文件是 `UPSTREAM.md`「未完成清单（backlog）」的详情页，**只记录当前仍未对齐的项**。
> 完整差异扫描见 `UPSTREAM-PARITY.md`（可重跑：`python3 tools.d/.parity/scan.py`）。
> 已对齐项的改动明细见 git 历史，此处不再保留。
>
> 分类：
> - **B 类** —— 原版存在，但对 Rust 版属 legacy 迁移 / Node 平台适配 / 一致性测试，
>   不属于 harness runtime 的产品逻辑。按需取舍，不必 1:1 逐行搬。
> - **C 类** —— 类型层 / API 形态差异（**无运行时行为**，或已核实等价），建议按需补。

## 当前复刻程度（已对齐部分，仅列概览）

- **drive 引擎**：40 个 leaf + `drive_operation`，含 deferred 轮询（`stream_deferred`）、`run_tools` 接入 reconcile、`cancel_deferred` best-effort。
- **lane**：command/settle/continue + accept/drive/request_abort + 31 个 agent 方法（含 `watch`/`run_when_idle`）+ idle 管理。
- **事件系统**：强类型 29 种 `HarnessEvent` + `HarnessEventBus`/`BufferedEventWatcher`（epoch / resnapshot boundary）。
- **reducer / restore / harness**（`Harness` 类 + `create_agent_harness`），fault 与 close 分别对应 `HarnessError::Fault` / `Closed`。
- **session 存储**：memory / jsonl 主路径（`create_fork`、两阶段流式 `run_jsonl_fork`、`list`、`capture_fork_next_seq`）。
- **compaction**（含 branch-summarization 的 LLM 生成）、hooks、execution、skills、prompt-templates。
- **telemetry**：`AI_TELEMETRY_SCHEMA` + `HARNESS_TELEMETRY_SCHEMA`（12 个 span）+ `start_ai_span` / `start_harness_span`。
- **工具**：10 个内置工具已复刻 —— bash（流式 `onUpdate` + `commandPrefix`/`prepare`）、read（图片 + `imageProcessor`）、edit（含 `prepareArguments` 的 legacy / 字符串形态规范化）、edit-diff（NFKC 归一化）等。
- **工具执行管线**：`prepareArguments` → 校验 → `beforeToolCall` → 执行 → `afterToolCall`，`isError` / `structuredContent` 全程贯通，公开入口 `run_tool_call`。

## B 类清单（平台 / 测试 / 迁移）

| # | 模块 | 原版路径 | 约行数 | 性质 | 建议 |
|---|------|----------|-------|------|------|
| B1 | Node 环境适配 | `harness/env/nodejs.ts` | 851 | Node 平台细节（findBashOnPath / WSL bash 检测 / killProcessTree） | 不搬 |
| B2 | 会话一致性测试套件 | `harness/session/testing/conformance/*` + `benchmark/*` + `gating-storage.ts` + `storage-decorator.ts` | ~2,275 | 测试基建，验证 storage/repo 契约 | 不搬（或按需补测试） |
| B3 | 旧版 JSONL 迁移 | `harness/session/jsonl/legacy-v3.ts` | 539 | 老版本 JSONL 会话格式的读取迁移 | 不搬（除非兼容旧数据） |

合计约 **3,665 行 TS**。

## C 类清单（仍存在的类型层 / API 形态差异）

| # | 项 | 原版位置 | Rust 现状 | 影响 | 估算 | 建议 |
|---|----|----------|-----------|------|------|------|
| C2 | 类型层豁免（无运行时行为）：`harness/telemetry.ts` 的 16 个 span 类型、工具的 `*ToolInput`（`BashToolInput`/`EditToolInput`/`ReadToolInput`/`WriteToolInput`） | `harness/telemetry.ts`、`harness/tools/*.ts` | 均为从 schema 推导的类型（`TelemetrySchemaSpanName<typeof SCHEMA>` / `Static<typeof schema>`），Rust 无类型级推导能力；运行时部分均已具备 | 无运行时行为 | — | 豁免 |
| C3 | session 具名错误 `SessionInvalidBranchError` / `SessionBranchExistsError` / `SessionPendingAssistantMessageError` / `SessionUnknownTargetError` | `harness/session/session.ts` | `Session` trait 统一 `Result<_, String>`，仅保留 `SessionInvariantError` | **已核实**：消息文本逐字对齐，上游自身无 `instanceof` 分支 → 无行为差异 | 大（需改 trait 错误类型） | 不建议 |
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

### C2 类型层豁免（telemetry span 类型 + 工具输入类型）

上游 `harness/telemetry.ts` 的 16 个 span 类型是 `TelemetrySchemaSpanName<typeof AI_TELEMETRY_SCHEMA>`
这类**从 schema 推导**的类型；四个工具的 `*ToolInput` 则是 `Static<typeof xxxSchema>`。
Rust 没有类型级推导能力，手写会与 schema 脱节。两者的运行时部分均已就位：
`start_ai_span` / `start_harness_span` / 两个 `*_SCHEMA`，以及 edit 的 `prepareEditArguments`。
结论：**豁免**。

### C3 session 具名错误

上游有 4 个具名错误类，Rust 用 `Result<_, String>` 承载，但**消息文本已逐字对齐**
（`Invalid branch "x": <reason>` / `Branch already exists: x` / `Unknown target: x` /
`Cannot persist a pending assistant message`），且上游自身没有任何 `instanceof` 分支。
补它需要把 `Session` trait 的错误类型整体换成枚举，牵动所有实现与调用点 —— **建议不投入**。

### C5 = B2 的子集

`conformance` 的 storage/session-repo 契约测试，归入 B2 一致处理。
