# pi_rs ↔ upstream 方法级对比报告

> 基线：`upstream/` 检出于 `v1.1.0`（HEAD `abe508e1b`）

> 方法：提取 TS 顶层导出（class/function/const/interface/type/enum）与 class 方法，
> 按 camelCase→snake_case 在 Rust 侧同名查找。`local` 命中视为 OK；
> `global`（同 crate 其他文件）记为「移位」；整 crate 无同名记 MISSING。
> 机械扫描，用于定位可疑缺口；命名差异、结构合并、宏生成（如 `tagged_error!`）、
> trait 默认实现会产生误报，需人工确认。**文末附人工逐项复核结论（豁免/适配/缺口三档）**。

## 总览

上游包 → Rust crate 映射（用于确认范围）：

- `packages/agent` → `crates/pi-agent-core`（**完整复刻目标**）
- `packages/ai` → `crates/pi-ai`（**声明子集**，范围见 `crates/pi-ai/AGENT.md`）
- `packages/telemetry` → `crates/pi-telemetry`
- `packages/durable` → `crates/pi-durable`（**完整复刻目标**）
- `packages/chord` → `crates/pi-durable/src/chord`（**子集**，范围见 `scan.py` 的 `CHORD_SCOPE`）
- 未复刻：`client`、`codemode`、`coding-agent`、`env`、`evals`、
  `mcp`、`protocol`、`server`、`tui`（产品层）

扫描统计：

- **agent** → `crates/pi-agent-core/src`：配对 6 · 缺文件 0 · 范围外 0 · 缺符号 7 · 移位 2 · 私有函数差异 3
- **ai** → `crates/pi-ai/src`：配对 40 · 缺文件 0 · 范围外 155 · 缺符号 102 · 移位 5 · 私有函数差异 69
- **telemetry** → `crates/pi-telemetry/src`：配对 6 · 缺文件 0 · 范围外 0 · 缺符号 12 · 移位 0 · 私有函数差异 3
- **durable** → `crates/pi-durable/src`：配对 58 · 缺文件 9 · 范围外 0 · 缺符号 59 · 移位 34 · 私有函数差异 32
- **chord** → `crates/pi-durable/src/chord`：配对 4 · 缺文件 5 · 范围外 20 · 缺符号 15 · 移位 1 · 私有函数差异 11

## packages/agent → `crates/pi-agent-core/src`

统计：配对 6 文件 · 文件缺失 0 · 范围外 0 · 符号缺失 7 · 符号移位 2 · 私有函数差异 3

### 1) 文件级缺失（范围内，Rust 侧无对应文件）

（无）

### 2) 符号级缺失（TS 有，Rust 整 crate 未见同名）


**`agent.ts`** → `agent.rs`

- MISSING `AgentInitialState` (type) → 期望 `agent_initial_state`  ← 候选: `state`
- MISSING `continue` (method) → 期望 `continue`  ← 候选: `continue_turn`, `agent_loop_continue`, `run_agent_loop_continue`
- MISSING `normalizePromptInput` (method) → 期望 `normalize_prompt_input`
- MISSING `prompt` (method) → 期望 `prompt`  ← 候选: `prompt_text`, `prompt_messages`, `run_prompt_messages`, `prompt_text_with_images`

**`types.ts`** → `types.rs`

- MISSING `CustomAgentMessages` (type) → 期望 `custom_agent_messages`
- MISSING `FinishTurn` (type) → 期望 `finish_turn`
- MISSING `PrepareRequest` (type) → 期望 `prepare_request`
- <sub>移位 1 项：`AgentToolCallOutcome`</sub>

### 3) 私有顶层函数差异（TS 非导出实现函数，Rust 未见同名）

> 上游 `function foo()` 这类非导出实现函数。Rust 常把它们内联进调用方，
> 因此大量属于正常；但**真实缺口也藏在这里**——edit.ts 的 `prepareEditArguments`
> 就是靠人工读到这一层才发现的（处理 `edits` 为字符串/单对象/legacy 顶层字段）。
> 需人工逐条确认。


**`agent-loop.ts`** → `agent-loop.rs`

- PRIVATE `emitToolExecutionUpdate`

**`agent.ts`** → `agent.rs`

- PRIVATE `createMutableAgentState`  ← 候选: `state`

**`proxy.ts`** → `proxy.rs`

- PRIVATE `buildProxyRequestOptions`  ← 候选: `proxy_request`

## packages/ai → `crates/pi-ai/src`

统计：配对 40 文件 · 文件缺失 0 · 范围外 155 · 符号缺失 102 · 符号移位 5 · 私有函数差异 69

### 1) 文件级缺失（范围内，Rust 侧无对应文件）

（无）

<details><summary>范围外文件 155 个（AGENT.md 已声明不复刻）</summary>

- `bedrock-provider.ts`
- `bun-oauth.ts`
- `cli.ts`
- `compat.ts`
- `env-api-keys.ts`
- `image-models.ts`
- `images-api-registry.ts`
- `images.ts`
- `legacy-api-aliases.ts`
- `model-catalog.ts`
- `models-store.ts`
- `oauth.ts`
- `session-resources.ts`
- `compat/extension-oauth-types.ts`
- `auth/oauth/anthropic.ts`
- `auth/oauth/callback-server.ts`
- `auth/oauth/github-copilot.ts`
- `auth/oauth/kimi-coding.ts`
- `auth/oauth/load.ts`
- `auth/oauth/meta.ts`
- `auth/oauth/openai-chatgpt.ts`
- `auth/oauth/openai-codex.ts`
- `auth/oauth/openrouter.ts`
- `auth/oauth/radius.ts`
- `auth/oauth/xai.ts`
- `providers/all.ts`
- `providers/amazon-bedrock.models.ts`
- `providers/amazon-bedrock.ts`
- `providers/ant-ling.models.ts`
- `providers/ant-ling.ts`
- `providers/anthropic.models.ts`
- `providers/anthropic.ts`
- `providers/azure.models.ts`
- `providers/azure.ts`
- `providers/baseten.models.ts`
- `providers/baseten.ts`
- `providers/cerebras.models.ts`
- `providers/cerebras.ts`
- `providers/cloudflare-ai-gateway.models.ts`
- `providers/cloudflare-ai-gateway.ts`
- `providers/cloudflare-auth.ts`
- `providers/cloudflare-stream.ts`
- `providers/cloudflare-workers-ai.models.ts`
- `providers/cloudflare-workers-ai.ts`
- `providers/deepseek.models.ts`
- `providers/fireworks.models.ts`
- `providers/fireworks.ts`
- `providers/github-copilot.models.ts`
- `providers/github-copilot.ts`
- `providers/google-vertex.models.ts`
- `providers/google-vertex.ts`
- `providers/google.models.ts`
- `providers/google.ts`
- `providers/groq.models.ts`
- `providers/groq.ts`
- `providers/huggingface.models.ts`
- `providers/huggingface.ts`
- `providers/kimi-coding.models.ts`
- `providers/kimi-coding.ts`
- `providers/meta.models.ts`
- `providers/meta.ts`
- `providers/minimax-cn.models.ts`
- `providers/minimax-cn.ts`
- `providers/minimax.models.ts`
- `providers/minimax.ts`
- `providers/mistral.models.ts`
- `providers/mistral.ts`
- `providers/moonshotai-cn.models.ts`
- `providers/moonshotai-cn.ts`
- `providers/moonshotai.models.ts`
- `providers/moonshotai.ts`
- `providers/nvidia.models.ts`
- `providers/nvidia.ts`
- `providers/openai-codex.models.ts`
- `providers/openai-codex.ts`
- `providers/openai.models.ts`
- `providers/opencode-go.models.ts`
- `providers/opencode-go.ts`
- `providers/opencode-headers.ts`
- `providers/opencode.models.ts`
- `providers/opencode.ts`
- `providers/openrouter.models.ts`
- `providers/openrouter.ts`
- `providers/qwen-token-plan-cn.models.ts`
- `providers/qwen-token-plan-cn.ts`
- `providers/qwen-token-plan-individual.models.ts`
- `providers/qwen-token-plan-individual.ts`
- `providers/qwen-token-plan.models.ts`
- `providers/qwen-token-plan.ts`
- `providers/radius-config.ts`
- `providers/radius.models.ts`
- `providers/radius.ts`
- `providers/together.models.ts`
- `providers/together.ts`
- `providers/typesafe.models.ts`
- `providers/typesafe.ts`
- `providers/vercel-ai-gateway.models.ts`
- `providers/vercel-ai-gateway.ts`
- `providers/xai.models.ts`
- `providers/xai.ts`
- `providers/xiaomi-token-plan-ams.models.ts`
- `providers/xiaomi-token-plan-ams.ts`
- `providers/xiaomi-token-plan-cn.models.ts`
- `providers/xiaomi-token-plan-cn.ts`
- `providers/xiaomi-token-plan-sgp.models.ts`
- `providers/xiaomi-token-plan-sgp.ts`
- `providers/xiaomi.models.ts`
- `providers/xiaomi.ts`
- `providers/zai-coding-cn.models.ts`
- `providers/zai-coding-cn.ts`
- `providers/zai.models.ts`
- `providers/zai.ts`
- `providers/images/register-builtins.ts`
- `utils/abort-signals.ts`
- `utils/model-operations.ts`
- `utils/models-error.ts`
- `utils/oauth-page.ts`
- `api/anthropic-messages.lazy.ts`
- `api/anthropic-messages.ts`
- `api/azure-openai-config.ts`
- `api/azure-openai-responses.lazy.ts`
- `api/azure-openai-responses.ts`
- `api/bedrock-converse-stream.lazy.ts`
- `api/bedrock-converse-stream.ts`
- `api/classifier-shared.ts`
- `api/cloudflare-ai-binding.ts`
- `api/cloudflare-workers-ai-system-one.lazy.ts`
- `api/cloudflare-workers-ai-system-one.ts`
- `api/cloudflare.ts`
- `api/github-copilot-headers.ts`
- `api/google-generative-ai.lazy.ts`
- `api/google-generative-ai.ts`
- `api/google-shared.ts`
- `api/google-vertex.lazy.ts`
- `api/google-vertex.ts`
- `api/lazy.ts`
- `api/llama-cpp-classify.lazy.ts`
- `api/llama-cpp-classify.ts`
- `api/mistral-conversations.lazy.ts`
- `api/mistral-conversations.ts`
- `api/openai-codex-responses.lazy.ts`
- `api/openai-codex-responses.ts`
- `api/openai-completions.lazy.ts`
- `api/openai-decisions.lazy.ts`
- `api/openai-decisions.ts`
- `api/openai-prompt-cache.ts`
- `api/openai-responses-shared.ts`
- `api/openai-responses.lazy.ts`
- `api/openrouter-images.lazy.ts`
- `api/openrouter-images.ts`
- `api/pi-messages.lazy.ts`
- `api/pi-messages.ts`
- `api/system-one-shared.ts`
- `api/typesafe-system-one.lazy.ts`
- `api/typesafe-system-one.ts`

</details>

### 2) 符号级缺失（TS 有，Rust 整 crate 未见同名）


**`models.ts`** → `models.rs`

- MISSING `CreateModelsOptions` (type) → 期望 `create_models_options`  ← 候选: `create_models`
- MISSING `ModelsApiStreamOptions` (type) → 期望 `models_api_stream_options`
- MISSING `ModelsClassifierOptions` (type) → 期望 `models_classifier_options`
- MISSING `ModelsDeferredCancelOptions` (type) → 期望 `models_deferred_cancel_options`
- MISSING `ModelsDeferredFetchOptions` (type) → 期望 `models_deferred_fetch_options`
- MISSING `ModelsImagesOptions` (type) → 期望 `models_images_options`
- MISSING `ModelsPublication` (type) → 期望 `models_publication`
- MISSING `ModelsRefreshOptions` (type) → 期望 `models_refresh_options`  ← 候选: `refresh`
- MISSING `ModelsRefreshResult` (type) → 期望 `models_refresh_result`  ← 候选: `refresh`, `result`
- MISSING `ModelsRequestTransforms` (type) → 期望 `models_request_transforms`
- MISSING `ModelsSimpleStreamOptions` (type) → 期望 `models_simple_stream_options`
- MISSING `MutableModels` (type) → 期望 `mutable_models`
- MISSING `RefreshModelsContext` (type) → 期望 `refresh_models_context`  ← 候选: `refresh`
- MISSING `applyAuth` (method) → 期望 `apply_auth`
- MISSING `beginProviderRefresh` (method) → 期望 `begin_provider_refresh`  ← 候选: `refresh`
- MISSING `classify` (method) → 期望 `classify`
- MISSING `complete` (method) → 期望 `complete`  ← 候选: `complete_simple`
- MISSING `generateImages` (method) → 期望 `generate_images`
- MISSING `getAllAvailable` (method) → 期望 `get_all_available`
- MISSING `getAllModels` (method) → 期望 `get_all_models`
- MISSING `getAuthenticatedProviders` (method) → 期望 `get_authenticated_providers`  ← 候选: `get_auth`
- MISSING `getAvailableOfType` (method) → 期望 `get_available_of_type`  ← 候选: `get_available`
- MISSING `getModelOfType` (method) → 期望 `get_model_of_type`  ← 候选: `get_model`
- MISSING `getModelsOfType` (method) → 期望 `get_models_of_type`  ← 候选: `get_models`, `get_model`
- MISSING `publishProviderModels` (method) → 期望 `publish_provider_models`
- MISSING `requireChatProvider` (method) → 期望 `require_chat_provider`
- MISSING `requireProvider` (method) → 期望 `require_provider`
- MISSING `resolveRefreshCredential` (method) → 期望 `resolve_refresh_credential`  ← 候选: `refresh`, `resolve`
- MISSING `runProviderRefreshPhase` (method) → 期望 `run_provider_refresh_phase`  ← 候选: `refresh`
- MISSING `stream` (method) → 期望 `stream`  ← 候选: `stream_error`, `stream_simple`, `stream_request`, `stream_deferred`
- MISSING `supersedeProviderRefresh` (method) → 期望 `supersede_provider_refresh`  ← 候选: `refresh`
- <sub>移位 1 项：`refresh`</sub>

**`types.ts`** → `types.rs`

- MISSING `AnthropicAllowedFallbackModel` (type) → 期望 `anthropic_allowed_fallback_model`
- MISSING `AnthropicMessagesCompat` (type) → 期望 `anthropic_messages_compat`  ← 候选: `message`
- MISSING `AnyModel` (type) → 期望 `any_model`
- MISSING `ApiOptionsMap` (type) → 期望 `api_options_map`
- MISSING `ApiStreamOptions` (type) → 期望 `api_stream_options`
- MISSING `AssistantImages` (type) → 期望 `assistant_images`
- MISSING `BaseModel` (type) → 期望 `base_model`
- MISSING `BedrockCompat` (type) → 期望 `bedrock_compat`
- MISSING `ChatTemplateKwargValue` (type) → 期望 `chat_template_kwarg_value`
- MISSING `ClassifierAnswer` (type) → 期望 `classifier_answer`
- MISSING `ClassifierApi` (type) → 期望 `classifier_api`
- MISSING `ClassifierBoolAnswer` (type) → 期望 `classifier_bool_answer`
- MISSING `ClassifierBoolQuestion` (type) → 期望 `classifier_bool_question`
- MISSING `ClassifierChoiceAnswer` (type) → 期望 `classifier_choice_answer`
- MISSING `ClassifierChoiceQuestion` (type) → 期望 `classifier_choice_question`
- MISSING `ClassifierContext` (type) → 期望 `classifier_context`
- MISSING `ClassifierFunction` (type) → 期望 `classifier_function`
- MISSING `ClassifierModel` (type) → 期望 `classifier_model`
- MISSING `ClassifierOptions` (type) → 期望 `classifier_options`
- MISSING `ClassifierQuestion` (type) → 期望 `classifier_question`
- MISSING `ClassifierResult` (type) → 期望 `classifier_result`  ← 候选: `result`
- MISSING `ClassifierScoreAnswer` (type) → 期望 `classifier_score_answer`
- MISSING `ClassifierScoreQuestion` (type) → 期望 `classifier_score_question`
- MISSING `ClassifierStopReason` (type) → 期望 `classifier_stop_reason`
- MISSING `FetchFunction` (type) → 期望 `fetch_function`
- MISSING `GrammarFormat` (type) → 期望 `grammar_format`
- MISSING `GrammarVariants` (type) → 期望 `grammar_variants`
- MISSING `ImageApi` (type) → 期望 `image_api`
- MISSING `ImageModel` (type) → 期望 `image_model`
- MISSING `ImagesContext` (type) → 期望 `images_context`
- MISSING `ImagesFunction` (type) → 期望 `images_function`
- MISSING `ImagesInputContent` (type) → 期望 `images_input_content`
- MISSING `ImagesOptions` (type) → 期望 `images_options`
- MISSING `ImagesOutputContent` (type) → 期望 `images_output_content`
- MISSING `ImagesStopReason` (type) → 期望 `images_stop_reason`
- MISSING `JsonObject` (type) → 期望 `json_object`
- MISSING `JsonRepresentation` (type) → 期望 `json_representation`
- MISSING `JsonValue` (type) → 期望 `json_value`
- MISSING `KnownApi` (type) → 期望 `known_api`
- MISSING `KnownClassifierApi` (type) → 期望 `known_classifier_api`
- MISSING `KnownImageApi` (type) → 期望 `known_image_api`
- MISSING `KnownProvider` (type) → 期望 `known_provider`
- MISSING `MistralConversationsCompat` (type) → 期望 `mistral_conversations_compat`
- MISSING `ModelType` (type) → 期望 `model_type`
- MISSING `ModelTypeMap` (type) → 期望 `model_type_map`
- MISSING `NestedToolCallRecord` (type) → 期望 `nested_tool_call_record`
- MISSING `NestedToolCalls` (type) → 期望 `nested_tool_calls`
- MISSING `OpenAICompletionsCompat` (type) → 期望 `open_ai_completions_compat`
- MISSING `OpenAIResponsesCompat` (type) → 期望 `open_ai_responses_compat`
- MISSING `OpenRouterRouting` (type) → 期望 `open_router_routing`
- MISSING `ProviderClassifier` (type) → 期望 `provider_classifier`
- MISSING `ProviderImages` (type) → 期望 `provider_images`
- MISSING `ProviderImagesOptions` (type) → 期望 `provider_images_options`
- MISSING `ProviderStreamOptions` (type) → 期望 `provider_stream_options`
- MISSING `ProviderStreams` (type) → 期望 `provider_streams`
- MISSING `SessionAffinityFormat` (type) → 期望 `session_affinity_format`  ← 候选: `detect_session_affinity_format`
- MISSING `TextSignatureV1` (type) → 期望 `text_signature_v1`  ← 候选: `encode_text_signature_v1`
- MISSING `ThinkingTokenBudgetField` (type) → 期望 `thinking_token_budget_field`  ← 候选: `token`
- MISSING `VercelGatewayRouting` (type) → 期望 `vercel_gateway_routing`

**`auth/resolve.ts`** → `auth/resolve.rs`

- MISSING `refreshStoredOAuthCredential` (fn) → 期望 `refresh_stored_o_auth_credential`  ← 候选: `refresh`

**`auth/types.ts`** → `auth/types.rs`

- MISSING `LoginOptions` (type) → 期望 `login_options`  ← 候选: `login`

**`providers/faux.ts`** → `providers/faux.rs`

- MISSING `FauxProviderRegistration` (type) → 期望 `faux_provider_registration`  ← 候选: `faux_provider`
- MISSING `createFauxCore` (fn) → 期望 `create_faux_core`

**`utils/event-stream.ts`** → `utils/event-stream.rs`

- MISSING `dequeue` (method) → 期望 `dequeue`
- MISSING `length` (method) → 期望 `length`  ← 候选: `is_recoverable_length`
- <sub>移位 1 项：`enqueue`</sub>

**`utils/transcript.ts`** → `utils/transcript.rs`

- MISSING `TranscriptMessages` (type) → 期望 `transcript_messages`  ← 候选: `message`

**`api/openai-completions.ts`** → `api/openai-completions.rs`

- MISSING `ConvertCompletionsMessagesOptions` (type) → 期望 `convert_completions_messages_options`  ← 候选: `message`
- MISSING `OpenAICompletionsOptions` (type) → 期望 `open_ai_completions_options`
- MISSING `stream` (const) → 期望 `stream`  ← 候选: `stream_error`, `stream_simple`, `stream_request`, `stream_deferred`
- <sub>移位 2 项：`convertMessages`, `streamSimple`</sub>

**`api/openai-responses.ts`** → `api/openai-responses.rs`

- MISSING `OpenAIResponsesOptions` (type) → 期望 `open_ai_responses_options`
- MISSING `stream` (const) → 期望 `stream`  ← 候选: `stream_error`, `stream_simple`, `stream_request`, `stream_deferred`
- <sub>移位 1 项：`streamSimple`</sub>

### 3) 私有顶层函数差异（TS 非导出实现函数，Rust 未见同名）

> 上游 `function foo()` 这类非导出实现函数。Rust 常把它们内联进调用方，
> 因此大量属于正常；但**真实缺口也藏在这里**——edit.ts 的 `prepareEditArguments`
> 就是靠人工读到这一层才发现的（处理 `edits` 为字符串/单对象/legacy 顶层字段）。
> 需人工逐条确认。


**`models.ts`** → `models.rs`

- PRIVATE `hasKnownModelType`
- PRIVATE `withKnownModelTypes`

**`auth/context.ts`** → `auth/context.rs`

- PRIVATE `getProcessEnv`

**`providers/faux.ts`** → `providers/faux.rs`

- PRIVATE `assistantContentToText`
- PRIVATE `cloneMessage`  ← 候选: `message`, `clone`
- PRIVATE `commonPrefixLength`
- PRIVATE `commonPromptPrefixLength`  ← 候选: `prompt`
- PRIVATE `contentToText`
- PRIVATE `createAbortedMessage`  ← 候选: `aborted_message`, `aborted`, `message`, `abort`
- PRIVATE `createDeferredMessage`  ← 候选: `message`
- PRIVATE `estimateTokens`  ← 候选: `token`
- PRIVATE `joinedLength`
- PRIVATE `messageToText`  ← 候选: `message`
- PRIVATE `normalizeFauxAssistantContent`
- PRIVATE `randomId`
- PRIVATE `scheduleChunk`
- PRIVATE `splitStringByTokenSize`  ← 候选: `token`
- PRIVATE `toolResultToText`  ← 候选: `result`
- PRIVATE `withUsageEstimate`

**`utils/error-body.ts`** → `utils/error-body.rs`

- PRIVATE `extractBody`
- PRIVATE `extractStatus`
- PRIVATE `isPlainNonEmptyObject`
- PRIVATE `isReadableStreamLike`
- PRIVATE `pickBodyText`

**`utils/estimate.ts`** → `utils/estimate.rs`

- PRIVATE `estimateTextAndImageContentChars`

**`utils/node-http-proxy.ts`** → `utils/node-http-proxy.rs`

- PRIVATE `parseProxyTargetUrl`

**`utils/pi-user-agent.ts`** → `utils/pi-user-agent.rs`

- PRIVATE `loadNodeOs`

**`utils/provider-env.ts`** → `utils/provider-env.rs`

- PRIVATE `getBunSandboxEnvValue`

**`utils/provider-retry.ts`** → `utils/provider-retry.rs`

- PRIVATE `createAbortError`  ← 候选: `abort`
- PRIVATE `isProviderError`

**`utils/transcript.ts`** → `utils/transcript.rs`

- PRIVATE `isSystemMessage`  ← 候选: `message`

**`utils/validation.ts`** → `utils/validation.rs`

- PRIVATE `getSubSchemaValidator`  ← 候选: `sub_schema_valid`
- PRIVATE `getValidator`

**`api/openai-completions.ts`** → `api/openai-completions.rs`

- PRIVATE `addCacheControlToInstructionMessage`  ← 候选: `message`
- PRIVATE `addCacheControlToLastConversationMessage`  ← 候选: `message`
- PRIVATE `addCacheControlToLastTool`
- PRIVATE `addCacheControlToMessage`  ← 候选: `message`
- PRIVATE `addCacheControlToSystemPrompt`  ← 候选: `prompt`
- PRIVATE `addCacheControlToTextContent`
- PRIVATE `appendOpenAIReasoningDetail`
- PRIVATE `applyAnthropicCacheControl`
- PRIVATE `buildChatTemplateValues`
- PRIVATE `buildParams`
- PRIVATE `createClient`
- PRIVATE `detectCompat`
- PRIVATE `fillMissingCommonReasoningDetailFields`
- PRIVATE `getClientApiKey`
- PRIVATE `getCompatCacheControl`  ← 候选: `get_compat`
- PRIVATE `hasHeader`
- PRIVATE `hasToolHistory`
- PRIVATE `hasValidCommonReasoningDetailFields`
- PRIVATE `isImageContentBlock`  ← 候选: `block`
- PRIVATE `isOpenAICompletionsReasoningField`
- PRIVATE `isOpenAIReasoningDetail`
- PRIVATE `isReasoningDetailObject`
- PRIVATE `isTextContentBlock`  ← 候选: `block`
- PRIVATE `isThinkingContentBlock`  ← 候选: `block`
- PRIVATE `isToolCallBlock`  ← 候选: `block`
- PRIVATE `parseChunkUsage`
- PRIVATE `parseLegacyEncryptedReasoningDetail`
- PRIVATE `parseOpenAIReasoningDetails`
- PRIVATE `resolveChatTemplateKwargValue`  ← 候选: `resolve`
- PRIVATE `resolveClampedThinkingBudget`  ← 候选: `resolve`
- PRIVATE `resolveThinkingTokenBudgetField`  ← 候选: `resolve`, `token`

**`api/openai-responses.ts`** → `api/openai-responses.rs`

- PRIVATE `buildParams`
- PRIVATE `createClient`
- PRIVATE `getClientApiKey`
- PRIVATE `hasHeader`
- PRIVATE `isChatGPTSignIn`

## packages/telemetry → `crates/pi-telemetry/src`

统计：配对 6 文件 · 文件缺失 0 · 范围外 0 · 符号缺失 12 · 符号移位 0 · 私有函数差异 3

### 1) 文件级缺失（范围内，Rust 侧无对应文件）

（无）

### 2) 符号级缺失（TS 有，Rust 整 crate 未见同名）


**`index.ts`** → `lib.rs`

- MISSING `ExactTelemetryAttributes` (type) → 期望 `exact_telemetry_attributes`
- MISSING `InferEventAttributes` (type) → 期望 `infer_event_attributes`
- MISSING `InferOptionalAttributes` (type) → 期望 `infer_optional_attributes`
- MISSING `InferRequiredAndOptionalAttributes` (type) → 期望 `infer_required_and_optional_attributes`
- MISSING `InferStartAttributes` (type) → 期望 `infer_start_attributes`
- MISSING `SchemaTelemetrySpan` (type) → 期望 `schema_telemetry_span`
- MISSING `TelemetrySchemaSpanEndAttributes` (type) → 期望 `telemetry_schema_span_end_attributes`
- MISSING `TelemetrySchemaSpanEventAttributes` (type) → 期望 `telemetry_schema_span_event_attributes`
- MISSING `TelemetrySchemaSpanEventName` (type) → 期望 `telemetry_schema_span_event_name`
- MISSING `TelemetrySchemaSpanName` (type) → 期望 `telemetry_schema_span_name`
- MISSING `TelemetrySchemaSpanStartAttributes` (type) → 期望 `telemetry_schema_span_start_attributes`
- MISSING `TelemetrySchemaSpanUnion` (type) → 期望 `telemetry_schema_span_union`

### 3) 私有顶层函数差异（TS 非导出实现函数，Rust 未见同名）

> 上游 `function foo()` 这类非导出实现函数。Rust 常把它们内联进调用方，
> 因此大量属于正常；但**真实缺口也藏在这里**——edit.ts 的 `prepareEditArguments`
> 就是靠人工读到这一层才发现的（处理 `edits` 为字符串/单对象/legacy 顶层字段）。
> 需人工逐条确认。


**`noop.ts`** → `noop.rs`

- PRIVATE `startNoopSpan`

**`testing/conformance.ts`** → `testing/conformance.rs`

- PRIVATE `rejectsWithSameValue`
- PRIVATE `unreadable`

## packages/durable → `crates/pi-durable/src`

统计：配对 58 文件 · 文件缺失 9 · 范围外 0 · 符号缺失 59 · 符号移位 34 · 私有函数差异 32

### 1) 文件级缺失（范围内，Rust 侧无对应文件）

- `storage/sqlite/cloudflare.ts`
- `storage/sqlite/database.ts`
- `storage/sqlite/migrations.ts`
- `storage/sqlite/node.ts`
- `storage/sqlite/storage.ts`
- `storage/jsonl/node.ts`
- `testing/assertions.ts`
- `testing/runner.ts`
- `testing/types.ts`

### 2) 符号级缺失（TS 有，Rust 整 crate 未见同名）


**`documents.ts`** → `documents.rs`

- MISSING `AnyDocDefinition` (type) → 期望 `any_doc_definition`  ← 候选: `definition`

**`types.ts`** → `types.rs`

- MISSING `CommonDocDefinition` (type) → 期望 `common_doc_definition`  ← 候选: `definition`
- MISSING `ConversationDocFamilyToken` (type) → 期望 `conversation_doc_family_token`  ← 候选: `conversation`
- MISSING `ConversationDocToken` (type) → 期望 `conversation_doc_token`  ← 候选: `conversation`
- MISSING `DocDefinition` (type) → 期望 `doc_definition`  ← 候选: `definition`
- MISSING `DocFamilyDefinition` (type) → 期望 `doc_family_definition`  ← 候选: `definition`
- MISSING `LatestConversationSemantics` (type) → 期望 `latest_conversation_semantics`  ← 候选: `conversation`, `semantics`
- MISSING `PhaseHandler` (type) → 期望 `phase_handler`  ← 候选: `phase`
- MISSING `RewindableConversationDocFamilyToken` (type) → 期望 `rewindable_conversation_doc_family_token`  ← 候选: `conversation`
- MISSING `RewindableConversationDocToken` (type) → 期望 `rewindable_conversation_doc_token`  ← 候选: `conversation`
- MISSING `RewindableConversationSemantics` (type) → 期望 `rewindable_conversation_semantics`  ← 候选: `conversation`, `semantics`
- MISSING `SessionDocFamilyToken` (type) → 期望 `session_doc_family_token`
- MISSING `SessionDocToken` (type) → 期望 `session_doc_token`
- MISSING `TaskDefinition` (type) → 期望 `task_definition`  ← 候选: `definition`
- MISSING `TaskDocFamilyToken` (type) → 期望 `task_doc_family_token`
- MISSING `TaskDocToken` (type) → 期望 `task_doc_token`
- MISSING `TypedEntryDraft` (type) → 期望 `typed_entry_draft`  ← 候选: `entry`, `typed`
- <sub>移位 9 项：`DocumentObserver`, `DocumentReader`, `DocumentState`, `DocumentWatch`, `HookRunner`, `NextTaskState`, `RunningTask`, `Session`, `TaskRuntime`</sub>

**`tools/bash.ts`** → `tools/bash.rs`

- MISSING `BashPrepare` (type) → 期望 `bash_prepare`  ← 候选: `prepare`
- MISSING `BashToolInput` (type) → 期望 `bash_tool_input`
- MISSING `PowerShellToolInput` (type) → 期望 `power_shell_tool_input`
- MISSING `PowerShellToolOptions` (type) → 期望 `power_shell_tool_options`

**`tools/edit.ts`** → `tools/edit.rs`

- MISSING `EditToolInput` (type) → 期望 `edit_tool_input`

**`tools/read.ts`** → `tools/read.rs`

- MISSING `ReadToolInput` (type) → 期望 `read_tool_input`

**`tools/write.ts`** → `tools/write.rs`

- MISSING `WriteToolInput` (type) → 期望 `write_tool_input`

**`storage/memory.ts`** → `storage/memory.rs`

- MISSING `PreparedMemoryCommit` (type) → 期望 `prepared_memory_commit`  ← 候选: `prepare`, `commit`
- MISSING `applyDocumentActions` (method) → 期望 `apply_document_actions`  ← 候选: `document`, `apply`
- MISSING `applyPreparedCommit` (method) → 期望 `apply_prepared_commit`  ← 候选: `prepare`, `commit`, `apply`
- MISSING `checkDocumentActions` (method) → 期望 `check_document_actions`  ← 候选: `document`
- MISSING `checkGlobalIds` (method) → 期望 `check_global_ids`
- MISSING `prepareCommit` (method) → 期望 `prepare_commit`  ← 候选: `prepare`, `commit`
- MISSING `prepareDocumentActions` (method) → 期望 `prepare_document_actions`  ← 候选: `document`, `prepare`
- MISSING `resolveDocumentCopies` (method) → 期望 `resolve_document_copies`  ← 候选: `document`, `resolve`
- <sub>移位 1 项：`materializeDocument`</sub>

**`storage/jsonl/storage.ts`** → `storage/jsonl/storage.rs`

- MISSING `JsonlCorruptionError` (class) → 期望 `jsonl_corruption_error`
- MISSING `JsonlStoragePoisonedError` (class) → 期望 `jsonl_storage_poisoned_error`  ← 候选: `poisoned_error`, `storage`, `poison`
- MISSING `adoptSidecarState` (method) → 期望 `adopt_sidecar_state`  ← 候选: `adopt`, `state`
- MISSING `confirmRecord` (method) → 期望 `confirm_record`  ← 候选: `record`
- MISSING `planReclamations` (method) → 期望 `plan_reclamations`
- MISSING `readLines` (method) → 期望 `read_lines`  ← 候选: `read_line`, `lines`
- MISSING `reclaimSidecars` (method) → 期望 `reclaim_sidecars`
- MISSING `recover` (method) → 期望 `recover`
- MISSING `replaceSidecar` (method) → 期望 `replace_sidecar`  ← 候选: `replace`, `place`
- MISSING `resolveFile` (method) → 期望 `resolve_file`  ← 候选: `resolve`
- MISSING `store` (method) → 期望 `store`  ← 候选: `stored_bytes`, `restore_line_endings`, `items_round_trip_through_their_stored_json`, `select_extensions_uses_the_stored_array_when_present`
- <sub>移位 2 项：`encodeCommit`, `poison`</sub>

**`env/index.ts`** → `env/index.rs`

- MISSING `Result` (type) → 期望 `result`  ← 候选: `result_of`, `tool_result`, `final_result`, `closed_result`
- MISSING `err` (fn) → 期望 `err`
- MISSING `getOrThrow` (fn) → 期望 `get_or_throw`
- MISSING `getOrUndefined` (fn) → 期望 `get_or_undefined`
- MISSING `ok` (fn) → 期望 `ok`
- MISSING `toError` (fn) → 期望 `to_error`

**`testing/env-conformance.ts`** → `testing/env_conformance.rs`

- MISSING `createEnvConformance` (fn) → 期望 `create_env_conformance`  ← 候选: `create`

**`testing/storage-conformance.ts`** → `testing/storage_conformance.rs`

- MISSING `createStorageConformance` (fn) → 期望 `create_storage_conformance`  ← 候选: `storage`, `create`

**`harness/compaction.ts`** → `harness/compaction.rs`

- MISSING `CompactionTask` (const) → 期望 `compaction_task`  ← 候选: `make_compaction_task`, `compact`, `compaction_task_metadata_matches_upstream`

**`harness/generation.ts`** → `harness/generation.rs`

- MISSING `GenerationTask` (const) → 期望 `generation_task`  ← 候选: `make_generation_task`, `create_generation_task_id`, `generation_task_metadata_matches_upstream`
- <sub>移位 1 项：`startRun`</sub>

**`harness/registry.ts`** → `harness/registry.rs`

- MISSING `BUILTIN_TASKS` (const) → 期望 `builtin_tasks`  ← 候选: `tasks`, `builtin_tasks_are_installed_in_the_snapshot`
- <sub>移位 6 项：`extension`, `installed`, `sections`, `task`, `tasks`, `tools`</sub>

**`harness/task-graph.ts`** → `harness/task_graph.rs`

- MISSING `TaskGraphWatch` (type) → 期望 `task_graph_watch`  ← 候选: `task_graph`, `watch`

**`harness/tool.ts`** → `harness/tool.rs`

- MISSING `ToolTask` (const) → 期望 `tool_task`  ← 候选: `make_tool_task`, `tool_task_checkpoint_round_trips`, `tool_task_metadata_matches_upstream`

**`harness/types.ts`** → `harness/types.rs`

- MISSING `AnyTask` (type) → 期望 `any_task`
- MISSING `HooksOf` (type) → 期望 `hooks_of`  ← 候选: `hooks`

**`harness/usage.ts`** → `harness/usage.rs`

- MISSING `addUsage` (fn) → 期望 `add_usage`  ← 候选: `usage`, `add_usage_json`, `add_usage_state`

### 3) 私有顶层函数差异（TS 非导出实现函数，Rust 未见同名）

> 上游 `function foo()` 这类非导出实现函数。Rust 常把它们内联进调用方，
> 因此大量属于正常；但**真实缺口也藏在这里**——edit.ts 的 `prepareEditArguments`
> 就是靠人工读到这一层才发现的（处理 `edits` 为字符串/单对象/legacy 顶层字段）。
> 需人工逐条确认。


**`documents.ts`** → `documents.rs`

- PRIVATE `ownerId`

**`tools/image.ts`** → `tools/image.rs`

- PRIVATE `readUint16LE`
- PRIVATE `readUint32BE`
- PRIVATE `readUint32LE`

**`env/line-scan.ts`** → `env/line_scan.rs`

- PRIVATE `decodedBytes`  ← 候选: `decode`

**`env/node-watch.ts`** → `env/node_watch.rs`

- PRIVATE `isNodeError`  ← 候选: `is_node_error_kind`

**`env/node.ts`** → `env/node.rs`

- PRIVATE `fileInfoFromStats`  ← 候选: `file_info`
- PRIVATE `fileKindFromStats`
- PRIVATE `findBashOnPath`
- PRIVATE `getBashShellConfig`
- PRIVATE `getShellConfig`
- PRIVATE `getShellEnv`
- PRIVATE `isLegacyWslBashPath`
- PRIVATE `isNodeError`  ← 候选: `is_node_error_kind`
- PRIVATE `pathExists`  ← 候选: `exists`
- PRIVATE `waitForChildProcess`  ← 候选: `process`

**`testing/env-conformance.ts`** → `testing/env_conformance.rs`

- PRIVATE `abortedContext`  ← 候选: `aborted`, `context`, `abort`, `waiters_reject_an_already_aborted_context`
- PRIVATE `covers`  ← 候选: `failed_outcome_covers_held_and_terminal_failures`
- PRIVATE `errorCode`
- PRIVATE `readAll`
- PRIVATE `watching`  ← 候选: `watch`

**`testing/storage-conformance.ts`** → `testing/storage_conformance.rs`

- PRIVATE `assertionFacade`
- PRIVATE `createCase`  ← 候选: `create`

**`harness/agent.ts`** → `harness/agent.rs`

- PRIVATE `isList`
- PRIVATE `names`  ← 候选: `names_json`, `sidecar_file_names_are_validated`, `summary_failure_names_the_reason`, `marker_json_uses_upstream_field_names`

**`harness/compaction.ts`** → `harness/compaction.rs`

- PRIVATE `contentText`  ← 候选: `content_text_of_user`, `content_text_of_blocks`

**`harness/context.ts`** → `harness/context.rs`

- PRIVATE `freezeJson`

**`harness/generation.ts`** → `harness/generation.rs`

- PRIVATE `createToolTask`  ← 候选: `create`

**`harness/scheduler.ts`** → `harness/scheduler/mod.rs`

- PRIVATE `delay`  ← 候选: `progress_delays_the_next_commit_by_the_minimum_interval`, `schedule_expiry_clamps_a_late_expiry_to_the_maximum_delay`
- PRIVATE `erased`
- PRIVATE `sessionMethod`

**`session/observation.ts`** → `session/observation.rs`

- PRIVATE `toError`

## packages/chord → `crates/pi-durable/src/chord`

统计：配对 4 文件 · 文件缺失 5 · 范围外 20 · 符号缺失 15 · 符号移位 1 · 私有函数差异 11

### 1) 文件级缺失（范围内，Rust 侧无对应文件）

- `delta/apply-immutable-trusted.ts`
- `delta/tracker.ts`
- `services/state-codec.ts`
- `services/state-internals.ts`
- `services/state.ts`

<details><summary>范围外文件 20 个（AGENT.md 已声明不复刻）</summary>

- `api.ts`
- `bundler.ts`
- `node.ts`
- `types.ts`
- `delta/diff.ts`
- `delta/draft.ts`
- `delta/revision-validator.ts`
- `facets/host.ts`
- `facets/loader.ts`
- `node/bundle-loader.ts`
- `node/bundle.ts`
- `node/manifest.ts`
- `node/package.ts`
- `services/consumer.ts`
- `services/errors.ts`
- `services/handle.ts`
- `services/instances.ts`
- `services/loopback.ts`
- `services/provider.ts`
- `services/wire.ts`

</details>

### 2) 符号级缺失（TS 有，Rust 整 crate 未见同名）


**`json.ts`** → `json.rs`

- MISSING `CopyJsonOptions` (type) → 期望 `copy_json_options`  ← 候选: `copy_json`
- MISSING `isJsonValue` (fn) → 期望 `is_json_value`  ← 候选: `value`

**`delta/index.ts`** → `delta.rs`

- MISSING `NonEmptyPath` (type) → 期望 `non_empty_path`
- MISSING `PathError` (class) → 期望 `path_error`
- MISSING `PathRef` (type) → 期望 `path_ref`
- MISSING `RESERVED_SEGMENTS` (const) → 期望 `reserved_segments`
- MISSING `Seg` (type) → 期望 `seg`
- MISSING `UnsafePathError` (class) → 期望 `unsafe_path_error`
- MISSING `assertValidOp` (fn) → 期望 `assert_valid_op`
- MISSING `assertValidWireOp` (fn) → 期望 `assert_valid_wire_op`
- MISSING `isBase` (const) → 期望 `is_base`
- MISSING `isReplace` (const) → 期望 `is_replace`  ← 候选: `replace`

**`context/index.ts`** → `context.rs`

- MISSING `createContextKey` (fn) → 期望 `create_context_key`
- MISSING `toString` (method) → 期望 `to_string`
- MISSING `withContextValue` (fn) → 期望 `with_context_value`  ← 候选: `value`
- <sub>移位 1 项：`value`</sub>

### 3) 私有顶层函数差异（TS 非导出实现函数，Rust 未见同名）

> 上游 `function foo()` 这类非导出实现函数。Rust 常把它们内联进调用方，
> 因此大量属于正常；但**真实缺口也藏在这里**——edit.ts 的 `prepareEditArguments`
> 就是靠人工读到这一层才发现的（处理 `edits` 为字符串/单对象/legacy 顶层字段）。
> 需人工逐条确认。


**`json.ts`** → `json.rs`

- PRIVATE `check`
- PRIVATE `copy`
- PRIVATE `defineData`

**`delta/index.ts`** → `delta.rs`

- PRIVATE `applyOps`  ← 候选: `apply`
- PRIVATE `assertIndexInRange`  ← 候选: `index`
- PRIVATE `assertPathArg`
- PRIVATE `assertPermutation`
- PRIVATE `copyContainers`
- PRIVATE `resolve`  ← 候选: `resolve_mut`, `resolve_parent_mut`
- PRIVATE `resolveValue`  ← 候选: `value`

**`context/index.ts`** → `context.rs`

- PRIVATE `abortError`  ← 候选: `abort`

---

# 人工复核结论

> 上方是机械扫描结果。本节的职责只有两件事：**(1) 说明哪些 MISSING 是假阳性，(2) 列出仍未对齐的项**。
> 已对齐项的改动明细见 git 历史，此处不再保留。
> 本轮已按 v1.1.0 新架构重跑扫描，并逐类人工复核过；P10 又做了一次双向
> （TS→Rust 缺失 + Rust→TS 多余）逐文件逐方法审计并修复，结论见 `tools.d/parity/AUDIT-REPORT.md`。

## 一、机械扫描的已知假阳性（非遗漏）

以下类别扫描器会报 MISSING，但 Rust 侧已有等价实现：

- **重载拆分 / 命名适配**：agent 的 `prompt`（字符串/消息重载）→ `prompt_text` /
  `prompt_text_with_images` / `prompt_messages`，`continue` → `continue_turn`；
  `steeringMode` / `followUpMode` getter/setter → `set_steering_mode` / `steering_mode` /
  `set_follow_up_mode` / `follow_up_mode` 访问器（P10 补全）；`restoreSession`→`restore_session_arc` 等。
- **类型合并**：durable `types.ts` 的 `*DocToken` / `*DocDefinition` / `*DocFamilyToken` /
  `*Semantics` 十几项 → `DocToken` / `DocDefinitionSpec` / `DocumentSemantics` 等合并类型；
  `AnyTask` / `HooksOf` 是条件类型推导，Rust 无对应能力。
- **语言机制豁免**：`env/index.ts` 的 `Result` / `ok` / `err` / `getOrThrow` / `getOrUndefined` /
  `toError` → Rust 标准库 `Result`；chord `json.ts` 的 `isJsonValue` / `omitUndefinedProperties` →
  Rust `serde_json::Value` 类型系统保证严格 JSON（无 `undefined`/cycle/symbol），序列化时用
  `skip_serializing_if` 剔除；chord `context` 的 `createContextKey` / `withContextValue` → pi-durable 不用
  （`context.rs` 已声明只实现 `abortSignal`）。
  （chord `delta` 的 `WireOp` / `Encoder` / `Decoder` path-interning 已由 P10 补全，不再是 serde 替代。）
- **类型级编程**：`pi-telemetry` 的 12 项与 `harness/types.ts` 的 span 推导类型；tools 的
  `*ToolInput` 是 TypeBox schema 推导，Rust 用 JSON schema + 运行时校验。
- **合并函数**：`storage/memory.ts` 的 `prepareCommit` / `applyPreparedCommit` / `checkGlobalIds` /
  `checkDocumentActions` / `prepareDocumentActions` / `resolveDocumentCopies` / `applyDocumentActions`
  → `validate_writes` + `apply_writes`；`storage/jsonl` 的 `JsonlCorruptionError` /
  `JsonlStoragePoisonedError` → `StorageError` 变体。
- **工厂模式**：`harness/compaction.ts` 的 `CompactionTask`、`generation.ts` 的 `GenerationTask`、
  `tool.ts` 的 `ToolTask`、`registry.ts` 的 `BUILTIN_TASKS` 这些模块级 const → Rust 用
  `make_*_task()` 工厂 + `create_registry()` 里的 `OnceLock` 组装（Rust 无模块级可变初始化）。
- **内联实现**：第 3 节私有函数档多属此类（`isList`/`names`→`names_json`、`freezeJson`→Rust
  owned 值、`createToolTask`→registry 组装、`fileInfoFromStats`/`fileKindFromStats`→
  `file_info_from_metadata`、`decodedBytes`→`str::len()`、`readUint*`→`read_u*` 等）。
- **文件合并**：chord 的 `delta/apply-immutable-trusted.ts` / `delta/index.ts` → `delta.rs`，
  `delta/tracker.ts` → `tracker.rs`，`services/state*.ts` → `state.rs`（扫描配对规则未识别 1:N 合并）。
- **范围外**：`pi-ai` 108 项中的绝大多数（155 个范围外文件：其他 provider / Classifier /
  Images / 各厂商 Compat / Routing / OAuth 登录流程）；chord 的 `facets`/`node`/`services` 其余。
- **durable 9 个「文件级缺失」是已知豁免**：`storage/sqlite/{cloudflare,database,node,migrations}.ts`
  与 `storage/jsonl/node.ts` 是跨运行时异步 facade（不复刻 / 语言机制豁免）；
  `storage/sqlite/storage.ts`（934 行）已合并进 `storage/sqlite.rs`（1:N）；
  `testing/{assertions,runner}.ts` 是 JS 测试适配器；`testing/types.ts` 已合并进
  `storage_conformance.rs` / `env_conformance.rs`。

## 二、仍未对齐的项（P10 双向审计后）

详情与工作量见 `todos.md`：

- **B1** Node 平台细节（`findBashOnPath` / WSL 检测 / `getShellConfig` 的 Windows 分支）——
  macOS/Linux 下 `std::process` 已等价覆盖，Windows 分支不在本项目范围。
- **C2** `pi-telemetry` 的类型级推导 12 项 —— 豁免。
- **C3** session 4 个具名错误 —— 消息已逐字对齐、上游无 `instanceof` 分支，不建议投入。
- **storage-benchmark**（489 行）—— 性能基准，非正确性验证，待续（可选）。
- **上游能力偏差**：Z.AI CN overflow / HTTP-date `Retry-After`。
  （`onProviderStreamEvent` 已在 P10 补全，不再列为偏差。）

## 三、扫描口径与已知盲区

- 符号匹配用「去分隔符 + 小写」的 compact 键，因此 `lazyOAuth` ↔ `lazy_oauth` 这类缩写差异不会误报。
- `local`（同文件命中）视为 OK；`global`（同 crate 其他文件）记为「移位」；整 crate 无同名记 MISSING。
- 第 3 节的私有函数档覆盖上游非导出 `function`，是导出符号扫描的补充。
- **已知盲区**：`tagged_error!` 等宏生成的类型扫不到（见第一节）；Rust 侧内联实现无法自动识别；
  1:N 文件合并（如 chord `delta/index.ts` + `apply-immutable-trusted.ts` → `delta.rs`）扫不到。
