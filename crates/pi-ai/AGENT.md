# pi-ai

统一多 provider 的 LLM API：消息/模型/工具类型、事件流抽象、工具参数校验、provider 框架，以及 openai / deepseek 的 SSE 流式实现。

## 复刻来源与范围

复刻自 [earendil-works/pi](https://github.com/earendil-works/pi) 的 `packages/ai`。

原版有 177 个文件（40+ provider 的 HTTP 实现、OAuth 认证、图像生成、模型目录等）。按「provider 不需要实现所有，但要有 openai 和 deepseek」的要求，本模块翻译**核心子集**（16 文件）：

| 原 TS 文件 | Rust 文件 | 说明 |
|---|---|---|
| `src/types.ts` | `src/types.rs` | 完整消息/内容/工具/模型/事件类型 |
| `src/utils/event-stream.ts` | `src/utils/event-stream.rs` | EventStream + 工厂 |
| `src/utils/validation.ts` | `src/utils/validation.rs` | 工具参数校验（完整 coercion 逻辑） |
| `src/utils/uuid.ts` | `src/utils/uuid.rs` | uuidv7 |
| `src/utils/text.ts` | `src/utils/text.rs` | contentText |
| `src/utils/json-parse.ts` | `src/utils/json-parse.rs` | repairJson / parseStreamingJson |
| `src/models.ts` | `src/models.rs` | Provider/Models 框架、calculateCost 等 |
| `src/providers/faux.ts` | `src/providers/faux.rs` | 测试用 provider（完整 stream 回放） |
| `src/api/openai-completions.ts` | `src/api/openai-completions.rs` | SSE 流式（openai + deepseek 共用） |
| `src/providers/openai.ts` | `src/providers/openai.rs` | OpenAI 工厂 + 模型 |
| `src/providers/deepseek.ts` | `src/providers/deepseek.rs` | DeepSeek 工厂 + 模型 |
| （`lazyStream` 相关） | `src/utils/error-stream.rs` | 错误流构造 |

## 功能介绍

- **消息与内容类型**：`Message`（User/Assistant/ToolResult）、`ContentBlock`（Text/Thinking/Image/ToolCall）、`Usage`、`StopReason`，全部带 serde。
- **事件流**：`EventStream<T, R>`（生产者 `push`、消费者异步迭代、`result()` 取最终结果）与 `AssistantMessageEventStream`。
- **工具与校验**：`Tool`（JSON schema 参数）+ `validate_tool_arguments`（含类型强制转换 coercion）。
- **provider 框架**：`Provider`/`Models`/`create_provider`/`create_models`、`calculate_cost`、thinking level 支持。
- **真实流式**：`openai_completions_stream` 用 reqwest + SSE 解析，把 delta 累积成 text/tool_calls，再回放成事件流。
- **测试 provider**：`faux` 把脚本化的 `AssistantMessage` 按完整事件序列回放，是 agent 单元测试的依赖。

## 原理

1. **流协议**：所有流式请求统一返回 `AssistantMessageEventStream`，事件序列固定为 `start → text/thinking/toolcall 增量 → done|error`。失败编码进 `error` 事件 + `stopReason`，**不抛出**（这是 `StreamFn` 的契约）。
2. **EventStream 内部**：`mpsc::unbounded` channel 做事件队列，`oneshot`（`Shared`）承载最终结果，`AtomicBool done` 防重复终结；`EventStream` 可 clone（生产句柄与消费句柄分离）。
3. **工具参数 coercion**：LLM 给的参数经常是「字符串数字」「缺失可选字段」等，`validate_tool_arguments` 按 JSON schema 递归做类型强制转换（`coerce_with_json_schema`），再交给 `jsonschema` 校验。
4. **SSE 流式**：逐行解析 `data: {...}`，累积 `delta.content` 与 `delta.tool_calls`（arguments 是分片字符串），`finish_reason` 映射到 `StopReason`，`usage` 从最后一个 chunk 提取。

## 复刻过程中的变化

1. **TypeBox → JSON Schema + jsonschema**：TS 的 `Tool.parameters` 是 TypeBox schema（含运行时校验 + 类型推断）。Rust 用 `serde_json::Value`（JSON schema）+ `jsonschema` crate，coercion 逻辑完整照搬。
2. **40+ provider → openai/deepseek/faux**：`providers/`（87 文件）只保留 3 个；`api/`（32 文件）只保留 openai-completions。
3. **auth 省略**：OAuth/CredentialStore（16 文件）改为 provider 工厂直读环境变量（`OPENAI_API_KEY`/`DEEPSEEK_API_KEY`）。
4. **模型目录硬编码**：TS 从生成的 JSON 目录加载模型，Rust 硬编码常用模型（gpt-4o、deepseek-chat 等）。
5. **`parseStreamingJson` 的 partial-json 补全简化**：TS 依赖 `partial-json` 包，Rust 用 `repairJson` + 标准解析，失败返回 `{}`。
6. **openai 走 chat.completions**：TS 里 openai 用 `openai-responses`（新 API）、deepseek 用 `openai-completions`；Rust 统一用 OpenAI Chat Completions 兼容端点。
7. **compat 类型省略**：40+ provider 的兼容性配置（`OpenAICompletionsCompat` 等大量字段）未译，`Model.compat` 用 `serde_json::Value` 占位。
8. **流式事件回放复用**：网络层是真正的 SSE 流式读取，事件层复用 `faux` 的 `stream_with_deltas`（累积后按事件序列回放）。

## 阅读步骤

1. `src/types.rs` —— 所有消息/内容/工具/事件/模型类型，先建立全貌（约 640 行）。
2. `src/utils/event-stream.rs` —— 流抽象，理解 `push`/`end`/`result` 与 Stream 实现。
3. `src/utils/validation.rs` —— 工具参数 coercion + 校验。
4. `src/models.rs` —— Provider/Models 框架、`calculate_cost`、thinking level。
5. `src/providers/faux.rs` —— 最清晰可运行的 stream 参考实现（不依赖 HTTP）。
6. `src/api/openai-completions.rs` —— 真实 SSE 流式（openai/deepseek 的 HTTP 层）。
7. `src/providers/openai.rs` / `deepseek.rs` —— provider 工厂与模型定义。
