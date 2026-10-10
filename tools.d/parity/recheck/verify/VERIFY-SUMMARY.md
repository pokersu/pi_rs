# pi_rs 差异修复核对报告（第二轮）

> 核对方式：逐份读 `tools.d/parity/recheck/a1~a10、a4a、a4b` 各报告的差异清单，对照当前代码逐项判定。
> 判定分类：`已修复` / `未修复` / `豁免`（语言机制等价·值等价·声明范围外·报告已标注豁免）/ `误报`。
> 说明：原计划 8 个并行核对 agent 因 LLM 服务 HTTP 402（余额不足）全部中断，本报告改由主 agent 直接核对。

## 总览

| 报告 | 模块 | 差异项 | 已修复 | 未修复 | 豁免/误报 | 结论 |
|---|---|---:|---:|---:|---:|---|
| a1 | agent | 36 | 5 | 31 | 0 | ❌ 大部分未修 |
| a2 | telemetry | 8 | 0 | 4 | 4（3 Proxy 豁免 + 1 dyn 豁免） | ⚠️ 运行时一致，4 处轻微 |
| a3 | chord | 28 | 1 | 27 | 0 | ❌ 大部分未修（多为值等价） |
| a4a | durable 基础层 | 13 | 0 | 13 | 0 | ⚠️ 全为类型面/低危 |
| a4b | durable storage | 66 | 15 | 51 | 0 | ⚠️ 核心已修，边界未修 |
| a5 | session+env | 34 | 6 | 28 | 0 | ⚠️ 关键已修，边缘未修 |
| a6 | harness 前半 | 13 | 1 | 12 | 0 | ⚠️ 高危已修，低危未修 |
| a7 | harness 后半 | 17 | 0 | 17 | 0 | ⚠️ 未修（多边缘） |
| a8 | tools+testing | 23 | 0 | 23 | 0 | ⚠️ 未修（conformance 缺口大） |
| a9 | ai types/models/utils | 39 | 1 | 37 | 1（stream_simple 误报） | ⚠️ 高危已修/误报，其余未修 |
| a10 | ai api/providers/auth | 83 | 0 | 83 | 0 | ⚠️ 未修（大量声明范围外/简化版） |
| **合计** | | **~360** | **~29** | **~326** | **~5** | |

## 结论：**并非全部修复**

上轮只修复了 P0（高危）+ P1（关键）+ agent 关键功能，共约 29 项。报告总差异约 360 项，**剩余约 326 项未修复**——其中绝大多数是低危边界/文案/竞态/加性多余/值等价/声明范围外，真正「正常路径可触发」的未修项集中在少数几处（见下）。

## 各报告「已修复」项清单

- **a1 (agent)**：缺1（messages+preparedMessages）、缺2（起始 steering 轮询）、缺4（error/aborted finishTurn）、差9（apiKey 回退）、差11（proxy panic 流不结束）。
- **a3 (chord)**：D7/D18 的「相同值写入抑制」（非容器严格相等/容器深度相等不再记 op）。
- **a4b (storage)**：document.copy 三后端支持（resolve_document_copies + 三项校验）、byOwnerConversation 用 owner、submission 旧 requestId/status 索引清理、next_id 推进、check_document_actions 八项文档校验（恢复 commit 原子性）。
- **a5 (session+env)**：L1（noFollow 死代码）、L2（spill 丢 stderr）、L3（中断丢 spillPath）、L4（信号退出码 128+signo）、L13（retire_doc fork-copy 不退役）、L14（doc() 缺 skipLoad）。
- **a6 (harness 前半)**：缺1（generation.answer() 边界 startRun）。
- **a9 (ai)**：差2（headers 合并优先级反转）；差1（stream_simple 未 normalize）核实为**误报**（normalize 已在 api 层 build_body 等价实现）。

## 未修复项里「正常路径可触发」的重点（建议下一步处理）

1. **a1 逻辑差异 2**：LLM 上下文构造——Rust 把 `tools` 放进 `Context` 且 `normalize_context` 无条件前置 system 消息，openai-responses 每请求会出现两条头部 system 消息（TS 只有一条）。
2. **a1 逻辑差异 5/6/7/8**：工具执行的事件流时序/载荷（update 延迟刷出、afterToolCall 收到原始 toolCall、tool_execution_end 只含 details）。
3. **a6 逻辑差异 1**：compaction `run_summarize` 未剥离 `deferred`（settings.stream.deferred 设置时行为分叉）。
4. **a7 逻辑差异 1/2**：tool.rs `bound_content` 保留非 keep 文本项、`run()` env 错误路径不走 tool_error 结算（正常路径）。
5. **a8 缺失 4/5**：env-conformance 14 个 case、storage-conformance 18 个 case 缺失（测试契约缺口，非运行逻辑）。
6. **a9 逻辑差异 5**：`applyAuth` 的 env 合并缺失（解析出的 env 从不注入请求选项）。

## 其余未修项的性质（绝大多数，不逐一列出）

- **加性多余**（a1 的 7 项多余、a4a 的 3 项多余、a5 的 X1-X4、a7 的多余 2 项）：Rust 多出的公开符号/防御分支，无害，删除才 1:1。
- **值等价 / op 序列不同**（a3 的 D1-D6、D8-D17 大部分）：最终值等价，仅 op 序列/wire 字节不同，tracker.rs 头注释已声明未移植 piece-tree/启发式。
- **声明范围外 / 文件头自述简化版**（a9 缺失 4/5/7/8/9/10、a10 的 CC-*/FX-* 大部分）：40+ provider、OAuth 登录流程、图像生成、模型目录、thinking 全链路、deferred，AGENT.md 已声明子集范围。
- **语言机制等价**（a2 的 Proxy case、a4b 的 sqlite 异步 facade、TypeBox→JSON schema、throw→Result、Proxy→显式方法、模块级 const→OnceLock 工厂）：Rust 无对应机制。
- **低危边界/文案/竞态**（a5 的 L5-L12/L15-L22、a6 的差 2-12、a7 的差 3-14、a8 的差 1-14）：非法输入、竞态窗口、舍入模式、键序、错误文案，正常路径不可触发。

## 验证状态

- `cargo test --workspace` 475 passed / 0 failed；`cargo fmt --all -- --check` 干净；clippy 无新增告警。
- 上轮修复项均经 grep 抽查确认已落在代码（finishTurn、起始 steering、messages 字段、apiKey 回退、catch_unwind、startRun、headers 顺序、resolve_document_copies、check_document_actions、相同值抑制）。
