# pi-agent

agent 运行时核心：`Agent` 类 + 双层循环 + 工具执行管线 + 消息队列，以及完整的 harness（文件/Shell 抽象、内置工具、session 持久化、上下文压缩、skills、telemetry）。

## 复刻来源

1:1 复刻自 [earendil-works/pi](https://github.com/earendil-works/pi) 的 `packages/agent`，50 个 TS 文件全部对应（Rust 53 文件，多出的 `mod.rs`/`node.rs` 是 Rust 的模块声明与 re-export 惯例）：

| 原 TS 目录/文件 | Rust 对应 |
|---|---|
| `agent.ts` / `agent-loop.ts` / `types.ts` / `stream-fn.ts` / `proxy.ts` | 同名 `.rs` |
| `search/`（`index.ts` + `scanning.ts`） | `search/` |
| `harness/types.ts` / `result.ts` / `messages.ts` / `events.ts` / `system-prompt.ts` / `prompt-templates.ts` / `skills.ts` / `telemetry.ts` / `agent-harness.ts` / `reducer.ts` | 同名 `.rs` |
| `harness/env/nodejs.ts` | `harness/env/nodejs.rs` |
| `harness/tools/`（read/write/edit/edit-diff/bash/image/path-utils/tool-context/file-mutation-queue） | 同名 `.rs` |
| `harness/utils/`（truncate/shell-output） | 同名 `.rs` |
| `harness/compaction/`（compaction/utils/branch-summarization） | 同名 `.rs` |
| `harness/session/`（types/context/state/session/memory/jsonl/testing） | 同名 `.rs` |

## 功能介绍

**核心运行时**（`src/` 根）：
- `Agent` 类 —— 有状态封装：transcript、事件订阅、steering/follow-up 消息队列、生命周期（abort/waitForIdle/reset）。
- `agent-loop` —— 无状态循环：`runAgentLoop`/`runAgentLoopContinue` + 双层 `run_loop`。
- `proxy` —— 远程代理流（通过 server 转发 LLM 调用，重建精简事件）。
- `stream-fn` —— 默认流函数注册。

**harness**（`src/harness/`）：
- 抽象：`FileSystem`/`Shell`/`ExecutionEnv`（文件与进程能力）、`Skill`/`PromptTemplate`。
- 内置工具：`read`/`write`/`edit`（含 diff 算法）/`bash`。
- 消息：`AgentMessage` 含 4 种自定义消息（bashExecution/custom/branchSummary/compactionSummary）+ `convertToLlm`。
- session：`SessionState`（内存）+ `InMemory` 与 `JSONL` 两种后端 + `reducer`（单写者记录协议的状态归约）。
- compaction：token 估计、切点查找、branch summary。
- skills / prompt-templates / system-prompt / telemetry / events / agent-harness。

## 原理

1. **双层循环**（`agent-loop.rs` 的 `run_loop`）：内层循环处理「工具调用 + steering 消息」，外层循环处理「agent 本应停止时的 follow-up 消息」。每一轮：`prepareNextTurn`（可做 compaction）→ 注入 pending 消息 → `streamAssistantResponse`（transformContext → convertToLlm → streamFn）→ 执行 tool calls → `turn_end` → 检查 `shouldStopAfterTurn` → 拉取新 steering 消息。
2. **工具执行管线**：`prepare`（参数校验 + `beforeToolCall` 钩子）→ `execute`（工具 `execute`，`onUpdate` 流式进度）→ `finalize`（`afterToolCall` 钩子字段级覆盖）。支持 `sequential`/`parallel` 两种模式，`terminate` 标志实现批次提前终止。
3. **事件系统**：`AgentEvent` 联合类型覆盖 agent/turn/message/tool 四层生命周期；`subscribe` 的 listener 是 run settlement 的一部分（`agent_end` 后 listener 完成才算 idle）。
4. **消息队列**：`steer()`（当前 turn 后注入）与 `followUp()`（agent 停止后注入），`one-at-a-time`/`all` 两种 drain 模式。
5. **消息边界**：内部 `AgentMessage`（含自定义消息）在 LLM 调用边界通过 `convert_to_llm` 转成 `Message`，`transform_context` 做上下文窗口管理。
6. **session 单写者记录协议**：`Entry`（消息/配置变更/compaction 等）与 `LaneRecord`（操作/工具/队列记录）按 seq 严格递增 append；`reducer.rs` 从恢复切片重建 lane 状态并校验一致性（`validate_record_log`）。
7. **compaction**：`estimate_tokens`（字符启发式）、`should_compact`（阈值判断）、`find_cut_point`（保持近期 token 预算的切点）。

## 复刻过程中的变化

1. **`CustomAgentMessages` → enum**：TS 用 declaration merging 扩展 `AgentMessage`，Rust 改为 `AgentMessage` enum（User/Assistant/ToolResult + 4 种自定义消息），`role()` 返回判别字符串。
2. **`AsyncIterable` → `Vec`**：`SessionSearch.search`、`scanningEntries` 返回 `Vec`（Rust 无内置异步生成器，流式可后续用 `Stream` 补）。
3. **LLM 调用路径留空**：`generateBranchSummary`/`completeSimpleWithRetries`/`prepareCompaction`/`generateSummary` 依赖模型流式调用，保留 prompt 常量与 prepare/collect 逻辑，实际 LLM 调用待接 provider 时补。
4. **bash 流式 `onUpdate` 省略**：`Shell.exec` 用 `std::process::Command::output` 一次性返回，省略 100ms 节流的流式进度。
5. **read 工具图片**：返回提示而非 base64 attachment（TS 依赖 photon wasm）。
6. **`jsonl::list` 简化**：元数据目录扫描返回空列表。
7. **conformance 测试简化**：`harness/session/testing/conformance.ts`（1000+ 行 storage 契约测试）简化为空工厂，保留 fixture 类型。
8. **`AgentHarness` 操作方法是占位**：与 TS 原版一致（原版也是 `unavailable()` 返回 `HarnessNotImplemented`），仅 getter/setter 真实现。
9. **TypeBox → JSON 值**：`AgentTool` 参数用 `serde_json::Value`，`prepareArguments` 暂未实现。

## 阅读步骤

1. `src/types.rs` —— 先建立 `AgentMessage`/`AgentState`/`AgentTool`/`AgentEvent`/`AgentLoopConfig` 全貌。
2. `src/stream-fn.rs`（20 行）—— 默认流函数注册，最短入口。
3. `src/agent-loop.rs` —— 无状态核心循环，理解双层循环 + 工具执行管线（重点）。
4. `src/agent.rs` —— 有状态 `Agent` 封装，理解生命周期、消息队列、事件归约。
5. `src/proxy.rs` —— 远程代理流（可选）。
6. `harness/types.rs` + `harness/env/nodejs.rs` —— 文件/Shell 抽象与真实实现。
7. `harness/tools/read.rs`/`write.rs`/`bash.rs`/`edit.rs`/`edit-diff.rs` —— 内置工具（edit-diff 含 diff 算法）。
8. `harness/messages.rs` —— 自定义消息与 `convert_to_llm`。
9. `harness/session/`（types → state → memory → context → reducer）—— 单写者记录协议与状态归约。
10. `harness/compaction/`（utils → compaction → branch-summarization）—— token 估计与切点。
11. `harness/skills.rs`/`prompt-templates.rs`/`system-prompt.rs`/`telemetry.rs`/`events.rs`/`agent-harness.rs` —— 收尾。
