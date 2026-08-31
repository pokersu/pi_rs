# pi-telemetry

vendor 中立的 telemetry 契约与类型化 schema 工具，是 `pi-ai` 与 `pi-agent` 的可观测性底层。

## 复刻来源

1:1 复刻自 [earendil-works/pi](https://github.com/earendil-works/pi) 的 `packages/telemetry`，6 个文件全部对应：

| 原 TS 文件 | Rust 文件 |
|---|---|
| `src/index.ts` | `src/lib.rs` |
| `src/memory.ts` | `src/memory.rs` |
| `src/noop.ts` | `src/noop.rs` |
| `src/testing/index.ts` | `src/testing/mod.rs` |
| `src/testing/types.ts` | `src/testing/types.rs` |
| `src/testing/conformance.ts` | `src/testing/conformance.rs` |

## 功能介绍

- **span 生命周期**：`TelemetryContext::start_span(options, callback)` 打开一个 span，callback 结束即 settle。
- **事件与属性**：span 上可 `add_event`、`set_attributes`、`set_status`。
- **递归子 span**：`TelemetrySpan::start_child_span` 在父 span 下创建子 span，形成树状关系。
- **两种后端**：`InMemoryTelemetryContext`（进程内记录，测试用）与 `NOOP_TELEMETRY_CONTEXT`（空操作单例）。
- **schema 定义**：`TelemetrySchemaDefinition` 等类型用于描述 span 词汇表（文档生成、自描述），`define_telemetry_schema` / `create_typed_span_starter` 提供绑定入口。
- **conformance 契约测试**：`create_telemetry_adapter_conformance` 生成与运行器无关的契约 case，验证任意 adapter 的行为。

## 原理

核心是 **callback 式的 span 生命周期**：

1. `start_span` 创建 `MutableRecordedTelemetrySpan` 并压入 `state.spans`。
2. 同步/异步 callback 以该 span 为参数执行；callback 抛错（Rust 中为 panic）时 span 被标记为 error。
3. callback 完成后 `settle_span`：设置 `settled = true`、分配 `end_sequence`（记录完成顺序）。
4. settle 之后 span 上的所有调用（`set_attributes`/`add_event`/`set_status`/`start_child_span`）**惰性失效**（inert），不再修改状态。

容错原则是 **passive（被动）**：记录失败不回滚、不抛出，尽力而为。`InMemoryTelemetryContext` 用 `Mutex` 保护共享状态（对应 TS 单线程的 JS event loop），并发安全。

## 复刻过程中的变化

1. **类型体操简化**：TS 的 `TypedSpanStarter`、`Infer*`、`ExactTelemetryAttributes`、`SchemaTelemetrySpan` 等是纯编译期类型（conditional types / infer / union-to-intersection），Rust 无法表达。`create_typed_span_starter` 退化为泛型 `TypedSpanStarter<C>`，运行时行为一致；schema 定义类型保留为数据（见 `lib.rs` 注释）。
2. **dyn 兼容性拆分**：TS 中 `TelemetrySpan extends TelemetryContext`（span 继承泛型 `startSpan`）。Rust 的 trait 对象不能有泛型方法，因此 `TelemetrySpan` 独立出来，递归启动拆为类型擦除的 `start_child_span`（返回值 `Box<dyn Any>` 擦除）。这是本模块唯一的实质结构变化。
3. **3 个 passivity case 省略**：`conformance.ts` 里 3 个依赖 JS `Proxy`（构造「属性读取时抛错」对象）的 case，Rust 静态类型下无对应物，在文件头注释说明。
4. **错误传播用 panic**：TS 的 `throw`/`Promise.reject` 对应 Rust panic，通过 `catch_unwind` 捕获并 settle 为 error 后 `resume_unwind` 继续传播。

## 阅读步骤

1. `src/lib.rs` —— 类型（`AttributeValue`/`SpanOptions`/`SpanStatus`）、trait（`TelemetryContext`/`TelemetrySpan`）、schema 类型、`TypedSpanStarter`。
2. `src/noop.rs` —— 最简单的实现，先看它理解 trait 的形状（约 50 行）。
3. `src/memory.rs` —— 参考实现，理解 span 生命周期、settle、passive 容错（约 370 行）。
4. `src/testing/conformance.rs` —— 契约测试，验证 1~4 步理解正确。
