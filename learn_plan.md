# 从零搭建 Agent 学习计划（一周 · 以 Pi 为教材）

> **最终目标**：不依赖任何 agent 框架，自己从零搭出一个能用的 agent。
>
> **本计划的做法**：把 Pi 当**教材和参照系**——每天先看 Pi 在这个子系统上做了什么、为什么这么做，然后**自己写一个最小实现**。一周结束后你会拥有一个自研 mini agent（约 1500–2500 行），并且理解每一行背后的取舍。
>
> **一周产出**：一个能跑的自研 agent，具备
> ① 流式输出 ② 工具调用循环 ③ 工具注册与安全 ④ 上下文自动压缩 ⑤ 会话持久化与崩溃恢复 ⑥ 中断取消与错误重试 ⑦ 结构化日志与评测脚本。
>
> **强度**：每天 3–4.5 小时。节奏：看 Pi 30–50m → 自己写 120–180m → 验收 30m → 笔记 15m。

---

## 0. 先划清"从零"的边界

**允许用（属于语言基础设施）**
- HTTP 客户端：`reqwest` / `hyper`
- JSON：`serde` / `serde_json`
- 异步运行时：`tokio`
- CLI 与日志：`clap`、`tracing`
- 小工具：`uuid`、`similar`（diff）等

**禁止用（这些正是你要学会的东西）**
- agent 框架：`rig`、`swiftide`、`langchain` 类
- 厂商 LLM SDK：`async-openai`、`openai-api-rs` 等 —— **协议你要自己解析**
- 现成的 agent loop / tool calling 编排库

**建议做法**：新建一个独立 crate（例如 `my-agent`），把本仓库和 Pi 文档当**对照物**，需要时去查 Pi 怎么处理边界情况，但代码自己写。抄会跳过学习发生的地方。

---

## Day 1 — 打通 LLM：消息模型与流式解析（3h）

**从零实现**
1. 定义消息模型：`System` / `User` / `Assistant` / `Tool` 四种角色，内容支持文本 + 工具调用。
2. 手写 HTTP 请求（不要用 SDK）：POST 到 OpenAI Responses API 或 Chat Completions API。
3. **自己解析 SSE**：逐块读取 → 缓冲按行切分 → 处理 `data:` 前缀 → 累积增量 → 打印流式文本 → 收尾拿到 usage 与 stop reason。

**看 Pi 的哪里**
- `crates/pi-ai/src/types.rs` —— `Message` / `ContentBlock`（Text/Thinking/Image/ToolCall）/ `Usage` / `StopReason`：看一套完整消息模型要覆盖哪些形态
- `crates/pi-ai/src/api/openai-responses.rs` —— 真实的 SSE 事件累积（`response.output_item.added` / `*.delta` / `*.done`）、多 slot 累积、ID 规范化
- `crates/pi-ai/src/utils/event-stream.rs` —— 生产者/消费者分离的流抽象（你的版本可以先用 channel）

**必须自己搞懂的原理**
- SSE 协议：事件边界、多行 data、`[DONE]`、连接中断
- 流式增量 ≠ 完整消息：文本要拼接、工具调用参数是**分片字符串**要累积后再解析
- 错误模型：网络错误、HTTP 错误、API 错误、流中途错误，应该统一成结构化错误返回给上层
- token usage 只在流结束时给出，所以上下文预算要靠估算

**验收**
- 流式打印一段回复，结束时打印 usage 与 stop reason。
- 拔网线/给错 key，程序给出结构化错误而不是 panic。
- 能说出你解析出的每个 SSE 事件名和它的作用。

**坑**：UTF-8 字符可能被切在两个 chunk 之间（要按字节缓冲再解码）；不同 provider 的字段名不一致。

---

## Day 2 — 工具调用循环（4h）

**从零实现**
1. 定义工具描述（name / description / JSON Schema），序列化进请求。
2. 从流式响应里累积出 `tool_calls`（id、name、arguments 分片）。
3. 执行工具 → 把结果作为 `tool` 消息回填 → **再次请求** → 直到模型不再调用工具。
4. 加最大轮数保护（防止无限循环）与"工具不存在/参数非法"时的错误反馈（让模型自我纠正）。

**看 Pi 的哪里**
- `crates/pi-agent/src/agent-loop.rs` —— `run_loop` 的骨架：内层处理工具调用、外层的终止判断、`turn_end` 的语义
- `pi-agent/AGENT.md` 的「原理」第 1 条 —— 双层循环的动机（工具调用循环 vs follow-up）
- `harness.md` Part 3 的 assistant / tools 两节 —— 一次"助手运行"由哪些相位组成

**必须自己搞懂的原理**
- ReAct 循环：推理 → 行动 → 观察 → 再推理
- **配对约束**：assistant 消息带 `tool_calls`，随后必须有**一一对应**的 tool 结果消息，顺序和 id 都不能错
- 终止条件：没有工具调用即结束；还要处理"模型幻觉出不存在的工具"
- 工具结果也是上下文：太长会爆窗口（Day 3 处理），出错也要回填（模型能据此重试）

**验收**
- agent 能完成：读文件 → 改一行 → 写回，并汇报结果。
- 故意给一个不存在的工具名，模型被错误信息引导后能改对。
- 能画出你的循环流程图，标出所有终止分支。

**坑**：并行工具调用的顺序、`arguments` 是分片 JSON（要累积后 repair 再解析）、不要把 tool 结果塞成 user 消息。

---

## Day 3 — 工具系统与安全（3.5h）

**从零实现**
1. 工具注册表 + 分发（name → 执行函数）。
2. **参数校验与强制转换**：按 JSON Schema 递归 coercion（字符串数字 → 数字、单值 → 数组）再校验，非法则返回可读错误。
3. 审批钩子：危险操作（写文件、执行命令）在**执行前**询问用户 `Y/n`。
4. 四个基础工具：`read`（带行号/偏移）、`write`、`edit`（唯一匹配 + diff）、`bash`（超时 + 输出上限）。
5. 结果截断：行数 + 字节双限制；同一路径的写操作串行化。

**看 Pi 的哪里**
- `crates/pi-agent/src/harness/execution/tools.rs` —— `prepare` / `execute` / `finalize` 三段式（校验与审批在前、执行在中、结果修补在后）
- `crates/pi-agent/src/harness/tools/{read,write,edit,edit-diff,bash}.rs` —— 真实工具的边界处理
- `harness/utils/truncate.rs` —— 双限制截断与"尾行部分截断"的边界
- `tools/file-mutation-queue.rs` —— 同一文件并发写的串行化

**必须自己搞懂的原理**
- 模型输出是**不可信输入**：类型、字段、路径都要当作来自网络的恶意数据处理
- 工具设计三要素：清晰描述（决定模型用不用得对）、严格 schema、可读的错误信息
- 上下文保护：大输出必须截断，并告诉模型"被截断了，完整内容在文件 X"
- 幂等性预告：`bash` 这类不可逆工具，后面做恢复时会是最大麻烦（Day 5 会遇到）

**验收**
- 四个工具全部可用；`edit` 在文本不唯一时拒绝并提示。
- 越权路径（`../` 逃出工作目录）被拒绝。
- 100KB 输出被截断且模型收到明确提示。

**坑**：coercion 只做"安全转换"别做猜测；审批要能"拒绝但继续这一轮"。

---

## Day 4 — 上下文工程（3.5h）

**从零实现**
1. token 估算：字符数启发式（中英有别），并用 API 返回的真实 usage 校准系数。
2. 预算管理：每轮请求前检查估算值，超阈值就压缩。
3. 压缩：选切点（保留最近 N 轮 + 工具调用完整性）→ 调 LLM 生成结构化摘要 → 用摘要替换被切掉的部分。
4. system prompt 组装：角色、工具说明、约束、当前工作目录；预留"技能/模板"注入位。

**看 Pi 的哪里**
- `crates/pi-agent/src/harness/compaction/compaction.rs` —— `estimate_tokens` / `should_compact` / `find_cut_point` / `prepare_compaction` / `generate_summary`
- `crates/pi-agent/src/harness/messages.rs` —— 内部消息 → LLM 消息的转换（哪些自定义消息要降级/丢弃）
- `docs/coding-agent/compaction.md` —— 产品层的压缩策略与经验

**必须自己搞懂的原理**
- 上下文是**最稀缺资源**：压缩的目标不是"变短"，而是"不丢关键状态"
- 切点必须在消息边界，**不能切在工具调用中间**（否则配对约束被破坏）
- 摘要要有结构：任务目标 / 已完成 / 当前状态 / 待办 / 关键文件与命令
- 压缩本身要花一次 LLM 调用，属于"必要成本"

**验收**
- 构造超长对话，自动压缩后任务能继续（用一个跨压缩点的任务验证）。
- 打印压缩前后的估算 token 与消息条数。
- 能解释你的切点策略为什么安全。

**坑**：压缩后不要把 system prompt 也压掉；摘要要标记为"历史摘要"而不是新的 user 消息。

---

## Day 5 — 持久化与崩溃恢复（4.5h）

这是从"玩具"走向"能干活"的分水岭，也是 Pi 最值得学的地方。

**从零实现**
1. append-only 会话日志（JSONL）：每个事件带递增 seq，写入即 flush。
2. 启动时 replay 重建上下文（不含不可序列化的运行时状态）。
3. **副作用安全**：工具执行前先写"意图 + 幂等键"，执行后写"结果"；恢复时遇到"有意图无结果"的操作要**重新确认而不是盲目重放**。
4. resume：崩溃重启后从日志恢复，继续未完成的工作。

**看 Pi 的哪里**
- `harness.md` Part 1（存储：事务、绑定值、账本）+ Part 4（recovery / abort / close）
- `docs/agent/tool-durability.md` —— **关键**：checkpoint 只是进度，永远不能证明效果完成
- `docs/agent/assistant-durability.md` —— 助手运行的 durable 语义
- `crates/pi-agent/src/harness/session/{commit,jsonl/io,jsonl/storage}.rs` —— 事务校验、原子发布、replay

**必须自己搞懂的原理**
- event sourcing：状态是日志的投影，日志是唯一真相
- 事务边界：**先校验再应用**，永远不暴露半成品状态
- at-least-once vs exactly-once：真正落地只能做到"至少一次 + 幂等"，所以要把副作用设计成可识别、可查询的
- 高水位 seq：它是恢复时判断"哪些已完成"的依据

**验收**
- 在工具执行中途 kill 进程，重启后能继续任务，且**已完成的副作用不重复执行**。
- 手工写一个非法日志（重复 id / seq 断档），程序拒绝并给出明确错误。
- 能画出"写入 → 校验 → 应用 → 高水位"的数据流。

**坑**：`bash` 类不可逆操作只能靠"人工确认 + 记录"缓解；日志要原子写（temp + rename）。

---

## Day 6 — 中断、取消、超时与重试（3.5h）

**从零实现**
1. 取消传播：`CancellationToken` 贯穿 HTTP 请求、工具执行、LLM 等待；Ctrl-C 立刻停。
2. 超时：单次 LLM 请求、单次工具执行各自设限。
3. 错误分类与重试：可重试（429 / 5xx / 网络抖动 / 流中断）用指数退避 + 上限；不可重试（400 / 401 / 内容策略）立即失败。
4. 并发：同一轮内的多个工具调用可并行（注意同一文件的写要串行）。

**看 Pi 的哪里**
- `crates/pi-ai/src/utils/{retry,provider-retry,abort}.rs` —— 重试分类、退避上限、可中断 sleep
- `crates/pi-agent/src/agent.rs` —— `abort` / `waitForIdle` 的语义边界
- `harness.md` §4 的 abort 一节 —— 取消与已完成副作用的边界

**必须自己搞懂的原理**
- 取消的语义：停止**尚未产生副作用**的工作；已经落地的效果要如实记录
- 重试的前提是幂等：读操作随便重试，写操作要靠幂等键
- 错误分类决定用户体验：可恢复错误不该让会话崩掉
- 退避要有上限，否则用户以为程序卡死

**验收**
- 请求中按 Ctrl-C，立即停止且日志里没有半截事务。
- 模拟 429，自动退避后恢复成功（打印重试间隔）。
- 能说出你的系统里哪些操作可重试、哪些不可。

**坑**：取消竞态（取消信号与结果返回同时到达）；重试放大（并发 × 重试 = 打爆配额）。

---

## Day 7 — 可观测、评测与对照（3.5h+）

**从零实现**
1. 结构化事件日志：每次 LLM 调用、每个工具调用、每个 turn 都产出带耗时/ token 的事件。
2. 简单指标：token 用量、延迟分布、工具成功率、压缩次数。
3. 评测脚本：5 个端到端任务（读改文件、跨多轮工具、触发压缩、崩溃恢复、错误恢复），一键跑通并打印结果。

**看 Pi 的哪里**
- `crates/pi-agent/src/harness/events.rs` —— 事件总线与监听者错误隔离
- `harness/telemetry.rs` + `docs/agent/telemetry.md` —— span 与属性设计
- `harness.md` Part 9 —— 38 条不变量 + 竞态目录（**对照着检查你的实现缺了什么**）

**验收**
- 评测脚本全绿，输出一份指标摘要。
- 拿你的实现与 Pi 逐项对照，列出差距清单（按你的判断排序：哪些必须补、哪些不需要）。

**交付**：`README.md`（架构图 + 用法）+ 指标样例 + 差距清单。

---

## 附录 A：最小内核架构（建议的模块划分）

```
my-agent/
  main.rs           # CLI：输入循环、Ctrl-C、会话恢复
  llm/
    client.rs       # HTTP 请求构造、SSE 解析、错误映射
    stream.rs       # 事件流抽象（生产者/消费者）
    types.rs        # Message / ContentBlock / ToolCall / Usage / StopReason
  agent/
    loop.rs         # 工具调用循环、终止判断、最大轮数
    context.rs      # 上下文预算、压缩调度、system prompt 组装
  tools/
    registry.rs     # 注册、分发、审批钩子
    schema.rs       # 参数 coercion + 校验
    fs.rs           # read / write / edit
    shell.rs        # bash（超时、截断、串行化）
  session/
    log.rs          # append-only JSONL、seq 高水位、原子写
    replay.rs       # 恢复重建
    idempotency.rs  # 意图/结果记录、幂等键
  obs/
    events.rs       # 结构化事件
    metrics.rs      # token / 延迟 / 成功率
```

关键接口（自己定，别照抄 Pi）：

```rust
async fn stream(&self, messages: &[Message], tools: &[ToolDef], cancel: &CancellationToken)
    -> Result<EventStream<AssistantEvent>, LlmError>;

fn dispatch(&self, call: &ToolCall) -> Result<ToolOutput, ToolError>;   // 校验 + 审批 + 执行

async fn append(&self, event: SessionEvent) -> Result<Seq>;            // 持久化
```

## 附录 B：一周之后的进阶路线

- **第 2 周｜durability 深化**：把"意图/结果"升级成完整的操作状态机（对照 `harness.md` Part 3：meta/state 分离、状态转移全量替换、terminal 结果），实现"接受与执行分离"（可以只接受不执行，稍后再 drive）。
- **第 3 周｜多 agent 与并发**：一个 planner agent + 多个 worker，各自的会话与工具集；学习 Pi 的 lane 隔离与 branch/fork 语义（`harness.md` Part 2）。
- **第 4 周｜产品化**：多 provider 适配层、RPC/流式 UI、插件与技能机制、评测集扩充、成本控制。参考 `docs/coding-agent/` 全套。

## 附录 C：Pi 的资料索引（按主题查）

- 系统模型 / 三存储 / 四原语：`docs/agent/harness.md` §0
- 存储与会话树：§1、§2；`docs/agent/values.md`
- 状态机与执行：§3、§4
- durability（最值得学）：`docs/agent/{assistant-durability,tool-durability}.md`
- 公共接口/事件/hooks/telemetry：§5；`docs/agent/telemetry.md`
- 不变量与竞态：§9
- 设计取舍：`docs/agent/runtime-simplification.md`
- 产品层用法：`docs/coding-agent/{sessions,session-format,compaction,skills,prompt-templates,extensions,providers,security}.md`
- Rust 实现对照：`crates/pi-ai`（流式与消息）、`crates/pi-agent`（循环/工具/会话/运行时）

## 附录 D：常见坑速查

1. SSE 分片切在多字节字符中间 —— 按字节缓冲后再解码。
2. 工具调用 `arguments` 是分片 JSON —— 累积完再 repair 解析。
3. assistant 消息与 tool 结果必须成对且 id 匹配 —— 压缩时要特别小心。
4. 把工具结果当 user 消息 —— 模型会混乱。
5. 无最大轮数保护 —— 模型可能无限调用工具。
6. 不做参数 coercion —— 模型给的 `"10"` 会让你的 `i64` 反序列化失败。
7. 大工具输出不截断 —— 一次 `cat` 就打爆上下文。
8. 在压缩点切断工具调用 —— 破坏配对约束。
9. 副作用重放 —— 没做幂等键，恢复时重复执行。
10. 不做原子写 —— 崩溃时留下半截日志。
11. 重试所有错误 —— 401 重试 10 次只会更慢。
12. 取消信号不传到工具层 —— Ctrl-C 后子进程还在跑。
13. 忽略 usage 校准 —— token 估算偏差会越来越大。
14. 把密钥写进日志/事件 —— 泄漏风险。

## 附录 E：一周自查表

- [ ] 能不看资料讲清 ReAct 循环，并画出我的终止分支
- [ ] 手写过 SSE 解析，处理过 UTF-8 分片
- [ ] 手写过 tool_calls 累积与配对回填
- [ ] 实现过参数 coercion 与校验
- [ ] 实现过工具审批与结果截断
- [ ] 实现过 token 估算与摘要压缩（且跨压缩点任务能继续）
- [ ] 实现过 append-only 日志与 replay 恢复
- [ ] 验证过"崩溃后不重复副作用"
- [ ] 实现过取消传播与错误分类重试
- [ ] 有结构化日志 + 一个能跑的端到端评测脚本
- [ ] 与 Pi 对照过，有明确的差距清单与优先级

---

**最后一条**：每天结束时问自己一句 —— *这个模块解决的是什么问题？如果让我重新设计，我会怎么做？* 能回答，才算真的从零搭起来了。
