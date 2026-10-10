# pi-durable session + env 复刻审计（v1.1.0 @ abe508e1b → pi_rs）

- 审计员：worker（1:1 复刻审计）
- 上游基线：`upstream/packages/durable/src/`（@earendil-works/pi-durable v1.1.0）
- Rust 复刻：`crates/pi-durable/src/`
- 方法：逐文件、逐导出符号/方法比对参数、返回值、控制流、分支、边界、错误处理、默认值、常量与字符串字面量。**只读审计，未运行任何测试**（共享 checkout 下 bash 被拒，cargo 无法执行；证据为源码逐行比对）。

## 一、结论

session + env 两层的总体结构、契约面（`FileSystem`/`Shell`/`BinaryReader`/`DirReader`/`FileWatcher`/`LineScan` 等 trait）、核心算法（行扫描、快照对比 watcher、提交线、事务装配、原子批次、adopt 指针替换、fork 拷贝策略）均已落地且大体一致。但存在：

- **逻辑差异 22 处**（其中 4 处会影响可观察行为的关键缺陷：`noFollow` 失效、retireDoc 对 fork-copy 不退役、doc() 缺 skipLoad、spill 丢 stderr）
- **缺失 8 处**
- **多余 4 处**（均为 Rust 侧新增且无害，`env/testing.rs` 无上游对应）

判定分类：
| 分类 | 数量 | 说明 |
|---|---|---|
| 缺失 | 8 | 上游存在、Rust 无对应的符号/行为 |
| 多余 | 4 | Rust 侧新增，上游无对应 |
| 逻辑差异 | 22 | 两侧均存在但行为不同 |
| 命名差异 | 若干 | 不算问题（Rust 风格命名） |
| 豁免 | 若干 | 语言机制等价差异，均已说明 |

---

## 二、逐文件核对总表

### env 层

| TS 文件 | Rust 文件 | 结论 |
|---|---|---|
| env/decode.ts | env/decode.rs | ✅ 一致（`from_utf8_lossy` 近似 WHATWG 流式解码，见豁免 E4） |
| env/index.ts | env/index.rs | ✅ 契约一致（Result→Rust Result 豁免；缺 `toError` 与 decode 顶层 re-export，见 M4/M8） |
| env/line-scan.ts | env/line_scan.rs | ✅ 算法 1:1（构造校验 panic 化，见 L11） |
| env/node-watch.ts | env/node_watch.rs | ✅ 大体一致（单 notify watcher vs 逐路径 fs.watch，见豁免 E3） |
| env/node.ts (1226 行) | env/node.rs (1262 行) | ⚠️ 13 处差异（L1–L12、L21，M1/M2/M5） |
| env/testing（上游不存在） | env/testing.rs | ⚠️ Rust 新增，无上游对应（X1） |

### session 层

| TS 文件 | Rust 文件 | 结论 |
|---|---|---|
| session/forks.ts | session/forks.rs | ✅ 1:1（asOf@commitSeq + current 两遍收集、copiedAddresses 去重、报错文案一致） |
| session/observation.ts | session/observation.rs | ⚠️ 2 处差异（L18、L19），其余 1:1（含 MAX_PENDING=100、溢出替换、r-op 退役） |
| session/session.ts | session/session.rs | ⚠️ 2 处差异（L17、L22），其余 1:1（提交线、publish、loadDocument、snapshotAsOf 文案一致） |
| session/transaction.ts (1042 行) | session/transaction.rs (1830 行) | ⚠️ 6 处差异（L13–L16、L20；M3），applySubmissionChange、settle_success 装配、adopt、stampTimes、validateOwners、checkpoint 谓词等 1:1 |

---

## 三、逻辑差异明细（22）

### env/node.rs（NodeExecutionEnv / 各 Reader）

**L1. `open_binary_reader` 的 `noFollow` 失效（关键）**
TS 用 `O_NOFOLLOW`（open 前平台分支）+ ELOOP/EMLINK → `symlinkRefused`；且 `O_NONBLOCK` 防 FIFO 阻塞。
Rust 先 `File::open`（跟随链接）再取 `file.metadata()`——句柄元数据永远不报 symlink，`no_follow == Some(true) && metadata.is_symlink()` 是死代码：指向文件的符号链接会被静默跟随打开；FIFO 打开可能阻塞。`symlink_refused` 无 cause。

**L2. exec 的 spill 文件只含 stdout（关键）**
TS spill 文件含两流按到达序交错的完整原始输出（前缀 + 后续块流式写入，带背压）。
Rust 只在 stdout 读取分支执行 `collected.extend_from_slice(...)`，stderr 分支不写入；spill 阈值（seen_bytes/seen_newlines）两流都计数，但落盘内容丢 stderr。且 `lines` 的“末块未以换行结尾”判断基于 collected（stdout）末字节，与 TS 的两流合并语义不同。

**L3. exec 中断结果丢失 spillPath；spill 写失败静默**
TS：timeout/aborted 的 ExecutionError 会附上 `spillPath`；spill 创建/写失败 → `ExecutionError("unknown", "Failed to preserve complete shell output: ...")` 并杀进程。
Rust：`if timed_out { return Err(ExecutionError::new(Timeout, ...)) }` 与 aborted 分支都不带 `spill_path`；`create_temp_file`/`std::fs::write` 失败被 `&& let Ok` 静默吞掉。

**L4. exec 信号终止的退出码**
TS：`code ?? (exitSignal ? 128 + signals[exitSignal] : 1)`（注释明说避免把 OOM 等误认为成功退出）。
Rust：`status.code().unwrap_or(1)`——信号终止一律 1，无 128+signo。

**L5. exec 的 onOutput 回调错误语义**
TS：emit 内 try/catch → `callback_error`（带 cause.message）+ 杀进程树，最终 settle 返回 callback_error；settled / 空文本 / callbackError 后不再回调。
Rust：`emit` 直接调用闭包，无 catch（回调 panic 直接传播）；无 settled 抑制；空 chunk 也回调（TS 跳过 `text === ""`）。

**L6. exec 流排空以 stdout EOF 为准；stderr EOF 分支忙等**
TS：`waitForChildProcess` 等两流 end + 退出 + spill 排空（EXIT_STDIO_GRACE_MS 宽限，防后代进程持有 stdio 的输出丢失）。
Rust：`(Some, Some)` 分支 stdout `Ok(0) => break`（stderr 尚未 EOF 时其尾部数据被丢弃）；stderr `Ok(0) => {}` 不 break → stdout 仍开时 select 忙等（空转）。

**L7. `remove` 与 TS `rm` 语义不同**
TS：`rm(path, {recursive: false})` 对空目录报 EISDIR（is_directory）；`recursive: true` 对普通文件成功删除。
Rust：`recursive == Some(true)` → `remove_dir_all`（对文件报 NotADirectory）；否则 `metadata.is_dir()` → `remove_dir`（空目录删除成功）。两处分支行为与 TS 相反。

**L8. `NodeDirReader` 打开时全量枚举 + 静默跳过全部 metadata 错误**
TS：`opendir` 惰性分页读，每条目 lstat，仅 ENOENT（消失条目）跳过，其他错误（如 EACCES）返回 FileError；不支持的 kind 跳过。
Rust：`open_dir_reader` 即读完整个目录并把 `symlink_metadata` 失败**全部**静默跳过（含权限错误），消失条目仍以陈旧元数据上报；打开后才出现/消失的条目行为不同。

**L9. `read_text_lines` 错误路径不关闭 reader**
TS：`try { ... } finally { await opened.value.close(context) }`——读错也关。
Rust：`let Some(line) = reader.read_line(context).await? else ...`——`?` 直接传播，错误路径泄漏文件句柄（`reader.close` 只在成功后调用）。

**L10. 各 Reader 的中止检查顺序**
TS：`abortResult` 在方法入口最先执行（先于 closed 检查、先于 buffered 行返回），每次读后再查。
Rust：`closed` 检查在前、buffered/ended 行先返回、abort 检查在循环内读之前。→ aborted+closed 时 TS 返回 aborted、Rust 返回 "is closed"；aborted+已缓冲行时 TS 返回 aborted、Rust 返回该行。

**L11. `scan_lines` 非法行范围 panic**
TS：`new LineScanner` 抛 RangeError，node.ts 捕获转 `FileError("invalid", "Invalid line range", path)`。
Rust：`LineScanner::new` 用 `assert!` → 非法范围直接 panic（不可恢复），不产生 FileError。

**L12. 文件写/截断/刷新缺操作后 abort 检查**
TS：`appendFile` 后 `afterAppendAbort`、`truncate` 后、`flush` 后均检查并返回 aborted（即使写入已发生）；`readBinaryFile`/`writeFile` 全程带 signal。
Rust：append/truncate/flush 无任何后置检查；读写不经 signal。

**L21. exec 的 cwd 检查**
TS：`access(cwd, F_OK)` 只查存在性（cwd 是文件时由 spawn 报 spawn_error）。
Rust：`Path::is_dir()`——cwd 是文件时提前返回 "Working directory does not exist: ..." 错误，与 TS 错误路径不同。

### session/transaction.rs

**L13. `retire_doc` 对 fork-copy 分支不置退役标记（关键）**
TS：`if (latest?.target?.kind === "fork-copy") { checkRecordScope(definition, latest.target.record); latest.retireOnCommit = true; return; }` → 装配写出 `document.copy` **并** `document.retire`。
Rust：`if is_fork_copy { return Ok(()); }`——不设 `retire_on_commit`、不校验 scope：分叉拷贝被创建但**不退役**。

**L14. `doc()`/`acquire()` 缺 skipLoad（关键）**
TS：替换正在退役的化身时 `#acquire(docEntry, seed, latest?.retireOnCommit === true)` 跳过加载（skipLoad）→ 创建新文档。
Rust：`acquire` 无 skip 参数，总是 `host.load` → 命中缓存里正在退役的同一 tracker，同一次提交内对同一记录产生两份 plan（retire + 变更复用同一化身）。

**L15. `reject_fork_source_writes` 缺 `fork === "current"` 条件**
TS 仅在 `record.scope.kind === "conversation" && record.fork === "current"` 时报错——允许在 fork 事务中修改 asOf 策略文档。
Rust：凡 `fork_source_conversation_ids` 命中即报错 → 过度拒绝 asOf 文档的修改。

**L16. `settle_submission` / `place_submission` / `set_task` 已结算时 panic**
TS：`#assertOpen()` 抛 `Error("Transaction has settled")`（可捕获）；setTask 的终态候选/换会话错误也是 throw。
Rust：`self.assert_open().expect("transaction must be open")` 与 `panic!("Task {} already has a terminal candidate")` 等——panic，不可按错误处理。

**L20. `committed_task` 无 Promise 级记忆化**
TS：`task.committedRead ??= storage.task(id)` 记忆化 promise（含 undefined 结果，并发读共享一次 I/O）。
Rust：只缓存 `Some(record)`；任务不存在时每次重复查询；并发读会重复发存储请求。

### session/session.rs 与 session/observation.rs

**L17. `watch_doc` 排队等待期间的 abort 不生效**
TS：注册 `markCancelled` 监听（once），入队等待/加载期间被取消 → 任务内 `throw cancellationError`，结束后 `watch.cancel()` 并抛错。
Rust：`cancelled: AtomicBool` 只在入口初始化，之后从不更新 → 等待期间 abort 被忽略：返回 `Ok(Some(watch))`，随后 `observe_cancellation` 把 watch 静默终止。TS 返回错误，Rust 返回已终止的 watch。

**L18. `observe_cancellation` 守卫条件错误**
TS：已装过取消 → 抛 "Watch cancellation is already installed"；watch 已结束 → 静默返回。
Rust：`assert!(state.end.is_none(), "Watch cancellation is already installed")`——条件用错：已结束的 watch 上调用会 panic，而重复安装不报错（且重复安装会再 spawn 一个取消任务，不记录已安装信号）。

**L19. `drain_watch` listener 错误丢失原始错误**
TS：`terminate({ reason: "listener_error", error: toError(error) })`——携带真实错误。
Rust：`WatchEnd::ListenerError("watch listener failed".to_string())`——固定文案，原始错误文本丢失。

**L22. `close()` 的关闭过程是惰性 future**
TS：`#closing = Promise.resolve().then(beforeClose).then(...)`——首次调用即启动（不 await 也执行 beforeClose 与 storage.close）。
Rust：`ensure_closing` 构造的 `BoxFuture` 只在被 poll 时执行；调用方丢弃返回的 future 则 before_close/storage.close 永不运行（close 监听器两侧都会同步触发）。

---

## 四、缺失（8）

| # | 位置 | 内容 |
|---|---|---|
| M1 | env/node.rs `exec` | 构造选项 `shellEnv`（基础环境变量）与 `inheritEnv: false` 语义缺失：TS `getShellEnv(baseEnv, extraEnv, inheritEnv)` 合并三源；Rust 只有 `options.env` 覆盖，默认继承，无 `shell_env` 字段 |
| M2 | env/node.rs `getShellConfig` | macOS/Linux 回退链缺失：TS 自定义 shell 不存在 → `shell_unavailable`("Custom shell path not found: ...")；`/bin/bash` 不存在 → `which bash` → `sh`。Rust 固定 `shell_path` 或 `/bin/bash`，失败转为 spawn_error |
| M3 | session/transaction.rs | `#track`/pendingOperations 跟踪与 `settleSuccess` 的 "Session commit callback settled before its pending Tx operations" 保护缺失（头注释声明为机制差异；后果：未 await 的 Tx 操作被静默丢弃而非报错） |
| M4 | env/index.ts→index.rs | 导出函数 `toError(error: unknown): Error`（instanceof/字符串/JSON.stringify 归一化）无对应实现 |
| M5 | env/node.rs `to_file_error` | `ABORT_ERR → aborted` 映射缺失（TS 有；Rust 的 aborted 全部走显式信号检查） |
| M6 | env/index.rs | `FileError`/`ExecutionError` 缺 `cause` 字段（TS 构造函数第 4/3 参数；Rust 把 cause 丢弃或并入 message） |
| M7 | session/transaction.rs `retire_doc` | fork-copy 分支缺 `check_record_scope(definition, target.record)` 调用 |
| M8 | env/mod.rs | decode 三符号（`rangeDecoder`/`StreamDecoder`/`startsWithBom`）未在 env 顶层重导出（TS index.ts 有 re-export；Rust 仅 `pub mod decode`） |

## 五、多余（4）

| # | 位置 | 内容 |
|---|---|---|
| X1 | env/testing.rs | `InMemoryFileSystem`：上游 `packages/durable` 无 env/testing（`env/testing.ts`、`env/testing/*.ts` 均不存在，package.json exports 无测试 env）。Rust 自研最小实现（line_reader/dir_reader/watch/temp 等返回 NotSupported），属新增测试辅助，非复刻 |
| X2 | session/session.rs | `is_closing()` / `poison_error()` / `drain_line()` 诊断与测试助手，TS 无对应（无害） |
| X3 | env/node.rs | `with_shell()` / `with_watch_options()` builder 风格构造器（TS 构造参数直传；纯 API 新增，无害） |
| X4 | env/node_watch.rs | Windows `cfg` 分支的 `metadata_dev`/`metadata_ino`（范围外平台，无害） |

---

## 六、豁免 / 命名差异（说明）

| # | 内容 |
|---|---|
| E1 | env/index.ts 的 `Result`/`ok`/`err`/`getOrThrow`/`getOrUndefined` → Rust 标准库 `Result`（任务书已知豁免） |
| E2 | 事务 pending-operations：Rust async fn 惰性求值、无 Promise 起始副作用（M3 的行为后果见 L 类说明） |
| E3 | node_watch：`fs.watch` 逐路径 watcher → notify 单实例 `RecommendedWatcher`；`Installed.watcher` 退化为 dev/ino 身份（Rust 头注释声明）。两个近似点：notify error 事件仅 schedule_flush（TS 会 close 并删除该路径 watcher，靠 reconcile 按 dev/ino 重建）；事件路径为空的事件不触发 flush（TS 目录级事件必 flush）。整体"事件仅触发去抖重扫、变化来自快照差集"的设计保持一致 |
| E4 | decode：`from_utf8_lossy` + 尾部不完整序列回退，近似 WHATWG `TextDecoder(stream:true)`（标准无效序列均 U+FFFD 一致；仅在极端非法序列边角可能有差异） |
| E5 | session：`HarnessImpl extends SessionImpl` 钩子 → `SessionHooks` trait + `DefaultSessionHooks`；`#tail` Promise 链 → tokio 公平 Mutex；queueMicrotask → `tokio::spawn`（无运行时退化同步）；`AbortSignal.addEventListener` → `cancelled() + select!`（均为声明式机制映射） |
| E6 | createTempFile：TS 在新建临时目录内放 UUID 文件；Rust 直接在 temp_dir 放 `pid-nanos-counter` 文件——唯一性与前缀/后缀语义等价（路径形态不同） |
| E7 | Windows 特有分支（Git Bash/WSL/taskkill、`~\\`、win32 轮询默认）按任务书明确不移植 |
| E8 | 类型级校验：offset/length/size/maxLines 非负整数、EntryId 安全整数检查由 u64/usize 类型承担（等价）；`resolvePath` 的 `file://` 仅剥前缀（TS 走 `fileURLToPath`，畸形 URL 回退为普通路径，行为仅在畸形 URL 上不同） |
| E9 | mtime 取不到时 Rust 回退 0.0；`to_file_error` 用 fallback path（TS 用 `nodeError.path`）；错误 message 来自 OS（个别文案与 TS 构造的 "Is a directory" 等不同，code 一致） |

---

## 七、未验证事项（风险）

1. 本环境共享 checkout 拒绝 bash，无法运行 `cargo test`/TS 测试；以上结论全部来自源码逐行比对，未经动态验证。
2. notify（macOS FSEvents/Linux inotify）事件到 `on_event` 路径的等价性（E3）依赖 notify 后端行为，未做运行时验证。
3. `from_utf8_lossy` 与 WHATWG 流式解码在构造型非法序列上的完全等价性（E4）未做差异测试。
4. Rust `Prepared`（已计算批次）丢弃即放弃、与 TS `prepared.abort()` 的 tracker 回滚语义等价，系据 chord::delta 移植说明推断，未逐 op 验证。

## 八、重点核对结论（任务书指定项）

| 指定项 | 结论 |
|---|---|
| transaction.ts 表读写 / 文档获取、创建、退役 | 表读写（read/write、ReadAfterWrite、requireConversation）✅；`settle_submission`/`place_submission` 的 applySubmissionChange 状态机 ✅ 1:1；**retire_doc fork-copy 分支不退役（L13）、doc() 缺 skipLoad（L14）** |
| settle_success 组装原子批次 | ✅ 装配顺序（plans → rejectForkSourceWrites → validateOwners → 任务替换校验 → 终态退役扫描 → 发布归属 → 提交变更 → writes 拼接）与 TS 一致；`reject_fork_source_writes` 过滤条件缺失（L15） |
| adopt 按指针替换并发布 | ✅ tracker.adopt/prepared.abort、deltasSinceBase、install/evict、三类 publication 与 TS 1:1 |
| apply_submission_change 提交生命周期 | ✅ done/unanswered 早退、placed→input placed/write done、done 需 placed input、unanswered 保留 entry——全部一致（含报错文案） |
| session.rs 生命周期、CommittedStateSource/CommittedWatch、帧观察、退役/停止/取消 | 提交线、close 顺序、poison、loadDocument、snapshotAsOf ✅；CommittedStateSource ✅；CommittedWatch 溢出替换（100 帧、r-op）✅；**observe_cancellation 守卫（L18）、listener 错误文本（L19）、watch_doc 等待期 abort（L17）** |
| forks.rs prepare_fork_document_copies asOf/current 策略 | ✅ 1:1：entry(commitSeq) → asOf@At(commitSeq) + current@Current 两遍、copiedAddresses 共享去重、latest→latest / 其余→rewindable、报错文案一致 |
| env/node.ts 文件读写、shell 执行、watch | 文件读写大体一致（L7/L8/L9/L10/L12 除外）；shell 执行差异集中（L2–L6、L21、M1/M2）；watch 委托与 TS 一致（E3 近似点） |
| macOS/Linux 分支需一致（Git Bash/WSL/Windows 明确不移植） | Windows 分支未移植 ✅；macOS/Linux 上 `getShellConfig` 回退链缺失属范围外豁免但为行为差异（M2） |
| env/line-scan.ts 行扫描算法 | ✅ 1:1（含 BOM 头暂存、feed 边界、endFirstLine/endSelection、finish 空选择；构造校验 panic 化见 L11） |
| env/testing ↔ env/testing.rs | 上游无此文件，Rust 为新增（X1） |
