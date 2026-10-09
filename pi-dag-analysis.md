# pi-dag 可行性分析：抽取 DAG 执行内核，用确定性换取确定性与成本

> 调研对象：`../x.engine`（Rust 编排引擎）+ 当前仓库 `pi_rs`（Pi 的 Rust 实现）
> 你的目标：**只从 x.engine 抽取 DAG 执行引擎**，装进 pi，用 DAG 的确定性降低 agent 的不确定性与成本
> 结论：**可行，且比预期便宜** —— 执行语义层与存储耦合很浅（只用 55 个存储方法），但**建议「抽语义、不抽调度与存储」**，而不是整引擎搬运

---

## 0. 结论摘要

1. **抽取是可行的中等工程**。x.engine 的执行树语义（节点状态判定、子节点推进、并发分支）大多是**纯函数**，操作内存中的 `&[ExecutionNode]`，不碰数据库；复合块处理器（parallel/race/router/loop/for_each/try_catch）对 storage 的调用只有 0–5 处。
2. **真正的耦合点在调度层与存储层，而这两层正是 pi 已有的东西**（lane/operation/drive + session entry/values）。整引擎搬运会把多租户、集群、外部化、webhook、cron、SLA、加密一并带进来，收益远小于成本。
3. **推荐路线**：抽取「执行树模型 + 复合块推进语义 + 状态判定」约 **2–3k 行**（可原样搬），调度与持久化**用 pi 的现有机制承担**；不要抽 `scheduler.rs`（tick loop + DB 轮询）与 `x-storage`（207 方法）。
4. **确定性与成本的收益是真实的，但有边界**：结构性不确定可以消除（控制流由图固定），LLM 节点的内容不确定只能"冻结"（缓存 + 快照 + 温度/模型固定）；成本下降的主要来源不是并行，而是**上下文裁剪**（agent 每轮带全量历史，DAG 节点只带依赖数据）。
5. **第一步应该先量化收益**：用同一个任务做对照实验（agent 自由执行 vs 提取成 DAG 后重放），比较 token、调用次数、成功率、墙钟。收益不显著就不该投工程。

---

## 1. x.engine 调研（与抽取相关的部分）

### 1.1 规模与模块

- 5 crate / 约 7.2 万行：`x-types`(5.2k) `x-storage`(19.9k) `x-engine`(45.1k) `x-api`(1.5k) `x-server`(63)
- `x-engine` 内部：`handlers/`(32 文件/20.5k 行) `evaluator/`(2.1k) `scheduler/`(2.0k) `scheduling/`(1.2k) + 23 个顶层模块

### 1.2 执行模型

```
Sequence(定义) → Block 树(10 种) → Instance(一次运行, 状态机) → Execution Tree(节点状态) → Handler(步骤实现)
```

- 执行是**三相位求值**：`ensure_execution_tree` → `evaluate`（root activation / running steps / composite re-evaluation）→ 判定 → 推进
- 节点状态机：`Pending → Running → Completed | Failed | Waiting | Cancelled`，另有 `Skipped`
- 复合块：`parallel / race / router / loop / for_each / try_catch / sub_sequence / ab_split / cancellation_scope`

### 1.3 与抽取直接相关的三个事实

| 事实 | 证据 | 对抽取的意义 |
|---|---|---|
| 执行树判定是**纯函数** | `evaluator.rs` 的 `children_of` / `all_terminal` / `any_completed` / `all_completed` / `any_failed` / `has_waiting_nodes` 全部只操作 `&[ExecutionNode]` | 语义层可干净剥离 |
| 复合块与存储**几乎不耦合** | storage 调用：`race` 0、`try_catch` 0、`cancellation_scope` 0、`parallel` 1、`loop` 1、`router` 2、`for_each` 5 | 块推进逻辑可原样搬 |
| 存储接口虽大但**用得少** | `StorageBackend` = 10 子 trait / 207 方法，实际只调用 **55 个**（`create_instance`/`get_instance`/`create_execution_node`/`update_node_state`/`get_execution_tree`/`save_block_output`/`enqueue_signal`/KV 等） | 若需自建后端，实现 ~50 方法即可 |

**注意**：`scheduler.rs`（storage 调用 38 处）与 `scheduler/step_exec.rs`（`step_block.rs` 45 处）耦合最深——它们负责 tick 轮询、实例领取、并发限制、信号、deadline。**这一层与 pi 的 lane/operation/drive 职责重叠**。

### 1.4 一个必须知道的语义差距

x.engine **没有显式 DAG**：控制流由递归 Block 树表达，不存在 `depends_on` 边，也没有拓扑调度。你要的"真 DAG"（含 fan-out/fan-in 汇合、跨分支数据绑定）需要在其执行树模型之上**自己加一层**（边与绑定），否则只能得到树形工作流。

### 1.5 另一个重叠：x.engine 自带 agent

`handlers/agent.rs`（1519 行）是 **durable ReAct loop**；另有 `llm/`(多 provider+failover)、`tool_call.rs`(HTTP)、`mcp.rs`(928 行)。**抽取时应明确排除它们**——agent 循环由 pi 提供，DAG 只负责编排。

---

## 2. pi_rs 侧的地基（抽取后要靠它承接）

- **会话**：append-only entry 树 + seq 高水位 + 原子发布（`harness/session/*`）→ 存 DAG 定义与运行记录
- **运行时**：operation 状态机（meta/state 分离、全量替换、terminal 结果、接受/执行分离）+ lane/branch/fork
- **重放安全**：intent → effect → settlement，`replay: "never" | "safe"` + checkpoint（`drive/tools.rs`）→ **DAG 节点重放直接复用**
- **执行器**：工具注册表 + 审批 + 截断 + 文件变更串行化（`harness/tools/*`）
- **观测**：事件总线 + telemetry + 恢复/归约

---

## 3. 抽取方案：三种粒度

| 粒度 | 抽什么 | 代码量 | 需要新增 | 评价 |
|---|---|---|---|---|
| **① 全量搬运** | x-types 领域类型 + evaluator + scheduler + 复合块 handlers + expression/template + 一个 SQLite 后端 | ~19k 行 | 存储后端接入、handler 全替换 | ❌ 带入多租户/集群/外部化/webhook/cron/SLA/加密；tick+DB 轮询对单机是过度设计 |
| **② 裁剪抽取（推荐）** | 执行树模型 + 状态判定 + 复合块推进语义 + 表达式/模板（可选） | **~2–3k 行** | 调度用 pi 的 drive/operation；持久化用 pi 的 session | ✅ 拿工业级语义，弃无关复杂度 |
| **③ 借鉴重写** | 不搬代码，照模型重写精简内核 | ~2–2.5k 行 | 全部自写 | ⚠️ 可控但会丢掉 x.engine 已解决的边界情况（重试/信号/并发/取消的细节） |

### 3.1 方案② 的具体清单

**建议搬（原样或轻改）**
- `x-types`：`execution.rs`（ExecutionNode/NodeState，201 行）、`sequence.rs` 的 Block 定义（992 行，可裁剪掉 tenant/status/interceptor 等字段）、`context.rs`（393 行，ExecutionContext 的 data/config/audit/runtime 分区）、`error.rs`（207 行）
- `x-engine/src/evaluator.rs`：`children_of`/`all_terminal`/`any_completed`/`all_completed`/`any_failed`/`has_waiting_nodes`/`activate_pending_children`/`cancel_subtree` 以及 `complete_node`/`fail_node` 的语义（把 storage 写入改成回调）
- `x-engine/src/handlers/` 的复合块推进：`parallel.rs`(793)、`race.rs`(608)、`router.rs`(852)、`loop_block.rs`(999)、`for_each.rs`(946)、`try_catch.rs`(918)、`cancellation_scope.rs`(481) —— 共约 5.6k 行，实际核心逻辑少于此
- 可选：`expression.rs`(2825) / `template.rs`(2355) —— 参数化与条件求值需要；若嫌重，先用简单的占位符替换

**明确不搬**
- `scheduler.rs` / `scheduler/step_exec.rs`：tick loop、实例领取、并发信号量、deadline、扩展性机制 → **由 pi 的 lane/operation/drive 承担**
- `x-storage` 全部（sqlx / 207 方法 / 外部化 / 加密 / 多后端）
- `handlers/{agent,llm,tool_call,mcp,human_review,memory,self_modify,wasm,grpc,activepieces,ab_split}.rs`
- 顶层：`webhooks / cron / metrics / cluster / interceptors / triggers / credentials / gc / lint / preload / sequence_cache / circuit_breaker / sla / externalized`

**需要新增（约 800–1200 行）**
- 一个 `DagStore` trait（约 15–20 个方法：读取执行树、写节点状态、保存节点输出、KV）→ 实现对接 pi 的 session values/entries
- `NodeExecutor`：把节点映射到 pi 的工具 / LLM / 人工输入
- 调度循环：把 x.engine 的"tick + 轮询"换成"事件驱动的推进"（pi 已有 drive 的推进语义）

### 3.2 抽取后的形态

```
crates/pi-dag/
  core/         ← 从 x.engine 裁剪而来（执行树 + 块推进语义，纯逻辑）
    execution.rs   NodeState / ExecutionNode / 判定函数
    block.rs       Block 定义（裁剪版）
    advance.rs     activate_pending_children / complete / fail / cancel_subtree
    composite/     parallel / race / router / loop / for_each / try_catch
  graph.rs      ← 新增：显式 DAG 的边与绑定（x.engine 没有）
  ir.rs         ← 新增：Dag / Node / Edge / Param（对 pi 友好的门面）
  extract/      ← 新增：轨迹 → IR
  exec/         ← 新增：NodeExecutor + 事件驱动调度（复用 pi 的 operation/drive）
  store.rs      ← 新增：DagStore trait + pi session 实现
```

---

## 4. 用确定性换成本：机制与边界

### 4.1 不确定性从哪来，能被消除到什么程度

| 来源 | 能否消除 | 手段 |
|---|---|---|
| **控制流由 LLM 决定**（下一步做什么） | ✅ 完全消除 | 把轨迹固化成显式图：分支条件、循环次数、并行组都写死在 IR |
| **数据依赖隐含在上下文里** | ⚠️ 大部分 | 显式参数 + 上游输出绑定；提取阶段的依赖推断 + 人工审阅 |
| **LLM 节点的输出内容** | ⚠️ 只能"冻结" | 固定模型/温度、结果缓存（cache_key）、运行快照；或对确定性步骤降级为规则/代码 |
| **外部世界状态**（文件、网络、时间） | ❌ 不能 | 沙箱、快照、幂等 + `replay: never/safe` |
| **失败与重试路径** | ✅ 可控 | 节点级 retry 策略写进 IR；失败只重跑该节点 |

**结论**：能得到的是"**结构完全确定 + 关键节点可冻结**"的可复现执行，而不是数学意义上的 deterministic。对外表述应用"可复现（reproducible）"而非"确定性（deterministic）"。

### 4.2 成本下降的四个来源（按预期收益排序）

1. **上下文裁剪（最大项）**：pi 的 agent loop 每轮把全量历史发给模型（`convert_to_llm` 全量 + 上下文压缩兜底）；DAG 节点只携带它依赖的参数与上游输出 → 单次调用 token 显著下降。
2. **不再重复推理已固化的决策**：原轨迹中"读文件 → 判断 → 改文件 → 判断 → 跑测试"里的判断若已固化成图，重放时这些轮次**完全不需要 LLM**。
3. **缓存命中**：`cache_key`（x.engine 已有此字段）或按 DAG 节点指纹缓存输出；重复/相似任务第二次近乎零成本。
4. **失败只重跑失败节点**：agent 现在失败要整轮重推理（可能重跑多次工具），DAG 只重试失败节点，且可以用更小的模型。

反向成本（要诚实计入）：**提取阶段本身要花 LLM 调用**；DAG 维护有复杂度成本；不能覆盖"探索型任务"（图无法预先确定）。

### 4.3 必须先做的收益实验（阶段 0）

选 3–5 个**已完成的任务轨迹**（有代表性的：读改文件、多工具链、条件分支、失败重试），对每个做两件事：
1. 原样重跑一遍 agent（记录 token、LLM 调用次数、工具调用次数、墙钟、成功与否）
2. 手工把轨迹整理成 DAG 并执行（同样记录）

对比四项指标 → 得到**收益上限**。若 token 下降不明显，说明该任务的 LLM 决策无法固化，应重新选择适用场景。这个实验**不需要任何引擎**，用现成工具手工跑即可完成。

---

## 5. 路线图（更新）

**阶段 0 — 收益验证（3–5 天，零工程风险）**
- 手工把 3–5 个真实轨迹整理成 DAG（JSON 手写），用一个 200 行的顺序执行脚本跑通
- 产出：token/调用次数/成败的对照表 + "哪类任务适合 DAG"的结论
- 通过条件：至少一类任务 token 下降 ≥ 30% 且成功率不降

**阶段 1 — 抽取执行语义内核（1 周）**
- 从 `x.engine` 搬 `execution.rs` + `sequence.rs`(裁剪) + `evaluator.rs` 的纯函数 + `parallel/race/router/try_catch` 的推进逻辑
- 自写 `DagStore`（对接 pi session 的 values/entries）与事件驱动调度
- 产出：`pi-dag` crate 能用 IR 跑通"顺序 + 分支 + 并行 + 重试"
- 验收：与阶段 0 的手工结果语义一致

**阶段 2 — 轨迹提取器（1–2 周）**
- 从会话 entries / operation 记录提取步骤、依赖、参数；输出 IR + Mermaid
- 保守策略：不确定即串行；LLM 辅助推断必须过校验器
- 验收：3 个真实会话的提取结果人工评审通过

**阶段 3 — 与 pi 深度融合（2 周）**
- 节点执行走 pi 工具注册表（含审批）；`replay` 策略复用 intent/settlement
- 运行记录写回 session（custom entry + values），支持恢复与审计
- 验收：kill 进程后 resume 不重复副作用；同一 DAG 两次执行结果一致（除外部世界）

**阶段 4 — 循环增益（持续）**
- 把"提取 → 执行 → 对比"做成闭环：DAG 执行结果回写为新的轨迹样本，用于下次提取
- 建立任务分类：哪些该走 DAG（确定性任务）、哪些该走 agent（探索性任务）→ **混合执行器**（DAG 节点内可嵌套一个 agent 子任务）

---

## 6. 需要你拍板的三个决策

1. **抽取粒度**：方案② 裁剪抽取（推荐）还是方案③ 借鉴重写？（① 全量搬运不建议）
2. **调度归属**：DAG 的推进挂在 pi 的 operation 状态机上（复用 durability，改动 pi-agent）还是 pi-dag 自带轻量调度（不碰核心，但 durability 要自己维护）？
3. **DAG 与 agent 的关系**：DAG 作为 agent 的**工具**（agent 调用 `dag_run`），还是作为**并行执行体**（agent 与 DAG 各自独立）？前者更符合 Pi 的"核心最小"哲学。

---

## 7. 附：x.engine 关键模块与 DAG 概念映射

- `step` ↔ 工具/LLM/人工节点
- `parallel`/`race` ↔ 并行组/竞速组（x.engine 用 `branch_index` 区分子节点）
- `router` ↔ 条件分支
- `loop`/`for_each` ↔ 循环/集合展开
- `try_catch` ↔ 错误恢复
- `sub_sequence` ↔ 子 DAG
- `cancellation_scope` ↔ 不可取消区（对应"已产生副作用不可撤销"）
- `cache_key` ↔ 节点级缓存键（成本优化的关键字段）
- `instance`/`execution tree` ↔ DAG run / 运行记录
- `signal`(pause/resume/cancel/update_context) ↔ pi 的 abort/cancel/steering
- `handler` 注册表 ↔ pi 的工具注册表
- `agent`/`llm`/`tool_call`/`mcp` handler ↔ **重叠，抽取时排除**（agent 循环由 pi 提供）

### x.engine 代码索引（抽取时直接定位）

- 执行树语义：`x-engine/src/evaluator.rs`（1224 行，含 `evaluate` 三相位）
- 块分发：`x-engine/src/evaluator/dispatch.rs`（约 300 行）
- 复合块推进：`x-engine/src/handlers/{parallel,race,router,loop_block,for_each,try_catch,cancellation_scope}.rs`
- 类型定义：`x-types/src/{execution,sequence,context,instance,error,signal}.rs`
- 调度（不抽）：`x-engine/src/scheduler.rs`、`scheduler/step_exec.rs`
- 存储（不抽）：`x-storage`（`StorageBackend` 10 子 trait / 207 方法）
