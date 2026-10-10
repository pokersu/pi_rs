# pi_rs ↔ upstream 差异修复记录（P0 / P1 / P2）

> 基线：上游 `v1.1.0` @ `abe508e1b`
> 依据：`tools.d/parity/recheck/` 下 11 份方法级审计报告（第二轮重审）
> 验证：`cargo test --workspace` 475 passed / 0 failed；`cargo fmt --all -- --check` 干净；
> clippy 仅 pi-durable 7 处历史告警（无新增），pi-ai / pi-agent-core / pi-telemetry 0 告警。

## 已修复项

### P0（高危，正常路径可触发）

| # | 模块 | 修复 | 文件 |
|---|---|---|---|
| P0-1 | durable harness | `generation.answer()` 补「边界选中用户」的 `start_run` 启动（TS `if (users.length > 0) await startRun(...)` 被漏写） | `harness/generation.rs` |
| P0-2 | pi-ai | `stream_simple` 未 normalize_context —— **核实为误报**：normalize 已在 provider 的 api 层 `build_body`（`openai-responses.rs:600`）执行，行为等价，未改代码 | — |
| P0-3 | pi-ai | headers 合并优先级反转：三处 `merge_headers(opts.headers, auth.headers)` 改为 `merge_headers(auth.headers, opts.headers)`（请求选项覆盖认证，对齐 TS `mergeHeaders(auth.headers, options.headers)`） | `models.rs` |
| P0-4 | durable storage | `document.copy` 在 jsonl/sqlite 后端被拒绝 → 抽共享 `resolve_document_copies`，三后端 commit 前把 copy 物化源并展开为 `document.create`（含「源本批被改 / 源不可读 / 记录不一致」三项校验 + StorageRejected 包装） | `storage/mod.rs`、`storage/memory.rs`、`storage/jsonl/storage.rs`、`storage/sqlite.rs` |
| P0-5 | durable storage | ① `conversation_ids_by_owner_conversation` 索引用 `parent` 而非 `owner.conversation_id`（memory + sqlite 的 `scan_conversations` 过滤）；② submission 更新不清理旧 requestId/status 索引；③ `commit` 不推进 `next_id`；④ 补齐 `check_document_actions` 八项文档语义校验（Unknown/already exists/is retired/delta no base/version transition/多内容命令/二次 retire/双活化身），恢复 memory/jsonl 的 commit 原子性 | `storage/memory.rs`、`storage/sqlite.rs` |

### P1（关键，正常路径可触发）

| # | 模块 | 修复 | 文件 |
|---|---|---|---|
| P1-1a | env | `open_binary_reader` noFollow 死代码：`File::open` 跟随链接后再查 `metadata.is_symlink()` 永远为假 → 改为 open 前用 `symlink_metadata` 检查最终组件 | `env/node.rs` |
| P1-1b | env | `exec` spill 文件丢 stderr → stderr 读取分支补 `collected.extend_from_slice` | `env/node.rs` |
| P1-1c | env | `exec` 中断（timeout/aborted）返回的 `ExecutionError` 丢失 `spill_path` | `env/node.rs` |
| P1-1d | env | `exec` 信号终止退出码：`code().unwrap_or(1)` → `128 + 信号编号`（对齐 TS） | `env/node.rs` |
| P1-1e | session | `retire_doc` 对 fork-copy 分支直接 `return Ok(())` 不退役 → 补 `check_record_scope` + 置 `retire_on_commit` | `session/transaction.rs` |
| P1-1f | session | `doc()/acquire` 缺 `skipLoad` 参数（旧化身退役中时不加载旧值） | `session/transaction.rs` |
| P1-2 | chord | tracker 相同值写入抑制：`set` 对非容器严格相等 / 容器深度相等不再记录 op（对齐 TS `emitChangedValue`，避免空变更发布） | `chord/tracker.rs` |

### P2（agent 关键功能项；其余低危/值等价项已核实豁免）

| # | 模块 | 修复 | 文件 |
|---|---|---|---|
| P2-1 | agent | error/aborted turn 上补 `finishTurn` 调用（TS 硬退出前会先跑 hook） | `agent-loop.rs` |
| P2-2 | agent | `AgentLoopTurnUpdate` 补 `messages` 字段 + `run_loop` 处理 prepareNextTurn 返回的追加消息 | `types.rs`、`agent-loop.rs` |
| P2-3 | agent | `run_loop` 起始补一次 steering 轮询（agent 空闲期 `steer()` 的消息注入首个 turn） | `agent-loop.rs` |
| P2-4 | agent | apiKey 回退：`get_api_key` 未配置时回退到预置 `api_key`（对齐 TS `\|\| config.apiKey`） | `agent-loop.rs` |
| P2-5 | agent | proxy 协议违规 `panic!` → 用 `catch_unwind` 捕获转成 error 事件 + 流正常结束（对齐 TS 抛 Error 被 catch → error 事件） | `proxy.rs` |

## 核实为「豁免 / 值等价 / 声明范围外」、未改代码的项（要点）

- **P0-2**：`stream_simple` 的 normalize 已在 provider api 层（`build_body`）等价实现。
- **chord D7 其余部分**（字符串 a/t 压缩、readd d+s、dense region、数组结构归一化）：值等价、仅 op 序列不同，属「性能优化」，tracker.rs 头注释已声明不复刻 piece-tree/启发式。
- **sqlite 异步 facade / cloudflare / migrations / Windows 分支**：语言机制豁免或明确不复刻。
- **TypeBox → JSON schema、throw → Result、Proxy → 显式方法、模块级 const → OnceLock 工厂**：语言机制等价。
- **ai 声明范围外**（40+ provider、OAuth 登录流程、图像生成、模型目录）：声明子集外。

## 未修复的低危项（待用户决定）

剩余约 180 项为「非法输入 / 竞态 / 文案 / 极低概率边界」类，正常路径不可触发，分布在：

- agent：`tool_execution_update` 延迟刷出、`afterToolCall` 收到原始 toolCall 而非 prepared、`tool_execution_end.result` 只含 details、proxy 若干增量事件边界。
- durable：`findLatestHeadMarker` 段优先 vs 全局最新（sqlite）、未知会话静默返回 vs 抛错、`scanDocuments` 游标 order 语义、limit=0 边界。
- ai：openai-completions/faux 文件头自述「简化版」的声明范围内取舍。

这些若需 1:1 逐项消除，工作量仍较大且多为不可观测；建议按需逐项处理。

---

## 第二轮修复：正常路径可触发的 6 处

> 依据：`tools.d/parity/recheck/verify/VERIFY-SUMMARY.md` 列出的 6 处「正常路径可触发」未修项。
> 验证：`cargo test --workspace` 475 passed / 0 failed；`cargo fmt` 干净；clippy 仍仅 pi-durable 7 处历史告警。

| # | 来源 | 修复 | 文件 |
|---|---|---|---|
| 修1 | a1 差2 | LLM 上下文构造：`llm_context` 不再传 `tools`（工具声明已在 fold 时折进 messages 首条 system 消息，避免 pi-ai 重复前置）；`run_agent_loop_continue` 补 `fold_initial_system_message` | `pi-agent-core/src/agent-loop.rs` |
| 修2 | a1 差5/6/8 | 工具事件流：update 事件改为执行期间即时 emit（tokio::spawn）+ 工具 panic 时也刷出（`drain_emit_tasks`）；update 事件 `args` 改用原始 toolCall.arguments；`tool_execution_end.result` 改为完整 `AgentToolResult` 并补 `duration_ms`。差7（afterToolCall 收到原始 toolCall）**核实为误报**（Rust 解构后传的是 prepared.toolCall） | `pi-agent-core/src/agent-loop.rs`、`types.rs` |
| 修3 | a6 差1 | compaction `run_summarize` 剥离 `deferred`（对齐 TS `const { deferred: _deferred, ...forwarded }`） | `pi-durable/src/harness/compaction.rs` |
| 修4 | a7 差1/2 | tool.rs `bound_content` 丢弃非 keep 文本项（对齐 TS）；`run()` 的 env 构建错误改走 tool_error + failed 结算（而非 `?` 传播），`duration_ms` 只计 execute 耗时 | `pi-durable/src/harness/tool.rs` |
| 修5 | a8 缺4/5 | **未完成**：env/storage conformance 共 32 个 case 补齐，见下方说明 | — |
| 修6 | a9 差5 | `ProviderRequestOptions` 补 `env` 字段；`stream_simple`/`fetch_deferred`/`cancel_deferred` 注入 `merge_env(resolution.env, options.env)`（请求选项覆盖解析结果） | `pi-ai/src/types.rs`、`models.rs`、`api/simple-options.rs` |

### 修5 说明（conformance 32 个 case）

- `env-conformance`：缺 14 个 TS case（8 watch、3 dir-reader、windowed exec、2 symlink）。
- `storage-conformance`：缺 18 个 TS case（含全部 8 个 document 契约 case、ID 耗尽、fork 历史、deep-fork 扫描等）。
- 性质：**测试契约覆盖缺口，非运行逻辑**（a8 报告自述）。补齐需翻译 2000+ 行 TS 断言到 Rust 并适配后端 API，建议分批单独安排（先 storage 的 8 个 document case，再 env 的 watch/symlink）。

---

## 第三轮修复：剩余差异中筛选出的前 5 项

> 依据：`tools.d/parity/recheck/` 各报告中的「正常路径可触发 / 可见」类剩余项（由用户要求「修前 5 项」筛选）。
> 验证：`cargo test --workspace` 475 passed / 0 failed；`cargo fmt --all -- --check` 干净；clippy 仍仅 pi-durable 7 处历史告警（无新增）。

| # | 来源 | 修复 | 文件 |
|---|---|---|---|
| 修1 | a8 差1 | bash 超时文案：`"Command timed out after {timeout:?}"`（Debug 打印泄漏 `Some(30.0)`）→ `timeout.unwrap_or_default()` 只打印秒数，对齐 TS | `pi-durable/src/tools/bash.rs` |
| 修2 | a1 差3 | `thinkingLevel` 写入时机：`result()` 收尾时先写 `final_message.thinking_level = config.stream.reasoning` 再写回 `context.messages` 并发射 `message_end`（两处：`Done/Error` 分支与 result() 直通收尾），对齐 TS 流结束时附加 thinkingLevel | `pi-agent-core/src/agent-loop.rs` |
| 修3 | a1 差4 | `finishTurn` 的 `continue` 与 follow-up 次序：引入 `explicit_continuation` 标志，`AgentTurnDecision::Continue` 不再直接置 `has_more_tool_calls`，改在内层循环末尾统一处理；有工具结果/steering/follow-up 时清除标志，否则用「仅上下文的空 turn」履行 continuation | `pi-agent-core/src/agent-loop.rs` |
| 修4 | a7 差5 | `append_tool_result` 存出消息的 `details`/`usage`/`added_tool_names` 序列化为 `"xxx": null`：三字段补 `#[serde(default, skip_serializing_if = "Option::is_none")]`，None 时整键省略（对齐 TS 不产出 undefined 键，与 `duration_ms` 同法） | `pi-ai/src/types.rs` |
| 修5 | a1 缺8 | `run_agent_loop` 初始 prompts 未 `declareToolChanges`：抽取 `seed_leading_system_message`（对应 `createMutableAgentState` 的 seed），seed 后对初始 prompts 调 `declare_tool_changes`，将 `initial_messages` 用于 `new_messages`/`current_context.messages`/事件循环（对齐 TS `const initialMessages = declareToolChanges(context, prompts)`） | `pi-agent-core/src/agent-loop.rs` |

---

## 第四轮修复：分层视图第 1 层（正常路径可触发）6 项

> 依据：`DIFF.md`「〇、分层视图」第 1 层列出的 6 项正常路径可触发的行为差异。
> 验证：`cargo test --workspace` 475 passed / 0 failed；`cargo fmt --all -- --check` 干净；clippy 仍仅 pi-durable 7 处历史告警（无新增）。

| # | 来源 | 修复 | 文件 |
|---|---|---|---|
| 修1 | a10 AR-1/AR-2 | OAuth 刷新取消语义：`refresh` 只受 `AbortSignal::timeout(15000)` 约束（不再 `any(signal, timeout)`）；modify 用独立 `lock_wait` signal，调用方 signal abort 只在「等待锁」阶段触发它、进入 modify 回调后解除关联（`Arc<AtomicBool>` 标志 + spawn 监听）；credential-store 的 `enqueue` 改用 `tokio::spawn`，abort 时排队任务继续后台执行并持久化（对齐 TS promise 无法取消） | `pi-ai/src/auth/resolve.rs`、`pi-ai/src/auth/credential-store.rs` |
| 修2 | a10 OR-14 | `map_stop_reason` 未知 status 改为返回 `Err`（`Unhandled stop reason: {status}`），不再静默当 stop；`finalize_stop_reason` 改返回 `Result` 并 `?` 传播 | `pi-ai/src/api/openai-responses.rs` |
| 修3 | a10 OR-16 | 终态：pending → `Err("OpenAI Responses stream ended without a stop reason")`，aborted/error → `Err(errorMessage \|\| "An unknown error occurred")`；toolUse 时检查 slots 里残留的 `Slot::ToolCall`（output_item.done 未到）并抛错 | `pi-ai/src/api/openai-responses.rs` |
| 修4 | a7 差-11 | registry `notify` 先快照监听者数组（clone Arc 列表）、释放锁后再逐个调用，避免监听者回调内再 subscribe/install/uninstall 时 Mutex 死锁（对齐 TS `[...this.#listeners]` 快照遍历） | `pi-durable/src/harness/registry.rs` |
| 修5 | a1 差-10 | proxy 错误/中止时用「已累积内容的 partial」发 error 事件：`partial` 移到 `stream_proxy` 创建并传入 `proxy_request`（`&mut AssistantMessage`），失败时 `stream_proxy` 直接复用该 partial（对齐 TS catch 块用 output 发 error） | `pi-agent-core/src/proxy.rs` |
| 修6 | a1 差-13 | proxy `toolcall_delta` 在原始 partialJson 文本上累积：新增 `HashMap<usize, String>` 维护每个 content_index 的原始 JSON 缓冲，`toolcall_start` 初始化空串、`toolcall_delta` 追加 delta 后 parse、`toolcall_end` 移除（对齐 TS 的 `(content as any).partialJson += delta`，不再在已解析参数的再序列化上累积） | `pi-agent-core/src/proxy.rs` |

---

## 第五轮修复：serde 持久化形态（a4a 差-3）

> 依据：`DIFF.md`「〇、分层视图」第 1 层唯一待办；核实结论为真实差异（jsonl/sqlite 直接 serde 内部记录类型落盘，形态与上游不兼容）。
> 验证：`cargo test --workspace` 475 passed / 0 failed；`cargo fmt --all -- --check` 干净；clippy 仍仅 pi-durable 7 处历史告警（无新增）；临时序列化断言验证形态对齐上游（已验证后删除）。

给 `pi-durable/src/types.rs` 里 17 个落盘契约类型补齐 serde 属性，对齐上游「扁平判别 + camelCase」：

- `TaskState`/`TaskOutcome`：`#[serde(tag = "status", rename_all = "lowercase")]`。
- `TaskRecord`/`EntryRecord`/`SubmissionIdentity`/`DocumentRecord`/`DocumentCreate`：`#[serde(rename_all = "camelCase")]`。
- `TaskOwnership`/`ConversationOwnership`/`DocumentScope`：`#[serde(tag = "kind", rename_all = "lowercase")]` + 变体字段 camelCase。
- `ContextEdit`：`#[serde(tag = "action", rename_all = "lowercase")]`。
- `SubmissionRecord`：`#[serde(tag = "type", rename_all = "lowercase")]` + `#[serde(flatten)]`（identity/status 平铺）。
- `InputSubmissionStatus`/`WriteSubmissionStatus`/`SubmissionSettlement`：`#[serde(tag = "status", rename_all = "lowercase")]`。
- `ConversationHistory`：`rename_all = "lowercase"`；`ConversationFork`：`rename_all = "camelCase"`。
- `DocumentContent`：`#[serde(tag = "kind", rename_all = "lowercase")]`（newtype 变体 `Base(DocumentBase)` 由 serde 自动平铺 version/value）。
- 27 处 `Option` 字段补 `#[serde(skip_serializing_if = "Option::is_none")]`（对齐 TS「省略 undefined」而非序列化 null；`Option` 字段缺省自动 None，无需 `default`）。

未改（不在落盘路径）：`EntryHead`/`EntryDraft`（draft 不落盘）、`DocumentPoint`（查询参数）、`StorageWrite`/`CommitChange` 等（jsonl 走 codec.rs `MainOperation`、sqlite 走记录表，均有独立序列化）。
