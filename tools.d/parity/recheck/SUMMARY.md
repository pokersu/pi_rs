# pi_rs ↔ upstream 方法级 1:1 复刻审计汇总（第二轮重审）

> 基线：上游 `v1.1.0` @ `abe508e1b`（`upstream/packages/`）
> 方法：11 个独立审计 agent 并行，逐文件逐符号逐方法对比（正向 TS→Rust 查缺失 + 反向 Rust→TS 查多余）。
> 判定分类：`缺失`（TS 有 Rust 无）/ `多余`（Rust 有 TS 无）/ `逻辑差异`（两边都有但行为不同）/
> `命名差异`（允许的 Rust 风格，不算问题）/ `豁免`（语言机制等价差异）。
> 明细见 `tools.d/parity/recheck/a1` ~ `a10`、`a4a`、`a4b` 各目录 `report.md`。

## 结论：并非 1:1

11 个模块中，只有 **telemetry** 运行时逻辑完全一致；其余模块均存在真实缺失/多余/逻辑差异。
累计：**缺失 ~103 项、多余 ~39 项、逻辑差异 ~207 项**（含大量低危边界项，也含数处高危功能缺口）。

| 模块 | 复刻目标 | 缺失 | 多余 | 逻辑差异 | 结论 |
|---|---|---:|---:|---:|---|
| agent → pi-agent-core | 完整（6 文件） | 8 | 7 | 21 | ❌ 非 1:1 |
| telemetry → pi-telemetry | 完整（6 文件） | 3* | 1* | 4 | ✅ 运行时 1:1（*=Proxy 豁免） |
| chord → pi-durable/chord | 子集（8 文件） | 6 | 4 | 18 | ❌ 非 1:1 |
| durable 基础层（types/documents/…） | 完整 | 5 | 3 | 5 | ⚠️ 核心等价（差异多为类型面/serde 形态） |
| durable storage（memory/jsonl/sqlite） | 完整 | 29 | 4 | 33 | ❌ 非 1:1（含功能缺口） |
| durable session+env | 完整 | 8 | 4 | 22 | ❌ 非 1:1 |
| durable harness 前半（agent…output） | 完整 | 1 | 0 | 12 | ⚠️ 基本 1:1（1 处高危） |
| durable harness 后半（prompt…scheduler） | 完整 | 1 | 2 | 14 | ⚠️ 基本 1:1（差异多为边缘） |
| durable tools+testing | 完整 | 6 | 3 | 14 | ❌ 未达 1:1（conformance 缺口大） |
| ai types/models/utils | 声明子集（44） | 15 | 4 | 20 | ❌ 未 1:1（含 2 高危） |
| ai api/providers/auth | 声明子集（44） | 32 | 7 | 44 | ❌ 未 1:1（部分为声明「简化版」） |

## 高危项（建议优先修复）

1. **storage 三后端 `document.copy` 不可提交**（a4b）：Rust jsonl/sqlite 直接拒绝 `document.copy`
   （"must be resolved to document.create before commit"），但 Session 实际会下发 `DocumentCopy`
   （transaction.rs:1767）→ 分叉/拷贝会话文档功能在 jsonl/sqlite 后端失效。
2. **storage commit 文档语义校验缺失**（a4b）：退休/版本转换/多内容命令/双活化身等校验在 memory/jsonl
   缺失或挪到应用阶段，导致 commit **非原子**（批次中途失败留下部分状态/磁盘污染）。
3. **storage 可见性查询语义错位**（a4b）：`byOwnerConversation` 索引用 `parent` 而非 `owner`；
   `findLatestHeadMarker` 段优先 vs 全局最新；未知会话静默返回空页 vs TS 抛错；`scanDocuments`
   游标 order 语义与 TS 相反。
4. **harness generation.answer() 漏掉边界启动**（a6）：`if (users.length > 0) await startRun(...)` 分支
   丢失 → 边界选中的用户提交停在 `placed` 永不运行。
5. **pi-ai stream_simple 未 normalize_context**（a9）：TS 先折叠 systemPrompt/tools 再分发，Rust 直传
   原始 Context（AGENT.md 声称"已切换"与代码不符）；headers 合并优先级反转（认证覆盖调用方）。
6. **session/env 关键缺陷**（a5）：noFollow 死代码失效；retire_doc fork-copy 分支不置 retire 标记；
   doc()/acquire 缺 skipLoad（复用退役中化身 tracker）；exec spill 落盘丢 stderr。
7. **agent loop**（a1）：`AgentLoopTurnUpdate.messages` 字段与 preparedMessages 处理整体丢失；
   error/aborted turn 上 `finishTurn` 钩子未调用；runLoop 起始 steering 轮询缺失。
8. **chord tracker op 压缩缺失**（a3）：no-op 抑制 / a+t 压缩 / readd / dense region / 排列归一化未移植，
   导致线上 op 与 TS 字节级不同；`adopt` 无 stale/consumed/aborted 校验；source listener panic 无隔离。

## 各模块明细入口

- `a1/report.md` — agent（缺 8/多 7/差 21）
- `a2/report.md` — telemetry（运行时一致，仅 3 Proxy case 豁免 + 4 断言/时序弱化）
- `a3/report.md` — chord（缺 6/多 4/差 18）
- `a4a/report.md` — durable 基础层（缺 5/多 3/差 5）
- `a4b/report.md` — durable storage（缺 29/多 4/差 33）
- `a5/report.md` — durable session+env（缺 8/多 4/差 22）
- `a6/report.md` — durable harness 前半（缺 1/差 12）
- `a7/report.md` — durable harness 后半（缺 1/多 2/差 14）
- `a8/report.md` — durable tools+testing（缺 6/多 3/差 14）
- `a9/report.md` — ai types/models/utils（缺 15/多 4/差 20）
- `a10/report.md` — ai api/providers/auth（缺 32/多 7/差 44）

## 说明与局限

- 本轮为**静态源码逐方法比对**，未运行 cargo test（共享 checkout 下子代理的 bash 被写锁拒绝）。
  各报告均已标注「结论为源码级静态审计，动态行为未实测」。
- ai 模块为「声明子集」，其中部分缺失为文件头自述的「简化版」（如 openai-completions 的 thinking
  全链路、faux 的 deferred），属声明范围内的已知取舍，但也存在声明范围内未声明的高危差异（a9 第 5 条）。
- 部分差异仅影响非法输入/竞态/文案（低危），已在各报告分级标注。
