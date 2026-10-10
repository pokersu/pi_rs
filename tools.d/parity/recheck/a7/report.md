# durable harness 后半 1:1 复刻审计报告（recheck a7）

- 基线：upstream `packages/durable/src/harness/`（v1.1.0，commit abe508e1b）
- 复刻：`crates/pi-durable/src/harness/`（含 `scheduler/{mod,exec,expiry,ownership,reserve,state}.rs`）
- 方法：逐文件、逐导出符号、逐方法对照。本次会话共享 checkout 禁用了 bash，全部对照通过只读 `read` 工具完成；未运行 Rust 测试（见「证据」）。
- 结论：**整体 1:1 复刻度高**；调度器状态机（observe/reserve/exec/reconcile/ownership/expiry）与 prompt 章节规划逐行吻合。共记 **缺失 1、多余 2、逻辑差异 14**，其中约一半是损坏数据/竞态/错误文案等边缘情形，另发现 2 处会影响正常路径的行为差异（`bound_content` 文本项处理、`run()` 的 env 错误路径）。

---

## 逐文件结论

| TS 文件 | Rust 文件 | 结论 |
|---|---|---|
| prompt.ts | prompt.rs | ✅ 1:1（replay/render/planSystemEntries/planTools/planSections/systemEntry 全部核对；IndexMap 顺序语义正确；plan_sections 多出的长度检查为恒真冗余，无行为差异） |
| provider.ts | provider.rs | ⚠️ 基本 1:1；损坏文档边界行为不同（见差异 #12） |
| registry.ts | registry.rs | ⚠️ 基本 1:1（BUILTIN_TASKS→create_registry+OnceLock 豁免；panic vs throw 豁免）；错误消息格式与通知持锁见差异 #13/#14 |
| scheduler.ts | scheduler/{mod,exec,expiry,ownership,reserve,state}.rs | ⚠️ 结构拆分不属差异；observe 副作用收集、reserve、exec、reconcile、finalize、terminate、commitState、validateWait、runtime 逐段核对通过；差异见 #6/#7/#9/#10/#11/#15/#16 及边缘清单 |
| submissions.ts | submissions.rs | ⚠️ 基本 1:1；差异见 #6/#7/#8；startRun 注入为豁免 |
| task-graph.ts | task_graph.rs | ✅ 基本 1:1（build/advance/nodeOf/stateOf 全核对；JSON 相等 vs 结构相等语义等价）；边缘 #17 |
| tool.ts | tool.rs | ❌ 存在正常路径差异（#1/#2/#3/#4/#5） |
| types.ts | types.rs | ⚠️ 基本 1:1；`entry` 的 kind 过滤参数缺失（缺失 #1）；TypeBox→JSON Schema、重载拆分、泛型擦除均豁免 |
| usage.ts | usage.rs | ⚠️ 基本 1:1；损坏数据防御见多余 #2；omitUndefinedProperties→剔除 null 为豁免 |
| util.ts | util.rs | ⚠️ 基本 1:1；Waiters.keys() 顺序与固定 AbortError 见 #13 与豁免 |
| view.ts | view.rs | ✅ 基本 1:1（build/advance/prefixed/attach/state/watch 全核对；conversationViews 由 P5h 决策豁免）；attach 失败时挂载残留见边缘 |

---

## 缺失（N=1）

1. **`TaskRuntime.entry` 丢失 kind 过滤参数** — 上游 `entry(token?, id, context)`：传 `token` 时按 `entry.kind === token.kind` 过滤，不匹配返回 `undefined`。Rust `TaskRuntime::entry(id, context)` 无 token 参数（`types.rs` / `exec.rs`）。当前唯一调用方 tool.rs 的 `read_call` 用 `Message::Assistant` 角色匹配兜住了语义，故暂不可观察，但 API 面不完整。

## 多余（N=2）

1. **tool.rs `prepare`/`validate` 的非对象防御检查** — TS `prepareArguments(args) as JsonObject` / `validateToolArguments(...) as JsonObject` 是 unchecked cast，返回非对象会带着脏值继续（通常在下游爆错）。Rust 新增 `"prepareArguments returned a non-object"` / `"validated arguments are not an object"` 两个上游不存在的错误文案分支（`tool.rs` prepare/validate）。
2. **usage.rs 对损坏账本数据的防御** — `record_usage` 对非对象形状返回 SessionError；`add_usage_json` 在 total 缺 `cost` 时补建（TS 直接 `total.cost.input +=` 抛 TypeError）；`add_usage_state` 用 `let _ =` 吞掉合并错误（TS 就地抛出）。上游无这些分支。

## 逻辑差异（N=14）

1. **tool.rs `bound_content` 保留非 keep 文本项（正常路径）** — TS `boundContent`：`for (const item of content) { if (item.type !== "text") result.push(item); else if (item === keep) result.push({...item, text: bounded.text}); }` —— 除 keep 外的**所有文本项被丢弃**，结果只剩一个被裁剪的文本项。Rust 第三个分支 `Text(text) => result.push(Text(text))` 把其余文本项**原样保留**，越界后文本总量未真正受限，且内容形状与上游不同（多文本项时可见）。
2. **tool.rs `run()` 的 env 构建错误路径（正常路径）** — TS 把 `runtime.env(context)` 放在 try 内：env 抛错 → 生成 `tool_error` 诊断、追加结果条目、以 `failed`（`Tool ${name} threw`）结算。Rust `let env = runtime.env(...).await?;` 在 match 之外直接 `?` 传播 → 无结果条目、不结算、任务错误路径交给执行层。另：Rust `started_at` 在 env 之前取（durationMs 含 env 构建耗时），TS 在 env 之后取（只计 execute 耗时）。
3. **tool.rs `publish_progress` 字节计数不全** — TS `bytes` 除文本差额外还累加 `utf8ByteLength(JSON.stringify(details))` 与新增 diagnostics 的 JSON 字节，用于 Progress 按字节节流。Rust 只统计 `snapshot.text[shared..]` 的字节，details/diagnostics 写入不计费（节流节奏变快，不影响正确性）。
4. **tool.rs beforeTool hook 收到原始 arguments** — TS 每次调用传 `{ ...call, arguments: args }`（当前累计 args，前序 hook 的替换对后序 hook 可见）。Rust `hook.before_tool(&call, ...)` 传 `read_call` 的原始 call，`decision.arguments` 只更新任务侧 `args`，后序 hook 读到的仍是原始参数。
5. **`append_tool_result` 消息 JSON 形状** — TS 对 `details`/`usage` 为 `undefined` 时整键省略，且无 `addedToolNames` 字段。Rust 直接存 `ToolResultMessage`（pi-ai serde 对 `details`/`usage`/`added_tool_names` 无 `skip_serializing_if`）→ 存出 `"details": null, "usage": null, "addedToolNames": null`。该消息「就是模型看到的存储内容」，形状不一致。
6. **submissions.rs `wait()` 关闭检查时点** — TS 在 readOnLine 任务内检查 `this.#closed`（close 先置位再 rejectAll，因此后到的 line job 会直接抛 closedError）。Rust `closed_now` 在进入 `read_on_line` **之前**捕获：若关闭发生在捕获与 line job 之间，会注册一个 rejectAll 已经扫过、永不结清的等待者（只能等 context 取消），存在悬挂窗口。
7. **submissions.rs 请求冲突与 ConversationBusy 的错误面** — 类型冲突文案：TS `Request X already identifies a submission of type Y`，Rust `...of the other type`（不报已有类型）。`ConversationBusy` 在 TS 是专用错误类（可被宿主捕获类型判别），Rust 退化为 `SessionError::Message("Conversation {id} is busy")`。
8. **submissions.rs `submit()` 的 now/queueModes 求值时点** — TS 在 commit 回调内求值 `this.#now()` / `this.#queueModes()`（Session 线上）；Rust 在进入 commit 之前求值。时钟/队列模式读取晚于上游一个提交窗口。
9. **reserve.rs `resolve()` 迁移失败的记忆与上报** — TS 迁移抛错 → `failedMigrations.set` + `report(error)`（同一注册表定义下不再重试）。Rust `migrate` 返回 `None`（对应抛错）→ 只返回 Blocked，**不写 failed_migrations、不上报**，下一轮 reserve 会反复重试同一失败迁移。
10. **exec.rs `decide()` 替换定义上报文案** — TS `new Error("Task X keeps running under its old Y definition", { cause })`：消息不含 cause，cause 在 Error.cause。Rust 消息带后缀 `({cause})`（`...definition (missing_task)`）。
11. **ownership.rs `live_records` 产出旧记录而非 overlay 候选** — TS `yield candidate`（overlay 替换后的记录）；Rust 过滤用候选的终态判断，但 `.cloned()` 产出的是 live 里的旧记录。当前调用点（owned_live 只看 id/background、finalize 只在「仅打标记」的 reconcile 提交中读状态）不可观察，属潜在偏差。
12. **provider.rs 损坏 `pi.provider` 文档** — TS `existing.sessionId` 缺失时直接返回 `undefined`（静默）；Rust 走迁移提交路径、`tx.doc` 时 `unwrap_or_default()` 返回空串成功。两边对损坏数据的落点不同。
13. **util.rs `Waiters.keys()` 顺序** — TS `Map` 插入序；Rust `BTreeMap` 键序。唯一消费者 settle_idle 按 key 独立判定，无语义差别（expiry.rs 注释已承认），保留记录。
14. **registry.rs 通知期间持锁** — TS `#publish` 先 `[...this.#listeners]` 快照再逐个调用（监听者内可安全 install/uninstall）。Rust `notify()` 在 `self.listeners.lock()` 持锁期间调用监听者：监听者在回调里再改注册表（subscribe/install）会 Mutex 死锁。

---

## 边缘差异（已核实，判为低影响，不计入上表）

- `plan_sections` 的 `patched_order.len() != desired_order.len()` 恒真冗余检查（无行为差异）。
- registry `validate_extension` 章节键错误：TS `TypeError(Section key ${JSON.stringify(key)} must match /^[a-z][a-z0-9_-]*$/)`；Rust panic + `{key:?}` Debug 格式（panic-vs-throw 本身属豁免，消息格式有出入）。
- exec.rs `wait_for_task` 存储读取用 `deps.context`（TS 用调用方 context）→ 调用方取消不中断该读。
- exec.rs `InvocationRuntime` hook 分发：`HookRunner::handlers()` 读已解析的 agent 缓存，未解析时静默返回空（TS `hooks.each` 会 `await agent()` 触发按需解析）。tool 任务因 call 阶段先解析 agent 而不受影响；generation/compaction 若先调 hooks 再调 agent 会跳过 hooks。
- exec.rs agent 缓存并发竞态：两个并发调用同时解析时 Rust 可能各解析一次（TS 共享首次 Promise）。
- task_graph.rs `phase_of`：checkpoint 无 `phase` 时 TS 序列化为缺键（undefined），Rust 为 `""`。
- tool.rs `final_result` 截断判定：TS 引用相等（`final.content === content`），Rust 结构相等（hook 返回等值新对象时行为分叉）。
- tool.rs `api.details()`：TS 在写入 details 前先 `throwIfAborted`，Rust 先存再等取消 → abort 时 details 已入库。
- view.rs / task_graph.rs `attach`：create 或 closed/abort 检查失败时，Rust 已把 mount 插入 map（TS 不注册）；值仍随 publication 推进，最终一致。
- scheduler `open` 的 scan 顺序、`load_scopes` 返回会话列表排序（BTreeSet）等：消费者不依赖顺序。

## 豁免（语言机制等价，已核对）

- 模块级 const → `LazyLock`/`OnceLock` 工厂：`ProviderDoc`/`UsageDoc`/`InboxDoc`/`LiveDoc`/`AgentDoc`、`BUILTIN_TASKS → create_registry`（generation/tool/compaction 循环依赖用 OnceLock 打破）。
- throw → panic：registry `validate_extension`/`#publish`、runtime `now`/`report` 调用结束后抛错 → assert。
- `AbortSignal.reason`：pi-ai Rust 侧 AbortSignal 无 reason，`SessionError::Aborted(AbortError)` 为平台统一替代（util Waiters、view/watch、exec sleep、step 等所有取消点一致）。
- TypeBox schema → JSON Schema（`Tool.parameters`、`*ToolInput` 等）。
- `undefined`/`null` 三态 → `Option` + `skip_serializing_if` / `FieldChange` / `to_json_without_nulls`（ToolControl、Usage 序列化、submissions 值拷贝）。
- 泛型方法 → boxed 闭包 + HRTB（`ToolExecutionApi::commit`→`CommitOperation`、`createTask`→`TaskCreationOptions`、`TaskRuntime::commit`→`TaskCommit`）。
- 重载拆分：`memo`/`memo_or`；`entry(token?,...)` 的重载拆分导致缺失 #1，其余等价。
- `conversationViews(harness)` WeakMap → P5h 决策由 HarnessImpl 直接持有 `ConversationViews`（view.rs 头注 + harness.rs 已落地）。
- 挂载值以 JSON 保存并在推进时重建（view/task_graph），freezeJson 与 owned 语义等价。
- `setTimeout`/`unref` → `ExpiryPlan` + tokio 定时器（expiry.rs 决策与执行层分离，决策逻辑逐分支对照一致）。
- `startRun` 依赖注入（submissions.rs，避免模块环）；`#resolve` 的 report 移为 `ResolutionOutcome::report`；`#inspectTask` 的 `has_invocation` 参数——均为等价重构。
- `#contextRetentionMs` 不再 catch 设置异常（宿主保证不 panic，expiry.rs 头注声明）。
- `TaskInspectionState::Blocked` 不带 `error`（P5b 已决策，types.rs 头注声明）。
- scheduler 单文件 → 5 模块拆分：纯逻辑（ownership/reserve/expiry/state）与 I/O（exec）边界清晰，逐方法映射完整（open/resume/join/abort/waitForTask/waitForIdle/abortConversation/observe/seal/kick/drain/reserve/start/run/decide/runAbort/step/terminate/commitState/validateWait/end/runtime/read/gated/sleep/watchDoc/loadScopes/loadChain/finalize/anyFailed/reconcile/settleIdle/scheduleExpiry/inspect 全部有对应实现）。
- 七标志齐备：`reconcile_scheduled`/`cascade_pending`/`enabled`/`closing`/`dirty`/`draining` 在 `SchedulerState`，`#unsubscribeRegistry` 在 `SchedulerShared`；observe 副作用按 settled/reconcile/settle_idle/kick 顺序收集并由执行层按序执行，与 TS 同步监听器的效果一致。

## 证据

- 全量读取并逐方法对照的文件（TS 11 + Rust 13，共 24 个）：`prompt/provider/registry/scheduler/submissions/task-graph/tool/types/usage/util/view`（.ts/.rs 成对），`scheduler/{mod,exec,expiry,ownership,reserve,state}.rs`。
- 佐证抽查：`pi-ai/src/types.rs`（AbortSignal 无 reason；`ToolResultMessage` 的 `details`/`usage`/`added_tool_names` 无 `skip_serializing_if`，支撑差异 #5）、`harness/inbox.rs`（`user_entry_draft`/`InboxItem::to_json` 形状一致）、`harness/harness.rs`（ConversationViews 由 Harness 直接持有，豁免成立）、`harness/mod.rs`（模块清单与分阶段落地说明）。
- 受限说明：共享 checkout 下 `bash`/`exec_shell` 被写保护拒绝，故未运行 Rust 侧测试；本报告为静态逐行审计结论。除第 1–5、6、9、10、12 号差异外，其余均不影响正常路径的可见行为。
