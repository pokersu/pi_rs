# pi-ai parity recheck — a9（types + models + utils 子集）

- 基线：upstream `earendil-works/pi` v1.1.0 @ abe508e1b（`upstream/packages/ai/src/`）
- 复刻：`crates/pi-ai/src/`
- 方法：逐文件提取导出符号，Rust 侧逐方法比对参数/返回值/控制流/边界/默认值/字符串字面量。只读审计，未改任何源码。
- 已知豁免（按任务约定，均已复核确认）：TypeBox→JSON schema+jsonschema；parseStreamingJson partial-json 补全简化；sanitize-unicode Rust 恒等。

**环境限制**：本次读取途经沙箱，TS/Rust 两侧的部分字面量被统一 redact（如 `Usage.totalTokens`、`Model.maxTokens`、`ModelCostTier.inputTokensAbove`、`CHARS_PER_TOKEN`、`calculateCost` 的 `inputTokens` 表达式、`ThinkingTokenBudgetField` 字面量、`AbortSignal.token` 字段类型、`ContextUsageEstimate` 字段类型、overflow case2/3 阈值表达式）。两侧同字段同被 redact，结构位置一致，推断等价但**无法逐字核对**，已在下文单独列出。

---

## 1. types.ts → types.rs（声明子集）

### 1.1 总体结论
消息/内容/工具/事件/模型核心类型基本 1:1；多模型类型（image/classifier）、typed compat、类型级映射（ApiOptionsMap 等）未移植（AGENT.md 已声明的子集简化）。

### 1.2 核对通过（等价）
- `ToolChoice`/`ThinkingLevel`/`ModelThinkingLevel`/`CacheRetention`/`Transport` 枚举值与序列化名 ✓（含 "xhigh"/"max"、"websocket-cached"）
- `ThinkingLevelMap`（`Partial<Record<..., string|null>>`）✓；`ThinkingBudgets` 4 字段（minimal/low/medium/high）✓
- `TextContent`/`ThinkingContent`/`ImageContent`/`ToolCall`（含 thoughtSignature/namespace/redacted）✓；`ContentBlock` internally tagged ✓（含 round-trip 测试）
- `Usage`（含 cacheWrite1h/reasoning）✓；`UsageCost` ✓；`StopReason` 7 值 ✓；`DeferredHandle` 6 字段 ✓
- `SystemMessage`：content（`string | TextContent[]` untagged）、`sections: IndexMap<String, Option<String>>`（**保序，与任务要求一致**；AGENT.md 第 12 条"BTreeMap"描述已过时，第 13 条 IndexMap 与代码一致）、toolsAdded/toolsRemoved、timestamp ✓；role 由 `Message` 枚举 tag 承载（豁免）
- `UserMessage`/`AssistantMessage` 其余字段、`Message` 4 变体、`Context`/`TranscriptContext`（brand 由类型系统替代，豁免）✓
- `AssistantMessageEvent` 12 变体（start/text_*/thinking_*/toolcall_*/done/error）与 `TerminalStopReason`/`ErrorStopReason` ✓
- `ModelCostRates`/`ModelCostTier`（flatten + inputTokensAbove）/`ModelCost`（tiers）✓
- `ModelPromptCache`（short/long）✓；`ModelImageResizeOptions`/`ModelImageInputLimits`/`ModelInputLimits` ✓
- `Model`：reasoning/thinkingLevelMap/promptCache/contextWindow/maxTokens/samplingParams/samplingParamsByThinkingLevel/inputLimits/headers ✓（compat→JSON 为豁免，见下）
- `ConstrainedSamplingConfig`（json_schema{strict}/grammar{variants}，strict rename_all lowercase）✓；`Tool`/`ToolReference` ✓
- `ProviderRequestOptions` 其余字段（signal/apiKey/headers/timeoutMs/maxRetries/maxRetryDelayMs/onPayload/onResponse）✓；`StreamOptions`（temperature/samplingParams/maxTokens/transport/cacheRetention/sessionId/websocketConnectTimeoutMs/metadata/onProviderStreamEvent）✓；`DeferredFetchOptions.wait` ✓；`SimpleStreamOptions` 4 字段 ✓（`deferred` 用 JSON 占位 boolean|{window}，豁免-简化）
- 时间戳 u64 vs JS number、`ProviderResponse.status` u16、`expiresAt` u64 —— 语义等价（豁免）

### 1.3 缺失
1. `AssistantMessage.diagnostics` 字段缺失（TS `diagnostics?: AssistantMessageDiagnostic[]`；utils/diagnostics.rs 有类型但未挂到消息上，frame 编码时诊断信息随之丢失）。
2. `TextSignatureV1` 缺失。
3. `NestedToolCallRecord`/`NestedToolCalls` 缺失；`ToolResultMessage.nestedCalls` 字段缺失。
4. Images 类型族缺失：`ImagesInputContent`/`ImagesOutputContent`/`ImagesContext`/`ImagesStopReason`/`AssistantImages`。
5. Classifier 类型族缺失：`ClassifierChoiceQuestion`/`ClassifierScoreQuestion`/`ClassifierBoolQuestion`/`ClassifierQuestion`/`ClassifierContext`/`ClassifierChoiceAnswer`/`ClassifierScoreAnswer`/`ClassifierBoolAnswer`/`ClassifierAnswer`/`ClassifierStopReason`/`ClassifierResult`。
6. `ImageModel`/`ClassifierModel`/`BaseModel`（并入 Model）/`ModelTypeMap`/`ModelType`/`AnyModel` 缺失；`Model.type?: "chat"` 字段缺失（Rust 无模型类型区分）。
7. `ProviderStreams`/`ProviderImages`/`ProviderClassifier`/`ClassifierOptions`/`ImagesOptions`/`ProviderImagesOptions`/`ApiOptionsMap`/`ApiStreamOptions` 缺失（类型级分发无运行时影响，部分豁免）。
8. typed compat 缺失：`OpenAICompletionsCompat`（26 字段）/`OpenAIResponsesCompat`（11 字段）/`AnthropicMessagesCompat`（13 字段）/`BedrockCompat`/`MistralConversationsCompat`；`Model.compat: Option<serde_json::Value>` 占位（AGENT.md 第 8 条声明，豁免；但 openai-responses.rs 内 `get_compat` 只解析 10 个布尔/字符串字段，其余 compat 字段语义丢失）。
9. `OpenRouterRouting`/`VercelGatewayRouting`/`AnthropicAllowedFallbackModel` 缺失。
10. `ChatTemplateKwargValue`/`ThinkingTokenBudgetField`/`SessionAffinityFormat` 字面量 union 类型缺失（仅存在于 compat JSON）。
11. `ProviderRequestOptions.telemetryContext`/`fetch`/`env` 字段缺失（fetch/telemetry 注释声明"去掉"；`env` 未在注释中声明但同样缺失，`ProviderEnv` 类型存在却未被引用）。

### 1.4 多余
1. `ToolResultMessage.added_tool_names: Option<Vec<String>>` —— TS 无此字段（TS 侧为 `nestedCalls`）。连带 utils/estimate.rs 的 added-tools 过滤逻辑（见 §5.4）。
2. `ImagesApi`/`ImagesProviderId` 类型别名 —— TS 无对应（TS 用 `ImageApi`/`ProviderId`）。

### 1.5 逻辑差异
1. `AssistantMessage.thinkingLevel`：TS `ModelThinkingLevel`（含显式 `"off"`）；Rust `Option<ThinkingLevel>` 把 `"off"` 折叠为 `None`，丢失「显式 off」与「缺省」的区分；且反序列化收到 `"off"` 会整体报错。注释已声明该简化，但属语义损失。
2. `Tool.constrainedSampling`：TS `false | ConstrainedSamplingConfig`（显式 `false` 有意义）；Rust `Option<...>` 无法表示 `false`，反序列化 `false` 报错。
3. `GrammarVariants` 键 `openai_lark | openai_regex` 放宽为 `BTreeMap<String,String>`（serde 字面量键限制，豁免-简化）。
4. `ThinkingLevelMap`/`SamplingParamsByThinkingLevel` 用 BTreeMap（键有序）vs JS 对象插入序 —— 序列化键序差异（对请求字节级保真/缓存有潜在影响，低危）。

---

## 2. models.ts → models.rs

### 2.1 核对通过
- `mergeHeaders`（大小写不敏感去重、override 覆盖）✓；`calculateCost` 结构 ✓（tier 选择 `inputTokens > tier.inputTokensAbove && > matchedThreshold` 最高阈值生效；1h cacheWrite 双倍 input 价 ✓；就地更新 usage.cost 并返回 ✓）
- `EXTENDED_THINKING_LEVELS` 顺序（off,minimal,low,medium,high,xhigh,max）✓；`getSupportedThinkingLevels`（null→剔除、xhigh/max 需显式映射、!reasoning→["off"]）✓；`clampThinkingLevel`（向上再向下搜索 + 回退 off）✓；`modelsAreEqual` 空值处理 ✓
- `stream_simple`/`stream_deferred`/`cancel_deferred`/`fetch_deferred` 的 auth 解析 → baseUrl 覆盖 → apiKey/headers 注入 → 事件转发骨架 ✓；错误流文案（"Unknown provider: X"、"Provider is not configured: X"、"Provider X does not support deferred responses"）✓
- `login`/`logout`/`checkAuth`/`getAuth`（provider 与 model 重载）/`readCredential` 消息文案 ✓；`setProvider`/`deleteProvider`/`clearProviders` ✓
- `hasApi`/`modelsAreEqual` 基本形态 ✓（见差异）

### 2.2 缺失
1. `Provider` 接口缺：`headers`、`getAllModels`、`refreshModels`、`filterModels`、`filterAllModels`、`stream`（按 Api 类型分发的 typed stream）、`generateImages`、`classify`（注释声明简化，但 get_available 因此无法应用 filterModels，见 2.3）。
2. `Models` 方法缺：`stream`、`complete`、`getModelsOfType`、`getModelOfType`、`getAllModels`、`getAvailableOfType`、`getAllAvailable`、`refresh`、`generateImages`、`classify`。
3. refresh 框架整体缺失：`ModelsPublication`/`RefreshModelsContext`/`ModelsRefreshOptions`/`ModelsRefreshResult`/`ModelsRequestTransforms`/`ModelsApiStreamOptions`/`ModelsSimpleStreamOptions`/`ModelsDeferredFetchOptions`/`ModelsDeferredCancelOptions`/`ModelsImagesOptions`/`ModelsClassifierOptions`、`KNOWN_MODEL_TYPES`/`hasKnownModelType`/`withKnownModelTypes`、`supersedeProviderRefresh`/`beginProviderRefresh`/`publishProviderModels`/`runProviderRefreshPhase`/`resolveRefreshCredential`/`getAuthenticatedProviders`。
4. `CreateProviderOptions` 缺：`headers`、`fetchModels`、`filterModels`、`filterAllModels`、`api`（单实现/按 api 映射二义分发）、`images`、`classifiers`（"at least one of api/images/classifiers" 校验随整体简化消失）。
5. `applyAuth` 的 env 合并缺失：TS 输出 `env = {...resolution.env, ...options.env}`（请求选项覆盖解析结果）；Rust 只填 `env: None`，从不把解析出的 env 注入请求选项。

### 2.3 逻辑差异
1. **（高）`Models::stream_simple` 未调用 `normalize_context`**：TS `streamSimple`/`stream` 先 `normalizeContext(context)` 再进 lazyStream，provider 收到折叠后的 TranscriptContext（systemPrompt/tools 已进首条 system 消息）。Rust 把原始 `Context` 直接传给 `provider.stream_simple`，`StreamFunction` 签名也是 `&Context` 而非 `TranscriptContext`。AGENT.md 第 13 条声称「stream 入口已切到 normalizeContext」与代码不符（AGENT.md 过期或未完成）。provider 侧（api/openai-responses.rs 等，超出本次范围）是否自行补偿未验证。
2. **（高）headers 合并优先级反转**：TS `applyAuth` 为 `mergeHeaders(auth.headers, options.headers)`（请求选项覆盖认证默认）；Rust `merge_headers(opts.stream.request.headers, resolution.auth.headers)`（认证解析结果覆盖调用方请求选项）。`get_auth(model)` 路径方向正确（model.headers 覆盖 result.headers ✓）。
3. `check_provider_auth`：TS 仅在 `apiKey.check` **不存在**时 fallback 到 `resolveProviderAuth`（check 存在但返回 undefined 即为最终结论）；Rust 因 trait 方法恒存在，`check` 返回 `Ok(None)` 时也 fallback resolve —— 对「自定义 check 明确返回未配置」的场景行为不同。
4. `calculate_cost`：`short_write = cache_write - long_write` 在 TS 中可为负（JS number，后续公式按负值计）；Rust u64 下溢（debug panic / release wrap）。cacheWrite1h > cacheWrite 的脏数据边缘。
5. `has_api`：TS 为 `isModelType(model, "chat") && model.api === api`（非 chat 模型绝不匹配）；Rust 仅 `model.api == api`（无模型类型维度，属子集简化的必然结果，但语义不等价）。
6. `models_are_equal`：TS 比较 type+id+provider；Rust 只比较 id+provider。
7. `get_models`/`getAllModels` 的 best-effort：TS 对每个 provider 的 `getModels()` 抛错 try/catch 返回 []；Rust 无捕获（panic 直接传播）。
8. `login` 失败包装：TS 将 `method.login()` 的 rejection 原样抛出；Rust 包装为 `ModelsError(Auth, "Login failed for {id}", cause)`。`cancel_deferred` 错误：TS 抛 `ModelsError`；Rust 返回 `Err(String)`。
9. `get_available`：TS 并发 `Promise.all` 且应用 `filterModels`；Rust 串行遍历、无 filter（filter 能力缺失的连带后果）。
10. `stream_simple` 的 provider 查找：TS 在 lazyStream 内延迟（首个消费者拉取时）；Rust 在调用处同步查（未知 provider 立即返回错误流）—— 对可观察行为等价（豁免-机制）。

---

## 3. utils/event-stream.ts → event-stream.rs

- `FifoQueue`→mpsc channel、`push`（done 后丢弃、isComplete 设 done+发 final、事件仍投递）、`end`（done+可选 result+唤醒消费者）、`result()`（oneshot+Shared 多次 await）、`Stream` 实现 ✓ 等价。
- `AssistantMessageEventStream` 计时守卫三条件（done / 已有 durationMs / timestamp 早于流开始）✓；`push`/`end` 先计时后投递顺序 ✓；工厂 ✓（附测试）。

逻辑差异（低）：
1. `durationMs = Math.max(0, Math.round(...))` vs Rust `elapsed().as_millis()`（截断）：毫秒级 off-by-one。
2. `result()` 在流未终态被 drop 时 TS 永久挂起、Rust `expect` panic。
3. TS 支持多个并发 async iterator（FIFO 共享队列，事件均分）；Rust 单一共享 receiver（多消费者争抢同一序列，无法各自全量消费）—— 单消费者场景等价（豁免-机制）。
4. `end()` 后 final 已通过 done 事件设置时，TS 的 `resolveFinalResult` 二次调用为 no-op、Rust `final_tx.take()` 为 None 跳过 —— 等价 ✓。

---

## 4. utils/validation.ts → validation.rs（coercion 完整逻辑）

- `getSchemaTypes`/`matchesJsonType`/`coercePrimitiveByType`（number/integer/boolean/string/null 五分支的 null/字符串/布尔/数字转换）、`applySchemaObjectCoercion`（properties 已定义键 + additionalProperties object 补余）、`applySchemaArrayCoercion`（tuple 按位 / 单 schema 全量）、`coerceWithUnionSchema`（两遍：先原值匹配后 coercion 匹配）、`coerceWithJsonSchema` 递归顺序（allOf→anyOf→oneOf→type union→object→array）、`normalizeOptionalNulls`（数组/对象递归、required 豁免、$ref 豁免、子 schema 校验 null 失败才删）、`validateToolCall`（"Tool \"X\" not found"）、`validateToolArguments` 错误文案格式（`Validation failed for tool "...":\n  - path: msg\n\nReceived arguments:\n<pretty JSON>`）✓ 全部对齐。

逻辑差异：
1. `formatValidationPath` 缺 `keyword === "required"` 特判：TS 用 `error.params.requiredProperties[0]` 补出 `parent.missingProperty` 路径；Rust 只拼 instancePath（required 错误路径会变成父路径或 "root"）。
2. 原语 coercion 的早退路径：TS 当 `coerced !== args` 且双方非对象时 `return validator.Check(coerced) ? coerced : args`（coerced 校验不过时**静默返回未校验的原值，不抛错**）；Rust 无条件 `args = coerced` 后统一校验，校验不过抛错。
3. 字符串→数字边界：TS `Number("0x10")=16`（hex 会转）、integer 的 `Number.isInteger(Number("3.0"))` 为真（"3.0"→3）、超出 i64 的整数字符串可转；Rust `parse::<f64>/<i64>` 严格（hex 失败、"3.0" 失败、大数失败）→ 这些输入 TS 会 coercion 而 Rust 不转。
4. 错误明细文本来自 jsonschema crate vs TypeBox 本地化消息 —— 不同校验库产物（豁免-机制，但输出字面不同）。
5. `Value.Convert(tool.parameters, args)`（TypeBox 默认值/类型转换步骤）在 Rust 无对应 —— 属 TypeBox→jsonschema 豁免范围（jsonschema crate 不施加默认值），已按任务豁免归类。

---

## 5. 其余 utils 逐文件

### 5.1 uuid.ts → uuid.rs ✓ 1:1
- 48 位时间戳校验（Rust assert 仅上界，u64 无负数/小数——机制差异）、lastOrdinaryTimestamp 单调（follower 不更新 ✓）、41 位 sequence 初始化/递增/MAX 抛错、字节布局（version 7 高 4 位、variant 10、seq 41 位跨 6 字节、bytes[11] 低 1 位随机保留）、8-4-4-4-12 hex ✓。sequence 初值用独立随机数组 vs TS 复用同一数组 —— 等价。panic vs RangeError 为机制差异。

### 5.2 text.ts → text.rs ✓ 1:1
- `contentText`（string 直返 / 仅 text 块 join，默认分隔 "\n" 在 Rust 为显式参数）、`getSystemMessageText`（content + 非 null sections、滤空、"\n\n" join）、`renderSystemMessageUpdate`（`Removed system prompt section "X".` / `Updated system prompt section "X":\n\n<值>` 字面量）✓ 逐字一致。

### 5.3 json-parse.ts → json-parse.rs ✓（含声明的豁免）
- `repairJson`（引号外直传、字符串内控制字符转义、`\u`+4hex 复制、合法转义直传、非法转义双反斜杠且下一字符下轮再处理、行尾孤立反斜杠）✓ 逐分支一致；`parseJsonWithRepair`（先直解析、repair 后重解析、未变则抛原错）✓。`parseStreamingJson` 的 partial-json 补全缺失 —— 已声明的豁免 ✓。

### 5.4 estimate.ts → estimate.rs
- `calculateContextTokens`（totalTokens>0 优先，否则四分量和）✓；`estimateTextTokens`（ceil(len/CHARS_PER_TOKEN)）✓；`estimateTextAndImageContentTokens`（string 与 blocks 两形态等价覆盖）✓；`estimateMessageTokens` 四角色分支（system=文本+toolsAdded+toolsRemoved、user/toolResult、assistant 的 text/thinking/toolCall 计数）✓；`estimateToolsTokens`（空数组 0）✓；`safeJsonStringify` ✓（回退串见 error-body 差异）；`getLastAssistantUsageInfo`（usage 适用性：timestamp >= 最近前缀时间戳、stopReason 非 aborted/error、tokens>0）✓（TS -Infinity 起步 vs Rust 0 起步对 epoch 毫秒时间戳等价）。

逻辑差异/多余：
1. **`estimate_context_tokens` 对 `Context` 输入额外计入 systemPrompt/tools token（以及基于 `added_tool_names` 过滤 context.tools 后追加的 token）**：TS 对 `Context | Message[]` 一视同仁、只估 messages；Rust 的 Context 分支会输出更大的 tokens。此为 Rust 独有逻辑（多余 + 输出差异），且依赖 §1.4 的 `added_tool_names` 多余字段。
2. CHARS_PER_TOKEN 常量、`ContextUsageEstimate` 字段类型、`inputTokens` 表达式被 redact 无法逐字核对（两侧结构一致）。
3. `safe_json_stringify` 回退文本差异（见 §5.6）。

### 5.5 overflow.ts → overflow.rs
- 25 个 OVERFLOW_PATTERNS 中 23 个 ✓ 逐字一致；NON_OVERFLOW 3 个 ✓（见下差异）；Case1 顺序（先 NON_OVERFLOW 排除再 OVERFLOW 判定）✓；Case2（stop + input>contextWindow）✓；Case3（length + output==0 + input>=阈值）结构 ✓（阈值表达式 redact 无法核对）；`isRecoverableLength` ✓；`getOverflowPatterns` ✓（内容差异见下）。

逻辑差异/缺失：
1. 缺失：`prompt exceeds max length`（z.ai CN overflow 文案）模式未移植。
2. `prompt (?:is )?too long` → Rust `prompt is too long`：不再匹配 "prompt too long"（z.ai "Prompt too long"、部分 Anthropic 兼容网关文案漏检）。
3. `CEREBRAS_BODYLESS_OVERFLOW_PATTERN` 被并入主列表（TS 仅当 `provider === "cerebras"` 时生效）：Rust 下任何 provider 的 "400 (no body)" 类错误都会被判 overflow。
4. NON_OVERFLOW `^(Throttling error|Service unavailable):` 的冒号在 Rust 丢失（排除面略宽）。
5. Case2/3 的 inputTokens 表达式与阈值（`[redacted]`）无法核对。

### 5.6 error-body.ts → error-body.rs
- `formatProviderError`（messageCarriesBody/status/body 三分支、prefix 有无两形态）✓ 逐字一致；`normalizeProviderError` 的 trim/空 body 丢弃/4000 截断/`messageCarriesBody = message.includes(body)` ✓（TS 对非 Error 抛值的 `safeJsonStringify(error)` 分支因类型约束缺失 —— 机制差异）。
- 豁免：SDK 字段探测（statusCode/status/$metadata/$response.body/error.error/pipe 嗅探/plain-object 校验）被显式参数替代（注释声明，reqwest 架构使然）。

逻辑差异（低）：
1. `truncateErrorText` 计数单位：TS UTF-16 code units（length/slice）；Rust Unicode 标量（chars()）—— 非 BMP 文本截断点不同。
2. `safeJsonStringify` 失败回退：TS `String(value)`；Rust `"[unserializable]"`（estimate.rs 与 diagnostics 同受影响）。

### 5.7 provider-retry.ts → provider-retry.rs
- `isRetryableProviderError`（x-should-retry true/false、status undefined→true、408/409/429/≥500）✓；`validateServerRetryDelayMs`（默认 60000、上限超时抛错、`Server requested Xs retry delay (max: Ys). msg` 文案与 ceil 计算）✓；指数退避 `min(0.5*2^retryIndex, 8)*1000 * (1-rand*0.25)`（jitter 区间 [0.75,1)）✓；`retryProviderRequest` 主循环（abort→AbortError、非 ProviderError 原样抛、预算耗尽/不可重试抛、retryIndex=maxRetries-retriesRemaining、sleep 中断）✓。

缺失/差异：
1. 缺失：`noRetryStatuses` 选项（TS 允许对指定 HTTP 状态立即失败）。
2. `retry-after-ms` 解析：TS `parseFloat` 接受部分数字（"12abc"→12）；Rust 严格 parse 失败后跳过该 header。
3. HTTP-date 形式的 `retry-after`：Rust 忽略（TS `Date.parse`）—— 代码注释声明的简化（豁免）。
4. 非 ProviderError 在 TS 原样重抛（保留类型/stack）；Rust 收窄为 `ProviderRequestError::Other(String)`（机制差异）。

### 5.8 hash.ts → hash.rs ✓ 1:1
- cyrb53：UTF-16 code unit 迭代（`encode_utf16` ✓）、4 个乘法常量、Math.imul 32 位语义（wrapping_mul ✓）、`>>>` 位运算、先 h1 后 h2 的交叉混合顺序、`(h2>>>0).toString(36)+(h1>>>0).toString(36)` 小写 base36 ✓。

### 5.9 headers.ts → headers.rs
- `headersToRecord` ✓（Headers→HeaderMap 机制豁免；非 UTF-8 值丢弃与 JS 字符串值等价）。

缺失/差异：
1. 缺失：`providerHeadersToRecord` 的**变参多源合并**（TS `...headerSources` 按序覆盖、null 删除先前同名项、保留最后写入的原始大小写）；Rust 仅单 source。
2. 单 source 语义等价（null 过滤、空→None ✓）；输出 BTreeMap 键排序 vs TS 插入序（低危）。

### 5.10 pi-user-agent.ts → pi-user-agent.rs
逻辑差异：TS `pi (<platform> <release>; <arch>)`，platform 为 node:os 值（`darwin`/`win32`/`linux`），browser 环境为 `pi (browser)`；Rust 用 `std::env::consts`（`macos`/`windows`/`linux`），release 缺失（macOS/Windows）时输出 `pi (platform; arch)`（TS 无此形态）。UA 字面量在不同平台不同。

### 5.11 provider-env.ts → provider-env.rs
- 豁免：Bun sandbox `/proc/self/environ` 回退（注释声明，Bun 特化）。
- 逻辑差异（低）：TS `env?.[name] || process.env[name] || ...`（空字符串 override 会回退到进程 env）；Rust `Some("")` 直接返回。

### 5.12 sleep.ts → sleep.rs ✓
- 立即 aborted 检查、abort 中断、信号监听 ✓（TS reject `signal.reason`，Rust 统一 AbortError —— 既有 reason 简化）。

### 5.13 sanitize-unicode.ts → sanitize-unicode.rs —— 豁免（已声明）
- Rust 恒等 ✓（UTF-8 String 无未配对代理），复核确认。

### 5.14 typebox-helpers.ts → typebox-helpers.rs ✓
- `StringEnum(values, {description?, default?})` → `{type:"string", enum, ...}` 条件字段 ✓（TypeBox→JSON schema 豁免）。

### 5.15 transcript.ts → transcript.rs（16 个导出函数逐一核对）
16 个函数全部存在且逻辑对齐：`createInitialSystemMessage`（双空→None、content 空串、toolsAdded 条件、timestamp 0）✓；`normalizeContext` ✓；`getInitialSystemMessage` ✓；`withoutInitialSystemMessage` ✓；`getCurrentTools`（remove→add、Map 保序语义：同名更新保持位置 ✓）；`getCurrentSystemMessage`（timestamp ??=、content "\n\n" join、sections 删除/覆盖（shift_remove 保序）、tools、双空→None）✓；`getCurrentSystemPrompt` ✓；`collapseSystemMessages` ✓；`resolveTranscript`（undefined→collapse ✓）；`toToolDeclaration`/`declarationsEqual`（见下差异）；`getToolStateChanges`（定义变化=移除+添加、added 走 declaration）✓；`getDeclaredTools`（首次声明序、同名保持位置）✓；`hasToolRedefinitions` ✓；`hasNonAdditiveToolChanges` ✓；`resolveTranscriptTools` ✓。

差异/说明：
1. `toToolDeclaration`：TS 做 JSON 往返（丢弃 undefined 字段、规范化键序）；Rust 为 clone（注释声明：强类型 + Value 无 undefined）。后果：TS 的 JSON 往返会剥离 `undefined` 属性，Rust clone 不改变任何内容。
2. `declarationsEqual`：TS 比较**序列化字符串**（对象键序敏感：同 schema 不同键插入序 → 不相等）；Rust 用 serde_json `PartialEq`（对象键序不敏感）→ 两种工具状态在 Rust 被判相等、TS 判不等。
3. `TranscriptMessages`（任意 role 消息列表）在 Rust 收紧为 `&[Message]`（AGENT.md 声明的分层约定，豁免-简化）。
4. `contentText` 对 system 内容的默认分隔符 "\n" 在 Rust 由 `system_content_text` 固化 ✓ 等价。

### 5.16 error-stream（TS 源为 api/lazy.ts）→ error-stream.rs
上游无 `utils/error-stream.ts`；对应物是 `api/lazy.ts` 的 `lazyStream` + `createSetupErrorMessage`（AGENT.md 映射表已注明"（lazyStream 相关）"）。

- `createSetupErrorMessage` vs `create_error_message`：字段集 ✓（content 空、api/provider/model、零 usage、stopReason "error"、errorMessage）；`default_usage` ✓（total_tokens redact 未核对）。
- 差异（低）：TS 的 timestamp 为 lazyStream 创建时刻 `startedAt`；Rust 为 `now_ms()`（调用时刻，通常同毫秒）。
- 缺失：`lazyApi`（capabilities.fetchDeferred/cancelDeferred 的动态加载包装，属 api 层）与 `forwardStream`（内层流转发后 `end(await source.result())`）无对应。models.rs 的转发循环不调用 `producer.end(...)`——对正常 done/error 终态等价（isComplete 已设 final），对"无终态事件即闭合"的异常内层流两者都挂起（TS 的 `await source.result()` 同样挂起），可观察行为等价。

### 5.17 assistant-message-frame.ts → assistant-message-frame.rs
- `AssistantMessageFrame` 11 变体（start/text_start/delta/end/thinking_start/delta/end/toolcall_start/checkpoint/delta/end）tag 与字段 ✓（serde rename 逐字对齐）；`isJsonPrefix` 递归（string startsWith/数组前缀/对象 hasOwn 前缀/原语 Object.is）✓；`EMPTY_PARSED_TOOL_ARGUMENTS="{}"` ✓；encoder 状态机（started/terminal/双 start/terminal 后事件/done 前必须有 start/error 无需 start）✓；text/thinking 的 coveredChars/deltaChars 覆盖压缩 ✓；toolCall 的 catchup 逻辑（caughtUp 直通、catchupJson 累积、与 snapshot 比对、isJsonPrefix 兜底、checkpoint 输出）✓；startBlock 重复/block 未启动/类型不符/endBlock 删除 ✓；reducer（appendBlock 严格连续索引 "already exists"/"would leave a gap"、activeBlock 守卫、各 _end 的签名/redacted 覆盖语义、未终 toolCall 的 json 补解析）✓。

差异：
1. `cloneStartMessage`：TS 丢弃 thinkingLevel（不在 start partial 内）；Rust **保留** `thinking_level` —— start frame 的 partial 内容不一致（还原端可观测）。
2. TS `cloneStartMessage` 复制 diagnostics；Rust 因 AssistantMessage.diagnostics 缺失（§1.3-1）连带丢失。
3. `coveredChars`/`deltaChars` 计数单位：TS UTF-16 code units；Rust Unicode 标量 —— 非 BMP 文本的 delta 覆盖切片（`delta.slice(covered)` vs `chars().skip(covered)`）结果不同。
4. encoder 错误文案差异：TS `text_start event points to ${content.type} block at index X`（含实际块类型）；Rust `...points to a non-text block at index X`（措辞不同）。
5. `isJsonPrefix` 原语比较：TS `Object.is`（区分 0/-0）；Rust `Value ==`（0.0==-0.0）—— JSON 输入几乎不可达（低危）。

---

## 6. abort.ts → abort.rs / diagnostics.ts → diagnostics.rs

### 6.1 abort ✓
- `operationSignal`（缺省新建 signal）✓；`raceWithAbortSignal`（已 aborted 立即 reject、事件监听与清理、operation 结果直通）✓；TS 对被放弃 promise 的观察在 Rust 为 future drop（机制豁免）。
- 简化：`abortReason` 不携带 `signal.reason`（统一 AbortError），注释已声明。

### 6.2 diagnostics
逻辑差异：
1. `extractDiagnosticError`：TS `name: error.name || undefined`（错误类型名，如 "AbortError"）；Rust `name: Some(error.to_string())`（Display 全文，与 message 重复）。`stack`/`code` 恒 None（TS 提取 stack 与 string|number code）。
2. `formatThrownValue`：TS 接受任意值（Error→message||name、string→原样、其他→String(value)）；Rust 仅 `&dyn Error`、空 message 回退硬编码 "Error"（TS 回退 `name`）。
3. 非 Error 抛值分支（`{name:"ThrownValue", message: formatThrownValue}`）因类型约束缺失。
- `createAssistantMessageDiagnostic`（timestamp Date.now↔now_ms ✓）、`appendAssistantMessageDiagnostic`（trait vs 结构化约束，机制豁免）✓。

---

## 7. 无法逐字核对项（沙箱 redact）

| 位置 | 说明 |
|---|---|
| types.ts/rs: `Usage.totalTokens`、`Model.maxTokens`、`ModelCostTier.inputTokensAbove` | 字段存在、两侧同被 redact，类型推断为 number/u64，未逐字确认 |
| types.ts: `ThinkingTokenBudgetField`、`thinkingTokenBudgetField`/`supportsThinkingTokenBudget`/`supportsMaxOutputTokens` 字面量 | 两侧同被 redact |
| models.ts/rs: `calculateCost` 的 `inputTokens` 表达式 | 两侧同被 redact；Rust 结构与之对齐 |
| estimate.ts/rs: `CHARS_PER_TOKEN` 常量、`ContextUsageEstimate` 字段类型、usage/trailing 累计表达式 | 同被 redact |
| overflow.ts/rs: Case2/Case3 的 `inputTokens` 与阈值表达式 | 同被 redact |
| error-stream: `default_usage.totalTokens` | 同被 redact |

---

## 8. 与 AGENT.md 的出入（独立核对发现）

1. AGENT.md §原理/第 13 条：「stream 入口已切到 normalizeContext，provider 侧也已改为消费 transcript」——**与 models.rs 实际代码不符**（`stream_simple`/`stream_deferred` 直接传 `&Context`，`StreamFunction` 签名为 `&Context`）。
2. AGENT.md 第 12 条「sections 在 Rust 用 BTreeMap」—— 代码实际为 `IndexMap`（第 13 条描述为 IndexMap，与代码一致；第 12 条为陈旧文字）。
3. AGENT.md 映射表「`src/utils/error-stream.ts`」不存在；TS 对应物为 `api/lazy.ts`（映射表自身已注明"（lazyStream 相关）"，此处仅为核对记录）。
4. models.rs 头注释「去掉 login/refreshModels 等模型刷新能力」—— login/logout 实际已实现，仅 refresh 未实现（注释部分过时）。

---

## 9. 汇总

### 缺失（15 项）
1. types：`AssistantMessage.diagnostics`、`TextSignatureV1`、`NestedToolCallRecord`/`NestedToolCalls`（`ToolResultMessage.nestedCalls`）
2. types：Images 类型族（`ImagesContext`/`AssistantImages`/`ImagesStopReason` 等）
3. types：Classifier 类型族（Question/Answer/`ClassifierResult` 等 11 个）
4. types：`ImageModel`/`ClassifierModel`/`BaseModel`/`ModelTypeMap`/`ModelType`/`AnyModel` 及 `Model.type` 字段
5. types：`ProviderStreams`/`ProviderImages`/`ProviderClassifier`/`ClassifierOptions`/`ImagesOptions`/`ApiOptionsMap`/`ApiStreamOptions`
6. types：5 个 typed compat 接口 + `OpenRouterRouting`/`VercelGatewayRouting`/`AnthropicAllowedFallbackModel` + `ChatTemplateKwargValue`/`ThinkingTokenBudgetField`/`SessionAffinityFormat`（JSON 占位，AGENT.md 声明的子集简化）
7. types：`ProviderRequestOptions.telemetryContext`/`fetch`/`env`
8. models：Provider 接口的 `headers`/`getAllModels`/`refreshModels`/`filterModels`/`filterAllModels`/`stream`/`generateImages`/`classify`
9. models：Models 的 `stream`/`complete`/`getModelsOfType`/`getModelOfType`/`getAllModels`/`getAvailableOfType`/`getAllAvailable`/`refresh`/`generateImages`/`classify` 及整个 refresh 框架类型
10. models：`CreateProviderOptions` 的 `headers`/`fetchModels`/`filterModels`/`filterAllModels`/多 api 映射/`images`/`classifiers` + `applyAuth` 的 env 合并
11. retry：9 个错误分类正则（1 非重试：`subscription_sharing_usage_limit_exceeded`；8 重试：`server_busy`、`servers are currently busy`、`currently experiencing high demand`、`model is at capacity`、`520`、`pending stream has been canceled`、`subscription_sharing_usage_unavailable`、`subscription_sharing_user_unavailable`）
12. overflow：`prompt exceeds max length` 模式
13. provider-retry：`noRetryStatuses` 选项
14. headers：`providerHeadersToRecord` 多源变参合并
15. error-stream：`lazyApi`/`forwardStream`（TS 位于 api/lazy.ts）

### 多余（4 项）
1. `ToolResultMessage.added_tool_names` 字段（TS 无；被 estimate.rs 引用）
2. `estimate_context_tokens` 对 `Context` 输入额外计入 systemPrompt/tools（及 added_tool_names 过滤）的 token —— TS 只估 messages
3. `ImagesApi`/`ImagesProviderId` 别名
4. 机制性辅助：`ContentTextInput`、`DiagnosticContainer`（低危，语言机制适配）

### 逻辑差异（20 项）
1. 【高】`Models::stream_simple` 未 `normalize_context`（TS streamSimple/stream 先折叠 systemPrompt/tools 再分发；AGENT.md 声称已切但与代码不符）
2. 【高】stream_simple/stream_deferred 的 headers 合并优先级反转（Rust 认证 headers 覆盖请求选项；TS 相反）
3. `AssistantMessage.thinkingLevel`：`"off"` 折叠为 `None`，丢失显式 off 与缺省区分；反序列化 `"off"` 报错
4. `Tool.constrainedSampling` 显式 `false` 不可表示
5. `check_provider_auth`：check 返回 None 时 Rust 额外 fallback resolve（TS 仅无 check 方法时 fallback）
6. `calculate_cost`：`short_write` u64 下溢（TS 可为负）
7. `has_api`/`models_are_equal` 缺模型类型维度（TS 含 isModelType/type 比较）
8. `login` 失败被包装为 ModelsError（TS 原样抛出）；`cancel_deferred` 返回 String 错误
9. `get_models` 等 best-effort 无 try/catch（TS 捕获抛错 provider）
10. validation：required 错误路径特判缺失；coercion 后原语校验失败时 TS 静默返回原值 vs Rust 抛错；hex/"3.0"/大数字符串 coercion 边界
11. diagnostics：`extract_diagnostic_error` name=Display 全文、stack/code 恒空；`format_thrown_value` 仅 Error、回退 "Error"
12. overflow：`prompt (?:is )?too long` 收窄为 `prompt is too long`；cerebras 无 body 模式全局化（TS 仅 provider==cerebras）；NON_OVERFLOW 冒号缺失
13. estimate：Context 分支 tokens 多计（与「多余 2」同源）
14. event-stream：`durationMs` Math.round vs 截断
15. error-body：截断按 Unicode 标量 vs UTF-16 code unit；`safeJsonStringify` 回退 "[unserializable]" vs String(value)
16. pi-user-agent：平台名 darwin/win32→macos/windows、release 缺失时输出形态不同
17. provider-env：空字符串 override 不回落 process.env（TS `||` 语义）
18. provider-retry：`retry-after-ms` parseFloat 宽松 vs 严格解析
19. transcript：`declarationsEqual` 结构相等 vs 序列化字符串（键序敏感）；`toToolDeclaration` clone vs JSON 往返
20. frame：`clone_start_message` 保留 thinkingLevel（TS 丢弃）；UTF-16 vs 标量计数；编码错误文案措辞

### 豁免（复核确认，不计入问题）
TypeBox→JSON schema+jsonschema（validation/typebox-helpers，含 Value.Convert 缺失）；parseStreamingJson partial-json 补全；sanitize-unicode 恒等；AbortSignal/AbortError 自建模；Message role 由 enum tag 承载；TranscriptContext brand 由类型系统替代；error-body SDK 字段探测→显式参数；Bun sandbox env 回退；provider-retry HTTP-date retry-after；compat→JSON 占位；时间戳/状态码 u64/u16 表示。
