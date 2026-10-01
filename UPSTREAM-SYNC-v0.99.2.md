# 复刻方案：对齐上游 tag v0.99.2

> 目标：把 pi_rs 从基线 `f3564a1d`（= `v0.85.1-35-gf3564a1d4`）推进到 **`v0.99.2`**（2026-09-30，最新稳定 tag）
> 范围：`packages/{agent,ai,telemetry}`；忽略 `coding-agent`/`tui`/`protocol` 与实验性 `pico3`
> 距离：**312 个 commit / 12 个 tag**；主代码（排除 pico3）agent 侧 9 文件 +383/-174
> 预计工作量：**8–10 人日（约 1.5–2 周）**

---

## 0. 一页总览

| 阶段 | 内容 | 主要落点 | 工作量 |
|---|---|---|---|
| P1 | **消息模型扩展**：新增 `SystemMessage` | `pi-ai/types.rs`、`pi-agent/types.rs` | 1–1.5 天 |
| P2 | **transcript 工具层**：14 个函数 + `normalizeContext` | 新增 `pi-ai/utils/transcript.rs` | 1–1.5 天 |
| P3 | **工具激活机制迁移**（替换现有 deferred 增量） | `drive/tool-placement.rs`、`openai-responses.rs`、`agent-loop.rs` | 1.5–2 天 |
| P4 | **循环钩子 Breaking**：`finishTurn` / `prepareRequest` / `peekQueuedMessages` | `types.rs`、`agent-loop.rs`、`agent.rs` | 2 天 |
| P5 | 小项：`thinkingLevel`、`onProviderStreamEvent`、image/retry/overflow 修复 | 多处 | 1 天 |
| P6 | telemetry（2 文件 +17/-3） | `pi-telemetry` | 0.5 天 |
| P7 | 验证与文档收尾 | 测试、`UPSTREAM.md`、`AGENT.md` | 1 天 |

**依赖顺序**：P1 → P2 → P3 → P4 → P5/P6 → P7（P1/P2 是地基，P3 与 P4 都依赖它们）。

---

## P1 — 消息模型扩展（新增 System 消息）

### 上游做了什么

`packages/ai/src/types.ts`：
```ts
export type Message = SystemMessage | UserMessage | AssistantMessage | ToolResultMessage;

export interface SystemMessage {
  role: "system";
  content: string | TextContent[];   // 首条=基础 prompt；后续=追加指令
  sections?: Record<string, string | null>;  // 命名分节，后续按名替换，null 删除
  toolsAdded?: Tool[];               // 此刻变为可用的工具（完整定义）
  toolsRemoved?: ToolReference[];    // 此刻失效的工具
  timestamp: number;
}
```

`packages/agent/src/types.ts`：`AgentMessage = Message | CustomAgentMessages[...]` —— 因此 **agent 层也自动获得 system 消息**。

### pi-rs 现状

- `pi_ai::Message`：`User | Assistant | ToolResult`（3 变体，无 system）
- `AgentMessage`：`User | Assistant | ToolResult | BashExecution | Custom | BranchSummary | CompactionSummary`（7 变体）

### 复刻步骤

1. `pi-ai/src/types.rs`
   - 新增 `SystemMessage { content, sections, tools_added, tools_removed, timestamp }`（serde camelCase，字段全部 optional 除 content/timestamp）
   - 新增 `ToolReference { name }`
   - `Message` 增加 `System(SystemMessage)` 变体，`role()` 返回 `"system"`
2. `pi-agent/src/types.rs`：`AgentMessage` 增加 `System(pi_ai::SystemMessage)` 变体，`role()` 同步
3. **修掉所有 `match Message` / `match AgentMessage` 的编译错误**（这是本阶段主要工作量）。预计受影响位置：
   - `pi-ai`：`transform-messages.rs`（跨 provider 规范化）、`estimate.rs`（token 估算）、`api/openai-responses.rs` / `openai-completions.rs`（消息→请求体）、`utils/text.rs`（contentText）
   - `pi-agent`：`harness/messages.rs`（convert_to_llm）、`compaction/*`、`runtime/*`、`session/*`（序列化）
4. **向后兼容**：旧的 JSONL 会话文件不含 system 消息 → serde 需容忍缺省（`#[serde(default)]`），反序列化路径不得因缺字段失败

### 验证

- `cargo check --workspace` 无遗漏分支
- 用旧会话文件回放，确认仍可加载（兼容性）
- 新增最小单测：`Message::System` 的 serde 往返

---

## P2 — transcript 工具层

### 上游做了什么

新增 `packages/ai/src/utils/transcript.ts`（234 行），导出 14+ 个函数，把「system prompt + 工具声明」变成可从 transcript 推导的派生值：

- `createInitialSystemMessage(systemPrompt, tools)` —— 构造首条 system message
- `normalizeContext(context)` —— 把 `Context.systemPrompt`/`tools` 简写折叠成首条 system message
- `getInitialSystemMessage` / `withoutInitialSystemMessage`
- `getCurrentTools` / `getCurrentSystemMessage` / `getCurrentSystemPrompt`
- `collapseSystemMessages` / `resolveTranscript`
- `toToolDeclaration` / `declarationsEqual`
- `getToolStateChanges(previous, current)` → `{ toolsAdded, toolsRemoved }`
- `getDeclaredTools` / `hasToolRedefinitions` / `hasNonAdditiveToolChanges`
- `resolveTranscriptTools(messages, supportsToolAdditions)` → `{ immediate, deferred }`

同时 `Context` 语义变化：`systemPrompt`/`tools` 成为**简写**，各 stream 入口先 `normalizeContext()`。

### 复刻步骤

1. ✅ 新增 `crates/pi-ai/src/utils/transcript.rs`，逐函数移植（17 个导出 + `TranscriptContext`/`ToolStateChanges`/`TranscriptTools`）
2. ⏭ `Context` 保持现有字段（`system_prompt` / `tools`）；**stream 入口的 normalize 与 provider 侧消费推迟到 P3**（与工具激活迁移一起做，避免两套 system 逻辑并存）
3. ✅ 补单测（`getToolStateChanges` / `resolveTranscriptTools` / `collapseSystemMessages` / `getCurrentSystemMessage` / `hasNonAdditiveToolChanges` 等，共 10 例）
4. ✅ `SystemMessage.sections` 改为 `IndexMap`（保插入序，对齐上游 JS 对象键序）

### 与 pi-rs 现有的对应关系

- 上游已删除的 `utils/deferred-tools.ts` ⇒ pi-rs 现有的是**内联在 `openai-responses.rs` 的 `split_deferred_tools`**，由 `resolveTranscriptTools` 取代（见 P3）

---

## P3 — 工具激活机制迁移（**最需要小心的一步**）

### 上游做了什么

`drive/tool-placement.ts` **移除**了「从 tool result 的 `addedToolNames` 增量写回 `activeToolNames`」的逻辑（-36 行）。原因是：**工具可用性变化现在由 system message 承载**（`toolsAdded` / `toolsRemoved`），不再靠 lane config 的增量更新。

`agent-loop.ts` 新增 `withToolChanges(systemMessage, {toolsAdded, toolsRemoved})`：把工具变化合并进待发的 system message（`agent-loop.ts:368`）。

### pi-rs 现状（**正是上一轮刚同步的机制**）

- `runtime/drive/tool-placement.rs`：有 `added_names` → 写回 `lane_config.active_tool_names`（约 40 行）
- `api/openai-responses.rs`：内联 `split_deferred_tools`（按 `added_tool_names` 分流 immediate/deferred）
- `lane.rs` 的 `set_active_tools` / `get_active_tools`、`generation.rs` 的 tools 构造（按 `active_tool_names`）

### 复刻步骤

1. `drive/tool-placement.rs`：删除 `added_names` 收集与 `active_tool_names` 写回；保留 `added_tool_names` 在消息上的记录（仍需用于 transcript 推导）
2. `agent-loop.rs`：新增 `with_tool_changes`，在构造待发 system message 时合并工具增删
3. `openai-responses.rs`：`split_deferred_tools` 改用 `transcript::resolve_transcript_tools(messages, supports_tool_additions)`
4. **决策点**：`lane.configuration.active_tool_names` 是否保留？
   - 保留（推荐）：仍作为"当前激活集"的运行时投影，但**写入来源**从 tool-placement 改为 system message 推导
   - 移除：改动更大，且会牵动 `setActiveTools` 公共 API
5. 检查 `harness/runtime/drive/generation.rs` 的 tools 构造是否要改为「从 transcript 推导」

### 风险

**这是"替换掉刚同步的机制"**，必须一次性改到位，不能只做一半（否则工具激活会出现两条真相来源）。建议本阶段单独提交，便于回滚。

---

## P4 — 循环钩子 Breaking

### 上游做了什么（0.86.0 + 0.87.0）

| 变更 | 语义 |
|---|---|
| ❌ 移除 `shouldStopAfterTurn` | —— |
| ✅ 新增 `finishTurn(turn, signal)` | 在 assistant + 所有工具结果 finalize 后、`turn_end` **之前**运行；返回 `{action:"end"}` 结束 run（**不动 steering/follow-up 队列，跳过 prepareNextTurn**）、`{action:"continue"}` 确保再发一次请求、`undefined` 保持正常调度。**error/aborted 响应仍为硬退出**（其决策被忽略） |
| ✅ 新增 `prepareRequest(request, signal)` | **每次** provider 请求前运行（含首次）；此时 pending 消息已 append 并 emit；返回的 context/model/thinkingLevel 替换运行时值；**不轮询队列** |
| ✅ 新增 `Agent.peekQueuedMessages()` | 预览下一批队列消息（不消费） |
| 🔄 `prepareNextTurn*` 语义收窄 | 仅在确定要开始下一个 assistant turn 时运行（不再在 final/terminating turn 之后运行）；end-of-run 工作应挪到 `agent_end` |

`agent-loop.ts` 中的调用点：`prepareRequest`（:219）、`finishTurn`（:252 与 :286，两个分支）。

### pi-rs 现状

- `AgentLoopConfig { should_stop_after_turn, prepare_next_turn }`（`types.rs:388-389`）
- `agent-loop.rs:211`（prepare_next_turn）、`:322`（should_stop_after_turn）

### 复刻步骤

1. `pi-agent/src/types.rs`
   - 删除 `ShouldStopAfterTurnFn`，新增 `FinishTurnFn`（返回 `AgentTurnDecision`）、`PrepareRequestFn`
   - 新增 `AgentTurnDecision { End | Continue }`、`AgentRequestUpdate { context?, model?, thinking_level? }`、`PrepareRequestContext`
2. `pi-agent/src/agent-loop.rs`
   - 在 assistant + 工具结果 finalize 后、`turn_end` 前插入 `finish_turn` 调用（对齐上游两处分支）
   - 每次 provider 请求前插入 `prepare_request`（含首次）
   - `prepare_next_turn` 收窄为"仅在确定继续时运行"
3. `pi-agent/src/agent.rs`：新增 `peek_queued_messages()`；透传两个新钩子
4. **迁移指南**：在 `crates/pi-agent/AGENT.md` 的「复刻过程中的变化」记录 Breaking（旧 `shouldStopAfterTurn` → 新 `finishTurn`），因为这是公共 API 破坏性变更

### 验证

对照上游 `test/agent-loop.test.ts` 中 finishTurn/prepareRequest 的用例，移植为 Rust 测试（至少覆盖：end 决策、continue 决策、error 响应下决策被忽略、prepareRequest 在首次请求前运行）

---

## P5 — 小项

| 项 | 上游位置 | pi-rs 落点 | 说明 |
|---|---|---|---|
| `AssistantMessage.thinkingLevel` | ai types.ts / agent-loop | `pi-ai/types.rs` + `agent-loop.rs` | 记录本次请求的 thinking level |
| `onProviderStreamEvent` | agent option（0.99.0） | `pi-agent/types.rs` + provider stream 调用处 | 在**归一化前**观察 provider 事件 |
| image 误判修复 | `harness/tools/image.ts` | `harness/tools/image.rs` | 以 `GIF` 开头的**文本**文件被误判为图片 |
| `Retry-After` 不可解析 | `utils/provider-retry.ts` | `pi-ai/utils/provider-retry.rs` | 改用指数退避 |
| Z.AI CN 溢出检测 | `utils/overflow.ts` | `pi-ai/utils/overflow.rs` | 可选（若不需要 Z.AI 可跳过） |
| `utils/retry.ts` +11 行 | ai | `pi-ai/utils/retry.rs` | 需比对具体内容后决定 |
| `utils/headers.ts` / `text.ts` / `estimate.ts` 改动 | ai | 对应文件 | 逐一比对，按需同步 |

---

## P6 — telemetry

`packages/telemetry` 仅 2 文件 +17/-3，按 diff 直接同步到 `pi-telemetry`。

---

## P7 — 验证与收尾

1. `cargo test --workspace` / `cargo clippy --all-targets` / `cargo fmt --check` 全绿
2. 手工验证（`demo-agent`）：
   - 正常对话仍工作
   - **system message 恢复**：中途插入的 system 消息在 resume 后仍在
   - **分支导航**：切分支后 system/工具状态正确还原
   - **动态工具加载**：装载后工具可用，且 transcript 里能看到 `toolsAdded`
3. 文档更新：
   - `UPSTREAM.md`：基线推进到 `v0.99.2`，清空待同步清单
   - `crates/pi-agent/AGENT.md` / `pi-ai/AGENT.md`：记录 Breaking（finishTurn、System 消息）与新增（transcript 层）
   - `README.md`：若引用了版本/文件数，同步更新

---

## 范围外（明确不做）

- **ai 图像模型统一**（`ImagesModels` 移除、`ImageModel`、schema v6）—— pi-rs 只保留 openai/deepseek/faux，图像模型体系不在子集内
- **classifier 模型与 `Models.classify()`**
- **模型目录新增**（Claude Sonnet 5.5 / GPT-6 Sol/Luna / Grok 4.7 / GPT-6.1 Sol 等）
- **OAuth 页面迁移**（`auth/oauth/oauth-page.ts` → `utils/oauth-page.ts`）、Sign in with ChatGPT
- **pico3**（8,074 行实验性内核）—— 待其稳定、进入包根导出后再评估

---

## 风险登记

1. **消息模型变更触及面广**：新增 `System` 变体会让所有 `match Message` 报错；这是"编译期强制检查"，反而是好事，但需要逐个确认语义（尤其 `transform-messages` 与 provider 请求构造）
2. **P3 是机制替换**：与上一轮刚同步的 deferred 增量激活冲突，必须一次改到位
3. **旧会话兼容**：JSONL 中没有 system 消息，反序列化需容忍
4. **上游测试不可直接移植**（TS→Rust），需自行设计等价用例
5. **`prepareNextTurn` 语义收窄**可能让现有调用方的 end-of-run 逻辑失效 → 迁移到 `agent_end`（需在 AGENT.md 中写明）
6. **上游仍在快速演进**（tag 频率接近每天）：本方案对齐 `v0.99.2` 后，建议按 `UPSTREAM.md` 的触发条件批量跟进，而不是持续追 main

---

## 提交策略（建议）

按阶段分提交，便于回滚与审阅：

1. `feat(pi-ai): add SystemMessage to message model`（P1）
2. `feat(pi-ai): port transcript utilities`（P2）
3. `refactor(pi-agent): move tool activation from lane config to transcript system messages`（P3）
4. `feat(pi-agent): replace shouldStopAfterTurn with finishTurn, add prepareRequest`（P4）
5. `fix(pi-agent,pi-ai): sync minor upstream fixes (thinkingLevel, image, retry, overflow)`（P5）
6. `chore(pi-telemetry): sync upstream telemetry changes`（P6）
7. `docs: record sync baseline v0.99.2`（P7）
