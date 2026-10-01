# pi-agent

agent 运行时核心：`Agent` 类 + 双层循环 + 工具执行管线 + 消息队列，以及完整的 harness（文件/Shell 抽象、内置工具、session 持久化、上下文压缩、skills、telemetry）。

## 复刻来源

1:1 复刻自 [earendil-works/pi](https://github.com/earendil-works/pi) 的 `packages/agent`，91 个非测试 TS 文件中 83 个已配对（Rust 侧另有若干 `mod.rs` 模块声明，属 Rust 惯例）。剩余 9 个未对应文件集中在 **B 类**：`session/testing/*` 8 个（测试基建）+ `jsonl/legacy-v3.ts`（明确不复刻）；完整台账见根 `todos.md`：

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
- session：`SessionState`（内存）+ `InMemory` 与 `JSONL` 两种后端 + `reducer`（单写者记录协议的状态归约）+ 分支查询（`find_entries_on_branch`）+ lane 视图（`Session.view`）+ 仓库 `fork`。
- compaction：token 估计、切点查找、branch summary。
- skills / prompt-templates / system-prompt / telemetry / events / agent-harness。

## 原理

1. **双层循环**（`agent-loop.rs` 的 `run_loop`）：内层循环处理「工具调用 + steering 消息」，外层循环处理「agent 本应停止时的 follow-up 消息」。每一轮：`prepareNextTurn`（可做 compaction）→ 注入 pending 消息 → `streamAssistantResponse`（transformContext → convertToLlm → streamFn）→ 执行 tool calls → `turn_end` → 检查 `shouldStopAfterTurn` → 拉取新 steering 消息。
2. **工具执行管线**：`prepare`（参数校验 + `beforeToolCall` 钩子）→ `execute`（工具 `execute`，`onUpdate` 流式进度）→ `finalize`（`afterToolCall` 钩子字段级覆盖）。支持 `sequential`/`parallel` 两种模式，`terminate` 标志实现批次提前终止。
3. **事件系统**：`AgentEvent` 联合类型覆盖 agent/turn/message/tool 四层生命周期；`subscribe` 的 listener 是 run settlement 的一部分（`agent_end` 后 listener 完成才算 idle）。
4. **消息队列**：`steer()`（当前 turn 后注入）与 `followUp()`（agent 停止后注入），`one-at-a-time`/`all` 两种 drain 模式。
5. **消息边界**：内部 `AgentMessage`（含自定义消息）在 LLM 调用边界通过 `convert_to_llm` 转成 `Message`，`transform_context` 做上下文窗口管理。
6. **session 单写者记录协议**：`Entry`（消息/配置变更/compaction 等）与 `LaneRecord`（操作/工具/队列记录）按 seq 严格递增 append；`reducer.rs` 从恢复切片重建 lane 状态并校验一致性（`validate_record_log`）。
7. **compaction**：`estimate_tokens`（字符启发式）、`should_compact`（阈值判断）、`find_cut_point`（保持近期 token 预算的切点）、`generate_summary`（调 LLM 生成/更新 summary，含重试）与 `generate_branch_summary`（branch 摘要）。

## 复刻过程中的变化

1. **`CustomAgentMessages` → enum**：TS 用 declaration merging 扩展 `AgentMessage`，Rust 改为 `AgentMessage` enum（User/Assistant/ToolResult + 4 种自定义消息），`role()` 返回判别字符串。
2. **`AsyncIterable` → `Vec`**：`SessionSearch.search`、`scanningEntries` 返回 `Vec`（Rust 无内置异步生成器，流式可后续用 `Stream` 补）。
3. **compaction LLM 调用已补齐**：`complete_simple_with_retries`（有界重试）、`generate_summary`/`generate_summary_with_usage`、`prepare_compaction`、`generate_branch_summary`/`collect_entries_for_branch_summary` 均已实现，走 `models.complete_simple` 调 LLM。
4. **bash 流式 `onUpdate` 已补**：`Shell.exec` 逐行读取 stdout，经 100ms 节流后通过 `on_update` 回调流式上报。
5. **read 工具图片**：已补 —— 基于字节内容检测 mime + 返回 `ImageContent`（base64）；可选 `imageProcessor` 注入（对齐原版 `ReadToolOptions`）。
6. **`jsonl::list`**：已补 —— 目录扫描枚举所有 session 的 header 元数据。
7. **conformance 测试未复刻**：`harness/session/testing/conformance/*`（1700+ 行 storage 契约测试）未复刻（`create_session_backend_conformance` 返回空列表）；此为 B 类测试基建，见根 `todos.md`。
8. **`AgentHarness` 错误的占位 struct 已删除**：早期遗留的 `AgentHarness` struct（含 `prompt`/`steer`/`followUp`/`compact`/`resume` 五个 `unavailable()` 占位方法）不对应原版接口且无引用，已删除；运行时是 `runtime/harness.rs` 的 `Harness` 类 + `runtime/lane.rs` 的 `LaneImpl`。
9. **TypeBox → JSON 值**：`AgentTool` 参数用 `serde_json::Value`；`prepare_arguments` 回调已支持（execution/tools.rs 中应用）。
10. **`watch` 快照订阅已对齐**：`HarnessEventBus`/`BufferedEventWatcher` 完整实现（epoch / resnapshot boundary / handler_error 隔离），`AgentLane::watch` 返回 `WatchHandle<LaneSnapshot>`；`run_when_idle` 亦已对齐原版 `AgentLane` 接口。
11. **`values` 地址强类型化**：`branch_tip`/`lane_config`/`operation_result` 等地址函数返回 `Value<具体类型>`（对齐原版 `values.ts`）；`getValue<T>` 因 Rust `dyn` trait 不支持泛型方法无法复刻，读路径以 `.erased()` 显式擦除类型。
12. **fork 重构（对齐上游 2026-09 fork 系列 commit）**：`fork-policy.rs` 由旧 `ForkScope`/`ForkDisposition`/`classify_fork_address` 改为 `ForkCurrentStatePlan`（Branch{ branch, destination_tip }/Tree）+ `select_branch_fork`/`project_fork_current_state_write`；内存后端 `create_fork`（InMemoryStorageState.select_fork_plan）与 JSONL 后端两阶段流式 `run_jsonl_fork`（新增 `jsonl/fork.rs` + `jsonl/io.rs`，先索引后投影，`publish_jsonl` 原子发布）均对齐原版；`legacy-v3` 迁移未复刻（fork 开放/关闭 v3 源与 `JsonlStorage.open` v3 分支均显式返回 Err）。retry 侧同步 `RetryPolicy.max_agent_delay_ms` 上限（`retry_delay_ms` 指数退避 cap）。
13. **system 消息（对齐 v0.99.2）**：`AgentMessage` 新增 `System` 变体；`convert_to_llm` 对 system 与其他可转换消息一样**原样透传**（对齐上游 `case "system"`）。压缩侧对齐上游的「未列 system」语义：`estimate_tokens` 对 system 返回 0（不参与压缩预算）、`is_valid_cut_point` 不把 system 作为切点、`serialize_conversation` 不把 system 纳入摘要文本。
14. **工具装载变化改由 system 消息承载（对齐 v0.99.2）**：`agent-loop.rs` 新增 `declare_tool_changes`（对应上游 `declareToolChanges`）——`context.tools` 是运行时**可执行**工具集，transcript 的 system 消息声明模型**可用**工具；每轮请求前两者之差写成 system 消息上的 `toolsAdded`/`toolsRemoved`，保证「重放 transcript 后恰好等于可执行集」的不变式。另有 `with_tool_changes`（复制并替换工具字段，空列表省略）、`declared_tools`（从 AgentMessage 列表抽 system 消息后用 `get_current_tools` 求当前声明）、`executable_tools`（`AgentTool` → LLM 层 `Tool`）。
    - `fold_initial_system_message`：把 `context.system_prompt` + `tools` 折叠成**首条 system 消息**（对应上游在 agent 层表达的 `createInitialSystemMessage` + `normalizeContext`），折叠后 `system_prompt` 置空，避免 provider 侧重复生成首条 system 消息。接入点：`run_agent_loop` 入口（折叠）与 `run_loop` 每轮 pending 注入前（声明）。
    - `runtime/drive/tool-placement.rs`：**移除** `addedToolNames` → `activeToolNames` 的自动增量写回与对应 `ConfigUpdate::ActiveTools` 事件（对齐上游：工具激活改为显式——由 `setActiveTools` 或调用方更新工具集；装载变化由 transcript 承载）。同时删除了不再使用的 `next_config_contains` 与该 lane patch 中的 configuration 改写。
15. **循环钩子 Breaking（对齐 v0.99.2）**：删除 `shouldStopAfterTurn`，改为 `finishTurn`（返回 `AgentTurnDecision::End | Continue`）——在 assistant 与全部工具结果 finalize 后、`turn_end` **之前**运行，决策在 `turn_end` **之后**应用（`End` 结束正常 run 且不动 steering/follow-up 队列；`Continue` 确保再进行一次 provider 请求；error/aborted 仍为硬退出，不会走到该回调）。新增 `prepareRequest`（每次 provider 请求前运行，含首次；可替换 context/model/thinkingLevel，且不轮询队列）与 `Agent.peekQueuedMessages()`（预览下一轮队列消息，steering 优先、为空则 follow-up）。`ShouldStopAfterTurnContext` 更名为 `AgentTurnContext`（旧名保留为类型别名）；`AgentRequestUpdate` 的 `thinking_level` 用 `Option<ThinkingLevel>`（`None` = 不更新，可表达 `off`），`PrepareRequestContext.thinking_level` 与 `stream.reasoning` 一致。
16. **小项同步（对齐 v0.99.2）**：`AssistantMessage` 新增 `thinking_level`（agent-loop 在流结果上填入 `config.stream.reasoning`，对应上游 `Object.assign(result, { thinkingLevel })`）；图片检测改为 `GIF87a` / `GIF89a`（避免以 `GIF` 开头的文本文件被误判）；`provider-retry` 对非有限的 `Retry-After` 值回落到指数退避。**未同步**：`onProviderStreamEvent`（需穿透 provider 流层）、overflow 的 Z.AI CN 检测（本项目未用该 provider）。
17. **与上游的对齐状态**：工具执行层与会话层已对照上游逐项核实，并修复 9 处不匹配；
    其中 `isError` 曾是**真实缺陷**（工具抛错被上报为成功），已由 `ExecutedToolCallOutcome` 贯通修复。
    尚未对齐的项（工具入参类型、telemetry span 类型层、session 具名错误等）见根 `todos.md` 的 C 类。

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
