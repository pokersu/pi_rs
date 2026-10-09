# 从 x.engine 裁剪复刻 pi-dag：工作量与内容评估

> 目标：只取 x.engine 的 **DAG/工作流执行内核**，装进 pi 作为 `pi-dag` crate
> 数据来源：`../x.engine`（Apache-2.0，5 crate / 约 7.2 万行）逐文件统计（已剔除 `#[cfg(test)]` 段落）
> 结论先行：**搬运量 4.1k–8.5k 有效行，工期 3–10 周（视档位）**；真正的工作量不在"搬"，而在**剥离**与**重接**

---

## 0. 三档结论

| 档位 | 搬运内容 | 有效行 | 工期（单人，含测试） |
|---|---|---|---|
| **最小集**：顺序 + 分支 + try/catch + 重试 | 执行树语义 + step 执行 + 路由 | ~4,100 | **3–4 周** |
| **标准集**：+ 并行/竞速/循环/遍历 | 再并入 6 个复合块 | ~6,000 | **5–6 周** |
| **完整集**：+ 表达式/模板/缓存 | 再并入参数求值与缓存层 | ~8,500 | **8–10 周** |

> 均不含「轨迹提取器」（另计 1–2 周）与「与 pi 工具/durability 深度融合」（另计 1–2 周）。

---

## 1. 原始规模（有效代码，已去测试）

- `x-types`：**3,514** 有效行 / 31 文件
- `x-engine` 顶层：**6,022** 有效行 / 23 文件
- `x-engine/evaluator` + `scheduler`：**1,499**（`step_exec.rs` 1,083 / `dispatch.rs` 281 / `sla.rs` 135）
- `x-engine/handlers`：**7,870** 有效行 / 32 文件
- `x-engine/scheduling`：**277**
- `x-storage`：约 19.9k 行（**不搬**，但要写一个薄适配层）

关键事实：**实际只调用 `StorageBackend` 的 55 个方法**（接口本身有 207 个，分 10 个子 trait）。

---

## 2. 搬运清单

### 2.1 最小集（必搬）

**类型层（`x-types`，裁剪后约 1,000 行）**

- `sequence.rs`（992 → 裁到约 700）：Block 定义保留，删 `tenantId` / `status(Draft/Staging/…)` / `interceptors` / 版本发布语义
- `execution.rs`（122）：`ExecutionNode` / `NodeState` —— **几乎原样**
- `context.rs`（185 → 约 140）：`ExecutionContext` 的 data/config/audit/runtime 四分区
- `error.rs`（87）、`ids.rs`（288 → 约 120，删租户相关 id）、`instance.rs`（167，裁状态机到需要的子集）

**执行核心（`x-engine`，约 2,900 行）**

- `evaluator.rs`（800）：`ensure_execution_tree` / `evaluate` 三相位 / `complete_node` / `fail_node` / `cancel_subtree` / `activate_pending_children` + **纯判定函数**（`children_of`/`all_terminal`/`any_completed`/`all_completed`/`any_failed`/`has_waiting_nodes`）
- `evaluator/dispatch.rs`（281）：按 block type 分发
- `handlers/step.rs`（453）+ `step_block.rs`（545）：节点执行与块推进（**改造成本最高**，见 §3）
- `handlers/step_dispatch.rs`（250）
- `handlers/router.rs`（138）、`handlers/try_catch.rs`（123）
- `handlers/param_resolve.rs`（207）：参数模板求值的前置
- `handlers/util.rs`（110）、`handlers/mod.rs`（227，注册表——需改造）

### 2.2 标准集追加（约 1,900 行）

- `handlers/parallel.rs`（133）、`race.rs`（86）、`loop_block.rs`（300）、`for_each.rs`（431）、`cancellation_scope.rs`（57）
- `handlers/builtin.rs`（214）、`emit_event.rs`（203）
- `x-types/signal.rs`（135，若要 pause/resume/cancel 语义）
- `scheduling/delay.rs`（124，step 的 delay/jitter）
- `x-types/checkpoint.rs`（17）、`worker.rs`（45，若要"等待外部执行"）

### 2.3 完整集追加（约 2,500 行）

- `expression.rs`（1,004）+ `template.rs`（904）：条件表达式与 `{{ }}` 模板（**可替代**：自写简易替换约 200 行，见 §5）
- `sequence_cache.rs`（160）、`context_cache.rs`（62）
- `x-types/config.rs`（296 → 裁 150）、`filter.rs`（44）、`audit.rs`（30）

### 2.4 明确不搬

**整个 crate**：`x-storage`（19.9k）、`x-api`（1.5k）、`x-server`（63）

**调度与横切**（合计约 5,000 行）

- `scheduler.rs`（1,593）与 `scheduler/step_exec.rs`（1,083）—— tick loop / `claim_due_instances` / 并发信号量 / deadline / 信号队列 → **由 pi 的 drive/operation 替换**
- `metrics.rs`（105）、`webhooks.rs`（213）、`interceptors.rs`（116）、`preload.rs`（128）、`required_fields.rs`（141）、`gc.rs`（223）、`lint.rs`（819）、`credentials.rs`（310）、`cron.rs`（201）、`triggers.rs`（13）、`recovery.rs`（38）、`circuit_breaker.rs`（438，可选保留简化版）、`externalized.rs`（162）、`sla.rs`（135）

**handler 中与 pi 重叠或无关的**（合计约 4,250 行）

- `agent.rs`（619，pi 就是 agent 运行时）、`llm/*`（673）、`mcp.rs`（501）、`tool_call.rs`（359）、`memory.rs`（454）、`blob.rs`（228）、`activepieces.rs`（219）、`grpc_plugin.rs`（182）、`human_review.rs`（153）、`ab_split.rs`（143）、`query_instance.rs`（100）、`send_signal.rs`（96）、`self_modify.rs`（73）、`wasm_plugin.rs`（448）

---

## 3. 改造点清单（真正的成本所在）

按改造难度排序，这是工期估算的依据：

| # | 改造项 | 影响面 | 难度 |
|---|---|---|---|
| 1 | `StorageBackend`（207 方法 / 10 子 trait）→ `DagStore`（~20 方法） | 55 个调用点，集中在 `step_block.rs`(45)、`dispatch.rs`(17)、`evaluator.rs`(16) | 高 |
| 2 | `scheduler.rs` 的 tick loop → pi 的事件驱动推进 | 1,593 行整体替换；`Drive`/`operation` 接管 | 高 |
| 3 | `StepContext`（含 storage/instance/tenant/block）→ pi 执行上下文 | 所有 handler 的签名与内部取数 | 中高 |
| 4 | handler 注册表 → pi 工具 + LLM 适配器 | `handlers/mod.rs` + 调用点 | 中 |
| 5 | 多租户字段清除（`TenantId` 等） | `x-types` 多数文件 + 所有 handler | 中 |
| 6 | `metrics::`（47 处）→ 删除或接 pi telemetry | 散布在 handlers/evaluator | 中 |
| 7 | `lifecycle::`（30 处）→ 保留必要钩子，其余删 | 实例状态转移路径 | 中 |
| 8 | `webhooks::`(14) + `interceptors::`(10) + `externalized::`(7) → 删除 | 局部 | 低 |
| 9 | 错误类型统一：`EngineError`/`StepError`/`StorageError` → pi-dag 自有错误 | 全面 | 中 |
| 10 | 显式 DAG 边与数据绑定（**x.engine 没有**） | 新增层，见 §4 | 中高 |

**最容易搬的**（几乎零改造）：`evaluator.rs` 的 6 个纯判定函数、`x-types/execution.rs`、`handlers/{race,try_catch,cancellation_scope}.rs`（storage 调用为 0）。
**最难搬的**：`handlers/step_block.rs`（45 处 storage）、`evaluator.rs` 的 `ensure_execution_tree`/`activate_pending_children`（写执行树）、`handlers/mod.rs`（注册表与调度耦合）。

---

## 4. 新增内容（约 1,100–1,500 行）

| 模块 | 内容 | 行数 |
|---|---|---|
| `store.rs` | `DagStore` trait（读执行树/写节点状态/存输出/KV）+ pi session 适配实现 | ~400 |
| `exec/node.rs` | `NodeExecutor`：节点 → pi 工具 / LLM / 人工输入 | ~300 |
| `exec/scheduler.rs` | 事件驱动推进（替代 tick loop） | ~400 |
| `graph.rs` | **显式 DAG 边与字段绑定**（x.engine 缺失的一层） | ~300–500 |
| `ir.rs` | 面向 pi 的门面类型（Dag/Node/Edge/Param） | ~200 |

---

## 5. 可选的省钱路径

- **不搬 `expression.rs` + `template.rs`（省 1,900 行）**：自写约 200 行的 `{{path.to.value}}` 替换 + 简单比较运算，够覆盖 80% 的参数化场景；等真需要复杂表达式再引入
- **不搬 `signal`/`worker`（省 180 行）**：先只支持"同步推进"，不实现"等待外部 worker 回调"
- **不搬 `circuit_breaker`（省 438 行）**：先用 pi 的 retry 退避兜住
- **缓存（`sequence_cache`/`context_cache`，省 222 行）**：pi-dag 短期不需要热路径优化

---

## 6. 工期估算（单人，含调试与测试）

### 最小集：3–4 周

- 第 1 周：建 crate + vendor 类型层 + 编译打通（含大量删除）
- 第 2 周：`evaluator` 语义 + `DagStore` + 内存实现 + 纯函数单测
- 第 3 周：`step`/`step_block` 执行 + `NodeExecutor` 接 pi 工具 + 顺序执行端到端
- 第 4 周：`router`/`try_catch` + 重试 + 恢复 + 补测试

### 标准集：+2 周
并行/竞速/循环/遍历 + join 语义 + 并发下的状态一致性测试

### 完整集：+3–4 周
表达式/模板引擎 + 缓存 + 信号 + 压力测试

---

## 7. 风险与注意事项

1. **测试资产会随裁剪流失**：x.engine 号称 1334 个测试，裁剪后大部分失效。保留部分**必须补测试**，否则等于接手一堆未验证代码。
2. **双状态机协调**：pi 已有 `operation` 状态机（meta/state、全量替换、terminal 结果），x.engine 有 `instance` + `execution tree`。两套并存需要明确边界（建议：DAG run 的状态由 pi-dag 持有，操作级 durability 仍走 pi）。
3. **语义漂移**：删掉 CAS、并发限制、信号队列后，行为与原版不同——**不要声称"复刻 x.engine"，而是"取其语义"**。
4. **License/NOTICE**：x.engine 是 Apache-2.0，搬运需保留版权与来源声明；建议记录来源 commit（便于日后比对）。
5. **上游无法同步**：一旦分叉，x.engine 的后续修复不能直接 merge → 用 `vendor/` 目录 + 来源说明，定期人工比对。
6. **表达式引擎的安全面**：若搬 `expression.rs`，需审计它能访问什么（x.engine 的表达式可读 `outputs/context`，在 agent 场景下要防止越权读敏感数据）。

---

## 8. 建议的落地顺序（每步可独立验证）

1. 建 `crates/pi-dag`，vendor 类型层子集，`cargo check` 通过（1–2 天）
2. 搬 `evaluator.rs` 纯判定函数 + 单测（2–3 天）—— **零风险起点**
3. 写 `DagStore` trait + 内存实现（2–3 天）
4. `step`/`step_block` 执行 + `NodeExecutor` 接一个 pi 工具（3–5 天）
5. `router` + `try_catch` + 重试（2–3 天）
6. 恢复 + 幂等 + 对接 pi 的 `replay: never|safe`（3–5 天）
7. 并行/竞速/循环/遍历（5–7 天，进入标准集）
8. 表达式/模板（可选，3–5 天）

**验证方式**：每步结束时，用同一份 DAG JSON 跑"顺序/分支/失败重试/恢复"四类用例，结果与手写预期一致。

---

## 9. 与其它方案的一句话对比

- **全量搬运**：42k+ 行（含 x-storage），还要养 DB/多租户/HTTP → 工期翻倍，收益不增
- **完全自研**：约 2k 行，少搬但要在实践中重新踩一遍并发/恢复/取消的坑
- **裁剪复刻（本方案）**：4.1k–8.5k 行，**拿工业级语义、弃无关包袱** → 推荐

---

## 10. 建议的下一步

先做 §8 的第 1–4 步（约 1–1.5 周，最小集的一半），得到一个"能用 DAG JSON 驱动 pi 工具并顺序执行"的骨架。这一步就能验证三件事：搬运是否顺畅、`DagStore` 抽象是否够用、与 pi 工具/审批的接缝是否自然。**验证通过再决定是否继续投入并行与表达式层。**
