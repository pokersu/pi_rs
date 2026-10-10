# durable 基础层 1:1 复刻审计报告（recheck a4a）

- 基线：upstream `earendil-works/pi` v1.1.0（commit abe508e1b）
- TS 根：`upstream/packages/durable/src/`；Rust 根：`crates/pi-durable/src/`
- 分类：缺失 / 多余 / 逻辑差异 / 命名差异（不算问题）/ 豁免（语言机制等价，需说明）

## index.ts → lib.rs（导出面）

- index.ts 从 `types.ts` 导出全部类型 + 值导出 `ROOT_CONVERSATION_ID`；从 `documents.ts` 导出 `defineDoc, defineDocFamily`；从 `entries.ts` 导出 6 个类型；从 `errors.ts` 导出 3 个错误类；从 `tasks.ts` 导出 `defineTask`；另有 harness/session/storage 的大量导出（超出本次范围）。
- lib.rs 只声明 `pub mod`（chord/documents/entries/env/errors/harness/ids/session/storage/tasks/testing/tools/truncate/types），**没有**在 crate 根做 `pub use` 平铺导出（TS 的 index.ts 是平铺再导出）。
- 判定：
  - 【缺失】(低) lib.rs 未在 crate 根平铺 re-export `defineDoc`/`defineTask`/`ROOT_CONVERSATION_ID`/错误类型等基础层符号；需确认是否有 prelude 或用户通过模块路径访问（modules 本身齐全）。Rust 模块风格下可部分视为命名差异/风格差异，但 TS 消费方 `import { defineDoc } from "..."` 在 Rust 无对应根级路径。
  - 【豁免】harness/session/storage/tools/env/testing 等超出本次范围，lib.rs 均有对应 mod 声明，未核对内部。

## errors.ts → errors.rs

- `ReadAfterWrite(method)`：TS 消息 `Tx.${method}() cannot read tables after the first table write` + `this.name`。Rust Display 消息逐字一致，含测试断言。`name` 字段为 JS Error 机制 → 豁免。
- `StorageRejected(message, options?)`：TS 接受 `ErrorOptions`（含 `cause`），Rust 仅保留 message，**丢弃 cause**。判定：豁免（Rust 错误链机制不同；但 `cause` 信息被丢弃可记为轻微逻辑差异——建议在报告列出）。归为「豁免（附注）」。
- `ConversationBusy(conversationId)`：字段、消息 `Conversation ${id} is busy` 一致。
- 结论：errors 1:1 等价（2 处 JS Error 机制豁免，1 处 cause 丢弃附注）。

## ids.ts → ids.rs

- `idFromNumber(value): I`（品牌施加）：TS 是 `value as I` 类型断言；Rust `id_from_number<I: IdBrand>(u64)` 经 trait 构造。等价（需 types.ts 确认 Id 数值类型）。
- `seqFromNumber(value): Seq`：Rust `Seq::new(value)`。等价。
- 【多余】(机制性) 公开 trait `IdBrand` 是上游没有的导出符号（为泛型约束引入，可私有化）；功能性等价，计入多余。
- 【豁免】(已定论，见 types 节) TS 参数类型是 `number`（float64），Rust 是 `u64`：上游各入口（ownerId 等）均要求安全整数，实际使用 <2^53，无语义收窄。

## truncate.ts → truncate.rs

- 常量 `DEFAULT_MAX_LINES=2000`、`DEFAULT_MAX_BYTES=50*1024` ✓。
- `TruncationResult` 11 个字段全部对应 ✓（Rust 附加 derive 为机制，豁免）。
- `TruncationOptions`（maxLines?/maxBytes?）对应 ✓。
- `utf8ByteLength`：TS 有 Buffer 快路径 + 手写 UTF-8 计数；Rust `str::len()`。有效 UTF-8 下等价；JS 孤代理项（lone surrogate）在 Rust 不存在 → 豁免。
- `splitLinesForCounting`：空串/结尾换行处理一致 ✓。
- `formatSize`：
  - 阈值与单位格式一致（0B / xKB / xMB，1 位小数）✓。
  - 【逻辑差异】(低) 舍入模式：TS `toFixed(1)`（V8 对精确 .x5 半数向上，如 `(1.25).toFixed(1)==="1.3"`），Rust `{:.1}` 用 round-half-even（`format!("{:.1}",1.25)==="1.2"`）。在字节数恰为 1024 的 n.25/n.75 边界（如 1280B→1.25KB）输出会差 0.1。非边界值（1024→1.0KB、1536→1.5KB）一致，测试已覆盖。
- `truncateHead(content, options={})` / `truncateHeadOf(prefix, totals, options={})`：
  - TS 的 options 默认参数在 Rust 需要显式传 `TruncationOptions::default()` → 豁免（无默认参数机制，Default derive 等价）。
  - 主逻辑逐行一致：先总行数/总字节双不超限则原样返回（outputBytes=totalBytes）；首行超字节限返回空 + firstLineExceedsLimit；循环 `i<lines.length && i<maxLines`，每行字节 +（i>0 ? 1:0），超限置 bytes 并 break；无字节中断时 `output.length<totalLines ? lines : bytes`；`join("\n")` 重算 outputBytes。✓
  - 【逻辑差异】(低/边界) 空前缀且总量超限时：TS `lines[0]` 为 undefined → `utf8ByteLength(undefined)` 抛 TypeError；Rust `lines.first().unwrap_or("")` 返回 0 字节 → 走正常截断返回空内容不崩溃。属契约违约输入下的健壮性差异，不影响合法输入。
  - 注：TS totals 用 `{lines,bytes}`，Rust 用 `(usize,usize)` 元组 → 命名/结构差异，不算问题。
- 结论：truncate 等价（1 处低危舍入差异、1 处边界健壮性差异、2 处豁免）。

## entries.ts → entries.rs

- `defineEntry(kind)`：TS 运行时校验 `typeof kind !== "string" || kind.length === 0` → TypeError（消息 `Entry kind must be a non-empty string`）；Rust `define_entry(&'static str)` 用 `assert!(!kind.is_empty())`，消息逐字一致。类型保证 string，panic vs TypeError → 豁免。
- 返回值 `{ kind, is }`：TS `is(entry): entry is TypedEntry<D>`（类型守卫）；Rust `DefinedEntry::kind()` 访问器 + `is(Option<&EntryRecord>) -> bool`。类型窄化无法在 trait 表达（文件已注明）→ 豁免；`kind` 属性→方法 → 命名差异。
- 六个内置常量字符串逐一核对：`pi.user` / `pi.assistant` / `pi.system` / `pi.tool-result` / `pi.reset` / `pi.compaction` ✓（含测试断言）。
- 泛型 `D`：TS `D extends JsonValue = never`；Rust `DefinedEntry<D>` 默认 `()`。类型级差异，runtime 等价。
- `ToolResultData{diagnostics}`、`CompactionData{reason}` ✓。TS 的 `ToolDiagnostic{severity:"info"|"warn"|"error", message, code?}` 与 `CompactionReason="manual"|"threshold"|"overflow"` 已对照 harness/types.ts 原文核实：Rust `DiagnosticSeverity`(serde lowercase)、`CompactionReason`(serde lowercase) 取值/序列化一致 ✓。
- 【多余】(低) `ToolDiagnostic`/`DiagnosticSeverity`/`CompactionReason` 在 entries.rs **重复定义**了一份（harness/types.rs 也有一份，camelCase+lowercase serde）。两份形状一致但属内部重复，建议 entries 改为引用 harness 定义。
- 结论：entries 逻辑等价；1 处多余（重复类型定义）、若干豁免。

## tasks.ts → tasks.rs

- TS `defineTask<I,S,R,H>(definition: TaskDefinition<I,S,R,H>): Task` 返回 `{ definition }`；Rust `define_task(Arc<dyn TaskDefinitionSpec>) -> Task`，`Task::new` 存 Arc 并提供 `definition()`。泛型擦除 + trait object → 豁免（机制）。
- `Task` 结构 ✓（types.rs：`{definition: Arc<dyn TaskDefinitionSpec>}`）。
- `TaskDefinitionSpec` 对照 TS `TaskDefinition`：name/version/initial(input)/phases/abort/migrate?(input,checkpoint,fromVersion)/hooks? 全部有对应（phases 拆为 `phases()` 键集 + `run_phase(phase,…)`；`has_migrate()` 对应 `migrate === undefined` 判断）✓。TS 的 `abort`/`phases` 为必填，Rust trait 默认实现返回错误 → 豁免（机制）。
- 结论：tasks 等价（机制性适配，无逻辑差异）。

## documents.ts → documents.rs

- `defineDoc`/`defineDocFamily`：先 `validateDefinition` 再返回 token ✓。TS 校验 `!Number.isSafeInteger(version) || version < 1`；Rust `version() < 1`（u32 类型保证整数与安全范围）→ 豁免。消息 `Document {kind} version must be a positive integer` 逐字一致 ✓。TS 的 8 个 token 类型重载在 Rust 由 `DocDefinitionSpec`（含 `semantics()`/`is_family()`）统一 → 豁免（类型级重载消失，运行时行为一致）。
- `AnyDocDefinition`：Rust `DocDefinitionSpec` trait 覆盖全部字段（kind/version/semantics/family/initial(seed?)/migrate?/has_migrate/checkpointWhen?）✓。
- `AnyDocToken`：TS 为擦除联合（工具对该行做了内容遮蔽，未能读全原文，推断为 DocToken|DocFamilyToken 联合）；Rust 用 `AnyDocToken` trait 统一两种 token ✓。
- `ResolvedAddress`：address/id/nextArgument ✓。
- `resolveAddress(definition, args[])`：TS 按 scope 消费 owner（conversation/task）+ family 时消费 key；Rust `resolve_address(definition, owner: Option<u64>, key: Option<String>)` 显式参数（文件注明 rest-args 不可行），next_argument 计数逐分支一致 ✓。TS 对 owner 做 `typeof number && isSafeInteger` 运行时校验（TypeError `Document {kind} requires a {scope} ID`）；Rust `require_owner` 在 None 时报 `DocError::MissingOwner`，消息逐字一致（u64 类型保证整数）→ 豁免。family 且 key 缺失：TS `key===undefined`→地址不带 key；Rust `key: None`→`address.key=None` ✓。
- `addressId(address)`：`JSON.stringify([kind, scopeKind, owner|null, key??null])` 与 serde 数组逐项一致（测试断言 `["demo.conversation","conversation",7,"alpha"]`、`["demo.session","session",null,null]`）✓。
- `documentCreate`：session/task 无 history/fork；conversation 带 `definition.history!`/`definition.fork!`；Rust 从 `semantics()` 取 `(Some(history), Some(fork))`，其余为 None（serde skip None → 键缺席）✓。
- `checkRecordScope`：分支与比较逻辑一致（session/task 只看 scope 种类；conversation 另比 history/fork），错误消息逐字一致 ✓。TS 抛 TypeError，Rust `DocError::ScopeMismatch` → 豁免（错误类型收敛为枚举，消息保留）。
- `checkRecordVersion`：`version > def.version` → `has newer version {v} than {dv}`；`version < def.version && !has_migrate` → `requires migration from version {v}` ✓ 消息逐字一致（测试覆盖两分支）。
- `materializeDocument`/`materializeDocumentValue`：checkRecordScope→checkRecordVersion→同版本直返→否则 migrate；TS 用 `copyJson(migrate!(…))` 深拷贝，Rust migrate 返回 owned JsonObject → 豁免。Rust 在 migrate 意外缺席时走防御性 NeedsMigration（TS 依赖 `!` 断言保证不可达）→ 等价。
- 结论：documents 等价（机制适配：DocError 枚举、HasDocumentScope/AnyDocToken trait、显式参数替代重载；无逻辑差异）。

## types.ts → types.rs（契约面，最大）

- **品牌 ID/Seq**：TS `Id<Kind,Type>=number&brand`、`Seq=number&brand`；Rust `Id<Kind,T>`（newtype u64 + PhantomData）、`Seq(u64)`。serde 均序列化为裸数字 ✓（有测试）。五个 ID 别名（ConversationId/EntryId/TaskId<Result>/SubmissionId/DocumentId）✓。`ROOT_CONVERSATION_ID = 1` ✓。TS `number`（float64）→ u64 是收窄：上游各入口（`ownerId`、`defineEntry` 之外）均要求安全整数，实际使用 <2^53 → 豁免（附注：>2^53 或小数 ID 在 Rust 无表达）。`Seq.next()` 为上游没有的辅助 → 多余（低，无害）。
- **文档定义侧**：`JsonObject` ✓；`CheckpointInfo{deltasSinceBase}` ✓；`CommonDocDefinition`/`DocDefinition`/`DocFamilyDefinition` → `DocDefinitionSpec` trait ✓（initial 统一 `Option<&JsonValue>` seed，checkpoint_when 签名一致）；`DocumentSemantics`（session/Conversation{history,fork}/task）✓；`LatestConversationSemantics`/`RewindableConversationSemantics` 独立别名未保留（并入 Conversation{history,fork}）→ 豁免（类型级）。`DocToken`/`DocFamilyToken` 泛型参数 T/D/I 全部擦除 → 豁免（类型级）。8 个 *DocToken/*DocFamilyToken 别名未保留 → 豁免（类型级）。
- **Entry 侧**：`EntryRecord` 8 字段全 ✓；`EntryDraft`（head 支持 `EntryId | "self"` → `EntryHead` 枚举）✓；`TypedEntry<D>`（Rust 为 `{record, data:D}` 嵌套结构 + `EntryRecord::typed()` 解析，vs TS 平铺 Omit<EntryRecord,"data">&{data:D}）→ 结构差异（命名差异，字段全可达）；`Entry<D>` → `EntryToken` trait（kind+is）✓；【缺失】`TypedEntryDraft<D>` 类型未落地。
- **Submission 侧**：`SubmissionRecord` 全集（input: queued/placed{entry}/done{entry,answer}/unanswered{entry?,reason,detail?}；write: queued/done{entry}/unanswered{reason,detail?}）字段逐一对上（identity + InputSubmissionStatus/WriteSubmissionStatus）✓；`SubmissionCreate` ✓；`SubmissionSettlement` ✓；`SubmissionStatus` 4 值 ✓。
- **Task 侧**：`TaskOwnership` ✓（Conversation/Task{task_id}）；`JoinPolicy` ✓（serde camelCase → failFast/allSettled）；`TaskOptions` ✓（ownership 必填、conversation_id?、background?）；`TaskDefinition`/`Task` → `TaskDefinitionSpec`/`Task` ✓（见 tasks 节）；`TaskState` 5 态 ✓（pending/running/waiting{checkpoint,on,policy}/completing{outcome}/terminal{outcome}，status() 判别符）；`TaskOutcome` 5 分支字段 ✓（completed{result}/failed{error,result?}/aborted{reason?,result?}/orphaned{reason}/faulted{error}）；`TaskOutcomeError` ✓；`TaskRecord` 全字段 ✓（含 started_at/ended_at/memos：TS 用判别联合「可运行态才有 memos」，Rust 用 Option 约定，文档已注明 → 豁免）。`RunningTask`/`NextTaskState`/`HookRunner`/`TaskRuntime` 由 harness/types.rs 承载（组织差异）：已核实存在且字段齐（TaskRuntime 21 个成员：task_id/conversation_id/signal/registry/agent/settings/models/env/hooks/commit/memo/memo_or/get_task/wait_for_task/outcomes/conversation/entry/context_view/now/report/sleep ✓）。
- **Conversation/存储**：`ConversationRecord` ✓（parent{conversation_id,at}/owner{conversation_id,task_id}）；`ConversationOwnership` ✓；`ContextEdit` ✓。
- **查询/分页**：`Page<T,C>` ✓；`Cursor`=JsonObject ✓；`ScanOrder` ✓（serde lowercase）；`ConversationQuery`/`EntryQuery`/`TaskQuery`/`SubmissionQuery` 字段全 ✓（TaskQuery.status 用 TaskStatus 5 值 ✓）。
- **文档读**：`DocumentPoint`（Seq|"current"）✓（serde camelCase：Current→"current" ✓；At(Seq)→{"at":N} vs TS 裸数字 → 形态差异，见下）；`DocumentAddress` ✓；`DocumentQuery` ✓；`DocumentContent`（Base{version,value}/Delta{version,ops}）✓ 字段；`DocumentCopySource` ✓；`StoredDocument` ✓（含 deltas_since_base）。
- **写入/发布**：`StorageWrite` 8 变体 ✓（conversation/entry/task/submission/document.create{document.create 的 content 用 DocumentBase}/document.copy/document.change/document.retire）；`TableCommitChange` ✓；`DocumentCommitChange` ✓（Document{record, conversationId→Option, version→Option, value→Option, ops}/DocumentCopy{record,conversationId,source}）；`CommitChange` ✓；`CommitPublication{seq, changes}` ✓；`as_table_change()` 辅助（上游无）→ 多余（机制辅助，低）。
- **Tx 契约**：16 个命名方法逐一核对 ✓（conversation/entry/task/scanConversations/scanEntries/latestHeadMarker/scanTasks/submissionByRequest/createConversation/forkConversation/appendEntry/createTask/createSubmission/settleSubmission(同步)/placeSubmission(同步)/doc/retireDoc）。注意：TS 的 Tx **没有** scanSubmissions，Rust 同样没有 ✓。doc/retireDoc 的 6 组重载由 `DocAccess{owner,key}` 统一（scope 由 token 语义解释）→ 豁免。`latestHeadMarker` 返回 `(EntryRecord, EntryId)` 元组 vs TS 交叉类型 → 结构差异。TS 的 `Draft<T>`（chord Proxy）→ Rust `Draft`（Arc<Mutex<Option<Change>>>，take/set/delete/append/truncate/splice/move_items）→ 豁免（机制，文件已注明）。【缺失】TS Tx 的 `entry(token, id)` 与 `appendEntry(token, id, TypedEntryDraft)` 类型化重载在 Rust 未提供（可用 EntryToken::is + typed() 组合，但未提供便捷面）。
- **Storage 契约（17 方法）**：全部核对 ✓ —— commit/mintId/conversation/scanConversations/entry(×2 重载→Rust 拆 entry+entry_in_conversation)/findLatestHeadMarker/scanEntries/task/scanTasks/submission/scanSubmissions/submissionByRequest/findDocument/document/scanDocuments/close。返回值 `undefined` → `Result<Option<..>, StorageError>`（StorageError{Rejected(StorageRejected),Closed,Message}）→ 豁免（错误通道机制；TS 以 StorageRejected 拒绝，Rust 保留该错误为变体）。`entry` 返回 `(EntryRecord, Seq)` 元组 vs `{entry, commitSeq}` → 结构差异。`mintId` 返回裸 u64，品牌由调用方经 ids.rs 施加 → 豁免。
- **观察/会话**：`WatchEnd` ✓（Reason×4 + ListenerError(String vs Error 对象)）；`WatchHandle` ✓（value/start/stop/closed；泛型 T 退化为 JsonValue）；`DocumentReader`/`DocumentObserver`/`Session`/`DocumentState`/`DocumentWatch` 由 session 模块承载（session/mod.rs 确认存在与重导出）→ 组织差异，会话层超出本次范围未逐方法核对。
- 【逻辑差异】(中危，范围外依赖) **serde JSON 形态与上游持久化 JSON 不一致**：多种契约类型的 derive 缺 tag/rename 属性 —— TaskState/TaskOutcome/SubmissionRecord/SubmissionSettlement/ContextEdit/StorageWrite/CommitChange/DocumentCommitChange 为 externally-tagged（TS 为 {status,…}/{type:…, value:…} 扁平判别）；ConversationHistory/Fork（"Latest"/"Current" vs "latest"/"current"）、TaskOwnership/ConversationOwnership（"Conversation" vs {kind:…}）、DocumentScope（"Session" vs {kind:"session"}）、EntryHead（"SelfEntry" vs "self"）、DocumentContent（{"Base":…} vs {kind:"base",…}）、DocumentPoint::At（{"at":N} vs N）均与 TS 形态不同；字段名统一 snake_case vs TS camelCase。若 jsonl/sqlite 后端直接 serde 落盘则与上游存储格式不兼容；storage 层在范围外，未核实是否有规范化层。
- 【逻辑差异】(低) HookRunner：TS `each(name, invoke)` 自带「普通 throw 上报并继续下一处理器、信号后错误传播」分发语义；Rust `HookRunner` 只交 `handlers()`，分发/合成转移到各内置任务（harness 范围，未核实语义是否完整保留）。
- 结论：types 契约面覆盖完整（唯一缺失为 TypedEntryDraft 与 Tx 的类型化 entry/appendEntry 重载）；主要风险在 serde 持久化形态与上游 JSON 的差异。

---

## 汇总

- 缺失：5（均为类型级/便捷面，无核心逻辑缺失）
  1. lib.rs 未在 crate 根平铺再导出基础层符号（index.ts 平铺导出 defineDoc/defineDocFamily/defineTask/ROOT_CONVERSATION_ID/errors/全部类型）
  2. Tx.entry(token, id) 类型化重载缺失（仅 entry(id)）
  3. Tx.appendEntry(token, conversationId, TypedEntryDraft<D>) 类型化重载缺失
  4. TaskRuntime.entry(token, id) 类型化重载缺失（harness/types.rs）
  5. TypedEntryDraft<D> 类型未落地
- 多余：3（均为机制性，低影响）
  1. ids.rs 公开 trait `IdBrand`
  2. types.rs `Seq::next()` 辅助
  3. entries.rs 重复定义 ToolDiagnostic/DiagnosticSeverity/CompactionReason（与 harness/types.rs 重复）
- 逻辑差异：5
  1. truncate.formatSize 舍入模式（toFixed(1) vs {:.1}，.x5 边界差 0.1）
  2. truncateHeadOf 空前缀+总量超限：TS 抛 TypeError，Rust 返回空结果（契约违约输入）
  3. serde JSON 形态与上游持久化 JSON 不一致（external-tag/snake_case/枚举字符串 vs 上游扁平 camelCase；storage 层未核实是否规范化）
  4. HookRunner 分发语义（each 的 report-and-continue）转移至内置任务，未核实保留
  5. errors.StorageRejected 丢弃 cause（ErrorOptions）
- 命名差异（不算问题）：kind 属性→kind() 方法、元组替代对象（(EntryRecord,Seq)、ResolvedAddress.next_argument 等）、TypedEntry 嵌套 {record,data}、latestHeadMarker 元组、TaskRuntime.context→context_view、snapshot_as_of 参数 owner 等。
- 豁免（语言机制等价，已注明）：类型守卫→bool+typed()、重载→显式参数/拆分方法（entry_in_conversation、DocAccess、memo/memo_or）、throw→Result/enum 错误（DocError/SessionError/StorageError，消息逐字保留）、泛型擦除（JsonValue/Arc<dyn>）、Proxy Draft→显式方法 Draft、JS Error name/cause 机制、number→u64（上游全链路要求安全整数）、const assert vs TypeError、类型级别名（8 个 token 别名、Latest/RewindableSemantics）擦除。

注：read 工具对个别行做了内容遮蔽（`AnyDocToken` 类型别名原文、`DocToken::new` 函数体、`token:` 参数类型等），均不影响方法级判定（有测试与调用面佐证）。
