# 同步方案：v0.99.2 → v1.1.0（架构重写）

> 基线：`v0.99.2`（`005af57d8`，2026-09-30）→ 目标 `v1.1.0`（`abe508e1b`，2026-10-07）
> 区间：185 个 commit / 6 个 tag（v1.0.0、v1.0.1…v1.0.4、v1.1.0）
> 结论：**这不是增量同步，是架构分拆**。方案性质与 `UPSTREAM-SYNC-v0.99.2.md` 完全不同。

## 一、上游发生了什么

`v1.0.0`（2026-10-01）发布了一个 breaking change，把 `@earendil-works/pi-agent` 拆成两半：

- **`@earendil-works/pi-agent-core`**（原包改名，`packages/agent/src` 从 117 文件 → **6 文件**）
  只保留 `Agent`、agent loop、proxy stream 及其类型。**整个 `harness/` 被移除**（lane、drive、
  session、reducer、events、hooks、execution、compaction、skills、prompt templates、telemetry schemas、
  search、pico3 全部删除，−31,094 行）。
- **`@earendil-works/pi-durable`**（`packages/durable`，67 文件 / **20,404 行**）
  harness 的新家 —— 但**是重写，不是搬运**。

另：`packages/session-backends` 消失；`packages/env` 新增（远程环境 / SSH，6 文件）；
`packages/telemetry` **零改动**；`packages/ai` 正常演进（+1023/−440）。

## 二、旧架构 vs 新架构（为什么不能搬）

| 维度 | v0.99.2 `harness/`（我们已 1:1 复刻） | v1.1.0 `durable/` |
|---|---|---|
| 执行模型 | lane + drive 状态机（`OperationState` 枚举 + `drive/*` 叶子） | **task-graph** + `TaskRuntime`（`TaskGraph`/`TaskGraphNode`/`scheduler`） |
| 状态载体 | `Session` + `Entry` + reducer 归约 | **Document/Doc**（`defineDoc`/`DocFamily`）+ `Tx` 事务 + `CommitChange` |
| 事件 | `HarnessEvent`（29 种）+ `HarnessEventBus` | `AgentEventStream` / `SnapshotEvent` / `watchEvents` |
| 扩展点 | `HookRegistry`（hook 名 + gate） | `defineExtension` / `hook` / `section` / `wrapSection` / `wrapTool` |
| 存储 | memory + jsonl（`Storage` trait） | memory + jsonl + **sqlite**（含 Cloudflare Durable Object 后端） |
| 工具 | `AgentHarnessTool` + execution/tools | `defineTool` + `ToolTask` + `ToolExecutionApi` |
| 依赖 | 仅 pi-ai | pi-ai + **chord**（`delta` 不可变状态 apply、`Context`、`JsonValue`） |

结论：**新架构没有一行可直接搬运**，需要按 v1.1.0 源码重新实现。

## 三、范围与规模

| 上游包 | 文件 | 行数 | 现状 | 本次要做 |
|---|---:|---:|---|---|
| `packages/agent`（agent-core） | 6 | 2,519 | `crates/pi-agent` 的 agent 部分已覆盖 | 小改：`durationMs`、`streamProxy` 返回值、移除 harness 导出 |
| `packages/chord`（**子集**） | 8 | ~4,100 | 无 | `delta`（3,635）+ `context`（121）+ `types`（Context/JsonValue，339） |
| `packages/durable` | 67 | 20,404 | 无 | 全新复刻 |
| `packages/ai` | 197 | +1023/−440 | `crates/pi-ai` 子集（44 文件） | 增量跟进（多数属声明范围外） |
| `packages/telemetry` | 6 | 935 | `crates/pi-telemetry` 已 1:1 | **无** |

**需新写约 24,500 行 TS → Rust**（Rust 通常再多 20–50%）。作为参照，现有 `crates/pi-agent`
（含 v0.99.2 全套 harness）也是两万行量级 —— 相当于**再来一遍同等规模的工作**。

durable 内部构成：

| 子模块 | 文件 | 行数 | 说明 |
|---|---:|---:|---|
| `harness/` | 22 | 7,015 | 核心：harness / task-graph / scheduler / registry / events / define / live / inbox / provider / usage / view / generation / tool / compaction / prompt / context |
| `storage/` | 11 | 3,221 | memory / jsonl / sqlite（3 个入口）/ scan |
| `testing/` | 7 | 2,810 | storage-conformance / env-conformance / runner / benchmark |
| `env/` | 5 | 2,250 | node env + watch + line-scan + decode |
| `session/` | 4 | 2,009 | session / forks / observation / transaction |
| `tools/` | 10 | 1,313 | bash / read / write / edit / edit-diff / image / path-utils / file-mutation-queue |
| 顶层 | 7 | 1,786 | types(1,107) / documents / truncate / entries / errors / ids / index |

## 四、分阶段计划

**P0 骨架与基线**〔✅ 已完成 2026-10-08〕
- `upstream/` 检出切到 `v1.1.0`
- 删除旧 harness：`crates/pi-agent/src/{harness,search,node.rs}`（约 80 文件）+ 4 个 harness 测试
- `agent.rs` 的转换函数改为上游 `defaultConvertToLlm` 的过滤语义（只保留 system/user/assistant/toolResult）
- crate 改名 `pi-agent` → **`pi-agent-core`**（目录 + Cargo.toml + workspace 成员 + `pi-tools` 依赖 + import）
- 新建 `crates/pi-durable` 骨架并加入 workspace
- `demo-agent` 整体依赖旧 harness，**已从 workspace 移出**（代码保留，待 durable 就绪后重建）
- 验证：`cargo check --workspace --all-targets` 无错误；`cargo test --workspace` 64 passed / 0 failed
  （从 90 降至 64：删除了 harness 侧测试）

**P1 chord 子集**〔✅ 已完成 2026-10-08，1,992 行 / 35 个测试〕

durable 有 63 处 chord import，全依赖这一层。已完成（`crates/pi-durable/src/chord/`）：

- ✅ `context.rs`（187 行）：`Context`（仅 `abortSignal` 部分 —— durable 未使用 `ContextKey::value`）、
  `BACKGROUND_CONTEXT` / `TODO_CONTEXT`、`with_abort_signal` / `without_abort_signal` / `with_cancel`、
  `await_with_context`
- ✅ `json.rs`（28 行）：`copy_json`
- ✅ `delta.rs`（684 行）：`Op` / `PathSegment` / `Path`、上游元组形式的 serde、
  `apply` / `apply_in_place` / `apply_immutable` / `apply_immutable_batches`、`overlap`
- ✅ `tracker.rs`（335 行）：`track` / `Tracker` / `Change` / `Prepared`、超限退化为整值替换
- ✅ `state.rs`（529 行）：`ReplicatedState` / `Subscription` / `StateSubscriber`（100 条待投递上限
  + 溢出保留最新 + 回调失败隔离）/ `ReplicatedStatePublisher` / `AttachedReplicatedState`
  （游标连续性校验、契约失败时释放 attachment）/ `ReplicatedStateSource` +
  `attach_replicated_state_source`

不纳入：`delta/diff.ts` 的 `diff_revisions`（durable 未直接使用）；`draft.ts` 的 `Draft<T>` 是纯类型级
映射，Rust 侧等价于 `JsonValue`（豁免）。

### P1 实现要点与差异

- 上游以异常报错 → Rust 返回 `DeltaError`；上游用 `Object.defineProperty` 绕原型链、
  拒绝 `__proto__` 等保留键 → Rust 的 `serde_json::Map` 无原型链，天然免疫，相应检查省略。
- 上游 `applyImmutable` 用 `WeakSet` 去重 → Rust 每次沿路径重建容器，语义等价。
- `Truncate` 是 JS `String.prototype.slice(n)` 语义（从第 n 个 **UTF-16 码元**起），
  不能用 `char_indices`；`overlap` 同理。两者均已用 `encode_utf16` 实现并测试覆盖。
- **tracker 的 Proxy → 显式 API**（语言机制所致）：上游 `Change::state` 是 Proxy，调用方像改
  普通对象一样改它；Rust 改为显式方法（`set`/`delete`/`append`/`truncate`/`splice`/`move_items`），
  **产生的 Op 语义与最终值不变**。上游为超大数组做的 piece-tree / 密集区域启发式（降 Op 数量）未移植，
  不影响结果；超 4096 条 Op 时按上游语义退化为整值替换。
- **state 的同步/异步监听器统一为异步**（Rust 闭包返回 `Future`）；`subscribe` 返回的取消函数
  改为 RAII（`Subscription` 的 `Drop` 即取消，保留幂等的 `unsubscribe`）。
- **`MutableReplicatedState`（`replicatedState(initial)`）暂未实现** —— 它的 `change(context,
  draft => …)` 同样依赖 Proxy 式 `Draft`；durable 只用 Attached 形态，留待有消费方时补。
- ⚠️ `pi-ai::AbortSignal::any` 的取消传播基于 `tokio::spawn`（上游是同步事件监听），
  因此 `with_abort_signal` 后的取消不是同步可见的 —— 后续阶段需注意。

**P2 durable 基础层**〔✅ 已完成 2026-10-08，2,453 行〕

| 上游 | 本 crate | 状态 |
|---|---|---|
| `src/types.ts`（1,107 行） | `src/types.rs`（1,384 行） | ✅ 数据模型 + 定义契约 + `Storage`/`Tx` |
| `src/ids.ts`（11 行） | `src/ids.rs`（46 行） | ✅ |
| `src/errors.ts`（28 行） | `src/errors.rs`（107 行） | ✅ |
| `src/truncate.ts`（205 行） | `src/truncate.rs`（307 行） | ✅ |
| `src/entries.ts`（34 行） | `src/entries.rs`（178 行） | ✅ |
| `src/documents.ts`（208 行） | `src/documents.rs`（522 行） | ✅ |
| `src/tasks.ts`（8 行） | `src/tasks.rs`（51 行） | ✅ |

`types.rs` 内容：品牌 ID/`Seq`、会话与分叉、上下文编辑、条目、任务（`TaskState`/`TaskRecord`/`TaskOutcome`）、
提交（`SubmissionRecord` 判别联合）、文档（`DocumentRecord`/`DocumentScope`/`DocDefinitionSpec`/`DocToken`）、
分页与查询、`StorageWrite`/`CommitChange`/`CommitPublication`、`TaskDefinitionSpec`/`Task`、
`TaskOptions`/`SubmissionCreate`/`DocAccess`、`Tx` trait。

**有意留到 P4/P5 的部分**：`types.ts` 里的**运行时接口**（`Session`/`Storage`/`TaskRuntime`/
`HookApi`/`ToolExecutionApi`/`Registry`/`Settings`/`HarnessOptions`/`DocumentObserver`/
`DocumentReader`/`WatchHandle`/`DocumentState`/`ContextView`/`ConversationHandle`/`SettledTask` 等）——
它们的方法签名依赖尚未存在的实现目标（session 事务线、harness 编排、工具执行设施），
现在定义只会得到一堆无实现者的空 trait，反而在 P4/P5 时因签名调整而返工。`Tx` 例外：
它的依赖（记录类型 + Query + Page）已全部就绪，且是 P3 存储的直接实现目标，因此已在 P2 定义。

### P2 实现要点与差异

- **品牌 ID**：`number & { __brand: Kind }` → 带 `PhantomData` 的 newtype；不同 `Kind` 不可互赋。
- **判别联合 → enum**：上游用 `status` 字段区分并靠 `?: never` 约束互斥字段，Rust 用 enum 载荷
  天然表达；同时保留 `status()` 判别符方便按上游语义过滤（如 `TaskQuery.status`）。
- **`Omit<>` / `Extract<>` → 显式类型**：`TaskRecord` 的「带 memos / 不带 memos」联合归为
  `memos: Option<...>`；`TableCommitChange` 单独定义并附 `StorageWrite::as_table_change()`。
- **类型守卫 → `bool`**：`Entry<D>::is()` 在上游是类型守卫，Rust 返回 `bool`，需要数据时再 `typed()`。
- **rest args → 显式参数**：`documents.ts` 的 `resolveAddress(definition, ...args)` 与重载式
  `doc()/retireDoc()` 改为显式 `owner`/`key`（及 [`DocAccess`]）；语义不变。
- **`checkpointWhen` / `migrate` 用 trait 方法**：TS 是可选的函数属性，Rust 用带默认实现的
  trait 方法（`migrate` / `checkpoint_when` / `has_migrate`）。

### P2 实现要点

- **品牌 ID**：TS 用 `number & { readonly __brand: Kind }`，Rust 用带 `PhantomData` 的 newtype
  —— 不同 `Kind` 的 ID 不可互相赋值，`Id<Kind, T>` 的 `T` 仅作类型标记（对应上游 `TaskId<Result>`）。
- **`utf8ByteLength` 豁免**：上游手写 UTF-8 计数是为了无 `Buffer` 环境；Rust 的 `str` 本就是 UTF-8，
  `len()` 即等价实现。
- **`truncateHeadOf` 已对齐**：v1.1.0 新增的「按前缀 + 总量」截断（避免全文扫描）也一并实现；
  注意本版**没有** v0.99.2 的尾部截断/`lastLinePartial` 路径（上游已简化）。

**P3 storage**〔✅ 已完成 2026-10-08〕

| 上游 | 本 crate | 状态 |
|---|---|---|
| `src/storage/scan.ts`（38 行） | `src/storage/scan.rs` | ✅ `scan_start` / `next_cursor` / `page` / `ScanStart`（6 个单元测试） |
| `Storage` 契约（`types.ts`） | `src/types.rs` | ✅ `Storage` trait（17 方法）+ `StorageError` |
| `src/storage/memory.ts`（864 行） | `src/storage/memory.rs` | ✅ 全部 17 方法 + 文档化身/增量物化（9 个集成测试） |
| `src/storage/jsonl/*.ts`（865 行） | `src/storage/jsonl/{codec,storage}.rs` | ✅ open/replay/两阶段提交/sidecar 回收（8 编解码测试 + 7 集成测试） |
| `src/storage/sqlite/storage.ts`（934 行） | `src/storage/sqlite.rs` | ✅ `SqliteStorage`（open/open_in_memory、两阶段提交、ID 归属、文档物化、close；9 个集成测试） |
| `src/storage/sqlite/migrations.ts`（125 行） | `src/storage/sqlite.rs` 的 `SCHEMA` | ⚠️ 改写为 JSON 列 schema（见下） |
| `src/storage/sqlite/{database,node}.ts`（248 行） | — | 豁免（语言机制所致，见下） |
| `src/storage/sqlite/cloudflare.ts`（139 行） | — | ⬜ 不移植（无对应运行时） |
| `src/storage/jsonl/node.ts`（14 行） | — | ⬜ 依赖 `env/node.ts`，随 P6 落地 |

**依赖顺序调整**：jsonl 后端依赖 durable 自己的 `env` 模块（`FileSystem` 等），而它原本属于 P6。
因此先抽取 `env/index.ts` 的**契约层**（`src/env/index.rs`，553 行：`FileSystem`/`Shell` trait、
`FileError`/`ExecutionError`、`BinaryReader`/`DirReader`/`FileWatcher`/`LineScan` 等）；
`env/node.ts`（Node 实现，1,226 行）仍留在 P6。测试用 `env/testing.rs`（`InMemoryFileSystem`）。

### P3 实现要点与差异（storage 三个后端）

**memory 后端**

- **索引结构**：上游用「Map + 有序 ID 数组 + 手写二分（`lowerBound`/`upperBound`/`insertSorted`）」
  为扫描加速；Rust 用 `BTreeMap`/`BTreeSet` —— 天然有序，区间迭代即扫描。语义一致，少了手写二分。
- **两阶段提交**：上游 `prepareCommit()` 先校验/冻结、再由调用方 `apply()`；Rust 在 `commit()` 内
  先只读校验（局部 `claimed` 集合）再应用，失败时**不污染任何状态**（测试 `duplicate_id_...` 验证）。
- **`checkGlobalIds` 的两类语义**已对齐：`conversation`/`entry`/`document` 是「不可变创建」，
  任何 ID 重复（含同批内）都拒绝；`task`/`submission` 是「最新记录替换」，同表可更新、跨表仍拒绝。
  错误消息文本对齐上游（`ID {id} already belongs to {table}` / `is written more than once` /
  `is written as two record types`）。
- **文档物化**：从选定点之前（含）的最新 base 开始重放 delta，跨版本边界缺 base 时报错；
  复用 P1 的 `apply_immutable_batches`。
- **分页**：三个后端共用 `storage/scan.rs` 的 `page()`（对应上游 `page`）——截断到 `limit`、
  以第 `limit` 项的 id 生成续扫游标。避免各后端各写一份走样。

**jsonl 后端**

- **两阶段写**：先写 sidecar 增量、再追加主文件 marker；`open()` 重放主文件并回收未完成的 sidecar。
- **sidecar 定位按 `(seq, ordinal)`**：`ordinal` 只在单次提交内递增，而 sidecar 文件跨提交追加，
  仅按 `ordinal` 会串文件（P3 期间修掉的真实 bug）。
- **`is_sidecar_like` 后缀剥离顺序**：先剥 `.reclaim` 再剥 `.jsonl`，否则 `.jsonl.reclaim` 识别错。
- **`encode_commit` 拒绝 `document.copy`**：上游要求先由 `resolveDocumentCopies()` 展开，
  编解码层不能编造假值。
- **legacy-v3 迁移**：按用户明确要求不复刻。

**sqlite 后端**

- **schema**：上游用结构化列（每记录类型字段各占一列）+ 125 行 migrations 以支持 SQL 索引与谓词下推；
  Rust 用 JSON 列（`records(table_name, id, seq, data)` + `documents(id, record, revisions)` + `meta`），
  过滤在内存里做。**数据往返与语义一致**，差的是查询下推能力（记录量很大时可再优化）。
- **`SqliteDatabase`/`SqliteExecutor` 抽象豁免**（语言机制所致）：上游为跨运行时
  （Node `node:sqlite` / Cloudflare Durable Object）引入异步 facade，附带事务队列、
  admitted-reads drain、结算控制与「事务句柄过期」检查。Rust 直接持有 `Arc<Mutex<Connection>>`：
  Mutex 已串行化全部操作，事务在 `connection.transaction()` 内原子完成，
  上述异步队列语义由同步原语等价覆盖，无需移植这一层。
- **`open` 签名**：上游 `open(db: SqliteDatabase)` 接收 facade；Rust 提供 `open(path)` 与
  `open_in_memory()`，直接从路径建连并建表（无 migrations 目录）。
- **`close()`**：上游返回幂等的 `closing` promise，并等待已受理的多查询读排空；
  Rust 置 `closed` 标记后立即返回 `Ok`（读在 Mutex 内同步完成，不存在跨 `await` 的已受理读），
  重复调用同样成功。关闭后一切操作返回 `StorageError::Closed`（对应上游 `SqliteStorage is closed`）。
- **`mint_id`**：`Storage::mint_id` 的签名无法返回错误，关闭后以上游 `assertOpen` 抛异常为对等语义
  改为 panic；同时不再吞 DB 错误（原 `.unwrap_or(2)` 已移除）。
- **`cloudflare.ts`（139 行）不移植**：无对应运行时。

**P4 session 层**〔✅ 已完成 2026-10-08〕

| 上游 | 本 crate | 状态 |
|---|---|---|
| `src/session/forks.ts`（97 行） | `src/session/forks.rs` | ✅ `prepare_fork_document_copies` |
| `src/session/observation.ts`（301 行） | `src/session/observation.rs` | ✅ `CommittedStateSource` / `CommittedWatch` |
| `src/session/transaction.ts`（1,042 行） | `src/session/transaction.rs` | ✅ 表读写 / 文档 / 结算 / 采纳（编译通过，集成测试待补） |
| `src/session/session.ts`（569 行） | `src/session/session.rs` | ✅ `SessionImpl` + 提交线 + `Session` 契约（14 个集成测试） |

**P4 前置结构修正**：上游把 `history` / `fork` 放在 `DocumentRecord` / `DocumentCreate` **顶层**
（只有会话文档声明），`scope` 只含 `{kind, conversationId}`；P2 曾把它们并进 `DocumentScope`。
本阶段已拆开：新增 `DocumentSemantics`（定义侧），`DocumentScope` 去掉这两项，
`DocDefinitionSpec::scope()` → `semantics()`；`check_record_scope` 改为比较记录顶层字段。
另外补上 `AnyDocToken`（单例/家族令牌的共同视图）与 `Draft`（事务内共享草稿）。

### P4 实现要点与差异

- **继承 → 钩子 trait**：上游 `HarnessImpl extends SessionImpl` 并覆盖 `conversationCreated` /
  `beforeClose`；Rust 用 `SessionHooks`（默认 `DefaultSessionHooks` 为空实现）。
- **Promise 链 → 公平异步互斥**：上游用 `#tail` Promise 链串行提交；Rust 用 `tokio::sync::Mutex`
  （FIFO 公平锁）—— 等待顺序等于调用顺序，且 job 失败不影响后续 job。并发读-改-写测试
  （`mutation_line_serialises_concurrent_commits`）验证了这一等价性。
- **可重复 await 的 `close()`**：上游 `#closing` 是 Promise；Rust 用 `Shared<BoxFuture>`，
  多次 `close` 共享同一结果；调用方取消只中断「等待」，不阻止清理（`withoutAbortSignal`）。
- **取消函数 → RAII**：`subscribeCommits` / `subscribeClose` 返回 `SessionSubscription`。
- **重载 + rest args → 显式参数**：上游 `resolveAddress(definition, ...args)` 用参数个数区分
  `conversationId` / `key` / `context`；Rust 显式接收 `owner` / `key` / `context`。
- **泛型方法 vs dyn trait**：`commit` / `commitWith` / `readOnLine` / `conversationDocumentOnLine`
  是泛型方法，保留为 `SessionImpl` 的固有方法；`Session` trait 覆盖其余（上游 `DocumentReader`）。
  闭包签名为 `for<'a> FnOnce(&'a Transaction) -> BoxFuture<'a, _>` —— Rust 无法在 HRTB 下返回
  `impl Future`，因此调用方写 `Box::pin(async move { … })`。
- **`snapshotAsOf` 的 entry 查询**：上游用 `Storage.entry(conversationId, id, context)` 重载
  → Rust 的 `entry_in_conversation`（带会话可见性），不是全局 `entry`。
- `DOMException("AbortError")` → `SessionError::Aborted`；`new Error(message, { cause })` →
  `SessionError::MessageWithCause`（用于 `#poison` 的包装错误）。

**P4 的收尾修掉了两个 P3 后端缺陷**（由新测试发现，不是新增范围）：memory 与 sqlite 的
`document(id, At(seq))` 在重放增量时**没有按选定点过滤修订**，历史读取会返回最新值；两者的
`currentOnly` 历史读取检查也缺失。现在三后端一致：按点物化只重放选定点之前（含）的修订，
非当前点读取 `currentOnly` 记录报错。sqlite 的 `revisions` JSON 列随之改为随附每条修订的提交序号。

**P4 遗留**：无。P5 的 harness 将注入 `SessionHooks::conversation_created`，并在 `SessionImpl`
之上构建 task-graph。

- **`Draft<T>` 的共享**：上游 Proxy 让调用方直接改写草稿，事务随后 `prepare()` 它；Rust 用
  `Arc<Mutex<Option<Change>>>` 让事务与调用方共享同一份变更，事务在 `settle_success()` 时取出并
  `prepare`，`adopt()` 时按指针替换（`prepared` 所有权直接交给 tracker）。
- **`queueMicrotask` → `tokio::spawn`**（无运行时则同步执行）：观察帧的投递仍是异步的，
  但不再依赖 JS 微任务队列。
- **`Promise` → `oneshot` + `futures::future::Shared`**：`WatchHandle::closed` 可多次等待。
- **`Prepared` 的放弃 = drop**：Rust 的 `Prepared` 只是已计算的操作批次（无副作用），
  上游的 `prepared.abort()` 在 Rust 侧为丢弃。
- **未完成操作的跟踪不适用**：上游用 `Set<Promise>` 在 `settleSuccess` 拒绝「回调先于挂起操作结束」；
  Rust 的 future 是惰性的，调用方必须 `await` 才能取得结果，因此这一保护无对应物。
- **`#write()` 的同步标记**：上游在**调用时**标记 `hasTableWrite`（驱动 `ReadAfterWrite`）。
  Rust 的 `async fn` 体在首次 poll 时执行；实践中调用方立即 `await`，时序差异仅在
  「先构造多个 future 再 await」时可见。

**P5 harness 核心**〔🚧 进行中，最大一块〕

依赖自底向上分阶段；每阶段以全量 `cargo test` + clippy 绿灯为完成条件。

- **P5a 基础工具**〔✅ 已完成 2026-10-08〕：`harness/{json,output,util}.rs`
  - `json.rs`（上游 31 行）：`assignJson` 逐叶写入
  - `output.rs`（341 行）：`sanitizeOutput`、`boundOutput`、`characterEnd`、`OutputBuffer`
  - `util.rs`（58 行）：`scanAll`、`closedError`
  - **对照验证**：`tools.d/parity/output-parity.mjs` 用 Node 直接加载上游 `output.ts`，
    生成 234 个用例（`output-cases.json` / `output-expected.json`）；
    `crates/pi-durable/tests/output_parity.rs` 重放并与上游结果逐项比对（含抛错用例）。
- **P5b 类型契约**〔✅ 已完成 2026-10-08〕：`harness/types.rs`（上游 679 行）+ `harness/define.rs`（44 行）
  - 已落地：数据模型（`ModelRef` / `FieldChange` / `SubmissionDraft` / `ToolExecutionResult` /
    `AgentState` / `AgentChange` / `Agent` / 三类策略与 `Settings` / `ContextView` /
    `TaskInspection` / `HarnessInspection`）、扩展契约（`ToolRegistration` / `PromptSection` /
    `Extension` / `Wrap` / `HookRegistration` / `RegistrySnapshot` + 三个内置任务 Hook / `HookApi` /
    `ToolExecutionApi` / `ConversationHandle` / `Submission`），以及 `DocumentReader` /
    `DocumentObserver`（上游定义在 durable `types.ts`，现已抽到 `session` 并由 `SessionImpl` 实现）
  - **有意推迟到 P5h**：`Harness` / `Conversation` / `ConversationWatch` / `Registry` /
    `RegistryReader` / `HarnessOptions` / `ConversationInit` / `ConversationCreateOptions` ——
    它们的方法面直接建立在 scheduler / 内置文档 / 组装之上，现在定形会随实现返工
  - 要点：`undefined`/`null`/值 三态 → `FieldChange`；`Partial<Hooks>` → 带默认实现的 trait；
    泛型（`T extends JsonValue` / `TaskId<R>`）→ 擦除为 `JsonValue` / `TaskId`；
    `ToolExecutionApi::commit` → boxed 闭包 + HRTB
  - **上游默认值逐一对齐**（我在初稿里编造过，已改）：`DEFAULT_RETRY_POLICY`
    （2000ms / 60000ms）、`DEFAULT_COMPACTION_POLICY`（16384 / 20000 / 32768）、
    `DEFAULT_PROGRESS_POLICY`（100 / 100）、`DEFAULT_CONTEXT_RETENTION_MS`（600000）、
    `steeringMode` / `followUpMode` 默认 `one-at-a-time`；已有测试锁定这些数值
- **P5c 内置文档**〔✅ 已完成 2026-10-08〕：`harness/{agent,provider,usage,inbox,live}.rs`
  - `agent.rs`（上游 266 行）：`AgentDoc`、`resolveSettings`、`configure` / `addTools` /
    `applyChange`、`createAgent`、`agentHooks`、`resolveAgent` / `selectExtensions` / `applyWrap`
  - `provider.rs`（39）：`ProviderDoc`
  - `usage.rs`（72）：`UsageDoc`、`recordUsage`、`addUsage` / `addUsageState`
  - `inbox.rs`（132）：`InboxDoc`、`prepareBoundary` / `applyBoundary` / `isStale` /
    `removeInboxItem` / `withdrawQueuedInputs`
  - `live.rs`（175）：`LiveDoc`、`endRun`、压缩状态增删查、`toolSlot` / `finishSlot` / `clearProgress`
  - **有意推迟**：`ensureProviderSessionId`（需 `TaskRuntime`）、`settleSchedulerOutcome`
    （需 `SchedulerOutcome` + `convertPartial`）、`output.ts` 的 `Progress`
  - 要点：上游 Proxy 草稿的数组操作 → `Draft::splice` + 路径写入；
    `Object.assign` 逐键覆盖 → 逐键 `set`；`copyJson(..., {omitUndefinedProperties})` →
    序列化后剔除 `null` 成员
  - **验证**：`tests/harness_documents.rs` 的 12 个端到端测试（真实 Session + Transaction 提交），
    覆盖写入形状、累加、条目放置与提交结算；**P5c 没有 Node 对照**——上游这些模块依赖
    `@earendil-works/chord` 的 Proxy 草稿，仓库无 `node_modules`，无法加载。
- **P5d 视图与上下文**〔下一步〕：`view`（244）/ `task-graph`（222）/ `context`（293）
- **P5d 视图与上下文**〔✅ 已完成 2026-10-08〕：`harness/{context,view}.rs`
  - `context.rs`（上游 293 行）：`ContextBounds` / `captureContextBounds`、`activeEntries`、
    `readContext` / `readContextFrom`（区间复用）、`deriveContext`、`orderToolResults`（含缺失结果合成）、
    `leadWithSystem`、`settle`
  - `view.rs`（244）：`ConversationView` / `ConversationViews` 挂载、`ViewObserver`、
    `advance`（条目与文档操作派生）、`prefixed`
  - 要点：挂载值以 `JsonValue` 保存（上游直接交给 chord），`ConversationView` 是它的反序列化视图；
    `freezeJson` 豁免（Rust 值 owned）；`WeakMap<object, …>` 的 `conversationViews` 推迟到 P5h
  - **有意推迟**：`task-graph.ts`（已排入 P5d 但未做——需要 `JoinPolicy`/`TaskOutcome` 的完整面，
    放在 P5f 调度阶段一并落地）
  - **验证**：`tests/harness_views.rs` 的 9 个端到端测试（真实提交 → 视图帧投递）
- **P5e 事件流**〔✅ 已完成 2026-10-08〕：`harness/events.rs`（上游 425 行）
  - 类型：`MessageChange` / `SnapshotEvent` / `AgentEvent`（24 种）/ `AgentEventStream` / `ToolOutputUpdate`
  - 纯逻辑：`parts` / `snapshot_of` / `queued` / `result_of` / **`translate`**（一次发布引发的全部事件，
    按 spec §9.4 顺序）/ `message_changes`（视图操作 → 消息改动）/ `tool_update`
  - **有意推迟**：`watchEvents()`（需要 `conversationViews(harness)`，即 Harness 对象）→ P5h
  - 要点：`AgentEvent` / `MessageChange` 用内部标签 + `rename_all_fields = "camelCase"`；
    `SnapshotEvent` 不带 `type` 字段（由 enum 标签提供同一 JSON 形状）；
    `op[1] as Path` 之类的元组索引 → `Op` 模式匹配
  - **顺带修掉一个 pi-ai 缺陷**：`ContentBlock`（internally tagged）序列化正常但**无法反序列化**——
    外层的 `type` 标签会消费内层 `TextContent.kind` 字段（`#[serde(rename = "type")]`）而后者是必填，
    导致任何含内容块的 JSON 读不回来。已给四个内层 `kind` 字段加 `#[serde(default)]` +
    `Default`，并给 pi-ai 补了往返测试。这是 `events.rs` 的 `message_changes` 必需的（它要读回内容块）

- **P5f-1 任务图与等待**〔✅ 已完成 2026-10-08〕：`harness/task_graph.rs` + `util.rs` 的 `Waiters`
  - `task_graph.rs`（上游 222 行）：`TaskGraphView` 挂载（由第一个观察者在 Session 线上建立、
    随最后一个丢弃）、`advance`（从提交出版物增减节点、追加被拥有的会话）、
    `TaskGraph` / `TaskGraphNode` / `TaskGraphState`
  - `util.rs` 的 `Waiters`（58 行）：按 key 挂起等待，由 `resolve` / `reject_all` / 取消结清
  - 复用 `view::ViewObserver` 与 `view::ReleaseSlot`（观察者形状相同），不再另立一份
  - **验证**：`tests/harness_task_graph.rs` 的 5 个端到端测试 + 11 个单元测试
- **P5f-2 提交与运行时契约**〔✅ 已完成 2026-10-08〕
  - `harness/submissions.rs`（上游 207 行）：`Submissions`（接纳/查询/等待/撤回）+ `admit_submission`
    （spec §6 的接纳规则：request ID 复用、繁忙排队、`whenBusy: reject`、空闲直接放置或追加、
    stale 写入）+ `SubmissionHandle`
  - **运行时契约**（scheduler 的前提）：`RunningTask` / `NextTaskState` / `TaskRuntime`（大 trait，
    方法泛型擦除）/ `HookRunner`；`TaskDefinitionSpec` 扩展出 `run_phase` / `abort` / `migrate` / `hooks`
  - **补两个 P4 遗漏**：`Transaction::submission(id)`（上游 `Transaction` 类有、而 `Tx` 接口没有，
    所以 P4 做 session 时漏了）；`Waiters::add` 改为拥有 `Arc` 的 `'static` future（否则无法跨出 Session 线）
  - **`startRun` 注入**：上游直接 import generation.ts；Rust 侧作为 [`StartRun`] 注入，
    因此 submissions 可在 generation 之前独立落地，也不存在模块环
  - **验证**：`tests/harness_submissions.rs` 的 12 个端到端测试
- **P5f-3a 调度器的所有权与派生逻辑**〔✅ 已完成 2026-10-08〕：`harness/scheduler/ownership.rs`
  - 任务节点与所有权树遍历：`TaskNode` / `Up` / `Step` / `Overlay` / `above` / `chain_known` /
    `owned_live` / `in_scope` / `below_cancelled` / `idle` / `live_records`
  - 记录派生：`parent_of` / `node_of` / `with_state`（结果落定时丢弃 memos）/ `failed_outcome` /
    `cancellation_intent` / `can_reserve` / `memo_of` / `overlay_of` / `json_equal`
  - 这些在上游是 `TaskScheduler` 的私有方法与模块级函数，全为纯逻辑；Rust 侧用 [`OwnerLookup`]
    把遍历与状态机解耦，因此可以只用构造的记录做单测（**17 个测试**）
  - **有意推迟**：`scheduler.ts` 的状态机主体（调用生命周期、reconcile、上下文档存与过期、
    `TaskRuntime` 实现）→ 见 P5f-3b
  - **P5a 补充**〔✅ 2026-10-08〕：`output.ts` 的 `Progress`（自适应进度提交节流）已落地
    —— 空闲后首次改动立即提交、之后按 `minIntervalMs` 与写入量（100 KiB/s）推迟、
    至多一次在途、`markAndWait` / `stop` 的等待者语义
- **P5g 提示与任务**〔🚧 进行中〕
  - **P5g-1 系统提示**〔✅ 已完成 2026-10-08〕：`harness/prompt.rs`（上游 158 行）
    - `replay_sections`（系统消息的章节重放）、`render_sections`（按序渲染，抛错时保留已显示文本）、
      `plan_system_entries`（baseline / 最小补丁 / 重排三条路径）、`plan_tools` / `plan_sections`
    - 要点：章节顺序是语义的一部分，因此用 `IndexMap` 而非 `BTreeMap`（`BTreeMap` 会按键排序，
      使「重排」分支永远不可达——这是实现时被测试暴露的）
    - **验证**：12 个单元测试
  - **P5g-2 内置 Task**〔🚧 进行中〕
    - **`generation.ts` 的运行控制**〔✅ 已完成 2026-10-08〕：`harness/generation.rs` 的
      `make_start_run`（对应上游 `startRun`，第 664 行）、`create_generation`（第 674 行）、
      `hand_over`（第 679 行）
      - `startRun` 做成接受 generation 任务定义的工厂：上游直接引用模块级常量 `GenerationTask`，
        Rust 侧该定义尚未落地，工厂让运行控制可以先行接线（即 `Submissions` 的 `StartRun` 注入点）
      - `createGeneration` 用 `ownership: { kind: "conversation" }` + `conversationId` 建任务；
        `handOver` 只在 `live.run.taskId === from` 时改写 `taskId`，`inputs` 随同一子树移动
      - `thresholdCompaction`（第 306 行）：纯逻辑的阈值判定，只有 `selectCut` 找得到切点时才返回
        `blocking` / `background`；Rust 用 `i128` 做差，以免上下文窗口小于保留量时 `u64` 下溢
      - **验证**：4 个单元测试（启动即建 generation 并写 `pi.live.run`，且任务确实持久化为会话所有；
        `handOver` 在 `from` 不匹配时不动、匹配时只换 `taskId` 而保留 `inputs`；
        阈值三档、无切点/策略关闭/窗口未知/后台阈值禁用）
    - **`compaction.ts` 的选段与摘要文本**〔✅ 已完成 2026-10-08〕：`harness/compaction.rs`
      - 常量（`TOOL_RESULT_MAX_CHARS` / `SUMMARY_PREFIX` / `SUMMARY_SUFFIX` /
        `SUMMARIZATION_SYSTEM_PROMPT` / `SUMMARIZATION_PROMPT`）、类型 `CompactionInput` /
        `SummaryRequest` / `CompactionCheckpoint`，以及 `make_create_compaction`（上游 `createCompaction`）、
        `select_cut` / `summarized_messages` / `estimate_context` / `serialize_conversation`
      - 差异（均为语言机制所致）：`contentText` 的 string / 块数组两种入参拆成两个函数；
        `estimateContext` 用值相等代替 TS 的对象身份比较；`serializeConversation` 的工具调用参数按字典序
        （serde_json 默认有序 map，见 `harness/json.rs`）；`truncate` 按 UTF-16 码元切分以匹配 `String.slice`
      - `summary_text` / `summary_failure` / `summary_prompt` 暂无调用者（待 `CompactionTask` 本体）
      - **验证**：15 个单元测试
- **P5g-2c CompactionTask 本体**〔✅ 已完成 2026-10-09〕：`harness/compaction.rs`
  - `CompactionTaskDefinition` + `make_compaction_task(start_run)` 工厂（`admit_submission` 需要
    `StartRun`，会话自有压缩的摘要通过写入提交放置）
  - `select` / `summarize` / `retry` 三个 phase + `abort`；`place_summary` / `complete` /
    `fail_no_model` / `place` 辅助
  - 配套：`harness/provider.rs` 的 `ensure_provider_session_id`；`harness/types.rs` 的
    `RuntimeHookApi`（把 `Arc<dyn TaskRuntime>` 委托桥接成 `HookApi`，Rust 无 trait object upcast）、
    `CompactionResult` 补 `Serialize`/`Deserialize`（`skip_serializing_if`）；`stream_options` 把
    `ConversationStreamOptions` 映到 pi-ai `SimpleStreamOptions`
  - **验证**：17 个单元测试（含 `compaction_task_metadata_matches_upstream`、
    `stream_options_maps_conversation_settings`）
- **P5g-2t ToolTask 本体**〔✅ 已完成 2026-10-09〕：`harness/tool.rs`
  - `ToolTaskDefinition` + `make_tool_task()`（无状态，不需宿主依赖）；`call` / `execute` 两个
    phase + `abort`；`run` / `publish_progress`（[`Progress`] 节流）/ `final_result` / `settle`；
    `ToolApi` 实现 [`ToolExecutionApi`]（委托 [`TaskRuntime`]，`output`/`diagnostic`/`details` 写入
    上报槽位）
  - 辅助：`read_call` / `prepare` / `validate` / `invalid` / `harness_error` / `tool_diagnostic` /
    `truncated` / `append_tool_result` / `render_diagnostics` / `bound_content` / `from_slot`
  - 配套：`pi-ai` `ToolResultMessage` 补 `duration_ms`；`harness/types.rs` `ToolControl` / `Replay`
    补 `Serialize`（`skip_serializing_if` 对齐上游 `copyJson` 去 `undefined`）；`ToolExecutionApi::commit`
    的 `CommitOperation<'static>`
  - **验证**：4 个单元测试（`tool_task_metadata_matches_upstream`、
    `tool_task_checkpoint_round_trips`、`harness_error_carries_an_error_diagnostic`、
    `render_diagnostics_wraps_in_harness_tags`）
- **P5g-2g GenerationTask 本体**〔✅ 已完成 2026-10-09〕：`harness/generation.rs`
  - `GenerationTaskDefinition` + `make_generation_task(start_run, create_compaction, tool_task,
    generation_task)` 工厂（`generation_task` 用惰性工厂打破与 `start_run` 的循环依赖）
  - `prepare` / `request` / `retry` / `poll` / `tools` 五个 phase + `abort`（取消 deferred、未启动调用
    `aborted`、`convertPartial` + `endRun`）；`classify`（deferred→poll、toolUse→startToolRound、
    stop/length→answer、overflow→compaction、retry→model_error）；`answer` / `start_tool_round` /
    `finish_tool_round` / `stream_response`（部分消息节流）
  - 配套：`make_start_run` 改为接收惰性工厂 `Arc<dyn Fn() -> Arc<Task>>`；`harness/prompt.rs`
    `render_sections` 的 `report` 收紧为 `&(dyn Fn(SessionError) + Send + Sync)`；
    `harness/tool.rs` 的 `append_tool_result` 改为 `pub`；`harness/compaction.rs` 的 `stream_options` /
    `thinking_level` 改为 `pub`
  - **验证**：5 个单元测试（含 `generation_task_metadata_matches_upstream`）
- **P5g-r Registry 写面 + BUILTIN_TASKS**〔✅ 已完成 2026-10-09〕：`harness/registry.rs`
  - `Registry` / `RegistryImpl`：`install`（校验 + 替换同名扩展）/ `uninstall` / `snapshot` /
    `subscribe`；`section_key_valid`（`^[a-z][a-z0-9_-]*$`）、`validate_extension`
    （工具名/章节键唯一、键合法、`INSTRUCTIONS_KEY` 保留）
  - `create_registry()`：用 `OnceLock` 组装三任务（generation → start_run/compaction → tool），
    `builtin = [generation, tool, compaction]`
  - 差异：`install`/`#publish` 的 throw → panic；`subscribe` 返回取消闭包
  - **验证**：3 个单元测试（`builtin_tasks_are_installed_in_the_snapshot`、
    `section_key_validation_matches_upstream`、`validate_extension_rejects_reserved_instructions_key`）

- **P5f-3b-1 调度器的状态表与提交监听**〔✅ 已完成 2026-10-08〕：`harness/scheduler/state.rs`
  - `SchedulerState`：对应 `#live` / `#edges` / `#conversationOwners` / `#settled` /
    `#failedMigrations` / `#failFastChecks` / `#contexts` 与七个标志（`#reconcileScheduled` /
    `#cascadePending` / `#enabled` / `#closing` / `#dirty` / `#draining`），并实现 `OwnerLookup`
    —— 于是 P5f-3a 的全部遍历函数可以直接作用在真实状态上
  - `observe`：对应 `#observe(publication)` 的全部逻辑（终态记录离开 `#live` 并进入 `#settled`、
    新 abort 标记的级联与运行中止信号、`failFast` 等待者登记、会话边只登记一次、
    已取消拥有者之下的排队输入与新建工作、以及 `#scheduleReconcile` / `#settleIdle` / `#kick` 的触发）。
    上游在这里直接做副作用；Rust 返回 `ObserveOutcome`（状态表改动已应用），由执行层按序执行
    —— 之所以可以这么拆：这些副作用幂等（`#reconcileScheduled` / `#dirty` 都是标志），
    唯一有序的终态记录与等待者结算按出现顺序排在 `settled` 里
  - **验证**：14 个单元测试（含 6 个针对 `observe`）
- **P5f-3b-2a 调度器的预留决策**〔✅ 已完成 2026-10-08〕：`harness/scheduler/reserve.rs`
  - `waiting_on`（`#waitingOn`）：abort 标记的任务等它的活动普通自有工作，否则等 `on` 里仍然活动的部分
  - `fit` / `resolve`（`#fit` / `#resolve`）：定义能否接手（`missing_task` / `task_too_old` /
    `migration_failed`），以及旧版本的迁移（版本与 input 连同 checkpoint 一起换新，状态变体保留）
  - `inspect_task`（`#inspectTask`）：running → completing → waiting → blocked / ready 的优先级
  - 差异：`#resolve` 的失败上报变成 `ResolutionOutcome::report`；`migrate` 抛错 → Rust 用
    `Option::None` 表达，不产生上游那条异常文案；`#fit` 的定义比较用 `Arc::ptr_eq`；
    `#inspectTask` 多收一个 `has_invocation`（`#invocations` 属执行层）
  - **验证**：10 个单元测试
- **P5f-3b-2b scheduler 执行层**〔✅ 已完成 2026-10-09〕：`harness/scheduler/exec.rs`
  - 生命周期：`open`（订阅提交/关闭/注册表 + 扫入 live、`running`→`pending`）、`resume` / `join` /
    `abort` / `wait_for_task` / `wait_for_idle` / `abort_conversation` / `seal`
  - 调度推进：`reconcile` / `any_failed` / `finalize` / `load_scopes` / `load_chain` / `drain` /
    `reserve`（I/O 部分）/ `inspect` / `observe`（执行 [`ObserveOutcome`]）
  - 任务调用：`create_invocation` / `run` / `decide` / `run_abort` / `step` / `terminate` /
    `commit_state` / `validate_wait` / `end`
  - 调用运行时：`runtime`（实现 [`TaskRuntime`]）/ `read` / `gated` / `sleep` / `watch_doc`；
    `InvocationHooks` 实现 [`HookRunner`]（同步读 phase 缓存的 agent）
  - 差异：`Promise.withResolvers` → `oneshot`+`Shared`；`setTimeout`/`queueMicrotask` →
    `tokio::time::sleep`/`tokio::spawn`；`#gated` 的 `change` 统一为
    `FnOnce(&Transaction, RunningTask) -> BoxFuture`（`current` 按值 move，见 [`TaskCommit`]）；
    `now`/`report` 结束后调用改为 panic（对等语义）；`#scheduleExpiry` 的「未变化」判断补
    `current_expiry.is_some()`（上游 `this.#expiry !== undefined`）；`Waiters::add` 改为
    **入队时立即注册**（上游 `Promise` 构造器语义，修复了 `add` 后、`await` 前 `resolve` 丢等待者的偏差）
  - **验证**：`cargo check --workspace --all-targets` 无错误；`cargo test --workspace` 全绿
    （280 个 lib 测试 + 各集成测试）
- **P5h 组装**〔✅ 已完成 2026-10-09〕：`harness/harness.rs` + `watchEvents`
  - `Harness` / `Conversation` / `ConversationWatch` / `HarnessOptions` / `ConversationInit` /
    `ConversationCreateOptions` 契约（`harness/types.rs`）
  - `HarnessImpl`（持 `Arc<SessionImpl>` + scheduler + submissions + task-graph + views）与
    `ConversationImpl` / `boundConversation` / `BoundSubmission`；`open()` 工厂（校验内置任务）
  - `HarnessHooks` 实现 `SessionHooks::conversation_created`（内置 `pi.*` 文档 + `create_agent` +
    选项钩子）与 `before_close`（`tasks.join()`）；`settle_scheduler_outcome`（live.rs）与
    `convert_partial`（generation.rs）落地
  - `events.rs` 的 `watchEvents`（`AgentEventsObserver` 同时实现 `ViewObserver` + `AgentEventStream`）
  - 差异：`WeakMap<object,…>` 的 `conversationViews` 改为 Harness 直接持有 [`ConversationViews`]；
    scheduler/submissions/harness 的循环依赖用 `OnceLock` 打破；`attach` 的 `create` 改为 async
  - **验证**：`cargo check --workspace --all-targets` 无错误无警告；`cargo test --workspace` 全绿
    （290 个 lib 测试 + 各集成测试）
  - ✅ 重建 `demo-agent`（`crates/demo-agent`）：REPL + `create_registry` + `CODING_TOOLS` +
    `NodeExecutionEnv` + `MemoryStorage`，用 `root` / `submit` / `wait` / `context` 接线

**P5a 实现要点与差异**

- **UTF-8 字节语义**：上游用 `TextEncoder`/`TextDecoder` 精确切片；Rust 的 `str` 已是 UTF-8，
  直接按字节切片 + `String::from_utf8_lossy`。`OutputBuffer` 需要 `TextDecoder(stream: true)` 的
  **增量解码**（跨块的半个字符要留到下一块），Rust 侧自实现 `Utf8Stream`（保留不完整尾部序列）。
- **`utf8ByteLength` → `str::len()`**（上游手写计数是为了无 `Buffer` 环境）。
- **`Error` → panic**：`push` 在 `head` 保留下遇到 `skipped` 时上游 `throw`，Rust 用 `panic!`
  （与 `Storage::mint_id` 的处理一致）。
- **`Progress` 留到 P5g**：它的 `write: () => Promise<number>` 与 harness 的提交路径耦合，
  待 generation 落地时一并实现。

**P6 tools + env**〔✅ 已完成 2026-10-09〕
- ✅ `tools/`（10 文件 / 1,313 行）全部落地：`bash` / `read` / `write` / `edit` / `edit-diff` /
  `image` / `path-utils` / `file-mutation-queue` / `env`（`crates/pi-durable/src/tools/`），
  建立在已落地的 [`ExecutionEnv`] + [`ToolRegistration`] 契约上；`edit-diff` 用 `similar`
  实现 diff/patch，模糊匹配用 `unicode-normalization` NFKC + 字符归一化
- ✅ `env/`（5 文件 / 1,923 行）全部落地：`decode`（流式 UTF-8 解码）、`line-scan`（增量行扫描）,
  `node`（真实文件系统 + Shell，`NodeExecutionEnv` 实现 [`FileSystem`] + [`Shell`]），
  `node-watch`（基于 notify 的文件监视：快照 diff + native 事件去抖 + 轮询 fallback +
  不可靠文件系统检测，1:1 完整复刻）
- 差异：Windows 特有分支（Git Bash / WSL / `taskkill.exe`）不在范围；`Result` 的 `ok`/`err`
  枚举 → Rust `Result`

**P7 agent-core 跟进 + ai 增量**〔✅ 已完成 2026-10-09〕
- ✅ agent-core：`durationMs`（`ExecutedToolCallOutcome` / `FinalizedToolCallOutcome` +
  `execute_prepared_tool_call` 计时）与 `streamProxy` 返回 `AssistantMessageEventStream` 已对齐
- ✅ ai（声明范围内核心）：
  - `AssistantMessage.durationMs?` / `ToolResultMessage.durationMs?`（`types.rs`）
  - `SamplingParams` / `SamplingParamsByThinkingLevel` 类型 + `Model.samplingParamsByThinkingLevel`
  - `resolveSamplingParams`（`simple-options.rs`：模型默认 + 分层覆盖 + 请求参数合并）
  - `AssistantMessageEventStream` 计时（`event-stream.rs`：从 type alias 改为带单调钟计时的 struct）
  - 声明范围外（其他 provider / classifier / `refreshStoredOAuthCredential` 重构）未跟进

**P8 testing / conformance**〔✅ 已完成 2026-10-09〕
- ✅ `storage-conformance` 核心落地：`src/testing/storage_conformance.rs`（7 个核心 case：ID 保留、
  原子提交回滚、detach、索引顺序、扫描分页、任务替换、close 拒绝），`tests/storage_conformance.rs`
  对 memory / jsonl / sqlite 三个后端参数化运行
- 顺带修复三个真实 bug：jsonl `commit` 先写盘后校验导致 Rejected 后 poison → 改为预校验；
  memory 读方法未检查 closed → 加 `assert_open`；jsonl `delegated` 把 `Closed` 折成 `Message` → 保留
- ✅ `env-conformance` 核心落地：`src/testing/env_conformance.rs`（11 个核心 case：binary reader
  字节范围/行扫描/rename、directory reader 分页、argv exec 传参/流式/ cwd/spawn 错误/timeout），
  `tests/env_conformance.rs` 对 `NodeExecutionEnv` 运行
- ✅ `storage-benchmark`（489 行性能基准）落地：`src/testing/storage_benchmark.rs`（scale 常量、
  `seedStorageBenchmark` / `seedStorageWriteBenchmark`、`STORAGE_READ_BENCHMARKS` /
  `STORAGE_WRITE_BENCHMARKS`），`tests/storage_benchmark.rs` 对 memory / jsonl / sqlite 三后端
  参数化验证读/写基准的 expected
- 顺带修复第四个真实 bug（benchmark 的 fork 场景暴露）：三后端的 `scan_entries` /
  `entry_in_conversation` / `find_latest_head_marker` 未实现 fork 链可见性（分叉会话看不到祖先条目、
  查不到 head marker）→ memory 补 `visible_entry_ids`（`/ascending`），sqlite 补 `load_fork_segments`
  （jsonl 委托 memory 自动修复）；sqlite 还修了 `u64::MAX as i64` 溢出（无上限时上界折成 -1）

**P9 收尾**〔✅ 已完成 2026-10-09〕
- 全量对比扫描、文档、验证

**P10 双向审计 + 修复**〔✅ 已完成 2026-10-09〕

P9 之后做了一次双向（TS→Rust 缺失 + Rust→TS 多余）逐文件逐方法审计，并修复全部发现。
结论见 `tools.d/parity/AUDIT-REPORT.md`，逐项修复状态见 `todos.md`：

- **agent → pi-agent-core（9 缺失 + 5 多余）**：proxy 四缺陷（`AbortSignal` / 干净 EOF /
  `providerThinkingLevel` / 非 2xx 错误体）、`onPayload`/`onResponse`/`onProviderStreamEvent`
  三钩子链路（穿透到 pi_ai 的 `ProviderRequestOptions`/`StreamOptions` + openai-responses/
  openai-completions 调用点）、`steeringMode`/`followUpMode` 运行期访问器、`subscribe` 退订、
  `prompt` images 重载、`handleRunFailure` 模型信息；清理 `proxy_stream_fn`/
  `get_default_stream_fn` 再导出/`FinalizedToolCallOutcome` 公开/`ShouldStopAfterTurnContext`/
  `set_system_prompt` 5 处多余。
- **ai → pi-ai（3 缺项）**：`Model.promptCache`、`Model.inputLimits`、strict-schema 回调。
- **chord → pi-durable/src/chord（4 缺失）**：`MutableReplicatedState` + `replicated_state` 工厂、
  `ReplicatedStateReplica`、`state-internals` 注册表（Weak 弱引用）、`state_codec`
  （`ServiceStateEncoder`/`Decoder`，含 delta 的 `Encoder`/`Decoder` path-interning）。

新增工具：`tools.d/.parity/reverse.py`（反向多余扫描）、`audit_brief.py`（逐模块审计简报）；
修正 `scan.py` 的 `declared_scope`（durable 曾被误判为 6 文件子集）。
验证：`cargo test --workspace` 470 passed / 0 failed，`cargo fmt` 无差异。

**P11 第二轮重审 + 三轮修复**〔✅ 已完成 2026-10-10〕

P10 之后，重新用 11 个独立审计 agent 对全部模块做了第二轮逐方法重审（`tools.d/parity/recheck/`
下 a1~a10、a4a、a4b 各 `report.md`），发现 P10 结论偏乐观——报告总差异约 360 项（P10 只覆盖
符号级差异，漏掉了大量方法级/边界级差异）。随后按「运行逻辑优先」做了三轮修复，共约 38 项，
结论见 `tools.d/parity/recheck/FINAL-REPORT.md` 与 `FIXES.md`。

- **第一轮（P0/P1/P2，29 项）**：document.copy 三后端支持 + commit 原子性/可见性、
  generation.answer() 边界启动、headers 优先级、session/env 六处、chord 相同值抑制、agent 五处。
- **第二轮（5 处）**：llm_context 不再传 tools、工具事件流时序/载荷、compaction 剥离 deferred、
  tool.rs bound_content + env 错误路径、applyAuth env 合并。
- **第三轮（4 项）**：bash prepare/env/inheritEnv、PowerShell programs、read truncation 字段、
  tools re-export。

误报核实：stream_simple normalize（normalize 已在 api 层 build_body 等价实现）、
agent afterToolCall 收到原始 toolCall（Rust 解构后传的实为 prepared.toolCall）。

剩余差异：
- **唯一「明确」剩余**：conformance 32 个测试 case（env 14 + storage 18），属测试契约覆盖缺口，
  非运行逻辑。
- **其余约 300 项**：加性多余 / 值等价（chord op 序列）/ 声明范围外（ai 40+ provider、OAuth、
  图像、thinking 全链路）/ 语言机制豁免（Proxy、TypeBox→JSON schema）/ 低危边界，绝大多数无需修。

验证：`cargo test --workspace` 475 passed / 0 failed，`cargo fmt` 干净，clippy 无新增告警。

**对照验证方法**：P5a 起引入「用 Node 直接加载上游 TS 计算期望值」的对照测试
（`tools.d/parity/*.mjs` + `crates/pi-durable/tests/*_parity.rs`），用于纯逻辑模块的 1:1 校验；
后续 P5 阶段可沿用该模式。

**合计 ≈ 4–6 周**（单人节奏）。

## 五、关键决策点（需拍板）

1. **现有 `crates/pi-agent` 的 harness 怎么处理？**
   （它 1:1 复刻自 v0.99.2，约两万行，但上游已无对应物。）
   - a. 保留不动，新架构放独立 crate，旧 harness 标记 legacy
   - b. 删除，彻底切到新架构
   - c. 保留但冻结（不再跟随），新架构并行开发
2. **新代码放哪？** 建议新建 `crates/pi-durable`；chord 子集建议并入 `crates/pi-durable` 的
   `chord` 模块（避免为一个 4k 行子集单开 crate），也可独立 `crates/chord`。
3. **sqlite 后端是否要？** 需要引入 `rusqlite`；Cloudflare Durable Object 后端在 Rust 侧无意义。
   建议 P3 只做 memory + jsonl，sqlite 按需。
4. **testing/conformance 是否这次做？** 纯测试投资（2,810 行）。
5. **`crates/pi-agent` 是否改名？** 上游已改叫 `pi-agent-core`；改名会牵动所有 import。

## 六、风险

- **工期**：4–6 周量级，且中途上游仍在快速发版（7 天 6 个 tag）。
- **语义漂移**：新架构的 task-graph/document 模型与旧 lane/drive 无对应关系，
  「1:1 复刻」只能相对 v1.1.0 而言；旧 harness 的知识不能直接复用。
- **依赖面扩大**：durable 依赖 chord（delta 引擎），Rust 侧要自己实现 immutable apply。
- **无参考实现**：旧 harness 至少还有我们自己的 Rust 版本可对照；durable 是全新的。
