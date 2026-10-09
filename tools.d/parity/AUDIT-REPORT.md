# pi_rs ↔ upstream 双向 1:1 一致性审计报告

> 基线：上游 `v1.1.0`（commit `abe508e1b`）
> 方法：正向（TS→Rust 查缺失）+ 反向（Rust→TS 查多余）双方向逐文件逐方法比对。
> 机械扫描见 `UPSTREAM-PARITY.md`（`tools.d/.parity/scan.py`）；本报告为**人工逐方法复核结论**。
> 5 个模块各派独立审计子 agent 交叉核对，关键结论经主 agent grep 复核。

## 修复状态（2026-10-09 更新）

- ✅ **agent 9 缺失 + 5 多余全部修复**：proxy 四缺陷（AbortSignal/EOF/providerThinkingLevel/非 2xx 错误体）、
  三钩子链路、steeringMode/followUpMode 访问器、subscribe 退订、prompt images、handleRunFailure 模型信息。
- ✅ **ai 3 缺项修复**：`Model.promptCache` / `Model.inputLimits` / strict-schema 回调。
- ✅ **chord 4/4 修复**：`MutableReplicatedState` / `ReplicatedStateReplica` / `state-internals` 注册表 /
  `state-codec`（含 delta 的 `Encoder`/`Decoder` path-interning）。

`cargo test --workspace` 全通过，clippy/fmt 无告警。


## 总览

| 模块 | 复刻目标 | 配对文件 | 真实缺失 | 真实多余 | 结论 |
|---|---|---:|---:|---:|---|
| `agent` → `pi-agent-core` | 完整 | 6/6 | **9** | **5** | ❌ 不一致 |
| `ai` → `pi-ai` | 声明子集 | 36（+155 范围外） | 3（轻微） | 1（超范围） | ⚠️ 核心一致 |
| `telemetry` → `pi-telemetry` | 完整 | 6/6 | 0（生产）/ 3 case 测试省略 | 0 | ✅ 生产一致 |
| `durable` → `pi-durable` | 完整 | 58（+9 豁免） | 0 | 0 | ✅ 完全一致 |
| `chord` → `pi-durable/src/chord` | 子集（8 文件） | 3+5（合并） | **4** | 0 | ❌ 不一致 |

**结论：并非完全 1:1。** 不一致集中在 `agent`（14 处）与 `chord`（4 处）；`durable` 与 `telemetry` 生产逻辑完全一致；`ai` 声明子集内核心逻辑一致。

---

## 一、agent → crates/pi-agent-core（完整复刻）

### 真实缺失（9 项）

| # | 文件 | TS 符号 | 缺失的业务逻辑 |
|---|---|---|---|
| 1 | agent.ts | `AgentOptions.onPayload/onResponse/onProviderStreamEvent` + `Agent` 同名字段 + `createLoopConfig` 转发 | 三个流式观测钩子整条链路缺失；根因 `pi_ai::SimpleStreamOptions` 本身也无这三字段（与 UPSTREAM.md「已知偏差 1」一致） |
| 2 | agent.ts | `steeringMode` / `followUpMode` get/set 访问器 | Rust 无运行期访问器；`QueueMode` 只能在 `new()` 时设置，运行期不可改（`PendingMessageQueue.mode` 私有） |
| 3 | agent.ts | `subscribe(listener)` 返回退订函数 | Rust `subscribe` 返回 `()`，无法取消订阅 |
| 4 | agent.ts | `prompt(input: string, images?)` 重载 | Rust 只有 `prompt_text(&str)`，无 images 参数；`normalizePromptInput` 的 text+images 合流逻辑缺失 |
| 5 | agent.ts | `handleRunFailure` 取 `this._state.model` | Rust `failure_message` 把 api/provider/model 硬编码 `"unknown"`，失败消息丢失当前模型信息 |
| 6 | proxy.ts | `streamProxy` 的 AbortSignal 处理 | Rust `proxy_request` 忽略 `options.signal`：无 `reader.cancel`、无逐块 aborted 检查、catch 分支永远报 `Error` 而非 `Aborted` |
| 7 | proxy.ts | `ProxyAssistantMessageEvent` 的 `providerThinkingLevel?` | Rust Done/Error 变体无此字段，重建不写 `partial.provider_thinking_level` |
| 8 | proxy.ts | 干净 EOF 无 done/error 保护 | TS 推 `error`（"Connection closed by proxy server…"）；Rust 直接 `stream.end(None)`，消费者悬等 |
| 9 | proxy.ts | 非 2xx 响应体 `{error}` 解析 | Rust 只 `format!("Proxy error: {}", status)`，不读响应体、无 statusText |

### 真实多余（5 项）

| # | 文件 | Rust 符号 | 说明 |
|---|---|---|---|
| 1 | proxy.rs | `proxy_stream_fn(proxy_url, auth_token)` | TS 无此函数（上游仅文档示例手写闭包），新增便捷构造 |
| 2 | lib.rs | `get_default_stream_fn` 再导出 | TS `index.ts` 仅 re-export `setDefaultStreamFn` |
| 3 | agent-loop.rs/lib.rs | `FinalizedToolCallOutcome` 公开 + 别名 | TS 中是私有 type 别名，仅 `AgentToolCallOutcome` 公开（低危，仅可见性） |
| 4 | types.rs | `ShouldStopAfterTurnContext` | 死别名；TS v1.1.0 已删除 `shouldStopAfterTurn`（低危） |
| 5 | agent.rs | `set_system_prompt` | TS `AgentState.systemPrompt` 为 `readonly`（派生自 messages），无直接 setter（已 grep 复核） |

### 存疑项（6，低危）
`continue_turn` 缺「全为 system」守卫；`skipInitialSteeringPoll` 未建模（竞态下差异）；proxy `toolcall_delta` 用 `arguments.to_string()+delta` 重建（与 TS 原始 `partialJson+=delta` 不等价，疑似移植 bug）；`state().messages` 快照不含首条 system 消息；`getApiKey` 强制异步；运行期可变字段面收窄。

---

## 二、chord → crates/pi-durable/src/chord（子集，8 文件）

### 真实缺失（4 项）

| # | TS 文件 | 符号 | 说明 |
|---|---|---|---|
| 1 | services/state.ts | `ReplicatedStateReplica` | 冷副本 `value/subscribe/hydrate/update/clear/#deliverAll` 整体未移植；**state.rs 头注释未声明延后，属隐性缺口** |
| 2 | services/state.ts | `MutableReplicatedState` | `replicatedState(initial)` 的 `change/replace/get value/subscribe` 未移植（头注释已声明延后） |
| 3 | services/state-internals.ts | `registerReplicatedStateInternals/getReplicatedStateInternals` | WeakMap 注册表无对应；全仓无消费方 |
| 4 | services/state-codec.ts | `createServiceStateEncoder/Decoder` 等 | 服务订阅线格式层整体未移植（非 serde 替代） |

0 多余。1:N 合并（delta/index+apply-immutable-trusted→delta.rs、tracker.ts→tracker.rs、state*.ts→state.rs）语义已核实等价；wire 编解码（WireOp/Encoder/Decoder/path interning）由 serde 未压缩 Op 元组替代（**不保证跨语言 wire 字节兼容**）。

---

## 三、ai → crates/pi-ai（声明子集）

### 真实缺失（3 项，轻微，范围内无消费者）
1. `Model.promptCache`（`ModelPromptCache`）字段缺失
2. `Model.inputLimits`（`ModelInputLimits`）字段缺失
3. `UnsupportedStrictSchemaKeywordCheck`/`isUnsupportedKeyword` 回调缺失（仅 Anthropic/Bedrock 等范围外 provider 用）

### 真实多余（1 项）
`utils/assistant-message-frame.rs` 超出 AGENT.md 44 文件表（忠实移植上游，非虚构逻辑）。

`stream/complete` → `stream_simple/complete_simple`（Api 类型参数抹去）、`refresh` 系列 → 模型目录硬编码、`classify/generateImages` → 范围外，均属等价映射/范围外。

---

## 四、telemetry → crates/pi-telemetry（完整复刻）

**生产逻辑 0 缺失、0 多余。** 唯一实质差异是测试 conformance 3 个 case + 3 个子用例省略（依赖 JS `Proxy`「属性读取即抛错」与 `undefined` 语义，Rust 无法表达，conformance.rs 头已声明）。

## 五、durable → crates/pi-durable（完整复刻）

**0 缺失、0 多余，完全一致。** 9 个未配对文件定性全部成立：

| 未配对 TS | 判定 |
|---|---|
| storage/sqlite/cloudflare.ts | 不复刻（Cloudflare 运行时） |
| storage/sqlite/database.ts + node.ts + migrations.ts | 语言机制豁免（异步 facade + 结构化 schema → Arc\<Mutex\> + JSON 列） |
| storage/sqlite/storage.ts（934 行） | 合并进 sqlite.rs（17/17 方法对齐，已逐方法核实） |
| storage/jsonl/node.ts | 语言机制豁免 |
| testing/assertions.ts + runner.ts | JS 测试适配器 → #[test] |
| testing/types.ts | 已合并进 storage_conformance.rs/env_conformance.rs |

---

## 优先级建议

1. **P0（功能性缺口）**：agent proxy 的 AbortSignal/EOF/错误体处理（缺失 6/7/8/9）、`onProviderStreamEvent` 链路（缺失 1）、`steeringMode/followUpMode` 运行期访问器（缺失 2）。
2. **P1（API 面差异）**：agent 的 `subscribe` 退订（缺失 3）、`prompt` images 重载（缺失 4）、5 处多余清理；chord 的 4 处 state 系列缺失。
3. **P2（低危）**：ai 的 3 处字段/钩子；telemetry 测试 case 省略；agent 6 处存疑项。

> 备注：`scan.py` 的 `declared_scope` 曾把 durable 误判为 6 文件子集（其 AGENT.md 状态表含 `src/*.ts` 路径），已修正为仅 ai/chord 是声明子集；`durable` 的 AGENT.md 状态表本身也已过时（实际 67 文件全部落地）。
