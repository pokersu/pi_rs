# pi-ai

统一多 provider 的 LLM API：消息/模型/工具类型、事件流抽象、工具参数校验、provider 框架、认证（auth），以及 openai / deepseek 的 SSE 流式实现。

## 复刻来源与范围

复刻自 [earendil-works/pi](https://github.com/earendil-works/pi) 的 `packages/ai`。

原版有 177 个文件（40+ provider 的 HTTP 实现、OAuth 认证、图像生成、模型目录等）。按「provider 不需要实现所有，但要有 openai 和 deepseek」的要求，本模块翻译**核心子集**（44 文件）：核心类型 + openai/deepseek/faux（统一走 responses API）+ **auth 认证核心框架**（不含各厂商 OAuth 登录流程）+ 通用 utils/流式选项/消息转换/约束采样。

| 原 TS 文件 | Rust 文件 | 说明 |
|---|---|---|
| `src/types.ts` | `src/types.rs` | 完整消息/内容/工具/模型/事件类型 + `AbortSignal` |
| `src/utils/event-stream.ts` | `src/utils/event-stream.rs` | EventStream + 工厂 |
| `src/utils/validation.ts` | `src/utils/validation.rs` | 工具参数校验（完整 coercion 逻辑） |
| `src/utils/uuid.ts` | `src/utils/uuid.rs` | uuidv7 |
| `src/utils/text.ts` | `src/utils/text.rs` | contentText |
| `src/utils/json-parse.ts` | `src/utils/json-parse.rs` | repairJson / parseStreamingJson |
| `src/utils/abort.ts` | `src/utils/abort.rs` | operationSignal / raceWithAbortSignal / abortReason |
| `src/utils/diagnostics.ts` | `src/utils/diagnostics.rs` | formatThrownValue 等诊断工具 |
| `src/utils/retry.ts` | `src/utils/retry.rs` | 有界重试的 assistant 调用分类与退避 |
| `src/utils/estimate.ts` | `src/utils/estimate.rs` | context token 估算（usage 优先 + 启发式回退） |
| `src/utils/overflow.ts` | `src/utils/overflow.rs` | 各 provider context overflow 错误检测 |
| `src/utils/error-body.ts` | `src/utils/error-body.rs` | provider 错误对象归一化与格式化 |
| `src/utils/provider-retry.ts` | `src/utils/provider-retry.rs` | provider HTTP 请求重试（408/409/429/5xx + retry-after） |
| `src/utils/hash.ts` | `src/utils/hash.rs` | shortHash（cyrb53） |
| `src/utils/headers.ts` | `src/utils/headers.rs` | headers → record 转换 |
| `src/utils/pi-user-agent.ts` | `src/utils/pi-user-agent.rs` | pi User-Agent 字符串 |
| `src/utils/provider-env.ts` | `src/utils/provider-env.rs` | scoped env → 进程 env 解析 |
| `src/utils/sleep.ts` | `src/utils/sleep.rs` | 可中断睡眠 |
| `src/utils/sanitize-unicode.ts` | `src/utils/sanitize-unicode.rs` | 移除未配对代理字符（Rust 下恒等） |
| `src/utils/typebox-helpers.ts` | `src/utils/typebox-helpers.rs` | StringEnum JSON schema 构造 |
| `src/models.ts` | `src/models.rs` | Provider/Models 框架、calculateCost 等 |
| `src/providers/faux.ts` | `src/providers/faux.rs` | 测试用 provider（完整 stream 回放） |
| `src/api/openai-completions.ts` | `src/api/openai-completions.rs` | Chat Completions SSE 流式（兼容实现） |
| `src/api/openai-responses.ts` + `openai-responses-shared.ts` | `src/api/openai-responses.rs` | Responses API SSE 流式（openai + deepseek 主路径）+ 签名/ID 规范化 |
| `src/api/constrained-sampling.ts` | `src/api/constrained-sampling.rs` | 严格 JSON Schema 与 grammar 约束采样 |
| `src/api/simple-options.ts` | `src/api/simple-options.rs` | 基础流式选项 + thinking 预算 |
| `src/api/transform-messages.ts` | `src/api/transform-messages.rs` | 跨 provider 消息规范化（图片降级/thinking/toolCall ID/孤儿 toolCall） |
| `src/providers/openai.ts` | `src/providers/openai.rs` | OpenAI 工厂 + 模型 |
| `src/providers/deepseek.ts` | `src/providers/deepseek.rs` | DeepSeek 工厂 + 模型 |
| `src/auth/types.ts` | `src/auth/types.rs` | Credential/CredentialStore/AuthContext/ApiKeyAuth/OAuthAuth 等 |
| `src/auth/context.ts` | `src/auth/context.rs` | defaultProviderAuthContext |
| `src/auth/credential-store.ts` | `src/auth/credential-store.rs` | InMemoryCredentialStore |
| `src/auth/helpers.ts` | `src/auth/helpers.rs` | envApiKeyAuth / lazyOAuth |
| `src/auth/resolve.ts` | `src/auth/resolve.rs` | resolveProviderAuth + ModelsError |
| `src/auth/oauth/pkce.ts` | `src/auth/oauth/pkce.rs` | generatePKCE |
| `src/auth/oauth/device-code.ts` | `src/auth/oauth/device-code.rs` | pollOAuthDeviceCodeFlow |
| `src/auth/oauth/oauth-page.ts` | `src/auth/oauth/oauth-page.rs` | oauthSuccessHtml / oauthErrorHtml |
| （`lazyStream` 相关） | `src/utils/error-stream.rs` | 错误流构造 |

## 功能介绍

- **消息与内容类型**：`Message`（User/Assistant/ToolResult）、`ContentBlock`（Text/Thinking/Image/ToolCall）、`Usage`、`StopReason`，全部带 serde。
- **事件流**：`EventStream<T, R>`（生产者 `push`、消费者异步迭代、`result()` 取最终结果）与 `AssistantMessageEventStream`。
- **工具与校验**：`Tool`（JSON schema 参数）+ `validate_tool_arguments`（含类型强制转换 coercion）。
- **provider 框架**：`Provider`/`Models`/`create_provider`/`create_models`、`calculate_cost`、thinking level 支持。
- **真实流式**：`openai_responses_stream` 用 reqwest + SSE 解析 Responses API 事件（`response.output_item.added`/`*.delta`/`*.done`），累积 text/thinking/tool_call，映射成事件流；`openai_completions_stream` 保留为 Chat Completions 兼容实现。
- **测试 provider**：`faux` 把脚本化的 `AssistantMessage` 按完整事件序列回放，是 agent 单元测试的依赖。
- **认证核心**：凭据体系（`Credential` = ApiKey/OAuth）、`CredentialStore`（唯一写路径 `modify`，per-provider 串行化）、`AuthContext`（env/fileExists）、`resolve_provider_auth`（含 OAuth 双重检查锁定刷新）、`env_api_key_auth`/`lazy_oauth` 工厂、`InMemoryCredentialStore`、PKCE / RFC 8628 设备轮询 / OAuth 回调页等通用工具。

## 原理

1. **流协议**：所有流式请求统一返回 `AssistantMessageEventStream`，事件序列固定为 `start → text/thinking/toolcall 增量 → done|error`。失败编码进 `error` 事件 + `stopReason`，**不抛出**（这是 `StreamFn` 的契约）。
2. **EventStream 内部**：`mpsc::unbounded` channel 做事件队列，`oneshot`（`Shared`）承载最终结果，`AtomicBool done` 防重复终结；`EventStream` 可 clone（生产句柄与消费句柄分离）。
3. **工具参数 coercion**：LLM 给的参数经常是「字符串数字」「缺失可选字段」等，`validate_tool_arguments` 按 JSON schema 递归做类型强制转换（`coerce_with_json_schema`），再交给 `jsonschema` 校验。
4. **SSE 流式**：`openai-responses` 逐行解析 `data: {...}` 事件（`response.output_item.added`/`*.delta`/`*.done`/`response.completed`），按 output_index 维护 slot 累积 text/thinking/tool_call，并在 `output_item.done` 编码 text/thinking signature；`openai-completions` 则累积 `delta.content`/`delta.tool_calls`（arguments 是分片字符串），`finish_reason` 映射 `StopReason`。
5. **认证解析**：`resolve_provider_auth` 遵循「已存储凭据拥有 provider」原则——有存储用存储、无存储才查 ambient/env，refresh 失败后不做静默 env 回退；OAuth token 临近过期时走双重检查锁定（乐观检查 → `modify` 锁下复查过期 → 全局刷新一次 → 持久化轮换后的凭据），避免并发请求 double-refresh。
6. **凭据存储串行化**：`CredentialStore.modify` 是唯一写路径，`InMemoryCredentialStore` 按 `Provider.id` 用 per-provider 队列串行化，`resolve` 的 OAuth 刷新跑在 `modify` 锁内。
7. **AbortSignal**：基于 `CancellationToken` 的封装，`operation_signal`/`race_with_abort_signal` 让可选 signal 的公开 API 拥有 operation-local 取消，并保证被放弃的 operation 不会产生未处理错误。

## 复刻过程中的变化

1. **TypeBox → JSON Schema + jsonschema**：TS 的 `Tool.parameters` 是 TypeBox schema（含运行时校验 + 类型推断）。Rust 用 `serde_json::Value`（JSON schema）+ `jsonschema` crate，coercion 逻辑完整照搬。
2. **40+ provider → openai/deepseek/faux**：`providers/`（87 文件）只保留 3 个；`api/`（32 文件）保留 openai-responses（主）+ openai-completions + constrained-sampling + simple-options + transform-messages（通用层，其余为其他 provider 专属 api）。
3. **auth 核心已复刻，厂商 OAuth 流程省略**：`auth/` 的类型层、`resolve`（含 OAuth 双重检查锁定刷新）、`credential-store`、`helpers`、`context`、oauth 通用工具（pkce/device-code/oauth-page）均已 1:1 翻译；但各厂商的 OAuth 登录流程（anthropic/github-copilot/openrouter/xai/kimi-coding/openai-codex/radius，TS 中同样位于 `auth/oauth/` 下）未实现，按需后续补充。
4. **`ModelsError` 位置对齐 TS**：TS 中 `ModelsError`/`ModelsErrorCode` 定义于 `auth/resolve.ts`、由 `models.ts` re-export；Rust 同样在 `auth/resolve.rs` 定义（`code` 为 enum + `message` + `cause`），`models.rs` re-export。
5. **模型目录硬编码**：TS 从生成的 JSON 目录加载模型，Rust 硬编码常用模型（gpt-4o、deepseek-chat 等）。
6. **`parseStreamingJson` 的 partial-json 补全简化**：TS 依赖 `partial-json` 包，Rust 用 `repairJson` + 标准解析，失败返回 `{}`。
7. **openai/deepseek 统一走 responses**：TS 里 openai 用 `openai-responses`、deepseek 用 `openai-completions`；Rust 已对齐——openai 与 deepseek 都走 `openai-responses`（`POST /responses`），`openai-completions` 保留为兼容实现。
8. **compat 类型省略**：40+ provider 的兼容性配置（`OpenAICompletionsCompat` 等大量字段）未译，`Model.compat` 用 `serde_json::Value` 占位。
9. **流式事件回放复用**：网络层是真正的 SSE 流式读取，事件层复用 `faux` 的 `stream_with_deltas`（累积后按事件序列回放）。
10. **`OAuthCredential` 的 index signature**：TS 用 `[key: string]: unknown` 承载 `scope`/`accountId`/`enterpriseUrl` 等 provider 附加字段，Rust 用 `extra: BTreeMap<String, serde_json::Value>`（`serde(flatten)`）承载。

## 阅读步骤

1. `src/types.rs` —— 所有消息/内容/工具/事件/模型类型，先建立全貌（约 640 行）。
2. `src/utils/event-stream.rs` —— 流抽象，理解 `push`/`end`/`result` 与 Stream 实现。
3. `src/utils/validation.rs` —— 工具参数 coercion + 校验。
4. `src/models.rs` —— Provider/Models 框架、`calculate_cost`、thinking level。
5. `src/providers/faux.rs` —— 最清晰可运行的 stream 参考实现（不依赖 HTTP）。
6. `src/api/openai-responses.rs` —— Responses API SSE 流式（openai/deepseek 的 HTTP 主路径）+ 签名/ID 规范化（重点）。
7. `src/api/openai-completions.rs` —— Chat Completions 兼容流式。
8. `src/api/constrained-sampling.rs` / `simple-options.rs` / `transform-messages.rs` —— 约束采样、流式选项、消息转换。
9. `src/providers/openai.rs` / `deepseek.rs` —— provider 工厂与模型定义。
10. `src/auth/types.rs` —— 认证类型层（Credential/CredentialStore/AuthContext/ApiKeyAuth/OAuthAuth）。
11. `src/auth/resolve.rs` —— 认证解析入口 + OAuth 双重检查锁定刷新 + ModelsError。
12. `src/auth/credential-store.rs` —— InMemoryCredentialStore 的 per-provider 串行化写路径。
13. `src/auth/helpers.rs` + `context.rs` —— envApiKeyAuth / lazyOAuth 与默认上下文。
14. `src/auth/oauth/` —— pkce（SHA-256 挑战）、device-code（RFC 8628 轮询）、oauth-page（回调页 HTML）。
