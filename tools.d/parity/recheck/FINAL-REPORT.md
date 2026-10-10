# pi_rs ↔ upstream 方法级 1:1 复刻审计报告（第二轮全量重审 + 修复进展）

> 基线：上游 `v1.1.0` @ `abe508e1b`（`upstream/packages/`）
> 更新日期：2026-10-10（含三轮修复进展与最终剩余差异）
> 方法：11 个独立审计 agent 并行，逐文件逐符号**逐方法**双向对比（TS→Rust 查缺失 + Rust→TS 查多余）。
> 判定分类：`缺失`（TS 有 Rust 无）/ `多余`（Rust 有 TS 无）/ `逻辑差异`（两边都有但行为不同）/
> `命名差异`（允许的 Rust 风格，不算问题）/ `豁免`（语言机制等价差异）。
> 明细报告见 `tools.d/parity/recheck/` 下 `a1`~`a10`、`a4a`、`a4b` 各目录的 `report.md`。

## 审计方式

- 基线：上游 `v1.1.0` @ `abe508e1b`（`upstream/packages/`）
- 11 个独立审计 agent 并行，逐文件逐符号**逐方法**双向对比，产出 11 份明细报告 + 本汇总。
- 明细报告目录：`tools.d/parity/recheck/`（`a1`~`a10`、`a4a`、`a4b`）。

## 总结论：并非 1:1

| 模块 | 缺失 | 多余 | 逻辑差异 | 结论 |
|---|---:|---:|---:|---|
| agent → pi-agent-core | 8 | 7 | 21 | ❌ 非 1:1 |
| telemetry → pi-telemetry | 3* | 1* | 4 | ✅ 运行时 1:1（*=Proxy 豁免） |
| chord → pi-durable/chord | 6 | 4 | 18 | ❌ 非 1:1 |
| durable 基础层 | 5 | 3 | 5 | ⚠️ 核心等价（多为类型面/serde 形态） |
| durable storage | 29 | 4 | 33 | ❌ 非 1:1（含功能缺口） |
| durable session+env | 8 | 4 | 22 | ❌ 非 1:1 |
| durable harness 前半 | 1 | 0 | 12 | ⚠️ 基本 1:1（1 处高危） |
| durable harness 后半 | 1 | 2 | 14 | ⚠️ 基本 1:1 |
| durable tools+testing | 6 | 3 | 14 | ❌ 未达 1:1 |
| ai types/models/utils | 15 | 4 | 20 | ❌ 未 1:1（2 处高危） |
| ai api/providers/auth | 32 | 7 | 44 | ❌ 未 1:1（部分为声明「简化版」） |

只有 **telemetry** 运行时逻辑完全一致。累计约 **缺失 103 / 多余 39 / 逻辑差异 207** 项。

## 已人工复核确认的高危项（修复状态见下节）

1. **`document.copy` 在 jsonl/sqlite 后端被直接拒绝** —— ✅ 已修复（P0-4）。
2. **`generation.answer()` 漏了边界启动** —— ✅ 已修复（P0-1）。
3. **`stream_simple` 未 `normalize_context`** —— ⚠️ 核实为**误报**：normalize 已在 provider 的 api 层
   `build_body`（`openai-responses.rs:600`）等价实现，无需修复。
4. **headers 合并优先级反转** —— ✅ 已修复（P0-3）。

此外，storage 的 `commit` 文档语义校验缺失（导致非原子）与 `byOwnerConversation` 索引错位等
可见性查询语义 —— ✅ 已修复（P0-5）。

## 修复进展（三轮，共约 38 项）

验证：`cargo test --workspace` 475 passed / 0 failed；`cargo fmt --all -- --check` 干净；
clippy 仅 pi-durable 7 处历史告警（无新增），pi-ai / pi-agent-core / pi-telemetry 0 告警。

### 第一轮（P0 / P1 / P2，29 项）

- **P0（高危）**：generation.answer() 边界 startRun；headers 合并优先级；document.copy 三后端支持
  （共享 resolve_document_copies + 三项校验 + StorageRejected 包装）；storage 可见性（owner 索引/
  submission 索引清理/next_id 推进）+ check_document_actions 八项文档校验恢复 commit 原子性。
- **P1（关键）**：session/env 六处（noFollow 死代码、spill 丢 stderr、中断丢 spillPath、信号退出码
  128+signo、retire_doc fork-copy 不退役、doc() 缺 skipLoad）；chord tracker 相同值写入抑制。
- **P2（agent 关键功能）**：error/aborted 上补 finishTurn、AgentLoopTurnUpdate.messages + preparedMessages、
  起始 steering 轮询、apiKey 回退、proxy 协议违规 panic → error 事件流正常结束。

### 第二轮（正常路径可触发的 5/6 处）

- **修1（a1 差2）**：`llm_context` 不再传 `tools`（工具声明已在 fold 时折进 messages，避免 pi-ai 重复前置）；
  `run_agent_loop_continue` 补 `fold_initial_system_message`。
- **修2（a1 差5/6/8）**：update 事件改执行期间即时 emit + panic 时也刷出；update `args` 用原始
  toolCall.arguments；`tool_execution_end.result` 改完整 `AgentToolResult` 并补 `duration_ms`。
  （a1 差7「afterToolCall 收到原始 toolCall」核实为误报——Rust 解构后传的是 prepared.toolCall。）
- **修3（a6 差1）**：compaction `run_summarize` 剥离 `deferred`。
- **修4（a7 差1/2）**：tool.rs `bound_content` 丢弃非 keep 文本项；`run()` 的 env 构建错误改走
  tool_error + failed 结算，`duration_ms` 只计 execute 耗时。
- **修6（a9 差5）**：`ProviderRequestOptions` 补 `env` 字段 + 三处 `merge_env`（请求选项覆盖解析结果）。

### 第三轮（a8 的 4 项运行逻辑缺失）

- **bash**：新增 `BashPrepare` 类型、`BashToolOptions.prepare`、`BashExecution.env`/`inherit_env`；
  `prepare_execution` 改 async 并调用钩子；`run_command` 改用 execution.env/inherit_env。
- **PowerShell**：新增 `PowerShellToolOptions{command_prefix, prepare, programs}`，`programs` 可配置。
- **read**：`TruncationDetails` 补 `max_lines`/`max_bytes`。
- **tools 导出**：re-export `BashExecution`/`BashPrepare`/`PowerShellToolOptions`/`EditToolDetails`/`ReadToolDetails`。

## 剩余差异

### 唯一「明确」剩余：conformance 测试 case（纯测试用例，32 个）

- **env-conformance**：缺 14 个 TS case（8 watch、3 dir-reader、windowed exec、2 symlink）。
- **storage-conformance**：缺 18 个 TS case（含全部 8 个 document 契约 case、ID 耗尽、fork 历史、
  deep-fork 扫描等）。

性质：**测试契约覆盖缺口，非运行逻辑**（a8 报告自述）。补齐需翻译 2000+ 行 TS 断言到 Rust 并适配后端 API。

### 其余（约 300 项，绝大多数无需或不宜修）

- **加性多余**（约 20 项）：Rust 多出的无害公开符号/防御分支（如 a1 的 7 项多余、a5 的 X1-X4、
  a7 的多余 2 项），删除才严格 1:1，不影响行为。
- **值等价 / op 序列不同**（chord D1-D17 大部分）：最终值等价，仅 op 序列/wire 字节不同，
  tracker.rs 头注释已声明未移植 piece-tree/启发式。
- **声明范围外 / 文件头自述简化版**（a9/a10 的 40+ provider、OAuth 登录流程、图像生成、模型目录、
  thinking 全链路、deferred）：AGENT.md 已声明子集范围。
- **语言机制等价**（Proxy、TypeBox→JSON schema、throw→Result、sqlite 异步 facade、模块级 const→OnceLock）。
- **低危边界/文案/竞态**（非法输入、竞态窗口、舍入模式、键序、错误文案）：正常路径不可触发。
  例如 agent 的 runToolCall 钩子 context.tools、proxy 若干增量事件边界；durable 的 findLatestHeadMarker
  段优先 vs 全局最新（sqlite）、未知会话静默返回、scanDocuments 游标 order、limit=0 边界。

## 明细入口

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
- `verify/VERIFY-SUMMARY.md` — 第二轮修复核对结论
- `FIXES.md` — 逐项修复记录

## 说明与局限

- 审计轮为**静态源码逐方法比对**，未运行 cargo test（共享 checkout 下子代理的 bash 被写锁拒绝）。
  修复后已运行 `cargo test --workspace`（475 passed）与 `cargo fmt`/`clippy`。
- ai 模块为「声明子集」，其中部分缺失为文件头自述的「简化版」，属声明范围内的已知取舍。
- 部分差异仅影响非法输入/竞态/文案（低危），已在各报告分级标注。
