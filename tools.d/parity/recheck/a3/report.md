# chord 子集 1:1 复刻审计（recheck a3）

- 基线：`earendil-works/pi` v1.1.0（commit abe508e1b）
- TS 源：`/Users/pokersu/Projects/pi_rs/upstream/packages/chord/src/`
- Rust 源：`/Users/pokersu/Projects/pi_rs/crates/pi-durable/src/chord/`
- 范围：delta/index.ts + delta/apply-immutable-trusted.ts → delta.rs；delta/tracker.ts → tracker.rs；json.ts → json.rs；context/index.ts → context.rs；services/state.ts + state-internals.ts → state.rs；services/state-codec.ts → state_codec.rs。
- 方法：逐文件提取导出符号与方法逐一比对（本报告为独立核对，不沿用既有结论）。
- 约束说明：本次会话 bash/exec 被共享 checkout 写锁禁用，全程用只读 `read` 核对源码，未运行任何测试/构建；结论为纯代码比对。
- 判定分类：`缺失` / `多余` / `逻辑差异` / `命名差异`（不算问题）/ `豁免`（语言机制等价差异）。

---

## 1. delta/index.ts + apply-immutable-trusted.ts → delta.rs

### 1.1 符号对照

| TS | Rust | 判定 |
|---|---|---|
| `Seg`/`Path`/`NonEmptyPath`/`PathRef` | `PathSegment`(Key(String)/Index(usize))、`Path=Vec<PathSegment>`；NonEmpty 由运行时 `EmptyPath` 错误表达 | 命名差异 |
| `Op` 7 变体元组 | `Op` enum 7 变体（Replace/Set/Delete/Append/Truncate/Splice/Move） | 等价 |
| `WireOp` 元组 | `WireOp = JsonValue`（松类型） | 命名/类型差异 |
| `isReplace` / `isBase` | **缺失**（isBase 逻辑内联在 `state.rs::ReplicatedStateReplica::hydrate` 的 `matches!(ops.first(), Some(Op::Replace(_)))`；isReplace 无对应） | 缺失 |
| `overlap(a,b,scan,probe=64,maxCandidates=8)` | `overlap(a,b,scan,probe,max_candidates)`（无默认值） | 逻辑差异（默认值缺失，见 1.3-D6） |
| `RESERVED_SEGMENTS`/`UnsafePathError`/`PathError` 公开符号 | **缺失**；由 `DeltaError::{UnsafePath,Path}` 与私有 `assert_safe_path`（硬编码 `__proto__/constructor/prototype`）承担 | 缺失（语义承载一致） |
| `assertValidOp` / `assertValidWireOp` / `assertSafePath` | **未移植**（模块文档自述）；仅解码 `#` 定义时调 `assert_safe_path` | 缺失（后果见 1.3-D2/D3） |
| `apply` / `applyImmutable` / `applyImmutableBatches` | `apply` / `apply_in_place` / `apply_immutable` / `apply_immutable_batches` | 见 1.3 |
| `encoder()/Encoder`、`decoder()/Decoder` | 同名 | 见 1.2 |
| `applyImmutableTrusted` | 未单独移植（语义并入 `apply_immutable`） | 豁免（模块文档自述；`copy_along` 每次沿路径重建容器，最终值一致，不改输入） |

### 1.2 wire 编解码（Encoder/Decoder）逐点核对

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| 跨批次 interning 状态（seen/ids/nextId 存活于 encode 调用之间） | ✓ | ✓（`Encoder` 结构体字段） | 一致 |
| 每批 `previous` 重置 | ✓ | ✓（`encode` 内局部变量） | 一致 |
| `r`：推送后清空 seen/ids/nextId/previous | ✓ | ✓ | 一致 |
| 同路径连发 → short form（s/d/a/t/p/m arity 省略） | ✓ | ✓ | 一致 |
| 第二次使用定义 id（`["#",id,path]` 先定义后引用） | ✓ | ✓ | 一致 |
| 第三次及以后引用 id | ✓ | ✓ | 一致 |
| `pathKey` = JSON 字符串化路径 | ✓ | `path_key` = `serde_json::to_string(path)`（`["a",0]` 形态一致） | 一致（注：TS `Seg` 可为小数/负数、serde 不可，但合法 op 中不会出现） |
| decode：`#` 定义（含 assertSafePath） | ✓ | ✓（`assert_safe_path`） | 一致 |
| decode：`r` 清空 paths + previous | ✓ | ✓ | 一致 |
| short 判定（d=1、p=4、其余=2） | ✓ | ✓ | 一致 |
| short 且 previous 无 → 抛错 | PathError([]) | DeltaError::Path | 错误类型差异（豁免） |
| 数字 ref 未定义 → 抛错 | PathError(ref)（携带 ref） | DeltaError::Path（不携带 ref） | 错误载荷差异（豁免，信息丢失） |
| s/d/a/t 空路径拒绝 | PathError(path) | DeltaError::EmptyPath | 错误类型差异（豁免） |
| **arity 严格性** | `assertValidWireOp` 严格拒绝多余元素（如 `["s",path,v,x]`、`["#",id,path,x]`） | Rust **忽略多余元素**（仅取所需下标） | **逻辑差异 D3** |
| **m 排列双射校验** | `assertPermutation`：非负整数、唯一、覆盖 0..n-1 | Rust 仅要求元素为 u64（无范围/双射校验） | **逻辑差异 D3** |
| inline 路径 assertSafePath | ✓（okRef） | ✗（`json_to_path` 后无保留键检查） | 豁免（serde_json Map 无原型链，文档自述） |

### 1.3 apply 系列逐点核对

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| `r` 整值替换 | adopt 不复制 | `*root = value.clone()` | 豁免（所有权机制，文档自述） |
| `p` splice：index 钳制到 len、deleteCount 钳制到 len-index | JS 原生 splice 语义 | `start=index.min(len)`、`removed=delete.min(len-start)`、`splice` | 一致 |
| `p` 插入按 10,000 分块 | 调用栈限制 | 无分块 | 豁免 |
| `m` 长度不等 → 错 | PathError | DeltaError::Path | 一致 |
| `m` 越界/重复排列 | `assertValidOp` 前置拒绝（不落盘） | 越界→循环中途 Err（**部分写入后报错**）；重复→**静默接受**（`get(*source)` 越界才报） | **逻辑差异 D2**（根因=缺失 M2） |
| `s` 对象键（含新建键） | defineProperty 写入 | `insert` | 一致 |
| **`s` 数组 index==length（追加一格）** | `assertIndexInRange` 允许 `index <= length` → 追加 | `get_mut(*index)` 不存在 → `DeltaError::Path` 拒绝 | **逻辑差异 D1**（Rust 测试仅覆盖 index>length） |
| `s` 数组 index>length | UnsafePathError | DeltaError::Path | 错误类型差异（豁免） |
| `d` 对象键（可不存在） | `delete`（无错） | `map.remove`（无错） | 一致 |
| `d` 数组（key>=len → 错；否则 splice 删除） | ✓ | ✓（index>=len → Path） | 一致 |
| `a` 目标非字符串/键不存在 → 错 | PathError | DeltaError::Path | 一致 |
| `t` = JS `slice(count)`（UTF-16 码元） | ✓ | `slice_utf16_from`（encode_utf16） | 基本一致 |
| **`t` 在代理对中间截断** | 保留孤立代理（序列化为 `\udXXX`） | `String::from_utf16_lossy` 替换为 `U+FFFD` | **逻辑差异 D4**（`"😀abc".slice(1)`：TS→`"\uDE00abc"`，Rust→`"\uFFFDabc"`；现有测试只覆盖对齐边界 2/3） |
| 数字段作用对象（path `[5]` 于 `{"5":...}`） | hasOwn 字符串化可行走 | `Index` 对非数组 → UnsafePath | **逻辑差异 D5**（退化路径；tracker 不会产生此类路径） |
| 空路径 `s/d/a/t` | assertValidOp 抛 "path is empty" | EmptyPath | 一致（错误类型差异豁免） |
| 输入 target 为 `undefined` 且 ops 为空 | 返回 `undefined` | 返回 `JsonValue::Null` | 豁免（JSON 无 undefined） |
| applyImmutable（WeakSet 复用已复制容器） | 跨批复用 | 每次沿路径重建（文档自述语义等价） | 豁免 |
| 错误机制 | throw | Result<_, DeltaError> | 豁免（文档自述） |
| RESERVED_SEGMENTS 拒绝（apply 路径） | assertSafePath 全量检查 | 无检查（Map 无原型链，语义免检） | 豁免（文档自述） |

---

## 2. delta/tracker.ts → tracker.rs

### 2.1 接口对照（Proxy → 显式方法，任务声明等价映射）

| TS（Proxy 语义） | Rust 显式方法 | 判定 |
|---|---|---|
| `state[key] = value` | `set(path, value)` | 见 2.2 |
| `delete state[key]` | `delete(path)` | 见 2.2 |
| `state[key] += text` / 前缀压缩 | `append(path, text)` | 见 2.2 |
| 字符串截断（overlap 压缩 t+a） | `truncate(path, count)` | 见 2.2 |
| push/pop/shift/unshift/splice | `splice(path, index, delete_count, items)` | 值等价 |
| reverse/sort/copyWithin/fill | `move_items(path, permutation)` / `splice` | 值等价（方法缺失见 M5） |
| `arr.length = n` | `splice` 表达 | 值等价 |

### 2.2 关键语义核对

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| 相同值写入抑制（`current === stored && !isContainer` → 不记 op） | ✓ | ✗（`record` 无条件记录 Set） | **逻辑差异 D7**（TS 相同值变更 → ops=[] → 不发布；Rust → 1 条 Set → 会发布） |
| 字符串 op 压缩：前缀 → `a`；overlap(65_536, 默认 probe=64/maxCandidates=8) → `t`(+`a`) | ✓（emitChangedValue） | ✗（每次编辑一条原始 op，压缩由调用方选择 append/truncate 表达） | **逻辑差异 D7**（最终值等价，op 序列不同；模块文档自述「每次编辑一个 Op」） |
| 删除后重加（readd → 先 `d` 后 `s`，保序） | ✓ | ✗（调用方需自行 delete+set） | **逻辑差异 D7**（值等价） |
| 数组结构归一化（removeRuns `p`、排列 `m`、insertRuns `p`） | ✓ | ✗（每次编辑一条 splice/move） | **逻辑差异 D7**（值等价） |
| dense region（≥256 覆盖 ≥50% → 整段 `p`） | ✓ | ✗ | **逻辑差异 D7**（值等价，文档自述未移植 piece-tree/启发式） |
| `MAX_DELTA_OPERATIONS=4096` 超限 → 单条 `["r", clone]` | 排放中检查 `operations.length > MAX` | prepare 时 `ops.len() > 4096` → Replace(current) | 一致（阈值 4096 与 `>` 比较相同；常量命名一致） |
| `Prepared.value === apply(base, ops)` 不变式 | ✓ | ✓（测试断言） | 一致 |
| `prepare()` 无编辑 → ops=[]、value=base（同引用） | ✓ | ✓（ops 空；value 为 base 的 clone，值相等） | 一致（引用身份差异豁免） |
| `prepareReplace` 等值 → ops=[]（replacementNoop，equalTrustedJson 深比较） | ✓ | ✓（`value == self.value` 深比较） | 一致 |
| **adopt 校验链**：owner/consumed/aborted/stale/baseRevision/`#value === prepared.base` 身份 → 抛 "Prepared change is stale" 等 | ✓ | ✗（`adopt` 无任何校验，直接 `value=prepared.value; revision+=1`） | **逻辑差异 D9** |
| adopt 后并发 draft 失效（#invalidate → stale + releaseOverlayReferences） | ✓ | ✗（无多 draft 失效机制；Change 为所有权模型） | **缺失 M7**（chord 内部用法（state.rs 立即 adopt）不受影响，但 Tracker API 语义不等价） |
| `Prepared.abort()`（置 aborted，adopt 拒绝） | ✓ | ✗（Rust Prepared 无 abort；Drop 即弃） | **缺失 M5**（Drop 等价于「放弃」，但无 abort 状态语义） |
| 编辑错误时机 | Proxy 只记录，错误推迟到 apply/prepare 阶段 | `record` 立即 `apply_in_place` 校验并 Err（文档自述） | **逻辑差异 D8**（对最终结果无影响，错误时机不同） |
| `state[key]=undefined` → 删除 | ✓ | ✗（无 undefined；调用方用 delete） | 豁免（JSON 无 undefined） |
| 数组不可有洞（delete 数组元素/越界写 → TypeError "Overlay arrays cannot contain holes"） | ✓ | `delete` 数组元素 = Op::Delete 删除元素（成功）；越界 → DeltaError | 映射差异（豁免：显式方法下调用方语义自选） |
| `sort` 默认比较器 = String() 字典序；结构变化后 deduplicate | ✓ | ✗（无 sort 方法；调用方需自算排列） | **缺失 M5**（值等价但实现责任移交调用方） |
| `fill`/`copyWithin` 返回 proxy；clonePlacement 深拷贝放置 | ✓ | ✗（无方法） | **缺失 M5**（可用 splice+克隆表达） |
| revision 从 0 起、仅 adopt 递增 | ✓ | ✓ | 一致 |
| track() 取得独占不可变 JSON | ✓ | ✓ | 一致 |

---

## 3. json.ts → json.rs

| 符号 | TS | Rust | 判定 |
|---|---|---|---|
| `copyJson(value, options?)` | 严格校验 + 深拷贝 + 循环检测 + 稀疏数组/非枚举/非有限数/符号拒绝；`omitUndefinedProperties` 跳过 undefined 对象键 | `copy_json(&JsonValue) = clone()`（类型不变式保证严格 JSON） | 豁免（文档自述） |
| `omitUndefinedProperties` | 可选 | n/a（无 undefined） | 豁免 |
| **`isJsonValue(value)`** | ✓ | **缺失** | **缺失 M4** |

---

## 4. context/index.ts → context.rs（仅 abortSignal 部分，按任务范围）

| 符号 | TS | Rust | 判定 |
|---|---|---|---|
| `BACKGROUND_CONTEXT` | `"[Context BACKGROUND_CONTEXT]"` | 同名字符串，LazyLock<Arc> | 一致 |
| `TODO_CONTEXT` | `"[Context TODO_CONTEXT]"` | 同名字符串 | 一致 |
| `withAbortSignal` | `AbortSignal.any([parent, signal])` **同步**传播 | `AbortSignal::any` 基于 `tokio::spawn`，**异步**传播（测试注释自认：abort 后立即读 `aborted()` 可能仍为 false） | **逻辑差异 D15** |
| `withoutAbortSignal` | ✓ | ✓ | 一致 |
| `withCancel` | `cancel(reason)` → reason 成为 signal.reason | `cancel()` 无 reason（`AbortError` 为单元结构，reason 全丢） | **逻辑差异 D17** |
| `awaitWithContext`：无 signal → 原样返回 promise；已 abort → reject `abortError(signal)`（reason 为 Error 则用该 Error，否则 `DOMException("AbortError")`）；等待中 abort 只拒绝等待者，**底层 promise 继续运行** | 无 signal → `Ok(future.await)`；已 abort → `Err(AbortError)`；abort 时 `tokio::select!` **drop future → 取消底层工作**；错误无 reason/文案 | **逻辑差异 D16** |
| `ContextValue.toString()` = `${parent}.WithValue(${description ?? "anonymous"})` | `AbortSignalContext::describe()` 直接返回 `parent.describe()`，无 `.WithValue(...)` 后缀 | **逻辑差异 D17** |
| `ContextKey`/`createContextKey`/`withContextValue`/`value<T>` | 未移植（模块文档自述；任务范围仅 abortSignal） | 豁免（范围外） |

---

## 5. services/state.ts + state-internals.ts → state.rs

### 5.1 StateSubscriber / 投递队列

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| push 上限 100；满时丢弃，未开始则保留首条 hydration | ✓ | ✓（`MAX_PENDING_DELIVERIES=100`） | 一致 |
| close 清队列；clear 仅清队列 | ✓ | ✓ | 一致 |
| 监听器失败隔离、继续投递 | sync throw → report 继续；Promise reject → report+resume | catch_unwind → `report_error(panic_message)` 继续 | 一致（错误形态豁免） |
| **同一订阅者回调严格串行**（`#running` 守卫；sync 回调内联执行） | ✓ | ✗：`spawn_drain` 每次投递 spawn 一个任务，drain 无 running 守卫 → **同一订阅者的两个回调可能并发执行**（任务 A await 回调1 时任务 B 已 pop 帧2） | **逻辑差异 D10**（模块文档「回调串行执行」声明与实际不符） |
| 订阅建立即投递 hydrate（sync 回调在 subscribe 返回前执行） | ✓ | spawn_drain 异步投递 | 豁免（异步运行时） |
| 报告回调自身抛错 → reportErrorAsync | ✓（try/catch） | ✗（report_error 直接调用，panic 会传播） | 微小差异（并入 D11 类） |

### 5.2 ReplicatedStatePublisher

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| subscribe 快照 {value, sequence}、hydrate 帧、hydratedSequence 登记 | ✓ | ✓ | 一致 |
| publish：更新 value/sequence → 入队 → reentrancy 短路 → FIFO 排空 | ✓ | ✓ | 一致 |
| `sequence <= hydratedSequence` 跳过 | ✓ | ✓ | 一致 |
| **source listener 错误收集**（catch 每个、publish 返回 errors、change() 经 throwCollectedErrors 抛给调用方） | ✓ | ✗（source listener 直接调用，**panic 无隔离直接传播**；publish 无返回值；`*delivering = false` 无 finally 保护 → panic 后 delivering 恒 true，之后所有 publish 只入队不投递） | **逻辑差异 D11** |
| 订阅者监听错误 | 订阅者自身 reportError | report_error | 一致 |

### 5.3 MutableReplicatedStateImpl

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| change 重入 → throw Error("Replicated state cannot be changed reentrantly from a change callback") | ✓ | panic 同文案 | 一致（throw/panic 豁免） |
| mutate 返回 promise → 吞拒绝 + TypeError("Replicated state change callbacks must be synchronous") | ✓ | FnOnce 无返回（编译期保证） | 豁免 |
| 异常 → change.abort() 后重抛 | ✓ | Change 被丢弃（等价 abort） | 豁免 |
| adopt 后 ops 空 → 不发布 | ✓ | ✓ | 一致 |
| 发布错误抛回 change/replace 调用方 | ✓ | ✗（见 D11） | **逻辑差异 D11** |
| replace 重入守卫 | ✓ | ✓ | 一致 |
| 构造注册 internals {snapshot, subscribeSource} | ✓ | `replicated_state()` 以 `Arc::as_ptr` 为 key 注册 | 豁免（键机制差异） |

### 5.4 AttachedReplicatedState / attach

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| 构造 assertCursor（safe integer） | ✓ | u64 类型保证 | 豁免 |
| 帧游标缺口 → dispose + 上报，错误文案 `"Replicated state source cursor has a gap: expected {expected}, received {frame.cursor}"` | ✓ | ✓（文案一致） | 一致 |
| **onError 默认**：`reportErrorAsync`（queueMicrotask 内 throw） | ✓ | `default_reporter` → `eprintln!` | **逻辑差异 D12** |
| 1 个/多个监听错误 → #report / AggregateError | ✓ | publish 不返回错误（见 D11） | **逻辑差异 D11** |
| **attach 失败补偿**：catch → attachment.dispose()（dispose 失败 → AggregateError("Failed to attach replicated state source")） | ✓ | ✗（attach/new/activate 无 try/dispose 补偿；panic 时 attachment 泄漏） | **逻辑差异 D13** |
| dispose 幂等 | ✓ | ✓（+Drop 时 unregister internals） | 一致 |
| #fail：dispose 抛错 → AggregateError("Replicated state source contract failed") | ✓ | ✗（dispose 直接调用） | 微小健壮性差异（并入 D13 类） |

### 5.5 ReplicatedStateReplica

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| hydrate 校验顺序 | 先 `isBase(ops)`（Error "Replicated state snapshot is not a base operation batch"）→ 再 `JsonRevisionValidator.validate(applyImmutable(undefined, ops))` | 先 `apply_immutable(None, ops)`，后检查 `ops.first()` 为 Replace（`DeltaError::InvalidOp`） | **逻辑差异 D14**（结果一致：失败都 clear+报错；错误文案/类型不同） |
| update 未 hydrate → Error("Replicated state received an update before hydration") | ✓ | DeltaError::InvalidOp（文案缺失） | 错误文案差异（豁免） |
| 序号缺口 → clear + Error("Replicated state update sequence has a gap") | ✓ | clear + DeltaError::InvalidOp（文案缺失） | 错误文案差异（豁免） |
| 每修订 `JsonRevisionValidator` 结构校验（稠密数组/纯对象/有限数/无环） | ✓ | ✗（serde_json::Value 类型不变式保证） | 豁免（mod.rs 自述 revision-validator 未移植） |
| clear/订阅（未 hydrate 不投递）/deliverAll 双阶段 | ✓ | ✓ | 一致 |

### 5.6 state-internals

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| register（WeakMap<object, internals>） | ✓ | 全局 `OnceLock<Mutex<HashMap<usize, Weak>>>` + 自增 key | 豁免（键机制差异） |
| get | ✓ | `get_replicated_state_internals(key: usize)` | 豁免 |
| 显式移除 | 无 | `unregister_replicated_state_internals` | 多余 |

---

## 6. services/state-codec.ts → state_codec.rs

| 行为 | TS | Rust | 判定 |
|---|---|---|---|
| `stateKey` = JSON.stringify([key??null, generation??null, member]) | ✓ | `serde_json::json!([Option<&str>, Option<u64>, member])`（None→null） | 一致 |
| `describeState` 文案（`member` 或 `key@generation.member`） | ✓ | ✓ | 一致 |
| `sameAddress`（left undefined → false） | ✓ | ✓ | 一致 |
| registry：reset/add（重复 → throw `Duplicate service state ...`）/get（缺失 → throw `Unknown service state ...`）/removeInstance | ✓ | panic 同文案 | 一致（throw/panic 豁免） |
| encodeSnapshot：reset → 逐实例 encodeInstance（state 成员 add+encode） | ✓ | ✓ | 一致 |
| encodeUpdate 各分支：state=get+encode；reset=reset+map；replaced=reset+encodeInstance；spawned=encodeInstance（不 reset）；unavailable=reset；closed=removeInstance | ✓ | ✓ 全部分支一致 | 一致 |
| decode 对称（Result 包装 throw） | ✓ | ✓ | 一致 |
| Wire 类型字段（serviceId/mode/instances、kind、type、camelCase） | 未核对（types.ts/wire.ts 不在范围） | serde camelCase/tag | 风险（见第 8 节） |
| wire.ts parse/assert 校验 | 未移植 | serde 反序列化承担 | 豁免（文档自述，范围外文件） |

---

## 7. 判定汇总

### 缺失（6）
1. `isReplace` / `isBase` 导出缺失（isBase 逻辑内联于 ReplicatedStateReplica::hydrate）。
2. `assertValidOp` / `assertValidWireOp` / `assertSafePath`（公开校验层）未移植（文档自述；后果见 D2/D3）。
3. `PathError` / `UnsafePathError` / `RESERVED_SEGMENTS` 公开符号缺失（由 DeltaError + 硬编码 `assert_safe_path` 替代）。
4. `json.ts::isJsonValue` 缺失。
5. `Prepared.abort()` 缺失；Change 无 sort/fill/copyWithin/reverse 显式方法（以 move_items/splice 表达，实现责任移交调用方）。
6. Tracker 并发 draft 失效（`#invalidate`/releaseContext/registry 剪枝）未移植。

### 多余（4）
1. `delta::apply_in_place` 公开导出（TS 无公开对应；上游 `apply` 直接原地）。
2. tracker 辅助 API：`Change::len/is_empty/state`、`Prepared::into_value/into_parts`（Rust 机制辅助，无害）。
3. state 诊断 API：`ReplicatedStatePublisher::subscriber_count`、`AttachedReplicatedState::cursor`（上游无）。
4. `unregister_replicated_state_internals`（上游 WeakMap 无显式移除）。

### 逻辑差异（18）
1. D1 `apply` 数组 `s` 于 index==length：TS 允许（追加一格），Rust 拒绝（`get_mut` 不存在）。
2. D2 `m` 非法排列：TS 前置拒绝；Rust 越界 → 部分写入后 Err、重复 → 静默接受。
3. D3 wire decode arity 宽松：Rust 忽略多余元组元素（TS 严格拒绝）；`m` 排列无双射校验。
4. D4 Truncate 截断代理对中间：TS 保留孤立代理（`\udXXX`），Rust 替换为 `U+FFFD`。
5. D5 数字段作用对象路径（`{"5": ...}`）：TS 可走，Rust 报 UnsafePath。
6. D6 `overlap` 丢失默认参数 probe=64 / maxCandidates=8（调用点必须显式传；未发现仓内调用点）。
7. D7 tracker op 压缩缺失：no-op 抑制、a/t 压缩、readd d+s、dense region、结构归一化（值等价，op 序列不同；相同值写入 TS 不发布、Rust 会发布）。
8. D8 编辑错误时机：Rust 编辑即 `apply_in_place` 校验（TS 延迟到 apply/prepare）。
9. D9 `Tracker::adopt` 无 stale/consumed/aborted/baseRevision 校验（TS 抛 "Prepared change is stale" 等）。
10. D10 订阅者回调「串行执行」保证弱化：多个 spawn_drain 任务可并发执行同一订阅者回调。
11. D11 source listener 错误未隔离（panic 传播）、publish 无错误返回值、`delivering` 无 finally → panic 后投递永久卡死（TS catch 收集 + throwCollectedErrors）。
12. D12 默认 onError 语义不同：TS `reportErrorAsync`（queueMicrotask 抛错）vs Rust `eprintln!`。
13. D13 `attach_replicated_state_source` 无失败补偿（TS catch → dispose + AggregateError）。
14. D14 replica hydrate 校验顺序/错误类型（TS 先 isBase 抛 Error 文案；Rust 先 apply 后 InvalidOp）。
15. D15 `AbortSignal::any` 异步传播（tokio::spawn）vs TS 同步事件监听。
16. D16 `await_with_context` abort 时 drop future（取消底层工作，TS 只取消等待者）；abort reason 丢失。
17. D17 `with_cancel` 无 reason；`AbortSignalContext::describe` 缺 `.WithValue(...)` 后缀。
18. D18（并入 D7）相同值 set 导致空操作变更在 Rust 仍产生 op 并触发发布（TS 完全静默）。

### 豁免（要点）
- Proxy → 显式方法（任务声明的等价映射）；undefined/数组洞等 JS 专有语义（无 JSON 对应物）。
- RESERVED_SEGMENTS：serde_json Map 无原型链，保留键检查仅解码层保留。
- 错误机制 throw ↔ Result/panic；错误类型与载荷差异（如 PathError 携带 ref）。
- `r` adopt-vs-clone 所有权差异；WeakSet 复用 vs 每次重建的 copy-on-write 实现差异。
- `JsonRevisionValidator` 未移植（类型不变式等价）；wire.ts parse/assert 由 serde 承担。
- WeakMap → 全局 HashMap<usize, Weak>；splice 10,000 分块（调用栈限制）；同步 drain vs tokio spawn 投递时机；Drop 即取消。
- `ContextKey/value<T>` 未移植（任务范围仅 abortSignal，文档自述）。

---

## 8. 风险与未验证项

1. 本次会话 bash 被共享 checkout 写锁禁用，**未运行任何 Rust 测试/构建**（delta.rs/tracker.rs/state.rs/context.rs/state_codec.rs 均含测试，但仅代码阅读）。
2. **跨语言 wire 兼容**未端到端验证：types.ts / wire.ts 不在本次范围，Wire 结构的字段名/枚举字面量（serviceId、`"type":"state|reset|..."`、`"kind":"method|state"`）依赖 serde 配置与上游一致，未经双向序列化比对。
3. `overlap` 的 probe/maxCandidates 默认值依赖调用点显式传 64/8；未能在全部仓内调用点确认（只读限制，未穷举搜索）。
4. tracker 的 op 压缩缺失（D7）意味着 Rust 端产生的线上 op 与 TS 端**字节级不同**（仅语义/终值等价）；若存在以 op 序列为判定依据的协议层（如去重、幂等、测试快照），需注意。
5. D10（订阅者回调并发）在低并发/快速回调场景几乎不可观测，但破坏了上游文档承诺的严格串行性。
