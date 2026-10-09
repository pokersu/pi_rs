# 剩余复刻 TODO

> 本文件是 `UPSTREAM.md`「未完成清单（backlog）」的详情页，**只记录当前仍未对齐的项**。
> 基线：上游 tag `v1.1.0`（commit `abe508e1b`，2026-10-07）。
> 换代计划与逐项差异见 `UPSTREAM-SYNC-v1.1.0.md`；全量对比报告见 `UPSTREAM-PARITY.md`。
> 已对齐项的改动明细见 git 历史，此处不再保留。

## 一、durable 仍未落地的模块

| 模块 | 上游路径 | 文件 | 约行数 | 说明 |
|---|---|---:|---:|---|
| testing | `src/testing/**` | 7 | 2,810 | storage-conformance + env-conformance + storage-benchmark 均已落地（对 memory/jsonl/sqlite 与 NodeExecutionEnv 参数化验证） |

这一块是 runtime 测试投资，已 1:1 落地。`src/session/`（4 文件 / 2,009 行）已于 P4 全部落地；
`src/harness/`（22 文件 / 7,015 行）已于 P5a–P5h 全部落地；`src/tools/`（10 文件 / 1,313 行）与
`src/env/`（`index`/`decode`/`line-scan`/`node`/`node-watch`，4 文件）已于 P6 全部落地。
（durable 主体已全部落地。）

## 二、agent-core 与 ai 的增量

| 项 | 上游 | 说明 |
|---|---|---|
| agent-core | `packages/agent` | ✅ `durationMs`、`streamProxy` 返回 `AssistantMessageEventStream` 已对齐 |
| ai | `packages/ai` | 本区间 +1,023 / −440；核心增量已跟进：`durationMs`（assistant/tool 消息 + 流计时）、`samplingParamsByThinkingLevel`（分层采样参数）；其余多属声明范围外（其他 provider / classifier / OAuth 重构） |

## 三、语言机制豁免（不是简化，是机制映射）

| 项 | 上游 | Rust 处理 |
|---|---|---|
| SQLite 异步 facade | `storage/sqlite/database.ts`（38 行）+ `node.ts`（210 行） | 上游为跨运行时引入异步 `SqliteDatabase`/`SqliteExecutor`（事务队列、admitted-reads drain、结算控制）。Rust 用 `Arc<Mutex<Connection>>`：串行化、事务原子性、关闭拒绝由同步原语等价覆盖 |
| SQLite schema | `storage/sqlite/migrations.ts`（125 行） | 上游结构化列 + migrations 以支持下推；Rust 用 JSON 列（`records`/`documents`/`meta`），过滤在内存里。`documents.revisions` 随附每条修订的提交序号，以支持按点物化 |
| `close()` 语义 | `storage/{memory,jsonl,sqlite}` | 上游等待已受理的异步读排空；Rust 读在锁内同步完成，置 `closed` 标记后返回 |
| `mint_id` 关闭后行为 | `assertOpen()` 抛异常 | `Storage::mint_id` 无 `Result`，改为 panic（对等语义） |
| chord `Draft<T>` / `diff_revisions` | `packages/chord` | 纯类型级映射（Rust 等价于 `JsonValue`）/ durable 未直接使用，不纳入 |
| telemetry span 类型、工具 `*ToolInput` | schema 推导类型 | Rust 无类型级推导能力；运行时部分已具备，**豁免** |
| session 具名错误 | `session.ts` 4 个错误类 | 已核实消息逐字对齐、上游无 `instanceof` 分支 → 无行为差异；改 trait 错误类型成本大，不建议 |

## 四、明确不复刻

| 项 | 原因 |
|---|---|
| `storage/sqlite/cloudflare.ts`（139 行） | Cloudflare Durable Object 运行时，Rust 无对应 |
| legacy-v3 JSONL 迁移 | **用户已明确要求不复刻** |
| `packages/{client,codemode,coding-agent,evals,mcp,protocol,server,tui}` | 产品层，不在覆盖范围（见 `UPSTREAM.md`） |
| `packages/chord` 其余部分 | durable 未使用，不纳入 |

## 五、上游已知偏差（未同步的能力）

1. `onProviderStreamEvent`（0.99.0）—— 需穿透 provider 流层（`StreamOptions` + 各 provider 事件解析点）
2. overflow 的 Z.AI CN 端点检测 —— 本项目未接入 Z.AI
3. HTTP-date 形式的 `Retry-After` —— Rust 侧忽略该 header 走指数退避（上游用 `Date.parse`）

## 六、双向审计新发现（2026-10-09，逐文件逐方法）

> 完整报告见 `tools.d/parity/AUDIT-REPORT.md`。与之前只查「TS→Rust 缺失」不同，
> 本轮新增「Rust→TS 多余」反向检查，暴露了一批旧扫描未覆盖的差异。
> **同日已按本节修复**，剩余待续项单独标注。

### agent → pi-agent-core（9 缺失 + 5 多余）—— 已全部修复

**真实缺失（已修）**：
1. ✅ `onPayload`/`onResponse`/`onProviderStreamEvent` 三钩子（pi_ai 新增回调类型 + provider 调用点穿透）
2. ✅ `steeringMode`/`followUpMode` 运行期 get/set 访问器（`set_steering_mode`/`steering_mode`/`set_follow_up_mode`/`follow_up_mode`）
3. ✅ `subscribe` 返回退订闭包（listeners 加 id）
4. ✅ `prompt(input, images?)` 重载（`prompt_text_with_images`，且 `prompt_text` 改为 `Blocks` 对齐上游）
5. ✅ `handleRunFailure` 模型信息（`failure_message` 接收当前 model）
6. ✅ proxy `AbortSignal` 处理（`tokio::select` + aborted 检查）
7. ✅ proxy `providerThinkingLevel` 字段（Done/Error 变体）
8. ✅ proxy 干净 EOF 保护（`saw_terminal_event` + error 兜底）
9. ✅ proxy 非 2xx 错误体 `{error}` 解析

**真实多余（已清理）**：`proxy_stream_fn`、`get_default_stream_fn` 再导出、`FinalizedToolCallOutcome`
（改私有别名，`AgentToolCallOutcome` 改公开 struct）、`ShouldStopAfterTurnContext`、`set_system_prompt`。

**存疑（已修/待续）**：✅ `continue_turn` 全-system 守卫 + drain 顺序已对齐上游；
⏳ `skipInitialSteeringPoll` 竞态、proxy `toolcall_delta` 重建语义（低危）未修。

### chord → pi-durable/src/chord（4 缺失）—— 已全部修复

1. ✅ `ReplicatedStateReplica`（state.rs 新增，hydrate/update/clear）
2. ✅ `MutableReplicatedState`（`MutableReplicatedStateImpl` + `replicated_state` 工厂，基于 Tracker）
3. ✅ `state-internals` 注册表（`ReplicatedStateInternals` trait + Weak 注册表，Mutable/Attached 均注册）
4. ✅ `state-codec`（delta 新增 `Encoder`/`Decoder` path-interning + `state_codec.rs` 的 `ServiceStateEncoder`/`Decoder`）

### ai → pi-ai（3 轻微缺项 + 1 超范围）—— 缺项已修

1. ✅ `Model.promptCache`（`ModelPromptCache`）
2. ✅ `Model.inputLimits`（`ModelInputLimits`/`ModelImageInputLimits`/`ModelImageResizeOptions`）
3. ✅ `UnsupportedStrictSchemaKeywordCheck` 回调（`constrained-sampling` 加可选回调）
4. ⏳ `utils/assistant-message-frame.rs` 超出 AGENT.md 声明范围（忠实移植，非虚构，保留并已从 AGENT.md 补声明）

### telemetry / durable

telemetry 生产逻辑 0 缺失 0 多余（仅测试 conformance 3 case 因 JS Proxy 不可表达省略）；
durable 0 缺失 0 多余，9 个未配对文件定性全部成立。

### 工具修正

- `tools.d/.parity/scan.py` 的 `declared_scope` 曾把 durable 误判为 6 文件子集（其 AGENT.md 状态表含 `src/*.ts` 路径），已修正为仅 ai/chord 是声明子集。
- 新增 `tools.d/.parity/reverse.py`（反向多余扫描）与 `audit_brief.py`（逐模块审计简报）。
