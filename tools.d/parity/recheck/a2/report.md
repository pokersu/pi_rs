# Telemetry 模块 1:1 复刻审计报告（recheck a2）

- **基线**：upstream `earendil-works/pi` v1.1.0，HEAD `abe508e1b89912adde45528136c3221eb69acdd7`（经只读 git log 验证，`Release v1.1.0`）。
- **范围**：`upstream/packages/telemetry/src/` → `crates/pi-telemetry/src/`，6 文件。
- **方法**：逐文件提取导出符号 → Rust 侧对应 → 逐方法对比参数/返回值/控制流/边界/错误处理/默认值/常量/字符串字面量。独立核对，不采信既有注释结论。
- **限制**：共享 checkout 下 bash 被拒，无法运行 `cargo test`/`cargo check` 做动态验证；本报告为逐行静态比对结论（见 RISKS）。

## 总览

| TS | Rust | 结论 |
|---|---|---|
| index.ts | lib.rs | 运行时逻辑一致；类型级导出无法移植（豁免） |
| memory.ts | memory.rs | 一致（含被动容错语义）；1 处不可达回退分支缺失（豁免） |
| noop.ts | noop.rs | 一致 |
| testing/types.ts | testing/types.rs | 一致（fixture 具体化、factory 同步化，豁免） |
| testing/index.ts | testing/mod.rs | 一致 |
| testing/conformance.ts | testing/conformance.rs | 6/9 case 保留，3 个 Proxy case 省略（已验证属实）；保留 case 内有 2 处断言弱化 + 1 处时序弱化 |

---

## 1. index.ts → lib.rs

### 导出符号核对

| TS 导出 | Rust | 判定 |
|---|---|---|
| `AttributeValue` | `enum AttributeValue`（String/Number(f64)/Boolean/三数组） | ✓ |
| `SpanAttributes` | `BTreeMap<String, Option<AttributeValue>>`（`None`=undefined） | ✓（键序差异见豁免 D3） |
| `SpanOptions` | struct（name、attributes: Option） | ✓ |
| `SpanStatus` | enum（Ok / Error{error:Option<SpanError>}） | ✓ |
| `SpanError` 分支 `{name,message}` | struct SpanError | ✓ |
| `TelemetryContext` | trait（`start_span` 返回 boxed Future） | ✓（Send/Sync 约束为 Rust 附加，豁免） |
| `TelemetrySpan extends TelemetryContext` | trait `TelemetrySpan` + 擦除的 `start_child_span` | ✓（机制豁免 A1） |
| `NOOP_TELEMETRY_CONTEXT` | pub use noop::NOOP_TELEMETRY_CONTEXT | ✓ |
| `TelemetryAttributeType` | enum | ✓ |
| `TelemetryAttributeMetadata` | struct（description/sensitive/cardinality） | ✓ |
| `TelemetryAttributeDefinition`（判别联合） | struct + `TelemetryAttributeKind`（6 变体，字段一一对应） | ✓ |
| `TelemetryStartAttributeDefinition` | struct {definition, required} | ✓ |
| `TelemetryEventAttributeDefinition` | struct {definition, required} | ✓ |
| `TelemetryEventDefinition` | struct | ✓ |
| `TelemetryParentDefinition`（any/root_or_external/spans） | enum（3 变体） | ✓ |
| `TelemetrySpanDefinition` | struct（description/parents/start_attributes/end_attributes/events/status） | ✓ |
| `status: {default:"ok", errorWhen:string}` | `TelemetrySpanStatus{default: SpanDefaultStatus::Ok, error_when}` | ✓ |
| `TelemetrySchemaDefinition` | struct（version: u32，spans: BTreeMap） | ✓（version: number→u32，整数语义，豁免） |
| `defineTelemetrySchema` | `define_telemetry_schema`（恒等） | ✓（const 类型捕获无法表达，豁免） |
| `createTypedSpanStarter` | `create_typed_span_starter(context: Arc<C>, _schemas)` | ✓（运行时忽略 schemas，一致） |
| `TypedSpanStarter` | struct + `start_span(name, attributes, callback)` | ✓ 运行时一致；类型级重载退化（豁免 A2） |
| 私有 `bindTypedSpanStarter` | `bind_typed_span_starter` | ✓ |
| 类型级导出 15 项 | 无对应 | 缺失-豁免（见缺失表） |
| re-export RecordedTelemetryEvent/Span、InMemoryTelemetryContext | pub use | ✓ |

### 重点差异

- **D1（逻辑差异）同步入场丢失**：TS `bindTypedSpanStarter.startSpan` → `telemetryContext.startSpan({name, attributes}, cb)`，`startSpan` 在**调用当帧同步**调用 callback。Rust `TypedSpanStarter::start_span` / `InMemoryTelemetryContext::start_span` / `NoopTelemetrySpan::start_span` 全部返回 `Box::pin(async move {...})`，callback 延迟到**首次 poll** 才执行。`Fut` 不被 poll 则 callback 永不执行；入场时序可观察差异。TS 契约测试明确断言同步入场（`strictEqual(admitted, true)` 在 await 之前），Rust conformance case 1 相应弱化为 await 后断言（见 §6）。
- **A1（豁免）span 递归启动**：TS `TelemetrySpan` 继承泛型 `startSpan`；Rust trait 对象无法承载泛型方法，拆为 `start_child_span`（callback 返回 `Box<dyn Any+Send>` 擦除）。运行时等价：以该 span 为父递归 `start_in_memory_span`。类型级类型推断（SchemaTelemetrySpan 等）无法表达，已注释声明。
- **A2（豁免）typed starter 回调形参**：TS callback 收 `(span, startChildSpan)` 双参（child starter 绑定到 span）；Rust callback 仅收 `span`，子 span 经 `span.start_child_span` 启动。运行时等价；TS 的按名重载/精确属性校验为编译期机制，Rust 无法移植。

---

## 2. memory.ts → memory.rs

### 逐函数核对

| TS | Rust | 判定 |
|---|---|---|
| `copyAttributeValue`（数组浅拷贝） | `copy_attribute_value`（数组 clone） | ✓ |
| `copyAttributes`（跳过 undefined） | `copy_attributes`（跳过 None） | ✓ |
| `mergeAttributes`（拷贝后覆盖，跳过 undefined） | `merge_attributes` | ✓ |
| `copyStatus`（ok/error±error 三态拷贝） | `copy_status` | ✓ |
| `automaticErrorStatus`（Error→name/message；检查抛错→无细节；非 Error→无细节） | `automatic_error_status`（String/&str payload→name 硬编码 "Error"+message；其他→Error{error:None}） | 豁免 B1 |
| `settleSpan`（settled 短路；failed&&!explicit→自动状态；settled=true；endSequence=nextEndSequence++） | `settle_span`（同序语义；先取值后自增） | ✓ |
| `createSpan`（id=nextSpanId++、parentId、name、copyAttributes、events=[]、status ok、explicitStatus=false、settled=false） | `create_span`（next_span_id 从 1 起、parent_id=父 span id、其余同） | ✓ |
| `startInMemorySpan`（父已 settle→NOOP；createSpan 失败→NOOP；span 三方法 settled 短路+try/catch 被动；callback 同步抛→settle(true,err)+reject；Promise.resolve(result).then 成功 settle(false)/失败 settle(true,err)+rethrow） | `start_in_memory_span`（父已 settle→NOOP.start_span；catch_unwind 包 callback 与 future；Ok→settle(false)；Err(payload)→settle(true,Some(payload))+resume_unwind） | ✓（createSpan 回退分支缺失，豁免 B2） |
| span.startSpan 递归（父=recordedSpan） | `InMemoryTelemetrySpan` 的 `TelemetryContext::start_span` / `start_child_span`（父=Some(index)） | ✓ |
| span.addEvent / setAttributes / setStatus（settled 短路；try/catch 原子被动） | 同序同语义（lock 后短路；纯 owned 数据操作不可失败） | ✓（豁免 B3） |
| `InMemoryTelemetryContext`（spans:[]、nextSpanId:1、nextEndSequence:1；startSpan 父=undefined；getSpans 分离快照） | state Arc<Mutex>、next_span_id=1、next_end_sequence=1；父=None；get_spans 快照拷贝 | ✓ |
| getSpans 的 endSequence 条件展开 | `end_sequence: Option<u64>`（恒有字段） | ✓（豁免 D4） |

### 重点核查：span 生命周期

- `start_span` → 创建/记录 → callback 同步抛错：TS settle(true, err) 后 reject，**settle 先于传播**；Rust catch_unwind → settle(true, payload) → resume_unwind，**顺序一致** ✓。
- 异步拒绝：TS `.then(onFulfilled, onRejected)` 中 settle 后 rethrow；Rust future catch_unwind → settle 后 resume_unwind ✓。
- 成功路径：TS settleSpan(state, span, false)（失败标记 false，不碰 explicit status）；Rust `settle_span(state, index, false, None)` ✓。
- 显式状态不被自动覆盖：`failed && !explicit_status` 条件两版完全一致；conformance case 3 的四个状态断言逐一核对一致 ✓。
- 父已 settle 时子 span：TS 转 NOOP（callback 收到 noop span、不记录、结果保留）；Rust `NOOP_TELEMETRY_CONTEXT.start_span(...).await` 同 ✓（case 6 验证）。
- id/endSequence 起点均为 1，自增顺序一致 ✓。

### 重点核查：InMemoryTelemetryContext 被动容错（记录失败不回滚不抛出）

- TS 三方法 try/catch：copyAttributes/mergeAttributes/copyStatus 在**读取 unreadable Proxy 属性时抛错**，赋值语句位于 try 块末尾，失败时不产生部分写入（原子性由"先算后赋值"保证）。
- Rust：`SpanAttributes` 为 owned `BTreeMap`，copy/merge 为纯 clone 操作**不可失败**，原子性平凡成立；三方法均无 panic 路径（B3 豁免，语言机制等价：TS 防御的失败模式在 Rust 中不存在）。
- TS createSpan 失败→NOOP 回退（回调照常执行、不记录）；Rust `create_span` 无对应 catch_unwind，因 copy 不可失败、分支不可达（B2 豁免，但严格 1:1 上该分支缺失，注明）。

### B1（豁免）自动错误状态映射

TS：`error instanceof Error` → `{name: error.name, message}`；**非 Error 抛出值**（含字符串、undefined）→ `{status:"error"}` 无细节。Rust：panic payload 为 `String`/`&str` → `Error{name:"Error"(硬编码), message}`；其他 → `Error{error:None}`。映射约定为 `panic!("msg")` ↔ `throw new Error("msg")`（默认 name 恰为 "Error"，一致）；但 TS 中 `throw "string"` 会得到无细节状态，而 Rust `panic!("string")` 得到带 message 状态——这是 panic 机制的近似映射，已在代码注释说明，判豁免。

---

## 3. noop.ts → noop.rs

| TS | Rust | 判定 |
|---|---|---|
| `startNoopSpan`（try { Promise.resolve(callback(noopSpan)) } catch → reject） | `NoopTelemetrySpan::start_span`（`async move { callback(Arc::new(NoopTelemetrySpan)).await }`） | ✓（抛错→panic 传播，机制豁免；惰性入场见 D1） |
| `noopTelemetrySpan`（三空方法 + startSpan；Object.freeze） | unit struct，三空方法 + start_span/start_child_span | ✓（ZST 无状态，冻结语义平凡） |
| `NOOP_TELEMETRY_CONTEXT`（共享单例） | `static NOOP_TELEMETRY_CONTEXT` | ✓ |
| 子 span 复用同一 span 对象（TS 测试 `expect(child).toBe(span)`） | `start_child_span` 传新 `NoopTelemetrySpan`（unit struct，身份平凡等价） | ✓ |

---

## 4. testing/types.ts → testing/types.rs

| TS | Rust | 判定 |
|---|---|---|
| `TelemetryAdapterFixture`（context: TelemetryContext 抽象、getSpans async、AsyncDisposable） | struct（`context: Arc<InMemoryTelemetryContext>` 具体化、get_spans async、无 dispose） | 豁免 C1/C2 |
| `TelemetryAdapterFixtureFactory`（`() => Promise<Fixture>`） | `Arc<dyn Fn() -> Fixture + Send + Sync>`（同步） | 豁免 C2 |
| `TelemetryAdapterConformanceCase`（group/name/run(): Promise<void>） | struct（group/name/`run: Box<dyn FnOnce...>`；`pub async fn run(self)`） | D2 逻辑差异（轻微） |

- **C1（豁免）** context 具体化：TS 面向抽象 `TelemetryContext` 契约测试任意 adapter；Rust trait 含泛型方法不可做对象，fixture 固定为参考实现（代码注释已声明）。
- **C2（豁免）** factory 同步化 + AsyncDisposable 省略：InMemory 构造同步、无资源需释放。
- **D2（逻辑差异，轻微）** case 可运行次数：TS `run()` 为方法，可重复调用（每次新 fixture）；Rust `run(self)` 消费 `FnOnce`，编译期限制单次运行。注册到运行器场景下无影响，但严格 1:1 存在差异。

## 5. testing/index.ts → testing/mod.rs

导出 `createTelemetryAdapterConformance` + 三个类型 ✓ 一一对应，无差异。

---

## 6. testing/conformance.ts → testing/conformance.rs

### 辅助函数

| TS | Rust | 判定 |
|---|---|---|
| `createCase`（`await using fixture = await factory(); await test(fixture)`） | `create_case`（创建 fixture、运行 test，无 dispose） | ✓（C2） |
| `findSpan`（ok() 断言 + 相同消息文案） | `find_span`（panic `"Expected recorded span {name}"`） | ✓（消息字面量一致） |
| `rejectsWithSameValue`（await→fail；catch→`strictEqual(error, expected)` **同值断言**） | 内联 `AssertUnwindSafe(...).catch_unwind().await` + `assert!(is_err())` | 内联但断言弱化，见 D3/D4 |
| `unreadable`（Proxy 四 trap 抛错） | 无 | 缺失-豁免（Proxy） |

### Case 契约核对

TS 共 **9** case，Rust **6** case：

| # | TS case（group/name） | Rust | 判定 |
|---|---|---|---|
| 1 | callback lifecycle / admits once synchronously and preserves the result | ✓ 存在 | D1 弱化（详见下） |
| 2 | callback lifecycle / preserves synchronous and asynchronous rejection values | ✓ 存在 | D3 弱化 |
| 3 | status / uses last explicit status without automatic overwrite | ✓ | D4 弱化（状态断言全一致） |
| 4 | recording / merges attributes and records ordered events | ✓ | **逐断言一致**（含 `ignored: undefined`→`None`、覆盖序、事件序、最终 `{start,overwrite:"end",count:1}`） |
| 5 | recording / ignores failed attribute calls atomically（用 `unreadable(["value"])`） | ✗ | 缺失-豁免（Proxy） |
| 6 | recording / makes calls after settlement inert | ✓ | **逐断言一致**（late 调用全部 inert、late-child 走 NOOP 返回 7、spans.len()==1、attributes/events/status 不变） |
| 7 | parentage / records nested and concurrent child relationships | ✓ | **逐断言一致**（parentId 关系、三 endSequence 均存在、second<first<parent 严格序，oneshot 门控等价 firstGate） |
| 8 | passivity / suppresses unreadable telemetry payload failures（unreadable options/attributes/status） | ✗ | 缺失-豁免（Proxy） |
| 9 | passivity / ignores failed status calls atomically（unreadable status） | ✗ | 缺失-豁免（Proxy） |

**验证结论：既有声明"Rust 省略 3 个依赖 JS Proxy 的 case"属实**——省略的是 case 5、8、9，全部使用 `unreadable()` Proxy 构造"属性读取抛错"对象；Rust 静态类型下该失败模式不存在，无法等价表达（Rust 头注释已声明，但其措辞称三个均为 "passivity case" 略不精确：case 5 属 "recording" 组）。保留的 6 个 case 顺序与 TS 一致。

### 保留 case 内的弱化（计为逻辑差异）

- **D1（同 §1）** case 1：TS 在 `startSpan` 返回后、await 前断言 `admitted===true && calls===1`（同步入场契约）；Rust 先 `.await` 再断言（`admitted/calls` 用 Atomic 记录），同步性无法验证。期望值 `{value:42}`→`42u32` 为等价意图，不计。
- **D3** case 2：TS 5 个拒绝场景（sync Error / async 普通对象 / `Promise.reject(undefined)` / sync unreadable / async unreadable）+ 对 5 个 span 断言 status=error；Rust 仅 2 个场景（`panic!("sync")`/`panic!("async")`）+ 对 2 个 span 断言 `SpanStatus::Error{..}`。其中 unreadable×2 属 Proxy 豁免；"reject undefined" 与"async 普通对象"属 JS 值模型差异；但 **"拒绝值原样保留"契约断言（`strictEqual(error, expected)`）整体被弱化为 `is_err()`**，未验证 panic payload 经 resume_unwind 原样传播（Rust 机制上可行）。
- **D4** case 3：四个 span 的状态断言与 TS 逐一相同（last-status=ok、explicit-before-throw=ok、explicit-before-rejection=Expected/async failure、expected-failure=Expected/returned failure，字面量一致）；但两处 `rejectsWithSameValue`（同值拒绝断言）被替换为 `is_err()`。

---

## 分类汇总

### 缺失（主计数 3，另有豁免类缺失明细）

| # | 项目 | 判定 |
|---|---|---|
| M1 | conformance case 5（recording/ignores failed attribute calls atomically） | 缺失-豁免（Proxy） |
| M2 | conformance case 8（passivity/suppresses unreadable telemetry payload failures） | 缺失-豁免（Proxy） |
| M3 | conformance case 9（passivity/ignores failed status calls atomically） | 缺失-豁免（Proxy） |
| M4 | `unreadable` 测试辅助（Proxy） | 缺失-豁免 |
| M5 | index.ts 类型级导出 15 项：`InferRequiredAndOptionalAttributes`、`InferStartAttributes`、`InferOptionalAttributes`、`ExactTelemetryAttributes`、`InferEventAttributes`、`TelemetrySchemaSpanName`、`TelemetrySchemaSpanStartAttributes`、`TelemetrySchemaSpanEndAttributes`、`TelemetrySchemaSpanEventName`、`TelemetrySchemaSpanEventAttributes`、`SchemaTelemetrySpan`、`TelemetrySchemaSpanUnion`、`AttributeDefinitionValue`、`RequiredAttributeNames`、`OptionalAttributeNames`（含 private 类型体操） | 缺失-豁免（纯编译期类型，无运行时对应） |
| M6 | `startInMemorySpan` 的 createSpan 失败→NOOP 回退分支 | 缺失-豁免（Rust 中不可达，见 B2） |
| M7 | case 2 的 3 个子场景（reject undefined、sync/async unreadable） | 缺失-豁免（JS 值模型 + Proxy）；断言整体弱化另计 D3 |
| M8 | `rejectsWithSameValue` 辅助 | 缺失-内联（catch_unwind 等价替换，但断言弱化计 D3/D4） |

### 多余（主计数 1）

| # | 项目 | 判定 |
|---|---|---|
| X1 | `ErasedTelemetryContext` + `ErasedSpanCallback`/`ErasedSpanFuture` + `TelemetrySpan::start_child_span` 擦除适配层 | 多余-豁免（trait 对象 dyn 兼容所需，代码注释已声明） |
| X2 | conformance.rs 测试构造辅助 `attrs`/`attr_str`/`attr_bool`/`attr_num`/`opts` | 多余（无害，测试脚手架） |
| X3 | `InMemoryTelemetrySpan: TelemetryContext` | 非多余——对应 TS span 继承的泛型 `startSpan`，计入映射 |

### 逻辑差异（4）

| # | 位置 | 描述 |
|---|---|---|
| D1 | memory.rs / noop.rs / lib.rs（全部 start 路径） | **同步入场语义丢失**：TS startSpan 同步执行 callback（契约测试显式断言）；Rust 惰性化到首次 poll。conformance case 1 相应弱化。 |
| D2 | testing/types.rs | `TelemetryAdapterConformanceCase::run(self)` 消费式单次运行 vs TS 可重复 `run()`（轻微）。 |
| D3 | testing/conformance.rs case 2 | 拒绝场景 5→2，拒绝值同值断言弱化为 `is_err()`。 |
| D4 | testing/conformance.rs case 3 | `rejectsWithSameValue` 同值断言弱化为 `is_err()`（span 状态断言全一致）。 |

### 豁免清单（语言机制等价差异，非问题）

| # | 说明 |
|---|---|
| E1 | A1/A2：span 递归启动与 typed starter 的类型级机制退化（已注释声明） |
| E2 | B1：panic payload→错误状态映射（name 硬编码 "Error"；字符串 payload ↔ Error.message） |
| E3 | B2/B3：Proxy 诱导的失败模式在 owned 数据下不存在，try/catch 被动容错平凡成立；createSpan NOOP 回退不可达 |
| E4 | D3 中 Proxy/JS 值模型子场景（unreadable、reject undefined、async 普通对象） |
| E5 | C1/C2：fixture 具体化为 InMemoryTelemetryContext、factory 同步化、AsyncDisposable 省略 |
| E6 | D4 相邻：throw↔panic 机制映射（catch_unwind/resume_unwind 等价 Promise reject/rethrow，settle 先于传播顺序一致） |
| E7 | 表示层：BTreeMap 键排序 vs 插入序；u64 vs number（整数值语义）；`end_sequence: Option` 恒有字段 vs 条件展开；`defineTelemetrySchema` 无 const 字面量捕获；schema version u32；`Send + Sync`/`'a` 约束；Object.freeze→无状态 static 单例 |
| E8 | `TelemetryContext` 泛型方法非 dyn 兼容 → ErasedTelemetryContext（X1） |

---

## 结论

运行时核心（memory 记录语义、span 生命周期、settle/inert、被动容错、NOOP 单例、typed starter 运行时行为）与 TS **1:1 一致**；span settle 顺序、id/endSequence 起点与自增、错误状态不回滚、显式状态不被自动覆盖等边界均核对通过。已知声明"Rust 省略 3 个依赖 JS Proxy 的 conformance case"**属实**（case 5/8/9，均使用 `unreadable()`）。主要偏差集中在：① 同步入场被 Rust 惰性 Future 取代（D1，可观察时序差异）；② 保留的 conformance case 中"拒绝值原样保留"的同值断言被弱化为"发生过拒绝"（D3/D4）；③ case 2 场景从 5 减为 2。无多余运行时逻辑（仅 dyn 兼容适配层，豁免）。
