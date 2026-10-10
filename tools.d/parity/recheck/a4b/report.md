# Durable storage 1:1 复刻审计 — a4b

基线：upstream v1.1.0 (commit abe508e1b)。TS 根 `upstream/packages/durable/src/storage/`，Rust 根 `crates/pi-durable/src/storage/`。
判定分类：`缺失` / `多余` / `逻辑差异` / `命名差异`（不算问题）/ `豁免`（语言机制等价，需说明）。

---

## 1. scan.ts → scan.rs

TS 导出：`ScanStart` 类型、`scanStart(requested, cursor, fallback)`、`nextCursor(id, order)`。
Rust 导出：`ScanStart`、`ScanError`、`scan_start`、`next_cursor`，外加共享 `page()` helper。

| 判定 | 差异 |
|---|---|
| 命名差异 | `scanStart→scan_start`、`nextCursor→next_cursor`、`ScanStart.order/after` 同名（不算问题）。 |
| 逻辑差异 | **requested 校验位置/文案**：TS 在 `scanStart` 内先校验 `requested` 并抛 `TypeError("Invalid scan order: ${String(requested)}")`；Rust 的 `requested` 已是 `Option<ScanOrder>`（校验前移到反序列化边界），`ScanError::InvalidOrder` 变体被定义但全库从未构造（仅 Display 引用，grep 证实），实际错误文案为 serde 的 `unknown variant` 而非 `Invalid scan order: …`。行为语义等价（先于一切执行），文案不可达。 |
| 逻辑差异 | **after 边界校验**：TS `typeof after === "number" && Number.isSafeInteger(after)`（接受负数如 -1，拒绝 >2^53-1）；Rust `Value::as_u64`（拒绝负数/非整数，接受至 u64::MAX）。两个方向都存在可观察差异（游标 after=-1 或 2^60）。 |
| 豁免 | TS 抛 `TypeError`、Rust 返回 `Result<_, ScanError>`：错误通道语言机制等价；无游标分支 `requested ?? fallback` ≡ `unwrap_or(fallback)`；stored 校验、`stored ?? fallback`、order 不一致报错（"The cursor continues a {order} scan; the query asks for {requested}" 文案逐字一致）均一致。 |
| 逻辑差异 | **`nextCursor` 一致** ✓，但 Rust 新增共享 `page(values, limit, order, id_of)` helper（TS 各后端内联）：`limit=0` 且非空时，TS 内联实现 `items.at(-1)!.id` 抛 `TypeError`；Rust `saturating_sub(1)` 取 `values[0]` 作游标、`truncate(0)` → 返回 `{items:[], next:Some(cursor)}`。边界行为不同（各后端同样受影响）。 |
| 多余 | Rust 侧 `page()` 为 TS 无对应符号的新 helper（重构提取，非逻辑多余，但 limit=0 语义与 TS 内联实现不同，见上）。 |

其余逻辑（order 校验顺序、fallback、游标缺 after/order 报 "Invalid storage cursor"）逐条一致。

## 2. memory.ts → memory.rs

TS 类 `MemoryStorage` 方法：`commit`、`prepareCommit`、`mintId`、`conversation`、`scanConversations`、`entry`(×2 重载)、`findLatestHeadMarker`、`scanEntries`、`task`、`scanTasks`、`submission`、`scanSubmissions`、`submissionByRequest`、`findDocument`、`document`、`scanDocuments`、`close` + 私有 `resolveDocumentCopies`/`applyPreparedCommit`/`materializeDocument`/`visibleEntries`/`visibleEntriesAscending`/`checkGlobalIds`/`prepareDocumentActions`/`checkDocumentActions`/`applyDocumentActions`/`assertOpen`。

| 判定 | 差异 |
|---|---|
| 逻辑差异 | **conversationIdsByOwnerConversation 索引键错位**：TS `applyPreparedCommit` 用 `owner.conversationId` 建索引（`conversationIdsByOwnerConversation`）与 `owner.taskId`（`conversationIdsByOwnerTask`）；Rust `apply_writes` 把 `record.parent.conversation_id` 放入 `conversation_ids_by_owner_conversation`、`record.owner.task_id` 放入 `by_owner_task`。`scanConversations({ownerConversationId})` 在 Rust 中返回的是「父会话=该 id」的会话而非「属主会话=该 id」。 |
| 逻辑差异 | **scanConversations 分支优先级与过滤**：TS `ownerTaskId` 优先于 `ownerConversationId` 选择索引，且循环内总是执行 `value.owner?.conversationId !== query.ownerConversationId → continue` 过滤（对 ownerTaskId 路径生效）；Rust `owner_conversation_id` 优先、无任何循环内过滤。两者同时给出时结果不同。 |
| 逻辑差异 | **未知 conversation 不报错**：TS `visibleEntries`/`visibleEntriesAscending`/`findLatestHeadMarker` 抛 `Error("Unknown conversation: ${conversationId}")`（scanEntries、entry(cid,id)、findLatestHeadMarker 均受影响）；Rust `visible_entry_ids*`/`find_latest_head_marker` 静默 break/返回 `Ok(None)` → 空页。 |
| 逻辑差异 | **submission 旧 requestId 索引不清理**：TS 更新 submission 时若 `previous.requestId` 存在且映射指向本 id，先删除旧映射再写入新映射；Rust 只 insert 新 `(conv, requestId)` 映射，从不删除旧映射 → `submissionByRequest` 用旧 requestId 仍能查到已改键的 submission。 |
| 逻辑差异 | **commit 原子性被破坏（重要）**：Rust `apply_writes` 先做 `validate_writes`（仅 = TS `checkGlobalIds`），文档写入的语义校验全部在应用循环内逐条进行 → 批次中途失败（如 `document.change`/`document.retire` 指向未知文档、copy 源不可读）会留下部分已应用状态。TS `prepareCommit` 在校验阶段（resolveDocumentCopies + checkGlobalIds + checkDocumentActions）全部通过后才 apply，零部分应用。 |
| 缺失 | 对应缺失的 TS 校验（均应在 apply 前发生）：① `Fork source document ${id} is changed in the copy batch`（copy 源在本批被改）；② `Fork source document ${id} cannot be read`（Rust `materialize` 不检查 `is_alive_at`/`is_current_only`，TS `materializeDocument` 检查 → 已退役/当前只读历史不可读的源在 Rust 可被拷贝）；③ `Fork source document ${id} does not match the copied record`（scope.kind/kind/key/history/fork 一致性）；④ 非 StorageRejected 错误的包装 `StorageRejected("Document copy ${record.id} was rejected", {cause})`；⑤ `Document ${id} is retired`（Rust 对已退役文档继续 push 修订）；⑥ `Document ${id} delta has no base`；⑦ `Document ${id} version transition requires a base`（Rust 直到物化才报 "crosses a stored version boundary"）；⑧ `Document ${id} has more than one content command`；⑨ `Document ${id} is retired more than once`；⑩ `Document address already has a current incarnation`（live 计数校验完全缺失，Rust 可创建双活化身，`index_document` 无条件 `current = Some(id)` 覆盖）。 |
| 逻辑差异 | **错误文案/类型**：未知文档：TS `Unknown document: ${id}` → Rust `Document ${id} does not exist`；TS `checkGlobalIds` 抛普通 `Error`、Rust 包成 `StorageError::Rejected(StorageRejected)`；TS `assertOpen` 抛 `Error("MemoryStorage is closed")` → Rust `StorageError::Closed`（Display "storage is closed"）。错误通道为语言机制（豁免），但文案逐字不同。 |
| 逻辑差异 | **mintId 耗尽检查缺失**：TS `!Number.isSafeInteger(nextId)` 抛 `"ID space is exhausted"`（ID 上限 2^53-1）；Rust `mint_id` 无此检查（u64 溢出时 debug panic / release 回绕），签名 `async fn mint_id(&self) -> u64` 无法返回错误（panic 代替，注释自认）。 |
| 逻辑差异 | **commit 不推进 next_id**：TS 每个写入 `nextId = Math.max(nextId, id + 1)`；Rust 从不更新 `next_id` → 后续 `mint_id` 可能与已提交 ID 冲突（validate_writes 会拒掉后续提交）。 |
| 逻辑差异 | **scanEntries 游标边界下溢**：`after=0` 时 TS `maxEntryId = min(max, -1)` → 空结果；Rust `after - 1` u64 下溢（debug panic / release 回绕 u64::MAX）。同族：`after+1` 在 `after=u64::MAX` 上溢（TS 不会遇到：safe-integer 校验拒绝；Rust 接受）。`scan_keys` 升序 `after + 1..` 同理。 |
| 逻辑差异 | **scanDocuments 游标语义**：TS 用私有 `cursorId(cursor)` 只取 `after`、完全忽略 `cursor.order`，恒升序，且游标缺 `after` 视为无游标（start=0）；Rust `scan_start(None, cursor, Ascending)` 校验并采用游标里的 order（可降序续扫），游标缺 `after` 报 `InvalidCursor`。 |
| 逻辑差异 | **limit=0 + 非空扫描**：TS 内联 `page`：`items.at(-1)!.id` → `TypeError`；Rust 共享 `page`：返回空 items + 指向 `values[0]` 的续扫游标。（scanConversations/Entries/Tasks/Submissions/Documents 全部受影响。） |
| 豁免 | BTreeMap/BTreeSet 替代「Map+有序数组+二分」（语义等价）；clone/freeze → `#[derive(Clone)]`+Mutex（等价）；`prepareCommit` 两阶段 → `commit` 内先校验后应用（意图等价，但 Rust 校验不完整，见上）；`entry` 重载拆分 `entry`/`entry_in_conversation`（trait 注释自认）；`mintId` panic vs throw；`headEntryIds` 专用索引 → 扫描时按 `record.head` 过滤（findLatestHeadMarker 结果等价）；DocumentChange 对 current-only 文档的 base 替换（TS `revisions=[revision]`，Rust 总是 push）与 retire 清空（TS 清空，Rust 保留）：materialize 均取最后 base 且 current-only 不支持 as-of 读，不可观测，仅内存占用差异；findDocument 遍历顺序（TS 升序取首个 alive，Rust rev 取首个 alive）：TS 的 live 计数不变量下至多一个 alive，结果相同——但 Rust 缺该校验（见上）时两化身并存、结果不同。 |
| 多余 | `MemoryStorage::validate_writes`（公开）与自由函数 `validate_writes`：TS 无此 API（为 jsonl 后端复用而加，仅含全局 ID 检查、不含文档校验）。 |
| 逻辑差异 | `scan_documents` 升序游标范围：Rust `scan_keys` 降序 `range(..after.unwrap_or(u64::MAX))` 排除 `u64::MAX` 本身（TS lowerBound 包含全部）——仅当 id==u64::MAX 时可观察（实际不可达，低危）。 |

一致项确认：`checkGlobalIds` 四类表判定与三条报错文案逐字一致（Rust `validate_writes`）；task/submission 状态索引增删语义等价；`scanTasks`/`scanSubmissions` 过滤器与默认升序一致；`findLatestHeadMarker` 链式上溯/`upper = min(upper, parent.at)`/`upper < min break` 一致；`visibleEntries*` 截断逻辑一致；`materialize` 取最后 base、版本边界报错 `crosses a stored version boundary without a base`、`missing a required base` 文案一致；`isAliveAt`/`isCurrentOnly`/`scopeKey`/`addressKey` 结构一致；`close` 置位等价。

## 3. jsonl/{index,node,storage}.ts → jsonl/{mod,codec,storage}.rs

TS 导出：`index.ts`（re-export）、`node.ts`（`openNodeJsonlStorage`，Node 环境胶水）、`storage.ts`（`JsonlStorage`、`JsonlStorageOptions{fsync}`、`JsonlCorruptionError`、`JsonlStoragePoisonedError`）。Rust：`mod.rs`、`codec.rs`（纯编解码）、`storage.rs`。

| 判定 | 差异 |
|---|---|
| 豁免 | `node.ts` 为 Node 环境胶水（Rust 侧由 `env::FileSystem` 抽象替代，属语言机制）；错误类 `JsonlCorruptionError`/`JsonlStoragePoisonedError` → `StorageError::Message`（文案前缀 `JsonlCorruptionError:` 手动拼接，poison 文案逐字一致）；`MemoryStorage` 代理读路径（Rust 同样委托 memory）。 |
| 命名差异 | `encodeCommit→encode_commit`、`sidecarFileName→sidecar_file_name`、`isSidecarFileName→is_sidecar_file_name`、`isReclaimFileName→is_reclaim_file_name`、`sidecarKey→sidecar_key`、`isCurrentOnly→is_current_only`（不算问题）。 |
| 一致 | `FORMAT_VERSION=1`、`MAIN_FILE="main.jsonl"`、`RECLAIM_SUFFIX=".reclaim"`、文件名正则（禁前导零）、`sidecar_key` 的 `[file,seq,ordinal]` JSON 形状、`fsync` 默认 false、marker/sidecar 记录字段形状与顺序（format/type/seq/ordinal/payload）、encodeCommit 的分派（conversation/entry/submission/document.retire 直接进主文件；task 终态进主文件否则 sidecar+`task.sidecar`；document.create/change 进 sidecar）、ordinal 全提交内全局递增——全部逐字对齐（codec.rs 有测试佐证）。 |
| 缺失 | **sidecar 回收完全未实现**（mod.rs 自认「暂未实现 sidecar 回收」）：TS 的 `planReclamations`/`reclaimSidecars`/`replaceSidecar`（`.reclaim` 原子替换、终态 task sidecar 删除、current-only 文档 base 替换/退役清空）在 Rust 无对应物；`adoptSidecarState`（currentOnlyDocuments/liveTaskSidecars 维护）缺失。 |
| 逻辑差异 | **跨实现不兼容（重要）**：TS 会删除终态任务的 sidecar 文件，重放时 `task.sidecar` 引用按 optional 处理（缺失不报错）；Rust `resolve_marker` 一律 `find_sidecar_record` 硬失败（`Missing sidecar record …`）→ **Rust 无法打开 TS 写过的（发生过回收的）存储目录**。同理 TS 的 `document.create` 被回收后回退为 `{kind:"base",version:1,value:{}}`、`document.change` 按 `isBeforeLatestBase` 跳过——Rust 均无此逻辑。 |
| 缺失 | `recover()` 的大量损坏检测：① 主文件 seq 严格递增校验（"Commit sequence does not strictly increase in main.jsonl"）；② 目录列举 + 清理 `*.reclaim` 残留文件；③ sidecar 记录内排序校验（"Sidecar records are out of order in ${file}"）；④ 重复确认检测（"Sidecar record is confirmed more than once"）；⑤ 未确认尾截断（"Confirmed record follows an unconfirmed tail in ${file}" + truncateFile）；⑥ 撕裂尾行截断（TS 读二进制、按最后 0x0a 截断损坏尾行；Rust `read_text_file`+`lines()` 会把撕裂行当整行 → serde 失败 → 打开失败）；⑦ UTF-8 校验（"Invalid UTF-8 in complete … line N"，fatal decoder）；⑧ 空行处理（TS 空行 → "Malformed complete …" 损坏；Rust `trim().is_empty() → continue` 静默跳过）。 |
| 缺失 | `open()` 不创建目录：TS `absolutePath → createDir(recursive) → joinPath`；Rust 无 createDir（目录不存在时 exists=false 空库通过，之后 append 失败）。 |
| 缺失 | 结构校验弱化：TS `parseMainMarker`/`validateMainOperation`/`parseSidecarRecord` 的语义校验（seq≥1、id 安全整数、ordinal 安全整数、主文件 task 必须 terminal、sidecar task 必须非 terminal「Invalid live task record」、payload.id 与引用一致「Confirmed … data does not match commit」、"Document creation lacks a confirmed base"）→ Rust 依赖 serde 类型校验（seq=0、非终态 task 进主文件、payload.id 错位均不被发现）。 |
| 逻辑差异 | **document.copy 提交不可用**：TS jsonl commit 先 `memory.prepareCommit`（resolveDocumentCopies 展开为 create）；Rust `write_commit` 用 `validate_writes`（不展开 copy）→ `encode_commit` 返回 `UnresolvedDocumentCopy` 错误。Session 层确实会下发 `DocumentCopy`（transaction.rs:1767）→ **Rust jsonl 后端无法提交 document.copy**。 |
| 逻辑差异 | **commit 预校验不完整 → 磁盘污染**：TS 在写盘前完成全部校验（全局 ID + 文档动作）；Rust 只做全局 ID 校验，文档错误（未知文档、已退役、delta 无 base、版本转换等）在 marker 落盘后的 `memory.commit` 才失败 → `record_failure` 置 poisoned → 该目录重开后因同样的错误永久无法打开（TS 在写盘前拒绝、目录无损）。 |
| 逻辑差异 | **poison 时机不同**：TS 在 sidecar append/flush/main append 任一步失败即 poison；Rust 只有 marker 落盘后内存态失败才 poison（写盘失败仅返回错误、存储仍可用）。 |
| 逻辑差异 | **poison/close 后读取**：TS 所有读经 `store` getter → `assertUsable()` → 关闭/中毒后读也抛错；Rust 读方法直接委托 memory 不查 `assert_usable` → 中毒后读继续可用。`mint_id` 同理：close 后 Rust `memory.mint_id` panic（"MemoryStorage is closed"），TS 抛 `Error("JsonlStorage is closed")`。 |
| 逻辑差异 | **fsync 范围**：TS commit 只 flush sidecar、不 flush main（main 只在回收前 flush）；Rust fsync 时多 flush 一次 main（更保守，行为偏差但无害）。 |
| 逻辑差异 | 错误文案：TS `errorFromFile` 带文件名与动作（"JSONL append to task-1.jsonl failed: …"）；Rust "JSONL append sidecar failed: …" / "JSONL read sidecar failed: …"（文件名缺失）。"Missing confirmed sidecar record ${file} at sequence ${seq}" vs "Missing sidecar record {file}#{ordinal}@{seq}"。 |
| 逻辑差异 | 重放时 `memory.commit`（内部 next_seq 连续）vs TS `prepareCommit(writes, marker.seq)`（按 marker seq 显式落 seq）；Rust 最终用「最后 marker seq+1」初始化 next_seq —— 结果等价（豁免级别，仅内部序号推进机制不同）。 |
| 多余 | `unresolved_copy()` 死代码 helper（`#[allow(dead_code)]`）；`open()` 里 `exists()` 探测逻辑（TS 无）。 |
| 逻辑差异 | 撕裂尾行/UTF-8 处理见上（缺失⑧）：TS 截断恢复、Rust 打开失败 → 同一损坏目录 TS 可恢复、Rust 不可。 |

## 4. sqlite/{index,database,node,migrations,storage}.ts → sqlite.rs

已知豁免：`database.ts`/`node.ts`（异步 facade + 串行队列，Rust 用 `Arc<Mutex<Connection>>` 同步串行化等价）、`migrations.ts`（版本化 schema，Rust `open()` 直接建表）、`cloudflare.ts`（明确不移植）。`storage.ts` 17 个公开方法（含静态 `open`）逐一对比：

| 判定 | 差异 |
|---|---|
| 豁免 | schema 形态：TS 结构化列 + record JSON + 索引；Rust `meta/records(table_name,id,seq,data)/documents(id,record,revisions)` JSON 列（mod 注释自认）。逐列核对数据往返等价：`entries.commit_seq` ≡ `records.seq`；`documents.created_at/retired_at` 在 record JSON；revisions 每条自带 `seq`（`StoredRevision`）等价 TS `document_revisions.seq` + PK(document_id,seq)。谓词下推能力不同（性能），语义一致 → 豁免。 |
| 命名差异 | `mintId→mint_id`、`scanConversations→scan_conversations`、`findLatestHeadMarker→find_latest_head_marker`、`submissionByRequest→submission_by_request`、`findDocument→find_document` 等；`entry` 重载拆 `entry`/`entry_in_conversation`（trait 自认）（不算问题）。 |
| 多余 | `open_in_memory()`、`from_connection()`（TS 无，测试/组合用途）；`same_scope` 辅助。 |
| 逻辑差异 | **scanConversations 属主过滤错误（与 memory.rs 同款 bug）**：TS sqlite 用列 `owner_conversation_id`（= `record.owner.conversationId`）过滤；Rust 内存过滤用 `record.parent.conversation_id`。`{ownerConversationId}` 查询在 Rust 返回「父会话=该 id」的会话。 |
| 逻辑差异 | **findLatestHeadMarker 语义改写**：TS（memory 与 sqlite 一致）按 fork 链**段优先**：先查查询会话自身段内最新 head 条目（≤截止），本段没有才上溯父段；Rust 全局按 id 降序扫描、返回**全局最新可见** head 条目。反例：查询会话 A 有 head 条目 id=5，父会话 B 有 head 条目 id=9 且 A.parent.at≥9（9 对 A 可见）→ TS 返回 5，Rust 返回 9。 |
| 逻辑差异 | **未知 conversation 不报错**：TS sqlite 在 `entry(cid,id)`/`findLatestHeadMarker`/`scanEntries`（readEntry/readLatestHeadMarker/readEntries/readEntriesAscending 均先 `readConversation` 判 undefined 抛 `Unknown conversation: ${id}`）；Rust `load_fork_segments` 静默 break → 空页/None。 |
| 逻辑差异 | **document(未知 id)**：TS `materializeDocument` 首查 record 行，undefined → 返回 undefined；Rust `read_document` 缺失 → 返回错误 `"Document {} does not exist"`。（TS memory 与 Rust memory 均返回 None，仅 sqlite 不一致。） |
| 逻辑差异 | **materialize 检查顺序**：TS 先 `isCurrentOnly` 抛 "does not retain historical content" 再 `isAliveAt` 判空；Rust sqlite `materialize_document` 先 `alive_at` 返回 None 再查 current-only → 对「已退役的 current-only 文档 + 历史点」TS 抛错、Rust 返回 None（Rust memory.rs 的顺序是对的，sqlite.rs 内部不一致）。 |
| 逻辑差异 | **scanDocuments 游标语义**（与 memory.rs 同款）：TS 用 `cursorId` 只取 after、忽略 `cursor.order`、恒升序、缺 after 视为无游标；Rust `scan_start(None, cursor, Ascending)` 校验并采用游标 order（可降序）、缺 after 报 InvalidCursor。 |
| 缺失 | **document.copy 提交被拒绝**：TS sqlite 支持 copy（checkDocumentActions 的 copy-source-in-batch 检查 `StorageRejected("Document copy ${id} source is changed in the copy batch")`、applyDocumentActions 内物化源 + 六字段一致性校验 + `StorageRejected("Document copy ${id} was rejected", {cause})` 包装）；Rust `apply` 直接返回 `"document.copy … must be resolved to document.create before commit"` → 功能缺失（Session 会下发 DocumentCopy，transaction.rs:1767）。 |
| 缺失 | commit 的文档校验（TS `checkDocumentActions` 全部缺失，仅部分以不同文案/时机替代）：① "Document ${id} is retired"（Rust 对已退役文档继续 push 修订）；② "Document ${id} delta has no base"；③ "Document ${id} version transition requires a base"（Rust 推迟到物化时报 crosses …）；④ "Document ${id} has more than one content command"；⑤ "Document ${id} is retired more than once"；⑥ "Document address already has a current incarnation"（live 计数缺失 → 可产生双活化身；`insert_document` UPSERT 会静默覆盖）；⑦ "Unknown document: ${id}"（Rust 以 `"Document {} does not exist"` 在应用阶段报，文案与时序不同）。原子性本身 OK（rusqlite 事务回滚）。 |
| 逻辑差异 | **next_id 推进缺失**（与 memory.rs 同款）：TS commit 计算 `candidateNextId = max(nextId, 写入 id+1)` 并持久化 `next_id = max(metadata.next_id, candidate)`；Rust commit 不推进 next_id → mintId 可能与已提交 ID 冲突（后续提交被 "already belongs" 拒绝）。 |
| 逻辑差异 | **mintId 行为差异**：TS mintId 只动内存 nextId（提交时才持久化），且 `!Number.isSafeInteger(nextId)` 抛 "ID space is exhausted"；Rust `mint_id` 每次调用都 `bump_counter` 写库（崩溃后 ID 不复用，TS 会复用）、无耗尽检查（u64 溢出 panic）、closed 时 panic。 |
| 逻辑差异 | **scan_records 升序哨兵**：TS `scanSql` 升序 `id > -1`（含 id=0）；Rust `after.unwrap_or(0)` → `id > 0`（排除 id=0 记录）。降序哨兵 TS `MAX_SAFE_INTEGER` vs Rust `i64::MAX`（>2^53 的 id 表现不同，与 scan.ts 的 after 边界差异同族）。 |
| 逻辑差异 | **submissionByRequest 无索引**：TS 用 `request_id = encodeIndexedString(requestId)` 列查询；Rust 全表扫描按 id 升序比对解码值——语义等价（request_id 在会话内唯一的不变量下），但存储格式不同（TS 列存 JSON 引号形式）→ 豁免级别，仅在重复 requestId 的非法数据上可观察（TS 无序任意、Rust 最小 id）。 |
| 逻辑差异 | **findDocument 选取顺序**：TS `ORDER BY created_at DESC LIMIT 1`（最新创建）；Rust 按 id 升序取最后一个 alive（最高 id）。正常不变量（至多一个 alive）下等价；Rust 缺 live 计数校验时可观察差异。 |
| 豁免 | close：TS 等待已接纳读完成后真关库（幂等）；Rust 置标记、连接随 Arc 释放（注释自认，语义等价）；admitRead 机制（close 与读的排序）由 rusqlite 单连接串行化替代；错误通道 `Error/StorageRejected` → `StorageError::Rejected/Message`（文案基本沿用，类型语言机制）。 |
| 逻辑差异 | limit=0 边界、"MemoryStorage/SqliteStorage is closed" 文案 → StorageError::Closed（"storage is closed"）、全局 ID 报错包成 StorageRejected——与 memory 后端同款差异，见 §2。 |
| 一致 | 17/17 方法均有对应：open/commit/mintId/conversation/scanConversations/entry(×2 拆)/findLatestHeadMarker/scanEntries/task/scanTasks/submission/scanSubmissions/submissionByRequest/findDocument/document/scanDocuments/close。scanTasks 五过滤、scanSubmissions 两过滤、scanEntries 游标收窄语义（Rust 由 SQL `id>?/id<?` 承担 after 收窄，等价）、alive_at/current-only 语义、base+delta 重放（"missing a required base"/"crosses a stored version boundary without a base" 文案一致）、records.seq≡commit_seq、task/submission UPSERT 替换语义——均对齐。 |

## 5. 汇总

**结论：三个后端均非 1:1 复刻。** 核心结构（scan 游标、checkGlobalIds 四表规则、base+delta 物化、两阶段写）大体对齐，但存在 3 类系统性偏差：① `document.copy` 在 Rust jsonl/sqlite 后端不可提交（memory 的 copy 校验也不全）；② commit 的文档语义校验（退休/版本转换/多命令/双活化身等）在 Rust 全部三个后端缺失或挪到应用阶段，memory/jsonl 因此破坏原子性；③ 多处「属主/可见性」查询语义错位（byOwnerConversation 用 parent、findLatestHeadMarker 段优先 vs 全局最新、未知会话静默、scanDocuments 忽略/尊重游标 order 相反）。

| 分类 | 数量 | 分布 |
|---|---|---|
| 缺失 | 29 | memory 10、jsonl 11、sqlite 8 |
| 多余 | 4 | scan 1（共享 page helper）、memory 1（validate_writes 公开 API）、jsonl 1（unresolved_copy/exists 探测）、sqlite 1（open_in_memory/from_connection） |
| 逻辑差异 | 33 | scan 3、memory 12、jsonl 8、sqlite 10 |
| 豁免 | — | sqlite 异步 facade、cloudflare 不移植、BTreeMap 替代手写二分、错误通道 Result/panic、clone→derive、schema JSON 列形态、close/admitRead 串行化 |

最高优先级：Rust jsonl 无法提交 document.copy（功能缺失）；Rust jsonl/sqlite 无法打开 TS 写过的发生回收的目录 / 语义查询错位；memory/jsonl commit 校验缺失导致部分应用与磁盘污染。

