# PI-DAG：从对话蒸馏到 DAG 执行的完整设计

> 状态：设计稿 v0.1 · 2026-09-15
> 依赖：`pi_rs`（Pi 运行时的 Rust 实现）、`x.engine`（Apache-2.0，作为执行语义的来源）
> 阅读顺序建议：§1 摘要 → §2 动机 → §5 工作量 → §7 路线图 → §9 待决策

---

## 1. 摘要

**一句话**：做一遍，之后就自动跑 —— 人与 agent 对话完成任务，把这段对话**蒸馏成 DAG**，之后由 DAG 引擎确定性执行。

**为什么**：现实工作中大部分内容流程化。纯交给 agent 成本高且有不确定性；让人手工定义流程又太麻烦。蒸馏把"定义工作流的成本"从**人工建模**转移到**示范一次**。

**怎么做**：在 pi 中新增 `pi-dag` crate —— 执行引擎从 x.engine **裁剪复刻**（取其语义、弃其调度与存储），产品层做「录制 → 蒸馏 → 审核 → 执行 → 失败回退 → 迭代」闭环。

**关键数字**

| 项目 | 数值 |
|---|---|
| 最小集搬运量 | ~4,100 有效行（顺序 + 分支 + try/catch + 重试） |
| 标准集搬运量 | ~6,000 行（+ 并行/竞速/循环/遍历） |
| 完整集搬运量 | ~8,500 行（+ 表达式/模板/缓存） |
| 新增代码 | ~1,100–1,500 行（DagStore / NodeExecutor / 调度 / 显式 DAG 边 / IR） |
| 工期（单人） | 最小集 3–4 周 / 标准集 5–6 周 / 完整集 8–10 周 |
| 首个可验证里程碑 | 1–1.5 周（骨架：DAG JSON 驱动 pi 工具顺序执行） |

**第一步不是写引擎**：先用真实任务做一次"agent vs 手工 DAG"的成本对照（3–5 天），验证收益成立再投入。

---

## 2. 为什么做：动机与定位

### 2.1 三方权衡

| 方案 | 定义成本 | 单次成本 | 确定性 | 适应性 |
|---|---|---|---|---|
| 纯 agent | 0 | 高 | 低 | 高 |
| 手工 workflow | 高（建模/连线/调试） | 低 | 高 | 低 |
| **蒸馏 DAG** | **≈ 做一遍** | **低** | **高** | 中（失败可回退） |

蒸馏 DAG 的生态位：**用 agent 生成工作流**，介于「agent」与「工作流引擎」之间。

### 2.2 任务分布：中间地带是主战场

"大部分工作流程化"成立，但分布不均匀：

1. **完全流程化**（结构固定、只有参数变）：数据同步、文件批处理、报表生成、发版检查 → 纯 DAG 的理想场景
2. **半流程化**（主体固定、少数判断点变化）：工单处理、发票/邮件分类、代码审查、数据清洗 → **占比最大、痛感最强**
3. **非流程化**（探索、判断密集）：排障、方案设计 → 继续用 agent

**结论**：产品必须原生支持**混合图**（确定性节点 + 少量 LLM 判断节点），而不是追求"纯确定性 DAG"。混合不是折中，是主形态。

### 2.3 定位与指标

> **做一遍，之后就自动跑。**（Do it once, then it runs itself.）

- 北极星：**被复用的流程数 × 平均执行次数**（复用率才是价值）
- 辅助：单次成本下降比例、流程成功率、失败自动修复率

### 2.4 适用与不适用

- ✅ 结构稳定、重复发生（≥3 次）、有明确成功判据
- ⚠️ 主体确定但细节有变化 → 保留 1–2 个 LLM 节点
- ❌ 一次性、探索型、强依赖外部语境 → 继续用 agent

---

## 3. 产品设计

### 3.1 生命周期

```
① 录制      人机对话 + 工具调用（正常使用 agent，无需特殊模式）
② 蒸馏      轨迹 → DAG IR（切步骤、推依赖、抽参数、标副作用）
③ 审核      看图 + 勾参数 + 试运行（信任建立的关键一步）
④ 执行      引擎运行（确定性、可恢复、成本低）
⑤ 修复      失败 → 回退 agent 修复 → 产出补丁建议
⑥ 迭代      新版本 / 回滚 / 回归对拍
```

**录制**：Pi 已有足够素材 —— `MessageEntry`（含 toolCall 与 tool result）+ operation 记录（intent 含参数、checkpoint、settlement、`replay` 策略）。蒸馏应优先用 operation 记录，信息比聊天记录全。

**蒸馏原则**：保守（不确定即串行）、可解释（每个判断给理由）、可编辑（产物人可读）。

**审核必须展示**：步骤清单（工具/参数/节点类型）、**副作用清单**（写哪些文件、发哪些请求、可否撤销）、参数候选表、dry-run 结果。

**修复（最关键）**：失败不是报错结束，而是带着失败上下文唤起 agent 修复 → 完成任务 → 产出"补丁建议" → 人确认生成新版本。**这是产品从玩具到可用的分水岭。**

### 3.2 五个关键决策

1. **触发**：❌ 自动检测"任务完成"（误判高）；✅ 显式 `/distill` + 结束时提示 + 支持选择性蒸馏（排除试错片段）
2. **粒度**：混合图。确定性节点（工具，执行时不花钱）+ LLM 节点（判断，上下文被裁剪后更便宜）
3. **参数化**：交互式勾选（系统列候选 → 用户命名/默认值 → 用示例值渲染一遍）。避免全参数化（不可读）与零参数化（只能原样重跑）
4. **数据流**：把"第 3 步输入 = 第 1 步输出某字段"显式化成边。这是"顺序链"与"真 DAG"的分界，也是并行化的前提
5. **信任**：看得懂（Mermaid + 白话说明）、试得起（dry-run + 单步）、退得回（运行记录 + 回滚 + 幂等）

### 3.3 失败策略（做成可配置）

1. **严格**：失败即停等人（财务/生产）
2. **回退 agent**（推荐默认）：唤起 agent 修复 → 继续 → 记录修复动作
3. **整体降级**：退回"从头跑一遍 agent"（最贵最稳）

### 3.4 交互形态（示意）

```
✓ 任务完成
[系统] 这个任务看起来会重复。要固化成一个流程吗？ (Y/n)  → Y

[蒸馏中…] 识别 7 个步骤 / 2 个候选参数 / 3 个副作用

┌ 流程提案：发票整理 v1
│ 1. ls downloads/*.pdf              [只读]
│ 2. 提取日期与商家（LLM）            [只读，参数 {月份} 过滤]
│ 3. 重命名 ${日期}_${商家}.pdf       [写文件 ×N, replay=never]
│ 4. 汇总写 summary.csv              [写文件]
│ 参数：{月份="2026-03"} {输出目录="./out"}
└
[试运行] 0 个副作用被触发，参数解析正常
```

命令面：`/distill`、`/flows`、`/flow show|diff|rollback|test`、`/run <flow> --param k=v`

### 3.5 反模式

蒸馏幻觉（把偶然当必要）｜过度参数化｜副作用失控｜成本倒挂（蒸馏贵于手做）｜长期漂移｜预期错位（以为什么都能蒸馏）｜安全边界（流程是可执行资产，需权限与密钥管理）

### 3.6 MVP 边界与成功标准

**做**：单用户单机、显式 `/distill`、顺序 + 简单分支、参数化、dry-run、失败回退 agent、DAG 存为 Custom entry + 文件库
**不做**：并行/race/子流程、定时调度、多租户、分享市场、自动蒸馏、拖拽编辑器
**成功标准**：三类真实任务（批量文件、API 编排、数据转换）做到「示范一次 → 参数化重放 3 次全成功」，且 token 比重新跑 agent **下降 ≥ 30%**

---

## 4. 技术基础盘点

### 4.1 Pi 已有（可直接复用）

| 能力 | 位置 | 用途 |
|---|---|---|
| append-only 会话 + seq 高水位 + 原子发布 | `harness/session/*` | 存 DAG 定义与运行记录 |
| `Entry::Custom`（custom_type + JSON） | `session/types.rs` | DAG 定义的天然容器 |
| operation 状态机（meta/state、全量替换、terminal 结果） | `harness/runtime/*` | DAG run 的宿主参照 |
| **intent/settlement + `replay: never\|safe` + checkpoint** | `drive/tools.rs` | **重放安全，已解决** |
| 工具注册表 + 审批 + 截断 + 文件写串行化 | `harness/tools/*` | DAG 节点的执行器 |
| lane / branch / fork | `session/{fork,fork-policy}.rs` | 分支与并发单位 |
| 事件总线 + telemetry | `harness/{events,telemetry}.rs` | 节点级进度与成本统计 |
| 可中断 sleep / 退避重试 / deferred 轮询 | `pi-ai/utils/sleep.rs` 等 | 调度原语（注意：非定时任务） |

### 4.2 缺口

- **无定时/调度**：Pi 刻意不做（`harness.md`："no scheduler, auto-start-on-reopen, or hidden continuation exists below this layer"），但明确把调度留给宿主层（"A serving layer may instead schedule `drive` calls through alarms, jobs, or another host runtime"）→ **正是 pi-dag 的位置**
- **无 DAG 语义**：Pi 的 operation 是 lane 内串行；DAG 需要节点级并发与 join
- **无"轨迹 → 流程"的编译器**：这是全新模块

### 4.3 与 x.engine 的关系

x.engine 是成熟的编排引擎，且**自带 durable ReAct agent、LLM/tool_call/MCP handler**，与 Pi 高度重叠。因此定位是：

- **取**：执行树模型、复合块推进语义、状态判定
- **弃**：调度（tick loop + DB 轮询）、存储（sqlx/207 方法）、多租户/集群/外部化/webhook/cron/SLA、与 Pi 重叠的 handler

---

## 5. 执行引擎方案：裁剪复刻

### 5.1 x.engine 关键事实（实测）

- 5 crate / 约 7.2 万行；`x-types` 3,514 有效行、`x-engine` 顶层 6,022、`handlers` 7,870、`evaluator+scheduler` 1,499
- 执行树判定是**纯函数**（操作 `&[ExecutionNode]`，不碰存储）→ 可干净剥离
- 复合块与存储**几乎不耦合**（`race`/`try_catch`/`cancellation_scope` 为 0 处 storage 调用）
- `StorageBackend` 有 207 方法 / 10 子 trait，**实际只调用 55 个**
- 它**没有显式 DAG**（是递归 Block 树，无 `depends_on`）→ 需要自己加边与绑定层

### 5.2 三档方案

| 档位 | 搬运 | 有效行 | 工期 |
|---|---|---|---|
| 最小集 | 执行树语义 + step 执行 + 路由 | ~4,100 | 3–4 周 |
| 标准集 | + 并行/竞速/循环/遍历 | ~6,000 | 5–6 周 |
| 完整集 | + 表达式/模板/缓存 | ~8,500 | 8–10 周 |

### 5.3 搬运清单（最小集）

**类型层（`x-types`，裁剪后约 1,000 行）**：`sequence.rs`(992→~700，删租户/发布状态/拦截器)、`execution.rs`(122)、`context.rs`(185→~140)、`error.rs`(87)、`ids.rs`(288→~120)、`instance.rs`(167)

**执行核心（约 2,900 行）**：`evaluator.rs`(800)、`evaluator/dispatch.rs`(281)、`handlers/{step 453, step_block 545, step_dispatch 250, router 138, try_catch 123, param_resolve 207, util 110, mod 227}`

**标准集追加**：`handlers/{parallel 133, race 86, loop_block 300, for_each 431, cancellation_scope 57, builtin 214, emit_event 203}`、`scheduling/delay.rs`(124)、`x-types/signal.rs`(135)

**完整集追加**：`expression.rs`(1,004)、`template.rs`(904)、`sequence_cache`(160)、`context_cache`(62)、`x-types/config.rs`(296→150)

### 5.4 不搬清单

- **整个 crate**：`x-storage`(19.9k)、`x-api`(1.5k)、`x-server`
- **调度与横切约 5,000 行**：`scheduler.rs`(1,593)、`scheduler/step_exec.rs`(1,083)、`metrics`(105)、`webhooks`(213)、`interceptors`(116)、`lifecycle`(131，保留必要钩子)、`preload`(128)、`required_fields`(141)、`gc`(223)、`lint`(819)、`credentials`(310)、`cron`(201)、`triggers`(13)、`externalized`(162)、`sla`(135)、`circuit_breaker`(438，可选简化)
- **与 Pi 重叠的 handler 约 4,250 行**：`agent`(619)、`llm/*`(673)、`mcp`(501)、`tool_call`(359)、`memory`(454)、`blob`(228)、`activepieces`(219)、`grpc_plugin`(182)、`human_review`(153)、`ab_split`(143)、`query_instance`(100)、`send_signal`(96)、`self_modify`(73)、`wasm_plugin`(448)

### 5.5 十项改造（真正的成本）

| # | 改造 | 影响面 | 难度 |
|---|---|---|---|
| 1 | `StorageBackend`(207 方法) → `DagStore`(~20 方法) | 55 调用点（`step_block` 45 处最密） | 高 |
| 2 | tick loop → 事件驱动推进（`Drive`/operation 接管） | 1,593 行整体替换 | 高 |
| 3 | `StepContext` → pi 执行上下文（工具注册表/Context/session reader） | 所有 handler | 中高 |
| 4 | handler 注册表 → pi 工具 + LLM 适配器 | `handlers/mod.rs` | 中 |
| 5 | 多租户字段清除（`TenantId` 等） | `x-types` 多数文件 | 中 |
| 6 | `metrics::` 47 处 → 删或接 telemetry | 散布各处 | 中 |
| 7 | `lifecycle::` 30 处 → 保留必要钩子 | 状态转移路径 | 中 |
| 8 | `webhooks`(14)+`interceptors`(10)+`externalized`(7) → 删 | 局部 | 低 |
| 9 | 错误类型统一（`EngineError`/`StepError`/`StorageError` → 自有） | 全面 | 中 |
| 10 | **显式 DAG 边与数据绑定**（x.engine 没有） | 新增层 §6.2 | 中高 |

**最好搬**：`evaluator.rs` 的 6 个纯判定函数、`x-types/execution.rs`、`race`/`try_catch`/`cancellation_scope`
**最难搬**：`handlers/step_block.rs`、`ensure_execution_tree`/`activate_pending_children`、`handlers/mod.rs`

### 5.6 新增内容

`store.rs`（DagStore trait + pi session 适配，~400）｜`exec/node.rs`（NodeExecutor，~300）｜`exec/scheduler.rs`（事件驱动推进，~400）｜`graph.rs`（显式 DAG 边与绑定，~300–500）｜`ir.rs`（门面类型，~200）

### 5.7 省钱路径

不搬 `expression`+`template`（省 1,900 行，改自写 ~200 行模板）｜不搬 `signal`/`worker`（省 180，先只做同步推进）｜不搬 `circuit_breaker`（省 438，先用 pi 退避）｜不搬缓存（省 222）

---

## 6. pi-dag 架构设计

### 6.1 crate 结构

```
crates/pi-dag/
  core/             ← 从 x.engine 裁剪（执行树 + 块推进语义，纯逻辑）
    execution.rs      NodeState / ExecutionNode / 判定函数
    block.rs          Block 定义（裁剪版）
    advance.rs        activate_pending_children / complete / fail / cancel_subtree
    composite/        parallel / race / router / loop / for_each / try_catch
  graph.rs          ← 新增：显式 DAG 边与字段绑定
  ir.rs             ← 新增：Dag / Node / Edge / Param（对 pi 友好的门面）
  extract/          ← 新增：轨迹 → IR（trace / deps / params）
  exec/
    scheduler.rs      事件驱动推进
    node.rs           NodeExecutor（Tool / Llm / Human）
  store.rs          ← 新增：DagStore trait + pi session 实现
  view.rs           Mermaid / DOT
```

### 6.2 核心类型（草图）

```rust
pub struct Dag { pub id: String, pub name: String, pub version: u32,
                 pub params: Vec<Param>, pub nodes: Vec<Node>, pub edges: Vec<Edge> }

pub struct Node { pub id: String, pub kind: NodeKind, pub handler: String,
                  pub params: Json, pub retry: Option<RetryPolicy>,
                  pub timeout_ms: Option<u64>, pub cache_key: Option<String>,
                  pub replay: ReplayPolicy,        // 复用 pi 的 never | safe
                  pub side_effects: Vec<EffectTag> }

pub enum NodeKind { Step, Parallel(Vec<String>), Join(String),
                    Branch { condition: String, then: String, otherwise: String },
                    Loop { condition: String }, ForEach { source: String },
                    TryCatch { catch: String, finally: Option<String> },
                    SubDag(String) }

pub struct Edge { pub from: String, pub to: String,
                  pub kind: EdgeKind,              // Data | Control
                  pub binding: Option<String> }    // 如 "steps.s1.output.items -> params.files"
```

### 6.3 关键接口

```rust
#[async_trait]
pub trait NodeExecutor {
    async fn execute(&self, node: &Node, ctx: &NodeContext, cancel: &CancellationToken)
        -> Result<NodeOutput, NodeError>;
}

#[async_trait]
pub trait DagStore {                     // 约 20 个方法
    async fn load_tree(&self, run_id: &str) -> Result<Vec<ExecutionNode>, StoreError>;
    async fn upsert_node(&self, node: &ExecutionNode) -> Result<(), StoreError>;
    async fn save_output(&self, run_id: &str, node_id: &str, out: &Json) -> Result<(), StoreError>;
    // … KV / 运行元数据 / 事件
}
```

**Pi 侧实现**：`PiToolExecutor`（包装 `harness/tools` 注册表 + 审批回调）、`PiLlmExecutor`（走 `pi-ai` 的 `Models::stream_simple`）、`PiSessionStore`（写 Custom entry + values）。

### 6.4 与 pi-agent 的接缝（只依赖，不侵入）

- 工具执行：复用 `AgentHarnessTool` + `beforeToolCall` 审批 + 截断
- 重放安全：复用 intent/settlement 与 `ReplayPolicy`
- 观测：发 `HarnessEvent`，接 telemetry
- 失败回退：DAG 节点触发一个 agent 子任务（lane）

### 6.5 持久化

- DAG 定义：会话内 `Entry::Custom`（随会话持久化、可 fork）+ 文件系统流程库（可分享，参考 skills/prompt-templates 的加载方式）
- 运行记录：pi-dag 自己的 `DagStore`（短期内存/JSONL），阶段 3 后迁到 session values
- **调度状态（若做定时）**：必须自己持久化 `next_fire_at`（定时器不是 durable 状态）

---

## 7. 路线图与验收

| 阶段 | 内容 | 周期 | 验收 |
|---|---|---|---|
| **0** | **收益验证**：3–5 个真实任务，手工 DAG vs 重跑 agent | 3–5 天 | token 下降 ≥30%、成功率不降 |
| **1** | **抽取骨架**：vendor 类型层 + 纯判定函数 + DagStore + step 执行接 pi 工具 | 1–1.5 周 | DAG JSON 驱动 pi 工具顺序执行成功 |
| **2** | **控制流**：router / try_catch / 重试 / 恢复 / 幂等对接 | 1–2 周 | 分支与失败重试用例通过；kill 后恢复不重复副作用 |
| **3** | **蒸馏器 + 审核视图**：轨迹 → IR + Mermaid + 参数表 + 副作用清单 | 1–2 周 | 3 个真实会话提取结果人工评审通过 |
| **4** | **失败回退闭环**：失败 → agent 修复 → 补丁建议 → 新版本 | 2 周 | 人为制造失败可被修复并产出补丁 |
| **5** | **并行与运维**：并行/循环 + 版本/回归/健康度/成本统计 | 2–4 周 | 并行组真并发；流程可回滚、有健康看板 |

**阶段 0 决定值不值得做；阶段 1 决定架构是否成立；阶段 4 决定产品能否长期活。**

### 建议的第一步（1–1.5 周）

1. 建 `crates/pi-dag`，vendor 类型层子集，`cargo check` 通过（1–2 天）
2. 搬 `evaluator.rs` 纯判定函数 + 单测（2–3 天，零风险起点）
3. `DagStore` trait + 内存实现（2–3 天）
4. `step`/`step_block` 执行 + `NodeExecutor` 接一个 pi 工具（3–5 天）

验收：同一份 DAG JSON 跑通"顺序 / 分支 / 重试 / 恢复"四类用例，结果与手写预期一致。

---

## 8. 风险登记册

**产品侧**

1. 蒸馏幻觉（把偶然当必要）→ 强制审核 + 试运行 + 二次验证
2. 漂移（外部系统变化）→ 健康度监控 + 失败自动降级 + 修复闭环
3. 成本倒挂 → 重复 ≥3 次才推荐蒸馏，显示成本对比
4. 预期错位 → 明确适用场景，主动建议"继续用 agent"
5. 维护成本（流程是个人/团队特定的，会漂移）→ 把维护做便宜，否则用户一两个月后弃用

**技术侧**

6. **测试资产流失**：x.engine 的 1334 个测试随裁剪大量失效 → 保留部分必须补测
7. **双状态机**：pi 的 operation 状态机 vs x.engine 的 instance/execution tree → 明确边界（DAG run 状态归 pi-dag，操作级 durability 仍走 pi）
8. **语义漂移**：删掉 CAS/并发限制/信号后行为变化 → 措辞用"取其语义"，不声称"复刻"
9. **License**：Apache-2.0 需保留版权与来源 commit
10. **上游无法同步**：分叉后不能直接 merge → `vendor/` + 来源说明 + 定期人工比对
11. **表达式引擎安全面**：若搬 `expression.rs`，需审计其可访问的数据范围（防越权读敏感数据）
12. **并发与 durability 组合**：DAG 节点级并发与 pi 的单 lane 串行语义冲突 → 三条映射路线（绕过 lane / 分支→lane / 节点→operation）需择一

---

## 9. 待决策清单

**产品**

1. 触发：只支持显式 `/distill`，还是允许自动提示/自动蒸馏？
2. 产物：混合图（保留 LLM 节点）还是尽量纯确定性？
3. 失败默认策略：严格停止 / **回退 agent** / 整体降级？（建议回退）
4. DAG 存哪：会话内 Custom entry / 文件库 / 两者兼有？
5. MVP 是否同意砍掉并行、定时、分享，只做「顺序 + 参数化 + 失败回退」？

**技术**

6. 抽取粒度：裁剪复刻（推荐）/ 借鉴重写 / 全量搬运（不建议）
7. 调度归属：挂 pi 的 operation 状态机（复用 durability，改 pi-agent）还是 pi-dag 自带轻量调度（不碰核心）？
8. DAG 与 agent 的关系：DAG 作为 agent 的**工具**（`dag_run`）还是**并行执行体**？（前者更符合 Pi「核心最小」）

---

## 附录 A：概念映射（x.engine → DAG → pi）

- `step` ↔ 工具/LLM/人工节点
- `parallel` / `race` ↔ 并行组 / 竞速组
- `router` ↔ 条件分支
- `loop` / `for_each` ↔ 循环 / 集合展开
- `try_catch` ↔ 错误恢复
- `sub_sequence` ↔ 子 DAG
- `cancellation_scope` ↔ 不可取消区（对应"已产生副作用不可撤销"）
- `cache_key` ↔ 节点级缓存键（成本优化抓手）
- `instance` / `execution tree` ↔ DAG run / 运行记录
- `signal`(pause/resume/cancel/update_context) ↔ pi 的 abort/cancel/steering
- `handler` 注册表 ↔ pi 的工具注册表
- `agent` / `llm` / `tool_call` / `mcp` handler ↔ **重叠，抽取时排除**

## 附录 B：x.engine 代码索引（抽取时定位用）

- 执行树语义：`x-engine/src/evaluator.rs`（1224 行，含 `evaluate` 三相位）
- 块分发：`x-engine/src/evaluator/dispatch.rs`（301 行）
- 复合块推进：`x-engine/src/handlers/{parallel,race,router,loop_block,for_each,try_catch,cancellation_scope}.rs`
- 节点执行：`x-engine/src/handlers/{step,step_block,step_dispatch}.rs`
- 类型定义：`x-types/src/{execution,sequence,context,instance,error,signal}.rs`
- **不抽**：`x-engine/src/scheduler.rs`、`scheduler/step_exec.rs`、`x-storage/`

## 附录 C：术语表

- **会话（Session）**：不可变 entry 树 + 绑定值 + 分支/车道 + 用量账本
- **轨迹（Trajectory）**：一次任务完成的完整记录（消息 + 工具调用 + 操作状态）
- **蒸馏（Distillation）**：把轨迹编译成 DAG 的过程
- **DAG run**：一次 DAG 执行实例（对应 x.engine 的 instance）
- **节点（Node）**：DAG 中的一步（工具调用 / LLM 判断 / 人工输入）
- **绑定（Binding）**：节点间的数据流（上游输出字段 → 下游参数）
- **replay 策略**：`never`（有副作用，崩溃后不重跑）/ `safe`（只读，可安全重跑）—— 来自 Pi
- **混合图**：确定性节点 + LLM 节点并存的工作流
- **回退（Fallback）**：DAG 失败时唤起 agent 接管修复

## 附录 D：相关过程稿

本次设计由以下过程稿整合而成，可保留作为背景材料：

- `pi-dag-analysis.md` —— 技术可行性与 x.engine 调研
- `pi-dag-product.md` —— 产品设想与生命周期
- `pi-dag-extraction-plan.md` —— 裁剪复刻的清单与工期评估

若不需要追溯推导过程，可只保留本文档。
