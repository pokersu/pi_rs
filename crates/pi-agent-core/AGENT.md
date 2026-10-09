# pi-agent-core

agent 核心运行时：`Agent` 类、双层 agent loop、proxy stream 与共享类型。

## 复刻来源

1:1 复刻自 [earendil-works/pi](https://github.com/earendil-works/pi) 的 `packages/agent`（v1.1.0，
包名 `@earendil-works/pi-agent-core`），**6 个文件**：

| 上游 | Rust |
|---|---|
| `agent.ts` | `src/agent.rs` |
| `agent-loop.ts` | `src/agent-loop.rs` |
| `proxy.ts` | `src/proxy.rs` |
| `stream-fn.ts` | `src/stream-fn.rs` |
| `types.ts` | `src/types.rs` |
| `index.ts` | `src/lib.rs` |

## ⚠️ 与 v0.99.2 的差别（重要）

上游 **v1.0.0** 把整个 harness 从本包移除，只保留 agent 核心：

> Removed the experimental harness from `@earendil-works/pi-agent-core`: `AgentHarness`, sessions and
> session storage, the durable runtime, pico3, harness tools, compaction, skills, prompt templates,
> system prompt helpers, telemetry schemas… The package now contains only `Agent`, the agent loop,
> the proxy stream, and their types.

因此本 crate 现仅 6 个文件，原先的 `harness/`、`search/`、`node.rs` 已删除（见 git 历史）。
那部分能力现在由 **`crates/pi-durable`**（上游 `packages/durable`，全新 document/task-graph 架构）
承载，重建计划见根目录 `UPSTREAM-SYNC-v1.1.0.md`。

`demo-agent` 曾依赖旧 harness，已暂时移出 workspace。

## 功能介绍

- **`Agent`** —— 有状态封装：transcript、事件订阅（`subscribe` 返回退订闭包）、
  steering / follow-up 消息队列（含 `steeringMode`/`followUpMode` 运行期访问器）、生命周期
  （abort / waitForIdle / reset）；`prompt` 支持文本/消息/文本+图片（`prompt_text_with_images`）。
- **agent loop** —— 无状态双层循环：`run_agent_loop` / `run_agent_loop_continue` + 内层工具执行。
  外层管 follow-up，内层管 tool call + steering；`finishTurn` 决策在 `turn_end` 之后应用。
- **流式观测钩子** —— `onPayload` / `onResponse` / `onProviderStreamEvent`（穿透到 pi_ai 的
  provider 请求/响应/事件解析点）。
- **工具执行管线** —— `prepareArguments` → schema 校验 → `beforeToolCall` → 执行 → `afterToolCall`，
  `isError` / `structuredContent` 全程贯通；公开入口 `run_tool_call`。
- **proxy** —— 远程代理流（通过 server 转发 LLM 调用，重建精简事件）。
- **类型** —— `AgentMessage`、`AgentContext`、`AgentTool`、`AgentEvent`、`AgentLoopConfig` 等。

## 构造自定义消息

`AgentMessage` 额外带有 4 种自定义消息变体（`BashExecution` / `Custom` / `BranchSummary` /
`CompactionSummary`），对应用户在应用层扩展的消息类型。

`default_convert_to_llm` 对应上游 `defaultConvertToLlm`：**只保留** LLM 原生消息
（system / user / assistant / toolResult），自定义消息在发往 provider 前被过滤掉。需要保留它们时，
由调用方通过 `AgentLoopConfig.convert_to_llm` 提供自己的转换。

> 上游 `defaultConvertToLlm` 是模块私有函数；这里设为 `pub` 以便集成测试与调用方复用。
