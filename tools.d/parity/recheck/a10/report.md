# pi-ai 1:1 复刻审计报告（recheck a10）

- 基线：upstream `earendil-works/pi` v1.1.0 commit abe508e1b（`/Users/pokersu/Projects/pi_rs/upstream/packages/ai/src/`）
- Rust 侧：`/Users/pokersu/Projects/pi_rs/crates/pi-ai/src/`
- 范围：api/（constrained-sampling、openai-completions、openai-responses(+shared)、simple-options、transform-messages）+ providers/（faux、openai、deepseek）+ auth/（types、context、credential-store、helpers、resolve、oauth/pkce、oauth/device-code、oauth/oauth-page）+ lib.rs vs index.ts
- 判定分类：`缺失` / `多余` / `逻辑差异` / `命名差异`(不算问题) / `豁免`(语言机制等价差异，需说明)
- 注意：工具输出对疑似密钥/常量的行做了显示级 redaction（`[redacted]`）。凡涉及 redacted 常量/表达式的比对点一律列入「不可核对项」，不计入差异数。

## 结论

**VERDICT: FAIL（非 1:1）**。transform-messages、simple-options、pkce、device-code、auth/types、auth/context、auth/helpers、credential-store 基本 1:1；openai-completions.rs 与 faux.rs 是显式声明（文件头注释）的简化版，缺失大量上游逻辑；openai-responses.rs 主路径大体对齐但存在若干真实逻辑差异；auth/resolve.rs 的 OAuth 刷新存在信号语义差异。

## 一、api/constrained-sampling.ts → constrained-sampling.rs

对齐：`UNSUPPORTED_STRICT_SCHEMA_KEYS` 列表、`isStructuredSchema`、`schemaAllowsNull`、`makeJsonSchemaNodeStrict` 的检查顺序与错误文案、`makeStrictJsonSchema`、`resolveJsonSchemaStrictSampling`（含 `strict !== "require"` → undefined 与 require 报错文案）、`resolveGrammarConstrainedSampling`（lark 优先、trim 判空、错误文案）、`inferGrammarInputProperty`、`getGrammarToolInput` 全部一致。

| # | 分类 | 差异 |
|---|------|------|
| CS-1 | 逻辑差异 | `make_json_schema_node_strict`：TS 对 `schema.items` **原地**递归（array-of-object 时 items 的 properties/required/additionalProperties 被改写）；Rust `let mut items = items.clone()` 递归后**丢弃**，`schema` 的 items 不变。`{type:"array", items:{type:"object",...}}` 输出 schema 不同。anyOf 分支同为 clone 丢弃，但 anyOf 变体被禁止为结构化 schema，无观察差异（仅理论）。 |
| CS-2 | 逻辑差异 | `get_json_schema_tool_parameters`：TS `strict===true ? makeStrictJsonSchema(...)` **抛** UnsupportedStrictJsonSchemaError；Rust `unwrap_or_else(|_| tool.parameters.clone())` **静默回退**原参数。错误处理不等价。 |
| CS-3 | 逻辑差异 | `create_grammar_tool_input_properties`：TS 直接调用 resolve（无变体时**抛出**错误，循环中止）；Rust `if let Ok(Some(..))` **吞掉**错误继续。 |
| CS-4 | 逻辑差异(边缘) | `append_grammar_tool_input_json_delta`：起始头 TS 为 `{${JSON.stringify(inputProperty)}:"`（属性名转义）；Rust `format!("{{\"{input_property}\":\"")` 不转义。含 `"`/`\` 的属性名输出不同。 |
| CS-5 | 豁免 | anyOf 变体 clone 丢弃（见 CS-1 说明，无观察差异）；错误以 `Result<_, String>` 表达 vs TS throw（语言机制等价）。 |

## 二、api/openai-completions.ts → openai-completions.rs

Rust 文件头自述「简化版：基础 text + tool call，省略 thinking/cache/compat/deferred」。以下为逐项核对结果：

| # | 分类 | 差异 |
|---|------|------|
| CC-1 | 缺失 | thinking 全链路：reasoning_fields（reasoning_content/reasoning/reasoning_text）三字段探测、ensureThinkingBlock、thinking_delta/thinking_end 事件、opencode-go 的 "reasoning"→"reasoning_content" 签名改写、reasoning_details 合并（appendOpenAIReasoningDetail/fillMissingCommonReasoningDetailFields）、加密 reasoning 解析（parseOpenAIReasoningDetails / parseLegacyEncryptedReasoningDetail）。 |
| CC-2 | 缺失 | prompt cache：resolveCacheRetention(env)、prompt_cache_key/prompt_cache_retention（clampOpenAIPromptCacheKey）、cacheControlFormat（applyAnthropicCacheControl 一族）、session affinity headers（x-session-id / session_id / x-client-request-id / x-session-affinity）。 |
| CC-3 | 缺失 | 重试与取消：retryProviderRequest、maxRetries/maxRetryDelayMs、signal、timeoutMs；aborted 检查（`options.signal.aborted` / stopReason==="aborted" 抛错）。 |
| CC-4 | 缺失 | 工具转换高级分支：convertTools 的 grammar（type:"custom"）与 strict 分支、zaiToolStream、grammarToolInputProperties 注入、custom tool call input 累积（appendCustomToolCallInput）；Rust build_tools 恒为纯 function 工具且无 strict。 |
| CC-5 | 缺失 | `hasToolHistory` → `tools: []`（Anthropic via proxy 兼容）。 |
| CC-6 | 缺失 | `requiresAssistantAfterToolResult` 合成 assistant 消息（"I have processed the tool results."）。 |
| CC-7 | 缺失 | toolResult 图片处理："(see attached image)"/"(no tool output)" 占位、图片以 user 消息回注（"Attached image(s) from tool result:"）、requiresToolResultName.name 字段。 |
| CC-8 | 缺失 | maxTokensField 选择（max_tokens vs max_completion_tokens）、store=false、stream_options 条件化（supportsUsageInStreaming）、vllmPriority、openRouterRouting/vercelGatewayRouting providerOptions、samplingParams 合并（resolveSamplingParams）。 |
| CC-9 | 缺失 | thinking 请求参数全家族：zai/qwen/qwen-chat-template/chat-template/baseten/deepseek/openrouter/ant-ling/together/string-thinking/reasoning_effort、thinking budget（resolveClampedThinkingBudget/thinkingBudgetForLevel/clampThinkingBudgetToAnswerRoom）、chat_template_kwargs/args。 |
| CC-10 | 缺失 | responseId（`output.responseId ||= chunk.id`）、responseModel、choice.usage 回退（Moonshot）。 |
| CC-11 | 缺失 | 错误消息拼装：formatProviderError + normalizeProviderError + rawMetadata 追加。 |
| CC-12 | 逻辑差异 | `map_finish_reason`：TS `"end"`→stop（Rust 默认分支碰巧同果）、`"function_call"`→toolUse（Rust→**Stop**）、`"content_filter"`/`"network_error"`→error（Rust→**Stop**）、未知值→error+`Provider finish_reason: X`（Rust→**Stop**）。 |
| CC-13 | 逻辑差异 | 无 finish_reason 回退：TS 在 `!compat.supportsFinishReason` 时按内容含 toolCall 置 toolUse/stop；Rust 恒 Stop。 |
| CC-14 | 逻辑差异 | 错误/异常传播：TS 对 error stop reason 抛错、`supportsFinishReason && !hasFinishReason` 抛 "Stream ended without finish_reason"、stopReason==="pending" 抛错；Rust 一律 Done。 |
| CC-15 | 逻辑差异 | sanitize：TS 对 user 文本 content、user 文本块、assistant 文本块都调 sanitizeSurrogates；Rust 全不调。 |
| CC-16 | 逻辑差异 | user 块过滤：TS 过滤空文本块（`item.type!=="text" || item.text.length>0`）；Rust 不过滤。 |
| CC-17 | 逻辑差异 | assistant 消息：TS 过滤空白 text 块、无内容且无 tool_calls 时跳过；Rust 无条件 push（空 content 也会发送）。 |
| CC-18 | 逻辑差异 | 事件流：TS 逐 chunk 增量 text_delta/thinking_delta/toolcall_delta + start/done；Rust 缓冲全量后经 faux 的 `stream_with_deltas` 一次性回放（每块单 delta，无增量事件、无 partialArgs 解析事件）。 |
| CC-19 | 逻辑差异 | 事件 partial 时序：Rust `stream_with_deltas` 的 Text/ThinkingDelta 事件中 `partial` **不含**本次增量（TS 先追加再 push）。 |
| CC-20 | 逻辑差异 | usage：TS parseChunkUsage 解析 prompt_tokens_details.cached_tokens/prompt_cache_hit_tokens/cached_tokens、cache_write_tokens、reasoning_tokens，input=max(0,prompt−cacheRead−cacheWrite)、calculateCost；Rust 仅 prompt/completion/total 三字段，cost 恒 0。 |
| CC-21 | 逻辑差异 | api key：TS getClientApiKey 支持 header-auth（authorization / cf-aig-authorization → "unused"），报错文案 "No API key for provider: X"；Rust 由上层注入，报错 "Missing API key for provider: X"（文案不同，无 header 路径）。 |
| CC-22 | 逻辑差异 | 工具调用 ID 规范化：TS convertMessages 内 normalizeToolCallId（pipe 拆分、40 字符、shortHash 回退）；Rust 直接透传 id。 |
| CC-23 | 逻辑差异 | instructionRole：TS `model.reasoning && compat.supportsDeveloperRole`，其中 detectCompat 对 openrouter 非 anthropic/openai 及非标 provider 为 false；Rust get_compat 默认 true 恒成立。 |
| CC-24 | 缺失 | github-copilot 动态头（buildCopilotDynamicHeaders/hasCopilotVisionInput）、User-Agent/headers 合并。 |
| CC-25 | 缺失 | streamSimple 的 reasoningEffort clamp（clampThinkingLevel→off 时置 undefined）、thinkingBudgets 透传。 |
| CC-26 | 豁免 | detectCompat/getCompat 全量（30+ 字段）在 Rust 收缩为 3 字段 Compat——按 brief「compat 类型 serde_json::Value 占位属豁免」处理；其行为后果（CC-12/CC-13/CC-23 等）已单独计入逻辑差异。 |
| CC-27 | 不可核对 | max_tokens/max_completion_tokens 赋值、thinkingTokenBudgetField、usage.total_tokens 等 redacted 表达式。 |

## 三、api/openai-responses.ts + openai-responses-shared.ts → openai-responses.rs

对齐项（核对通过）：`encodeTextSignatureV1`/`parseTextSignature`（phase 白名单 commentary/final_answer）、`convertToolResultOutput`（占位文案、input_image detail:"auto"）、`normalizeIdPart`/`buildForeignResponsesItemId`/`normalizeToolCallId` 主体、`detectSessionAffinityFormat`、`getPromptCacheRetention`（24h）与 `getPromptCacheOptions`（explicit/ttl 30m）、instructionRole 公式、user 消息结构、reasoning item done 的 summary/content 取字与 "\n\n" 分隔、output_text/refusal delta、function_call_arguments.delta 累积、mapStopReason 的 completed/incomplete(max_output_tokens→length)/failed/cancelled/in_progress/queued 分支、toolUse 覆盖、sawTerminal 检查与 "OpenAI Responses stream ended before a terminal response event"。

| # | 分类 | 差异 |
|---|------|------|
| OR-1 | 逻辑差异 | `append_system_tool_additions`：TS tool-search call_id 为 `pi_tool_load_${shortHash(\`${seed}:${names.join(",")}\`)}`（seed=`system:${msgIndex}`）；Rust `short_hash(&names.join(","))` **缺 seed** → call_id 与上游不匹配，跨端回放配对会断。 |
| OR-2 | 多余 | Rust append_system_tool_additions 按工具名 `loaded_names` 去重；TS 不去重（每次 system 消息全量输出 toolsAdded）。 |
| OR-3 | 逻辑差异 | tool_search_output 的 tools：TS `convertResponsesTools(tools, {...toolOptions, toolSearchResult:true})` → 每个工具带 `defer_loading: true`；Rust `convert_tools(&tools, compat)` 无 defer_loading。 |
| OR-4 | 逻辑差异 | sanitize 缺失：TS 对 user 字符串内容、user 文本块、assistant output_text 均 sanitizeSurrogates；Rust 全部未调（toolResult 输出有调）。 |
| OR-5 | 逻辑差异 | assistant thinking 回放：TS `JSON.parse(block.thinkingSignature)` 失败会**抛出**；Rust `if let Ok(item)=serde_json::from_str` **静默跳过**。 |
| OR-6 | 逻辑差异 | toolCall 回放 item id 丢弃条件：TS `isDifferentModel || !itemId?.startsWith(itemIdPrefix)`（isDifferentModel 恒丢弃；custom 检查 "ctc_" 前缀）；Rust 仅 `(is_different_model && item_id 以 fc_ 开头) || (无 custom && item_id 不以 fc_ 开头)`——不同模型且非 fc_ 前缀时 Rust 保留、custom 且不以 ctc_ 开头时 Rust 保留。 |
| OR-7 | 逻辑差异 | 消息索引：TS msgIndex 不计首条 leading system、空内容 continue 时不递增；Rust 用转换后数组下标 → transcript 以 system 开头时 fallback id `msg_pi_N` 偏移 1、空 assistant 消息后编号不同。 |
| OR-8 | 逻辑差异 | create_slot(custom_tool_call)：TS arguments 键 = `grammarToolInputProperties.get(name) ?? "input"`；Rust 恒 `{"input": input}`。 |
| OR-9 | 逻辑差异 | create_slot(function_call)：TS id 恒 `${call_id}|${item.id}`（空 item.id 得 "call_id|"）；Rust item_id 为空时仅 call_id。 |
| OR-10 | 逻辑差异 | `response.function_call_arguments.done`：TS 仅当 `event.arguments.startsWith(previousPartialJson)` 时推增量；Rust 不匹配时推**完整** arguments 作为 delta。 |
| OR-11 | 缺失 | `response.custom_tool_call_input.delta` / `.done` 无任何处理分支 → 自定义工具输入增量丢失、custom 工具永不发出 toolcall_end、slot 不清理。 |
| OR-12 | 逻辑差异 | function_call 的 output_item.done：TS `parseStreamingJson(item.arguments || partialJson || "{}")` 并设置 `item.namespace`；Rust 用 `item.arguments 为字符串` 否则保留累积值（不回落 "{}"）、**不**设置 done 事件的 namespace（仅 create_slot 时）。 |
| OR-13 | 逻辑差异 | message 的 output_item.done：content 缺失时 TS 置 `""`、Rust 保留累积文本；textSignature 的 id 缺失时 TS 输出 `{"v":1}`（id 键省略）、Rust 输出 `{"v":1,"id":""}`。 |
| OR-14 | 逻辑差异 | map_stop_reason 默认分支：TS `throw new Error(\`Unhandled stop reason: ${status}\`)`；Rust `_ => (Stop, None)` 静默当 stop。 |
| OR-15 | 逻辑差异 | rawStopReason：TS `${status}.${incompleteReason}`（如 "incomplete.max_output_tokens"）；Rust 仅 status。 |
| OR-16 | 逻辑差异 | 终态：TS 对 stopReason pending/aborted/error 抛错走 error 事件；Rust `_ => TerminalStopReason::Stop` 静默吞掉；toolUse 时未完成 tool call（partialJson/customInput 残留）检查缺失。 |
| OR-17 | 逻辑差异 | usage：TS input=max(0,input_tokens−cached−cacheWrite)（cacheWrite 取自 input_tokens_details）、calculateCost；Rust 仅减 cached、cache_write 恒 0、cost 恒 0。 |
| OR-18 | 缺失 | reasoning encrypted_content backfill（Azure #6409，reasoningBlocksById + response.completed.output 回填）。 |
| OR-19 | 逻辑差异 | "error" 事件文案：TS 缺 code/message 时为 "undefined"；Rust 为 "unknown"。response.failed 的 incomplete_details 分支（`incomplete: ${reason}`）缺失。 |
| OR-20 | 逻辑差异 | prompt_cache_key：sessionId 缺省且 retention≠none 时 TS 省略字段；Rust 发送 `"prompt_cache_key": null`。 |
| OR-21 | 缺失 | isChatGPTSignIn / omitUnsupportedFields 门控（ChatGPT 订阅凭据会收到 max_output_tokens/temperature/prompt_cache_* 等被拒字段）、CHATGPT_USAGE_URL 订阅错误增强、formatProviderError 的 `OpenAI API error` 前缀。 |
| OR-22 | 逻辑差异 | service_tier：TS 为顶层 option；Rust 从 sampling_params JSON 提取。且 TS resolveSamplingParams 结果全量 Object.assign 进 params；Rust 仅取 service_tier，其余 sampling params 丢弃。 |
| OR-23 | 逻辑差异 | `get_service_tier_cost_multiplier`：TS `case "priority": case "fast":` 同值（gpt-5.5→2.5 else 2）；Rust 仅匹配 "priority"，**"fast" 落入 1.0**。 |
| OR-24 | 逻辑差异 | resolve_cache_retention：TS 经 `getProviderEnvValue("PI_CACHE_RETENTION", options.env)`（可注入 env）；Rust 直接 `std::env::var`，忽略 ProviderEnv。 |
| OR-25 | 缺失 | reasoning 请求参数：effort/summary（reasoningSummary 默认 "auto"）、include ["reasoning.encrypted_content"]、xai include、`model.thinkingLevelMap?.off` 分支。 |
| OR-26 | 逻辑差异 | convert_tools：TS 仅 supportsStrictMode 时携带 strict 字段；Rust **恒发** `"strict": false`。grammar resolve 错误 TS 抛出、Rust `if let Ok` 吞掉。 |
| OR-27 | 缺失 | tool_choice、maxRetries/signal/timeoutMs、github-copilot 动态头、session affinity headers（createClient 头构建）。 |
| OR-28 | 缺失 | applyMessagePhaseStopReason（message item phase==="final_answer" → stopReason="stop"，create_slot 与 output_item.done 两处）。 |
| OR-29 | 逻辑差异 | SSE 解析：TS 走 OpenAI SDK；Rust 手写解析仅取每个事件的**最后一条** data 行（多行 data 事件会丢行）、`[DONE]` 直接 break（TS 依赖 SDK 行为）。 |
| OR-30 | 逻辑差异(边缘) | reasoning signature 序列化：serde_json 默认 BTreeMap 键序（字母序）vs TS JSON.stringify 保留服务端字段序 → signature 字符串字节序不同（语义等价）。 |
| OR-31 | 不可核对 | OPENAI_RESPONSES_MIN_OUTPUT_TOKENS、max_output_tokens clamp 表达式、usage.total_tokens 等 redacted 段。 |

## 四、api/simple-options.ts → simple-options.rs

| # | 分类 | 差异 |
|---|------|------|
| SO-1 | 缺失 | `buildBaseOptions`：TS 携带 telemetryContext、fetch、env 字段；Rust StreamOptions 无这三项。 |
| SO-2 | 逻辑差异(边缘) | `thinkingBudgetForLevel`：TS `{...DEFAULT, ...customBudgets}`，custom 显式 `undefined` 会覆盖默认并返回 undefined；Rust `unwrap_or(默认)` 回退。 |
| SO-3 | 对齐 | clampMaxTokensToContext（contextWindow<=0 → max(MIN,maxTokens)；available 饱和减）、resolveSamplingParams 合并顺序、clampReasoning、clampThinkingBudgetToAnswerRoom、adjustMaxTokensForThinking 形状、DEFAULT_THINKING_BUDGETS 数值(1024/2048/8192/16384) 一致。 |
| SO-4 | 不可核对 | MIN_MAX_TOKENS/CONTEXT_SAFETY_TOKENS/MIN_ANSWER_TOKENS 常量值与 maxTokens 表达式被 redacted。 |

## 五、api/transform-messages.ts → transform-messages.rs

| # | 分类 | 差异 |
|---|------|------|
| TM-1 | 逻辑差异(边缘) | thoughtSignature 剥离：TS 真值判断（`""` 不剥离）；Rust `is_some()`（`Some("")` 也剥离）。 |
| TM-2 | 逻辑差异(边缘) | toolCallId 映射应用：TS `normalizedId &&`（空串不应用）；Rust `Some("")` 也会替换。 |
| TM-3 | 豁免 | `msg.content == null → []` 归一化由 Rust 类型系统保证（文件内注释同述）。 |
| TM-4 | 对齐 | 图片降级两占位符文案、placeholder 去重（previousWasPlaceholder）、redacted thinking 跨模型丢弃、空 thinking 丢弃、跨模型 thinking→text、synthetic tool result（"No result provided"、isError:true）、held system 消息、error/aborted assistant 跳过、两遍处理顺序——全部一致。 |

## 六、providers/faux.ts → faux.rs

Rust 文件头自述「省略 deferred/token 速率控制」。

| # | 分类 | 差异 |
|---|------|------|
| FX-1 | 缺失 | deferred 全链路：RegisterFauxProviderOptions.deferred（pendingFetches/pollAfterMs）、DeferredHandle 生成、deferredResponses 表、fetchDeferred/cancelDeferred、cancelledDeferred 语义、createDeferredMessage（stopReason:"deferred"）。 |
| FX-2 | 缺失 | token 速率控制：tokensPerSecond/tokenSize{min,max}、splitStringByTokenSize、scheduleChunk、DEFAULT_MIN/MAX_TOKEN_SIZE。 |
| FX-3 | 缺失 | withUsageEstimate（promptCache 会话前缀缓存、usage 估算、cacheRead/cacheWrite）。 |
| FX-4 | 缺失 | stream 中的 onResponse({status:200,headers:{}}) 回调；Rust stream 忽略 options。 |
| FX-5 | 缺失 | fauxAssistantMessage 的 options（deferred/errorMessage/responseId/timestamp）与 string\|block\|blocks 归一化输入。 |
| FX-6 | 缺失 | FauxResponseFactory 为异步（TS Promise）；Rust Box<dyn Fn> 同步。 |
| FX-7 | 缺失 | FauxProviderHandle 的 api/models/getModel/appendResponses；fauxProvider 参数（TS 收 RegisterFauxProviderOptions，Rust 收 models: Vec<Model>）。 |
| FX-8 | 逻辑差异 | `fauxToolCall` 默认 id：TS `tool:${Date.now()}:${Math.random().toString(36).slice(2)}`；Rust `tool-{uuidv7}`（唯一性等价，格式不同）。 |
| FX-9 | 逻辑差异 | stream_with_deltas：TS stopReason==="pending" **抛** "Faux response ended without a stop reason"；Rust 映射为 Done(Stop)。 |
| FX-10 | 逻辑差异 | stream_with_deltas 事件 partial：TS 先累积再 push delta（partial 含增量）；Rust push 时 partial 内容仍为空。 |
| FX-11 | 逻辑差异 | 错误路径：TS "No more faux responses queued" 消息经 withUsageEstimate 包装；Rust 直接 create_error_message（无 usage 估算）——文案一致。 |
| FX-12 | 多余 | Rust 额外导出 `faux_default_model`（TS 无）。 |
| FX-13 | 对齐 | 常量 DEFAULT_API/PROVIDER/MODEL_ID/NAME/BASE_URL、默认模型（reasoning:false、input text+image、contextWindow 128000、cost 全 0）、setResponses 覆盖/appendResponses 追加/getPendingResponseCount、callCount 递增、error 事件 reason 映射、createErrorMessage 文案逻辑一致。 |

## 七、providers/openai.ts → openai.rs、providers/deepseek.ts → deepseek.rs

| # | 分类 | 差异 |
|---|------|------|
| PV-1 | 缺失 | openai：OAuth（lazyOAuth ChatGPT 订阅、loadOpenAIChatGPTOAuth）、classifiers（openai-decisions）、filterAllModels（oauth 凭据剔除 classifier 模型）。Rust oauth: None。 |
| PV-2 | 豁免(声明) | 模型目录：TS 全量 OPENAI_MODELS/OPENAI_CLASSIFIER_MODELS/DEEPSEEK_MODELS；Rust 各硬编码 2 个模型——brief 明示「模型目录硬编码属豁免」。 |
| PV-3 | 豁免(声明) | deepseek：TS deepseekProvider 用 `openAICompletionsApi()`（Chat Completions），Rust 用 `openai_responses_stream`（Responses）。brief 明示「openai/deepseek 都走 responses 属对齐」按豁免处理；注意该豁免掩盖了 completions 与 responses 两个主路径的全部行为差异（见 CC/OR 两节）。 |
| PV-4 | 对齐 | id/name/baseUrl、envApiKeyAuth 名称（"OpenAI API key"/"DeepSeek API key"）与 env 列表（OPENAI_API_KEY / DEEPSEEK_API_KEY）一致。 |

## 八、auth/types.ts → types.rs

| # | 分类 | 差异 |
|---|------|------|
| AT-1 | 缺失 | OAuthAuth.login 的 LoginOptions 参数（getDeviceId/agentName）——Rust trait 无此参数（openai-chatgpt 流需要 deviceId，但该实现不在本子集）。 |
| AT-2 | 豁免 | OAuthCredential 的 index signature → `extra: BTreeMap<String, Value>` + 访问器；CredentialStore.modify 回调 Result 化；ApiKeyAuth.login 可选 → trait 默认实现返回错误（机制等价）。 |
| AT-3 | 对齐 | 其余接口（ModelAuth/ApiKeyCredential/CredentialInfo/AuthOperationOptions/AuthResult/AuthCheck/AuthPrompt 四变体/AuthEvent 四变体/AuthInteraction/ProviderAuthInteraction/ApiKeyAuth.check/resolve/OAuthAuth.refresh/toAuth）字段与文案一致。 |

## 九、auth/context.ts → context.rs

| # | 分类 | 差异 |
|---|------|------|
| AC-1 | 对齐 | env（trim 空 → None）、fileExists（"~" 前缀展开）语义一致。 |
| AC-2 | 豁免 | node:os.homedir() vs HOME 环境变量（平台差异，机制等价）。 |

## 十、auth/credential-store.ts → credential-store.rs

| # | 分类 | 差异 |
|---|------|------|
| CS-1 | 逻辑差异(边缘) | `list` 顺序：TS Map 插入序；Rust BTreeMap 按 providerId 排序。 |
| CS-2 | 逻辑差异(边缘) | 链清理：TS 任务结束后删除 chains 条目；Rust 条目永驻（仅内存差异）。 |
| CS-3 | 对齐 | per-provider 串行化、锁内 abort 检查、modify 的 current→fn→abort 检查→写入顺序、`next ?? current` 返回、delete 走同一队列、read/list 先 abort 检查——一致。 |
| CS-4 | 对齐 | modify 是唯一写路径（trait 文档与实现均满足）。 |

## 十一、auth/helpers.ts → helpers.rs

| # | 分类 | 差异 |
|---|------|------|
| AH-1 | 对齐 | envApiKeyAuth：login 的 abort 检查（前后）、`Enter ${name}` 文案、resolve 顺序（credential.key → env 循环、每步 abort 检查）、source 文案（"stored credential"/env 名）、env 仅随 stored 分支——一致。 |
| AH-2 | 对齐 | lazyOAuth：name/isSubscription/loginLabel 透传、惰性加载缓存（promise vs OnceCell）——一致。 |

## 十二、auth/resolve.ts → resolve.rs（含 ModelsError）

| # | 分类 | 差异 |
|---|------|------|
| AR-1 | 逻辑差异 | OAuth refresh 信号：TS `oauth.refresh(current, AbortSignal.timeout(15000))`——**刷新本身忽略调用方 signal**（文档明示）；Rust `AbortSignal::any([signal, timeout])`——调用方 abort 可取消刷新。 |
| AR-2 | 逻辑差异 | 取消语义：TS raceWithAbortSignal 仅停止等待，被放弃的 promise 继续执行（refresh 完成并持久化）；Rust race_with_abort_signal 用 tokio::select! **丢弃** operation future → 取消会中止 modify 闭包，刷新结果不持久化。与 TS「refresh 及其持久化忽略 signal」契约相反。 |
| AR-3 | 逻辑差异(边缘) | apiKey override：TS 仅检查 overrides.apiKey（provider.auth.apiKey 缺失时经 resolve 调用失败包装为 ModelsError auth）；Rust 要求 apiKeyAuth 存在，缺失时**跳过 override** 走 stored/ambient。 |
| AR-4 | 逻辑差异(边缘) | overlayEnvAuthContext：TS `env[name] || base`（空串回退 base）；Rust `Some("")` 直接返回。 |
| AR-5 | 缺失 | `refreshStoredOAuthCredential` 独立导出函数（Rust 内联进 resolve_stored_oauth，锁内 needsRefresh 复查逻辑本身保留）。 |
| AR-6 | 对齐 | 存储优先（无存储才 ambient/env）、stored api_key 的 env 合并、无对应 handler 返回 undefined、refresh 失败不静默 env 回退、双重检查（锁内复查 expiresSoon）、minOAuthValidityMs = max(5min, override)、refresh 后仍过期抛 "expires too soon"（仅显式 override 时）、ModelsError 包装文案（"OAuth refresh failed for X" / "Credential store modify failed for X" / "Credential store read failed for X" / "API key auth failed for provider X" / "OAuth auth derivation failed for X"）与 ModelsErrorCode 六值、withCauseDetail 拼接——一致。 |

## 十三、auth/oauth/pkce.ts → pkce.rs、device-code.ts → device-code.rs、oauth-page

| # | 分类 | 差异 |
|---|------|------|
| OD-1 | 对齐 | pkce：32 随机字节、base64url no-pad、SHA-256(verifier 字符串)——一致（豁免：Web Crypto vs rand+sha2）。 |
| OD-2 | 对齐 | device-code：常量文案（CANCEL/TIMEOUT/SLOW_DOWN_TIMEOUT）、MINIMUM_INTERVAL_MS=1000、DEFAULT_POLL_INTERVAL_SECONDS=5、SLOW_DOWN_INTERVAL_INCREMENT_MS=5000、deadline 计算、slow_down 的服务器 interval 优先逻辑、abortableSleep 语义、超时消息选择——一致。 |
| OD-3 | 逻辑差异 | oauth-page 的 LOGO_SVG：TS 为三色 logo（#F09082/#4D9ABF/#F1BE58 三个 path）；Rust 为单色白 fill 单 path。字符串字面量不一致。 |
| OD-4 | 命名差异 | 位置：TS 实际位于 `utils/oauth-page.ts`（非 brief 所列 auth/oauth/oauth-page.ts）；Rust 置于 auth/oauth/oauth-page.rs。不算逻辑问题。 |
| OD-5 | 对齐 | escapeHtml 顺序与五组映射、renderPage 的 CSS/结构、oauthSuccessHtml/oauthErrorHtml 标题文案——一致。 |

## 十四、index.ts → lib.rs（导出面对齐）

| # | 分类 | 差异 |
|---|------|------|
| IX-1 | 缺失 | providers/faux 仅部分 re-export：`faux_thinking`、`FauxProviderState`、`FauxResponseStep`、`FauxModelDefinition`、`FauxResponseFactory`、`FauxProviderHandle` 未从 lib.rs 导出（TS `export *`）。 |
| IX-2 | 多余 | lib.rs 导出 `auth::resolve::{resolve_provider_auth, ModelsError, ModelsErrorCode, ProviderAuthRef, AuthResolutionOverrides}`——TS index.ts 不导出 auth/resolve（内部模块）。 |
| IX-3 | 缺失 | TS 导出 `utils/diagnostics`（export *）、`utils/assistant-message-frame`；Rust lib.rs 未导出 diagnostics。 |
| IX-4 | 多余 | lib.rs 导出 `utils/error_stream::{create_error_message, default_usage, stream_error}`——TS index.ts 不导出 error-stream。 |
| IX-5 | 缺失 | TS 的 api 层选项类型导出（OpenAICompletionsOptions、OpenAIResponsesOptions、Anthropic/Bedrock/Google/Mistral/Codex/PiMessages 等类型）在 lib.rs 无对应（Rust 选项形状并入 SimpleStreamOptions；本子集外的 api 不计）。 |
| IX-6 | 缺失(子集外) | models-store、session-resources、compat/extension-oauth-types、api/lazy、typebox Type/Static/TSchema（Rust 以 string_enum/StringEnumOptions 代替）——超出声明子集，仅记录。 |
| IX-7 | 对齐 | auth/context、credential-store、helpers、auth/types、types、models 核心、utils/json-parse、overflow、retry、text（contentText 对应 content_text）、transcript、uuid（uuidv7）、validation 均对齐（命名差异不计）。 |

## 汇总

| 分类 | 数量 |
|------|------|
| 逻辑差异 | 44（CS-1..4、CC-12..23、OR-1,3..10,12..17,19,20,22,23,24,26,29,30、SO-2、TM-1,2、FX-8..11、OD-3、AR-1..4、CS-1,2） |
| 缺失 | 32（CC-1..11,24,25、OR-11,18,21,25,27,28、SO-1、FX-1..7、PV-1、AT-1、AR-5、IX-1,3,5,6） |
| 多余 | 7（OR-2、FX-12、IX-2、IX-4、Rust append_system_tool_additions 去重、faux_default_model、completions 恒发 stream_options） |
| 豁免 | 8 组（compat 占位、模型目录、openai/deepseek 走 responses、null 归一化、语言机制等价若干） |
| 命名差异 | 若干（snake_case、oauth-page 位置等，不算问题） |

## 不可核对项（工具输出 redaction）

常量/表达式级：MIN_MAX_TOKENS、CONTEXT_SAFETY_TOKENS、MIN_ANSWER_TOKENS、OPENAI_RESPONSES_MIN_OUTPUT_TOKENS、DEFAULT_MIN/MAX_TOKEN_SIZE、max_tokens 相关赋值（CC-27、SO-4、OR-31、FX 若干）、api_key 抽取表达式、usage.total_tokens 赋值。这些点无法确认或否认 1:1，报告中未计入差异。

## 复核说明

本报告独立核对，未采信任何既有结论。faux.rs 与 openai-completions.rs 的「简化版」是文件头显式声明，但按 1:1 审计标准仍计入缺失/差异；openai-responses.rs 未声明简化却存在 OR-1/OR-10/OR-11/OR-14/OR-16 等实质性差异，建议优先修复：OR-11（custom_tool_call 输入事件）、OR-10（arguments.done 前缀语义）、OR-1（tool-search seed）、OR-14（未知 status 抛错）、OR-16（错误终态传播）、AR-1/AR-2（OAuth 刷新取消语义）。
