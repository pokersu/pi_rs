# pi-durable

durable 会话运行时：document + transaction 状态模型、task-graph 执行、多后端存储。

1:1 复刻自 [earendil-works/pi](https://github.com/earendil-works/pi) 的 `packages/durable`
（v1.1.0，67 文件 / 20,404 行）。它是上游 v1.0.0 架构分拆后 harness 能力的新家
（旧的 lane/drive 状态机已不存在），因此**按新架构重写**而非搬运。
本 crate 正在分阶段落地，逐项计划与差异见根目录 `UPSTREAM-SYNC-v1.1.0.md`。

## 复刻范围

| 上游 | 本 crate | 状态 |
|---|---|---|
| `packages/chord`（子集） | `src/chord/` | ✅ `delta` / `context` / `json` / `tracker` / `state` |
| `src/types.ts` | `src/types.rs` | ✅ 数据模型 + 定义契约 + `Storage` / `Tx` |
| `src/ids.ts` / `errors.ts` / `truncate.ts` | `src/ids.rs` / `errors.rs` / `truncate.rs` | ✅ |
| `src/documents.ts` / `entries.ts` / `tasks.ts` | `src/documents.rs` / `entries.rs` / `tasks.rs` | ✅ |
| `src/storage/scan.ts` | `src/storage/scan.rs` | ✅ |
| `src/storage/memory.ts` | `src/storage/memory.rs` | ✅ |
| `src/storage/jsonl/` | `src/storage/jsonl/` | ✅ `codec` + `storage` |
| `src/storage/sqlite/` | `src/storage/sqlite.rs` | ✅ `storage`；`cloudflare.ts` 不移植 |
| `src/env/index.ts` | `src/env/index.rs` + `testing.rs` | ✅ 契约层 + node 实现 |
| `src/session/` | `src/session/` | ✅ `forks` / `observation` / `transaction` / `session` |
| `src/harness/` | `src/harness/` | ✅ 类型/定义/上下文/视图/事件/调度器/内置 Task/registry |
| `src/tools/` | `src/tools/` | ✅ read / write / edit / edit-diff / bash / image / path-utils / file-mutation-queue |
| `src/testing/` | `src/testing/` | ✅ storage-conformance / env-conformance / storage-benchmark |

## 模块

### `chord/` —— 不可变状态与 Op 应用

durable 有 63 处依赖 chord，全部落在这一层：

- `delta.rs`：`Op` / `PathSegment` / `Path`、`apply` / `apply_in_place` / `apply_immutable` /
  `apply_immutable_batches`、`overlap`。**`Truncate` 是 JS `slice(n)` 语义**（按 UTF-16 码元），
  实现上用 `encode_utf16`。
- `tracker.rs`：`track` / `Tracker` / `Change` / `Prepared`。上游 `Change.state` 是 Proxy，
  Rust 改为显式方法（`set` / `delete` / `append` / `truncate` / `splice` / `move_items`），
  产生的 Op 语义与最终值不变；超大数组的 piece-tree 启发式未移植（不影响结果）。
- `state.rs`：`ReplicatedState` / `Subscription`（`Drop` 即取消）/ `ReplicatedStatePublisher` /
  `AttachedReplicatedState`（游标连续性校验）/ `MutableReplicatedState`（`replicated_state` 工厂，
  `change` 用 `Change` 显式编辑 API）/ `ReplicatedStateReplica`（冷副本）/
  `registerReplicatedStateInternals`（自省注册表）。
- `state_codec.rs`：`ServiceStateEncoder` / `ServiceStateDecoder`（基于 delta 的 `encoder`/`decoder`
  path-interning，把 `Op` 快照/更新转成 `WireOp` 线格式，见 `src/chord/state_codec.rs`）。
- `context.rs`：`Context`（仅 abortSignal 部分）、`BACKGROUND_CONTEXT` / `TODO_CONTEXT`、
  `with_abort_signal` / `await_with_context`。

不纳入：`delta/diff.ts` 的 `diff_revisions`、`draft.ts` 的 `Draft<T>`（纯类型级映射，
Rust 侧等价于 `JsonValue`）。

### 基础层

- `types.rs`：品牌 ID / `Seq`、会话与分叉、上下文编辑、条目、任务（`TaskState` / `TaskRecord` /
  `TaskOutcome`）、提交（`SubmissionRecord`）、文档（`DocumentRecord` / `DocumentScope` /
  `DocDefinitionSpec` / `DocToken`）、分页与查询、`StorageWrite` / `CommitChange`、
  `TaskDefinitionSpec` / `Task`、`Tx` 与 `Storage` 契约。
- `documents.rs`：`defineDoc` 家族、地址解析、`resolve_*` 辅助。
- `truncate.rs`：会话截断（`truncateHeadOf` 的「按前缀 + 总量」路径）。
- `errors.rs` / `ids.rs` / `entries.rs` / `tasks.rs`：具名错误、ID 构造、条目与任务的小工具。

### `storage/` —— 三个后端

共享 `Storage` 契约（17 方法）与 `storage/scan.rs` 的扫描起点/游标/分页：

- `memory.rs`：参考实现。用 `BTreeMap`/`BTreeSet` 替代上游的
  「Map + 有序 ID 数组 + 手写二分」；`commit()` 内先只读校验再应用，失败不污染状态。
- `jsonl/codec.rs` + `jsonl/storage.rs`：两阶段写（sidecar 增量 → 主文件 marker）、
  `open()` 重放与未完成 sidecar 回收。
- `sqlite.rs`：`SqliteStorage`（`open` / `open_in_memory`、事务提交、全局 ID 归属、文档物化、关闭）。

**全局 ID 归属**（`checkGlobalIds`）在两个后端一致：`conversation` / `entry` / `document` 是
「不可变创建」，任何重复都拒绝；`task` / `submission` 是「最新记录替换」，同表可更新、跨表仍拒绝。

### `session/` —— 会话层

- `forks.rs`：`prepare_fork_document_copies` —— 分叉时按 `asOf` / `current` 策略选出并拷贝会话文档。
- `observation.rs`：`CommittedStateSource`（已提交化身 → chord 状态源）与 `CommittedWatch`
  （串行精确帧观察、100 帧有界待投递、退役/停止/取消/会话关闭终止）。
- `transaction.rs`：`Transaction` —— 表读写、文档获取/创建/退役、`settle_success` 组装原子批次、
  `adopt` 按指针替换并发布；`apply_submission_change` 的提交生命周期。
- `session.rs`：`SessionImpl` —— 一条提交线（公平 FIFO 异步互斥）、已加载文档的 tracker 缓存、
  已提交发布；`create_session`、`commit` / `commitWith` / `readOnLine` 与四个读取 API
  （`snapshot` / `snapshotAsOf` / `documentState` / `watchDoc`）、`close` 与两个订阅、
  [`Session`] 契约与 [`SessionHooks`]。

上游把 `history` / `fork` 放在文档记录**顶层**（只有会话文档声明），`scope` 只含
`{kind, conversationId}`；本 crate 已按此拆分（`DocumentSemantics` 为定义侧语义）。

**session 内核的实现取舍**

- **继承 → 钩子 trait**：上游 `HarnessImpl extends SessionImpl` 并覆盖 `conversationCreated` /
  `beforeClose`；Rust 用 `SessionHooks`（默认 `DefaultSessionHooks` 为空实现），P5 的 harness 注入。
- **Promise 链 → 公平异步互斥**：上游用 `#tail` Promise 链串行提交；Rust 用 `tokio::sync::Mutex`
  （FIFO 公平锁），等待顺序等于调用顺序，job 失败不影响后续 job。
- **可重复 await 的 `close()`**：上游 `#closing` 是 Promise；Rust 用 `Shared<BoxFuture>`，
  多次 `close` 共享同一结果；调用方取消只中断「等待」，不阻止存储关闭（`withoutAbortSignal`）。
- **取消函数 → RAII**：`subscribeCommits` / `subscribeClose` 返回 `SessionSubscription`，`Drop` 即取消。
- **重载 + rest args → 显式参数**：上游靠 `resolveAddress(definition, ...args)` 的参数个数推断
  `conversationId` / `key` / `context`；Rust 显式接收（与 `documents::resolve_address` 一致）。
- **泛型方法不进 dyn trait**：`commit` / `commitWith` / `readOnLine` / `conversationDocumentOnLine`
  保留为 `SessionImpl` 的固有方法；`Session` trait 覆盖其余（对应上游 `DocumentReader` 的用途）。
  闭包签名是 `for<'a> FnOnce(&'a Transaction) -> BoxFuture<'a, _>`（Rust 无法在 HRTB 里返回 impl Future）。

### `harness/` —— agent harness（P5，分阶段落地）

已落地的基础工具（P5a，对应上游 `harness/{json,output,util}.ts`）：

- `json.rs`：`assign_json` —— 把局部 JSON 值逐叶写入容器，避免 chord 把整对象记录成一次 `set`。
- `output.rs`：`sanitize_output`（移除控制字符）、`bound_output`（按行/字节的精确切片）、
  `character_end`、`OutputBuffer`（有界运行输出：`head` 装满即停、`tail` 在快照时丢前缀，
  全流计数保证丢弃量精确）。UTF-8 增量解码自实现（`Utf8Stream`）。
- `util.rs`：`scan_all`、`closed_error`。

已落地的类型契约（P5b，对应上游 `harness/{types,define}.ts`）：

- `types.rs`：数据模型与扩展契约。数据模型包含提交（`SubmissionDraft` / `SettledTask`）、
  工具结果（`ToolExecutionResult` / `ToolDiagnostic` / `ToolControl`）、agent
  （`AgentState` / `AgentChange` / `Agent`，`undefined`/`null`/值 三态用 `FieldChange`）、
  策略与设置（`Settings` + 上游的四组 `DEFAULT_*` 常量）、视图与探查
  （`ContextView` / `TaskInspection` / `HarnessInspection`）。扩展契约包含 `ToolRegistration`、
  `PromptSection`、`Extension` / `Wrap` / `HookRegistration`、`RegistrySnapshot`（不可变快照，
  用 struct 而非上游的 interface）、`ToolExecutionApi`、`ConversationHandle` / `Submission`、
  以及 generation / tool / compaction 三类 Hook 与 `HookApi`（`Partial<Hooks>` →
  带默认实现的 trait）。
- `define.rs`：DSL 构造器（`define_extension` / `define_tool` / `section` / `hook` /
  `wrap_tool` / `wrap_section`）。

**有意推迟到 P5h**：`Harness` / `Conversation` / `Registry` 等顶层编排接口（理由见模块文档）。

已落地的内置文档与事务内操作（P5c，对应上游 `harness/{agent,provider,usage,inbox,live}.ts`）：

- `agent.rs`：`pi.agent` 文档、`resolveSettings`（四组 `DEFAULT_*` 常量）、`configure` / `addTools` /
  `applyChange`、`createAgent`、`agentHooks`、`resolveAgent` / `selectExtensions` / `applyWrap`。
- `provider.rs`：`pi.provider`（会话的 provider 侧稳定身份）。
- `usage.rs`：`pi.usage` 账本、`recordUsage`、`addUsage` / `addUsageState`。
- `inbox.rs`：`pi.inbox` 队列、`prepareBoundary` / `applyBoundary`（按 spec §6 选择与放置）/ `isStale` /
  `removeInboxItem` / `withdrawQueuedInputs`。
- `live.rs`：`pi.live` 活动状态、`endRun`、压缩状态增删查、`toolSlot` / `finishSlot` / `clearProgress`。

这些模块的端到端行为见 `tests/harness_documents.rs`（真实 Session + Transaction 提交）。

已落地的上下文与视图（P5d，对应上游 `harness/{context,view}.ts`）：

- `context.rs`：上下文边界捕获（`ContextBounds` / `captureContextBounds`）、活动条目、
  `readContext` / `readContextFrom`（增量复用上一次区间）、模型上下文派生、`orderToolResults`
  （按调用顺序安放工具结果，并合成缺失结果）。
- `view.rs`：`ConversationView` 与 `ConversationViews` 挂载——每个会话至多一个，由第一个观察者
  在 Session 线上建立、随最后一个观察者丢弃；随提交出版物增量推进条目与内置文档；
  `state()` 给出 chord 状态，`watch()` 给出串行精确帧观察。

其端到端行为见 `tests/harness_views.rs`。

已落地的事件流（P5e，对应上游 `harness/events.ts`）：

- `events.rs`：`AgentEvent`（24 种）/ `MessageChange` / `SnapshotEvent` / `AgentEventStream`；
  纯翻译逻辑 `translate`（一次发布引发的全部事件，按 spec §9.4 顺序）、`message_changes`
  （视图操作 → 消息改动）、`tool_update`（输出裁剪/追加、细节与诊断的清空）。
- `watchEvents()` 需要 Harness 对象，推迟到 P5h。

已落地的提交与运行时契约（P5f-2，对应上游 `harness/submissions.ts`）：

- `submissions.rs`：`Submissions`（接纳 / 查询 / 等待 / 撤回）与 `admit_submission`
  （spec §6：request ID 复用、繁忙入队、`whenBusy: reject` 拒绝、空闲直接放置或追加、stale 写入）。
  上游 import 的 `startRun` 改为注入，因此本模块先于 generation 落地。
- 运行时契约（`types.rs`）：`RunningTask` / `NextTaskState` / `TaskRuntime` / `HookRunner`；
  `TaskDefinitionSpec` 扩展出 `run_phase` / `abort` / `migrate` / `hooks`（默认实现返回错误，
  不跑阶段的测试桩无需实现）。

其端到端行为见 `tests/harness_submissions.rs`。

已落地的调度器纯逻辑（P5f-3a / P5f-3b-1 / P5f-3b-2a，对应上游 `scheduler.ts`）：

- `scheduler/ownership.rs`（P5f-3a）：任务节点与所有权树遍历（`above` / `chain_known` / `owned_live` /
  `in_scope` / `below_cancelled` / `idle` / `live_records`）与记录派生（`parent_of` / `node_of` /
  `with_state` / `failed_outcome` / `cancellation_intent` / `can_reserve` / `overlay_of` / `json_equal`）。
  上游的 `#node` / `#edge` 改为 [`OwnerLookup`]，因此这些纯逻辑可脱离状态机单测。
- `scheduler/state.rs`（P5f-3b-1）：`SchedulerState` 承载 `#live` / `#edges` / `#conversationOwners` /
  `#settled` / `#failedMigrations` / `#failFastChecks` / `#contexts` 与七个标志，并实现 `OwnerLookup`；
  `observe` 对应 `#observe(publication)` 的全部逻辑，把副作用收集进 `ObserveOutcome` 交给执行层
  （上游直接做副作用；这些副作用幂等，所以收集是安全的）。
- `scheduler/reserve.rs`（P5f-3b-2a）：`waiting_on` / `fit` / `resolve` / `inspect_task`——
  「一个待处理记录现在能不能被保留」的全部判定（含旧版本迁移与迁移失败记忆）。
- 执行层（调用生命周期、reconcile、上下文档存与过期、`TaskRuntime` 实现）待落地。

已落地的系统提示规划（P5g-1，对应上游 `prompt.ts`）：

- `prompt.rs`：`replay_sections`（系统消息的章节重放）、`render_sections`、
  `plan_system_entries`（baseline / 最小补丁 / 重排）、`plan_tools`、`plan_sections`。
  上游的 `Map` 在 Rust 侧必须是 `IndexMap`：章节顺序是语义的一部分。

已落地的 generation 运行控制与阈值判定（P5g-2，对应上游 `generation.ts` 第 306 行与第 664–681 行）：

- `generation.rs`：`make_start_run`（`startRun` 的工厂形态）、`create_generation`、`hand_over`，
  以及 `threshold_compaction` / `ThresholdOver`（`thresholdCompaction` 的阈值判定）。
  上游直接引用模块级常量 `GenerationTask`；Rust 侧该定义要到 P5g-2 才落地，因此 `startRun`
  接受任务定义作为参数，运行控制可以先接线到 `Submissions` 的 `StartRun` 注入点。
  `GenerationTask` 本体（上游其余 663 行）仍待落地。

已落地的压缩选段与摘要文本（P5g-2，对应上游 `compaction.ts` 的常量与纯函数部分）：

- `compaction.rs`：常量（`TOOL_RESULT_MAX_CHARS` / `SUMMARY_PREFIX` / `SUMMARY_SUFFIX` /
  `SUMMARIZATION_SYSTEM_PROMPT` / `SUMMARIZATION_PROMPT`）、类型 `CompactionInput` /
  `SummaryRequest` / `CompactionCheckpoint`、`make_create_compaction`（`createCompaction` 的工厂形态）、
  `select_cut` / `summarized_messages` / `estimate_context` / `serialize_conversation`。
  上游的 `contentText` 拆成 `content_text_of_user` / `content_text_of_blocks`；
  `serializeConversation` 的工具调用参数按字典序（serde_json 的有序 map）。
  `CompactionTask` 本体仍待落地。

后续阶段见 `UPSTREAM-SYNC-v1.1.0.md` 的 P5f-3b–P5h。

已落地的任务图与等待（P5f-1）：

- `task_graph.rs`：`TaskGraphView` —— 由第一个观察者在 Session 线上建立、随最后一个观察者丢弃；
  从提交出版物增减节点（终态任务离开图）、追加任务拥有的会话；`TaskGraph` / `TaskGraphNode` /
  `TaskGraphState`（`pending` / `running` / `waiting` / `completing` + 结果判别符）。
- `util.rs` 的 `Waiters`：按 key 挂起等待，由 `resolve` / `reject_all` / 上下文取消结清。

其端到端行为见 `tests/harness_task_graph.rs`。

### `env/`

从 `env/index.ts` 抽取的**契约层**：`FileSystem` / `Shell` trait、`FileError` / `ExecutionError`、
`BinaryReader` / `DirReader` / `FileWatcher` / `LineScan`。`env/node.ts`（Node 实现，1,226 行）
属 P6；测试用 `env/testing.rs` 的 `InMemoryFileSystem`。

## 与上游的差异

**语言机制所致（非简化）**

- **sqlite 的异步 facade 豁免**：上游为跨运行时（Node `node:sqlite` / Cloudflare Durable Object）
  引入 `SqliteDatabase` / `SqliteExecutor` 异步接口，附带事务队列、admitted-reads drain、
  结算控制与「事务句柄过期」检查（`database.ts` 248 行）。Rust 直接持有 `Arc<Mutex<Connection>>`：
  Mutex 已串行化全部操作，事务在 `connection.transaction()` 内原子完成，上述语义由同步原语等价覆盖。
- **`close()`**：上游返回幂等的 `closing` promise 并等待已受理的多查询读排空；Rust 置 `closed`
  标记后立即返回 `Ok`（读在 Mutex 内同步完成，不存在跨 `await` 的已受理读），重复调用同样成功。
  关闭后一切操作返回 `StorageError::Closed`。
- **`mint_id`**：`Storage::mint_id` 的签名无法返回错误，关闭后以上游 `assertOpen` 抛异常为对等语义
  改为 panic。
- **异常 → `Result`**：上游用 `throw` 报错，Rust 统一为 `StorageError` / `DeltaError` 等。
- **Proxy / `Object.defineProperty`**：Rust 无原型链，`serde_json::Map` 天然免疫保留键问题，
  相关检查省略。

**实现取舍**

- **按点物化（`DocumentPoint::At`）**：三个后端都按「选定点之前（含）的修订」重放增量——
  memory / jsonl 用修订自带的 `seq`，sqlite 的 `revisions` JSON 列同样随附每条修订的 `seq`
  （上游修订数组自带该字段）。非当前点读取 `currentOnly` 记录会报错（与上游一致）。

- **sqlite schema**：上游用结构化列 + 125 行 migrations 以支持 SQL 索引与谓词下推；Rust 用 JSON 列
  （`records(table_name, id, seq, data)` + `documents(id, record, revisions)` + `meta`），
  过滤在内存里做。**数据往返与语义一致**，差的是查询下推能力。
- **`cloudflare.ts` 不移植**：无对应运行时。
- **legacy-v3 JSONL 迁移不移植**：按明确要求不做。
- **`ReplicatedState`（draft 形态）暂未实现**：依赖 Proxy 式 `Draft`，durable 只用 Attached 形态。

## 阅读顺序

1. `src/types.rs` —— 先建立记录模型与两个契约（`Storage` / `Tx`）
2. `src/storage/memory.rs` —— 参考后端，最快看清存储语义（ID 归属、文档物化、分页）
3. `src/storage/scan.rs` —— 扫描起点与游标规则（游标延续其创建时的顺序）
4. `src/storage/jsonl/` —— 两阶段写与重放
5. `src/storage/sqlite.rs` —— 同一契约的 SQL 实现
6. `src/chord/delta.rs` → `tracker.rs` → `state.rs` —— Op 的生成与应用

## 验证

```bash
cargo test -p pi-durable      # 单元 + 集成测试
cargo clippy -p pi-durable --all-targets
```
