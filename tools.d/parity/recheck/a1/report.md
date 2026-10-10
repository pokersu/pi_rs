# agent 模块 1:1 复刻重审报告（recheck/a1）

> 基线：upstream `v1.1.0` @ `abe508e1b`（`upstream/packages/agent/src/`）
> Rust：`crates/pi-agent-core/src/`（agent.rs / agent-loop.rs / proxy.rs / stream-fn.rs / types.rs / lib.rs）
> 方法：逐文件逐符号正向（TS→Rust 查缺失）+ 反向（Rust→TS 查多余）核对，不采信已有 `tools.d/parity/AUDIT-REPORT.md` 结论（其明细表为修复前旧状态，本报告以当前代码为准）。
> 交叉依据：`crates/pi-ai/src/`（types.rs、utils/transcript.rs、utils/event-stream.rs、utils/json-parse.rs、utils/error-stream.rs、utils/validation.rs）与 `upstream/packages/ai/src/`（types.ts、utils/transcript.ts、utils/event-stream.ts、utils/json-parse.ts）。
> 方法备注：`read` 工具对 token/key/auth 模式行做值遮蔽，已用 `grep_files` 恢复全部被遮蔽值（如 `totalTokens: 0`、`maxTokens: 0`、`getApiKey?: (provider: string) => Promise<string | undefined> | string | undefined`）。

## 结论

**非 1:1。** 缺失 8、多余 7、逻辑差异 21（其中 14 项有可观察行为影响，7 项为低危边界差异）。`stream-fn.ts→stream-fn.rs` 与 `index.ts→lib.rs` 的导出布局基本一致；主要偏差集中在 agent-loop 主循环与 proxy 流解析。

---

## 一、缺失（8）

### 1. `AgentLoopTurnUpdate.messages` 字段 + runLoop 的 preparedMessages 处理
- TS 符号：`AgentLoopTurnUpdate.messages?: AgentMessage[]`（types.ts）+ `runLoop` 中 `const nextTurnSnapshot = await config.prepareNextTurn?.(lastCompletedTurn); preparedMessages = nextTurnSnapshot.messages ?? []`（agent-loop.ts:196-201）
- Rust 文件：types.rs（`AgentLoopTurnUpdate { context, model, thinking_level }`）、agent-loop.rs（run_loop 无 preparedMessages）
- 说明：Rust `AgentLoopTurnUpdate` 没有 `messages` 字段，`run_loop` 也从未处理 prepareNextTurn 返回的追加消息。TS 中 prepareNextTurn 可以返回要追加的消息（带生命周期事件）；Rust 完全丢失该能力。`AgentRequestUpdate`（Omit messages）反而两边一致，说明 messages 被无意删掉。

### 2. runLoop 起始 steering 轮询
- TS 符号：`runLoop` 首行 `let pendingMessages: AgentMessage[] = (await config.getSteeringMessages?.()) || [];`（agent-loop.ts:192）
- Rust 文件：agent-loop.rs（`run_loop` 起始 `let mut pending_messages = Vec::new();`）
- 说明：TS 在进入外层循环前先轮询一次 steering 队列——agent 空闲期间 `steer()` 的消息会注入**第一个 turn**（首个 LLM 请求之前）。Rust 无此初始轮询，同样的消息只能等第一个 turn 结束后（turn_end 之后）才被取走。prompt() 前的 steering 消息整体延迟一个 turn。

### 3. `skipInitialSteeringPoll` 机制
- TS 符号：`Agent.continue()` 调 `this.runPromptMessages(queuedSteering, { skipInitialSteeringPoll: true })` + `createLoopConfig(options)` 中 `let skipInitialSteeringPoll = options.skipInitialSteeringPoll === true;` 及 `getSteeringMessages` 闭包首轮返回 `[]`（agent.ts:383-385、477-480、492-498）
- Rust 文件：agent.rs（`run_prompt_messages(&self, messages)` 无 options 参数；`make_queue_drain` 无 skip 逻辑）
- 说明：TS 语义：continue() 手动 drain 出的 steering 消息作为 prompt 传入后，循环内**首次** steering 轮询必须跳过（避免 setup 窗口内新入队的消息被塞进同一 turn）。Rust 无此标志。虽然 Rust 因缺失项 2（无初始轮询）在 continue 路径的常规时序下碰巧等效，但 continue 路径中 `drain()` 之后、首个 turn 结束之前新 `steer()` 的消息，TS 会等到下一 turn，Rust 会在该 turn 的 turn_end 轮询后于下一 turn 注入——时点不同，且 TS 的一等公民语义（skip 标志）整体缺失。

### 4. error/aborted turn 上的 `finishTurn` 调用
- TS 符号：`runLoop` 中 `if (message.stopReason === "error" || message.stopReason === "aborted") { lastCompletedTurn = {...}; await config.finishTurn?.(lastCompletedTurn, signal); await emit({type:"turn_end",...}); ... }`（agent-loop.ts:243-253）
- Rust 文件：agent-loop.rs（`if message.stop_reason == Error || Aborted { emit TurnEnd; emit AgentEnd; return; }`）
- 说明：TS 在 error/aborted 硬退出前仍调用 `finishTurn`（文档也写明 error/aborted 是硬退出但 hook 会先跑）；Rust 直接发 turn_end/agent_end，`finish_turn` 未调用。

### 5. `AgentEvent.tool_execution_end.durationMs` 字段
- TS 符号：`AgentEvent` 的 `tool_execution_end` 分支含 `durationMs?: number`（types.ts:519-526）
- Rust 文件：types.rs（`ToolExecutionEnd { tool_call_id, tool_name, result, is_error }`）
- 说明：Rust 事件无 `duration_ms` 字段；`AgentToolCallOutcome.duration_ms` 虽然存在且 `emit_tool_execution_end` 可用它，但事件结构里丢弃了。订阅者无法拿到工具耗时。

### 6. `AgentTool.outputSchema`
- TS 符号：`AgentTool.outputSchema?: TSchema`（types.ts:455-459）
- Rust 文件：types.rs（`AgentTool { label, tool, execute, prepare_arguments, execution_mode, replay }`）
- 说明：字段整体未移植。v1.1.0 中该字段的运行时消费者在 packages/coding-agent（范围外），agent 包内无消费点，故为低危类型面缺失；但声明 `outputSchema` 的工具在 Rust 侧无法表达。

### 7. prepareNextTurn / prepareNextTurnWithContext 的 AbortSignal 与签名
- TS 符号：`AgentOptions.prepareNextTurn?: (signal?: AbortSignal) => ...`、`AgentOptions.prepareNextTurnWithContext?: (context, signal?) => ...`；`createLoopConfig` 包装器把 `this.signal` 传给两者（agent.ts:135-143、487-492）
- Rust 文件：agent.rs（`AgentOptions.prepare_next_turn: Option<PrepareNextTurnFn>`，两者同型）、types.rs（`PrepareNextTurnFn = Fn(&PrepareNextTurnContext) -> ...`）
- 说明：① Rust 两个回调都不接收 AbortSignal（TS 的 Agent 层明确传 `this.signal`）；② TS 的 `prepareNextTurn`（无 context 版）只收 signal，Rust 版却收 `&PrepareNextTurnContext`——参数含义完全错位。loop 层 `config.prepareNextTurn(context)` 本身两边一致。

### 8. runAgentLoop 初始 prompts 的工具变更声明
- TS 符号：`runAgentLoop` 中 `const initialMessages = declareToolChanges(context, prompts);`，并以 initialMessages（可能插入 system 工具声明消息）发事件并入上下文（agent-loop.ts:115-131）；且 runLoop 内每轮对 `[...preparedMessages, ...pendingMessages]` 无条件调用 declareToolChanges（即使为空数组，仍可能插入 system 声明）（agent-loop.ts:207-215）
- Rust 文件：agent-loop.rs（`run_agent_loop` 直接 emit 原始 prompts，不调用 declare_tool_changes；`run_loop` 仅在 `!pending_messages.is_empty()` 时才声明）
- 说明：场景：Agent 构造后、prompt() 前调用 `set_tools()` 修改工具集（或用户消息本身携带 system 消息的恢复会话）：TS 第一个 turn 会先 emit 一条 toolsAdded/toolsRemoved system 消息再发用户消息；Rust 直接发用户消息，模型不知道工具集变了。同理 runLoop 中 prepareRequest/prepareNextTurn 替换了带不同 tools 的 context 时，TS 在无 pending 消息的情况下也会声明差异，Rust 不会。

---

## 二、多余（7）

### 1. `default_convert_to_llm` 公开并再导出
- Rust：agent.rs（`pub fn default_convert_to_llm`）+ lib.rs（`pub use agent::default_convert_to_llm`）
- TS：agent.ts 中 `function defaultConvertToLlm` 为模块私有，无任何导出。
- 说明：Rust 头注释自认「上游该函数为模块私有；这里设为 pub 以便集成测试」。过滤语义本身（role ∈ system/user/assistant/toolResult，即非此四类丢弃）与 TS 逐条一致。

### 2. `AgentMessage` 额外 4 个 variant 及配套类型
- Rust：types.rs 的 `BashExecutionMessage`、`CustomMessage`、`CustomMessageContent`、`BranchSummaryMessage`、`CompactionSummaryMessage` 及 `AgentMessage::{BashExecution, Custom, BranchSummary, CompactionSummary}`
- TS：v1.1.0 `Message = SystemMessage | UserMessage | AssistantMessage | ToolResultMessage`（packages/ai/src/types.ts:626），`CustomAgentMessages` 默认为空接口 → `AgentMessage` 实际只有 4 种角色。
- 说明：这些是 v1.0.0 移除 harness 前的历史消息类型，上游 v1.1.0 已不存在。序列化面（serde tag="role"）与 TS 不同，任何下游若依赖 `AgentMessage` 的穷尽性都会多出 4 个分支。

### 3. `AgentToolResult.added_tool_names`
- Rust：types.rs `AgentToolResult.added_tool_names: Option<Vec<String>>`，并在 agent-loop.rs `create_tool_result_message` 中透传到 `ToolResultMessage.added_tool_names`
- TS：`AgentToolResult`（types.ts）无 `addedToolNames` 字段；全上游 packages/ai、packages/agent 均无 `addedToolNames` 符号（grep 0 命中）。
- 说明：纯 Rust 扩展。TS 工具无法携带该字段，Rust 工具可以，且会进入会话历史的 toolResult 消息。

### 4. `ReplayPolicy` 公开枚举
- Rust：types.rs `pub enum ReplayPolicy { Never, Safe }`
- TS：`AgentTool.replay?: "never" | "safe"` 为内联字面量联合，无独立导出类型。
- 说明：无害的命名化差异，但属于 Rust 有、TS 无的公开符号。

### 5. `to_ai_thinking_level` 公开函数
- Rust：types.rs `pub fn to_ai_thinking_level(...)`
- TS：无对应符号（"off"→undefined 的映射内联在 `createLoopConfig` 中）。
- 说明：纯辅助函数，加性，无行为差异。

### 6. `AgentContext.system_prompt` 字段
- Rust：types.rs `AgentContext { system_prompt, messages, tools }`
- TS：`AgentContext { messages, tools? }`（types.ts）——无 systemPrompt。
- 说明：这是 Rust 状态表示法改造的载体（见逻辑差异 1），作为公开字段是 TS 没有的符号。

### 7. `ProxyAssistantMessageEvent::Done` 接受 `"deferred"`
- Rust：proxy.rs `Done { reason: TerminalStopReason, ... }`，`TerminalStopReason` 含 `Deferred`，`terminal_to_stop` 映射 deferred
- TS：proxy.ts `type: "done"; reason: Extract<StopReason, "stop" | "length" | "toolUse">`——明确排除 deferred。
- 说明：服务器若发 `reason:"deferred"` 的 done，TS 反序列化后按类型约定不可能出现；Rust 接受并映射为 StopReason::Deferred。加性宽松，低危。

---

## 三、逻辑差异（20）

### 高影响（可观察行为不同）

#### 1. `state.systemPrompt` / `state.messages` / `reset()` 表示法
- TS：`createMutableAgentState` 把 `createInitialSystemMessage(systemPrompt, tools)` **前置进 messages**；`systemPrompt` getter = `getCurrentSystemPrompt(messages)`（重放全部 system 消息，追加 system 消息会实时改变它）；`reset()` 保留 `getCurrentSystemMessage(messages)` 重放基线（agent.ts:83-96、268-283）
- Rust：agent.rs `MutableAgentState.system_prompt` 为构造期字段，永不更新；`state.messages` 不含合成 system 消息（每 run 由 `fold_initial_system_message` 在 context 克隆上临时生成）；`reset()` 直接 `messages.clear()`，system_prompt 原样保留
- 说明：① 用户追加 system 消息后，TS `state.systemPrompt` 变化、Rust 不变；② `reset()` 后 TS 保留重放后的 prompt+工具基线 system 消息，Rust 丢失全部追加的 system 消息与工具声明基线，只剩构造期 prompt 文本；③ `state.messages` 快照内容两边不同（TS 含首条 system 消息）。

#### 2. LLM 上下文构造（tools 入 Context + normalize 前置）
- TS：`streamAssistantResponse` 中 `const llmContext = normalizeContext({ messages: llmMessages })`——只传消息；工具声明只靠 transcript 里的 system 消息，无 tools 字段（agent-loop.ts:391-396）
- Rust：agent-loop.rs 构造 `Context { system_prompt: Some/None, messages: llm_messages, tools: Some(executable) }`；而 `pi_ai::utils::transcript::normalize_context` 在 system_prompt 或 tools 非空时**无条件前置一条新 system 消息**（crates/pi-ai/src/utils/transcript.rs:44-58），且 openai-responses / openai-completions 都在请求前调用它
- 说明：Rust 每次请求 provider 都会额外收到一条前置 system 消息（空 prompt + 完整工具声明），与 transcript 中已有声明重复；对 supports-mid-convo-system-messages 的 provider（openai-responses）payload 出现两条头部 system 消息（TS 只有一条）。continue 路径（run_agent_loop_continue 不 fold）system_prompt 全程非空，每条请求都再前置一条。onPayload 观察者可验证。

#### 3. `thinkingLevel` 在 `message_end` 之后才写入
- TS：`const result = async () => Object.assign(await response.result(), { thinkingLevel: config.reasoning ?? "off" });` —— done/error 与流自然结束两个分支都在**发 message_end 之前**取 result()（含 thinkingLevel），并把含 thinkingLevel 的消息写回 context.messages 与事件（agent-loop.ts:400、435-461）
- Rust：agent-loop.rs 两个分支都是先 `emit(MessageEnd)`、先写回 context.messages，**之后**才 `final_message.thinking_level = config.stream.reasoning` 并返回
- 说明：message_end 事件载荷与 context.messages 中的 assistant 消息在 Rust 中缺 thinking_level；后续 `finishTurn`/`prepareNextTurn` 拿到的 `turn.context.messages` 里该消息没有 thinking_level（TS 有）。返回给 run_loop 的 message 本身两边都有。

#### 4. `finishTurn` "continue" 与 follow-up 的次序
- TS：`explicitContinuation = decision?.action === "continue"`；内层循环退出后**先查 follow-up**，有则 `pendingMessages = followUpMessages; continue;`，没有才履行显式 continuation 的空上下文 turn（agent-loop.ts:288-310）
- Rust：`Some(AgentTurnDecision::Continue) => { has_more_tool_calls = true; }` ——直接再迭代内层循环（agent-loop.rs:472-478）
- 说明：decision=continue 且 follow-up 队列非空时：TS 下一请求直接携带 follow-up 消息（1 次请求）；Rust 先发一次**未变化的上下文**请求，再在后续轮次取 follow-up（多 1 次 provider 请求，且消息注入次序不同）。

#### 5. `tool_execution_update` 延迟刷出、工具 panic 时丢弃
- TS：`executePreparedToolCall` 的 onUpdate 闭包在工具调用期间**实时**调 `onUpdate(partialResult)`（`updateEvents.push(Promise.resolve(onUpdate(partialResult)))`——emit 立即发生），catch 分支也 `await Promise.all(updateEvents)` 刷出（agent-loop.ts:829-862）
- Rust：agent-loop.rs `execute_prepared_tool_call` 把 update 事件收进 Mutex Vec，`execute` 返回后统一冲刷；**panic 分支直接 return，不冲刷**（agent-loop.rs:1140-1201）
- 说明：① TS 订阅者边执行边收到部分结果（实时进度条语义），Rust 全部推迟到工具完成后一次性到达；② TS 工具抛错时已发 update 仍会送达，Rust 工具 panic 时 update 全部丢失。

#### 6. `tool_execution_update.args` 用校验后参数
- TS：`emitToolExecutionUpdate(toolCall, emit)` 用**原始** `toolCall.arguments`（agent-loop.ts:777-786）
- Rust：`execute_prepared_tool_call` 的 `args_owned = args.clone()` 是 `PreparedToolCall.args` = **schema 校验后**的参数（agent-loop.rs:1148-1160）
- 说明：校验/转换过参数的 update 事件载荷与 TS 不同（如类型强制转换后的值、缺省填充等都会被订阅者看到）。

#### 7. `afterToolCall` 与 finalized 结果收到原始 toolCall 而非 prepared
- TS：`finalizeExecutedToolCall(context, assistantMessage, preparation, ...)`——hook 收到 `preparation.toolCall`（经 `prepareArguments` 重写后的），`AgentToolCallOutcome.toolCall` 也是 prepared（agent-loop.ts:851-903）
- Rust：sequential/parallel 路径 `finalize_executed_tool_call(..., &tool_call, ...)` 传的是**循环里的原始** tool_call（agent-loop.rs:816-826、930-940）
- 说明：工具声明了 `prepareArguments` 且改写了 arguments 时，TS 的 afterToolCall 上下文与最终 outcome 的 `toolCall.arguments` 是重写后的；Rust 是模型原始值。`run_tool_call` 路径两边一致（都传 prepared）。

#### 8. `tool_execution_end.result` 只含 details
- TS：`emitToolExecutionEnd` 发 `result: finalized.result`（完整 AgentToolResult：content/details/structuredContent/usage/isError/terminate）（agent-loop.ts:905-914）
- Rust：`emit_tool_execution_end` 发 `result: finalized.result.details.clone()`（agent-loop.rs:1249-1258）
- 说明：订阅者拿到的 result 从「完整工具结果」降级为「details JSON」。类型也由 AgentToolResult 变成 serde_json::Value。

#### 9. apiKey 回退 `|| config.apiKey` 缺失
- TS：`const resolvedApiKey = (config.getApiKey ? await config.getApiKey(config.model.provider) : undefined) || config.apiKey;`（agent-loop.ts:399-401）
- Rust：`let resolved_api_key = match &config.get_api_key { Some(f) => f(&config.model.provider).await, None => None };` 然后 `options.stream.request.api_key = resolved_api_key;`（agent-loop.rs:578-585）
- 说明：TS 中 getApiKey 未配置时回退使用 options.apiKey（保持原值）；Rust 无回退，直接把 `config.stream.request.api_key` **覆盖为 None**。对 Agent 路径无影响（Agent 从不预置 api_key），对直接调用 run_agent_loop 且预置了 stream.request.api_key 的调用方行为改变。

#### 10. proxy 错误/中止时 partial 内容丢失
- TS：catch 分支把**已累积内容**的 `partial` 赋 stopReason/errorMessage 后作为 error 事件发出（proxy.ts:204-216）
- Rust：`proxy_request` 返回 Err 后，`stream_proxy` 用 `partial_message(&model)` **新建空 partial**（content: Vec::new()）再发 error（proxy.rs:322-345）
- 说明：流中途断连/中止时，TS 的 error 事件携带已收到的部分内容（UI 可显示半截输出），Rust 的 error 事件内容是空的。

#### 11. proxy 协议违规 panic → 流永不结束
- TS：`processProxyEvent` 对类型不匹配的 text_delta/text_end/thinking_*/toolcall_delta 抛 Error，被外层 catch 捕获 → 发 error 事件 + `stream.end()`（proxy.ts:230-305、204-216）
- Rust：`process_proxy_event` 对应分支 `panic!`；panics 发生在 `tokio::spawn` 任务里，`proxy_request` 的 Err 分支不执行，**error 事件不发、stream 不 end**（proxy.rs:60-135）
- 说明：服务器发出乱序/类型不符的增量事件时，TS 消费者收到带错误消息的 error 事件后流正常终止；Rust 消费者永久挂起（result() 永远不会 resolve）。

#### 12. proxy `toolcall_end` 非 toolCall 内容仍发事件
- TS：content 不是 toolCall 时 `return undefined;`（不发事件，也不抛错）（proxy.ts:289-300）
- Rust：`if let ContentBlock::ToolCall(tc) = ... { *tc = tool_call.clone(); }` ——不匹配时什么都不做，但**仍然返回 Some(ToolCallEnd 事件)**（proxy.rs:186-197）
- 说明：类型不符的 toolcall_end 在 TS 被静默跳过，在 Rust 会向消费者推送一个指向不存在内容的 toolcall_end 事件。

#### 13. proxy `toolcall_delta` 用序列化参数重建而非原始 partialJson
- TS：`(content as any).partialJson += proxyEvent.delta; content.arguments = parseStreamingJson((content as any).partialJson) || {};`——在**原始 JSON 文本**上累积（proxy.ts:270-284）
- Rust：`let accumulated = tc.arguments.to_string() + delta; tc.arguments = parse_streaming_json(Some(&accumulated));`——在**已解析参数的 serde 再序列化**上累积（proxy.rs:169-181）
- 说明：两者积累的字符串不同（TS 是原始字节流；Rust 是每次 salvage 解析后的重序列化，空白/转义/键序都可能被规范化后再拼 delta）。带空白或转义的分片 JSON 上 salvage 结果可能分叉。toolcall_start 的 `arguments: {}` 两边一致。

### 低危（边界/表示差异）

#### 14. `runToolCall` 钩子 context.tools 被覆盖
- TS：`runToolCall` 传 `prepareToolCall(context, ..., options, signal, options.tools)`，before/after 钩子收到**原始** `options.context`（agent-loop.ts:805-812）
- Rust：`run_tool_call` 先 `context.tools = Some(options.tools.clone())` 再跑 prepare/finalize，钩子收到**覆盖后**的 context（agent-loop.rs:960-970）
- 说明：钩子内读 `context.tools` 看到的值不同（Rust 是本次调用的 tools，TS 是调用方传入的 context.tools）。

#### 15. `createErrorToolResult` 的 isError/terminate 默认值
- TS：`createErrorToolResult` 只设 content+details，`isError`/`terminate` 为 undefined；错误标记挂在 outcome 上（agent-loop.ts:898-903）
- Rust：`create_error_tool_result` 设 `is_error: true, terminate: false`（agent-loop.rs:1236-1248）
- 说明：afterToolCall 上下文中 `result.isError` 在 TS 为 undefined（falsy），在 Rust 为 true。shouldTerminate（`=== true` 比较）与 toolResult 消息的 isError 两边一致。

#### 16. proxy URL 尾斜杠
- TS：`fetch(`${options.proxyUrl}/api/stream`, ...)`——原样拼接（proxy.ts:146）
- Rust：`options.proxy_url.trim_end_matches('/')` 再拼 `/api/stream`（proxy.rs:397-400）
- 说明：proxyUrl 带尾斜杠时 TS 发 `//api/stream`，Rust 发 `/api/stream`。

#### 17. SSE 行解析
- TS：`if (!line.startsWith("data: ")) return;`——严格要求 `data: `（含空格、无前导空白）（proxy.ts:167）
- Rust：`line.trim()` 后 `strip_prefix("data:")`——接受前导空白与无空格形式（proxy.rs:445-450）
- 说明：`data:{}`（无空格）或 `  data: x` 的行 TS 忽略、Rust 处理。

#### 18. proxy 请求体 None → null
- TS：`JSON.stringify` 丢弃值为 undefined 的键（buildProxyRequestOptions 未设的项不出现在请求体）（proxy.ts:152-160）
- Rust：`serde_json::json!` 把 Option::None 序列化为 `null`（proxy.rs:383-395）
- 说明：服务器看到 `"temperature": null` 等键 vs TS 完全没有该键；对严格 schema 的代理服务器可能拒绝。

#### 19. proxy partial 稀疏 content 填充
- TS：`partial.content[contentIndex] = {...}` 直接下标赋值——跳过的下标为 hole（JSON 序列化为 null）（proxy.ts:231 等）
- Rust：`set_content` resize 用空 Text 块填充（proxy.rs:259-271）
- 说明：服务器跳 index 发事件（如先发 index 2 的 start）时，重建消息的 content 数组内容不同（holes vs 空 Text 块）。

#### 20. durationMs 取整
- TS：`Math.round(performance.now() - startedAt)`（agent-loop.ts:828）
- Rust：`started_at.elapsed().as_millis() as u64`（截断）（agent-loop.rs:1167-1200）
- 说明：亚毫秒余数上 TS 四舍五入、Rust 向下取整，最多差 1ms。

#### 21. Agent 公开可变 hook 字段面收窄
- TS：`Agent` 类把 convertToLlm/transformContext/streamFunction/getApiKey/onPayload/onResponse/onProviderStreamEvent/beforeToolCall/afterToolCall/finishTurn/prepareRequest/prepareNextTurn/prepareNextTurnWithContext/sessionId/thinkingBudgets/maxRetryDelayMs/toolExecution 全部声明为 `public` 可写字段，调用方可在运行期直接赋值替换（agent.ts:184-216）
- Rust：agent.rs 这些值全部收进私有 `AgentInner`，只能在 `Agent::new(AgentOptions)` 时设置，无任何运行期 setter（steering/followUp 队列与 state 有 setter，hooks 没有）
- 说明：TS 调用方「构造后替换 finishTurn/streamFn/钩子」的惯用法在 Rust 不可用。低危（多数宿主构造期一次性配置），但属公开 API 面差异。

---

## 四、豁免（语言机制等价，不计入差异）

| TS | Rust | 为何等价 |
|---|---|---|
| `throw new Error(...)` | `panic!(...)`（prompt/continue/reset/agent_loop 守卫、process_events 的 expect） | 消息文本一致；run 内路径经 catch_unwind→handle_run_failure 等价于 try/catch；直接 API 抛出机制不同属语言惯例 |
| `AgentInitialState`（initialState 嵌套） | AgentOptions 扁平字段 system_prompt/model/thinking_level/tools/messages | 纯结构重组；语义差异见逻辑差异 1 |
| `state` 活对象 + getter/setter | `state()` 快照 + `set_tools/set_messages/set_model/set_thinking_level` | 线程安全要求下的等价访问面；赋值即拷贝语义由所有权天然满足 |
| `AgentLoopConfig extends SimpleStreamOptions` | `AgentLoopConfig.stream: SimpleStreamOptions` 子结构 | 纯结构重组；Agent 所设字段（maxRetryDelayMs/onPayload/onResponse/transport/sessionId/onProviderStreamEvent/reasoning/thinkingBudgets）全部映射到位 |
| `PrepareRequestContext.thinkingLevel: ThinkingLevel`（"off" 为值） | `thinking_level: Option<pi_ai::ThinkingLevel>`（off≡None） | 文档化双向映射，钩子返回值处理两边等价 |
| `getApiKey` 可同步返回 string | `GetApiKeyFn` 强制 Future | TS 用法处有 await，等价；仅类型形状收窄 |
| `Set<listener>` | `Vec<(u64, listener)>` | JS Set 保持插入序，Rust Vec 同序；退订闭包两边等价 |
| `void promise.then(...)` | `tokio::spawn` | 同一「fire-and-forget + 后台结束流」语义 |
| 模块组织：TS `export * from "./proxy.ts"` 根导出 | `pub mod proxy`（`pi_agent_core::proxy::stream_proxy`） | 仅路径不同，符号均公开可达 |
| `performance.now()`/`Date.now()` | `Instant::now()`/`uuid::now_ms()` | 单调钟/纪元毫秒语义一致（取整差异见逻辑差异 20） |
| JS 数组/引用语义 vs 所有权/克隆 | — | 结构性克隆在事件/快照路径语义等价 |
| `streamFn ?? getDefaultStreamFn()`（runAgentLoop 内） | Rust 类型禁止 undefined，Agent::new 时 `unwrap_or_else(get_default_stream_fn)` | 默认解析点不同但结果一致；低层调用者如需默认须显式取 |

## 五、命名差异（不计入问题）

snake_case 全局重命名；`prompt(input, images?)` 重载 → `prompt_text/prompt_text_with_images/prompt_messages`；`continue()` → `continue_turn()`；`followUp` → `follow_up`；访问器 `steeringMode` → `steering_mode()/set_steering_mode()` 等。均属允许的 Rust 风格。

## 六、文件级核对摘要

| TS 文件 | Rust 文件 | 关键结论 |
|---|---|---|
| agent.ts | agent.rs | Agent 全部方法/访问器/队列机制均在；差异：systemPrompt/messages 表示法、reset 基线、skipInitialSteeringPoll、hook 公开可变面收窄（TS `agent.finishTurn = ...` 可运行期改，Rust 仅构造期） |
| agent-loop.ts | agent-loop.rs | 双层循环、双执行模式、runToolCall、hooks 合并语义（content/details/usage/terminate/structuredContent 联动、blocked 文案）均对齐；差异见缺失 1/2/4/8 与逻辑差异 3-8 |
| proxy.ts | proxy.rs | AbortSignal（select 中断 + 逐块 aborted 检查）、非 2xx `{error}` 体、EOF 无终结事件保护、providerThinkingLevel、done/error 重建均**已对齐**；差异见逻辑差异 10-13、16-19 与多余 7 |
| stream-fn.ts | stream-fn.rs | 完全一致（含未配置时的错误文案） |
| types.ts | types.rs | 类型主体对齐；差异见缺失 5/6/7、多余 2/3/4/6 |
| index.ts | lib.rs | 导出集合除 `default_convert_to_llm`（多余 1）外一致；`setDefaultStreamFn` 根导出与 TS 相同，`getDefaultStreamFn` 仅模块路径可达（与 TS 一致） |

## 七、与既有 AUDIT-REPORT.md 的关系

旧报告明细表（9 缺失 + 5 多余）为修复前快照；当前代码中其「缺失 1/2/3/4/5/6/7/8/9」（onPayload 链路、steeringMode/followUpMode 访问器、subscribe 退订、prompt images、handleRunFailure 模型信息、proxy AbortSignal/EOF/错误体、providerThinkingLevel）**均已修复核实**；「多余」中的 proxy_stream_fn / get_default_stream_fn 再导出 / FinalizedToolCallOutcome 公开 / ShouldStopAfterTurnContext / set_system_prompt 也**均已清理核实**。旧「存疑项」中 `skipInitialSteeringPoll`、proxy toolcall_delta 重建、state 消息表示法三项经本次复核确认仍存在（见缺失 3、逻辑差异 13、逻辑差异 1），并新发现 17 项未记录差异。跨模块提示：pi_ai 的 `SimpleStreamOptions` 已含 onPayload/onResponse/onProviderStreamEvent 字段（旧「已知偏差 1」已消除），但 adapter 层是否实际调用属 ai 模块范围，agent 侧只负责转发（已核实转发链路完整）。
