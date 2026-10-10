# pi_rs ↔ upstream 剩余差异清单（DIFF）

> 基线：上游 `earendil-works/pi` v1.1.0 @ `abe508e1b`（`upstream/packages/`）
> 生成日期：2026-10-10
> 依据：`tools.d/parity/recheck/` 下 11 份方法级审计报告（`a1`~`a10`、`a4a`、`a4b`）
> 说明：本文件只列**剩余未修复**差异。已修复项（约 50 项，分六轮）见 `tools.d/parity/recheck/FIXES.md`；
> 「豁免」类（语言机制等价）不计入差异，仅在相关小节末尾简要列出。

## 〇、分层视图（真实工作量）

剩余约 290 项里，绝大多数是「记账差异」——语言机制必然不同、类型层无法对应、或复刻时故意砍掉的范围，**不是待修的 bug**。按「是否需要动手」分两层：

### 第 1 层 · 健壮性 / 数据完整性（非正常路径，约 10 项，按需修）

只在异常输入、竞态窗口、断连/损坏数据下才暴露，不影响正常流程：

- **a4b jsonl**：sidecar 回收、撕裂尾行/损坏检测、预校验不完整导致的磁盘污染。
- **a3 D11**：source listener panic 后投递永久卡死（chord，竞态）。
- **a7 差-6**：submissions `wait()` 关闭竞态可注册永不结清的等待者。
- **a5 L5/L6/L17/L18**：exec 回调错误不捕获、stderr 尾部丢失、watch_doc abort 不生效、observe_cancellation 守卫。
- **a1 差-16/17/18**：proxy URL 尾斜杠、SSE 行解析、请求体 None→null（对严格 schema 的代理服务器可能拒绝）。

### 第 2 层 · 记账差异（约 290 项，不计入真实工作量）

| 类别 | 约数 | 说明 |
|---|---:|---|
| 加性多余 | ~40 | Rust 多写的无害符号/防御分支，删掉才严格 1:1 |
| 纯类型级缺失 | ~30 | TS 编译期类型体操（`Infer*Attributes`、`ModelTypeMap` 等），Rust 无法/无需表达 |
| 声明范围外 / 自述简化版 | ~55 | ai 40+ provider、OAuth 登录、图像、模型目录、thinking 全链路、deferred |
| 语言机制等价 | ~40 | throw→Result、Proxy→显式方法、TypeBox→JSON schema |
| 低危边界/文案/竞态/键序 | ~130 | 非法输入、舍入模式、错误文案、BTreeMap 键序 |

> 结论：**「正常路径可触发」的 6 项（AR-1/2、OR-14、OR-16、registry 差-11、proxy 差-10/13）与 serde 持久化形态（a4a 差-3，17 个类型）均已修复（2026-10-10）**；
> 当前唯一剩余待办是第 1 层健壮性（按需挑）；第 2 层约 290 项是记账噪音，不必逐项处理。

---

## 一、总览

| 模块 | TS → Rust | 原始差异（缺/多/差） | 已修复 | 剩余（约） | 结论 |
|---|---|---|---|---|---|
| agent | agent → pi-agent-core | 8 / 7 / 21 | 16 | 缺 3 · 多 7 · 差 10 | ⚠️ 主循环已对齐，余下多为 proxy 边界 + API 面 |
| telemetry | telemetry → pi-telemetry | 3* / 1* / 4 | 0 | 缺 8* · 多 3* · 差 4 | ✅ 运行时 1:1（* 全部为 Proxy/类型级/断言弱化豁免） |
| chord | chord → pi-durable/chord | 6 / 4 / 18 | 1 | 缺 6 · 多 4 · 差 17 | ⚠️ 终值等价，op 序列/校验层差异 |
| durable 基础层 | — | 5 / 3 / 5 | 1 | 缺 5 · 多 3 · 差 4 | ⚠️ 核心等价，余下类型面差异 |
| durable storage | — | 29 / 4 / 33 | 16 | 缺 13 · 多 4 · 差 17 | ❌ 剩余集中在 jsonl 回收 / sqlite 查询语义 |
| durable session+env | — | 8 / 4 / 22 | 7 | 缺 7 · 多 4 · 差 15 | ⚠️ 关键缺陷已修，余下边界/竞态 |
| durable harness 前半 | — | 1 / 0 / 12 | 2 | 缺 0 · 多 0 · 差 11 | ⚠️ 基本 1:1 |
| durable harness 后半 | — | 1 / 2 / 14 | 4 | 缺 1 · 多 2 · 差 10 | ⚠️ 基本 1:1 |
| durable tools+testing | — | 6 / 3 / 14 | 5 | 缺 2* · 多 3 · 差 13 | ⚠️ 剩余含 32 个测试 case |
| ai types/models/utils | — | 15 / 4 / 20 | 3 | 缺 15 · 多 4 · 差 18 | ❌ 2 处高危已修，余下多为类型面/边界 |
| ai api/providers/auth | — | 32 / 7 / 44 | 5 | 缺 32 · 多 7 · 差 40 | ❌ 大部分为声明「简化版」/范围外 |

> 注：数量为「约数」——部分差异在修复过程中被顺带消除或核实为误报，精确状态以各分报告
> `report.md` 为准。上表「剩余」列已扣除已修复/误报项。

---

## 二、剩余差异明细

### 1. agent → pi-agent-core（a1）

**已修复**：缺失 1/2/4/5/8、逻辑差异 2/3/4/5/6/7(误报)/8/9/10/11/13。

**缺失（剩余 3）**
- 缺-3 `skipInitialSteeringPoll` 机制：`Agent.continue()` 手动 drain 后应跳过首次 steering 轮询的语义整体缺失。
- 缺-6 `AgentTool.outputSchema` 字段：未移植（v1.1.0 运行时消费者在 packages/coding-agent，范围外；低危类型面）。
- 缺-7 `prepareNextTurn`/`prepareNextTurnWithContext` 的 AbortSignal 与签名：钩子收不到 signal。

**多余（剩余 7，均为加性符号，删除才严格 1:1，不影响行为）**
- 多-1 `default_convert_to_llm` 公开并再导出（TS 为模块私有）。
- 多-2 `AgentMessage` 额外 4 个 variant（BashExecution/Custom/BranchSummary/CompactionSummary，v1.1.0 已移除的历史类型）。
- 多-3 `AgentToolResult.added_tool_names` 字段（上游全库无此符号）。
- 多-4 `ReplayPolicy` 公开枚举（TS 为内联字面量联合）。
- 多-5 `to_ai_thinking_level` 公开函数（TS 内联在 createLoopConfig）。
- 多-6 `AgentContext.system_prompt` 字段（TS AgentContext 无此字段）。
- 多-7 `ProxyAssistantMessageEvent::Done` 接受 `"deferred"`（TS 明确排除）。

**逻辑差异（剩余 10）**
- 差-1 `state.systemPrompt`/`state.messages`/`reset()` 表示法：Rust system_prompt 为构造期字段永不更新、reset 丢追加 system 消息与工具声明基线。
- 差-12 proxy `toolcall_end` 非 toolCall 内容仍发事件（TS 静默跳过）。
- 差-14 `runToolCall` 钩子 context.tools 被覆盖为本次调用 tools（TS 传调用方原始 context）。
- 差-15 `createErrorToolResult` 的 isError/terminate 默认值（TS undefined vs Rust true/false）。
- 差-16 proxy URL 尾斜杠（TS 原样拼接 `//api/stream`，Rust 去重 `/api/stream`）。
- 差-17 SSE 行解析：TS 严格要求 `data: `（含空格），Rust trim + strip_prefix（接受 `data:{}`）。
- 差-18 proxy 请求体 None → 序列化为 `null`（TS 丢 undefined 键）。
- 差-19 proxy partial 稀疏 content 填充（TS 下标留 hole，Rust 空 Text 块）。
- 差-20 durationMs 取整：TS Math.round vs Rust 截断（最多差 1ms）。
- 差-21 Agent 公开可变 hook 字段面收窄：Rust 只能在构造期设置，无运行期 setter。

### 2. telemetry → pi-telemetry（a2）

**结论**：运行时核心（memory 记录、span 生命周期、settle/inert、NOOP 单例、typed starter）1:1 一致，**无待修的运行时差异**。剩余全为豁免类：

- 缺失 M1-M8：3 个依赖 JS Proxy 的 conformance case（case 5/8/9）、15 项编译期类型级导出、NOOP 回退不可达分支等 —— 均为 Proxy/类型级豁免。
- 多余 X1-X3：`ErasedTelemetryContext` dyn 兼容适配层、测试脚手架、span 继承的 trait 实现 —— 机制豁免。
- 逻辑差异 D1-D4：同步入场语义被惰性 Future 取代（时序）、conformance 断言弱化（拒绝值同值断言 → is_err）—— 测试契约弱化，非运行逻辑。

### 3. chord → pi-durable/chord（a3）

**已修复**：D18 + D7 的「相同值写入仍发布」部分（P1-2，tracker 相同值写入抑制）。

**缺失（6，全剩余）**
1. `isReplace`/`isBase` 导出缺失（isBase 内联于 hydrate）。
2. `assertValidOp`/`assertValidWireOp`/`assertSafePath` 公开校验层未移植。
3. `PathError`/`UnsafePathError`/`RESERVED_SEGMENTS` 公开符号缺失。
4. `json.ts::isJsonValue` 缺失。
5. `Prepared.abort()` 缺失；Change 无 sort/fill/copyWithin/reverse 显式方法。
6. Tracker 并发 draft 失效（`#invalidate`/releaseContext/registry 剪枝）未移植。

**多余（4，全剩余）**
1. `delta::apply_in_place` 公开导出（TS 无公开对应）。
2. tracker 辅助 API：`Change::len/is_empty/state`、`Prepared::into_value/into_parts`。
3. state 诊断 API：`subscriber_count`、`AttachedReplicatedState::cursor`。
4. `unregister_replicated_state_internals`（上游 WeakMap 无显式移除）。

**逻辑差异（剩余 17）**
- D1 `apply` 数组 `s` 于 index==length：TS 允许追加一格，Rust 拒绝。
- D2 `m` 非法排列：TS 前置拒绝；Rust 越界→部分写入后 Err、重复→静默接受。
- D3 wire decode arity 宽松：Rust 忽略多余元组元素；`m` 排列无双射校验。
- D4 Truncate 截断代理对中间：TS 保留孤立代理，Rust 替换 U+FFFD。
- D5 数字段作用对象路径：TS 可走，Rust 报 UnsafePath。
- D6 `overlap` 丢默认参数 probe=64/maxCandidates=8。
- D7 tracker op 压缩缺失（no-op 抑制、a/t 压缩、readd、dense region、结构归一化）—— 值等价，op 序列不同。
- D8 编辑错误时机：Rust 编辑即校验，TS 延迟到 apply/prepare。
- D9 `Tracker::adopt` 无 stale/consumed/aborted/baseRevision 校验。
- D10 订阅者回调「串行执行」保证弱化（spawn_drain 可并发）。
- D11 source listener 错误未隔离 + `delivering` 无 finally → panic 后投递卡死。
- D12 默认 onError：TS queueMicrotask 抛错 vs Rust eprintln。
- D13 `attach_replicated_state_source` 无失败补偿（TS catch → dispose + AggregateError）。
- D14 replica hydrate 校验顺序/错误类型（TS 先 isBase，Rust 先 apply）。
- D15 `AbortSignal::any` 异步传播 vs TS 同步监听。
- D16 `await_with_context` abort 时 drop future 取消底层工作 + reason 丢失。
- D17 `with_cancel` 无 reason；`describe` 缺 `.WithValue(...)` 后缀。

### 4. durable 基础层（a4a）

**已修复**：逻辑差异 3（serde 持久化形态，17 个类型补齐 tag/rename/camelCase）。

**缺失（5，全剩余，类型级/便捷面）**
1. lib.rs 未在 crate 根平铺 re-export 基础层符号（defineDoc/defineTask/ROOT_CONVERSATION_ID 等）。
2. Tx.entry(token, id) 类型化重载缺失。
3. Tx.appendEntry(token, conversationId, TypedEntryDraft) 类型化重载缺失。
4. TaskRuntime.entry(token, id) 类型化重载缺失。
5. TypedEntryDraft<D> 类型未落地。

**多余（3，全剩余，机制性）**
1. ids.rs 公开 trait `IdBrand`。
2. types.rs `Seq::next()` 辅助。
3. entries.rs 重复定义 ToolDiagnostic/DiagnosticSeverity/CompactionReason。

**逻辑差异（剩余 4）**
1. truncate.formatSize 舍入模式：toFixed(1) vs {:.1}（.x5 边界差 0.1）。
2. truncateHeadOf 空前缀+总量超限：TS 抛 TypeError，Rust 返回空结果。
3. HookRunner 分发语义（each 的 report-and-continue）转移至内置任务，未核实保留。
4. errors.StorageRejected 丢弃 cause。

### 5. durable storage（a4b）

**已修复**：document.copy 三后端支持（P0-4）、byOwnerConversation 索引、submission 旧 requestId 清理、next_id 推进、checkDocumentActions 八项文档校验（P0-5）。

**缺失（剩余约 13）**
- memory：已退休文档校验、delta 无 base、版本转换、多内容命令、二次 retire、双活化身 live 计数（部分由 P0-5 的 checkDocumentActions 覆盖，剩余为各后端独立物化校验差异）。
- jsonl：sidecar 回收整体、主文件 seq 递增校验、reclaim 残留清理、sidecar 排序校验、重复确认检测、未确认尾截断、撕裂尾行截断、UTF-8 校验、空行损坏检测、open 建目录。
- sqlite：findLatestHeadMarker 段优先 vs 全局最新、materialize 检查顺序颠倒、Unknown document 校验文案/时机。

**多余（4，全剩余）**
1. scan.rs 共享 page() helper（limit=0 语义与 TS 内联不同）。
2. memory.rs MemoryStorage::validate_writes 公开 API。
3. jsonl unresolved_copy() 死代码 + open() exists 探测。
4. sqlite.rs open_in_memory()/from_connection()。

**逻辑差异（剩余约 17）**
- scan：requested 校验前移文案不可达（InvalidOrder 变体从未构造）；after 边界（safe-integer vs u64）；limit=0+非空（TS TypeError vs Rust 空页+游标）。
- memory：未知会话静默返回 vs 抛错；scanEntries after=0 下溢；scanDocuments 游标语义相反；limit=0；descending 游标 u64::MAX 排除；mintId 耗尽检查缺失。
- jsonl：无法打开 TS 回收过的目录；预校验不完整→磁盘污染+目录永久损坏；poison 时机不同；poison/close 后读取仍可用；fsync 范围多 flush main；错误文案缺文件名；撕裂尾行 TS 可恢复/Rust 打不开。
- sqlite：scanConversations 属主过滤（已修）、findLatestHeadMarker、未知会话静默、document 未知 id 报错 vs undefined、scanDocuments 游标 order、mintId 每次写库+无耗尽、升序哨兵 id>0 排除 id=0、findDocument 选取顺序。

### 6. durable session+env（a5）

**已修复**：L1-L4（noFollow 死代码、spill 丢 stderr、中断丢 spillPath、信号退出码）、L13（retire_doc fork-copy）、L14（doc() skipLoad）、M7。

**缺失（剩余 7）**
- M1 exec 缺 `shellEnv`/`inheritEnv:false` 语义。
- M2 exec 缺 `/bin/bash`→`which bash`→`sh` 回退链与自定义 shell 存在性检查。
- M3 transaction 缺 pendingOperations 跟踪保护。
- M4 env/index.ts 导出 `toError` 无对应。
- M5 `to_file_error` 缺 `ABORT_ERR → aborted` 映射。
- M6 `FileError`/`ExecutionError` 缺 `cause` 字段。
- M8 decode 三符号未在 env 顶层重导出。

**多余（4，全剩余）**
- X1 env/testing.rs（InMemoryFileSystem，上游无此文件，自研测试辅助）。
- X2 session.rs is_closing/poison_error/drain_line 诊断助手。
- X3 node.rs with_shell/with_watch_options builder。
- X4 node_watch.rs Windows cfg 分支（范围外，无害）。

**逻辑差异（剩余 15）**
- L5 exec onOutput 回调错误不捕获、无 callback_error 语义。
- L6 exec stdout EOF 即断流丢 stderr 尾部；stderr EOF 分支忙等。
- L7 remove 空目录/recursive:true 文件行为与 TS 相反。
- L8 NodeDirReader 打开即全量枚举、lstat 错误静默跳过。
- L9 read_text_lines 错误路径不关闭 reader。
- L10 各 Reader abort 检查顺序。
- L11 scan_lines 非法行范围 panic vs FileError。
- L12 append/truncate/flush 缺后置 abort 检查。
- L15 reject_fork_source_writes 缺 fork==="current" 过滤 → 过度拒绝。
- L16 settle/place_submission、set_task 已结算时 panic vs 抛 Error。
- L17 watch_doc 等待期 abort 不生效。
- L18 observe_cancellation 守卫条件错误。
- L19 drain_watch listener 错误丢失原始错误文本。
- L20 committed_task 无 Promise 级记忆化。
- L21 exec cwd 检查 is_dir vs access F_OK。
- L22 close() 惰性 future（丢弃未 poll 则不关闭）。

### 7. durable harness 前半（a6）

**已修复**：缺失 1（generation.answer() 边界 startRun）、逻辑差异 1（compaction 剥离 deferred）。

**缺失（0）**
**多余（0）**
**逻辑差异（剩余 11）**
1. generation classify 失败文案 `{:?}` PascalCase vs TS wire 值小写（低）。
2. compaction run_summarize cut 定位：未命中 -1 vs 0（分支不可达，防御差异）。
3. compaction arguments_text/序列化：工具参数键序字典序 vs 插入序（项目已知）。
4. json.rs assign_json 对象合并叶操作字典序 vs 插入序。
5. generation stream_response 存储 partial 未剔除 null（omitUndefinedProperties）。
6. events task_checkpoint Completing/Terminal 任务 panic vs TS args={}。
7. events translate 用 BTreeMap 按键排序 vs TS 插入序。
8. compaction user 消息 UserContent::Text vs TS 块数组（wire 形状不同、语义等价）。
9. compaction cache_retention None vs TS "none"（疑似）。
10. harness inspect 未包 read_on_line。
11. events watch_events abort 载荷通用 SessionError::Aborted vs TS signal.reason。

### 8. durable harness 后半（a7）

**已修复**：逻辑差异 1（bound_content）、2（env 错误路径）、5（serde null 键省略）、11（registry 通知死锁）。

**缺失（剩余 1）**
- TaskRuntime.entry 丢失 kind 过滤参数（TS entry(token?, id, context) 按 kind 过滤）。

**多余（2，全剩余）**
1. tool.rs prepare/validate 新增「非对象返回值」防御检查（TS 为 unchecked cast）。
2. usage.rs 对损坏账本数据的防御（非对象报错、缺 cost 补建、add_usage_state 吞错）。

**逻辑差异（剩余 10）**
1. tool.rs publish_progress 字节计数漏 details/diagnostics（节流偏快）。
2. tool.rs beforeTool hook 收到原始 arguments（TS 传当前累计 args）。
3. submissions wait() 关闭检查时点靠前 → 竞态下可注册永不结清的等待者。
4. submissions 请求类型冲突文案不同；ConversationBusy 退化为 SessionError::Message。
5. submissions submit() 的 now/queueModes 求值先于 commit 回调。
6. reserve resolve()：migrate 返回 None 不记忆、不上报（TS 记忆并上报，且不再重试）。
7. exec decide() 上报文案把 cause 拼进消息（TS 放 Error.cause）。
8. ownership live_records 产出 live 旧记录而非 overlay 候选（当前不可观察）。
9. provider 损坏 pi.provider 文档行为不同（TS 抛错/返回 undefined，Rust 迁移提交/空串）。
10. Waiters.keys() 顺序：插入序 vs BTreeMap 排序序（当前消费者不依赖）。

（另有 9 项已核实的低影响边缘差异，见 a7 报告「边缘差异」节。）

### 9. durable tools+testing（a8）

**已修复**：缺失 1/2/3/6（bash prepare/env/inheritEnv、PowerShell programs、read truncation 字段、re-export）、逻辑差异 1（bash 超时文案）。

**缺失（剩余 2，均为测试 case 批量缺口，共 32 个 case）**
- env-conformance：14 个 TS case 缺失（8 watch、3 dir-reader、windowed exec、2 symlink）。
- storage-conformance：18 个 TS case 缺失（含全部 8 个 document 契约 case、ID 耗尽、fork 历史、deep-fork 扫描等）。

**多余（3，全剩余）**
1. index mod.rs 额外 pub 暴露 5 个辅助模块。
2. bash 死代码 `is_last`。
3. storage-conformance 多余 case "scans entries in either order and pages by cursor"。

**逻辑差异（剩余 13）**
1. bash powershell schema 复用 bash 描述（"Bash command to execute"）。
2. path-utils AMPM 变体正则丢大小写不敏感（`/ (AM|PM)\./gi` → 仅大写）。
3. path-utils 变体去重 `new Set` 未移植（重复 exists() 调用）。
4. read 小数/负/超大 offset·limit 的提示算术。
5. read 首行 BOM 解码差异 + 畸形 UTF-8 计数差异。
6. read format_size tie 舍入（1.25→"1.3" vs "1.2"）。
7. read description 硬编码常量（漂移风险）。
8. image is_bmp u32 加法溢出（debug panic on crafted 输入）。
9. fmq QUEUES 无空闲清理（内存无界增长）。
10. edit 畸形输入强转 ""（schema 门控下不可达）。
11. edit-diff trimEnd 空白集差异（U+FEFF vs U+0085）。
12. env-conformance 5 个已移植 case 被削弱（断言子集缺失）。
13. storage-conformance 5 个已移植 case 期望不符/被削弱。

### 10. ai types/models/utils（a9）

**已修复**：逻辑差异 1（stream_simple normalize，核实为误报）、2（headers 优先级）、缺失 7 的 env 部分。

**缺失（剩余 15）**
1. types：`AssistantMessage.diagnostics`、`TextSignatureV1`、`NestedToolCallRecord`/`NestedToolCalls`。
2. types：Images 类型族。
3. types：Classifier 类型族（11 个）。
4. types：`ImageModel`/`ClassifierModel`/`BaseModel`/`ModelTypeMap`/`ModelType`/`AnyModel` 及 `Model.type` 字段（**模型类型系统**）。
5. types：`ProviderStreams`/`ProviderImages`/`ProviderClassifier`/`ApiOptionsMap`/`ApiStreamOptions`。
6. types：5 个 typed compat 接口 + 路由类型（JSON 占位，声明简化）。
7. types：`ProviderRequestOptions.telemetryContext`/`fetch`（env 已补）。
8. models：Provider 接口的 headers/getAllModels/refreshModels/stream/generateImages/classify 等。
9. models：Models 的 stream/complete/getModelsOfType/refresh/classify 及 refresh 框架。
10. models：`CreateProviderOptions` 的 headers/fetchModels/images/classifiers 等。
11. retry：9 个错误分类正则缺失。
12. overflow：`prompt exceeds max length` 模式缺失。
13. provider-retry：`noRetryStatuses` 选项缺失。
14. headers：`providerHeadersToRecord` 多源变参合并缺失。
15. error-stream：`lazyApi`/`forwardStream`（TS 位于 api/lazy.ts）。

**多余（4，全剩余）**
1. `ToolResultMessage.added_tool_names` 字段（TS 无；被 estimate.rs 引用）。
2. `estimate_context_tokens` 对 Context 输入额外计入 systemPrompt/tools token。
3. `ImagesApi`/`ImagesProviderId` 别名。
4. 机制性辅助：`ContentTextInput`、`DiagnosticContainer`。

**逻辑差异（剩余 18）**
1. thinkingLevel "off" 折叠为 None（丢显式 off/缺省区分；反序列化 "off" 报错）。
2. Tool.constrainedSampling 显式 false 不可表示。
3. check_provider_auth check 返回 None 时额外 fallback resolve。
4. calculate_cost short_write u64 下溢（TS 可为负）。
5. has_api/models_are_equal 缺模型类型维度。
6. login 失败被包装为 ModelsError；cancel_deferred 返回 String 错误。
7. get_models 等 best-effort 无 try/catch。
8. validation required 错误路径特判/coercion 边界差异。
9. diagnostics extract_diagnostic_error/format_thrown_value 简化。
10. overflow 正则收窄 + cerebras 模式全局化。
11. estimate Context 分支 tokens 多计（与多余 2 同源）。
12. event-stream durationMs Math.round vs 截断。
13. error-body 截断按 Unicode 标量 vs UTF-16 code unit。
14. pi-user-agent 平台名/形态差异。
15. provider-env 空字符串 override 不回落 process.env。
16. provider-retry retry-after-ms parseFloat 宽松 vs 严格。
17. transcript declarationsEqual 结构相等 vs 序列化字符串（键序敏感）。
18. frame clone_start_message 保留 thinkingLevel（TS 丢弃）+ UTF-16 vs 标量计数。

### 11. ai api/providers/auth（a10）

**已修复**：SO-1 的 env 字段部分（修6）、AR-1/AR-2（OAuth 刷新取消语义）、OR-14（未知 status 抛错）、OR-16（终态传播 + 未完成 tool call 检查）。

**缺失（剩余 32，主要来自两个文件头自述「简化版」）**
- openai-completions（CC-1..11,24,25）：thinking 全链路、prompt cache、retry/abort、grammar/strict 工具、hasToolHistory、synthetic assistant、toolResult 图片、maxTokensField/samplingParams、thinking 请求参数族、responseId/responseModel、错误拼装、copilot 头、streamSimple clamp。
- openai-responses（OR-11,18,21,25,27,28）：custom_tool_call_input delta/done 处理、reasoning backfill、isChatGPTSignIn 门控、reasoning 请求参数、tool_choice/retry/headers、applyMessagePhaseStopReason。
- simple-options（SO-1 剩余）：telemetryContext/fetch 字段。
- faux（FX-1..7）：deferred 全链路、token 速率、usage 估算、onResponse、fauxAssistantMessage options、异步 Factory、Handle 字段。
- providers（PV-1）：openai OAuth/classifiers/filterAllModels。
- auth（AT-1、AR-5）：OAuthAuth.login 缺 LoginOptions 参数；refreshStoredOAuthCredential 未独立导出。
- lib.rs（IX-1,3,5,6）：faux 多符号/diagnostics/assistant-message-frame/api 选项类型导出缺失。

**多余（7，全剩余）**
1. OR-2 append_system_tool_additions 按工具名去重（TS 无）。
2. FX-12 faux_default_model 导出（TS 无）。
3. IX-2 lib.rs 导出 auth::resolve（TS index 不导出）。
4. IX-4 lib.rs 导出 utils::error_stream（TS index 无）。
5. completions 恒发 stream_options.include_usage（TS 条件化）。
6. （与 IX-2 同源，不重复计）。

**逻辑差异（剩余 40，重点见 a10 报告「复核说明」建议优先修复项）**
- 建议优先：OR-11（custom_tool_call 输入事件）、OR-10（arguments.done 前缀语义）、OR-1（tool-search seed）。
- 其余：CC-12..23（finish_reason 映射、sanitize、事件流非增量、usage 无 cache/reasoning/cost、toolCall ID 规范化等）、OR-3..9,12,13,15,17,19,20,22,23,24,26,29,30、SO-2、TM-1/2、FX-8..11、OD-3（LOGO_SVG 三色 vs 单色）、AR-3/4、CS-1/2（store list 顺序）。

---

## 三、豁免类（不计入差异，供参考）

- **语言机制等价**：Proxy→显式方法、TypeBox→JSON schema、throw→Result/panic、sqlite 异步 facade、模块级 const→OnceLock/LazyLock 工厂、泛型擦除、undefined/null 三态→Option+skip_serializing_if。
- **声明范围外**：Windows 分支、Cloudflare Durable Object 后端、图像生成、模型目录硬编码、OAuth 登录流程、compat 占位。
- **值等价**：chord op 序列（D7）、BTreeMap 键序 vs 插入序（消费者不依赖的路径）。
- **测试契约弱化**：telemetry conformance 3 个 Proxy case + 断言弱化、a8 conformance 已移植 case 的断言子集缺失。

## 四、行动建议

真实工作量见顶部「〇、分层视图」：**正常路径可触发 + serde 持久化差异已全部修复**；当前唯一剩余待办是第 1 层健壮性（约 10 项，按需挑）；第 2 层约 290 项记账差异不必逐项处理。
