# durable harness 前半 1:1 复刻审计报告（recheck/a6）

- 基线：upstream `earendil-works/pi` v1.1.0（commit `abe508e1b`），TS 根 `upstream/packages/durable/src/harness/`
- Rust 根：`crates/pi-durable/src/harness/`
- 方法：逐文件、逐导出符号、逐方法静态比对（参数/返回值/控制流/分支/边界/常量/字符串字面量/错误处理）。
- 执行限制：本会话处于共享 checkout，bash 被拒绝，**无法运行 cargo/node 校验**；全部结论来自源码级静态比对。
- 只读：未修改 `crates/` 与 `upstream/` 任何文件，仅写本报告。

## 0. 显示层脱敏说明（重要）

读取文件时，显示层会隐藏「行内标识符含 `token`/`key` 且其后有 `=`/`:`」的赋值值（显示为 `[redacted]`）。
据此无法直接看到以下位置的原文（两测同规则被隐藏，Rust 侧另有测试断言可交叉验证）：

- TS `agent.ts` `DEFAULT_COMPACTION_POLICY` 三个数值（`reserveTokens`/`keepRecentTokens`/`backgroundTokens`）；
  Rust `types.rs` 同款常量同样被隐藏；Rust `reserve_tokens=16_384` 由 `agent.rs` 测试断言确认，
  `keep_recent_tokens`/`background_tokens` 两侧均无法核对，**此项留待可见后复核**。
- TS `compaction.ts` select 阶段 `maxTokens: Math.min(⌊0.8·reserve⌋, maxTokens>0 ? maxTokens : X)` 的 X（推断为
  `Infinity`/`Number.POSITIVE_INFINITY`，与 Rust `else { floor }` 分支语义等价）；`estimateContext` 的
  `tokens` 初值/累加行（由 Rust `.map(calculate_context_tokens).unwrap_or(0)` 与 `+= estimate_message_tokens` 交叉确认一致）。
- TS `generation.ts` `thresholdCompaction` 的 `estimateContext(...)` 行（由 Rust 交叉确认一致）。
- Rust 侧 `api_key: None`、`max_tokens: None/Some(...)`、`token: &DocToken` 等参数类型（按上下文推断为 None/&DocToken）。

## 1. 结论汇总

| 分类 | 数量 |
|---|---|
| 缺失 | **1** |
| 多余 | **0** |
| 逻辑差异 | **12**（2 处确证、10 处低危/疑似，见 §3） |
| 命名差异（不算问题） | 若干（不计） |
| 豁免（语言机制等价） | 若干（§4） |

总体：11 个文件的主干逻辑（任务阶段机、阈值判定、边界应用、事件翻译、输出裁剪）与上游逐行对齐；
发现 1 处真实缺失（generation `answer()` 未为最终边界选中的输入启动新运行）与 1 处行为级差异
（compaction summarize 未剥离 `deferred`），另有 1 处错误文案差异与若干低危边界/顺序差异。

## 2. 逐文件核对

### agent.ts → agent.rs ✅（1 低危项）
- `DEFAULT_RETRY_POLICY`{enabled:true,maxRetries:3,baseDelayMs:2000,maxAgentDelayMs:60000} ✓
  （Rust `DEFAULT_RETRY_POLICY` 一致，`types.rs` 测试断言）。
- `DEFAULT_PROGRESS_POLICY`{100,100} ✓；`DEFAULT_CONTEXT_RETENTION_MS=600_000` ✓；`INSTRUCTIONS_KEY="instructions"` ✓。
- `AgentDoc`：kind `pi.agent`、v1、conversation/rewindable/asOf、initial {}、checkpointWhen=true ✓。
- `resolveSettings` ✓ 逐字段合并语义一致（`stream: {...settings?.stream}` ↔ `unwrap_or(default)`，
  默认 stream 全空等价；progress 逐字段 `??` ✓；toolExecution "parallel"、steering/followUp "one-at-a-time" ✓）。
- `configure`/`applyChange` ✓（undefined 不变 / null 删除 / 值替换三态；extensions `{add,remove}` 仅在提供时写入；
  tools `names|{remove}` ✓）。
- `addTools` ✓（数组形态追加缺失名；`{remove}` 形态过滤；未设置不写）。
- `createAgent` ✓（parent 直接返回；无 owner 返回；owner 逐键覆盖复制）。
- `agentHooks` ✓（扩展顺序、task 名匹配）。
- `resolveAgent`/`selectExtensions`/`applyWrap` ✓（Map 覆盖、去重取首、改名/抛错删除目标并上报；
  Rust 用 `catch_unwind` 捕获 panic 对应 TS throw——豁免）。
- 低危：`applyWrap` 上报 payload 为字符串（TS 上报 `Error` 对象）；`resolve_agent` 的 report 回调类型同理。

### compaction.ts → compaction.rs ✅（1 逻辑差异、3 低危）
- 常量：`TOOL_RESULT_MAX_CHARS=2000` ✓、`SUMMARY_PREFIX`/`SUMMARY_SUFFIX` ✓（逐字符一致）、
  `SUMMARIZATION_SYSTEM_PROMPT`/`SUMMARIZATION_PROMPT` ✓（逐字符一致，含 "## Goal"… 模板全文）。
- `CompactionInput`{reason, instructions?} ✓；`SummaryRequest`{attempt,model,thinkingLevel,streamOptions,
  maxTokens,tail,firstKept} ✓；`CompactionCheckpoint` select/summarize(flatten)/retry(until+flatten) ✓（serde tag=camelCase）。
- `CompactionTask`：name `pi.compaction`、version 1、initial `{phase:"select"}`、phases select/summarize/retry ✓；
  `make_compaction_task(start_run)` 工厂 = 模块级 const 的 DI 化（豁免，文件头有说明）。
- `select`：agent/settings/model 解析、failNoModel、`selectCut`、beforeCompact（首个决定胜出）、
  decline→complete、summary→place、request 组装（attempt=1、thinkingLevel、settings.stream、tail=fold max）、
  running checkpoint ✓。maxTokens 计算（floor(0.8·reserve) 与 model.maxTokens>0 取 min，否则 floor）与
  TS 推断的 `Math.min(...)` 语义一致 ✓。
- `summarize`：model 解析、`context(at:tail)`、cut 定位（⚠ 见差异 3）、messages（system+user，时间戳 now）✓、
  options 组装、`throwIfAborted`、`summaryText`、retry 判定（error && isRetryable && enabled && attempt<=maxRetries）、
  `until = now + retryDelayMs`、commit 内 recordUsage→live→summary?place : retry?状态+checkpoint : 失败 ✓。
  ⚠ **逻辑差异 1（确证）**：TS 显式 `const { deferred: _deferred, ...forwarded } = streamOptions` 剥离 `deferred`，
  Rust `stream_options()` 原样映射 `deferred` 且不覆盖——`settings.stream.deferred` 被设置时，Rust 的摘要请求
  会带 deferred 而 TS 不会（行为分叉）。
  ⚠ 低危：TS `cacheRetention: "none"` vs Rust `options.stream.cache_retention = None`（缺省）；若 pi-ai 对
  缺省与 `"none"` 语义一致则等价，未在本范围内确认 provider 层语义。
  ⚠ 低危：TS 摘要请求 user 消息 content 为 `[{type:"text",text}]` 块数组，Rust 用 `UserContent::Text`（字符串）——
  同为合法 user content，wire 形状不同。
- `retry`：sleep(until)→attempt+1→状态 attempt/删 retry→summarize checkpoint ✓。
- `abort`：删状态→terminal aborted ✓。
- `createCompaction`：ownership（无 owner=conversation/有 owner=task）、`background = owner===undefined &&
  reason!=="manual"`、createTask、status{taskId,reason,blocking:owner!==undefined,attempt:1}、addCompactionStatus ✓；
  `make_create_compaction` 工厂 = DI（豁免）。
- `selectCut`/`isCandidate` ✓（候选规则：assistant 开头恒候选；user 开头需前一个 assistant 的 toolCallId 无
  后续结果；遍历方向/边界逐行一致）。
- `summarizedMessages` ✓（slice(0,cut) 展平 + orderToolResults；Rust `.get(..cut).unwrap_or(全量)` 对越界 cut
  与 JS slice 钳制等价）。
- `estimateContext` ✓（measured 取 head 之后最新的 contextTokens>0 的 assistant；from=lastIndexOf+1；
  tokens=measured.usage 或 0；加 from 之后与 extra 的 estimate——与 TS 推断行一致）。
  ⚠ 豁免：`lastIndexOf`（对象身份）↔ Rust `rposition`（PartialEq 相等）——文件头已说明。
- `summaryText`/`summaryFailure` ✓（文案逐字符一致："Summarization failed: …"/"Summarization hit the token
  limit; the summary is incomplete"/"Summarization attempted to call a tool"/"Summarization produced no text"）。
- `summaryPrompt` ✓（`<conversation>\n…\n</conversation>\n\n` + 提示 + 可选 `\n\nAdditional focus: …`）。
- `serializeConversation` ✓（`[User]`/`[Assistant thinking]`/`[Assistant]`/`[Assistant tool calls]`/`[Tool result]`
  前缀、空段跳过、system 省略、`\n\n` 连接、工具结果 truncate）。
  ⚠ **逻辑差异 2（项目已知）**：工具调用参数 `key=JSON.stringify(value)` 的键序——JS 插入序 vs
  serde_json BTreeMap 字典序（文件头已声明，影响摘要请求文本）。
- `contentText` string|blocks → `content_text_of_user`/`content_text_of_blocks` 两函数（豁免，已声明）。
- `truncate` UTF-16 码元切片 ✓（注释已声明与 JS `slice(0,maxChars)`/`length` 对齐）。
- `place`/`placeSummary`/`complete`/`failNoModel` ✓（`SUMMARY_PREFIX+summary+SUMMARY_SUFFIX`、head=firstKept、
  user 消息+timestamp now、data={reason}、owner 分支 admitSubmission/appendEntry、文案
  "No model is configured"/`Model ${provider}/${modelId} is not available`、detail {reason:"no_model"} ✓）。
  ⚠ 豁免：`admitSubmission` 的 `start_run` 参数为 DI（TS 直接 import startRun）。

### context.ts → context.rs ✅
- `SCAN_PAGE_SIZE=256` ✓、`EXCLUDED_STOP_REASONS`{aborted,error,deferred} ✓、
  `MISSING_RESULT_TEXT` ✓（逐字符一致）。
- `captureContextBounds` ✓（at 缺省取最新条目；显式 at 不可见则 `Entry ${at} is not visible…`；findLatestHeadMarker）。
- `readContext`/`readContextFrom` ✓（same head 增量扩展；tail 回退过滤；minEntryId=prev.tail+1；视图持有独立数组）。
  ⚠ 豁免：`freezeJson` 无对应物（Rust owned/Arc 无共享可变，已声明）。
- `deriveRange` ✓（edits 全量收集（含被 selectActive 丢弃的旧 head 标记）；active=selectActive；
  contribute（omit→[]、replace→messages、排除停止原因）；settle/open；leadWithSystem；messages=settled+order(open)）。
- `extendRange` ✓（有 edits/head/被编辑条目则整段重建，否则只派生新增贡献）。
- `leadWithSystem` ✓（首个非 user 为 system 且 index>0 时前移）。
- `contribute` ✓（assistant 且 stopReason∈EXCLUDED 丢弃）。
- `settle` ✓（findLast assistant>0 才切分）。
- `activeEntries`/`rangeQuery`/`scanRange`（倒序恢复）✓。
- `selectActive` ✓（head 在前 + 非 head 条目）。
- `orderToolResults` ✓（toolResult 跳过、assistant 后按调用序插结果、每 id 首见、缺失合成
  `{isError:true, details:{reason:"missing_result"}, content:[MISSING_RESULT_TEXT], timestamp=assistant.timestamp}`、
  未匹配结果丢弃）。
- `emptyView` ✓。

### define.ts → define.rs ✅
- `defineExtension`（恒等）✓；`defineTool`（恒等+装箱 Arc，豁免）✓；
  `section(key, render, options?)`：缺省 tag=true ✓（TS 无 tag 字段等价 tagged）；
  `hook(task, handlers)`（task.definition.name）✓；`wrapTool`/`wrapSection` ✓。

### events.ts → events.rs ✅（3 低危）
- `MessageChange` 8 种变体 ✓（tag=type snake_case：text_start/thinking_start/toolcall_start/text_delta/
  thinking_delta/toolcall_delta/block/message，字段 camelCase）。
- `SnapshotEvent` ✓（entries/run{inputs}/generation{attempt,message?,retry?,deferred?}/tools/compactions/inbox/
  agent(缺省 {})/usage(缺省初始值)）；Rust 由 `AgentEvent::Snapshot` 内部标签提供 `type:"snapshot"`（豁免，已声明）。
- `AgentEvent` 21 种事件 ✓（run_start/run_end/turn_start/turn_end/message_start/message_update/message_end/
  tool_execution_start/tool_execution_update/tool_execution_end/inbox_update/submission/auto_retry_start/
  auto_retry_end/deferred_poll/entry_appended/agent_changed/usage_changed/task_failed/compaction_start/
  compaction_end，字段与 camelCase 映射一致）。
- `watchEvents` ✓（Session 线上原子 snapshot+注册；held=completing 的 pi.generation；overflow 以 snapshot 替换；
  abort 后 cancel+拒绝；observeCancellation）。⚠ 低危：TS `throw signal.reason`，Rust 返回通用 `SessionError::Aborted`。
- `parts`/`snapshotOf`/`queued`（含 write 条目 id/mode）✓；`resultOf` ✓。
- `translate` ✓（先过滤本会话 entries/tasks/submissions；全空提前返回；submissions 按 id 排序；
  tool_execution_start（running 新槽位，args 取 task checkpoint.arguments ?? {}）；
  message_start/message_update（partial 出现/变化）；tool_execution_update；auto_retry_start/end；
  deferred_poll；tool 结束（done 变化/消失时 resultOf）；条目循环（tool_end 前置、streamed 抑制重复
  message_start、message_end、entry_appended）；compaction_end；turn_end（completing 首次/terminal 未 held）；
  task_failed（faulted/orphaned）；run_end/run_start（inputs[0] 变化判定）；submission/inbox_update/
  agent_changed/usage_changed；compaction_start；turn_start（run.taskId 变化且为新 pi.generation）✓）。
  ⚠ 低危 A：`tasks`/`slots_before` 用 BTreeMap（按 TaskId/callId 排序）迭代，TS 用 Map（插入序）——
  同一提交多任务/多槽位时事件顺序可能不同。
  ⚠ 低危 B：`task_checkpoint` 对 Completing/Terminal 任务 `unreachable!`（panic）；TS 取 `state.checkpoint`
  得 undefined → args={}。运行中槽位任务已终态的极端场景下 Rust 会 panic。
- `messageChanges` ✓（路径前缀判断、整条替换→message、usage 跳过、非 content→message、splice 且 deleteCount=0
  才产出 *_start、text/thinking append、arguments append→toolcall_delta、其余→block+whole 去重）。
- `toolUpdate` ✓（Trim/Append/其他→set；details 移除→null、diagnostics 移除→[]；全空→None）。

### generation.ts → generation.rs ⚠（1 缺失、1 逻辑差异、2 低危）
- `GenerationInput`/`GenerationCheckpoint`（prepare/request/retry/poll/tools 全字段，compacted/overflow
  skip_serializing_if）/`GenerationResult{entryId}`/`Request`/`DEFAULT_POLL_AFTER_MS=5000` ✓。
- `GenerationTask`：name `pi.generation`、v1、initial `{phase:"prepare",attempt:1}`、phases
  prepare/request/retry/poll/tools ✓；`make_generation_task` 工厂（DI：start_run/create_compaction/
  tool_task/generation_task，豁免）。
- `prepare` ✓（agent/settings/model 解析、failNoModel；compacted+overflow 时 outcomes 校验 entryId 否则
  failModelError；replaySections；env 错误：abort 则抛出、否则 report；PromptInput；renderSections；
  planSystemEntries（agent.tools→pi Tool 映射）；thresholdCompaction（仅无 compacted 时）；
  blocking→createCompaction(reason threshold, owner=taskId)+waiting allSettled；否则 commit：scanEntries(1)
  取 cutoff→append SystemEntry 逐个推进 cutoff→无条目抛错→background 且无 compactions 时后台压缩→
  request checkpoint{attempt,compacted?,model,thinkingLevel,settings.stream,cutoff} ✓）。
- `thresholdCompaction` ✓（!enabled||window<=0→None；tokens=estimateContext(view, planned 的 model 展平)；
  blocking=window-reserve；background=blocking-backgroundTokens；tokens>blocking→blocking，否则
  tokens>background→background；无切点→None）。⚠ 等价差异：Rust 加 `background_tokens>0` 守卫（TS 无，
  但 0 时 background==blocking 使该分支不可达）；i128 做差防下溢（豁免，已声明）。
- `request` ✓（commit：convertPartial→generation={attempt}；model 解析；context(at:cutoff)；beforeRequest
  逐 hook 替换 messages；options=stream_options+signal+sessionId+reasoning(thinkingLevel≠off)；streamResponse；
  Request{messages:view.messages}；classify）。
- `retry` ✓（sleep；generation={attempt+1}；prepare checkpoint）。
- `poll` ✓（model 解析；sleep(pollAt)；fetchDeferred(signal)；Request{pollAt}；classify）。
- `tools` ✓（pending 空→finishToolRound；否则 commit：createToolTask(ownership=task)、更新 slot.taskId、
  tools+taskId、pending=rest、waiting allSettled）。
- `abort` ✓（poll 时 cancelDeferred（失败 report）；tools 时 readCalls 取未启动调用；
  commit：convertPartial→逐个 appendToolResult(harnessError("aborted",`Tool ${name} was aborted`))→
  endRun(unanswered aborted)→terminal aborted）。
- `streamResponse` ✓（partialIntervalMs 节流；event done/error 跳过、空 content 跳过；pending 提交
  generation.message（保留已有 attempt/retry/deferred）；错误且非 aborted 才 report；finally 停表并等在途提交）。
  ⚠ **逻辑差异 3（低危）**：TS 提交前 `copyJson(partial,{omitUndefinedProperties:true})` 剔除 undefined 键；
  Rust `serde_json::to_value(&generation)` 把 `Option=None` 序列化为 null（responseModel/responseId/
  errorMessage 等）——`pi.live.generation.message` 存储形状不一致（json.rs 的 `omit_null_members` 未用于此路径）。
- `classify` ✓（throwIfAborted；deferred→pollAt=max(now+pollAfter??5000, 前次 pollAt+1)、generation=
  {attempt,deferred:{pollAt}}、poll checkpoint；afterResponse 全部 hook；toolUse+有调用→startToolRound；
  stop/length/toolUse→answer；overflow（error && isContextOverflow && 无 compacted && enabled && 有切点）→
  appendAssistant+删 generation+createCompaction(overflow,owner)+prepare checkpoint{overflow:text}；
  retry 判定（error && !overflow && isRetryable && enabled && attempt<=maxRetries）、until、
  commit：appendAssistant→retry?generation={attempt,retry:{at,error}}+retry checkpoint :
  endRun(unanswered model_error)+failed{reason:"model_error"}）。
  ⚠ **逻辑差异 4**：失败文案 `format!("Model response ended with stop reason {:?}", stop_reason)`——StopReason
  为 derive(Debug)，输出 PascalCase（"Error"/"ToolUse"/"Aborted"），TS 用 wire 值（"error"/"toolUse"/"aborted"）。
  `errorMessage` 存在时不触发；无 errorMessage 的 error/toolUse(无调用)/aborted 终态会得到大小写不同的文案。
- `answer` ⚠ **缺失 1（确证）**：continuation 分支 ✓（onYield 首个非空、append UserEntry、handOver、删
  generation、返回 result）；但 else 分支仅 `end_run(...Done{answer})` 后直接返回——**缺少 TS 的
  `if (users.length > 0) await startRun(tx, conversationId, live, users);`**。`end_run`（live.rs 已核）只结算
  run.inputs 并删 run/generation/tools，不会启动运行；`apply_boundary`（inbox.rs 已核）只返回 users/reset。
  最终边界选中的用户提交在 Rust 中会停留在 placed 状态而永远不会运行（finish_tool_round 的同款调用存在，
  证明此处是漏写而非机制差异）。
- `startToolRound` ✓（messages=request.messages ?? context(at:cutoff)；offered=getCurrentTools 名集合；
  sequential=设置 sequential 或任一已提供工具的 executionMode=sequential；commit：appendAssistant→
  逐调用（未提供→harnessError("tool_unavailable",`Tool ${name} is not available`)+appendToolResult、
  slot done+entry；sequential 且已有任务→pending；否则 createToolTask、slot{taskId,pending}）→
  删 generation→live.tools=slots→waiting allSettled）。
- `finishToolRound` ✓（outcomes→controls；afterTools（results=有 entry 的槽位）；terminate=非空且全部
  taskId 的 control.terminate===true；added=addTools 展平；handoff=最后一个非空；commit：prepareBoundary→
  added 时 addTools→terminate/handoff：handoff 时 append ResetEntry{head:"self",user 消息}、boundary.head、
  applyBoundary(final)、endRun(done,answer=assistant)、users 非空则 startRun；否则 applyBoundary(postTools)：
  reset→endRun(unanswered reset)+users 时 startRun；否则删 tools、run 匹配时 run.inputs.push(users)、
  handOver(createGeneration)；返回 terminal completed{entryId:assistant} ✓（本函数 users 启动逻辑完整）。
- `appendAssistant`（recordUsage("models",`${provider}/${model}`)→appendEntry AssistantEntry）✓。
- `startRun`（live.run={taskId:createGeneration(),inputs}，Rust 用 set 同形状写入——豁免，已声明）✓；
  `createGeneration`（ownership conversation）✓；`handOver`（taskId 匹配才替换）✓。
- `convertPartial` ✓（generation.message 存在→stopReason 改 aborted→appendAssistant；调用方负责删/替换）。
- `readCalls` ✓（entry.model[0] 为 assistant→toolCall 过滤；callIds 按序取每 id 首个匹配）。

### harness.ts → harness.rs ✅（1 低危）
- 组装：SessionHooks（conversationCreated：LiveDoc/InboxDoc/UsageDoc/ProviderDoc→createAgent→
  options.conversationCreated；beforeClose→tasks.join）✓；OnceLock 破循环依赖（豁免，已声明）；
  make_start_run/make_create_compaction 接线 ✓；Scheduler options（agent/settings/env/settleOutcome/
  withdrawInputs/conversation/withoutAbortSignal(context)）✓；Submissions options ✓。
- `open`：abortSignal.throwIfAborted；内置任务缺失校验，文案 `Registry lacks built-in tasks ${names};
  create it with createRegistry()` ✓，`BUILTIN_TASK_NAMES=["pi.generation","pi.tool","pi.compaction"]` 与
  TS `BUILTIN_TASKS=[GenerationTask, ToolTask, CompactionTask]` 顺序一致（registry.ts 已核）；open 失败时
  无 signal 关闭并 rethrow ✓。
- `resolveAgent`/`buildEnv` ✓（snapshot ?? 当前快照；env 无选项→None；cwd 传入 EnvTarget）。
- `ConversationImpl`：agent/configure/submit/compact（先 resume；{reason:"manual",instructions?}；
  commitWith(createCompaction)）/reset（无 handoff 时 model 缺省、head:"self"）/commit（conversationId
  作用域）/context/entries（bounded query 与 readOnLine）/fork/abort（background===true）/waitForIdle/
  viewState/watch ✓。
- `HarnessImpl`：resume（⚠ 低危：closed 时 panic vs TS throw Error("Harness is closed")——机制差异）；
  root（root 已存在则复用 ROOT_CONVERSATION_ID）/conversation（assertOpen+readOnLine）/createConversation/
  getTask/inspect（⚠ 低危：TS 整体包 `readOnLine`，Rust 未包——tasks.inspect 与两次 submission 扫描不在
  同一条 Session 线上，快照一致性弱于上游；queued+placed 合并后按 id 排序 ✓）/submission/abortSubmission
  （not_found 映射 ✓）/abortTask/waitForTask/waitForIdle/usage（readOnLine 扫描会话→逐会话 addUsageState ✓）/
  taskGraph/watchTaskGraph ✓。
- `boundConversation`/`BoundSubmission` ✓（binding.check+withAbortSignal 包装 status/wait/abort；
  abort options.background==true；waitForIdle(id)）。

### inbox.ts → inbox.rs ✅
- `InboxItem`（input{mode steer|followUp,content}/write{entry}）/`InboxState{items}`/`InboxDoc`
  （pi.inbox、v1、latest/initial、initial {items:[]}、checkpointWhen items.length===0）✓。
- `prepareBoundary`（latestHeadMarker 的 head + inbox doc + 两队列模式）✓。
- `applyBoundary` ✓（reset=任一 write 的 head==="self"；final=at==="final"||reset；pick：all 或 take(1)；
  writes 先行（stale→settle unanswered stale；否则 appendEntry、head 非空时前推 boundary.head
  （"self"→新条目 id）、placeSubmission）；users（steer+final 时 followUp、id 升序）逐个 append UserEntry+
  placeSubmission；按位置倒序移除；返回 {users:placed, reset}）。
- `isStale`（仅数字 head 且 < boundary.head）✓；`removeInboxItem` ✓；
  `withdrawQueuedInputs`（倒序、跳过 write、settle unanswered aborted、移除）✓。

### json.ts → json.rs ✅（1 低危）
- `assignJson` ✓（对象×对象：先删值没有的成员再逐叶递归；数组×数组且 current.length<=value.length：
  按下标递归、多出追加；其余值不等时整体替换；Rust 补 `slot` 缺失时对象插入/数组 push、`assign_json_name`、
  `is_record`、`omit_null_members`/`to_json_without_nulls` 对应 `copyJson(omitUndefinedProperties)`）。
  ⚠ 低危：对象合并迭代 serde_json Map（BTreeMap 字典序）vs JS 插入序——最终状态一致，但 chord delta 的
  叶操作顺序不同（影响 publish op 流）。
- 数组越界下标：JS `slots[key]=value` 会带空洞扩展，Rust `push` 追加——本范围内无此调用形态，未触发。

### live.ts → live.rs ✅
- `ToolSlot`/`CompactionStatus`/`RetryBackoff`/`LiveRun`/`LiveGeneration`/`DeferredPoll`/`LiveState` 全字段
  camelCase + skip_serializing_if ✓。
- `LiveDoc`：pi.live、v1、latest/initial、initial {}、checkpointWhen（无 generation 且无 running 槽位）✓。
- `RUN_TASK_KINDS=["pi.generation"]`/`TOOL_TASK_KIND="pi.tool"`/`COMPACTION_TASK_KIND="pi.compaction"` ✓。
- `endRun` ✓（run.taskId 匹配→settle 每个输入+删 run；恒删 generation/tools）。**（本函数不启动新运行——
  支撑 generation.rs answer() 缺失的判定。）**
- `addCompactionStatus`（尾插=任务 ID 序）✓；`compactionStatus`→`compaction_status_index` ✓；
  `removeCompactionStatus`（空列表时删字段）✓；`toolSlot`→`tool_slot_index` ✓；
  `finishSlot`（status done、entry 可选、clearProgress）✓；`clearProgress`（删 output/droppedBytes/
  droppedLines/details/diagnostics）✓。
- `settleSchedulerOutcome` ✓（pi.tool→finishSlot(undefined)；pi.compaction→删状态；非运行种类忽略；
  运行种类且 run.taskId 匹配→convertPartial→faulted/orphaned 结算）。

### output.ts → output.rs ✅（纯逻辑重点核对）
- `sanitizeOutput`：正则 `[\x00-\x08\x0b-\x1f\ufff9-\ufffb]` ↔ `is_valid_output_char`（保留 0x09/0x0a、
  保留 U+FFFC、删 interlinear U+FFF9–U+FFFB）✓ 逐范围一致。
- `boundOutput`/`headRange`/`tailRange`/`characterEnd`/`characterStart`/`lineCount` ✓ 逐行一致
  （零限制、整行裁剪、尾换行语义、maxBytes 截断取字符边界、`bytes.lastIndexOf(NEWLINE, maxBytes-1)`、
  `indexOf(NEWLINE, from-1)` 等细节全部对应；`decoder(ignoreBOM)` ↔ `String::from_utf8_lossy` 语义一致，
  BOM 在切片起始按文本保留）。
- `OutputBuffer`：`Utf8Stream`（增量解码、尾部不完整序列缓冲、flush 出 U+FFFD）✓；
  `push(chunk, skipped?)` ✓（string/skipped 先 flush pending；首字节块剥 BOM；skipped 仅 tail（TS throw
  Error ↔ Rust panic，豁免）；pending→skip→text 顺序）；`skip`（bytes==0 直接返回、计数、清 chunks）✓；
  `accept` ✓（head 满判据 `storedBytes>maxBytes || storedNewlines>=maxLines`；tail 丢弃循环
  `bytesAfter<=maxBytes+1 && newlinesAfter<=maxLines+1` 停止）；`snapshot` ✓（stored 拼接→boundOutput→
  keptLines→tail 或 chunks>1 时 tailMargin/整段重组→sanitize+total-kept 计数）；
  `tailMargin` ✓（byteStart=characterEnd(len-max-1)；lineStart 回扫 >maxLines；取 max）。
- `lines`/`countNewlines` ✓；`PROGRESS_BYTES_PER_SECOND=100*1024` ✓。
- `Progress` ✓（mark/markAndWait/stop 交还未结清等待者；schedule 定时器（generation 计数防旧唤醒）；flush
  一次在途；成功计费 `max(minInterval, bytes*1000/100KiB)`、失败 `minInterval`；resolve/reject 等待者；
  onError；dirty 再调度）。setTimeout/Date.now→tokio、PromiseWithResolvers→oneshot/ProgressWaiter（豁免，已声明）。
- 与 `tools.d/parity/output-parity.mjs` 的用例集对照：sanitize 8 例、bound 各 retain×各限制组合、
  buffer 边界（BOM、跨块字符、skip、panic 分支）在 Rust 单测中均有对应断言（output.rs 测试已读）✓。

## 3. 问题清单

### 缺失（1）
1. **generation.rs `answer()`**：最终边界分支缺少 `if !users.is_empty() { start_run(...) }`（TS
   `generation.ts` 第 ~520 行：`if (users.length > 0) await startRun(tx, conversationId, live, users);`）。
   最终边界选中的用户提交将停留在 placed 而不会启动新 generation（`end_run`/`apply_boundary` 均已核不含
   启动逻辑；`finish_tool_round` 的同款调用存在）。严重度：高。

### 逻辑差异（12）
1. **compaction.rs `run_summarize`（确证）**：未剥离 `deferred`。TS 显式解构丢弃；Rust 经 `stream_options()`
   原样带入。`settings.stream.deferred` 被设置时摘要请求行为分叉。严重度：中。
2. **generation.rs `classify`（确证）**：终态失败文案用 `{:?}`（PascalCase："Error"/"ToolUse"/"Aborted"）vs
   TS wire 值（"error"/"toolUse"/"aborted"）。严重度：低（仅无 errorMessage 的分支）。
3. **compaction.rs `run_summarize` cut 定位**：`findIndex` 未命中时 TS 为 -1（slice(0,-1)=除末条），Rust
   `unwrap_or(0)`（空）。该分支不可达（firstKept≤tail 恒在视图内），防御行为不同。严重度：低。
4. **compaction.rs `arguments_text`/序列化**（项目已知）：工具参数键序字典序 vs 插入序，影响摘要请求文本。
   严重度：低（已声明）。
5. **json.rs `assign_json` 对象合并**：叶操作按字典序 vs 插入序，影响 chord op 顺序（事件流顺序）。严重度：低。
6. **generation.rs `stream_response`**：存储 partial 未剔除 null（TS `omitUndefinedProperties`），
   `pi.live.generation.message` 存储形状不一致。严重度：低-中。
7. **events.rs `task_checkpoint`**：Completing/Terminal 任务 `unreachable!` panic vs TS `args={}`。严重度：低。
8. **events.rs `translate`**：tasks/slots_before 用 BTreeMap（按键排序）迭代 vs TS Map 插入序——同提交多
   任务/多槽位事件顺序可能不同。严重度：低。
9. **compaction.rs `run_summarize` user 消息**：`UserContent::Text`（字符串）vs TS `[{type:"text",text}]`
   块数组——wire 形状不同、语义等价。严重度：低。
10. **compaction.rs `cache_retention = None` vs TS `cacheRetention: "none"`**：若 pi-ai 对缺省的语义与
    "none" 不同则不等价（未在本次范围内确认 provider 层）。严重度：低（疑似）。
11. **harness.rs `inspect`**：未包 `read_on_line`，任务与提交扫描不在同一条 Session 线上。严重度：低。
12. **events.rs `watch_events`**：接入时已 abort 的拒绝载荷为通用 `SessionError::Aborted` vs TS
    `throw signal.reason`。严重度：低。

（另见 out-of-scope 观察：submissions.rs 的 busy-reject 用 `SessionError::Message` 对应 TS 的
`ConversationBusy` 错误类型；pi-ai retry 模式表较 TS 少 "server_busy"/"520" 等条目——均不在本 11 文件范围内。）

### 多余（0）
Rust 各 phase 对 checkpoint 形状不符返回 `SessionError::Message(...)` 的分支为不可达防御（TS 由调度器按
phase 路由保证），记为豁免而非多余。

## 4. 豁免清单（语言机制等价）
- 模块级 const（`CompactionTask`/`GenerationTask`/`startRun`/`createCompaction`）→ 工厂函数/DI/OnceLock；
- Proxy 草稿就地改写 → `Draft::set/delete/splice` 路径写入（op 等价）；
- `freezeJson`/`copyJson` → owned/Arc 值；`omitUndefinedProperties` → `skip_serializing_if`/`omit_null_members`；
- `contentText(string|blocks)` → 两函数拆分；`estimateContext.lastIndexOf`（身份）→ `rposition`（PartialEq）；
- `truncate` UTF-16 码元；`thresholdCompaction` i128 做差防下溢；
- setTimeout/Date.now → tokio；PromiseWithResolvers → oneshot/ProgressWaiter；
- throw Error ↔ panic（OutputBuffer skip 校验、resume 关闭断言）；try/catch ↔ catch_unwind；
- `SystemContent::Text` 包装、`entry(type,id)` → `entry(id)` 按 role 判别；泛型擦除（TaskId<R>/JsonValue）；
- `hooks.each(name)` → `HookRunner::handlers()` 逐个分发（多 hook 语义保留）；
- `RuntimeHookApi` 桥接；`TaskId` 无结果类型参数；`deferred` 的 Option<Value> 存储形式。

## 5. 验证局限
- 未运行任何测试/编译（bash 被拒）：结论为静态比对，未做动态验证。
- 显示层脱敏导致 5 处常量/表达式无法直接比对（§0），已用 Rust 测试与交叉结构推断，标注待复核。
- pi-ai 依赖层（retry 模式表、estimate 公式、provider 对 cache_retention/deferred 的处理）未在本次范围内
  逐项核对，仅核对了 harness 侧调用形状。
